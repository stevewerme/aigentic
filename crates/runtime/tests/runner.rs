//! The build runner's tests (issue #57).

mod common;

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use aigentic_core::Budget;
use aigentic_core::{
    AgentId, Author, ContentBlock, Event, EventKind, ProviderError, ProviderEvent, UserId,
};
use aigentic_log::{
    BUDGET_GATE, BudgetRaisedPayload, CheckpointAnswer, NextMove, PermissionRequestedPayload,
    RunStartedPayload, StepReport, StepStatus, ThreadLog, ThreadStartedPayload, run_state,
};
use aigentic_policy::Policy;
use aigentic_runtime::runner::{
    Advanced, FakeForge, FakeInstaller, Forge, GhForge, GitRepo, IssueView, Runner, RunnerError,
    RunnerHost, WriteGuard,
};
use aigentic_runtime::workflow::{LoadedWorkflow, SlotKind, WorkflowFile, WorkflowOrigin};
use aigentic_runtime::{Answer, Approver, LENGTH_STOP, MAX_USD, Prices, Runtime};
use aigentic_runtime::{
    RuntimeError, STEP_REPORTED,
    harness_tools::FINISH_STEP,
    runner::{
        BUDGET_ASK_AT, CALL_FINISH_STEP, CONTINUE_PROMPT, REPORT_AGAIN_PROMPT, REPORT_WITHOUT_BODY,
    },
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

/// One scripted `bash` call, the shape `tests/finish_step.rs` uses.
fn bash_call(id: &str, command: &str) -> ProviderEvent {
    ProviderEvent::ToolCall(aigentic_core::ToolCall {
        id: id.into(),
        name: "bash".into(),
        args: json!({ "command": command }),
    })
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
    /// Whether a child's registry is the builtin one, rooted in the
    /// fixture's work repo, so a test can see a real command's policy
    /// verdict and what the command did.
    bash: bool,
}

impl FakeHost {
    fn new(
        dir: PathBuf,
        lead: Ulid,
        children: &[(Ulid, Vec<Vec<ProviderEvent>>)],
        caps: BTreeMap<Ulid, u32>,
        prices: Option<Prices>,
        bash: bool,
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
            bash,
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
            let registry: ToolRegistry = if self.bash {
                // The fixture's work repo is the child's workdir, so a
                // scripted `bash` call reads and writes the tree the run's
                // checks read.
                ToolRegistry::builtin(
                    aigentic_tools::Workdir::new(self.dir.join("repo")),
                    aigentic_tools::DEFAULT_TIMEOUT,
                )
            } else {
                vec![Box::new(Noop) as Box<dyn aigentic_core::Tool>].into()
            };
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
                    max_cost_usd: None,
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
    /// Whether the children's registry is the builtin one, rooted in the
    /// fixture's work repo.
    bash: bool,
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
            bash: false,
        }
    }

    /// Replace one child's script: a test that must spell a value it only
    /// knows once the fixture exists (a recorded head) scripts it here.
    fn script(mut self, id: Ulid, replies: Vec<Vec<ProviderEvent>>) -> Self {
        if let Some((_, script)) = self.children.iter_mut().find(|(child, _)| *child == id) {
            *script = replies;
        }
        self
    }

    /// Give every child the builtin tools, rooted in the fixture's work
    /// repo, so a test can see a real command's policy verdict and what it
    /// did to that repo.
    fn with_bash(mut self) -> Self {
        self.bash = true;
        self
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
            self.bash,
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
        self.restore_child_prefix_from(snapshot, snapshot, id, keep);
    }

    /// As `restore_child_prefix`, with the child's prefix cut from a later
    /// snapshot: the lead at a step's `step_started`, the child part-way
    /// through the turn that only a later move runs.
    fn restore_child_prefix_from(
        &self,
        snapshot: &Snapshot,
        source: &Snapshot,
        id: Ulid,
        keep: impl Fn(&[Event]) -> bool,
    ) {
        self.restore(snapshot);
        let Some(events) = source.children.get(&id) else {
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

/// A lead event's payload as a rebuild must reproduce it. A rebuild
/// from `step_started` runs the child's turn itself, so the report it
/// points at is a new event: `reported_event` is compared by presence.
fn same_move(event: &Event) -> Value {
    let mut payload = event.payload.clone();
    if event.kind == EventKind::StepFinished
        && let Some(reported) = payload.get_mut("reported_event")
        && !reported.is_null()
    {
        *reported = Value::from("some");
    }
    payload
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
    // The move before this drive (a rebuild's first, say) may have run a
    // child's whole turn, so its git work is due before the next move.
    between();
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

/// Every `tool_result` payload of a run, oldest first: the verdict a
/// command got and whether it ran.
fn tool_results(events: &[Event]) -> Vec<aigentic_log::ToolResultPayload> {
    events
        .iter()
        .filter(|event| event.kind == EventKind::ToolResult)
        .map(|event| serde_json::from_value(event.payload.clone()).unwrap())
        .collect()
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
                same_move(&after[k]),
                same_move(&run.lead()[k]),
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

/// The nudge asks for the step's remaining work, not only for a report: it
/// names the tool, says the step is unfinished, and names the gate and the
/// commits a step's prompt may ask for.
#[test]
fn the_finish_step_nudge_asks_for_the_rest_of_the_work() {
    assert!(
        CALL_FINISH_STEP.contains(FINISH_STEP),
        "the nudge names the tool the step must call: {CALL_FINISH_STEP}"
    );
    assert_ne!(
        CALL_FINISH_STEP, FINISH_STEP,
        "the nudge is more than the tool's name: {CALL_FINISH_STEP}"
    );
    assert_ne!(
        CALL_FINISH_STEP, CONTINUE_PROMPT,
        "a turn that stopped on its own is told more than a capped one: {CALL_FINISH_STEP}"
    );
    assert!(
        CALL_FINISH_STEP.contains("continue"),
        "it asks the step to continue where it stopped: {CALL_FINISH_STEP}"
    );
    assert!(
        CALL_FINISH_STEP.contains("gate"),
        "the rest of the work includes the prompt's gate: {CALL_FINISH_STEP}"
    );
    assert!(
        CALL_FINISH_STEP.contains("commit"),
        "the rest of the work includes the prompt's commits: {CALL_FINISH_STEP}"
    );
    assert!(
        !CALL_FINISH_STEP.contains('#'),
        "the nudge carries no issue number: {CALL_FINISH_STEP}"
    );
    assert!(
        !CALL_FINISH_STEP.contains('\n'),
        "the nudge is one line, like the other prompts: {CALL_FINISH_STEP}"
    );
}

/// A turn that stops without reporting earns one nudge, and the report that
/// follows it finishes the step: the nudge asks for the work the step has
/// left, not only for the report.
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
    assert_eq!(
        finished[2].status,
        StepStatus::Done,
        "the report that follows one nudge finishes the step"
    );
    assert_eq!(finished[2].end_reason, STEP_REPORTED);
    assert!(
        finished[2].reported_event.is_some(),
        "the finish points at the report it posted"
    );
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

/// A child whose turn dies on a provider error surfaces that error and is
/// told nothing: the provider, not the step, failed, so the runner returns
/// the error for the caller to act on instead of nudging the step.
#[tokio::test]
async fn a_child_provider_error_posts_no_nudge() {
    let brief_child = Ulid::generate();
    let implementer_child = Ulid::generate();
    let fx = Fixture::new(vec![
        (brief_child, vec![report("r1", brief_report())]),
        (
            implementer_child,
            vec![vec![ProviderEvent::Error(ProviderError::Http {
                status: 500,
                body: "boom".into(),
            })]],
        ),
    ]);

    let mut runner = fx.runner();
    let error = loop {
        match runner.advance().await {
            Ok(Advanced::Moved) => {}
            Ok(other) => panic!("the run ended without the provider's error: {other:?}"),
            Err(error) => break error,
        }
    };
    assert!(
        matches!(
            error,
            RunnerError::Turn(RuntimeError::Provider(ProviderError::Http {
                status: 500,
                ..
            }))
        ),
        "the provider's error surfaces from the run: {error:?}"
    );

    let child = fx.child_events_of(implementer_child);
    assert_eq!(
        prompts(&child).len(),
        1,
        "the child holds the rendered prompt and no nudge after it"
    );
    let ended = child
        .iter()
        .rev()
        .find(|event| event.kind == EventKind::TurnEnded)
        .expect("the turn ended");
    let reason = ended.payload["reason"].as_str().unwrap_or_default();
    assert!(
        reason.starts_with("provider_error: http 500"),
        "the turn ended on the provider's error, not on the model's: {reason}"
    );

    let lead = fx.lead_events();
    let started: Vec<_> = step_started(&lead)
        .into_iter()
        .filter(|started| started.step == "implement-alone")
        .collect();
    assert_eq!(started.len(), 1, "no second attempt started");
    assert_eq!(started[0].attempt, 1);
    assert!(
        !step_finished(&lead)
            .iter()
            .any(|finished| finished.step == "implement-alone"),
        "the step has no end, so nothing was posted after the error"
    );
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

/// A report with no body has nothing to post: the step is asked to
/// report again once, in the same child, and the body it then gives is
/// posted. `finish_step` refuses such a report, so the case is a log an
/// earlier binary wrote, met on resume: the daemon died after the child
/// reported and before the runner read it.
#[tokio::test]
async fn a_report_without_a_body_is_asked_again_once() {
    let brief_child = Ulid::generate();
    let implementer_child = Ulid::generate();
    let fx = Fixture::new(vec![
        (brief_child, vec![report("r1", brief_report())]),
        (
            implementer_child,
            vec![
                report("r2", implementer_report()),
                report("r3", implementer_report()),
            ],
        ),
    ]);
    let run = trace(&fx).await;
    fx.restore_child_prefix_from(
        run.after_where(EventKind::StepStarted, 2),
        run.after_where(EventKind::StepFinished, 2),
        implementer_child,
        |events| {
            !events
                .iter()
                .any(|event| event.kind == EventKind::TurnEnded)
        },
    );
    fx.rewind_forge(run.after_where(EventKind::StepStarted, 2));
    let mut events = fx.child_events_of(implementer_child);
    let reported = events
        .iter_mut()
        .find(|event| event.kind == EventKind::StepReported)
        .expect("the implementer reported");
    reported.payload["body"] = Value::Null;
    write_events(
        &fx.dir.path().join(format!("{implementer_child}.jsonl")),
        &events,
    );

    let mut runner = fx.runner();
    assert_eq!(
        drive(&fx, &mut runner).await,
        Advanced::Finished {
            outcome: aigentic_log::RunOutcome::Closed
        },
        "the second report carries the run on"
    );
    let reasons: Vec<String> = step_finished(&fx.lead_events())
        .iter()
        .map(|finished| finished.end_reason.clone())
        .collect();
    assert_eq!(
        reasons,
        [STEP_REPORTED, REPORT_WITHOUT_BODY, STEP_REPORTED],
        "the headless attempt is partial, the next one reports"
    );
    let prompts = child_prompts(&fx, implementer_child);
    assert_eq!(
        prompts.len(),
        2,
        "one prompt per attempt, in the same child"
    );
    assert_eq!(prompts[1], REPORT_AGAIN_PROMPT);
    let posted = fx.forge.posted();
    assert!(
        !posted
            .iter()
            .any(|comment| comment.contains("step=implement-alone attempt=1")),
        "nothing is posted for the report with no body"
    );
    assert!(
        posted
            .iter()
            .any(|comment| comment.contains("step=implement-alone attempt=2")),
        "the second report is posted"
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
    fx.restore_child_prefix_from(
        run.after_where(EventKind::StepStarted, 1),
        run.after_where(EventKind::StepFinished, 1),
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

/// A rebuild in the window between `step_started(2)` and its nudge posts
/// the nudge once, and the step goes on to report: the nudge is the next
/// attempt's prompt, so a crash cannot double it.
#[tokio::test]
async fn the_nudge_is_posted_once_on_the_retry() {
    let brief_child = Ulid::generate();
    let implementer_child = Ulid::generate();
    let fx = Fixture::new(vec![
        (brief_child, vec![report("r1", brief_report())]),
        (
            implementer_child,
            vec![stop("done"), report("r2", implementer_report())],
        ),
    ]);
    let run = trace(&fx).await;
    assert_eq!(
        run.terminal,
        Advanced::Finished {
            outcome: aigentic_log::RunOutcome::Closed
        },
        "the run closes"
    );

    // The lead has `step_started(2)`; the child's log holds attempt 1's
    // prompt and its end, but not the nudge.
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
    // The child's log ends after attempt 1's turn: the crash landed
    // between `step_started` and the nudge.
    fx.restore_child_prefix(snapshot, implementer_child, |events| turn_ends(events) <= 1);

    let (advanced, mut runner) = replay(&fx, snapshot).await;
    assert_eq!(advanced, Advanced::Moved, "no escalation");
    let messages = prompts(&fx.child_events_of(implementer_child));
    assert_eq!(
        messages,
        vec![messages[0].clone(), CALL_FINISH_STEP.to_owned()],
        "the nudge is posted once, after the rendered prompt"
    );
    assert_eq!(
        messages.iter().filter(|m| *m == CALL_FINISH_STEP).count(),
        1
    );
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
        false,
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

/// The fold route a send-back names, as the templates spell it.
fn fold_sentence(base: &str) -> String {
    format!(
        "To fold work into an earlier commit, `git reset --soft {base}` and recommit it with the same subject and trailer; the tree must not change."
    )
}

/// A send-back names the fold route, spelled with the step's own start;
/// a first prompt does not.
#[tokio::test]
async fn t19_a_send_back_names_the_fold_route_with_the_steps_own_start() {
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

    let started: Vec<_> = step_started(run.lead())
        .into_iter()
        .filter(|started| started.step == "implement-alone")
        .collect();
    assert_eq!(started.len(), 2, "one start per attempt");
    let base = started[0]
        .head_at_start
        .clone()
        .expect("the first attempt records where the step started");
    assert_eq!(
        started[1].head_at_start, None,
        "a later attempt records no start of its own"
    );

    let prompts = child_prompts(fx, h.implementer_child);
    assert_eq!(prompts.len(), 2, "one prompt per attempt");
    assert!(
        !prompts[0].contains("git reset --soft "),
        "a first prompt does not name a fold: {}",
        prompts[0]
    );
    assert!(
        prompts[1].contains(&fold_sentence(&base)),
        "the send-back names the fold route and the step's own start: {}",
        prompts[1]
    );
}

/// The route is withheld once the remote has moved past the step's
/// start, and the run is never pushed over the stranger.
#[tokio::test]
async fn t20_the_fold_route_is_withheld_once_the_remote_has_moved() {
    let h = retryable();
    let fx = &h.fx;
    let mut runner = fx.runner();
    let mut subject = WrongSubject::new(h.implementer_child);
    let mut stranger = None;
    let advanced = loop {
        let advanced = runner.advance().await.expect("the run advances");
        subject.between(fx, true);
        if fx.head() != fx.initial_head() && stranger.is_none() {
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
    assert_eq!(
        fx.remote_head(),
        stranger.unwrap(),
        "the remote holds the stranger's commit, not the step's"
    );

    let prompts = child_prompts(fx, h.implementer_child);
    assert_eq!(prompts.len(), 2, "one prompt per attempt");
    assert!(
        prompts[1].contains("Sent back by the runner"),
        "the second prompt is the send-back: {}",
        prompts[1]
    );
    assert!(
        !prompts[1].contains("git reset --soft ") && !prompts[1].contains("{{"),
        "a moved remote withholds the fold route: {}",
        prompts[1]
    );
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

/// An error the runner hit is written to the lead once, without ending
/// the run: the fold still moves, so the next build resumes it.
#[tokio::test]
async fn a_runner_error_is_noted_once_and_the_move_still_resumes() {
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
    let reason = err.to_string();

    // What the log says before the note: the cost the event carries, and
    // a run that has not ended.
    let events = fx.lead_events();
    let before_len = events.len();
    let before = aigentic_log::run_state(&events).expect("the lead folds");
    assert!(before.outcome.is_none(), "the run has not ended");
    assert!(before.errored.is_none(), "and nothing errored yet");

    runner
        .note_run_error(&reason)
        .expect("the lead is writable");
    let events = fx.lead_events();
    assert_eq!(events.len(), before_len + 1, "one event");
    let last = events.last().expect("the note is the last event");
    assert_eq!(last.kind, EventKind::RunErrored);
    let payload: aigentic_log::RunErroredPayload =
        serde_json::from_value(last.payload.clone()).unwrap();
    assert_eq!(payload.reason, reason, "the error's own text");
    assert_eq!(payload.cost_usd, before.cost_usd, "the fold's cost");

    let state = runner.state().expect("the lead folds");
    assert!(state.errored.is_some(), "the stop is on the record");
    assert!(state.outcome.is_none(), "the run is not final");
    // The note decided nothing: the move is the one the log gave before it
    // was appended, and a rebuild off that log carries the run on instead
    // of refusing the lead as "not a run".
    let without_the_note = aigentic_log::run_state(&events[..before_len]).expect("the lead folds");
    assert_eq!(
        state.next_move(),
        without_the_note.next_move(),
        "a note moves no run"
    );

    // The same error seen twice writes nothing the second time.
    runner
        .note_run_error(&reason)
        .expect("the lead is writable");
    assert_eq!(fx.lead_events().len(), events.len(), "one event per error");

    let mut resumed = fx.runner();
    assert_eq!(
        drive(fx, &mut resumed).await,
        Advanced::Finished {
            outcome: aigentic_log::RunOutcome::Closed
        },
        "the run stopped, not the issue's work"
    );
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

    // The rebuilt runner still sends the step back with the failure in it:
    // one move starts attempt 2, the next posts its prompt.
    let mut runner = fx.runner();
    assert_eq!(
        runner.advance().await.expect("the run advances"),
        Advanced::Moved,
        "the send-back starts attempt 2"
    );
    assert_eq!(
        runner.advance().await.expect("the run advances"),
        Advanced::Moved,
        "attempt 2 gets its prompt"
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

/// The repository's own `loop` workflow loads: every template parses,
/// every slot it uses is declared, every `next` and route names a step,
/// and its checkpoint, pushes and CI waits are where the loop needs them.
#[test]
fn the_repositorys_loop_workflow_loads() {
    let loaded = WorkflowFile::load_dir(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.aigentic/workflows/loop"),
        WorkflowOrigin::Project,
    )
    .expect("the loop workflow loads");
    let ids: Vec<&str> = loaded
        .workflow
        .steps
        .iter()
        .map(|step| step.id.as_str())
        .collect();
    assert_eq!(
        ids,
        [
            "spec",
            "speccheck",
            "decide",
            "merge",
            "implement",
            "judge",
            "fix",
            "rejudge"
        ]
    );
    assert!(loaded.step("decide").unwrap().checkpoint);
    for id in ["implement", "fix"] {
        let step = loaded.step(id).unwrap();
        assert!(step.writes && step.push && step.ci && !step.install, "{id}");
    }
    assert_eq!(
        loaded
            .step("judge")
            .unwrap()
            .routes
            .get("changes")
            .map(String::as_str),
        Some("fix")
    );
}

/// Both loop prompts and the bundled standalone implementer name the one
/// form the checks can read: a named run as its own bash call, inside the
/// gate section — and, where the prompt carries the `1 passed` rule, under
/// it.
///
/// The standalone prompt is rendered twice with the whole slot map filled:
/// once with the `ui` section and once without it, since a sentence that
/// landed in that section would be missing from the code-only prompt.
#[test]
fn the_implementation_prompts_ask_for_one_unpiped_call_per_test() {
    const CALL: &str = "Run each planned test as its own bash call, `cargo test -p <crate> <exact name>`, with no pipe, no redirection and no loop, so the checks can read its output.";

    /// Every declared slot filled by its kind, so optional sections are
    /// kept and no tag is left empty.
    fn filled(loaded: &LoadedWorkflow) -> BTreeMap<String, Value> {
        loaded
            .workflow
            .slots
            .iter()
            .map(|slot| {
                let value = match slot.kind {
                    SlotKind::Bool => Value::Bool(true),
                    SlotKind::String => Value::String(format!("x {}", slot.name)),
                };
                (slot.name.clone(), value)
            })
            .collect()
    }

    /// The offset of a phrase asserted to occur exactly once: a sentence
    /// copied twice, or into the safety list, fails the position it is
    /// read at.
    fn only(prompt: &str, needle: &str) -> usize {
        let mut found = prompt.match_indices(needle).map(|(at, _)| at);
        let at = found
            .next()
            .unwrap_or_else(|| panic!("{needle:?} is missing"));
        assert!(found.next().is_none(), "{needle:?} occurs more than once");
        at
    }

    fn checked(prompt: &str, carries_the_rule: bool) {
        let call = only(prompt, CALL);
        assert!(
            only(prompt, "## The gate") < call,
            "the sentence sits outside the gate section"
        );
        assert!(
            call < only(prompt, "## Safety"),
            "the sentence sits in the safety list"
        );
        if carries_the_rule {
            assert!(
                only(prompt, "`1 passed`") < call,
                "the sentence sits above the rule it qualifies"
            );
        }
    }

    let loop_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.aigentic/workflows/loop");
    let loop_workflow =
        WorkflowFile::load_dir(&loop_dir, WorkflowOrigin::Project).expect("the loop loads");
    let loop_slots = filled(&loop_workflow);
    for (step, carries_the_rule) in [("implement", true), ("fix", false)] {
        let prompt = loop_workflow.render(step, &loop_slots).expect("it renders");
        checked(&prompt, carries_the_rule);
    }

    let build_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../workflows/build");
    let build_workflow = WorkflowFile::load_dir(&build_dir, WorkflowOrigin::Bundled)
        .expect("the bundled build workflow loads");
    let mut build_slots = filled(&build_workflow);
    checked(
        &build_workflow
            .render("implement-alone", &build_slots)
            .expect("it renders"),
        true,
    );
    build_slots.insert("ui".to_string(), Value::Bool(false));
    checked(
        &build_workflow
            .render("implement-alone", &build_slots)
            .expect("it renders"),
        true,
    );
}

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

/// A one-step workflow whose step routes on `size`: the fallback is for
/// `verdict` alone, so a missing `size` slot still escalates.
const SIZE_TOML: &str = r###"
name = "size-test"
version = 1

[budget]
trivial = 3.0
full = 10.0
max_raise = 2.0

[[slots]]
name = "issue"
kind = "string"
filled_by = "runner"

[[steps]]
id = "sizer"
role = "sizer"
profile = "flash"
template = "templates/sizer.md"
marker = "## Size"
route_by = "size"
routes = { trivial = "ask", full = "ask" }
"###;

/// A non-writing step whose template carries the fold block and whose deny
/// list holds `git reset`: a slot the runner must not fill shows up in its
/// prompt, and a wrong arm shows up in its overlay.
const FOLDLESS_TOML: &str = r###"
name = "foldless-test"
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
name = "head_at_start"
kind = "string"
required = false
filled_by = "runner"

[[steps]]
id = "foldless"
role = "sizer"
profile = "flash"
template = "templates/foldless.md"
marker = "## Size"
deny = ["git reset"]
next = "done"
"###;

fn foldless_files() -> Vec<(&'static str, &'static str)> {
    vec![
        ("workflow.toml", FOLDLESS_TOML),
        (
            "templates/foldless.md",
            "size issue {{issue}}\n{{#head_at_start}}\nTo fold work into an earlier commit, `git reset --soft {{head_at_start}}` and recommit it with the same subject and trailer; the tree must not change.\n{{/head_at_start}}\n",
        ),
    ]
}

/// The sanctioned fold line reaches a writing step's own runtime, and no
/// other base is armed there.
#[tokio::test]
async fn t21_the_fold_is_armed_end_to_end_and_another_base_is_not() {
    let brief = Ulid::generate();
    let implementer = Ulid::generate();
    let fx = Fixture::assemble(
        vec![
            (brief, vec![report("r1", brief_report())]),
            (implementer, Vec::new()),
        ],
        Vec::new(),
        None,
    )
    .with_bash()
    .capped(implementer, 6);
    // The seed commit is what the step's first attempt records, so it is
    // both the base the runner arms and the value the script spells.
    let base = fx.initial_head();
    let other = {
        let mut digits: Vec<char> = base.chars().collect();
        let last = *digits.last().unwrap();
        *digits.last_mut().unwrap() = if last == '0' { '1' } else { '0' };
        digits.into_iter().collect::<String>()
    };
    assert_ne!(other, base);
    let fx = fx.script(
        implementer,
        vec![
            vec![
                bash_call("call-1", &format!("git reset --soft {base}")),
                usage(10, 5),
                done("tool_use"),
            ],
            vec![
                bash_call("call-2", &format!("git reset --soft {other}")),
                usage(10, 5),
                done("tool_use"),
            ],
            report("r2", implementer_report()),
        ],
    );
    let mut runner = fx.runner();
    let mut moves = 0;
    while tool_results(&fx.child_events_of(implementer)).len() < 2 {
        moves += 1;
        assert!(moves < 20, "the child's two calls land within a few moves");
        runner.advance().await.expect("the run advances");
        assert_eq!(
            fx.head(),
            base,
            "the sanctioned reset folded nothing: the step had committed nothing"
        );
    }
    let started = step_started(&fx.lead_events());
    let record = started
        .iter()
        .find(|started| started.step == "implement-alone")
        .expect("the implementer step started");
    assert_eq!(
        record.head_at_start.as_deref(),
        Some(base.as_str()),
        "the arm's source is the step's own start"
    );
    let results = tool_results(&fx.child_events_of(implementer));
    assert!(
        !results[0].result.is_error,
        "the step's own start is sanctioned: {}",
        results[0].result.content
    );
    assert!(
        results[1].result.is_error && results[1].result.content.contains("step deny git reset"),
        "another base is refused: {}",
        results[1].result.content
    );
}

/// A step that does not write gets no fold route: no slot in its prompt,
/// no start recorded, and nothing armed in the child's runtime, so the
/// very line a writing step is sanctioned to use is refused there.
#[tokio::test]
async fn t22_a_step_without_a_start_record_gets_no_fold_route() {
    let child = Ulid::generate();
    let seed = Fixture::assemble(Vec::new(), Vec::new(), None).initial_head();
    let fold = format!("git reset --soft {seed}");
    let fx = Fixture::with_workflow(
        vec![(
            child,
            vec![
                vec![bash_call("call-1", &fold)],
                report("s1", reported("## Size", json!({}))),
            ],
        )],
        &foldless_files(),
    )
    .with_bash();

    let mut runner = fx.runner();
    assert_eq!(
        drive(&fx, &mut runner).await,
        Advanced::Finished {
            outcome: aigentic_log::RunOutcome::Closed
        },
        "the one-step run closes"
    );

    let prompts = child_prompts(&fx, child);
    assert_eq!(prompts.len(), 1, "one prompt");
    assert!(
        !prompts[0].contains("git reset --soft ") && !prompts[0].contains("{{"),
        "the fold route is withheld: {}",
        prompts[0]
    );
    assert!(
        step_started(&fx.lead_events())
            .iter()
            .all(|started| started.head_at_start.is_none()),
        "a step that does not write records no start"
    );
    let results = tool_results(&fx.child_events_of(child));
    assert_eq!(
        results.len(),
        2,
        "the scripted bash call, then the step's own report"
    );
    let Some(aigentic_log::PolicyRecord::Rule { rule, .. }) = results[0].policy.as_ref() else {
        panic!("a rule refused the call: {:?}", results[0].policy);
    };
    assert_eq!(
        rule, "step deny git reset",
        "nothing is armed without a start: {}",
        results[0].result.content
    );
}

fn size_files() -> Vec<(&'static str, &'static str)> {
    vec![
        ("workflow.toml", SIZE_TOML),
        ("templates/sizer.md", "size issue {{issue}}\n"),
    ]
}

/// The same one-step shape with a `verdict` branch that routes `approve`
/// only, so the mapped `changes` word names no route.
const VERDICT_ONLY_TOML: &str = r###"
name = "verdict-only-test"
version = 1

[budget]
trivial = 3.0
full = 10.0
max_raise = 2.0

[[slots]]
name = "issue"
kind = "string"
filled_by = "runner"

[[steps]]
id = "judge"
role = "judge"
profile = "flash"
template = "templates/judge.md"
marker = "## Review"
route_by = "verdict"
routes = { approve = "done" }
"###;

fn verdict_only_files() -> Vec<(&'static str, &'static str)> {
    vec![
        ("workflow.toml", VERDICT_ONLY_TOML),
        ("templates/judge.md", "judge issue {{issue}}\n"),
    ]
}

/// One step of the fixture's workflow, by id.
fn step_named(fx: &Fixture, id: &str) -> aigentic_runtime::workflow::Step {
    fx.workflow
        .workflow
        .steps
        .iter()
        .find(|step| step.id == id)
        .unwrap_or_else(|| panic!("the fixture's workflow has a `{id}` step"))
        .clone()
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

/// The same report with `verdict` set as `finish_step`'s top-level field.
fn with_field(mut args: Value, verdict: &str) -> Value {
    args["verdict"] = json!(verdict);
    args
}

/// A report carrying `verdict` as its top-level field and no `slots` key at
/// all: the shape a judge wrote when a finished, approved run ended
/// `stopped`.
fn verdict_report(marker: &str, verdict: &str) -> Value {
    json!({
        "status": "done",
        "body": format!("{marker}\n\nwhat this step found"),
        "verdict": verdict,
        "release_impact": "patch",
    })
}

struct Looped {
    fx: Fixture,
    check: Ulid,
    implement: Ulid,
    judge: Ulid,
}

/// The loop fixture whose judge reports `judge_args`, whatever shape the
/// verdict comes in.
fn looped_report(judge_args: Value) -> Looped {
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
            (judge, vec![report("j1", judge_args)]),
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

/// The same world with the verdict as a `slots.verdict` entry.
fn looped(verdict: &str) -> Looped {
    looped_report(reported("## Review", json!({ "verdict": verdict })))
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

/// A judge whose verdict is only `finish_step`'s top-level field still
/// approves: the run it was written by ended `stopped` while the report
/// said `approve`, because the runner read the slot alone.
#[tokio::test]
async fn a_report_with_the_verdict_only_at_the_top_level_approves() {
    let l = looped_report(verdict_report("## Review", "approve"));
    let fx = &l.fx;
    let mut runner = fx.runner();
    drive(fx, &mut runner).await;
    runner
        .answer("decide", aigentic_log::CheckpointAnswer::Go, None, steve())
        .unwrap();
    assert_eq!(
        drive(fx, &mut runner).await,
        Advanced::Finished {
            outcome: aigentic_log::RunOutcome::Closed
        }
    );
    // The fixture's own route is the expectation, not a typed word.
    let judge = step_named(fx, "judge");
    let branch = judge.route_by.as_deref().expect("the judge has a branch");
    let word = serde_json::to_value(aigentic_log::Verdict::Approve)
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned();
    let taken = judge
        .routes
        .get(&word)
        .expect("the fixture's judge routes the approve word");
    let lead = fx.lead_events();
    let last = routes(&lead).pop().expect("the judge took a route");
    assert_eq!(last.branch, branch);
    assert_eq!(last.proposed, word);
    assert_eq!(&last.taken, taken);
    assert_eq!(gate(&lead), None);
}

/// A top-level `changes_needed` reaches the fixture's `changes` route and
/// opens the route gate, the move a person can answer.
#[tokio::test]
async fn a_top_level_changes_needed_reaches_the_changes_route() {
    let l = looped_report(verdict_report("## Review", "changes_needed"));
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
    let judge = step_named(fx, "judge");
    let branch = judge.route_by.as_deref().expect("the judge has a branch");
    // The mapped word is the issue's rule, so the judge found `changes`.
    let taken = judge
        .routes
        .get("changes")
        .expect("the fixture's judge routes the changes word");
    let lead = fx.lead_events();
    let judge_routes: Vec<_> = routes(&lead)
        .into_iter()
        .filter(|route| route.branch == branch)
        .collect();
    assert_eq!(judge_routes.len(), 1, "the judge routed once");
    assert_eq!(judge_routes[0].proposed, "changes");
    assert_eq!(&judge_routes[0].taken, taken);
    assert_eq!(gate(&lead), Some("route".to_owned()));
}

/// The slot wins when both spellings are present, and a non-string slot is
/// no value at all: three worlds, one test.
#[tokio::test]
async fn a_verdict_slot_wins_over_the_top_level_field() {
    use aigentic_log::RunOutcome;
    let cases = [
        (
            with_field(
                reported("## Review", json!({ "verdict": "changes" })),
                "approve",
            ),
            true,
            "changes",
        ),
        (
            with_field(
                reported("## Review", json!({ "verdict": "approve" })),
                "changes_needed",
            ),
            false,
            "approve",
        ),
        (
            with_field(reported("## Review", json!({ "verdict": true })), "approve"),
            false,
            "approve",
        ),
    ];
    for (args, waits, proposed) in cases {
        let l = looped_report(args);
        let fx = &l.fx;
        let mut runner = fx.runner();
        drive(fx, &mut runner).await;
        runner
            .answer("decide", aigentic_log::CheckpointAnswer::Go, None, steve())
            .unwrap();
        let advanced = drive(fx, &mut runner).await;
        if waits {
            assert_eq!(
                advanced,
                Advanced::WaitingHuman {
                    gate: "route".into()
                }
            );
        } else {
            assert_eq!(
                advanced,
                Advanced::Finished {
                    outcome: RunOutcome::Closed
                }
            );
        }
        let last = routes(&fx.lead_events())
            .pop()
            .expect("the judge took a route");
        assert_eq!(last.proposed, proposed);
    }
}

/// A branch whose slot is missing still escalates, and the gate names the
/// branch: the fallback is for `verdict` alone.
#[tokio::test]
async fn a_missing_size_slot_still_escalates() {
    let sizer = Ulid::generate();
    let fx = Fixture::with_workflow(
        vec![(sizer, vec![report("s1", reported("## Size", json!({})))])],
        &size_files(),
    );
    let mut runner = fx.runner();
    assert_eq!(
        drive(&fx, &mut runner).await,
        Advanced::WaitingHuman {
            gate: "route_escalate".into()
        }
    );
    let step = step_named(&fx, "sizer");
    let branch = step.route_by.as_deref().expect("the step has a branch");
    let lead = fx.lead_events();
    assert!(
        routes(&lead).iter().all(|route| route.branch != branch),
        "the sizer took no route"
    );
    let shown = gate_shown(&lead);
    assert_eq!(shown[0], step.id);
    assert_eq!(shown[2], format!("the report has no `{branch}` slot"));
}

/// A `verdict` step with neither spelling escalates, and its gate names
/// both places it looked.
#[tokio::test]
async fn a_verdict_step_with_neither_spelling_escalates() {
    let l = looped_report(reported("## Review", json!({})));
    let fx = &l.fx;
    let mut runner = fx.runner();
    drive(fx, &mut runner).await;
    runner
        .answer("decide", aigentic_log::CheckpointAnswer::Go, None, steve())
        .unwrap();
    assert_eq!(
        drive(fx, &mut runner).await,
        Advanced::WaitingHuman {
            gate: "route_escalate".into()
        }
    );
    let step = step_named(fx, "judge");
    let branch = step.route_by.as_deref().expect("the judge has a branch");
    let lead = fx.lead_events();
    assert!(
        routes(&lead).iter().all(|route| route.branch != branch),
        "the judge took no route"
    );
    assert_eq!(
        gate_shown(&lead)[2],
        format!("the report has no `{branch}` slot and no `{branch}` field")
    );
}

/// A verdict naming no route still escalates, and the gate names the value.
#[tokio::test]
async fn a_verdict_naming_no_route_still_escalates() {
    let judge = Ulid::generate();
    let fx = Fixture::with_workflow(
        vec![(
            judge,
            vec![report("j1", verdict_report("## Review", "changes_needed"))],
        )],
        &verdict_only_files(),
    );
    let mut runner = fx.runner();
    assert_eq!(
        drive(&fx, &mut runner).await,
        Advanced::WaitingHuman {
            gate: "route_escalate".into()
        }
    );
    let step = step_named(&fx, "judge");
    let branch = step.route_by.as_deref().expect("the step has a branch");
    let lead = fx.lead_events();
    assert!(
        routes(&lead).iter().all(|route| route.branch != branch),
        "the judge took no route"
    );
    assert_eq!(
        gate_shown(&lead)[2],
        format!("`{branch} = changes` names no route")
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
            same_move(&after[k]),
            same_move(&full[k]),
            "prefix {k}: with the same payload"
        );
    }
}

// ---------------------------------------------------------------------------
// A `checks_failed` gate can grant one more attempt
// ---------------------------------------------------------------------------

/// The happy path with a third scripted reply for the implementer, so an
/// attempt past the send-back has a report of its own.
fn retryable_twice() -> Happy {
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
                    report("r4", implementer_report()),
                ],
            ),
        ]),
        brief_child,
        implementer_child,
    }
}

/// Whether the lead log holds a `retry` answer. The driver keys both its
/// amend and its next answer on the log, not on its own state, so a
/// rebuild from any prefix behaves like the run.
fn retry_answered(events: &[Event]) -> bool {
    events.iter().any(|event| {
        event.kind == EventKind::CheckpointAnswered
            && serde_json::from_value::<aigentic_log::CheckpointAnsweredPayload>(
                event.payload.clone(),
            )
            .map(|payload| payload.answer == aigentic_log::CheckpointAnswer::Retry)
            .unwrap_or(false)
    })
}

/// Drive to the `checks_failed` gate, answer `retry`, and keep driving.
/// The retried attempt's checks fail too, so the second gate is answered
/// `stop`; `fix` amends the wrong subject once the retry answer is in the
/// log, so that attempt passes instead and the run closes.
async fn drive_a_retry(fx: &Fixture, implementer: Ulid, fix: bool) -> Trace {
    let mut runner = fx.runner();
    let mut snapshots = vec![snapshot(fx)];
    let mut subject = WrongSubject::new(implementer);
    let terminal = loop {
        match runner.advance().await.expect("the run advances") {
            Advanced::Moved => {
                subject.between(fx, false);
                if fix && retry_answered(&fx.lead_events()) && fx.head_subject() != "runtime: two" {
                    fx.amend_named("runtime: two");
                }
                snapshots.push(snapshot(fx));
            }
            Advanced::WaitingHuman { gate } => {
                assert_eq!(gate, "checks_failed", "the only gate this run raises");
                snapshots.push(snapshot(fx));
                let answer = if retry_answered(&fx.lead_events()) {
                    aigentic_log::CheckpointAnswer::Stop
                } else {
                    aigentic_log::CheckpointAnswer::Retry
                };
                runner
                    .answer("checks_failed", answer, None, steve())
                    .expect("the gate takes the answer it offered");
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

/// The gate offers `retry` beside `stop`, and nothing else about its ask
/// moved.
#[tokio::test]
async fn a_checks_failed_gate_offers_retry_and_stop() {
    let h = retryable();
    let fx = &h.fx;
    let run = drive_a_wrong_subject(fx, h.implementer_child, false).await;
    assert_eq!(
        run.terminal,
        Advanced::WaitingHuman {
            gate: "checks_failed".to_owned()
        }
    );

    let events = run.lead();
    let asked = checkpoints(events).pop().expect("the gate was asked");
    assert_eq!(asked.gate, "checks_failed");
    assert_eq!(
        asked.options,
        vec!["retry", "stop"],
        "both answers, in order"
    );

    let failing = checks_run(events)
        .into_iter()
        .rfind(|run| {
            run.checks
                .iter()
                .any(|check| check.result == aigentic_log::CheckResult::Fail)
        })
        .expect("a run of the checks failed");
    assert_eq!(asked.shown[0], failing.step, "the gate names the step");
    let attempt = step_started(events)
        .into_iter()
        .rfind(|started| started.step == failing.step)
        .expect("the failing step started")
        .attempt;
    assert_eq!(
        asked.shown[1],
        attempt.to_string(),
        "and the attempt that failed"
    );
    for check in failing
        .checks
        .iter()
        .filter(|check| check.result == aigentic_log::CheckResult::Fail)
    {
        assert!(
            asked.shown.join(" ").contains(&check.id),
            "the gate names {}: {:?}",
            check.id,
            asked.shown
        );
    }
}

/// The retry runs one more attempt in the step's own child, is told
/// the checks that failed it, and the gate asks again when it fails too.
#[tokio::test]
async fn a_retry_runs_one_more_attempt_in_the_same_child_with_the_failures() {
    let h = retryable_twice();
    let fx = &h.fx;
    let run = drive_a_retry(fx, h.implementer_child, false).await;
    assert_eq!(
        run.terminal,
        Advanced::Finished {
            outcome: aigentic_log::RunOutcome::Stopped
        },
        "the second gate's stop ends the run"
    );

    let events = run.lead();
    let answers: Vec<aigentic_log::CheckpointAnswer> = events
        .iter()
        .filter(|event| event.kind == EventKind::CheckpointAnswered)
        .map(|event| {
            serde_json::from_value::<aigentic_log::CheckpointAnsweredPayload>(event.payload.clone())
                .expect("a checkpoint_answered payload")
                .answer
        })
        .collect();
    assert_eq!(
        answers,
        vec![
            aigentic_log::CheckpointAnswer::Retry,
            aigentic_log::CheckpointAnswer::Stop
        ]
    );

    let answered_at = events
        .iter()
        .position(|event| event.kind == EventKind::CheckpointAnswered)
        .expect("the gate was answered");
    let started_after = step_started(&events[answered_at..]);
    assert_eq!(started_after.len(), 1, "one attempt follows the answer");
    let retried = started_after.into_iter().next().expect("an attempt");
    let failing = step_started(&events[..answered_at])
        .into_iter()
        .rfind(|started| started.step == retried.step)
        .expect("the failing step started");
    assert_eq!(retried.attempt, failing.attempt + 1, "one attempt more");
    assert_eq!(
        retried.child_thread, failing.child_thread,
        "in the step's own child"
    );

    // The retried attempt is told which checks failed it, with their lines.
    let failed_run = checks_run(&events[..answered_at])
        .into_iter()
        .rfind(|run| {
            run.checks
                .iter()
                .any(|check| check.result == aigentic_log::CheckResult::Fail)
        })
        .expect("a run of the checks failed");
    let prompts = child_prompts(fx, h.implementer_child);
    assert_eq!(prompts.len(), 3, "one prompt per attempt");
    let retry_prompt = &prompts[2];
    for check in failed_run
        .checks
        .iter()
        .filter(|check| check.result == aigentic_log::CheckResult::Fail)
    {
        assert!(
            retry_prompt.contains(&check.id),
            "the retried attempt is told about {}: {retry_prompt}",
            check.id
        );
        if let Some(detail) = &check.detail {
            assert!(
                retry_prompt.contains(detail),
                "and the line it failed with: {retry_prompt}"
            );
        }
    }

    let asked = checkpoints(events);
    assert_eq!(asked.len(), 2, "the retried attempt failed too");
    assert_eq!(asked[1].gate, "checks_failed");
    assert_eq!(asked[1].options, vec!["retry", "stop"]);
    assert!(pushed_events(events).is_empty(), "nothing is pushed");
    assert_eq!(fx.remote_head(), fx.initial_head());
    assert_eq!(fx.installer.calls(), 0, "and nothing is installed");
}

/// A rebuild from just after the retry answer makes that attempt's
/// move once, in the same child, and never twice.
#[tokio::test]
async fn a_rebuild_after_the_retry_answer_starts_that_attempt_once() {
    let h = retryable_twice();
    let fx = &h.fx;
    let run = drive_a_retry(fx, h.implementer_child, false).await;
    let snapshot = run.after_where(EventKind::CheckpointAnswered, 1);
    let answered_at = run
        .lead()
        .iter()
        .position(|event| event.kind == EventKind::CheckpointAnswered)
        .expect("the gate was answered");
    let expected = &run.lead()[answered_at + 1];
    assert_eq!(expected.kind, EventKind::StepStarted, "the retry's move");

    let (advanced, _) = replay(fx, snapshot).await;
    assert_eq!(advanced, Advanced::Moved, "the move was due");
    let after = fx.lead_events();
    assert_eq!(
        after.len(),
        snapshot.lead.len() + 1,
        "exactly one lead event appended"
    );
    assert_eq!(after.last().expect("an event").kind, expected.kind);
    assert_eq!(
        same_move(after.last().expect("an event")),
        same_move(expected),
        "the same step_started"
    );
    assert_eq!(
        after
            .iter()
            .filter(|event| event.kind == EventKind::CheckpointAnswered)
            .count(),
        1,
        "no second answer is written"
    );

    let started = step_started(&after);
    let retried = started.last().expect("the retried attempt started");
    let same_step: Vec<_> = started
        .iter()
        .filter(|started| started.step == retried.step)
        .collect();
    assert_eq!(
        retried.attempt,
        same_step[1].attempt + 1,
        "one attempt more"
    );
    assert_eq!(
        retried.child_thread, same_step[1].child_thread,
        "in the step's own child"
    );
    let children: std::collections::BTreeSet<Ulid> = same_step
        .iter()
        .map(|started| started.child_thread)
        .collect();
    assert_eq!(children.len(), 1, "no second child thread for the step");

    let (again, _) = replay(fx, snapshot).await;
    assert_eq!(again, Advanced::Moved);
    let after_two = fx.lead_events();
    assert_eq!(after_two.len(), after.len(), "the same number of events");
    assert_eq!(
        same_move(after_two.last().expect("an event")),
        same_move(after.last().expect("an event")),
        "the second rebuild appends the identical event"
    );
}

/// The retried attempt's checks pass, so it pushes the amended commit
/// and the run installs and closes.
#[tokio::test]
async fn a_retried_attempt_that_passes_pushes_and_closes() {
    let h = retryable_twice();
    let fx = &h.fx;
    let run = drive_a_retry(fx, h.implementer_child, true).await;
    assert_eq!(
        run.terminal,
        Advanced::Finished {
            outcome: aigentic_log::RunOutcome::Closed
        },
        "the retried attempt closes the run"
    );

    let events = run.lead();
    let asked = checkpoints(events);
    assert_eq!(asked.len(), 1, "the retried attempt passes, so one gate");
    assert_eq!(asked[0].gate, "checks_failed");
    assert_eq!(asked[0].options, vec!["retry", "stop"]);

    let checks = checks_run(events);
    assert_eq!(checks.len(), 3, "each attempt runs the checks");
    assert!(
        checks[..2].iter().all(|run| run
            .checks
            .iter()
            .any(|check| check.result == aigentic_log::CheckResult::Fail)),
        "the first attempt and the send-back fail"
    );
    assert!(
        checks[2]
            .checks
            .iter()
            .all(|check| check.result == aigentic_log::CheckResult::Pass),
        "the amended subject passes every check"
    );

    let pushed = pushed_events(events);
    assert_eq!(pushed.len(), 1, "one push, after the checks passed");
    assert_eq!(pushed[0].ref_after, fx.head());
    assert_eq!(
        pushed[0]
            .commits
            .iter()
            .map(|commit| commit.subject.as_str())
            .collect::<Vec<_>>(),
        vec!["runtime: two", "runtime: one"],
        "the amended commit and its parent"
    );
    assert_eq!(fx.remote_head(), fx.head());
    assert_eq!(fx.head_subject(), "runtime: two");
    assert!(fx.forge.is_closed(), "the issue is closed");
    assert_eq!(
        run_finished(events).outcome,
        aigentic_log::RunOutcome::Closed
    );
}

// ---------------------------------------------------------------------------
// The budget gate: a run stops at its budget and asks before spending more.
// ---------------------------------------------------------------------------

/// The figure the fixture's brief proposes and its route records: the
/// workflow's trivial figure.
const BUDGET_TRIVIAL: f64 = 3.0;

/// The outer figure the fixture's workflow allows.
const BUDGET_FULL: f64 = 10.0;

/// How far above `full` a brief may propose.
const BUDGET_MAX_RAISE: f64 = 2.0;

/// The figure whose ask line a priced reply lands on exactly: `0.8 ×` this
/// figure is $1.60, a whole number of the input tokens the fixture's
/// replies are priced from, so a run's spend can reach the line itself and
/// not only cross it.
const AT_LINE_FIGURE: f64 = 2.0;

/// The workflow the budget tests run: a brief that routes a trivial issue
/// straight to a writing step, with the shipped `[budget]` shape. Its
/// figures are the constants above, so a test cannot drift from it.
fn budget_workflow() -> String {
    format!(
        r####"
name = "budget"
version = 1

[budget]
trivial = {BUDGET_TRIVIAL:.1}
full = {BUDGET_FULL:.1}
max_raise = {BUDGET_MAX_RAISE:.1}

[[steps]]
id = "brief"
role = "brief"
profile = "flash"
template = "templates/step.md"
marker = "## Brief"
route_by = "size"
routes = {{ trivial = "implement-alone" }}
budget = {BUDGET_TRIVIAL:.1}
wall_secs = 900

[[steps]]
id = "implement-alone"
role = "implementer"
profile = "flash"
template = "templates/step.md"
marker = "## Implementation"
next = "done"
budget = {BUDGET_TRIVIAL:.1}
wall_secs = 3600
writes = true
push = false
"####
    )
}

/// The figure in force after the brief's route: what the run_started
/// carries and what the route records.
fn budget_figure() -> f64 {
    BUDGET_TRIVIAL
}

/// The most a brief may propose without an ask: `full × max_raise`.
fn budget_ceiling() -> f64 {
    BUDGET_FULL * BUDGET_MAX_RAISE
}

/// The figure the tests answer the gate with: clear of the line for a run
/// that has spent `budget_figure()`, so the run continues rather than
/// parking again.
fn clearing_raise() -> f64 {
    budget_figure() * 4.0
}

/// The smallest whole number of cents that clears the budget gate for a run
/// that has spent `cost` against `budget`: the guard's own comparison, walked
/// from one cent up rather than taken from the figure the runner names.
fn least_clearing_figure(cost: f64, budget: f64) -> f64 {
    for cents in 1..=1_000_000 {
        let usd = f64::from(cents) / 100.0;
        if usd > budget && cost < BUDGET_ASK_AT * usd {
            return usd;
        }
    }
    panic!("no figure under 10,000 clears {cost} spent against {budget}");
}

/// The price table the fixture's replies are priced by.
fn budget_prices() -> Prices {
    Prices {
        input: 10.0,
        cache_read: 1.0,
        cache_write: 12.5,
        output: 30.0,
    }
}

/// One scripted reply that reports `args` and costs `usd` to make. The
/// fixture rates a reply on its tokens, so the tokens come from the figure
/// the test wants spent.
fn priced_reply(args: Value, usd: f64) -> Vec<ProviderEvent> {
    let input_tokens = (usd * 1_000_000.0 / budget_prices().input).round() as u64;
    vec![
        finish("done", args),
        usage(input_tokens, 0),
        done("tool_use"),
    ]
}

/// The brief's report with the figure it proposes for the run.
fn brief_report_proposing(usd: f64) -> Value {
    let mut report = brief_report();
    report["slots"]["budget"] = json!(usd);
    report
}

/// The fixture the budget tests run, with the writing child's id: the
/// test that watches a mid-step rebuild needs it.
struct BudgetFixture {
    fx: Fixture,
    write: Ulid,
}

/// A brief whose reply costs `budget_figure()` and proposes `proposal`,
/// and a writing step whose reply costs the same: the run crosses the ask
/// line at the brief's end, and not again.
fn budget_fixture(proposal: f64) -> BudgetFixture {
    budget_fixture_spending(budget_figure(), proposal)
}

/// A brief whose reply costs `spend` and proposes `proposal`, and a
/// writing step whose reply costs `budget_figure()`: the test that weighs
/// the guard at the ask line picks the brief's spend, the one figure a run
/// reaches the line with exactly.
fn budget_fixture_spending(spend: f64, proposal: f64) -> BudgetFixture {
    let brief = Ulid::generate();
    let write = Ulid::generate();
    let workflow = budget_workflow();
    let fx = Fixture::with_workflow(
        vec![
            (
                brief,
                vec![priced_reply(brief_report_proposing(proposal), spend)],
            ),
            (
                write,
                vec![priced_reply(implementer_report(), budget_figure())],
            ),
        ],
        &[
            ("workflow.toml", workflow.as_str()),
            ("templates/step.md", "do the step\n"),
        ],
    )
    .priced(budget_prices());
    BudgetFixture { fx, write }
}

/// The world after every lead event of a run that met the budget gate.
struct Raised {
    run: Trace,
    /// The 1-based place of the answer, and of the raise it took.
    answered_at: usize,
    raised_at: usize,
}

/// Snapshot the world once per lead event a move has written. A move that
/// parks writes the route and the ask, and a rebuild from between the two
/// is what the replay tests ask for; nothing else a park does moves the
/// repository, a child or the forge.
fn push_snapshots(fx: &Fixture, snapshots: &mut Vec<Snapshot>) {
    let written = fx.lead_events().len();
    while snapshots.len() < written {
        let mut one = snapshot(fx);
        one.lead.truncate(snapshots.len() + 1);
        snapshots.push(one);
    }
}

/// Drive the fixture to its end, answering the budget gate with `raise`
/// and `figure` when it comes, and snapshotting the world after every
/// event. Answering the gate is what a person does in a live run.
async fn drive_and_raise(fx: &Fixture, figure: f64) -> Raised {
    let mut runner = fx.runner();
    let mut snapshots = vec![snapshot(fx)];
    let mut answered_at = 0;
    let mut raised_at = 0;
    let terminal = loop {
        let advanced = runner.advance().await.expect("the run advances");
        push_snapshots(fx, &mut snapshots);
        match advanced {
            Advanced::Moved => {
                let events = fx.lead_events();
                if answered_at == 0
                    && let Some(at) = events_at(&events, EventKind::CheckpointAnswered).first()
                {
                    answered_at = *at;
                }
                if let Some(at) = events_at(&events, EventKind::BudgetRaised).first() {
                    raised_at = *at;
                }
            }
            Advanced::WaitingHuman { gate } if gate == BUDGET_GATE => {
                assert_eq!(answered_at, 0, "the run parks at the budget gate once");
                runner
                    .answer(
                        BUDGET_GATE,
                        CheckpointAnswer::Raise,
                        Some(format!("{figure:.2}")),
                        steve(),
                    )
                    .expect("the raise clears the guard");
                push_snapshots(fx, &mut snapshots);
                answered_at = fx.lead_events().len();
            }
            other => break other,
        }
    };
    push_snapshots(fx, &mut snapshots);
    Raised {
        run: Trace {
            snapshots,
            terminal,
        },
        answered_at,
        raised_at,
    }
}

/// The 1-based places of every lead event of `kind`.
fn events_at(events: &[Event], kind: EventKind) -> Vec<usize> {
    events
        .iter()
        .enumerate()
        .filter(|(_, event)| event.kind == kind)
        .map(|(i, _)| i + 1)
        .collect()
}

/// Rebuild from `snapshot` with its lead events edited, then do one move:
/// a log an older runner could have written, seen by the guard of this one.
/// An edit that drops events renumbers what is left: the log's sequence is
/// contiguous, and a gap is refused on open.
async fn replay_edited(
    fx: &Fixture,
    snapshot: &Snapshot,
    edit: impl FnOnce(&mut Vec<Event>),
) -> (Advanced, TestRunner) {
    fx.restore(snapshot);
    let mut events = snapshot.lead.clone();
    edit(&mut events);
    for (i, event) in events.iter_mut().enumerate() {
        event.seq = i as u64;
    }
    write_events(&fx.dir.path().join(format!("{}.jsonl", fx.lead)), &events);
    let mut runner = fx.runner();
    let advanced = runner.advance().await.expect("the rebuilt runner advances");
    (advanced, runner)
}

/// A run that has spent the ask line of its figure parks before the
/// next step, and the ask names the spend and the figure.
#[tokio::test]
async fn a_run_at_eighty_percent_of_its_budget_parks_before_the_next_step() {
    let fx = budget_fixture(budget_figure()).fx;
    let mut runner = fx.runner();
    assert_eq!(
        drive(&fx, &mut runner).await,
        Advanced::WaitingHuman {
            gate: BUDGET_GATE.to_owned()
        },
        "the run stops instead of starting the next step"
    );
    let events = fx.lead_events();
    let state = run_state(&events).expect("the log derives a run");
    assert_eq!(
        state.cost_usd,
        budget_figure(),
        "the priced reply is the spend the guard weighs"
    );
    assert_eq!(
        gate(&events).as_deref(),
        Some(BUDGET_GATE),
        "the ask is the budget gate"
    );
    assert_eq!(
        step_started(&events).len(),
        1,
        "and the next step never started"
    );
    let asks = checkpoints(&events);
    assert_eq!(asks.len(), 1, "one ask");
    assert_eq!(
        asks[0].options,
        vec!["raise".to_owned(), "stop".to_owned()],
        "the ask offers a raise and a stop"
    );
    let spent = format!("{:.2}", state.cost_usd);
    let figure = format!("{:.2}", state.budget_usd);
    let shown = &asks[0].shown;
    assert!(
        shown.iter().any(|line| line.contains(&spent)),
        "the ask says what the run spent: {shown:?}"
    );
    assert!(
        shown.iter().any(|line| line.contains(&figure)),
        "and the figure in force: {shown:?}"
    );
}

/// The budget ask says where to raise to and how, beside the spend it
/// already states: the smallest figure the guard takes and the command that
/// answers with it.
#[tokio::test]
async fn the_budget_ask_names_the_smallest_figure_that_clears_it() {
    let fx = budget_fixture(budget_figure()).fx;
    let mut runner = fx.runner();
    assert_eq!(
        drive(&fx, &mut runner).await,
        Advanced::WaitingHuman {
            gate: BUDGET_GATE.to_owned()
        },
        "the run parks at the budget gate"
    );
    let events = fx.lead_events();
    let state = run_state(&events).expect("the log derives a run");
    let least = least_clearing_figure(state.cost_usd, state.budget_usd);
    let asks = checkpoints(&events);
    assert_eq!(asks.len(), 1, "one ask");
    assert_eq!(
        asks[0].raise_usd,
        Some(least),
        "the ask carries the figure that clears the gate"
    );
    assert_eq!(
        asks[0].shown.len(),
        2,
        "the spend's line and the raise's, nothing replaced: {:?}",
        asks[0].shown
    );
    let figure = format!("{least:.2}");
    assert!(
        asks[0]
            .shown
            .iter()
            .any(|line| line.contains(&figure) && line.contains("/answer raise")),
        "one line names the figure and the command: {:?}",
        asks[0].shown
    );
}

/// The figure the ask names is one the gate takes: answering with it clears
/// the line, records the raise and starts the next step.
#[tokio::test]
async fn the_figure_the_ask_names_is_the_one_the_gate_accepts() {
    let fx = budget_fixture(budget_figure()).fx;
    let mut runner = fx.runner();
    assert_eq!(
        drive(&fx, &mut runner).await,
        Advanced::WaitingHuman {
            gate: BUDGET_GATE.to_owned()
        },
        "the run parks at the budget gate"
    );
    let asks = checkpoints(&fx.lead_events());
    let figure = format!("{:.2}", asks[0].raise_usd.expect("the ask names a figure"));
    runner
        .answer(
            BUDGET_GATE,
            CheckpointAnswer::Raise,
            Some(figure.clone()),
            steve(),
        )
        .expect("the figure the ask names clears the gate");
    assert_eq!(
        runner.advance().await.expect("the answer is taken"),
        Advanced::Moved,
        "the raise is recorded"
    );
    let events = fx.lead_events();
    let raised: Vec<BudgetRaisedPayload> = events
        .iter()
        .filter(|event| event.kind == EventKind::BudgetRaised)
        .filter_map(|event| serde_json::from_value(event.payload.clone()).ok())
        .collect();
    assert_eq!(
        raised.len(),
        1,
        "one raise, carrying the figure: {raised:?}"
    );
    assert_eq!(
        raised[0].usd,
        figure.parse::<f64>().expect("the figure is a number"),
        "the figure the answer sent"
    );
    assert_eq!(
        run_state(&events)
            .expect("the log derives a run")
            .budget_usd,
        raised[0].usd,
        "and the fold takes it"
    );
    assert_eq!(
        runner.advance().await.expect("the run goes on"),
        Advanced::Moved,
        "the writing step runs"
    );
    assert_eq!(
        events_at(&fx.lead_events(), EventKind::StepStarted).len(),
        2,
        "the brief and the writing step"
    );
}

/// The figure is read off the log, so a rebuilt run asks the same thing
/// again: the same figure, in the same words.
#[tokio::test]
async fn a_rebuild_shows_the_same_figure() {
    let fx = budget_fixture(budget_figure()).fx;
    let mut runner = fx.runner();
    let mut snapshots = vec![snapshot(&fx)];
    let terminal = loop {
        let advanced = runner.advance().await.expect("the run advances");
        if advanced != Advanced::Moved {
            break advanced;
        }
        push_snapshots(&fx, &mut snapshots);
    };
    assert_eq!(
        terminal,
        Advanced::WaitingHuman {
            gate: BUDGET_GATE.to_owned()
        },
        "the run parks at the budget gate"
    );
    let asked = checkpoints(&fx.lead_events());
    assert_eq!(asked.len(), 1, "one ask");
    // The world before the ask: the rebuilt run writes it again from the
    // same events, so the figure is the log's, not the first writer's.
    fx.restore(snapshots.last().expect("the run moved at least once"));
    let mut rebuilt = fx.runner();
    assert_eq!(
        rebuilt.advance().await.expect("the rebuilt run advances"),
        Advanced::WaitingHuman {
            gate: BUDGET_GATE.to_owned()
        },
        "and parks at the same gate"
    );
    let again = checkpoints(&fx.lead_events());
    assert_eq!(again.len(), 1, "one ask, written again");
    assert_eq!(again[0].raise_usd, asked[0].raise_usd, "the same figure");
    assert_eq!(again[0].shown, asked[0].shown, "and the same words");
}

/// A run whose spend has reached the ask line exactly parks before the next
/// step: the guard asks at the line itself, not only past it.
#[tokio::test]
async fn a_run_exactly_at_the_ask_line_parks_before_the_next_step() {
    let fx = budget_fixture_spending(BUDGET_ASK_AT * AT_LINE_FIGURE, AT_LINE_FIGURE).fx;
    let mut runner = fx.runner();
    assert_eq!(
        drive(&fx, &mut runner).await,
        Advanced::WaitingHuman {
            gate: BUDGET_GATE.to_owned()
        },
        "the run stops instead of starting the next step"
    );
    let events = fx.lead_events();
    let state = run_state(&events).expect("the log derives a run");
    assert_eq!(
        state.budget_usd, AT_LINE_FIGURE,
        "the brief's proposal is the figure in force"
    );
    assert_eq!(
        state.cost_usd,
        BUDGET_ASK_AT * state.budget_usd,
        "the spend the guard weighs sits on the ask line, not past it"
    );
    assert_eq!(
        step_started(&events).len(),
        1,
        "and no step starts behind the ask"
    );
    assert_eq!(
        gate(&events).as_deref(),
        Some(BUDGET_GATE),
        "the ask is the budget gate"
    );
}

/// A second `advance()` while parked starts nothing and stacks no ask.
#[tokio::test]
async fn a_parked_budget_gate_starts_nothing_and_does_not_stack_asks() {
    let fx = budget_fixture(budget_figure()).fx;
    let mut runner = fx.runner();
    assert_eq!(
        drive(&fx, &mut runner).await,
        Advanced::WaitingHuman {
            gate: BUDGET_GATE.to_owned()
        },
        "the run parks"
    );
    let parked = fx.lead_events();
    assert_eq!(
        runner
            .advance()
            .await
            .expect("a parked run advances nothing"),
        Advanced::WaitingHuman {
            gate: BUDGET_GATE.to_owned()
        },
        "the run is still waiting"
    );
    assert_eq!(fx.lead_events().len(), parked.len(), "and writes nothing");
    assert_eq!(checkpoints(&fx.lead_events()).len(), 1, "one ask, not two");
    assert_eq!(
        step_started(&fx.lead_events()).len(),
        1,
        "and no child starts behind the ask"
    );
}

/// An answered `raise` sends the run on under the new figure.
#[tokio::test]
async fn an_answered_raise_continues_the_run_under_the_new_figure() {
    let fx = budget_fixture(budget_figure()).fx;
    let figure = clearing_raise();
    let raised = drive_and_raise(&fx, figure).await;
    assert_eq!(
        raised.run.terminal,
        Advanced::Finished {
            outcome: aigentic_log::RunOutcome::Closed
        },
        "the run goes on to its end"
    );
    let events = raised.run.lead();
    assert!(
        raised.answered_at < raised.raised_at,
        "the answer comes before the raise"
    );
    let answer: aigentic_log::CheckpointAnsweredPayload =
        serde_json::from_value(events[raised.answered_at - 1].payload.clone())
            .expect("the answer carries its word");
    assert_eq!(answer.answer, CheckpointAnswer::Raise);
    let raise = &events[raised.raised_at - 1];
    assert_eq!(
        raise.kind,
        EventKind::BudgetRaised,
        "the raise is its own event"
    );
    let payload: BudgetRaisedPayload =
        serde_json::from_value(raise.payload.clone()).expect("the raise carries its figure");
    assert_eq!(payload.usd, figure, "the figure the answer offered");
    assert_eq!(
        run_state(events).expect("the log derives a run").budget_usd,
        payload.usd,
        "and the fold takes it"
    );
    let starts = events_at(events, EventKind::StepStarted);
    assert_eq!(starts.len(), 2, "the brief and the writing step");
    assert!(
        raised.raised_at < starts[1],
        "the writing step starts after the raise"
    );
}

/// A rebuild over the same log neither parks again nor repeats a move.
#[tokio::test]
async fn a_raised_budget_survives_a_rebuild() {
    let fx = budget_fixture(budget_figure()).fx;
    let raised = drive_and_raise(&fx, clearing_raise()).await;
    let figure = &raised.run.lead()[raised.raised_at - 1].payload["usd"];
    let snapshot = raised.run.after(raised.raised_at);
    let before = snapshot.lead.len();
    let (advanced, _runner) = replay(&fx, snapshot).await;
    assert_eq!(
        advanced,
        Advanced::Moved,
        "the rebuild continues instead of parking again"
    );
    let after = fx.lead_events();
    assert_eq!(after.len(), before + 1, "the rebuild does one move");
    assert_eq!(checkpoints(&after).len(), 1, "and asks no second time");
    assert_eq!(
        run_state(&after).expect("the log derives a run").budget_usd,
        figure.as_f64().expect("the raise's figure"),
        "the figure the raise wrote is still in force"
    );
}

/// `stop` at the budget gate ends the run as stopped.
#[tokio::test]
async fn a_stop_at_the_budget_gate_ends_the_run_as_stopped() {
    let fx = budget_fixture(budget_figure()).fx;
    let mut runner = fx.runner();
    assert_eq!(
        drive(&fx, &mut runner).await,
        Advanced::WaitingHuman {
            gate: BUDGET_GATE.to_owned()
        },
        "the run parks"
    );
    runner
        .answer(BUDGET_GATE, CheckpointAnswer::Stop, None, steve())
        .expect("stop is always offered");
    assert_eq!(
        drive(&fx, &mut runner).await,
        Advanced::Finished {
            outcome: aigentic_log::RunOutcome::Stopped
        },
        "the answer ends the run"
    );
    let events = fx.lead_events();
    assert_eq!(step_started(&events).len(), 1, "and starts no further step");
    assert_eq!(checkpoints(&events).len(), 1, "and asks no second time");
}

/// A run whose figure is zero is not asked, and starts its next step
/// whatever it has spent.
#[tokio::test]
async fn a_run_with_no_budget_still_starts_its_next_step() {
    let fx = budget_fixture(budget_figure()).fx;
    let run = trace(&fx).await;
    let ask = events_at(run.lead(), EventKind::CheckpointAsked)[0];
    // A log an older runner could have written: the workflow's zero figure,
    // no proposal taken, and no ask at all.
    let snapshot = run.after(ask - 1);
    let (advanced, _runner) = replay_edited(&fx, snapshot, |events| {
        let started = events
            .iter_mut()
            .find(|event| event.kind == EventKind::RunStarted)
            .expect("the run's first event");
        started.payload["budget_usd"] = json!(0.0);
        let route = events
            .iter_mut()
            .find(|event| event.kind == EventKind::RouteTaken)
            .expect("the brief's route");
        route.payload["budget_usd"] = Value::Null;
    })
    .await;
    assert_eq!(
        advanced,
        Advanced::Moved,
        "a zero figure parks nothing, so the writing step starts"
    );
    let after = fx.lead_events();
    assert_eq!(step_started(&after).len(), 2, "the writing step started");
    assert_eq!(checkpoints(&after).len(), 0, "and no gate was asked");
    assert_eq!(
        run_state(&after).expect("the log derives a run").budget_usd,
        0.0,
        "the figure in force is the zero the log records"
    );
}

/// A brief that proposes above the ceiling parks at the route, with the
/// figure in force left alone.
#[tokio::test]
async fn a_brief_that_proposes_above_the_ceiling_parks_at_the_route() {
    let proposal = budget_ceiling() + 1.0;
    let fx = budget_fixture(proposal).fx;
    let mut runner = fx.runner();
    assert_eq!(
        drive(&fx, &mut runner).await,
        Advanced::WaitingHuman {
            gate: BUDGET_GATE.to_owned()
        },
        "the run parks rather than taking the proposal"
    );
    let events = fx.lead_events();
    let route = routes(&events).pop().expect("the route is recorded");
    assert_eq!(
        route.budget_usd,
        Some(budget_figure()),
        "the route leaves the figure in force"
    );
    assert_eq!(
        run_state(&events)
            .expect("the log derives a run")
            .budget_usd,
        budget_figure(),
        "which is what the run is spending against"
    );
    assert_eq!(step_started(&events).len(), 1, "and no step starts on it");
    let asks = checkpoints(&events);
    assert_eq!(asks.len(), 1, "one ask");
    let shown = &asks[0].shown;
    assert!(
        shown
            .iter()
            .any(|line| line.contains(&format!("{proposal:.2}"))),
        "the ask names the proposal: {shown:?}"
    );
    assert!(
        shown
            .iter()
            .any(|line| line.contains(&format!("{:.2}", budget_ceiling()))),
        "and the ceiling it went over: {shown:?}"
    );
}

/// The ask a brief's own proposal opens carries the figure too: every
/// budget ask is written by the same path.
#[tokio::test]
async fn the_briefs_proposal_ask_carries_the_figure_too() {
    let proposal = budget_ceiling() + 1.0;
    let fx = budget_fixture(proposal).fx;
    let mut runner = fx.runner();
    assert_eq!(
        drive(&fx, &mut runner).await,
        Advanced::WaitingHuman {
            gate: BUDGET_GATE.to_owned()
        },
        "the run parks at the route"
    );
    let events = fx.lead_events();
    let state = run_state(&events).expect("the log derives a run");
    let asks = checkpoints(&events);
    assert_eq!(asks.len(), 1, "one ask");
    assert_eq!(
        asks[0].raise_usd,
        Some(least_clearing_figure(state.cost_usd, state.budget_usd)),
        "the proposal's ask names the figure that clears the gate"
    );
    assert!(
        asks[0]
            .shown
            .iter()
            .any(|line| line.contains(&format!("{proposal:.2}"))),
        "and still names the proposal: {:?}",
        asks[0].shown
    );
}

/// A proposal at the ceiling exactly is the run's figure, with no ask.
#[tokio::test]
async fn a_brief_that_proposes_at_the_ceiling_is_taken_without_asking() {
    let proposal = budget_ceiling();
    let fx = budget_fixture(proposal).fx;
    let run = trace(&fx).await;
    let events = run.lead();
    assert_eq!(
        run.terminal,
        Advanced::Finished {
            outcome: aigentic_log::RunOutcome::Closed
        },
        "the run goes on to its end"
    );
    assert_eq!(
        checkpoints(events).len(),
        0,
        "the boundary is not over the line"
    );
    assert_eq!(
        routes(events).pop().expect("the route").budget_usd,
        Some(proposal),
        "the proposal at the ceiling is the run's figure"
    );
    assert_eq!(
        run_state(events).expect("the log derives a run").budget_usd,
        proposal
    );
}

/// A raise that does not clear the gate is refused and writes nothing,
/// and the run stays parked.
#[tokio::test]
async fn a_raise_that_does_not_clear_the_gate_is_refused_without_writing() {
    let fx = budget_fixture(budget_figure()).fx;
    let mut runner = fx.runner();
    assert_eq!(
        drive(&fx, &mut runner).await,
        Advanced::WaitingHuman {
            gate: BUDGET_GATE.to_owned()
        },
        "the run parks"
    );
    let parked = fx.lead_events();
    let state = run_state(&parked).expect("the log derives a run");
    let quotient = state.cost_usd / BUDGET_ASK_AT;
    let least = format!("{}", quotient);
    let figure = least_clearing_figure(state.cost_usd, state.budget_usd);
    let current = format!("{:.2}", state.budget_usd);
    let offers = vec![
        ("an unparseable figure", "a lot".to_owned()),
        ("a figure not above the one in force", current),
        ("exactly the ask point", least),
    ];
    for (why, offer) in offers {
        let refused = runner
            .answer(
                BUDGET_GATE,
                CheckpointAnswer::Raise,
                Some(offer.clone()),
                steve(),
            )
            .expect_err("the answer is refused");
        match refused {
            RunnerError::RaiseRefused {
                offered,
                budget,
                spent,
                least: told,
            } => {
                assert_eq!(offered, offer, "{why}: the refusal names the offer");
                assert_eq!(budget, state.budget_usd, "{why}: and the figure in force");
                assert_eq!(spent, state.cost_usd, "{why}: and what was spent");
                assert_eq!(
                    told, figure,
                    "{why}: and the smallest figure that clears the line"
                );
            }
            other => panic!("{why}: the refusal is the raise's, not {other:?}"),
        }
        assert_eq!(
            fx.lead_events().len(),
            parked.len(),
            "{why}: the refusal writes nothing"
        );
        assert_eq!(
            runner.advance().await.expect("the run is still parked"),
            Advanced::WaitingHuman {
                gate: BUDGET_GATE.to_owned()
            },
            "{why}: so the run stays parked and may be answered again"
        );
    }
    // The figure the refusal points at is the one that clears: the smallest
    // whole cent the guard's strict test accepts.
    runner
        .answer(
            BUDGET_GATE,
            CheckpointAnswer::Raise,
            Some(format!("{figure:.2}")),
            steve(),
        )
        .expect("the smallest figure that clears the line clears it");
}

/// A rebuild from every prefix of a raised run does each move once and
/// leaves the same log.
#[tokio::test]
async fn a_rebuild_after_every_lead_event_does_each_move_once_at_the_budget_gate() {
    let fx = budget_fixture(budget_figure()).fx;
    let raised = drive_and_raise(&fx, clearing_raise()).await;
    let full = raised.run.lead().to_vec();
    assert!(full.len() >= 5, "the run is several moves long");
    for k in 1..=full.len() {
        fx.restore(raised.run.after(k));
        let rebuilt = drive_and_raise(&fx, clearing_raise()).await;
        assert_eq!(
            rebuilt.run.terminal, raised.run.terminal,
            "prefix {k}: the run ends the same way"
        );
        let after = fx.lead_events();
        assert_eq!(
            after.len(),
            full.len(),
            "prefix {k}: the same number of events"
        );
        for (i, (one, other)) in after.iter().zip(&full).enumerate() {
            assert_eq!(one.kind, other.kind, "prefix {k}: event {i}'s kind");
            assert_eq!(
                same_move(one),
                same_move(other),
                "prefix {k}: event {i}'s payload"
            );
        }
    }
}

/// A rebuild mid-step with the run over the line parks before it
/// resumes the child.
#[tokio::test]
async fn a_rebuild_mid_step_over_the_line_parks_before_resuming_the_child() {
    let budget = budget_fixture(budget_figure());
    let fx = &budget.fx;
    let raised = drive_and_raise(fx, clearing_raise()).await;
    let started = events_at(raised.run.lead(), EventKind::StepStarted);
    let mid_step = *started.last().expect("the writing step started");
    // The log an older runner could have left: no gate at all, and a step
    // that crossed the line this runner's guard weighs.
    let snapshot = raised.run.after(mid_step);
    let children = {
        fx.restore(snapshot);
        let events = fx.child_events_of(budget.write);
        assert!(!events.is_empty(), "the child had begun its turn");
        events.len()
    };
    let (advanced, _runner) = replay_edited(fx, snapshot, |events| {
        events.retain(|event| {
            !matches!(
                event.kind,
                EventKind::CheckpointAsked
                    | EventKind::CheckpointAnswered
                    | EventKind::BudgetRaised
            )
        });
        let finished = events
            .iter_mut()
            .find(|event| event.kind == EventKind::StepFinished)
            .expect("the brief's step finished");
        finished.payload["cost_usd"] = json!(budget_figure() * 4.0);
    })
    .await;
    assert_eq!(
        advanced,
        Advanced::WaitingHuman {
            gate: BUDGET_GATE.to_owned()
        },
        "the rebuild asks before resuming the child"
    );
    let after = fx.lead_events();
    assert_eq!(checkpoints(&after).len(), 1, "once");
    assert_eq!(
        step_started(&after).len(),
        2,
        "the step it was mid-way through"
    );
    assert_eq!(
        fx.child_events_of(budget.write).len(),
        children,
        "and the child is not resumed"
    );
}

// ---------------------------------------------------------------------------
// A step's turn is capped at its own figure, or at the run's room
// ---------------------------------------------------------------------------

/// `budget_workflow` with both step figures dropped: the shape
/// `.aigentic/workflows/loop/workflow.toml` has, where a step's cap can
/// only come from the run's room.
fn budget_workflow_unfigured() -> String {
    budget_workflow().replace(&format!("budget = {BUDGET_TRIVIAL:.1}\n"), "")
}

/// One scripted reply that calls the harmless tool and costs `usd` under
/// the fixture's price table: a turn that neither reports nor stops, at a
/// price the test derives. The tokens come from the figure the test wants
/// spent, as `priced_reply`'s do.
fn priced_works(id: &str, usd: f64) -> Vec<ProviderEvent> {
    let mut reply = works(id);
    reply[1] = usage(
        (usd * 1_000_000.0 / budget_prices().input).round() as u64,
        0,
    );
    reply
}

/// The smallest price at or above `usd` that the fixture's table can
/// charge: a reply is priced from whole input tokens, so a figure between
/// two tokens' worth is charged as the one above it.
fn price_at_least(usd: f64) -> f64 {
    (usd * 1_000_000.0 / budget_prices().input).ceil() * budget_prices().input / 1_000_000.0
}

/// A brief priced at the figure, proposing a raise that clears the run's
/// ask line, and a writing child whose first call costs `spend` and whose
/// next reply is its report. `workflow` decides where the writing step's
/// cap comes from: the step's own figure, or the run's room.
fn capped_fixture(spend: f64, workflow: String) -> BudgetFixture {
    let brief = Ulid::generate();
    let write = Ulid::generate();
    let fx = Fixture::with_workflow(
        vec![
            (
                brief,
                vec![priced_reply(
                    brief_report_proposing(clearing_raise()),
                    budget_figure(),
                )],
            ),
            (
                write,
                vec![
                    priced_works("w1", spend),
                    priced_reply(implementer_report(), budget_figure()),
                ],
            ),
        ],
        &[
            ("workflow.toml", workflow.as_str()),
            ("templates/step.md", "do the step\n"),
        ],
    )
    .priced(budget_prices());
    BudgetFixture { fx, write }
}

/// Drive a fixture to its end, keeping the world after every move: a step
/// whose turn ended at its cap asks at a later move, and only the moves
/// up to the cap run the child.
async fn capped_run(fx: &Fixture) -> Trace {
    let mut runner = fx.runner();
    let mut snapshots = vec![snapshot(fx)];
    let terminal = loop {
        match runner.advance().await.expect("the run advances") {
            Advanced::Moved => snapshots.push(snapshot(fx)),
            other => break other,
        }
    };
    snapshots.push(snapshot(fx));
    Trace {
        snapshots,
        terminal,
    }
}

/// A child's `turn_ended` reasons, oldest first.
fn turn_reasons(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter(|event| event.kind == EventKind::TurnEnded)
        .map(|event| {
            let payload: aigentic_log::TurnEndedPayload =
                serde_json::from_value(event.payload.clone()).unwrap();
            payload.reason
        })
        .collect()
}

/// A child's assistant messages: the calls it made, in order.
fn calls_of(events: &[Event]) -> usize {
    events
        .iter()
        .filter(|event| event.kind == EventKind::AssistantMessage)
        .count()
}

/// A capped child's turn is `Partial` at the step, at the figure it spent:
/// the call that reached the cap ran, and the turn stopped before the
/// next one.
#[tokio::test]
async fn t4_a_child_that_spends_its_step_budget_ends_its_turn() {
    let capped = capped_fixture(budget_figure(), budget_workflow());
    let run = capped_run(&capped.fx).await;
    let child = capped.fx.child_events_of(capped.write);

    assert_eq!(
        calls_of(&child),
        1,
        "the reply that reached the cap is the child's only call"
    );
    assert_eq!(turn_reasons(&child), vec![MAX_USD.to_owned()]);

    let finished = step_finished(run.lead());
    assert_eq!(finished.len(), 2, "the brief's end and the writing step's");
    assert_eq!(finished[1].status, StepStatus::Partial);
    assert_eq!(finished[1].end_reason, MAX_USD);
    assert_eq!(finished[1].cost_usd, budget_figure());
}

/// Under the cap there is no stop: the child's call, then its report, and
/// the run reaches its end with no gate asked.
#[tokio::test]
async fn t5_a_child_under_its_step_budget_writes_no_budget_stop() {
    let capped = capped_fixture(budget_figure() / 2.0, budget_workflow());
    let run = capped_run(&capped.fx).await;

    assert_eq!(
        checkpoints(run.lead()).len(),
        0,
        "no gate is asked of a step that stays under its figure"
    );
    assert!(
        gate(run.lead()).is_none(),
        "and none is left open when the run ends"
    );
    let child = capped.fx.child_events_of(capped.write);
    assert_eq!(
        turn_reasons(&child),
        vec![STEP_REPORTED.to_owned()],
        "the child's turn ends at its report, not at a cap"
    );
    assert!(matches!(run.terminal, Advanced::Finished { .. }));
}

/// A capped step parks where it cannot be retried: `step_stop`, once, with
/// the step, its attempt and its figure named, and nothing started behind
/// it.
#[tokio::test]
async fn t6_a_capped_step_parks_at_step_stop() {
    let capped = capped_fixture(budget_figure(), budget_workflow());
    let run = capped_run(&capped.fx).await;

    assert_eq!(
        run.terminal,
        Advanced::WaitingHuman {
            gate: "step_stop".to_owned()
        }
    );
    let starts = step_started(run.lead());
    assert_eq!(starts.len(), 2, "the capped attempt is not retried");
    assert_eq!(starts[1].attempt, 1);

    let asks = checkpoints(run.lead());
    assert_eq!(asks.len(), 1, "the ask is written once");
    assert_eq!(asks[0].gate, "step_stop");
    let shown = asks[0].shown.join("\n");
    assert_eq!(asks[0].shown[0], "implement-alone", "the step: {shown}");
    assert_eq!(
        asks[0].shown[1],
        (starts[1].attempt + 1).to_string(),
        "the attempt the park blocks: {shown}"
    );
    assert!(
        shown.contains(&format!("{BUDGET_TRIVIAL:.2}")),
        "the figure: {shown}"
    );

    let child = capped.fx.child_events_of(capped.write);
    let capped_at = run.after_where(EventKind::StepFinished, 2);
    assert_eq!(
        Some(&child),
        capped_at.children.get(&capped.write),
        "the park adds nothing to the child's log"
    );
}

/// The park is derived from the log, not remembered: a new runner over the
/// same log moves the same way and writes nothing.
#[tokio::test]
async fn t7_a_rebuild_of_a_capped_step_parks_without_resuming_the_child() {
    let capped = capped_fixture(budget_figure(), budget_workflow());
    let run = capped_run(&capped.fx).await;
    assert_eq!(
        run.terminal,
        Advanced::WaitingHuman {
            gate: "step_stop".to_owned()
        }
    );

    let asked = checkpoints(&capped.fx.lead_events()).len();
    let child = capped.fx.child_events_of(capped.write);
    let mut fresh = capped.fx.runner();
    let moved = fresh.advance().await.expect("the run advances");

    assert_eq!(
        moved,
        Advanced::WaitingHuman {
            gate: "step_stop".to_owned()
        }
    );
    assert_eq!(step_started(&capped.fx.lead_events()).len(), 2);
    assert_eq!(checkpoints(&capped.fx.lead_events()).len(), asked);
    assert_eq!(capped.fx.child_events_of(capped.write), child);
}

/// A step with no figure of its own is capped by the run's room to its ask
/// line: the call that reaches the room ends the child's turn, and the
/// run's own guard asks right after.
#[tokio::test]
async fn t8_a_step_with_no_figure_is_capped_by_the_runs_room() {
    let room = BUDGET_ASK_AT * clearing_raise() - budget_figure();
    let price = price_at_least(room);
    assert!(
        price >= room,
        "the price reaches the room: a reply is priced from whole tokens"
    );
    let capped = capped_fixture(price, budget_workflow_unfigured());
    let run = capped_run(&capped.fx).await;

    let starts = step_started(run.lead());
    assert_eq!(starts.len(), 2);
    assert_eq!(starts[1].budget_usd, 0.0, "the step carries no figure");

    let child = capped.fx.child_events_of(capped.write);
    assert_eq!(
        calls_of(&child),
        1,
        "the cap stops the turn on its first call"
    );
    assert_eq!(turn_reasons(&child), vec![MAX_USD.to_owned()]);

    assert_eq!(
        run.terminal,
        Advanced::WaitingHuman {
            gate: BUDGET_GATE.to_owned()
        }
    );
    assert_eq!(checkpoints(run.lead()).len(), 1);
}

/// The room's park is the run's own gate, so a raise frees the step: the
/// retry runs, reports, and the run ends.
#[tokio::test]
async fn t9_a_raise_frees_a_step_capped_by_the_runs_room() {
    let room = BUDGET_ASK_AT * clearing_raise() - budget_figure();
    let capped = capped_fixture(price_at_least(room), budget_workflow_unfigured());
    let run = capped_run(&capped.fx).await;
    assert_eq!(
        run.terminal,
        Advanced::WaitingHuman {
            gate: BUDGET_GATE.to_owned()
        }
    );

    let mut runner = capped.fx.runner();
    runner
        .answer(
            BUDGET_GATE,
            CheckpointAnswer::Raise,
            Some(format!("{:.2}", budget_ceiling())),
            steve(),
        )
        .expect("the raise clears the gate");
    let terminal = loop {
        match runner.advance().await.expect("the run advances") {
            Advanced::Moved => {}
            other => break other,
        }
    };

    let starts = step_started(&capped.fx.lead_events());
    assert_eq!(starts.len(), 3, "the freed attempt starts");
    assert_eq!(starts[2].attempt, 2);
    let finished = step_finished(&capped.fx.lead_events());
    assert_eq!(finished.len(), 3, "and finishes");
    assert_ne!(
        finished[2].status,
        StepStatus::Partial,
        "the report it made is the step's end"
    );
    assert!(matches!(terminal, Advanced::Finished { .. }));
}

/// A step's spend does not reset between attempts: asked again, the parked
/// step parks where it stands, with no attempt and no call behind it.
#[tokio::test]
async fn t10_a_step_stops_where_its_last_attempt_spent_its_figure() {
    let capped = capped_fixture(budget_figure(), budget_workflow());
    let run = capped_run(&capped.fx).await;
    assert_eq!(
        run.terminal,
        Advanced::WaitingHuman {
            gate: "step_stop".to_owned()
        }
    );

    let turns = turn_ends(&capped.fx.child_events_of(capped.write));
    let mut runner = capped.fx.runner();
    let first = runner.advance().await.expect("the run advances");
    let again = runner.advance().await.expect("the run advances");

    assert_eq!(first, again);
    assert_eq!(
        again,
        Advanced::WaitingHuman {
            gate: "step_stop".to_owned()
        }
    );
    assert_eq!(step_started(&capped.fx.lead_events()).len(), 2);
    assert_eq!(checkpoints(&capped.fx.lead_events()).len(), 1);
    assert_eq!(
        turn_ends(&capped.fx.child_events_of(capped.write)),
        turns,
        "no child turn is started or resumed"
    );
}

/// What an attempt has already spent counts, not only what the step's
/// closed attempts spent: a crash after a call that reached the figure
/// parks instead of resuming the torn turn, and the report is never taken.
#[tokio::test]
async fn t11_a_rebuild_of_an_attempt_whose_crashed_call_spent_the_room_parks() {
    let capped = capped_fixture(budget_figure(), budget_workflow());
    let run = capped_run(&capped.fx).await;
    assert_eq!(
        run.terminal,
        Advanced::WaitingHuman {
            gate: "step_stop".to_owned()
        }
    );

    // The lead at the writing step's `step_started`, and the child cut
    // where the crash would have landed: the prompt and the call that
    // spent the figure, no result line yet.
    let lead = run.after_where(EventKind::StepStarted, 2);
    let source = run.after(run.lead().len());
    let before = source
        .children
        .get(&capped.write)
        .expect("the child's log by the cap")
        .len();
    assert!(
        source.children[&capped.write]
            .iter()
            .any(|event| event.kind == EventKind::ToolResult),
        "the traced child went past the call, so the cut really cuts"
    );
    capped
        .fx
        .restore_child_prefix_from(lead, source, capped.write, |events| {
            !events
                .iter()
                .any(|event| event.kind == EventKind::ToolResult)
        });
    let mut runner = capped.fx.runner();
    let moved = runner.advance().await.expect("the run advances");

    assert_eq!(
        moved,
        Advanced::WaitingHuman {
            gate: "step_stop".to_owned()
        },
        "the torn call spent the figure, so the attempt's room is spent"
    );
    let child = capped.fx.child_events_of(capped.write);
    assert!(
        child.len() < before,
        "the child's log is the cut the crash left, not a resumed turn"
    );
    assert_eq!(
        calls_of(&child),
        1,
        "its one call is not followed by another"
    );
    assert!(
        !child
            .iter()
            .any(|event| event.kind == EventKind::StepReported),
        "the report it would have written was never taken"
    );
}

/// The world of a crashed attempt whose call spent half its figure: the
/// traced run's first call, then the priced call the attempt's second turn
/// makes, then the report it would otherwise reach.
fn continuation_fixture() -> BudgetFixture {
    let brief = Ulid::generate();
    let write = Ulid::generate();
    let half = budget_figure() / 2.0;
    let fx = Fixture::with_workflow(
        vec![
            (
                brief,
                vec![priced_reply(
                    brief_report_proposing(clearing_raise()),
                    budget_figure(),
                )],
            ),
            (
                write,
                vec![
                    priced_works("w1", half),
                    priced_works("w2", half),
                    priced_reply(implementer_report(), budget_figure()),
                ],
            ),
        ],
        &[
            ("workflow.toml", budget_workflow().as_str()),
            ("templates/step.md", "do the step\n"),
        ],
    )
    .priced(budget_prices());
    BudgetFixture { fx, write }
}

/// The attempt's second turn runs under what the attempt has left, not
/// under its whole figure: its one call lands on the remainder, and the
/// retry that follows has nothing left to spend.
#[tokio::test]
async fn t12_a_resumed_attempts_continuation_runs_under_what_the_attempt_has_left() {
    let capped = continuation_fixture();
    let run = capped_run(&capped.fx).await;
    assert_eq!(
        run.terminal,
        Advanced::WaitingHuman {
            gate: "step_stop".to_owned()
        }
    );

    let lead = run.after_where(EventKind::StepStarted, 2);
    let source = run.after(run.lead().len());
    capped
        .fx
        .restore_child_prefix_from(lead, source, capped.write, |events| {
            !events
                .iter()
                .any(|event| event.kind == EventKind::ToolResult)
        });
    let mut runner = capped.fx.runner();
    let terminal = loop {
        match runner.advance().await.expect("the run advances") {
            Advanced::Moved => {}
            other => break other,
        }
    };

    let child = capped.fx.child_events_of(capped.write);
    assert_eq!(
        calls_of(&child),
        2,
        "the cut call and the continuation's own: {child:#?}"
    );
    assert_eq!(
        turn_reasons(&child).last(),
        Some(&MAX_USD.to_owned()),
        "the continuation's call lands on the remainder, not on the figure"
    );
    assert!(
        !child
            .iter()
            .any(|event| event.kind == EventKind::StepReported),
        "the report is never reached"
    );
    assert_eq!(
        terminal,
        Advanced::WaitingHuman {
            gate: "step_stop".to_owned()
        },
        "the attempt's total is now the step's closed spend"
    );
}

// ---------------------------------------------------------------------------
// A step whose turn died on a provider fault
// ---------------------------------------------------------------------------

/// How many `step_finished` records the lead holds for `step`.
fn attempts_finished(fx: &Fixture, step: &str) -> usize {
    step_finished(&fx.lead_events())
        .iter()
        .filter(|finished| finished.step == step)
        .count()
}

/// The one scripted reply a child gets when the provider fails it: no
/// usage line, because the call never finished.
fn a_fault(error: ProviderError) -> Vec<ProviderEvent> {
    vec![ProviderEvent::Error(error)]
}

/// Drive a fixture whose implementer's one reply is `reply` until the live
/// pass surfaces the provider's error, then hand back the fixture and a
/// runner rebuilt over the logs that fault left.
async fn a_dead_turn_restarted(fx: Fixture) -> (Fixture, RunnerError, TestRunner) {
    let mut live = fx.runner();
    let error = loop {
        match live.advance().await {
            Ok(Advanced::Moved) => {}
            Ok(other) => panic!("the live pass ended without the provider's error: {other:?}"),
            Err(error) => break error,
        }
    };
    drop(live);
    let rebuilt = fx.runner();
    (fx, error, rebuilt)
}

/// The child's last `turn_ended`, as the log wrote it: what the parked
/// step ended on.
fn child_turn_end(fx: &Fixture, child: Ulid) -> aigentic_log::TurnEndedPayload {
    let events = fx.child_events_of(child);
    let ended = events
        .iter()
        .rev()
        .find(|event| event.kind == EventKind::TurnEnded)
        .expect("the child's turn ended");
    serde_json::from_value(ended.payload.clone()).unwrap()
}

/// A 5xx that killed a step's turn is re-applied by a resumed pass as the
/// step's end, and the park that opens offers the fresh attempt beside the
/// stop, so a run a transient fault stopped can go on with one answer.
#[tokio::test]
async fn a_resumed_run_offers_a_retry_for_a_recoverable_fault() {
    let brief_child = Ulid::generate();
    let implementer_child = Ulid::generate();
    let fx = Fixture::new(vec![
        (brief_child, vec![report("r1", brief_report())]),
        (
            implementer_child,
            vec![a_fault(ProviderError::Http {
                status: 503,
                body: "unavailable".into(),
            })],
        ),
    ]);
    let (fx, error, mut rebuilt) = a_dead_turn_restarted(fx).await;
    assert!(
        matches!(
            error,
            RunnerError::Turn(RuntimeError::Provider(ProviderError::Http {
                status: 503,
                ..
            }))
        ),
        "the live pass still surfaces the fault: {error:?}"
    );
    assert_eq!(
        drive_with(&fx, &mut rebuilt, || {}).await,
        Advanced::WaitingHuman {
            gate: "step_stop".to_owned()
        },
        "the resumed pass ends the step on the fault"
    );

    let events = fx.lead_events();
    let asked = checkpoints(&events).pop().expect("the park was asked");
    assert_eq!(asked.gate, "step_stop");
    assert_eq!(
        asked.options,
        vec!["retry", "stop"],
        "both answers, in order"
    );

    let ended = child_turn_end(&fx, implementer_child);
    let starts = step_started(&events);
    let attempt = starts.last().expect("the attempt started");
    assert_eq!(asked.shown[0], attempt.step, "the gate names the step");
    assert_eq!(
        asked.shown[1],
        attempt.attempt.to_string(),
        "and the attempt"
    );
    assert_eq!(asked.shown[2], ended.reason, "and what its turn ended on");

    let finishes = step_finished(&events);
    let last = finishes.last().expect("the step finished");
    assert_eq!(last.status, StepStatus::Failed);
    assert_eq!(
        last.end_reason, ended.reason,
        "the step's end carries the reason the child's turn ended on"
    );
    assert_eq!(
        last.cost_usd, 0.0,
        "the fixture prices nothing, so the fault's attempt cost nothing"
    );
}

/// A 4xx is not a fault a fresh call gets past, so that step parks as it
/// did before: `stop` alone. The word is written into the log here because
/// the daemon refuses an unoffered answer before writing it, which leaves
/// this the path a client answering the runtime directly takes.
#[tokio::test]
async fn a_fault_a_fresh_call_cannot_get_past_parks_with_stop_alone() {
    let brief_child = Ulid::generate();
    let implementer_child = Ulid::generate();
    let fx = Fixture::new(vec![
        (brief_child, vec![report("r1", brief_report())]),
        (
            implementer_child,
            vec![a_fault(ProviderError::Http {
                status: 404,
                body: "not found".into(),
            })],
        ),
    ]);
    let (fx, _, mut rebuilt) = a_dead_turn_restarted(fx).await;
    assert_eq!(
        drive_with(&fx, &mut rebuilt, || {}).await,
        Advanced::WaitingHuman {
            gate: "step_stop".to_owned()
        },
        "the resumed pass ends the step on the fault"
    );

    let events = fx.lead_events();
    let asked = checkpoints(&events).pop().expect("the park was asked");
    assert_eq!(
        asked.options,
        vec!["stop"],
        "a 4xx is not a fresh call's to fix"
    );
    let starts = step_started(&events).len();
    let finishes = step_finished(&events).len();

    rebuilt
        .answer("step_stop", CheckpointAnswer::Retry, None, steve())
        .expect("the runner writes the answer it was not offered");
    let error = rebuilt
        .advance()
        .await
        .expect_err("the move refuses the word the ask never offered");
    assert!(
        matches!(error, RunnerError::SliceTwo { ref gate } if gate == "step_stop"),
        "the refusal names the gate: {error:?}"
    );

    let after = fx.lead_events();
    assert_eq!(
        step_started(&after).len(),
        starts,
        "nothing started, so the refused word moved no attempt"
    );
    assert_eq!(step_finished(&after).len(), finishes, "and nothing ended");
    assert_eq!(
        after
            .iter()
            .filter(|event| event.kind == EventKind::CheckpointAnswered)
            .count(),
        1,
        "the ask counts as answered, so one answer stands"
    );
    assert!(
        matches!(
            rebuilt.answer("step_stop", CheckpointAnswer::Retry, None, steve()),
            Err(RunnerError::NotWaiting { .. })
        ),
        "and the park takes no second answer"
    );
}

/// The new offer stays where the fault is: a step cut off twice still parks
/// with `stop` alone, and the retry word moves nothing there either.
#[tokio::test]
async fn a_cap_park_still_offers_stop_alone() {
    let brief_child = Ulid::generate();
    let implementer_child = Ulid::generate();
    let fx = Fixture::new(vec![
        (brief_child, vec![report("r1", brief_report())]),
        (implementer_child, vec![works("w1"), works("w2")]),
    ])
    .capped(implementer_child, 1);

    let mut runner = fx.runner();
    assert_eq!(
        drive(&fx, &mut runner).await,
        Advanced::WaitingHuman {
            gate: "step_stop".to_owned()
        },
        "twice cut off is the human's problem"
    );

    let events = fx.lead_events();
    let asked = checkpoints(&events).pop().expect("the park was asked");
    assert_eq!(asked.options, vec!["stop"], "a cap is not a provider fault");
    let starts = step_started(&events).len();

    runner
        .answer("step_stop", CheckpointAnswer::Retry, None, steve())
        .expect("the runner writes the answer it was not offered");
    let error = runner
        .advance()
        .await
        .expect_err("the move refuses the word the ask never offered");
    assert!(
        matches!(error, RunnerError::SliceTwo { ref gate } if gate == "step_stop"),
        "the refusal names the gate: {error:?}"
    );
    assert_eq!(
        step_started(&fx.lead_events()).len(),
        starts,
        "the refused word moved no attempt"
    );
}

/// The park's `retry` starts exactly one new attempt of the step, in a
/// child the step has never used, and that child is prompted with the whole
/// task: a fresh attempt has no work to be told to continue. The attempt
/// then ends on its own report, not on the fault.
#[tokio::test]
async fn the_retry_answer_starts_a_fresh_attempt_with_a_first_attempts_prompt() {
    let brief_child = Ulid::generate();
    let implementer_child = Ulid::generate();
    let retry_child = Ulid::generate();
    let prices = Prices {
        input: 3.0,
        cache_read: 0.5,
        cache_write: 4.0,
        output: 15.0,
    };
    let fx = Fixture::new(vec![
        (brief_child, vec![report("r1", brief_report())]),
        (
            implementer_child,
            vec![a_fault(ProviderError::Http {
                status: 503,
                body: "unavailable".into(),
            })],
        ),
        (retry_child, vec![report("r2", implementer_report())]),
    ])
    .priced(prices);
    let (fx, _, mut rebuilt) = a_dead_turn_restarted(fx).await;
    assert_eq!(
        drive_with(&fx, &mut rebuilt, || {}).await,
        Advanced::WaitingHuman {
            gate: "step_stop".to_owned()
        },
        "the resumed pass parks at the fault"
    );

    rebuilt
        .answer("step_stop", CheckpointAnswer::Retry, None, steve())
        .expect("the park takes the retry");
    // The answer's move is `step_started`; the next moves post the fresh
    // attempt's prompt and run its turn, which ends on its report.
    let mut moves = 0;
    while step_finished(&fx.lead_events())
        .iter()
        .filter(|finished| finished.step == "implement-alone")
        .count()
        < 2
    {
        rebuilt.advance().await.expect("the fresh attempt runs");
        moves += 1;
        assert!(moves < 8, "the fresh attempt ends instead of looping");
    }

    let events = fx.lead_events();
    let attempts: Vec<_> = step_started(&events)
        .into_iter()
        .filter(|started| started.step == "implement-alone")
        .collect();
    assert_eq!(attempts.len(), 2, "one new attempt, not two");
    assert_eq!(attempts[0].attempt, 1);
    assert_eq!(
        attempts[1].attempt, 2,
        "the attempt after the one that died"
    );
    assert_ne!(
        attempts[0].child_thread, attempts[1].child_thread,
        "the fresh attempt runs in a child the step has never used"
    );

    // The child that reported is the one the new attempt started, derived
    // from the logs rather than named here: the dead attempt reported
    // nothing, the brief's child is not this step's.
    let reporters: Vec<Ulid> = fx
        .child_events()
        .into_iter()
        .filter(|(_, events)| events.iter().any(|e| e.kind == EventKind::StepReported))
        .map(|(id, _)| id)
        .filter(|id| *id != brief_child)
        .collect();
    assert_eq!(
        reporters,
        vec![attempts[1].child_thread],
        "the report of the new attempt lands in that child"
    );

    let template = prompts(&fx.child_events_of(attempts[0].child_thread));
    assert_eq!(template.len(), 1, "the dead attempt was prompted once");
    assert_eq!(
        template[0],
        prompts(&fx.child_events_of(attempts[1].child_thread))[0],
        "the fresh attempt gets the whole task, not an instruction"
    );
    assert_eq!(
        prompts(&fx.child_events_of(attempts[0].child_thread)).len(),
        1,
        "the retry leaves the dead child alone"
    );

    let finished = step_finished(&events);
    let last = finished.last().expect("the retry's attempt finished");
    assert_eq!(last.step, "implement-alone");
    assert_eq!(last.status, StepStatus::Done, "the report's status decides");
    assert_eq!(
        last.end_reason,
        aigentic_runtime::STEP_REPORTED,
        "the attempt ends on its report"
    );
    assert_ne!(
        last.end_reason,
        child_turn_end(&fx, implementer_child).reason,
        "not on the fault that killed the first attempt"
    );

    // Both steps' calls are priced by the fixture's table, and the fault's
    // attempt logged no usage, so the run carries the two reports' calls.
    let per_call = prices.cost_usd(&aigentic_log::Usage::reported(aigentic_core::Usage {
        input_tokens: 10,
        output_tokens: 5,
        ..Default::default()
    }));
    assert!(per_call > 0.0, "the fixture prices a real cost");
    assert_eq!(
        rebuilt.state().expect("the run state reads").cost_usd,
        per_call * 2.0,
        "the brief's call and the fresh attempt's"
    );
    assert_eq!(
        last.cost_usd, per_call,
        "the step's cost is the fresh attempt's own call"
    );
}

/// A cut stream is the fault a fresh call may get past even though
/// `is_transient` says no, and it is what actually stopped the run this
/// ticket came from: its park offers the retry, and one answer starts the
/// fresh attempt.
#[tokio::test]
async fn a_cut_stream_is_offered_a_retry_too() {
    let brief_child = Ulid::generate();
    let implementer_child = Ulid::generate();
    let retry_child = Ulid::generate();
    let fx = Fixture::new(vec![
        (brief_child, vec![report("r1", brief_report())]),
        (implementer_child, vec![a_fault(ProviderError::Cut)]),
        (retry_child, vec![report("r2", implementer_report())]),
    ]);
    let (fx, error, mut rebuilt) = a_dead_turn_restarted(fx).await;
    assert!(
        matches!(
            error,
            RunnerError::Turn(RuntimeError::Provider(ProviderError::Cut))
        ),
        "a cut stream still surfaces from the live pass: {error:?}"
    );
    assert_eq!(
        drive_with(&fx, &mut rebuilt, || {}).await,
        Advanced::WaitingHuman {
            gate: "step_stop".to_owned()
        },
        "the resumed pass parks at the fault"
    );

    let events = fx.lead_events();
    let asked = checkpoints(&events).pop().expect("the park was asked");
    assert_eq!(asked.options, vec!["retry", "stop"], "both answers");

    rebuilt
        .answer("step_stop", CheckpointAnswer::Retry, None, steve())
        .expect("the park takes the retry");
    assert_eq!(
        rebuilt.advance().await.expect("the retry's move runs"),
        Advanced::Moved,
        "the move writes the new attempt's start and ends there"
    );
    let attempts: Vec<_> = step_started(&fx.lead_events())
        .into_iter()
        .filter(|started| started.step == "implement-alone")
        .collect();
    assert_eq!(attempts.len(), 2, "one new attempt");
    assert_eq!(attempts[1].attempt, 2);
    assert_ne!(
        attempts[0].child_thread, attempts[1].child_thread,
        "the fresh attempt runs in a child the step has never used"
    );
}

/// The fresh attempt's prompt is what makes a rebuild safe: a crash between
/// the prompt and the turn's end must resume that attempt, not write the
/// template into a child that already holds it.
#[tokio::test]
async fn a_fresh_attempt_is_prompted_once_across_a_rebuild() {
    let brief_child = Ulid::generate();
    let implementer_child = Ulid::generate();
    let retry_child = Ulid::generate();
    let fx = Fixture::new(vec![
        (brief_child, vec![report("r1", brief_report())]),
        (
            implementer_child,
            vec![a_fault(ProviderError::Http {
                status: 503,
                body: "unavailable".into(),
            })],
        ),
        (retry_child, vec![report("r2", implementer_report())]),
    ]);
    let (fx, _, mut rebuilt) = a_dead_turn_restarted(fx).await;
    assert_eq!(
        drive_with(&fx, &mut rebuilt, || {}).await,
        Advanced::WaitingHuman {
            gate: "step_stop".to_owned()
        },
        "the resumed pass parks at the fault"
    );
    rebuilt
        .answer("step_stop", CheckpointAnswer::Retry, None, steve())
        .expect("the park takes the retry");
    assert_eq!(
        rebuilt.advance().await.expect("the retry's move runs"),
        Advanced::Moved,
        "the new attempt's start is written"
    );
    // The crash this test rebuilds from: the lead at the fresh attempt's
    // `step_started`, its child prompted, its turn never ended.
    let short = snapshot(&fx);

    let mut moves = 0;
    while attempts_finished(&fx, "implement-alone") < 2 {
        rebuilt.advance().await.expect("the fresh attempt runs");
        moves += 1;
        assert!(moves < 8, "the fresh attempt ends instead of looping");
    }
    let later = snapshot(&fx);
    let new_child = step_started(&later.lead)
        .into_iter()
        .filter(|started| started.step == "implement-alone")
        .nth(1)
        .expect("the fresh attempt is started")
        .child_thread;
    let before = prompts(&later.children[&new_child]);
    assert_eq!(before.len(), 1, "the whole run prompted it once");

    fx.restore_child_prefix_from(&short, &later, new_child, |events| {
        !events
            .iter()
            .any(|event| event.kind == EventKind::AssistantMessage)
    });
    let crashed = fx.child_events_of(new_child);
    assert_eq!(
        prompts(&crashed),
        before,
        "the crash left the prompt posted"
    );
    assert_eq!(turn_ends(&crashed), 0, "and its turn never ended");
    assert_eq!(
        attempts_finished(&fx, "implement-alone"),
        1,
        "the lead is back at the attempt's start"
    );

    let mut runner = fx.runner();
    let mut moves = 0;
    while attempts_finished(&fx, "implement-alone") < 2 {
        runner
            .advance()
            .await
            .expect("the rebuilt runner advances the attempt");
        moves += 1;
        assert!(moves < 8, "the rebuilt attempt ends instead of looping");
    }
    assert_eq!(
        prompts(&fx.child_events_of(new_child)),
        before,
        "the rebuild resumes the attempt instead of re-prompting it"
    );
    assert_eq!(
        attempts_finished(&fx, "implement-alone"),
        2,
        "one step_finished for the fresh attempt, not two"
    );
}
