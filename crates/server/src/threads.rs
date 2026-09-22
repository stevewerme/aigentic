//! The daemon's threads: one actor per open thread, started on first
//! use, unloaded after an idle period with no open sessions and nothing
//! waited for. The log is the state, so unloading loses nothing.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aigentic_api::{ThreadInfo, ThreadState};
use aigentic_runtime::aigentic_core::{Author, ContentBlock, EventKind};
use aigentic_runtime::aigentic_log::{
    NewEvent, ThreadLog, ThreadStartedPayload, UserMessagePayload,
};
use aigentic_runtime::aigentic_policy::Participants;
use aigentic_runtime::{Project, ProjectFile};
use time::format_description::well_known::Rfc3339;
use tokio::sync::oneshot;
use ulid::Ulid;

use crate::actor::{Mail, Mailbox, Reports, ThreadActor};
use crate::build::{BuildError, ProviderFactory, Root, build_thread};
use crate::config::{Config, ServerConfig};

/// Threads of a root without a project file.
pub const NO_PROJECT_DIR: &str = "_none";

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
}

impl ThreadTable {
    pub fn new(
        config: Arc<Config>,
        config_dir: PathBuf,
        server: Arc<ServerConfig>,
        providers: Arc<dyn ProviderFactory>,
        reports: Arc<dyn Reports>,
        threads_base: PathBuf,
    ) -> Self {
        Self {
            config,
            config_dir,
            server,
            providers,
            reports,
            threads_base,
            entries: Mutex::new(HashMap::new()),
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
        let project = self
            .project_of(thread)
            .ok_or(ThreadError::NoThread(thread))?;
        let root = self.root_of(&project)?;
        let built = build_thread(
            &self.config,
            &self.config_dir,
            &*self.providers,
            &root,
            thread,
            None,
        )
        .await?;
        let (actor, mailbox) = ThreadActor::new(built.runtime, built.torn, self.reports.clone())?;
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
