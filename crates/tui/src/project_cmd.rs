//! `aigentic project init | setup` and the local `aigentic threads`. The
//! `project show` report lives in the daemon crate's `reports` since
//! phase 5 step 9. See `docs/PLAN-phase4.md` sections 5, 7 and 8.

use std::path::{Path, PathBuf};

use aigentic_runtime::ProjectKind;
use aigentic_runtime::aigentic_core::{ContentBlock, Event, EventKind};
use aigentic_runtime::aigentic_log::UserMessagePayload;
use aigentic_runtime::project::{DOT_DIR, FILE_NAME, INSTRUCTIONS_FILE, KNOWLEDGE_DIR, MEMORY_DIR};
use aigentic_server::workspaces::{self, Workspace};
use anyhow::{Context, bail};
use time::format_description::well_known::Rfc3339;
use ulid::Ulid;

use crate::init_cmd::{Ask, Terminal};
use crate::threads_index::{Found, catalogue, label, project_of};

#[derive(Debug, clap::Subcommand)]
pub enum ProjectCommand {
    /// Write a minimal aigentic.toml here and create .aigentic/{knowledge,memory}.
    Init {
        /// The workspace this folder joins, or `none`. Without it a
        /// terminal is asked; anything else joins none.
        #[arg(long, value_name = "NAME|none")]
        workspace: Option<String>,
    },
    /// Render docs/agents/*.md and the `## Agent skills` block from [pocock].
    Setup,
    /// The layers, the knowledge mode and every tool's fate.
    Show,
}

/// `aigentic project init` at the terminal, asking for the workspace
/// when stdin is one and `--workspace` is absent.
pub fn init(cwd: &Path, config_dir: &Path, workspace: Option<&str>) -> anyhow::Result<i32> {
    init_with(cwd, config_dir, workspace, &mut Terminal)
}

/// Which workspace the new project joins.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Join {
    None,
    Existing(String),
    New { name: String, shared: PathBuf },
}

/// `aigentic project init` over any answer source. Refuses to nest inside
/// an existing project. Every question is asked before anything is
/// written; `kind = "notes"` is written when no `.git` is at or above
/// `cwd`. Without `workspace` and without a terminal the project joins
/// no workspace and a line says how to join one.
pub fn init_with(
    cwd: &Path,
    config_dir: &Path,
    workspace: Option<&str>,
    ask: &mut dyn Ask,
) -> anyhow::Result<i32> {
    if let Some(root) = aigentic_runtime::project::find_root(cwd) {
        bail!(
            "already a project: {} (delete it first to start over)",
            root.join(FILE_NAME).display()
        );
    }
    let all = workspaces::load_all(config_dir)?;
    let listed = workspaces::workspace_of(&all, cwd).map(|w| w.name.clone());
    let join = match (&listed, workspace) {
        (Some(current), Some(name))
            if !name.eq_ignore_ascii_case("none") && !same_name(current, name) =>
        {
            bail!("{} is already in workspace {current:?}", cwd.display())
        }
        (Some(_), _) => Join::None,
        (None, Some(name)) if name.eq_ignore_ascii_case("none") => Join::None,
        (None, Some(name)) => match all.iter().find(|w| same_name(&w.name, name)) {
            Some(w) => Join::Existing(w.name.clone()),
            None => bail!("no workspace named {name:?}"),
        },
        (None, None) if !ask.interactive() => Join::None,
        (None, None) => choose(cwd, &all, ask)?,
    };

    let name = cwd
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "project".into());
    let kind = kind_at(cwd);
    // A refused name or a taken one stops here, before any file is written.
    let joined = match join {
        Join::None => None,
        Join::Existing(w) => Some(w),
        Join::New { name, shared } => {
            Some(workspaces::create(config_dir, &name, Some(&shared))?.name)
        }
    };
    let file = cwd.join(FILE_NAME);
    std::fs::write(&file, init_template(&name, kind))
        .with_context(|| format!("writing {}", file.display()))?;
    for dir in [KNOWLEDGE_DIR, MEMORY_DIR] {
        let path = cwd.join(DOT_DIR).join(dir);
        std::fs::create_dir_all(&path).with_context(|| format!("creating {}", path.display()))?;
        // Git keeps no empty directory; `.gitkeep` is not a `*.md`, so it
        // is neither knowledge nor memory.
        std::fs::write(path.join(".gitkeep"), "")?;
    }
    ask.show(&format!("wrote {}", file.display()));
    ask.show(&format!(
        "created {}/{{{KNOWLEDGE_DIR},{MEMORY_DIR}}}",
        cwd.join(DOT_DIR).display()
    ));
    ask.show(&format!(
        "instructions: {DOT_DIR}/{INSTRUCTIONS_FILE} if present, else AGENTS.md"
    ));
    match (joined, listed) {
        (Some(w), _) => {
            workspaces::add_project(config_dir, &w, cwd)?;
            ask.show(&format!("joined workspace {w}"));
        }
        (None, Some(current)) => ask.show(&format!("already in workspace {current}")),
        (None, None) if workspace.is_none() && !ask.interactive() => {
            ask.show("in no workspace; join one with `aigentic workspace add . --workspace <name>`")
        }
        (None, None) => {}
    }
    Ok(0)
}

fn same_name(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

/// Asks which workspace to join: an existing one, `none` or `new`. The
/// default is the workspace whose shared folder or one of whose projects
/// sits beside `cwd`, else `none`.
fn choose(cwd: &Path, all: &[Workspace], ask: &mut dyn Ask) -> anyhow::Result<Join> {
    let parent = cwd.parent();
    let beside = all.iter().find(|w| {
        parent.is_some_and(|parent| {
            w.shared.as_deref() == Some(parent)
                || w.projects.iter().any(|p| p.parent() == Some(parent))
        })
    });
    let default = beside.map_or("none", |w| w.name.as_str());
    let mut options: Vec<&str> = all.iter().map(|w| w.name.as_str()).collect();
    options.extend(["none", "new"]);
    ask.show(&format!("workspaces: {}", options.join(", ")));
    let answer = ask.ask("join which workspace", default)?;
    let answer = answer.trim();
    if answer.eq_ignore_ascii_case("none") {
        return Ok(Join::None);
    }
    if answer.eq_ignore_ascii_case("new") {
        let name = ask.ask("new workspace's name", "")?;
        let name = name.trim();
        if name.is_empty() {
            bail!("a new workspace needs a name");
        }
        let shared = ask.ask("its shared folder", &cwd.display().to_string())?;
        // Collecting the components drops a `.`, so the default's `.`
        // answer is stored as the folder itself.
        let shared: PathBuf = cwd
            .join(workspaces::expand(Path::new(shared.trim())))
            .components()
            .collect();
        return Ok(Join::New {
            name: name.to_string(),
            shared,
        });
    }
    match all.iter().find(|w| same_name(&w.name, answer)) {
        Some(w) => Ok(Join::Existing(w.name.clone())),
        None => bail!("no workspace named {answer:?}"),
    }
}

/// `Notes` when neither `cwd` nor any folder above it has a `.git` entry,
/// a directory or a file (a worktree's or a submodule's).
fn kind_at(cwd: &Path) -> ProjectKind {
    if cwd
        .ancestors()
        .any(|d| d.join(".git").symlink_metadata().is_ok())
    {
        ProjectKind::Code
    } else {
        ProjectKind::Notes
    }
}

/// A new `aigentic.toml`: the name, `kind = "notes"` for a notes
/// project (a code project leaves the key out), and every other key
/// commented.
pub fn init_template(name: &str, kind: ProjectKind) -> String {
    let kind = match kind {
        ProjectKind::Notes => "kind = \"notes\"\n",
        ProjectKind::Code => "",
    };
    format!(
        r#"[project]
name = "{name}"
{kind}# description = ""

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

# [brief]
# refresh = true

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
                job: false,
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

    /// Answers by prompt fragment, else the default; `interactive` false
    /// stands for no terminal.
    struct Scripted {
        answers: Vec<(&'static str, &'static str)>,
        interactive: bool,
        shown: Vec<String>,
    }
    impl Scripted {
        fn new(answers: &[(&'static str, &'static str)]) -> Self {
            Self {
                answers: answers.to_vec(),
                interactive: true,
                shown: Vec::new(),
            }
        }
    }
    impl Ask for Scripted {
        fn show(&mut self, text: &str) {
            self.shown.push(text.to_owned());
        }
        fn ask(&mut self, prompt: &str, default: &str) -> anyhow::Result<String> {
            assert!(self.interactive, "asked {prompt:?} with no terminal");
            Ok(self
                .answers
                .iter()
                .find(|(k, _)| prompt.contains(k))
                .map_or(default, |(_, v)| v)
                .to_owned())
        }
        fn confirm(&mut self, prompt: &str, _default: bool) -> anyhow::Result<bool> {
            panic!("unexpected confirm {prompt:?}")
        }
        fn interactive(&self) -> bool {
            self.interactive
        }
    }

    /// A temp dir with a config dir and one workspace, `brand`, whose
    /// project `site` sits in `projects/`.
    fn with_brand() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("cfg");
        let projects = dir.path().join("projects");
        std::fs::create_dir_all(projects.join("site")).unwrap();
        workspaces::create(&cfg, "brand", None).unwrap();
        workspaces::add_project(&cfg, "brand", &projects.join("site")).unwrap();
        (dir, cfg, projects)
    }

    fn workspace_of_dir(cfg: &Path, root: &Path) -> Option<String> {
        let all = workspaces::load_all(cfg).unwrap();
        workspaces::workspace_of(&all, root).map(|w| w.name.clone())
    }

    #[test]
    fn init_writes_the_file_and_folders_and_refuses_to_nest() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("cfg");
        let root = dir.path().join("myapp");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let mut ask = Scripted::new(&[]);
        assert_eq!(init_with(&root, &cfg, None, &mut ask).unwrap(), 0);
        let p = Project::open_root(&root).unwrap();
        assert_eq!(p.name, "myapp");
        assert!(root.join(".aigentic/knowledge/.gitkeep").is_file());
        assert!(root.join(".aigentic/memory/.gitkeep").is_file());
        assert!(p.memory.is_empty(), ".gitkeep is not a memory file");
        let err = init_with(&root.join("sub"), &cfg, None, &mut ask)
            .unwrap_err()
            .to_string();
        assert!(err.contains("already a project"), "{err}");
        assert!(init_with(&root, &cfg, None, &mut ask).is_err());
    }

    #[test]
    fn a_scripted_answer_joins_the_workspace_beside_it_by_default() {
        let (_dir, cfg, projects) = with_brand();
        let app = projects.join("app");
        std::fs::create_dir_all(&app).unwrap();
        let mut ask = Scripted::new(&[]);
        init_with(&app, &cfg, None, &mut ask).unwrap();
        assert!(
            ask.shown
                .contains(&"workspaces: brand, none, new".to_owned()),
            "{:?}",
            ask.shown
        );
        assert_eq!(workspace_of_dir(&cfg, &app).as_deref(), Some("brand"));
        assert!(ask.shown.contains(&"joined workspace brand".to_owned()));
    }

    #[test]
    fn a_scripted_answer_creates_a_new_workspace() {
        let (dir, cfg, _) = with_brand();
        let notes = dir.path().join("elsewhere/notes");
        std::fs::create_dir_all(&notes).unwrap();
        let mut ask = Scripted::new(&[
            ("join which", "new"),
            ("name", "Writing"),
            ("shared folder", "."),
        ]);
        init_with(&notes, &cfg, None, &mut ask).unwrap();
        let all = workspaces::load_all(&cfg).unwrap();
        let w = all.iter().find(|w| w.name == "Writing").unwrap();
        assert_eq!(w.projects, vec![notes.clone()]);
        assert_eq!(w.shared.as_ref(), Some(&notes));
        let text = std::fs::read_to_string(cfg.join("workspaces/writing.toml")).unwrap();
        assert!(!text.contains("/.\""), "{text}");
    }

    #[test]
    fn a_scripted_answer_chooses_none() {
        let (_dir, cfg, projects) = with_brand();
        let app = projects.join("app");
        std::fs::create_dir_all(&app).unwrap();
        let before = workspaces::load_all(&cfg).unwrap();
        let mut ask = Scripted::new(&[("join which", "none")]);
        init_with(&app, &cfg, None, &mut ask).unwrap();
        assert!(app.join(FILE_NAME).is_file());
        assert_eq!(workspaces::load_all(&cfg).unwrap(), before);
    }

    #[test]
    fn an_unknown_answer_is_refused_before_anything_is_written() {
        let (_dir, cfg, projects) = with_brand();
        let app = projects.join("app");
        std::fs::create_dir_all(&app).unwrap();
        let mut ask = Scripted::new(&[("join which", "nope")]);
        let err = init_with(&app, &cfg, None, &mut ask).unwrap_err();
        assert_eq!(err.to_string(), "no workspace named \"nope\"");
        assert!(!app.join(FILE_NAME).exists());
        let err = init_with(&app, &cfg, Some("nope"), &mut ask).unwrap_err();
        assert_eq!(err.to_string(), "no workspace named \"nope\"");
        assert!(!app.join(FILE_NAME).exists());
    }

    #[test]
    fn the_flag_names_the_workspace_without_asking() {
        let (_dir, cfg, projects) = with_brand();
        let app = projects.join("app");
        std::fs::create_dir_all(&app).unwrap();
        let mut ask = Scripted::new(&[]);
        ask.interactive = false;
        init_with(&app, &cfg, Some("Brand"), &mut ask).unwrap();
        assert_eq!(workspace_of_dir(&cfg, &app).as_deref(), Some("brand"));
    }

    #[test]
    fn with_no_terminal_and_no_flag_the_file_is_written_and_no_workspace_changes() {
        let (_dir, cfg, projects) = with_brand();
        let app = projects.join("app");
        std::fs::create_dir_all(&app).unwrap();
        let before = workspaces::load_all(&cfg).unwrap();
        let mut ask = Scripted::new(&[]);
        ask.interactive = false;
        init_with(&app, &cfg, None, &mut ask).unwrap();
        assert!(app.join(FILE_NAME).is_file());
        assert_eq!(workspaces::load_all(&cfg).unwrap(), before);
        assert_eq!(
            ask.shown.last().map(String::as_str),
            Some("in no workspace; join one with `aigentic workspace add . --workspace <name>`")
        );
    }

    #[test]
    fn a_folder_already_in_a_workspace_is_a_no_op() {
        let (_dir, cfg, projects) = with_brand();
        let site = projects.join("site");
        let before = workspaces::load_all(&cfg).unwrap();
        let mut ask = Scripted::new(&[]);
        ask.interactive = false;
        let err = init_with(&site, &cfg, Some("other"), &mut ask).unwrap_err();
        assert!(err.to_string().contains("already in workspace"), "{err}");
        init_with(&site, &cfg, Some("brand"), &mut ask).unwrap();
        assert_eq!(workspaces::load_all(&cfg).unwrap(), before);
        assert_eq!(
            ask.shown.last().map(String::as_str),
            Some("already in workspace brand")
        );
    }

    #[test]
    fn a_folder_with_no_git_gets_kind_notes() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("cfg");
        let notes = dir.path().join("notes");
        std::fs::create_dir_all(&notes).unwrap();
        // The temp dir must not sit inside a repository for this to mean
        // anything.
        assert_eq!(kind_at(&notes), ProjectKind::Notes);
        init_with(&notes, &cfg, Some("none"), &mut Scripted::new(&[])).unwrap();
        let text = std::fs::read_to_string(notes.join(FILE_NAME)).unwrap();
        assert!(
            text.contains("[project]\nname = \"notes\"\nkind = \"notes\"\n"),
            "{text}"
        );
        assert_eq!(
            Project::open_root(&notes).unwrap().kind(),
            ProjectKind::Notes
        );
    }

    #[test]
    fn a_folder_under_a_git_directory_or_file_leaves_kind_out() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("cfg");
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("app")).unwrap();
        let worktree = dir.path().join("worktree");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(worktree.join(".git"), "gitdir: /elsewhere\n").unwrap();
        for root in [repo.join("app"), worktree] {
            init_with(&root, &cfg, Some("none"), &mut Scripted::new(&[])).unwrap();
            let text = std::fs::read_to_string(root.join(FILE_NAME)).unwrap();
            assert!(!text.contains("kind"), "{text}");
            assert_eq!(Project::open_root(&root).unwrap().kind(), ProjectKind::Code);
        }
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
