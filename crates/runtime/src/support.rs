//! Helpers the loop calls; none of them is the loop.

use std::time::Instant;

use aigentic_core::{Author, ContentBlock, Event, EventKind, Message, Role, ToolCall, ToolResult};
use aigentic_log::{NewEvent, TurnEndedPayload, Usage};

use crate::{Runtime, RuntimeError, Signal, TurnOutcome};

/// What a turn has used so far, checked against the `Budget`.
pub(crate) struct Spent {
    pub(crate) started: Instant,
    pub(crate) iterations: u32,
    pub(crate) tokens: u64,
}

impl Runtime {
    /// Usage for a call whose provider reported none: `count_tokens` over
    /// the context for input and over the produced blocks for output,
    /// flagged `estimated`.
    pub(crate) fn estimate_usage(
        &self,
        context: &[Message],
        agent: &Author,
        blocks: &[ContentBlock],
    ) -> Usage {
        let produced = Message {
            role: Role::Assistant,
            author: agent.clone(),
            blocks: blocks.to_vec(),
        };
        Usage {
            input_tokens: self.provider.count_tokens(context),
            output_tokens: self.provider.count_tokens(std::slice::from_ref(&produced)),
            estimated: true,
        }
    }

    pub(crate) async fn run_tool(&self, call: &ToolCall) -> ToolResult {
        let id = call.id.clone();
        let Some(tool) = self.tools.iter().find(|t| t.name() == call.name) else {
            return ToolResult {
                id,
                content: format!("unknown tool: {}", call.name),
                is_error: true,
            };
        };
        match tool.call(call.args.clone()).await {
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
        }
    }

    pub(crate) fn budget_reason(&self, spent: &Spent) -> Option<&'static str> {
        if spent.iterations >= self.budget.max_iterations {
            Some("max_iterations")
        } else if spent.tokens >= self.budget.max_tokens {
            Some("max_tokens")
        } else if spent.started.elapsed() >= self.budget.max_wall_time {
            Some("max_wall_time")
        } else {
            None
        }
    }

    pub(crate) fn end_turn(
        &mut self,
        reason: &str,
        spent: &Spent,
        observe: &mut dyn FnMut(Signal<'_>),
    ) -> Result<TurnOutcome, RuntimeError> {
        let payload = serde_json::to_value(TurnEndedPayload {
            reason: reason.to_owned(),
        })
        .expect("serialisable");
        self.append(
            EventKind::TurnEnded,
            Author::Agent(self.agent.clone()),
            payload,
            None,
            observe,
        )?;
        Ok(TurnOutcome {
            reason: reason.to_owned(),
            iterations: spent.iterations,
            tokens: spent.tokens,
            elapsed: spent.started.elapsed(),
        })
    }

    pub(crate) fn append(
        &mut self,
        kind: EventKind,
        author: Author,
        payload: serde_json::Value,
        parent_event: Option<ulid::Ulid>,
        observe: &mut dyn FnMut(Signal<'_>),
    ) -> Result<Event, RuntimeError> {
        let event = self.log.append(NewEvent {
            kind,
            author,
            payload,
            parent_event,
        })?;
        observe(Signal::Event(&event));
        Ok(event)
    }
}

pub(crate) fn flush_text(text: &mut String, blocks: &mut Vec<ContentBlock>) {
    if !text.is_empty() {
        blocks.push(ContentBlock::Text(std::mem::take(text)));
    }
}
