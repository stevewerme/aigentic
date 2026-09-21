//! Append-only JSONL event store: one file per thread, one event per line.
//!
//! The log is the source of truth. [`ThreadLog::append`] assigns ULID ids
//! and a gapless `seq`; [`ThreadLog::read_all`] replays and validates the
//! file; [`project`] turns events into the canonical messages a provider
//! sees, with compaction applied. Resume is `read_all` followed by
//! `project`; a torn tail after a crash can be cut with
//! [`ThreadLog::open_with`].

mod payload;
mod projection;
mod store;

pub use payload::{
    AssistantMessagePayload, CompactedPayload, CompactionStrategy, DecisionScope,
    InterruptedPayload, Invoker, PermissionDecidedPayload, PermissionRequestedPayload,
    PinnedPayload, PolicyRecord, SkillLoadedPayload, ToolResultPayload, TurnEndedPayload, Usage,
    UserMessagePayload,
};
pub use projection::{
    Projection, project, project_body, skill_marker, summary_marker, truncate_middle,
};
pub use store::{LogError, NewEvent, Repair, ThreadLog};
