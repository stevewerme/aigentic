//! The agent loop. Build context from the log, call the provider, run tool
//! calls in order, append results, repeat until the model stops calling
//! tools or the budget is hit. Every outcome is an event; the log is the
//! only state.
//!
//! Phase 0 leaves three named seams that currently pass through, see
//! [`seams`]: the policy check, the turn queue and compaction.

// Re-exported so a client can build a `Runtime` while depending on this
// crate alone, per the dependency rule.
pub use aigentic_core;
pub use aigentic_log;
pub use aigentic_providers;
pub use aigentic_tools;

mod compaction;
mod context;
mod error;
mod instructions;
mod resume;
mod runtime;
pub mod seams;
mod support;
mod turn;

pub use compaction::SUMMARY_PROMPT;
pub use context::build_context;
pub use error::RuntimeError;
pub use instructions::load_instructions;
pub use resume::{INTERRUPTED_RESULT, Resumed};
pub use runtime::{
    CompactionSettings, DEFAULT_BUDGET, DEFAULT_COMPACTION, Runtime, Signal, TurnOutcome,
};
