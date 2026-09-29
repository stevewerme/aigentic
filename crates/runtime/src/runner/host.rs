//! The seam between the runner and the world it drives: child thread ids,
//! child logs and the child's [`Runtime`].
//!
//! A host is built for one lead thread, so it knows the parent it writes on
//! a child's `thread_started`. The runner never opens a child log itself:
//! [`RunnerHost::build_child`] reopens one that exists, so a rebuilt runner
//! continues in the child's own file.

use aigentic_log::ThreadLog;
use ulid::Ulid;

use crate::Runtime;

use super::RunnerError;

/// What the runner needs from outside itself to drive a run.
pub trait RunnerHost {
    /// A fresh thread id for a new child.
    fn new_child_id(&mut self) -> Ulid;

    /// Writes the child's `thread_started` with `parent_thread` = lead id
    /// and `step`.
    fn create_child(&mut self, id: Ulid, step: &str) -> Result<(), RunnerError>;

    /// Builds the child's Runtime for `profile`, reopening its log if it
    /// exists, with `with_step(step, deny)` applied.
    fn build_child(
        &mut self,
        id: Ulid,
        profile: &str,
        step: &str,
        deny: &[String],
    ) -> Result<Runtime, RunnerError>;

    /// Whether the child's log is already on disk.
    fn child_exists(&self, id: Ulid) -> bool;

    /// Opens the child's log, reading the file as it stands. The runner
    /// reads a child's own events from here rather than keeping them.
    fn child_log(&self, id: Ulid) -> Result<ThreadLog, RunnerError>;

    /// The model a profile runs, for a template's `model` slot.
    fn model_of(&self, profile: &str) -> Result<String, RunnerError>;
}
