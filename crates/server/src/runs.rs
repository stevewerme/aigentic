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
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use aigentic_api::{CheckpointAnswer, Notice};
use aigentic_runtime::aigentic_core::{AgentId, Author, EventKind};
use aigentic_runtime::aigentic_log::{
    CheckpointAnswer as LogAnswer, NewEvent, Repair, RunStartedPayload, ThreadLog,
    ThreadStartedPayload,
};
use aigentic_runtime::runner::forge::{Forge, GhForge};
use aigentic_runtime::runner::git::{GitRepo, Repo};
use aigentic_runtime::runner::install::{CargoInstaller, Installer};
use aigentic_runtime::runner::{Advanced, Runner, RunnerError, RunnerHost};
use aigentic_runtime::workflow::{LoadedWorkflow, WorkflowFile, WorkflowRoots};
use aigentic_runtime::{Mode, Runtime};
use tokio::sync::{mpsc, oneshot};
use ulid::Ulid;

use crate::awake::{Held, KeepAwake};
use crate::build::{ProviderFactory, Root, build_thread};
use crate::config::Config;
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
    /// The profile's provider, built from the config.
    pub providers: Arc<dyn ProviderFactory>,
    /// The forge, installer and repo seams.
    pub deps: Arc<dyn RunDeps>,
    /// The daemon's keep-awake guard: held while a run advances, so a
    /// long step is not a sleeping laptop half way through.
    pub guard: Arc<dyn KeepAwake>,
    /// The project the run works in.
    pub root: Root,
    /// The project's name, for the children's `thread_started`.
    pub project: String,
    /// Workspace files whose roots the daemon also serves.
    pub workspaces: Vec<Workspace>,
    /// The bundled root: its `workflows/` holds the workflows a project
    /// and the user did not override.
    pub bundled: PathBuf,
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
        Ulid::generate()
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
                &*world.providers,
                &world.root,
                &world.workspaces,
                id,
                Some(&profile),
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
        if let Err(e) = std::fs::create_dir_all(threads_dir) {
            return Err(format!("cannot make {}: {e}", threads_dir.display()));
        }
        let file = match std::fs::File::options()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
        {
            Ok(file) => file,
            Err(e) => return Err(format!("cannot open {}: {e}", path.display())),
        };
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
}

impl Runs {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
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
    }

    /// Give up `lead`, so a later claim may drive it again: the task
    /// calls this when it ends, finished or stopped.
    pub fn release(&self, lead: Ulid) {
        self.slots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&lead);
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
    let outcome = run(runs.clone(), &world, lead, &mut answers).await;
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
                // Nothing is working while the run waits for an answer:
                // the hold goes back until one arrives.
                drop(held.take());
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
            Err(e) => break Err(e),
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
        CheckpointAnswer::Stop => LogAnswer::Stop,
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
    let runner = Runner::new(log, lead, forge, host, installer, loaded, repo)?;
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
