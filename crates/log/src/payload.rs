//! Kind-specific payload shapes. `Event.payload` is free JSON on the wire;
//! these structs are the contract for what each phase-0 kind carries.

use aigentic_core::{ContentBlock, ToolResult};
use serde::{Deserialize, Serialize};

/// Payload of a `user_message` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserMessagePayload {
    pub blocks: Vec<ContentBlock>,
}

/// Token usage for one model call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// `true` when the numbers came from `Provider::count_tokens` because
    /// the provider reported no usage. `/cost` shows that share separately.
    #[serde(default)]
    pub estimated: bool,
}

/// Payload of an `assistant_message` event. Tool calls live in `blocks`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssistantMessagePayload {
    pub blocks: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

/// Payload of a `tool_result` event; `parent_event` points at the
/// `assistant_message` that made the call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ToolResultPayload(pub ToolResult);

/// Payload of a `turn_ended` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnEndedPayload {
    /// `"done"`, or the budget that was hit.
    pub reason: String,
}
