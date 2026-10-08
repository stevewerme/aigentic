//! Issue #47: the machine must not idle-sleep while a turn runs. This
//! file covers the guard itself (T1, T5, T6) and the actor's
//! hold-while-working / release-while-waiting scope over real turns
//! (T2–T4).

use std::collections::VecDeque;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use aigentic_api::{Notice, Response, ThreadState};
use aigentic_runtime::Runtime;
use aigentic_runtime::aigentic_core::{
    AgentId, Author, Capabilities, CompletionRequest, ContentBlock, Event, EventKind, Message,
    Provider, ProviderError, ProviderEvent, ToolCall, UserId,
};
use aigentic_runtime::aigentic_core::{BoxFuture, RiskClass, Tool, ToolError, ToolOutput};
use aigentic_runtime::aigentic_log::{ThreadLog, TurnEndedPayload};
use aigentic_runtime::aigentic_policy::Policy;
use aigentic_runtime::aigentic_tools::ToolRegistry;
use aigentic_server::awake::{
    KeepAwake, ProcessGuard, detect_with, group_gone, on_path, on_path_in,
};
use aigentic_server::config::Config;
use aigentic_server::{Mail, Mailbox, NoReports, ThreadActor};
use futures_core::Stream;
use serde_json::json;
use tokio::sync::{mpsc, oneshot};

// ---------------------------------------------------------------------
// The guard.

type Spawn = Box<dyn Fn() -> std::io::Result<std::process::Child> + Send + Sync>;

/// A scripted spawner (issue #47): each call records the child's pid and
/// starts `sh -c 'sleep 30 & wait'`, a group with a grandchild in it, so
/// a test can see that a release kills the whole group and not only the
/// shell.
struct Spawner {
    pids: Mutex<Vec<i32>>,
}

impl Spawner {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            pids: Mutex::new(Vec::new()),
        })
    }

    fn starts(&self) -> usize {
        self.lock().len()
    }

    fn last(&self) -> i32 {
        *self.lock().last().expect("something was started")
    }

    fn lock(&self) -> MutexGuard<'_, Vec<i32>> {
        self.pids.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn closure(self: &Arc<Self>) -> Spawn {
        let pids = self.clone();
        Box::new(move || {
            let mut command = std::process::Command::new("sh");
            command
                .arg("-c")
                .arg("sleep 30 & wait")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            // The guard's own spawner does this; the script must too, or
            // its pid would not be a group to kill.
            #[cfg(unix)]
            command.process_group(0);
            let child = command.spawn()?;
            pids.lock().push(child.id() as i32);
            Ok(child)
        })
    }
}

/// T1 (issue #47): two overlapping holds start one program; the first
/// release leaves it alive (a turn is still working), the second kills
/// its whole group so nothing of it survives; and a guard dropped with a
/// hold outstanding kills its child too, so a daemon that exits mid-turn
/// leaves no assertion behind.
#[test]
fn one_program_serves_overlapping_holds_and_dies_with_the_last_one() {
    let spawner = Spawner::new();
    let guard = ProcessGuard::with_spawner(spawner.closure());
    guard.hold();
    guard.hold();
    assert_eq!(spawner.starts(), 1, "the second hold started nothing new");
    let pid = spawner.last();
    assert!(!group_gone(pid), "the program is alive while a turn works");
    guard.release();
    assert!(!group_gone(pid), "a turn still works: its program stays up");
    guard.release();
    assert!(
        group_gone(pid),
        "the last release killed the group, grandchild included"
    );

    // A daemon that exits mid-turn: the drop is the only teardown left.
    let spawner = Spawner::new();
    let guard = ProcessGuard::with_spawner(spawner.closure());
    guard.hold();
    let pid = spawner.last();
    assert!(!group_gone(pid));
    drop(guard);
    assert!(group_gone(pid), "the dropped guard killed its child");
}

/// T5 (issue #47): what `detect_with` makes of a machine, with the
/// search path given so no test moves the process's own. Off wins over
/// everything when the config says so — a machine that could hold an
/// assertion is not asked to — and a machine without the program reports
/// itself unavailable rather than pretending.
#[test]
fn detect_reads_the_config_and_the_machine() {
    let dir = tempfile::tempdir().unwrap();
    let path = std::env::join_paths([dir.path()]).unwrap();
    // A directory holding a file for every program this platform could
    // want, so the test states the rule rather than the name.
    for name in ["caffeinate", "systemd-inhibit"] {
        std::fs::write(dir.path().join(name), b"#!/bin/sh\n").unwrap();
    }
    assert_eq!(
        detect_with(Some(&path), true).status(),
        "on",
        "the program is there, so the guard is on"
    );
    // The config wins: an off guard, and one that never starts anything
    // (`ProcessGuard`'s own spawner panics if it is ever asked to), and
    // never warns about a machine it was told not to use.
    let off = detect_with(Some(&path), false);
    assert_eq!(off.status(), "off");
    assert_eq!(off.program(), None);
    off.hold();
    off.hold();
    off.release();
    off.release();
    let empty = std::env::join_paths([dir.path().join("empty")]).unwrap();
    let missing = detect_with(Some(&empty), true).status();
    assert!(
        missing.starts_with("unavailable: ") && missing.ends_with("on PATH"),
        "a path without the program is unavailable: {missing}"
    );

    // The search rule itself: a directory with the file, one without it,
    // and no PATH at all.
    assert!(on_path_in(Some(&path), "caffeinate"));
    assert!(!on_path_in(Some(&path), "aigentic-no-such-program-47"));
    assert!(!on_path_in(None, "caffeinate"), "no PATH, nothing found");
    // And the real machine: `sh` is found (the daemon can find programs)
    // and a name nothing installs is not.
    assert!(on_path("sh"));
    assert!(!on_path("aigentic-no-such-program-47"));
}

/// T5 (issue #47), the config half: `keep_awake` is on unless the file
/// says otherwise, and turning it off is one key.
#[test]
fn keep_awake_defaults_to_on() {
    let profile = "[profiles.p]\nprovider = \"openai_compat\"\nbase_url = \"https://x/v1\"\nmodel = \"m\"\napi_key_env = \"K\"\n";
    let bare = Config::parse(profile).unwrap();
    assert!(bare.keep_awake, "holding the machine awake is the default");
    // The key is the config's, not a profile's, so it goes above the
    // table header.
    let off = Config::parse(&format!("keep_awake = false\n{profile}")).unwrap();
    assert!(!off.keep_awake);
}

/// T6 (issue #47): a guard whose program will not start warns once,
/// names the failure in its status, and is not retried; a turn that runs
/// while it is broken completes, and its payload carries that same
/// status, so the turn line has the hint where it matters (amendment 2
/// item 2: the status is read when the turn ends).
#[tokio::test]
async fn a_failed_guard_warns_once_and_names_the_turn() {
    let guard = Arc::new(ProcessGuard::with_spawner(Box::new(|| {
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no caffeinate",
        ))
    })));
    assert_eq!(guard.status(), "on", "before its first hold it is fine");
    let rig = rig_with_guard(
        vec![
            Some(vec![text("done"), done()]),
            Some(vec![text("again"), done()]),
        ],
        guard.clone(),
    );
    let mut notices = rig.subscribe().await;
    rig.post(steve(), "one", false).await;
    rig.until_idle(&mut notices).await;
    rig.settle().await;
    assert_eq!(guard.warnings(), 1, "one failure is one warning");
    let failed = guard.status();
    assert!(
        failed.starts_with("unavailable: ") && failed.contains("no caffeinate"),
        "the reason names the failure: {failed}"
    );
    assert!(
        rig.events()
            .last()
            .is_some_and(|e| e.kind == EventKind::TurnEnded),
        "the turn completed despite the broken guard"
    );
    assert_eq!(
        rig.turn_ended().keep_awake,
        Some(failed.clone()),
        "the turn reports the guard it actually had"
    );
    // A later turn holds again; the failed guard is not retried.
    rig.post(steve(), "two", false).await;
    rig.until_idle(&mut notices).await;
    rig.settle().await;
    assert_eq!(guard.warnings(), 1, "a failed guard is not retried");
}

// ---------------------------------------------------------------------
// The actor: a real turn, a recording guard.

/// A guard that records the calls the actor makes (issue #47), so a test
/// can read the hold/release scope around a turn.
struct Recording {
    inner: Mutex<Rec>,
}

#[derive(Default)]
struct Rec {
    /// Holds outstanding right now.
    held: usize,
    /// Holds and releases asked for, in order.
    calls: Vec<&'static str>,
    /// Periods that started the program: a hold with nothing held before
    /// it.
    starts: usize,
}

impl Recording {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Rec::default()),
        })
    }

    fn rec(&self) -> MutexGuard<'_, Rec> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn calls(&self) -> Vec<&'static str> {
        self.rec().calls.clone()
    }

    fn starts(&self) -> usize {
        self.rec().starts
    }

    fn outstanding(&self) -> usize {
        self.rec().held
    }
}

impl KeepAwake for Recording {
    fn hold(&self) {
        let mut rec = self.rec();
        if rec.held == 0 {
            rec.starts += 1;
        }
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

struct Gated {
    script: Mutex<VecDeque<Option<Vec<ProviderEvent>>>>,
}

impl Provider for Gated {
    fn complete(
        &self,
        _request: &CompletionRequest<'_>,
    ) -> Pin<Box<dyn Stream<Item = ProviderEvent> + Send + '_>> {
        match self.script.lock().unwrap().pop_front().expect("script") {
            Some(events) => Box::pin(futures_util::stream::iter(events)),
            None => Box::pin(futures_util::stream::pending()),
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
            max_context_tokens: 1000,
        }
    }
}

fn steve() -> Author {
    Author::User(UserId("steve".into()))
}
fn text(t: &str) -> ProviderEvent {
    ProviderEvent::TextDelta(t.into())
}
fn done() -> ProviderEvent {
    ProviderEvent::Done {
        finish_reason: "stop".into(),
    }
}
/// A provider error, the one turn outcome the plan calls out.
fn failed() -> ProviderEvent {
    ProviderEvent::Error(ProviderError::Transport("gone".into()))
}
/// A call with a question: waiting on a person.
fn ask(id: &str) -> ProviderEvent {
    ProviderEvent::ToolCall(ToolCall {
        id: id.into(),
        name: "ask_human".into(),
        args: json!({"question": "which colour?"}),
    })
}
/// A `write`-class tool: `Policy::defaults` asks a person about it.
struct Slow;

impl Tool for Slow {
    fn name(&self) -> &str {
        "slow"
    }
    fn description(&self) -> &str {
        "slow"
    }
    fn schema(&self) -> schemars::schema::RootSchema {
        schemars::schema_for!(String)
    }
    fn risk_class(&self) -> RiskClass {
        RiskClass::Write
    }
    fn call(&self, _args: serde_json::Value) -> BoxFuture<'_, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            Ok(ToolOutput {
                content: "slept".into(),
                is_error: false,
            })
        })
    }
}

/// A call to that tool: the permission request of T4.
fn slow(id: &str, ms: u64) -> ProviderEvent {
    ProviderEvent::ToolCall(ToolCall {
        id: id.into(),
        name: "slow".into(),
        args: json!({"ms": ms}),
    })
}

fn tool_use() -> ProviderEvent {
    ProviderEvent::Done {
        finish_reason: "tool_use".into(),
    }
}

struct Rig {
    mailbox: Mailbox,
    guard: Arc<Recording>,
    dir: tempfile::TempDir,
    thread: ulid::Ulid,
}

fn rig(script: Vec<Option<Vec<ProviderEvent>>>) -> Rig {
    let guard = Recording::new();
    let mut rig = rig_with_guard(script, guard.clone());
    rig.guard = guard;
    rig
}

/// The same rig with the guard given: a test that watches `Recording`
/// keeps its own `Arc`, and T6 installs a real broken `ProcessGuard`.
fn rig_with_guard(script: Vec<Option<Vec<ProviderEvent>>>, guard: Arc<dyn KeepAwake>) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let thread = ulid::Ulid::generate();
    let log = ThreadLog::open(dir.path(), thread).unwrap();
    // A `write`-class tool: `Policy::defaults` asks about it, so a turn
    // can be made to wait on a person (T4).
    let mut registry = ToolRegistry::empty();
    registry.register(Box::new(Slow)).unwrap();
    let runtime = Runtime::new(
        Box::new(Gated {
            script: Mutex::new(script.into()),
        }),
        registry,
        log,
        AgentId("worker".into()),
    )
    .with_policy(Policy::defaults());
    let (actor, mailbox) =
        ThreadActor::new(runtime, None, Arc::new(NoReports), Arc::new(|_: &str| None))
            .expect("the actor builds");
    let actor = actor.with_keep_awake(guard);
    tokio::spawn(actor.run());
    Rig {
        mailbox,
        guard: Recording::new(),
        dir,
        thread,
    }
}

impl Rig {
    async fn post(&self, author: Author, text: &str, interrupt: bool) -> Response {
        let (reply, rx) = oneshot::channel();
        self.mailbox
            .send(Mail::Post {
                author,
                blocks: vec![ContentBlock::Text(text.into())],
                interrupt,
                reply,
            })
            .unwrap();
        rx.await.unwrap()
    }

    async fn answer(&self, call_id: &str, text: &str) -> Response {
        let (reply, rx) = oneshot::channel();
        self.mailbox
            .send(Mail::Answer {
                by: steve(),
                call_id: call_id.into(),
                text: text.into(),
                reply,
            })
            .unwrap();
        rx.await.unwrap()
    }

    async fn decide(&self, by: Author, call_id: &str, allow: bool) -> Response {
        let (reply, rx) = oneshot::channel();
        self.mailbox
            .send(Mail::Decide {
                by,
                call_id: call_id.into(),
                allow,
                session: false,
                prefix: None,
                reason: None,
                reply,
            })
            .unwrap();
        rx.await.unwrap()
    }

    async fn subscribe(&self) -> mpsc::UnboundedReceiver<Notice> {
        let (notices, rx) = mpsc::unbounded_channel();
        let (reply, reply_rx) = oneshot::channel();
        self.mailbox
            .send(Mail::Subscribe {
                from_seq: 0,
                notices,
                reply,
            })
            .unwrap();
        reply_rx.await.unwrap();
        rx
    }

    fn events(&self) -> Vec<Event> {
        ThreadLog::open(self.dir.path(), self.thread)
            .unwrap()
            .read_all()
            .unwrap()
    }

    fn turn_ended(&self) -> TurnEndedPayload {
        let ended = self
            .events()
            .into_iter()
            .find(|e| e.kind == EventKind::TurnEnded)
            .expect("the turn ended in the log");
        serde_json::from_value(ended.payload).unwrap()
    }

    /// Wait for a state.
    async fn until_state<F: Fn(&ThreadState) -> bool>(
        &self,
        notices: &mut mpsc::UnboundedReceiver<Notice>,
        want: F,
    ) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let n = tokio::time::timeout_at(deadline, notices.recv())
                .await
                .expect("a state in time")
                .expect("the channel is open");
            if let Notice::State { state, .. } = &n
                && want(state)
            {
                return;
            }
        }
    }

    async fn until_idle(&self, notices: &mut mpsc::UnboundedReceiver<Notice>) {
        self.until_state(notices, |s| *s == ThreadState::Idle).await;
    }

    /// Let the actor run whatever it does after the last state change —
    /// the guard is released after the side jobs, past `Idle`.
    async fn settle(&self) {
        tokio::time::sleep(Duration::from_millis(150)).await;
    }

    /// The guard's calls, once nothing more is coming.
    async fn settled_calls(&self) -> Vec<&'static str> {
        self.settle().await;
        self.guard.calls()
    }
}

/// T2 (issue #47): a turn that ends on a provider error holds the
/// assertion while it works and gives it back when it ends — one hold,
/// one release, nothing left outstanding.
#[tokio::test]
async fn a_turn_that_ends_on_a_provider_error_is_balanced() {
    // The retry a transient error after content earns (issue #114) fails
    // too, so the turn still ends on the error; the hold is taken once per
    // turn, so the retry adds none.
    let rig = rig(vec![
        Some(vec![text("half"), failed()]),
        Some(vec![failed()]),
    ]);
    let mut notices = rig.subscribe().await;
    rig.post(steve(), "one", false).await;
    rig.until_idle(&mut notices).await;
    assert_eq!(
        rig.settled_calls().await,
        vec!["hold", "release"],
        "one hold, one release"
    );
    assert_eq!(rig.guard.outstanding(), 0, "nothing is left held");
    assert_eq!(rig.guard.starts(), 1, "one program for the one busy period");
    let payload = rig.turn_ended();
    assert!(
        payload.reason.starts_with("provider_error: "),
        "the reason names the provider: {}",
        payload.reason
    );
    // Every start was matched by a stop: the calls are balanced.
    let calls = rig.settled_calls().await;
    assert_eq!(
        calls.iter().filter(|c| **c == "hold").count(),
        calls.iter().filter(|c| **c == "release").count(),
        "holds and releases balance: {calls:?}"
    );
    assert_eq!(rig.guard.outstanding(), 0, "nothing is left held");
}

/// T3 (issue #47): an interrupt while the turn waits on a person still
/// balances — no hold is left outstanding and the thread ends Idle.
#[tokio::test]
async fn an_interrupt_while_waiting_keeps_the_guard_balanced() {
    let rig = rig(vec![
        Some(vec![ask("q1"), tool_use()]),
        Some(vec![text("ok"), done()]),
    ]);
    let mut notices = rig.subscribe().await;
    rig.post(steve(), "one", false).await;
    rig.until_state(&mut notices, |s| {
        matches!(s, ThreadState::AwaitingHuman { .. })
    })
    .await;
    assert_eq!(
        rig.guard.calls(),
        vec!["hold", "release"],
        "holding stops the moment the turn waits on a person"
    );
    rig.post(steve(), "no", true).await;
    rig.until_idle(&mut notices).await;
    rig.settle().await;
    assert_eq!(
        rig.guard.outstanding(),
        0,
        "the interrupt left nothing held"
    );
    assert!(
        rig.events()
            .last()
            .is_some_and(|e| e.kind == EventKind::TurnEnded),
        "the interrupted turn ended"
    );
}

/// Beyond the plan (issue #47), found by the live reference check: a
/// mid-turn message posted while the thread waits on a person must not
/// hold the machine awake again. The first live run kept `caffeinate`
/// alive while an approval prompt sat unanswered, because the steer
/// message called the actor's `running`.
#[tokio::test]
async fn a_mid_turn_message_during_a_wait_does_not_re_hold() {
    let rig = rig(vec![
        Some(vec![ask("q1"), tool_use()]),
        Some(vec![text("ok"), done()]),
    ]);
    let mut notices = rig.subscribe().await;
    rig.post(steve(), "one", false).await;
    rig.until_state(&mut notices, |s| {
        matches!(s, ThreadState::AwaitingHuman { .. })
    })
    .await;
    assert_eq!(rig.guard.calls(), vec!["hold", "release"]);
    // Thinking out loud while the question is unanswered.
    rig.post(steve(), "still thinking", false).await;
    rig.settle().await;
    assert_eq!(
        rig.guard.calls(),
        vec!["hold", "release"],
        "the wait still holds nothing: a person's minutes are not work"
    );
    assert_eq!(rig.guard.outstanding(), 0);
    // The answer ends the wait, and the guard comes back with the work.
    assert_eq!(rig.answer("q1", "red").await, Response::Ok);
    rig.until_idle(&mut notices).await;
    assert_eq!(
        rig.settled_calls().await,
        vec!["hold", "release", "hold", "release"],
        "the guard returned when the answer did"
    );
}

/// T4 (issue #47): the calls around a permission request, in order —
/// hold while the turn works, release while a person thinks, hold when
/// the answer lands, release when the turn ends.
#[tokio::test]
async fn a_waited_turn_stops_before_the_wait_and_starts_after_it() {
    let rig = rig(vec![
        Some(vec![slow("c1", 0), tool_use()]),
        Some(vec![text("ok"), done()]),
    ]);
    let mut notices = rig.subscribe().await;
    rig.post(steve(), "one", false).await;
    rig.until_state(
        &mut notices,
        |s| matches!(s, ThreadState::AwaitingApproval { call_id, .. } if call_id == "c1"),
    )
    .await;
    assert_eq!(
        rig.guard.calls(),
        vec!["hold", "release"],
        "the program stops while nothing is working: that is what lets the machine sleep"
    );
    assert_eq!(rig.decide(steve(), "c1", true).await, Response::Ok);
    rig.until_idle(&mut notices).await;
    assert_eq!(
        rig.settled_calls().await,
        vec!["hold", "release", "hold", "release"],
        "a working turn, the wait, the rest of the turn, the end"
    );
    assert_eq!(rig.guard.outstanding(), 0);
    assert_eq!(
        rig.guard.starts(),
        2,
        "the wait is a period of its own: the program stops and starts again"
    );
}
