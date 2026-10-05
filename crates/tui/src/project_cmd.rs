//! `aigentic project init | setup` and the local `aigentic threads`. The
//! `project show` report lives in the daemon crate's `reports` since
//! phase 5 step 9. See `docs/PLAN-phase4.md` sections 5, 7 and 8.

use std::path::Path;

use aigentic_runtime::aigentic_core::{ContentBlock, Event, EventKind};
use aigentic_runtime::aigentic_log::UserMessagePayload;
use aigentic_runtime::project::{DOT_DIR, FILE_NAME, INSTRUCTIONS_FILE, KNOWLEDGE_DIR, MEMORY_DIR};
use aigentic_server::workspaces::Workspace;
use anyhow::{Context, bail};
use time::format_description::well_known::Rfc3339;
use ulid::Ulid;

use crate::threads_index::{Found, catalogue, label, project_of};

#[derive(Debug, clap::Subcommand)]
pub enum ProjectCommand {
    /// Write a minimal aigentic.toml here and create .aigentic/{knowledge,memory}.
    Init,
    /// Render docs/agents/*.md and the `## Agent skills` block from [pocock].
    Setup,
    /// The layers, the knowledge mode and every tool's fate.
    Show,
}

/// `aigentic project init`: refuses to nest inside an existing project.
pub fn init(cwd: &Path) -> anyhow::Result<i32> {
    if let Some(root) = aigentic_runtime::project::find_root(cwd) {
        bail!(
            "already a project: {} (delete it first to start over)",
            root.join(FILE_NAME).display()
        );
    }
    let name = cwd
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "project".into());
    let file = cwd.join(FILE_NAME);
    std::fs::write(&file, init_template(&name))
        .with_context(|| format!("writing {}", file.display()))?;
    for dir in [KNOWLEDGE_DIR, MEMORY_DIR] {
        let path = cwd.join(DOT_DIR).join(dir);
        std::fs::create_dir_all(&path).with_context(|| format!("creating {}", path.display()))?;
        // Git keeps no empty directory; `.gitkeep` is not a `*.md`, so it
        // is neither knowledge nor memory.
        std::fs::write(path.join(".gitkeep"), "")?;
    }
    println!("wrote {}", file.display());
    println!(
        "created {}/{{{KNOWLEDGE_DIR},{MEMORY_DIR}}}",
        cwd.join(DOT_DIR).display()
    );
    println!("instructions: {DOT_DIR}/{INSTRUCTIONS_FILE} if present, else AGENTS.md");
    Ok(0)
}

pub fn init_template(name: &str) -> String {
    format!(
        r#"[project]
name = "{name}"
# description = ""

# [model]
# profile = "tensorx"            # a profile in config.toml; --profile overrides

# [tools]
# allow = ["read_file", "bash"]  # empty or absent: every registered tool

# [knowledge]
# threshold_fraction = 0.4       # of the model's window; over it, index + search_knowledge
# max_hits = 5

# [memory]
# enabled = true
# every_n_turns = 1

# [skills]
# enabled = []
"#
    )
}

/// One thread in a listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadSummary {
    pub id: Ulid,
    /// Date of the first event, else the id's timestamp.
    pub date: String,
    /// `None` when the log could not be read.
    pub events: Option<u64>,
    /// First line of the first user message, or empty.
    pub first_line: String,
}

/// Every thread whose attributed project is `project`, newest first. The
/// walk is `stats`' own (`crate::threads_index`): the flat directory
/// first, then any legacy project directory (#83). A thread with no
/// project of its own is not listed under any name.
pub fn list_threads(base: &Path, project: &str, workspaces: &[Workspace]) -> Vec<ThreadSummary> {
    let mut out: Vec<ThreadSummary> = Vec::new();
    for found in catalogue(base) {
        match found.read() {
            Ok(events) => {
                let attributed = project_of(&events, found.legacy.as_deref(), workspaces);
                if label(attributed.as_deref()) == project {
                    out.push(summarise(&found, Some(&events)));
                }
            }
            // A log nothing can read says nothing about its project, so
            // its legacy directory is all it has: it is listed under
            // that name, with `?` for its line count, rather than
            // hidden.
            Err(_) if found.legacy.as_deref() == Some(project) => {
                out.push(summarise(&found, None));
            }
            Err(_) => {}
        }
    }
    out
}

fn summarise(found: &Found, events: Option<&[Event]>) -> ThreadSummary {
    let id = found.id;
    let fallback_date = || {
        time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(id.timestamp_ms()) * 1_000_000)
            .ok()
            .and_then(|t| t.format(&Rfc3339).ok())
            .map(|s| s[..10].to_owned())
            .unwrap_or_default()
    };
    let Some(events) = events else {
        return ThreadSummary {
            id,
            date: fallback_date(),
            events: None,
            first_line: String::new(),
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
    ThreadSummary {
        id,
        date,
        events: Some(events.len() as u64),
        first_line,
    }
}

const FIRST_LINE_CHARS: usize = 72;

pub fn first_line_of(text: &str) -> String {
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

/// `id  date  events  first line`, one per thread; a note naming the
/// project and the base when there are none.
pub fn render_threads(threads: &[ThreadSummary], project: &str, base: &Path) -> String {
    if threads.is_empty() {
        return format!("no threads in {project} under {}", base.display());
    }
    let mut out = String::new();
    for t in threads {
        let events = t.events.map_or("?".to_owned(), |n| n.to_string());
        out.push_str(&format!(
            "{}  {}  {:>5}  {}\n",
            t.id, t.date, events, t.first_line
        ));
    }
    out.trim_end().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::threads_index::{LEGACY_NONE_PROJECT, NO_PROJECT};
    use aigentic_runtime::Project;
    use aigentic_runtime::aigentic_core::{AgentId, Author, UserId};
    use aigentic_runtime::aigentic_log::{
        NewEvent, ProjectSwitchedPayload, ThreadLog, ThreadStartedPayload,
    };
    use aigentic_server::threads::project_name_at;
    use aigentic_server::workspaces::Workspace;
    use serde_json::json;

    use crate::stats::{PriceBook, collect};
    use crate::threads_index::folder_project;

    fn user_message(text: &str) -> NewEvent {
        NewEvent {
            kind: EventKind::UserMessage,
            author: Author::User(UserId("steve".into())),
            payload: json!({"blocks": [{"type": "text", "text": text}]}),
            parent_event: None,
        }
    }

    /// `thread_started` as the daemon writes it, built from the payload
    /// type: a hand-written payload missing `created_by` would be
    /// skipped silently, and a test would pass for the wrong reason.
    fn thread_started(project: Option<&str>, root: &Path) -> NewEvent {
        NewEvent {
            kind: EventKind::ThreadStarted,
            author: Author::Agent(AgentId("runtime".into())),
            payload: serde_json::to_value(ThreadStartedPayload {
                project: project.map(str::to_owned),
                root: root.to_path_buf(),
                created_by: Author::User(UserId("steve".into())),
                parent_thread: None,
                step: None,
                front: false,
            })
            .unwrap(),
            parent_event: None,
        }
    }

    /// `project_switched`, built from the payload type.
    fn project_switched(from: &str, to: &str, root: &Path) -> NewEvent {
        NewEvent {
            kind: EventKind::ProjectSwitched,
            author: Author::Agent(AgentId("runtime".into())),
            payload: serde_json::to_value(ProjectSwitchedPayload {
                from: Some(from.to_owned()),
                to: Some(to.to_owned()),
                root: root.to_path_buf(),
                workspace: None,
            })
            .unwrap(),
            parent_event: None,
        }
    }

    /// One log under `dir`: `id` seconds after a fixed instant, so the
    /// listing's order is the fixture's.
    fn write_thread(dir: &Path, id: Ulid, events: &[NewEvent]) {
        std::fs::create_dir_all(dir).unwrap();
        let mut log = ThreadLog::open(dir, id).unwrap();
        for event in events {
            log.append(event.clone()).unwrap();
        }
    }

    fn id_at(n: u64) -> Ulid {
        Ulid::from_parts(1_700_000_000_000 + n * 1_000, n as u128 + 1)
    }

    #[test]
    fn init_writes_the_file_and_folders_and_refuses_to_nest() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("myapp");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        assert_eq!(init(&root).unwrap(), 0);
        let p = Project::open_root(&root).unwrap();
        assert_eq!(p.name, "myapp");
        assert!(root.join(".aigentic/knowledge/.gitkeep").is_file());
        assert!(root.join(".aigentic/memory/.gitkeep").is_file());
        assert!(p.memory.is_empty(), ".gitkeep is not a memory file");
        let err = init(&root.join("sub")).unwrap_err().to_string();
        assert!(err.contains("already a project"), "{err}");
        assert!(init(&root).is_err());
    }

    /// T6 (issue #83): the local `aigentic threads` lists the threads
    /// whose own logs name the folder's project — flat and legacy alike —
    /// newest first, with each one's date, line count and first line.
    #[test]
    fn the_local_listing_follows_the_flat_directory() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let started = id_at(0);
        let switched_in = id_at(1);
        let switched_away = id_at(2);
        let legacy = id_at(3);

        write_thread(
            base,
            started,
            &[
                thread_started(Some("alpha"), base),
                user_message("  \nFix the off-by-one in cost.rs\nmore"),
            ],
        );
        write_thread(
            base,
            switched_in,
            &[
                thread_started(Some("beta"), base),
                project_switched("beta", "alpha", base),
                user_message("started in beta"),
            ],
        );
        write_thread(
            base,
            switched_away,
            &[
                thread_started(Some("alpha"), base),
                project_switched("alpha", "beta", base),
                user_message("left it"),
            ],
        );
        // A legacy `alpha/<id>.jsonl`, which names nothing itself.
        write_thread(&base.join("alpha"), legacy, &[user_message("legacy log")]);
        std::fs::write(base.join("not-a-ulid.jsonl"), "").unwrap();

        let threads = list_threads(base, "alpha", &[]);
        assert_eq!(
            threads.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![legacy, switched_in, started],
            "started here, switched in and the legacy log; never the one switched away"
        );
        assert_eq!(threads[2].events, Some(2));
        assert_eq!(threads[2].first_line, "Fix the off-by-one in cost.rs");
        assert_eq!(threads[2].date.len(), 10);
        let text = render_threads(&threads, "alpha", base);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(
            lines[2].starts_with(&format!("{started}  {}      2  Fix the", threads[2].date)),
            "{text}"
        );
        assert!(
            list_threads(&base.join("missing"), "alpha", &[]).is_empty(),
            "a base that is not there lists nothing"
        );

        // The empty message names the project and the base.
        let empty = render_threads(&[], "alpha", base);
        assert_eq!(
            empty,
            format!("no threads in alpha under {}", base.display())
        );
    }

    /// T6 (issue #83): the folder's name is the daemon's own — the
    /// project file's name, else `project_name_at` — and an old `_none`
    /// log in a folder whose basename a workspace project holds is not
    /// listed there, and is `(no project)` in stats.
    #[test]
    fn the_folder_name_follows_the_daemon() {
        // A project file names the folder.
        let named = tempfile::tempdir().unwrap();
        std::fs::write(
            named.path().join(FILE_NAME),
            "[project]\nname = \"vendela\"\n",
        )
        .unwrap();
        let opened = Project::open_root(named.path()).unwrap();
        assert_eq!(folder_project(named.path(), Some(&opened), &[]), "vendela");

        // A bare folder takes the daemon's name for it, with and without
        // a workspace project holding that name at another root.
        let holder = tempfile::tempdir().unwrap();
        let empty: &[Workspace] = &[];
        assert_eq!(
            folder_project(holder.path(), None, empty),
            project_name_at(holder.path(), empty)
        );
        let other = tempfile::tempdir().unwrap();
        let elsewhere = other.path().join(holder.path().file_name().unwrap());
        std::fs::create_dir_all(&elsewhere).unwrap();
        let ws = Workspace {
            name: "w".to_owned(),
            shared: None,
            projects: vec![elsewhere],
        };
        let name = folder_project(holder.path(), None, std::slice::from_ref(&ws));
        assert_eq!(
            name,
            project_name_at(holder.path(), std::slice::from_ref(&ws))
        );
        assert_ne!(
            name,
            project_name_at(holder.path(), empty),
            "the clash renames it"
        );

        // Its old `_none` log: not under that name, and attributed to no
        // project, exactly as the daemon lists it.
        write_thread(
            &holder.path().join(LEGACY_NONE_PROJECT),
            id_at(4),
            &[
                thread_started(Some(LEGACY_NONE_PROJECT), holder.path()),
                user_message("outside any project"),
            ],
        );
        assert!(list_threads(holder.path(), &name, std::slice::from_ref(&ws)).is_empty());
        let stats = collect(
            holder.path(),
            std::slice::from_ref(&ws),
            None,
            None,
            &PriceBook::default(),
        )
        .unwrap();
        assert_eq!(
            stats
                .projects
                .iter()
                .map(|p| p.project.as_str())
                .collect::<Vec<_>>(),
            vec![NO_PROJECT]
        );
    }
}
