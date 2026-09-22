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

use crate::decisions::CancelToken;
use crate::harness_tools::{ASK_HUMAN, HARNESS_CLASS, harness_specs, is_harness_tool};
use crate::runtime::{ASKED_HUMAN, INTERRUPTED};
use crate::seams::{Verdict, denial_text};
use crate::support::{Spent, flush_text};
use crate::{Runtime, RuntimeError, Signal, TurnOutcome, build_context};

impl Runtime {
    /// Run one turn: append the user message, then iterate until the model
    /// returns no tool calls or the budget is hit. Every outcome, including
    /// a budget stop or a provider failure, ends with a `turn_ended` event.
    pub async fn run_turn(
        &mut self,
        author: Author,
        blocks: Vec<ContentBlock>,
        observe: &mut dyn FnMut(Signal<'_>),
    ) -> Result<TurnOutcome, RuntimeError> {
        self.run_turn_until(author, blocks, &CancelToken::never(), observe)
            .await
    }

    /// `run_turn` with an interrupt token (phase 5): the daemon's actor
    /// calls this so a `Post { interrupt: true }` can end the turn.
    pub async fn run_turn_until(
        &mut self,
        author: Author,
        blocks: Vec<ContentBlock>,
        cancel: &CancelToken,
        observe: &mut dyn FnMut(Signal<'_>),
    ) -> Result<TurnOutcome, RuntimeError> {
        let payload = serde_json::to_value(UserMessagePayload::new(blocks)).expect("serialisable");
        self.append(EventKind::UserMessage, author, payload, None, observe)?;
        self.continue_turn_until(cancel, observe).await
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
        observe: &mut dyn FnMut(Signal<'_>),
    ) -> Result<TurnOutcome, RuntimeError> {
        self.append_skill_loaded_as(name, Invoker::User, author.clone(), observe)?;
        let text = if args.trim().is_empty() {
            format!("Run the `{name}` skill now.")
        } else {
            args.trim().to_owned()
        };
        self.run_turn(author, vec![ContentBlock::Text(text)], observe)
            .await
    }

    /// The loop without a new user message: what `run_turn` does after the
    /// append, and what resume uses to finish an interrupted turn.
    pub async fn continue_turn(
        &mut self,
        observe: &mut dyn FnMut(Signal<'_>),
    ) -> Result<TurnOutcome, RuntimeError> {
        self.continue_turn_until(&CancelToken::never(), observe)
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
        observe: &mut dyn FnMut(Signal<'_>),
    ) -> Result<TurnOutcome, RuntimeError> {
        let mut spent = Spent {
            started: Instant::now(),
            iterations: 0,
            tokens: 0,
        };
        self.refresh_knowledge()?;
        self.refresh_memory()?;
        let specs: Vec<ToolSpec> = self.tool_specs();

        loop {
            if let Some(by) = cancel.cancelled_by() {
                return self.interrupt_turn(by, Vec::new(), &spent, observe);
            }
            // The budget is checked here, before a model call, and never
            // between an assistant message and its tool results. Once an
            // assistant message with tool calls is in the log, every call
            // must get a result: OpenAI-compatible endpoints reject a
            // conversation whose tool calls have no matching tool message,
            // so a log left in that state could not be resumed.
            if let Some(reason) = self.budget_reason(&spent) {
                return self.end_turn(reason, &spent, observe);
            }
            // Compaction, at an iteration boundary only: every tool call
            // already has its result, so no summary range splits a turn.
            if let Err(e) = self.compact(observe).await {
                if let RuntimeError::Provider(p) = &e {
                    self.end_turn(&format!("provider_error: {p}"), &spent, observe)?;
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
            if let Some(by) = interrupted_by {
                // Dropped mid-call: no partial message, as a crash would
                // leave none.
                return self.interrupt_turn(by, Vec::new(), &spent, observe);
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
            let usage = usage.unwrap_or_else(|| self.estimate_usage(&context, &agent, &blocks));
            spent.tokens += usage.total();
            if let Some(e) = error {
                self.end_turn(&format!("provider_error: {e}"), &spent, observe)?;
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
                return self.end_turn("done", &spent, observe);
            }

            let mut answered = false;
            let mut calls = calls.into_iter();
            for call in calls.by_ref() {
                observe(Signal::ToolCallStarted(&call));
                let (result, record) = self.execute(&call, cancel, observe).await?;
                answered |= call.name == ASK_HUMAN && !result.is_error;
                let payload = serde_json::to_value(ToolResultPayload::new(result, record))
                    .expect("serialisable");
                self.append(
                    EventKind::ToolResult,
                    Author::System,
                    payload,
                    Some(assistant.id),
                    observe,
                )?;
                if cancel.cancelled_by().is_some() {
                    break;
                }
            }
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
                return self.interrupt_turn(by, synthetic, &spent, observe);
            }
            // A human's answer starts a turn: everything after it is new
            // work with its own budget. The client continues at once.
            if answered {
                return self.end_turn(ASKED_HUMAN, &spent, observe);
            }
        }
    }

    /// `interrupted` naming who, then `turn_ended` with reason
    /// `interrupted`. `unanswered` lists the calls given synthetic results.
    fn interrupt_turn(
        &mut self,
        by: Author,
        unanswered: Vec<String>,
        spent: &Spent,
        observe: &mut dyn FnMut(Signal<'_>),
    ) -> Result<TurnOutcome, RuntimeError> {
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
        self.end_turn(INTERRUPTED, spent, observe)
    }

    /// The one call site for tool execution, behind `policy_check`. An
    /// unknown tool is refused with a rule record; a harness tool is
    /// answered here; anything else runs from the registry.
    async fn execute(
        &mut self,
        call: &ToolCall,
        cancel: &CancelToken,
        observe: &mut dyn FnMut(Signal<'_>),
    ) -> Result<(ToolResult, PolicyRecord), RuntimeError> {
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
            ));
        };
        match self.policy_check(call, class, cancel, observe).await? {
            Verdict::Run(record) => {
                let result = if is_harness_tool(&call.name) {
                    self.run_harness_tool(call, cancel, observe).await?
                } else {
                    self.run_tool(call).await
                };
                Ok((result, record))
            }
            Verdict::Refuse(record) => Ok((
                ToolResult {
                    id: call.id.clone(),
                    content: denial_text(&record),
                    is_error: true,
                },
                record,
            )),
            Verdict::RefuseWith { record, text } => Ok((
                ToolResult {
                    id: call.id.clone(),
                    content: text,
                    is_error: true,
                },
                record,
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
