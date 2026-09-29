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

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use aigentic_core::{AgentId, Author, ContentBlock, Event, EventKind};
use aigentic_log::{
    CheckpointAskedPayload, LogError, NewEvent, NextMove, PlannedTest, ReportStatus,
    RouteTakenPayload, RunFinishedPayload, RunOutcome, RunStartedPayload, RunState,
    StepFinishedPayload, StepReport, StepStartedPayload, StepStatus, ThreadLog, TurnEndedPayload,
    run_state,
};
use serde_json::Value;
use ulid::Ulid;

use crate::workflow::{LoadedWorkflow, Step, WorkflowError, render::trailer_model};
use crate::{Runtime, RuntimeError, STEP_REPORTED, Signal};

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
// `PausedAtChecks` carries the whole report: the caller needs it, and the
// other variants are zero-sized, so the difference is not worth a box.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum Advanced {
    /// A move was made; call `advance` again.
    Moved,
    /// A gate is open and unanswered: wait for the human.
    WaitingHuman {
        /// The gate that was asked.
        gate: String,
    },
    /// The step's checks are #65's: the report is where the runner hands
    /// over, and the checks have not run.
    PausedAtChecks {
        /// The step whose checks are due.
        step: String,
        /// Its report.
        report: StepReport,
    },
    /// The run ended.
    Finished {
        /// How it ended.
        outcome: RunOutcome,
    },
}

/// One run's lead thread: the log it appends to, and the seams it drives
/// children and the forge through.
pub struct Runner<F: Forge, H: RunnerHost> {
    lead: ThreadLog,
    lead_id: Ulid,
    forge: F,
    host: H,
    workflow: LoadedWorkflow,
    repo: PathBuf,
    lock: Option<WriteGuard>,
}

impl<F: Forge, H: RunnerHost> Runner<F, H> {
    /// A runner over a log the daemon already wrote `run_started` to.
    pub fn new(
        lead: ThreadLog,
        lead_id: Ulid,
        forge: F,
        host: H,
        workflow: LoadedWorkflow,
        repo: PathBuf,
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
            NextMove::ChecksDone { .. } | NextMove::Pushed { .. } | NextMove::Answered { .. } => {
                Err(RunnerError::OwnedBy65 {
                    what: "checks, push, install",
                })
            }
        }
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
            && let Some(waiting) = self.take_write_lock(&step)?
        {
            return Ok(waiting);
        }
        let fresh = child.is_none();
        let child = child.unwrap_or_else(|| self.host.new_child_id());
        self.append(
            EventKind::StepStarted,
            &StepStartedPayload {
                step: step.id.clone(),
                role: step.role.clone(),
                profile: step.profile.clone(),
                child_thread: child,
                attempt,
                budget_usd: step.budget.unwrap_or(0.0),
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
            && let Some(waiting) = self.take_write_lock(&step)?
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
                .build_child(child, &step.profile, &step.id, &step.deny)?;
            self.run_turn(&mut runtime, &message).await?;
            if prompt_count(self.host.child_log(child)?.events()) != posted + 1 {
                return Err(RunnerError::MessageNotPosted);
            }
        } else {
            let mut runtime = self
                .host
                .build_child(child, &step.profile, &step.id, &step.deny)?;
            let mut observe = |_: Signal<'_>| {};
            if let crate::Resumed::Interrupted { .. } = runtime.resume(None, &mut observe)? {
                runtime.continue_turn(&mut observe).await?;
            }
        }
        self.apply_end(&step, attempt)?;
        Ok(Advanced::Moved)
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
            if !step.checks.is_empty() {
                return Ok(Advanced::PausedAtChecks {
                    step: step.id.clone(),
                    report: report.clone(),
                });
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
        let Some(budget) = slot("budget").and_then(|v| v.as_f64()) else {
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

    // -- pieces -------------------------------------------------------------

    /// Write `run_finished` and end the run.
    fn finish(&mut self, outcome: RunOutcome) -> Result<Advanced, RunnerError> {
        let cost_usd = self.cost();
        self.append(
            EventKind::RunFinished,
            &RunFinishedPayload {
                outcome,
                cost_usd,
                release_impact: None,
            },
        )?;
        self.lock = None;
        Ok(Advanced::Finished { outcome })
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

    /// Take the repository's write lock, or escalate `write_lock`.
    fn take_write_lock(&mut self, step: &Step) -> Result<Option<Advanced>, RunnerError> {
        let _ = step;
        match WriteGuard::take(self.repo.clone(), self.lead_id) {
            Ok(guard) => {
                self.lock = Some(guard);
                Ok(None)
            }
            Err(holder) => {
                let shown = vec![
                    self.repo.display().to_string(),
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
        let payload = serde_json::to_value(payload).expect("payloads are serialisable");
        let event = self.lead.append(NewEvent {
            kind,
            author: Author::Agent(AgentId(RUNNER.into())),
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
            .build_child(child, &step.profile, &step.id, &step.deny)?;
        self.run_turn(&mut runtime, text).await?;
        if prompt_count(self.host.child_log(child)?.events()) != before + 1 {
            return Err(RunnerError::MessageNotPosted);
        }
        Ok(())
    }

    /// The prompt an attempt gets: the step's template rendered for the
    /// first, a short instruction afterwards. The render can fail, and a
    /// failed render is a `render_failed` escalation.
    fn attempt_message(&self, step: &Step, attempt: u32) -> Result<String, RunnerError> {
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
        let Some(end) = tail.iter().rev().find(|e| e.kind == EventKind::TurnEnded) else {
            return Err(RunnerError::Host(format!(
                "child {child} never ended its turn"
            )));
        };
        let end: TurnEndedPayload = serde_json::from_value(end.payload.clone())
            .map_err(|_| RunnerError::Host(format!("child {child}: unreadable turn_ended")))?;
        let cost_usd: f64 = tail
            .iter()
            .filter(|e| e.kind == EventKind::AssistantMessage)
            .filter_map(|e| e.payload.get("cost_usd").and_then(Value::as_f64))
            .sum();
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
        match end.reason.as_str() {
            STEP_REPORTED => {
                let Some(report_event) = reported else {
                    return Err(RunnerError::Host(format!("child {child} reported nothing")));
                };
                let report: StepReport = serde_json::from_value(report_event.payload.clone())
                    .map_err(|_| {
                        RunnerError::Host(format!("child {child}: unreadable step_reported"))
                    })?;
                if report.body.clone().unwrap_or_default().trim().is_empty() {
                    // No `step_finished`: the child did end its turn, but a
                    // report with no body is not a step's outcome the log
                    // may record as one.
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
                Ok(())
            }
            "done" | "resumed" | "max_iterations" | "max_tokens" | "max_wall_time" => {
                let finished = finish(StepStatus::Partial, end.reason, reported.map(|e| e.id));
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

/// `T1 — what — derivation` per line, the shape the implementer's template
/// shows the model (#57's T10). The runner renders it, so no template ever
/// meets a list.
fn planned_tests_text(tests: &[PlannedTest]) -> String {
    tests
        .iter()
        .map(|test| format!("{} — {} — {}", test.id, test.what, test.derivation))
        .collect::<Vec<_>>()
        .join("\n")
}

/// One commit subject per line, when every item is a string.
fn commits_text(items: &[Value]) -> Option<String> {
    items
        .iter()
        .map(|item| item.as_str().map(str::to_owned))
        .collect::<Option<Vec<_>>>()
        .map(|subjects| subjects.join("\n"))
}

// ---------------------------------------------------------------------------
// The slot map (commit 3 moves this into `runner/slots.rs` and adds the
// template line)
// ---------------------------------------------------------------------------

impl<F: Forge, H: RunnerHost> Runner<F, H> {
    /// The slot map one step's template is rendered with: every earlier
    /// step's report, then the runner's own slots, which win.
    pub fn slot_map(&self, step: &Step) -> Result<BTreeMap<String, Value>, RunnerError> {
        let mut map: BTreeMap<String, Value> = BTreeMap::new();
        for report in self.earlier_reports(step)? {
            if let Some(slots) = report.slots {
                map.extend(slots);
            }
            // Typed report fields render to text: a list used as `{{name}}`
            // is an error, so the runner does it here.
            if let Some(tests) = report.planned_tests {
                map.insert(
                    "planned_tests".into(),
                    Value::String(planned_tests_text(&tests)),
                );
            }
            if let Some(Value::Array(items)) = map.get("commits").cloned()
                && let Some(text) = commits_text(&items)
            {
                map.insert("commits".into(), Value::String(text));
            }
        }
        map.extend(self.runner_slots(step)?);
        Ok(map)
    }

    /// What the runner fills itself, in the workflow's declaration order.
    pub fn runner_slots(&self, step: &Step) -> Result<BTreeMap<String, Value>, RunnerError> {
        let run = self.run()?;
        let budget = &self.workflow.workflow.budget;
        let model = trailer_model(&self.host.model_of(&step.profile)?).to_owned();
        // The gate's log path: where the child appends its own gate runs.
        let gate_log = std::env::temp_dir()
            .join(format!("aigentic-gate-{}.log", run.issue))
            .display()
            .to_string();
        Ok(BTreeMap::from([
            ("issue".to_owned(), Value::String(run.issue.to_string())),
            (
                "title".to_owned(),
                Value::String(self.forge.issue(run.issue)?.title),
            ),
            ("model".to_owned(), Value::String(model)),
            ("gate_log".to_owned(), Value::String(gate_log)),
            (
                "budget_trivial".to_owned(),
                Value::String(budget.trivial.to_string()),
            ),
            (
                "budget_full".to_owned(),
                Value::String(budget.full.to_string()),
            ),
            (
                "max_raise".to_owned(),
                Value::String(budget.max_raise.to_string()),
            ),
        ]))
    }

    /// What the steps before `step` reported, in the workflow's order, read
    /// from their children's logs.
    pub fn earlier_reports(&self, step: &Step) -> Result<Vec<StepReport>, RunnerError> {
        let state = self.state()?;
        let before: Vec<String> = self
            .workflow
            .workflow
            .steps
            .iter()
            .take_while(|candidate| candidate.id != step.id)
            .map(|candidate| candidate.id.clone())
            .collect();
        let mut reports = Vec::new();
        for id in before {
            let Some(record) = state
                .steps
                .iter()
                .rev()
                .find(|record| record.step == id && record.reported_event.is_some())
            else {
                continue;
            };
            if let Some((report, _)) = self.report_of(&id, record.attempt)? {
                reports.push(report);
            }
        }
        Ok(reports)
    }
}
