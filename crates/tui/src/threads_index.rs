//! The flat threads directory (issue #9) and what a log says about its
//! project (issue #83). Every local reader — `aigentic stats`, the local
//! `aigentic threads` — walks the same catalogue and attributes a thread
//! by the embedded daemon's own rules, so the two never name one thread
//! two ways.
//!
//! The layout: every log is `threads/<id>.jsonl`. A directory made
//! before #9 left a `threads/<project>/<id>.jsonl` behind; a log there is
//! still read, still attributed from its own lines, and the directory's
//! name is only a fallback for a log that says nothing.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use aigentic_runtime::Project;
use aigentic_runtime::aigentic_core::{Event, EventKind};
use aigentic_runtime::aigentic_log::{
    LogError, ProjectSwitchedPayload, ThreadLog, ThreadStartedPayload,
};
use aigentic_server::threads::{name_clashes, project_name_at};
use aigentic_server::workspaces::{self, Workspace};
use ulid::Ulid;

/// The stand-in name a log written before phase 6 gave a root with no
/// project file. Named exactly as the daemon names it.
pub const LEGACY_NONE_PROJECT: &str = "_none";

/// The label a thread with no project gets in every report and every
/// error message. Never an empty string.
pub const NO_PROJECT: &str = "(no project)";

/// The label for an attributed project: the name, or [`NO_PROJECT`].
pub fn label(project: Option<&str>) -> &str {
    project.unwrap_or(NO_PROJECT)
}

/// One log the catalogue found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub id: Ulid,
    /// The directory the log lives in: `base`, or a legacy subdirectory.
    pub dir: PathBuf,
    /// The legacy subdirectory's name, `None` for a flat log.
    pub legacy: Option<String>,
}

impl Found {
    /// `<dir>/<id>.jsonl`.
    pub fn path(&self) -> PathBuf {
        self.dir.join(format!("{}.jsonl", self.id))
    }

    /// The log's events, in order.
    pub fn read(&self) -> Result<Vec<Event>, LogError> {
        ThreadLog::open(&self.dir, self.id).and_then(|log| log.read_all())
    }
}

/// Every `<ulid>.jsonl` in `base`, then in each legacy subdirectory,
/// newest id first, each id once: the flat copy wins. A missing base
/// gives an empty list.
pub fn catalogue(base: &Path) -> Vec<Found> {
    let flat = std::iter::once((base.to_path_buf(), None));
    let legacy = legacy_dirs(base).into_iter().map(|d| (d.0, Some(d.1)));
    let mut found: BTreeMap<Ulid, Found> = BTreeMap::new();
    for (dir, legacy) in flat.chain(legacy) {
        for id in log_ids(&dir) {
            found.entry(id).or_insert_with(|| Found {
                id,
                dir: dir.clone(),
                legacy: legacy.clone(),
            });
        }
    }
    let mut out: Vec<Found> = found.into_values().collect();
    out.sort_unstable_by_key(|f| std::cmp::Reverse(f.id));
    out
}

/// `base`'s subdirectories, by name, skipping the dot-directories.
fn legacy_dirs(base: &Path) -> Vec<(PathBuf, String)> {
    let Ok(entries) = std::fs::read_dir(base) else {
        return Vec::new();
    };
    let mut dirs: Vec<(PathBuf, String)> = entries
        .filter_map(Result::ok)
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_owned();
            (!name.starts_with('.')).then(|| (e.path(), name))
        })
        .collect();
    dirs.sort();
    dirs
}

/// The ids of the `<ulid>.jsonl` files in `dir`, newest first.
fn log_ids(dir: &Path) -> Vec<Ulid> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut ids: Vec<Ulid> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
        .filter_map(|p| p.file_stem()?.to_str()?.parse().ok())
        .collect();
    ids.sort_unstable_by_key(|id| std::cmp::Reverse(*id));
    ids
}

/// The project a `_none` log's `root` really belongs to, by the daemon's
/// `home_of` rule: the root's project file name, else its basename —
/// and no project at all when a workspace project holds that name at a
/// different root.
pub fn legacy_home(root: &Path, workspaces: &[Workspace]) -> Option<String> {
    let name = workspaces::project_name(root);
    if name_clashes(&name, root, workspaces) {
        None
    } else {
        Some(name)
    }
}

/// Which project a thread belongs to, from its own lines (#83):
///
/// 1. its last `project_switched` whose `to` is `Some`;
/// 2. else its `thread_started`'s `project`, with [`LEGACY_NONE_PROJECT`]
///    mapped through [`legacy_home`];
/// 3. else the legacy directory it was found in, unless that directory is
///    [`LEGACY_NONE_PROJECT`];
/// 4. else none.
///
/// Two differences from the daemon's `project_of`:
///
/// - a switch to a project the daemon does not know still counts here:
///   a local command has no daemon to ask, and hiding such a thread would
///   hide its log;
/// - `server.toml`'s projects are not consulted for the [`LEGACY_NONE_PROJECT`]
///   clash — only the workspaces are, which is all a local command loads.
pub fn project_of(
    events: &[Event],
    legacy: Option<&str>,
    workspaces: &[Workspace],
) -> Option<String> {
    let mut switched: Option<String> = None;
    let mut started: Option<Option<String>> = None;
    for event in events {
        match event.kind {
            EventKind::ProjectSwitched => {
                if let Ok(p) =
                    serde_json::from_value::<ProjectSwitchedPayload>(event.payload.clone())
                    && let Some(to) = p.to
                {
                    switched = Some(to);
                }
            }
            EventKind::ThreadStarted if started.is_none() => {
                if let Ok(p) = serde_json::from_value::<ThreadStartedPayload>(event.payload.clone())
                {
                    started = Some(match p.project {
                        Some(name) if name == LEGACY_NONE_PROJECT => {
                            legacy_home(&p.root, workspaces)
                        }
                        other => other,
                    });
                }
            }
            _ => {}
        }
    }
    if let Some(name) = switched {
        return Some(name);
    }
    if let Some(home) = started {
        return home;
    }
    legacy
        .filter(|name| *name != LEGACY_NONE_PROJECT)
        .map(str::to_owned)
}

/// The name a folder's threads are filed under (#83): the project file's
/// name when there is a project here, else the embedded daemon's name for
/// the bare folder — its basename, or `<name>-<8 hex>` when a workspace
/// project already holds that name at another root.
pub fn folder_project(cwd: &Path, opened: Option<&Project>, workspaces: &[Workspace]) -> String {
    match opened {
        Some(project) => project.name.clone(),
        None => project_name_at(cwd, workspaces),
    }
}
