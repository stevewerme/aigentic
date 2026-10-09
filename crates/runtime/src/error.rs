use aigentic_core::ProviderError;
use aigentic_log::LogError;
use aigentic_log::MemoryHome;

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error(transparent)]
    Log(#[from] LogError),
    /// The provider stream failed. A `turn_ended` event with reason
    /// `provider_error` has already been appended when this is returned.
    #[error("provider: {0}")]
    Provider(#[from] ProviderError),
    #[error("skill `{0}` is not enabled")]
    UnknownSkill(String),
    /// `/remember` with no project loaded: the memory files live under
    /// the project's `.aigentic/`, and this thread has none.
    #[error("no project: no memory files to remember into")]
    NoProject,
    /// `/remember` aimed at a home this thread has not got: no folder
    /// for it, or the person's home without an owner to own it. A
    /// person's home too, for anyone but the owner.
    #[error("no {0} memory in this thread")]
    NoMemoryHome(MemoryHome),
    #[error(transparent)]
    Project(#[from] crate::ProjectError),
    /// A step thread may not switch project (issue #55): the move would
    /// replace the policy its deny overlay stands in front of.
    #[error("step thread: {0}")]
    StepThread(String),
    /// An answer to a proposal that isn't open in this log (issue #85):
    /// an id that names no `decision_proposed`, or one already answered.
    /// The idle path has no `Decisions` entry to refuse it, so the log
    /// is asked instead — a second answer would read as an orphan.
    #[error("no open proposal: {0}")]
    NoOpenProposal(String),
}
