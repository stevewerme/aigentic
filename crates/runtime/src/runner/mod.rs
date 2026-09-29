//! The build runner: one lead thread drives a workflow's steps.
//!
//! **Scope.** A runner that is code drives a lead thread through: brief →
//! `size` route → implement-alone, up to the implementer's report. Each move
//! is written to the lead log *before* it is acted on, so a runner rebuilt
//! over the same logs does the next move once and never repeats one. The
//! checks, push, install and close are #65's; the daemon wiring is #58's.
//! Nothing here knows about `core`'s types beyond what a log line holds.

pub mod forge;
pub mod host;

pub use forge::{FakeForge, Forge, ForgeError, GhForge, IssueView};
pub use host::RunnerHost;

use aigentic_log::LogError;
use ulid::Ulid;

use crate::RuntimeError;

/// Every way the runner can fail.
#[derive(Debug, thiserror::Error)]
pub enum RunnerError {
    /// The lead log could not be read or written.
    #[error("log: {0}")]
    Log(#[from] LogError),
    /// A child's turn failed.
    #[error("turn: {0}")]
    Turn(#[from] RuntimeError),
    /// The workflow could not be read, rendered or looked up.
    #[error("workflow: {0}")]
    Workflow(#[from] crate::workflow::WorkflowError),
    /// A step's deny list is malformed.
    #[error("deny list: {0}")]
    Deny(#[from] aigentic_policy::DenyParseError),
    /// The forge could not be read or posted to.
    #[error("forge: {0}")]
    Forge(#[from] ForgeError),
    /// The host refused to do something, or has nothing to do it with.
    #[error("host: {0}")]
    Host(String),
    /// The thread is not a run: the daemon writes `run_started` first.
    #[error("this thread is not a run: the daemon writes `run_started` first")]
    NotARun,
    /// The log belongs to another thread than the one the caller named.
    #[error("the log is thread {log}, not the lead {lead}")]
    ThreadMismatch {
        /// The id the log carries.
        log: Ulid,
        /// The id the caller passed.
        lead: Ulid,
    },
    /// A move that belongs to the checks, push and close step.
    #[error("`{what}` is owned by #65")]
    OwnedBy65 {
        /// The move's name, for the message.
        what: &'static str,
    },
    /// A message was posted into the child and the child's log does not
    /// hold it, so the turn never started.
    #[error("the child's log does not hold the message just posted")]
    MessageNotPosted,
}
