//! Helpers the loop calls; none of them is the loop.

use std::time::Instant;

use aigentic_core::{Author, ContentBlock, Event, EventKind, Message, Role};
use aigentic_log::{NewEvent, TurnEndedPayload, Usage};

use crate::{Runtime, RuntimeError, Signal, TurnOutcome};

/// What a turn has used so far, checked against the `Budget`.
pub(crate) struct Spent {
    pub(crate) started: Instant,
    /// Time parked on a human (a permission prompt, an `ask_human`),
    /// which the wall-time budget does not count: a turn that waited
    /// twenty minutes for approvals has not worked for twenty minutes.
    pub(crate) waited: std::time::Duration,
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
        Usage::estimated(aigentic_core::Usage {
            input_tokens: self.provider.count_tokens(context),
            output_tokens: self.provider.count_tokens(std::slice::from_ref(&produced)),
            ..Default::default()
        })
    }

    pub(crate) fn budget_reason(&self, spent: &Spent) -> Option<&'static str> {
        if spent.iterations >= self.budget.max_iterations {
            Some("max_iterations")
        } else if spent.tokens >= self.budget.max_tokens {
            Some("max_tokens")
        } else if spent.started.elapsed().saturating_sub(spent.waited) >= self.budget.max_wall_time
        {
            Some("max_wall_time")
        } else {
            None
        }
    }

    pub(crate) fn end_turn(
        &mut self,
        reason: &str,
        spent: &Spent,
        observe: &mut (dyn FnMut(Signal<'_>) + Send),
    ) -> Result<TurnOutcome, RuntimeError> {
        let touched = self.touched_this_turn()?;
        let payload = serde_json::to_value(TurnEndedPayload {
            reason: reason.to_owned(),
            touched: touched.clone(),
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
            touched,
        })
    }

    /// Paths of `write_file` and `edit_file` calls since the last
    /// `turn_ended` whose result is not an error, in order, each once.
    pub(crate) fn touched_this_turn(&self) -> Result<Vec<String>, RuntimeError> {
        let events = self.log.read_all()?;
        let start = events
            .iter()
            .rposition(|e| e.kind == EventKind::TurnEnded)
            .map_or(0, |i| i + 1);
        let mut pending: Vec<(String, String)> = Vec::new();
        let mut touched: Vec<String> = Vec::new();
        for e in &events[start..] {
            match e.kind {
                EventKind::AssistantMessage => {
                    let Ok(p) = serde_json::from_value::<aigentic_log::AssistantMessagePayload>(
                        e.payload.clone(),
                    ) else {
                        continue;
                    };
                    for b in p.blocks {
                        if let ContentBlock::ToolCall(c) = b
                            && (c.name == "write_file" || c.name == "edit_file")
                            && let Some(path) = c.args.get("path").and_then(|v| v.as_str())
                        {
                            pending.push((c.id, path.to_owned()));
                        }
                    }
                }
                EventKind::ToolResult => {
                    let Ok(p) = serde_json::from_value::<aigentic_log::ToolResultPayload>(
                        e.payload.clone(),
                    ) else {
                        continue;
                    };
                    if p.result.is_error {
                        continue;
                    }
                    if let Some(i) = pending.iter().position(|(id, _)| *id == p.result.id) {
                        let (_, path) = pending.remove(i);
                        if !touched.contains(&path) {
                            touched.push(path);
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(touched)
    }

    pub(crate) fn append(
        &mut self,
        kind: EventKind,
        author: Author,
        payload: serde_json::Value,
        parent_event: Option<ulid::Ulid>,
        observe: &mut (dyn FnMut(Signal<'_>) + Send),
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
