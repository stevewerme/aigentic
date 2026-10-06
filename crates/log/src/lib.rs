//! Append-only JSONL event store: one file per thread, one event per line.
//!
//! The log is the source of truth. [`ThreadLog::append`] assigns ULID ids
//! and a gapless `seq`; [`ThreadLog::open`] reads and validates the file
//! once, and [`ThreadLog::read_all`] then serves the events the log holds
//! without touching the disk again; [`project`] turns events into the
//! canonical messages a provider sees, with compaction applied. Resume is
//! `read_all` followed by `project`; a torn tail after a crash can be cut
//! with [`ThreadLog::open_with`].

mod decision_log;
mod payload;
mod projection;
mod recall;
mod run_state;
mod store;

pub use decision_log::{
    DecisionFold, DecisionRecord, RecordedAnswer, decision_records, declined_at_startup,
};
pub use payload::{
    AssistantMessagePayload, BudgetScope, BudgetWarnedPayload, CheckOutcome, CheckResult,
    CheckpointAnswer, CheckpointAnsweredPayload, CheckpointAskedPayload, ChecksRunPayload,
    CommitRef, CompactedPayload, CompactionStrategy, ContextEvictedPayload,
    ContextSaturatedPayload, DecisionAnswer, DecisionAnsweredPayload, DecisionKind,
    DecisionProposedPayload, DecisionScope, DecisionStage, Finding, FindingMark, FixSize, Handoff,
    InterruptedPayload, Invoker, LedgerEntry, LedgerStatus, MemoryExtractedPayload, MemoryLine,
    MemoryRememberedPayload, PermissionDecidedPayload, PermissionRequestedPayload, PinnedPayload,
    PlannedTest, PolicyRecord, ProjectSwitchedPayload, ProviderRetriedPayload, PushedPayload,
    ReleaseImpact, ReportStatus, ResultsStubbedPayload, RevealMark, Route, RouteTakenPayload,
    RunFinishedPayload, RunOutcome, RunStartedPayload, STARTUP_PREFIX, SkillLoadedPayload,
    StepFinishedPayload, StepReport, StepStartedPayload, StepStatus, ThreadRenamedPayload,
    ThreadStartedPayload, ToolResultPayload, TurnEndedPayload, Usage, UserMessagePayload, Verdict,
    is_startup_call,
};
pub use projection::{
    Projection, STUB_ARG_MAX_CHARS, project, project_body, shorten_call_args, skill_marker,
    summary_marker, truncate_middle,
};
pub use recall::{
    Hit, SNIPPET_MAX_CHARS, call_index, call_of, kind_name, project_at, render_range, search,
    short_args,
};
pub use run_state::{ChecksOutcome, NextMove, RunState, StepRecord, run_state};
pub use store::{LogError, NewEvent, Repair, ThreadLog};
