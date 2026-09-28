use aigentic_core::ProviderError;
use aigentic_log::LogError;

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
    #[error(transparent)]
    Project(#[from] crate::ProjectError),
    /// A step thread may not switch project (issue #55): the move would
    /// replace the policy its deny overlay stands in front of.
    #[error("step thread: {0}")]
    StepThread(String),
}
