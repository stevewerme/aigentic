//! The build runner: one lead thread drives a workflow's steps.
//!
//! **Scope.** A runner that is code drives a lead thread through: brief →
//! `size` route → implement-alone, up to the implementer's report. Each move
//! is written to the lead log *before* it is acted on, so a runner rebuilt
//! over the same logs does the next move once and never repeats one. The
//! checks, push, install and close are #65's; the daemon wiring is #58's.
//! Nothing here knows about `core`'s types beyond what a log line holds.

pub mod forge;
pub mod git;
pub mod host;
pub mod install;
pub mod slots;

pub use forge::{FakeForge, Forge, ForgeError, GhForge, IssueView};
pub use git::{GitRepo, Repo, RepoError};
pub use host::RunnerHost;
pub use install::{CargoInstaller, FakeInstaller, Installer};

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use aigentic_core::{AgentId, Author, ContentBlock, Event, EventKind};
use aigentic_log::{
    CheckOutcome, CheckResult, CheckpointAnswer, CheckpointAnsweredPayload, CheckpointAskedPayload,
    ChecksOutcome, ChecksRunPayload, CommitRef, LogError, NewEvent, NextMove, PushedPayload,
    ReleaseImpact, ReportStatus, RouteTakenPayload, RunFinishedPayload, RunOutcome,
    RunStartedPayload, RunState, StepFinishedPayload, StepReport, StepStartedPayload, StepStatus,
    ThreadLog, TurnEndedPayload, run_state,
};
use serde_json::Value;
use ulid::Ulid;

use crate::checks::git::{read_commits_between, read_uncommitted};
use crate::checks::{CheckInput, run_checks};
use crate::workflow::{LoadedWorkflow, Step, WorkflowError};
use crate::{LENGTH_STOP, Runtime, RuntimeError, STEP_REPORTED, Signal};

/// The author id of the lead's own events, and of the messages a runner
/// posts into a child.
const RUNNER: &str = "runner";

/// What a step's next attempt is told when the last one ended without a
/// report: it stopped on its own, so it must call `finish_step`.
pub const CALL_FINISH_STEP: &str = "call finish_step";

/// What a step's next attempt is told when the last one was cut off by a
/// cap: the work is not finished, so it continues.
pub const CONTINUE_PROMPT: &str = "continue";

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
    /// A git read or push failed.
    #[error("repo: {0}")]
    Repo(#[from] RepoError),
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
    /// Installing the binary failed, or the installed binary could not be
    /// asked its version.
    #[error("install: {0}")]
    Install(String),
    /// A gate was answered `go` or `amend`: acting on that is a later
    /// slice's work, so the runner stops rather than guessing.
    #[error("gate `{gate}` was answered go or amend, and acting on that is a later slice")]
    SliceTwo {
        /// The gate that was answered.
        gate: String,
    },
    /// A message was posted into the child and the child's log does not
    /// hold it, so the turn never started.
    #[error("the child's log does not hold the message just posted")]
    MessageNotPosted,
    /// An answer named a gate the run is not asking at. Nothing is
    /// written: the open gate is the run's, not the answerer's.
    #[error("the run waits at `{asked}`, not `{answered}`")]
    WrongGate {
        /// The gate the run is waiting at.
        asked: String,
        /// The gate the answer named.
        answered: String,
    },
    /// An answer arrived while the run waits at no gate (issue #58).
    #[error("the run is not waiting at a gate, so `{gate}` cannot be answered")]
    NotWaiting {
        /// The gate the answer named.
        gate: String,
    },
}

// ---------------------------------------------------------------------------
// The repository write lock
// ---------------------------------------------------------------------------

/// Who holds each repository's write lock, in this process. A restarted
/// runner sees the same map, which is the point: a second lead over one
/// repository must not start a writing step.
static WRITE_LOCKS: Mutex<BTreeMap<PathBuf, Ulid>> = Mutex::new(BTreeMap::new());

/// One writing step at a time per repository. Held from a writing step's
/// `start_step` until the run escalates or finishes, or the runner drops.
#[derive(Debug)]
pub struct WriteGuard {
    repo: PathBuf,
    lead: Ulid,
}

impl WriteGuard {
    /// Take `repo`'s lock for `lead`. `Err` names the lead holding it. A
    /// runner rebuilt over its own run finds its own lead there and keeps
    /// working; another lead is refused.
    fn take(repo: PathBuf, lead: Ulid) -> Result<Self, Ulid> {
        let mut locks = WRITE_LOCKS.lock().unwrap_or_else(|e| e.into_inner());
        match locks.get(&repo) {
            Some(holder) if *holder != lead => return Err(*holder),
            _ => {
                locks.insert(repo.clone(), lead);
            }
        }
        Ok(Self { repo, lead })
    }

    /// The lead holding `repo`'s write lock, if any.
    pub fn holder(repo: &Path) -> Option<Ulid> {
        WRITE_LOCKS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(repo)
            .copied()
    }
}

impl Drop for WriteGuard {
    fn drop(&mut self) {
        let mut locks = WRITE_LOCKS.lock().unwrap_or_else(|e| e.into_inner());
        if locks.get(&self.repo) == Some(&self.lead) {
            locks.remove(&self.repo);
        }
    }
}

// ---------------------------------------------------------------------------
// The runner
// ---------------------------------------------------------------------------

/// What one `advance` did.
#[derive(Debug, Clone, PartialEq)]
pub enum Advanced {
    /// A move was made; call `advance` again.
    Moved,
    /// A gate is open and unanswered: wait for the human.
    WaitingHuman {
        /// The gate that was asked.
        gate: String,
    },
    /// The run ended.
    Finished {
        /// How it ended.
        outcome: RunOutcome,
    },
}

/// Where a writing step started, as its first attempt recorded it: the
/// head its commits are read from and the remote ref its push is judged
/// against.
struct Start {
    head: String,
    remote: String,
}

/// The first attempt's start record, or the field it lacks.
enum StartRecord {
    /// The first attempt's `head_at_start` and `remote_at_start`.
    Ready { head: String, remote: String },
    /// The record is missing this field, named as the gate names it.
    Missing(&'static str),
}

/// One check that failed, as a send-back and a gate name it.
struct FailedCheck {
    id: String,
    detail: String,
}

impl FailedCheck {
    /// `- <id>: <detail>`, the line a send-back's message carries.
    fn line(&self) -> String {
        format!("- {}: {}", self.id, self.detail)
    }

    /// `Id: detail`, the shape a gate's `shown` carries: no list marker a
    /// gate reader would mistake for the list itself.
    fn named(&self) -> String {
        format!("{}: {}", self.id, self.detail)
    }
}

/// One run's lead thread: the log it appends to, and the seams it drives
/// children and the forge through.
pub struct Runner<F: Forge, H: RunnerHost, R: Repo> {
    lead: ThreadLog,
    lead_id: Ulid,
    forge: F,
    host: H,
    installer: Box<dyn Installer>,
    workflow: LoadedWorkflow,
    repo: R,
    lock: Option<WriteGuard>,
}

impl<F: Forge, H: RunnerHost, R: Repo> Runner<F, H, R> {
    /// A runner over a log the daemon already wrote `run_started` to.
    pub fn new(
        lead: ThreadLog,
        lead_id: Ulid,
        forge: F,
        host: H,
        installer: Box<dyn Installer>,
        workflow: LoadedWorkflow,
        repo: R,
    ) -> Result<Self, RunnerError> {
        if lead.thread_id() != lead_id {
            return Err(RunnerError::ThreadMismatch {
                log: lead.thread_id(),
                lead: lead_id,
            });
        }
        Ok(Self {
            lead,
            lead_id,
            forge,
            host,
            installer,
            workflow,
            repo,
            lock: None,
        })
    }

    /// The lead log, for a caller that wants to read it back.
    pub fn log(&self) -> &ThreadLog {
        &self.lead
    }

    /// The run's replayed state.
    pub fn state(&self) -> Result<RunState, RunnerError> {
        Ok(run_state(self.lead.events())?)
    }

    /// The run's `run_started` payload.
    fn run(&self) -> Result<RunStartedPayload, RunnerError> {
        let event = self
            .lead
            .events()
            .iter()
            .find(|e| e.kind == EventKind::RunStarted)
            .ok_or(RunnerError::NotARun)?;
        serde_json::from_value(event.payload.clone()).map_err(|_| RunnerError::NotARun)
    }

    /// The issue the run is for.
    pub fn issue(&self) -> Result<u64, RunnerError> {
        Ok(self.run()?.issue)
    }

    /// One workflow step by id.
    pub fn step(&self, id: &str) -> Result<&Step, RunnerError> {
        self.workflow
            .workflow
            .steps
            .iter()
            .find(|step| step.id == id)
            .ok_or_else(|| WorkflowError::UnknownStep { step: id.into() }.into())
    }

    /// Do exactly one move. Every move writes what it decided before it
    /// acts, so an `advance` that returns never needs doing twice.
    pub async fn advance(&mut self) -> Result<Advanced, RunnerError> {
        match self.state()?.next_move() {
            NextMove::NotARun => Err(RunnerError::NotARun),
            NextMove::StartFirstStep => {
                let first = self.first_step()?;
                self.start_step(&first, 1, None).await
            }
            NextMove::ReAwait {
                step,
                child_thread,
                attempt,
            } => self.re_await(&step, attempt, child_thread).await,
            NextMove::AfterStep {
                step,
                attempt,
                status,
            } => self.after_step(&step, attempt, status).await,
            NextMove::FollowRoute { taken, .. } => self.follow_route(&taken).await,
            NextMove::AwaitingCheckpoint { gate } => Ok(Advanced::WaitingHuman { gate }),
            NextMove::Done { outcome } => Ok(Advanced::Finished { outcome }),
            NextMove::ChecksDone {
                step,
                attempt,
                outcome,
            } => self.checks_done(&step, attempt, outcome).await,
            NextMove::Pushed { step, .. } => self.after_pushed(&step),
            NextMove::Answered { gate, answer, .. } => self.answered(&gate, answer),
        }
    }

    /// Answer the run's open checkpoint, then let the caller advance it.
    ///
    /// `gate` must be the gate the run is waiting at, checked here,
    /// immediately before the append: the lead log has one writer, so
    /// this is where "a second answer to the same gate is refused"
    /// (issue #58) holds. A run that is not waiting, or waits elsewhere,
    /// writes nothing. `by` is the answering user; the amendment, when
    /// there is one, is the answer's text.
    pub fn answer(
        &mut self,
        gate: &str,
        answer: CheckpointAnswer,
        amendment: Option<String>,
        by: Author,
    ) -> Result<Ulid, RunnerError> {
        match self.waiting_gate()? {
            Some(asked) if asked == gate => {}
            Some(asked) => {
                return Err(RunnerError::WrongGate {
                    asked,
                    answered: gate.to_owned(),
                });
            }
            None => {
                return Err(RunnerError::NotWaiting {
                    gate: gate.to_owned(),
                });
            }
        }
        let payload = CheckpointAnsweredPayload {
            answer,
            amendment,
            marks: Vec::new(),
        };
        self.append_as(by, EventKind::CheckpointAnswered, &payload)
    }

    /// The gate the run waits at, or `None` when it is not waiting.
    fn waiting_gate(&self) -> Result<Option<String>, RunnerError> {
        Ok(match self.state()?.next_move() {
            NextMove::AwaitingCheckpoint { gate } => Some(gate),
            _ => None,
        })
    }

    /// `advance` until the run pauses, waits or ends.
    pub async fn run_to_pause(&mut self) -> Result<Advanced, RunnerError> {
        loop {
            match self.advance().await? {
                Advanced::Moved => continue,
                other => return Ok(other),
            }
        }
    }

    // -- moves --------------------------------------------------------------

    /// The workflow's first step.
    fn first_step(&self) -> Result<String, RunnerError> {
        self.workflow
            .workflow
            .steps
            .first()
            .map(|step| step.id.clone())
            .ok_or_else(|| {
                WorkflowError::UnknownStep {
                    step: "<first>".into(),
                }
                .into()
            })
    }

    /// Enter a step: decide its prompt and its child, write `step_started`,
    /// create the child, then post the prompt as the child's turn.
    async fn start_step(
        &mut self,
        step_id: &str,
        attempt: u32,
        child: Option<Ulid>,
    ) -> Result<Advanced, RunnerError> {
        let step = self.step(step_id)?.clone();
        // The prompt is decided before any child exists, so a slot the
        // brief never reported stops the run here, not mid-step.
        let message = match self.attempt_message(&step, attempt) {
            Ok(message) => message,
            Err(err @ RunnerError::Workflow(_)) => {
                return self.escalate(
                    "render_failed",
                    vec![step.id.clone(), attempt.to_string(), err.to_string()],
                );
            }
            Err(other) => return Err(other),
        };
        if step.writes
            && self.lock.is_none()
            && let Some(waiting) = self.take_write_lock()?
        {
            return Ok(waiting);
        }
        let fresh = child.is_none();
        let child = child.unwrap_or_else(|| self.host.new_child_id());
        // Where a writing step started is read on the first attempt only:
        // the checks and the push want the base the step's commits sit on,
        // and a send-back must not move it.
        let (head_at_start, remote_at_start) = if step.writes && attempt == 1 {
            let head = self.repo.head()?;
            let remote = self.repo.remote_head(&self.repo.branch()?)?;
            (Some(head), remote)
        } else {
            (None, None)
        };
        self.append(
            EventKind::StepStarted,
            &StepStartedPayload {
                step: step.id.clone(),
                role: step.role.clone(),
                profile: step.profile.clone(),
                child_thread: child,
                attempt,
                budget_usd: step.budget.unwrap_or(0.0),
                head_at_start,
                remote_at_start,
            },
        )?;
        if fresh {
            self.host.create_child(child, &step.id)?;
        }
        self.post_message(&step, child, &message).await?;
        Ok(Advanced::Moved)
    }

    /// A `step_started` with no `step_finished`: the prompt is posted once
    /// if the child's log lacks it, otherwise the child is resumed.
    async fn re_await(
        &mut self,
        step_id: &str,
        attempt: u32,
        child: Ulid,
    ) -> Result<Advanced, RunnerError> {
        let step = self.step(step_id)?.clone();
        if step.writes
            && self.lock.is_none()
            && let Some(waiting) = self.take_write_lock()?
        {
            return Ok(waiting);
        }
        let posted = prompt_count(self.host.child_log(child)?.events());
        if posted < attempt {
            // The prompt never reached the child's log: repair by posting
            // it once. The crash may also have happened before the child
            // was created, in which case it is created now — with the id
            // the `step_started` already names, never a fresh one.
            if !self.child_started(child)? {
                self.host.create_child(child, &step.id)?;
            }
            let message = self.attempt_message(&step, attempt)?;
            let mut runtime = self
                .host
                .build_child(child, &step.profile, &step.id, &step.deny)
                .await?;
            self.run_turn(&mut runtime, &message).await?;
            if prompt_count(self.host.child_log(child)?.events()) != posted + 1 {
                return Err(RunnerError::MessageNotPosted);
            }
        } else if !self.attempt_reported(child, attempt)? {
            let mut runtime = self
                .host
                .build_child(child, &step.profile, &step.id, &step.deny)
                .await?;
            let mut observe = |_: Signal<'_>| {};
            if let crate::Resumed::Interrupted { .. } = runtime.resume(None, &mut observe)? {
                runtime.continue_turn(&mut observe).await?;
            }
        }
        self.apply_end(&step, attempt)?;
        Ok(Advanced::Moved)
    }

    /// Whether this attempt's report is already in the child's log: a
    /// `step_reported` written after the attempt's own runner message.
    /// Such a report is the attempt's outcome, so the turn must not be
    /// resumed or continued to make it report again — the crash may have
    /// landed between the report and the child's `turn_ended`.
    fn attempt_reported(&self, child: Ulid, attempt: u32) -> Result<bool, RunnerError> {
        let log = self.host.child_log(child)?;
        let events = log.events();
        let from = prompt_at(events, attempt).unwrap_or(0);
        Ok(events[from..]
            .iter()
            .any(|event| event.kind == EventKind::StepReported))
    }

    /// Whether a child's log has been started at all: the crash can
    /// land between `step_started` and the child's creation.
    fn child_started(&self, child: Ulid) -> Result<bool, RunnerError> {
        Ok(self
            .host
            .child_log(child)?
            .events()
            .iter()
            .any(|event| event.kind == EventKind::ThreadStarted))
    }

    /// A child ended: report, route, retry or hand over.
    async fn after_step(
        &mut self,
        step_id: &str,
        attempt: u32,
        status: StepStatus,
    ) -> Result<Advanced, RunnerError> {
        let step = self.step(step_id)?.clone();
        match status {
            StepStatus::Done => {
                let Some((report, _)) = self.report_of(step_id, attempt)? else {
                    return self.escalate(
                        "step_stop",
                        vec![
                            step.id.clone(),
                            attempt.to_string(),
                            "done without a report".into(),
                        ],
                    );
                };
                self.post_report(&step, attempt, &report)?;
                self.route(&step, attempt, &report)
            }
            StepStatus::Partial => {
                let reason = self.end_reason(step_id, attempt)?;
                if self.partial_attempts(step_id, attempt) == 0 {
                    let child = self.child_of(step_id, attempt)?;
                    self.start_step(step_id, attempt + 1, Some(child)).await
                } else {
                    self.escalate(
                        "step_stop",
                        vec![step.id.clone(), attempt.to_string(), reason],
                    )
                }
            }
            StepStatus::Failed => {
                let reason = self.end_reason(step_id, attempt)?;
                self.escalate(
                    "step_stop",
                    vec![step.id.clone(), attempt.to_string(), reason],
                )
            }
        }
    }

    /// A logged route decision: follow it, never re-derive it.
    async fn follow_route(&mut self, taken: &str) -> Result<Advanced, RunnerError> {
        match taken {
            "ask" => {
                let (step, attempt) = self.routing_step()?;
                let why = self
                    .state()?
                    .routes
                    .last()
                    .map(|route| {
                        format!(
                            "the brief read `{} = {}`, and full goes to the human",
                            route.branch, route.proposed
                        )
                    })
                    .unwrap_or_else(|| "the brief asked for a human".into());
                self.escalate("route", vec![step, attempt.to_string(), why])
            }
            "done" => self.finish(RunOutcome::Closed),
            other => match self.step(other) {
                Ok(step) => {
                    let id = step.id.clone();
                    self.start_step(&id, 1, None).await
                }
                Err(_) => self.escalate(
                    "route_escalate",
                    vec![
                        String::new(),
                        String::new(),
                        format!("the route targets `{other}`, and no such step exists"),
                    ],
                ),
            },
        }
    }

    /// The brief's route: write `route_taken`, or hand over.
    fn route(
        &mut self,
        step: &Step,
        attempt: u32,
        report: &StepReport,
    ) -> Result<Advanced, RunnerError> {
        let Some(branch) = step.route_by.clone() else {
            // A writing step's report is not the end: its checks and its
            // push come next, and both are decided from the log's state,
            // never from git read twice. A `push` step with no checks
            // still goes there, so a plan that names no check is not a
            // plan that silently skips the push.
            if step.push || !step.checks.is_empty() {
                return self.handover(step, attempt);
            }
            if step.next.as_deref() == Some("done") {
                return self.finish(RunOutcome::Closed);
            }
            return Ok(Advanced::Moved);
        };
        let slot = |name: &str| {
            report
                .slots
                .as_ref()
                .and_then(|slots| slots.get(name))
                .cloned()
        };
        let Some(size) = slot(&branch).and_then(|v| v.as_str().map(str::to_owned)) else {
            return self.escalate(
                "route_escalate",
                vec![
                    step.id.clone(),
                    attempt.to_string(),
                    format!("the report has no `{branch}` slot"),
                ],
            );
        };
        let Some(taken) = step.routes.get(&size).cloned() else {
            return self.escalate(
                "route_escalate",
                vec![
                    step.id.clone(),
                    attempt.to_string(),
                    format!("`{branch} = {size}` names no route"),
                ],
            );
        };
        let Some(budget) = slot("budget").as_ref().and_then(budget_usd) else {
            return self.escalate(
                "route_escalate",
                vec![
                    step.id.clone(),
                    attempt.to_string(),
                    "the report has no numeric `budget` slot".into(),
                ],
            );
        };
        self.append(
            EventKind::RouteTaken,
            &RouteTakenPayload {
                branch,
                proposed: size,
                taken,
                preconditions: Vec::new(),
                fallback_reason: None,
                budget_usd: Some(budget),
            },
        )?;
        Ok(Advanced::Moved)
    }

    // -- the writing step's handover: checks, push, install, close ---------

    /// After a writing step reported: run its checks, or go straight to the
    /// push when it names none. The first attempt's start record is read
    /// first — a step that began without one is not judged and not pushed.
    fn handover(&mut self, step: &Step, attempt: u32) -> Result<Advanced, RunnerError> {
        let start = match self.start_record(&step.id)? {
            StartRecord::Ready { head, remote } => Start { head, remote },
            StartRecord::Missing(field) => {
                return self.escalate("no_start_record", vec![step.id.clone(), field.into()]);
            }
        };
        if step.checks.is_empty() {
            return self.push_and_install(step, attempt, &start);
        }
        let Some((report, _)) = self.report_of(&step.id, attempt)? else {
            return self.escalate(
                "step_stop",
                vec![
                    step.id.clone(),
                    attempt.to_string(),
                    "done without a report".into(),
                ],
            );
        };
        let child = self.child_of(&step.id, attempt)?;
        let log = self.host.child_log(child)?;
        // Every failing read is the `checks_error` gate with git's own
        // words: a check that could not run is never a substituted value.
        let commits = match read_commits_between(self.repo.root(), &start.head, "HEAD") {
            Ok(commits) => commits,
            Err(err) => {
                return self.escalate(
                    "checks_error",
                    vec![step.id.clone(), attempt.to_string(), err.to_string()],
                );
            }
        };
        let uncommitted = match read_uncommitted(self.repo.root()) {
            Ok(paths) => paths,
            Err(err) => {
                return self.escalate(
                    "checks_error",
                    vec![step.id.clone(), attempt.to_string(), err.to_string()],
                );
            }
        };
        let named = self.named_subjects(step)?;
        let model = self.host.model_of(&step.profile)?;
        let input = CheckInput {
            commits: &commits,
            events: log.events(),
            report: &report,
            named_subjects: &named,
            model: &model,
            uncommitted: &uncommitted,
        };
        let checks = match run_checks(&step.checks, &input) {
            Ok(checks) => checks,
            Err(err) => {
                return self.escalate(
                    "checks_error",
                    vec![step.id.clone(), attempt.to_string(), err.to_string()],
                );
            }
        };
        self.append(
            EventKind::ChecksRun,
            &ChecksRunPayload {
                step: step.id.clone(),
                checks,
            },
        )?;
        Ok(Advanced::Moved)
    }

    /// A `checks_run` was written: a `fail` sends the step back once, a
    /// second hands it to the human; a pass or a flag pushes.
    async fn checks_done(
        &mut self,
        step_id: &str,
        attempt: u32,
        outcome: ChecksOutcome,
    ) -> Result<Advanced, RunnerError> {
        let step = self.step(step_id)?.clone();
        // A rebuilt runner takes the write lock again here: the run is
        // still the writing step's until `run_finished` (rule 9).
        if step.writes
            && self.lock.is_none()
            && let Some(waiting) = self.take_write_lock()?
        {
            return Ok(waiting);
        }
        match outcome {
            ChecksOutcome::Pass | ChecksOutcome::Flagged => {
                if !step.push {
                    return self.escalate(
                        "no_push",
                        vec![
                            step.id.clone(),
                            attempt.to_string(),
                            "the step's checks did not block the push, and it does not push".into(),
                        ],
                    );
                }
                let start = match self.start_record(&step.id)? {
                    StartRecord::Ready { head, remote } => Start { head, remote },
                    StartRecord::Missing(field) => {
                        return self
                            .escalate("no_start_record", vec![step.id.clone(), field.into()]);
                    }
                };
                self.push_and_install(&step, attempt, &start)
            }
            ChecksOutcome::Fail => {
                let failing = self.failing_checks(&step.id);
                if self.failed_runs(&step.id) <= 1 {
                    let child = self.child_of(&step.id, attempt)?;
                    return self.start_step(&step.id, attempt + 1, Some(child)).await;
                }
                let mut shown = vec![step.id.clone(), attempt.to_string()];
                shown.extend(failing.iter().map(|check| check.named()));
                self.escalate("checks_failed", shown)
            }
        }
    }

    /// Push the step's commits once, install the binary, and record both.
    /// The remote is read first: a remote that moved during the step is
    /// never pushed over, never forced.
    fn push_and_install(
        &mut self,
        step: &Step,
        attempt: u32,
        start: &Start,
    ) -> Result<Advanced, RunnerError> {
        let branch = self.repo.branch()?;
        let head = self.repo.head()?;
        let remote = self.repo.remote_head(&branch)?;
        if remote.as_deref() != Some(head.as_str()) {
            if remote.as_deref() != Some(start.remote.as_str()) {
                let moved = remote.unwrap_or_else(|| "(the remote has no such branch)".into());
                return self.escalate(
                    "remote_moved",
                    vec![
                        step.id.clone(),
                        format!("remote moved during the step: {}..{moved}", start.remote),
                    ],
                );
            }
            if let Err(error) = self.repo.push(&branch) {
                let moved_during_push = match &error {
                    RepoError::Command { message, .. } => {
                        message.contains("non-fast-forward") || message.contains("fetch first")
                    }
                    RepoError::Io(_) => false,
                };
                let detail = if moved_during_push {
                    format!("remote moved during the push: {error}")
                } else {
                    error.to_string()
                };
                return self.escalate("push_error", vec![step.id.clone(), detail]);
            }
        }
        let installed = self.installer.install(&self.repo)?;
        let short = &head[..head.len().min(12)];
        if !installed.contains(short) {
            return self.escalate(
                "install_mismatch",
                vec![step.id.clone(), installed, head.clone()],
            );
        }
        let commits = match read_commits_between(self.repo.root(), &start.head, "HEAD") {
            Ok(commits) => commits
                .into_iter()
                .map(|commit| CommitRef {
                    sha: commit.sha,
                    subject: commit.subject,
                })
                .collect(),
            Err(err) => {
                return self.escalate(
                    "checks_error",
                    vec![step.id.clone(), attempt.to_string(), err.to_string()],
                );
            }
        };
        self.append(
            EventKind::Pushed,
            &PushedPayload {
                commits,
                ref_before: start.remote.clone(),
                ref_after: head,
                installed: Some(installed),
            },
        )?;
        Ok(Advanced::Moved)
    }

    /// After the push is written: close the issue when this step is the
    /// last, and end the run. Both are safe to repeat — a rebuild re-reads
    /// the issue's comments and never posts the closing comment twice.
    fn after_pushed(&mut self, step_id: &str) -> Result<Advanced, RunnerError> {
        let step = self.step(step_id)?.clone();
        // A rebuilt runner takes the write lock again: the closing comment
        // and the close are this run's, and nothing here repeats them
        // anyway, but the lock outlives every move up to `run_finished`.
        if step.writes
            && self.lock.is_none()
            && let Some(waiting) = self.take_write_lock()?
        {
            return Ok(waiting);
        }
        if step.next.as_deref() != Some("done") {
            return self.escalate(
                "no_next_step",
                vec![
                    step.id.clone(),
                    format!("`{}` is the last step and names no `next`", step.id),
                ],
            );
        }
        let issue = self.issue()?;
        let (report, _) = self.latest_report(&step.id)?;
        let impact = report.release_impact;
        let tag = self.close_tag();
        if !self
            .forge
            .comments(issue)?
            .iter()
            .any(|comment| comment.contains(&tag))
        {
            let stated = match impact {
                Some(impact) => impact_word(impact).to_owned(),
                None => "unstated".to_owned(),
            };
            let text = format!(
                "## Closing\n\n{}\n\nRelease impact: {stated}\n\n{tag}",
                report.body.clone().unwrap_or_default()
            );
            self.forge.comment(issue, &text)?;
        }
        self.forge.close(issue)?;
        self.finish_with(RunOutcome::Closed, impact)
    }

    /// Where a step's first attempt started: the head its commits are read
    /// from and the remote ref its push is judged against. A later attempt
    /// records nothing, so the first attempt is the only source.
    fn start_record(&self, step_id: &str) -> Result<StartRecord, RunnerError> {
        let event = self.lead.events().iter().find(|event| {
            event.kind == EventKind::StepStarted
                && serde_json::from_value::<StepStartedPayload>(event.payload.clone())
                    .is_ok_and(|payload| payload.step == step_id && payload.attempt == 1)
        });
        let Some(event) = event else {
            return Ok(StartRecord::Missing("head_at_start"));
        };
        let payload: StepStartedPayload = serde_json::from_value(event.payload.clone())
            .map_err(|_| RunnerError::Host(format!("unreadable step_started for {step_id}")))?;
        match (payload.head_at_start, payload.remote_at_start) {
            (Some(head), Some(remote)) => Ok(StartRecord::Ready { head, remote }),
            (None, _) => Ok(StartRecord::Missing("head_at_start")),
            (_, None) => Ok(StartRecord::Missing("remote_at_start")),
        }
    }

    /// The subjects the brief named, in its own order: the `commits` slot
    /// of the last step before this one that reported. Absent, or not a
    /// list of strings, is an empty list — never a guess.
    fn named_subjects(&self, step: &Step) -> Result<Vec<String>, RunnerError> {
        let Some(report) = self.earlier_reports(step)?.pop() else {
            return Ok(Vec::new());
        };
        let Some(Value::Array(items)) =
            report.slots.and_then(|slots| slots.get("commits").cloned())
        else {
            return Ok(Vec::new());
        };
        Ok(items
            .iter()
            .map(|item| item.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
            .unwrap_or_default())
    }

    /// This step's latest `checks_run`, and the checks it failed.
    fn failing_checks(&self, step_id: &str) -> Vec<FailedCheck> {
        self.last_checks(step_id)
            .into_iter()
            .flatten()
            .filter(|check| check.result == CheckResult::Fail)
            .map(|check| FailedCheck {
                id: check.id,
                detail: check.detail.unwrap_or_else(|| "no detail".into()),
            })
            .collect()
    }

    /// The checks of a step's latest `checks_run`.
    fn last_checks(&self, step_id: &str) -> Option<Vec<CheckOutcome>> {
        self.lead
            .events()
            .iter()
            .rev()
            .filter(|event| event.kind == EventKind::ChecksRun)
            .filter_map(|event| {
                serde_json::from_value::<ChecksRunPayload>(event.payload.clone()).ok()
            })
            .find(|payload| payload.step == step_id)
            .map(|payload| payload.checks)
    }

    /// How many `checks_run` events of this step hold a `fail`: the first
    /// sends the step back, so a second means the send-back did not help.
    fn failed_runs(&self, step_id: &str) -> usize {
        self.lead
            .events()
            .iter()
            .filter(|event| event.kind == EventKind::ChecksRun)
            .filter_map(|event| {
                serde_json::from_value::<ChecksRunPayload>(event.payload.clone()).ok()
            })
            .filter(|payload| {
                payload.step == step_id
                    && payload.checks.iter().any(|c| c.result == CheckResult::Fail)
            })
            .count()
    }

    /// Whether this step's latest state event is a `checks_run` holding a
    /// `fail`: that is what a send-back answers. Any other event in
    /// between — a budget warning, say — does not change it.
    fn send_back_due(&self, step_id: &str) -> bool {
        self.lead
            .events()
            .iter()
            .rev()
            .find(|event| {
                matches!(
                    event.kind,
                    EventKind::StepStarted | EventKind::StepFinished | EventKind::ChecksRun
                )
            })
            .filter(|event| event.kind == EventKind::ChecksRun)
            .and_then(|event| {
                serde_json::from_value::<ChecksRunPayload>(event.payload.clone()).ok()
            })
            .is_some_and(|payload| {
                payload.step == step_id
                    && payload.checks.iter().any(|c| c.result == CheckResult::Fail)
            })
    }

    /// The tag a run's closing comment carries, so a rebuild never posts a
    /// second one.
    fn close_tag(&self) -> String {
        format!("<!-- aigentic run={} step=close -->", self.lead_id)
    }

    /// A step's latest attempt that reported, and its report.
    fn latest_report(&self, step_id: &str) -> Result<(StepReport, u32), RunnerError> {
        let state = self.state()?;
        let record = state
            .steps
            .iter()
            .rev()
            .find(|record| record.step == step_id && record.reported_event.is_some())
            .ok_or_else(|| RunnerError::Host(format!("no report for `{step_id}`")))?;
        let attempt = record.attempt;
        let Some((report, _)) = self.report_of(step_id, attempt)? else {
            return Err(RunnerError::Host(format!(
                "no report for `{step_id}` attempt {attempt}"
            )));
        };
        Ok((report, attempt))
    }

    // -- pieces -------------------------------------------------------------

    /// Write `run_finished` and end the run.
    fn finish(&mut self, outcome: RunOutcome) -> Result<Advanced, RunnerError> {
        self.finish_with(outcome, None)
    }

    /// Write `run_finished` with the closing comment's release impact, and
    /// end the run.
    fn finish_with(
        &mut self,
        outcome: RunOutcome,
        release_impact: Option<ReleaseImpact>,
    ) -> Result<Advanced, RunnerError> {
        let cost_usd = self.cost();
        self.append(
            EventKind::RunFinished,
            &RunFinishedPayload {
                outcome,
                cost_usd,
                release_impact,
            },
        )?;
        self.lock = None;
        Ok(Advanced::Finished { outcome })
    }

    /// A human answered a gate: `stop` ends the run, and acting on `go` or
    /// `amend` belongs to a later slice, so it is an error, not a guess.
    fn answered(&mut self, gate: &str, answer: CheckpointAnswer) -> Result<Advanced, RunnerError> {
        match answer {
            CheckpointAnswer::Stop => self.finish(RunOutcome::Stopped),
            CheckpointAnswer::Go | CheckpointAnswer::Amend => Err(RunnerError::SliceTwo {
                gate: gate.to_owned(),
            }),
        }
    }

    /// Write `checkpoint_asked` and hand the run to the human. This is the
    /// one act safe to repeat after a crash.
    fn escalate(&mut self, gate: &str, shown: Vec<String>) -> Result<Advanced, RunnerError> {
        self.append(
            EventKind::CheckpointAsked,
            &CheckpointAskedPayload {
                gate: gate.into(),
                shown,
                options: vec!["stop".into()],
            },
        )?;
        self.lock = None;
        Ok(Advanced::WaitingHuman { gate: gate.into() })
    }

    /// Take this repository's write lock for this lead, keyed by the
    /// repository root in the process-wide map. A rebuild over the same
    /// run finds its own lead there and keeps working; another lead gets
    /// the `write_lock` gate — the pause is returned as `Some` — naming
    /// the holder. On success the guard is held in `self.lock` until the
    /// runner escalates, finishes or drops, so a second writing step in
    /// this run never takes it again.
    fn take_write_lock(&mut self) -> Result<Option<Advanced>, RunnerError> {
        match WriteGuard::take(self.repo.root().to_path_buf(), self.lead_id) {
            Ok(guard) => {
                self.lock = Some(guard);
                Ok(None)
            }
            Err(holder) => {
                let shown = vec![
                    self.repo.root().display().to_string(),
                    format!("lead {holder} holds it"),
                    format!("lead {} waits", self.lead_id),
                ];
                self.escalate("write_lock", shown).map(Some)
            }
        }
    }

    /// Append one lead event, authored `agent:runner`.
    fn append(
        &mut self,
        kind: EventKind,
        payload: &impl serde::Serialize,
    ) -> Result<Ulid, RunnerError> {
        self.append_as(Author::Agent(AgentId(RUNNER.into())), kind, payload)
    }

    /// `append`, authored by `by` rather than the runner: a human's
    /// answer at a gate carries their name.
    fn append_as(
        &mut self,
        by: Author,
        kind: EventKind,
        payload: &impl serde::Serialize,
    ) -> Result<Ulid, RunnerError> {
        let payload = serde_json::to_value(payload).expect("payloads are serialisable");
        let event = self.lead.append(NewEvent {
            kind,
            author: by,
            payload,
            parent_event: None,
        })?;
        Ok(event.id)
    }

    /// Run one turn in the child, authored as the runner.
    async fn run_turn(&self, runtime: &mut Runtime, text: &str) -> Result<(), RunnerError> {
        let mut observe = |_: Signal<'_>| {};
        runtime
            .run_turn(
                Author::Agent(AgentId(RUNNER.into())),
                vec![ContentBlock::Text(text.to_owned())],
                &mut observe,
            )
            .await?;
        Ok(())
    }

    /// Post a step's prompt into its child, and prove it arrived.
    async fn post_message(
        &mut self,
        step: &Step,
        child: Ulid,
        text: &str,
    ) -> Result<(), RunnerError> {
        let before = prompt_count(self.host.child_log(child)?.events());
        let mut runtime = self
            .host
            .build_child(child, &step.profile, &step.id, &step.deny)
            .await?;
        self.run_turn(&mut runtime, text).await?;
        if prompt_count(self.host.child_log(child)?.events()) != before + 1 {
            return Err(RunnerError::MessageNotPosted);
        }
        Ok(())
    }

    /// The prompt an attempt gets: the step's template rendered for the
    /// first, a short instruction afterwards, and the template again with
    /// the failing checks when a send-back brings the step back. The render
    /// can fail, and a failed render is a `render_failed` escalation.
    fn attempt_message(&self, step: &Step, attempt: u32) -> Result<String, RunnerError> {
        // A send-back reads as a first attempt with the failures handed to
        // it: the same template, plus the failing checks, so the step is
        // told exactly which lines to fix (#57's `check_failures` slot).
        if self.send_back_due(&step.id) {
            let mut slots = self.slot_map(step)?;
            let lines: Vec<String> = self
                .failing_checks(&step.id)
                .iter()
                .map(FailedCheck::line)
                .collect();
            slots.insert("check_failures".to_string(), Value::from(lines.join("\n")));
            return Ok(self.workflow.render(&step.id, &slots)?);
        }
        if attempt <= 1 {
            let slots = self.slot_map(step)?;
            return Ok(self.workflow.render(&step.id, &slots)?);
        }
        let previous = self.end_reason(&step.id, attempt - 1)?;
        Ok(match previous.as_str() {
            // It stopped on its own or was picked up without reporting.
            "done" | "resumed" => CALL_FINISH_STEP.to_owned(),
            _ => CONTINUE_PROMPT.to_owned(),
        })
    }

    /// Read the child's end: write `step_finished`, and escalate a step
    /// that failed outright.
    fn apply_end(&mut self, step: &Step, attempt: u32) -> Result<(), RunnerError> {
        let child = self.child_of(&step.id, attempt)?;
        let log = self.host.child_log(child)?;
        let events = log.events();
        let from = prompt_at(events, attempt).unwrap_or(0);
        let tail = &events[from..];
        // The price the child's runtime stamped on each call, inside its
        // `usage`: `payload["cost_usd"]` at the top level is always absent,
        // which is why this used to sum to an empty `-0.0`. A `fold` from a
        // positive zero keeps a run with no prices from writing `-0.0`.
        let cost_usd: f64 = tail
            .iter()
            .filter(|e| e.kind == EventKind::AssistantMessage)
            .filter_map(|e| {
                e.payload
                    .get("usage")
                    .and_then(|usage| usage.get("cost_usd"))
                    .and_then(Value::as_f64)
            })
            .fold(0.0_f64, |total, cost| total + cost);
        let reported = tail
            .iter()
            .rev()
            .find(|e| e.kind == EventKind::StepReported);
        let finish = |status: StepStatus, end_reason: String, reported_event: Option<Ulid>| {
            StepFinishedPayload {
                step: step.id.clone(),
                status,
                end_reason,
                cost_usd,
                reported_event,
            }
        };
        // A report this attempt wrote is its outcome, whatever the latest
        // `turn_ended` says: the crash may have landed between the report
        // and the turn's end, and the report is in the log either way. This
        // is reached only when `re_await` left the turn alone for the same
        // reason, so the turn is never run again to produce a second one.
        if let Some(report_event) = reported {
            let report: StepReport =
                serde_json::from_value(report_event.payload.clone()).map_err(|_| {
                    RunnerError::Host(format!("child {child}: unreadable step_reported"))
                })?;
            if report.body.clone().unwrap_or_default().trim().is_empty() {
                // No `step_finished`: a report with no body is not a step's
                // outcome the log may record as one.
                self.escalate(
                    "step_stop",
                    vec![
                        step.id.clone(),
                        attempt.to_string(),
                        "the report has no body".into(),
                    ],
                )?;
                return Ok(());
            }
            let status = match report.status {
                Some(ReportStatus::Done) => StepStatus::Done,
                _ => StepStatus::Partial,
            };
            let finished = finish(status, STEP_REPORTED.into(), Some(report_event.id));
            self.append(EventKind::StepFinished, &finished)?;
            return Ok(());
        }
        let Some(end) = tail.iter().rev().find(|e| e.kind == EventKind::TurnEnded) else {
            return Err(RunnerError::Host(format!(
                "child {child} never ended its turn"
            )));
        };
        let end: TurnEndedPayload = serde_json::from_value(end.payload.clone())
            .map_err(|_| RunnerError::Host(format!("child {child}: unreadable turn_ended")))?;
        match end.reason.as_str() {
            STEP_REPORTED => Err(RunnerError::Host(format!("child {child} reported nothing"))),
            // `LENGTH_STOP` is a budget-style stop like `max_tokens`: the
            // step is Partial, not Failed, and nothing escalates (issue
            // #96).
            "done" | "resumed" | LENGTH_STOP | "max_iterations" | "max_tokens"
            | "max_wall_time" => {
                let finished = finish(StepStatus::Partial, end.reason, None);
                self.append(EventKind::StepFinished, &finished)?;
                Ok(())
            }
            other => {
                let finished = finish(StepStatus::Failed, other.to_owned(), None);
                self.append(EventKind::StepFinished, &finished)?;
                self.escalate(
                    "step_stop",
                    vec![step.id.clone(), attempt.to_string(), other.to_owned()],
                )?;
                Ok(())
            }
        }
    }

    /// Post a step's report comment, once per (step, attempt).
    fn post_report(
        &mut self,
        step: &Step,
        attempt: u32,
        report: &StepReport,
    ) -> Result<(), RunnerError> {
        let issue = self.issue()?;
        let tag = format!(
            "<!-- aigentic run={} step={} attempt={} -->",
            self.lead_id, step.id, attempt
        );
        if self
            .forge
            .comments(issue)?
            .iter()
            .any(|comment| comment.contains(&tag))
        {
            return Ok(());
        }
        let text = format!(
            "{}\n\n{}\n\n{}",
            step.marker,
            report.body.clone().unwrap_or_default(),
            tag
        );
        self.forge.comment(issue, &text)?;
        Ok(())
    }

    // -- reading the logs ---------------------------------------------------

    /// The child of a step's attempt.
    fn child_of(&self, step_id: &str, attempt: u32) -> Result<Ulid, RunnerError> {
        self.state()?
            .steps
            .iter()
            .rev()
            .find(|record| record.step == step_id && record.attempt == attempt)
            .map(|record| record.child_thread)
            .ok_or_else(|| RunnerError::Host(format!("no child for {step_id} attempt {attempt}")))
    }

    /// The report a step's attempt made, read from its child's log.
    fn report_of(
        &self,
        step_id: &str,
        attempt: u32,
    ) -> Result<Option<(StepReport, Ulid)>, RunnerError> {
        let state = self.state()?;
        let Some(record) = state
            .steps
            .iter()
            .rev()
            .find(|record| record.step == step_id && record.attempt == attempt)
        else {
            return Ok(None);
        };
        let Some(id) = record.reported_event else {
            return Ok(None);
        };
        let log = self.host.child_log(record.child_thread)?;
        let Some(event) = log.events().iter().find(|e| e.id == id) else {
            return Ok(None);
        };
        let report: StepReport = serde_json::from_value(event.payload.clone()).map_err(|_| {
            RunnerError::Host(format!("child {}: unreadable report", record.child_thread))
        })?;
        Ok(Some((report, id)))
    }

    /// How many earlier attempts of this step already ended `Partial`.
    fn partial_attempts(&self, step_id: &str, attempt: u32) -> usize {
        self.state()
            .map(|state| {
                state
                    .steps
                    .iter()
                    .filter(|record| {
                        record.step == step_id
                            && record.attempt < attempt
                            && record.status == Some(StepStatus::Partial)
                    })
                    .count()
            })
            .unwrap_or(0)
    }

    /// A step's recorded end reason, for a gate's `shown`.
    fn end_reason(&self, step_id: &str, attempt: u32) -> Result<String, RunnerError> {
        Ok(self
            .state()?
            .steps
            .iter()
            .rev()
            .find(|record| record.step == step_id && record.attempt == attempt)
            .and_then(|record| record.end_reason.clone())
            .unwrap_or_else(|| "no end recorded".into()))
    }

    /// The step that last wrote a route decision, and its attempt: a
    /// gate's `shown` names them.
    fn routing_step(&self) -> Result<(String, u32), RunnerError> {
        let event = self
            .lead
            .events()
            .iter()
            .rev()
            .find(|e| e.kind == EventKind::StepStarted)
            .ok_or(RunnerError::NotARun)?;
        let payload: StepStartedPayload = serde_json::from_value(event.payload.clone())
            .map_err(|_| RunnerError::Host("unreadable step_started".into()))?;
        Ok((payload.step, payload.attempt))
    }

    /// What the run has cost so far.
    fn cost(&self) -> f64 {
        self.state().map(|state| state.cost_usd).unwrap_or(0.0)
    }
}

// ---------------------------------------------------------------------------
// The prompt log lines
// ---------------------------------------------------------------------------

/// Whether this event is a prompt the runner posted into a child.
fn is_prompt(event: &Event) -> bool {
    event.kind == EventKind::UserMessage && event.author == Author::Agent(AgentId(RUNNER.into()))
}

/// How many prompts the log holds.
fn prompt_count(events: &[Event]) -> u32 {
    events.iter().filter(|event| is_prompt(event)).count() as u32
}

/// Where the `attempt`-th prompt sits, 1-based.
fn prompt_at(events: &[Event], attempt: u32) -> Option<usize> {
    events
        .iter()
        .enumerate()
        .filter(|(_, event)| is_prompt(event))
        .nth(attempt.saturating_sub(1) as usize)
        .map(|(index, _)| index)
}

/// A release impact as its serde name (`none`, `patch`, `minor`,
/// `breaking`): the word a closing comment writes, read from the one place
/// that decides it, so a rename cannot drift.
fn impact_word(impact: ReleaseImpact) -> String {
    serde_json::to_value(impact)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unstated".to_owned())
}

/// A brief's `budget` slot as USD: a JSON number, or a string that
/// starts with one (`"3"`, `"3 USD"`, `"$3.50"`). Models write the unit
/// even when asked for a number, and the wording must not stop a run
/// (slice 1's acceptance: `"3 USD"` escalated).
fn budget_usd(value: &serde_json::Value) -> Option<f64> {
    if let Some(n) = value.as_f64() {
        return Some(n);
    }
    let text = value.as_str()?.trim();
    let text = text.strip_prefix('$').unwrap_or(text).trim_start();
    let end = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    text[..end].parse::<f64>().ok().filter(|n| n.is_finite())
}

#[cfg(test)]
mod tests {
    use super::budget_usd;
    use serde_json::json;

    #[test]
    fn a_budget_is_a_number_or_starts_with_one() {
        assert_eq!(budget_usd(&json!(3)), Some(3.0));
        assert_eq!(budget_usd(&json!(4.5)), Some(4.5));
        assert_eq!(budget_usd(&json!("3")), Some(3.0));
        assert_eq!(budget_usd(&json!("3 USD")), Some(3.0));
        assert_eq!(budget_usd(&json!("$3.50")), Some(3.5));
        assert_eq!(budget_usd(&json!(" 6usd ")), Some(6.0));
        assert_eq!(budget_usd(&json!("three")), None);
        assert_eq!(budget_usd(&json!("USD 3")), None);
        assert_eq!(budget_usd(&json!("")), None);
        assert_eq!(budget_usd(&json!(null)), None);
    }
}
