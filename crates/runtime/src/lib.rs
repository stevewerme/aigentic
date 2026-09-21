//! The agent loop. Build context from the log, call the provider, run tool
//! calls in order, append results, repeat until the model stops calling
//! tools or the budget is hit. Every outcome is an event; the log is the
//! only state.
//!
//! Phase 0 leaves three named seams that currently pass through, see
//! [`seams`]: the policy check, the turn queue and compaction.

mod context;
mod error;
mod instructions;
mod runtime;
pub mod seams;
mod support;
mod turn;

pub use context::build_context;
pub use error::RuntimeError;
pub use instructions::load_instructions;
pub use runtime::{DEFAULT_BUDGET, Runtime, Signal, TurnOutcome};
