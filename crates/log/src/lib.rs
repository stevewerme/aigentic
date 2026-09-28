//! Append-only JSONL event store: one file per thread, one event per line.
//!
//! The log is the source of truth. [`ThreadLog::append`] assigns ULID ids
//! and a gapless `seq`; [`ThreadLog::open`] reads and validates the file
//! once, and [`ThreadLog::read_all`] then serves the events the log holds
//! without touching the disk again; [`project`] turns events into the
//! canonical messages a provider sees, with compaction applied. Resume is
//! `read_all` followed by `project`; a torn tail after a crash can be cut
//! with [`ThreadLog::open_with`].

mod payload;
mod projection;
mod store;

pub use payload::{
    AssistantMessagePayload, CompactedPayload, CompactionStrategy, ContextEvictedPayload,
    ContextSaturatedPayload, DecisionScope, InterruptedPayload, Invoker, MemoryExtractedPayload,
    MemoryLine, MemoryRememberedPayload, PermissionDecidedPayload, PermissionRequestedPayload,
    PinnedPayload, PolicyRecord, ProjectSwitchedPayload, ProviderRetriedPayload,
    SkillLoadedPayload, ThreadRenamedPayload, ThreadStartedPayload, ToolResultPayload,
    TurnEndedPayload, Usage, UserMessagePayload,
};
pub use projection::{
    Projection, STUB_ARG_MAX_CHARS, project, project_body, shorten_call_args, skill_marker,
    summary_marker, truncate_middle,
};
pub use store::{LogError, NewEvent, Repair, ThreadLog};
