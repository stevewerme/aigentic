//! Kind-specific payload shapes. `Event.payload` is free JSON on the wire;
//! these structs are the contract for what each phase-0 kind carries.

use aigentic_core::{ContentBlock, ToolResult};
use serde::{Deserialize, Serialize};

/// Payload of a `user_message` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserMessagePayload {
    pub blocks: Vec<ContentBlock>,
}

/// Token usage for one model call, as persisted. The token fields mirror
/// `aigentic_core::Usage`; older log lines without the cache fields read
/// back as zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
    #[serde(default)]
    pub reasoning_tokens: Option<u64>,
    /// `true` when the numbers came from `Provider::count_tokens` because
    /// the provider reported no usage. `/cost` shows that share separately.
    #[serde(default)]
    pub estimated: bool,
}

impl Usage {
    /// Persist what the provider reported.
    pub fn reported(u: aigentic_core::Usage) -> Self {
        Self::from_core(u, false)
    }

    /// Persist an estimate made by the runtime.
    pub fn estimated(u: aigentic_core::Usage) -> Self {
        Self::from_core(u, true)
    }

    fn from_core(u: aigentic_core::Usage, estimated: bool) -> Self {
        Self {
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
            cache_read_tokens: u.cache_read_tokens,
            cache_write_tokens: u.cache_write_tokens,
            reasoning_tokens: u.reasoning_tokens,
            estimated,
        }
    }

    pub fn total(&self) -> u64 {
        self.input_tokens + self.cache_read_tokens + self.cache_write_tokens + self.output_tokens
    }
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
    /// `"done"`, `"resumed"`, the budget that was hit, or `provider_error: ...`.
    pub reason: String,
}

/// What a `compacted` event does to its range in the projection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CompactionStrategy {
    /// Tool results in the range are shortened to head and tail. Reclaims
    /// most of a full context and costs no model call.
    TruncateResults { max_bytes: usize },
    /// Events in the range are replaced by one summary message.
    Summary {
        text: String,
        model: String,
        usage: Usage,
    },
}

/// Payload of a `compacted` event. Originals stay in the log; only the
/// projection changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactedPayload {
    /// Inclusive.
    pub from_seq: u64,
    /// Inclusive; always a turn boundary.
    pub to_seq: u64,
    pub strategy: CompactionStrategy,
}

/// Payload of a `pinned` event; the event's author is who pinned it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinnedPayload {
    pub text: String,
}

/// Payload of an `interrupted` event, appended on resume after a crash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterruptedPayload {
    pub reason: String,
    /// Last event that was fully written before the crash.
    pub after_seq: u64,
    /// Tool call ids that received synthetic error results.
    #[serde(default)]
    pub unanswered_calls: Vec<String>,
}
