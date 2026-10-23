//! What makes a thread a background job's: the approval it runs under, and
//! the deny list that binds it whatever the mode says.
//!
//! A job is a child of the front thread that proposed it, and it runs with
//! nobody watching its calls: it asks nobody, it never leaves its project,
//! and it never pushes. Those are the job's own rules, checked before the
//! policy rules, so no project rule, no mode and no session grant can talk
//! past them.

use aigentic_policy::StepOverlay;
use ulid::Ulid;

use crate::mode::Mode;

/// The rule a call a job may not make records.
pub const JOB_RULE_ASK: &str = "job asks nobody";

/// The rule a write a read-only job may not make records.
pub const JOB_RULE_READ_ONLY: &str = "job read-only";

/// The rule a call reaching outside the job's project records.
pub const JOB_RULE_BOUNDARY: &str = "job boundary";

/// The loop's deny list: the entries the build workflow pins for the step
/// that runs unattended, which is what a job is. Parsed once, when the
/// job's runtime is built, so a typo fails there and not at the first call.
///
/// The bundled workflow's own test asserts this list equals that step's,
/// so an entry added there cannot silently miss jobs.
pub const JOB_DENY: &[&str] = &[
    "git push",
    "git rebase",
    "git reset",
    "git checkout --",
    "git stash",
    "git clean",
    "git add -A",
    "git update-ref",
    "git commit-tree",
    "git read-tree",
    "git update-index",
    "git replace",
    "git filter-branch",
    "git filter-repo",
    "git reflog expire",
    "git reflog delete",
    "git symbolic-ref",
    "git pull --rebase",
    "git pull -r",
    "copy .env",
];

/// What a job's thread runs under: the front thread it reports to, whether
/// it may change files, and the permission mode the front was in when the
/// person approved it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobSpec {
    /// The front thread the job was approved in, and reports back to.
    pub front: Ulid,
    /// Whether the job may change nothing: no write tool, no write class
    /// call, no shell write the rules would ask about.
    pub read_only: bool,
    /// The front's mode at the moment of the answer, which is the job's.
    pub mode: Mode,
}

/// A job's session state: the spec, and the deny overlay parsed from
/// [`JOB_DENY`]. Never persisted — the job's own log is the record of what
/// it did — and set by [`crate::Runtime::with_job`] alone.
#[derive(Debug, Clone)]
pub(crate) struct Job {
    spec: JobSpec,
    overlay: StepOverlay,
}

impl Job {
    pub(crate) fn new(spec: JobSpec, overlay: StepOverlay) -> Self {
        Self { spec, overlay }
    }

    pub(crate) fn spec(&self) -> &JobSpec {
        &self.spec
    }

    pub(crate) fn overlay(&self) -> &StepOverlay {
        &self.overlay
    }
}
