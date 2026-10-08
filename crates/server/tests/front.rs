//! The front thread (issue #84): the newest `front: true` thread a person
//! made, resumed from anywhere, and the two requests that make one.
//! Nothing here touches a real threads directory: every base is a
//! `tempdir`.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aigentic_api::client::{Addr, Client};
use aigentic_api::{
    FrontOutcome, Notice, PROTOCOL_VERSION, Request, Response, RunThread, SwitchReply, ThreadInfo,
    ThreadKind, ThreadState,
};
use aigentic_runtime::aigentic_core::{
    Author, Capabilities, CompletionRequest, ContentBlock, EventKind, Message, Provider,
    ProviderEvent, ToolCall, UserId,
};
use aigentic_runtime::aigentic_log::{
    DecisionAnswer, DecisionAnsweredPayload, DecisionKind, DecisionProposedPayload, NewEvent,
    RunStartedPayload, STARTUP_PREFIX, ThreadLog, ThreadStartedPayload,
};
use aigentic_runtime::runner::RunnerHost;
use aigentic_server::awake::KeepAwake;
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

/// One script shared by every provider the factory builds (issue #92): a
/// `None` step parks the turn, which is what a test needs to see a
/// running thread refuse the start-up proposal.
struct Scripted {
    script: Arc<Mutex<VecDeque<Option<Vec<ProviderEvent>>>>>,
}

impl Provider for Scripted {
    fn complete(
        &self,
        _: &CompletionRequest<'_>,
    ) -> Pin<Box<dyn Stream<Item = ProviderEvent> + Send + '_>> {
        match self
            .script
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
        {
            Some(Some(events)) => Box::pin(futures_util::stream::iter(events)),
            _ => Box::pin(futures_util::stream::pending()),
        }
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
            max_context_tokens: 100_000,
        }
    }
}

struct ScriptedFactory {
    script: Arc<Mutex<VecDeque<Option<Vec<ProviderEvent>>>>>,
}

impl ScriptedFactory {
    fn new(script: Vec<Option<Vec<ProviderEvent>>>) -> Arc<Self> {
        Arc::new(Self {
            script: Arc::new(Mutex::new(script.into())),
        })
    }
}

impl ProviderFactory for ScriptedFactory {
    fn build(&self, _: &str) -> Result<(Box<dyn Provider>, String), BuildError> {
        Ok((
            Box::new(Scripted {
                script: self.script.clone(),
            }),
            "scripted".into(),
        ))
    }
}

/// A guard that records the calls the daemon makes (issue #47), so a test
/// can read the hold/release scope around an answer or a withdrawal with
/// no turn to hide it — a copy of `tests/awake.rs`'s, which is private to
/// that file.
struct Recording {
    inner: Mutex<Rec>,
}

#[derive(Default)]
struct Rec {
    held: usize,
    calls: Vec<&'static str>,
}

impl Recording {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Rec::default()),
        })
    }

    fn rec(&self) -> std::sync::MutexGuard<'_, Rec> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn calls(&self) -> Vec<&'static str> {
        self.rec().calls.clone()
    }

    fn outstanding(&self) -> usize {
        self.rec().held
    }
}

impl KeepAwake for Recording {
    fn hold(&self) {
        let mut rec = self.rec();
        rec.held += 1;
        rec.calls.push("hold");
    }

    fn release(&self) {
        let mut rec = self.rec();
        rec.held = rec.held.saturating_sub(1);
        rec.calls.push("release");
    }

    fn status(&self) -> String {
        "on".to_owned()
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
        Self::new_with(dir, projects, users, bundled, Arc::new(OkFactory)).await
    }

    /// The same, with the factory given: a scripted provider lets a test
    /// park a turn, so a running thread can be asked for its proposal
    /// (issue #92).
    async fn new_with(
        dir: &Path,
        projects: Vec<ProjectConfig>,
        users: &[&str],
        bundled: bool,
        factory: Arc<dyn ProviderFactory>,
    ) -> Self {
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
            factory,
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

/// A hand-written `thread_started` for a step child of `lead`: the
/// parent makes the index call the log a run's child.
#[allow(clippy::too_many_arguments)]
fn append_child_started(
    log: &mut ThreadLog,
    project: Option<&str>,
    root: &Path,
    lead: Ulid,
    step: &str,
) {
    log.append(NewEvent {
        kind: EventKind::ThreadStarted,
        author: Author::System,
        payload: to_value(ThreadStartedPayload {
            project: project.map(str::to_owned),
            root: root.to_path_buf(),
            created_by: Author::System,
            parent_thread: Some(lead),
            step: Some(step.to_owned()),
            front: false,
        })
        .unwrap(),
        parent_event: None,
    })
    .unwrap();
}

/// A hand-written `project_switched`, the other half of a thread's
/// project: `to` is where it lives now.
fn append_switch(log: &mut ThreadLog, to: &str, root: &Path) {
    log.append(NewEvent {
        kind: EventKind::ProjectSwitched,
        author: Author::System,
        payload: to_value(aigentic_runtime::aigentic_log::ProjectSwitchedPayload {
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

/// The owner's connection, for looking at what a refused user left
/// behind.
async fn steve(daemon: &Daemon) -> Client {
    daemon.connect("steve").await
}

/// Ask for the front thread and answer with its row and outcome.
async fn front(client: &mut Client, project_name: &str) -> (Ulid, FrontOutcome) {
    front_here(client, project_name, None).await
}

/// The same ask, standing in a folder whose project the client names in
/// `here` (issue #92); `None` is a bare folder, the old client's frame.
async fn front_here(
    client: &mut Client,
    project_name: &str,
    here: Option<&str>,
) -> (Ulid, FrontOutcome) {
    match client
        .request(Request::Front {
            project: project_name.into(),
            here: here.map(str::to_owned),
            asked: None,
        })
        .await
        .unwrap()
    {
        Response::Front { thread, outcome } => (thread.id, outcome),
        other => panic!("a front thread: {other:?}"),
    }
}

/// Open `id` and answer with the state the client would draw.
async fn open_state(client: &mut Client, id: Ulid) -> ThreadState {
    match client
        .request(Request::Open {
            thread: id,
            from_seq: 0,
        })
        .await
        .unwrap()
    {
        Response::Opened { state, .. } => state,
        other => panic!("an open: {other:?}"),
    }
}

/// Every `decision_proposed` in `id`'s log, with the author that wrote
/// it.
fn proposed_of(base: &Path, id: Ulid) -> Vec<(Author, DecisionProposedPayload)> {
    events_of(base, id)
        .into_iter()
        .filter(|e| e.kind == EventKind::DecisionProposed)
        .map(|e| {
            let payload = serde_json::from_value(e.payload.clone()).unwrap();
            (e.author, payload)
        })
        .collect()
}

/// Every `decision_answered` in `id`'s log, in order.
fn answered_of(base: &Path, id: Ulid) -> Vec<DecisionAnsweredPayload> {
    events_of(base, id)
        .into_iter()
        .filter(|e| e.kind == EventKind::DecisionAnswered)
        .map(|e| serde_json::from_value(e.payload.clone()).unwrap())
        .collect()
}

/// The id `ListThreads` lists, if it lists it.
async fn listed(client: &mut Client, project_name: &str) -> Vec<Ulid> {
    match client
        .request(Request::ListThreads {
            project: Some(project_name.into()),
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
            project: Some("p".into()),
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
            here: None,
            asked: None,
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
            here: None,
            asked: None,
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

// ---------------------------------------------------------------------------
// T2–T5 (issue #92): the start-up project proposal
// ---------------------------------------------------------------------------

/// The kinds of `t`'s log.
fn kinds_of(base: &Path, t: Ulid) -> Vec<EventKind> {
    events_of(base, t).iter().map(|e| e.kind).collect()
}

/// The position of the first event of `kind` in `t`'s log.
fn at(base: &Path, t: Ulid, kind: EventKind) -> Option<usize> {
    kinds_of(base, t).iter().position(|k| *k == kind)
}

/// The `AwaitingSwitch` in `t`'s state, if it is waiting for one.
async fn waiting(client: &mut Client, t: Ulid) -> (String, String) {
    match open_state(client, t).await {
        ThreadState::AwaitingSwitch {
            call_id, project, ..
        } => (call_id, project),
        other => panic!("a waiting switch: {other:?}"),
    }
}

/// Answer a switch as the given role.
async fn answer_switch(
    client: &mut Client,
    t: Ulid,
    call_id: &str,
    answer: SwitchReply,
) -> Response {
    client
        .request(Request::AnswerSwitch {
            thread: t,
            call_id: call_id.into(),
            answer,
        })
        .await
        .unwrap()
}

/// Post a message.
async fn post(client: &mut Client, t: Ulid, text: &str) -> Response {
    client
        .request(Request::Post {
            thread: t,
            blocks: vec![ContentBlock::Text(text.into())],
            interrupt: false,
        })
        .await
        .unwrap()
}

/// A daemon with `a`, `b` and `c` known, all writable by `steve`.
async fn three_projects(dir: &Path) -> (Daemon, PathBuf, PathBuf, PathBuf) {
    let a = project(dir, "a", "");
    let b = project(dir, "b", "");
    let c = project(dir, "c", "");
    let daemon = Daemon::new(
        dir,
        vec![pc("a", &a), pc("b", &b), pc("c", &c)],
        &["steve"],
        false,
    )
    .await;
    (daemon, a, b, c)
}

/// A front thread in `a`, made and unloaded, ready for a start-up
/// proposal against `here`.
async fn idle_front_in_a(daemon: &Daemon) -> (Client, Ulid) {
    let mut steve = daemon.connect("steve").await;
    let (t, outcome) = front(&mut steve, "a").await;
    assert_eq!(outcome, FrontOutcome::First);
    // Load it and let the session go, so the sweep below has something to
    // unload: that is the baseline the proposal then has to survive.
    let _ = steve
        .request(Request::Open {
            thread: t,
            from_seq: 0,
        })
        .await
        .unwrap();
    assert_eq!(
        steve.request(Request::Close { thread: t }).await.unwrap(),
        Response::Ok
    );
    let swept = daemon.threads().sweep(Duration::ZERO).await;
    assert!(swept.contains(&t), "unloaded, so the next Front loads it");
    (steve, t)
}

/// T2 — the proposal shows: a `Resumed` front thread in `b`'s folder
/// raises it, the client sees `AwaitingSwitch`, the log holds the
/// System `decision_proposed`, a subscriber gets it as a
/// `Notice::Event`, and a sweep leaves it loaded while it waits.
#[tokio::test]
async fn t2_the_start_up_proposal_shows_and_is_not_swept_away() {
    let dir = tempfile::tempdir().unwrap();
    let (daemon, _a, _b, _c) = three_projects(dir.path()).await;
    let (mut steve, t) = idle_front_in_a(&daemon).await;
    let viewer = daemon.connect("steve").await;

    // A live subscriber: `Open` replays, then keeps the client live, so
    // the proposal reaches it as an event.
    assert!(matches!(
        viewer
            .request(Request::Open {
                thread: t,
                from_seq: 0,
            })
            .await
            .unwrap(),
        Response::Opened { .. }
    ));
    let mut notices = viewer.take_notices().expect("notices");

    let (again, outcome) = front_here(&mut steve, "b", Some("b")).await;
    assert_eq!(
        outcome,
        FrontOutcome::Resumed,
        "the front reply is unchanged"
    );
    assert_eq!(again, t);

    let (call_id, project) = waiting(&mut steve, t).await;
    assert_eq!(project, "b");
    assert!(
        call_id.starts_with(STARTUP_PREFIX),
        "the call id is a start-up one: {call_id}"
    );

    let proposed = proposed_of(daemon.base(), t);
    assert_eq!(proposed.len(), 1, "{proposed:?}");
    assert_eq!(proposed[0].1.kind, DecisionKind::Project);
    assert_eq!(proposed[0].0, Author::System, "raised by the System");
    let events = events_of(daemon.base(), t);
    let proposal = events
        .iter()
        .find(|e| e.kind == EventKind::DecisionProposed)
        .unwrap();
    assert_eq!(proposal.author, Author::System);

    // A live subscriber was told, as #47's keep-awake path expects.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut seen_proposal = false;
    while tokio::time::Instant::now() < deadline {
        let Ok(Some(notice)) = tokio::time::timeout(Duration::from_secs(5), notices.recv()).await
        else {
            break;
        };
        if matches!(&notice, Notice::Event { event, .. }
            if event.kind == EventKind::DecisionProposed)
        {
            seen_proposal = true;
            break;
        }
    }
    assert!(seen_proposal, "the subscriber saw the decision_proposed");
    drop(notices);

    // While it waits, the sweep leaves the thread loaded: every session
    // on it lets go first, so only the pending answer can be the reason.
    assert_eq!(
        viewer.request(Request::Close { thread: t }).await.unwrap(),
        Response::Ok
    );
    let swept = daemon.threads().sweep(Duration::ZERO).await;
    assert!(
        !swept.contains(&t),
        "a thread waiting for an answer stays loaded"
    );
    let (again, _) = waiting(&mut steve, t).await;
    assert_eq!(again, call_id, "still the same proposal");
}

/// T3a — `Yes` switches: the log records the switch, then the answer,
/// `project_of` moves, and the state returns to `Idle`.
#[tokio::test]
async fn t3_a_yes_switches_the_project_and_returns_to_idle() {
    let dir = tempfile::tempdir().unwrap();
    let (daemon, _a, _b, _c) = three_projects(dir.path()).await;
    let (mut steve, t) = idle_front_in_a(&daemon).await;
    front_here(&mut steve, "b", Some("b")).await;
    let (call_id, _) = waiting(&mut steve, t).await;

    assert_eq!(
        answer_switch(&mut steve, t, &call_id, SwitchReply::Yes).await,
        Response::Ok
    );
    assert_eq!(daemon.threads().project_of(t).as_deref(), Some("b"));
    assert_eq!(open_state(&mut steve, t).await, ThreadState::Idle);

    let switched = at(daemon.base(), t, EventKind::ProjectSwitched);
    let answered = at(daemon.base(), t, EventKind::DecisionAnswered);
    assert!(
        switched.zip(answered).is_some_and(|(s, a)| s < a),
        "the switch is recorded before the answer"
    );
    let answers = answered_of(daemon.base(), t);
    assert_eq!(answers[0].answer, DecisionAnswer::Yes);
}

/// T3b — `No` and `Corrected` record their answer and leave the
/// project alone.
#[tokio::test]
async fn t3_b_no_and_corrected_record_and_do_not_switch() {
    for reply in [SwitchReply::No, SwitchReply::Corrected { to: "1".into() }] {
        let dir = tempfile::tempdir().unwrap();
        let (daemon, _a, _b, _c) = three_projects(dir.path()).await;
        let (mut steve, t) = idle_front_in_a(&daemon).await;
        front_here(&mut steve, "b", Some("b")).await;
        let (call_id, _) = waiting(&mut steve, t).await;

        assert_eq!(
            answer_switch(&mut steve, t, &call_id, reply.clone()).await,
            Response::Ok,
            "{reply:?}"
        );
        assert_eq!(daemon.threads().project_of(t).as_deref(), Some("a"));
        assert_eq!(open_state(&mut steve, t).await, ThreadState::Idle);
        assert_eq!(at(daemon.base(), t, EventKind::ProjectSwitched), None);
        let answers = answered_of(daemon.base(), t);
        assert_eq!(answers.len(), 1, "{answers:?}");
        assert_ne!(answers[0].answer, DecisionAnswer::Yes, "{reply:?}");
    }
}

/// T3c — a `No` is remembered: a second start-up in the same folder
/// raises nothing, and the state stays `Idle`.
#[tokio::test]
async fn t3_c_a_declined_start_up_is_not_raised_again() {
    let dir = tempfile::tempdir().unwrap();
    let (daemon, _a, _b, _c) = three_projects(dir.path()).await;
    let (mut steve, t) = idle_front_in_a(&daemon).await;
    front_here(&mut steve, "b", Some("b")).await;
    let (call_id, _) = waiting(&mut steve, t).await;
    answer_switch(&mut steve, t, &call_id, SwitchReply::No).await;

    // Unload, so the next Front loads it afresh and would propose.
    daemon.threads().sweep(Duration::ZERO).await;
    let (again, outcome) = front_here(&mut steve, "b", Some("b")).await;
    assert_eq!(outcome, FrontOutcome::Resumed);
    assert_eq!(again, t);
    assert_eq!(open_state(&mut steve, t).await, ThreadState::Idle);
    assert_eq!(proposed_of(daemon.base(), t).len(), 1, "no second proposal");
}

/// T3d — a real switch afterwards clears the decline: the same folder
/// proposes again once the thread has been there and come back.
#[tokio::test]
async fn t3_d_a_real_switch_lets_the_same_folder_propose_again() {
    let dir = tempfile::tempdir().unwrap();
    let (daemon, _a, _b, _c) = three_projects(dir.path()).await;
    let (mut steve, t) = idle_front_in_a(&daemon).await;
    front_here(&mut steve, "b", Some("b")).await;
    let (call_id, _) = waiting(&mut steve, t).await;
    answer_switch(&mut steve, t, &call_id, SwitchReply::No).await;

    // Switch to b by hand, then back to a.
    for to in ["b", "a"] {
        assert_eq!(
            steve
                .request(Request::SwitchProject {
                    thread: t,
                    project: to.into(),
                })
                .await
                .unwrap(),
            Response::Ok,
            "{to}"
        );
    }
    daemon.threads().sweep(Duration::ZERO).await;
    let (again, outcome) = front_here(&mut steve, "b", Some("b")).await;
    assert_eq!(outcome, FrontOutcome::Resumed);
    assert_eq!(again, t);
    let (call_id, project) = waiting(&mut steve, t).await;
    assert_eq!(project, "b");
    assert!(call_id.starts_with(STARTUP_PREFIX), "{call_id}");
    assert_eq!(proposed_of(daemon.base(), t).len(), 2);
}

/// T3e — keep-awake balances: after a yes and after a withdrawal the
/// holds and releases match, with nothing held once the thread is
/// `Idle` (issue #47).
#[tokio::test]
async fn t3_e_the_proposal_never_leaves_the_machine_awake() {
    for reply in [SwitchReply::Yes, SwitchReply::No] {
        let dir = tempfile::tempdir().unwrap();
        let daemon = three_projects(dir.path()).await.0;
        let guard = Recording::new();
        daemon.threads().with_keep_awake(guard.clone());
        let (mut steve, t) = idle_front_in_a(&daemon).await;
        front_here(&mut steve, "b", Some("b")).await;
        let (call_id, _) = waiting(&mut steve, t).await;
        answer_switch(&mut steve, t, &call_id, reply.clone()).await;

        // The last state change settles; the guard must be balanced.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if guard.outstanding() == 0 && open_state(&mut steve, t).await == ThreadState::Idle {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "{reply:?}: {} holds outstanding, {:?}",
                guard.outstanding(),
                guard.calls()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(guard.outstanding(), 0, "{reply:?}: {:?}", guard.calls());
        assert_eq!(
            guard.calls().iter().filter(|c| **c == "hold").count(),
            guard.calls().iter().filter(|c| **c == "release").count(),
            "balanced: {:?}",
            guard.calls()
        );
        assert!(
            guard.calls().contains(&"hold"),
            "{reply:?}: the answer put the guard back up, so releasing it is the \
             thing under test: {:?}",
            guard.calls()
        );
        assert!(
            guard.calls().contains(&"release"),
            "{reply:?}: and it came down again: {:?}",
            guard.calls()
        );
    }
}

/// T4a — a bare folder (`here: None`) raises nothing.
#[tokio::test]
async fn t4_a_a_bare_folder_raises_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (daemon, _a, _b, _c) = three_projects(dir.path()).await;
    let (mut steve, t) = idle_front_in_a(&daemon).await;
    let (again, outcome) = front(&mut steve, "b").await;
    assert_eq!(outcome, FrontOutcome::Resumed);
    assert_eq!(again, t);
    assert_eq!(open_state(&mut steve, t).await, ThreadState::Idle);
    assert!(proposed_of(daemon.base(), t).is_empty());
}

/// T4b — the thread's own project raises nothing.
#[tokio::test]
async fn t4_b_the_threads_own_project_raises_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (daemon, _a, _b, _c) = three_projects(dir.path()).await;
    let (mut steve, t) = idle_front_in_a(&daemon).await;
    front_here(&mut steve, "a", Some("a")).await;
    assert_eq!(open_state(&mut steve, t).await, ThreadState::Idle);
    assert!(proposed_of(daemon.base(), t).is_empty());
}

/// T4c — a folder whose project the daemon does not know raises
/// nothing.
#[tokio::test]
async fn t4_c_an_unknown_folder_project_raises_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (daemon, _a, _b, _c) = three_projects(dir.path()).await;
    let (mut steve, t) = idle_front_in_a(&daemon).await;
    front_here(&mut steve, "b", Some("nowhere")).await;
    assert_eq!(open_state(&mut steve, t).await, ThreadState::Idle);
    assert!(proposed_of(daemon.base(), t).is_empty());
}

/// T4d — a user with only `read` in the folder's project: no proposal.
#[tokio::test]
async fn t4_d_a_reader_of_the_folder_gets_no_proposal() {
    let dir = tempfile::tempdir().unwrap();
    let a = project(dir.path(), "a", "[participants]\nmagnus = \"write\"\n");
    let b = project(dir.path(), "b", "[participants]\nmagnus = \"read\"\n");
    let daemon = Daemon::new(
        dir.path(),
        vec![pc("a", &a), pc("b", &b)],
        &["steve", "magnus"],
        false,
    )
    .await;
    let mut magnus = daemon.connect("magnus").await;
    // Magnus makes his own front thread while he may still write `a`.
    let (t, outcome) = front(&mut magnus, "a").await;
    assert_eq!(outcome, FrontOutcome::First);
    // Then he is reduced to `read` there, which is all a resume needs.
    participants(dir.path(), "a", "[participants]\nmagnus = \"read\"\n");

    let (again, outcome) = front_here(&mut magnus, "a", Some("b")).await;
    assert_eq!(outcome, FrontOutcome::Resumed);
    assert_eq!(again, t);
    assert_eq!(open_state(&mut magnus, t).await, ThreadState::Idle);
    assert!(proposed_of(daemon.base(), t).is_empty());
}

/// T4e — a user who could switch here but cannot write the thread's own
/// project is offered nothing: an unanswerable proposal is never raised.
#[tokio::test]
async fn t4_e_a_user_who_cannot_answer_gets_no_proposal() {
    let dir = tempfile::tempdir().unwrap();
    let a = project(dir.path(), "a", "[participants]\nmagnus = \"write\"\n");
    let b = project(dir.path(), "b", "[participants]\nmagnus = \"write\"\n");
    let daemon = Daemon::new(
        dir.path(),
        vec![pc("a", &a), pc("b", &b)],
        &["steve", "magnus"],
        false,
    )
    .await;
    let mut magnus = daemon.connect("magnus").await;
    let (t, outcome) = front(&mut magnus, "a").await;
    assert_eq!(outcome, FrontOutcome::First);
    // He may write `b`, but only read `a`, the thread's own project.
    participants(dir.path(), "a", "[participants]\nmagnus = \"read\"\n");

    let (again, outcome) = front_here(&mut magnus, "a", Some("b")).await;
    assert_eq!(outcome, FrontOutcome::Resumed);
    assert_eq!(again, t);
    assert_eq!(open_state(&mut magnus, t).await, ThreadState::Idle);
    assert!(
        proposed_of(daemon.base(), t).is_empty(),
        "nobody could answer it"
    );
}

/// T4f — an outcome other than `Resumed` raises nothing: a first front
/// thread, and a replacement.
#[tokio::test]
async fn t4_f_first_and_replaced_raise_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (daemon, _a, _b, _c) = three_projects(dir.path()).await;
    let mut steve = daemon.connect("steve").await;
    let (t, outcome) = front_here(&mut steve, "b", Some("c")).await;
    assert_eq!(outcome, FrontOutcome::First);
    assert_eq!(open_state(&mut steve, t).await, ThreadState::Idle);
    assert!(proposed_of(daemon.base(), t).is_empty());

    // A front thread in a project this daemon does not know is replaced,
    // not resumed; the new one is in `b`, and `here` is still not its
    // project, so nothing is proposed.
    let other = tempfile::tempdir().unwrap();
    let old = project(other.path(), "old", "");
    let id = Ulid::generate();
    let mut log = hand_log(&other.path().join("threads"), id);
    append_started(&mut log, Some("old"), &old, "steve", true);
    let b = project(other.path(), "b", "");
    let c = project(other.path(), "c", "");
    let daemon = Daemon::new(
        other.path(),
        vec![pc("b", &b), pc("c", &c)],
        &["steve"],
        false,
    )
    .await;
    let mut steve = daemon.connect("steve").await;
    let (t, outcome) = front_here(&mut steve, "b", Some("c")).await;
    let FrontOutcome::Replaced { reason } = outcome else {
        panic!("a replacement: {outcome:?}")
    };
    assert!(reason.contains("does not know"), "{reason}");
    assert_ne!(t, id, "not the thread in the unknown project");
    assert_eq!(open_state(&mut steve, t).await, ThreadState::Idle);
    assert!(proposed_of(daemon.base(), t).is_empty());
}

/// T4g — a thread that is already waiting for an approval, or running,
/// is not offered a switch.
#[tokio::test]
async fn t4_g_a_busy_thread_is_not_offered_a_switch() {
    let dir = tempfile::tempdir().unwrap();
    let a = project(dir.path(), "a", "");
    let b = project(dir.path(), "b", "");
    // First script: a turn that asks for a tool call and stops there, so
    // the thread waits. Second: one that never answers, so it runs.
    let factory = ScriptedFactory::new(vec![
        Some(vec![
            ProviderEvent::ToolCall(ToolCall {
                id: "c1".into(),
                name: "write_file".into(),
                args: serde_json::json!({"path": "x", "content": "y"}),
            }),
            ProviderEvent::Done {
                finish_reason: "tool_use".into(),
            },
        ]),
        None,
    ]);
    let daemon = Daemon::new_with(
        dir.path(),
        vec![pc("a", &a), pc("b", &b)],
        &["steve"],
        false,
        factory,
    )
    .await;
    let mut steve = daemon.connect("steve").await;
    let (t, _) = front(&mut steve, "a").await;

    // A tool call that writes waits for a person: `AwaitingApproval`.
    post(&mut steve, t, "do it").await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if matches!(
            open_state(&mut steve, t).await,
            ThreadState::AwaitingApproval { .. }
        ) {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "a waiting turn");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let (_, outcome) = front_here(&mut steve, "b", Some("b")).await;
    assert_eq!(
        outcome,
        FrontOutcome::Resumed,
        "the front thread is the same"
    );
    assert!(proposed_of(daemon.base(), t).is_empty(), "already waiting");

    // Now a running turn: the second script never answers.
    let dir = tempfile::tempdir().unwrap();
    let a = project(dir.path(), "a", "");
    let b = project(dir.path(), "b", "");
    let factory = ScriptedFactory::new(vec![None]);
    let daemon = Daemon::new_with(
        dir.path(),
        vec![pc("a", &a), pc("b", &b)],
        &["steve"],
        false,
        factory,
    )
    .await;
    let mut steve = daemon.connect("steve").await;
    let (t, _) = front(&mut steve, "a").await;
    post(&mut steve, t, "go").await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if matches!(open_state(&mut steve, t).await, ThreadState::Running { .. }) {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "a running turn");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let (_, outcome) = front_here(&mut steve, "b", Some("b")).await;
    assert_eq!(outcome, FrontOutcome::Resumed);
    assert!(proposed_of(daemon.base(), t).is_empty(), "already running");
}

/// T5a — a message withdraws the proposal, with its note, before the
/// turn's own `user_message`, and the turn runs.
#[tokio::test]
async fn t5_a_a_message_withdraws_the_proposal_before_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let (daemon, _a, _b, _c) = three_projects(dir.path()).await;
    let (mut steve, t) = idle_front_in_a(&daemon).await;
    front_here(&mut steve, "b", Some("b")).await;
    let (call_id, _) = waiting(&mut steve, t).await;

    assert_eq!(post(&mut steve, t, "hello").await, Response::Ok);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if at(daemon.base(), t, EventKind::UserMessage).is_some() {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "the turn started");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let withdrawn = at(daemon.base(), t, EventKind::DecisionAnswered).expect("withdrawn");
    let message = at(daemon.base(), t, EventKind::UserMessage).expect("the message");
    assert!(withdrawn < message, "the withdrawal comes first");
    let answers = answered_of(daemon.base(), t);
    assert_eq!(answers[0].answer, DecisionAnswer::Withdrawn);
    assert_eq!(
        answers[0].note.as_deref(),
        Some("a message was sent instead")
    );

    // The stale call id is refused, and nothing new is written.
    let before = answered_of(daemon.base(), t).len();
    let refused = answer_switch(&mut steve, t, &call_id, SwitchReply::Yes).await;
    assert!(matches!(refused, Response::Refused { .. }), "{refused:?}");
    assert_eq!(answered_of(daemon.base(), t).len(), before);
}

/// T5d — the proposal is answered by another path (here, the person
/// answers `No`): the message that follows is never refused for it, and
/// no second `decision_answered` is written. The withdrawal itself is a
/// no-op then, and a withdrawal that fails must never refuse a message.
#[tokio::test]
async fn t5_d_a_message_is_never_refused_by_the_proposal() {
    let dir = tempfile::tempdir().unwrap();
    let (daemon, _a, _b, _c) = three_projects(dir.path()).await;
    let (mut steve, t) = idle_front_in_a(&daemon).await;
    front_here(&mut steve, "b", Some("b")).await;
    let (call_id, _) = waiting(&mut steve, t).await;

    // Another path answers it first: the proposal is settled and gone.
    let answered = answer_switch(&mut steve, t, &call_id, SwitchReply::No).await;
    assert_eq!(answered, Response::Ok, "{answered:?}");
    assert_eq!(answered_of(daemon.base(), t).len(), 1);
    assert_eq!(open_state(&mut steve, t).await, ThreadState::Idle);

    // The message runs its turn, and the earlier answer stays the only one.
    assert_eq!(post(&mut steve, t, "hello").await, Response::Ok);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if at(daemon.base(), t, EventKind::UserMessage).is_some() {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "the turn started");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        answered_of(daemon.base(), t).len(),
        1,
        "the answer stays the only one"
    );
}

/// T5b — a skill withdraws it too, with its own note, before the turn.
#[tokio::test]
async fn t5_b_a_skill_withdraws_the_proposal() {
    let dir = tempfile::tempdir().unwrap();
    let (daemon, _a, _b, _c) = three_projects(dir.path()).await;
    let (mut steve, t) = idle_front_in_a(&daemon).await;
    front_here(&mut steve, "b", Some("b")).await;
    waiting(&mut steve, t).await;

    assert_eq!(
        steve
            .request(Request::InvokeSkill {
                thread: t,
                name: "any".into(),
                args: "{}".to_owned(),
            })
            .await
            .unwrap(),
        Response::Ok
    );
    let answers = answered_of(daemon.base(), t);
    assert_eq!(answers[0].answer, DecisionAnswer::Withdrawn);
    assert_eq!(answers[0].note.as_deref(), Some("a skill was run instead"));
}

/// T5c — a hand switch withdraws it, with its own note, and the switch
/// still happens.
#[tokio::test]
async fn t5_c_a_hand_switch_withdraws_the_proposal() {
    let dir = tempfile::tempdir().unwrap();
    let (daemon, _a, _b, _c) = three_projects(dir.path()).await;
    let guard = Recording::new();
    daemon.threads().with_keep_awake(guard.clone());
    let (mut steve, t) = idle_front_in_a(&daemon).await;
    front_here(&mut steve, "b", Some("b")).await;
    waiting(&mut steve, t).await;

    assert_eq!(
        steve
            .request(Request::SwitchProject {
                thread: t,
                project: "c".into(),
            })
            .await
            .unwrap(),
        Response::Ok
    );
    let answers = answered_of(daemon.base(), t);
    assert_eq!(answers[0].answer, DecisionAnswer::Withdrawn);
    assert_eq!(
        answers[0].note.as_deref(),
        Some("the project was switched by hand")
    );
    assert_eq!(daemon.threads().project_of(t).as_deref(), Some("c"));
    assert_eq!(open_state(&mut steve, t).await, ThreadState::Idle);

    // No turn ran, so only the withdrawal's own hold can be here: it must
    // be balanced, with the machine released once the thread is idle.
    assert_eq!(guard.outstanding(), 0, "{:?}", guard.calls());
    assert_eq!(
        guard.calls().iter().filter(|c| **c == "hold").count(),
        guard.calls().iter().filter(|c| **c == "release").count(),
        "balanced: {:?}",
        guard.calls()
    );
    assert!(
        guard.calls().contains(&"hold"),
        "the withdrawal put the guard up, so bringing it down is the thing \
         under test: {:?}",
        guard.calls()
    );
}

// ---------------------------------------------------------------------------
// T4–T6 (issue #86): one listing across every project the caller can read
// ---------------------------------------------------------------------------

/// The listing with no project: every thread the caller can read, in the
/// order `/threads` will show them.
async fn across(client: &mut Client) -> Vec<ThreadInfo> {
    match client
        .request(Request::ListThreads { project: None })
        .await
        .unwrap()
    {
        Response::Threads { threads } => threads,
        other => panic!("a cross-project listing: {other:?}"),
    }
}

/// Newest first, the index's order: the ULID is the creation.
fn newest_first(ids: &[Ulid]) -> Vec<Ulid> {
    let mut ids = ids.to_vec();
    ids.sort_unstable_by(|a, b| b.cmp(a));
    ids
}

/// T4 — `None` lists every project the caller can read, and only those:
/// the caller's front thread, then the builds, then the rest newest
/// first.
#[tokio::test]
async fn t86_t4_a_listing_across_projects_through_a_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let p = project(
        dir.path(),
        "p",
        "[participants]\nsteve = \"read\"\nanna = \"write\"\n",
    );
    let q = project(
        dir.path(),
        "q",
        "[participants]\nsteve = \"write\"\nanna = \"write\"\n",
    );
    let r = project(dir.path(), "r", "[participants]\nanna = \"write\"\n");
    let base = dir.path().join("threads");
    // Hand-made ids, so the fixture's order does not depend on how fast
    // the test runs; all of them are older than any thread the daemon
    // makes below.
    let mk = |n: u64| Ulid::from_parts(1_000 + n, 0);
    let plain_p = mk(1);
    let lead = mk(2);
    let child = mk(3);
    let switched_in = mk(4);
    let switched_out = mk(5);
    let annas_front = mk(6);

    let mut log = hand_log(&base, plain_p);
    append_started(&mut log, Some("p"), &p, "anna", false);
    drop(log);

    // A build in `q`: its lead, and the step child it leads.
    let mut log = hand_log(&base, lead);
    append_started(&mut log, Some("q"), &q, "steve", false);
    append_run_started(&mut log, 86);
    drop(log);
    let mut log = hand_log(&base, child);
    append_child_started(&mut log, Some("q"), &q, lead, "implement");
    drop(log);

    // Switched into `p` from a project he cannot read: listed, under `p`.
    let mut log = hand_log(&base, switched_in);
    append_started(&mut log, Some("r"), &r, "anna", false);
    append_switch(&mut log, "p", &p);
    drop(log);

    // Switched the other way: out of his reach, so not listed at all.
    let mut log = hand_log(&base, switched_out);
    append_started(&mut log, Some("p"), &p, "anna", false);
    append_switch(&mut log, "r", &r);
    drop(log);

    // Another person's front thread: for steve it is an ordinary row.
    let mut log = hand_log(&base, annas_front);
    append_started(&mut log, Some("q"), &q, "anna", true);
    drop(log);

    let daemon = Daemon::new(
        dir.path(),
        vec![pc("p", &p), pc("q", &q), pc("r", &r)],
        &["steve", "anna"],
        false,
    )
    .await;
    let mut steve_client = daemon.connect("steve").await;
    let (front_row, _) = front(&mut steve_client, "q").await;

    let rows = across(&mut steve_client).await;
    let ids: Vec<Ulid> = rows.iter().map(|r| r.id).collect();

    assert!(
        !ids.contains(&switched_out) && !rows.iter().any(|r| r.project.as_deref() == Some("r")),
        "nothing from a project he cannot read: {rows:?}"
    );
    assert!(
        rows.iter().all(|r| r.project.is_some()),
        "every listed row has a project: {rows:?}"
    );
    assert_eq!(
        rows.iter()
            .find(|r| r.id == switched_in)
            .unwrap()
            .project
            .as_deref(),
        Some("p"),
        "a switched thread is in its current project"
    );

    // The order, from the fixture: his front thread, the lead and the
    // child it leads, then the plain rows newest first.
    let mut expected = vec![front_row, lead, child];
    expected.extend(newest_first(&[plain_p, switched_in, annas_front]));
    assert_eq!(ids, expected, "{rows:?}");

    assert_eq!(rows[0].kind, ThreadKind::Front);
    assert_eq!(rows[1].kind, ThreadKind::Run(RunThread::Lead { issue: 86 }));
    assert_eq!(
        rows[2].kind,
        ThreadKind::Run(RunThread::Child {
            lead,
            step: Some("implement".into()),
        })
    );
    assert_eq!(
        rows.iter().find(|r| r.id == annas_front).unwrap().kind,
        ThreadKind::Thread,
        "another person's front thread is not steve's"
    );
    assert!(
        rows.iter()
            .filter(|r| r.kind == ThreadKind::Front)
            .all(|r| r.id == front_row),
        "one front row, his own: {rows:?}"
    );
}

/// T5a — a user with no role anywhere is listed nothing, and a user who
/// reads one project sees exactly that project.
#[tokio::test]
async fn t86_t5a_users_without_a_role_see_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let p = project(
        dir.path(),
        "p",
        "[participants]\nsteve = \"write\"\nreada = \"read\"\n",
    );
    let q = project(dir.path(), "q", "[participants]\nsteve = \"write\"\n");
    let base = dir.path().join("threads");
    let in_p = Ulid::from_parts(1_000, 0);
    let in_q = Ulid::from_parts(1_001, 0);
    let mut log = hand_log(&base, in_p);
    append_started(&mut log, Some("p"), &p, "steve", false);
    drop(log);
    let mut log = hand_log(&base, in_q);
    append_started(&mut log, Some("q"), &q, "steve", false);
    drop(log);

    let daemon = Daemon::new(
        dir.path(),
        vec![pc("p", &p), pc("q", &q)],
        &["steve", "reada", "maggie"],
        false,
    )
    .await;

    // No role anywhere: nothing across projects, and one project refuses.
    let mut maggie = daemon.connect("maggie").await;
    assert!(across(&mut maggie).await.is_empty());
    let reason = refusal(
        &mut maggie,
        Request::ListThreads {
            project: Some("p".into()),
        },
    )
    .await;
    assert!(reason.contains("role"), "{reason}");

    // A reader in `p` alone: exactly `p`'s threads, in both shapes.
    let mut reada = daemon.connect("reada").await;
    assert_eq!(
        across(&mut reada)
            .await
            .iter()
            .map(|r| r.id)
            .collect::<Vec<_>>(),
        vec![in_p]
    );
    assert_eq!(listed(&mut reada, "p").await, vec![in_p]);
}

/// T5b — a thread with no project is never listed, whoever asks: a
/// pre-phase-4 log and a `_none` thread whose name another project
/// holds at a different root.
#[tokio::test]
async fn t86_t5b_threads_with_no_project_are_never_listed() {
    let dir = tempfile::tempdir().unwrap();
    let p = project(dir.path(), "p", "[participants]\nsteve = \"write\"\n");
    // A project named `p` at another root: the clashing `_none` thread's
    // home is not this daemon's `p`, so `project_of` cannot place it.
    let elsewhere = tempfile::tempdir().unwrap();
    let other_p = project(elsewhere.path(), "p", "");
    let base = dir.path().join("threads");
    let in_p = Ulid::from_parts(1_000, 0);
    let pre_phase_4 = Ulid::from_parts(1_001, 0);
    let clash = Ulid::from_parts(1_002, 0);

    let mut log = hand_log(&base, in_p);
    append_started(&mut log, Some("p"), &p, "steve", false);
    drop(log);
    // A pre-phase-4 log: a user message and nothing else.
    let mut log = hand_log(&base, pre_phase_4);
    log.append(NewEvent {
        kind: EventKind::UserMessage,
        author: Author::User(UserId("steve".into())),
        payload: to_value(aigentic_runtime::aigentic_log::UserMessagePayload::new(
            vec![ContentBlock::Text("hello".into())],
        ))
        .unwrap(),
        parent_event: None,
    })
    .unwrap();
    drop(log);
    let mut log = hand_log(&base, clash);
    append_started(&mut log, Some("_none"), &other_p, "steve", false);
    drop(log);

    let daemon = Daemon::new(dir.path(), vec![pc("p", &p)], &["steve"], false).await;
    let mut steve_client = daemon.connect("steve").await;
    assert_eq!(
        daemon.threads().project_of(pre_phase_4),
        None,
        "the pre-phase-4 log has no project"
    );
    assert_eq!(
        daemon.threads().project_of(clash),
        None,
        "the `_none` clash"
    );

    let ids: Vec<Ulid> = across(&mut steve_client)
        .await
        .iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(ids, vec![in_p], "neither unplaceable log is listed");
}

/// T5c — a front thread in a project whose role the caller has lost is
/// not listed, and gives no `Front` row: `Front` replaces it, and the
/// listing must not resurrect it.
#[tokio::test]
async fn t86_t5c_a_lost_role_leaves_no_front_row() {
    let dir = tempfile::tempdir().unwrap();
    let p = project(dir.path(), "p", "[participants]\nsteve = \"write\"\n");
    let q = project(dir.path(), "q", "[participants]\nsteve = \"write\"\n");
    let base = dir.path().join("threads");
    let in_q = Ulid::from_parts(1_000, 0);
    let mut log = hand_log(&base, in_q);
    append_started(&mut log, Some("q"), &q, "steve", false);
    drop(log);

    let daemon = Daemon::new(
        dir.path(),
        vec![pc("p", &p), pc("q", &q)],
        &["steve"],
        false,
    )
    .await;
    let mut steve_client = daemon.connect("steve").await;
    let (front_row, _) = front(&mut steve_client, "p").await;
    // The file is edited under him: no role in `p` any more. It keeps a
    // participant, or `p` would fall back to "the owner has a role".
    participants(dir.path(), "p", "[participants]\nanna = \"write\"\n");

    let rows = across(&mut steve_client).await;
    assert!(
        !rows.iter().any(|r| r.id == front_row),
        "his old front thread is out of his sight: {rows:?}"
    );
    assert!(
        rows.iter().all(|r| r.kind != ThreadKind::Front),
        "and no row claims to be his front thread: {rows:?}"
    );
    assert_eq!(
        rows.iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![in_q],
        "the thread in the project he still writes in"
    );
}

/// T6 — the workspace names group rows: the projects it names carry its
/// name and sit together, and a project outside it is in the `None`
/// group. The file is written before the daemon, because workspaces
/// load once, at construction.
#[tokio::test]
async fn t86_t6_workspace_names_group_rows() {
    let dir = tempfile::tempdir().unwrap();
    let p = project(dir.path(), "p", "[participants]\nsteve = \"write\"\n");
    let q = project(dir.path(), "q", "[participants]\nsteve = \"write\"\n");
    let s = project(dir.path(), "s", "[participants]\nsteve = \"read\"\n");
    let name = "w";
    workspace_naming(dir.path(), name, &[&p, &q]);
    let base = dir.path().join("threads");
    let in_p = Ulid::from_parts(1_000, 0);
    let in_q = Ulid::from_parts(1_001, 0);
    let outside = Ulid::from_parts(1_002, 0);
    for (id, project_name, root) in [(&in_p, "p", &p), (&in_q, "q", &q), (&outside, "s", &s)] {
        let mut log = hand_log(&base, *id);
        append_started(&mut log, Some(project_name), root, "steve", false);
        drop(log);
    }

    let daemon = Daemon::new(
        dir.path(),
        vec![pc("p", &p), pc("q", &q), pc("s", &s)],
        &["steve"],
        false,
    )
    .await;
    let mut steve_client = daemon.connect("steve").await;
    let rows = across(&mut steve_client).await;

    for row in &rows {
        if row.project.as_deref() == Some("s") {
            assert_eq!(row.workspace, None, "outside the workspace");
        } else {
            assert_eq!(row.workspace.as_deref(), Some(name));
        }
    }
    // The group `w` is one run of rows, newest first, and so is the
    // group of no workspace; the groups go by their newest row, newest
    // first, so `in_p` and `in_q` stay adjacent.
    let w_group = newest_first(&[in_p, in_q]);
    let none_group = vec![outside];
    let mut groups = [w_group, none_group];
    groups.sort_by(|a, b| b[0].cmp(&a[0]));
    let expected: Vec<Ulid> = groups.concat();
    let ids: Vec<Ulid> = rows.iter().map(|r| r.id).collect();
    assert_eq!(ids, expected, "{rows:?}");

    // T7's new assertion: the `Some(p)` listing fills `kind` and
    // `workspace` too.
    let one_project = listed(&mut steve_client, "p").await;
    assert_eq!(one_project, vec![in_p]);
    let rows = match steve_client
        .request(Request::ListThreads {
            project: Some("p".into()),
        })
        .await
        .unwrap()
    {
        Response::Threads { threads } => threads,
        other => panic!("a listing: {other:?}"),
    };
    assert!(
        rows.iter()
            .all(|r| r.kind == ThreadKind::Thread && r.workspace.as_deref() == Some(name)),
        "{rows:?}"
    );
}

// ---------------------------------------------------------------------------
// T4–T5 (issue #121): the start-up ask's answer rides `Front`
// ---------------------------------------------------------------------------

/// The ask's answer, as the client sends it after `start_ask` ran: the
/// offered project, the chosen one, and the reason the offer carried.
async fn front_asked(
    client: &mut Client,
    offered: &str,
    chosen: &str,
    reason: &str,
) -> (Ulid, FrontOutcome) {
    match client
        .request(Request::Front {
            project: chosen.into(),
            here: Some(chosen.to_owned()),
            asked: Some(aigentic_api::StartAsk {
                offered: offered.into(),
                chosen: chosen.into(),
                reason: reason.into(),
            }),
        })
        .await
        .unwrap()
    {
        Response::Front { thread, outcome } => (thread.id, outcome),
        other => panic!("a front thread: {other:?}"),
    }
}

/// The `decision_proposed`/`decision_answered` pair a start-up ask
/// leaves in `t`'s log, or nothing where the server logged none.
fn ask_pair(
    base: &Path,
    t: Ulid,
) -> (
    DecisionProposedPayload,
    Option<(DecisionAnsweredPayload, Option<Ulid>)>,
) {
    let events = events_of(base, t);
    let proposed = events
        .iter()
        .filter(|e| e.kind == EventKind::DecisionProposed)
        .map(|e| {
            (
                e.id,
                serde_json::from_value::<DecisionProposedPayload>(e.payload.clone()).unwrap(),
            )
        })
        .find(|(_, p)| aigentic_runtime::aigentic_log::is_startup_call(p.call_id.as_deref()))
        .expect("a start-up proposal");
    let answered = events
        .iter()
        .find(|e| e.kind == EventKind::DecisionAnswered && e.parent_event == Some(proposed.0))
        .map(|e| {
            (
                serde_json::from_value::<DecisionAnsweredPayload>(e.payload.clone()).unwrap(),
                e.parent_event,
            )
        });
    (proposed.1, answered)
}

/// T4 — a `Resumed` front thread in another project: the answer moves it
/// there, raises no switch proposal, and the pair lands after the move.
#[tokio::test]
async fn t4_ask_a_resumed_thread_moves_to_the_chosen_project_and_logs_the_pair() {
    let dir = tempfile::tempdir().unwrap();
    let (daemon, _a, _b, _c) = three_projects(dir.path()).await;
    let mut steve = daemon.connect("steve").await;
    let (t, outcome) = front(&mut steve, "a").await;
    assert_eq!(outcome, FrontOutcome::First);

    let (again, outcome) = front_asked(&mut steve, "a", "b", "the ask says so").await;
    assert_eq!(outcome, FrontOutcome::Resumed, "the reply's own outcome");
    assert_eq!(again, t, "the same front thread, moved");

    // Moved: `b` lists it now, and `a` does not.
    assert!(listed(&mut steve, "b").await.contains(&t));
    assert!(!listed(&mut steve, "a").await.contains(&t));

    // The pair, and nothing else: no `AwaitingSwitch` was left behind.
    let (proposed, answered) = ask_pair(daemon.base(), t);
    assert_eq!(proposed.kind, DecisionKind::Project);
    assert_eq!(
        proposed.target, None,
        "the ask declines nothing: {proposed:?}"
    );
    assert!(
        proposed
            .call_id
            .as_deref()
            .is_some_and(|c| c.starts_with(&format!(
                "{}ask-",
                aigentic_runtime::aigentic_log::STARTUP_PREFIX
            ))),
        "{proposed:?}"
    );
    assert_eq!(proposed.reason, "the ask says so");
    let (answered, parent) = answered.expect("an answer to the ask");
    assert_eq!(answered.answer, DecisionAnswer::Corrected);
    assert_eq!(answered.correction.as_deref(), Some("b"));
    assert_eq!(
        parent,
        Some(
            events_of(daemon.base(), t)
                .iter()
                .find(|e| e.kind == EventKind::DecisionProposed)
                .expect("the proposal")
                .id
        ),
        "the answer names its proposal"
    );

    // After the move, so the fold's scope is the new project.
    let switch = at(daemon.base(), t, EventKind::ProjectSwitched).expect("a switch");
    let proposal = at(daemon.base(), t, EventKind::DecisionProposed).expect("a proposal");
    assert!(switch < proposal, "{switch} before {proposal}");

    // Nothing is waiting on the person's answer.
    assert_eq!(open_state(&mut steve, t).await, ThreadState::Idle);
    assert_eq!(proposed_of(daemon.base(), t).len(), 1, "one, and only one");
}

/// T4 — `chosen == offered` logs `Yes` and does not move the thread.
#[tokio::test]
async fn t4_ask_b_choosing_the_offered_project_logs_yes_and_does_not_move() {
    let dir = tempfile::tempdir().unwrap();
    let (daemon, _a, _b, _c) = three_projects(dir.path()).await;
    let mut steve = daemon.connect("steve").await;
    let (t, _) = front(&mut steve, "a").await;

    let (again, outcome) = front_asked(&mut steve, "a", "a", "the ask says so").await;
    assert_eq!(outcome, FrontOutcome::Resumed);
    assert_eq!(again, t);

    assert!(listed(&mut steve, "a").await.contains(&t), "it stayed in a");
    assert!(at(daemon.base(), t, EventKind::ProjectSwitched).is_none());
    let (proposed, answered) = ask_pair(daemon.base(), t);
    assert_eq!(proposed.target, None);
    let (answered, _) = answered.expect("an answer to the ask");
    assert_eq!(answered.answer, DecisionAnswer::Yes);
    assert_eq!(answered.correction, None);
}

/// T4 — `target: None` declines nothing: a later #92 start-up proposal
/// for the project the ask steered away from is still offered.
#[tokio::test]
async fn t4_ask_c_the_answer_declines_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (daemon, _a, _b, _c) = three_projects(dir.path()).await;
    let (mut steve, t) = idle_front_in_a(&daemon).await;

    // The ask took the person from `a` to `b`: a `Corrected` for `a`.
    let (again, outcome) = front_asked(&mut steve, "a", "b", "the ask says so").await;
    assert_eq!(outcome, FrontOutcome::Resumed);
    assert_eq!(again, t);
    let (_, answered) = ask_pair(daemon.base(), t);
    assert_eq!(
        answered.expect("an answer").0.answer,
        DecisionAnswer::Corrected
    );

    // Now a start from `a`'s folder: #92's proposal for `a` is raised
    // anyway, because the ask's own record names no target.
    let (again, outcome) = front_here(&mut steve, "a", Some("a")).await;
    assert_eq!(outcome, FrontOutcome::Resumed);
    assert_eq!(again, t);
    let (call_id, project) = waiting(&mut steve, t).await;
    assert_eq!(project, "a");
    assert!(call_id.starts_with(STARTUP_PREFIX), "{call_id}");
}

/// T4 — a busy thread ignores the answer: no move, no pair, resumed as
/// today, for a thread running and for one waiting on a person.
#[tokio::test]
async fn t4_ask_d_a_busy_thread_ignores_the_answer() {
    for waiting_on_a_person in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let a = project(dir.path(), "a", "");
        let b = project(dir.path(), "b", "");
        // First script: a tool call that waits for a person. Second (and
        // for the other leg, first): a turn that never answers.
        let script = if waiting_on_a_person {
            vec![
                Some(vec![
                    ProviderEvent::ToolCall(ToolCall {
                        id: "c1".into(),
                        name: "write_file".into(),
                        args: serde_json::json!({"path": "x", "content": "y"}),
                    }),
                    ProviderEvent::Done {
                        finish_reason: "tool_use".into(),
                    },
                ]),
                None,
            ]
        } else {
            vec![None]
        };
        let daemon = Daemon::new_with(
            dir.path(),
            vec![pc("a", &a), pc("b", &b)],
            &["steve"],
            false,
            ScriptedFactory::new(script),
        )
        .await;
        let mut steve = daemon.connect("steve").await;
        let (t, _) = front(&mut steve, "a").await;
        post(&mut steve, t, "go").await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let state = open_state(&mut steve, t).await;
            let busy = match &state {
                ThreadState::Running { .. } => !waiting_on_a_person,
                ThreadState::AwaitingApproval { .. } => waiting_on_a_person,
                _ => false,
            };
            if busy {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "a busy turn");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let (again, outcome) = front_asked(&mut steve, "a", "b", "the ask says so").await;
        assert_eq!(outcome, FrontOutcome::Resumed);
        assert_eq!(again, t);
        assert!(
            listed(&mut steve, "a").await.contains(&t),
            "a busy thread is not moved"
        );
        assert!(
            proposed_of(daemon.base(), t).is_empty(),
            "and no pair is written"
        );
    }
}

/// T4 — the move works with the thread not loaded yet, the case a
/// just-started daemon is in.
#[tokio::test]
async fn t4_ask_e_an_unloaded_thread_still_moves() {
    let dir = tempfile::tempdir().unwrap();
    let (daemon, _a, _b, _c) = three_projects(dir.path()).await;
    let (mut steve, t) = idle_front_in_a(&daemon).await;

    let (again, outcome) = front_asked(&mut steve, "a", "b", "the ask says so").await;
    assert_eq!(outcome, FrontOutcome::Resumed);
    assert_eq!(again, t);
    assert!(listed(&mut steve, "b").await.contains(&t));
    let (_, answered) = ask_pair(daemon.base(), t);
    assert_eq!(
        answered.expect("an answer").0.answer,
        DecisionAnswer::Corrected
    );
}

/// T4 — the answer needs `write` in the chosen project: a `read`-only
/// user is refused with the answer's own shape, and nothing moves.
#[tokio::test]
async fn t4_ask_f_without_write_in_the_chosen_project_it_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let a = project(
        dir.path(),
        "a",
        "[participants]\nsteve = \"write\"\ncara = \"write\"\n",
    );
    let b = project(
        dir.path(),
        "b",
        "[participants]\nsteve = \"write\"\ncara = \"read\"\n",
    );
    let daemon = Daemon::new(
        dir.path(),
        vec![pc("a", &a), pc("b", &b)],
        &["steve", "cara"],
        false,
    )
    .await;
    let mut cara = daemon.connect("cara").await;
    let (t, _) = front(&mut cara, "a").await;

    let reason = refusal(
        &mut cara,
        Request::Front {
            project: "b".into(),
            here: Some("b".into()),
            asked: Some(aigentic_api::StartAsk {
                offered: "a".into(),
                chosen: "b".into(),
                reason: "the ask says so".into(),
            }),
        },
    )
    .await;
    assert_eq!(
        reason, "in b: cara is read in this project; this needs write",
        "the `AnswerSwitch` shape"
    );
    assert!(listed(&mut cara, "a").await.contains(&t), "still in a");
    assert!(
        proposed_of(daemon.base(), t).is_empty(),
        "a refusal logs nothing"
    );
}

/// T4 — `First` creates the front thread in the chosen project and logs
/// the pair there.
#[tokio::test]
async fn t4_ask_g_first_creates_in_the_chosen_project_and_logs_the_pair() {
    let dir = tempfile::tempdir().unwrap();
    let (daemon, _a, _b, _c) = three_projects(dir.path()).await;
    let mut steve = daemon.connect("steve").await;

    let (t, outcome) = front_asked(&mut steve, "a", "b", "the ask says so").await;
    assert_eq!(outcome, FrontOutcome::First);
    assert_eq!(
        front_of(daemon.base(), t).project.as_deref(),
        Some("b"),
        "created where the person chose"
    );
    assert!(listed(&mut steve, "b").await.contains(&t));
    let (proposed, answered) = ask_pair(daemon.base(), t);
    assert_eq!(proposed.target, None);
    let (answered, _) = answered.expect("an answer to the ask");
    assert_eq!(answered.answer, DecisionAnswer::Corrected);
    assert_eq!(answered.correction.as_deref(), Some("b"));
    assert_eq!(open_state(&mut steve, t).await, ThreadState::Idle);
}
