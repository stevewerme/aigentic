//! The start-up ask (issue #121): which project to work in when the
//! folder doesn't say. One pure ladder decides whether to ask; the rows
//! come from the daemon — its projects, matched by root, and the light
//! `StartRows` listing — never from a side table (AGENTS.md: the log is
//! the source of truth); the prompt goes through `init_cmd::Ask`, so it
//! is scripted in tests and line-based at a terminal, and one keystroke
//! takes the default.
//!
//! Nothing here opens a thread or moves one: it answers "which project",
//! and the answer rides `Request::Front` as `asked` (issue #121's first
//! commit), where the daemon logs it as a `project` decision and honours
//! it.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, SystemTime};

use aigentic_api::client::Client;
use aigentic_api::{ProjectInfo, Request, Response, StartAsk, StartRow};
use aigentic_runtime::Project;
use aigentic_server::workspaces::{Workspace, project_name};
use anyhow::anyhow;
use ulid::Ulid;

use crate::app::status::elapsed_short;
use crate::init_cmd::Ask as Asker;

/// What the launch already says, before the ladder runs. `--project` and
/// `--thread` settle the question; `exec` never asks; `interactive` is
/// stdin and stdout both being terminals.
#[derive(Debug, Clone, Copy)]
pub struct Launch<'a> {
    pub project: Option<&'a str>,
    pub thread: Option<Ulid>,
    pub exec: bool,
    pub interactive: bool,
}

impl Launch<'_> {
    /// The launch already names the project or the thread: no ask.
    fn settled(&self) -> bool {
        self.project.is_some() || self.thread.is_some() || self.exec
    }
}

/// One line of the ask: a project, its workspace heading, and how long
/// since a thread was last started in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub project: String,
    /// The workspace heading; `None` is `other`.
    pub workspace: Option<String>,
    /// The newest thread id's timestamp in that project, if any. "Last
    /// used" here means "last started a thread in", the only ordering
    /// the listing can give (issue #121).
    pub last: Option<SystemTime>,
    /// Row 1: what Enter takes.
    pub default: bool,
    /// Read as `continue in <project>`: the front thread is there.
    pub continue_in: bool,
}

impl Row {
    /// The label the row shows: `continue in web`, or just the name.
    pub fn label(&self) -> String {
        if self.continue_in {
            format!("continue in {}", self.project)
        } else {
            self.project.clone()
        }
    }

    /// `· last thread 3h 05m` / `· no threads yet`, from the newest
    /// thread id's timestamp through `elapsed_short`'s shape.
    pub fn note(&self, now: SystemTime) -> String {
        match self.last.and_then(|t| now.duration_since(t).ok()) {
            Some(d) => format!("· last thread {}", elapsed_short(d)),
            None => "· no threads yet".to_owned(),
        }
    }
}

/// The ask: one flat list, grouped by workspace, every row shown and
/// numbered `1..n`, then the projects the daemon cannot reach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ask {
    pub rows: Vec<Row>,
    /// Workspace projects the daemon does not know (issue #121): a
    /// same-named project in another workspace took the name, so
    /// `workspaces::merge` dropped this one and no answer can reach it
    /// (#122 is the daemon-side half). Shown as a dim line, never
    /// numbered.
    pub shadowed: Vec<Shadowed>,
    /// Why the ladder asked, in its own words, recorded on the decision.
    pub reason: String,
}

/// A workspace project the daemon cannot reach (issue #121), and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shadowed {
    /// The workspace it belongs to, for the line's place in the block.
    pub workspace: String,
    /// The name it wanted.
    pub project: String,
    /// The plain-language reason: `not reachable: its name is also
    /// getscale's web; rename one in its aigentic.toml`.
    pub reason: String,
}

impl Ask {
    /// The block the person reads, and the rows they answer by number.
    /// The question first, then the groups; no paging row, so a long
    /// sheet is honest about its size. Each group's unreachable
    /// projects follow its rows as a dim, unnumbered line.
    pub fn render(&self, now: SystemTime) -> String {
        let labels: Vec<String> = self.rows.iter().map(Row::label).collect();
        let width = labels.iter().map(|l| l.chars().count()).max().unwrap_or(0);
        // Every group, in the order its first row appears. A group whose
        // projects are all unreachable has no rows of its own, so it
        // comes last, and each group's unreachable projects follow its
        // rows as a dim line: a row's shape without a number.
        let mut groups: Vec<Option<String>> = Vec::new();
        for row in &self.rows {
            if !groups.contains(&row.workspace) {
                groups.push(row.workspace.clone());
            }
        }
        for s in &self.shadowed {
            let group = Some(s.workspace.clone());
            if !groups.contains(&group) {
                groups.push(group);
            }
        }
        let mut out = String::from("Which project? (Enter = 1)");
        for group in &groups {
            out.push_str(&format!("\n  {}", group.as_deref().unwrap_or("other")));
            for (i, row) in self.rows.iter().enumerate() {
                if &row.workspace != group {
                    continue;
                }
                out.push_str(&format!(
                    "\n    {:>2}  {:<width$}  {}",
                    i + 1,
                    labels[i],
                    row.note(now),
                    width = width
                ));
            }
            for s in self
                .shadowed
                .iter()
                .filter(|s| group.as_deref() == Some(s.workspace.as_str()))
            {
                out.push_str(&format!("\n   {}  \u{b7} {}", s.project, s.reason));
            }
        }
        out
    }
}

/// What the ladder decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ladder {
    /// Ask: these are the rows.
    Ask(Box<Ask>),
    /// No ask, and nothing to say.
    None,
    /// No ask, and say one line: there was something to choose from, but
    /// this run is not at a terminal (the plain REPL behind a pipe).
    Hint,
}

/// What the prompt came back with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Prompting {
    /// No ask happened: `Front` goes as it always did.
    None,
    /// The person answered.
    Answered(Box<StartAsk>),
    /// The person quit: no thread is opened.
    Quit,
}

/// The ladder (design 1). `folder` is the project the folder resolves to
/// on its own — the embedded daemon's ad hoc project in a bare folder —
/// `projects` are the projects the daemon knows, and `threads` is the
/// light `StartRows` listing.
///
/// The spec's reasons for no ask are all here: `--project`, `--thread`,
/// `exec`, a cwd inside a known project, a run that is not at a terminal
/// (`Hint` instead, so the caller can print its one line), and a first
/// run with nothing to choose from.
pub fn ladder(
    cwd: &Path,
    workspaces: &[Workspace],
    projects: &[ProjectInfo],
    threads: &[StartRow],
    folder: &str,
    launch: &Launch<'_>,
) -> Ladder {
    if launch.settled() {
        return Ladder::None;
    }
    // Inside a project: the project is obvious, and this clause wins over
    // the shared-folder clause below, so a folder that is both is a
    // project and nobody is asked.
    if project_here(cwd, workspaces).is_some() {
        return Ladder::None;
    }
    let in_workspace = workspace_folder(cwd, workspaces);
    let (rows, shadowed) = rows(
        projects,
        threads,
        workspaces,
        in_workspace.map(|w| w.name.as_str()),
    );
    // Nothing to choose from: no workspace is configured and no project
    // is known besides the ad hoc one.
    if workspaces.is_empty() && !rows.iter().any(|r| r.project != folder) {
        return Ladder::None;
    }
    if !launch.interactive {
        return Ladder::Hint;
    }
    let reason = match in_workspace {
        Some(w) => format!(
            "in the {} workspace's folder; which of its projects to work in",
            w.name
        ),
        None => "the folder doesn't say which project; asked at start-up".to_owned(),
    };
    Ladder::Ask(Box::new(Ask {
        rows,
        shadowed,
        reason,
    }))
}

/// The project the folder is in: an `aigentic.toml` up the tree, or a
/// workspace project's root at or above it.
fn project_here(cwd: &Path, workspaces: &[Workspace]) -> Option<String> {
    if let Ok(Some(p)) = Project::open(cwd) {
        return Some(p.name);
    }
    workspaces
        .iter()
        .flat_map(|w| w.projects.iter())
        .find(|root| cwd.starts_with(root))
        .map(|root| project_name(root))
}

/// The workspace whose folder the cwd is in: inside its `shared` folder,
/// or an ancestor of one of its project roots (which includes the roots
/// themselves — callers check `project_here` first).
pub fn workspace_folder<'a>(cwd: &Path, workspaces: &'a [Workspace]) -> Option<&'a Workspace> {
    workspaces.iter().find(|w| {
        w.shared.as_deref().is_some_and(|s| cwd.starts_with(s))
            || w.projects.iter().any(|root| root.starts_with(cwd))
    })
}

/// The rows: the projects the daemon knows, grouped by workspace, `other`
/// last, each group by last used, never-used last by name. When the cwd
/// is in a workspace, only that workspace's projects are listed.
///
/// A project is tied to a workspace, and to this user's threads, by
/// **root**, never by name (issue #121): a name is not an identity, since
/// two workspaces may each hold a `web`. A workspace project the daemon
/// does not know — a same-named project elsewhere took the name, so
/// `workspaces::merge` dropped this one — is not a row at all: it comes
/// back as a `Shadowed` line, and answering for it re-asks.
fn rows(
    projects: &[ProjectInfo],
    threads: &[StartRow],
    workspaces: &[Workspace],
    only: Option<&str>,
) -> (Vec<Row>, Vec<Shadowed>) {
    let workspace_of = |root: &Path| -> Option<String> {
        workspaces
            .iter()
            .find(|w| w.projects.iter().any(|p| same_root(p, root)))
            .map(|w| w.name.clone())
    };
    // One row per known project, carrying the newest thread started in
    // it. Only this user's threads are here; the daemon sent no others.
    let mut found: BTreeMap<(Option<String>, String), Option<SystemTime>> = BTreeMap::new();
    for p in projects {
        found
            .entry((workspace_of(&p.root), p.name.clone()))
            .or_insert(None);
    }
    for t in threads {
        let Some(p) = projects
            .iter()
            .find(|p| p.name == t.project && root_matches(t.root.as_deref(), &p.root))
        else {
            continue;
        };
        let at = SystemTime::UNIX_EPOCH + Duration::from_millis(t.id.timestamp_ms());
        let entry = found
            .entry((workspace_of(&p.root), p.name.clone()))
            .or_insert(Some(at));
        if entry.is_none_or(|l| at > l) {
            *entry = Some(at);
        }
    }
    // A workspace project the daemon does not know, and the project that
    // took its name, said plainly.
    let mut shadowed: Vec<Shadowed> = Vec::new();
    for w in workspaces {
        if only.is_some_and(|name| name != w.name) {
            continue;
        }
        for root in &w.projects {
            if projects.iter().any(|p| same_root(&p.root, root)) {
                continue;
            }
            let name = project_name(root);
            let owner = projects
                .iter()
                .find(|p| p.name == name)
                .and_then(|p| workspace_of(&p.root))
                .unwrap_or_else(|| "another".to_owned());
            shadowed.push(Shadowed {
                workspace: w.name.clone(),
                project: name.clone(),
                reason: format!(
                    "not reachable: its name is also {owner}'s {name}; \
                     rename one in its aigentic.toml"
                ),
            });
        }
    }
    let mut rows: Vec<Row> = found
        .into_iter()
        .filter(|((ws, _), _)| only.is_none_or(|name| ws.as_deref() == Some(name)))
        .map(|((workspace, project), last)| Row {
            project,
            workspace,
            last,
            default: false,
            continue_in: false,
        })
        .collect();
    // Newest first inside a group, never used last by name.
    rows.sort_by(|a, b| {
        b.last
            .cmp(&a.last)
            .then_with(|| a.project.cmp(&b.project))
            .then_with(|| a.workspace.cmp(&b.workspace))
    });

    // Row 1: the front thread's project when it is listed, else the most
    // recently used project. Its group leads the page too, so `Enter = 1`
    // is the default and the grouping still reads straight down.
    let front = threads.iter().find(|t| t.front);
    let front_row = front.and_then(|f| {
        let p = projects
            .iter()
            .find(|p| p.name == f.project && root_matches(f.root.as_deref(), &p.root))?;
        let ws = workspace_of(&p.root);
        rows.iter()
            .position(|r| r.project == p.name && r.workspace == ws)
    });
    let default = front_row
        .or_else(|| rows.iter().position(|r| r.last.is_some()))
        .or_else(|| (!rows.is_empty()).then_some(0));
    let Some(d) = default else {
        return (rows, shadowed);
    };
    rows[d].default = true;
    rows[d].continue_in = front_row == Some(d);
    let group = rows[d].workspace.clone();
    let mut first: Vec<Row> = rows
        .iter()
        .filter(|r| r.workspace == group)
        .cloned()
        .collect();
    let rest: Vec<Row> = rows
        .iter()
        .filter(|r| r.workspace != group)
        .cloned()
        .collect();
    if let Some(at) = first.iter().position(|r| r.default) {
        let d = first.remove(at);
        first.insert(0, d);
    }
    first.extend(rest);
    (first, shadowed)
}

/// Two roots are the same project directory: canonicalised when both
/// exist, compared as written otherwise (a workspace file may name a path
/// that is not there yet).
fn same_root(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    norm(a) == norm(b)
}

/// A thread's root (when it names one) matches a project's root. A thread
/// that never moved has no root of its own, so its project's name — the
/// daemon's own name for the log's `project` — is all there is to match.
fn root_matches(thread: Option<&Path>, project: &Path) -> bool {
    thread.is_none_or(|root| same_root(root, project))
}

/// What an answer means.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Picked {
    /// The row they named; `0` is Enter and row 1.
    Row(usize),
    Quit,
    /// Unknown, ambiguous, or no such row.
    Bad,
    /// A project the daemon cannot reach (issue #121, finding 1), with the
    /// reason to show: its name is taken by a same-named project in
    /// another workspace, so choosing it would pick the wrong project.
    Unreachable(String),
}

/// Resolve one answer: a row number, a project name, or
/// `<workspace>/<project>` when a name repeats.
fn resolve(ask: &Ask, answer: &str) -> Picked {
    let answer = answer.trim();
    if answer.is_empty() {
        return Picked::Row(0);
    }
    if answer.eq_ignore_ascii_case("q") || answer.eq_ignore_ascii_case("quit") {
        return Picked::Quit;
    }
    if let Ok(n) = answer.parse::<usize>() {
        return if n >= 1 && n <= ask.rows.len() {
            Picked::Row(n - 1)
        } else {
            Picked::Bad
        };
    }
    let (ws, project) = match answer.split_once('/') {
        Some((ws, project)) => (Some(ws), project),
        None => (None, answer),
    };
    let hits: Vec<usize> = ask
        .rows
        .iter()
        .enumerate()
        .filter(|(_, r)| {
            r.project == project && ws.is_none_or(|ws| r.workspace.as_deref() == Some(ws))
        })
        .map(|(i, _)| i)
        .collect();
    match hits.as_slice() {
        [one] => Picked::Row(*one),
        // No row: a project the daemon cannot reach says why, so an answer
        // for it never lands in the same-named project that took its name.
        [] => match ask
            .shadowed
            .iter()
            .find(|s| s.project == project && ws.is_none_or(|ws| s.workspace == ws))
        {
            Some(s) => Picked::Unreachable(s.reason.clone()),
            None => Picked::Bad,
        },
        _ => Picked::Bad,
    }
}

/// Ask, through `init_cmd::Ask` (design 3): the block, then one line. An
/// unknown or ambiguous answer re-asks once, then takes row 1 and says
/// so; `q` quits.
pub fn choose(ask: &Ask, ask_trait: &mut dyn Asker, now: SystemTime) -> anyhow::Result<Prompting> {
    ask_trait.show(&ask.render(now));
    let mut tried = false;
    loop {
        let answer = ask_trait.ask("Number, name or q", "1")?;
        match resolve(ask, &answer) {
            Picked::Quit => return Ok(Prompting::Quit),
            Picked::Row(i) => return Ok(Prompting::Answered(Box::new(answered(ask, i)))),
            Picked::Bad if !tried => {
                ask_trait.show("[not one of the rows: try again, or Enter for 1]");
                tried = true;
            }
            Picked::Unreachable(reason) if !tried => {
                ask_trait.show(&format!("[{reason}]"));
                ask_trait.show("[try again, or Enter for 1]");
                tried = true;
            }
            Picked::Bad | Picked::Unreachable(_) => {
                ask_trait.show(&format!("[taking 1 — {}]", ask.rows[0].label()));
                return Ok(Prompting::Answered(Box::new(answered(ask, 0))));
            }
        }
    }
}

/// The answer, as `Front` carries it: `offered` is row 1, the one Enter
/// takes; `chosen` is the row they took.
fn answered(ask: &Ask, row: usize) -> StartAsk {
    StartAsk {
        offered: ask.rows[0].project.clone(),
        chosen: ask.rows[row].project.clone(),
        reason: ask.reason.clone(),
    }
}

/// The whole ask, in the order the launch needs it: the cheap reasons
/// first, so a start inside a project pays nothing, then the listing,
/// then the prompt. Prints the one line a piped run gets (design 1).
pub async fn run(
    client: &Client,
    cwd: &Path,
    workspaces: &[Workspace],
    projects: &[ProjectInfo],
    folder: &str,
    launch: Launch<'_>,
) -> anyhow::Result<Prompting> {
    // The ladder's own reasons, before the listing costs anything.
    if launch.settled() || project_here(cwd, workspaces).is_some() {
        return Ok(Prompting::None);
    }
    // The ask's own light listing (issue #121, review finding 3): the
    // index, never a log body. `ListThreads` stays what `/threads` reads.
    let rows = match client.request(Request::StartRows).await? {
        Response::StartRows { rows } => rows,
        other => return Err(anyhow!("unexpected reply listing threads: {other:?}")),
    };
    match ladder(cwd, workspaces, projects, &rows, folder, &launch) {
        Ladder::None => Ok(Prompting::None),
        Ladder::Hint => {
            // Say where this run lands, so a script's reader knows what
            // it got without asking: the front thread's project if one
            // resumes, else the folder's.
            let into = rows
                .iter()
                .find(|r| r.front)
                .map(|r| r.project.clone())
                .unwrap_or_else(|| folder.to_owned());
            println!("[starting in {into}; pass --project to choose]");
            Ok(Prompting::None)
        }
        Ladder::Ask(ask) => choose(&ask, &mut crate::init_cmd::Terminal, SystemTime::now()),
    }
}

/// The `exec` guard (design 6): today's rule, plus a refusal when the cwd
/// is a workspace's folder, since that names the group and not the
/// project. The project clause wins, as in the ladder: an `aigentic.toml`
/// here, or a workspace project's root, is a project. `settled` is
/// `--project` or `--thread`; `opened` is a project file found here.
pub fn exec_guard(
    cwd: &Path,
    workspaces: &[Workspace],
    known: &[String],
    settled: bool,
    opened: bool,
) -> Option<String> {
    if settled || opened || project_here(cwd, workspaces).is_some() {
        return None;
    }
    let mut parts: Vec<String> = Vec::new();
    if let Some(w) = workspace_folder(cwd, workspaces) {
        let names: Vec<String> = w.projects.iter().map(|r| project_name(r)).collect();
        if !names.is_empty() {
            parts.push(format!("{}: {}", w.name, names.join(", ")));
        }
        let other: Vec<&str> = known
            .iter()
            .map(String::as_str)
            .filter(|n| !names.iter().any(|q| q == n))
            .collect();
        if !other.is_empty() {
            parts.push(format!("other: {}", other.join(", ")));
        }
    }
    let list = if parts.is_empty() {
        if known.is_empty() {
            "one of: none".to_owned()
        } else {
            format!("one of: {}", known.join(", "))
        }
    } else {
        parts.join("; ")
    };
    Some(format!(
        "exec needs a project: pass --project ({list}), run it in a project directory, or pass --thread"
    ))
}
#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::path::PathBuf;

    use tempfile::TempDir;

    use super::*;

    /// One clock for the whole module, so the notes are exact.
    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(4_000_000)
    }

    fn at(secs_ago: u64) -> SystemTime {
        now() - Duration::from_secs(secs_ago)
    }

    fn id_at(when: SystemTime) -> Ulid {
        let ms = when
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        Ulid::from_parts(ms, 7)
    }

    /// One `StartRows` row: `project`, started `secs_ago` (issue #121's
    /// light listing). No root, which matches by name — what a thread
    /// that never moved does.
    fn thread(project: &str, secs_ago: u64) -> StartRow {
        StartRow {
            id: id_at(at(secs_ago)),
            project: project.to_owned(),
            root: None,
            workspace: None,
            front: false,
        }
    }

    /// The front thread's row.
    fn front(project: &str, secs_ago: u64) -> StartRow {
        StartRow {
            front: true,
            ..thread(project, secs_ago)
        }
    }

    /// A project the daemon knows, as `Welcome` lists it.
    fn proj(name: &str, root: &Path) -> ProjectInfo {
        ProjectInfo {
            name: name.to_owned(),
            root: root.to_path_buf(),
            role: Some("write".to_owned()),
            threads: 0,
        }
    }

    /// The projects the daemon knows from these workspaces: one per root,
    /// and a name that repeats is kept by the **first** — that is what
    /// `workspaces::merge` does (issue #121, finding 1), so the daemon
    /// knows only one `web` and the second is unreachable.
    fn known(workspaces: &[Workspace]) -> Vec<ProjectInfo> {
        let mut out: Vec<ProjectInfo> = Vec::new();
        for w in workspaces {
            for root in &w.projects {
                let name = project_name(root);
                if out.iter().any(|p| p.name == name) {
                    continue;
                }
                out.push(proj(&name, root));
            }
        }
        out
    }

    /// A workspace file's shape: a name, a shared folder, roots.
    fn workspace(name: &str, shared: Option<&Path>, projects: &[&Path]) -> Workspace {
        Workspace {
            name: name.to_owned(),
            shared: shared.map(|p| p.to_path_buf()),
            projects: projects.iter().map(|p| p.to_path_buf()).collect(),
        }
    }

    /// A directory with an `aigentic.toml` naming the project.
    fn project_dir(name: &str) -> TempDir {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("aigentic.toml"),
            format!("[project]\nname = \"{name}\"\n"),
        )
        .unwrap();
        dir
    }

    fn interactive() -> Launch<'static> {
        Launch {
            project: None,
            thread: None,
            exec: false,
            interactive: true,
        }
    }

    /// The two workspaces and folders #121 keeps using: getscale (web,
    /// api) and vendela (site) — and each workspace holds a `web`, so one
    /// of them is a project the daemon cannot reach (finding 1). getscale
    /// comes first, so its `web` is the one that keeps the name.
    struct Fixture {
        _root: TempDir,
        shared: PathBuf,
        web: PathBuf,
        workspaces: Vec<Workspace>,
    }

    fn fixture() -> Fixture {
        let root = TempDir::new().unwrap();
        let base = root.path();
        let shared = base.join("getscale");
        let web = shared.join("web");
        let api = shared.join("api");
        let site = base.join("site");
        let vendela_web = base.join("vendela").join("web");
        for p in [&web, &api, &site, &vendela_web] {
            std::fs::create_dir_all(p).unwrap();
        }
        let workspaces = vec![
            workspace("getscale", Some(&shared), &[&web, &api]),
            workspace("vendela", None, &[&site, &vendela_web]),
        ];
        Fixture {
            _root: root,
            shared,
            web,
            workspaces,
        }
    }

    fn rows_of(l: &Ladder) -> Vec<String> {
        match l {
            Ladder::Ask(a) => a.rows.iter().map(|r| r.project.clone()).collect(),
            other => panic!("expected an ask, got {other:?}"),
        }
    }

    fn ask_of(l: &Ladder) -> &Ask {
        match l {
            Ladder::Ask(a) => a,
            other => panic!("expected an ask, got {other:?}"),
        }
    }

    // ---- T1: the ladder ----

    /// T1: a bare folder with a workspace configured asks, and its rows
    /// are that workspace's projects.
    #[test]
    fn t1_a_bare_folder_with_a_workspace_asks() {
        let f = fixture();
        let bare = TempDir::new().unwrap();
        let l = ladder(
            bare.path(),
            &f.workspaces,
            &known(&f.workspaces),
            &[],
            "bare",
            &interactive(),
        );
        assert_eq!(rows_of(&l), vec!["api", "web", "site"]);
    }

    /// T1: an `aigentic.toml` up the tree is a project: no ask.
    #[test]
    fn t1_b_inside_a_project_by_toml_asks_nothing() {
        let f = fixture();
        let p = project_dir("random");
        let inside = p.path().join("src");
        std::fs::create_dir_all(&inside).unwrap();
        let l = ladder(
            &inside,
            &f.workspaces,
            &known(&f.workspaces),
            &[],
            "random",
            &interactive(),
        );
        assert_eq!(l, Ladder::None);
    }

    /// T1: a workspace project's root, and anything under it, is a
    /// project: no ask.
    #[test]
    fn t1_c_inside_a_workspace_project_root_asks_nothing() {
        let f = fixture();
        let under = f.web.join("src");
        std::fs::create_dir_all(&under).unwrap();
        for cwd in [&f.web, &under] {
            let l = ladder(
                cwd,
                &f.workspaces,
                &known(&f.workspaces),
                &[],
                "web",
                &interactive(),
            );
            assert_eq!(l, Ladder::None, "at {}", cwd.display());
        }
    }

    /// T1: a folder that is both a workspace's shared folder and inside a
    /// project is a project: the project clause wins, so no ask.
    #[test]
    fn t1_d_a_shared_folder_that_is_also_a_project_asks_nothing() {
        let p = project_dir("both");
        let child = p.path().join("child");
        std::fs::create_dir_all(&child).unwrap();
        let workspaces = vec![workspace("both", Some(p.path()), &[&child])];
        let l = ladder(
            p.path(),
            &workspaces,
            &known(&workspaces),
            &[],
            "both",
            &interactive(),
        );
        assert_eq!(l, Ladder::None);
    }

    /// T1: `--project`, `--thread` and `exec` each settle it.
    #[test]
    fn t1_e_a_settled_launch_asks_nothing() {
        let f = fixture();
        let bare = TempDir::new().unwrap();
        let mut l = interactive();
        l.project = Some("web");
        assert_eq!(
            ladder(
                bare.path(),
                &f.workspaces,
                &known(&f.workspaces),
                &[],
                "bare",
                &l
            ),
            Ladder::None
        );
        let mut l = interactive();
        l.thread = Some(Ulid::generate());
        assert_eq!(
            ladder(
                bare.path(),
                &f.workspaces,
                &known(&f.workspaces),
                &[],
                "bare",
                &l
            ),
            Ladder::None
        );
        let mut l = interactive();
        l.exec = true;
        assert_eq!(
            ladder(
                bare.path(),
                &f.workspaces,
                &known(&f.workspaces),
                &[],
                "bare",
                &l
            ),
            Ladder::None
        );
    }

    /// T1: behind a pipe there is no ask; the caller prints one line
    /// instead, so the ladder says `Hint`.
    #[test]
    fn t1_f_a_pipe_hints_instead_of_asking() {
        let f = fixture();
        let bare = TempDir::new().unwrap();
        let mut l = interactive();
        l.interactive = false;
        assert_eq!(
            ladder(
                bare.path(),
                &f.workspaces,
                &known(&f.workspaces),
                &[],
                "bare",
                &l
            ),
            Ladder::Hint
        );
    }

    /// T1: a first run with nothing to choose from — no workspace, and
    /// only the ad hoc project known — asks nothing.
    #[test]
    fn t1_g_nothing_configured_asks_nothing() {
        let bare = TempDir::new().unwrap();
        let projects = vec![proj("bare", bare.path())];
        let listing = vec![thread("bare", 60)];
        assert_eq!(
            ladder(
                bare.path(),
                &[],
                &projects,
                &listing,
                "bare",
                &interactive()
            ),
            Ladder::None
        );
    }

    /// T1: a workspace's shared folder lists that workspace's projects
    /// only, not the projects of another one.
    #[test]
    fn t1_h_a_shared_folder_lists_only_that_workspace() {
        let f = fixture();
        let listing = vec![thread("site", 60)];
        let l = ladder(
            &f.shared,
            &f.workspaces,
            &known(&f.workspaces),
            &listing,
            "getscale",
            &interactive(),
        );
        assert_eq!(rows_of(&l), vec!["api", "web"]);
    }

    /// T1: an ancestor of a workspace's roots does the same.
    #[test]
    fn t1_i_an_ancestor_of_the_roots_lists_only_that_workspace() {
        let f = fixture();
        let l = ladder(
            &f.shared,
            &f.workspaces,
            &known(&f.workspaces),
            &[],
            "getscale",
            &interactive(),
        );
        assert_eq!(rows_of(&l), vec!["api", "web"]);
    }

    /// T1: a bare folder gives every known project, grouped, `other`
    /// last.
    #[test]
    fn t1_j_a_bare_folder_lists_every_project_grouped() {
        let f = fixture();
        let bare = TempDir::new().unwrap();
        let mut projects = known(&f.workspaces);
        projects.push(proj("loose", &bare.path().join("loose")));
        let listing = vec![thread("web", 60), thread("site", 30), thread("loose", 10)];
        let l = ladder(
            bare.path(),
            &f.workspaces,
            &projects,
            &listing,
            "bare",
            &interactive(),
        );
        let ask = ask_of(&l);
        let groups: Vec<Option<String>> = ask.rows.iter().map(|r| r.workspace.clone()).collect();
        assert_eq!(
            groups,
            vec![
                None,
                Some("vendela".to_owned()),
                Some("getscale".to_owned()),
                Some("getscale".to_owned())
            ]
        );
        assert_eq!(rows_of(&l), vec!["loose", "site", "web", "api"]);
    }

    /// T1/T2 (issue #121, finding 1): a workspace project the daemon
    /// cannot reach is not a row. Its name is taken by the same-named
    /// project of the workspace that came first, so the daemon knows one
    /// `web`, and vendela's is the one that lost.
    #[test]
    fn t1_k_a_project_the_daemon_cannot_reach_is_not_a_row() {
        let f = fixture();
        let bare = TempDir::new().unwrap();
        let l = ladder(
            bare.path(),
            &f.workspaces,
            &known(&f.workspaces),
            &[],
            "bare",
            &interactive(),
        );
        let ask = ask_of(&l);
        assert_eq!(
            rows_of(&l),
            vec!["api", "web", "site"],
            "one `web`, not two"
        );
        assert_eq!(ask.shadowed.len(), 1, "{:?}", ask.shadowed);
        assert_eq!(ask.shadowed[0].workspace, "vendela");
        assert_eq!(ask.shadowed[0].project, "web");
        assert_eq!(
            ask.shadowed[0].reason,
            "not reachable: its name is also getscale's web; \
             rename one in its aigentic.toml"
        );
        // Inside getscale's own scope nothing is unreachable.
        let inside = ladder(
            &f.shared,
            &f.workspaces,
            &known(&f.workspaces),
            &[],
            "getscale",
            &interactive(),
        );
        assert!(ask_of(&inside).shadowed.is_empty());
    }

    // ---- T2: the rows ----

    /// T2: row 1 is the front thread's project when it is listed, read as
    /// `continue in <project>`.
    #[test]
    fn t2_a_row_one_is_the_front_threads_project_when_listed() {
        let f = fixture();
        let bare = TempDir::new().unwrap();
        let listing = vec![front("api", 10), thread("web", 20), thread("site", 30)];
        let l = ladder(
            bare.path(),
            &f.workspaces,
            &known(&f.workspaces),
            &listing,
            "bare",
            &interactive(),
        );
        let ask = ask_of(&l);
        assert_eq!(ask.rows[0].project, "api");
        assert!(ask.rows[0].default);
        assert!(ask.rows[0].continue_in);
        assert_eq!(ask.rows[0].label(), "continue in api");
    }

    /// T2: without a front thread, row 1 is the most recently used
    /// project of the list.
    #[test]
    fn t2_b_row_one_is_the_newest_used_project_without_a_front_thread() {
        let f = fixture();
        let bare = TempDir::new().unwrap();
        let listing = vec![thread("web", 600), thread("site", 60), thread("api", 3000)];
        let l = ladder(
            bare.path(),
            &f.workspaces,
            &known(&f.workspaces),
            &listing,
            "bare",
            &interactive(),
        );
        let ask = ask_of(&l);
        assert_eq!(ask.rows[0].project, "site");
        assert!(ask.rows[0].default);
        assert!(!ask.rows[0].continue_in);
        assert_eq!(ask.rows[0].label(), "site");
    }

    /// T2: the rest are by newest thread id per project, never-used last
    /// by name.
    #[test]
    fn t2_c_the_rest_are_by_newest_thread_id_never_used_last_by_name() {
        let f = fixture();
        let bare = TempDir::new().unwrap();
        let mut projects = known(&f.workspaces);
        projects.push(proj("aaa", &bare.path().join("aaa")));
        // getscale: web used 60s ago, api never. vendela: site used
        // 600s ago. `other`: aaa used 30s ago.
        let listing = vec![thread("web", 60), thread("site", 600), thread("aaa", 30)];
        let l = ladder(
            bare.path(),
            &f.workspaces,
            &projects,
            &listing,
            "bare",
            &interactive(),
        );
        let ask = ask_of(&l);
        // Row 1 is the newest used (aaa, 30s), its group `other` leads.
        assert_eq!(ask.rows[0].project, "aaa");
        assert_eq!(
            ask.rows
                .iter()
                .map(|r| r.project.as_str())
                .collect::<Vec<_>>(),
            vec!["aaa", "web", "site", "api"]
        );
        // Within getscale: web was used, api never.
        let getscale: Vec<&Row> = ask
            .rows
            .iter()
            .filter(|r| r.workspace.as_deref() == Some("getscale"))
            .collect();
        assert_eq!(getscale[0].project, "web");
        assert!(getscale[1].last.is_none());
    }

    /// T2: the page is grouped by workspace, `other` last, and the
    /// default's group leads it.
    #[test]
    fn t2_d_groups_are_by_workspace_with_other_last() {
        let f = fixture();
        let bare = TempDir::new().unwrap();
        let listing = vec![thread("web", 600), thread("site", 30)];
        let l = ladder(
            bare.path(),
            &f.workspaces,
            &known(&f.workspaces),
            &listing,
            "bare",
            &interactive(),
        );
        let ask = ask_of(&l);
        // site is the newest used, so vendela leads.
        assert_eq!(ask.rows[0].project, "site");
        assert_eq!(
            ask.rows
                .iter()
                .map(|r| r.workspace.as_deref().unwrap_or("other"))
                .collect::<Vec<_>>(),
            vec!["vendela", "getscale", "getscale"]
        );
    }

    /// T2: every row is shown, numbered `1..n`, with no paging row; the
    /// note is `last thread <elapsed>` or `no threads yet`.
    #[test]
    fn t2_e_every_row_is_shown_numbered_with_its_note() {
        let f = fixture();
        let bare = TempDir::new().unwrap();
        let listing = vec![thread("web", 7200)];
        let l = ladder(
            bare.path(),
            &f.workspaces,
            &known(&f.workspaces),
            &listing,
            "bare",
            &interactive(),
        );
        let ask = ask_of(&l);
        let text = ask.render(now());
        for (i, row) in ask.rows.iter().enumerate() {
            assert!(
                text.contains(&format!("{}", i + 1)),
                "row {} is numbered:\n{text}",
                i + 1
            );
            assert!(text.contains(&row.project), "{text}");
        }
        assert!(!text.contains("more…"), "no paging row:\n{text}");
        assert!(text.starts_with("Which project? (Enter = 1)"), "{text}");
        assert!(text.contains("getscale\n"), "{text}");
        assert!(text.contains("vendela\n"), "{text}");
    }

    /// T2: the note is the newest thread id's timestamp through
    /// `elapsed_short`'s shape.
    #[test]
    fn t2_f_the_note_is_the_newest_ids_timestamp() {
        let f = fixture();
        let bare = TempDir::new().unwrap();
        // Two threads in web: the newest id wins.
        let listing = vec![thread("web", 90_000), thread("web", 7200)];
        let l = ladder(
            bare.path(),
            &f.workspaces,
            &known(&f.workspaces),
            &listing,
            "bare",
            &interactive(),
        );
        let ask = ask_of(&l);
        let web = ask.rows.iter().find(|r| r.project == "web").unwrap();
        assert_eq!(
            web.note(now()),
            format!("· last thread {}", elapsed_short(Duration::from_secs(7200)))
        );
        let api = ask.rows.iter().find(|r| r.project == "api").unwrap();
        assert_eq!(api.note(now()), "· no threads yet");
    }

    /// T2 (issue #121, finding 1): the unreachable project is one dim
    /// line under its workspace, and answering for it re-asks with that
    /// reason — it never selects the workspace that took the name.
    #[test]
    fn t2_g_a_same_named_project_is_one_dim_line_and_its_name_re_asks() {
        let f = fixture();
        let bare = TempDir::new().unwrap();
        let l = ladder(
            bare.path(),
            &f.workspaces,
            &known(&f.workspaces),
            &[],
            "bare",
            &interactive(),
        );
        let ask = ask_of(&l);
        let text = ask.render(now());
        assert!(
            text.contains(
                "not reachable: its name is also getscale's web; \
                 rename one in its aigentic.toml"
            ),
            "{text}"
        );
        let site = ask.rows.iter().position(|r| r.project == "site").unwrap();
        assert_ne!(ask.rows[site].project, "web");
        // `vendela/web` is not a row: the ask gives the reason and
        // re-asks. The next answer takes its own row.
        let mut scripted = Scripted::new(&["vendela/web", &format!("{}", site + 1)]);
        let out = choose(ask, &mut scripted, now()).unwrap();
        match out {
            Prompting::Answered(a) => {
                assert_eq!(a.chosen, ask.rows[site].project);
                assert_ne!(a.chosen, "web", "never the other workspace's `web`");
            }
            other => panic!("expected an answer, got {other:?}"),
        }
        assert!(
            scripted
                .shown
                .iter()
                .any(|s| s.contains("not reachable: its name is also getscale's web")),
            "the re-ask carries the reason: {:?}",
            scripted.shown
        );
    }

    // ---- T3: the prompt ----

    struct Scripted {
        answers: VecDeque<String>,
        shown: Vec<String>,
    }

    impl Scripted {
        fn new(answers: &[&str]) -> Self {
            Scripted {
                answers: answers.iter().map(|a| (*a).to_owned()).collect(),
                shown: Vec::new(),
            }
        }
    }

    impl Asker for Scripted {
        fn show(&mut self, text: &str) {
            self.shown.push(text.to_owned());
        }
        fn ask(&mut self, _prompt: &str, default: &str) -> anyhow::Result<String> {
            Ok(self
                .answers
                .pop_front()
                .unwrap_or_else(|| default.to_owned()))
        }
        fn confirm(&mut self, _prompt: &str, _default: bool) -> anyhow::Result<bool> {
            Ok(false)
        }
    }

    /// The ask the T3 cases answer: getscale (web used 2h ago, api never)
    /// and vendela (site used 3 days ago).
    fn prompt_fixture() -> Ask {
        let f = fixture();
        let bare = TempDir::new().unwrap();
        let listing = vec![thread("web", 7200), thread("site", 260_000)];
        let l = ladder(
            bare.path(),
            &f.workspaces,
            &known(&f.workspaces),
            &listing,
            "bare",
            &interactive(),
        );
        ask_of(&l).clone()
    }

    /// T3: Enter takes row 1.
    #[test]
    fn t3_a_enter_takes_row_one() {
        let ask = prompt_fixture();
        let mut scripted = Scripted::new(&[""]);
        let out = choose(&ask, &mut scripted, now()).unwrap();
        match out {
            Prompting::Answered(a) => {
                assert_eq!(a.offered, ask.rows[0].project);
                assert_eq!(a.chosen, ask.rows[0].project);
            }
            other => panic!("expected an answer, got {other:?}"),
        }
        assert!(scripted.shown[0].contains("Which project? (Enter = 1)"));
    }

    /// T3: a number takes its row.
    #[test]
    fn t3_b_a_number_takes_its_row() {
        let ask = prompt_fixture();
        let mut scripted = Scripted::new(&["3"]);
        let out = choose(&ask, &mut scripted, now()).unwrap();
        match out {
            Prompting::Answered(a) => assert_eq!(a.chosen, ask.rows[2].project),
            other => panic!("expected an answer, got {other:?}"),
        }
    }

    /// T3: a project name resolves, and `<workspace>/<project>` resolves
    /// a name that repeats.
    #[test]
    fn t3_c_a_project_name_and_a_workspace_slash_project_resolve() {
        let ask = prompt_fixture();
        let mut scripted = Scripted::new(&["site"]);
        let out = choose(&ask, &mut scripted, now()).unwrap();
        match out {
            Prompting::Answered(a) => assert_eq!(a.chosen, "site"),
            other => panic!("expected an answer, got {other:?}"),
        }
        let mut scripted = Scripted::new(&["getscale/api"]);
        let out = choose(&ask, &mut scripted, now()).unwrap();
        match out {
            Prompting::Answered(a) => assert_eq!(a.chosen, "api"),
            other => panic!("expected an answer, got {other:?}"),
        }
    }

    /// T3: an unknown or ambiguous answer re-asks once, then takes row 1
    /// and says so.
    #[test]
    fn t3_d_a_bad_answer_re_asks_once_then_takes_row_one() {
        let ask = prompt_fixture();
        // `99` is past the last row, `nosuch` is no project.
        for bad in ["99", "nosuch"] {
            let mut scripted = Scripted::new(&[bad, bad]);
            let out = choose(&ask, &mut scripted, now()).unwrap();
            match out {
                Prompting::Answered(a) => assert_eq!(a.chosen, ask.rows[0].project),
                other => panic!("expected an answer, got {other:?}"),
            }
            assert_eq!(scripted.answers.len(), 0, "one re-ask, not two: {bad}");
            assert!(
                scripted.shown.iter().any(|s| s.contains("[taking 1")),
                "it says so: {:?}",
                scripted.shown
            );
        }
    }

    /// T3: an ambiguous name re-asks too: two rows share it.
    #[test]
    fn t3_e_an_ambiguous_name_re_asks_once() {
        let ask = Ask {
            rows: vec![
                Row {
                    project: "web".to_owned(),
                    workspace: Some("one".to_owned()),
                    last: None,
                    default: true,
                    continue_in: false,
                },
                Row {
                    project: "web".to_owned(),
                    workspace: Some("two".to_owned()),
                    last: None,
                    default: false,
                    continue_in: false,
                },
            ],
            shadowed: Vec::new(),
            reason: "test".to_owned(),
        };
        let mut scripted = Scripted::new(&["web"]);
        let out = choose(&ask, &mut scripted, now()).unwrap();
        match out {
            Prompting::Answered(a) => assert_eq!(a.chosen, "web"),
            other => panic!("expected an answer, got {other:?}"),
        }
        assert!(
            scripted
                .shown
                .iter()
                .any(|s| s.contains("not one of the rows"))
        );
        // The typed form says which one.
        let mut scripted = Scripted::new(&["two/web"]);
        let out = choose(&ask, &mut scripted, now()).unwrap();
        match out {
            Prompting::Answered(a) => assert_eq!(a.chosen, "web"),
            other => panic!("expected an answer, got {other:?}"),
        }
    }

    /// T3: `q` quits with no thread opened.
    #[test]
    fn t3_f_q_quits() {
        let ask = prompt_fixture();
        let mut scripted = Scripted::new(&["q"]);
        assert_eq!(choose(&ask, &mut scripted, now()).unwrap(), Prompting::Quit);
    }

    // ---- T6: exec ----

    /// T6: in a workspace's folder, exec refuses and names that
    /// workspace's projects first.
    #[test]
    fn t6_a_exec_in_a_workspace_folder_names_its_projects_first() {
        let f = fixture();
        let known = vec!["site".to_owned(), "web".to_owned()];
        let message = exec_guard(&f.shared, &f.workspaces, &known, false, false).unwrap();
        assert!(message.starts_with("exec needs a project: pass --project (getscale: web, api"));
        assert!(message.contains("other: site"), "{message}");
    }

    /// T6: in a project, and with a project or thread named, exec is
    /// never refused — and behind a pipe there is nothing to prompt.
    #[test]
    fn t6_b_exec_never_prompts() {
        let f = fixture();
        let known = vec!["site".to_owned()];
        assert_eq!(
            exec_guard(&f.web, &f.workspaces, &known, false, false),
            None
        );
        assert_eq!(
            exec_guard(&f.shared, &f.workspaces, &known, true, false),
            None
        );
        let mut l = interactive();
        l.exec = true;
        l.interactive = false;
        assert_eq!(
            ladder(&f.shared, &f.workspaces, &[], &[], "getscale", &l),
            Ladder::None
        );
    }
}
