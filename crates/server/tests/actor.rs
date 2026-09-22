//! The thread actor with a scripted provider and no socket (phase 5 step
//! 6): a post during a turn queues and the next turn holds both messages;
//! an interrupt ends the turn with `interrupted { by }` and no partial
//! message; a running tool's result is recorded first; an interrupt
//! while awaiting approval denies with the interrupter; subscribers get
//! every notice in order and a late one gets the events it missed.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aigentic_api::{Notice, ReportKind, Response, ThreadState};
use aigentic_runtime::aigentic_core::{
    Author, BoxFuture, Capabilities, CompletionRequest, ContentBlock, EventKind, Message, Provider,
    ProviderEvent, RiskClass, Tool, ToolCall, ToolError, ToolOutput, UserId,
};
use aigentic_runtime::aigentic_log::{
    InterruptedPayload, PermissionDecidedPayload, ThreadLog, UserMessagePayload,
};
use aigentic_runtime::aigentic_policy::{Decision, Policy, Rule};
use aigentic_runtime::aigentic_tools::ToolRegistry;
use aigentic_runtime::{Mode, Runtime};
use aigentic_server::{Mail, Mailbox, NoReports, ThreadActor};
use futures_core::Stream;
use serde_json::json;
use tokio::sync::{mpsc, oneshot};

/// Scripted; a `None` entry pends until the turn is interrupted. Every
/// request's messages are kept.
struct Gated {
    script: Mutex<VecDeque<Option<Vec<ProviderEvent>>>>,
    seen: Arc<Mutex<Vec<Vec<Message>>>>,
}

impl Provider for Gated {
    fn complete(
        &self,
        request: &CompletionRequest<'_>,
    ) -> Pin<Box<dyn Stream<Item = ProviderEvent> + Send + '_>> {
        self.seen.lock().unwrap().push(request.messages.to_vec());
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

/// A `write`-class tool that sleeps `ms`.
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
    fn call(&self, args: serde_json::Value) -> BoxFuture<'_, Result<ToolOutput, ToolError>> {
        let ms = args["ms"].as_u64().unwrap_or(0);
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            Ok(ToolOutput {
                content: format!("slept {ms}"),
                is_error: false,
            })
        })
    }
}

fn steve() -> Author {
    Author::User(UserId("steve".into()))
}
fn magnus() -> Author {
    Author::User(UserId("magnus".into()))
}
fn text(t: &str) -> ProviderEvent {
    ProviderEvent::TextDelta(t.into())
}
fn done() -> ProviderEvent {
    ProviderEvent::Done {
        finish_reason: "stop".into(),
    }
}
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
    seen: Arc<Mutex<Vec<Vec<Message>>>>,
    dir: tempfile::TempDir,
    thread: ulid::Ulid,
    task: tokio::task::JoinHandle<()>,
}

fn rig(script: Vec<Option<Vec<ProviderEvent>>>, allow_slow: bool) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let thread = ulid::Ulid::generate();
    let log = ThreadLog::open(dir.path(), thread).unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ToolRegistry::empty();
    registry.register(Box::new(Slow)).unwrap();
    let policy = if allow_slow {
        Policy::configured(vec![Rule::tool("slow", Decision::Allow, "test")], None)
    } else {
        Policy::defaults()
    };
    let runtime = Runtime::new(
        Box::new(Gated {
            script: Mutex::new(script.into()),
            seen: seen.clone(),
        }),
        registry,
        log,
        aigentic_runtime::aigentic_core::AgentId("worker".into()),
    )
    .with_policy(policy);
    let (actor, mailbox) = ThreadActor::new(runtime, None, Arc::new(NoReports)).unwrap();
    let task = tokio::spawn(actor.run());
    Rig {
        mailbox,
        seen,
        dir,
        thread,
        task,
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

    async fn subscribe(
        &self,
        from_seq: u64,
    ) -> (ThreadState, Vec<EventKind>, mpsc::UnboundedReceiver<Notice>) {
        let (notices, rx) = mpsc::unbounded_channel();
        let (reply, reply_rx) = oneshot::channel();
        self.mailbox
            .send(Mail::Subscribe {
                from_seq,
                notices,
                reply,
            })
            .unwrap();
        let (state, events, _mode) = reply_rx.await.unwrap();
        (state, events.iter().map(|e| e.kind).collect(), rx)
    }

    async fn decide(&self, by: Author, call_id: &str, allow: bool) -> Response {
        let (reply, rx) = oneshot::channel();
        self.mailbox
            .send(Mail::Decide {
                by,
                call_id: call_id.into(),
                allow,
                session: false,
                reply,
            })
            .unwrap();
        rx.await.unwrap()
    }

    async fn send(&self, mail: impl FnOnce(oneshot::Sender<Response>) -> Mail) -> Response {
        let (reply, rx) = oneshot::channel();
        self.mailbox.send(mail(reply)).unwrap();
        rx.await.unwrap()
    }

    fn log(&self) -> Vec<aigentic_runtime::aigentic_core::Event> {
        ThreadLog::open(self.dir.path(), self.thread)
            .unwrap()
            .read_all()
            .unwrap()
    }

    fn kinds(&self) -> Vec<EventKind> {
        self.log().iter().map(|e| e.kind).collect()
    }

    /// Wait until the state notice `pred` accepts arrives; returns
    /// every notice seen up to it.
    async fn until_state(
        rx: &mut mpsc::UnboundedReceiver<Notice>,
        pred: impl Fn(&ThreadState) -> bool,
    ) -> Vec<Notice> {
        let mut seen = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let n = tokio::time::timeout_at(deadline, rx.recv())
                .await
                .expect("state in time")
                .expect("channel open");
            let hit = matches!(&n, Notice::State { state, .. } if pred(state));
            seen.push(n);
            if hit {
                return seen;
            }
        }
    }
}

fn texts(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .filter_map(|m| match m.blocks.first() {
            Some(ContentBlock::Text(t)) => Some(t.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_post_during_a_turn_queues_and_the_next_turn_holds_both() {
    let rig = rig(
        vec![
            Some(vec![slow("c1", 150), tool_use()]),
            Some(vec![text("reply one"), done()]),
            Some(vec![text("reply two"), done()]),
        ],
        true,
    );
    let (state, kinds, mut notices) = rig.subscribe(0).await;
    assert_eq!(state, ThreadState::Idle);
    assert!(kinds.is_empty());

    assert_eq!(rig.post(steve(), "one", false).await, Response::Ok);
    // The turn is running the slow tool; magnus posts.
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert_eq!(rig.post(magnus(), "two", false).await, Response::Ok);
    let seen = Rig::until_state(&mut notices, |s| {
        matches!(s, ThreadState::Running { queued: 1, .. })
    })
    .await;
    // Magnus's message is an event at once, flagged mid-turn.
    let queued_event = seen.iter().find_map(|n| match n {
        Notice::Event { event, .. } if event.author == magnus() => Some(event.clone()),
        _ => None,
    });
    let p: UserMessagePayload = serde_json::from_value(queued_event.unwrap().payload).unwrap();
    assert!(p.mid_turn);

    // Turn one ends, turn two starts by magnus, then idle.
    Rig::until_state(&mut notices, |s| *s == ThreadState::Idle).await;
    assert_eq!(
        rig.kinds(),
        vec![
            EventKind::UserMessage,
            EventKind::AssistantMessage,
            EventKind::UserMessage, // magnus, mid-turn, before the result
            EventKind::ToolResult,
            EventKind::AssistantMessage,
            EventKind::TurnEnded,
            EventKind::AssistantMessage,
            EventKind::TurnEnded,
        ]
    );
    // The first turn's second request did not see "two"; the next did.
    {
        let seen = rig.seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        assert!(
            !texts(&seen[1]).iter().any(|t| t.contains("two")),
            "{:?}",
            texts(&seen[1])
        );
        let last = texts(&seen[2]);
        assert!(last.iter().any(|t| t == "steve: one"), "{last:?}");
        assert!(last.iter().any(|t| t == "magnus: two"), "{last:?}");
    }
    drop(rig.mailbox);
    rig.task.await.unwrap();
}

#[tokio::test]
async fn an_interrupt_ends_the_turn_and_the_new_turn_holds_the_message() {
    let rig = rig(vec![None, Some(vec![text("after"), done()])], true);
    let (_, _, mut notices) = rig.subscribe(0).await;
    rig.post(steve(), "one", false).await;
    Rig::until_state(&mut notices, |s| matches!(s, ThreadState::Running { .. })).await;
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(rig.post(magnus(), "stop that", true).await, Response::Ok);
    Rig::until_state(&mut notices, |s| *s == ThreadState::Idle).await;
    assert_eq!(
        rig.kinds(),
        vec![
            EventKind::UserMessage,
            EventKind::UserMessage, // magnus's interrupt, in the log first
            EventKind::Interrupted,
            EventKind::TurnEnded,
            EventKind::AssistantMessage,
            EventKind::TurnEnded,
        ]
    );
    let events = rig.log();
    let i: InterruptedPayload = serde_json::from_value(events[2].payload.clone()).unwrap();
    assert_eq!(i.by, Some(magnus()));
    let seen = rig.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    let last = texts(&seen[1]);
    assert!(last.iter().any(|t| t == "magnus: stop that"), "{last:?}");
    assert!(
        last.iter().any(|t| t.contains("interrupted by magnus")),
        "{last:?}"
    );
}

#[tokio::test]
async fn an_interrupt_while_awaiting_approval_denies_with_the_interrupter() {
    let rig = rig(
        vec![
            Some(vec![slow("c1", 0), tool_use()]),
            Some(vec![text("ok"), done()]),
        ],
        false,
    );
    let (_, _, mut notices) = rig.subscribe(0).await;
    rig.post(steve(), "one", false).await;
    let seen = Rig::until_state(
        &mut notices,
        |s| matches!(s, ThreadState::AwaitingApproval { call_id, .. } if call_id == "c1"),
    )
    .await;
    assert!(seen.iter().any(
        |n| matches!(n, Notice::Event { event, .. } if event.kind == EventKind::PermissionRequested)
    ));
    // A wrong answer kind is refused; then magnus interrupts.
    let (reply, rx) = oneshot::channel();
    rig.mailbox
        .send(Mail::Answer {
            by: magnus(),
            call_id: "c1".into(),
            text: "x".into(),
            reply,
        })
        .unwrap();
    assert!(matches!(rx.await.unwrap(), Response::Refused { .. }));
    rig.post(magnus(), "no", true).await;
    Rig::until_state(&mut notices, |s| *s == ThreadState::Idle).await;
    let events = rig.log();
    let decided = events
        .iter()
        .find(|e| e.kind == EventKind::PermissionDecided)
        .unwrap();
    let d: PermissionDecidedPayload = serde_json::from_value(decided.payload.clone()).unwrap();
    assert_eq!((d.allow, d.reason.as_deref()), (false, Some("interrupted")));
    assert_eq!(decided.author, magnus());
    // A late decision is refused.
    assert!(matches!(
        rig.decide(magnus(), "c1", true).await,
        Response::Refused { .. }
    ));
}

fn ask(id: &str) -> ProviderEvent {
    ProviderEvent::ToolCall(ToolCall {
        id: id.into(),
        name: "ask_human".into(),
        args: json!({"question": "which colour?"}),
    })
}

#[tokio::test]
async fn an_answer_to_ask_human_is_the_answerers_event() {
    let rig = rig(
        vec![
            Some(vec![ask("q1"), tool_use()]),
            Some(vec![text("blue it is"), done()]),
        ],
        false,
    );
    let (_, _, mut notices) = rig.subscribe(0).await;
    rig.post(steve(), "pick one", false).await;
    Rig::until_state(
        &mut notices,
        |s| matches!(s, ThreadState::AwaitingHuman { call_id, .. } if call_id == "q1"),
    )
    .await;
    let r = rig
        .send(|reply| Mail::Answer {
            by: magnus(),
            call_id: "q1".into(),
            text: "blue".into(),
            reply,
        })
        .await;
    assert_eq!(r, Response::Ok);
    Rig::until_state(&mut notices, |s| *s == ThreadState::Idle).await;
    let events = rig.log();
    let result = events
        .iter()
        .find(|e| e.kind == EventKind::ToolResult)
        .unwrap();
    assert_eq!(result.author, magnus(), "the answer is magnus's event");
    assert_eq!(result.payload["content"], "blue");
    // The continuation ran and its own results are the system's.
    assert_eq!(
        rig.kinds(),
        vec![
            EventKind::UserMessage,
            EventKind::AssistantMessage,
            EventKind::ToolResult,
            EventKind::TurnEnded,
            EventKind::AssistantMessage,
            EventKind::TurnEnded,
        ]
    );
}

#[tokio::test]
async fn a_decision_resumes_the_turn_and_a_late_subscriber_catches_up() {
    let rig = rig(
        vec![
            Some(vec![slow("c1", 0), tool_use()]),
            Some(vec![text("ok"), done()]),
        ],
        false,
    );
    let (_, _, mut notices) = rig.subscribe(0).await;
    rig.post(steve(), "one", false).await;
    Rig::until_state(&mut notices, |s| {
        matches!(s, ThreadState::AwaitingApproval { .. })
    })
    .await;
    // A second subscriber joining now sees the state and every event so far.
    let (state, kinds, mut late) = rig.subscribe(2).await;
    assert!(matches!(state, ThreadState::AwaitingApproval { .. }));
    assert_eq!(kinds, vec![EventKind::PermissionRequested]);
    assert_eq!(rig.decide(magnus(), "c1", true).await, Response::Ok);
    Rig::until_state(&mut notices, |s| *s == ThreadState::Idle).await;
    Rig::until_state(&mut late, |s| *s == ThreadState::Idle).await;
    let events = rig.log();
    let decided = events
        .iter()
        .find(|e| e.kind == EventKind::PermissionDecided)
        .unwrap();
    assert_eq!(decided.author, magnus());
    assert_eq!(
        rig.kinds(),
        vec![
            EventKind::UserMessage,
            EventKind::AssistantMessage,
            EventKind::PermissionRequested,
            EventKind::PermissionDecided,
            EventKind::ToolResult,
            EventKind::AssistantMessage,
            EventKind::TurnEnded,
        ]
    );
    // Idle: a decide is refused, the mode and a report answer, pin lands.
    assert!(matches!(
        rig.decide(magnus(), "c9", true).await,
        Response::Refused { .. }
    ));
    assert_eq!(
        rig.send(|reply| Mail::SetMode {
            mode: Mode::Auto,
            reply
        })
        .await,
        Response::Ok
    );
    assert!(matches!(
        rig.send(|reply| Mail::Report {
            kind: ReportKind::Cost,
            reply
        })
        .await,
        Response::Text { text } if text.contains("no renderer")
    ));
    assert_eq!(
        rig.send(|reply| Mail::Pin {
            author: steve(),
            text: "Use Swedish.".into(),
            reply
        })
        .await,
        Response::Ok
    );
    assert_eq!(rig.kinds().last(), Some(&EventKind::Pinned));
    // The mode change was broadcast before the pin landed.
    assert!(matches!(
        late.recv().await,
        Some(Notice::Mode { mode, .. }) if mode == "auto"
    ));
    assert!(matches!(
        late.recv().await,
        Some(Notice::Event { event, .. }) if event.kind == EventKind::Pinned
    ));
}

#[tokio::test]
async fn a_resumed_open_turn_is_continued_before_the_first_mail() {
    // A log that ended mid-turn: user message, assistant with a call, no
    // result. The actor repairs it and continues on its own.
    let dir = tempfile::tempdir().unwrap();
    let thread = ulid::Ulid::generate();
    {
        let mut log = ThreadLog::open(dir.path(), thread).unwrap();
        log.append(aigentic_runtime::aigentic_log::NewEvent {
            kind: EventKind::UserMessage,
            author: steve(),
            payload: serde_json::to_value(UserMessagePayload::new(vec![ContentBlock::Text(
                "go".into(),
            )]))
            .unwrap(),
            parent_event: None,
        })
        .unwrap();
        log.append(aigentic_runtime::aigentic_log::NewEvent {
            kind: EventKind::AssistantMessage,
            author: Author::Agent(aigentic_runtime::aigentic_core::AgentId("worker".into())),
            payload: json!({"blocks": [{"type": "tool_call", "id": "c1", "name": "slow", "args": {"ms": 0}}]}),
            parent_event: None,
        })
        .unwrap();
    }
    let log = ThreadLog::open(dir.path(), thread).unwrap();
    let runtime = Runtime::new(
        Box::new(Gated {
            script: Mutex::new(vec![Some(vec![text("recovered"), done()])].into()),
            seen: Arc::new(Mutex::new(Vec::new())),
        }),
        ToolRegistry::empty(),
        log,
        aigentic_runtime::aigentic_core::AgentId("worker".into()),
    );
    let (actor, mailbox) = ThreadActor::new(runtime, None, Arc::new(NoReports)).unwrap();
    let task = tokio::spawn(actor.run());
    // Subscribe after the continuation had time to finish.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (notices, _rx) = mpsc::unbounded_channel();
    let (reply, reply_rx) = oneshot::channel();
    mailbox
        .send(Mail::Subscribe {
            from_seq: 0,
            notices,
            reply,
        })
        .unwrap();
    let (state, events, _mode) = reply_rx.await.unwrap();
    assert_eq!(state, ThreadState::Idle);
    assert_eq!(
        events.iter().map(|e| e.kind).collect::<Vec<_>>(),
        vec![
            EventKind::UserMessage,
            EventKind::AssistantMessage,
            EventKind::ToolResult, // synthetic, from resume
            EventKind::Interrupted,
            EventKind::AssistantMessage, // "recovered"
            EventKind::TurnEnded,
        ]
    );
    drop(mailbox);
    task.await.unwrap();
}
