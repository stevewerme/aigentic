//! The loop. Kept small on purpose; behaviour that is not the loop lives
//! behind the seams in `seams.rs`.

use std::time::Instant;

use aigentic_core::{
    Author, CompletionRequest, ContentBlock, EventKind, ProviderEvent, ToolCall, ToolResult,
    ToolSpec,
};
use aigentic_log::{
    AssistantMessagePayload, InterruptedPayload, ToolResultPayload, Usage, UserMessagePayload,
};
use futures_util::StreamExt;

use aigentic_log::{Invoker, PolicyRecord};

use crate::decisions::{CancelToken, Inbox, Queued};
use crate::harness_tools::{ASK_HUMAN, HARNESS_CLASS, harness_specs, is_harness_tool};
use crate::runtime::{ASKED_HUMAN, INTERRUPTED};
use crate::seams::{Verdict, author_name, denial_text};
use crate::support::{Spent, append_queued, flush_text};
use crate::{Runtime, RuntimeError, Signal, TurnOutcome, build_context};

impl Runtime {
    /// Run one turn: append the user message, then iterate until the model
    /// returns no tool calls or the budget is hit. Every outcome, including
    /// a budget stop or a provider failure, ends with a `turn_ended` event.
    pub async fn run_turn(
        &mut self,
        author: Author,
        blocks: Vec<ContentBlock>,
        observe: &mut (dyn FnMut(Signal<'_>) + Send),
    ) -> Result<TurnOutcome, RuntimeError> {
        self.run_turn_until(
            author,
            blocks,
            &CancelToken::never(),
            &mut Inbox::none(),
            observe,
        )
        .await
    }

    /// `run_turn` with an interrupt token and an inbox (phase 5): the
    /// daemon's actor calls this so a `Post` mid-turn reaches the log
    /// at the next safe point and a `Post { interrupt: true }` can end
    /// the turn.
    pub async fn run_turn_until(
        &mut self,
        author: Author,
        blocks: Vec<ContentBlock>,
        cancel: &CancelToken,
        inbox: &mut Inbox,
        observe: &mut (dyn FnMut(Signal<'_>) + Send),
    ) -> Result<TurnOutcome, RuntimeError> {
        let payload = serde_json::to_value(UserMessagePayload::new(blocks)).expect("serialisable");
        self.append(EventKind::UserMessage, author, payload, None, observe)?;
        self.continue_turn_until(cancel, inbox, observe).await
    }

    /// Append what arrived mid-turn as `mid_turn`, `steer` user messages
    /// (issue #33): in the log at once, and projected where they sit so
    /// the next model call sees them — never between an assistant
    /// message and that message's own tool results.
    fn drain_inbox(
        &mut self,
        inbox: &mut Inbox,
        observe: &mut (dyn FnMut(Signal<'_>) + Send),
    ) -> Result<(), RuntimeError> {
        for queued in inbox.drain() {
            append_queued(&mut self.log, queued, observe, true)?;
        }
        Ok(())
    }

    /// A user-invoked skill (`/implement fix the off-by-one`): appends
    /// `skill_loaded` with the body, attributed to the user, then a user
    /// message with the arguments, then runs the turn. Any enabled skill
    /// can be invoked this way; the client decides which to expose.
    pub async fn invoke_skill(
        &mut self,
        author: Author,
        name: &str,
        args: &str,
        observe: &mut (dyn FnMut(Signal<'_>) + Send),
    ) -> Result<TurnOutcome, RuntimeError> {
        self.invoke_skill_until(
            author,
            name,
            args,
            &CancelToken::never(),
            &mut Inbox::none(),
            observe,
        )
        .await
    }

    /// `invoke_skill` with an interrupt token and an inbox (phase 5).
    pub async fn invoke_skill_until(
        &mut self,
        author: Author,
        name: &str,
        args: &str,
        cancel: &CancelToken,
        inbox: &mut Inbox,
        observe: &mut (dyn FnMut(Signal<'_>) + Send),
    ) -> Result<TurnOutcome, RuntimeError> {
        self.append_skill_loaded_as(name, Invoker::User, author.clone(), observe)?;
        let text = if args.trim().is_empty() {
            format!("Run the `{name}` skill now.")
        } else {
            args.trim().to_owned()
        };
        self.run_turn_until(
            author,
            vec![ContentBlock::Text(text)],
            cancel,
            inbox,
            observe,
        )
        .await
    }

    /// The loop without a new user message: what `run_turn` does after the
    /// append, and what resume uses to finish an interrupted turn.
    pub async fn continue_turn(
        &mut self,
        observe: &mut (dyn FnMut(Signal<'_>) + Send),
    ) -> Result<TurnOutcome, RuntimeError> {
        self.continue_turn_until(&CancelToken::never(), &mut Inbox::none(), observe)
            .await
    }

    /// `continue_turn` with an interrupt (phase 5). When `cancel` fires:
    /// the in-flight model call is dropped and nothing partial is
    /// appended; a tool already running finishes (its timeout bounds
    /// it) and its result is recorded; the rest of that batch gets
    /// synthetic error results so every call has one; then
    /// `interrupted` names who, and `turn_ended` says `interrupted`.
    pub async fn continue_turn_until(
        &mut self,
        cancel: &CancelToken,
        inbox: &mut Inbox,
        observe: &mut (dyn FnMut(Signal<'_>) + Send),
    ) -> Result<TurnOutcome, RuntimeError> {
        let mut spent = Spent {
            started: Instant::now(),
            waited: std::time::Duration::ZERO,
            iterations: 0,
            tokens: 0,
        };
        self.refresh_knowledge()?;
        self.refresh_memory()?;
        let specs: Vec<ToolSpec> = self.tool_specs();
        // What a stream held back (issue #33): a message posted mid-call
        // waits here, and leaves the log only where the model can first
        // read it — `steer` at the top of the loop below, `steer` unset
        // in `end_turn`, before `turn_ended`, if the turn ends first.
        let mut held: Vec<Queued> = Vec::new();

        loop {
            self.drain_inbox(inbox, observe)?;
            if let Some(by) = cancel.cancelled_by() {
                return self.interrupt_turn(by, Vec::new(), &spent, &mut held, observe);
            }
            // The budget is checked here, before a model call, and never
            // between an assistant message and its tool results. Once an
            // assistant message with tool calls is in the log, every call
            // must get a result: OpenAI-compatible endpoints reject a
            // conversation whose tool calls have no matching tool message,
            // so a log left in that state could not be resumed.
            if let Some(reason) = self.budget_reason(&spent) {
                return self.end_turn(reason, &spent, &mut held, observe);
            }
            // What a stream held, appended at the point of first sight:
            // the reply that was streaming when it arrived had tool
            // calls, and they are all answered now, so this is after
            // that reply's last tool result and before the model call
            // that reads it — never inside the assistant-and-results
            // pair. The budget above has agreed, so no message is
            // promised `steer` to a call that never happens, and nothing
            // below can lose it.
            for queued in std::mem::take(&mut held) {
                append_queued(&mut self.log, queued, observe, true)?;
            }
            // Compaction, at an iteration boundary only: every tool call
            // already has its result, so no summary range splits a turn.
            // Eviction runs at the same boundary, and first: stubbing is
            // free where a summary costs a call (issue #30).
            self.evict_stale(observe)?;
            if let Err(e) = self.compact(observe).await {
                if let RuntimeError::Provider(p) = &e {
                    self.end_turn(&format!("provider_error: {p}"), &spent, &mut held, observe)?;
                }
                return Err(e);
            }
            let events = self.log.read_all()?;
            let context = build_context(&self.prefix(), &events)?;
            let request = CompletionRequest {
                messages: &context,
                tools: &specs,
                max_output_tokens: None,
            };

            let (mut blocks, mut text, mut usage, mut error) =
                (Vec::new(), String::new(), None, None);
            let mut stream = self.provider.complete(&request);
            let mut interrupted_by = None;
            loop {
                let event = tokio::select! {
                    biased;
                    by = cancel.cancelled() => {
                        interrupted_by = Some(by);
                        break;
                    }
                    // A message posted mid-call is held, not appended:
                    // the stream borrows the provider, the log is
                    // another field, and this call is already out, so
                    // nothing can show the message to it. It leaves
                    // `held` at the next safe point — after that reply's
                    // last tool result and before the call that reads it
                    // (`steer`), or before `turn_ended` if the turn ends
                    // first (`steer` unset), so the next turn, which
                    // the actor starts, picks it up.
                    queued = inbox.recv() => {
                        held.push(queued);
                        continue;
                    }
                    event = stream.next() => event,
                };
                let Some(event) = event else { break };
                match event {
                    ProviderEvent::TextDelta(t) => {
                        observe(Signal::TextDelta(&t));
                        text.push_str(&t);
                    }
                    ProviderEvent::ToolCall(call) => {
                        flush_text(&mut text, &mut blocks);
                        blocks.push(ContentBlock::ToolCall(call));
                    }
                    ProviderEvent::Blob(blob) => blocks.push(ContentBlock::ProviderBlob(blob)),
                    ProviderEvent::Usage(u) => usage = Some(Usage::reported(u)),
                    ProviderEvent::Done { .. } => {}
                    ProviderEvent::Error(e) => {
                        error = Some(e);
                        break;
                    }
                }
            }
            drop(stream);
            // What arrived in the instants the last polls of the stream
            // raced past joins what the arm held — same rule, same point
            // of first sight — rather than landing before the assistant
            // message of the reply it interrupted.
            held.extend(inbox.drain());
            if let Some(by) = interrupted_by {
                // Dropped mid-call: no partial message, as a crash would
                // leave none.
                return self.interrupt_turn(by, Vec::new(), &spent, &mut held, observe);
            }
            flush_text(&mut text, &mut blocks);
            spent.iterations += 1;
            let agent = Author::Agent(self.agent.clone());
            self.measured = usage.map(|u| {
                (
                    u.input_tokens + u.cache_read_tokens + u.cache_write_tokens,
                    context.len(),
                )
            });
            observe(Signal::Usage(self.window_usage(&context)));
            let usage = usage.unwrap_or_else(|| self.estimate_usage(&context, &agent, &blocks));
            spent.tokens += self.budget.spent_of(&usage.to_core());
            if let Some(e) = error {
                self.end_turn(&format!("provider_error: {e}"), &spent, &mut held, observe)?;
                return Err(RuntimeError::Provider(e));
            }

            let calls: Vec<ToolCall> = blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolCall(c) => Some(c.clone()),
                    _ => None,
                })
                .collect();
            let payload = serde_json::to_value(AssistantMessagePayload {
                blocks,
                usage: Some(usage),
            })
            .expect("serialisable");
            let assistant =
                self.append(EventKind::AssistantMessage, agent, payload, None, observe)?;
            if calls.is_empty() {
                return self.end_turn("done", &spent, &mut held, observe);
            }

            let mut answered = false;
            let mut calls = calls.into_iter();
            for call in calls.by_ref() {
                self.drain_inbox(inbox, observe)?;
                observe(Signal::ToolCallStarted(&call));
                let (result, record, by) = self
                    .execute(&call, cancel, inbox, observe, &mut spent.waited)
                    .await?;
                answered |= call.name == ASK_HUMAN && !result.is_error;
                let payload = serde_json::to_value(ToolResultPayload::new(result, record))
                    .expect("serialisable");
                // An answered `ask_human` is the human's event, like a
                // decision; every other result is the system's.
                self.append(
                    EventKind::ToolResult,
                    by,
                    payload,
                    Some(assistant.id),
                    observe,
                )?;
                if cancel.cancelled_by().is_some() {
                    break;
                }
            }
            self.drain_inbox(inbox, observe)?;
            if let Some(by) = cancel.cancelled_by() {
                // Every call must have a result before the turn ends, or
                // the log could not be projected; the rest get synthetic
                // ones that say why.
                let mut synthetic = Vec::new();
                for call in calls {
                    let result = ToolResult {
                        id: call.id.clone(),
                        content: format!(
                            "not run: the turn was interrupted by {} before this call",
                            crate::seams::author_name(&by)
                        ),
                        is_error: true,
                    };
                    let payload = serde_json::to_value(ToolResultPayload::new(
                        result,
                        PolicyRecord::rule(INTERRUPTED, "deny"),
                    ))
                    .expect("serialisable");
                    self.append(
                        EventKind::ToolResult,
                        Author::System,
                        payload,
                        Some(assistant.id),
                        observe,
                    )?;
                    synthetic.push(call.id);
                }
                return self.interrupt_turn(by, synthetic, &spent, &mut held, observe);
            }
            // A human's answer starts a turn: everything after it is new
            // work with its own budget. The client continues at once.
            if answered {
                return self.end_turn(ASKED_HUMAN, &spent, &mut held, observe);
            }
        }
    }

    /// `run_tool`, appending what arrives in the inbox while the tool
    /// runs: the tool borrows the registry, the log is another field.
    /// An interrupt kills the call where it stands. Waiting for it to
    /// finish would make Esc's answer the tool's own timeout (now up
    /// to 900 s), so the future is dropped — `bash` tears its process
    /// group down from there — and the result records what happened.
    async fn run_tool_draining(
        &mut self,
        call: &ToolCall,
        cancel: &CancelToken,
        inbox: &mut Inbox,
        observe: &mut (dyn FnMut(Signal<'_>) + Send),
    ) -> Result<ToolResult, RuntimeError> {
        let id = call.id.clone();
        let Some(tool) = self.registry.get(&call.name) else {
            return Ok(ToolResult {
                id,
                content: format!("unknown tool: {}", call.name),
                is_error: true,
            });
        };
        let mut fut = tool.call(call.args.clone());
        let out = loop {
            tokio::select! {
                out = &mut fut => break out,
                queued = inbox.recv() => append_queued(&mut self.log, queued, observe, true)?,
                by = cancel.cancelled() => {
                    return Ok(ToolResult {
                        id,
                        content: format!(
                            "interrupted by {} while running: the call was killed; \
                             rerun it if needed",
                            author_name(&by)
                        ),
                        is_error: true,
                    });
                }
            }
        };
        Ok(match out {
            Ok(out) => ToolResult {
                id,
                content: out.content,
                is_error: out.is_error,
            },
            Err(e) => ToolResult {
                id,
                content: e.to_string(),
                is_error: true,
            },
        })
    }

    /// `interrupted` naming who, then `turn_ended` with reason
    /// `interrupted`. `unanswered` lists the calls given synthetic
    /// results. `held` — what a stream was holding — goes into the log
    /// first, `steer` unset: it was posted before the interrupt, so it
    /// belongs before the point `interrupted` marks, and before
    /// `turn_ended`, which is what lets the projection emit it for the
    /// turn that answers it.
    fn interrupt_turn(
        &mut self,
        by: Author,
        unanswered: Vec<String>,
        spent: &Spent,
        held: &mut Vec<Queued>,
        observe: &mut (dyn FnMut(Signal<'_>) + Send),
    ) -> Result<TurnOutcome, RuntimeError> {
        for queued in std::mem::take(held) {
            append_queued(&mut self.log, queued, observe, false)?;
        }
        let after_seq = self.log.len().saturating_sub(1);
        let payload = serde_json::to_value(InterruptedPayload {
            reason: "interrupt".into(),
            after_seq,
            unanswered_calls: unanswered,
            by: Some(by),
        })
        .expect("serialisable");
        self.append(
            EventKind::Interrupted,
            Author::System,
            payload,
            None,
            observe,
        )?;
        self.end_turn(INTERRUPTED, spent, held, observe)
    }

    /// The one call site for tool execution, behind `policy_check`. An
    /// unknown tool is refused with a rule record; a harness tool is
    /// answered here; anything else runs from the registry. The author
    /// is the result event's: the human who answered an `ask_human`,
    /// else the system. Time spent in the policy check (a prompt) and in
    /// `ask_human` is added to `waited`.
    async fn execute(
        &mut self,
        call: &ToolCall,
        cancel: &CancelToken,
        inbox: &mut Inbox,
        observe: &mut (dyn FnMut(Signal<'_>) + Send),
        waited: &mut std::time::Duration,
    ) -> Result<(ToolResult, PolicyRecord, Author), RuntimeError> {
        // A tool the layers hide is unknown to this thread: the same
        // refusal as a name that was never registered.
        let class = if !self.tool_visible(&call.name) {
            None
        } else if is_harness_tool(&call.name) {
            Some(HARNESS_CLASS)
        } else {
            self.registry.get(&call.name).map(|t| t.risk_class())
        };
        let Some(class) = class else {
            return Ok((
                ToolResult {
                    id: call.id.clone(),
                    content: format!("unknown tool: {}", call.name),
                    is_error: true,
                },
                PolicyRecord::rule("unknown tool", "deny"),
                Author::System,
            ));
        };
        let asked = Instant::now();
        let verdict = self.policy_check(call, class, cancel, observe).await?;
        *waited += asked.elapsed();
        match verdict {
            Verdict::Run(record) => {
                let (result, by) = if is_harness_tool(&call.name) {
                    let started = Instant::now();
                    let ran = self.run_harness_tool(call, cancel, observe).await?;
                    if call.name == ASK_HUMAN {
                        *waited += started.elapsed();
                    }
                    ran
                } else {
                    (
                        self.run_tool_draining(call, cancel, inbox, observe).await?,
                        Author::System,
                    )
                };
                Ok((result, record, by))
            }
            Verdict::Refuse(record) => Ok((
                ToolResult {
                    id: call.id.clone(),
                    content: denial_text(&record),
                    is_error: true,
                },
                record,
                Author::System,
            )),
            Verdict::RefuseWith { record, text } => Ok((
                ToolResult {
                    id: call.id.clone(),
                    content: text,
                    is_error: true,
                },
                record,
                Author::System,
            )),
        }
    }

    /// Registry specs plus the harness tools, narrowed by the layers and
    /// sorted by name. What the model sees.
    pub fn tool_specs(&self) -> Vec<ToolSpec> {
        let mut specs = self.registry.specs();
        specs.extend(harness_specs(!self.skills.model_invoked().is_empty()));
        specs.retain(|s| self.layers.decided_tool(&s.name) == crate::Decided::Allowed);
        specs.sort_by(|a, b| a.name.cmp(&b.name));
        specs
    }

    /// Whether the layers let the model see this tool.
    pub fn tool_visible(&self, name: &str) -> bool {
        self.layers.decided_tool(name) == crate::Decided::Allowed
    }
}
