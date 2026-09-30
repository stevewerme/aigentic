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
    Advanced, FakeForge, Forge, GhForge, IssueView, Runner, RunnerError, RunnerHost, WriteGuard,
};
use aigentic_runtime::workflow::{LoadedWorkflow, WorkflowFile, WorkflowOrigin};
use aigentic_runtime::{Answer, Approver, Prices, Runtime};
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
fn implementer_report() -> Value {
    json!({
        "status": "done",
        "body": "## Implementation\n\nall landed",
        "slots": {"size": "trivial"},
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
    ) -> Result<Runtime, RunnerError> {
        let log = ThreadLog::open(&self.dir, id)?;
        let script = self
            .scripts
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .unwrap_or_default();
        // What the child's log has already answered, so a rebuild gets
        // the replies that are still to come.
        let answered = log
            .events()
            .iter()
            .filter(|event| event.kind == EventKind::AssistantMessage)
            .count();
        let (provider, _seen) = scripted(script.into_iter().skip(answered).collect());
        let registry: ToolRegistry = vec![Box::new(Noop) as Box<dyn aigentic_core::Tool>].into();
        let cap = self.caps.get(&id).copied().unwrap_or(50);
        let mut runtime = Runtime::new(provider, registry, log, AgentId("child".into()))
            .with_policy(Policy::defaults())
            .with_approver(Box::new(Yes))
            .with_budget(Budget {
                max_iterations: cap,
                max_tokens: u64::MAX,
                max_wall_time: std::time::Duration::from_secs(60),
                cache_read_price_ratio: 0.25,
            })
            .with_step(step, deny)?;
        // A test that wants a real `usage.cost_usd` on the child's lines
        // prices this child's endpoint; an unpriced host behaves as before.
        if let Some(prices) = self.prices {
            runtime.set_pricing(profile, Some(prices));
        }
        Ok(runtime)
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
    children: Vec<(Ulid, Vec<Vec<ProviderEvent>>)>,
    forge: Arc<FakeForge>,
    workflow: LoadedWorkflow,
    caps: BTreeMap<Ulid, u32>,
    prices: Option<Prices>,
}

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
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let workflow = WorkflowFile::load_dir(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../workflows/build"),
            WorkflowOrigin::Bundled,
        )
        .expect("the build workflow loads");
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
            children,
            forge,
            workflow,
            caps: BTreeMap::new(),
            prices: None,
        }
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
    fn runner(&self) -> Runner<Arc<FakeForge>, FakeHost> {
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
            self.workflow.clone(),
            self.repo.clone(),
        )
        .expect("the log is the lead's")
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

/// Drive a fixture to its pause, recording the world after every move.
async fn trace(fx: &Fixture) -> Trace {
    let mut runner = fx.runner();
    let mut snapshots = vec![snapshot(fx)];
    let terminal = loop {
        match runner.advance().await.expect("the run advances") {
            Advanced::Moved => snapshots.push(snapshot(fx)),
            other => break other,
        }
    };
    // A pause writes its gate, so the world moved once more without a
    // `Moved`: record it, or a rebuild's last event would look missing.
    snapshots.push(snapshot(fx));
    Trace {
        snapshots,
        terminal,
    }
}

fn snapshot(fx: &Fixture) -> Snapshot {
    Snapshot {
        lead: fx.lead_events(),
        children: fx.child_events(),
        comments: fx.forge.posted(),
    }
}

/// Rebuild from `snapshot` and do one move.
async fn replay(fx: &Fixture, snapshot: &Snapshot) -> (Advanced, Runner<Arc<FakeForge>, FakeHost>) {
    fx.restore(snapshot);
    let mut runner = fx.runner();
    let advanced = runner.advance().await.expect("the rebuilt runner advances");
    (advanced, runner)
}

/// Drive a runner to its pause.
async fn drive(runner: &mut Runner<Arc<FakeForge>, FakeHost>) -> Advanced {
    runner
        .run_to_pause()
        .await
        .expect("the run reaches a pause")
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

/// A pause, as the step (or gate) it names, so a test can read a terminal
/// without matching the whole report.
fn pause_step(advanced: &Advanced) -> String {
    match advanced {
        Advanced::Moved => "moved".into(),
        Advanced::WaitingHuman { gate } => format!("gate:{gate}"),
        Advanced::PausedAtChecks { step, .. } => step.clone(),
        Advanced::Finished { outcome } => format!("finished:{outcome:?}"),
    }
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

/// T1: trivial happy path → `PausedAtChecks` on implement-alone; one
/// `## Brief` and one `## Implementation` comment;
/// `route_taken.budget_usd == brief budget slot`; `reported_event`
/// resolves in the child's log.
#[tokio::test]
async fn t1_the_happy_path_pauses_on_the_implementer() {
    let h = happy_path();
    let fx = &h.fx;
    let run = trace(fx).await;
    let events = run.lead();

    assert_eq!(
        run.terminal,
        Advanced::PausedAtChecks {
            step: "implement-alone".to_owned(),
            report: child_report(fx, h.implementer_child),
        },
        "the run hands the implementer's report to the checks"
    );
    assert_eq!(gate(events), None, "implement-alone pauses at its checks");

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
    assert_eq!(posted.len(), 2, "one comment per reporting step");
    let brief_marker = &fx.workflow.workflow.steps[0].marker;
    let implementer_marker = &fx.workflow.workflow.steps[1].marker;
    assert!(posted[0].contains(brief_marker.as_str()));
    assert!(posted[0].contains("the brief"));
    assert!(posted[1].contains(implementer_marker.as_str()));
    assert!(posted[1].contains("all landed"));
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

/// T3: rebuild the `Runner` after **every** lead event of T1's run; each
/// next `advance()` does the move once.
#[tokio::test]
async fn t3_a_rebuild_after_every_lead_event_does_each_move_once() {
    let h = happy_path();
    let fx = &h.fx;
    let run = trace(fx).await;
    let n = run.lead().len();
    assert!(n >= 5, "the happy path is several moves long");

    for k in 1..=n {
        let snapshot = run.after(k);
        let (advanced, mut runner) = replay(fx, snapshot).await;
        let after = fx.lead_events();
        if k < n {
            assert_eq!(advanced, Advanced::Moved, "prefix {k}: a move was due");
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
        let done = drive(&mut runner).await;
        assert_eq!(done, run.terminal, "prefix {k}: the same pause");
        assert_eq!(
            kinds(&fx.lead_events()),
            kinds(run.lead()),
            "prefix {k}: the same moves, once each"
        );
    }
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

    let run = trace(&fx).await;
    let events = run.lead();
    assert_eq!(
        run.terminal,
        Advanced::PausedAtChecks {
            step: "implement-alone".to_owned(),
            report: child_report(&fx, implementer_child),
        }
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

    // Attempt 1 reported nothing, so only the implementer's report is a
    // comment; the brief's is the other.
    let posted = fx.forge.posted();
    assert_eq!(posted.len(), 2);
    assert!(
        posted[1].contains("attempt=2"),
        "the report says which attempt"
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
    let paused = drive(&mut runner).await;
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

    let run = trace(&fx).await;
    assert_eq!(
        run.terminal,
        Advanced::PausedAtChecks {
            step: "implement-alone".to_owned(),
            report: child_report(&fx, implementer_child),
        }
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
        drive(&mut runner).await,
        Advanced::WaitingHuman {
            gate: "step_stop".to_owned()
        }
    );
    assert_eq!(step_started(&fx.lead_events()).len(), 3);
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
    assert_eq!(pause_step(&run.terminal), "implement-alone");
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
        pause_step(&drive(&mut runner).await),
        "implement-alone",
        "the repaired turn ends the attempt, not the run"
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
        drive(&mut runner).await,
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
        pause_step(&drive(&mut runner).await),
        "implement-alone",
        "the run continues past the repaired step"
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
    assert_eq!(
        fx.forge.posted().len(),
        2,
        "the written report is posted once, then the implementer's"
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
    assert_eq!(pause_step(&drive(&mut runner).await), "implement-alone");

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
    drive(&mut runner).await;
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
    drive(&mut runner).await;
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
    assert_eq!(pause_step(&run.terminal), "implement-alone");

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
    let terminal = drive(&mut runner).await;
    assert_eq!(
        pause_step(&terminal),
        "implement-alone",
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
        drive(&mut runner).await,
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
    assert_eq!(posted.len(), 2);

    // Every lead event boundary of the happy path: rebuild there, catch
    // up, and see the same comments once each. A rebuild *after* a post
    // finds it on the issue; one before posts it.
    for k in 1..=full.len() {
        let snapshot = run.after(k);
        let (_, mut runner) = replay(fx, snapshot).await;
        drive(&mut runner).await;
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
    drive(&mut runner).await;
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
    let run = trace(fx).await;
    assert_eq!(pause_step(&run.terminal), "implement-alone");
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
    assert_eq!(pause_step(&drive(&mut first).await), "implement-alone");
    assert_eq!(
        WriteGuard::holder(&a.fx.repo),
        Some(a.fx.lead),
        "the paused run holds the repo"
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
        drive(&mut runner).await,
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
    assert_eq!(pause_step(&run.terminal), "implement-alone");

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
      *) printf '%s' '{"title":"A title","body":"A body"}' ;;
    esac
    ;;
  "issue comment")
    while [ $# -gt 0 ]; do
      if [ "$1" = "--body-file" ]; then shift; cp "$1" "$here/body.txt"; fi
      shift
    done
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
