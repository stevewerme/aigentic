//! The daemon's threads: one actor per open thread, started on first
//! use, unloaded after an idle period with no open sessions and nothing
//! waited for. The log is the state, so unloading loses nothing.

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aigentic_api::{CheckpointAnswer, Notice, ReportKind, ThreadInfo, ThreadState};
use aigentic_runtime::ProjectFile;
use aigentic_runtime::aigentic_core::{Author, ContentBlock, Event, EventKind, UserId};
use aigentic_runtime::aigentic_log::{
    NewEvent, Repair, RunStartedPayload, ThreadLog, ThreadStartedPayload, UserMessagePayload,
};
use aigentic_runtime::aigentic_policy::Participants;
use aigentic_runtime::workflow::WorkflowFile;
use time::format_description::well_known::Rfc3339;
use tokio::sync::{Mutex as AsyncMutex, mpsc, oneshot};
use ulid::Ulid;

use crate::actor::{Mail, Mailbox, Reports, ThreadActor};
use crate::awake::KeepAwake;
use crate::build::{BuildError, ProviderFactory, Root, build_thread, project_context};
use crate::config::{Config, ServerConfig};
use crate::listing::{Listed, projects_listing};
use crate::migrate::Migrated;
use crate::runs::{Answer, Claim, IssueLock, ProdDeps, RunDeps, RunWorld, Runs, drive};
use crate::session::role_in_project;
use crate::skills::SkillPaths;
use crate::workspaces::{Workspace, workspace_of};

/// The project name a pre-phase-6 log wrote for a root with no project
/// file, when the per-project directory was the index. #9 dropped the
/// stand-in name — a bare root is named by `project_name_at` now — but a
/// log already written still says this, so the index must know it.
const LEGACY_NONE_PROJECT: &str = "_none";

/// The workflow a `Build` runs when the request names none (issue #58):
/// the bundled build workflow, the one the acceptance uses.
pub const DEFAULT_WORKFLOW: &str = "build";

struct Entry {
    mailbox: Mailbox,
    project: String,
    /// Who started the thread, from its log's `thread_started` (issue
    /// #81): the listing is theirs, so someone else opening the thread
    /// sees the same one. `None` for a thread an agent started — a
    /// build's or a step's child — which gets no block.
    creator: Option<UserId>,
    /// Sessions that opened it and have not closed.
    open: usize,
    /// When the last session closed, for the idle clock.
    idle_since: Instant,
}

/// One thread, as its log says it is (issue #9). The per-project
/// directory used to be the index; now the log is, and this is what a
/// daemon start reads out of it.
#[derive(Debug, Clone)]
pub struct Indexed {
    /// Where its log lives: `<base>/<id>.jsonl`, or `<base>/<project>/`
    /// while a held lead waits for a later start.
    pub dir: PathBuf,
    /// The project the log says the thread is in, canonicalised for a
    /// pre-phase-6 `_none` thread.
    pub home: Option<String>,
    /// The last `project_switched.to`.
    pub switched: Option<String>,
    /// `thread_started.root`.
    pub root: Option<PathBuf>,
    /// Who started it, a person only, filled only where it is read.
    pub creator: Option<UserId>,
    /// The run this thread is part of, if any.
    pub run: RunThread,
    /// A line promised a `run_started` and wasn't: its issue is unknown.
    pub torn_run: bool,
    /// `thread_started.front` (issue #84): the person's front thread,
    /// the newest of which plain `aigentic` reopens.
    pub front: bool,
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
    /// task holds it too, while it advances. Swapped under a lock so a
    /// test can install a recording guard, as `with_run_deps` swaps the
    /// world's seams.
    guard: Mutex<Arc<dyn KeepAwake>>,
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
    /// and the lead it creates under it within this process; the issue's
    /// OS lock ([`IssueLock`]) does the same across processes.
    build_lock: AsyncMutex<()>,
    entries: Mutex<HashMap<Ulid, Entry>>,
    /// Which project and which directory each thread is in, read from
    /// the logs at start-up and kept in step as threads are made and
    /// switch (issue #9). A miss is a cache miss, never "no thread".
    index: Mutex<HashMap<Ulid, Indexed>>,
    /// What `migrate` did at start-up: the move's counts, or why it
    /// failed. Kept for tests and a later `doctor` (issue #9).
    migrated: Mutex<Result<Migrated, String>>,
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
        // The index is read before anything can serve from it, so a
        // daemon that starts over a tree another one wrote sees every
        // log that is there now (issue #9).
        let index = scan_index(&threads_base, &server);
        Self {
            config,
            config_dir,
            server,
            providers,
            reports,
            threads_base,
            guard: Mutex::new(guard),
            runs: Arc::new(Runs::new()),
            run_deps: Mutex::new(Arc::new(ProdDeps)),
            build_lock: AsyncMutex::new(()),
            profile_override: None,
            workspaces: Vec::new(),
            entries: Mutex::new(HashMap::new()),
            index: Mutex::new(index),
            migrated: Mutex::new(Ok(Migrated::default())),
        }
    }

    /// The guard every actor is built with: a test installs a recording
    /// one here, so a thread built from the table can be watched.
    pub fn guard(&self) -> Arc<dyn KeepAwake> {
        self.guard.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Install the keep-awake guard every actor and run task gets from
    /// now on: a test's recording one, so it can watch what a run holds,
    /// step by step (#58 fix 1). A thread built before the swap keeps
    /// the guard it was built with.
    pub fn with_keep_awake(&self, guard: Arc<dyn KeepAwake>) {
        *self.guard.lock().unwrap_or_else(|e| e.into_inner()) = guard;
    }

    /// The run registry: which lead a task drives, and who watches it.
    pub fn runs(&self) -> Arc<Runs> {
        self.runs.clone()
    }

    /// What the start-up migration did to the threads directory: the
    /// counts, or why it failed. An error never stopped the daemon
    /// (issue #9).
    pub fn migrated(&self) -> Result<Migrated, String> {
        self.migrated
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Record what `migrate` did, kept for tests and a later `doctor`.
    pub fn with_migrated(self, migrated: Result<Migrated, String>) -> Self {
        *self.migrated.lock().unwrap_or_else(|e| e.into_inner()) = migrated;
        self
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
        let ctx = self.build_target(thread, project).await?;
        let (reply, rx) = oneshot::channel();
        mailbox
            .send(Mail::SwitchProject {
                ctx: Box::new(ctx),
                by,
                reply,
            })
            .map_err(|_| ThreadError::Gone)?;
        match rx.await.map_err(|_| ThreadError::Gone)? {
            aigentic_api::Response::Ok => {
                self.note_project(thread, project);
                Ok(())
            }
            aigentic_api::Response::Refused { reason } => Err(ThreadError::Refused(reason)),
            other => Err(ThreadError::Refused(format!("{other:?}"))),
        }
    }

    /// The context a move to `project` needs: its providers, seams and
    /// listing, built the one way both a `SwitchProject` and a `yes` to
    /// a `suggest_project` proposal use (issue #7). The listing follows
    /// the switch: the target is now the current project. The creator
    /// comes from the entry, never a log read — a switch must not touch
    /// another project's threads directory (#81).
    pub async fn build_target(
        &self,
        thread: Ulid,
        project: &str,
    ) -> Result<aigentic_runtime::ProjectContext, ThreadError> {
        let root = self.root_of(project)?;
        let mut built = project_context(
            &self.config,
            &self.config_dir,
            &*self.providers,
            &root,
            &self.workspaces,
            self.profile_override.as_deref(),
        )
        .await?;
        built.ctx.projects = self.projects_in_reach(self.creator(thread).as_ref(), Some(project));
        Ok(built.ctx)
    }

    /// Remember that the thread is in `project` now. The log's
    /// `project_switched` is the truth; this is the entry, so the next
    /// open does not rebuild the old project.
    pub fn note_project(&self, thread: Ulid, project: &str) {
        if let Some(e) = self
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&thread)
        {
            e.project = project.to_owned();
        }
        // The index too, so a listing and the next open see the switch
        // without rereading the log (issue #9).
        if let Some(i) = self
            .index
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&thread)
        {
            i.switched = Some(project.to_owned());
        }
    }

    /// The project a live `suggest_project` proposal names, when one
    /// waits on `call_id` (issue #7): what a `yes` needs before it can
    /// build the target's context and check the role there. `None` when
    /// nothing is pending — the turn has moved on, or another client
    /// answered first.
    pub async fn waiting_switch(&self, thread: Ulid, call_id: &str) -> Option<String> {
        let mailbox = self.mailbox(thread)?;
        let (reply, rx) = oneshot::channel();
        mailbox.send(Mail::Status { reply }).ok()?;
        match rx.await.ok()? {
            ThreadState::AwaitingSwitch {
                call_id: waiting,
                project,
                ..
            } if waiting == call_id => Some(project),
            _ => None,
        }
    }

    /// Names the workspace a project sits in (issue #7), for the actor:
    /// `workspace_label` without the table, so an actor never holds one.
    fn labels(&self) -> crate::actor::WorkspaceLabel {
        let projects = self.server.projects.clone();
        let workspaces = self.workspaces.clone();
        Arc::new(move |project: &str| {
            let root = &projects.iter().find(|p| p.name == project)?.root;
            workspace_of(&workspaces, root).map(|w| w.name.clone())
        })
    }

    /// The workspace naming a project (issue #81): `workspace_of`'s
    /// first match over the loaded workspace files. #7's
    /// `AwaitingSwitch { workspace }` reuses it.
    pub fn workspace_label(&self, project: &str) -> Option<String> {
        let root = self.root_of(project).ok()?;
        workspace_of(&self.workspaces, &root.root).map(|w| w.name.clone())
    }

    /// The projects block for a thread (issue #81), rendered for whoever
    /// asks: the projects its creator holds a role in, the thread's
    /// `project` marked as its own. `None` when the creator is not a
    /// person (an agent's thread) or holds no role anywhere — no block
    /// at all, so the prefix is exactly what it was.
    pub fn shown_projects(&self, thread: Ulid, project: Option<&str>) -> Option<String> {
        let creator = self.creator(thread);
        self.projects_in_reach(creator.as_ref(), project)
    }

    /// The thread's creator, stored when it was opened; `None` for an
    /// agent's thread and for one this table has not opened.
    fn creator(&self, thread: Ulid) -> Option<UserId> {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&thread)
            .and_then(|e| e.creator.clone())
    }

    /// The listing for a creator: the projects where they hold any role
    /// — `read` counts — in the daemon's order, each with the workspace
    /// naming it, and `project` as the thread's own. The rule is
    /// `role_in_project`'s, the same one the client's project list uses.
    pub(crate) fn projects_in_reach(
        &self,
        creator: Option<&UserId>,
        project: Option<&str>,
    ) -> Option<String> {
        let user = creator?.0.as_str();
        let all: Vec<Listed> = self
            .server
            .projects
            .iter()
            .filter(|p| role_in_project(&self.server, self, &p.name, user).is_some())
            .map(|p| Listed {
                name: p.name.clone(),
                root: p.root.clone(),
                workspace: self.workspace_label(&p.name),
            })
            .collect();
        let current = project.and_then(|name| {
            all.iter().find(|l| l.name == name).cloned().or_else(|| {
                // The thread's own project can be outside its
                // creator's reach: line 1 still names it, from
                // itself, and no row is marked as it.
                self.server.project(name).map(|p| Listed {
                    name: p.name.clone(),
                    root: p.root.clone(),
                    workspace: self.workspace_label(&p.name),
                })
            })
        });
        let home = std::env::var_os("HOME").map(PathBuf::from);
        projects_listing(&all, current.as_ref(), home.as_deref())
    }

    fn root_of(&self, project: &str) -> Result<Root, ThreadError> {
        let p = self
            .server
            .project(project)
            .ok_or_else(|| ThreadError::NoProject(project.to_owned()))?;
        Ok(Root {
            name: p.name.clone(),
            root: p.root.clone(),
            // New threads and new children are written flat (issue #9):
            // the directory is no longer the index, so nothing is filed
            // by project.
            threads_dir: self.threads_base.clone(),
        })
    }

    /// The directory a thread's log lives in: `<base>` for everything
    /// written since #9, `<base>/<project>` for a held lead the
    /// migration left behind.
    pub fn dir_of(&self, thread: Ulid) -> Option<PathBuf> {
        self.lookup(thread).map(|i| i.dir.clone())
    }

    /// Who started a thread, from the index (issue #81): only a person
    /// counts, so an agent's thread — a build's or a step's child — gets
    /// no block. A miss reads the log, which is why this is a method and
    /// not a free function over a directory any more (issue #9).
    fn creator_of(&self, thread: Ulid) -> Option<UserId> {
        self.lookup(thread)?.creator
    }

    /// The thread's title, from its log's last `thread_renamed` (issue
    /// #9): read on demand, since it is not on any request's hot path.
    /// `None` for an unknown id.
    pub fn title_of(&self, thread: Ulid) -> Option<String> {
        let dir = self.dir_of(thread)?;
        let events = ThreadLog::open(dir, thread).ok()?.read_all().ok()?;
        aigentic_runtime::title::title_of(&events)
    }

    /// Every id this daemon knows: the index's, plus every log file on
    /// disk — the flat directory and each legacy one (issue #9). The
    /// names are a `read_dir` each; a file already indexed is not read
    /// again, so the only reads are for logs this daemon has not seen.
    fn known_ids(&self) -> Vec<Ulid> {
        let mut ids: Vec<Ulid> = self
            .index
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .copied()
            .collect();
        let mut seen: std::collections::HashSet<Ulid> = ids.iter().copied().collect();
        for path in log_files(&self.threads_base).into_iter().chain(
            legacy_dirs(&self.threads_base)
                .into_iter()
                .flat_map(|sub| log_files(&sub)),
        ) {
            let Some(id) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.parse::<Ulid>().ok())
            else {
                continue;
            };
            if seen.insert(id) {
                ids.push(id);
            }
        }
        ids
    }

    /// The index entry for `thread`, scanning on a miss (issue #9): the
    /// flat log first, then each legacy subdirectory. A miss is a cache
    /// miss, never "no thread" — a child another daemon wrote, or a log
    /// written into a legacy directory after this table was built, turns
    /// up here. A miss is not cached as nothing.
    fn lookup(&self, thread: Ulid) -> Option<Indexed> {
        if let Some(found) = self
            .index
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&thread)
        {
            return Some(found.clone());
        }
        let mut memo = HashMap::new();
        let flat = self.threads_base.join(format!("{thread}.jsonl"));
        let found = scan_log(&flat, &self.server, &mut memo)
            .map(|(_, entry)| entry)
            .or_else(|| {
                legacy_dirs(&self.threads_base).into_iter().find_map(|sub| {
                    scan_log(
                        &sub.join(format!("{thread}.jsonl")),
                        &self.server,
                        &mut memo,
                    )
                    .map(|(_, entry)| entry)
                })
            })?;
        self.index
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(thread, found.clone());
        Some(found)
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

    /// Which project a thread belongs to (issue #9). The entry while it
    /// is open; otherwise what its log says: its last switch if this
    /// daemon knows that project — the one `open` would build in — else
    /// its home. `None` for a log with no project: a pre-phase-4 one, or
    /// a `_none` thread whose name another project holds at a different
    /// root.
    pub fn project_of(&self, thread: Ulid) -> Option<String> {
        if let Some(e) = self
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&thread)
        {
            return Some(e.project.clone());
        }
        let indexed = self.lookup(thread)?;
        indexed
            .switched
            .filter(|name| self.server.project(name).is_some())
            .or(indexed.home)
    }

    pub fn projects(&self) -> Vec<(String, PathBuf, u64)> {
        // A row's count is the threads whose project is that row: a
        // switched thread counts under its current project, and one
        // whose project names no configured project counts nowhere
        // (issue #9). `known_ids` adds what is on disk, so a thread
        // another daemon made since this one started counts too (#9
        // review); `project_of` scans it on its first miss.
        let ids = self.known_ids();
        let projects: Vec<Option<String>> = ids.iter().map(|id| self.project_of(*id)).collect();
        self.server
            .projects
            .iter()
            .map(|p| {
                let count = projects
                    .iter()
                    .filter(|name| name.as_deref() == Some(p.name.as_str()))
                    .count() as u64;
                (p.name.clone(), p.root.clone(), count)
            })
            .collect()
    }

    /// Every thread of a project, newest first, with the state of the
    /// ones that are open. From the index (issue #9): the ids whose
    /// project is `project`, summarised from wherever their log lives.
    pub fn list(&self, project: &str) -> Result<Vec<ThreadInfo>, ThreadError> {
        // The project has to be one this daemon knows, as before.
        self.root_of(project)?;
        // The index plus what is on disk: a thread another daemon made
        // since this one started is listed too (#9 review).
        let mut ids = self.known_ids();
        ids.retain(|id| self.project_of(*id).as_deref() == Some(project));
        ids.sort_unstable_by(|a, b| b.cmp(a));
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        Ok(ids
            .into_iter()
            .map(|id| {
                let dir = self.dir_of(id).unwrap_or_else(|| self.threads_base.clone());
                let mut info = summarise(&dir, id, project);
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
    /// It is written flat, and entered in the index. `front` makes it
    /// the person's front thread (issue #84); only `Front`'s and
    /// `NewFront`'s handlers set it.
    pub async fn create(
        &self,
        project: &str,
        by: Author,
        front: bool,
    ) -> Result<ThreadInfo, ThreadError> {
        let root = self.root_of(project)?;
        std::fs::create_dir_all(&root.threads_dir).map_err(BuildError::from)?;
        let id = Ulid::generate();
        // The creator, for the index and for the log (issue #81): only a
        // person counts, so a lead a build starts through `create` gets
        // none.
        let creator = match &by {
            Author::User(user) => Some(user.clone()),
            Author::Agent(_) | Author::System => None,
        };
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
                front,
            })
            .expect("serialisable"),
            parent_event: None,
        })?;
        drop(log);
        self.index.lock().unwrap_or_else(|e| e.into_inner()).insert(
            id,
            Indexed {
                dir: root.threads_dir.clone(),
                home: Some(project.to_owned()),
                switched: None,
                root: Some(root.root.clone()),
                creator,
                run: RunThread::No,
                torn_run: false,
                front,
            },
        );
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
        // Where the log is and which project the thread is now in: both
        // from the log, not from a directory (issue #9). A thread whose
        // log names no project, or whose name another project holds at a
        // different root, cannot be built and is refused.
        let project = self
            .project_of(thread)
            .ok_or(ThreadError::NoThread(thread))?;
        let dir = self.dir_of(thread).ok_or(ThreadError::NoThread(thread))?;
        // The creator, once, from the log: the listing is theirs for as
        // long as the thread lives (#81).
        let creator = self.creator_of(thread);
        let root = Root {
            threads_dir: dir,
            ..self.root_of(&project)?
        };
        let projects = self.projects_in_reach(creator.as_ref(), Some(&project));
        let built = build_thread(
            &self.config,
            &self.config_dir,
            &*self.providers,
            &root,
            &self.workspaces,
            thread,
            self.profile_override.as_deref(),
            projects,
        )
        .await?;
        let (actor, mailbox) = ThreadActor::new(
            built.runtime,
            built.torn,
            self.reports.clone(),
            self.labels(),
        )?;
        let actor = actor.with_keep_awake(self.guard());
        tokio::spawn(actor.run());
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        // Two sessions may have raced to build it; the first in wins and
        // the second's actor is dropped with its mailbox.
        let e = entries.entry(thread).or_insert_with(|| Entry {
            mailbox,
            project: project.clone(),
            creator,
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
    ///
    /// `threads_dir` is where the run writes: its lead's log and its
    /// children live there (issue #9), which is `<base>` for a lead
    /// written since the move and `<base>/<project>` for one the
    /// migration held back.
    pub fn run_world(&self, project: &str, threads_dir: PathBuf) -> Result<RunWorld, ThreadError> {
        let mut root = self.root_of(project)?;
        root.threads_dir = threads_dir;
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
            guard: self.guard(),
            root,
            project: project.to_owned(),
            workspaces: self.workspaces.clone(),
            bundled,
        })
    }

    /// The world of the run whose lead is `lead`: its children are
    /// written beside its log, wherever that is (issue #9).
    pub fn run_world_for(&self, project: &str, lead: Ulid) -> Result<RunWorld, ThreadError> {
        let dir = self.dir_of(lead).ok_or(ThreadError::NoThread(lead))?;
        self.run_world(project, dir)
    }

    /// The lead of the unfinished run for `(project, issue)`: a thread
    /// whose log holds `run_started { issue }` and no `run_finished`.
    /// Read with repair, because a `kill -9` leaves a torn tail.
    ///
    /// A lead log that cannot be read is **not** "no unfinished run"
    /// (issue #58 fix 2): a corrupt log read as nothing would start a
    /// second lead over a run that is still there, so this reports it —
    /// a note naming the file and the error — and refuses, which stops
    /// the project's next `Build` until a person has looked.
    pub fn unfinished_run(&self, project: &str, issue: u64) -> Result<Option<Ulid>, ThreadError> {
        self.root_of(project)?;
        // The index's leads for the project, newest first: a run that was
        // restarted has one lead per issue, and the newest is the one the
        // last `Build` made (issue #9).
        //
        // The ids are the index's plus every log file on disk: a lead
        // another daemon created a moment ago is not in this daemon's
        // index yet, and a `Build` must find it rather than write a
        // second lead (t15c). A file already indexed costs no read.
        let mut ids = self.known_ids();
        ids.sort_unstable_by(|a, b| b.cmp(a));
        for id in ids {
            if self.project_of(id).as_deref() != Some(project) {
                continue;
            }
            let Some(indexed) = self.lookup(id) else {
                continue;
            };
            // A torn log that is not a lead no longer refuses a `Build`
            // for its project (issue #9): the index knows whether the log
            // claims a `run_started` at all.
            if indexed.run == RunThread::No && !indexed.torn_run {
                continue;
            }
            let dir = indexed.dir.clone();
            let path = dir.join(format!("{id}.jsonl"));
            let refuse = |error: String| -> Result<Option<Ulid>, ThreadError> {
                self.runs.broadcast(
                    id,
                    Notice::Note {
                        thread: id,
                        text: format!("cannot read {}: {error}", path.display()),
                    },
                );
                Err(ThreadError::Refused(format!(
                    "cannot read {}: {error} — fix or remove it before starting a run in {project}",
                    path.display()
                )))
            };
            if indexed.torn_run {
                // A line promised a `run_started` and did not parse: the
                // log says it is a lead, and its issue is unreadable.
                // Reading it with `Refuse` gives the error's own words.
                let why = match ThreadLog::open_with(&dir, id, Repair::Refuse) {
                    Err(e) => e.to_string(),
                    Ok(_) => "a line that promised a `run_started` cannot be read".to_owned(),
                };
                return refuse(why);
            }
            let (log, _cut) = match ThreadLog::open_with(&dir, id, Repair::TruncateTornTail) {
                Ok(opened) => opened,
                Err(e) => return refuse(e.to_string()),
            };
            if crate::runs::unfinished(&log, issue).is_some() {
                return Ok(Some(id));
            }
        }
        Ok(None)
    }

    /// Which kind of run-owned thread `thread` is, from its own log,
    /// wherever that log lives.
    pub fn run_thread(&self, thread: Ulid) -> RunThread {
        let Some(dir) = self.dir_of(thread) else {
            return RunThread::No;
        };
        // Repair: a torn tail is not a reason to start an actor over a
        // run-owned thread.
        let Ok((log, _cut)) = ThreadLog::open_with(&dir, thread, Repair::TruncateTornTail) else {
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
        let dir = self.dir_of(thread).ok_or(ThreadError::NoThread(thread))?;
        let (log, _cut) = ThreadLog::open_with(&dir, thread, Repair::TruncateTornTail)?;
        Ok(log
            .events()
            .iter()
            .filter(|event| event.seq >= from_seq)
            .cloned()
            .collect())
    }

    /// A report for a run-owned thread (issue #58, rule 5c). A run's
    /// thread has no actor, so `Report`, which only reads, is served from
    /// its log here: `build_thread` opens the log and assembles the
    /// runtime, nothing is appended and no turn runs. The log is the
    /// source of truth and the renderer projects it.
    pub async fn report_of(&self, thread: Ulid, kind: ReportKind) -> Result<String, ThreadError> {
        let project = self
            .project_of(thread)
            .ok_or(ThreadError::NoThread(thread))?;
        let root = Root {
            threads_dir: self.dir_of(thread).ok_or(ThreadError::NoThread(thread))?,
            ..self.root_of(&project)?
        };
        let events = self.events_from(thread, 0)?;
        // The same lock a build takes: assembling a runtime touches the
        // log and the project, and never happens beside a `Build`.
        let _serial = self.build_lock.lock().await;
        let built = build_thread(
            &self.config,
            &self.config_dir,
            &*self.providers,
            &root,
            &self.workspaces,
            thread,
            self.profile_override.as_deref(),
            // A report renders events; it builds no prompt, so the
            // listing would go nowhere (#81).
            None,
        )
        .await?;
        Ok(self.reports.render(&built.runtime, &events, kind))
    }

    /// The start-up scan (issue #58, rule 8): every unfinished run in
    /// every project is claimed, so a daemon killed mid-run picks the runs
    /// up again. A served daemon does this; an embedded one
    /// (`resume_runs: false`) does not - opening the REPL must not
    /// silently resume someone's build and push. It is the one scan: a
    /// session no longer repeats it per connection (#58 fix 5).
    ///
    /// A lead waiting at a checkpoint gets a task that waits; one left
    /// mid-child continues it. A finished lead is ignored, and so is a
    /// lead holding only `thread_started`.
    pub fn resume_unfinished_runs(&self) {
        if !self.server.resume_runs {
            return;
        }
        // The index's leads, grouped by their project (issue #9): a lead
        // never switches, so its project is its home. A group the daemon
        // does not know is skipped, as the old `root_of` guard skipped it.
        // The ids come out under the lock, then `project_of` takes it
        // again: iterating the guard while asking it deadlocks.
        let ids: Vec<Ulid> = self
            .index
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|(_, indexed)| matches!(indexed.run, RunThread::Lead { .. }))
            .map(|(id, _)| *id)
            .collect();
        let mut groups: BTreeMap<String, Vec<Ulid>> = BTreeMap::new();
        for id in ids {
            let Some(project) = self.project_of(id) else {
                continue;
            };
            if self.server.project(&project).is_none() {
                continue;
            }
            groups.entry(project).or_default().push(id);
        }
        for (project, mut ids) in groups {
            // Oldest first, so a restarted run's newest lead is claimed
            // last and stays the claimed one.
            ids.sort_unstable();
            for id in ids {
                // The world is the lead's own: a lead the migration held
                // back still writes in its legacy directory (issue #9).
                let Some(dir) = self.dir_of(id) else {
                    continue;
                };
                let Ok(world) = self.run_world(&project, dir) else {
                    continue;
                };
                // Repair, like every other read here: a `kill -9` mid-write
                // leaves a half line that was never an event.
                let Ok((log, _cut)) =
                    ThreadLog::open_with(&world.root.threads_dir, id, Repair::TruncateTornTail)
                else {
                    continue;
                };
                let Some(started) = crate::runs::run_started_of(&log).ok() else {
                    continue;
                };
                if crate::runs::unfinished(&log, started.issue).is_none() {
                    continue;
                }
                // A lead another aigentic process drives is left to it:
                // this daemon must not write the same log.
                let _ = self.claim_or_attach(id, &world);
            }
        }
    }

    /// Start a run for `(project, issue)`, or resume the one already
    /// there, and answer with its lead. `resumed` says which happened.
    ///
    /// The whole check-and-create runs under the table's lock and the
    /// issue's OS lock, so two concurrent `Build`s for one issue, in one
    /// process or in two, cannot both find no run and both create one.
    /// Exactly one task ever drives a lead: [`Runs::claim`] decides it.
    pub async fn build_run(
        &self,
        project: &str,
        issue: u64,
        workflow: Option<String>,
        by: Author,
    ) -> Result<(Ulid, bool), ThreadError> {
        let _serial = self.build_lock.lock().await;
        let base = self.threads_base.clone();
        // Held until the lead exists and is claimed: another process's
        // `Build` for this issue then finds the lead instead of making
        // a second one. The lock names the project (issue #9), so two
        // projects' issue 7 do not wait on each other.
        let _issue = IssueLock::take(&base, project, issue)
            .await
            .map_err(ThreadError::Refused)?;
        if let Some(lead) = self.unfinished_run(project, issue)? {
            // The run continues where its lead's log is (issue #9): in
            // the flat base, or in a legacy subdirectory the migration
            // held back.
            let world = self.run_world_for(project, lead)?;
            self.claim_or_attach(lead, &world)?;
            return Ok((lead, true));
        }
        // A new run is written flat.
        let world = self.run_world(project, base)?;
        // The workflow defaults to `build`, the name the acceptance uses
        // (issue #58, rule 4).
        let name = workflow.unwrap_or_else(|| DEFAULT_WORKFLOW.to_owned());
        let loaded = WorkflowFile::load(&name, &world.workflow_roots())?;
        let info = self.create(project, by.clone(), false).await?;
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
        self.note_lead(lead, issue, &world.root.threads_dir, project);
        self.claim_or_attach(lead, &world)?;
        Ok((lead, false))
    }

    /// Tell the index that `thread` is a lead of `issue` whose log is in
    /// `dir` (issue #9). A fresh lead's `run_started` is appended after
    /// `create` indexed it, so its own daemon has to be told; a scanned
    /// lead is already a lead and this only refreshes its directory.
    fn note_lead(&self, thread: Ulid, issue: u64, dir: &Path, project: &str) {
        let mut index = self.index.lock().unwrap_or_else(|e| e.into_inner());
        let entry = index.entry(thread).or_insert_with(|| Indexed {
            dir: dir.to_path_buf(),
            home: Some(project.to_owned()),
            switched: None,
            root: None,
            creator: None,
            run: RunThread::No,
            torn_run: false,
            // A lead is never front (issue #84).
            front: false,
        });
        entry.dir = dir.to_path_buf();
        entry.run = RunThread::Lead { issue };
        if entry.home.is_none() {
            entry.home = Some(project.to_owned());
        }
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
        let root = Root {
            threads_dir: self.dir_of(lead).ok_or(ThreadError::NoThread(lead))?,
            ..self.root_of(&project)?
        };
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
        let world = self.run_world_for(&project, lead)?;
        let tx = self.claim_or_attach(lead, &world)?;
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
    /// lead gets no second runner, and a lead another process drives is
    /// refused (issue #58 fix 2) rather than written a second time.
    fn claim_or_attach(
        &self,
        lead: Ulid,
        world: &RunWorld,
    ) -> Result<mpsc::UnboundedSender<Answer>, ThreadError> {
        match self.runs.claim(lead, world) {
            Claim::Attached(tx) => Ok(tx),
            Claim::Claimed(lock, rx, tx) => {
                let runs = self.runs.clone();
                let world = world.clone();
                // The lock travels into the task, which holds it until it
                // ends: no path leaves a claimed lead unlocked or a
                // released lead locked.
                let task = tokio::spawn(async move { drive(runs, world, lead, rx, lock).await });
                self.runs.remember(lead, task);
                Ok(tx)
            }
            Claim::Elsewhere { reason } => Err(ThreadError::Refused(reason)),
        }
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

/// Every log file in `dir`, sorted: `<dir>/<ulid>.jsonl`. An unreadable
/// directory is empty, not an error.
fn log_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("jsonl"))
        .filter(|path| {
            path.file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.parse::<Ulid>().is_ok())
        })
        .collect();
    paths.sort();
    paths
}

/// The legacy `<base>/<project>` directories: every subdirectory of
/// `base` whose name does not start with `.` (issue #9). A dot-named
/// directory is left alone, logs and all.
fn legacy_dirs(base: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(base) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .filter(|path| {
            !path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with('.'))
        })
        .collect();
    dirs.sort();
    dirs
}

/// Is `a` the same root as `b`? Both sides are canonicalised when they
/// exist, so `/tmp` and `/private/tmp` match on macOS (issue #9).
fn same_root(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// The project a `thread_started` gives a thread (issue #9): its own
/// `project`.
///
/// A pre-phase-6 log wrote [`LEGACY_NONE_PROJECT`] for a root with no
/// project file, because the per-project directory was the index. That
/// stand-in name is gone, so such a thread belongs to
/// `project_name_at(root)` — and to no project at all when that name is
/// another project's, at a different canonical root.
fn home_of(
    server: &ServerConfig,
    project: Option<&str>,
    root: Option<&Path>,
    memo: &mut HashMap<PathBuf, Option<String>>,
) -> Option<String> {
    let name = project?;
    if name != LEGACY_NONE_PROJECT {
        return Some(name.to_owned());
    }
    let root = root?;
    // Canonicalising is memoised per distinct root: a scan of 456 logs
    // must not canonicalise one path 456 times.
    if let Some(known) = memo.get(root) {
        return known.clone();
    }
    // The root's project file name if one exists now, else the root's
    // own basename: the rule the embedded daemon names a bare root by,
    // minus its hex suffix. A configured project at a different root
    // claims that name, so the thread has no project it can be built in.
    let candidate = crate::workspaces::project_name(root);
    let home = match server.project(&candidate) {
        Some(p) if !same_root(&p.root, root) => None,
        _ => Some(candidate),
    };
    memo.insert(root.to_path_buf(), home.clone());
    home
}

/// Read one log into its index entry (issue #9): the `thread_started`,
/// the last `project_switched` and the `run_started` are the only lines
/// that carry it. Nothing is validated and no line ends the read, since
/// a switch can come anywhere.
fn scan_log(
    path: &Path,
    server: &ServerConfig,
    memo: &mut HashMap<PathBuf, Option<String>>,
) -> Option<(Ulid, Indexed)> {
    let id: Ulid = path.file_stem()?.to_str()?.parse().ok()?;
    let dir = path.parent()?.to_path_buf();
    let file = std::fs::File::open(path).ok()?;
    let mut entry = Indexed {
        dir,
        home: None,
        switched: None,
        root: None,
        creator: None,
        run: RunThread::No,
        torn_run: false,
        front: false,
    };
    let mut parent: Option<Ulid> = None;
    let mut step: Option<String> = None;
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else {
            break;
        };
        let wants_started = line.contains("\"kind\":\"thread_started\"");
        let wants_switch = line.contains("\"kind\":\"project_switched\"");
        let wants_run = line.contains("\"kind\":\"run_started\"");
        if !(wants_started || wants_switch || wants_run) {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Event>(&line) else {
            // A line that promised a `run_started` and did not parse:
            // the log says it is a lead and its issue cannot be read.
            if wants_run {
                entry.torn_run = true;
            }
            continue;
        };
        match event.kind {
            EventKind::ThreadStarted if wants_started => {
                let Ok(p) = serde_json::from_value::<ThreadStartedPayload>(event.payload.clone())
                else {
                    continue;
                };
                entry.home = home_of(server, p.project.as_deref(), Some(&p.root), memo);
                entry.root = Some(p.root.clone());
                // Only a person counts (issue #81): an agent's thread (a
                // build's or a step's child) gets no projects block.
                entry.creator = match p.created_by {
                    Author::User(user) => Some(user),
                    Author::Agent(_) | Author::System => None,
                };
                // The person's front thread (issue #84); a line written
                // before the field reads as `false`.
                entry.front = p.front;
                parent = p.parent_thread;
                step = p.step.clone();
            }
            EventKind::ProjectSwitched if wants_switch => {
                if let Ok(p) = serde_json::from_value::<
                    aigentic_runtime::aigentic_log::ProjectSwitchedPayload,
                >(event.payload.clone())
                {
                    entry.switched = p.to;
                }
            }
            EventKind::RunStarted if wants_run => {
                if let Ok(p) = serde_json::from_value::<RunStartedPayload>(event.payload.clone()) {
                    entry.run = RunThread::Lead { issue: p.issue };
                }
            }
            _ => {}
        }
    }
    if let Some(lead) = parent {
        entry.run = RunThread::Child { lead, step };
    }
    Some((id, entry))
}

/// The index: every thread the threads directory holds, read from the
/// logs once, at start-up (issue #9). The flat directory is read first,
/// then each legacy subdirectory; if an id is in both, the flat one wins.
fn scan_index(base: &Path, server: &ServerConfig) -> HashMap<Ulid, Indexed> {
    let mut memo: HashMap<PathBuf, Option<String>> = HashMap::new();
    let mut index: HashMap<Ulid, Indexed> = HashMap::new();
    for path in log_files(base) {
        if let Some((id, entry)) = scan_log(&path, server, &mut memo) {
            index.insert(id, entry);
        }
    }
    for sub in legacy_dirs(base) {
        for path in log_files(&sub) {
            if let Some((id, entry)) = scan_log(&path, server, &mut memo) {
                index.entry(id).or_insert(entry);
            }
        }
    }
    index
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
        project: Some(project.to_owned()),
        date,
        events: events.len() as u64,
        first_line,
        state: ThreadState::Idle,
        title: aigentic_runtime::title::title_of(&events),
    }
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

/// The project name a root has, for the embedded daemon and for the
/// index: the project file's name, else the root's own file name — a
/// bare root *is* the project (issue #9).
///
/// A bare root whose name a workspace project already has at another
/// root takes `<name>-<8 hex>` instead, so the two never collide: the
/// hex is FNV-1a 32 over the canonical root path's bytes, written out
/// here rather than taken from `DefaultHasher`, which is not stable
/// across Rust versions.
pub fn project_name_at(root: &Path, workspaces: &[Workspace]) -> String {
    let name = crate::workspaces::project_name(root);
    if has_project_file(root) || !name_clashes(&name, root, workspaces) {
        return name;
    }
    format!(
        "{name}-{:08x}",
        fnv1a_32(canonical(root).to_string_lossy().as_bytes())
    )
}

/// Is `name` a workspace project's, at a root that is not `root`?
///
/// Public so a local reader — `aigentic stats`, `aigentic threads` — can
/// apply the same rule the daemon's `home_of` does (#83).
pub fn name_clashes(name: &str, root: &Path, workspaces: &[Workspace]) -> bool {
    workspaces
        .iter()
        .flat_map(|w| w.projects.iter())
        .any(|p| crate::workspaces::project_name(p) == name && !same_root(p, root))
}

/// `root` as it really is, or as given when it does not exist.
fn canonical(root: &Path) -> PathBuf {
    std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
}

/// FNV-1a, 32 bits: the hash the `-<8 hex>` suffix uses, written inline
/// so it stays the same across Rust versions (issue #9).
fn fnv1a_32(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in bytes {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}
