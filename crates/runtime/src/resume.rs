//! Resume after a crash. The log is never edited: an interrupted turn is
//! repaired by appending events, then continued.

use aigentic_core::{Author, ContentBlock, Event, EventKind, ToolResult};
use aigentic_log::{
    AssistantMessagePayload, InterruptedPayload, PolicyRecord, ThreadLog, ToolResultPayload,
    TurnEndedPayload,
};

use crate::{Runtime, RuntimeError, Signal};

/// What `resume` found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resumed {
    /// The log ended on a turn boundary (or the lost `turn_ended` was
    /// appended). Nothing to continue.
    Clean,
    /// The log ended mid-turn. Repair events have been appended; the
    /// caller should `continue_turn`.
    Interrupted {
        /// Last event that was fully written before the crash.
        after_seq: u64,
        /// Tool calls given synthetic error results.
        unanswered_calls: usize,
        /// Bytes cut from a torn last line, if `open_with` repaired one.
        torn_bytes: Option<u64>,
    },
}

/// Content of a synthetic tool result written on resume.
pub const INTERRUPTED_RESULT: &str =
    "interrupted before a result was recorded; the outcome is unknown, rerun if needed";

impl Runtime {
    /// Call once after opening the log. Looks at the open turn, if any:
    ///
    /// | log ends with | repair |
    /// | --- | --- |
    /// | `turn_ended` | nothing; `Clean` |
    /// | `user_message` | `interrupted`; continue |
    /// | assistant with unanswered tool calls | synthetic error results, then `interrupted`; continue |
    /// | assistant without tool calls | append the lost `turn_ended`; `Clean` |
    /// | `tool_result` | `interrupted`; continue |
    /// | `interrupted` (a previous resume crashed too) | nothing new; continue |
    pub fn resume(
        &mut self,
        torn_bytes: Option<u64>,
        observe: &mut dyn FnMut(Signal<'_>),
    ) -> Result<Resumed, RuntimeError> {
        let events = self.log.read_all()?;
        let Some(open) = ThreadLog::open_turn(&events) else {
            return Ok(Resumed::Clean);
        };
        let after_seq = open.last().map_or(0, |e| e.seq);
        let last_turn_event = open
            .iter()
            .rev()
            .find(|e| {
                matches!(
                    e.kind,
                    EventKind::UserMessage | EventKind::AssistantMessage | EventKind::ToolResult
                )
            })
            .expect("open_turn guarantees a turn event");

        // A lost turn_ended after a final answer: the turn was complete.
        if last_turn_event.kind == EventKind::AssistantMessage
            && tool_call_ids(last_turn_event)?.is_empty()
        {
            let payload = serde_json::to_value(TurnEndedPayload {
                reason: "resumed".into(),
            })
            .expect("serialisable");
            self.append(
                EventKind::TurnEnded,
                Author::Agent(self.agent.clone()),
                payload,
                None,
                observe,
            )?;
            return Ok(Resumed::Clean);
        }

        // Answer every tool call in the open turn that has no result.
        let mut unanswered = Vec::new();
        if let Some((pos, assistant)) = open
            .iter()
            .enumerate()
            .rev()
            .find(|(_, e)| e.kind == EventKind::AssistantMessage)
        {
            let answered: Vec<String> = open[pos + 1..]
                .iter()
                .filter(|e| e.kind == EventKind::ToolResult)
                .filter_map(|e| {
                    serde_json::from_value::<ToolResultPayload>(e.payload.clone())
                        .ok()
                        .map(|p| p.result.id)
                })
                .collect();
            for id in tool_call_ids(assistant)? {
                if !answered.contains(&id) {
                    unanswered.push((id, assistant.id));
                }
            }
        }
        let already_noted = open
            .last()
            .is_some_and(|e| e.kind == EventKind::Interrupted);
        if unanswered.is_empty() && already_noted {
            return Ok(Resumed::Interrupted {
                after_seq,
                unanswered_calls: 0,
                torn_bytes,
            });
        }
        let mut ids = Vec::new();
        for (id, parent) in unanswered {
            let payload = serde_json::to_value(ToolResultPayload::new(
                ToolResult {
                    id: id.clone(),
                    content: INTERRUPTED_RESULT.into(),
                    is_error: true,
                },
                PolicyRecord::synthetic(),
            ))
            .expect("serialisable");
            self.append(
                EventKind::ToolResult,
                Author::System,
                payload,
                Some(parent),
                observe,
            )?;
            ids.push(id);
        }
        let payload = serde_json::to_value(InterruptedPayload {
            reason: "process exited mid-turn".into(),
            after_seq,
            unanswered_calls: ids.clone(),
        })
        .expect("serialisable");
        self.append(
            EventKind::Interrupted,
            Author::System,
            payload,
            None,
            observe,
        )?;
        Ok(Resumed::Interrupted {
            after_seq,
            unanswered_calls: ids.len(),
            torn_bytes,
        })
    }
}

fn tool_call_ids(assistant: &Event) -> Result<Vec<String>, RuntimeError> {
    let p: AssistantMessagePayload =
        serde_json::from_value(assistant.payload.clone()).map_err(|source| {
            aigentic_log::LogError::Payload {
                seq: assistant.seq,
                kind: assistant.kind,
                source,
            }
        })?;
    Ok(p.blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolCall(c) => Some(c.id.clone()),
            _ => None,
        })
        .collect())
}
