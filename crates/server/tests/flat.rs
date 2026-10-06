//! The move to one flat threads directory (issue #9): `migrate` renames
//! every `<base>/<project>/<id>.jsonl` to `<base>/<id>.jsonl`, leaves a
//! log another process is driving where it is, and never rewrites a
//! byte. Nothing here touches a real threads directory: every base is a
//! `tempdir`.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use aigentic_api::client::{Addr, Client};
use aigentic_api::{Request, Response};
use aigentic_runtime::aigentic_core::{
    Author, Capabilities, CompletionRequest, ContentBlock, EventKind, Message, Provider,
    ProviderEvent, UserId,
};
use aigentic_runtime::aigentic_log::{
    NewEvent, ProjectSwitchedPayload, RunStartedPayload, ThreadLog, ThreadStartedPayload,
    UserMessagePayload,
};
use aigentic_runtime::runner::RunnerHost;
use aigentic_server::build::{BuildError, ProviderFactory};
use aigentic_server::config::{Config, ProjectConfig, ServerConfig, UserConfig};
use aigentic_server::migrate::migrate;
use aigentic_server::runs::{IssueLock, ServerHost};
use aigentic_server::threads::project_name_at;
use aigentic_server::workspaces::Workspace;
use aigentic_server::{Listener, NoReports, Server};
use futures_core::Stream;
use serde_json::to_value;
use ulid::Ulid;

/// Write `<dir>/<id>.jsonl` holding `body`, making `dir` first.
fn write_log(dir: &Path, id: Ulid, body: &str) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join(format!("{id}.jsonl"));
    std::fs::write(&path, body).unwrap();
    path
}

/// A log's body, so the test can compare bytes across the move.
fn body(id: Ulid) -> String {
    format!("{{\"kind\":\"thread_started\",\"id\":\"{id}\"}}\n")
}

/// Hold `path` — creating it — the way a running build does, so
/// `migrate` sees `WouldBlock`.
fn hold(path: &Path) -> File {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let file = File::options()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
        .unwrap();
    file.try_lock().expect("the lock is free");
    file
}

fn flat(base: &Path, id: Ulid) -> PathBuf {
    base.join(format!("{id}.jsonl"))
}

#[test]
fn migrate_moves_legacy_logs_flat_and_leaves_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("threads");
    std::fs::create_dir_all(&base).unwrap();

    let a = Ulid::generate();
    let b = Ulid::generate();
    let c = Ulid::generate();
    let d = Ulid::generate();
    let e = Ulid::generate();
    let f = Ulid::generate();

    let p = base.join("p");
    let q = base.join("q");
    let none = base.join("_none");
    let hidden = base.join(".hidden");

    write_log(&p, a, &body(a));
    let b_legacy = write_log(&p, b, &body(b));
    // b is a lead someone drives: its lock file is there, free.
    std::fs::write(p.join(format!("{b}.lock")), b"lock\n").unwrap();
    write_log(&q, c, &body(c));
    write_log(&none, d, &body(d));
    // A pre-phase-4 log, already flat.
    let e_flat = write_log(&base, e, &body(e));
    let f_legacy = write_log(&hidden, f, &body(f));
    // Not a log, and not ours to touch.
    std::fs::write(p.join("issue-7.lock"), b"issue\n").unwrap();

    let before_b = std::fs::read(&b_legacy).unwrap();
    let before_e = std::fs::read(&e_flat).unwrap();
    let before_f = std::fs::read(&f_legacy).unwrap();

    let out = migrate(&base).unwrap();
    assert_eq!(out.moved, 4, "a, b, c and d move: {out:?}");
    assert!(out.held.is_empty(), "{out:?}");
    assert!(out.clashes.is_empty(), "{out:?}");

    for id in [a, b, c, d] {
        assert!(flat(&base, id).is_file(), "{id} is flat");
    }
    assert!(!b_legacy.exists(), "b's legacy log is gone");
    assert!(
        !p.join(format!("{b}.lock")).exists(),
        "b's lock goes with it"
    );
    assert!(!q.join(format!("{c}.jsonl")).exists());
    assert!(!none.join(format!("{d}.jsonl")).exists());
    // The bytes are the same bytes.
    assert_eq!(std::fs::read(flat(&base, b)).unwrap(), before_b);
    // Untouched: another project's issue lock, a dot-named directory, a
    // flat log that had no legacy file.
    assert_eq!(std::fs::read(p.join("issue-7.lock")).unwrap(), b"issue\n");
    assert_eq!(std::fs::read(&f_legacy).unwrap(), before_f);
    assert_eq!(std::fs::read(&e_flat).unwrap(), before_e);

    // Idempotent: nothing is left to move.
    let again = migrate(&base).unwrap();
    assert_eq!(again.moved, 0, "{again:?}");

    // A base that doesn't exist is nothing to do, not an error.
    let missing = migrate(&dir.path().join("nope")).unwrap();
    assert_eq!(missing.moved, 0, "{missing:?}");
    assert!(missing.held.is_empty() && missing.clashes.is_empty());
}

#[test]
fn migrate_holds_a_driven_lead_and_skips_a_clash() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("threads");
    std::fs::create_dir_all(&base).unwrap();

    let b = Ulid::generate();
    let c = Ulid::generate();

    let p = base.join("p");
    let q = base.join("q");
    let b_legacy = write_log(&p, b, &body(b));
    let b_lock = p.join(format!("{b}.lock"));
    let held = hold(&b_lock);
    // c has a legacy log and a flat one: a clash.
    let c_legacy = write_log(&q, c, &body(c));
    let c_flat = write_log(&base, c, &body(c));

    let out = migrate(&base).unwrap();
    assert_eq!(out.moved, 0, "{out:?}");
    assert_eq!(out.held, vec![b], "{out:?}");
    assert_eq!(out.clashes, vec![c], "{out:?}");
    // b stays where it is, log and lock both.
    assert!(b_legacy.is_file());
    assert!(!flat(&base, b).exists());
    assert!(b_lock.is_file());
    // The flat log wins and the legacy file is left in place.
    assert!(c_flat.is_file());
    assert!(c_legacy.is_file());

    // Release it, as the driven build ending would, and the next start
    // moves b: a held lead is moved by a later daemon start.
    held.unlock().unwrap();
    drop(held);
    let after = migrate(&base).unwrap();
    assert_eq!(after.moved, 1, "{after:?}");
    assert!(after.held.is_empty(), "{after:?}");
    assert!(flat(&base, b).is_file());
    assert!(!b_legacy.exists());
    assert!(!b_lock.exists());
}

/// #9 review, item 1: a lead's step children have no lock of their own
/// and are written beside it, so a held lock anywhere in a directory —
/// a lead's or an issue's — leaves the whole directory, or a live child
/// would be moved and recreated at the old path, split in two.
#[test]
fn migrate_leaves_a_whole_directory_while_any_lock_in_it_is_held() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("threads");
    std::fs::create_dir_all(&base).unwrap();

    let p = base.join("p");
    let q = base.join("q");
    let r = base.join("r");
    let lead = Ulid::generate();
    let child = Ulid::generate();
    let plain = Ulid::generate();
    let starting = Ulid::generate();
    let lead_legacy = write_log(&p, lead, &body(lead));
    let child_legacy = write_log(&p, child, &body(child));
    let lead_lock = hold(&p.join(format!("{lead}.lock")));
    write_log(&q, plain, &body(plain));
    write_log(&r, starting, &body(starting));
    let issue_lock = hold(&r.join("issue-7.lock"));

    let out = migrate(&base).unwrap();
    assert_eq!(out.moved, 1, "only q's log moves: {out:?}");
    let mut held = out.held.clone();
    held.sort();
    let mut expected = vec![lead, child, starting];
    expected.sort();
    assert_eq!(held, expected, "{out:?}");
    assert!(lead_legacy.is_file() && child_legacy.is_file());
    assert!(
        !flat(&base, child).exists(),
        "the unlocked child stays beside its lead"
    );
    assert!(r.join(format!("{starting}.jsonl")).is_file());
    assert!(flat(&base, plain).is_file());

    // Both builds end: the next start moves the rest.
    drop(lead_lock);
    drop(issue_lock);
    let after = migrate(&base).unwrap();
    assert_eq!(after.moved, 3, "{after:?}");
    assert!(after.held.is_empty(), "{after:?}");
    for id in [lead, child, starting] {
        assert!(flat(&base, id).is_file());
    }
    assert!(
        !p.join(format!("{lead}.lock")).exists(),
        "the lead's lock went with it"
    );
    assert!(
        r.join("issue-7.lock").is_file(),
        "an issue lock is never touched"
    );
}

#[test]
fn two_migrations_at_once_move_every_log_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("threads");
    std::fs::create_dir_all(&base).unwrap();

    let mut ids = Vec::new();
    for i in 0..50 {
        let id = Ulid::generate();
        let sub = base.join(format!("p{}", i % 5));
        write_log(&sub, id, &body(id));
        ids.push(id);
    }

    let one = base.clone();
    let two = base.clone();
    let t1 = std::thread::spawn(move || migrate(&one).unwrap().moved);
    let t2 = std::thread::spawn(move || migrate(&two).unwrap().moved);
    let (m1, m2) = (t1.join().unwrap(), t2.join().unwrap());
    assert_eq!(m1 + m2, 50, "{m1} + {m2}");
    for id in ids {
        assert!(flat(&base, id).is_file(), "{id} is flat");
    }
    for i in 0..5 {
        let sub = base.join(format!("p{i}"));
        assert!(
            std::fs::read_dir(&sub).unwrap().next().is_none(),
            "p{i} kept something"
        );
    }
}

// ---------------------------------------------------------------------------
// T4–T9: the index, the directories, the listings and the runs (#9)
// ---------------------------------------------------------------------------

/// A project folder `dir/name`: its file (with `participants` as
/// written), its instructions, its root.
fn project(dir: &Path, name: &str, participants: &str) -> PathBuf {
    let root = dir.join(name);
    std::fs::create_dir_all(root.join(".aigentic")).unwrap();
    std::fs::write(
        root.join("aigentic.toml"),
        format!("[project]\nname = \"{name}\"\n{participants}[memory]\nenabled = false\n"),
    )
    .unwrap();
    std::fs::write(
        root.join(".aigentic/instructions.md"),
        format!("This is {name}."),
    )
    .unwrap();
    root
}

/// A project config row.
fn pc(name: &str, root: &Path) -> ProjectConfig {
    ProjectConfig {
        name: name.to_owned(),
        root: root.to_path_buf(),
    }
}

/// A provider that answers every call with `ok`, so nothing a test does
/// by accident hangs on a model.
struct AnswersOk;

impl Provider for AnswersOk {
    fn complete(
        &self,
        _: &CompletionRequest<'_>,
    ) -> Pin<Box<dyn Stream<Item = ProviderEvent> + Send + '_>> {
        Box::pin(futures_util::stream::iter(vec![
            ProviderEvent::TextDelta("ok".into()),
            ProviderEvent::Done {
                finish_reason: "stop".into(),
            },
        ]))
    }
    fn count_tokens(&self, _: &[Message]) -> u64 {
        7
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supports_tools: false,
            supports_images: false,
            supports_caching: false,
            supports_structured_output: false,
            max_context_tokens: 100_000,
        }
    }
}

struct OkFactory;

impl ProviderFactory for OkFactory {
    fn build(&self, _: &str) -> Result<(Box<dyn Provider>, String), BuildError> {
        Ok((Box::new(AnswersOk), "scripted".into()))
    }
}

/// A daemon over a temp directory the caller holds: `<dir>/threads` is
/// its threads base, so a test can write logs into it before the daemon
/// starts.
struct Daemon {
    server: Arc<Server>,
    base: PathBuf,
    socket: PathBuf,
}

impl Daemon {
    /// A daemon whose `server.toml` names `projects` and whose users are
    /// `users` (the first owns every project). With `bundled`, `dir` is
    /// also the bundled directory, so a `Build` finds a workflow there.
    async fn new(dir: &Path, projects: Vec<ProjectConfig>, users: &[&str], bundled: bool) -> Self {
        let dir = dir.to_path_buf();
        let cfg_dir = dir.join("cfg");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let base = dir.join("threads");
        if bundled {
            write_test_workflow(&dir);
        }
        let bundled_dir = if bundled {
            format!("bundled_dir = {:?}\n", dir.display().to_string())
        } else {
            String::new()
        };
        let config = Config::parse(&format!(
            "threads_dir = {:?}\n{bundled_dir}[profiles.a]\nbase_url = \"u\"\nmodel = \"m\"\napi_key_env = \"K\"\n",
            base.display()
        ))
        .unwrap();
        let server_cfg = ServerConfig {
            listen: "unix".into(),
            idle_unload_secs: 3600,
            users: users
                .iter()
                .map(|n| UserConfig {
                    name: (*n).to_owned(),
                    token_env: None,
                    token: Some(format!("tok-{n}")),
                })
                .collect(),
            projects,
            resume_runs: false,
        };
        let server = Arc::new(Server::new(
            config,
            cfg_dir,
            server_cfg,
            Arc::new(OkFactory),
            Arc::new(NoReports),
        ));
        let socket = dir.join("d.sock");
        tokio::spawn(server.clone().serve(Listener::Unix(socket.clone())));
        for _ in 0..400 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        Self {
            server,
            base,
            socket,
        }
    }

    async fn connect(&self, user: &str) -> Client {
        Client::connect(&Addr::Unix(self.socket.clone()), &format!("tok-{user}"))
            .await
            .unwrap()
            .0
    }

    fn threads(&self) -> &aigentic_server::threads::ThreadTable {
        &self.server.threads
    }

    fn base(&self) -> &Path {
        &self.base
    }
}

/// Write the bundled `build` workflow, cut to the checks that need no
/// gate, so a `Build` in a test finds one.
fn write_test_workflow(bundled: &Path) {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../workflows/build");
    let dest = bundled.join("workflows").join("build");
    std::fs::create_dir_all(dest.join("templates")).unwrap();
    let toml = std::fs::read_to_string(source.join("workflow.toml")).unwrap();
    let toml = toml.replace(
        "checks = [\"E1\", \"E2\", \"E3\", \"E4\", \"E5\", \"E7\"]",
        "checks = [\"E1\", \"E2\", \"E3\"]",
    );
    assert!(
        !toml.contains("\"E4\""),
        "the test workflow never runs the gate"
    );
    std::fs::write(dest.join("workflow.toml"), toml).unwrap();
    for template in ["brief.md", "implementer.md"] {
        std::fs::copy(
            source.join("templates").join(template),
            dest.join("templates").join(template),
        )
        .unwrap();
    }
}

/// Create a thread in `project` and open it.
async fn created(client: &mut Client, project: &str) -> Ulid {
    let Response::Thread { thread } = client
        .request(Request::CreateThread {
            project: project.into(),
        })
        .await
        .unwrap()
    else {
        panic!("a thread")
    };
    assert!(matches!(
        client
            .request(Request::Open {
                thread: thread.id,
                from_seq: 0
            })
            .await
            .unwrap(),
        Response::Opened { .. }
    ));
    thread.id
}

/// Move a thread and wait for the daemon to have taken the move.
async fn switch(client: &mut Client, thread: Ulid, project: &str) {
    assert_eq!(
        client
            .request(Request::SwitchProject {
                thread,
                project: project.into(),
            })
            .await
            .unwrap(),
        Response::Ok
    );
}

/// Close a thread and unload it, so its log is all the daemon has left
/// of it: what the log says has to be enough.
async fn unload(client: &mut Client, daemon: &Daemon, thread: Ulid) {
    assert_eq!(
        client.request(Request::Close { thread }).await.unwrap(),
        Response::Ok
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if daemon
            .threads()
            .sweep(Duration::ZERO)
            .await
            .contains(&thread)
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("the thread unloads");
}

/// Open a thread, answered or refused.
async fn open(client: &mut Client, thread: Ulid) -> Response {
    client
        .request(Request::Open {
            thread,
            from_seq: 0,
        })
        .await
        .unwrap()
}

/// A hand-written log at `<maybe sub>/<id>.jsonl`: a layout the daemon
/// cannot have written itself, or a lead the migration had to hold.
fn hand_log(base: &Path, sub: Option<&str>, id: Ulid) -> ThreadLog {
    let dir = match sub {
        Some(sub) => base.join(sub),
        None => base.to_path_buf(),
    };
    std::fs::create_dir_all(&dir).unwrap();
    ThreadLog::open(&dir, id).unwrap()
}

/// The `thread_started` line of a hand-written thread.
fn append_started(log: &mut ThreadLog, project: Option<&str>, root: &Path, by: &str) {
    log.append(NewEvent {
        kind: EventKind::ThreadStarted,
        author: Author::User(UserId(by.to_owned())),
        payload: to_value(ThreadStartedPayload {
            project: project.map(str::to_owned),
            root: root.to_path_buf(),
            created_by: Author::User(UserId(by.to_owned())),
            parent_thread: None,
            step: None,
            // never a front thread (issue #84)
            front: false,
        })
        .unwrap(),
        parent_event: None,
    })
    .unwrap();
}

/// A hand-written `project_switched`.
fn append_switch(log: &mut ThreadLog, to: &str, root: &Path) {
    log.append(NewEvent {
        kind: EventKind::ProjectSwitched,
        author: Author::System,
        payload: to_value(ProjectSwitchedPayload {
            from: None,
            to: Some(to.to_owned()),
            root: root.to_path_buf(),
            workspace: None,
        })
        .unwrap(),
        parent_event: None,
    })
    .unwrap();
}

/// A hand-written `run_started`.
fn append_run_started(log: &mut ThreadLog, issue: u64) {
    log.append(NewEvent {
        kind: EventKind::RunStarted,
        author: Author::System,
        payload: to_value(RunStartedPayload {
            issue,
            workflow: "build".into(),
            version: 1,
            content_hash: "x".into(),
            budget_usd: 3.0,
        })
        .unwrap(),
        parent_event: None,
    })
    .unwrap();
}

/// A hand-written `user_message`: a pre-phase-4 log's only line.
fn append_user_message(log: &mut ThreadLog, text: &str) {
    log.append(NewEvent {
        kind: EventKind::UserMessage,
        author: Author::User(UserId("steve".into())),
        payload: to_value(UserMessagePayload::new(vec![ContentBlock::Text(
            text.into(),
        )]))
        .unwrap(),
        parent_event: None,
    })
    .unwrap();
}

/// T4 — the index answers `project_of` from the log, and a miss is a
/// miss, not "no thread".
#[tokio::test]
async fn t4_the_index_says_which_project_a_thread_is_in() {
    let dir = tempfile::tempdir().unwrap();
    let p = project(dir.path(), "p", "");
    let q = project(dir.path(), "q", "");
    // A bare root named `x`, and one whose basename is a project's.
    let x = dir.path().join("x");
    std::fs::create_dir_all(&x).unwrap();
    let other_p = dir.path().join("other").join("p");
    std::fs::create_dir_all(&other_p).unwrap();
    let base = dir.path().join("threads");

    // Written by hand, before the daemon: the legacy layout, which the
    // start-up migration moves flat.
    let plain = Ulid::generate();
    let mut log = hand_log(&base, Some("p"), plain);
    append_started(&mut log, Some("p"), &p, "steve");
    let switched = Ulid::generate();
    let mut log = hand_log(&base, Some("q"), switched);
    append_started(&mut log, Some("q"), &q, "steve");
    append_switch(&mut log, "p", &p);
    let unknown = Ulid::generate();
    let mut log = hand_log(&base, Some("p"), unknown);
    append_started(&mut log, Some("p"), &p, "steve");
    append_switch(&mut log, "zzz", &dir.path().join("zzz"));
    // `_none`, rooted at `x`: `x` is its project.
    let none_x = Ulid::generate();
    let mut log = hand_log(&base, Some("_none"), none_x);
    append_started(&mut log, Some("_none"), &x, "steve");
    // `_none`, rooted at another `p`: no project at all.
    let clash = Ulid::generate();
    let mut log = hand_log(&base, Some("_none"), clash);
    append_started(&mut log, Some("_none"), &other_p, "steve");
    // A pre-phase-4 log, flat, with no `thread_started`.
    let pre4 = Ulid::generate();
    let mut log = hand_log(&base, None, pre4);
    append_user_message(&mut log, "hi");

    let daemon = Daemon::new(
        dir.path(),
        vec![pc("p", &p), pc("q", &q)],
        &["steve"],
        false,
    )
    .await;
    let t = daemon.threads();

    assert_eq!(t.project_of(plain).as_deref(), Some("p"));
    assert_eq!(t.project_of(switched).as_deref(), Some("p"));
    assert_eq!(
        t.project_of(unknown).as_deref(),
        Some("p"),
        "a switch this daemon doesn't know falls back to home"
    );
    assert_eq!(t.project_of(none_x).as_deref(), Some("x"));
    assert_eq!(
        t.project_of(clash),
        None,
        "a `_none` root whose basename is another project's"
    );
    assert_eq!(t.project_of(pre4), None, "a pre-phase-4 log has no project");
    // The migration ran before the table was built: p's log is flat now.
    assert!(daemon.base().join(format!("{plain}.jsonl")).is_file());
    assert!(
        !daemon
            .base()
            .join("p")
            .join(format!("{plain}.jsonl"))
            .exists(),
        "the legacy file moved"
    );

    // A miss is a miss: both threads are written after the daemon is up.
    let flat_new = Ulid::generate();
    let mut log = hand_log(&base, None, flat_new);
    append_started(&mut log, Some("q"), &q, "steve");
    let legacy_new = Ulid::generate();
    let mut log = hand_log(&base, Some("p"), legacy_new);
    append_started(&mut log, Some("p"), &p, "steve");
    assert_eq!(t.project_of(flat_new).as_deref(), Some("q"));
    assert_eq!(t.project_of(legacy_new).as_deref(), Some("p"));
    assert_eq!(t.dir_of(flat_new), Some(daemon.base().to_path_buf()));
    assert_eq!(t.dir_of(legacy_new), Some(daemon.base().join("p")));
}

/// T5 — roles follow the log, not the directory the log used to sit in.
#[tokio::test]
async fn t5_roles_follow_the_log_across_a_switch() {
    let dir = tempfile::tempdir().unwrap();
    let p = project(
        dir.path(),
        "p",
        "[participants]\nanna = \"write\"\ncara = \"write\"\n",
    );
    let q = project(
        dir.path(),
        "q",
        "[participants]\nanna = \"write\"\nbo = \"write\"\n",
    );
    let daemon = Daemon::new(
        dir.path(),
        vec![pc("p", &p), pc("q", &q)],
        &["steve", "anna", "cara", "bo"],
        false,
    )
    .await;
    let mut anna = daemon.connect("anna").await;
    let mut cara = daemon.connect("cara").await;
    let mut bo = daemon.connect("bo").await;

    let id = created(&mut anna, "p").await;
    switch(&mut anna, id, "q").await;
    unload(&mut anna, &daemon, id).await;

    // `bo` holds a role only in q, where the log says the thread is now.
    assert!(matches!(open(&mut bo, id).await, Response::Opened { .. }));
    // `cara` holds one only in p, and the thread is not in p any more.
    let refused = open(&mut cara, id).await;
    assert!(
        matches!(refused, Response::Refused { .. }),
        "cara holds no role in the thread's project: {refused:?}"
    );

    // A pre-phase-4 log and a clashing `_none` thread get `project_for`'s
    // refusal, not the role's: there is no project to check a role in.
    let other_p = dir.path().join("other").join("p");
    std::fs::create_dir_all(&other_p).unwrap();
    let pre4 = Ulid::generate();
    let mut log = hand_log(daemon.base(), None, pre4);
    append_user_message(&mut log, "hi");
    let clash = Ulid::generate();
    let mut log = hand_log(daemon.base(), None, clash);
    append_started(&mut log, Some("_none"), &other_p, "steve");
    for thread in [pre4, clash] {
        let r = open(&mut anna, thread).await;
        assert!(
            matches!(&r, Response::Refused { reason }
                if reason == "no such thread or project on this daemon"),
            "{r:?}"
        );
    }
}

/// T6 — listings and counts come from the log.
#[tokio::test]
async fn t6_listings_and_counts_follow_the_log() {
    let dir = tempfile::tempdir().unwrap();
    let p = project(dir.path(), "p", "");
    let q = project(dir.path(), "q", "");
    let daemon = Daemon::new(
        dir.path(),
        vec![pc("p", &p), pc("q", &q)],
        &["steve"],
        false,
    )
    .await;
    let mut steve = daemon.connect("steve").await;

    let stayed = created(&mut steve, "p").await;
    let moved = created(&mut steve, "p").await;
    switch(&mut steve, moved, "q").await;
    unload(&mut steve, &daemon, moved).await;
    unload(&mut steve, &daemon, stayed).await;

    // A thread whose project is no project of this daemon: in no row.
    let stranger = Ulid::generate();
    let mut log = hand_log(daemon.base(), None, stranger);
    append_started(&mut log, Some("zzz"), dir.path(), "steve");

    let holds = |threads: &[aigentic_api::ThreadInfo], id: Ulid| threads.iter().any(|t| t.id == id);
    let Response::Threads { threads } = steve
        .request(Request::ListThreads {
            project: Some("p".into()),
        })
        .await
        .unwrap()
    else {
        panic!("threads")
    };
    assert!(holds(&threads, stayed));
    assert!(!holds(&threads, moved), "the switched thread left p");

    let Response::Threads { threads } = steve
        .request(Request::ListThreads {
            project: Some("q".into()),
        })
        .await
        .unwrap()
    else {
        panic!("threads")
    };
    assert!(holds(&threads, moved), "the switched thread is in q");

    let Response::Projects { projects } = steve.request(Request::ListProjects).await.unwrap()
    else {
        panic!("projects")
    };
    let rows: Vec<(&str, u64)> = projects
        .iter()
        .map(|p| (p.name.as_str(), p.threads))
        .collect();
    assert_eq!(
        rows,
        vec![("p", 1), ("q", 1)],
        "one thread each, and the stranger counts nowhere"
    );
    assert_eq!(daemon.threads().projects().len(), 2);

    // A new thread is written flat: no per-project directory at all.
    let fresh = created(&mut steve, "p").await;
    assert!(daemon.base().join(format!("{fresh}.jsonl")).is_file());
    assert!(
        !daemon.base().join("p").exists(),
        "no per-project directory is made"
    );
}

/// #9 review, item 3: a thread another daemon made after this one
/// started — two terminals, two embedded daemons — is listed and counted,
/// written flat or under a legacy directory.
#[tokio::test]
async fn a_thread_made_behind_the_daemon_is_listed_and_counted() {
    let dir = tempfile::tempdir().unwrap();
    let p = project(dir.path(), "p", "");
    let q = project(dir.path(), "q", "");
    let daemon = Daemon::new(
        dir.path(),
        vec![pc("p", &p), pc("q", &q)],
        &["steve"],
        false,
    )
    .await;
    let mut steve = daemon.connect("steve").await;
    let mine = created(&mut steve, "p").await;

    // Written after the daemon was built, so not in its index yet.
    let flat_one = Ulid::generate();
    let mut log = hand_log(daemon.base(), None, flat_one);
    append_started(&mut log, Some("p"), &p, "steve");
    let legacy_one = Ulid::generate();
    let mut log = hand_log(daemon.base(), Some("p"), legacy_one);
    append_started(&mut log, Some("q"), &q, "steve");

    let Response::Threads { threads } = steve
        .request(Request::ListThreads {
            project: Some("p".into()),
        })
        .await
        .unwrap()
    else {
        panic!("threads")
    };
    let ids: Vec<Ulid> = threads.iter().map(|t| t.id).collect();
    assert!(ids.contains(&mine) && ids.contains(&flat_one), "{ids:?}");
    assert!(!ids.contains(&legacy_one), "its log says q: {ids:?}");

    let Response::Threads { threads } = steve
        .request(Request::ListThreads {
            project: Some("q".into()),
        })
        .await
        .unwrap()
    else {
        panic!("threads")
    };
    assert!(threads.iter().any(|t| t.id == legacy_one));

    let Response::Projects { projects } = steve.request(Request::ListProjects).await.unwrap()
    else {
        panic!("projects")
    };
    let rows: Vec<(&str, u64)> = projects
        .iter()
        .map(|p| (p.name.as_str(), p.threads))
        .collect();
    assert_eq!(rows, vec![("p", 2), ("q", 1)]);
}

/// T7 — the issue lock names the project, so two projects' builds of one
/// issue never wait on each other, and only one build of one issue in one
/// project runs.
#[tokio::test]
async fn t7_issue_locks_are_per_project() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("threads");
    std::fs::create_dir_all(&base).unwrap();

    let held = IssueLock::take(&base, "p", 7).await.unwrap();
    assert!(
        base.join("issue-7-p.lock").is_file(),
        "the lock names the project and the issue"
    );

    let other = tokio::time::timeout(Duration::from_secs(2), IssueLock::take(&base, "q", 7))
        .await
        .expect("q's build of issue 7 does not wait on p's")
        .unwrap();
    assert!(base.join("issue-7-q.lock").is_file());

    // A project name with a character a file name shouldn't hold.
    let _odd = IssueLock::take(&base, "a/b", 7).await.unwrap();
    assert!(base.join("issue-7-a_b.lock").is_file());

    // The same project's second build waits, as it did before #9.
    let waiting =
        tokio::time::timeout(Duration::from_millis(200), IssueLock::take(&base, "p", 7)).await;
    assert!(waiting.is_err(), "p's second lock waits for p's first");

    drop(held);
    drop(other);
}

/// T7b — a lead's directory is its own, so its children are written
/// beside it, and a `Build` writes the lead flat.
#[tokio::test]
async fn t7b_a_run_writes_beside_its_lead() {
    let dir = tempfile::tempdir().unwrap();
    let p = project(dir.path(), "p", "");
    let daemon = Daemon::new(dir.path(), vec![pc("p", &p)], &["steve"], true).await;

    // A lead in the legacy directory, written after the daemon is up: a
    // held lead the start-up migration had to leave where it was.
    let held = Ulid::generate();
    let mut log = hand_log(daemon.base(), Some("p"), held);
    append_started(&mut log, Some("p"), &p, "steve");
    append_run_started(&mut log, 7);
    drop(log);

    let t = daemon.threads();
    assert_eq!(t.dir_of(held), Some(daemon.base().join("p")));
    // The run's world is its lead's directory: its lock and its children
    // both live beside the lead.
    let world = t.run_world_for("p", held).unwrap();
    assert_eq!(world.root.threads_dir, daemon.base().join("p"));
    let mut host = ServerHost::new(world, held);
    let child = Ulid::generate();
    host.create_child(child, "implement-alone").unwrap();
    assert!(
        daemon
            .base()
            .join("p")
            .join(format!("{child}.jsonl"))
            .is_file(),
        "the child is written beside its lead"
    );

    // A `Build` writes its lead flat, with the issue's lock beside it.
    let steve = daemon.connect("steve").await;
    let Response::Run { lead, .. } = steve
        .request(Request::Build {
            project: "p".into(),
            issue: 9,
            workflow: None,
        })
        .await
        .unwrap()
    else {
        panic!("a build is answered with a run")
    };
    assert!(
        daemon.base().join(format!("{lead}.jsonl")).is_file(),
        "the lead is flat"
    );
    assert!(daemon.base().join("issue-9-p.lock").is_file());
}

/// T7c — a log that cannot be read and is not a lead no longer refuses a
/// `Build` for its project; one that is a lead still does.
#[tokio::test]
async fn t7c_an_unreadable_non_lead_log_does_not_refuse_a_build() {
    let dir = tempfile::tempdir().unwrap();
    let p = project(dir.path(), "p", "");
    let daemon = Daemon::new(dir.path(), vec![pc("p", &p)], &["steve"], false).await;

    // A plain thread, damaged past repair: a good header, a line no event
    // can parse, then a `user_message`. It is no run's lead, so it is no
    // reason to refuse a `Build` — the narrowing issue #9 makes.
    let plain = Ulid::generate();
    let mut log = hand_log(daemon.base(), None, plain);
    append_started(&mut log, Some("p"), &p, "steve");
    append_user_message(&mut log, "hello");
    drop(log);
    corrupt_middle(daemon.base(), plain);
    assert_eq!(
        daemon.threads().unfinished_run("p", 7).unwrap(),
        None,
        "an unreadable non-lead is no reason to refuse a build"
    );

    // The same damage to a lead's log: its issue is known, so it must be
    // read, and reading it fails (t15b's shape, reached through the
    // index).
    let lead = Ulid::generate();
    let mut log = hand_log(daemon.base(), None, lead);
    append_started(&mut log, Some("p"), &p, "steve");
    append_run_started(&mut log, 7);
    drop(log);
    corrupt_middle(daemon.base(), lead);
    let refused = daemon.threads().unfinished_run("p", 7);
    assert!(refused.is_err(), "a corrupt lead refuses: {refused:?}");

    // And a tail torn mid-line that promised a `run_started`: the log
    // says it is a lead and its issue cannot be read either.
    let torn = Ulid::generate();
    let mut log = hand_log(daemon.base(), None, torn);
    append_started(&mut log, Some("p"), &p, "steve");
    drop(log);
    tear(daemon.base(), torn, "run_started");
    let refused = daemon.threads().unfinished_run("p", 7);
    assert!(refused.is_err(), "a torn lead refuses: {refused:?}");
}

/// Put a line no event can parse between the header and the rest, as a
/// partial write or a lost byte does. A torn *tail* is repaired; this is
/// not a tail.
fn corrupt_middle(base: &Path, id: Ulid) {
    let path = base.join(format!("{id}.jsonl"));
    let text = std::fs::read_to_string(&path).unwrap();
    let (first, rest) = text.split_once('\n').unwrap();
    std::fs::write(&path, format!("{first}\nthis is not an event\n{rest}")).unwrap();
}

/// Cut a log's tail mid-line, as a `kill -9` does, so the file promises
/// an event of `kind` and holds no line for it.
fn tear(base: &Path, id: Ulid, kind: &str) {
    let path = base.join(format!("{id}.jsonl"));
    let mut body = std::fs::read_to_string(&path).unwrap();
    body.push_str(&format!(
        "{{\"seq\":9,\"created_at\":\"2026-01-01T00:00:00Z\",\"kind\":\"{kind}\""
    ));
    std::fs::write(path, body).unwrap();
}

/// T8 — `title_of` reads the log, and an unknown id has none.
#[tokio::test]
async fn t8_title_of_comes_from_the_log() {
    let dir = tempfile::tempdir().unwrap();
    let p = project(dir.path(), "p", "");
    let daemon = Daemon::new(dir.path(), vec![pc("p", &p)], &["steve"], false).await;
    let mut steve = daemon.connect("steve").await;

    let id = created(&mut steve, "p").await;
    assert_eq!(daemon.threads().title_of(id), None, "no name yet");
    for title in ["the first name", "the last name"] {
        assert_eq!(
            steve
                .request(Request::Rename {
                    thread: id,
                    title: title.into()
                })
                .await
                .unwrap(),
            Response::Ok
        );
    }
    assert_eq!(
        daemon.threads().title_of(id).as_deref(),
        Some("the last name")
    );
    assert_eq!(daemon.threads().title_of(Ulid::generate()), None);
}

/// T9 — the embedded daemon names a bare root by its basename, and a
/// clash with a workspace project at another root gets a hex suffix.
#[tokio::test]
async fn t9_embedded_naming() {
    let dir = tempfile::tempdir().unwrap();
    // A bare root: no project file, so its basename names it.
    let root = std::fs::canonicalize(dir.path()).unwrap().join("x");
    std::fs::create_dir_all(&root).unwrap();
    assert_eq!(project_name_at(&root, &[]), "x");

    // The same basename as a workspace project at another root: the bare
    // root is named `<basename>-<8 hex>`.
    let other = std::fs::canonicalize(dir.path()).unwrap().join("other/x");
    std::fs::create_dir_all(&other).unwrap();
    let workspace = Workspace {
        name: "w".into(),
        shared: None,
        projects: vec![other.clone()],
    };
    let named = project_name_at(&root, &[workspace]);
    let expected = format!("x-{:08x}", fnv1a32(root.to_string_lossy().as_bytes()));
    assert_eq!(named, expected, "the clash gets the root's hash");
}

/// FNV-1a 32 over `bytes`, the hash the clash suffix comes from.
fn fnv1a32(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in bytes {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}
