//! The agent loop. Build context from the log, call the provider, run tool
//! calls in order, append results, repeat until the model stops calling
//! tools or the budget is hit. Every outcome is an event; the log is the
//! only state.
//!
//! Since phase 3 every tool call passes [`Runtime::policy_check`] and the
//! result carries the record; [`seams`] still holds the turn queue
//! pass-through for phase 5.

// Re-exported so a client can build a `Runtime` while depending on this
// crate alone, per the dependency rule.
pub use aigentic_core;
pub use aigentic_log;
pub use aigentic_policy;
pub use aigentic_providers;
pub use aigentic_skills;
pub use aigentic_tools;

mod approver;
mod audit;
pub mod brief;
pub mod checks;
mod compaction;
pub mod config_keys;
mod context;
mod decisions;
mod error;
pub mod evict;
pub mod harness_tools;
pub mod knowledge;
pub mod layers;
mod memory;
pub mod mode;
pub mod project;
pub mod recall_tool;
mod resume;
pub mod runner;
mod runtime;
pub mod seams;
mod support;
pub mod title;
mod turn;
pub mod workflow;

pub use aigentic_core::CUT_STREAM;
pub use approver::{Answer, Approver, DenyAll};
pub use audit::audit_tool_results;
pub use brief::{BRIEF_FILE, PROJECT_BRIEF_CAP, WORKSPACE_BRIEF_CAP};
pub use compaction::{LINK_PROMPT, SUMMARY_PROMPT};
pub use context::{Prefix, build_context};
pub use decisions::{
    Answered, CancelToken, DecisionError, Decisions, Inbox, Outbox, Pending, Queued, SwitchAnswer,
    SwitchCtx, inbox,
};
pub use error::RuntimeError;
pub use evict::{
    Decision, EVICT_BLOCK_CALLS, EVICT_MIN_FREE_PERCENT, RATIO_MAX, RATIO_MIN, RATIO_SMOOTHING,
    calibrated, calibrated_delta, min_free, next_ratio, schemas_tokens,
};
pub use harness_tools::{IdleProposal, NOT_RUN_LENGTH, NOT_RUN_OVER_LIMIT, NOT_RUN_SOLO, Settled};
pub use knowledge::{Knowledge, KnowledgeMode, ScopeSources};
pub use layers::{Decided, GlobalLayer, Layers, PERSON_MEMORY_HEADING, WorkspaceLayer};
pub use memory::{MEMORY_FILES, MEMORY_PROMPT, MEMORY_REQUEST};
pub use mode::Mode;
pub use project::{Project, ProjectError, ProjectFile};
pub use resume::{DAEMON_RESTARTED, INTERRUPTED_RESULT, Resumed};
pub use runtime::{
    ASKED_HUMAN, CompactionSettings, DEFAULT_BUDGET, DEFAULT_COMPACTION, INTERRUPTED, LENGTH_STOP,
    MAX_TOKENS_STOP, Prices, ProjectContext, ProjectRow, Runtime, STEP_REPORTED, SUMMARY_LIMIT,
    Signal, TurnOutcome, WindowUsage,
};
pub use seams::{SessionGrant, Verdict};
