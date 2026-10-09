//! The build runner's tests (issue #57).

mod common;

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use aigentic_core::Budget;
use aigentic_core::{AgentId, Author, ContentBlock, Event, EventKind, ProviderEvent, UserId};
use aigentic_log::{
    NextMove, PermissionRequestedPayload, RunStartedPayload, StepReport, StepStatus, ThreadLog,
    ThreadStartedPayload, run_state,
};
use aigentic_policy::Policy;
use aigentic_runtime::runner::{
    Advanced, FakeForge, FakeInstaller, Forge, GhForge, GitRepo, IssueView, Runner, RunnerError,
    RunnerHost, WriteGuard,
};
use aigentic_runtime::workflow::{LoadedWorkflow, WorkflowFile, WorkflowOrigin};
use aigentic_runtime::{Answer, Approver, LENGTH_STOP, Prices, Runtime};
use aigentic_runtime::{
    STEP_REPORTED,
    runner::{CALL_FINISH_STEP, CONTINUE_PROMPT},
};
use aigentic_tools::ToolRegistry;
use common::{done, scripted, usage};
use serde_json::{Value, json};
use ulid::Ulid;

// ---------------------------------------------------------------------------
// Scripting
// ---------------------------------------------------------------------------

/// A `finish_step` call, the shape `tests/finish_step.rs` uses.
fn finish(id: &str, args: Value) -> ProviderEvent {
    ProviderEvent::ToolCall(aigentic_core::ToolCall {
        id: id.into(),
        name: "finish_step".into(),
        args,
    })
}

fn text(t: &str) -> ProviderEvent {
    ProviderEvent::TextDelta(t.into())
}

/// One scripted reply that reports a step and ends its turn.
fn report(reply: &str, args: Value) -> Vec<ProviderEvent> {
    vec![finish(reply, args), usage(10, 5), done("tool_use")]
}

/// One scripted reply that ends a turn without reporting.
fn stop(reason: &str) -> Vec<ProviderEvent> {
    vec![text("thinking"), usage(10, 5), done(reason)]
}

/// One scripted reply that calls the harmless tool: a turn that gets no
/// nearer its report, so a cap can end it.
fn works(id: &str) -> Vec<ProviderEvent> {
    vec![
        ProviderEvent::ToolCall(aigentic_core::ToolCall {
            id: id.into(),
            name: "noop".into(),
            args: json!({}),
        }),
        usage(10, 5),
        done("tool_use"),
    ]
}

/// A tool that does nothing, so a scripted turn can take an iteration
/// without ending.
struct Noop;

impl aigentic_core::Tool for Noop {
    fn name(&self) -> &str {
        "noop"
    }
    fn description(&self) -> &str {
        "does nothing"
    }
    fn schema(&self) -> schemars::schema::RootSchema {
        schemars::schema_for!(String)
    }
    fn risk_class(&self) -> aigentic_core::RiskClass {
        aigentic_core::RiskClass::Safe
    }
    fn call(
        &self,
        _args: Value,
    ) -> aigentic_core::BoxFuture<'_, Result<aigentic_core::ToolOutput, aigentic_core::ToolError>>
    {
        Box::pin(async move {
            Ok(aigentic_core::ToolOutput {
                content: "nothing".into(),
                is_error: false,
            })
        })
    }
}

/// The issue every fixture is for.
fn issue_view() -> IssueView {
    IssueView {
        title: "a runner issue".into(),
        body: "the body".into(),
    }
}

/// The brief's report: what a brief step reports for a `trivial` issue.
fn brief_report() -> Value {
    json!({
        "status": "done",
        "body": "## Brief\n\nthe brief",
        "slots": {
            "size": "trivial",
            "budget": 3,
            "purpose": "make the runner work",
            "must_not_undo": "nothing",
            "pointers": "crates/runtime/src/runner/mod.rs",
            "design": "a log-driven loop",
            "commits": ["runtime: one", "runtime: two"],
        },
        "planned_tests": [
            {"id": "T1", "what": "the happy path", "derivation": "from the spec"},
            {"id": "T2", "what": "the full route", "derivation": "from the spec"},
        ],
    })
}

/// The implementer's report: what the last step of the happy path reports.
/// It states a release impact, as a real implementer's does, so the
/// closing comment has something to carry.
fn implementer_report() -> Value {
    json!({
        "status": "done",
        "body": "## Implementation\n\nall landed",
        "slots": {"size": "trivial"},
        "release_impact": "patch",
    })
}

/// The `run_started` the daemon writes before the runner exists.
fn run_started(workflow: &LoadedWorkflow) -> RunStartedPayload {
    RunStartedPayload {
        issue: 57,
        workflow: workflow.workflow.name.clone(),
        version: workflow.workflow.version,
        content_hash: workflow.content_hash.clone(),
        budget_usd: workflow.workflow.budget.trivial,
    }
}

/// The trailer a passing commit carries: the E1 check reads the model the
/// step's profile resolved to, and `flash` is `tensorx/deepseek-v4.1-flash`.
const TRAILER: &str = "Co-Authored-By: aigentic (deepseek-v4.1-flash) <332865255+aigentic-bot@users.noreply.github.com>";

/// A work repo on `main` with one commit, pushed to a bare remote: what a
/// writing step's `head_at_start` and `remote_at_start` are read from, and
/// where its push lands.
fn init_git(repo: &Path, remote: &Path) {
    let run = |args: &[&str], cwd: &Path| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    };
    run(&["init", "-b", "main"], repo);
    run(&["config", "user.name", "aigentic test"], repo);
    run(&["config", "user.email", "test@example.invalid"], repo);
    std::fs::write(repo.join("README.md"), "the repo\n").unwrap();
    run(&["add", "README.md"], repo);
    run(&["commit", "-m", "initial commit"], repo);
    run(&["init", "--bare", "-b", "main"], remote);
    run(&["remote", "add", "origin", remote.to_str().unwrap()], repo);
    run(&["push", "-u", "origin", "main"], repo);
}

// ---------------------------------------------------------------------------
// The fakes
// ---------------------------------------------------------------------------

/// Allow everything a child might ask about, silently.
struct Yes;

impl Approver for Yes {
    fn author(&self) -> Author {
        Author::User(UserId("steve".into()))
    }
    fn ask(&mut self, _: &PermissionRequestedPayload) -> Answer {
        Answer::Allow
    }
    fn ask_human(&mut self, _: &str) -> Option<String> {
        None
    }
}

/// The host the runner drives: every child is a log in one directory, and
/// each child's replies are scripted in advance.
///
/// The ids are handed out from a queue the fixture prepared, so a test
/// knows which child is which, and a rebuilt runner hands out the same
/// ids again in the same order. A child's remaining replies are whatever
/// its log has not answered yet, so a rebuild continues a half-run turn
/// instead of replaying it.
struct FakeHost {
    dir: PathBuf,
    lead: Ulid,
    caps: BTreeMap<Ulid, u32>,
    planned: Mutex<VecDeque<(Ulid, Vec<Vec<ProviderEvent>>)>>,
    scripts: Mutex<BTreeMap<Ulid, Vec<Vec<ProviderEvent>>>>,
    models: BTreeMap<String, String>,
    /// The price table a child's runtime is built with, so a test can see
    /// a real `usage.cost_usd` on its lines.
    prices: Option<Prices>,
}

impl FakeHost {
    fn new(
        dir: PathBuf,
        lead: Ulid,
        children: &[(Ulid, Vec<Vec<ProviderEvent>>)],
        caps: BTreeMap<Ulid, u32>,
        prices: Option<Prices>,
    ) -> Self {
        Self {
            dir,
            lead,
            caps,
            planned: Mutex::new(children.iter().cloned().collect()),
            scripts: Mutex::new(children.iter().cloned().collect()),
            models: BTreeMap::from([
                ("kimi".to_owned(), "tensorx/kimi-k2".to_owned()),
                ("flash".to_owned(), "tensorx/deepseek-v4.1-flash".to_owned()),
            ]),
            prices,
        }
    }
}

impl RunnerHost for FakeHost {
    fn new_child_id(&mut self) -> Ulid {
        // A rebuilt runner hands out the ids the full run handed out: the
        // lead log says which are spent, and the fixture planned them in
        // start order.
        let lead = ThreadLog::open(&self.dir, self.lead)
            .expect("the lead log opens")
            .read_all()
            .expect("the lead log reads");
        let used: Vec<Ulid> = lead
            .iter()
            .filter(|event| event.kind == EventKind::StepStarted)
            .map(|event| {
                let payload: aigentic_log::StepStartedPayload =
                    serde_json::from_value(event.payload.clone()).expect("a step_started payload");
                payload.child_thread
            })
            .collect();
        loop {
            let (id, script) = self
                .planned
                .lock()
                .unwrap()
                .pop_front()
                .expect("the fixture planned every child the run needs");
            self.scripts.lock().unwrap().insert(id, script);
            if !used.contains(&id) {
                return id;
            }
        }
    }

    fn create_child(&mut self, id: Ulid, step: &str) -> Result<(), RunnerError> {
        let mut log = ThreadLog::open(&self.dir, id)?;
        log.append(aigentic_log::NewEvent {
            kind: EventKind::ThreadStarted,
            author: Author::Agent(AgentId("runner".into())),
            payload: serde_json::to_value(ThreadStartedPayload {
                project: None,
                root: self.dir.clone(),
                created_by: Author::User(UserId("steve".into())),
                parent_thread: Some(self.lead),
                step: Some(step.to_owned()),
                front: false,
            })
            .expect("thread_started serialises"),
            parent_event: None,
        })?;
        Ok(())
    }

    fn build_child(
        &mut self,
        id: Ulid,
        profile: &str,
        step: &str,
        deny: &[String],
    ) -> impl std::future::Future<Output = Result<Runtime, RunnerError>> + Send {
        // A scripted child needs nothing async; the trait is async for the
        // daemon's host, which connects MCP servers on the way (#58).
        let prepared = (|| -> Result<_, RunnerError> {
            let log = ThreadLog::open(&self.dir, id)?;
            // What the child's log has already answered, so a rebuild
            // gets the replies that are still to come.
            let answered = log
                .events()
                .iter()
                .filter(|event| event.kind == EventKind::AssistantMessage)
                .count();
            let script = self
                .scripts
                .lock()
                .unwrap()
                .get(&id)
                .cloned()
                .unwrap_or_default();
            let (provider, _seen) = scripted(script.into_iter().skip(answered).collect());
            let registry: ToolRegistry =
                vec![Box::new(Noop) as Box<dyn aigentic_core::Tool>].into();
            Ok((
                provider,
                registry,
                log,
                self.caps.get(&id).copied().unwrap_or(50),
                self.prices,
            ))
        })();
        let profile = profile.to_owned();
        let step = step.to_owned();
        let deny = deny.to_vec();
        async move {
            let (provider, registry, log, cap, prices) = prepared?;
            let mut runtime = Runtime::new(provider, registry, log, AgentId("child".into()))
                .with_policy(Policy::defaults())
                .with_approver(Box::new(Yes))
                .with_budget(Budget {
                    max_iterations: cap,
                    max_tokens: u64::MAX,
                    max_wall_time: std::time::Duration::from_secs(60),
                    cache_read_price_ratio: 0.25,
                })
                .with_step(&step, &deny)?;
            // A test that wants a real `usage.cost_usd` on the child's lines
            // prices this child's endpoint; an unpriced host behaves as before.
            if let Some(prices) = prices {
                runtime.set_pricing(&profile, Some(prices));
            }
            Ok(runtime)
        }
    }

    fn child_exists(&self, id: Ulid) -> bool {
        self.dir.join(format!("{id}.jsonl")).exists()
    }

    fn child_log(&self, id: Ulid) -> Result<ThreadLog, RunnerError> {
        Ok(ThreadLog::open(&self.dir, id)?)
    }

    fn model_of(&self, profile: &str) -> Result<String, RunnerError> {
        Ok(self
            .models
            .get(profile)
            .cloned()
            .unwrap_or_else(|| profile.to_owned()))
    }
}

// ---------------------------------------------------------------------------
// The fixture
// ---------------------------------------------------------------------------

/// One run's world: the lead log, the children's scripts, the forge. A
/// rebuilt runner is built from the same fixture, so a test can cut the
/// logs back to any point and see the next move happen once.
struct Fixture {
    dir: tempfile::TempDir,
    lead: Ulid,
    repo: PathBuf,
    remote: PathBuf,
    installer: FakeInstaller,
    initial_head: String,
    children: Vec<(Ulid, Vec<Vec<ProviderEvent>>)>,
    forge: Arc<FakeForge>,
    workflow: LoadedWorkflow,
    caps: BTreeMap<Ulid, u32>,
    prices: Option<Prices>,
}

/// The runner a fixture builds, named once so a helper's signature stays
/// short.
type TestRunner = Runner<Arc<FakeForge>, FakeHost, GitRepo>;

impl Fixture {
    /// A fixture whose children are scripted in the order they start. The
    /// lead log already holds `run_started`, as the daemon would leave it.
    fn new(children: Vec<(Ulid, Vec<Vec<ProviderEvent>>)>) -> Self {
        Self::with_comments(children, Vec::new())
    }

    /// The same, with comments the forge already holds — a rebuild after
    /// a post that the lead log never wrote down.
    fn with_comments(
        children: Vec<(Ulid, Vec<Vec<ProviderEvent>>)>,
        comments: Vec<String>,
    ) -> Self {
        Self::assemble(children, comments, None)
    }

    /// A fixture over a workflow the test writes: `files` are paths
    /// relative to the workflow folder and their text.
    fn with_workflow(
        children: Vec<(Ulid, Vec<Vec<ProviderEvent>>)>,
        files: &[(&str, &str)],
    ) -> Self {
        Self::assemble(children, Vec::new(), Some(files))
    }

    fn assemble(
        children: Vec<(Ulid, Vec<Vec<ProviderEvent>>)>,
        comments: Vec<String>,
        files: Option<&[(&str, &str)]>,
    ) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let remote = dir.path().join("remote.git");
        // A real repository with a real bare remote: a writing step's
        // checks, its push and the closing comment all read git, so the
        // fixture gives them one to read. See `init_git`.
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&remote).unwrap();
        let workflow = match files {
            Some(files) => {
                let folder = dir.path().join("workflow");
                for (rel, text) in files {
                    let path = folder.join(rel);
                    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                    std::fs::write(&path, text).unwrap();
                }
                WorkflowFile::load_dir(&folder, WorkflowOrigin::Project)
                    .expect("the test's workflow loads")
            }
            None => {
                let mut workflow = WorkflowFile::load_dir(
                    &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../workflows/build"),
                    WorkflowOrigin::Bundled,
                )
                .expect("the build workflow loads");
                // The runner's own tests drive the commit checks. E4 (the
                // gate), E5 and E7 (the report) read a child's tool calls,
                // and the fixture's children are scripted replies, not real
                // work: those three have their tests in `tests/checks.rs`.
                // Everything else is the bundled workflow, so a test can
                // still assert on its real ids, markers and templates.
                let implementer = workflow
                    .workflow
                    .steps
                    .iter_mut()
                    .find(|step| step.id == "implement-alone")
                    .expect("the bundled workflow has the implementer step");
                implementer.checks = vec!["E1".to_owned(), "E2".to_owned(), "E3".to_owned()];
                workflow
            }
        };
        init_git(&repo, &remote);
        let initial_head = {
            let out = std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&repo)
                .output()
                .expect("git runs");
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        };
        let lead = Ulid::generate();
        let mut log = ThreadLog::open(dir.path(), lead).unwrap();
        log.append(aigentic_log::NewEvent {
            kind: EventKind::RunStarted,
            author: Author::User(UserId("steve".into())),
            payload: serde_json::to_value(run_started(&workflow)).unwrap(),
            parent_event: None,
        })
        .unwrap();
        let forge = Arc::new(FakeForge::with_comments(issue_view(), comments));
        Self {
            dir,
            lead,
            repo,
            remote,
            installer: FakeInstaller::echoing_head(),
            initial_head,
            children,
            forge,
            workflow,
            caps: BTreeMap::new(),
            prices: None,
        }
    }

    /// The child the run gave the implementer step, from the lead log:
    /// `trace` commits the brief's work through it.
    fn implementer_child(&self) -> Option<Ulid> {
        step_started(&self.lead_events())
            .into_iter()
            .find(|started| started.step == "implement-alone" && started.attempt == 1)
            .map(|started| started.child_thread)
    }

    /// Give a child a turn cap, so a scripted turn can end on it.
    fn capped(mut self, id: Ulid, max_iterations: u32) -> Self {
        self.caps.insert(id, max_iterations);
        self
    }

    /// Price the children's endpoint, so their `usage` lines carry a real
    /// `cost_usd` for the runner to sum.
    fn priced(mut self, prices: Prices) -> Self {
        self.prices = Some(prices);
        self
    }

    /// A runner over the log as it stands, with the fixture's forge.
    fn runner(&self) -> TestRunner {
        let log = ThreadLog::open(self.dir.path(), self.lead).unwrap();
        let host = FakeHost::new(
            self.dir.path().to_path_buf(),
            self.lead,
            &self.children,
            self.caps.clone(),
            self.prices,
        );
        Runner::new(
            log,
            self.lead,
            self.forge.clone(),
            host,
            Box::new(self.installer.clone()),
            self.workflow.clone(),
            GitRepo::new(self.repo.clone()),
        )
        .expect("the log is the lead's")
    }

    /// A fixture whose writes land in a repository the checks pass.
    fn with_installer(mut self, installer: FakeInstaller) -> Self {
        self.installer = installer;
        self
    }

    /// The checks a named step runs, replacing the workflow's.
    fn with_checks_for(mut self, step: &str, ids: &[&str]) -> Self {
        let step = self
            .workflow
            .workflow
            .steps
            .iter_mut()
            .find(|candidate| candidate.id == step)
            .expect("the fixture names a step the workflow has");
        step.checks = ids.iter().map(|id| (*id).to_owned()).collect();
        self
    }

    /// The checks the implementer runs.
    fn with_checks(self, ids: &[&str]) -> Self {
        self.with_checks_for("implement-alone", ids)
    }

    /// A step that pushes nothing: the run stops after its checks instead.
    fn without_push(mut self) -> Self {
        let step = self
            .workflow
            .workflow
            .steps
            .iter_mut()
            .find(|candidate| candidate.id == "implement-alone")
            .expect("the bundled workflow has the implementer step");
        step.push = false;
        self
    }

    /// Someone else pushes to the remote while a step runs. Returns the
    /// commit they pushed, so a test can tell it from the step's own head.
    fn push_a_stranger(&self) -> String {
        let other = self.dir.path().join("stranger");
        let _ = std::fs::remove_dir_all(&other);
        self.git_in(
            self.dir.path(),
            &[
                "clone",
                "--branch",
                "main",
                self.remote.to_str().unwrap(),
                "stranger",
            ],
        );
        std::fs::write(other.join("stranger.txt"), "someone else\n").unwrap();
        self.git_in(&other, &["add", "stranger.txt"]);
        self.git_in(
            &other,
            &[
                "-c",
                "user.name=stranger",
                "-c",
                "user.email=stranger@example.invalid",
                "commit",
                "-m",
                "stranger: a commit the step never made",
            ],
        );
        let head = self.git_in(&other, &["rev-parse", "HEAD"]);
        self.git_in(&other, &["push", "origin", "HEAD:main"]);
        head
    }

    /// The work repo's head before the fixture made any work commit.
    fn initial_head(&self) -> String {
        self.initial_head.clone()
    }

    /// Run `git` in the work repo, panicking on failure.
    fn git(&self, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(&self.repo)
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// Run `git` somewhere else, panicking on failure.
    fn git_in(&self, dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?} in {} failed: {}",
            dir.display(),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// Commit one file change with the trailer the E1 check wants, so a
    /// step's writes pass the checks the fixture's workflow runs.
    fn commit_named(&self, subject: &str) {
        let path = self.repo.join("work.txt");
        let mut text = std::fs::read_to_string(&path).unwrap_or_default();
        text.push_str(subject);
        text.push('\n');
        std::fs::write(&path, text).unwrap();
        self.git(&["add", "work.txt"]);
        self.git(&["commit", "-m", &format!("{subject}\n\n{TRAILER}")]);
    }

    /// Rewrite the last commit's subject, keeping its trailer: what an
    /// implementer does when the checks say the subject is wrong.
    fn amend_named(&self, subject: &str) {
        self.git(&[
            "commit",
            "--amend",
            "-m",
            &format!("{subject}\n\n{TRAILER}"),
        ]);
    }

    /// The work repo's HEAD.
    fn head(&self) -> String {
        self.git(&["rev-parse", "HEAD"])
    }

    /// The subject of the work repo's HEAD.
    fn head_subject(&self) -> String {
        self.git(&["log", "-1", "--format=%s"])
    }

    /// The bare remote's `main`, read with `ls-remote` so nothing has to
    /// be fetched into the work repo to see it.
    fn remote_head(&self) -> String {
        let out = std::process::Command::new("git")
            .args([
                "ls-remote",
                self.remote.to_str().unwrap(),
                "refs/heads/main",
            ])
            .output()
            .expect("git runs");
        String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_owned()
    }

    /// The issue the run is for, as `run_started` says.
    fn issue(&self) -> u64 {
        let events = self.lead_events();
        let event = events
            .iter()
            .find(|event| event.kind == EventKind::RunStarted)
            .expect("the daemon wrote run_started");
        let payload: RunStartedPayload = serde_json::from_value(event.payload.clone()).unwrap();
        payload.issue
    }

    /// The lead log's events.
    fn lead_events(&self) -> Vec<Event> {
        ThreadLog::open(self.dir.path(), self.lead)
            .unwrap()
            .read_all()
            .unwrap()
    }

    /// Every child that exists, with its events.
    fn child_events(&self) -> BTreeMap<Ulid, Vec<Event>> {
        self.children
            .iter()
            .map(|(id, _)| *id)
            .filter(|id| self.child_exists(*id))
            .map(|id| {
                let events = ThreadLog::open(self.dir.path(), id)
                    .unwrap()
                    .read_all()
                    .unwrap();
                (id, events)
            })
            .collect()
    }

    fn child_events_of(&self, id: Ulid) -> Vec<Event> {
        ThreadLog::open(self.dir.path(), id)
            .unwrap()
            .read_all()
            .unwrap()
    }

    fn child_exists(&self, id: Ulid) -> bool {
        self.dir.path().join(format!("{id}.jsonl")).exists()
    }

    /// Put the world back to what it was after `snapshot`: the lead log,
    /// every child's log, and the children that did not exist yet.
    fn restore(&self, snapshot: &Snapshot) {
        self.restore_logs(snapshot);
        // The repository and the remote are rewound with the logs: the
        // world a rebuild starts from is the world of that moment.
        self.git(&["checkout", "--force", "-B", "main", &snapshot.head]);
        self.git_in(
            &self.remote,
            &["update-ref", "refs/heads/main", &snapshot.remote_head],
        );
    }

    /// Rewind only the logs, leaving the repository alone.
    fn restore_logs(&self, snapshot: &Snapshot) {
        write_events(
            &self.dir.path().join(format!("{}.jsonl", self.lead)),
            &snapshot.lead,
        );
        for (id, _) in &self.children {
            let path = self.dir.path().join(format!("{id}.jsonl"));
            match snapshot.children.get(id) {
                Some(events) => write_events(&path, events),
                None => {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
    }

    /// Restore the logs, but leave a named child's log as the longest
    /// prefix `keep` accepts: a crash inside the child's turn.
    fn restore_child_prefix(&self, snapshot: &Snapshot, id: Ulid, keep: impl Fn(&[Event]) -> bool) {
        self.restore(snapshot);
        let Some(events) = snapshot.children.get(&id) else {
            return;
        };
        let mut cut = 0;
        for end in 1..=events.len() {
            if keep(&events[..end]) {
                cut = end;
            }
        }
        write_events(&self.dir.path().join(format!("{id}.jsonl")), &events[..cut]);
    }

    /// As `restore`, but a named child's log keeps only its first `keep`
    /// events.
    fn restore_child(&self, snapshot: &Snapshot, id: Ulid, keep: usize) {
        self.restore(snapshot);
        if let Some(events) = snapshot.children.get(&id) {
            let keep = keep.min(events.len());
            write_events(
                &self.dir.path().join(format!("{id}.jsonl")),
                &events[..keep],
            );
        }
    }

    /// Put the forge back where `snapshot` found it: the comments it held
    /// and an open issue. A rebuild from a prefix needs this, because the
    /// full run that made the snapshot closed the issue on the way.
    fn rewind_forge(&self, snapshot: &Snapshot) {
        self.set_comments(snapshot.comments.clone());
        self.forge.set_closed(false);
    }

    /// What the issue holds, as a rebuild after a crash would find it.
    fn set_comments(&self, comments: Vec<String>) {
        *self.forge.comments.lock().unwrap() = comments;
    }

    /// As `restore`, but a named child's log is taken away: the crash
    /// landed between `step_started` and the child's creation.
    fn restore_without_child(&self, snapshot: &Snapshot, id: Ulid) {
        self.restore(snapshot);
        let _ = std::fs::remove_file(self.dir.path().join(format!("{id}.jsonl")));
    }
}

/// What the world held after one lead event: the lead's events and every
/// child's, plus the comments the forge had posted by then.
struct Snapshot {
    lead: Vec<Event>,
    children: BTreeMap<Ulid, Vec<Event>>,
    comments: Vec<String>,
    /// Where the work repo stood when the snapshot was taken. A rebuild
    /// starts from the tree of that moment, not from the commits a later
    /// move made, and a step's checks judge the commits between them.
    head: String,
    /// Where the bare remote stood, so a rebuilt push sees the remote the
    /// crash left behind rather than one a later move moved.
    remote_head: String,
}

/// A full run, kept so a test can rebuild from any point of it.
struct Trace {
    snapshots: Vec<Snapshot>,
    terminal: Advanced,
}

impl Trace {
    /// The state after the run's `k`-th lead event (1-based).
    fn after(&self, k: usize) -> &Snapshot {
        &self.snapshots[k - 1]
    }

    /// The events the full run ended with.
    fn lead(&self) -> &[Event] {
        &self.snapshots.last().unwrap().lead
    }

    /// The comments the full run ended with.
    fn comments(&self) -> Vec<String> {
        self.snapshots.last().unwrap().comments.clone()
    }

    /// The state after the `nth` lead event of a kind.
    fn after_where(&self, kind: EventKind, nth: usize) -> &Snapshot {
        let lead = self.lead();
        let mut seen = 0;
        for (i, event) in lead.iter().enumerate() {
            if event.kind == kind {
                seen += 1;
                if seen == nth {
                    return self.after(i + 1);
                }
            }
        }
        panic!("the run has no {nth}-th {kind:?}");
    }
}

/// Drive a fixture to its end, recording the world after every move.
///
/// A run that reaches a writing step's end reads the repository back: the
/// checks compare the commits between where the step started and HEAD. So
/// the driver makes the commits the brief named as soon as the
/// implementer's report is in its log, exactly where a real implementer
/// would have made them (#65).
async fn trace(fx: &Fixture) -> Trace {
    let mut committed = false;
    trace_between(fx, |_| {
        if let Some(child) = fx.implementer_child() {
            commit_what_the_brief_named(fx, child, &mut committed);
        }
    })
    .await
}

/// Drive a fixture move by move, letting the test do git work between
/// moves: `between(moves)` runs after the `moves`-th move. A writing
/// step's checks read the repository, so the commits its report claims
/// have to be made in the middle of the run.
async fn trace_between(fx: &Fixture, mut between: impl FnMut(usize)) -> Trace {
    let mut runner = fx.runner();
    let mut snapshots = vec![snapshot(fx)];
    let mut moves = 0;
    let terminal = loop {
        match runner.advance().await.expect("the run advances") {
            Advanced::Moved => {
                moves += 1;
                between(moves);
                snapshots.push(snapshot(fx));
            }
            other => break other,
        }
    };
    snapshots.push(snapshot(fx));
    Trace {
        snapshots,
        terminal,
    }
}

/// The commits the fixture's brief names, made as soon as the implementer
/// has reported and not before, with the trailer the E1 check wants. The
/// report is where a real implementer's commits exist; the run reads the
/// repo at the checks, which come after it.
fn commit_what_the_brief_named(fx: &Fixture, implementer: Ulid, done: &mut bool) {
    if *done {
        return;
    }
    let reported = fx
        .child_events_of(implementer)
        .iter()
        .any(|event| event.kind == EventKind::StepReported);
    // Only from the state before any of them: a rebuild from a point after
    // the report finds the commits already in the tree.
    if reported && fx.head() == fx.initial_head() {
        for subject in ["runtime: one", "runtime: two"] {
            fx.commit_named(subject);
        }
        *done = true;
    }
}

fn snapshot(fx: &Fixture) -> Snapshot {
    Snapshot {
        lead: fx.lead_events(),
        children: fx.child_events(),
        comments: fx.forge.posted(),
        head: fx.head(),
        remote_head: fx.remote_head(),
    }
}

/// Rebuild from `snapshot` and do one move.
async fn replay(fx: &Fixture, snapshot: &Snapshot) -> (Advanced, TestRunner) {
    fx.restore(snapshot);
    let mut runner = fx.runner();
    let advanced = runner.advance().await.expect("the rebuilt runner advances");
    (advanced, runner)
}

/// Drive a runner to its pause, doing the git work a run's middle needs:
/// once the implementer has reported, the commits its brief named land, so
/// a rebuilt run's checks read them and not an empty range.
async fn drive(fx: &Fixture, runner: &mut TestRunner) -> Advanced {
    let mut committed = false;
    drive_with(fx, runner, || {
        if let Some(child) = fx.implementer_child() {
            commit_what_the_brief_named(fx, child, &mut committed);
        }
    })
    .await
}

/// The same, with `between` called after each move: a run whose middle
/// needs other git work (a wrong subject, an amend) drives itself this
/// way, so a rebuilt run is caught up the same way the full one was.
async fn drive_with(_fx: &Fixture, runner: &mut TestRunner, mut between: impl FnMut()) -> Advanced {
    loop {
        match runner.advance().await.expect("the run advances") {
            Advanced::Moved => between(),
            other => return other,
        }
    }
}

/// The git work a wrong-subject run needs between its moves: commit a
/// subject the brief never named, then amend it once a check has failed.
struct WrongSubject {
    implementer: Ulid,
    committed: bool,
    amended: bool,
}

impl WrongSubject {
    fn new(implementer: Ulid) -> Self {
        Self {
            implementer,
            committed: false,
            amended: false,
        }
    }

    /// `fix` is whether the run is meant to pass: without it the subject
    /// stays wrong and the second check fails too.
    fn between(&mut self, fx: &Fixture, fix: bool) {
        let reported = fx
            .child_events_of(self.implementer)
            .iter()
            .any(|event| event.kind == EventKind::StepReported);
        if reported && !self.committed && fx.head() == fx.initial_head() {
            // "runtime: one" is named; "runtime: wrong" is not, so E2
            // fails on the commits the step made.
            fx.commit_named("runtime: one");
            fx.commit_named("runtime: wrong");
            self.committed = true;
        }
        // The amend is keyed on the log, not on this driver's own state:
        // a rebuild from a tree the full run already committed finds the
        // failing check in the log and must still fix the subject. It
        // never amends twice, so every prefix ends at the same commit the
        // full run ended at — the pushed sha is compared below.
        if fix
            && !self.amended
            && checks_failed(&fx.lead_events(), "implement-alone")
            && fx.head_subject() != "runtime: two"
        {
            fx.amend_named("runtime: two");
            self.amended = true;
        }
    }
}

/// Rewrite a JSONL log so it holds exactly `events`, as a prefix.
fn write_events(path: &Path, events: &[Event]) {
    let mut text = String::new();
    for event in events {
        text.push_str(&serde_json::to_string(event).expect("an event serialises"));
        text.push('\n');
    }
    std::fs::write(path, text).unwrap();
}

/// The kinds of a log's events.
fn kinds(events: &[Event]) -> Vec<EventKind> {
    events.iter().map(|event| event.kind).collect()
}

/// Every `step_finished` payload, oldest first.
fn step_finished(events: &[Event]) -> Vec<aigentic_log::StepFinishedPayload> {
    events
        .iter()
        .filter(|event| event.kind == EventKind::StepFinished)
        .map(|event| serde_json::from_value(event.payload.clone()).unwrap())
        .collect()
}

/// Every `checks_run` payload, oldest first.
fn checks_run(events: &[Event]) -> Vec<aigentic_log::ChecksRunPayload> {
    events
        .iter()
        .filter(|event| event.kind == EventKind::ChecksRun)
        .map(|event| serde_json::from_value(event.payload.clone()).unwrap())
        .collect()
}

/// Every `pushed` payload, oldest first.
fn pushed_events(events: &[Event]) -> Vec<aigentic_log::PushedPayload> {
    events
        .iter()
        .filter(|event| event.kind == EventKind::Pushed)
        .map(|event| serde_json::from_value(event.payload.clone()).unwrap())
        .collect()
}

/// The `run_finished` payload, if the run ended.
fn run_finished(events: &[Event]) -> aigentic_log::RunFinishedPayload {
    let event = events
        .iter()
        .find(|event| event.kind == EventKind::RunFinished)
        .expect("the run finished");
    serde_json::from_value(event.payload.clone()).unwrap()
}

/// The `run_finished` payloads, oldest first: a rebuilt runner must not
/// write a second one.
fn run_finished_all(events: &[Event]) -> Vec<aigentic_log::RunFinishedPayload> {
    events
        .iter()
        .filter(|event| event.kind == EventKind::RunFinished)
        .map(|event| serde_json::from_value(event.payload.clone()).unwrap())
        .collect()
}

/// Every `step_started` payload, oldest first.
fn step_started(events: &[Event]) -> Vec<aigentic_log::StepStartedPayload> {
    events
        .iter()
        .filter(|event| event.kind == EventKind::StepStarted)
        .map(|event| serde_json::from_value(event.payload.clone()).unwrap())
        .collect()
}

/// The `route_taken` payload, if the run took a route.
fn route_taken(events: &[Event]) -> Option<aigentic_log::RouteTakenPayload> {
    events
        .iter()
        .find(|event| event.kind == EventKind::RouteTaken)
        .map(|event| serde_json::from_value(event.payload.clone()).unwrap())
}

/// The `checkpoint_asked` payloads, oldest first.
fn checkpoints(events: &[Event]) -> Vec<aigentic_log::CheckpointAskedPayload> {
    events
        .iter()
        .filter(|event| event.kind == EventKind::CheckpointAsked)
        .map(|event| serde_json::from_value(event.payload.clone()).unwrap())
        .collect()
}

/// A child's report, read from the child's log.
fn child_report(fx: &Fixture, id: Ulid) -> StepReport {
    let events = fx.child_events_of(id);
    let event = events
        .iter()
        .find(|event| event.kind == EventKind::StepReported)
        .expect("the child reported");
    serde_json::from_value(event.payload.clone()).unwrap()
}

/// The prompts a log holds, in order: the runner's own messages.
fn prompts(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter(|event| is_prompt(event))
        .map(|event| {
            let payload: aigentic_log::UserMessagePayload =
                serde_json::from_value(event.payload.clone()).unwrap();
            payload
                .blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text(text) => Some(text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("")
        })
        .collect()
}

fn is_prompt(event: &Event) -> bool {
    event.kind == EventKind::UserMessage && event.author == Author::Agent(AgentId("runner".into()))
}

/// `run_state`'s view of where a log stands, as a debug string: the tests
/// compare moves, never hand-written event lists.
fn next_move(events: &[Event]) -> String {
    format!("{:?}", run_state(events).unwrap().next_move())
}

/// The gate an unanswered `checkpoint_asked` opened, if any.
fn gate(events: &[Event]) -> Option<String> {
    match run_state(events).unwrap().next_move() {
        NextMove::AwaitingCheckpoint { gate } => Some(gate),
        _ => None,
    }
}

/// The lines the latest gate showed the human.
fn gate_shown(events: &[Event]) -> Vec<String> {
    checkpoints(events)
        .pop()
        .map(|payload| payload.shown)
        .unwrap_or_default()
}

/// How many times a child's log ends a turn.
fn turn_ends(events: &[Event]) -> usize {
    events
        .iter()
        .filter(|event| event.kind == EventKind::TurnEnded)
        .count()
}

/// How many times a child's log was started.
fn thread_starteds(events: &[Event]) -> usize {
    events
        .iter()
        .filter(|event| event.kind == EventKind::ThreadStarted)
        .count()
}

// ---------------------------------------------------------------------------
// The happy path and the surrounding flows
// ---------------------------------------------------------------------------

/// The happy path's world: a brief that routes to implement-alone, which
/// reports `## Implementation`.
struct Happy {
    fx: Fixture,
    brief_child: Ulid,
    implementer_child: Ulid,
}

fn happy_path() -> Happy {
    let brief_child = Ulid::generate();
    let implementer_child = Ulid::generate();
    Happy {
        fx: Fixture::new(vec![
            (brief_child, vec![report("r1", brief_report())]),
            (implementer_child, vec![report("r2", implementer_report())]),
        ]),
        brief_child,
        implementer_child,
    }
}

/// T1s — a brief that writes its slots as text, as models do: `ui:
/// "false"`, and `commits` as a JSON list's text with objects for items
/// and prose after it, the shape slice 1's acceptance brief wrote. The implementer's prompt has no pty section, and E2 reads the
/// named subjects, so the run checks, pushes and closes.
#[tokio::test]
async fn t1s_slots_written_as_text_are_read_as_declared() {
    let brief_child = Ulid::generate();
    let implementer_child = Ulid::generate();
    let mut brief = brief_report();
    brief["slots"]["ui"] = json!("false");
    // The shape #29's second brief wrote: objects for items, prose after.
    brief["slots"]["commits"] = json!(
        "[{\"subject\": \"runtime: one\"}, {\"subject\": \"runtime: two\"}] (precedent: an earlier commit)"
    );
    let fx = &Fixture::new(vec![
        (brief_child, vec![report("r1", brief)]),
        (implementer_child, vec![report("r2", implementer_report())]),
    ]);
    let mut committed = false;
    let run = trace_between(fx, |_| {
        commit_what_the_brief_named(fx, implementer_child, &mut committed)
    })
    .await;

    let prompts = child_prompts(fx, implementer_child);
    assert!(
        !prompts[0].contains("## Pty check"),
        "`ui: \"false\"` drops the pty section: {}",
        prompts[0]
    );
    assert!(
        prompts[0].contains("runtime: one\nruntime: two"),
        "the commits render one subject per line: {}",
        prompts[0]
    );
    let checks = checks_run(run.lead());
    assert!(
        checks.iter().all(|run| run
            .checks
            .iter()
            .all(|check| check.result != aigentic_log::CheckResult::Fail)),
        "no check fails, E2 among them: {checks:?}"
    );
    assert_eq!(
        run.terminal,
        Advanced::Finished {
            outcome: aigentic_log::RunOutcome::Closed
        },
        "the run closes"
    );
}

/// T1: trivial happy path → `PausedAtChecks` on implement-alone; one
/// `## Brief` and one `## Implementation` comment;
/// `route_taken.budget_usd == brief budget slot`; `reported_event`
/// resolves in the child's log.
#[tokio::test]
async fn t1_the_happy_path_checks_pushes_installs_and_closes() {
    let h = happy_path();
    let fx = &h.fx;
    // The commits land while the implementer's turn is over and before the
    // handover, as a real implementer's would.
    let mut committed = false;
    let run = trace_between(fx, |_| {
        commit_what_the_brief_named(fx, h.implementer_child, &mut committed)
    })
    .await;
    let events = run.lead();

    let head = fx.head();
    let terminal = run.terminal.clone();
    assert_eq!(
        terminal,
        Advanced::Finished {
            outcome: aigentic_log::RunOutcome::Closed,
        },
        "the run passes its checks, pushes and closes"
    );
    assert_eq!(gate(events), None, "no gate is asked: the checks pass");

    let started = step_started(events);
    assert_eq!(started.len(), 2, "one child per step, no retry");
    assert_eq!(started[0].step, "brief");
    assert_eq!(started[0].role, "brief");
    assert_eq!(started[0].profile, "kimi");
    assert_eq!(started[0].attempt, 1);
    assert_eq!(started[0].child_thread, h.brief_child);
    assert_eq!(started[1].step, "implement-alone");
    assert_eq!(started[1].role, "implementer");
    assert_eq!(started[1].attempt, 1);
    assert_eq!(
        started[1].child_thread, h.implementer_child,
        "the brief's route started the step's own child"
    );

    // The brief's prompt is rendered from the template with the runner's
    // slots: the issue it is for, and the workflow's budget.
    let brief_prompts = prompts(&fx.child_events_of(h.brief_child));
    assert_eq!(brief_prompts.len(), 1, "one prompt for attempt 1");
    assert!(
        brief_prompts[0].contains(&issue_view().title),
        "the issue's title reaches the brief prompt"
    );
    assert!(
        brief_prompts[0].contains(&fx.workflow.workflow.budget.trivial.to_string()),
        "the workflow's trivial budget reaches it"
    );
    assert!(
        !brief_prompts[0].contains("{{"),
        "no template tag is left unrendered"
    );

    // The report the brief posted is what routes: the budget it set is
    // what `route_taken` records.
    let finished = step_finished(events);
    assert_eq!(finished.len(), 2);
    assert_eq!(finished[0].status, StepStatus::Done);
    let reported = finished[0].reported_event.expect("the brief reported");
    assert!(
        fx.child_events_of(h.brief_child)
            .iter()
            .any(|event| event.id == reported),
        "reported_event resolves in the child's log"
    );
    let brief_budget = brief_report()["slots"]["budget"]
        .as_f64()
        .expect("the fixture reports a budget");
    assert_eq!(
        route_taken(events).unwrap().budget_usd,
        Some(brief_budget),
        "the route records the budget the report set"
    );

    // The implementer's prompt carries the brief's slots, and the same
    // rules the workflow names are the ones the child was built with.
    let implementer_prompts = prompts(&fx.child_events_of(h.implementer_child));
    assert_eq!(implementer_prompts.len(), 1);
    let brief_args = brief_report();
    let purpose = brief_args["slots"]["purpose"]
        .as_str()
        .expect("the fixture reports a purpose");
    assert!(
        implementer_prompts[0].contains(purpose),
        "the brief's purpose reaches the implementer's prompt"
    );

    assert_eq!(
        child_report(fx, h.implementer_child).body.as_deref(),
        Some("## Implementation\n\nall landed")
    );

    // One comment per report, carrying the step's marker and the report.
    let posted = fx.forge.posted();
    assert_eq!(
        posted.len(),
        3,
        "one comment per reporting step, one closing"
    );
    let brief_marker = &fx.workflow.workflow.steps[0].marker;
    let implementer_marker = &fx.workflow.workflow.steps[1].marker;
    assert!(posted[0].contains(brief_marker.as_str()));
    assert!(posted[0].contains("the brief"));
    assert!(posted[1].contains(implementer_marker.as_str()));
    assert!(posted[1].contains("all landed"));

    // The checks ran on the step that wrote, and they passed.
    let checks = checks_run(events);
    assert_eq!(checks.len(), 1, "one checks_run for the writing step");
    assert_eq!(checks[0].step, "implement-alone");
    assert_eq!(
        checks[0]
            .checks
            .iter()
            .map(|c| c.id.as_str())
            .collect::<Vec<_>>(),
        vec!["E1", "E2", "E3"],
        "the step's checks, in the workflow's order"
    );
    assert!(
        checks[0]
            .checks
            .iter()
            .all(|check| check.result == aigentic_log::CheckResult::Pass),
        "the fixture's commits pass every check: {:?}",
        checks[0].checks
    );

    // The push landed on the bare remote, and `pushed` says what it moved:
    // from the remote head the step started at, to its own head.
    let pushed = pushed_events(events);
    assert_eq!(pushed.len(), 1, "one push");
    assert_eq!(
        pushed[0].ref_after, head,
        "the pushed ref is the step's head"
    );
    assert_eq!(
        pushed[0]
            .commits
            .iter()
            .map(|commit| commit.subject.as_str())
            .collect::<Vec<_>>(),
        vec!["runtime: two", "runtime: one"],
        "the commits the push carried, as `git log` lists them (newest first)"
    );
    assert_eq!(
        pushed[0].commits.first().map(|commit| commit.sha.as_str()),
        Some(head.as_str()),
        "the newest entry is the step's head"
    );
    assert_eq!(
        pushed[0].commits.last().map(|commit| commit.sha.as_str()),
        Some(fx.git(&["rev-parse", "HEAD~1"]).as_str()),
        "the oldest entry is the commit the step started from"
    );
    assert_eq!(fx.remote_head(), head, "the bare remote has the head");

    // The install is recorded, and names the commit just pushed.
    assert_eq!(fx.installer.calls(), 1, "the binary is installed once");
    let installed = pushed[0]
        .installed
        .as_deref()
        .expect("the push records the install");
    assert!(
        installed.contains(&head[..12]),
        "the installed binary names the pushed commit: {installed}"
    );

    // The closing comment is last, states the report's release impact, and
    // the issue is closed.
    let closing = fx.forge.posted().pop().expect("a closing comment");
    assert!(
        closing.contains("## Closing"),
        "the closing comment: {closing}"
    );
    assert!(
        closing.contains("all landed"),
        "with the implementer's report"
    );
    assert!(
        closing.contains("Release impact: patch"),
        "the closing states the report's impact: {closing}"
    );
    assert_eq!(
        implementer_report()["release_impact"],
        json!("patch"),
        "and the fixture's report is where that came from"
    );
    assert!(fx.forge.is_closed(), "the issue is closed");

    let finished = run_finished(events);
    assert_eq!(
        finished.outcome,
        aigentic_log::RunOutcome::Closed,
        "the run says how it ended"
    );
    assert_eq!(
        finished.release_impact,
        Some(aigentic_log::ReleaseImpact::Patch),
        "the closing comment's impact, as the event records it"
    );

    // The writing step held the write lock; the close released it.
    assert_eq!(
        WriteGuard::holder(&fx.repo),
        None,
        "the lock is free when the run is over"
    );
}

/// T2: `full` → `WaitingHuman{route}`, `budget_usd` set.
#[tokio::test]
async fn t2_the_full_route_asks_before_any_child_starts() {
    let brief_child = Ulid::generate();
    let mut full = brief_report();
    full["slots"]["size"] = json!("full");
    full["slots"]["budget"] = json!(4);
    let fx = Fixture::new(vec![(brief_child, vec![report("r1", full)])]);

    let mut runner = fx.runner();
    // The moves the state asks for, taken one at a time until the gate.
    let mut moves = 0;
    let paused = loop {
        match runner.advance().await.unwrap() {
            Advanced::Moved => moves += 1,
            paused => break paused,
        }
        assert!(moves < 10, "the run must not spin");
    };
    assert_eq!(moves, 3, "start the brief, finish it, take the route");
    assert_eq!(
        paused,
        Advanced::WaitingHuman {
            gate: "route".to_owned()
        },
        "the `full` route asks the human before it starts a step"
    );

    let events = fx.lead_events();
    let taken = route_taken(&events).expect("the route was written down");
    assert_eq!(taken.proposed, "full");
    assert_eq!(taken.taken, "ask");
    assert_eq!(
        taken.budget_usd,
        Some(4.0),
        "the budget the report set, not the workflow's"
    );
    assert_ne!(
        fx.workflow.workflow.budget.full, 4.0,
        "and it is not the workflow's number"
    );
    assert_eq!(step_started(&events).len(), 1, "no child beyond the brief");
    assert_eq!(gate(&events).as_deref(), Some("route"));
    assert_eq!(
        fx.forge.posted().len(),
        1,
        "the brief still posts its report"
    );
}

/// T9: rebuild the `Runner` after **every** lead event of `run`; each
/// next `advance()` does the move once, and the rebuilt runner never
/// repeats a move or stops somewhere else.
async fn sweep_every_prefix(
    fx: &Fixture,
    run: &Trace,
    implementer: Ulid,
    mut catch_up: impl FnMut() -> bool,
) {
    let n = run.lead().len();
    assert!(n >= 5, "the happy path is several moves long");

    for k in 1..=n {
        let snapshot = run.after(k);
        let (advanced, mut runner) = replay(fx, snapshot).await;
        let after = fx.lead_events();
        if k < n {
            // A move was due. The last of them ends the run, so the
            // rebuilt runner's one move finishes it rather than moving on.
            if run.lead()[k].kind != EventKind::RunFinished {
                assert_eq!(advanced, Advanced::Moved, "prefix {k}: a move was due");
            }
            assert_eq!(
                after.len(),
                k + 1,
                "prefix {k}: exactly one lead event was appended"
            );
            assert_eq!(
                after[k].kind,
                run.lead()[k].kind,
                "prefix {k}: the move the full run made"
            );
            assert_eq!(
                after[k].payload,
                run.lead()[k].payload,
                "prefix {k}: with the same payload"
            );
            assert_ne!(
                next_move(&after),
                next_move(&snapshot.lead),
                "prefix {k}: the move did not repeat itself"
            );
        } else {
            assert_eq!(advanced, run.terminal, "prefix {k}: the run pauses here");
            assert_eq!(after.len(), n, "prefix {k}: and writes nothing more");
        }

        // Catching up from the rebuilt state changes nothing either.
        let done = if catch_up() {
            let mut subject = WrongSubject::new(implementer);
            drive_with(fx, &mut runner, || subject.between(fx, true)).await
        } else {
            drive(fx, &mut runner).await
        };
        assert_eq!(done, run.terminal, "prefix {k}: the same pause");
        assert_eq!(
            kinds(&fx.lead_events()),
            kinds(run.lead()),
            "prefix {k}: the same moves, once each"
        );
    }
}

/// T3: rebuild the `Runner` after **every** lead event of T1's run; each
/// next `advance()` does the move once.
#[tokio::test]
async fn t3_a_rebuild_after_every_lead_event_does_each_move_once() {
    let h = happy_path();
    let run = trace(&h.fx).await;
    assert!(
        run.lead().len() >= 5,
        "the happy path is several moves long"
    );
    sweep_every_prefix(&h.fx, &run, h.implementer_child, || false).await;
}

/// T4: a cap then a report — `step_finished(1, Partial, "max_iterations")`,
/// `step_started(2)` on the same child, `continue` in the child, then the
/// report.
#[tokio::test]
async fn t4_a_cap_is_retried_in_the_same_child_with_continue() {
    let brief_child = Ulid::generate();
    let implementer_child = Ulid::generate();
    let fx = Fixture::new(vec![
        (brief_child, vec![report("r1", brief_report())]),
        (
            implementer_child,
            vec![works("w1"), report("r2", implementer_report())],
        ),
    ])
    .capped(implementer_child, 1);

    let mut committed = false;
    let run = trace_between(&fx, |_| {
        commit_what_the_brief_named(&fx, implementer_child, &mut committed)
    })
    .await;
    let events = run.lead();
    assert_eq!(
        run.terminal,
        Advanced::Finished {
            outcome: aigentic_log::RunOutcome::Closed,
        },
        "the retry passes its checks and closes"
    );

    let started = step_started(events);
    assert_eq!(
        started.len(),
        3,
        "the implementer started twice, no third child"
    );
    assert_eq!(started[1].step, "implement-alone");
    assert_eq!(started[1].attempt, 1);
    assert_eq!(started[2].step, "implement-alone");
    assert_eq!(started[2].attempt, 2);
    assert_eq!(
        started[2].child_thread, started[1].child_thread,
        "a retry keeps the same child"
    );
    assert_eq!(
        started[2].child_thread, implementer_child,
        "and it is the child the fixture scripted"
    );

    let finished = step_finished(events);
    assert_eq!(finished[1].status, StepStatus::Partial);
    assert_eq!(finished[1].end_reason, "max_iterations");
    assert_eq!(finished[1].reported_event, None);
    assert_eq!(finished[2].status, StepStatus::Done);
    assert_eq!(finished[2].end_reason, STEP_REPORTED);

    let messages = prompts(&fx.child_events_of(implementer_child));
    assert_eq!(messages.len(), 2, "one message per attempt");
    assert_eq!(
        messages[1], CONTINUE_PROMPT,
        "a cap is continued, not re-briefed"
    );
    assert_ne!(
        messages[0], messages[1],
        "attempt 1 was the rendered prompt"
    );
    assert!(!messages[0].contains("{{"), "rendered, not raw");

    // Attempt 1 reported nothing, so the comments are the brief's report,
    // the implementer's, and the run's closing one.
    let posted = fx.forge.posted();
    assert_eq!(posted.len(), 3, "two reports and the closing comment");
    assert!(
        posted[1].contains("attempt=2"),
        "the report says which attempt"
    );
    assert!(
        posted[2].contains("## Closing"),
        "and the last comment closes the run"
    );
}

/// T5: a second cap → `WaitingHuman{step_stop}`.
#[tokio::test]
async fn t5_a_second_cap_escalates() {
    let brief_child = Ulid::generate();
    let implementer_child = Ulid::generate();
    let fx = Fixture::new(vec![
        (brief_child, vec![report("r1", brief_report())]),
        (implementer_child, vec![works("w1"), works("w2")]),
    ])
    .capped(implementer_child, 1);

    let mut runner = fx.runner();
    let paused = drive(&fx, &mut runner).await;
    assert_eq!(
        paused,
        Advanced::WaitingHuman {
            gate: "step_stop".to_owned()
        },
        "twice cut off is the human's problem"
    );

    let events = fx.lead_events();
    assert_eq!(step_finished(&events).len(), 3);
    assert_eq!(
        step_started(&events).len(),
        3,
        "the second cap starts no third attempt"
    );
    let shown = gate_shown(&events);
    assert!(
        shown.iter().any(|line| line.contains("max_iterations")),
        "the gate says why: {shown:?}"
    );
    assert_eq!(fx.forge.posted().len(), 1, "only the brief reported");
}

/// T6: `done` without a report → `call finish_step` posted, then the report.
#[tokio::test]
async fn t6_a_turn_that_stops_without_reporting_is_told_to_finish() {
    let brief_child = Ulid::generate();
    let implementer_child = Ulid::generate();
    let fx = Fixture::new(vec![
        (brief_child, vec![report("r1", brief_report())]),
        (
            implementer_child,
            vec![stop("done"), report("r2", implementer_report())],
        ),
    ]);

    let mut committed = false;
    let run = trace_between(&fx, |_| {
        commit_what_the_brief_named(&fx, implementer_child, &mut committed)
    })
    .await;
    assert_eq!(
        run.terminal,
        Advanced::Finished {
            outcome: aigentic_log::RunOutcome::Closed,
        },
        "the report arrives and the run closes"
    );
    let messages = prompts(&fx.child_events_of(implementer_child));
    assert_eq!(messages.len(), 2);
    assert_eq!(
        messages[1], CALL_FINISH_STEP,
        "a turn that stopped on its own is told to report"
    );
    let finished = step_finished(run.lead());
    assert_eq!(finished[1].end_reason, "done");
    assert_eq!(finished[1].status, StepStatus::Partial);
}

/// T7: a second missing report → `step_stop`.
#[tokio::test]
async fn t7_a_second_missing_report_escalates() {
    let brief_child = Ulid::generate();
    let implementer_child = Ulid::generate();
    let fx = Fixture::new(vec![
        (brief_child, vec![report("r1", brief_report())]),
        (implementer_child, vec![stop("done"), stop("done")]),
    ]);

    let mut runner = fx.runner();
    assert_eq!(
        drive(&fx, &mut runner).await,
        Advanced::WaitingHuman {
            gate: "step_stop".to_owned()
        }
    );
    assert_eq!(step_started(&fx.lead_events()).len(), 3);
}

/// T7 (#96): a child whose turn ended at the model's output limit is
/// `Partial` — a budget-style stop, like `max_tokens` — not `Failed`, and
/// nothing escalates: the step is continued in the same child.
#[tokio::test]
async fn t7_a_length_stop_is_partial_and_does_not_escalate() {
    let brief_child = Ulid::generate();
    let implementer_child = Ulid::generate();
    let fx = Fixture::new(vec![
        (brief_child, vec![report("r1", brief_report())]),
        (
            implementer_child,
            // The first reply runs out of output room mid-answer; the
            // continue's reply reports.
            vec![stop(LENGTH_STOP), report("r2", implementer_report())],
        ),
    ]);

    let mut committed = false;
    let run = trace_between(&fx, |_| {
        commit_what_the_brief_named(&fx, implementer_child, &mut committed)
    })
    .await;

    let finished = step_finished(run.lead());
    assert_eq!(
        finished[1].status,
        StepStatus::Partial,
        "a length stop is Partial, like a budget stop"
    );
    assert_eq!(finished[1].end_reason, LENGTH_STOP);
    assert_eq!(finished[1].reported_event, None);
    assert!(
        !checkpoints(run.lead())
            .iter()
            .any(|c| c.shown.iter().any(|g| g == "step_stop")),
        "nothing escalated: no step_stop gate"
    );
    // The step is continued in the same child, as t4's cap is.
    let started = step_started(run.lead());
    assert_eq!(started.len(), 3);
    assert_eq!(started[2].step, "implement-alone");
    assert_eq!(started[2].attempt, 2);
    assert_eq!(started[2].child_thread, implementer_child);
    let messages = prompts(&fx.child_events_of(implementer_child));
    assert_eq!(messages[1], CONTINUE_PROMPT);
}

/// T13: `resumed` treated as `done`.
#[tokio::test]
async fn t13_resumed_is_treated_as_done() {
    // A child whose turn was answered but never ended: the machine died
    // between its last answer and `turn_ended`. A rebuild repairs that as
    // `resumed`, which the lead must treat like `done`.
    let brief_child = Ulid::generate();
    let implementer_child = Ulid::generate();
    let fx = Fixture::new(vec![
        (brief_child, vec![report("r1", brief_report())]),
        (
            implementer_child,
            vec![
                stop("the reply the seeded world already consumed"),
                report("r2", implementer_report()),
            ],
        ),
    ]);
    let run = trace(&fx).await;
    assert_eq!(
        run.terminal,
        Advanced::Finished {
            outcome: aigentic_log::RunOutcome::Closed
        },
        "the run goes on to close"
    );
    // The child is created; nothing else of the attempt exists: the prompt
    // below is the one the run posted, the answer the one it got back.
    fx.restore_child_prefix(
        run.after_where(EventKind::StepStarted, 2),
        implementer_child,
        |events| events.len() <= 1,
    );

    // The prompt is already in the child's log; its answer came, its turn
    // never ended.
    let mut child = ThreadLog::open(fx.dir.path(), implementer_child).unwrap();
    child
        .append(aigentic_log::NewEvent {
            kind: EventKind::UserMessage,
            author: Author::Agent(AgentId("runner".into())),
            payload: json!({"blocks": [{"type": "text", "text": "the prompt"}]}),
            parent_event: None,
        })
        .unwrap();
    child
        .append(aigentic_log::NewEvent {
            kind: EventKind::AssistantMessage,
            author: Author::Agent(AgentId("child".into())),
            payload: json!({"blocks": [{"type": "text", "text": "the answer"}]}),
            parent_event: None,
        })
        .unwrap();

    let mut runner = fx.runner();
    assert_eq!(
        drive(&fx, &mut runner).await,
        run.terminal,
        "the repaired turn ends the attempt; the run goes on to close"
    );
    let events = fx.lead_events();
    let finished = step_finished(&events);
    assert_eq!(finished[1].end_reason, "resumed");
    assert_eq!(finished[1].status, StepStatus::Partial);
    let messages = prompts(&fx.child_events_of(implementer_child));
    assert_eq!(
        messages[1], CALL_FINISH_STEP,
        "`resumed` is treated like `done`: report what you have"
    );
}

#[tokio::test]
async fn t14_a_report_without_a_body_escalates_and_posts_nothing() {
    let brief_child = Ulid::generate();
    let implementer_child = Ulid::generate();
    let mut headless = implementer_report();
    headless["body"] = json!(null);
    let fx = Fixture::new(vec![
        (brief_child, vec![report("r1", brief_report())]),
        (implementer_child, vec![report("r2", headless)]),
    ]);

    let mut runner = fx.runner();
    assert_eq!(
        drive(&fx, &mut runner).await,
        Advanced::WaitingHuman {
            gate: "step_stop".to_owned()
        }
    );

    let events = fx.lead_events();
    assert_eq!(
        step_finished(&events).len(),
        1,
        "the headless report is not a step_finished"
    );
    assert_eq!(
        fx.forge.posted().len(),
        1,
        "nothing is posted for a report with no body"
    );
    let shown = gate_shown(&events);
    assert!(
        shown.iter().any(|line| line.contains("no body")),
        "the gate says the body is missing: {shown:?}"
    );
}

/// T16: a report already in the log is the attempt's outcome, whatever
/// the latest `turn_ended` says — the review's J9 world, where the daemon
/// died between a child's `step_reported` and its `turn_ended`.
#[tokio::test]
async fn t16_a_written_report_is_the_attempts_outcome() {
    let h = happy_path();
    let fx = &h.fx;
    let run = trace(fx).await;
    // The crash landed inside the brief's turn, after its report.
    fx.restore_child_prefix(
        run.after_where(EventKind::StepStarted, 1),
        h.brief_child,
        |events| {
            !events
                .iter()
                .any(|event| event.kind == EventKind::TurnEnded)
        },
    );
    let events = fx.child_events_of(h.brief_child);
    let report_id = events
        .iter()
        .find(|event| event.kind == EventKind::StepReported)
        .expect("the report was written")
        .id;
    assert_eq!(turn_ends(&events), 0, "the turn never ended");

    let mut runner = fx.runner();
    assert_eq!(
        drive(fx, &mut runner).await,
        run.terminal,
        "the run continues past the repaired step to its end"
    );
    let lead = fx.lead_events();
    let finished = step_finished(&lead);
    assert_eq!(finished[0].end_reason, STEP_REPORTED);
    assert_eq!(finished[0].status, StepStatus::Done);
    assert_eq!(
        finished[0].reported_event,
        Some(report_id),
        "the report in the log is the attempt's outcome"
    );
    assert_eq!(
        prompts(&events).len(),
        1,
        "no second attempt: the turn is not made to run again"
    );
    let posted = fx.forge.posted();
    let reports: Vec<&String> = posted
        .iter()
        .filter(|body| !body.contains("step=close"))
        .collect();
    assert_eq!(
        reports.len(),
        2,
        "the written report is posted once, then the implementer's"
    );
    assert_eq!(
        fx.forge
            .posted()
            .iter()
            .filter(|body| body.contains("step=close"))
            .count(),
        1,
        "and the run closes the issue with its own comment"
    );
    assert!(
        fx.forge.posted()[0].contains("## Brief"),
        "the brief's own report is the one posted"
    );
}

/// T17: a step's cost is the price its children's `usage` lines carry,
/// summed, and `-0.0` never appears.
#[tokio::test]
async fn t17_a_steps_cost_is_the_childrens_usage() {
    // The same two-report world as `happy_path`, priced.
    let brief_child = Ulid::generate();
    let implementer_child = Ulid::generate();
    let prices = Prices {
        input: 3.0,
        cache_read: 0.5,
        cache_write: 4.0,
        output: 15.0,
    };
    let fx = Fixture::new(vec![
        (brief_child, vec![report("r1", brief_report())]),
        (implementer_child, vec![report("r2", implementer_report())]),
    ])
    .priced(prices);
    // The expected cost comes from the fixture's own price table and the
    // tokens the scripts report (`usage(10, 5)`).
    let expected = prices.cost_usd(&aigentic_log::Usage::reported(aigentic_core::Usage {
        input_tokens: 10,
        output_tokens: 5,
        ..Default::default()
    }));
    assert!(expected > 0.0, "the fixture prices a real cost");

    let mut runner = fx.runner();
    assert_eq!(
        drive(&fx, &mut runner).await,
        Advanced::Finished {
            outcome: aigentic_log::RunOutcome::Closed
        },
        "the priced run closes"
    );

    let events = fx.lead_events();
    let finished = step_finished(&events);
    assert_eq!(
        finished[0].cost_usd, expected,
        "the brief's step cost is its child's usage"
    );
    assert_eq!(
        finished[1].cost_usd, expected,
        "the implementer's step cost is its child's usage"
    );
    for step in &finished {
        assert_ne!(
            step.cost_usd.to_bits(),
            (-0.0_f64).to_bits(),
            "`-0.0` never appears"
        );
    }
    let state = run_state(&events).unwrap();
    assert_eq!(state.cost_usd, expected * 2.0, "the run sums its steps");
    assert_eq!(
        runner.state().unwrap().cost_usd,
        expected * 2.0,
        "the runner's own state agrees"
    );
}

/// T8: rebuild after `step_started`, no child log → one child, one prompt.
#[tokio::test]
async fn t8_a_rebuild_without_the_child_creates_and_prompts_once() {
    let h = happy_path();
    let fx = &h.fx;
    let run = trace(fx).await;
    let snapshot = run.after_where(EventKind::StepStarted, 1);
    // The crash landed between the append and the child's creation.
    fx.restore_without_child(snapshot, h.brief_child);
    assert!(!fx.child_exists(h.brief_child));

    let (advanced, mut runner) = replay(fx, snapshot).await;
    assert_eq!(advanced, Advanced::Moved);
    assert!(fx.child_exists(h.brief_child), "the child is created once");
    assert_eq!(
        prompts(&fx.child_events_of(h.brief_child)).len(),
        1,
        "one prompt, not two"
    );
    drive(fx, &mut runner).await;
    assert_eq!(
        prompts(&fx.child_events_of(h.brief_child)).len(),
        1,
        "and no second prompt later"
    );
    assert_eq!(
        step_started(&fx.lead_events()).len(),
        2,
        "no second step_started for the same attempt"
    );
}

/// T8b: the child exists, the prompt was never posted → one prompt.
#[tokio::test]
async fn t8b_a_child_without_the_prompt_gets_exactly_one() {
    let h = happy_path();
    let fx = &h.fx;
    let run = trace(fx).await;
    let snapshot = run.after_where(EventKind::StepStarted, 1);
    // The child was made, the prompt was not: keep only its
    // `thread_started`.
    fx.restore_child(snapshot, h.brief_child, 1);
    assert_eq!(prompts(&fx.child_events_of(h.brief_child)).len(), 0);

    let (advanced, mut runner) = replay(fx, snapshot).await;
    assert_eq!(advanced, Advanced::Moved);
    assert_eq!(
        prompts(&fx.child_events_of(h.brief_child)).len(),
        1,
        "the missing prompt is posted once"
    );
    assert_eq!(
        thread_starteds(&fx.child_events_of(h.brief_child)),
        1,
        "the existing child is not created again"
    );
    drive(fx, &mut runner).await;
    assert_eq!(prompts(&fx.child_events_of(h.brief_child)).len(), 1);
}

/// T8c: rebuild after `step_started(2)` before its `continue` → exactly one
/// `continue`, no escalation.
#[tokio::test]
async fn t8c_a_rebuild_before_the_continue_posts_it_once() {
    let brief_child = Ulid::generate();
    let implementer_child = Ulid::generate();
    let fx = Fixture::new(vec![
        (brief_child, vec![report("r1", brief_report())]),
        (
            implementer_child,
            vec![works("w1"), report("r2", implementer_report())],
        ),
    ])
    .capped(implementer_child, 1);
    let run = trace(&fx).await;
    assert_eq!(
        run.terminal,
        Advanced::Finished {
            outcome: aigentic_log::RunOutcome::Closed
        },
        "the run closes"
    );

    // The lead has `step_started(2)`; the child's log holds attempt 1's
    // prompt and its end, but not the `continue`.
    let k = run
        .lead()
        .iter()
        .rposition(|event| {
            event.kind == EventKind::StepStarted
                && serde_json::from_value::<aigentic_log::StepStartedPayload>(event.payload.clone())
                    .is_ok_and(|payload| payload.attempt == 2)
        })
        .expect("attempt 2 started");
    let snapshot = run.after(k + 1);
    // The child's log ends after attempt 1's turn: the `continue` was
    // never posted (the crash landed between `step_started` and it).
    fx.restore_child_prefix(snapshot, implementer_child, |events| turn_ends(events) <= 1);

    let (advanced, mut runner) = replay(&fx, snapshot).await;
    assert_eq!(advanced, Advanced::Moved, "no escalation");
    let messages = prompts(&fx.child_events_of(implementer_child));
    assert_eq!(
        messages,
        vec![messages[0].clone(), CONTINUE_PROMPT.to_owned()],
        "the `continue` is posted once, after the rendered prompt"
    );
    assert_eq!(messages.iter().filter(|m| *m == CONTINUE_PROMPT).count(), 1);
    let terminal = drive(&fx, &mut runner).await;
    assert_eq!(
        terminal, run.terminal,
        "and the run carries on rather than escalating"
    );
}

/// T8d: rebuild after `step_finished(1, Partial)` before `step_started(2)`
/// → attempt 2 starts once; a later second cap still escalates.
#[tokio::test]
async fn t8d_a_rebuild_before_the_retry_starts_it_once() {
    let brief_child = Ulid::generate();
    let implementer_child = Ulid::generate();
    let fx = Fixture::new(vec![
        (brief_child, vec![report("r1", brief_report())]),
        (implementer_child, vec![works("w1"), works("w2")]),
    ])
    .capped(implementer_child, 1);
    let run = trace(&fx).await;
    assert_eq!(
        run.terminal,
        Advanced::WaitingHuman {
            gate: "step_stop".to_owned()
        }
    );

    // Everything up to the first cap's `step_finished`.
    let snapshot = run.after_where(EventKind::StepFinished, 2);
    let (advanced, mut runner) = replay(&fx, snapshot).await;
    assert_eq!(advanced, Advanced::Moved, "the retry is due");
    assert_eq!(
        step_started(&fx.lead_events()).len(),
        3,
        "attempt 2 started exactly once"
    );
    let attempts: Vec<u32> = step_started(&fx.lead_events())
        .iter()
        .map(|payload| payload.attempt)
        .collect();
    assert_eq!(attempts, vec![1, 1, 2]);
    // The second cap still escalates, exactly as in the full run.
    assert_eq!(
        drive(&fx, &mut runner).await,
        Advanced::WaitingHuman {
            gate: "step_stop".to_owned()
        }
    );
    assert_eq!(
        kinds(&fx.lead_events()),
        kinds(run.lead()),
        "the rebuilt run's moves match the full run's"
    );
}

/// T9: rebuild before a post, between a post and the next event, and after
/// it → one comment per tag; a second attempt posts its own.
#[tokio::test]
async fn t9_a_rebuild_around_a_post_never_posts_twice() {
    let h = happy_path();
    let fx = &h.fx;
    let run = trace(fx).await;
    let full = run.lead();
    let posted = fx.forge.posted();
    assert_eq!(posted.len(), 3, "two reports and the closing comment");

    // Every lead event boundary of the happy path: rebuild there, catch
    // up, and see the same comments once each. A rebuild *after* a post
    // finds it on the issue; one before posts it.
    for k in 1..=full.len() {
        let snapshot = run.after(k);
        let (_, mut runner) = replay(fx, snapshot).await;
        drive(fx, &mut runner).await;
        let now = fx.forge.posted();
        assert_eq!(
            now, posted,
            "prefix {k}: one comment per report, byte for byte"
        );
    }

    // Between a post and the next lead event: the lead log still ends at
    // the brief's `step_finished`, and the report is already on the issue.
    // The move that follows still happens, and the tag keeps the post from
    // happening twice.
    let snapshot = run.after_where(EventKind::StepFinished, 1);
    fx.restore(snapshot);
    // The comment is on the issue; the lead log never wrote the post down.
    fx.set_comments(run.comments()[..1].to_vec());
    let mut runner = fx.runner();
    assert_eq!(
        runner.advance().await.expect("the rebuilt runner advances"),
        Advanced::Moved,
        "the route still follows the post"
    );
    assert_eq!(
        fx.forge.posted().len(),
        1,
        "the report is not posted a second time"
    );
    drive(fx, &mut runner).await;
    assert_eq!(
        fx.forge.posted(),
        run.comments(),
        "and the comments are the ones the full run left"
    );

    // A second attempt posts its own comment: seed attempt 1's tag and
    // check attempt 2 is not suppressed.
    // A two-attempt run: the report is attempt 2's, so the tag differs
    // from the one attempt 1 would have written.
    let brief_child = Ulid::generate();
    let implementer_child = Ulid::generate();
    let fx = &Fixture::new(vec![
        (brief_child, vec![report("r1", brief_report())]),
        (
            implementer_child,
            vec![works("w1"), report("r2", implementer_report())],
        ),
    ])
    .capped(implementer_child, 1);
    let first = trace(fx).await;
    let attempt_two = first
        .comments()
        .into_iter()
        .find(|body| body.contains("attempt=2"))
        .expect("the second attempt posted its report");
    let seeded = attempt_two.replace("attempt=2", "attempt=1");
    assert_ne!(seeded, attempt_two, "the tag names the attempt");
    fx.set_comments(vec![seeded]);
    // Rebuild from just before the post: the run's last attempt posts its
    // report again, and the earlier attempt's tag must not suppress it.
    let before_the_post = first
        .lead()
        .iter()
        .rposition(|event| event.kind == EventKind::StepFinished)
        .expect("the implementer's step finished");
    let snapshot = first.after(before_the_post);
    let (_, mut runner) = replay(fx, snapshot).await;
    assert_eq!(
        drive(fx, &mut runner).await,
        Advanced::Finished {
            outcome: aigentic_log::RunOutcome::Closed
        },
        "the run closes"
    );
    let now = fx.forge.posted();
    assert_eq!(
        now.iter().filter(|body| body.contains("attempt=1")).count(),
        1,
        "the earlier attempt's comment is not touched"
    );
    assert_eq!(
        now.iter().filter(|body| body.contains("attempt=2")).count(),
        1,
        "and the second attempt posts its own rather than being suppressed"
    );
}

/// T15: run A pauses, then run B starts; with A holding it, B →
/// `write_lock`.
#[tokio::test]
async fn t15_the_second_run_over_a_held_repo_waits() {
    let a = happy_path();
    // The paused run is kept: a dropped runner drops the write lock.
    let mut first = a.fx.runner();
    // Advance the first run until its writing step starts, which is where
    // it takes the write lock. Driving it to its end would release the
    // lock, and the wait below would never be seen.
    while step_started(&a.fx.lead_events()).len() < 2 {
        assert_eq!(
            first.advance().await.expect("the first run advances"),
            Advanced::Moved,
            "the first run is still going"
        );
    }
    assert_eq!(
        WriteGuard::holder(&a.fx.repo),
        Some(a.fx.lead),
        "the running run holds the repo"
    );

    let brief_child = Ulid::generate();
    let implementer_child = Ulid::generate();
    let mut b = Fixture::new(vec![
        (brief_child, vec![report("r1", brief_report())]),
        (implementer_child, vec![report("r2", implementer_report())]),
    ]);
    b.repo = a.fx.repo.clone();
    let mut runner = b.runner();
    assert_eq!(
        drive(&b, &mut runner).await,
        Advanced::WaitingHuman {
            gate: "write_lock".to_owned()
        },
        "the second run waits for the first"
    );
    let events = b.lead_events();
    assert_eq!(
        step_started(&events).len(),
        1,
        "the writing step is not entered while another lead holds the repo"
    );
    let shown = gate_shown(&events);
    assert!(
        shown
            .iter()
            .any(|line| line.contains(&a.fx.lead.to_string())),
        "the gate names the holder: {shown:?}"
    );
}

// T10 and T11: the slot map (issue #57).

use aigentic_log::PlannedTest;
use aigentic_runtime::workflow::render::trailer_model;

/// The slot values T10's brief reports: one scalar, one multi-line value
/// that carries a template-shaped line, and the `commits` list a brief
/// reports as JSON.
fn slot_values() -> Value {
    json!({
        "size": "trivial",
        "budget": 3,
        "purpose": "make the runner work:\n- `commits`: a JSON list in this repository's style\n- keep it byte for byte",
        "must_not_undo": "nothing",
        "pointers": "crates/runtime/src/runner/mod.rs",
        "design": "a log-driven loop",
        "commits": ["runtime: one", "runtime: two"],
    })
}

/// The planned tests the T10 brief reports.
fn planned_tests() -> Vec<PlannedTest> {
    vec![
        PlannedTest {
            id: "T1".into(),
            what: "the happy path".into(),
            derivation: "from the spec".into(),
        },
        PlannedTest {
            id: "T2".into(),
            what: "the full route".into(),
            derivation: "from the spec".into(),
        },
    ]
}

/// T10: the slots a step's template is rendered with. The brief's values
/// reach the implementer's prompt verbatim, `planned_tests` and `commits`
/// render one item per line, and the runner's own slots are the
/// workflow's and the forge's values.
#[tokio::test]
async fn t10_slots_reach_the_template_verbatim() {
    let brief_child = Ulid::generate();
    let implementer_child = Ulid::generate();
    let fx = Fixture::new(vec![
        (
            brief_child,
            vec![report(
                "r1",
                json!({
                    "status": "done",
                    "body": "## Brief",
                    "slots": slot_values(),
                    "planned_tests": [
                        {"id": "T1", "what": "the happy path", "derivation": "from the spec"},
                        {"id": "T2", "what": "the full route", "derivation": "from the spec"},
                    ],
                }),
            )],
        ),
        (implementer_child, vec![report("r2", implementer_report())]),
    ]);
    let run = trace(&fx).await;
    assert_eq!(
        run.terminal,
        Advanced::Finished {
            outcome: aigentic_log::RunOutcome::Closed
        },
        "the run closes"
    );

    let prompt = prompts(&fx.child_events_of(implementer_child))
        .into_iter()
        .next()
        .expect("the implementer was prompted");
    // The brief's multi-line value, byte for byte, template line included.
    assert!(
        prompt.contains(slot_values()["purpose"].as_str().unwrap()),
        "the slot's value is verbatim"
    );
    // Typed report fields render one item per line.
    let tests = planned_tests();
    assert!(
        prompt.contains(
            &tests
                .iter()
                .map(|test| format!("{} — {} — {}", test.id, test.what, test.derivation))
                .collect::<Vec<_>>()
                .join("\n")
        )
    );
    assert!(prompt.contains("runtime: one\nruntime: two"));
    // The runner's own slots: keyed by what the workflow declares as the
    // runner's, valued from the run, the host and the workflow.
    let step = fx
        .workflow
        .workflow
        .steps
        .iter()
        .find(|step| step.id == "implement-alone")
        .expect("the workflow has the routed step")
        .clone();
    let slots = fx.runner().runner_slots(&step).unwrap();
    let declared: Vec<&str> = fx
        .workflow
        .workflow
        .slots
        .iter()
        .filter(|slot| slot.filled_by == "runner")
        .map(|slot| slot.name.as_str())
        .collect();
    for name in slots.keys() {
        assert!(
            declared.contains(&name.as_str()),
            "{name:?} is a slot the workflow says the runner fills"
        );
    }
    let budget = &fx.workflow.workflow.budget;
    let host = FakeHost::new(
        fx.dir.path().to_path_buf(),
        fx.lead,
        &[],
        BTreeMap::new(),
        None,
    );
    for (name, expected) in [
        ("issue", fx.issue().to_string()),
        (
            "title",
            Forge::issue(&fx.forge, fx.issue()).unwrap().title.clone(),
        ),
        (
            "model",
            trailer_model(&host.model_of(&step.profile).unwrap()).to_owned(),
        ),
        (
            "gate_log",
            std::env::temp_dir()
                .join(format!("aigentic-gate-{}.log", fx.issue()))
                .display()
                .to_string(),
        ),
        ("budget_trivial", budget.trivial.to_string()),
        ("budget_full", budget.full.to_string()),
        ("max_raise", budget.max_raise.to_string()),
    ] {
        assert_eq!(
            slots.get(name).and_then(Value::as_str),
            Some(expected.as_str()),
            "the runner's slot {name:?}"
        );
    }

    // Every slot the implementer's template names is in the prompt as the
    // map renders it, so the two sides cannot drift.
    let template = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../workflows/build/templates/implementer.md"),
    )
    .unwrap();
    for name in slot_names(&template) {
        if let Some(Value::String(value)) = slots.get(&name) {
            assert!(
                prompt.contains(value.as_str()),
                "the prompt carries {name:?} as the map renders it"
            );
        }
    }
}

/// The slot names a template names, sections included: `{{x}}`, `{{#x}}`
/// and `{{/x}}` all name `x`.
fn slot_names(template: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        let Some(end) = rest[start..].find("}}") else {
            break;
        };
        let name = rest[start + 2..start + end].trim_start_matches(['#', '/', '&', '^']);
        if !name.is_empty() && !names.iter().any(|seen| seen == name) {
            names.push(name.to_string());
        }
        rest = &rest[start + end + 2..];
    }
    names
}

/// T11: a template's slot the map cannot fill. The route still happens,
/// the gate is `render_failed`, and no child of the unfilled step exists.
#[tokio::test]
async fn t11_a_missing_slot_escalates_before_any_child_starts() {
    let brief_child = Ulid::generate();
    let implementer_child = Ulid::generate();
    let fx = Fixture::new(vec![
        (
            brief_child,
            vec![report(
                "r1",
                json!({
                    "status": "done",
                    "body": "## Brief",
                    // `size` and `budget` route; every slot the
                    // implementer's template wants is missing.
                    "slots": {"size": "trivial", "budget": 3},
                }),
            )],
        ),
        (implementer_child, vec![report("r2", implementer_report())]),
    ]);
    let run = trace(&fx).await;
    match run.terminal {
        Advanced::WaitingHuman { gate, .. } => assert_eq!(gate, "render_failed"),
        other => panic!("a render failure waits for a human, got {other:?}"),
    }
    let events = fx.lead_events();
    assert!(
        !events.iter().any(|event| {
            event.kind == EventKind::StepStarted
                && serde_json::from_value::<aigentic_log::StepStartedPayload>(event.payload.clone())
                    .is_ok_and(|payload| payload.step == "implement-alone")
        }),
        "no step_started for a step whose template cannot render"
    );
    assert!(
        !fx.child_exists(implementer_child),
        "and no child exists for it"
    );
    // The gate names the step and the render error, and nothing reached a
    // child.
    let shown = gate_shown(&events);
    assert_eq!(shown.first().map(String::as_str), Some("implement-alone"));
    assert!(
        shown.get(1).is_some_and(|error| !error.is_empty()),
        "the gate names the render error"
    );
    let posted: usize = fx
        .child_events()
        .values()
        .map(|events| prompts(events).len())
        .sum();
    assert_eq!(posted, 1, "only the brief's own prompt was ever posted");
}

// ---------------------------------------------------------------------------
// The forge
// ---------------------------------------------------------------------------

/// `PATH` is process-wide and `GhForge` finds `gh` on it, so the test that
/// puts a fake `gh` there holds this lock while it does.
static PATH_LOCK: Mutex<()> = Mutex::new(());

/// A `gh` that only knows the calls T12 makes, and records a comment body
/// next to itself instead of posting it.
const FAKE_GH: &str = r#"#!/bin/sh
here=$(dirname "$0")
case "$1 $2" in
  "issue view")
    case "$*" in
      *comments*) printf '%s' '{"comments":[{"body":"first"},{"body":"second"}]}' ;;
      # `close` asks for the state after a failed close: the file the
      # `issue close` row below leaves behind says which answer to give.
      *state*)
        if [ -f "$here/closed" ]; then
          printf '%s' '{"state":"CLOSED"}'
        else
          printf '%s' '{"state":"OPEN"}'
        fi
        ;;
      *) printf '%s' '{"title":"A title","body":"A body"}' ;;
    esac
    ;;
  "issue comment")
    while [ $# -gt 0 ]; do
      if [ "$1" = "--body-file" ]; then shift; cp "$1" "$here/body.txt"; fi
      shift
    done
    ;;
  "issue close")
    # `gh` refuses a close when the issue is already closed. A file the
    # test drops decides whether this attempt is the one that fails.
    if [ -f "$here/close_fails" ]; then
      rm -f "$here/close_fails"
      if [ -f "$here/already_closed" ]; then touch "$here/closed"; fi
      echo "gh: issue is already closed" >&2
      exit 1
    fi
    touch "$here/closed"
    ;;
  *) echo "fake gh: unexpected args: $*" >&2; exit 1 ;;
esac
"#;

fn write_fake_gh(dir: &std::path::Path) {
    let path = dir.join("gh");
    std::fs::write(&path, FAKE_GH).unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    std::fs::set_permissions(&path, perms).unwrap();
}

/// A `PATH` that finds `dir`'s fake `gh` first, restored when the guard
/// drops. One test reads or writes `PATH` at a time.
struct FakeGh {
    dir: tempfile::TempDir,
    old: Option<std::ffi::OsString>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl FakeGh {
    fn new() -> Self {
        let lock = PATH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        write_fake_gh(dir.path());
        let old = std::env::var_os("PATH");
        let mut path = dir.path().as_os_str().to_os_string();
        if let Some(old) = &old {
            path.push(":");
            path.push(old);
        }
        // SAFETY: every test that reads or writes `PATH` holds `PATH_LOCK`.
        unsafe { std::env::set_var("PATH", &path) };
        Self {
            dir,
            old,
            _lock: lock,
        }
    }

    /// Leave a file the script reads, e.g. `close_fails`.
    fn touch(&self, name: &str) {
        std::fs::write(self.dir.path().join(name), "").unwrap();
    }

    /// A file the script wrote, if it did.
    fn read(&self, name: &str) -> Option<String> {
        std::fs::read_to_string(self.dir.path().join(name)).ok()
    }
}

impl Drop for FakeGh {
    fn drop(&mut self) {
        match &self.old {
            // SAFETY: as above.
            Some(previous) => unsafe { std::env::set_var("PATH", previous) },
            None => unsafe { std::env::remove_var("PATH") },
        }
    }
}

/// T13: `GhForge::close` asks `gh` to close the issue, and an issue that
/// is already closed is success — a rebuilt runner finishing what a crash
/// left open. The fake `gh` refuses the close and then reports the state,
/// which is the pair of calls the real one makes.
#[test]
fn t13_gh_forge_closes_an_issue_and_accepts_already_closed() {
    let gh = FakeGh::new();
    let forge = GhForge::new();

    // Already closed: `close` fails, the state is CLOSED, so it is done.
    gh.touch("close_fails");
    gh.touch("already_closed");
    assert!(
        forge.close(65).is_ok(),
        "an issue that is already closed is success"
    );
    assert!(
        gh.read("closed").is_some(),
        "the state the fake reported was CLOSED"
    );

    // Open: `close` fails and the state is OPEN, which is a real failure
    // and carries the message `gh` gave.
    drop(gh);
    let gh = FakeGh::new();
    gh.touch("close_fails");
    let err = GhForge::new()
        .close(65)
        .expect_err("an open issue that refuses to close is an error");
    assert!(
        format!("{err}").contains("already closed"),
        "the error carries gh's own message: {err}"
    );
    assert!(gh.read("closed").is_none(), "nothing was closed");

    // The plain path: `gh issue close` succeeds on the first call.
    drop(gh);
    let gh = FakeGh::new();
    assert!(GhForge::new().close(65).is_ok(), "a clean close");
    assert!(gh.read("closed").is_some(), "the fake recorded the close");
}

/// T12: `GhForge` reads the issue's title and body and posts a comment as a
/// body *file*. The fake `gh` on `PATH` pins the call shape the real one
/// gets; the body carries a quote, a backtick and a newline, which is what
/// a body on `argv` would mangle.
#[test]
fn t12_gh_forge_reads_the_issue_and_posts_a_comment() {
    let _guard = PATH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    write_fake_gh(dir.path());
    let old = std::env::var_os("PATH");
    let mut path = dir.path().as_os_str().to_os_string();
    if let Some(old) = &old {
        path.push(":");
        path.push(old);
    }
    // SAFETY: every test that reads or writes `PATH` holds `PATH_LOCK`.
    unsafe { std::env::set_var("PATH", &path) };

    let forge = GhForge::new();
    let issue = forge.issue(57).expect("a readable issue");
    assert_eq!(issue.title, "A title");
    assert_eq!(issue.body, "A body");
    assert_eq!(
        forge.comments(57).expect("readable comments"),
        vec!["first".to_string(), "second".to_string()],
        "every comment's body, oldest first"
    );

    let body = "## Implementation\n\nA `quote` and a \"mark\", on one line.";
    forge.comment(57, body).expect("a posted comment");
    let recorded = std::fs::read_to_string(dir.path().join("body.txt")).unwrap();
    assert_eq!(recorded, body, "the body arrived byte for byte");

    match old {
        // SAFETY: as above.
        Some(previous) => unsafe { std::env::set_var("PATH", previous) },
        None => unsafe { std::env::remove_var("PATH") },
    }
}

// ---------------------------------------------------------------------------
// Issue #65: the checks, the send-back, the push, the install, the close
// ---------------------------------------------------------------------------

/// The happy path with a second scripted reply for the implementer, so a
/// send-back's attempt 2 has a report to post.
fn retryable() -> Happy {
    let brief_child = Ulid::generate();
    let implementer_child = Ulid::generate();
    Happy {
        fx: Fixture::new(vec![
            (brief_child, vec![report("r1", brief_report())]),
            (
                implementer_child,
                vec![
                    report("r2", implementer_report()),
                    report("r3", implementer_report()),
                ],
            ),
        ]),
        brief_child,
        implementer_child,
    }
}

/// The happy path with the installer a test wants.
fn happy_path_installed(installer: FakeInstaller) -> Happy {
    let brief_child = Ulid::generate();
    let implementer_child = Ulid::generate();
    Happy {
        fx: Fixture::new(vec![
            (brief_child, vec![report("r1", brief_report())]),
            (implementer_child, vec![report("r2", implementer_report())]),
        ])
        .with_installer(installer),
        brief_child,
        implementer_child,
    }
}

/// The prompts a child was given, in order.
fn child_prompts(fx: &Fixture, id: Ulid) -> Vec<String> {
    prompts(&fx.child_events_of(id))
}

/// Whether a `checks_run` for `step` holds a `fail`.
fn checks_failed(events: &[Event], step: &str) -> bool {
    checks_run(events).iter().any(|run| {
        run.step == step
            && run
                .checks
                .iter()
                .any(|check| check.result == aigentic_log::CheckResult::Fail)
    })
}

/// Drive a run whose implementer commits a subject the brief never named
/// before the first check reads the repository, and — when `fix` is set —
/// amends it to the named one as soon as a check has failed.
async fn drive_a_wrong_subject(fx: &Fixture, implementer: Ulid, fix: bool) -> Trace {
    let mut runner = fx.runner();
    let mut snapshots = vec![snapshot(fx)];
    let mut subject = WrongSubject::new(implementer);
    let terminal = loop {
        match runner.advance().await.expect("the run advances") {
            Advanced::Moved => {
                subject.between(fx, fix);
                snapshots.push(snapshot(fx));
            }
            other => break other,
        }
    };
    snapshots.push(snapshot(fx));
    Trace {
        snapshots,
        terminal,
    }
}

/// T2: a failing check sends the step back once; the fixed attempt passes,
/// pushes, installs and closes the issue.
#[tokio::test]
async fn t2_a_failed_check_sends_the_step_back_and_a_pass_pushes() {
    let h = retryable();
    let fx = &h.fx;
    let run = drive_a_wrong_subject(fx, h.implementer_child, true).await;

    assert_eq!(
        run.terminal,
        Advanced::Finished {
            outcome: aigentic_log::RunOutcome::Closed
        },
        "the send-back's attempt passes and the run closes"
    );
    let events = run.lead();
    assert_eq!(gate(events), None, "no gate is asked");

    // The step was started twice, on the same child.
    let started: Vec<_> = step_started(events)
        .into_iter()
        .filter(|started| started.step == "implement-alone")
        .collect();
    assert_eq!(started.len(), 2, "one start per attempt");
    assert_eq!(started[0].attempt, 1);
    assert_eq!(started[1].attempt, 2);
    assert_eq!(
        started[1].child_thread, h.implementer_child,
        "the send-back continues in the step's own child"
    );

    // The send-back reads as a first attempt with the failures in it.
    let prompts = child_prompts(fx, h.implementer_child);
    assert_eq!(prompts.len(), 2, "one prompt per attempt");
    assert!(
        prompts[1].contains("Sent back by the runner"),
        "the second prompt is the send-back: {}",
        prompts[1]
    );
    assert!(
        prompts[1].contains("- E2:"),
        "and it names the failing check: {}",
        prompts[1]
    );
    assert!(
        prompts[1].contains(&issue_view().title),
        "the send-back is the same brief, not a bare message"
    );

    // Two check runs: the first fails, the amender's passes.
    let checks = checks_run(events);
    assert_eq!(checks.len(), 2, "one run per attempt");
    assert!(
        checks_failed(events, "implement-alone"),
        "the first run failed on the wrong subject"
    );
    assert!(
        checks[1]
            .checks
            .iter()
            .all(|check| check.result == aigentic_log::CheckResult::Pass),
        "the amended commits pass: {:?}",
        checks[1].checks
    );

    // The push carried the amended commit, and the run closed.
    let head = fx.head();
    let pushed = pushed_events(events);
    assert_eq!(pushed.len(), 1, "one push, after the checks passed");
    assert_eq!(pushed[0].ref_after, head);
    assert_eq!(
        pushed[0]
            .commits
            .iter()
            .map(|commit| commit.subject.as_str())
            .collect::<Vec<_>>(),
        vec!["runtime: two", "runtime: one"],
        "the amended subject, newest first"
    );
    assert_eq!(fx.remote_head(), head, "the remote has the amended head");
    assert!(fx.forge.is_closed(), "the issue is closed");
    assert_eq!(
        run_finished(events).outcome,
        aigentic_log::RunOutcome::Closed
    );
}

/// T3: a second failing run of the checks escalates instead of pushing.
#[tokio::test]
async fn t3_a_second_failed_check_escalates_without_pushing() {
    let h = retryable();
    let fx = &h.fx;
    let run = drive_a_wrong_subject(fx, h.implementer_child, false).await;

    assert_eq!(
        run.terminal,
        Advanced::WaitingHuman {
            gate: "checks_failed".to_owned()
        },
        "twice failed is the human's problem"
    );
    let events = run.lead();
    let checks = checks_run(events);
    assert_eq!(checks.len(), 2, "one failing run per attempt");
    assert!(
        checks.iter().all(|run| run
            .checks
            .iter()
            .any(|c| c.result == aigentic_log::CheckResult::Fail)),
        "both runs failed"
    );
    assert_eq!(
        step_started(events)
            .into_iter()
            .filter(|started| started.step == "implement-alone")
            .count(),
        2,
        "a second failure starts no third attempt"
    );
    assert!(
        pushed_events(events).is_empty(),
        "nothing is pushed after a second failure"
    );
    assert_eq!(
        fx.remote_head(),
        fx.initial_head(),
        "the remote never moved"
    );
    assert_eq!(fx.installer.calls(), 0, "and nothing is installed");
    assert!(!fx.forge.is_closed());
    let shown = gate_shown(events);
    assert!(
        shown.iter().any(|line| line.contains("E2")),
        "the gate names the failing check: {shown:?}"
    );
}

/// T4: a remote that moved while the step ran is never pushed over.
#[tokio::test]
async fn t4_a_remote_that_moved_is_never_pushed_over() {
    let h = happy_path();
    let fx = &h.fx;
    let mut runner = fx.runner();
    let mut committed = false;
    let mut stranger = None;
    let advanced = loop {
        let advanced = runner.advance().await.expect("the run advances");
        let reported = fx
            .child_events_of(h.implementer_child)
            .iter()
            .any(|event| event.kind == EventKind::StepReported);
        if reported && !committed && fx.head() == fx.initial_head() {
            commit_what_the_brief_named(fx, h.implementer_child, &mut committed);
        }
        if committed && stranger.is_none() {
            // Someone else pushes while the step is still running.
            stranger = Some(fx.push_a_stranger());
        }
        if !matches!(advanced, Advanced::Moved) {
            break advanced;
        }
    };

    assert_eq!(
        advanced,
        Advanced::WaitingHuman {
            gate: "remote_moved".to_owned()
        },
        "the moved remote stops the run"
    );
    let events = fx.lead_events();
    assert!(
        pushed_events(&events).is_empty(),
        "the step's commits are not pushed over the stranger's"
    );
    assert_eq!(
        fx.remote_head(),
        stranger.unwrap(),
        "the remote holds the stranger's commit, not the step's"
    );
    let shown = gate_shown(&events);
    assert!(
        shown
            .iter()
            .any(|line| line.contains("remote moved during the step")),
        "the gate says what happened: {shown:?}"
    );
    assert_eq!(fx.installer.calls(), 0, "nothing is installed");
}

/// T5: the push landed but the process died before `pushed`; a rebuild
/// pushes nothing again and records the push once.
#[tokio::test]
async fn t5_a_crash_between_push_and_pushed_does_not_push_twice() {
    let h = happy_path();
    let fx = &h.fx;
    let run = trace(fx).await;
    let head = fx.head();

    // Back to the moment the checks passed, then the push happens and the
    // process dies with `pushed` unwritten.
    let snapshot = run.after_where(EventKind::ChecksRun, 1);
    fx.restore(snapshot);
    fx.rewind_forge(snapshot);
    fx.git(&["push", "origin", "main"]);
    assert_eq!(fx.remote_head(), head, "the push landed before the crash");
    assert!(
        pushed_events(&fx.lead_events()).is_empty(),
        "and the lead log never heard of it"
    );

    let mut runner = fx.runner();
    let advanced = drive(fx, &mut runner).await;
    assert_eq!(
        advanced,
        Advanced::Finished {
            outcome: aigentic_log::RunOutcome::Closed
        },
        "the rebuilt run closes"
    );
    let events = fx.lead_events();
    let pushed = pushed_events(&events);
    assert_eq!(pushed.len(), 1, "the push is recorded once");
    assert_eq!(pushed[0].ref_after, head);
    assert_eq!(
        pushed[0].ref_before,
        fx.initial_head(),
        "from the base the step started at"
    );
    assert_eq!(
        fx.remote_head(),
        head,
        "the remote is where the step left it"
    );
    // The rebuild installed again: the crash left no `pushed` event, and
    // an install of the same commit is a no-op that `--force` repeats
    // safely. Only the push is guarded, because only a push can be lost.
    assert!(fx.installer.calls() >= 1, "the step's commit was installed");
    assert_eq!(run_finished_all(&events).len(), 1, "and one run_finished");
}

/// T6a: the push is recorded but the process died before the close; a
/// rebuild posts the closing comment once and finishes once.
#[tokio::test]
async fn t6a_a_crash_after_pushed_posts_the_closing_comment_once() {
    let h = happy_path();
    let fx = &h.fx;
    let run = trace(fx).await;

    let snapshot = run.after_where(EventKind::Pushed, 1);
    assert_eq!(
        snapshot.comments.len(),
        2,
        "the two reports, not yet the closing one"
    );
    fx.restore(snapshot);
    fx.rewind_forge(snapshot);

    let mut runner = fx.runner();
    let advanced = drive(fx, &mut runner).await;
    assert_eq!(
        advanced,
        Advanced::Finished {
            outcome: aigentic_log::RunOutcome::Closed
        }
    );
    let posted = fx.forge.posted();
    assert_eq!(posted.len(), 3, "one closing comment, not two");
    assert_eq!(
        posted
            .iter()
            .filter(|comment| comment.contains("## Closing"))
            .count(),
        1
    );
    assert!(fx.forge.is_closed());
    assert_eq!(
        run_finished_all(&fx.lead_events()).len(),
        1,
        "run_finished once"
    );
}

/// T6b: a close that fails once is retried by the rebuild, which posts no
/// second closing comment.
#[tokio::test]
async fn t6b_a_close_that_fails_is_retried_without_a_second_comment() {
    let h = happy_path();
    let fx = &h.fx;
    let run = trace(fx).await;
    let snapshot = run.after_where(EventKind::Pushed, 1);
    fx.restore(snapshot);
    fx.rewind_forge(snapshot);

    fx.forge.fail_close(1);
    let mut runner = fx.runner();
    let err = runner
        .advance()
        .await
        .expect_err("the forge is down for a moment");
    assert!(
        matches!(err, RunnerError::Forge(_)),
        "the close failure is a forge error: {err}"
    );
    assert!(
        fx.forge
            .posted()
            .iter()
            .any(|comment| comment.contains("## Closing")),
        "the closing comment landed before the close failed"
    );
    assert!(!fx.forge.is_closed());

    // The rebuild finds the comment already posted and closes.
    let mut runner = fx.runner();
    let advanced = drive(fx, &mut runner).await;
    assert_eq!(
        advanced,
        Advanced::Finished {
            outcome: aigentic_log::RunOutcome::Closed
        }
    );
    let posted = fx.forge.posted();
    assert_eq!(posted.len(), 3, "the rebuild posts nothing again");
    assert_eq!(
        posted
            .iter()
            .filter(|comment| comment.contains("## Closing"))
            .count(),
        1
    );
    assert!(fx.forge.is_closed());
    assert_eq!(run_finished_all(&fx.lead_events()).len(), 1);
}

/// T6c: the closing comment and the close both landed, and the process
/// died before `run_finished`; the rebuild repeats neither.
#[tokio::test]
async fn t6c_a_closing_comment_already_posted_is_not_posted_again() {
    let h = happy_path();
    let fx = &h.fx;
    let run = trace(fx).await;
    let snapshot = run.after_where(EventKind::Pushed, 1);
    fx.restore(snapshot);

    // The forge holds the closing comment and the closed issue; the lead
    // log's last event is still `pushed`.
    let tag = format!("<!-- aigentic run={} step=close -->", fx.lead);
    let mut comments = snapshot.comments.clone();
    comments.push(format!(
        "## Closing\n\nall landed\n\nRelease impact: patch\n\n{tag}"
    ));
    fx.set_comments(comments);
    fx.forge.set_closed(true);

    let mut runner = fx.runner();
    let advanced = drive(fx, &mut runner).await;
    assert_eq!(
        advanced,
        Advanced::Finished {
            outcome: aigentic_log::RunOutcome::Closed
        },
        "an already closed issue is a success"
    );
    let posted = fx.forge.posted();
    assert_eq!(posted.len(), 3, "no second closing comment");
    assert_eq!(
        posted
            .iter()
            .filter(|comment| comment.contains("## Closing"))
            .count(),
        1
    );
    let events = fx.lead_events();
    assert_eq!(run_finished_all(&events).len(), 1, "run_finished once");
    assert_eq!(
        run_finished(&events).outcome,
        aigentic_log::RunOutcome::Closed
    );
}

/// T7: a binary that does not name the pushed commit stops the run.
#[tokio::test]
async fn t7_an_install_that_does_not_name_the_head_escalates() {
    let h = happy_path_installed(FakeInstaller::new("aigentic 0.0.0 (000000000000)"));
    let fx = &h.fx;
    let run = trace(fx).await;

    assert_eq!(
        run.terminal,
        Advanced::WaitingHuman {
            gate: "install_mismatch".to_owned()
        },
        "an install of something else is not the step's head"
    );
    let events = run.lead();
    let head = fx.head();
    assert_eq!(
        fx.remote_head(),
        head,
        "the push had already landed when the install was judged"
    );
    assert!(
        pushed_events(events).is_empty(),
        "and `pushed` was not written, so a rebuild retries from the push"
    );
    assert_eq!(fx.installer.calls(), 1, "the install was asked once");
    let shown = gate_shown(events);
    assert!(
        shown.iter().any(|line| line.contains("000000000000")),
        "the gate says what the binary answered: {shown:?}"
    );
    assert!(
        shown.iter().any(|line| line.contains(&head)),
        "and which commit it should have named: {shown:?}"
    );
}

/// T8: a gate answered `stop` finishes the run and gives the write lock
/// back.
#[tokio::test]
async fn t8_a_stop_answer_finishes_the_run_and_releases_the_lock() {
    let brief_child = Ulid::generate();
    let mut full = brief_report();
    full["slots"]["size"] = json!("full");
    full["slots"]["budget"] = json!(4);
    let fx = Fixture::new(vec![(brief_child, vec![report("r1", full)])]);

    // The `full` route asks the human before it starts a step.
    let mut runner = fx.runner();
    let paused = loop {
        match runner.advance().await.expect("the run advances") {
            Advanced::Moved => {}
            other => break other,
        }
    };
    assert_eq!(
        paused,
        Advanced::WaitingHuman {
            gate: "route".to_owned()
        }
    );

    // The human says stop.
    let mut log = ThreadLog::open(fx.dir.path(), fx.lead).unwrap();
    log.append(aigentic_log::NewEvent {
        kind: EventKind::CheckpointAnswered,
        author: Author::User(UserId("steve".into())),
        payload: json!({"answer": "stop", "marks": []}),
        parent_event: None,
    })
    .unwrap();

    let mut runner = fx.runner();
    let advanced = runner.advance().await.expect("the answer is read");
    assert_eq!(
        advanced,
        Advanced::Finished {
            outcome: aigentic_log::RunOutcome::Stopped
        },
        "a stop ends the run"
    );
    let events = fx.lead_events();
    let finished = run_finished(&events);
    assert_eq!(finished.outcome, aigentic_log::RunOutcome::Stopped);
    assert_eq!(run_finished_all(&events).len(), 1, "run_finished once");
    // The lock is free: nobody holds it any more.
    assert_eq!(
        WriteGuard::holder(&fx.repo),
        None,
        "the write lock was released"
    );
}

/// T9: T2's run, rebuilt after every one of its lead events, does each
/// move once — the send-back and the amend included.
#[tokio::test]
async fn t9_a_rebuild_around_every_lead_event_does_each_move_once() {
    let h = retryable();
    let fx = &h.fx;
    let run = drive_a_wrong_subject(fx, h.implementer_child, true).await;
    assert_eq!(
        run.terminal,
        Advanced::Finished {
            outcome: aigentic_log::RunOutcome::Closed
        },
        "the send-back's run closes"
    );
    sweep_every_prefix(fx, &run, h.implementer_child, || true).await;
}

/// T14: a check id the workflow does not have stops the run before
/// anything is pushed.
#[tokio::test]
async fn t14_an_unknown_check_id_escalates_without_pushing() {
    let brief_child = Ulid::generate();
    let implementer_child = Ulid::generate();
    let fx = Fixture::new(vec![
        (brief_child, vec![report("r1", brief_report())]),
        (implementer_child, vec![report("r2", implementer_report())]),
    ])
    .with_checks(&["E9"]);

    let mut runner = fx.runner();
    let advanced = drive(&fx, &mut runner).await;
    assert_eq!(
        advanced,
        Advanced::WaitingHuman {
            gate: "checks_error".to_owned()
        },
        "an unknown check is an error, not a pass"
    );
    let events = fx.lead_events();
    assert!(
        checks_run(&events).is_empty(),
        "no checks_run is written for a run that could not check"
    );
    assert!(pushed_events(&events).is_empty(), "nothing is pushed");
    assert_eq!(fx.remote_head(), fx.initial_head());
    assert_eq!(fx.installer.calls(), 0);
    let shown = gate_shown(&events);
    assert!(
        shown.iter().any(|line| line.contains("implement-alone")),
        "the gate names the step: {shown:?}"
    );
}

/// T15: a `step_started` written before the start fields existed leaves
/// the run without a base, so it stops rather than guessing one.
#[tokio::test]
async fn t15_a_step_started_without_the_start_fields_escalates() {
    let h = happy_path();
    let fx = &h.fx;
    let run = trace(fx).await;

    // Back to the implementer's `step_started`, with the start fields
    // taken out of it: a log from before #65 reads like this.
    let snapshot = run.after_where(EventKind::StepStarted, 2);
    fx.restore(snapshot);
    let mut events = fx.lead_events();
    for event in &mut events {
        if event.kind == EventKind::StepStarted {
            let payload = event.payload.as_object_mut().expect("a payload object");
            payload.remove("head_at_start");
            payload.remove("remote_at_start");
        }
    }
    write_events(&fx.dir.path().join(format!("{}.jsonl", fx.lead)), &events);

    let mut runner = fx.runner();
    let advanced = drive(fx, &mut runner).await;
    assert_eq!(
        advanced,
        Advanced::WaitingHuman {
            gate: "no_start_record".to_owned()
        },
        "without a base the run cannot judge the step's commits"
    );
    let events = fx.lead_events();
    let shown = gate_shown(&events);
    assert!(
        shown.iter().any(|line| line == "implement-alone"),
        "the gate names the step: {shown:?}"
    );
    assert!(
        shown.iter().any(|line| line == "head_at_start"),
        "and the field it is missing: {shown:?}"
    );
    assert!(
        checks_run(&events).is_empty(),
        "no check ran against a guessed base"
    );
    assert!(pushed_events(&events).is_empty());
    assert_eq!(fx.remote_head(), fx.initial_head());
}

/// T16: a writing step that does not push stops after its checks.
#[tokio::test]
async fn t16_a_step_that_does_not_push_escalates() {
    let brief_child = Ulid::generate();
    let implementer_child = Ulid::generate();
    let fx = Fixture::new(vec![
        (brief_child, vec![report("r1", brief_report())]),
        (implementer_child, vec![report("r2", implementer_report())]),
    ])
    .without_push();

    let mut runner = fx.runner();
    let advanced = drive(&fx, &mut runner).await;
    assert_eq!(
        advanced,
        Advanced::WaitingHuman {
            gate: "no_push".to_owned()
        },
        "a writing step that pushes nothing is the human's to judge"
    );
    let events = fx.lead_events();
    assert_eq!(
        checks_run(&events).len(),
        1,
        "the checks still ran before the push was due"
    );
    assert!(pushed_events(&events).is_empty());
    assert_eq!(fx.remote_head(), fx.initial_head());
    assert_eq!(fx.installer.calls(), 0, "and nothing is installed");
}

/// T17: a routed step's checks do not swallow its route.
#[tokio::test]
async fn t17_a_routed_step_with_checks_still_takes_its_route() {
    let brief_child = Ulid::generate();
    let implementer_child = Ulid::generate();
    // The brief decides the route; give it a check id that would stop the
    // run if a routed step ever ran the checks it was given.
    let fx = &Fixture::new(vec![
        (brief_child, vec![report("r1", brief_report())]),
        (implementer_child, vec![report("r2", implementer_report())]),
    ])
    .with_checks_for("brief", &["E9"]);
    let run = trace(fx).await;

    assert_eq!(
        run.terminal,
        Advanced::Finished {
            outcome: aigentic_log::RunOutcome::Closed
        },
        "the routed run still closes"
    );
    let events = run.lead();
    let taken = route_taken(events).expect("the route is still written down");
    assert_eq!(taken.proposed, "trivial");
    let checks = checks_run(events);
    assert_eq!(checks.len(), 1, "one checks_run for the run");
    assert_eq!(
        checks[0].step, "implement-alone",
        "a routed step runs no checks of its own"
    );
}

/// T18: a `budget_warned` between the failing checks and the send-back
/// does not hide the failure from attempt 2's prompt.
#[tokio::test]
async fn t18_a_budget_warning_between_the_failure_and_the_send_back_keeps_it() {
    let h = retryable();
    let fx = &h.fx;
    let mut runner = fx.runner();
    let mut committed = false;
    loop {
        let advanced = runner.advance().await.expect("the run advances");
        let reported = fx
            .child_events_of(h.implementer_child)
            .iter()
            .any(|event| event.kind == EventKind::StepReported);
        if reported && !committed && fx.head() == fx.initial_head() {
            fx.commit_named("runtime: one");
            fx.commit_named("runtime: wrong");
            committed = true;
        }
        if checks_failed(&fx.lead_events(), "implement-alone") {
            break;
        }
        assert!(
            matches!(advanced, Advanced::Moved),
            "the run reaches its failing checks"
        );
    }

    // A budget warning lands between the failure and the send-back.
    let mut log = ThreadLog::open(fx.dir.path(), fx.lead).unwrap();
    log.append(aigentic_log::NewEvent {
        kind: EventKind::BudgetWarned,
        author: Author::Agent(AgentId("runner".into())),
        payload: serde_json::to_value(aigentic_log::BudgetWarnedPayload {
            scope: aigentic_log::BudgetScope::Issue,
            spent_usd: 2.6,
            limit_usd: 3.0,
        })
        .unwrap(),
        parent_event: None,
    })
    .unwrap();

    // The rebuilt runner still sends the step back with the failure in it.
    let mut runner = fx.runner();
    assert_eq!(
        runner.advance().await.expect("the run advances"),
        Advanced::Moved,
        "the send-back starts attempt 2"
    );
    let prompts = child_prompts(fx, h.implementer_child);
    assert_eq!(prompts.len(), 2, "one prompt per attempt");
    assert!(
        prompts[1].contains("- E2:"),
        "the warning between the two moves changed nothing: {}",
        prompts[1]
    );
}

// ---------------------------------------------------------------------------
// A workflow longer than brief → implement: steps that follow `next`, a
// person's checkpoint, a push that isn't the last step, and CI.
// ---------------------------------------------------------------------------

const LOOP_TOML: &str = r###"
name = "loop-test"
version = 1

[budget]
trivial = 3.0
full = 10.0
max_raise = 2.0

[[slots]]
name = "issue"
kind = "string"
filled_by = "runner"

[[slots]]
name = "amendment"
kind = "string"
required = false
filled_by = "runner"

[[steps]]
id = "check"
role = "checker"
profile = "flash"
template = "templates/check.md"
marker = "## Check"
next = "decide"

[[steps]]
id = "decide"
role = "person"
profile = "flash"
template = "templates/decide.md"
marker = "## Decide"
checkpoint = true
next = "implement"

[[steps]]
id = "implement"
role = "implementer"
profile = "flash"
template = "templates/implement.md"
marker = "## Implementation"
writes = true
push = true
ci = true
install = false
next = "judge"

[[steps]]
id = "judge"
role = "judge"
profile = "flash"
template = "templates/judge.md"
marker = "## Review"
route_by = "verdict"
routes = { approve = "done", changes = "ask" }
"###;

fn loop_files() -> Vec<(&'static str, &'static str)> {
    vec![
        ("workflow.toml", LOOP_TOML),
        ("templates/check.md", "check issue {{issue}}\n"),
        (
            "templates/decide.md",
            "read the check on #{{issue}}, then decide\n",
        ),
        (
            "templates/implement.md",
            "implement issue {{issue}}\n{{#amendment}}the person amended: {{amendment}}\n{{/amendment}}",
        ),
        ("templates/judge.md", "judge issue {{issue}}\n"),
    ]
}

/// A report with a body under `marker` and the given slots.
fn reported(marker: &str, slots: Value) -> Value {
    json!({
        "status": "done",
        "body": format!("{marker}\n\nwhat this step found"),
        "slots": slots,
        "release_impact": "patch",
    })
}

struct Looped {
    fx: Fixture,
    check: Ulid,
    implement: Ulid,
    judge: Ulid,
}

fn looped(verdict: &str) -> Looped {
    let check = Ulid::generate();
    let implement = Ulid::generate();
    let judge = Ulid::generate();
    let fx = Fixture::with_workflow(
        vec![
            (check, vec![report("c1", reported("## Check", json!({})))]),
            (
                implement,
                vec![report("i1", reported("## Implementation", json!({})))],
            ),
            (
                judge,
                vec![report(
                    "j1",
                    reported("## Review", json!({ "verdict": verdict })),
                )],
            ),
        ],
        &loop_files(),
    );
    Looped {
        fx,
        check,
        implement,
        judge,
    }
}

fn steve() -> Author {
    Author::User(UserId("steve".into()))
}

/// The routes a run took, in order.
fn routes(events: &[Event]) -> Vec<aigentic_log::RouteTakenPayload> {
    events
        .iter()
        .filter(|event| event.kind == EventKind::RouteTaken)
        .map(|event| serde_json::from_value(event.payload.clone()).unwrap())
        .collect()
}

/// A step with no route follows its `next`; a checkpoint step opens a
/// gate that offers go, amend and stop and shows its rendered template;
/// `amend` continues, and its text reaches the later step's prompt.
#[tokio::test]
async fn a_step_follows_next_and_a_checkpoint_continues_with_the_amendment() {
    let l = looped("approve");
    let fx = &l.fx;
    let mut runner = fx.runner();

    let paused = drive(fx, &mut runner).await;
    assert_eq!(
        paused,
        Advanced::WaitingHuman {
            gate: "decide".into()
        },
        "the check's `next` led to the checkpoint"
    );
    let events = fx.lead_events();
    let taken = routes(&events);
    assert_eq!(taken.len(), 1, "one route: check → decide");
    assert_eq!(
        (taken[0].branch.as_str(), taken[0].taken.as_str()),
        ("next", "decide")
    );
    let asked = checkpoints(&events);
    let gate = asked.last().unwrap();
    assert_eq!(gate.options, vec!["go", "amend", "stop"]);
    assert_eq!(
        gate.shown,
        vec![format!("read the check on #{}, then decide\n", fx.issue())],
        "the gate shows the checkpoint's rendered template"
    );
    assert!(
        step_started(&events).iter().all(|s| s.step != "decide"),
        "a checkpoint runs no child"
    );

    runner
        .answer(
            "decide",
            aigentic_log::CheckpointAnswer::Amend,
            Some("use the small fix".into()),
            steve(),
        )
        .unwrap();
    let ended = drive(fx, &mut runner).await;
    assert_eq!(
        ended,
        Advanced::Finished {
            outcome: aigentic_log::RunOutcome::Closed
        }
    );
    let prompts = child_prompts(fx, l.implement);
    assert!(
        prompts[0].contains("the person amended: use the small fix"),
        "the amendment reaches the next step: {}",
        prompts[0]
    );
    let check_prompt = &child_prompts(fx, l.check)[0];
    assert!(!check_prompt.contains("amended"), "and only later steps");

    let events = fx.lead_events();
    let pushed = pushed_events(&events);
    assert_eq!(pushed.len(), 1, "the implementer pushed once");
    assert_eq!(
        pushed[0].installed, None,
        "`install = false` installs nothing"
    );
    let taken = routes(&events);
    let path: Vec<(&str, &str)> = taken
        .iter()
        .map(|r| (r.branch.as_str(), r.taken.as_str()))
        .collect();
    assert_eq!(
        path,
        vec![
            ("next", "decide"),
            ("next", "implement"),
            ("next", "judge"),
            ("verdict", "done"),
        ],
        "every hop is a logged route"
    );
    assert!(
        !child_prompts(fx, l.judge).is_empty(),
        "the judge ran after the push"
    );
    assert!(!fx.forge.is_closed(), "a push before `done` closes nothing");
}

/// `go` continues a checkpoint with no amendment: the later prompt has
/// none. `stop` at a checkpoint ends the run.
#[tokio::test]
async fn go_continues_and_stop_ends_at_a_checkpoint() {
    let l = looped("approve");
    let fx = &l.fx;
    let mut runner = fx.runner();
    drive(fx, &mut runner).await;
    runner
        .answer("decide", aigentic_log::CheckpointAnswer::Go, None, steve())
        .unwrap();
    drive(fx, &mut runner).await;
    assert!(
        !child_prompts(fx, l.implement)[0].contains("amended"),
        "no amendment, no section"
    );

    let l = looped("approve");
    let fx = &l.fx;
    let mut runner = fx.runner();
    drive(fx, &mut runner).await;
    runner
        .answer(
            "decide",
            aigentic_log::CheckpointAnswer::Stop,
            None,
            steve(),
        )
        .unwrap();
    assert_eq!(
        drive(fx, &mut runner).await,
        Advanced::Finished {
            outcome: aigentic_log::RunOutcome::Stopped
        }
    );
    assert!(!fx.child_exists(l.implement), "nothing ran after stop");
}

/// A judge that routes to `ask` hands the run to a person.
#[tokio::test]
async fn a_judge_that_wants_changes_asks() {
    let l = looped("changes");
    let fx = &l.fx;
    let mut runner = fx.runner();
    drive(fx, &mut runner).await;
    runner
        .answer("decide", aigentic_log::CheckpointAnswer::Go, None, steve())
        .unwrap();
    assert_eq!(
        drive(fx, &mut runner).await,
        Advanced::WaitingHuman {
            gate: "route".into()
        }
    );
}

fn quick_ci() -> aigentic_runtime::runner::CiWait {
    aigentic_runtime::runner::CiWait {
        interval: std::time::Duration::from_millis(1),
        grace: std::time::Duration::from_millis(50),
        timeout: std::time::Duration::from_millis(200),
    }
}

/// CI that goes red after pending is a gate, and the judge never runs.
/// A runner's own gate takes `stop` only.
#[tokio::test]
async fn red_ci_after_the_push_is_a_gate() {
    use aigentic_runtime::runner::CiState;
    let l = looped("approve");
    let fx = &l.fx;
    fx.forge.script_ci(vec![
        CiState::Pending,
        CiState::Failed("ci: failure".into()),
    ]);
    let mut runner = fx.runner().with_ci_wait(quick_ci());
    drive(fx, &mut runner).await;
    runner
        .answer("decide", aigentic_log::CheckpointAnswer::Go, None, steve())
        .unwrap();
    assert_eq!(
        drive(fx, &mut runner).await,
        Advanced::WaitingHuman {
            gate: "ci_failed".into()
        }
    );
    let shown = gate_shown(&fx.lead_events());
    assert!(shown.iter().any(|line| line == "ci: failure"), "{shown:?}");
    assert!(!fx.child_exists(l.judge), "the judge never ran");
    runner
        .answer(
            "ci_failed",
            aigentic_log::CheckpointAnswer::Go,
            None,
            steve(),
        )
        .unwrap();
    assert!(
        matches!(runner.advance().await, Err(RunnerError::SliceTwo { .. })),
        "go at a runner's own gate is refused"
    );
}

/// A commit no CI run names past the grace is a gate, and so is CI that
/// never finishes.
#[tokio::test]
async fn missing_or_slow_ci_is_a_gate() {
    use aigentic_runtime::runner::CiState;
    for (states, gate) in [
        (vec![CiState::NoRuns], "ci_missing"),
        (vec![CiState::Pending], "ci_timeout"),
    ] {
        let l = looped("approve");
        let fx = &l.fx;
        fx.forge.script_ci(states);
        let mut runner = fx.runner().with_ci_wait(quick_ci());
        drive(fx, &mut runner).await;
        runner
            .answer("decide", aigentic_log::CheckpointAnswer::Go, None, steve())
            .unwrap();
        assert_eq!(
            drive(fx, &mut runner).await,
            Advanced::WaitingHuman { gate: gate.into() }
        );
    }
}

/// Rebuild the runner after every lead event of a looped run (answered
/// at its checkpoint): each rebuilt runner makes the full run's next move
/// once, with the same payload.
#[tokio::test]
async fn a_looped_run_rebuilds_after_every_event() {
    let l = looped("approve");
    let fx = &l.fx;
    let mut runner = fx.runner();
    let mut snapshots = vec![snapshot(fx)];
    let terminal = loop {
        match runner.advance().await.expect("the run advances") {
            Advanced::Moved => snapshots.push(snapshot(fx)),
            Advanced::WaitingHuman { gate } if gate == "decide" => {
                snapshots.push(snapshot(fx));
                runner
                    .answer("decide", aigentic_log::CheckpointAnswer::Go, None, steve())
                    .unwrap();
                snapshots.push(snapshot(fx));
            }
            other => break other,
        }
    };
    let full = fx.lead_events();
    assert_eq!(
        terminal,
        Advanced::Finished {
            outcome: aigentic_log::RunOutcome::Closed
        }
    );
    for k in 1..full.len() {
        // The person's answer is not a runner move: a rebuilt runner
        // waiting at the checkpoint writes nothing.
        if full[k].kind == EventKind::CheckpointAnswered {
            continue;
        }
        let snapshot = snapshots
            .iter()
            .find(|s| s.lead.len() == k)
            .expect("a snapshot per lead length");
        fx.restore(snapshot);
        fx.rewind_forge(snapshot);
        let mut rebuilt = fx.runner();
        rebuilt
            .advance()
            .await
            .expect("the rebuilt runner advances");
        let after = fx.lead_events();
        assert_eq!(after.len(), k + 1, "prefix {k}: one event appended");
        assert_eq!(after[k].kind, full[k].kind, "prefix {k}: the same move");
        assert_eq!(
            after[k].payload, full[k].payload,
            "prefix {k}: with the same payload"
        );
    }
}
