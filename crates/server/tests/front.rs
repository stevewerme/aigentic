//! The front thread (issue #84): the newest `front: true` thread a person
//! made, resumed from anywhere, and the two requests that make one.
//! Nothing here touches a real threads directory: every base is a
//! `tempdir`.

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use aigentic_api::client::{Addr, Client};
use aigentic_api::{FrontOutcome, PROTOCOL_VERSION, Request, Response};
use aigentic_runtime::aigentic_core::{
    Author, Capabilities, CompletionRequest, EventKind, Message, Provider, ProviderEvent, UserId,
};
use aigentic_runtime::aigentic_log::{
    NewEvent, RunStartedPayload, ThreadLog, ThreadStartedPayload,
};
use aigentic_runtime::runner::RunnerHost;
use aigentic_server::build::{BuildError, ProviderFactory};
use aigentic_server::config::{Config, ProjectConfig, ServerConfig, UserConfig};
use aigentic_server::runs::ServerHost;
use aigentic_server::{Listener, NoReports, Server};
use futures_core::Stream;
use serde_json::to_value;
use ulid::Ulid;

// ---------------------------------------------------------------------------
// The rig: a project, a daemon over a temp base, and the log readers
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

/// Rewrite `name`'s participants, as a person editing the file would.
fn participants(dir: &Path, name: &str, participants: &str) -> PathBuf {
    let root = dir.join(name);
    std::fs::write(
        root.join("aigentic.toml"),
        format!("[project]\nname = \"{name}\"\n{participants}[memory]\nenabled = false\n"),
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
            supports_tools: true,
            supports_images: false,
            supports_caching: false,
            supports_structured_output: false,
            max_context_tokens: 1000,
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
/// starts, and a second daemon over the same directory is its restart.
struct Daemon {
    server: Arc<Server>,
    base: PathBuf,
    socket: PathBuf,
    task: tokio::task::JoinHandle<()>,
}

impl Daemon {
    /// A daemon whose `server.toml` names `projects` and whose users are
    /// `users`. With `bundled`, `dir` is also the bundled directory, so a
    /// `Build` finds a workflow there.
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
        let serve = server.clone();
        let listen = socket.clone();
        let task = tokio::spawn(async move {
            let _ = serve.serve(Listener::Unix(listen)).await;
        });
        // Ready when a connection succeeds: a socket file left by an
        // earlier daemon over this directory is not a listening one.
        for _ in 0..400 {
            if tokio::net::UnixStream::connect(&socket).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        Self {
            server,
            base,
            socket,
            task,
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

    /// Take this daemon down, as a process ending would: the tasks stop
    /// and the socket goes, and the files it served are left behind for
    /// the next daemon.
    async fn stop(self) {
        self.server.threads.runs().stop_tasks().await;
        self.task.abort();
        tokio::time::sleep(Duration::from_millis(200)).await;
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

/// Every event of `id`'s log.
fn events_of(base: &Path, id: Ulid) -> Vec<aigentic_runtime::aigentic_core::Event> {
    ThreadLog::open(base, id).unwrap().read_all().unwrap()
}

/// `id`'s `thread_started` payload.
fn front_of(base: &Path, id: Ulid) -> ThreadStartedPayload {
    let events = events_of(base, id);
    let started = events
        .iter()
        .find(|e| e.kind == EventKind::ThreadStarted)
        .expect("a thread_started");
    serde_json::from_value(started.payload.clone()).unwrap()
}

/// A hand-written log in the flat directory, for the cases only a log
/// another process wrote can make.
fn hand_log(base: &Path, id: Ulid) -> ThreadLog {
    std::fs::create_dir_all(base).unwrap();
    ThreadLog::open(base, id).unwrap()
}

/// A hand-written `thread_started`.
fn append_started(log: &mut ThreadLog, project: Option<&str>, root: &Path, by: &str, front: bool) {
    log.append(NewEvent {
        kind: EventKind::ThreadStarted,
        author: Author::User(UserId(by.to_owned())),
        payload: to_value(ThreadStartedPayload {
            project: project.map(str::to_owned),
            root: root.to_path_buf(),
            created_by: Author::User(UserId(by.to_owned())),
            parent_thread: None,
            step: None,
            front,
        })
        .unwrap(),
        parent_event: None,
    })
    .unwrap();
}

/// A hand-written `run_started`, which is what makes a log a run's lead.
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

/// The owner's connection, for looking at what a refused user left
/// behind.
async fn steve(daemon: &Daemon) -> Client {
    daemon.connect("steve").await
}

/// Ask for the front thread and answer with its row and outcome.
async fn front(client: &mut Client, project_name: &str) -> (Ulid, FrontOutcome) {
    match client
        .request(Request::Front {
            project: project_name.into(),
        })
        .await
        .unwrap()
    {
        Response::Front { thread, outcome } => (thread.id, outcome),
        other => panic!("a front thread: {other:?}"),
    }
}

/// The id `ListThreads` lists, if it lists it.
async fn listed(client: &mut Client, project_name: &str) -> Vec<Ulid> {
    match client
        .request(Request::ListThreads {
            project: project_name.into(),
        })
        .await
        .unwrap()
    {
        Response::Threads { threads } => threads.into_iter().map(|t| t.id).collect(),
        other => panic!("a listing: {other:?}"),
    }
}

/// The refusal reason of a request that must be refused.
async fn refusal(client: &mut Client, request: Request) -> String {
    match client.request(request).await.unwrap() {
        Response::Refused { reason } => reason,
        other => panic!("a refusal: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// T4–T6: the first front thread, its restart, and `NewFront`
// ---------------------------------------------------------------------------

/// T4 — the first `Front` makes one and says `First`; the next resumes it.
#[tokio::test]
async fn t4_the_first_front_thread_is_made_and_then_resumed() {
    let dir = tempfile::tempdir().unwrap();
    let p = project(dir.path(), "p", "");
    let daemon = Daemon::new(dir.path(), vec![pc("p", &p)], &["steve"], false).await;
    let mut steve = daemon.connect("steve").await;

    let (made, outcome) = front(&mut steve, "p").await;
    assert_eq!(outcome, FrontOutcome::First);
    let payload = front_of(daemon.base(), made);
    assert!(payload.front, "the record says so: {payload:?}");
    assert_eq!(payload.created_by, Author::User(UserId("steve".into())));
    assert_eq!(payload.project.as_deref(), Some("p"));

    let (again, outcome) = front(&mut steve, "p").await;
    assert_eq!(outcome, FrontOutcome::Resumed);
    assert_eq!(again, made);
}

/// T5 — the front thread survives a daemon restart: its record is in the
/// log, not in the daemon's memory.
#[tokio::test]
async fn t5_the_front_thread_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let p = project(dir.path(), "p", "");
    let projects = vec![pc("p", &p)];
    let first = Daemon::new(dir.path(), projects.clone(), &["steve"], false).await;
    let mut steve = first.connect("steve").await;
    let (made, outcome) = front(&mut steve, "p").await;
    assert_eq!(outcome, FrontOutcome::First);
    first.stop().await;

    let second = Daemon::new(dir.path(), projects, &["steve"], false).await;
    let mut steve = second.connect("steve").await;
    let (again, outcome) = front(&mut steve, "p").await;
    assert_eq!(outcome, FrontOutcome::Resumed);
    assert_eq!(again, made, "the same thread, reopened");
}

/// T6 — `NewFront` makes a new one on purpose: `Front` resumes it from
/// then on, and the old one is still listed.
#[tokio::test]
async fn t6_new_front_makes_a_second_front_thread_the_front_now_resumes() {
    let dir = tempfile::tempdir().unwrap();
    let p = project(dir.path(), "p", "");
    let daemon = Daemon::new(dir.path(), vec![pc("p", &p)], &["steve"], false).await;
    let mut steve = daemon.connect("steve").await;

    let (old, _) = front(&mut steve, "p").await;
    let Response::Thread { thread: new } = steve
        .request(Request::NewFront {
            project: "p".into(),
        })
        .await
        .unwrap()
    else {
        panic!("a new thread")
    };
    assert_ne!(new.id, old);
    assert!(front_of(daemon.base(), new.id).front);

    let (resumed, outcome) = front(&mut steve, "p").await;
    assert_eq!(outcome, FrontOutcome::Resumed);
    assert_eq!(resumed, new.id, "the newest one wins");

    let threads = listed(&mut steve, "p").await;
    assert!(
        threads.contains(&old),
        "the old one stays listed: {threads:?}"
    );
    assert!(threads.contains(&new.id), "{threads:?}");
}

// ---------------------------------------------------------------------------
// T7–T8: what is never front, and whose front thread is whose
// ---------------------------------------------------------------------------

/// T7 — a plain `CreateThread`, a build's lead and a step's child are
/// none of them anybody's front thread.
#[tokio::test]
async fn t7_a_plain_thread_a_lead_and_a_child_are_never_front() {
    let dir = tempfile::tempdir().unwrap();
    let p = project(dir.path(), "p", "");
    let daemon = Daemon::new(dir.path(), vec![pc("p", &p)], &["steve"], true).await;
    let mut steve = daemon.connect("steve").await;

    let Response::Thread { thread: plain } = steve
        .request(Request::CreateThread {
            project: "p".into(),
        })
        .await
        .unwrap()
    else {
        panic!("a thread")
    };
    assert!(!front_of(daemon.base(), plain.id).front);

    let Response::Run { lead, .. } = steve
        .request(Request::Build {
            project: "p".into(),
            issue: 84,
            workflow: None,
        })
        .await
        .unwrap()
    else {
        panic!("a build is answered with a run")
    };
    let world = daemon.threads().run_world_for("p", lead).unwrap();
    let child = Ulid::generate();
    ServerHost::new(world, lead)
        .create_child(child, "implement-alone")
        .unwrap();

    assert_eq!(daemon.threads().latest_front("steve"), None);
    let (made, outcome) = front(&mut steve, "p").await;
    assert_eq!(
        outcome,
        FrontOutcome::First,
        "none of them could be resumed"
    );
    assert_ne!(made, lead);
    assert_ne!(made, plain.id);
    assert_ne!(made, child);
}

/// T8 — a front thread is a person's own: two users in one project each
/// resume theirs.
#[tokio::test]
async fn t8_each_user_resumes_their_own_front_thread() {
    let dir = tempfile::tempdir().unwrap();
    let p = project(
        dir.path(),
        "p",
        "[participants]\nsteve = \"write\"\ncara = \"write\"\n",
    );
    let daemon = Daemon::new(dir.path(), vec![pc("p", &p)], &["steve", "cara"], false).await;
    let mut steve = daemon.connect("steve").await;
    let mut cara = daemon.connect("cara").await;

    let (steves, _) = front(&mut steve, "p").await;
    let (caras, _) = front(&mut cara, "p").await;
    assert_ne!(steves, caras);
    assert_eq!(
        front_of(daemon.base(), caras).created_by,
        Author::User(UserId("cara".into()))
    );

    let (again, outcome) = front(&mut steve, "p").await;
    assert_eq!(outcome, FrontOutcome::Resumed);
    assert_eq!(again, steves);
    let (again, outcome) = front(&mut cara, "p").await;
    assert_eq!(outcome, FrontOutcome::Resumed);
    assert_eq!(again, caras);
}

// ---------------------------------------------------------------------------
// T9: a front thread that cannot be opened is replaced, and the reason
// ---------------------------------------------------------------------------

/// T9a — a lost role in the front thread's project replaces it, from a
/// project the person can still write in.
#[tokio::test]
async fn t9a_losing_the_role_in_the_front_threads_project_replaces_it() {
    let dir = tempfile::tempdir().unwrap();
    let p = project(
        dir.path(),
        "p",
        "[participants]\nsteve = \"write\"\ncara = \"write\"\n",
    );
    let q = project(
        dir.path(),
        "q",
        "[participants]\nsteve = \"write\"\ncara = \"write\"\n",
    );
    let daemon = Daemon::new(
        dir.path(),
        vec![pc("p", &p), pc("q", &q)],
        &["steve", "cara"],
        false,
    )
    .await;
    let mut cara = daemon.connect("cara").await;
    let (old, _) = front(&mut cara, "p").await;
    // The file is edited under her: no role in `p` any more.
    participants(dir.path(), "p", "[participants]\nsteve = \"write\"\n");

    let (made, outcome) = front(&mut cara, "q").await;
    assert_ne!(made, old);
    assert_eq!(
        outcome,
        FrontOutcome::Replaced {
            reason: "you no longer have a role in p".into()
        }
    );
    assert_eq!(front_of(daemon.base(), made).project.as_deref(), Some("q"));
    // She cannot even list `p` any more, so the old one is out of her
    // sight; `t6` covers the old thread staying listed where a role
    // remains.
    let reason = refusal(
        &mut cara,
        Request::ListThreads {
            project: "p".into(),
        },
    )
    .await;
    assert!(reason.contains("read"), "{reason}");
}

/// T9a, the other leg — asking from `p` itself, where she has no role:
/// the resume fails for the same reason and the create it falls back to
/// is refused, so she is never handed a front thread in a project she
/// has no hand in. A fresh rig, because the first leg gave her a front
/// thread in `q` that `Front` would resume without ever looking at the
/// requested project.
#[tokio::test]
async fn t9a_a_lost_role_in_the_requested_project_refuses_to_create_one() {
    let dir = tempfile::tempdir().unwrap();
    let p = project(dir.path(), "p", "[participants]\nsteve = \"write\"\n");
    let base = dir.path().join("threads");
    let lost = Ulid::generate();
    let mut log = hand_log(&base, lost);
    append_started(&mut log, Some("p"), &p, "cara", true);
    drop(log);

    let daemon = Daemon::new(dir.path(), vec![pc("p", &p)], &["steve", "cara"], false).await;
    let mut cara = daemon.connect("cara").await;
    let reason = refusal(
        &mut cara,
        Request::Front {
            project: "p".into(),
        },
    )
    .await;
    assert_eq!(
        reason, "in p: cara has no role in this project; this needs write",
        "the create it fell back to is checked in the requested project"
    );
    let threads = listed(&mut steve(&daemon).await, "p").await;
    assert!(
        !threads.iter().any(|id| *id != lost),
        "nothing new was made: {threads:?}"
    );
}

/// T9b — a front thread whose project this daemon does not know is
/// replaced, and the reason names the project.
#[tokio::test]
async fn t9b_a_daemon_that_does_not_know_the_project_replaces_the_front_thread() {
    let dir = tempfile::tempdir().unwrap();
    let p = project(dir.path(), "p", "");
    let q = project(dir.path(), "q", "");
    let first = Daemon::new(
        dir.path(),
        vec![pc("p", &p), pc("q", &q)],
        &["steve"],
        false,
    )
    .await;
    let mut steve = first.connect("steve").await;
    let (old, _) = front(&mut steve, "p").await;
    first.stop().await;

    // A daemon that serves `q` alone: `p` is a project it does not know.
    let second = Daemon::new(dir.path(), vec![pc("q", &q)], &["steve"], false).await;
    let mut steve = second.connect("steve").await;
    let (made, outcome) = front(&mut steve, "q").await;
    assert_ne!(made, old);
    assert_eq!(
        outcome,
        FrontOutcome::Replaced {
            reason: "your front thread is in p, which this daemon does not know".into()
        }
    );
}

/// T9c — a front thread a run owns is replaced, and the reason says so.
#[tokio::test]
async fn t9c_a_front_thread_owned_by_a_run_is_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let p = project(dir.path(), "p", "");
    let base = dir.path().join("threads");
    let held = Ulid::generate();
    let mut log = hand_log(&base, held);
    append_started(&mut log, Some("p"), &p, "steve", true);
    append_run_started(&mut log, 84);
    drop(log);

    let daemon = Daemon::new(dir.path(), vec![pc("p", &p)], &["steve"], false).await;
    let mut steve = daemon.connect("steve").await;
    let (made, outcome) = front(&mut steve, "p").await;
    assert_ne!(made, held);
    assert_eq!(
        outcome,
        FrontOutcome::Replaced {
            reason: format!("your front thread {held} belongs to a run")
        }
    );
}

/// T9d — a front thread whose log names no project is replaced, and the
/// reason says that instead.
#[tokio::test]
async fn t9d_a_front_log_with_no_project_is_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let p = project(dir.path(), "p", "");
    let base = dir.path().join("threads");
    let homeless = Ulid::generate();
    let mut log = hand_log(&base, homeless);
    append_started(&mut log, None, &p, "steve", true);
    drop(log);

    let daemon = Daemon::new(dir.path(), vec![pc("p", &p)], &["steve"], false).await;
    let mut steve = daemon.connect("steve").await;
    let (made, outcome) = front(&mut steve, "p").await;
    assert_ne!(made, homeless);
    assert_eq!(
        outcome,
        FrontOutcome::Replaced {
            reason: format!("your front thread {homeless} has no project this daemon can open")
        }
    );
}

// ---------------------------------------------------------------------------
// T10–T11: the roles the handler judges, and the protocol
// ---------------------------------------------------------------------------

/// T10a — `NewFront` creates, so the pre-check asks for `write` and a
/// `read`-only user is refused before the handler runs.
#[tokio::test]
async fn t10a_new_front_from_a_read_only_user_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let p = project(dir.path(), "p", "[participants]\ncara = \"read\"\n");
    let daemon = Daemon::new(dir.path(), vec![pc("p", &p)], &["steve", "cara"], false).await;
    let mut cara = daemon.connect("cara").await;

    let reason = refusal(
        &mut cara,
        Request::NewFront {
            project: "p".into(),
        },
    )
    .await;
    let denied = aigentic_runtime::aigentic_policy::needs(&Request::NewFront {
        project: "p".into(),
    })
    .unwrap();
    assert!(reason.contains("cara"), "{reason}");
    assert!(reason.contains(denied.name()), "{reason}");
}

/// T10b — a `Front` that has to create is refused where the user cannot
/// write, with the reason the create's own check gives.
#[tokio::test]
async fn t10b_a_front_that_must_create_is_refused_without_write() {
    let dir = tempfile::tempdir().unwrap();
    let p = project(dir.path(), "p", "[participants]\ncara = \"read\"\n");
    let daemon = Daemon::new(dir.path(), vec![pc("p", &p)], &["steve", "cara"], false).await;
    let mut cara = daemon.connect("cara").await;

    let reason = refusal(
        &mut cara,
        Request::Front {
            project: "p".into(),
        },
    )
    .await;
    assert_eq!(
        reason, "in p: cara is read in this project; this needs write",
        "the `AnswerSwitch` shape, and `write` is what a create needs"
    );
}

/// T10c — a resume needs `read` in the front thread's project alone, so
/// it works from a project where the user has no role at all.
#[tokio::test]
async fn t10c_a_resume_needs_only_read_where_the_front_thread_lives() {
    let dir = tempfile::tempdir().unwrap();
    let p = project(dir.path(), "p", "[participants]\ncara = \"write\"\n");
    // `q` is nobody's but the owner's: cara has no role there.
    let q = project(dir.path(), "q", "[participants]\nsteve = \"write\"\n");
    let daemon = Daemon::new(
        dir.path(),
        vec![pc("p", &p), pc("q", &q)],
        &["steve", "cara"],
        false,
    )
    .await;
    let mut cara = daemon.connect("cara").await;
    let (made, _) = front(&mut cara, "p").await;
    // `write` down to `read` where the front thread lives, and herself
    // nowhere in `q`.
    participants(dir.path(), "p", "[participants]\ncara = \"read\"\n");

    let (again, outcome) = front(&mut cara, "q").await;
    assert_eq!(outcome, FrontOutcome::Resumed);
    assert_eq!(again, made);
}

/// T11 — a client one protocol behind is refused, with both numbers.
#[tokio::test]
async fn t11_a_stale_client_is_refused_with_both_numbers() {
    use aigentic_api::{Body, Frame, decode, encode};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let dir = tempfile::tempdir().unwrap();
    let p = project(dir.path(), "p", "");
    let daemon = Daemon::new(dir.path(), vec![pc("p", &p)], &["steve"], false).await;
    let stream = tokio::net::UnixStream::connect(&daemon.socket)
        .await
        .unwrap();
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    let stale = PROTOCOL_VERSION - 1;
    let hello = Frame::request(
        1,
        Request::Hello {
            protocol: stale,
            token: "tok-steve".into(),
        },
    );
    write
        .write_all(format!("{}\n", encode(&hello)).as_bytes())
        .await
        .unwrap();
    let line = lines.next_line().await.unwrap().expect("a reply");
    let frame = decode(&line).unwrap();
    let Body::Response(Response::Refused { reason }) = frame.body else {
        panic!("a refusal: {line}")
    };
    assert!(reason.contains(&stale.to_string()), "{reason}");
    assert!(reason.contains(&PROTOCOL_VERSION.to_string()), "{reason}");
}

// ---------------------------------------------------------------------------
// Issue #88: the built-in daemon opens the front thread from any folder,
// and two daemons never race to make two front threads. The rig here is
// `embed_with`, the daemon plain `aigentic` starts, not `Server::new`.
// ---------------------------------------------------------------------------

/// A daemon the way plain `aigentic` starts one: `embed_with`, which
/// registers the front thread's project when it can (issue #88). One
/// config dir and one threads base per test dir, so a second one over
/// the same dir is the next `aigentic` in another folder.
struct Plain {
    inner: aigentic_server::Embedded,
}

impl Plain {
    async fn new(dir: &Path, root: &Path, user: &str) -> Self {
        let cfg_dir = dir.join("cfg");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let base = dir.join("threads");
        let config = Config::parse(&format!(
            "threads_dir = {:?}\n[profiles.a]\nbase_url = \"u\"\nmodel = \"m\"\napi_key_env = \"K\"\n",
            base.display()
        ))
        .unwrap();
        let inner = Server::embed_with(
            config,
            cfg_dir,
            root.to_path_buf(),
            user,
            None,
            Arc::new(OkFactory),
            Arc::new(NoReports),
        )
        .await
        .unwrap();
        Self { inner }
    }

    async fn connect(&self) -> Client {
        Client::connect(&Addr::Unix(self.inner.socket.clone()), &self.inner.token)
            .await
            .unwrap()
            .0
    }

    fn project(&self) -> &str {
        &self.inner.project
    }

    fn base(&self) -> PathBuf {
        self.inner.server.threads.threads_base().to_path_buf()
    }

    fn threads(&self) -> &aigentic_server::threads::ThreadTable {
        &self.inner.server.threads
    }
}

/// The files in `base` whose first line marks a front thread: what a
/// count of front threads has to look at, not a table's memory.
fn front_logs(base: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(base)
        .map(|d| {
            d.filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("jsonl"))
                .filter(|p| {
                    std::fs::read_to_string(p)
                        .unwrap_or_default()
                        .lines()
                        .next()
                        .is_some_and(|line| line.contains("\"front\":true"))
                })
                .collect()
        })
        .unwrap_or_default();
    found.sort();
    found
}

/// A workspace file naming `projects`, written the way a person's does.
fn workspace_naming(dir: &Path, name: &str, projects: &[&Path]) {
    let ws = dir.join("cfg").join("workspaces");
    std::fs::create_dir_all(&ws).unwrap();
    let list = projects
        .iter()
        .map(|p| format!("{:?}", p.display().to_string()))
        .collect::<Vec<_>>()
        .join(", ");
    std::fs::write(
        ws.join(format!("{name}.toml")),
        format!("name = \"{name}\"\nprojects = [{list}]\n"),
    )
    .unwrap();
}

/// A copy of a daemon's `Front` reply for `join!`, which cannot take
/// references across both arms for long.
async fn front_pair(
    first: &mut Client,
    second: &mut Client,
    name: &str,
) -> ((Ulid, FrontOutcome), (Ulid, FrontOutcome)) {
    tokio::join!(front(first, name), front(second, name))
}

/// T1 — a front thread made in `a` is resumed by a daemon embedded in
/// bare `b`, and `a` is in that daemon's project list.
#[tokio::test]
async fn t88_1_a_front_thread_opens_from_any_folder() {
    let dir = tempfile::tempdir().unwrap();
    let a = project(dir.path(), "a", "");
    let b = dir.path().join("b");
    std::fs::create_dir_all(&b).unwrap();

    let first = Plain::new(dir.path(), &a, "steve").await;
    assert_eq!(first.project(), "a");
    let mut client = first.connect().await;
    let (made, outcome) = front(&mut client, first.project()).await;
    assert_eq!(outcome, FrontOutcome::First);
    drop(first);

    let second = Plain::new(dir.path(), &b, "steve").await;
    assert_eq!(
        second.project(),
        aigentic_server::threads::project_name_at(&b, &[]),
        "the folder's own name, derived"
    );
    assert_eq!(second.inner.front_project.as_deref(), Some("a"));

    let mut client = second.connect().await;
    let (again, outcome) = front(&mut client, second.project()).await;
    assert_eq!(outcome, FrontOutcome::Resumed);
    assert_eq!(again, made, "the same front thread");
    assert!(
        opens(&mut client, again).await,
        "the resumed front thread opens"
    );

    // The registration is visible: the client's project list and the #81
    // block a thread of this daemon gets both name `a`.
    let Response::Projects { projects } = client.request(Request::ListProjects).await.unwrap()
    else {
        panic!("a project list")
    };
    assert!(
        projects.iter().any(|p| p.name == "a"),
        "the front project is listed: {projects:?}"
    );
    let block = second
        .threads()
        .shown_projects(again, Some("a"))
        .expect("a block");
    assert!(block.contains("a"), "{block}");
}

/// Whether an `Open` succeeds.
async fn opens(client: &mut Client, thread: Ulid) -> bool {
    !matches!(
        client
            .request(Request::Open {
                thread,
                from_seq: 0,
            })
            .await
            .unwrap(),
        Response::Refused { .. } | Response::Error { .. }
    )
}

/// Open a thread's actor, so a `SwitchProject` has one to act on. A
/// `Front` reply only names the id.
async fn open(client: &mut Client, thread: Ulid) {
    let opened = client
        .request(Request::Open {
            thread,
            from_seq: 0,
        })
        .await
        .unwrap();
    assert!(
        matches!(opened, Response::Opened { .. }),
        "opening the thread: {opened:?}"
    );
}

/// T2 — a front thread switched to `c` is registered at `c`'s root by a
/// daemon that has no workspace naming `c`.
#[tokio::test]
async fn t88_2_a_switched_front_thread_is_registered_where_it_switched_to() {
    let dir = tempfile::tempdir().unwrap();
    let a = project(dir.path(), "a", "");
    let c = project(dir.path(), "c", "");
    let b = dir.path().join("b");
    std::fs::create_dir_all(&b).unwrap();
    workspace_naming(dir.path(), "w", &[&c]);

    let first = Plain::new(dir.path(), &a, "steve").await;
    let mut client = first.connect().await;
    let (made, outcome) = front(&mut client, "a").await;
    assert_eq!(outcome, FrontOutcome::First);
    open(&mut client, made).await;
    assert_eq!(
        client
            .request(Request::SwitchProject {
                thread: made,
                project: "c".into(),
            })
            .await
            .unwrap(),
        Response::Ok
    );
    drop(first);

    // No workspace names `c` any more.
    std::fs::remove_file(dir.path().join("cfg/workspaces/w.toml")).unwrap();
    let second = Plain::new(dir.path(), &b, "steve").await;
    assert_eq!(second.inner.front_project.as_deref(), Some("c"));

    let mut client = second.connect().await;
    let (again, outcome) = front(&mut client, second.project()).await;
    assert_eq!(outcome, FrontOutcome::Resumed);
    assert_eq!(again, made);
}

/// T3 — the front thread's folder is gone, so nothing is registered and
/// `Front` says which project it cannot reach.
#[tokio::test]
async fn t88_3_a_gone_root_is_not_registered_and_is_named_in_the_reason() {
    let dir = tempfile::tempdir().unwrap();
    let x = dir.path().join("x");
    std::fs::create_dir_all(&x).unwrap();
    let b = dir.path().join("b");
    std::fs::create_dir_all(&b).unwrap();

    let first = Plain::new(dir.path(), &x, "steve").await;
    assert_eq!(first.project(), "x");
    let mut client = first.connect().await;
    let (made, outcome) = front(&mut client, "x").await;
    assert_eq!(outcome, FrontOutcome::First);
    drop(first);
    std::fs::remove_dir_all(&x).unwrap();

    let second = Plain::new(dir.path(), &b, "steve").await;
    assert_eq!(
        second.inner.front_project, None,
        "a gone root registers nothing"
    );
    let mut client = second.connect().await;
    let (again, outcome) = front(&mut client, second.project()).await;
    let FrontOutcome::Replaced { reason } = outcome else {
        panic!("a replacement: {outcome:?}")
    };
    assert!(reason.contains('x'), "{reason}");
    assert_ne!(again, made, "a new thread, not the gone one");
    assert_eq!(second.inner.front_project, None);
}

/// T4 — two bare folders named `app`: nothing is registered, and the
/// refusal names both roots.
#[tokio::test]
async fn t88_4_two_bare_folders_named_app_never_share_a_front_thread() {
    let dir = tempfile::tempdir().unwrap();
    let t1_app = dir.path().join("t1").join("app");
    let t2_app = dir.path().join("t2").join("app");
    std::fs::create_dir_all(&t1_app).unwrap();
    std::fs::create_dir_all(&t2_app).unwrap();

    let first = Plain::new(dir.path(), &t1_app, "steve").await;
    assert_eq!(first.project(), "app");
    let mut client = first.connect().await;
    let (made, outcome) = front(&mut client, "app").await;
    assert_eq!(outcome, FrontOutcome::First);
    drop(first);

    let second = Plain::new(dir.path(), &t2_app, "steve").await;
    assert_eq!(second.project(), "app");
    assert_eq!(
        second.inner.front_project, None,
        "a name already taken is not registered"
    );
    let mut client = second.connect().await;
    let (again, outcome) = front(&mut client, "app").await;
    let FrontOutcome::Replaced { reason } = outcome else {
        panic!("a replacement: {outcome:?}")
    };
    assert!(
        reason.contains(&t1_app.display().to_string()),
        "the reason names where the front thread is: {reason}"
    );
    assert!(
        reason.contains(&t2_app.display().to_string()),
        "the reason names this daemon's app: {reason}"
    );
    let payload = front_of(&second.base(), again);
    assert_eq!(payload.root, t2_app, "the new front thread is here");
    assert_ne!(again, made);
}

/// T5a — a front thread the daemon itself switched, unloaded and
/// resumed by the same daemon, comes back.
#[tokio::test]
async fn t88_5a_a_switched_front_thread_resumes_after_being_unloaded() {
    let dir = tempfile::tempdir().unwrap();
    let a = project(dir.path(), "a", "");
    let c = project(dir.path(), "c", "");
    let b = dir.path().join("b");
    std::fs::create_dir_all(&b).unwrap();
    workspace_naming(dir.path(), "w", &[&a, &c]);

    let daemon = Plain::new(dir.path(), &a, "steve").await;
    let mut client = daemon.connect().await;
    let (made, outcome) = front(&mut client, "a").await;
    assert_eq!(outcome, FrontOutcome::First);
    open(&mut client, made).await;
    assert_eq!(
        client
            .request(Request::SwitchProject {
                thread: made,
                project: "c".into(),
            })
            .await
            .unwrap(),
        Response::Ok
    );
    let root = daemon
        .threads()
        .root_of_thread(made)
        .expect("the index learned the switched root");
    assert!(root.ends_with("c"), "the switched project's root: {root:?}");
    // The last session closes the thread, so a sweep can unload it while
    // the daemon stays up.
    assert_eq!(
        client
            .request(Request::Close { thread: made })
            .await
            .unwrap(),
        Response::Ok
    );
    let unloaded = daemon.threads().sweep(Duration::ZERO).await;
    assert!(unloaded.contains(&made), "unloaded: {unloaded:?}");

    let (again, outcome) = front(&mut client, "c").await;
    assert_eq!(outcome, FrontOutcome::Resumed, "same daemon, still known");
    assert_eq!(again, made);
}

/// T5b — the same thread under a daemon whose project of that name is
/// another live root is refused.
#[tokio::test]
async fn t88_5b_another_root_of_the_same_name_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let a = project(dir.path(), "a", "");
    let c = project(dir.path(), "c", "");
    // A second project named `c`, at another root that exists.
    let other = dir.path().join("other");
    std::fs::create_dir_all(other.join(".aigentic")).unwrap();
    std::fs::write(
        other.join("aigentic.toml"),
        "[project]\nname = \"c\"\n[memory]\nenabled = false\n",
    )
    .unwrap();
    let b = dir.path().join("b");
    std::fs::create_dir_all(&b).unwrap();
    workspace_naming(dir.path(), "w", &[&c]);

    let first = Plain::new(dir.path(), &a, "steve").await;
    let mut client = first.connect().await;
    let (made, outcome) = front(&mut client, "a").await;
    assert_eq!(outcome, FrontOutcome::First);
    open(&mut client, made).await;
    assert_eq!(
        client
            .request(Request::SwitchProject {
                thread: made,
                project: "c".into(),
            })
            .await
            .unwrap(),
        Response::Ok
    );
    drop(first);

    // Now `c` is the other folder: the workspace names that one.
    workspace_naming(dir.path(), "w", &[&other]);
    let second = Plain::new(dir.path(), &b, "steve").await;
    assert_eq!(second.inner.front_project, None, "the name is taken");
    let mut client = second.connect().await;
    let (again, outcome) = front(&mut client, second.project()).await;
    assert!(
        matches!(outcome, FrontOutcome::Replaced { .. }),
        "another root's `c`: {outcome:?}"
    );
    assert_ne!(again, made);
}

/// T5c — a project whose folder moved: the recorded root no longer
/// exists, so the daemon's project of that name resumes it.
#[tokio::test]
async fn t88_5c_a_moved_project_still_resumes() {
    let dir = tempfile::tempdir().unwrap();
    let a = project(dir.path(), "a", "");
    let c = project(dir.path(), "c", "");
    let moved = dir.path().join("c-moved");
    let b = dir.path().join("b");
    std::fs::create_dir_all(&b).unwrap();
    workspace_naming(dir.path(), "w", &[&c]);

    let first = Plain::new(dir.path(), &a, "steve").await;
    let mut client = first.connect().await;
    let (made, outcome) = front(&mut client, "a").await;
    assert_eq!(outcome, FrontOutcome::First);
    open(&mut client, made).await;
    assert_eq!(
        client
            .request(Request::SwitchProject {
                thread: made,
                project: "c".into(),
            })
            .await
            .unwrap(),
        Response::Ok
    );
    drop(first);

    // The folder moves and the workspace file follows it.
    std::fs::rename(&c, &moved).unwrap();
    workspace_naming(dir.path(), "w", &[&moved]);
    let second = Plain::new(dir.path(), &b, "steve").await;
    let mut client = second.connect().await;
    let (again, outcome) = front(&mut client, second.project()).await;
    assert_eq!(
        outcome,
        FrontOutcome::Resumed,
        "the moved root is not a clash"
    );
    assert_eq!(again, made);
}

/// T6 — two daemons in one folder on one threads directory, both asked
/// for the front thread at once, make exactly one.
#[tokio::test]
async fn t88_6_two_daemons_starting_together_make_one_front_thread() {
    let dir = tempfile::tempdir().unwrap();
    let p = project(dir.path(), "p", "");
    let first = Plain::new(dir.path(), &p, "steve").await;
    let second = Plain::new(dir.path(), &p, "steve").await;
    assert_eq!(first.project(), second.project());
    let mut a = first.connect().await;
    let mut b = second.connect().await;

    let name = first.project().to_owned();
    let ((id_a, outcome_a), (id_b, outcome_b)) = front_pair(&mut a, &mut b, &name).await;
    assert_eq!(id_a, id_b, "one front thread");
    let made = [&outcome_a, &outcome_b]
        .iter()
        .filter(|o| matches!(o, FrontOutcome::First))
        .count();
    let resumed = [&outcome_a, &outcome_b]
        .iter()
        .filter(|o| matches!(o, FrontOutcome::Resumed))
        .count();
    assert_eq!(made, 1, "one made it: {outcome_a:?} / {outcome_b:?}");
    assert_eq!(resumed, 1, "one found it: {outcome_a:?} / {outcome_b:?}");
    assert_eq!(
        front_logs(&first.base()).len(),
        1,
        "exactly one front log: {:?}",
        front_logs(&first.base())
    );

    // The lock itself is what serialises them (issue #88): a `Front` that
    // is already in flight does not answer while the test holds the same
    // per-user lock, and does once it is released. Without the lock in
    // `front_thread`, it would answer immediately and this would fail.
    let held = aigentic_server::runs::FrontLock::take(&first.base(), "steve")
        .await
        .expect("holding the front lock");
    let queued = tokio::time::timeout(Duration::from_millis(300), front(&mut b, &name)).await;
    assert!(
        queued.is_err(),
        "a second Front waits for the lock: {queued:?}"
    );
    drop(held);
    let (id_c, outcome_c) = front(&mut b, &name).await;
    assert_eq!(outcome_c, FrontOutcome::Resumed, "released, so it finds it");
    assert_eq!(id_c, id_a, "still one front thread");
}

/// T7 — the per-user lock's file name comes from `lock_name`, and a
/// second take waits while the first is held.
#[tokio::test]
async fn t88_7_the_front_lock_is_per_user_and_refuses_a_second_taker() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("threads");
    let held = aigentic_server::runs::FrontLock::take(&base, "a b")
        .await
        .expect("the first take");
    let expected = base.join(format!(
        "front-{}.lock",
        aigentic_server::runs::lock_name("a b")
    ));
    assert!(expected.is_file(), "{}", expected.display());

    let second = tokio::time::timeout(
        Duration::from_millis(200),
        aigentic_server::runs::FrontLock::take(&base, "a b"),
    )
    .await;
    assert!(second.is_err(), "the second take waited, as IssueLock does");

    drop(held);
    assert!(
        aigentic_server::runs::FrontLock::take(&base, "a b")
            .await
            .is_ok(),
        "released"
    );
}

/// T8 — the served daemon never registers: `server.toml` is all it has.
#[tokio::test]
async fn t88_8_the_served_daemon_never_registers_a_front_project() {
    let dir = tempfile::tempdir().unwrap();
    let a = project(dir.path(), "a", "");
    let q = project(dir.path(), "q", "");
    // A front thread in `a`, in the same threads base the daemon opens.
    let id = Ulid::generate();
    let mut log = hand_log(&dir.path().join("threads"), id);
    append_started(&mut log, Some("a"), &a, "steve", true);

    let daemon = Daemon::new(dir.path(), vec![pc("q", &q)], &["steve"], false).await;
    let mut steve = steve(&daemon).await;
    let (made, outcome) = front(&mut steve, "q").await;
    let FrontOutcome::Replaced { reason } = outcome else {
        panic!("a replacement: {outcome:?}")
    };
    assert!(reason.contains("does not know"), "{reason}");
    assert_ne!(made, id, "not the thread in a");
}
