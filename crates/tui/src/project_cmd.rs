//! `aigentic project init | setup`, `aigentic threads` and the threads
//! directory per project. The `project show` report lives in the daemon
//! crate's `reports` since phase 5 step 9. See `docs/PLAN-phase4.md`
//! sections 5, 7 and 8.

use std::path::{Path, PathBuf};

use aigentic_runtime::Project;
use aigentic_runtime::aigentic_core::{ContentBlock, EventKind};
use aigentic_runtime::aigentic_log::{ThreadLog, UserMessagePayload};
use aigentic_runtime::project::{DOT_DIR, FILE_NAME, INSTRUCTIONS_FILE, KNOWLEDGE_DIR, MEMORY_DIR};
use anyhow::{Context, bail};
use time::format_description::well_known::Rfc3339;
use ulid::Ulid;

/// Threads of a run outside any project.
pub const NO_PROJECT_DIR: &str = "_none";

#[derive(Debug, clap::Subcommand)]
pub enum ProjectCommand {
    /// Write a minimal aigentic.toml here and create .aigentic/{knowledge,memory}.
    Init,
    /// Render docs/agents/*.md and the `## Agent skills` block from [pocock].
    Setup,
    /// The layers, the knowledge mode and every tool's fate.
    Show,
}

/// `threads_dir/<project name>/`, or `threads_dir/_none/` outside a project.
pub fn threads_dir_for(base: &Path, project: Option<&Project>) -> PathBuf {
    base.join(project.map_or(NO_PROJECT_DIR, |p| p.name.as_str()))
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

/// Every `<ulid>.jsonl` in `dir`, newest first. A missing directory lists
/// nothing.
pub fn list_threads(dir: &Path) -> anyhow::Result<Vec<ThreadSummary>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    let mut ids: Vec<Ulid> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
        .filter_map(|p| p.file_stem()?.to_str()?.parse().ok())
        .collect();
    ids.sort_unstable_by(|a, b| b.cmp(a));
    Ok(ids.into_iter().map(|id| summarise(dir, id)).collect())
}

fn summarise(dir: &Path, id: Ulid) -> ThreadSummary {
    let fallback_date = || {
        time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(id.timestamp_ms()) * 1_000_000)
            .ok()
            .and_then(|t| t.format(&Rfc3339).ok())
            .map(|s| s[..10].to_owned())
            .unwrap_or_default()
    };
    let events = ThreadLog::open(dir, id).and_then(|log| log.read_all());
    let Ok(events) = events else {
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

/// `id  date  events  first line`, one per thread; a note when empty.
pub fn render_threads(threads: &[ThreadSummary], dir: &Path) -> String {
    if threads.is_empty() {
        return format!("no threads under {}", dir.display());
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
    use aigentic_runtime::aigentic_core::{AgentId, Author, UserId};
    use aigentic_runtime::aigentic_log::NewEvent;
    use serde_json::json;

    fn user_message(text: &str) -> NewEvent {
        NewEvent {
            kind: EventKind::UserMessage,
            author: Author::User(UserId("steve".into())),
            payload: json!({"blocks": [{"type": "text", "text": text}]}),
            parent_event: None,
        }
    }

    #[test]
    fn the_threads_directory_is_per_project_name() {
        let base = Path::new("/t");
        assert_eq!(threads_dir_for(base, None), PathBuf::from("/t/_none"));
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(FILE_NAME),
            "[project]\nname = \"vendela\"\n",
        )
        .unwrap();
        let p = Project::open_root(dir.path()).unwrap();
        assert_eq!(threads_dir_for(base, Some(&p)), PathBuf::from("/t/vendela"));
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

    #[test]
    fn threads_list_newest_first_with_date_count_and_first_line() {
        let dir = tempfile::tempdir().unwrap();
        let older = Ulid::from_parts(1_700_000_000_000, 1);
        let newer = Ulid::from_parts(1_700_000_100_000, 2);
        let mut log = ThreadLog::open(dir.path(), older).unwrap();
        log.append(user_message("  \nFix the off-by-one in cost.rs\nmore"))
            .unwrap();
        log.append(NewEvent {
            kind: EventKind::TurnEnded,
            author: Author::Agent(AgentId("a".into())),
            payload: json!({"reason": "done"}),
            parent_event: None,
        })
        .unwrap();
        let mut log = ThreadLog::open(dir.path(), newer).unwrap();
        log.append(user_message(&"x".repeat(100))).unwrap();
        std::fs::write(dir.path().join("notes.txt"), "ignored").unwrap();
        std::fs::write(dir.path().join("not-a-ulid.jsonl"), "").unwrap();

        let threads = list_threads(dir.path()).unwrap();
        assert_eq!(
            threads.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![newer, older]
        );
        assert_eq!(threads[1].events, Some(2));
        assert_eq!(threads[1].first_line, "Fix the off-by-one in cost.rs");
        assert_eq!(threads[0].events, Some(1));
        assert_eq!(threads[0].first_line, format!("{}…", "x".repeat(72)));
        assert_eq!(threads[0].date.len(), 10);
        let text = render_threads(&threads, dir.path());
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(
            lines[1].starts_with(&format!("{older}  {}      2  Fix the", threads[1].date)),
            "{text}"
        );
        assert!(
            list_threads(&dir.path().join("missing"))
                .unwrap()
                .is_empty()
        );
        assert!(render_threads(&[], dir.path()).starts_with("no threads under"));
    }
}
