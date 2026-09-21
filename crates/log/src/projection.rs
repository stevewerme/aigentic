use aigentic_core::{ContentBlock, Event, EventKind, Message, Role};

use crate::payload::{AssistantMessagePayload, ToolResultPayload, UserMessagePayload};
use crate::store::LogError;

/// Project events into the canonical messages a provider sees, oldest first.
///
/// `user_message` and `assistant_message` become one message each;
/// `tool_result` becomes a `Role::Tool` message holding one `ToolResult`
/// block (adapters decide how to group them); `turn_ended` emits nothing.
pub fn project(events: &[Event]) -> Result<Vec<Message>, LogError> {
    let mut messages = Vec::with_capacity(events.len());
    for event in events {
        let payload = |err: serde_json::Error| LogError::Payload {
            seq: event.seq,
            kind: event.kind,
            source: err,
        };
        match event.kind {
            EventKind::UserMessage => {
                let p: UserMessagePayload =
                    serde_json::from_value(event.payload.clone()).map_err(payload)?;
                messages.push(Message {
                    role: Role::User,
                    author: event.author.clone(),
                    blocks: p.blocks,
                });
            }
            EventKind::AssistantMessage => {
                let p: AssistantMessagePayload =
                    serde_json::from_value(event.payload.clone()).map_err(payload)?;
                messages.push(Message {
                    role: Role::Assistant,
                    author: event.author.clone(),
                    blocks: p.blocks,
                });
            }
            EventKind::ToolResult => {
                let p: ToolResultPayload =
                    serde_json::from_value(event.payload.clone()).map_err(payload)?;
                messages.push(Message {
                    role: Role::Tool,
                    author: event.author.clone(),
                    blocks: vec![ContentBlock::ToolResult(p.0)],
                });
            }
            // Given meaning by the compaction-aware projection (phase 2 step 2).
            EventKind::TurnEnded
            | EventKind::Compacted
            | EventKind::Pinned
            | EventKind::Interrupted => {}
        }
    }
    Ok(messages)
}
