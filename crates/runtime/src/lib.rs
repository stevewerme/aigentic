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
mod compaction;
mod context;
mod error;
pub mod harness_tools;
pub mod knowledge;
pub mod layers;
mod memory;
pub mod project;
mod resume;
mod runtime;
pub mod seams;
mod support;
mod turn;

pub use approver::{Answer, Approver, DenyAll};
pub use audit::audit_tool_results;
pub use compaction::SUMMARY_PROMPT;
pub use context::{Prefix, build_context};
pub use error::RuntimeError;
pub use knowledge::{Knowledge, KnowledgeMode};
pub use layers::{Decided, GlobalLayer, Layers};
pub use memory::{MEMORY_FILES, MEMORY_PROMPT};
pub use project::{Project, ProjectError, ProjectFile};
pub use resume::{INTERRUPTED_RESULT, Resumed};
pub use runtime::{
    CompactionSettings, DEFAULT_BUDGET, DEFAULT_COMPACTION, Runtime, Signal, TurnOutcome,
};
pub use seams::Verdict;
