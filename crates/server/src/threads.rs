//! The daemon's threads: one actor per open thread, started on first
//! use, unloaded after an idle period with no open sessions and nothing
//! waited for. The log is the state, so unloading loses nothing.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aigentic_api::{CheckpointAnswer, ThreadInfo, ThreadState};
use aigentic_runtime::aigentic_core::{Author, ContentBlock, Event, EventKind};
use aigentic_runtime::aigentic_log::{
    NewEvent, Repair, RunStartedPayload, ThreadLog, ThreadStartedPayload, UserMessagePayload,
};
use aigentic_runtime::aigentic_policy::Participants;
use aigentic_runtime::workflow::WorkflowFile;
use aigentic_runtime::{Project, ProjectFile};
use time::format_description::well_known::Rfc3339;
use tokio::sync::{Mutex as AsyncMutex, mpsc, oneshot};
use ulid::Ulid;

use crate::actor::{Mail, Mailbox, Reports, ThreadActor};
use crate::awake::KeepAwake;
use crate::build::{BuildError, ProviderFactory, Root, build_thread, project_context};
use crate::config::{Config, ServerConfig};
use crate::runs::{Answer, Claim, ProdDeps, RunDeps, RunWorld, Runs, drive};
use crate::skills::SkillPaths;
use crate::workspaces::Workspace;

/// Threads of a root without a project file.
pub const NO_PROJECT_DIR: &str = "_none";

/// The workflow a `Build` runs when the request names none (issue #58):
/// the bundled build workflow, the one the acceptance uses.
pub const DEFAULT_WORKFLOW: &str = "build";

struct Entry {
    mailbox: Mailbox,
    project: String,
    /// Sessions that opened it and have not closed.
    open: usize,
    /// When the last session closed, for the idle clock.
    idle_since: Instant,
}

/// The table, shared by every session.
pub struct ThreadTable {
    config: Arc<Config>,
    config_dir: PathBuf,
    server: Arc<ServerConfig>,
    providers: Arc<dyn ProviderFactory>,
    reports: Arc<dyn Reports>,
    threads_base: PathBuf,
    /// A profile name that wins over every project's `[model] profile`:
    /// the client's `--profile` on an embedded daemon. `None` on a
    /// served daemon, where the profile is the project's.
    profile_override: Option<String>,
    /// Workspace files (phase 6 section 9b); their projects are already
    /// merged into `server.projects`.
    workspaces: Vec<Workspace>,
    /// The daemon's one keep-awake guard (issue #47). Every thread's
    /// actor gets a clone of it, so one program holds the machine awake
    /// for the whole daemon however many threads work at once. A run's
    /// task holds it too, while it advances.
    guard: Arc<dyn KeepAwake>,
    /// Which runs a task drives, and who watches them (issue #58). It
    /// hangs off the table because a session sees nothing but the table
    /// and the config.
    runs: Arc<Runs>,
    /// The forge, installer and repo a run drives. Production's are the
    /// real ones; the tests install their own through `with_run_deps`,
    /// which is why this is swapped under a lock rather than at build
    /// time: a test drives a daemon the way a client does, through
    /// `Server::new`.
    run_deps: Mutex<Arc<dyn RunDeps>>,
    /// Serialises the "is there an unfinished run for this issue?" check
    /// and the lead it creates under it, so two concurrent `Build`s for
    /// one issue cannot both find nothing and both start a run.
    build_lock: AsyncMutex<()>,
    entries: Mutex<HashMap<Ulid, Entry>>,
}

#[derive(Debug, thiserror::Error)]
pub enum ThreadError {
    #[error("no project named {0} on this daemon")]
    NoProject(String),
    #[error("no thread {0} in any project")]
    NoThread(Ulid),
    #[error("{0}")]
    Build(#[from] BuildError),
    #[error("{0}")]
    Log(#[from] aigentic_runtime::aigentic_log::LogError),
    #[error("{0}")]
    Runtime(#[from] aigentic_runtime::RuntimeError),
    #[error("the thread's actor is gone")]
    Gone,
    #[error("no run in thread {0}: its log holds no `run_started`")]
    NotARun(Ulid),
    #[error("workflow: {0}")]
    Workflow(#[from] aigentic_runtime::workflow::WorkflowError),
    #[error("{0}")]
    Refused(String),
}

impl ThreadTable {
    pub fn new(
        config: Arc<Config>,
        config_dir: PathBuf,
        server: Arc<ServerConfig>,
        providers: Arc<dyn ProviderFactory>,
        reports: Arc<dyn Reports>,
        threads_base: PathBuf,
        guard: Arc<dyn KeepAwake>,
    ) -> Self {
        Self {
            config,
            config_dir,
            server,
            providers,
            reports,
            threads_base,
            guard,
            runs: Arc::new(Runs::new()),
            run_deps: Mutex::new(Arc::new(ProdDeps)),
            build_lock: AsyncMutex::new(()),
            profile_override: None,
            workspaces: Vec::new(),
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// The guard every actor is built with: a test installs a recording
    /// one here, so a thread built from the table can be watched.
    pub fn guard(&self) -> Arc<dyn KeepAwake> {
        self.guard.clone()
    }

    /// The run registry: which lead a task drives, and who watches it.
    pub fn runs(&self) -> Arc<Runs> {
        self.runs.clone()
    }

    /// Replace the seams a run drives the world through. `ProdDeps`
    /// unless a test installed its own; see [`crate::runs::RunDeps`].
    pub fn with_run_deps(&self, deps: Arc<dyn RunDeps>) {
        *self.run_deps.lock().unwrap_or_else(|e| e.into_inner()) = deps;
    }

    /// The seams a run drives the world through, as they stand now.
    pub fn run_deps(&self) -> Arc<dyn RunDeps> {
        self.run_deps
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Build every thread from `profile` instead of its project's
    /// `[model] profile`; the embedded daemon's `--profile`.
    pub fn with_profile(mut self, profile: Option<String>) -> Self {
        self.profile_override = profile;
        self
    }

    pub fn with_workspaces(mut self, workspaces: Vec<Workspace>) -> Self {
        self.workspaces = workspaces;
        self
    }

    pub fn workspaces(&self) -> &[Workspace] {
        &self.workspaces
    }

    /// Move an open thread to `project`: its context is built here, the
    /// actor swaps it in while idle and records `project_switched`.
    pub async fn switch(&self, thread: Ulid, project: &str, by: Author) -> Result<(), ThreadError> {
        let mailbox = self.mailbox(thread).ok_or(ThreadError::NoThread(thread))?;
        let root = self.root_of(project)?;
        let built = project_context(
            &self.config,
            &self.config_dir,
            &*self.providers,
            &root,
            &self.workspaces,
            self.profile_override.as_deref(),
        )
        .await?;
        let (reply, rx) = oneshot::channel();
        mailbox
            .send(Mail::SwitchProject {
                ctx: Box::new(built.ctx),
                by,
                reply,
            })
            .map_err(|_| ThreadError::Gone)?;
        match rx.await.map_err(|_| ThreadError::Gone)? {
            aigentic_api::Response::Ok => {
                if let Some(e) = self
                    .entries
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get_mut(&thread)
                {
                    e.project = project.to_owned();
                }
                Ok(())
            }
            aigentic_api::Response::Refused { reason } => Err(ThreadError::Refused(reason)),
            other => Err(ThreadError::Refused(format!("{other:?}"))),
        }
    }

    fn root_of(&self, project: &str) -> Result<Root, ThreadError> {
        let p = self
            .server
            .project(project)
            .ok_or_else(|| ThreadError::NoProject(project.to_owned()))?;
        Ok(Root {
            name: p.name.clone(),
            root: p.root.clone(),
            threads_dir: self.threads_base.join(&p.name),
        })
    }

    /// The project's participants and the daemon's owner, for the role
    /// check. A root without a project file has no participants.
    pub fn participants(&self, project: &str) -> Result<Participants, ThreadError> {
        let root = self.root_of(project)?;
        let file = root.root.join(aigentic_runtime::project::FILE_NAME);
        if !file.is_file() {
            return Ok(Participants::default());
        }
        let file = ProjectFile::load(&file).map_err(BuildError::from)?;
        Ok(file.participants)
    }

    /// Which project a thread belongs to: the directory that holds its
    /// log, since the directory is the index (phase 4).
    pub fn project_of(&self, thread: Ulid) -> Option<String> {
        if let Some(e) = self
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&thread)
        {
            return Some(e.project.clone());
        }
        self.server
            .projects
            .iter()
            .map(|p| p.name.clone())
            .find(|name| {
                self.threads_base
                    .join(name)
                    .join(format!("{thread}.jsonl"))
                    .is_file()
            })
    }

    pub fn projects(&self) -> Vec<(String, PathBuf, u64)> {
        self.server
            .projects
            .iter()
            .map(|p| {
                let count = std::fs::read_dir(self.threads_base.join(&p.name))
                    .map(|d| {
                        d.filter_map(Result::ok)
                            .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
                            .count() as u64
                    })
                    .unwrap_or(0);
                (p.name.clone(), p.root.clone(), count)
            })
            .collect()
    }

    /// Every thread of a project, newest first, with the state of the
    /// ones that are open.
    pub fn list(&self, project: &str) -> Result<Vec<ThreadInfo>, ThreadError> {
        let root = self.root_of(project)?;
        let mut ids: Vec<Ulid> = match std::fs::read_dir(&root.threads_dir) {
            Ok(d) => d
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
                .filter_map(|p| p.file_stem()?.to_str()?.parse().ok())
                .collect(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(BuildError::from(e).into()),
        };
        ids.sort_unstable_by(|a, b| b.cmp(a));
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        Ok(ids
            .into_iter()
            .map(|id| {
                let mut info = summarise(&root.threads_dir, id, project);
                if entries.contains_key(&id) {
                    // Open: the actor knows the live state; the session
                    // asks it on Open. Listings show it as idle unless
                    // asked, which is cheap and never wrong for long.
                    info.state = ThreadState::Idle;
                }
                info
            })
            .collect())
    }

    /// A new thread in `project`: its log starts with `thread_started`.
    pub async fn create(&self, project: &str, by: Author) -> Result<ThreadInfo, ThreadError> {
        let root = self.root_of(project)?;
        std::fs::create_dir_all(&root.threads_dir).map_err(BuildError::from)?;
        let id = Ulid::generate();
        let mut log = ThreadLog::open(&root.threads_dir, id)?;
        log.append(NewEvent {
            kind: EventKind::ThreadStarted,
            author: by.clone(),
            payload: serde_json::to_value(ThreadStartedPayload {
                project: Some(project.to_owned()),
                root: root.root.clone(),
                created_by: by,
                parent_thread: None,
                step: None,
            })
            .expect("serialisable"),
            parent_event: None,
        })?;
        drop(log);
        Ok(summarise(&root.threads_dir, id, project))
    }

    /// The mailbox of a thread, starting its actor when it is not open.
    /// Counts one more open session on it.
    pub async fn open(&self, thread: Ulid) -> Result<(Mailbox, String), ThreadError> {
        {
            let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(e) = entries.get_mut(&thread) {
                e.open += 1;
                return Ok((e.mailbox.clone(), e.project.clone()));
            }
        }
        // The log lives under the project it was created in; the thread
        // is built in the project it last switched to.
        let home = self
            .project_of(thread)
            .ok_or(ThreadError::NoThread(thread))?;
        let home_root = self.root_of(&home)?;
        let project = last_switch(&home_root.threads_dir, thread)
            .filter(|p| self.server.project(p).is_some())
            .unwrap_or_else(|| home.clone());
        let root = Root {
            threads_dir: home_root.threads_dir,
            ..self.root_of(&project)?
        };
        let built = build_thread(
            &self.config,
            &self.config_dir,
            &*self.providers,
            &root,
            &self.workspaces,
            thread,
            self.profile_override.as_deref(),
        )
        .await?;
        let (actor, mailbox) = ThreadActor::new(built.runtime, built.torn, self.reports.clone())?;
        let actor = actor.with_keep_awake(self.guard.clone());
        tokio::spawn(actor.run());
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        // Two sessions may have raced to build it; the first in wins and
        // the second's actor is dropped with its mailbox.
        let e = entries.entry(thread).or_insert_with(|| Entry {
            mailbox,
            project: project.clone(),
            open: 0,
            idle_since: Instant::now(),
        });
        e.open += 1;
        Ok((e.mailbox.clone(), e.project.clone()))
    }

    /// One session closed the thread.
    pub fn close(&self, thread: Ulid) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(e) = entries.get_mut(&thread) {
            e.open = e.open.saturating_sub(1);
            if e.open == 0 {
                e.idle_since = Instant::now();
            }
        }
    }

    /// The mailbox of an open thread, without counting a session.
    pub fn mailbox(&self, thread: Ulid) -> Option<Mailbox> {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&thread)
            .map(|e| e.mailbox.clone())
    }

    /// Unload every thread that has had no open session for `idle` and
    /// is not running or waiting. Returns what was unloaded. Called by
    /// the daemon's sweeper and by tests.
    pub async fn sweep(&self, idle: Duration) -> Vec<Ulid> {
        let candidates: Vec<(Ulid, Mailbox)> = self
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|(_, e)| e.open == 0 && e.idle_since.elapsed() >= idle)
            .map(|(id, e)| (*id, e.mailbox.clone()))
            .collect();
        let mut unloaded = Vec::new();
        for (id, mailbox) in candidates {
            let (reply, rx) = oneshot::channel();
            if mailbox.send(Mail::Status { reply }).is_err() {
                continue;
            }
            let Ok(state) = rx.await else { continue };
            if state != ThreadState::Idle {
                continue; // never while running or awaiting anyone
            }
            let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
            if entries.get(&id).is_some_and(|e| e.open == 0) {
                entries.remove(&id); // the last mailbox: the actor's loop ends
                unloaded.push(id);
            }
        }
        unloaded
    }

    pub fn open_count(&self) -> usize {
        self.entries.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    // -- runs (issue #58) ----------------------------------------------------

    /// What a run's task needs of the daemon for `project`: the config,
    /// the providers, the seams, the guard, and the project's root. No
    /// handle on this table comes with it.
    pub fn run_world(&self, project: &str) -> Result<RunWorld, ThreadError> {
        let root = self.root_of(project)?;
        let bundled = SkillPaths::new(
            &root.root,
            &self.config_dir,
            self.config.bundled_dir.as_deref(),
        )
        .bundled;
        Ok(RunWorld {
            config: self.config.clone(),
            config_dir: self.config_dir.clone(),
            providers: self.providers.clone(),
            deps: self.run_deps(),
            guard: self.guard.clone(),
            root,
            project: project.to_owned(),
            workspaces: self.workspaces.clone(),
            bundled,
        })
    }

    /// The lead of the unfinished run for `(project, issue)`: a thread
    /// whose log holds `run_started { issue }` and no `run_finished`.
    /// Read with repair, because a `kill -9` leaves a torn tail.
    pub fn unfinished_run(&self, project: &str, issue: u64) -> Option<Ulid> {
        let root = self.root_of(project).ok()?;
        let ids = thread_ids(&root.threads_dir);
        // Newest first: a run that was restarted has one lead per issue,
        // and the newest is the one the last `Build` made.
        for id in ids {
            let Ok((log, _cut)) =
                ThreadLog::open_with(&root.threads_dir, id, Repair::TruncateTornTail)
            else {
                continue;
            };
            if crate::runs::unfinished(&log, issue).is_some() {
                return Some(id);
            }
        }
        None
    }

    /// Which kind of run-owned thread `thread` is, from its own log.
    pub fn run_thread(&self, thread: Ulid) -> RunThread {
        let Some(project) = self.project_of(thread) else {
            return RunThread::No;
        };
        let Ok(root) = self.root_of(&project) else {
            return RunThread::No;
        };
        // Repair: a torn tail is not a reason to start an actor over a
        // run-owned thread.
        let Ok((log, _cut)) =
            ThreadLog::open_with(&root.threads_dir, thread, Repair::TruncateTornTail)
        else {
            return RunThread::No;
        };
        if let Ok(started) = crate::runs::run_started_of(&log) {
            return RunThread::Lead {
                issue: started.issue,
            };
        }
        let child = log
            .events()
            .iter()
            .filter(|event| event.kind == EventKind::ThreadStarted)
            .filter_map(|event| {
                serde_json::from_value::<ThreadStartedPayload>(event.payload.clone()).ok()
            })
            .find_map(|p| p.parent_thread);
        match child {
            Some(lead) => {
                let step = log
                    .events()
                    .iter()
                    .filter(|event| event.kind == EventKind::ThreadStarted)
                    .filter_map(|event| {
                        serde_json::from_value::<ThreadStartedPayload>(event.payload.clone()).ok()
                    })
                    .find_map(|p| p.step);
                RunThread::Child { lead, step }
            }
            None => RunThread::No,
        }
    }

    /// The events of `thread` from `from_seq`, oldest first, read with
    /// repair. No actor is started and nothing is written.
    pub fn events_from(&self, thread: Ulid, from_seq: u64) -> Result<Vec<Event>, ThreadError> {
        let project = self
            .project_of(thread)
            .ok_or(ThreadError::NoThread(thread))?;
        let root = self.root_of(&project)?;
        let (log, _cut) =
            ThreadLog::open_with(&root.threads_dir, thread, Repair::TruncateTornTail)?;
        Ok(log
            .events()
            .iter()
            .filter(|event| event.seq >= from_seq)
            .cloned()
            .collect())
    }

    /// The start-up scan (issue #58, rule 8): every unfinished run in
    /// every project is claimed, so a daemon killed mid-run picks the runs
    /// up again. A served daemon does this; an embedded one
    /// (`resume_runs: false`) does not — opening the REPL must not
    /// silently resume someone's build and push.
    ///
    /// A lead waiting at a checkpoint gets a task that waits; one left
    /// mid-child continues it. A finished lead is ignored, and so is a
    /// lead holding only `thread_started`.
    pub fn resume_unfinished_runs(&self) {
        if !self.server.resume_runs {
            return;
        }
        let projects: Vec<String> = self
            .server
            .projects
            .iter()
            .map(|p| p.name.clone())
            .collect();
        for project in projects {
            let Ok(root) = self.root_of(&project) else {
                continue;
            };
            let Ok(world) = self.run_world(&project) else {
                continue;
            };
            for id in thread_ids(&root.threads_dir) {
                // Repair, like every other read here: a `kill -9` mid-write
                // leaves a half line that was never an event.
                let Ok((log, _cut)) =
                    ThreadLog::open_with(&root.threads_dir, id, Repair::TruncateTornTail)
                else {
                    continue;
                };
                let Some(started) = crate::runs::run_started_of(&log).ok() else {
                    continue;
                };
                if crate::runs::unfinished(&log, started.issue).is_none() {
                    continue;
                }
                self.claim_or_attach(id, &world);
            }
        }
    }

    /// Start a run for `(project, issue)`, or resume the one already
    /// there, and answer with its lead. `resumed` says which happened.
    ///
    /// The whole check-and-create runs under one lock per table, so two
    /// concurrent `Build`s for one issue cannot both find no run and both
    /// create one. Exactly one task ever drives a lead: [`Runs::claim`]
    /// decides it.
    pub async fn build_run(
        &self,
        project: &str,
        issue: u64,
        workflow: Option<String>,
        by: Author,
    ) -> Result<(Ulid, bool), ThreadError> {
        let _serial = self.build_lock.lock().await;
        let world = self.run_world(project)?;
        if let Some(lead) = self.unfinished_run(project, issue) {
            self.claim_or_attach(lead, &world);
            return Ok((lead, true));
        }
        // The workflow defaults to `build`, the name the acceptance uses
        // (issue #58, rule 4).
        let name = workflow.unwrap_or_else(|| DEFAULT_WORKFLOW.to_owned());
        let loaded = WorkflowFile::load(&name, &world.workflow_roots())?;
        let info = self.create(project, by.clone()).await?;
        let lead = info.id;
        // The lead's log starts `thread_started`, then `run_started`: the
        // runner insists on it, and this is the last write before a task
        // owns the log.
        let (mut log, _cut) =
            ThreadLog::open_with(&world.root.threads_dir, lead, Repair::TruncateTornTail)?;
        if !log
            .events()
            .iter()
            .any(|event| event.kind == EventKind::RunStarted)
        {
            log.append(NewEvent {
                kind: EventKind::RunStarted,
                author: by,
                payload: serde_json::to_value(RunStartedPayload {
                    issue,
                    workflow: name,
                    version: loaded.workflow.version,
                    content_hash: loaded.content_hash,
                    budget_usd: loaded.workflow.budget.full,
                })
                .expect("run_started serialises"),
                parent_event: None,
            })?;
        }
        self.claim_or_attach(lead, &world);
        Ok((lead, false))
    }

    /// Send one answer to the task that drives `lead`, starting that task
    /// when nobody holds the lead — the start-up scan may not have
    /// reached it, or its task may have ended. Blocks until the task has
    /// appended the answer or refused it.
    ///
    /// Nothing is written here: the task owns the lead log and checks the
    /// gate immediately before appending, so a second answer to one gate
    /// and an answer to a gate the run is not asking at are both refused.
    pub async fn answer_checkpoint(
        &self,
        lead: Ulid,
        gate: String,
        answer: CheckpointAnswer,
        amendment: Option<String>,
        by: Author,
    ) -> Result<(), ThreadError> {
        let RunThread::Lead { .. } = self.run_thread(lead) else {
            return Err(ThreadError::NotARun(lead));
        };
        let project = self.project_of(lead).ok_or(ThreadError::NoThread(lead))?;
        // The open gate is the run's, not the answerer's: check it here,
        // so a wrong gate, a run that is not waiting and a run that has
        // already finished are refused before a task is started over it.
        // The runner checks again, immediately before it appends.
        let root = self.root_of(&project)?;
        let (log, _cut) = ThreadLog::open_with(&root.threads_dir, lead, Repair::TruncateTornTail)?;
        match aigentic_runtime::aigentic_log::run_state(log.events())?.next_move() {
            aigentic_runtime::aigentic_log::NextMove::AwaitingCheckpoint { gate: asked }
                if asked == gate => {}
            aigentic_runtime::aigentic_log::NextMove::AwaitingCheckpoint { gate: asked } => {
                return Err(ThreadError::Refused(format!(
                    "the run waits at `{asked}`, not `{gate}`"
                )));
            }
            _ => {
                return Err(ThreadError::Refused(format!(
                    "the run is not waiting at a gate, so `{gate}` cannot be answered"
                )));
            }
        }
        drop(log);
        let world = self.run_world(&project)?;
        let tx = self.claim_or_attach(lead, &world);
        let (reply, rx) = oneshot::channel();
        tx.send(Answer {
            gate,
            answer,
            amendment,
            by,
            reply,
        })
        .map_err(|_| ThreadError::Gone)?;
        match rx.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(reason)) => Err(ThreadError::Refused(reason)),
            Err(_) => Err(ThreadError::Gone),
        }
    }

    /// Claim `lead` for this caller, starting a task over it when nobody
    /// holds it, and return where to send its answers. An [`Claim::Attached`]
    /// lead gets no second runner.
    fn claim_or_attach(&self, lead: Ulid, world: &RunWorld) -> mpsc::UnboundedSender<Answer> {
        match self.runs.claim(lead) {
            Claim::Attached(tx) => tx,
            Claim::Claimed(rx, tx) => {
                let runs = self.runs.clone();
                let world = world.clone();
                tokio::spawn(async move { drive(runs, world, lead, rx).await });
                tx
            }
        }
    }

    /// Claim every unfinished run of every project, as a restarting
    /// daemon does: a run left mid-step resumes, a run waiting at a
    /// checkpoint gets a task that waits for an answer. Finished leads
    /// are left alone. Returns how many runs a task was started over.
    ///
    /// An embedded daemon never calls this: opening the terminal must not
    /// silently resume someone's build and push.
    pub fn resume_runs(&self) -> usize {
        let mut started = 0;
        for (project, _root, _count) in self.projects() {
            let lead_world = self.run_world(&project).ok();
            let Some(world) = lead_world else { continue };
            let ids = thread_ids(&world.root.threads_dir);
            for id in ids {
                let Ok((log, _cut)) =
                    ThreadLog::open_with(&world.root.threads_dir, id, Repair::TruncateTornTail)
                else {
                    continue;
                };
                if crate::runs::run_started_of(&log).is_err() {
                    continue;
                }
                if log
                    .events()
                    .iter()
                    .any(|event| event.kind == EventKind::RunFinished)
                {
                    continue;
                }
                match self.runs.claim(id) {
                    Claim::Attached(_) => {}
                    Claim::Claimed(rx, _tx) => {
                        let runs = self.runs.clone();
                        let world = world.clone();
                        tokio::spawn(async move { drive(runs, world, id, rx).await });
                        started += 1;
                    }
                }
            }
        }
        started
    }
}

/// What a thread is to a run (issue #58, rule 5): a lead, a step child,
/// or an ordinary thread the daemon may run an actor for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunThread {
    /// Not part of a run: an ordinary thread.
    No,
    /// A run's lead; the run is for `issue`.
    Lead {
        /// The issue the run was started for.
        issue: u64,
    },
    /// A step's child thread, written by a run's runner: `lead` is the
    /// run it belongs to, `step` the step it works when the header names
    /// one.
    Child { lead: Ulid, step: Option<String> },
}

impl RunThread {
    /// This thread on the wire (issue #58): `None` for a thread no run
    /// owns.
    pub fn wire(&self) -> Option<aigentic_api::RunThread> {
        match self {
            RunThread::No => None,
            RunThread::Lead { issue } => Some(aigentic_api::RunThread::Lead { issue: *issue }),
            RunThread::Child { lead, step } => Some(aigentic_api::RunThread::Child {
                lead: *lead,
                step: step.clone(),
            }),
        }
    }
}

/// Every thread id in `dir`, newest first. An unreadable directory is
/// empty, not an error: a project that never ran anything has none.
fn thread_ids(dir: &Path) -> Vec<Ulid> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut ids: Vec<Ulid> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("jsonl"))
        .filter_map(|path| path.file_stem()?.to_str()?.parse().ok())
        .collect();
    ids.sort_unstable_by(|a, b| b.cmp(a));
    ids
}

/// A listing row from the log alone.
fn summarise(dir: &Path, id: Ulid, project: &str) -> ThreadInfo {
    let fallback_date = || {
        time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(id.timestamp_ms()) * 1_000_000)
            .ok()
            .and_then(|t| t.format(&Rfc3339).ok())
            .map(|s| s[..10].to_owned())
            .unwrap_or_default()
    };
    let events = ThreadLog::open(dir, id).and_then(|log| log.read_all());
    let Ok(events) = events else {
        return ThreadInfo {
            id,
            project: Some(project.to_owned()),
            date: fallback_date(),
            events: 0,
            first_line: String::new(),
            state: ThreadState::Idle,
            title: None,
        };
    };
    let date = events
        .first()
        .and_then(|e| e.created_at.format(&Rfc3339).ok())
        .map(|s| s[..10].to_owned())
        .unwrap_or_else(fallback_date);
    let recorded_project = events
        .first()
        .filter(|e| e.kind == EventKind::ThreadStarted)
        .and_then(|e| serde_json::from_value::<ThreadStartedPayload>(e.payload.clone()).ok())
        .and_then(|p| p.project);
    let first_line = events
        .iter()
        .find(|e| e.kind == EventKind::UserMessage)
        .and_then(|e| serde_json::from_value::<UserMessagePayload>(e.payload.clone()).ok())
        .and_then(|p| {
            p.blocks.into_iter().find_map(|b| match b {
                ContentBlock::Text(t) => Some(t),
                _ => None,
            })
        })
        .map(|t| first_line_of(&t))
        .unwrap_or_default();
    ThreadInfo {
        id,
        project: recorded_project.or_else(|| Some(project.to_owned())),
        date,
        events: events.len() as u64,
        first_line,
        state: ThreadState::Idle,
        title: aigentic_runtime::title::title_of(&events),
    }
}

/// The project a thread last switched to, from its log.
fn last_switch(dir: &Path, id: Ulid) -> Option<String> {
    let events = ThreadLog::open(dir, id).ok()?.read_all().ok()?;
    events
        .iter()
        .rev()
        .filter(|e| e.kind == EventKind::ProjectSwitched)
        .find_map(|e| {
            serde_json::from_value::<aigentic_runtime::aigentic_log::ProjectSwitchedPayload>(
                e.payload.clone(),
            )
            .ok()
        })
        .and_then(|p| p.to)
}

const FIRST_LINE_CHARS: usize = 72;

fn first_line_of(text: &str) -> String {
    let line = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    let mut out: String = line.chars().take(FIRST_LINE_CHARS).collect();
    if out.chars().count() < line.chars().count() {
        out.push('…');
    }
    out
}

/// Whether a root holds a project file; the embedded daemon uses it to
/// name the bare-directory project.
pub fn has_project_file(root: &Path) -> bool {
    root.join(aigentic_runtime::project::FILE_NAME).is_file()
}

/// The project name a root would have: its file's, else `_none`.
pub fn project_name_at(root: &Path) -> String {
    if has_project_file(root) {
        Project::open_root(root)
            .map(|p| p.name)
            .unwrap_or_else(|_| NO_PROJECT_DIR.into())
    } else {
        NO_PROJECT_DIR.into()
    }
}
