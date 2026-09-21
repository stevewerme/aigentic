//! Append-only JSONL event store: one file per thread, one event per line.
//!
//! The log is the source of truth. [`ThreadLog::append`] assigns ULID ids
//! and a gapless `seq`; [`ThreadLog::read_all`] replays and validates the
//! file; [`project`] turns events into the canonical messages a provider
//! sees. Resume is `read_all` followed by `project`.

mod payload;
mod projection;
mod store;

pub use payload::{
    AssistantMessagePayload, CompactedPayload, CompactionStrategy, InterruptedPayload,
    PinnedPayload, ToolResultPayload, TurnEndedPayload, Usage, UserMessagePayload,
};
pub use projection::project;
pub use store::{LogError, NewEvent, ThreadLog};
