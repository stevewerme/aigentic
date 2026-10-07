//! The loop. Kept small on purpose; behaviour that is not the loop lives
//! behind the seams in `seams.rs`.

use std::time::Instant;

use aigentic_core::{
    Author, CUT_STREAM, CompletionRequest, ContentBlock, EventKind, Message, ProviderError,
    ProviderEvent, Role, ToolCall, ToolResult, ToolSpec,
};
use aigentic_log::{
    AssistantMessagePayload, InterruptedPayload, ProviderRetriedPayload, ToolResultPayload, Usage,
    UserMessagePayload,
};
use futures_util::StreamExt;

use aigentic_log::{Invoker, NewEvent, PolicyRecord};

use crate::decisions::{CancelToken, Inbox, Queued};
use crate::harness_tools::{
    ASK_HUMAN, FINISH_STEP, HARNESS_CLASS, NOT_RUN_LENGTH, NOT_RUN_OVER_LIMIT, NOT_RUN_SIBLING,
    NOT_RUN_SOLO, SUGGEST_PROJECT, harness_specs, is_harness_tool, stale_tasks_reminder,
};
use crate::runtime::{ASKED_HUMAN, INTERRUPTED, LENGTH_STOP, MAX_TOKENS_STOP, STEP_REPORTED};
use crate::seams::{Verdict, author_name, denial_text};
use crate::support::{Spent, append_queued, block_start, flush_text};
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
            append_queued(
                &*self.provider,
                &mut self.log,
                &mut self.thread_raw,
                queued,
                observe,
                true,
            )?;
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
        // The turn's two starts, read through the clock seam so a test
        // can script a sleep (issue #47). Wall time keeps moving while
        // the machine is asleep; the monotonic clock counts only uptime.
        let (started, wall_started) = (self.clock)();
        let mut spent = Spent {
            started,
            wall_started,
            waited: std::time::Duration::ZERO,
            wall_waited: std::time::Duration::ZERO,
            iterations: 0,
            tokens: 0,
        };
        self.refresh_knowledge()?;
        self.refresh_memory()?;
        let specs: Vec<ToolSpec> = self.tool_specs();
        // What the request's schemas cost on the wire, at the estimator's
        // own rate, read here so a registry change between turns is
        // picked up (issue #52). The model that runs this turn is the one
        // whose reported counts teach `self.ratio` below.
        self.overhead = crate::evict::schemas_tokens(&specs);
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
                return self.end_turn(reason, None, &spent, &mut held, observe);
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
                append_queued(
                    &*self.provider,
                    &mut self.log,
                    &mut self.thread_raw,
                    queued,
                    observe,
                    true,
                )?;
            }
            // Compaction, at an iteration boundary only: every tool call
            // already has its result, so no summary range splits a turn.
            // Eviction runs at the same boundary, and first: stubbing is
            // free where a summary costs a call (issue #30).
            //
            // The closed-turn batch comes before both, and only at a
            // turn's first iteration (issue #76). A batch iteration does
            // nothing else except its link (issue #98): the batch has
            // already rewritten the history, so a sweep or a nested
            // summary in the same iteration would either price the stale
            // one or append a second cache break for no reason. The
            // batch's own continuous summary carries the same break, so
            // it rides along in the same iteration, before the first
            // model call.
            if self.at_turn_start() {
                match self.batch_stale(observe)? {
                    Some(Some((from, to))) => {
                        // Best effort and never fatal: only the log
                        // append inside can fail the turn, nothing else
                        // can (issue #98, design 3).
                        self.summarise_link(from, to, cancel, observe).await?;
                    }
                    Some(None) => {}
                    None => {
                        self.evict_stale(observe)?;
                    }
                }
            } else {
                self.evict_stale(observe)?;
            }
            // The emergency net (issue #98, design 5), in every
            // iteration and outside the batch's arm, so a batch whose
            // link was skipped is still covered once the context nears
            // the model's real window. Its error still ends the turn, as
            // compaction's always did.
            if let Err(e) = self.emergency_compact(observe).await {
                if let RuntimeError::Provider(p) = &e {
                    self.end_turn(
                        &format!("provider_error: {p}"),
                        Some(p.clone()),
                        &spent,
                        &mut held,
                        observe,
                    )?;
                }
                return Err(e);
            }
            let mut context = build_context(&self.prefix(), self.log.events())?;
            // The stale-checklist reminder (issue #102) rides at the very
            // end of this call's context and nowhere else: no event is
            // appended, so the log, the projection, replay and the cached
            // prefix are unchanged, and the next call's context is this
            // one without the reminder, which is what keeps it cached.
            // `Role::User` with `Author::System`, because Anthropic keeps
            // only the leading system run as the system prompt and would
            // drop a later one.
            //
            // The reminder stays part of `context` for `measured`, the
            // ratio, `window_usage` and the no-usage fallback
            // `estimate_usage` below, so all of them price what was
            // actually sent: a call whose provider reports no usage
            // estimates about 30-40 tokens more while the reminder is
            // present, and the eviction probes, which call `build_context`
            // directly, are a few tokens off. Both are accepted.
            if let Some(text) = stale_tasks_reminder(self.log.events()) {
                context.push(Message {
                    role: Role::User,
                    author: Author::System,
                    blocks: vec![ContentBlock::Text(text)],
                });
            }
            let request = CompletionRequest {
                messages: &context,
                tools: &specs,
                max_output_tokens: None,
            };

            let (mut blocks, mut text, mut usage, mut error) =
                (Vec::new(), String::new(), None, None);
            // The provider's own reason the reply ended (issue #96): the
            // difference between a reply that finished and one that was
            // cut off or ran out of output tokens.
            let mut finish_reason: Option<String> = None;
            // Issue #31: the call's waiting time, stamped on its usage
            // line. `ttft_ms` is the first streamed block, so a slow
            // model (thinking before any delta) reads separately from a
            // slow transport.
            let requested = Instant::now();
            let mut ttft_ms = None;
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
                if ttft_ms.is_none() && block_start(&event) {
                    ttft_ms = Some(requested.elapsed().as_millis() as u64);
                }
                match event {
                    // Live retries (issue #31): appended as they arrive,
                    // so the log and every client see the wait as it
                    // happens rather than after the call recovers.
                    ProviderEvent::Retried {
                        attempt,
                        retries,
                        reason,
                        wait,
                    } => {
                        let payload = serde_json::to_value(ProviderRetriedPayload {
                            attempt,
                            retries,
                            reason: self.retry_reason(&reason),
                            wait_ms: wait.as_millis() as u64,
                        })
                        .expect("serialisable");
                        let event = self.log.append(NewEvent {
                            kind: EventKind::ProviderRetried,
                            author: Author::System,
                            payload,
                            parent_event: None,
                        })?;
                        observe(Signal::Event(&event));
                    }
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
                    ProviderEvent::Done {
                        finish_reason: reason,
                    } => {
                        // A stream that ended without a reason was cut
                        // off mid-reply (issue #96): the text may stop
                        // mid-sentence and a tool call mid-arguments, so
                        // nothing about it can be trusted — the existing
                        // error path below ends the turn, writes no
                        // assistant message and runs no call.
                        if reason == CUT_STREAM {
                            error = Some(ProviderError::Cut);
                            break;
                        }
                        finish_reason = Some(reason);
                    }
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
            // The one place both numbers exist for the same context
            // (issue #52): what the provider counted against what the
            // estimator makes of the same messages. Memory extraction and
            // titles run on the utility provider and never reach here, so
            // only the thread's own model teaches the ratio.
            let prompt = usage
                .as_ref()
                .map(|u| u.input_tokens + u.cache_read_tokens + u.cache_write_tokens);
            self.measured = prompt.map(|p| (p, context.len()));
            if let Some(reported) = prompt {
                let estimate = self.provider.count_tokens(&context);
                self.ratio =
                    crate::evict::next_ratio(self.ratio, reported, estimate, self.overhead);
            }
            observe(Signal::Usage(self.window_usage(&context)));
            let mut usage = usage.unwrap_or_else(|| self.estimate_usage(&context, &agent, &blocks));
            usage.profile = self.profile.clone();
            usage.model = Some(self.model_label.clone());
            usage.effort = self.effort.clone();
            usage.latency_ms = Some(requested.elapsed().as_millis() as u64);
            usage.ttft_ms = ttft_ms;
            usage.cost_usd = self.prices.map(|p| p.cost_usd(&usage));
            spent.tokens += self.budget.spent_of(&usage.to_core());
            if let Some(e) = error {
                self.end_turn(
                    &format!("provider_error: {e}"),
                    Some(e.clone()),
                    &spent,
                    &mut held,
                    observe,
                )?;
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
                finish_reason: finish_reason.clone(),
            })
            .expect("serialisable");
            let assistant =
                self.append(EventKind::AssistantMessage, agent, payload, None, observe)?;
            // A reply that hit the model's output limit was cut short by
            // the model itself (issue #96): its calls were written by a
            // reply that could not finish thinking, so none runs. Every
            // call still gets a result — the invariant is that no call in
            // the log is left unanswered — and the turn ends `length`.
            let length_stop = finish_reason
                .as_deref()
                .is_some_and(|r| r == LENGTH_STOP || r == MAX_TOKENS_STOP);
            if length_stop && !calls.is_empty() {
                for call in &calls {
                    let result = ToolResult {
                        id: call.id.clone(),
                        content: NOT_RUN_LENGTH.to_owned(),
                        is_error: true,
                    };
                    let payload = serde_json::to_value(ToolResultPayload::new(
                        result,
                        PolicyRecord::rule(NOT_RUN_OVER_LIMIT, "deny"),
                    ))
                    .expect("serialisable");
                    self.append(
                        EventKind::ToolResult,
                        Author::System,
                        payload,
                        Some(assistant.id),
                        observe,
                    )?;
                }
                return self.end_turn(LENGTH_STOP, None, &spent, &mut held, observe);
            }
            if calls.is_empty() {
                return self.end_turn(
                    if length_stop { LENGTH_STOP } else { "done" },
                    None,
                    &spent,
                    &mut held,
                    observe,
                );
            }

            let mut answered = false;
            // A step's report also ends the turn at once (issue #55): the
            // runner reads the report, not the rest of the batch.
            let mut reported = false;
            let mut calls = calls.into_iter();
            while let Some(call) = calls.next() {
                self.drain_inbox(inbox, observe)?;
                observe(Signal::ToolCallStarted(&call));
                let (result, record, by) = self
                    .execute(
                        &call,
                        cancel,
                        inbox,
                        observe,
                        &mut spent.waited,
                        &mut spent.wall_waited,
                    )
                    .await?;
                answered |=
                    (call.name == ASK_HUMAN || call.name == SUGGEST_PROJECT) && !result.is_error;
                reported |= call.name == FINISH_STEP && !result.is_error;
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
                if call.name == SUGGEST_PROJECT {
                    // A proposal stands alone (issue #7): a call after it
                    // in the same batch was written before the person had
                    // answered, and nothing may run in a project they have
                    // not answered for yet. The results are written the
                    // way the interrupt path writes its own. Calls before
                    // it have already run; the instructions say to call it
                    // first.
                    for rest in calls.by_ref() {
                        let result = ToolResult {
                            id: rest.id.clone(),
                            content: NOT_RUN_SIBLING.to_owned(),
                            is_error: true,
                        };
                        let payload = serde_json::to_value(ToolResultPayload::new(
                            result,
                            PolicyRecord::rule(NOT_RUN_SOLO, "deny"),
                        ))
                        .expect("serialisable");
                        self.append(
                            EventKind::ToolResult,
                            Author::System,
                            payload,
                            Some(assistant.id),
                            observe,
                        )?;
                    }
                }
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
            // A step reporting is the step ending: the report is written, the
            // turn's reason says so, and the runner's `step_finished`
            // follows. Checked before `answered` because a report is the
            // step's own end, whatever else the batch held.
            if reported {
                return self.end_turn(STEP_REPORTED, None, &spent, &mut held, observe);
            }
            // A human's answer starts a turn: everything after it is new
            // work with its own budget. The client continues at once.
            if answered {
                return self.end_turn(ASKED_HUMAN, None, &spent, &mut held, observe);
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
                queued = inbox.recv() => append_queued(
                    &*self.provider, &mut self.log, &mut self.thread_raw, queued, observe, true,
                )?,
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
            append_queued(
                &*self.provider,
                &mut self.log,
                &mut self.thread_raw,
                queued,
                observe,
                false,
            )?;
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
        self.end_turn(INTERRUPTED, None, spent, held, observe)
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
        wall_waited: &mut std::time::Duration,
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
        // The policy check and the `ask_human` wait are timed on both
        // clocks: running time is what the wall-time budget discounts,
        // the wall reading is what tells a sleep inside the wait from
        // time actually parked on a human (issue #47).
        let (asked, asked_wall) = (self.clock)();
        let verdict = self.policy_check(call, class, cancel, observe).await?;
        *waited += asked.elapsed();
        *wall_waited += asked_wall.elapsed().unwrap_or_default();
        match verdict {
            Verdict::Run(record) => {
                let (result, by) = if is_harness_tool(&call.name) {
                    let (started, started_wall) = (self.clock)();
                    let ran = self.run_harness_tool(call, cancel, observe).await?;
                    if call.name == ASK_HUMAN || call.name == SUGGEST_PROJECT {
                        *waited += started.elapsed();
                        *wall_waited += started_wall.elapsed().unwrap_or_default();
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
        specs.extend(harness_specs(
            !self.skills.model_invoked().is_empty(),
            // Offered exactly in a step thread (issue #55).
            self.step.is_some(),
            // A proposal needs a person: offered in an ordinary thread,
            // never in a step thread, which nobody watches (issue #7).
            self.step.is_none(),
        ));
        specs.retain(|s| self.tool_visible(&s.name));
        specs.sort_by(|a, b| a.name.cmp(&b.name));
        specs
    }

    /// Whether the layers let the model see this tool. `finish_step` is
    /// answerable in any thread — the offered list cannot gate a call a
    /// model can always emit, so the arm decides (## Plan amendment 2
    /// item 3); its spec is offered in a step thread alone, whatever the
    /// project's allow list says.
    pub fn tool_visible(&self, name: &str) -> bool {
        if name == FINISH_STEP {
            return true;
        }
        self.layers.decided_tool(name) == crate::Decided::Allowed
    }
}
