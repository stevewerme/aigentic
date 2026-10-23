//! Runs (issue #58): the daemon's side of one workflow run over a
//! project's repository.
//!
//! A run is a lead thread whose log the [`Runner`] writes; its step
//! children are the threads the runner starts. The daemon hosts one
//! `Runner` per lead — [`Runs`] hands a lead to exactly one claimant, so
//! there is one writer per lead log and no action is repeated after a
//! restart — and streams the lead's events to whoever is watching.
//
// See `docs/PLAN-layer2.md` and the #58 spec.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aigentic_api::{CheckpointAnswer, JobState, Notice};
use aigentic_runtime::aigentic_core::{AgentId, Author, Event, EventKind};
use aigentic_runtime::aigentic_log::{
    CheckpointAnswer as LogAnswer, NewEvent, NextMove, Repair, RunErroredPayload,
    RunFinishedPayload, RunStartedPayload, StepFinishedPayload, ThreadLog, ThreadStartedPayload,
    run_state,
};
use aigentic_runtime::runner::forge::{Forge, GhForge};
use aigentic_runtime::runner::git::{GitRepo, Repo};
use aigentic_runtime::runner::install::{CargoInstaller, Installer};
use aigentic_runtime::runner::{Advanced, Runner, RunnerError, RunnerHost};
use aigentic_runtime::workflow::{LoadedWorkflow, WorkflowFile, WorkflowRoots};
use aigentic_runtime::{Mode, Phase, Runtime};
use time::OffsetDateTime;
use tokio::sync::{mpsc, oneshot, watch};
use ulid::Ulid;

use crate::awake::{Held, KeepAwake};
use crate::build::{ProviderFactory, Root, build_thread};
use crate::config::Config;
use crate::threads::Ids;
use crate::workspaces::Workspace;

/// Where a run gets the seams it drives the world through. Production
/// uses the real ones; a test replaces them through
/// [`crate::threads::ThreadTable::with_run_deps`], with no new Cargo
/// edge either way (issue #58).
pub trait RunDeps: Send + Sync {
    /// The issue host for the run's project.
    fn forge(&self) -> Arc<dyn Forge + Send + Sync>;
    /// The repository the run's step children commit and push in.
    fn repo(&self, root: &Root) -> Box<dyn Repo + Send + Sync>;
    /// How the run installs the binary its gate checks.
    fn installer(&self) -> Box<dyn Installer + Send + Sync>;
}

/// `gh`, `git` and `cargo install`: what a real run uses.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProdDeps;

impl RunDeps for ProdDeps {
    fn forge(&self) -> Arc<dyn Forge + Send + Sync> {
        Arc::new(GhForge::new())
    }

    fn repo(&self, root: &Root) -> Box<dyn Repo + Send + Sync> {
        Box::new(GitRepo::new(&root.root))
    }

    fn installer(&self) -> Box<dyn Installer + Send + Sync> {
        Box::new(CargoInstaller::new())
    }
}

/// What a run's task needs of the daemon: the pieces to build a child's
/// runtime in the run's project, and no handle on the [`ThreadTable`] it
/// came from — a task that outlived a session must not keep the table
/// alive, and the table must not keep the task's slot.
///
/// [`ThreadTable`]: crate::threads::ThreadTable
#[derive(Clone)]
pub struct RunWorld {
    /// The daemon's config: profiles, budgets, compaction.
    pub config: Arc<Config>,
    /// Where global instructions and the user's skills and workflows live.
    pub config_dir: PathBuf,
    /// The daemon's owner, whose memory folder is the person's home.
    /// `None` when server.toml lists no user.
    pub owner: Option<String>,
    /// The profile's provider, built from the config.
    pub providers: Arc<dyn ProviderFactory>,
    /// The forge, installer and repo seams.
    pub deps: Arc<dyn RunDeps>,
    /// The daemon's keep-awake guard: held while a run advances, so a
    /// long step is not a sleeping laptop half way through.
    pub guard: Arc<dyn KeepAwake>,
    /// The thread ids a run's children get, from the daemon's one source
    /// ([`Ids`]), so a child's id orders against its lead's and against
    /// the front threads'.
    ///
    /// [`Ids`]: crate::threads::Ids
    pub ids: Arc<Ids>,
    /// The project the run works in.
    pub root: Root,
    /// The project's name, for the children's `thread_started`.
    pub project: String,
    /// Workspace files whose roots the daemon also serves.
    pub workspaces: Vec<Workspace>,
    /// The bundled root: its `workflows/` holds the workflows a project
    /// and the user did not override.
    pub bundled: PathBuf,
    /// The daemon's working count, shared with every thread actor so a
    /// usage line carries it.
    pub working: Working,
}

/// How many builds and background jobs the daemon drives right now, and
/// the figure the wire carries.
///
/// Only the daemon that drives them can count them, so the count lives
/// here: a client watching any thread still sees how busy the machine is.
/// A build is in the map while its [`Runs`] slot is held, and a job while
/// its driver runs — exactly while each task is alive — and the figure is
/// what the wire's `working` reads.
#[derive(Clone, Default)]
pub struct Working(Arc<WorkingInner>);

#[derive(Default)]
struct WorkingInner {
    /// The held builds, by lead: the day each started and the state its
    /// last fold read, so the count is a lookup rather than a scan of
    /// every lead's log.
    jobs: Mutex<HashMap<Ulid, WorkingJob>>,
    /// The count, and the nudge a change sends: an actor waits on
    /// [`Working::changed`] and re-pushes its usage line when the figure
    /// moves.
    figure: watch::Sender<u32>,
}

/// One held build: the day it started and what its state was last folded
/// to.
#[derive(Debug, Clone)]
struct WorkingJob {
    day: Option<time::Date>,
    state: Option<JobState>,
}

impl Working {
    /// A build or a job is now driven: `lead` (a build's lead, or a job's
    /// own thread) joins the count as `Running`, under the day its id was
    /// minted on, the day its row is listed under. A lead resumed after a
    /// restart keeps the day it started. Called by the run path that takes
    /// the lead's slot, and by a job's driver when it starts.
    pub fn register(&self, lead: Ulid) {
        self.0
            .jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                lead,
                WorkingJob {
                    day: minted_day(lead),
                    state: Some(JobState::Running),
                },
            );
        self.recount();
    }

    /// A driven build moved: park it at a gate, or set it working again.
    /// The count does not move, so this nudges nobody: whoever knows the
    /// line it just changed calls [`Working::recount`].
    pub fn note_state(&self, lead: Ulid, state: Option<JobState>) {
        if let Some(job) = self
            .0
            .jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&lead)
        {
            job.state = state;
        }
    }

    /// The state a driven build or job was last noted at, or `None` when
    /// nothing here drives it.
    #[must_use]
    pub fn state_of(&self, id: Ulid) -> Option<Option<JobState>> {
        self.0
            .jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
            .map(|job| job.state.clone())
    }

    /// `lead` is no longer driven, whatever held it left behind.
    pub fn release(&self, lead: Ulid) {
        self.0
            .jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&lead);
        self.recount();
    }

    /// Forget every driven build and job: a daemon ending its tasks
    /// drives nothing.
    fn clear(&self) {
        self.0
            .jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.recount();
    }

    /// How many builds and jobs are driven right now, from the same rule
    /// the rows use: one whose state is `Running` or `WaitingOnYou`, on
    /// today's day, so the figure and the listing agree.
    #[must_use]
    pub fn count(&self) -> u32 {
        let today = day_of(OffsetDateTime::now_utc());
        self.0
            .jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .filter(|job| job.day == Some(today) && is_working(job.state.as_ref()))
            .count() as u32
    }

    /// Tell every thread the figure may have moved, so each one re-pushes
    /// the usage line it already sends. The count itself is read live, so
    /// a nudge that changes nothing costs one line and no wrong figure.
    pub fn recount(&self) {
        let count = self.count();
        self.0.figure.send_if_modified(|figure| {
            let moved = *figure != count;
            *figure = count;
            moved
        });
    }

    /// Wait for the figure to change. Never errors: the sender lives as
    /// long as the handle, so a thread's own copy keeps the channel open.
    pub async fn changed(&self) {
        let mut rx = self.0.figure.subscribe();
        // Mark the current value seen, so a thread that attaches now waits
        // for the next change rather than re-pushing at once.
        rx.borrow_and_update();
        let _ = rx.changed().await;
    }
}

/// Whether a row's state is a build at work: the two states the count is
/// made of.
fn is_working(state: Option<&JobState>) -> bool {
    matches!(state, Some(JobState::Running | JobState::WaitingOnYou))
}

/// The zone a build's day is read in, once, or `None` when it could not be
/// read.
static DAY_ZONE: std::sync::OnceLock<Option<time::UtcOffset>> = std::sync::OnceLock::new();

/// Read the machine's zone while the process is still single-threaded:
/// `time` refuses the read once a process may have threads, and a daemon
/// has them. A daemon calls this as it starts so a listing gets the
/// person's day rather than UTC's.
pub fn read_day_zone() {
    let _ = DAY_ZONE.get_or_init(|| time::UtcOffset::current_local_offset().ok());
}

/// The day `at` falls on, in the machine's zone where it is known.
///
/// A build's id is stamped in UTC and "today" is the person's own day, so
/// a build started at 00:30 in Stockholm is today's and not the previous
/// UTC day's. A zone that could not be read leaves the UTC day: a listing
/// still answers and only the boundary moves.
pub(crate) fn day_of(at: OffsetDateTime) -> time::Date {
    let zone = *DAY_ZONE.get_or_init(|| time::UtcOffset::current_local_offset().ok());
    match zone {
        Some(offset) => at.to_offset(offset).date(),
        None => at.date(),
    }
}

/// The day a build's id was minted on: the run's start, stamped in UTC and
/// read in the machine's zone. The rows and the count both date a build
/// by it, so the two cannot disagree about which day a build belongs to.
pub(crate) fn minted_day(id: Ulid) -> Option<time::Date> {
    time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(id.timestamp_ms()) * 1_000_000)
        .ok()
        .map(day_of)
}

/// Whether a build's id was minted on today's day.
pub fn minted_today(id: Ulid) -> bool {
    minted_day(id) == Some(day_of(time::OffsetDateTime::now_utc()))
}

/// What one build's row shows, from one read of its lead's log: its state
/// and what it has cost so far.
///
/// `held` says whether this daemon drives the lead: a driven build's state
/// is the run registry's own fold, and its cost is what its finished steps
/// added up to. A lead this daemon does not drive states only what its log
/// says: an errored end, a finished end, or nothing at all.
pub fn job_facts(threads: &Path, lead: Ulid, held: bool) -> (Option<JobState>, Option<f64>) {
    let Some(events) = read_events(threads, lead) else {
        return (None, None);
    };
    if held {
        let state = status_of(threads, &events)
            .and_then(|status| state_of(&status))
            .or_else(|| end_state(&events));
        return (state, step_cost(&events));
    }
    (end_state(&events), end_cost(&events))
}

/// A stopped reason's first line, clipped as a thread row's title is.
fn clipped(reason: &str) -> String {
    crate::threads::first_line_of(reason)
}

/// The state a folded run's phase stands for. A phase that is neither at
/// work nor at a gate is a build with no state to show: its end is what
/// would say so, and a held run's fold has already read the log.
fn state_of(status: &aigentic_runtime::RunStatus) -> Option<JobState> {
    match &status.phase {
        Phase::Step(_) | Phase::Move(_) => Some(JobState::Running),
        Phase::Gate { .. } => Some(JobState::WaitingOnYou),
        Phase::Stopped { reason } => Some(JobState::Stopped {
            reason: clipped(reason),
        }),
    }
}

/// What a lead's log says about how its run ended, for a lead this daemon
/// does not drive.
fn end_state(events: &[Event]) -> Option<JobState> {
    for event in events.iter().rev() {
        match event.kind {
            EventKind::RunErrored => {
                let payload: RunErroredPayload = payload_of(event)?;
                return Some(JobState::Stopped {
                    reason: clipped(&payload.reason),
                });
            }
            EventKind::RunFinished => return Some(JobState::Done),
            _ => {}
        }
    }
    None
}

/// What a lead's finished steps have cost so far, or `None` when none of
/// them is priced.
fn step_cost(events: &[Event]) -> Option<f64> {
    let priced: Vec<StepFinishedPayload> = events
        .iter()
        .filter(|event| event.kind == EventKind::StepFinished)
        .filter_map(payload_of)
        .collect();
    (!priced.is_empty()).then(|| priced.iter().map(|p| p.cost_usd).sum())
}

/// What a run's own end says it cost, or `None` when it was not priced.
fn end_cost(events: &[Event]) -> Option<f64> {
    for event in events.iter().rev() {
        match event.kind {
            EventKind::RunErrored => {
                let payload: RunErroredPayload = payload_of(event)?;
                return Some(payload.cost_usd);
            }
            EventKind::RunFinished => {
                let payload: RunFinishedPayload = payload_of(event)?;
                return Some(payload.cost_usd);
            }
            _ => {}
        }
    }
    None
}

/// A payload by kind, or `None` when the line cannot be read as one: a
/// listing shows what it can read rather than refuse the whole day.
fn payload_of<T: serde::de::DeserializeOwned>(event: &Event) -> Option<T> {
    serde_json::from_value(event.payload.clone()).ok()
}

impl RunWorld {
    /// The three workflow roots, in resolution order: the project's, the
    /// user's, the bundled one.
    pub fn workflow_roots(&self) -> WorkflowRoots {
        WorkflowRoots {
            project: Some(self.root.root.join(".aigentic").join("workflows")),
            user: Some(self.config_dir.join("workflows")),
            bundled: Some(self.bundled.join("workflows")),
        }
    }
}

impl std::fmt::Debug for RunWorld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunWorld")
            .field("project", &self.project)
            .field("root", &self.root.root)
            .finish_non_exhaustive()
    }
}

/// The one `RunnerHost` that drives a run in a real project: it starts
/// step children in the project's threads directory and builds their
/// runtimes through `build_thread`, as the daemon does for any thread.
pub struct ServerHost {
    world: RunWorld,
    /// The run's lead thread, which every child names as its parent.
    lead: Ulid,
}

impl ServerHost {
    /// A host over `world` for the lead `lead`.
    pub fn new(world: RunWorld, lead: Ulid) -> Self {
        Self { world, lead }
    }

    /// The threads directory of the run's project.
    fn threads_dir(&self) -> &PathBuf {
        &self.world.root.threads_dir
    }
}

impl RunnerHost for ServerHost {
    fn new_child_id(&mut self) -> Ulid {
        self.world.ids.next()
    }

    fn create_child(&mut self, id: Ulid, step: &str) -> Result<(), RunnerError> {
        // Every read and write here repairs a torn tail: a `kill -9`
        // mid-write leaves a half line that was never a complete event,
        // and cutting it is the repair (#58). Truncating a complete
        // event is never allowed.
        //
        // The threads directory may not exist yet on the very first
        // child of a project, and nothing else creates it for a run: the
        // log file's parent is made here, once, and the append is the
        // only write.
        std::fs::create_dir_all(self.threads_dir())
            .map_err(|e| RunnerError::Host(e.to_string()))?;
        let (mut log, _cut) =
            ThreadLog::open_with(self.threads_dir(), id, Repair::TruncateTornTail)?;
        // Idempotent: a file that already holds `thread_started` — even
        // one written by an earlier run of this step — is left alone.
        if log
            .events()
            .iter()
            .any(|event| event.kind == EventKind::ThreadStarted)
        {
            return Ok(());
        }
        log.append(NewEvent {
            kind: EventKind::ThreadStarted,
            author: Author::Agent(AgentId("runner".into())),
            payload: serde_json::to_value(ThreadStartedPayload {
                project: Some(self.world.project.clone()),
                root: self.world.root.root.clone(),
                created_by: Author::Agent(AgentId("runner".into())),
                parent_thread: Some(self.lead),
                step: Some(step.to_owned()),
                // A step's child is never a front thread (issue #84).
                front: false,
                job: false,
            })
            .expect("thread_started serialises"),
            parent_event: None,
        })?;
        Ok(())
    }

    fn build_child(
        &mut self,
        id: Ulid,
        profile: &str,
        step: &str,
        deny: &[String],
    ) -> impl Future<Output = Result<Runtime, RunnerError>> + Send {
        // Everything the future needs is cloned out first: it outlives
        // this borrow, and the pieces are cheap.
        let world = self.world.clone();
        let step = step.to_owned();
        let deny = deny.to_vec();
        let profile = profile.to_owned();
        async move {
            let built = build_thread(
                &world.config,
                &world.config_dir,
                world.owner.as_deref(),
                &*world.providers,
                &world.root,
                &world.workspaces,
                id,
                Some(&profile),
                // A step's child is an agent's thread — its
                // `thread_started` says `Author::Agent("runner")` — so
                // it gets no projects block (#81).
                None,
            )
            .await
            .map_err(|e| RunnerError::Host(e.to_string()))?;
            let mut runtime = built.runtime.with_step(&step, &deny)?;
            // A run's child is unattended: no approver prompts anyone, and
            // `auto` lets it work on its own. The step's deny overlay still
            // blocks what it blocks, ahead of the mode.
            runtime.set_mode(Mode::Auto);
            Ok(runtime)
        }
    }

    fn child_exists(&self, id: Ulid) -> bool {
        self.threads_dir().join(format!("{id}.jsonl")).is_file()
    }

    fn child_log(&self, id: Ulid) -> Result<ThreadLog, RunnerError> {
        let (log, _cut) = ThreadLog::open_with(self.threads_dir(), id, Repair::TruncateTornTail)?;
        Ok(log)
    }

    fn model_of(&self, profile: &str) -> Result<String, RunnerError> {
        Ok(self
            .world
            .config
            .select(Some(profile))
            .map(|(_, p)| p.model.clone())
            .unwrap_or_else(|_| profile.to_owned()))
    }
}

/// One answer on its way to the task that owns a lead. The task checks
/// the gate and appends, so the answer travels with where to put the
/// verdict.
pub struct Answer {
    /// The gate the answerer named.
    pub gate: String,
    /// `stop`, `go` or `amend` as the wire spells them.
    pub answer: CheckpointAnswer,
    /// The amendment text, when the answer carried one.
    pub amendment: Option<String>,
    /// Who answered.
    pub by: Author,
    /// Where the verdict goes: `Ok` when the answer was written, `Err`
    /// with the reason when it was refused and nothing was written.
    pub reply: oneshot::Sender<Result<(), String>>,
}

/// What [`Runs::claim`] decided for a lead.
pub enum Claim {
    /// Nobody held the lead, here or in any other process: the caller
    /// starts the task, keeps the lock alive for the task's whole life,
    /// and sends answers on this sender.
    Claimed(
        LeadLock,
        mpsc::UnboundedReceiver<Answer>,
        mpsc::UnboundedSender<Answer>,
    ),
    /// Somebody already drives the lead in this process. Never build a
    /// second `Runner` over it: send the answer on this sender and
    /// attach.
    Attached(mpsc::UnboundedSender<Answer>),
    /// Another `aigentic` process drives the lead (issue #58 fix 2).
    /// Nothing may be written: the holder is the lead log's one writer.
    Elsewhere {
        /// Why, ready to show a person.
        reason: String,
    },
}

/// The OS claim on one lead's log (issue #58 fix 2).
///
/// The in-process slot cannot see another daemon: two `aigentic build
/// <n>`, or a build beside `aigentic serve`, would both claim a lead
/// and both append to its log — duplicate seqs, then an unreadable log
/// and a second lead. An advisory lock on `<threads_dir>/<lead>.lock`,
/// taken inside the claim before any `Runner` is built and held for the
/// task's whole life, is what tells them apart. The file is left in
/// place when the lock is dropped, so the next claim locks the same
/// inode instead of racing a fresh path.
pub struct LeadLock {
    /// The locked file. Dropping it — the task ending, on any path,
    /// including an error — releases the lock.
    file: std::fs::File,
    /// Where it is, for the refusal's message and for a test.
    path: PathBuf,
}

impl LeadLock {
    /// Take `lead`'s lock under `threads_dir`, or say who holds it.
    fn take(threads_dir: &std::path::Path, lead: Ulid) -> Result<Self, String> {
        let path = threads_dir.join(format!("{lead}.lock"));
        let file = open_lock_file(threads_dir, &path)?;
        match file.try_lock() {
            Ok(()) => Ok(Self { file, path }),
            // Someone else — another daemon — holds it. `unlock` says
            // nothing here: the lock was never taken.
            Err(std::fs::TryLockError::WouldBlock) => Err(format!(
                "run {lead} is being driven by another aigentic process"
            )),
            Err(std::fs::TryLockError::Error(e)) => {
                Err(format!("cannot lock {}: {e}", path.display()))
            }
        }
    }

    /// Where the lock lives: `<threads_dir>/<lead>.lock`.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for LeadLock {
    fn drop(&mut self) {
        // Closing the file would release it anyway; unlocking says so.
        let _ = self.file.unlock();
    }
}

/// The OS claim on starting a run for one issue (issue #58, review 2).
///
/// [`LeadLock`] is keyed by the lead, so it cannot exist before the lead
/// does: two processes that both find no unfinished run for an issue
/// would each create a lead, and each would drive its own. This lock, on
/// `<base>/issue-<n>-<project>.lock`, is held across the whole
/// check-and-create, so the second process waits, then finds the first
/// one's lead and is refused (or attaches) rather than starting another.
/// Like the lead's lock, the file is left in place.
///
/// The project is in the name because the base is one flat directory
/// once threads moved there (issue #9): two projects building issue 7 at
/// once are two different runs, and must not wait for one another.
pub struct IssueLock {
    file: std::fs::File,
}

impl IssueLock {
    /// How long a `Build` waits for another process's check-and-create.
    /// That window is a few file writes, so waiting this long means the
    /// holder is stuck; the caller is refused rather than kept waiting.
    const WAIT: std::time::Duration = std::time::Duration::from_secs(10);

    /// Take `issue`'s lock under `base` for `project`, waiting while
    /// another process holds it.
    pub async fn take(base: &std::path::Path, project: &str, issue: u64) -> Result<Self, String> {
        let path = base.join(format!("issue-{issue}-{}.lock", lock_name(project)));
        let file = open_lock_file(base, &path)?;
        let deadline = tokio::time::Instant::now() + Self::WAIT;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self { file }),
                Err(std::fs::TryLockError::WouldBlock) => {
                    if tokio::time::Instant::now() >= deadline {
                        return Err(format!(
                            "another aigentic process has been starting a run for issue #{issue} for {}s; try again",
                            Self::WAIT.as_secs()
                        ));
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                Err(std::fs::TryLockError::Error(e)) => {
                    return Err(format!("cannot lock {}: {e}", path.display()));
                }
            }
        }
    }
}

impl Drop for IssueLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

/// The front thread's per-user lock (issue #88): two terminals launched
/// together on a first run must not both find no front thread and both
/// make one. Built like [`IssueLock`], over `<base>/front-<user>.lock`,
/// and held across `Front`'s check-and-create. The file is left in
/// place.
pub struct FrontLock {
    file: std::fs::File,
}

impl FrontLock {
    /// How long a `Front` waits for another process's check-and-create.
    /// That window is a few file writes, so waiting this long means the
    /// holder is stuck; the caller is refused rather than kept waiting.
    const WAIT: std::time::Duration = std::time::Duration::from_secs(10);

    /// Take `user`'s front-thread lock under `base`, waiting while
    /// another process holds it.
    pub async fn take(base: &std::path::Path, user: &str) -> Result<Self, String> {
        let path = base.join(format!("front-{}.lock", lock_name(user)));
        let file = open_lock_file(base, &path)?;
        let deadline = tokio::time::Instant::now() + Self::WAIT;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self { file }),
                Err(std::fs::TryLockError::WouldBlock) => {
                    if tokio::time::Instant::now() >= deadline {
                        return Err(format!(
                            "another aigentic process has been starting {user}'s front thread for {}s; try again",
                            Self::WAIT.as_secs()
                        ));
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                Err(std::fs::TryLockError::Error(e)) => {
                    return Err(format!("cannot lock {}: {e}", path.display()));
                }
            }
        }
    }
}

impl Drop for FrontLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

/// A project name as a file name: every character outside
/// `[A-Za-z0-9_-]` becomes `_` (issue #9), so an issue lock's name is a
/// safe path whatever a project is called.
pub fn lock_name(project: &str) -> String {
    project
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Open (creating it if needed) a lock file under `threads_dir`. Shared
/// by `IssueLock` and `FrontLock` (issue #88).
pub(crate) fn open_lock_file(
    threads_dir: &std::path::Path,
    path: &std::path::Path,
) -> Result<std::fs::File, String> {
    if let Err(e) = std::fs::create_dir_all(threads_dir) {
        return Err(format!("cannot make {}: {e}", threads_dir.display()));
    }
    std::fs::File::options()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))
}

/// Which leads a task owns, and who watches each of them.
///
/// Both maps are keyed by the lead. The slot and the subscription are
/// separate on purpose: a client may watch a lead whose run no task
/// holds (a finished run, or one on a daemon that did not resume), and
/// the slot outlives every session.
#[derive(Default)]
pub struct Runs {
    slots: Mutex<HashMap<Ulid, mpsc::UnboundedSender<Answer>>>,
    watchers: Mutex<HashMap<Ulid, Vec<mpsc::UnboundedSender<Notice>>>>,
    /// The task that drives each lead, so a daemon going away can end
    /// them (issue #58 fix 2). Each task's OS lock lives inside its
    /// future, so ending the tasks is what a process dying does to its
    /// advisory locks.
    tasks: Mutex<HashMap<Ulid, tokio::task::JoinHandle<()>>>,
    /// How many of these leads are being driven right now, shared with
    /// every thread actor.
    working: Working,
}

impl Runs {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// The working count this registry drives, for the wire and for the
    /// thread actors a usage line carries it on.
    #[must_use]
    pub fn working(&self) -> Working {
        self.working.clone()
    }

    /// Take `lead` for this caller, or find it taken — inserting and
    /// testing under one lock, before any `Runner` is built. Exactly one
    /// caller ever sees `Claimed` for a lead, so exactly one task drives
    /// it and one `ThreadLog` writes it. The in-process slot is checked
    /// first; a fresh claim then takes the lead's OS lock, so a lead
    /// another daemon drives is refused rather than driven twice (issue
    /// #58 fix 2).
    pub fn claim(&self, lead: Ulid, world: &RunWorld) -> Claim {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(tx) = slots.get(&lead) {
            return Claim::Attached(tx.clone());
        }
        let lock = match LeadLock::take(&world.root.threads_dir, lead) {
            Ok(lock) => lock,
            Err(reason) => return Claim::Elsewhere { reason },
        };
        let (tx, rx) = mpsc::unbounded_channel();
        slots.insert(lead, tx.clone());
        drop(slots);
        self.working.register(lead);
        Claim::Claimed(lock, rx, tx)
    }

    /// Remember the task that drives `lead`, so [`Runs::stop_tasks`] can
    /// end it. Called by whoever spawned it, in the same claim.
    pub fn remember(&self, lead: Ulid, task: tokio::task::JoinHandle<()>) {
        self.tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(lead, task);
    }

    /// End every task this registry drives and free every slot: a daemon
    /// going away, or a test standing in for one a `kill -9` took (issue
    /// #58 fix 2). Aborting a task drops its future, and with it the
    /// lead's OS lock — exactly what the dying process did.
    pub async fn stop_tasks(&self) {
        let tasks: Vec<tokio::task::JoinHandle<()>> = {
            let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
            tasks.drain().map(|(_, task)| task).collect()
        };
        for task in tasks {
            task.abort();
            // Draining the handle is what makes the drop final.
            let _ = task.await;
        }
        self.slots.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.working.clear();
    }

    /// Give up `lead`, so a later claim may drive it again: the task
    /// calls this when it ends, finished or stopped.
    pub fn release(&self, lead: Ulid) {
        self.slots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&lead);
        self.working.release(lead);
    }

    /// Whether a task holds `lead` now.
    pub fn held(&self, lead: Ulid) -> bool {
        self.slots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&lead)
    }

    /// Watch `lead`: every notice broadcast from now on goes to `tx`. A
    /// client that also wants what already happened reads the log.
    pub fn watch(&self, lead: Ulid, tx: mpsc::UnboundedSender<Notice>) {
        self.watchers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(lead)
            .or_default()
            .push(tx);
    }

    /// Send one notice to every watcher of `lead`. A watcher that has
    /// gone away is dropped, so a closed session leaves nothing behind.
    pub fn broadcast(&self, lead: Ulid, notice: Notice) {
        let mut watchers = self.watchers.lock().unwrap_or_else(|e| e.into_inner());
        let Some(list) = watchers.get_mut(&lead) else {
            return;
        };
        list.retain(|tx| tx.send(notice.clone()).is_ok());
        if list.is_empty() {
            watchers.remove(&lead);
        }
    }

    /// How many watchers `lead` has, for the tests.
    pub fn watchers(&self, lead: Ulid) -> usize {
        self.watchers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&lead)
            .map_or(0, Vec::len)
    }

    /// Every thread somebody watches now, the lead included: a ticker's
    /// work list, so a step's child is read only while a client has it
    /// open.
    pub fn watched(&self) -> Vec<Ulid> {
        self.watchers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .copied()
            .collect()
    }
}

/// Drive `lead`'s run until it finishes or stops, then give the lead up.
///
/// The task owns the `Runner`, which owns the lead's log: one writer.
/// It broadcasts every event the runner appends, waits at a checkpoint
/// for an answer, and ends on success or on an error. An error is not
/// fatal: it releases the lead unfinished, so the next `Build` (or the
/// next daemon's start-up scan) resumes it.
pub async fn drive(
    runs: Arc<Runs>,
    world: RunWorld,
    lead: Ulid,
    mut answers: mpsc::UnboundedReceiver<Answer>,
    lock: LeadLock,
) {
    // Held for this task's whole life, on every way out: `_lock` is
    // dropped when the task returns, finished, stopped or panicked, so
    // no path leaves the lead locked by a task that ended.
    let _lock = lock;
    let outcome = tokio::select! {
        outcome = run(runs.clone(), &world, lead, &mut answers) => outcome,
        // The ticker never resolves, so the run's own future is still the
        // one that ends this task; dropping the ticker is what stops the
        // pushes.
        () = tick_status(runs.clone(), &world, lead, PUSH_EVERY) => unreachable!(),
    };
    if let Err(e) = &outcome {
        runs.broadcast(
            lead,
            Notice::Note {
                thread: lead,
                text: format!("run stopped: {e}"),
            },
        );
    }
    runs.release(lead);
}

/// How often a watched lead's status is pushed: often enough to feel
/// live, rarely enough that a long step is not a stream of notices.
pub const PUSH_EVERY: Duration = Duration::from_secs(2);

/// Push `lead`'s status to its watchers every `every`, until stopped,
/// and each watched child's new events to that child's watchers.
/// Reads the lead's and the watched children's logs; repairs nothing and
/// pushes nothing it cannot read.
pub async fn tick_status(runs: Arc<Runs>, world: &RunWorld, lead: Ulid, every: Duration) {
    let mut interval = tokio::time::interval(every);
    // This ticker's own high-water mark per pushed thread. A thread
    // first seen starts at 0, so a child opened mid-run is caught up to
    // its log's present at the next tick.
    let mut marks: HashMap<Ulid, u64> = HashMap::new();
    loop {
        interval.tick().await;
        // Nobody has this lead open: reading two logs every tick would be
        // work for a line nobody sees.
        if runs.watchers(lead) == 0 {
            continue;
        }
        // The count is re-pushed with the status: a client watching this
        // thread sees how busy the daemon is on the same line.
        world.working.recount();
        if let Some(status) = read_status(world, lead) {
            runs.broadcast(
                lead,
                Notice::RunStatus {
                    thread: lead,
                    status: wire_status(status),
                },
            );
        }
        push_watched_children(&runs, world, lead, &mut marks);
    }
}

/// Push each watched thread's new events to its watchers.
///
/// The lead's own events already go out live from `broadcast_new`, and a
/// thread another lead started belongs to that lead's ticker: pushing
/// either here would send the same event twice.
fn push_watched_children(
    runs: &Runs,
    world: &RunWorld,
    lead: Ulid,
    marks: &mut HashMap<Ulid, u64>,
) {
    let threads = &world.root.threads_dir;
    for thread in runs.watched() {
        if thread == lead {
            continue;
        }
        // A log that is not readable this tick may still be mid-creation,
        // and a failed read pushes nothing rather than half a log.
        let Some(events) = read_events(threads, thread) else {
            continue;
        };
        if parent_of(&events) != Some(lead) {
            continue;
        }
        let mark = marks.entry(thread).or_insert(0);
        for event in events.iter() {
            if event.seq <= *mark {
                continue;
            }
            runs.broadcast(
                thread,
                Notice::Event {
                    thread,
                    event: event.clone(),
                },
            );
            *mark = event.seq;
        }
    }
}

/// The lead whose step started the thread: the parent its first
/// `ThreadStarted` names, or `None` for a thread a person started.
fn parent_of(events: &[Event]) -> Option<Ulid> {
    events
        .iter()
        .filter(|event| event.kind == EventKind::ThreadStarted)
        .filter_map(|event| {
            serde_json::from_value::<ThreadStartedPayload>(event.payload.clone()).ok()
        })
        .find_map(|started| started.parent_thread)
}

/// The run's status as the two logs fold it, or `None` when a log that
/// should be readable is not. A step's child is read only once the lead
/// names it: a child that has written nothing yet is an empty slice, so
/// the step is visible before its first line, while a child whose log is
/// present but unreadable is a failed read — and a failed read pushes
/// nothing rather than a partial line.
fn read_status(world: &RunWorld, lead: Ulid) -> Option<aigentic_runtime::RunStatus> {
    read_status_in(&world.root.threads_dir, lead)
}

/// The same fold over `threads`: a listing knows a lead's directory and
/// nothing else of its world.
pub fn read_status_in(threads: &Path, lead: Ulid) -> Option<aigentic_runtime::RunStatus> {
    status_of(threads, &read_events(threads, lead)?)
}

/// The fold of a lead whose log is already read, reading only the child
/// its run is waiting on.
fn status_of(threads: &Path, lead_events: &[Event]) -> Option<aigentic_runtime::RunStatus> {
    let child = match run_state(lead_events).ok()?.next_move() {
        NextMove::ReAwait { child_thread, .. } if exists(threads, child_thread) => {
            read_events(threads, child_thread)?
        }
        _ => Vec::new(),
    };
    aigentic_runtime::run_status(lead_events, &child, OffsetDateTime::now_utc())
}

/// Whether a thread's log has been written at all.
fn exists(threads: &Path, thread: Ulid) -> bool {
    threads.join(format!("{thread}.jsonl")).is_file()
}

/// A thread's events, or `None` when its log is missing or unreadable.
/// The `exists` check plus `ThreadLog::open`'s `Repair::Refuse` mean a
/// read never creates a log and never repairs a torn one.
fn read_events(threads: &Path, thread: Ulid) -> Option<Vec<Event>> {
    if !exists(threads, thread) {
        return None;
    }
    ThreadLog::open(threads, thread)
        .ok()
        .map(|log| log.events().to_vec())
}

/// The runtime's fold, in the shape the wire carries.
fn wire_status(status: aigentic_runtime::RunStatus) -> aigentic_api::RunStatus {
    let phase = match status.phase {
        aigentic_runtime::Phase::Step(step) => aigentic_api::RunPhase::Step {
            step: step.step,
            attempt: step.attempt,
            elapsed_secs: step.elapsed_secs,
            calls: step.calls,
            cost_usd: step.cost_usd,
            checklist: step.checklist.map(|checklist| aigentic_api::RunChecklist {
                done: checklist.done,
                total: checklist.total,
                active: checklist.active,
            }),
        },
        aigentic_runtime::Phase::Gate { gate } => aigentic_api::RunPhase::Gate { gate },
        aigentic_runtime::Phase::Move(what) => aigentic_api::RunPhase::Move { what },
        aigentic_runtime::Phase::Stopped { reason } => aigentic_api::RunPhase::Stopped { reason },
    };
    aigentic_api::RunStatus {
        issue: status.issue,
        phase,
        idle_secs: status.idle_secs,
    }
}

/// The run's own loop. See [`drive`].
async fn run(
    runs: Arc<Runs>,
    world: &RunWorld,
    lead: Ulid,
    answers: &mut mpsc::UnboundedReceiver<Answer>,
) -> Result<(), RunnerError> {
    let (mut runner, started, loaded) = build_runner(world, lead)?;
    // The workflow moved under the run: say so and carry on. The run's
    // own record of it is the truth for the run.
    if loaded != started.content_hash {
        runs.broadcast(
            lead,
            Notice::Note {
                thread: lead,
                text: format!(
                    "workflow `{}` changed since this run started: {} is on disk, {} was recorded",
                    started.workflow, loaded, started.content_hash
                ),
            },
        );
    }
    let mut seen = runner.log().len();
    // The hold follows the advancing (#58 fix): taken while the run
    // works — a child waiting on a provider is why #47 exists — and
    // given back at a gate, where nobody is working, and on every way
    // out of this loop. `Held` releases on drop, so no path leaks one.
    let mut held: Option<Held> = None;
    let outcome = loop {
        if held.is_none() {
            held = Some(Held::take(world.guard.clone()));
        }
        match runner.advance().await {
            Ok(Advanced::Moved) => {}
            Ok(Advanced::WaitingHuman { .. }) => {
                // The watchers see the gate before anyone is asked to
                // answer it: a client answers only a `checkpoint_asked`
                // it has received, so waiting first would wait forever.
                seen = broadcast_new(&runs, lead, &runner, seen);
                // Nothing is working while the run waits for an answer:
                // the hold goes back until one arrives.
                drop(held.take());
                world.working.note_state(lead, Some(JobState::WaitingOnYou));
                world.working.recount();
                let Some(answer) = answers.recv().await else {
                    // Nobody can answer any more: the task ends, and the
                    // run stays resumable.
                    break Ok(());
                };
                let verdict = runner.answer(
                    &answer.gate,
                    log_answer(answer.answer),
                    answer.amendment.clone(),
                    answer.by.clone(),
                );
                let wrote = verdict.is_ok();
                if wrote {
                    // The build works again: the count still holds it, and
                    // the line's words move with it.
                    world.working.note_state(lead, Some(JobState::Running));
                    world.working.recount();
                }
                let _ = answer
                    .reply
                    .send(verdict.map(|_| ()).map_err(|e| e.to_string()));
                if !wrote {
                    // A refused answer (the wrong gate, a second answer to
                    // one already answered) changes nothing, and the run
                    // still waits: hold nothing and loop back to the gate.
                    seen = broadcast_new(&runs, lead, &runner, seen);
                    continue;
                }
            }
            // Finished, whichever outcome: the log says which, the events
            // it wrote reach the watchers, and the caller only needs to
            // know the task ended.
            Ok(Advanced::Finished { .. }) => {
                broadcast_new(&runs, lead, &runner, seen);
                break Ok(());
            }
            Err(e) => {
                // The log is the source of truth, so the run's own record
                // of the error goes into it before the task ends: a log
                // that stops at `step_started` replays as a run still
                // working. What cannot be read cannot be written, and a
                // failure to write is itself worth a note.
                if let Err(write) = runner.note_run_error(&e.to_string()) {
                    runs.broadcast(
                        lead,
                        Notice::Note {
                            thread: lead,
                            text: format!("could not write the lead's log while stopping: {write}"),
                        },
                    );
                }
                broadcast_new(&runs, lead, &runner, seen);
                break Err(e);
            }
        }
        seen = broadcast_new(&runs, lead, &runner, seen);
    };
    // Released on every exit, `Finished` and an error alike.
    drop(held.take());
    outcome
}

/// The log's answer, from the wire's mirror of it.
fn log_answer(answer: CheckpointAnswer) -> LogAnswer {
    match answer {
        CheckpointAnswer::Go => LogAnswer::Go,
        CheckpointAnswer::Amend => LogAnswer::Amend,
        CheckpointAnswer::Retry => LogAnswer::Retry,
        CheckpointAnswer::Stop => LogAnswer::Stop,
        CheckpointAnswer::Raise => LogAnswer::Raise,
    }
}

/// Broadcast every lead event the runner appended since `seen`, and
/// return the new count.
fn broadcast_new(
    runs: &Runs,
    lead: Ulid,
    runner: &Runner<impl Forge, impl RunnerHost, impl Repo>,
    seen: u64,
) -> u64 {
    let events = runner.log().events();
    let from = (seen as usize).min(events.len());
    for event in &events[from..] {
        runs.broadcast(
            lead,
            Notice::Event {
                thread: lead,
                event: event.clone(),
            },
        );
    }
    events.len() as u64
}

/// The runner the daemon drives: the fake-able seams, named once.
pub type Run = Runner<Arc<dyn Forge + Send + Sync>, ServerHost, Box<dyn Repo + Send + Sync>>;

/// Open the lead's log (repairing a torn tail), load the workflow it
/// names, and build the runner over them.
fn build_runner(
    world: &RunWorld,
    lead: Ulid,
) -> Result<(Run, RunStartedPayload, String), RunnerError> {
    let (log, _cut) =
        ThreadLog::open_with(&world.root.threads_dir, lead, Repair::TruncateTornTail)?;
    let started = run_started_of(&log)?;
    let loaded: LoadedWorkflow = WorkflowFile::load(&started.workflow, &world.workflow_roots())?;
    let content_hash = loaded.content_hash.clone();
    let host = ServerHost::new(world.clone(), lead);
    let repo = world.deps.repo(&world.root);
    let forge = world.deps.forge();
    let installer = world.deps.installer();
    // The project's `[workflow]` switch, read from the run's root rather
    // than from the built context, which a restart does not rebuild.
    let auto_go = aigentic_runtime::project::decide_auto_go(&world.root.root);
    let runner =
        Runner::new(log, lead, forge, host, installer, loaded, repo)?.with_decide_auto_go(auto_go);
    Ok((runner, started, content_hash))
}

/// The lead log's `run_started` payload, or why there is none.
pub fn run_started_of(log: &ThreadLog) -> Result<RunStartedPayload, RunnerError> {
    let event = log
        .events()
        .iter()
        .find(|event| event.kind == EventKind::RunStarted)
        .ok_or(RunnerError::NotARun)?;
    serde_json::from_value(event.payload.clone()).map_err(|_| RunnerError::NotARun)
}

/// A lead's run: its `run_started`, and whether its log holds a
/// `run_finished`. Read from events, with the caller's repaired log.
pub fn unfinished(log: &ThreadLog, issue: u64) -> Option<RunStartedPayload> {
    let started = run_started_of(log).ok()?;
    if started.issue != issue {
        return None;
    }
    let finished = log
        .events()
        .iter()
        .any(|event| event.kind == EventKind::RunFinished);
    (!finished).then_some(started)
}
