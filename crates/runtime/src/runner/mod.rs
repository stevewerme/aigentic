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

pub use forge::{CiState, FakeForge, Forge, ForgeError, GhForge, IssueView, fold_ci};
pub use git::{GitRepo, Repo, RepoError};
pub use host::RunnerHost;
pub use install::{CargoInstaller, FakeInstaller, Installer};

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use aigentic_core::{AgentId, Author, ContentBlock, Event, EventKind};
use aigentic_log::{
    BUDGET_GATE, BudgetRaisedPayload, CheckOutcome, CheckResult, CheckpointAnswer,
    CheckpointAnsweredPayload, CheckpointAskedPayload, ChecksOutcome, ChecksRunPayload, CommitRef,
    DecideItemKind, DecideProposal, LogError, NewEvent, NextMove, PushedPayload, ReleaseImpact,
    ReportStatus, RouteTakenPayload, RunErroredPayload, RunFinishedPayload, RunOutcome,
    RunStartedPayload, RunState, STEP_STOP, StepFinishedPayload, StepReport, StepStartedPayload,
    StepStatus, ThreadLog, TurnEndedPayload, Verdict, run_state,
};
use serde_json::Value;
use ulid::Ulid;

use crate::checks::git::{read_commits_between, read_uncommitted};
use crate::checks::{CheckInput, run_checks};
use crate::support::recoverable_fault;
use crate::workflow::{LoadedWorkflow, Step, WorkflowError};
use crate::{LENGTH_STOP, MAX_USD, Runtime, RuntimeError, STEP_REPORTED, Signal};

/// The author id of the lead's own events, and of the messages a runner
/// posts into a child.
pub const RUNNER: &str = "runner";

/// What a step's next attempt is told when the last one ended without a
/// report: the step is unfinished, so it asks for the work that is left
/// and for `finish_step` once that is done.
pub const CALL_FINISH_STEP: &str = "Your turn ended without calling `finish_step`, so the step has no report. The step is not finished: continue it where it stopped, do the rest of the work the prompt asks for — its gate and its commits, where it names them — and only then call `finish_step` with the report.";

/// What a step's next attempt is told when the last one was cut off by a
/// cap: the work is not finished, so it continues.
pub const CONTINUE_PROMPT: &str = "continue";

/// The end reason of an attempt whose report had no body: there is
/// nothing to post, so the attempt is partial and asked to report again.
pub const REPORT_WITHOUT_BODY: &str = "report_without_body";

/// What a step's next attempt is told when its last report had no body.
pub const REPORT_AGAIN_PROMPT: &str = "Your finish_step call had no `body`. Call finish_step again with the same fields and your whole report in `body`, not inside `slots`.";

/// The share of the run's budget at which the runner asks before it starts
/// anything else. The step in flight is never interrupted, so this covers a
/// run at 100% of its budget too; the gate's `shown` lines quote it.
pub const BUDGET_ASK_AT: f64 = 0.8;

/// How far above its figure in force a step's `step_stop` ask proposes
/// to raise it: the prefill of the picker's raise row.
pub const STEP_RAISE_BY: f64 = 1.0;

/// What the budget guard found.
enum Park {
    /// The run must ask; these are the `shown` lines the ask carries.
    Ask(Vec<String>),
    /// The log's open checkpoint is already this gate: nothing is written.
    AlreadyAsked,
}

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
    /// The network kept failing every try of a CI wait, so the run stops as
    /// any forge failure does, with the last failure's own words.
    #[error("gh failed {tries} times in a row while reading CI: {failure}")]
    ForgeExhausted {
        /// How many `ci` reads this run of failures made.
        tries: usize,
        /// The last failure, as `gh` reported it.
        failure: ForgeError,
    },
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
    /// A `raise` whose figure would not clear the guard's line. Nothing is
    /// written: the run stays parked at the gate and the person may answer
    /// again.
    #[error(
        "a raise of `{offered}` does not clear the budget gate: {spent:.2} spent against \
         {budget:.2} in force, and {least:.2} would"
    )]
    RaiseRefused {
        /// The figure the answer offered, as it was written.
        offered: String,
        /// The figure in force when the answer came.
        budget: f64,
        /// What the run had spent then.
        spent: f64,
        /// The smallest figure that clears the line.
        least: f64,
    },
    /// A `raise` at `step_stop` that is not above what the step has spent.
    /// Nothing is written: the run stays parked at the gate and the person
    /// may answer again.
    #[error(
        "a raise of `{offered}` does not free step `{step}`: it has spent {spent:.2}, and \
         {least:.2} would"
    )]
    StepRaiseRefused {
        /// The figure the answer offered, as it was written.
        offered: String,
        /// The step the park holds.
        step: String,
        /// What the step had spent then.
        spent: f64,
        /// The smallest figure the gate accepts.
        least: f64,
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

/// How long the runner waits on CI after a `ci` step's push. Its fields
/// are: how often
/// it asks, how long a commit may show no run before that is a gate,
/// how long it waits in all, and how long it waits after each network
/// fault before asking again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CiWait {
    pub interval: std::time::Duration,
    pub grace: std::time::Duration,
    pub timeout: std::time::Duration,
    /// One wait per fault the network may drop in a row; past the last
    /// one the run stops as a forge failure.
    pub retry: [std::time::Duration; 3],
}

impl Default for CiWait {
    fn default() -> Self {
        Self {
            interval: std::time::Duration::from_secs(20),
            grace: std::time::Duration::from_secs(180),
            timeout: std::time::Duration::from_secs(1800),
            retry: [
                std::time::Duration::from_secs(5),
                std::time::Duration::from_secs(15),
                std::time::Duration::from_secs(45),
            ],
        }
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
    ci_wait: CiWait,
    decide_auto_go: bool,
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
            ci_wait: CiWait::default(),
            decide_auto_go: true,
        })
    }

    /// Replace how long a `ci` step's push waits on CI.
    pub fn with_ci_wait(mut self, wait: CiWait) -> Self {
        self.ci_wait = wait;
        self
    }

    /// Turn the runner's own answer at a `decided_by` gate on or off.
    pub fn with_decide_auto_go(mut self, on: bool) -> Self {
        self.decide_auto_go = on;
        self
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
            NextMove::Pushed { step, .. } => self.after_pushed(&step).await,
            NextMove::Answered {
                gate,
                answer,
                amendment,
            } => self.answered(&gate, answer, amendment).await,
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
        if gate == BUDGET_GATE && answer == CheckpointAnswer::Raise {
            self.checked_raise(amendment.as_deref())?;
        }
        // Only a park that offered `raise` validates one; an unoffered word
        // is written and refused at the move, as at every other gate.
        if gate == STEP_STOP
            && answer == CheckpointAnswer::Raise
            && self.offered(STEP_STOP, "raise")?
            && let Some(step) = self.state()?.parked_step()
        {
            self.checked_step_raise(step, amendment.as_deref())?;
        }
        let payload = CheckpointAnsweredPayload {
            answer,
            amendment,
            marks: Vec::new(),
        };
        self.append_as(by, EventKind::CheckpointAnswered, &payload)
    }

    /// The figure a `raise` offers, as the budget gate accepts it.
    ///
    /// `Err` when the amendment carries no number, when the number is not
    /// above the figure in force, or when it would leave the run at or over
    /// the guard's line and park again at once. All three are the same
    /// refusal, naming the offer, the figure in force and the smallest
    /// figure that would clear the line; nothing is written, so the run
    /// stays parked and the person may answer again.
    fn checked_raise(&self, amendment: Option<&str>) -> Result<f64, RunnerError> {
        let state = self.state()?;
        let offered = amendment.unwrap_or_default();
        let refused = || RunnerError::RaiseRefused {
            offered: offered.to_owned(),
            budget: state.budget_usd,
            spent: state.cost_usd,
            // The same figure the ask names, so a person who copies
            // either one is not refused again.
            least: least_clearing_raise(&state),
        };
        let usd = budget_usd(&Value::String(offered.to_owned())).ok_or_else(refused)?;
        if usd > state.budget_usd && state.cost_usd < BUDGET_ASK_AT * usd {
            return Ok(usd);
        }
        Err(refused())
    }

    /// The figure a `raise` at `step_stop` offers `step`.
    ///
    /// `Err` when the amendment carries no number or the number is not
    /// above what the step has spent. A step's figure parks at its whole,
    /// not at a share of it, so nothing else is weighed here; the run's own
    /// guard still runs at the next move. Nothing is written on a refusal.
    fn checked_step_raise(&self, step: &str, amendment: Option<&str>) -> Result<f64, RunnerError> {
        let spent = self.state()?.step_spend(step);
        let offered = amendment.unwrap_or_default();
        let refused = || RunnerError::StepRaiseRefused {
            offered: offered.to_owned(),
            step: step.to_owned(),
            spent,
            least: next_cent_above(spent),
        };
        let usd = budget_usd(&Value::String(offered.to_owned())).ok_or_else(refused)?;
        if usd > spent {
            return Ok(usd);
        }
        Err(refused())
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
        if let Some(park) = self.budget_guard()? {
            return self.park_at_budget(park);
        }
        // A step that has already spent its own figure starts nothing: the
        // retry of a capped attempt lands here, and its spend has not moved
        // since the cap. A cap that came from the run's room is left to the
        // run's own guard above, and applied to the child when it exists.
        if let StepRoom::Cap { left, own: true } = self.step_room(&step, attempt, child)?
            && left <= 0.0
        {
            return self.park_at_step_stop(&step, attempt, left);
        }
        if step.checkpoint {
            let open = self.open_checkpoint(&step)?;
            if self.answers_itself(&step)? {
                // The answer is the log's, so a replay takes the same path;
                // the fold moves the run on at the next advance.
                self.append(
                    EventKind::CheckpointAnswered,
                    &CheckpointAnsweredPayload {
                        answer: CheckpointAnswer::Go,
                        amendment: None,
                        marks: Vec::new(),
                    },
                )?;
                return Ok(Advanced::Moved);
            }
            return Ok(open);
        }
        // The prompt is decided before any child exists, so a slot the
        // brief never reported stops the run here, not mid-step.
        match self.attempt_message(&step, attempt, None, child.is_none()) {
            Ok(_) => {}
            Err(err @ RunnerError::Workflow(_)) => {
                return self.escalate(
                    "render_failed",
                    vec![step.id.clone(), attempt.to_string(), err.to_string()],
                );
            }
            Err(other) => return Err(other),
        }
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
        // The move ends at `step_started`, so a watcher sees the step
        // begin; the next move finds the prompt missing and posts it.
        Ok(Advanced::Moved)
    }

    /// Whether the run must ask before it starts anything else, and what
    /// the ask says if so. Read only: the log's budget, its spend and its
    /// open gate.
    ///
    /// A figure of `0.0` (nothing declared one, or a workflow with no
    /// figures) parks nothing, so such a run behaves as it did before the
    /// gate existed.
    fn budget_guard(&self) -> Result<Option<Park>, RunnerError> {
        let state = self.state()?;
        if state.budget_usd <= 0.0 || state.cost_usd < BUDGET_ASK_AT * state.budget_usd {
            return Ok(None);
        }
        if state.pending_checkpoint.as_deref() == Some(BUDGET_GATE) {
            return Ok(Some(Park::AlreadyAsked));
        }
        Ok(Some(Park::Ask(spent_shown(&state, None))))
    }

    /// Act on what the guard found: write the ask, or report the one the
    /// log already holds. A raise is not a route to `go`: the run's only
    /// answers here are a larger figure and a stop.
    fn park_at_budget(&mut self, park: Park) -> Result<Advanced, RunnerError> {
        match park {
            Park::Ask(shown) => self.escalate_with(
                BUDGET_GATE,
                shown,
                vec!["raise".into(), "stop".into()],
                None,
            ),
            Park::AlreadyAsked => Ok(Advanced::WaitingHuman {
                gate: BUDGET_GATE.into(),
            }),
        }
    }

    /// The figure above which a brief's own proposal needs a person:
    /// `full × max_raise`. `max_raise = 0.0` declares no ceiling, and a
    /// `full` of `0.0` parks nothing, so both are `None` and a run with
    /// either takes its brief's figure as written.
    fn ceiling(&self) -> Option<f64> {
        let budget = &self.workflow.workflow.budget;
        (budget.full > 0.0 && budget.max_raise > 0.0).then_some(budget.full * budget.max_raise)
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
        if let Some(park) = self.budget_guard()? {
            return self.park_at_budget(park);
        }
        // The room is read again here, before the write lock and before
        // either child is built, so the cap the child's turn runs under is
        // derived from the log as it stands at this move.
        let room = self.step_room(&step, attempt, Some(child))?;
        if let Some(park) = self.park_if_spent(&step, attempt, &room)? {
            return Ok(park);
        }
        if step.writes
            && self.lock.is_none()
            && let Some(waiting) = self.take_write_lock()?
        {
            return Ok(waiting);
        }
        // One value for the whole attempt: the prompt's fold route and the
        // child's sanction must never disagree.
        let base = self.restack_base(&step)?;
        let posted = prompt_count(self.host.child_log(child)?.events());
        if self.prompt_due(&step, attempt, child)? {
            // The prompt is not in the child's log yet, whether the step
            // just started or a crash came first: post it once. A crash
            // before the child was created leaves it to create here, with
            // the id `step_started` names, never a fresh one.
            if !self.child_started(child)? {
                self.host.create_child(child, &step.id)?;
            }
            let message = self.attempt_message(&step, attempt, base.as_deref(), posted == 0)?;
            let mut runtime = self
                .host
                .build_child(child, &step.profile, &step.id, &step.deny)
                .await?
                .with_restack(base.clone());
            if let Some(park) = self
                .hold_to_room(&step, attempt, &mut runtime, &room)
                .await?
            {
                return Ok(park);
            }
            self.run_turn(&mut runtime, &message).await?;
            if prompt_count(self.host.child_log(child)?.events()) != posted + 1 {
                return Err(RunnerError::MessageNotPosted);
            }
        } else if !self.attempt_reported(child, attempt)? {
            let mut runtime = self
                .host
                .build_child(child, &step.profile, &step.id, &step.deny)
                .await?
                .with_restack(base);
            let mut observe = |_: Signal<'_>| {};
            if let crate::Resumed::Interrupted { .. } = runtime.resume(None, &mut observe)? {
                // The continuation is this attempt's second turn, and the
                // repairs the resume wrote are not calls: the room is read
                // again so the turn cannot run on spend the attempt has
                // already made.
                let room = self.step_room(&step, attempt, Some(child))?;
                if let Some(park) = self
                    .hold_to_room(&step, attempt, &mut runtime, &room)
                    .await?
                {
                    return Ok(park);
                }
                runtime.continue_turn(&mut observe).await?;
            }
        }
        self.apply_end(&step, attempt)?;
        Ok(Advanced::Moved)
    }

    /// Whether the child still owes this attempt a prompt. The prompts
    /// already in the child are counted against the `step_started` events
    /// naming that child with an attempt no later than this one: every
    /// attempt's own start names the child it runs in, so a fresh attempt
    /// whose child was prompted and whose turn never ended is resumed
    /// rather than prompted a second time.
    fn prompt_due(&self, step: &Step, attempt: u32, child: Ulid) -> Result<bool, RunnerError> {
        let owed = self
            .lead
            .events()
            .iter()
            .filter(|event| event.kind == EventKind::StepStarted)
            .filter_map(|event| {
                serde_json::from_value::<StepStartedPayload>(event.payload.clone()).ok()
            })
            .filter(|payload| {
                payload.step == step.id
                    && payload.child_thread == child
                    && payload.attempt <= attempt
            })
            .count() as u32;
        let posted = prompt_count(self.host.child_log(child)?.events());
        Ok(posted < owed)
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
                        STEP_STOP,
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
                        STEP_STOP,
                        vec![step.id.clone(), attempt.to_string(), reason],
                    )
                }
            }
            StepStatus::Failed => {
                let reason = self.end_reason(step_id, attempt)?;
                self.escalate(
                    STEP_STOP,
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
            "done" => {
                let note = self.close_after_an_approve()?;
                self.finish(RunOutcome::Closed, note)
            }
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
            return self.follow_next(step, attempt);
        };
        let slot = |name: &str| {
            report
                .slots
                .as_ref()
                .and_then(|slots| slots.get(name))
                .cloned()
        };
        let size = match slot(&branch).and_then(|v| v.as_str().map(str::to_owned)) {
            Some(size) => size,
            None => match verdict_route(&branch, report) {
                Some(size) => size,
                None => {
                    return self.escalate(
                        "route_escalate",
                        vec![
                            step.id.clone(),
                            attempt.to_string(),
                            missing_reason(&branch),
                        ],
                    );
                }
            },
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
        // A route carries the budget the routing step reported, when it
        // reported one; a step that sizes nothing has none to carry. A
        // proposal above the ceiling is not taken as the run's figure: the
        // route is still recorded, so a replay follows it rather than
        // re-taking it, but the figure in force stands and the same move
        // parks at the budget gate.
        let proposed = slot("budget").as_ref().and_then(budget_usd);
        let over = match (proposed, self.ceiling()) {
            (Some(usd), Some(ceiling)) if usd > ceiling => Some((usd, ceiling)),
            _ => None,
        };
        let in_force = self.state()?.budget_usd;
        self.append(
            EventKind::RouteTaken,
            &RouteTakenPayload {
                branch,
                proposed: size,
                taken,
                preconditions: Vec::new(),
                fallback_reason: None,
                budget_usd: match over {
                    Some(_) => Some(in_force),
                    None => proposed,
                },
            },
        )?;
        match over {
            Some((usd, ceiling)) => self.escalate_with(
                BUDGET_GATE,
                spent_shown(&self.state()?, Some((usd, ceiling))),
                vec!["raise".into(), "stop".into()],
                None,
            ),
            None => Ok(Advanced::Moved),
        }
    }

    /// A step with no route continues to its `next`: `done` ends the run,
    /// a step id is written as a `route_taken` so replay follows it, and
    /// a missing `next` is a gate, never a silent stop.
    fn follow_next(&mut self, step: &Step, attempt: u32) -> Result<Advanced, RunnerError> {
        match step.next.as_deref() {
            Some("done") => self.finish(RunOutcome::Closed, None),
            Some(next) => {
                self.append(
                    EventKind::RouteTaken,
                    &RouteTakenPayload {
                        branch: "next".into(),
                        proposed: next.to_owned(),
                        taken: next.to_owned(),
                        preconditions: Vec::new(),
                        fallback_reason: None,
                        budget_usd: None,
                    },
                )?;
                Ok(Advanced::Moved)
            }
            None => self.escalate(
                "no_next_step",
                vec![
                    step.id.clone(),
                    attempt.to_string(),
                    format!("`{}` names no `next` and no route", step.id),
                ],
            ),
        }
    }

    /// Open a checkpoint step: show its rendered template and wait for
    /// `go`, `amend` or `stop`. Writing the ask is the whole move, so a
    /// crash before it repeats it and a crash after it waits.
    fn open_checkpoint(&mut self, step: &Step) -> Result<Advanced, RunnerError> {
        let shown = match self.attempt_message(step, 1, None, true) {
            Ok(text) => text,
            Err(err @ RunnerError::Workflow(_)) => {
                return self.escalate(
                    "render_failed",
                    vec![step.id.clone(), "1".into(), err.to_string()],
                );
            }
            Err(other) => return Err(other),
        };
        self.escalate_with(
            &step.id.clone(),
            vec![shown],
            vec!["go".into(), "amend".into(), "stop".into()],
            None,
        )
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
                STEP_STOP,
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
            root: self.repo.root(),
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
                self.escalate_with(
                    "checks_failed",
                    shown,
                    vec!["retry".to_owned(), "stop".to_owned()],
                    None,
                )
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
            match self.repo.push(&branch) {
                Ok(()) => {}
                Err(err) => {
                    // A plain push is refused when the remote has commits we lack.
                    // The remote's own head says whether it moved, whatever git's
                    // wording was: "non-fast-forward" and "fetch first" name it
                    // differently across versions, and a hook or an unreachable
                    // remote errors with the remote where it stood.
                    let moved = match self.repo.remote_head(&branch) {
                        Ok(Some(now)) if now != start.remote && now != head => Some(now),
                        _ => None,
                    };
                    let mut shown = vec![step.id.clone()];
                    if let Some(now) = moved {
                        shown.push(format!(
                            "remote moved during the push: {}..{now}",
                            start.remote
                        ));
                    }
                    shown.push(err.to_string());
                    return self.escalate("push_error", shown);
                }
            }
        }
        let installed = if step.install {
            let installed = self.installer.install(&self.repo)?;
            let short = &head[..head.len().min(12)];
            if !installed.contains(short) {
                return self.escalate(
                    "install_mismatch",
                    vec![step.id.clone(), installed, head.clone()],
                );
            }
            Some(installed)
        } else {
            None
        };
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
                installed,
            },
        )?;
        Ok(Advanced::Moved)
    }

    /// After the push is written: wait for CI when the step asks, then
    /// continue to `next`, or close the issue and end the run when `next`
    /// is `done`. Every part is safe to repeat: CI is read again, a
    /// `route_taken` is the next move's own record, and a rebuild re-reads
    /// the issue's comments and never posts the closing comment twice.
    async fn after_pushed(&mut self, step_id: &str) -> Result<Advanced, RunnerError> {
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
        if step.ci
            && let Some(gate) = self.wait_for_ci(&step).await?
        {
            return Ok(gate);
        }
        match step.next.as_deref() {
            Some("done") => {}
            Some(_) => return self.follow_next(&step, 1),
            None => {
                return self.escalate(
                    "no_next_step",
                    vec![
                        step.id.clone(),
                        format!("`{}` is the last step and names no `next`", step.id),
                    ],
                );
            }
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
        self.finish_with(RunOutcome::Closed, impact, None)
    }

    /// Wait on CI for the commit the last push left at the remote. `None`
    /// means it passed; `Some` is the gate a red, missing or too-slow CI
    /// opened. The wait writes nothing, so a restart simply asks again.
    async fn wait_for_ci(&mut self, step: &Step) -> Result<Option<Advanced>, RunnerError> {
        let Some(sha) = self.last_pushed_head() else {
            return self
                .escalate(
                    "ci_missing",
                    vec![step.id.clone(), "no push names a commit to wait on".into()],
                )
                .map(Some);
        };
        let wait = self.ci_wait;
        let began = std::time::Instant::now();
        // Only while the network keeps failing: a read that answers,
        // whatever it answers, starts the ladder again, so an occasional
        // blip over a half-hour wait is patient rather than fatal.
        let mut faults = 0usize;
        loop {
            let waited = began.elapsed();
            let state = match self.forge.ci(&sha) {
                Ok(state) => {
                    faults = 0;
                    state
                }
                Err(failure) if failure.is_network_fault() => {
                    let Some(delay) = wait.retry.get(faults) else {
                        return Err(RunnerError::ForgeExhausted {
                            tries: faults + 1,
                            failure,
                        });
                    };
                    faults += 1;
                    tokio::time::sleep(*delay).await;
                    continue;
                }
                Err(failure) => return Err(failure.into()),
            };
            match state {
                CiState::Passed => return Ok(None),
                CiState::Failed(why) => {
                    return self
                        .escalate("ci_failed", vec![step.id.clone(), sha, why])
                        .map(Some);
                }
                CiState::NoRuns if waited >= wait.grace => {
                    return self
                        .escalate(
                            "ci_missing",
                            vec![
                                step.id.clone(),
                                sha,
                                format!("no CI run named the commit in {}s", wait.grace.as_secs()),
                            ],
                        )
                        .map(Some);
                }
                CiState::NoRuns | CiState::Pending => {}
            }
            if waited >= wait.timeout {
                return self
                    .escalate(
                        "ci_timeout",
                        vec![
                            step.id.clone(),
                            sha,
                            format!("CI had not finished after {}s", wait.timeout.as_secs()),
                        ],
                    )
                    .map(Some);
            }
            tokio::time::sleep(wait.interval).await;
        }
    }

    /// The commit the last `pushed` left at the remote.
    fn last_pushed_head(&self) -> Option<String> {
        self.lead
            .events()
            .iter()
            .rev()
            .filter(|event| event.kind == EventKind::Pushed)
            .find_map(|event| {
                serde_json::from_value::<PushedPayload>(event.payload.clone())
                    .ok()
                    .map(|pushed| pushed.ref_after)
            })
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

    /// The base a writing step may fold its own commits back to: the head
    /// its first attempt recorded, and only while the branch's remote ref
    /// still sits where that attempt found it. Nothing of the step's work
    /// has been pushed then — the same rule its push follows — so a soft
    /// reset onto that head rewrites the step's own commits and nothing
    /// else.
    ///
    /// `None`, without reading git at all, for a step that does not write,
    /// for a step with no start record (a run begun before the record
    /// existed), and for a remote that has moved. A git read that fails is
    /// the runner's own error, as the reads in `start_step` already are.
    fn restack_base(&self, step: &Step) -> Result<Option<String>, RunnerError> {
        if !step.writes {
            return Ok(None);
        }
        let StartRecord::Ready { head, remote } = self.start_record(&step.id)? else {
            return Ok(None);
        };
        if self.repo.remote_head(&self.repo.branch()?)? == Some(remote) {
            return Ok(Some(head));
        }
        Ok(None)
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
    /// between — a budget warning, say — does not change it, and neither
    /// does the open attempt's own `step_started`, so the prompt reads the
    /// same whether it is posted before or after that event.
    fn send_back_due(&self, step_id: &str) -> bool {
        let mut state = self.lead.events().iter().rev().filter(|event| {
            matches!(
                event.kind,
                EventKind::StepStarted | EventKind::StepFinished | EventKind::ChecksRun
            )
        });
        let mut latest = state.next();
        if latest.is_some_and(|event| event.kind == EventKind::StepStarted) {
            latest = state.next();
        }
        latest
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

    /// Close the issue when the route just taken was an `approve` to the
    /// end, and say why the issue is still open when the close failed.
    ///
    /// It reads the route's `proposed`, the reported word, rather than the
    /// step's `route_by`, so a step may route on a slot named anything.
    /// A close that fails does not fail the run: the work is done, and the
    /// person is owed the reason it is not closed.
    fn close_after_an_approve(&self) -> Result<Option<String>, RunnerError> {
        let state = self.state()?;
        if !state
            .routes
            .last()
            .is_some_and(|route| route.proposed == "approve")
        {
            return Ok(None);
        }
        Ok(match self.forge.close(state.issue) {
            Ok(()) => None,
            Err(err) => Some(format!("the issue was not closed: {err}")),
        })
    }

    /// Write `run_finished` and end the run, with a note on why the issue
    /// is still open when a close failed.
    fn finish(
        &mut self,
        outcome: RunOutcome,
        note: Option<String>,
    ) -> Result<Advanced, RunnerError> {
        self.finish_with(outcome, None, note)
    }

    /// Write `run_finished` with the closing comment's release impact and a
    /// note on a close that failed, and end the run.
    fn finish_with(
        &mut self,
        outcome: RunOutcome,
        release_impact: Option<ReleaseImpact>,
        note: Option<String>,
    ) -> Result<Advanced, RunnerError> {
        let cost_usd = self.cost();
        self.append(
            EventKind::RunFinished,
            &RunFinishedPayload {
                outcome,
                cost_usd,
                release_impact,
                note,
            },
        )?;
        self.lock = None;
        Ok(Advanced::Finished { outcome })
    }

    /// Note that the run's turn ended in an error, without ending the run.
    ///
    /// One event per error: a second call before the next `step_started`
    /// or `run_finished` appends nothing, because the fold's own window —
    /// the newest `run_errored` since either — already holds this one. A
    /// log the fold cannot read writes nothing and returns that error.
    pub fn note_run_error(&mut self, reason: &str) -> Result<(), RunnerError> {
        let state = self.state()?;
        if state.errored.is_some() {
            return Ok(());
        }
        self.append(
            EventKind::RunErrored,
            &RunErroredPayload {
                reason: reason.to_owned(),
                cost_usd: state.cost_usd,
            },
        )?;
        Ok(())
    }

    /// A human answered a gate: `stop` ends the run. At a checkpoint step,
    /// `go` and `amend` continue to its `next` (the amendment is already in
    /// the answer, where the `amendment` slot reads it). At any other gate
    /// the runner raised itself, only `stop` is an answer, except `retry`
    /// at `checks_failed`, which grants the step one more attempt; at the
    /// budget gate a `raise` writes the larger figure and continues.
    async fn answered(
        &mut self,
        gate: &str,
        answer: CheckpointAnswer,
        amendment: Option<String>,
    ) -> Result<Advanced, RunnerError> {
        if answer == CheckpointAnswer::Stop {
            return self.finish(RunOutcome::Stopped, None);
        }
        if gate == BUDGET_GATE && answer == CheckpointAnswer::Raise {
            // A replay reaches this arm when an earlier runner wrote the
            // answer, so the figure is checked here too: a refusal writes
            // nothing and leaves the run parked.
            let usd = self.checked_raise(amendment.as_deref())?;
            self.append(
                EventKind::BudgetRaised,
                &BudgetRaisedPayload { usd, step: None },
            )?;
            // The move this raise unblocked is the one the fold derives
            // from here: the next `advance` runs it.
            return Ok(Advanced::Moved);
        }
        if gate == STEP_STOP
            && answer == CheckpointAnswer::Raise
            && self.offered(STEP_STOP, "raise")?
        {
            let Some(step) = self.state()?.parked_step().map(str::to_owned) else {
                return Err(RunnerError::SliceTwo {
                    gate: gate.to_owned(),
                });
            };
            let usd = self.checked_step_raise(&step, amendment.as_deref())?;
            self.append(
                EventKind::BudgetRaised,
                &BudgetRaisedPayload {
                    usd,
                    step: Some(step),
                },
            )?;
            // The fold gives back the move the park interrupted, which
            // starts the step's next attempt under the raised figure.
            return Ok(Advanced::Moved);
        }
        let checkpoint = self
            .workflow
            .workflow
            .steps
            .iter()
            .find(|step| step.checkpoint && step.id == gate)
            .cloned();
        if let Some(step) = checkpoint {
            return self.follow_next(&step, 1);
        }
        if answer == CheckpointAnswer::Retry && gate == "checks_failed" {
            return self.retry_attempt().await;
        }
        if answer == CheckpointAnswer::Retry
            && gate == STEP_STOP
            && self.offered(STEP_STOP, "retry")?
        {
            return self.retry_failed_step().await;
        }
        Err(RunnerError::SliceTwo {
            gate: gate.to_owned(),
        })
    }

    /// Whether the last ask at `gate` offered `option`. Options are what a
    /// client may answer with, so a word the standing ask never offered is
    /// not an answer to this park. A gate with no ask on record, or an ask
    /// whose line cannot be read, offers nothing: the move is refused
    /// rather than guessed at.
    fn offered(&self, gate: &str, option: &str) -> Result<bool, RunnerError> {
        let asked = self
            .lead
            .events()
            .iter()
            .rev()
            .filter(|event| event.kind == EventKind::CheckpointAsked)
            .filter_map(|event| {
                serde_json::from_value::<CheckpointAskedPayload>(event.payload.clone()).ok()
            })
            .find(|payload| payload.gate == gate);
        Ok(asked.is_some_and(|payload| payload.options.iter().any(|held| held == option)))
    }

    /// Start one fresh attempt of the step whose last attempt died on a
    /// recoverable provider fault. The fault said nothing about the step, so
    /// the new attempt runs in a child the step has never used and is
    /// prompted with the whole task instead of an instruction. Both the
    /// step and its attempt come from the log, so a rebuild between the
    /// answer and this move repeats it exactly.
    async fn retry_failed_step(&mut self) -> Result<Advanced, RunnerError> {
        let step = self
            .lead
            .events()
            .iter()
            .rev()
            .filter(|event| event.kind == EventKind::StepFinished)
            .filter_map(|event| {
                serde_json::from_value::<StepFinishedPayload>(event.payload.clone()).ok()
            })
            .next()
            .filter(|payload| payload.status == StepStatus::Failed)
            .ok_or_else(|| RunnerError::Host("step_stop with no failed step to retry".into()))?
            .step;
        let attempt = self
            .lead
            .events()
            .iter()
            .rev()
            .filter(|event| event.kind == EventKind::StepStarted)
            .filter_map(|event| {
                serde_json::from_value::<StepStartedPayload>(event.payload.clone()).ok()
            })
            .find(|payload| payload.step == step)
            .ok_or_else(|| RunnerError::Host(format!("step_stop with no step_started for {step}")))?
            .attempt;
        self.start_step(&step, attempt + 1, None).await
    }

    /// Grant one more attempt to the step whose checks last failed: the
    /// answer's move is a fresh `step_started` for the same child, one
    /// attempt on. Both the step and the attempt come from the log, so a
    /// rebuild between the answer and this move repeats it exactly. An ask
    /// can only stand after a run of the step, so a missing event is a
    /// broken lead, not a state to guess at.
    async fn retry_attempt(&mut self) -> Result<Advanced, RunnerError> {
        let step = self
            .lead
            .events()
            .iter()
            .rev()
            .filter(|event| event.kind == EventKind::ChecksRun)
            .filter_map(|event| {
                serde_json::from_value::<ChecksRunPayload>(event.payload.clone()).ok()
            })
            .next()
            .ok_or_else(|| RunnerError::Host("checks_failed with no checks_run to retry".into()))?
            .step;
        let attempt = self
            .lead
            .events()
            .iter()
            .rev()
            .filter(|event| event.kind == EventKind::StepStarted)
            .filter_map(|event| {
                serde_json::from_value::<StepStartedPayload>(event.payload.clone()).ok()
            })
            .find(|payload| payload.step == step)
            .ok_or_else(|| {
                RunnerError::Host(format!("checks_failed with no step_started for {step}"))
            })?
            .attempt;
        let child = self.child_of(&step, attempt)?;
        self.start_step(&step, attempt + 1, Some(child)).await
    }

    /// Write `checkpoint_asked` and hand the run to the human. This is the
    /// one act safe to repeat after a crash.
    fn escalate(&mut self, gate: &str, shown: Vec<String>) -> Result<Advanced, RunnerError> {
        self.escalate_with(gate, shown, vec!["stop".into()], None)
    }

    /// `escalate`, offering `options` instead of `stop` alone. An ask that
    /// offers a raise carries the figure the picker prefills: `raise_usd`
    /// when the caller names one, else the budget gate's clearing figure,
    /// and none at any other gate.
    fn escalate_with(
        &mut self,
        gate: &str,
        shown: Vec<String>,
        options: Vec<String>,
        raise_usd: Option<f64>,
    ) -> Result<Advanced, RunnerError> {
        let raise_usd = match raise_usd {
            Some(usd) => Some(usd),
            None if gate == BUDGET_GATE => Some(least_clearing_raise(&self.state()?)),
            None => None,
        };
        self.append(
            EventKind::CheckpointAsked,
            &CheckpointAskedPayload {
                gate: gate.into(),
                shown,
                options,
                raise_usd,
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

    /// The prompt an attempt gets: the step's template rendered for the
    /// first, a short instruction afterwards, and the template again with
    /// the failing checks when a send-back brings the step back. The render
    /// can fail, and a failed render is a `render_failed` escalation.
    ///
    /// `head_at_start` is the base the send-back may fold onto, handed in
    /// by the caller so the prompt and the armed overlay carry one value:
    /// the send-back offers the route only when it is `Some`.
    ///
    /// `fresh` says the child this attempt speaks into holds no prompt yet,
    /// so it has no work to be told to continue. An instruction only fits a
    /// child that holds the task already.
    fn attempt_message(
        &self,
        step: &Step,
        attempt: u32,
        head_at_start: Option<&str>,
        fresh: bool,
    ) -> Result<String, RunnerError> {
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
            if let Some(base) = head_at_start {
                slots.insert("head_at_start".to_string(), Value::from(base));
            }
            return Ok(self.workflow.render(&step.id, &slots)?);
        }
        if fresh || attempt <= 1 {
            let slots = self.slot_map(step)?;
            return Ok(self.workflow.render(&step.id, &slots)?);
        }
        let previous = self.end_reason(&step.id, attempt - 1)?;
        Ok(match previous.as_str() {
            // It stopped on its own or was picked up without reporting.
            "done" | "resumed" => CALL_FINISH_STEP.to_owned(),
            REPORT_WITHOUT_BODY => REPORT_AGAIN_PROMPT.to_owned(),
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
        // `usage`: `payload["cost_usd"]` at the top level is always absent.
        let cost_usd: f64 = call_spend(tail);
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
                // Nothing to post: the attempt is partial, so the step is
                // asked to report again once and escalates the second time.
                let finished = finish(StepStatus::Partial, REPORT_WITHOUT_BODY.into(), None);
                self.append(EventKind::StepFinished, &finished)?;
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
            "done" | "resumed" | LENGTH_STOP | MAX_USD | "max_iterations" | "max_tokens"
            | "max_wall_time" => {
                let finished = finish(StepStatus::Partial, end.reason, None);
                self.append(EventKind::StepFinished, &finished)?;
                Ok(())
            }
            other => {
                let finished = finish(StepStatus::Failed, other.to_owned(), None);
                self.append(EventKind::StepFinished, &finished)?;
                let shown = vec![step.id.clone(), attempt.to_string(), other.to_owned()];
                // A fault a fresh call could get past is one a fresh
                // attempt could too, so the person is offered it. A tail
                // whose `turn_ended` carries no error (a log written
                // before that field) keeps `stop` alone: the offer is
                // never guessed.
                match end.error.as_ref() {
                    Some(fault) if recoverable_fault(fault) => {
                        self.escalate_with(
                            STEP_STOP,
                            shown,
                            vec!["retry".to_owned(), "stop".to_owned()],
                            None,
                        )?;
                    }
                    _ => {
                        self.escalate(STEP_STOP, shown)?;
                    }
                }
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

    /// The report a step's latest attempt made, whatever its number: a
    /// decider that a check sent back and that re-ran is read from what it
    /// said last, and a last attempt that reported nothing reads as nothing.
    fn last_report(&self, step_id: &str) -> Result<Option<StepReport>, RunnerError> {
        let state = self.state()?;
        let Some(attempt) = state
            .steps
            .iter()
            .rev()
            .find(|record| record.step == step_id)
            .map(|record| record.attempt)
        else {
            return Ok(None);
        };
        Ok(self.report_of(step_id, attempt)?.map(|(report, _)| report))
    }

    /// Whether the runner answers this gate itself: the project's switch on,
    /// a decider whose latest report proposed `go`, and no choice among its
    /// items.
    fn answers_itself(&self, step: &Step) -> Result<bool, RunnerError> {
        if !self.decide_auto_go {
            return Ok(false);
        }
        let Some(decider) = step.decided_by.as_deref() else {
            return Ok(false);
        };
        if self.step(decider).is_err() {
            return Ok(false);
        }
        let Some(decide) = self.last_report(decider)?.and_then(|report| report.decide) else {
            return Ok(false);
        };
        Ok(decide.proposal == DecideProposal::Go
            && decide
                .items
                .iter()
                .all(|item| item.kind == DecideItemKind::Fact))
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

/// How much a step's turn may still spend.
enum StepRoom {
    /// Neither the step nor the run declares a figure.
    NoCap,
    /// What is left of the cap, and whether the cap is the step's own
    /// figure. The two differ in where a spent cap parks: a step over its
    /// own figure asks at `step_stop`, one over the run's room at the
    /// run's own `budget` gate.
    Cap { left: f64, own: bool },
}

impl<F: Forge, H: RunnerHost, R: Repo> Runner<F, H, R> {
    /// What a step's turn is held to: its own figure when it declares a
    /// usable one, else the run's room to its ask line, else nothing.
    ///
    /// `left` is what remains, so a figure already spent is `0.0` or less
    /// and starts no turn. The in-flight attempt's own spend is subtracted
    /// when it is known: its calls are in neither the run's fold (nothing
    /// closes an open attempt) nor, for the step's figure, its closed
    /// attempts — so without that term a resumed attempt would be granted
    /// its whole remainder again. A price table is what makes a cap
    /// reachable at all: an unpriced call adds nothing to either spend, so
    /// `left` never falls.
    fn step_room(
        &self,
        step: &Step,
        attempt: u32,
        child: Option<Ulid>,
    ) -> Result<StepRoom, RunnerError> {
        let state = self.state()?;
        let own = step
            .budget
            .filter(|figure| figure.is_finite() && *figure > 0.0);
        // An unposted prompt spends nothing, so a missing child, a child
        // with no prompt for this attempt and an unstarted attempt all
        // agree on zero.
        let in_flight = match child {
            Some(child) => self.attempt_spend(child, attempt)?,
            None => 0.0,
        };
        match own {
            Some(figure) => Ok(StepRoom::Cap {
                left: figure - state.step_spend(&step.id) - in_flight,
                own: true,
            }),
            None if state.budget_usd > 0.0 => Ok(StepRoom::Cap {
                // The room is the run's own remainder: the step's closed
                // attempts are already part of `state.cost_usd`, so only
                // the open attempt's spend is new.
                left: BUDGET_ASK_AT * state.budget_usd - state.cost_usd - in_flight,
                own: false,
            }),
            None => Ok(StepRoom::NoCap),
        }
    }

    /// What an attempt has spent so far: the same sum `apply_end` writes
    /// into `step_finished.cost_usd`, read from the child's log between
    /// this attempt's prompt and the log's end. An attempt whose prompt is
    /// not posted has spent nothing.
    fn attempt_spend(&self, child: Ulid, attempt: u32) -> Result<f64, RunnerError> {
        let events = self.host.child_log(child)?;
        let events = events.events();
        Ok(match prompt_at(events, attempt) {
            Some(from) => call_spend(&events[from..]),
            None => 0.0,
        })
    }

    /// Hold a child's turn to `room`: the cap rides on the child's
    /// `Budget`, and the step's own figure is what `step_started` recorded
    /// for it. Returns the park when the cap is spent, `None` when the
    /// turn may run.
    async fn hold_to_room(
        &mut self,
        step: &Step,
        attempt: u32,
        runtime: &mut Runtime,
        room: &StepRoom,
    ) -> Result<Option<Advanced>, RunnerError> {
        let left = match room {
            StepRoom::NoCap => {
                // Nothing declares a cap, so clear whatever the child
                // carries: a figure a test or an earlier attempt set must
                // not linger in a rebuilt child.
                let mut budget = *runtime.budget();
                budget.max_cost_usd = None;
                runtime.set_budget(budget);
                return Ok(None);
            }
            StepRoom::Cap { left, .. } => *left,
        };
        let mut budget = *runtime.budget();
        budget.max_cost_usd = (left > 0.0).then_some(left);
        runtime.set_budget(budget);
        if left > 0.0 {
            return Ok(None);
        }
        self.park_if_spent(step, attempt, room)
    }

    /// Park a spent cap, at the gate its source asks for: the step's own
    /// figure at `step_stop`, the run's room at the run's own gate, where
    /// a raise can free it.
    fn park_if_spent(
        &mut self,
        step: &Step,
        attempt: u32,
        room: &StepRoom,
    ) -> Result<Option<Advanced>, RunnerError> {
        match room {
            StepRoom::Cap { left, own } if *left <= 0.0 => {
                if *own {
                    self.park_at_step_stop(step, attempt, *left).map(Some)
                } else {
                    let park = match self.state()?.pending_checkpoint.as_deref() {
                        Some(BUDGET_GATE) => Park::AlreadyAsked,
                        _ => Park::Ask(spent_shown(&self.state()?, None)),
                    };
                    self.park_at_budget(park).map(Some)
                }
            }
            _ => Ok(None),
        }
    }

    /// Park a step that has spent its own figure, offering `raise` and
    /// `stop`. A raise records this step's figure for the rest of its visit
    /// and leaves the run's alone; the prefill is the figure plus
    /// `STEP_RAISE_BY`, or the first cent above the spend when a call cut
    /// mid-way left the spend past that.
    fn park_at_step_stop(
        &mut self,
        step: &Step,
        attempt: u32,
        left: f64,
    ) -> Result<Advanced, RunnerError> {
        let figure = step.budget.unwrap_or(0.0);
        let shown = vec![
            step.id.clone(),
            attempt.to_string(),
            format!("spent {:.2} of its {:.2} figure", figure - left, figure),
        ];
        let prefill =
            (figure + STEP_RAISE_BY).max(next_cent_above(self.state()?.step_spend(&step.id)));
        self.escalate_with(
            STEP_STOP,
            shown,
            vec!["raise".into(), "stop".into()],
            Some(prefill),
        )
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

/// What the calls in `events` cost: the price the child's runtime stamped
/// on each `assistant_message`'s `usage`. The fold starts from a positive
/// zero so a run with no prices reads `0.0`, not `-0.0`.
fn call_spend(events: &[Event]) -> f64 {
    events
        .iter()
        .filter(|e| e.kind == EventKind::AssistantMessage)
        .filter_map(|e| {
            e.payload
                .get("usage")
                .and_then(|usage| usage.get("cost_usd"))
                .and_then(Value::as_f64)
        })
        .fold(0.0_f64, |total, cost| total + cost)
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

/// The smallest whole number of cents strictly above `spend`: the least
/// figure a `step_stop` raise is accepted at. A spend that is not finite
/// counts as zero.
pub fn next_cent_above(spend: f64) -> f64 {
    let spend = if spend.is_finite() { spend } else { 0.0 };
    let mut cents = (spend * 100.0).floor() + 1.0;
    // A spend whose product rounds a hair low (0.29 × 100) lands back on
    // itself after the division, so the walk steps past it.
    while cents / 100.0 <= spend {
        cents += 1.0;
    }
    cents / 100.0
}

/// The smallest whole number of cents that clears the budget gate: above
/// the budget in force, and leaving the spend under the ask line the guard
/// reads. The walk tests the guard's own comparison, so a figure it would
/// read as *at* the line — a float product that rounds to the spend
/// exactly — is stepped past rather than offered.
fn least_clearing_raise(state: &RunState) -> f64 {
    let from_line = ((state.cost_usd / BUDGET_ASK_AT) * 100.0).floor();
    let from_budget = (state.budget_usd * 100.0).floor();
    let mut cents = if from_line.is_finite() {
        from_line
    } else {
        from_budget
    }
    .max(from_budget)
    .max(1.0);
    // The start is at most a couple of cents under the figure, so the cap
    // is reached only by a start JSON cannot record.
    for _ in 0..1_000 {
        let usd = cents / 100.0;
        if usd > state.budget_usd && state.cost_usd < BUDGET_ASK_AT * usd {
            return usd;
        }
        cents += 1.0;
    }
    (cents - 1.0) / 100.0
}

/// What the budget ask tells the person: what the run has spent, the
/// figure in force, what a raise takes to clear the gate, and — when a
/// brief's proposal opened the gate — that proposal and the ceiling it
/// crossed. The figures are formatted rather than ranged so one line
/// states them all.
fn spent_shown(state: &RunState, proposal: Option<(f64, f64)>) -> Vec<String> {
    let mut shown = vec![format!(
        "spent {:.2} of the run's {:.2} budget",
        state.cost_usd, state.budget_usd
    )];
    if let Some((offered, ceiling)) = proposal {
        shown.push(format!(
            "the brief proposed {:.2}, above the ceiling of {:.2}",
            offered, ceiling
        ));
    }
    shown.push(format!(
        "a raise of at least {:.2} clears the gate: /answer raise <usd>",
        least_clearing_raise(state)
    ));
    shown
}

/// The value a `verdict` branch routes on when the report has no `verdict`
/// slot: `finish_step`'s schema spells the same fact as a top-level field
/// (`approve`, `changes_needed`), the loop's routes on `approve` and
/// `changes`. Only `verdict` has two spellings; every other branch reads its
/// slot alone.
fn verdict_route(branch: &str, report: &StepReport) -> Option<String> {
    if branch != "verdict" {
        return None;
    }
    match report.verdict? {
        Verdict::Approve => Some("approve".to_owned()),
        Verdict::ChangesNeeded => Some("changes".to_owned()),
    }
}

/// What a `route_escalate` gate tells the person is missing; a `verdict`
/// branch looks for two spellings, so its message names both.
fn missing_reason(branch: &str) -> String {
    let mut reason = format!("the report has no `{branch}` slot");
    if branch == "verdict" {
        reason.push_str(&format!(" and no `{branch}` field"));
    }
    reason
}

#[cfg(test)]
mod tests {
    use super::{CiWait, StepReport, Verdict, budget_usd, verdict_route};
    use serde_json::json;

    #[test]
    fn the_ci_wait_retries_after_5_15_and_45_seconds() {
        let wait = CiWait::default();
        assert_eq!(
            wait.retry,
            [
                std::time::Duration::from_secs(5),
                std::time::Duration::from_secs(15),
                std::time::Duration::from_secs(45),
            ],
            "one fault each, then the wait gives up"
        );
        assert_eq!(wait.interval, std::time::Duration::from_secs(20));
        assert_eq!(wait.grace, std::time::Duration::from_secs(180));
        assert_eq!(wait.timeout, std::time::Duration::from_secs(1800));
    }

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

    /// Only a `verdict` branch reads the report's top-level field, and only
    /// the enum's two values map.
    #[test]
    fn only_the_verdict_branch_reads_the_verdict_field() {
        for (verdict, expected) in [
            (Verdict::Approve, "approve"),
            (Verdict::ChangesNeeded, "changes"),
        ] {
            let report = StepReport {
                verdict: Some(verdict),
                ..Default::default()
            };
            assert_eq!(verdict_route("verdict", &report).as_deref(), Some(expected));
            assert_eq!(verdict_route("size", &report), None);
            assert_eq!(verdict_route("fix", &report), None);
        }
        assert_eq!(verdict_route("verdict", &StepReport::default()), None);
    }
}
