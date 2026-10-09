//! Runs over the daemon (issue #58): the runner made reachable.
//!
//! Every test drives the real daemon — `session::serve` over a socket, the
//! real `ServerHost`, the real `build_thread` — with two seams replaced:
//! the provider is scripted (and performs the child's git work, as the
//! spec's tests do between events) and the forge, repo and installer are
//! fakes. The workflow the daemon resolves is a `build` workflow written
//! into the temporary *bundled* root, whose implementer checks only E1–E3:
//! E4 runs a real `cargo` gate, which belongs to the acceptance run, not
//! to a unit test.
//!
//! Every expected value is derived from what the fixtures wrote: the
//! commit subjects come from the brief's `commits` slot, the counted
//! prompts and pushes from the logs the run left behind.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aigentic_api::client::{Addr, Client};
use aigentic_api::{CheckpointAnswer, Notice, Request, Response};
use aigentic_runtime::aigentic_core::{
    Author, Capabilities, CompletionRequest, Event, EventKind, Message, Provider, ProviderEvent,
    RiskClass, ToolCall,
};
use aigentic_runtime::aigentic_log::{
    Repair, RunOutcome, StepStartedPayload, ThreadLog, ThreadStartedPayload,
};
use aigentic_runtime::runner::RunnerHost;
use aigentic_runtime::runner::forge::{FakeForge, Forge, IssueView};
use aigentic_runtime::runner::git::{GitRepo, Repo};
use aigentic_runtime::runner::install::{FakeInstaller, Installer};
use aigentic_runtime::workflow::{WorkflowFile, WorkflowOrigin};
use aigentic_runtime::{CancelToken, Verdict};
use aigentic_server::build::Root;
use aigentic_server::config::{Config, ProjectConfig, ServerConfig, UserConfig};
use aigentic_server::runs::RunDeps;
use aigentic_server::{Listener, NoReports, Server};
use futures_core::Stream;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use ulid::Ulid;

mod common;

// ---------------------------------------------------------------------------
// The scripts
// ---------------------------------------------------------------------------

/// One act of a scripted thread: a git side effect the provider performs
/// before it answers, or the events it streams as one reply.
///
/// The git work lives here because a running daemon has nobody to do it
/// between events: the child's step commits must exist in the repository
/// before the checks read the range, and the child itself only ever
/// streams scripted replies.
#[derive(Clone)]
pub enum Act {
    /// Write `file` with `body`, `git add` it and commit it with
    /// `subject`, a blank line, `body` and the trainer trailer.
    Commit {
        file: String,
        body: String,
        subject: String,
    },
    /// Run this git command in the run's repository.
    Git(Vec<String>),
    /// Stream these events as the reply to one provider call.
    Reply(Vec<ProviderEvent>),
}

/// The scripts a daemon hands out: one per thread it builds, in build
/// order. The queue is shared by every daemon a test starts, so a run
/// resumed by a second daemon continues where the first one stopped.
#[derive(Clone, Default)]
struct Scripts(Arc<Mutex<VecDeque<Vec<Act>>>>);

impl Scripts {
    fn of(scripts: impl IntoIterator<Item = Vec<Act>>) -> Self {
        Scripts(Arc::new(Mutex::new(scripts.into_iter().collect())))
    }

    /// How many scripts are left: zero means the fixture planned too few.
    fn left(&self) -> usize {
        self.0.lock().unwrap().len()
    }
}

/// The scripted provider of one built thread. Each provider call consumes
/// the next act: a `Commit` or `Git` performs the side effect and the call
/// continues; a `Reply` is streamed as that call's answer. A script that
/// runs out pends, which is what a `kill -9` mid-turn looks like.
struct Actor {
    acts: Mutex<VecDeque<Act>>,
    repo: PathBuf,
    /// The model id the trailer names.
    model: String,
}

impl Provider for Actor {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supports_tools: true,
            supports_images: false,
            supports_caching: false,
            supports_structured_output: false,
            max_context_tokens: 1000,
        }
    }

    fn count_tokens(&self, _: &[Message]) -> u64 {
        7
    }

    fn complete(
        &self,
        _: &CompletionRequest<'_>,
    ) -> Pin<Box<dyn Stream<Item = ProviderEvent> + Send + '_>> {
        loop {
            let next = self.acts.lock().unwrap().pop_front();
            match next {
                Some(Act::Reply(events)) => {
                    return Box::pin(futures_util::stream::iter(events));
                }
                Some(Act::Git(args)) => {
                    let args: Vec<&str> = args.iter().map(String::as_str).collect();
                    run_git(&self.repo, &args);
                }
                Some(Act::Commit {
                    file,
                    body,
                    subject,
                }) => commit(&self.repo, &file, &body, &subject, &self.model),
                None => return Box::pin(futures_util::stream::pending()),
            }
        }
    }
}

/// A factory handing out one [`Actor`] per built thread.
struct Actors {
    scripts: Scripts,
    repo: PathBuf,
    model: String,
}

impl aigentic_server::build::ProviderFactory for Actors {
    fn build(
        &self,
        _: &str,
    ) -> Result<(Box<dyn Provider>, String), aigentic_server::build::BuildError> {
        let acts = self
            .scripts
            .0
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_default();
        Ok((
            Box::new(Actor {
                acts: Mutex::new(acts.into()),
                repo: self.repo.clone(),
                model: self.model.clone(),
            }),
            self.model.clone(),
        ))
    }
}

/// Write `file`, add it and commit it with the trailer the E1 check wants.
fn commit(repo: &Path, file: &str, body: &str, subject: &str, model: &str) {
    let path = repo.join(file);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    // Unique content: a resumed step commits the same subjects again, and
    // a commit with nothing staged would fail the script.
    std::fs::write(&path, format!("{file}\n{}\n", Ulid::generate())).unwrap();
    run_git(repo, &["add", file]);
    // The trailer names the model the way the runner renders it: the
    // profile's model without its provider prefix, from the shipped
    // function rather than a hand-written literal.
    let message = format!(
        "{subject}\n\n{body}\n\nCo-Authored-By: aigentic ({}) <332865255+aigentic-bot@users.noreply.github.com>",
        aigentic_runtime::workflow::render::trailer_model(model)
    );
    let path = repo.join(".git-message");
    std::fs::write(&path, message).unwrap();
    run_git(repo, &["commit", "-F", ".git-message"]);
}

fn run_git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

// ---------------------------------------------------------------------------
// The replies
// ---------------------------------------------------------------------------

fn done() -> ProviderEvent {
    ProviderEvent::Done {
        finish_reason: "tool_use".into(),
    }
}

fn usage() -> ProviderEvent {
    ProviderEvent::Usage(aigentic_runtime::aigentic_core::Usage {
        input_tokens: 10,
        output_tokens: 5,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        reasoning_tokens: None,
    })
}

/// One scripted reply that reports a step and ends its turn.
fn report(id: &str, args: Value) -> Act {
    Act::Reply(vec![
        ProviderEvent::ToolCall(ToolCall {
            id: id.into(),
            name: "finish_step".into(),
            args,
        }),
        usage(),
        done(),
    ])
}

/// The subjects the fixture's brief names, oldest first.
const NAMED: [&str; 2] = ["pancake: one", "pancake: two"];

/// The brief's report: the slots the implementer's template needs, and
/// the commit messages E2 compares the step's commits against.
fn brief_report() -> Value {
    json!({
        "status": "done",
        "body": "## Brief\n\nthe brief",
        "slots": {
            "size": "trivial",
            "budget": 3,
            "purpose": "make the runner reachable",
            "must_not_undo": "nothing",
            "pointers": "crates/server/src/runs.rs",
            "design": "a log-driven loop",
            "commits": NAMED,
        },
        "planned_tests": [
            {"id": "T1", "what": "the runner runs", "derivation": "from the spec"},
        ],
    })
}

/// The brief's report for a `full` run: the route gate asks the human
/// before any step starts, which is where a run waits with nobody
/// driving it.
fn brief_report_full() -> Value {
    let mut report = brief_report();
    report["slots"]["size"] = json!("full");
    report["slots"]["budget"] = json!(4);
    report
}

/// The gate the lead is waiting at, from its log.
fn gate_of(events: &[Event]) -> Option<String> {
    events
        .iter()
        .filter(|event| event.kind == EventKind::CheckpointAsked)
        .filter_map(|event| {
            serde_json::from_value::<aigentic_runtime::aigentic_log::CheckpointAskedPayload>(
                event.payload.clone(),
            )
            .ok()
        })
        .next_back()
        .map(|asked| asked.gate)
}

/// The implementer's report.
fn implementer_report() -> Value {
    json!({
        "status": "done",
        "body": "## Implementation\n\nit landed",
        "slots": {"size": "trivial"},
        "release_impact": "patch",
    })
}

/// The commits the brief named, with the trailer E1 wants, then the
/// implementer's report. The two commits are made in the step, so the
/// checks read a range of two.
fn implementer_script() -> Vec<Act> {
    vec![
        Act::Commit {
            file: "pancake-one.txt".into(),
            body: "the first".into(),
            subject: NAMED[0].into(),
        },
        Act::Commit {
            file: "pancake-two.txt".into(),
            body: "the second".into(),
            subject: NAMED[1].into(),
        },
        report("r2", implementer_report()),
    ]
}

/// The whole trivial path: the brief reports, the implementer commits and
/// reports, and the run closes.
fn happy_scripts() -> Scripts {
    Scripts::of([vec![report("r1", brief_report())], implementer_script()])
}

// ---------------------------------------------------------------------------
// The fixture
// ---------------------------------------------------------------------------

/// A daemon over a temporary project: a real git repository with a bare
/// remote, its own threads directory, a config, and a bundled `build`
/// workflow whose implementer checks only E1–E3.
struct Daemon {
    fixture: Fixture,
    scripts: Scripts,
    deps: Arc<FakeDeps>,
    server: Arc<Server>,
    socket: PathBuf,
    _task: tokio::task::JoinHandle<Result<(), aigentic_server::ServerError>>,
}

/// The files a daemon serves: everything a restart keeps. A test that
/// kills one daemon and starts another hands the same fixture over.
struct Fixture {
    dir: tempfile::TempDir,
    repo: PathBuf,
    remote: PathBuf,
    threads: PathBuf,
    cfg_dir: PathBuf,
    bundled: PathBuf,
}

/// The seams a run drives the world through, faked: the forge holds one
/// issue and every comment; the repo is the fixture's; the installer
/// reports a version without installing anything.
struct FakeDeps {
    forge: Arc<FakeForge>,
    repo: PathBuf,
    installer: FakeInstaller,
}

impl RunDeps for FakeDeps {
    fn forge(&self) -> Arc<dyn Forge + Send + Sync> {
        self.forge.clone()
    }

    fn repo(&self, _: &Root) -> Box<dyn Repo + Send + Sync> {
        Box::new(GitRepo::new(&self.repo))
    }

    fn installer(&self) -> Box<dyn Installer + Send + Sync> {
        Box::new(self.installer.clone())
    }
}

impl FakeDeps {
    /// How many comments the run posted.
    fn comments(&self) -> Vec<String> {
        self.forge.comments.lock().unwrap().clone()
    }
}

/// The model the `flash` profile resolves to, so the E1 trailer is the
/// one the commits carry.
const MODEL: &str = "tensorx/deepseek-v4.1-flash";

impl Fixture {
    /// The files for a project `p`: a repository with a bare remote and
    /// its own threads, config and bundled workflows.
    fn new(comments: Vec<String>) -> Self {
        let _ = comments;
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let remote = dir.path().join("remote.git");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&remote).unwrap();
        init_git(&repo, &remote);
        std::fs::write(
            repo.join("aigentic.toml"),
            "[project]\nname = \"p\"\n[participants]\nsteve = \"approve\"\nmagnus = \"approve\"\nreviewer = \"read\"\nviewer = \"read\"\n[memory]\nenabled = false\n",
        )
        .unwrap();
        run_git(&repo, &["add", "aigentic.toml"]);
        run_git(&repo, &["commit", "-m", "the project file"]);
        run_git(&repo, &["push", "origin", "main"]);

        let threads = dir.path().join("threads");
        let cfg_dir = dir.path().join("cfg");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let bundled = dir.path().join("bundled");
        write_test_workflow(&bundled);
        Self {
            dir,
            repo,
            remote,
            threads,
            cfg_dir,
            bundled,
        }
    }

    /// The same files under a second fixture: the same repository,
    /// threads directory, config directory and bundled workflows, and a
    /// fresh scratch directory for its socket. Two daemons over one
    /// project need two fixtures, because a fixture owns the directory it
    /// made (#58 fix 2's cross-process case, in one test process).
    fn twin(&self) -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
            repo: self.repo.clone(),
            remote: self.remote.clone(),
            threads: self.threads.clone(),
            cfg_dir: self.cfg_dir.clone(),
            bundled: self.bundled.clone(),
        }
    }

    /// A daemon over these files: `scripts` hand out one thread's acts
    /// each, and with `resume` the start-up scan claims unfinished runs.
    async fn daemon(self, scripts: Scripts, resume: bool) -> Daemon {
        self.daemon_with(scripts, resume, Vec::new()).await
    }

    /// The same, with comments the forge already holds.
    async fn daemon_with(self, scripts: Scripts, resume: bool, comments: Vec<String>) -> Daemon {
        let config = Config::parse(&format!(
            "threads_dir = {:?}\nbundled_dir = {:?}\ndefault_profile = \"flash\"\n[profiles.brief]\nbase_url = \"u\"\nmodel = \"tensorx/kimi-k2\"\napi_key_env = \"K\"\n[profiles.flash]\nbase_url = \"u\"\nmodel = {:?}\napi_key_env = \"K\"\n",
            self.threads.display(),
            self.bundled.display(),
            MODEL,
        ))
        .unwrap();
        let server_cfg = ServerConfig {
            listen: "unix".into(),
            idle_unload_secs: 3600,
            users: ["steve", "magnus", "reviewer", "viewer"]
                .iter()
                .map(|n| UserConfig {
                    name: (*n).to_owned(),
                    token_env: None,
                    token: Some(format!("tok-{n}")),
                })
                .collect(),
            projects: vec![ProjectConfig {
                name: "p".into(),
                root: self.repo.clone(),
            }],
            resume_runs: resume,
        };
        let deps = Arc::new(FakeDeps {
            forge: Arc::new(FakeForge::with_comments(
                IssueView {
                    title: "a trivial issue".into(),
                    body: "make the runner reachable".into(),
                },
                comments,
            )),
            repo: self.repo.clone(),
            installer: FakeInstaller::echoing_head(),
        });
        let server = Arc::new(Server::new(
            config,
            self.cfg_dir.clone(),
            server_cfg,
            Arc::new(Actors {
                scripts: scripts.clone(),
                repo: self.repo.clone(),
                model: MODEL.into(),
            }),
            Arc::new(NoReports),
        ));
        server.threads.with_run_deps(deps.clone());
        let socket = self.dir.path().join(format!("d{}.sock", Ulid::generate()));
        let task = tokio::spawn(server.clone().serve(Listener::Unix(socket.clone())));
        for _ in 0..400 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        Daemon {
            fixture: self,
            scripts,
            deps,
            server,
            socket,
            _task: task,
        }
    }
}

impl Daemon {
    /// A daemon with `scripts`, serving the project `p`, with the
    /// start-up scan `resume` or not.
    async fn new(scripts: Scripts, resume: bool) -> Self {
        Self::with_comments(scripts, resume, Vec::new()).await
    }

    /// The same, with comments the forge already holds.
    async fn with_comments(scripts: Scripts, resume: bool, comments: Vec<String>) -> Self {
        Fixture::new(comments.clone())
            .daemon_with(scripts, resume, comments)
            .await
    }

    /// Stop the daemon as a killed process stops: its task is aborted and
    /// the files are handed back, so a second daemon can be started over
    /// the same logs, repository and threads directory.
    async fn stop(self) -> Fixture {
        // A killed process takes its advisory locks with it: ending the
        // run tasks drops the futures that hold them (#58 fix 2).
        self.server.threads.runs().stop_tasks().await;
        self._task.abort();
        // The task stops at its next await point. A moment later the run
        // is parked on the child's turn — the provider's script is spent,
        // so its stream never yields again — and the logs are quiet.
        tokio::time::sleep(Duration::from_millis(200)).await;
        self.fixture
    }

    async fn connect(&self, user: &str) -> (Client, aigentic_api::Welcome) {
        Client::connect(&Addr::Unix(self.socket.clone()), &format!("tok-{user}"))
            .await
            .unwrap()
    }

    /// The lead's log, read with repair as the daemon does.
    fn lead_log(&self, lead: Ulid) -> ThreadLog {
        ThreadLog::open_with(self.fixture.threads.clone(), lead, Repair::TruncateTornTail)
            .unwrap()
            .0
    }

    fn lead_events(&self, lead: Ulid) -> Vec<Event> {
        self.lead_log(lead).events().to_vec()
    }

    fn child_events(&self, child: Ulid) -> Vec<Event> {
        self.events(child)
    }

    /// Any thread's log, read with repair.
    fn events(&self, thread: Ulid) -> Vec<Event> {
        ThreadLog::open_with(
            self.fixture.threads.clone(),
            thread,
            Repair::TruncateTornTail,
        )
        .unwrap()
        .0
        .events()
        .to_vec()
    }

    /// Wait until `ready` holds, up to five seconds, then say whether it
    /// did.
    async fn wait_until(&self, mut ready: impl FnMut() -> bool) -> bool {
        for _ in 0..500 {
            if ready() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    }

    /// The notes the daemon sent about `lead`, drained from `notices`.
    fn notes(notices: &mut mpsc::Receiver<Notice>, lead: Ulid) -> Vec<String> {
        let mut notes = Vec::new();
        while let Ok(notice) = notices.try_recv() {
            if let Notice::Note { thread, text } = notice
                && thread == lead
            {
                notes.push(text);
            }
        }
        notes
    }

    /// Wait for the lead's run to end, and answer with its outcome.
    async fn wait_finished(&self, lead: Ulid) -> RunOutcome {
        let finished = self
            .wait_until(|| {
                self.lead_events(lead)
                    .iter()
                    .any(|event| event.kind == EventKind::RunFinished)
            })
            .await;
        if !finished {
            let events = self.lead_events(lead);
            panic!(
                "the run finished; the lead holds:\n{}",
                events
                    .iter()
                    .map(|e| format!(
                        "  {:?} {}",
                        e.kind,
                        &serde_json::to_string(&e.payload).unwrap_or_default()
                            [..serde_json::to_string(&e.payload)
                                .unwrap_or_default()
                                .len()
                                .min(200)]
                    ))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
        }
        let event = self
            .lead_events(lead)
            .into_iter()
            .find(|event| event.kind == EventKind::RunFinished)
            .unwrap();
        let payload: aigentic_runtime::aigentic_log::RunFinishedPayload =
            serde_json::from_value(event.payload).unwrap();
        payload.outcome
    }

    /// The child a step started, from the lead log.
    fn child_of(&self, lead: Ulid, step: &str, attempt: u32) -> Option<Ulid> {
        self.lead_events(lead)
            .iter()
            .filter(|event| event.kind == EventKind::StepStarted)
            .filter_map(|event| {
                serde_json::from_value::<StepStartedPayload>(event.payload.clone()).ok()
            })
            .find(|started| started.step == step && started.attempt == attempt)
            .map(|started| started.child_thread)
    }

    /// What the remote holds for `main`, or `None` before the first push.
    fn remote_head(&self) -> Option<String> {
        let text = run_git(&self.fixture.remote, &["rev-parse", "main"]);
        (!text.is_empty()).then_some(text)
    }

    /// The head the repository is at now.
    fn head(&self) -> String {
        run_git(&self.fixture.repo, &["rev-parse", "HEAD"])
    }
}

impl Fixture {
    /// A thread's log under this fixture's threads directory, read with
    /// repair as the daemon does.
    fn events(&self, thread: Ulid) -> Vec<Event> {
        ThreadLog::open_with(self.threads.clone(), thread, Repair::TruncateTornTail)
            .unwrap()
            .0
            .events()
            .to_vec()
    }
}

/// How many prompts the child's log holds: one per turn it was given.
fn prompts(events: &[Event]) -> usize {
    events
        .iter()
        .filter(|event| event.kind == EventKind::UserMessage)
        .count()
}

/// Tear the log's tail: every complete line stays, and a half-written
/// line sits after it with no trailing newline, as a `kill -9` between
/// an append and its flush leaves.
fn tear_tail(dir: &Path, thread: Ulid) {
    use std::io::Write as _;
    let path = dir.join(format!("{thread}.jsonl"));
    let text = std::fs::read_to_string(&path).unwrap();
    let cut = text.rfind('\n').map_or(text.len(), |i| i + 1);
    let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.set_len(cut as u64).unwrap();
    file.flush().unwrap();
    drop(file);
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    file.write_all(b"{\"ki").unwrap();
    file.flush().unwrap();
}

/// A work repository on `main` with one commit, pushed to a bare remote.
fn init_git(repo: &Path, remote: &Path) {
    run_git(repo, &["init", "-b", "main"]);
    run_git(repo, &["config", "user.name", "aigentic test"]);
    run_git(repo, &["config", "user.email", "test@example.invalid"]);
    std::fs::write(repo.join("README.md"), "the repo\n").unwrap();
    run_git(repo, &["add", "README.md"]);
    run_git(repo, &["commit", "-m", "the first commit"]);
    run_git(remote, &["init", "--bare", "-b", "main"]);
    run_git(repo, &["remote", "add", "origin", remote.to_str().unwrap()]);
    run_git(repo, &["push", "origin", "main"]);
}

/// Write the bundled `build` workflow: the repository's own workflow with
/// the implementer's checks cut to E1–E3, so no step runs a real gate.
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
        "the test workflow never runs the real gate"
    );
    std::fs::write(dest.join("workflow.toml"), toml).unwrap();
    for template in ["brief.md", "implementer.md"] {
        std::fs::copy(
            source.join("templates").join(template),
            dest.join("templates").join(template),
        )
        .unwrap();
    }
    let _ = bundled;
}

// ---------------------------------------------------------------------------
// T1, T2: the host
// ---------------------------------------------------------------------------

/// The world a run is driven in, for the host tests that need no daemon.
fn world(daemon: &Daemon) -> aigentic_server::runs::RunWorld {
    // The run's world points at the flat threads directory (issue #9):
    // children are written beside their lead, at `<base>/<id>.jsonl`.
    daemon
        .server
        .threads
        .run_world("p", daemon.fixture.threads.clone())
        .unwrap()
}

/// T1 — `create_child` is idempotent: one `thread_started` with
/// `parent_thread` and `step`; a second call adds nothing; an empty file
/// gets the header.
#[tokio::test]
async fn t1_create_child_is_idempotent() {
    let daemon = Daemon::new(Scripts::default(), false).await;
    let lead = Ulid::generate();
    let child = Ulid::generate();
    let mut host = aigentic_server::runs::ServerHost::new(world(&daemon), lead);

    // An empty file first: a `kill -9` between creating the file and
    // writing its header leaves one. Flat, like every log since #9.
    std::fs::create_dir_all(&daemon.fixture.threads).unwrap();
    std::fs::write(daemon.fixture.threads.join(format!("{child}.jsonl")), "").unwrap();
    host.create_child(child, "implement-alone").unwrap();
    host.create_child(child, "implement-alone").unwrap();

    let events = daemon.child_events(child);
    let started: Vec<ThreadStartedPayload> = events
        .iter()
        .filter(|event| event.kind == EventKind::ThreadStarted)
        .map(|event| serde_json::from_value(event.payload.clone()).unwrap())
        .collect();
    assert_eq!(started.len(), 1, "one header, however many calls");
    assert_eq!(started[0].parent_thread, Some(lead));
    assert_eq!(started[0].step.as_deref(), Some("implement-alone"));
    assert_eq!(started[0].project.as_deref(), Some("p"));
}

/// T2 — `build_child` returns a runtime in `Auto` mode, with the step's
/// deny overlay (a `git push` call is refused) and the step's profile.
#[tokio::test]
async fn t2_build_child_is_auto_with_the_step_deny_overlay() {
    let daemon = Daemon::new(Scripts::default(), false).await;
    let lead = Ulid::generate();
    let child = Ulid::generate();
    let mut host = aigentic_server::runs::ServerHost::new(world(&daemon), lead);
    host.create_child(child, "implement-alone").unwrap();

    let runtime = host
        .build_child(child, "flash", "implement-alone", &["git push".to_owned()])
        .await
        .expect("the child's runtime builds");

    assert_eq!(runtime.mode(), aigentic_runtime::Mode::Auto);
    assert_eq!(runtime.identity().0.as_deref(), Some("flash"));
    let mut runtime = runtime;
    let verdict = runtime
        .policy_check(
            &ToolCall {
                id: "c1".into(),
                name: "bash".into(),
                args: json!({"command": "git push origin main"}),
            },
            RiskClass::Write,
            &CancelToken::never(),
            &mut |_| {},
        )
        .await
        .expect("the seam answers");
    let Verdict::Refuse(record) = verdict else {
        panic!(
            "the step's deny overlay refuses a push, and nothing else may allow it: {verdict:?}"
        );
    };
    assert_eq!(
        record,
        aigentic_runtime::aigentic_log::PolicyRecord::rule_with_reason(
            "step deny git push",
            "deny",
            "the runner pushes once after its checks (PLAN-layer2 §6.4); commit and call finish_step",
        )
    );
}
// ---------------------------------------------------------------------------
// T3, T4: a build over the wire
// ---------------------------------------------------------------------------

/// The lead a `Build` answered, and the notices the client saw so far,
/// for the tests that watch a run.
struct Built {
    lead: Ulid,
    resumed: bool,
}

impl Daemon {
    /// Send `Build` as `user` and answer with the lead the daemon named.
    async fn build(&self, client: &Client, _user: &str, issue: u64) -> Built {
        match client
            .request(Request::Build {
                project: "p".into(),
                issue,
                workflow: None,
            })
            .await
            .unwrap()
        {
            Response::Run { lead, resumed } => Built { lead, resumed },
            other => panic!("a build is answered with a run: {other:?}"),
        }
    }

    /// Drain `notices` until `ready` holds of everything seen for
    /// `lead`, or five seconds pass: the events, in order.
    async fn collect(
        notices: &mut mpsc::Receiver<Notice>,
        lead: Ulid,
        mut ready: impl FnMut(&[Event]) -> bool,
    ) -> Vec<Event> {
        let mut events = Vec::new();
        for _ in 0..500 {
            while let Ok(notice) = notices.try_recv() {
                if let Notice::Event { thread, event } = notice
                    && thread == lead
                {
                    events.push(event);
                }
            }
            if ready(&events) {
                return events;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        events
    }
}

/// T3 — `Build` answers `Run { resumed: false }`; the lead's log starts
/// `thread_started`, `run_started { issue, content_hash }`; the client
/// receives the lead's events as `Notice::Event`.
#[tokio::test]
async fn t3_build_makes_a_lead_and_streams_it() {
    let daemon = Daemon::new(happy_scripts(), false).await;
    let (client, _welcome) = daemon.connect("steve").await;
    let mut notices = client.take_notices().expect("the notice stream");

    let built = daemon.build(&client, "steve", 58).await;
    assert!(!built.resumed, "a fresh issue is not a resume");

    // The log starts with the two headers the daemon writes, in order.
    let events = daemon.lead_events(built.lead);
    let kinds: Vec<EventKind> = events.iter().map(|event| event.kind).collect();
    assert_eq!(kinds[0], EventKind::ThreadStarted);
    assert_eq!(kinds[1], EventKind::RunStarted);
    let started: ThreadStartedPayload = serde_json::from_value(events[0].payload.clone()).unwrap();
    assert_eq!(started.project.as_deref(), Some("p"));
    let run: aigentic_runtime::aigentic_log::RunStartedPayload =
        serde_json::from_value(events[1].payload.clone()).unwrap();
    assert_eq!(run.issue, 58);
    assert_eq!(run.workflow, "build");
    assert!(!run.content_hash.is_empty(), "the workflow is hashed");
    // The budget is the workflow file's own `budget.full`, not a literal.
    let loaded = WorkflowFile::load_dir(
        &daemon.fixture.bundled.join("workflows").join("build"),
        WorkflowOrigin::Bundled,
    )
    .expect("the bundled build workflow loads");
    assert_eq!(run.budget_usd, loaded.workflow.budget.full);
    assert_eq!(run.version, loaded.workflow.version);
    assert_eq!(run.content_hash, loaded.content_hash);

    // The same events arrive as notices, the lead named on each: the live
    // stream the daemon broadcasts, which is the log's tail in order
    // (what was written before the subscription is the log's, read back
    // with `Open` — rule 5a).
    let outcome = daemon.wait_finished(built.lead).await;
    assert_eq!(outcome, RunOutcome::Closed, "the trivial path closes");
    let streamed = Daemon::collect(&mut notices, built.lead, |seen| {
        seen.iter()
            .any(|event| event.kind == EventKind::RunFinished)
    })
    .await;
    assert!(!streamed.is_empty(), "the client is subscribed to the lead");
    let log = daemon.lead_events(built.lead);
    assert_eq!(
        streamed,
        log[log.len() - streamed.len()..],
        "every notice is one of the lead's own events, in order"
    );
}

/// T18 — a gate that opens while a client watches reaches it as a
/// notice, before the run waits for the answer: a client answers only a
/// `checkpoint_asked` it has received, so the daemon must not wait first
/// (found by slice 1's acceptance run: `aigentic build` hung at a live
/// gate). The answer is then acted on and the run finishes, streamed.
#[tokio::test]
async fn t18_a_live_gate_is_streamed_before_the_run_waits() {
    let scripts = Scripts::of([vec![report("r1", brief_report_full())]]);
    let daemon = Daemon::new(scripts, false).await;
    let (client, _welcome) = daemon.connect("steve").await;
    let mut notices = client.take_notices().expect("the notice stream");

    let built = daemon.build(&client, "steve", 58).await;
    let streamed = Daemon::collect(&mut notices, built.lead, |seen| {
        seen.iter()
            .any(|event| event.kind == EventKind::CheckpointAsked)
    })
    .await;
    assert!(
        streamed
            .iter()
            .any(|event| event.kind == EventKind::CheckpointAsked),
        "the gate arrives as a notice while the run waits at it: {:?}",
        streamed.iter().map(|e| e.kind).collect::<Vec<_>>()
    );

    let answered = client
        .request(Request::AnswerCheckpoint {
            lead: built.lead,
            gate: "route".to_owned(),
            answer: CheckpointAnswer::Stop,
            amendment: None,
        })
        .await
        .unwrap();
    assert!(
        matches!(answered, Response::Ok),
        "the answer is taken: {answered:?}"
    );
    let rest = Daemon::collect(&mut notices, built.lead, |seen| {
        seen.iter()
            .any(|event| event.kind == EventKind::RunFinished)
    })
    .await;
    assert!(
        rest.iter()
            .any(|event| event.kind == EventKind::RunFinished),
        "the run finishes after the answer, streamed"
    );
    assert_eq!(daemon.wait_finished(built.lead).await, RunOutcome::Stopped);
}

/// T4 — the full trivial path over the server, with the daemon resolving
/// the **bundled** `build` workflow: `Build` → brief → implement → checks
/// → push (the bare remote advances) → close → `run_finished { Closed }`,
/// streamed.
#[tokio::test]
async fn t4_the_trivial_path_closes_over_the_server() {
    let daemon = Daemon::new(happy_scripts(), false).await;
    let before = daemon.head();
    let (client, _welcome) = daemon.connect("steve").await;
    let mut notices = client.take_notices().expect("the notice stream");

    let built = daemon.build(&client, "steve", 58).await;
    let streamed = Daemon::collect(&mut notices, built.lead, |seen| {
        seen.iter()
            .any(|event| event.kind == EventKind::RunFinished)
    })
    .await;
    assert_eq!(
        daemon.wait_finished(built.lead).await,
        RunOutcome::Closed,
        "the trivial path closes"
    );

    // The workflow is the bundled one, and it is the lead's recorded
    // name: `build`, from the project, user or bundled roots.
    let run: aigentic_runtime::aigentic_log::RunStartedPayload =
        serde_json::from_value(daemon.lead_events(built.lead)[1].payload.clone()).unwrap();
    assert_eq!(run.workflow, "build");

    // One step per step, one attempt each: the brief then the
    // implementer, which closed what it did.
    assert!(
        daemon.child_of(built.lead, "brief", 1).is_some(),
        "the brief ran"
    );
    let implementer = daemon
        .child_of(built.lead, "implement-alone", 1)
        .expect("the implementer ran");
    assert!(
        daemon.child_of(built.lead, "implement-alone", 2).is_none(),
        "the checks passed, so the step was not sent back"
    );

    // The checks that ran are the step's own list, all passing.
    let checks: aigentic_runtime::aigentic_log::ChecksRunPayload = serde_json::from_value(
        daemon
            .lead_events(built.lead)
            .into_iter()
            .find(|event| event.kind == EventKind::ChecksRun)
            .expect("the implementer's step ran its checks")
            .payload,
    )
    .unwrap();
    let ids: Vec<&str> = checks
        .checks
        .iter()
        .map(|check| check.id.as_str())
        .collect();
    assert_eq!(ids, ["E1", "E2", "E3"], "the step's own list, in order");
    assert!(
        checks
            .checks
            .iter()
            .all(|check| check.result == aigentic_runtime::aigentic_log::CheckResult::Pass),
        "every check passed: {:?}",
        checks
            .checks
            .iter()
            .map(|check| (&check.id, &check.detail))
            .collect::<Vec<_>>()
    );

    // The implementer's work is on the remote: the bare remote moved off
    // the head the run started at, and matches the repository's head.
    assert_eq!(
        daemon.remote_head().as_deref(),
        Some(daemon.head().as_str()),
        "the run pushed what it committed"
    );
    assert_ne!(daemon.remote_head(), Some(before), "the remote advanced");

    // The forge closed the issue, and the implementer's report is on it:
    // the step's own `## Implementation`.
    assert!(daemon.deps.forge.is_closed(), "the run closed the issue");
    let posted = daemon.deps.comments();
    assert!(
        posted
            .iter()
            .any(|body| body.starts_with("## Implementation")),
        "the implementer's report was posted: {posted:?}"
    );

    // The installer ran, and the run's outcome is in the stream.
    assert!(
        daemon.deps.installer.calls() > 0,
        "the binary was installed"
    );
    assert_eq!(
        streamed.last().map(|event| event.kind),
        Some(EventKind::RunFinished),
        "the outcome reached the client"
    );
    // The child's own log holds the prompt the step was given, which is
    // how a resumed run knows the step already started.
    assert!(
        daemon
            .child_events(implementer)
            .iter()
            .any(|event| event.kind == EventKind::UserMessage),
        "the step's prompt is in the child's log"
    );
}

// ---------------------------------------------------------------------------
// T5–T7, T12, T13: restarts, the scan, and the one-writer rule
// ---------------------------------------------------------------------------

/// The workflows a resumed run needs, and how a run left mid-child is
/// rebuilt: the lead log holds `step_started` and the child holds a turn
/// that never ended.
///
/// A run killed mid-child leaves the child's prompt written and its
/// answer missing. The resumed run re-awaits that step, finds the child's
/// log lacking a `finish_step`, and posts a fresh prompt: one more
/// prompt, then the script finishes. So the scripts for a resume are the
/// child's remaining acts, and the run needs no second `step_started`.
/// The rest of the implementer's turn, after the kill: the commits are
/// already in the repository, so the resumed child writes the second one
/// and reports.
fn resumed_scripts() -> Scripts {
    Scripts::of([vec![
        Act::Commit {
            file: "pancake-two.txt".into(),
            body: "the second".into(),
            subject: NAMED[1].into(),
        },
        report("r2", implementer_report()),
    ]])
}

/// T5 — two concurrent `Build`s for one issue, and a `Build` during the
/// start-up scan → one `run_started`, one `Runner` (one `step_started`
/// per attempt), the losers `resumed: true`.
#[tokio::test]
async fn t5_one_runner_per_lead() {
    // A run already waiting at its "route" gate: the `full` route asks
    // before it starts a step, so the lead sits unfinished with one
    // `step_started` (the brief) and no task holding it once the first
    // daemon is gone.
    let waiting = Scripts::of([
        vec![report("r1", brief_report_full())],
        implementer_script(),
    ]);
    let first = Daemon::new(waiting.clone(), false).await;
    let (client, _) = first.connect("steve").await;
    let built = first.build(&client, "steve", 58).await;
    assert!(
        first
            .wait_until(|| gate_of(&first.lead_events(built.lead)).as_deref() == Some("route"))
            .await,
        "the run waits at the route gate"
    );
    // The first daemon is gone: the run is unfinished and nobody drives
    // it. The next daemon's start-up scan claims it.
    let files = first.stop().await;

    let daemon = files.daemon(waiting, true).await;
    let (first, _) = daemon.connect("steve").await;
    let (second, _) = daemon.connect("magnus").await;

    // Two `Build`s at once: the scan and the first of them are already in
    // flight when the second arrives.
    let (one, two) = tokio::join!(
        daemon.build(&first, "steve", 58),
        daemon.build(&second, "magnus", 58)
    );
    assert_eq!(one.lead, built.lead, "the same run, not a second one");
    assert_eq!(two.lead, built.lead, "and the same for the second build");
    assert!(
        one.resumed && two.resumed,
        "both attached to the scan's run"
    );

    // One lead log holds one `run_started`, whatever claimed it, and the
    // run stayed where it was: one `step_started` per attempt.
    let events = daemon.lead_events(built.lead);
    assert_eq!(
        events
            .iter()
            .filter(|e| e.kind == EventKind::RunStarted)
            .count(),
        1,
        "one run_started, however many builds"
    );
    let attempts: Vec<String> = events
        .iter()
        .filter(|e| e.kind == EventKind::StepStarted)
        .filter_map(|e| serde_json::from_value::<StepStartedPayload>(e.payload.clone()).ok())
        .map(|started| format!("{}#{}", started.step, started.attempt))
        .collect();
    assert_eq!(attempts, vec!["brief#1"], "one runner stepped the run once");
    assert_eq!(
        daemon.scripts.left(),
        1,
        "the implementer's script is untouched"
    );

    // The answer that ends it: `stop` needs no model call.
    let stopped = first
        .request(Request::AnswerCheckpoint {
            lead: built.lead,
            gate: "route".to_owned(),
            answer: CheckpointAnswer::Stop,
            amendment: None,
        })
        .await
        .unwrap();
    assert!(
        matches!(stopped, Response::Ok),
        "the stop is accepted: {stopped:?}"
    );
    assert_eq!(daemon.wait_finished(built.lead).await, RunOutcome::Stopped);
}

/// T6 — killed mid-child (the lead's `step_started` has no
/// `step_finished`; the child holds a partial turn): a new `Build`
/// continues it once (one prompt, one push, one close).
#[tokio::test]
async fn t6_a_build_resumes_a_run_left_mid_child() {
    // The daemon that dies: its implementer writes one commit and then
    // never answers, as a `kill -9` mid-turn leaves the run.
    let dying = Scripts::of([
        vec![report("r1", brief_report())],
        vec![Act::Commit {
            file: "pancake-one.txt".into(),
            body: "the first".into(),
            subject: NAMED[0].into(),
        }],
    ]);
    let first = Daemon::new(dying, false).await;
    let (client, _) = first.connect("steve").await;
    let built = first.build(&client, "steve", 58).await;
    let child = first
        .child_of(built.lead, "implement-alone", 1)
        .expect("the implementer started");
    assert!(
        first
            .wait_until(|| first
                .child_events(child)
                .iter()
                .any(|e| e.kind == EventKind::UserMessage))
            .await,
        "the child's prompt is written before the kill"
    );
    assert!(
        first
            .child_events(child)
            .iter()
            .all(|event| event.kind != EventKind::StepFinished),
        "the kill left the step open"
    );
    // The child's log, the lead's log, the repo and the threads survive
    // the process: the fixture's directory does, and the new daemon is
    // handed the same ones.
    let files = first.stop().await;
    let resumed = files.daemon(resumed_scripts(), false).await;
    let (client, _) = resumed.connect("steve").await;
    let _notices = client.take_notices().expect("the notice stream");
    let again = resumed.build(&client, "steve", 58).await;
    assert_eq!(again.lead, built.lead, "the same lead, resumed");
    assert!(again.resumed, "the build attached to the unfinished run");
    assert_eq!(resumed.wait_finished(again.lead).await, RunOutcome::Closed);

    // One prompt per turn the child ran, and the lead holds one step and
    // one attempt: the resume continued the step rather than starting it.
    assert!(
        resumed.child_of(again.lead, "implement-alone", 2).is_none(),
        "no second attempt"
    );
    assert_eq!(
        prompts(&resumed.child_events(child)),
        1,
        "one prompt for the step: the resume continues the child's turn"
    );
    assert_eq!(resumed.deps.installer.calls(), 1, "one install, one close");
    assert!(
        resumed.deps.forge.is_closed(),
        "the resume closed the issue"
    );
}

/// T6b — the same with a **torn last line** in the child's log and in the
/// lead's: resumes and finishes, with no `run stopped` note.
#[tokio::test]
async fn t6b_a_torn_tail_is_repaired_and_the_run_finishes() {
    let dying = Scripts::of([
        vec![report("r1", brief_report())],
        vec![Act::Commit {
            file: "pancake-one.txt".into(),
            body: "the first".into(),
            subject: NAMED[0].into(),
        }],
    ]);
    let first = Daemon::new(dying, false).await;
    let (client, _) = first.connect("steve").await;
    let built = first.build(&client, "steve", 58).await;
    let child = first
        .child_of(built.lead, "implement-alone", 1)
        .expect("the implementer started");
    assert!(
        first
            .wait_until(|| first
                .child_events(child)
                .iter()
                .any(|e| e.kind == EventKind::UserMessage))
            .await,
        "the child's prompt is written before the kill"
    );
    let files = first.stop().await;

    // A half-written line at the end of both logs: bytes that were never
    // a complete event, as a `kill -9` between flush and fsync leaves.
    tear_tail(&files.threads, child);
    tear_tail(&files.threads, built.lead);

    // The torn lead is still read as an unfinished run, by the same
    // lookup the daemon uses.
    let (repaired, cut) =
        ThreadLog::open_with(files.threads.clone(), built.lead, Repair::TruncateTornTail).unwrap();
    assert!(cut.is_some(), "the tail was cut");
    assert!(
        aigentic_server::runs::unfinished(&repaired, 58).is_some(),
        "the repaired lead is an unfinished run"
    );
    let daemon = files.daemon(resumed_scripts(), false).await;
    let (client, _) = daemon.connect("steve").await;
    let mut notices = client.take_notices().expect("the notice stream");
    let again = daemon.build(&client, "steve", 58).await;
    assert!(again.resumed, "the torn tails are read, not refused");
    assert_eq!(daemon.wait_finished(again.lead).await, RunOutcome::Closed);

    // No note says the run stopped: the repair is not a failure.
    let notes = Daemon::notes(&mut notices, again.lead);
    assert!(
        !notes.iter().any(|text| text.contains("run stopped")),
        "nothing stopped the run: {notes:?}"
    );
}

/// T6c — during the child's turn, a client `Open` on the child returns
/// its log and starts no actor; a `Post` on it is refused; the child's
/// log gains no other writer.
#[tokio::test]
async fn t6c_a_run_owned_child_is_read_only() {
    let daemon = Daemon::new(happy_scripts(), false).await;
    let (client, _) = daemon.connect("steve").await;
    let built = daemon.build(&client, "steve", 58).await;
    let child = daemon
        .child_of(built.lead, "implement-alone", 1)
        .expect("the implementer started");

    // Open the child mid-turn: the log, and no actor.
    let opened = match client
        .request(Request::Open {
            thread: child,
            from_seq: 0,
        })
        .await
        .unwrap()
    {
        Response::Opened { run, events, .. } => (run, events),
        other => panic!("a run's child opens read-only: {other:?}"),
    };
    assert_eq!(
        opened.0,
        Some(aigentic_api::RunThread::Child {
            lead: built.lead,
            step: Some("implement-alone".into()),
        }),
        "the state says the thread is a run's child"
    );
    assert!(
        opened
            .1
            .iter()
            .any(|event| event.kind == EventKind::ThreadStarted),
        "the child's header comes back"
    );
    assert!(
        daemon.server.threads.mailbox(child).is_none(),
        "no actor was started for the run's child"
    );

    // A post is refused, and nothing was written.
    let before = daemon.child_events(child).len();
    let refused = match client
        .request(Request::Post {
            thread: child,
            blocks: vec![aigentic_runtime::aigentic_core::ContentBlock::Text(
                "hello".into(),
            )],
            interrupt: false,
        })
        .await
        .unwrap()
    {
        Response::Refused { reason } => reason,
        other => panic!("a post into a run-owned thread is refused: {other:?}"),
    };
    assert!(
        refused.contains("belongs to a run"),
        "the refusal names the reason: {refused}"
    );
    assert_eq!(
        daemon.child_events(child).len(),
        before,
        "the refused post wrote nothing"
    );
    assert_eq!(daemon.wait_finished(built.lead).await, RunOutcome::Closed);
}

/// T13 — `Open` on a running lead: the log's events from `from_seq`, then
/// the live events, with no actor.
#[tokio::test]
async fn t13_a_lead_opens_from_the_log_and_streams_live() {
    let daemon = Daemon::new(happy_scripts(), false).await;
    let (client, _) = daemon.connect("steve").await;
    let built = daemon.build(&client, "steve", 58).await;
    assert_eq!(daemon.wait_finished(built.lead).await, RunOutcome::Closed);

    // The log, from the start, with no actor for it.
    let opened = match client
        .request(Request::Open {
            thread: built.lead,
            from_seq: 0,
        })
        .await
        .unwrap()
    {
        Response::Opened { run, events, .. } => (run, events),
        other => panic!("a lead opens read-only: {other:?}"),
    };
    assert_eq!(
        opened.0,
        Some(aigentic_api::RunThread::Lead { issue: 58 }),
        "the state says the thread is a run's lead"
    );
    assert_eq!(
        opened.1,
        daemon.lead_events(built.lead),
        "the whole run, from the log"
    );
    assert!(
        daemon.server.threads.mailbox(built.lead).is_none(),
        "no actor was started for the run's lead"
    );

    // From the middle: the events the client had not seen.
    let all = daemon.lead_events(built.lead);
    let from = all.len() as u64 - 1;
    let tail = match client
        .request(Request::Open {
            thread: built.lead,
            from_seq: from,
        })
        .await
        .unwrap()
    {
        Response::Opened { events, .. } => events,
        other => panic!("a lead opens read-only: {other:?}"),
    };
    assert_eq!(tail, all[from as usize..], "from_seq is honoured");
}

// ---------------------------------------------------------------------------
// T7: the start-up scan
// ---------------------------------------------------------------------------

/// A fixture holding three leads: one waiting at its `route` gate, one
/// killed mid-implement, and one already finished. The first two come
/// from a first daemon; the third is a log written the way the daemon
/// would have left it.
async fn three_leads() -> (Fixture, Ulid, Ulid, Ulid) {
    let scripts = Scripts::of([
        vec![report("r1", brief_report_full())],
        vec![report("r1", brief_report())],
        vec![Act::Commit {
            file: "pancake-one.txt".into(),
            body: "the first".into(),
            subject: NAMED[0].into(),
        }],
    ]);
    let first = Daemon::new(scripts, false).await;
    let (client, _) = first.connect("steve").await;
    // One build at a time, each waited to a known point before the next:
    // the scripts are handed out in the order children are built, and two
    // builds in flight at once would race for them.
    let waiting = first.build(&client, "steve", 58).await.lead;
    assert!(
        first
            .wait_until(|| gate_of(&first.lead_events(waiting)).as_deref() == Some("route"))
            .await,
        "the first lead waits at its gate"
    );
    let mid = first.build(&client, "steve", 59).await.lead;
    assert!(
        first
            .wait_until(|| first.child_of(mid, "implement-alone", 1).is_some())
            .await,
        "the second lead reaches its implementer step"
    );
    let mid_child = first
        .child_of(mid, "implement-alone", 1)
        .expect("the implementer started");
    assert!(
        first
            .wait_until(|| first
                .child_events(mid_child)
                .iter()
                .any(|e| e.kind == EventKind::UserMessage))
            .await,
        "the second lead is left mid-child"
    );
    let left = first.scripts.left();
    let files = first.stop().await;
    eprintln!(
        "DIAG waiting={:?}",
        files
            .events(waiting)
            .iter()
            .map(|e| format!("{:?}", e.kind))
            .collect::<Vec<_>>()
    );
    eprintln!(
        "DIAG mid={:?} child={:?} scripts_left={}",
        files
            .events(mid)
            .iter()
            .map(|e| format!("{:?}", e.kind))
            .collect::<Vec<_>>(),
        files
            .events(mid_child)
            .iter()
            .map(|e| format!("{:?}", e.kind))
            .collect::<Vec<_>>(),
        left
    );

    // A finished lead: `thread_started`, `run_started`, `run_finished`,
    // written straight into the threads directory.
    let finished = Ulid::generate();
    let dir = files.threads.clone();
    let mut log = ThreadLog::open(&dir, finished).unwrap();
    let user = Author::User(aigentic_runtime::aigentic_core::UserId("steve".into()));
    for (kind, payload) in [
        (
            EventKind::ThreadStarted,
            serde_json::to_value(ThreadStartedPayload {
                project: Some("p".into()),
                root: files.repo.clone(),
                created_by: user.clone(),
                parent_thread: None,
                step: None,
                // never a front thread (issue #84)
                front: false,
            })
            .unwrap(),
        ),
        (
            EventKind::RunStarted,
            serde_json::to_value(aigentic_runtime::aigentic_log::RunStartedPayload {
                issue: 60,
                workflow: "build".into(),
                version: 1,
                content_hash: "hash".into(),
                budget_usd: 1.0,
            })
            .unwrap(),
        ),
        (
            EventKind::RunFinished,
            serde_json::to_value(aigentic_runtime::aigentic_log::RunFinishedPayload {
                outcome: RunOutcome::Closed,
                cost_usd: 0.0,
                release_impact: None,
            })
            .unwrap(),
        ),
    ] {
        log.append(aigentic_runtime::aigentic_log::NewEvent {
            kind,
            author: user.clone(),
            payload,
            parent_event: None,
        })
        .unwrap();
    }
    drop(log);
    (files, waiting, mid, finished)
}

/// The rest of the mid-implement lead's turn, after the kill.
fn mid_resume_scripts() -> Scripts {
    Scripts::of([vec![
        Act::Commit {
            file: "pancake-two.txt".into(),
            body: "the second".into(),
            subject: NAMED[1].into(),
        },
        report("r2", implementer_report()),
    ]])
}

/// T7 — a served daemon (`resume_runs: true`) finds a lead **waiting at a
/// checkpoint** and one left mid-implement, and claims both (one waits,
/// one resumes); finished leads are ignored; an embedded daemon
/// (`false`) claims neither.
#[tokio::test]
async fn t7_the_start_up_scan_claims_unfinished_runs_only() {
    let (files, waiting, mid, finished) = three_leads().await;
    let before_finished = files.events(finished).len();
    let before_waiting = files.events(waiting).len();

    let daemon = files.daemon(mid_resume_scripts(), true).await;
    // The scan claimed the mid-implement lead and nothing else: no
    // `Build` was sent, and the run finishes on the scan's own task.
    let outcome = daemon.wait_finished(mid).await;
    assert_eq!(outcome, RunOutcome::Closed);
    assert_eq!(
        daemon.scripts.left(),
        0,
        "the scan used the resumed child's script and no other"
    );
    // The waiting lead is claimed too, and stays where it was: a task
    // holds it, waiting for the human.
    let waiting_events = daemon.lead_events(waiting);
    assert!(
        waiting_events.len() >= before_waiting,
        "the waiting lead lost events"
    );
    assert_eq!(
        waiting_events
            .iter()
            .filter(|e| e.kind == EventKind::StepStarted)
            .count(),
        1,
        "one step: the waiting run was not advanced"
    );
    assert_eq!(
        daemon.events(finished).len(),
        before_finished,
        "a finished lead is left alone"
    );

    // The answer that ends it: the claimed run is answerable, and the
    // answer is acted on once.
    let (client, _) = daemon.connect("steve").await;
    let stopped = client
        .request(Request::AnswerCheckpoint {
            lead: waiting,
            gate: "route".to_owned(),
            answer: CheckpointAnswer::Stop,
            amendment: None,
        })
        .await
        .unwrap();
    assert!(matches!(stopped, Response::Ok), "{stopped:?}");
    assert_eq!(daemon.wait_finished(waiting).await, RunOutcome::Stopped);
}

/// T7 (the embedded half) — with `resume_runs: false` the scan claims
/// neither unfinished lead: nothing resumes someone's build behind the
/// REPL's back.
#[tokio::test]
async fn t7_an_embedded_daemon_claims_nothing() {
    let (files, waiting, mid, _finished) = three_leads().await;
    let before = files.events(mid).len();
    let daemon = files.daemon(Scripts::default(), false).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(
        daemon.events(mid).len(),
        before,
        "the embedded daemon left the mid-implement lead alone"
    );
    assert_eq!(
        daemon
            .lead_events(waiting)
            .iter()
            .filter(|e| e.kind == EventKind::RunFinished)
            .count(),
        0,
        "and the waiting lead alone"
    );
}

/// T8 — `AnswerCheckpoint { stop }` on a waiting lead with no task running
/// starts the task, appends one answer, and ends in
/// `run_finished { Stopped }`; `go` is refused with no `checkpoint_answered`
/// written; a wrong gate, a second answer and an unknown lead are refused.
#[tokio::test]
async fn t8_stop_is_answered_go_is_refused() {
    // A run left waiting at its route gate with no task holding it: the
    // daemon that made it is gone.
    let scripts = Scripts::of([vec![report("r1", brief_report_full())]]);
    let first = Daemon::new(scripts, false).await;
    let (client, _) = first.connect("steve").await;
    let lead = first.build(&client, "steve", 58).await.lead;
    assert!(
        first
            .wait_until(|| gate_of(&first.lead_events(lead)).is_some())
            .await,
        "the run waits at its gate"
    );
    let files = first.stop().await;

    let daemon = files.daemon(Scripts::default(), false).await;
    let (client, _) = daemon.connect("magnus").await;
    let mut notices = client.take_notices().expect("the notice stream");
    // `Open` on a lead starts no actor and subscribes this session to the
    // run's broadcast (rule 5a), so the answer and the outcome are seen.
    let opened = client
        .request(Request::Open {
            thread: lead,
            from_seq: 0,
        })
        .await
        .unwrap();
    assert!(
        matches!(opened, Response::Opened { .. }),
        "a lead opens read-only: {opened:?}"
    );
    assert!(
        daemon.server.threads.mailbox(lead).is_none(),
        "opening a lead starts no actor"
    );

    // `go` is refused before anything is appended.
    let before = daemon.events(lead).len();
    let refused = match client
        .request(Request::AnswerCheckpoint {
            lead,
            gate: "route".into(),
            answer: CheckpointAnswer::Go,
            amendment: None,
        })
        .await
        .unwrap()
    {
        Response::Refused { reason } => reason,
        other => panic!("the runner's own gate offers stop only, so go is refused: {other:?}"),
    };
    assert!(
        refused.contains("takes stop") && refused.contains("`go`"),
        "the refusal names what the gate takes: {refused}"
    );
    assert_eq!(
        daemon.events(lead).len(),
        before,
        "nothing was appended for a refused go"
    );
    assert_eq!(
        daemon
            .lead_events(lead)
            .iter()
            .filter(|e| e.kind == EventKind::CheckpointAnswered)
            .count(),
        0,
        "and no checkpoint_answered was written"
    );

    // A wrong gate and an unknown lead are refused too.
    for (named, gate, who) in [
        ("a gate the run is not at", "budget", lead),
        ("a lead that is not a run", "route", Ulid::generate()),
    ] {
        let answer = client
            .request(Request::AnswerCheckpoint {
                lead: who,
                gate: gate.into(),
                answer: CheckpointAnswer::Stop,
                amendment: None,
            })
            .await
            .unwrap();
        assert!(
            matches!(answer, Response::Refused { .. }),
            "{named} is refused: {answer:?}"
        );
    }
    assert_eq!(
        daemon.events(lead).len(),
        before,
        "still nothing written by the refusals"
    );

    // `stop` is accepted: the answer is written and acted on, and the run
    // ends `Stopped`. The task did not hold the lead when the request
    // arrived, so the request had to start it.
    let stopped = client
        .request(Request::AnswerCheckpoint {
            lead,
            gate: "route".into(),
            answer: CheckpointAnswer::Stop,
            amendment: None,
        })
        .await
        .unwrap();
    assert!(matches!(stopped, Response::Ok), "{stopped:?}");
    assert_eq!(daemon.wait_finished(lead).await, RunOutcome::Stopped);

    let events = daemon.lead_events(lead);
    let answered: Vec<aigentic_runtime::aigentic_log::CheckpointAnsweredPayload> = events
        .iter()
        .filter(|e| e.kind == EventKind::CheckpointAnswered)
        .map(|e| serde_json::from_value(e.payload.clone()).unwrap())
        .collect();
    assert_eq!(answered.len(), 1, "one answer, and only one");
    assert_eq!(
        answered[0].answer,
        aigentic_runtime::aigentic_log::CheckpointAnswer::Stop,
        "the event records the word it was given"
    );

    // A second answer to the same gate is refused: it is no longer open.
    let again = client
        .request(Request::AnswerCheckpoint {
            lead,
            gate: "route".into(),
            answer: CheckpointAnswer::Stop,
            amendment: None,
        })
        .await
        .unwrap();
    assert!(
        matches!(again, Response::Refused { .. }),
        "the gate is answered once: {again:?}"
    );
    assert_eq!(
        daemon
            .lead_events(lead)
            .iter()
            .filter(|e| e.kind == EventKind::CheckpointAnswered)
            .count(),
        1,
        "still one answer"
    );

    // The answer and the outcome reached the watcher as notices.
    let seen = Daemon::collect(&mut notices, lead, |events| {
        events.iter().any(|e| e.kind == EventKind::RunFinished)
    })
    .await;
    assert!(
        seen.iter().any(|e| e.kind == EventKind::CheckpointAnswered),
        "the answer was streamed"
    );
}

/// T9 — roles: `read` and `write` are refused `Build` and
/// `AnswerCheckpoint`; `approve` is allowed. (The row list in
/// `every_request_has_its_row` is hand-written, so the refusals here are
/// the real guard.)
#[tokio::test]
async fn t9_build_and_answering_need_approve() {
    // The project's participants: steve and magnus approve, reviewer and
    // viewer only read.
    let scripts = Scripts::of([vec![report("r1", brief_report())], implementer_script()]);
    let daemon = Daemon::new(scripts, false).await;

    for user in ["reviewer", "viewer"] {
        let (client, _) = daemon.connect(user).await;
        let built = client
            .request(Request::Build {
                project: "p".into(),
                issue: 58,
                workflow: None,
            })
            .await
            .unwrap();
        assert!(
            matches!(built, Response::Refused { .. }),
            "{user} may not start a run: {built:?}"
        );
        let answered = client
            .request(Request::AnswerCheckpoint {
                lead: Ulid::generate(),
                gate: "route".into(),
                answer: CheckpointAnswer::Stop,
                amendment: None,
            })
            .await
            .unwrap();
        assert!(
            matches!(answered, Response::Refused { .. }),
            "{user} may not answer a checkpoint: {answered:?}"
        );
    }
    assert!(
        daemon
            .server
            .threads
            .list(Some("p"), "steve", |_| true)
            .unwrap()
            .is_empty(),
        "no lead was created for a refused build"
    );

    // An approver may start one.
    let (approver, _) = daemon.connect("magnus").await;
    let built = daemon.build(&approver, "magnus", 58).await;
    assert!(built.lead != Ulid::nil(), "the approver started the run");
    assert_eq!(daemon.wait_finished(built.lead).await, RunOutcome::Closed);
}

/// T12 — an answer appended and the process dying before it was acted on:
/// the next claim acts once (one `run_finished { Stopped }`, no new
/// `checkpoint_asked`, no second answer).
#[tokio::test]
async fn t12_an_answer_written_before_the_crash_is_acted_on_once() {
    // The daemon that dies: it waits at its gate and never answers.
    let scripts = Scripts::of([vec![report("r1", brief_report_full())]]);
    let first = Daemon::new(scripts, false).await;
    let (client, _) = first.connect("steve").await;
    let lead = first.build(&client, "steve", 58).await.lead;
    assert!(
        first
            .wait_until(|| gate_of(&first.lead_events(lead)).as_deref() == Some("route"))
            .await,
        "the run waits at its gate"
    );
    let files = first.stop().await;

    // The answer was written and the process died before the runner saw
    // it: append it by hand, exactly as `Runner::answer` would, and leave
    // no task holding the lead.
    let (mut log, _cut) =
        ThreadLog::open_with(files.threads.clone(), lead, Repair::TruncateTornTail).unwrap();
    let before = log.len() as usize;
    log.append(aigentic_runtime::aigentic_log::NewEvent {
        kind: EventKind::CheckpointAnswered,
        author: aigentic_runtime::aigentic_core::Author::User(
            aigentic_runtime::aigentic_core::UserId("magnus".into()),
        ),
        payload: serde_json::to_value(aigentic_runtime::aigentic_log::CheckpointAnsweredPayload {
            answer: aigentic_runtime::aigentic_log::CheckpointAnswer::Stop,
            amendment: None,
            marks: Vec::new(),
        })
        .unwrap(),
        parent_event: None,
    })
    .unwrap();
    drop(log);

    let daemon = files.daemon(Scripts::default(), false).await;
    let (client, _) = daemon.connect("steve").await;
    let again = daemon.build(&client, "steve", 58).await;
    assert_eq!(again.lead, lead, "the same lead");
    assert!(again.resumed, "the unfinished run is resumed");
    assert_eq!(daemon.wait_finished(lead).await, RunOutcome::Stopped);

    let events = daemon.lead_events(lead);
    assert_eq!(
        events
            .iter()
            .filter(|e| e.kind == EventKind::CheckpointAnswered)
            .count(),
        1,
        "the answer that was already there is the only one"
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| e.kind == EventKind::RunFinished)
            .count(),
        1,
        "the next claim acts on it once"
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| e.kind == EventKind::CheckpointAsked)
            .count(),
        1,
        "no new checkpoint_asked"
    );
    assert_eq!(
        events.len(),
        before + 2,
        "one answer (written before) plus one run_finished: nothing repeated"
    );
}

// ---------------------------------------------------------------------------
// The fix round: #58 items 1, 2 and 3
// ---------------------------------------------------------------------------

/// A keep-awake guard that records what a run asks of it, so a test can
/// watch the hold follow the work (issue #58 fix 1). `peak` is the most
/// holds it ever saw outstanding at once.
#[derive(Default)]
struct Counting {
    inner: Mutex<(usize, usize)>,
}

impl Counting {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Holds outstanding right now.
    fn holds(&self) -> usize {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).0
    }

    /// The most holds outstanding at any time.
    fn peak(&self) -> usize {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).1
    }
}

impl aigentic_server::awake::KeepAwake for Counting {
    fn hold(&self) {
        let mut rec = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        rec.0 += 1;
        rec.1 = rec.1.max(rec.0);
    }

    fn release(&self) {
        let mut rec = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        rec.0 = rec.0.saturating_sub(1);
    }

    fn status(&self) -> String {
        "on".to_owned()
    }
}

/// T14 — a run holds the keep-awake guard while it advances and gives it
/// back at a gate and on every way out: mid-step a hold is outstanding,
/// a run waiting at a checkpoint holds none, and `Finished` and a
/// `RunnerError` both leave the count at zero (issue #58 fix 1).
#[tokio::test]
async fn t14_a_run_holds_keep_awake_only_while_it_advances() {
    // A `full` brief parks the run at its `route` gate: the script is one
    // reply, so the step after the answer does nothing and the run ends
    // `Stopped` when it is answered.
    let daemon = Daemon::new(
        Scripts::of([vec![report("r1", brief_report_full())]]),
        false,
    )
    .await;
    let guard = Counting::new();
    daemon.server.threads.with_keep_awake(guard.clone());
    let (client, _) = daemon.connect("steve").await;
    let lead = daemon.build(&client, "steve", 58).await.lead;

    // While the run advances — the brief's provider call — a hold is
    // outstanding.
    assert!(
        daemon.wait_until(|| guard.peak() > 0).await,
        "a step holds the machine awake while it works"
    );
    // At the gate nobody is working: #47's design releases it, and the
    // run must follow it, or a laptop stays up however long a gate waits.
    assert!(
        daemon
            .wait_until(
                || gate_of(&daemon.lead_events(lead)).as_deref() == Some("route")
                    && guard.holds() == 0
            )
            .await,
        "a run waiting at a gate holds nothing; holds are {}",
        guard.holds()
    );
    // A second look: it stays released while it waits.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(guard.holds(), 0, "still waiting, still holding nothing");

    // The answer resumes it, and it ends: nothing is left held.
    let answered = client
        .request(Request::AnswerCheckpoint {
            lead,
            gate: "route".into(),
            answer: CheckpointAnswer::Stop,
            amendment: None,
        })
        .await
        .unwrap();
    assert!(matches!(answered, Response::Ok), "{answered:?}");
    assert_eq!(daemon.wait_finished(lead).await, RunOutcome::Stopped);
    assert!(
        daemon.wait_until(|| guard.holds() == 0).await,
        "`Finished` leaves nothing held"
    );
    assert_eq!(
        guard.holds(),
        0,
        "the hold went back on the way out of `Finished`"
    );
}

/// T14b — a run that ends in a `RunnerError` leaves no hold behind
/// (issue #58 fix 1).
#[tokio::test]
async fn t14b_a_runner_error_leaves_no_hold() {
    let fixture = Fixture::new(Vec::new());
    let daemon = fixture
        .daemon(
            Scripts::of([vec![report("r1", brief_report_full())]]),
            false,
        )
        .await;
    let guard = Counting::new();
    daemon.server.threads.with_keep_awake(guard.clone());
    let (client, _) = daemon.connect("steve").await;
    let lead = daemon.build(&client, "steve", 58).await.lead;

    assert!(
        daemon.wait_until(|| guard.peak() > 0).await,
        "the run held the machine awake while it worked"
    );
    assert!(
        daemon
            .wait_until(|| gate_of(&daemon.lead_events(lead)).as_deref() == Some("route"))
            .await,
        "the run waits at the route gate"
    );
    assert_eq!(guard.holds(), 0, "a run waiting at a gate holds nothing");
    let files = daemon.stop().await;

    // The workflow the lead recorded is gone: a daemon that claims the
    // run cannot build it, so the run stops with an error. Nothing ran,
    // so nothing appends and the count must be zero again.
    std::fs::remove_dir_all(files.bundled.join("workflows").join("build")).unwrap();
    let daemon = files.daemon(Scripts::of(Vec::new()), false).await;
    daemon.server.threads.with_keep_awake(guard.clone());
    let (client, _) = daemon.connect("steve").await;
    let mut notices = client.take_notices().expect("the notice stream");
    // Subscribing first is what makes the lead's notes reach this client:
    // the error is broadcast the moment the claim builds the run.
    assert!(matches!(
        client
            .request(Request::Open {
                thread: lead,
                from_seq: 0
            })
            .await
            .unwrap(),
        Response::Opened { .. }
    ));
    let resumed = daemon.build(&client, "steve", 58).await;
    assert_eq!(resumed.lead, lead, "the same run");

    let stopped = daemon
        .wait_until(|| !Daemon::notes(&mut notices, lead).is_empty())
        .await;
    assert!(stopped, "the run reported the error as a note");
    assert_eq!(guard.holds(), 0, "an error leaves nothing held");
}

/// T15 — two daemons over one project: the lead's OS lock lets one drive
/// it and refuses the other, so one log has one writer and its seqs are
/// gapless (issue #58 fix 2). Two `Server`s in one test process, each
/// with its own registry, are exactly the cross-process case the
/// in-process slot cannot see.
#[tokio::test]
async fn t15_one_writer_per_lead_across_processes() {
    let fixture = Fixture::new(Vec::new());
    // The second daemon's files, taken before the first takes the
    // fixture: same repository, threads and config, its own scratch.
    let twin = fixture.twin();
    // The first daemon: one `full` brief, so the run parks at its gate
    // and holds the lead open while the second daemon tries to claim it.
    let scripts = Scripts::of([vec![report("r1", brief_report_full())]]);
    let first = fixture.daemon(scripts.clone(), false).await;
    let (client, _) = first.connect("steve").await;
    let lead = first.build(&client, "steve", 58).await.lead;
    assert!(
        first
            .wait_until(|| gate_of(&first.lead_events(lead)).as_deref() == Some("route"))
            .await,
        "the first daemon's run waits at its gate"
    );
    assert!(
        first.fixture.threads.join(format!("{lead}.lock")).exists(),
        "the lead's lock file sits beside its log"
    );

    // A second daemon over the same files: same project, same threads
    // directory, its own registry and its own server. Nothing holds the
    // lead for it, in process — that is what the OS lock is for.
    let other = twin.daemon(Scripts::default(), false).await;

    let (client2, _) = other.connect("magnus").await;
    let refused = match client2
        .request(Request::Build {
            project: "p".into(),
            issue: 58,
            workflow: None,
        })
        .await
        .unwrap()
    {
        Response::Refused { reason } => reason,
        other => panic!("the second daemon is refused the lead: {other:?}"),
    };
    assert_eq!(
        refused,
        format!("run {lead} is being driven by another aigentic process"),
        "the refusal says who holds it"
    );

    // The second daemon wrote nothing: one answer, from the first, and
    // the log's seqs are still gapless.
    let events = first.lead_events(lead);
    let seqs: Vec<u64> = events.iter().map(|e| e.seq).collect();
    assert_eq!(
        seqs,
        (0..seqs.len() as u64).collect::<Vec<_>>(),
        "one writer, gapless seqs"
    );

    // The first daemon's own re-claim is an attach, not a refusal: the
    // run is its.
    let again = first.build(&client, "steve", 58).await;
    assert_eq!(again.lead, lead, "the same lead");
    assert!(again.resumed, "the unfinished run is resumed");

    let _ = other.stop().await;
    let _ = first.stop().await;
}

/// T15c — two daemons that `Build` one issue with no lead yet, at the
/// same moment, make one lead between them (issue #58, review 2). The
/// lead's own lock cannot cover this window, because the lead does not
/// exist yet; the issue's OS lock does. The test holds that lock itself,
/// standing in for a third process mid-create: both `Build`s must wait on
/// it, and once it is released exactly one lead is made and the other
/// daemon is refused with the reason.
#[tokio::test]
async fn t15c_two_processes_racing_a_fresh_build_make_one_lead() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let fixture = Fixture::new(Vec::new());
    let twin = fixture.twin();
    let project_threads = fixture.threads.clone();
    std::fs::create_dir_all(&project_threads).unwrap();
    // The issue's lock names its project since #9: `issue-<n>-<project>.lock`.
    let held = std::fs::File::options()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(project_threads.join("issue-58-p.lock"))
        .unwrap();
    held.try_lock().expect("nobody holds the issue's lock yet");

    let scripts = Scripts::of([vec![report("r1", brief_report_full())]]);
    let first = fixture.daemon(scripts.clone(), false).await;
    let other = twin.daemon(scripts, false).await;
    let (client, _) = first.connect("steve").await;
    let (client2, _) = other.connect("magnus").await;

    let build = Request::Build {
        project: "p".into(),
        issue: 58,
        workflow: None,
    };
    let (a_done, b_done) = (AtomicBool::new(false), AtomicBool::new(false));
    let lead_logs = || -> Vec<std::path::PathBuf> {
        std::fs::read_dir(&project_threads)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
            .filter(|p| {
                std::fs::read_to_string(p)
                    .unwrap_or_default()
                    .contains("\"run_started\"")
            })
            .collect()
    };
    let (a, b, ()) = tokio::join!(
        async {
            let r = client.request(build.clone()).await.unwrap();
            a_done.store(true, Ordering::SeqCst);
            r
        },
        async {
            let r = client2.request(build.clone()).await.unwrap();
            b_done.store(true, Ordering::SeqCst);
            r
        },
        async {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            assert!(
                !a_done.load(Ordering::SeqCst) && !b_done.load(Ordering::SeqCst),
                "both `Build`s wait while another process holds the issue's lock"
            );
            assert!(lead_logs().is_empty(), "no lead is made while it is held");
            held.unlock().unwrap();
        },
    );

    assert_eq!(lead_logs().len(), 1, "one lead for issue 58: {a:?} / {b:?}");
    let (made, refused) = match (a, b) {
        (Response::Run { lead, resumed }, Response::Refused { reason })
        | (Response::Refused { reason }, Response::Run { lead, resumed }) => {
            ((lead, resumed), reason)
        }
        (a, b) => panic!("one daemon makes the lead, the other is refused: {a:?} / {b:?}"),
    };
    assert!(!made.1, "the lead is new, not resumed");
    assert_eq!(
        refused,
        format!("run {} is being driven by another aigentic process", made.0),
        "the loser found the winner's lead and says who holds it"
    );

    let _ = other.stop().await;
    let _ = first.stop().await;
}

/// T16 — `Report` on a run-owned thread is served from its log, with no
/// actor started: a running child and its lead both answer (issue #58
/// fix 3).
#[tokio::test]
async fn t16_report_on_a_run_owned_thread_answers_without_an_actor() {
    let daemon = Daemon::new(happy_scripts(), false).await;
    let (client, _) = daemon.connect("steve").await;
    let lead = daemon.build(&client, "steve", 58).await.lead;
    assert_eq!(daemon.wait_finished(lead).await, RunOutcome::Closed);
    let child = daemon
        .child_of(lead, "implement-alone", 1)
        .expect("the step started a child");

    for thread in [lead, child] {
        let answered = client
            .request(Request::Report {
                thread,
                report: aigentic_api::ReportKind::Project,
            })
            .await
            .unwrap();
        assert!(
            matches!(answered, Response::Text { .. }),
            "a report on a run-owned thread is answered from its log: {answered:?}"
        );
        assert!(
            daemon.server.threads.mailbox(thread).is_none(),
            "no actor was started for it"
        );
    }

    // A run-owned thread's other writes stay refused: the run's task is
    // its only writer.
    let refused = client
        .request(Request::Post {
            thread: child,
            blocks: vec![aigentic_runtime::aigentic_core::ContentBlock::Text(
                "hello".into(),
            )],
            interrupt: false,
        })
        .await
        .unwrap();
    assert!(
        matches!(refused, Response::Refused { .. }),
        "a post to a run's child is refused: {refused:?}"
    );
}

/// T15b — a log that cannot be read is not "no unfinished run" (issue
/// #58 fix 2): the lookup reports it — a note naming the file — and
/// refuses to start a new lead for the project, rather than writing a
/// second one over a run that is still there.
#[tokio::test]
async fn t15b_a_corrupt_lead_log_refuses_a_new_run() {
    let fx = Fixture::new(Vec::new());
    // The threads directory itself: logs live flat in it since #9.
    let threads = fx.threads.clone();
    let repo = fx.repo.clone();

    // A lead whose log was damaged: a good `thread_started`, then a line
    // no event can parse, then a `run_started` — the shape a partial
    // write or a copy that lost a byte leaves. Repair cuts a torn *tail*;
    // it must not silently read this as nothing.
    let lead = Ulid::generate();
    std::fs::create_dir_all(&threads).unwrap();
    let (mut log, _cut) = ThreadLog::open_with(&threads, lead, Repair::Refuse).unwrap();
    for (kind, payload) in [
        (
            EventKind::ThreadStarted,
            serde_json::to_value(ThreadStartedPayload {
                project: Some("p".into()),
                root: repo.clone(),
                created_by: Author::User(aigentic_runtime::aigentic_core::UserId("steve".into())),
                parent_thread: None,
                step: None,
                // never a front thread (issue #84)
                front: false,
            })
            .unwrap(),
        ),
        (
            EventKind::RunStarted,
            json!({
                "issue": 58,
                "workflow": "build",
                "version": 1,
                "content_hash": "abc",
                "budget_usd": 3.0,
            }),
        ),
    ] {
        log.append(aigentic_runtime::aigentic_log::NewEvent {
            kind,
            author: Author::User(aigentic_runtime::aigentic_core::UserId("steve".into())),
            payload,
            parent_event: None,
        })
        .unwrap();
    }
    drop(log);
    // Keep the two good lines, replace the middle one with noise.
    let path = threads.join(format!("{lead}.jsonl"));
    let text = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "the header and the run_started");
    std::fs::write(
        &path,
        format!("{}\nthis is not an event\n{}\n", lines[0], lines[1]),
    )
    .unwrap();

    let daemon = fx.daemon(Scripts::of(Vec::new()), false).await;
    // The note goes to the corrupt lead's watchers, so watch before the
    // lookup runs.
    let (tx, mut notes) = mpsc::unbounded_channel();
    daemon.server.threads.runs().watch(lead, tx);

    let refused = daemon
        .server
        .threads
        .build_run(
            "p",
            59,
            None,
            Author::User(aigentic_runtime::aigentic_core::UserId("steve".into())),
        )
        .await;
    let Err(error) = refused else {
        panic!("a new run must not start while a lead log cannot be read");
    };
    let text = error.to_string();
    assert!(
        text.contains(&format!("{lead}.jsonl")),
        "the refusal names the file: {text}"
    );
    assert!(
        text.contains("cannot read"),
        "the refusal says what went wrong: {text}"
    );

    let note = tokio::time::timeout(Duration::from_secs(5), notes.recv())
        .await
        .expect("a note reaches the watcher")
        .expect("the lane is open");
    let Notice::Note { thread, text } = note else {
        panic!("expected a note, got {note:?}");
    };
    assert_eq!(thread, lead);
    assert!(
        text.contains(&format!("{lead}.jsonl")),
        "the note names the file: {text}"
    );

    // The corrupt lead is not driven: a daemon does not drive what it
    // cannot read.
    assert!(
        !daemon.server.threads.runs().held(lead),
        "nothing claims it"
    );
}

/// A workflow's own checkpoint over the wire: the daemon starts the
/// named workflow, the run waits at the checkpoint with go, amend and
/// stop offered, an `amend` with no text is refused and writes nothing,
/// and an `amend` with text continues to `next = "done"`.
#[tokio::test]
async fn a_checkpoint_step_is_answered_amend_over_the_server() {
    let daemon = Daemon::new(Scripts::default(), false).await;
    let dir = daemon.fixture.bundled.join("workflows").join("gated");
    std::fs::create_dir_all(dir.join("templates")).unwrap();
    std::fs::write(
        dir.join("workflow.toml"),
        "name = \"gated\"\nversion = 1\n\n[budget]\ntrivial = 1.0\nfull = 1.0\nmax_raise = 1.0\n\n\
         [[slots]]\nname = \"issue\"\nkind = \"string\"\nfilled_by = \"runner\"\n\n\
         [[steps]]\nid = \"decide\"\nrole = \"person\"\nprofile = \"flash\"\n\
         template = \"templates/decide.md\"\nmarker = \"## Decide\"\ncheckpoint = true\nnext = \"done\"\n",
    )
    .unwrap();
    std::fs::write(dir.join("templates/decide.md"), "decide on #{{issue}}\n").unwrap();
    let (client, _) = daemon.connect("steve").await;
    let lead = match client
        .request(Request::Build {
            project: "p".into(),
            issue: 58,
            workflow: Some("gated".into()),
        })
        .await
        .unwrap()
    {
        Response::Run { lead, .. } => lead,
        other => panic!("a build is answered with a run: {other:?}"),
    };
    let mut asked = None;
    for _ in 0..250 {
        asked = daemon
            .lead_events(lead)
            .into_iter()
            .find(|event| event.kind == EventKind::CheckpointAsked);
        if asked.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let asked: aigentic_runtime::aigentic_log::CheckpointAskedPayload =
        serde_json::from_value(asked.expect("the run waits at its checkpoint").payload).unwrap();
    assert_eq!(asked.gate, "decide");
    assert_eq!(asked.options, ["go", "amend", "stop"]);
    assert_eq!(asked.shown, ["decide on #58\n"]);

    let before = daemon.lead_events(lead).len();
    let refused = client
        .request(Request::AnswerCheckpoint {
            lead,
            gate: "decide".into(),
            answer: CheckpointAnswer::Amend,
            amendment: None,
        })
        .await
        .unwrap();
    assert!(
        matches!(&refused, Response::Refused { reason } if reason.contains("amendment's text")),
        "an amend without text is refused: {refused:?}"
    );
    assert_eq!(
        daemon.lead_events(lead).len(),
        before,
        "nothing was written"
    );

    let answered = client
        .request(Request::AnswerCheckpoint {
            lead,
            gate: "decide".into(),
            answer: CheckpointAnswer::Amend,
            amendment: Some("keep it small".into()),
        })
        .await
        .unwrap();
    assert!(matches!(answered, Response::Ok), "{answered:?}");
    assert_eq!(
        daemon.wait_finished(lead).await,
        RunOutcome::Closed,
        "the checkpoint's `next = \"done\"` ends the run"
    );
}
