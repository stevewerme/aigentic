//! Issue #33: a message posted while a turn runs reaches the agent at
//! its next step. One posted while the model is replying is held, then
//! appended where the model first reads it — after that reply's last
//! tool result and before the next call, with `steer` — never between
//! an assistant message and that message's own tool results. If the
//! turn ends first (a final reply, an interrupt, a budget, a provider
//! error) it is appended before `turn_ended` without `steer`, and the
//! turn the actor starts next answers it. Replaying the log gives the
//! live run its contexts.

mod common;

use std::path::Path;
use std::time::Duration;

use aigentic_core::{
    AgentId, Author, BoxFuture, ContentBlock, Event, EventKind, Message, ProviderError,
    ProviderEvent, RiskClass, Tool, ToolCall, ToolError, ToolOutput, UserId,
};
use aigentic_log::{ThreadLog, TurnEndedPayload, UserMessagePayload};
use aigentic_runtime::{
    CancelToken, Layers, Outbox, Prefix, Queued, Runtime, RuntimeError, TurnOutcome, build_context,
    inbox,
};
use aigentic_tools::ToolRegistry;
use common::{Seen, Step, done, gate, steve};
use serde_json::json;

/// Sleeps for its `ms` argument, so a message can be queued while the
/// turn is inside the tool.
struct Slow;

impl Tool for Slow {
    fn name(&self) -> &str {
        "slow"
    }
    fn description(&self) -> &str {
        "sleeps, then answers"
    }
    fn schema(&self) -> schemars::schema::RootSchema {
        schemars::schema_for!(u64)
    }
    fn risk_class(&self) -> RiskClass {
        RiskClass::Safe
    }
    fn call(&self, args: serde_json::Value) -> BoxFuture<'_, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let ms = args.as_u64().unwrap_or(400);
            tokio::time::sleep(Duration::from_millis(ms)).await;
            Ok(ToolOutput {
                content: "slept".into(),
                is_error: false,
            })
        })
    }
}

fn slow_call(id: &str, ms: u64) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "slow".into(),
        args: json!(ms),
    }
}

struct Rig {
    seen: Seen,
    dir: tempfile::TempDir,
    thread: ulid::Ulid,
    outbox: Outbox,
    /// Fires the interrupt, as the actor does for `Post { interrupt:
    /// true }`.
    cancel: CancelToken,
    task: tokio::task::JoinHandle<Result<TurnOutcome, RuntimeError>>,
}

/// A turn on its own task with a live inbox: the test reads the log and
/// posts while the turn runs.
fn rig(script: Vec<Vec<ProviderEvent>>) -> Rig {
    spawn_rig(steps(script), false)
}

/// `rig` over a gated script: a reply is a list of steps, and a
/// [`common::gate`] step parks the stream mid-reply, so the test can
/// post between two of its deltas.
fn gated_rig(script: Vec<Vec<Step>>) -> Rig {
    spawn_rig(script, false)
}

/// `gated_rig`, and when the first turn is over the task runs the turn
/// the actor starts for a message no model call read (`after_turn`'s
/// `Continue`), so the test sees the context that answers it.
fn gated_rig_answering(script: Vec<Vec<Step>>) -> Rig {
    spawn_rig(script, true)
}

fn steps(script: Vec<Vec<ProviderEvent>>) -> Vec<Vec<Step>> {
    script
        .into_iter()
        .map(|reply| reply.into_iter().map(Step::Event).collect())
        .collect()
}

fn spawn_rig(script: Vec<Vec<Step>>, answer_next: bool) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let thread = ulid::Ulid::generate();
    let log = ThreadLog::open(dir.path(), thread).unwrap();
    let (provider, seen) = common::gated(script);
    let mut tools = ToolRegistry::empty();
    tools.register(Box::new(Slow)).unwrap();
    let mut runtime = Runtime::new(provider, tools, log, AgentId("worker".into()))
        .with_layers(Layers::global_instructions("Be terse."));
    let (outbox, mut inbox) = inbox();
    let cancel = CancelToken::never();
    let token = cancel.clone();
    let task = tokio::spawn(async move {
        let first = runtime
            .run_turn_until(
                steve(),
                vec![ContentBlock::Text("go".into())],
                &token,
                &mut inbox,
                &mut |_| {},
            )
            .await;
        if !answer_next {
            return first;
        }
        // A fresh token, as the actor's per-turn one: a fired token
        // stays fired.
        let fresh = CancelToken::never();
        let _ = runtime
            .continue_turn_until(&fresh, &mut inbox, &mut |_| {})
            .await;
        first
    });
    Rig {
        seen,
        dir,
        thread,
        outbox,
        cancel,
        task,
    }
}

/// The log of a finished rig.
fn log_of(dir: &Path, thread: ulid::Ulid) -> Vec<Event> {
    ThreadLog::open(dir, thread).unwrap().read_all().unwrap()
}

/// Replaying the log gives the live run its contexts: for every model
/// call that produced an assistant message, the context built from the
/// log as it stood — everything before that message — is the request's
/// tail. The prefix is empty (its system messages sit in front of the
/// tail); the naming rule is `build_context`'s, the one the live run
/// used. `requests` drops the calls that left no assistant message
/// behind (an interrupt, a provider error) from the front.
fn assert_replay(dir: &Path, thread: ulid::Ulid, requests: &[Vec<Message>]) {
    let events = ThreadLog::open(dir, thread).unwrap().read_all().unwrap();
    let ends: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, e)| e.kind == EventKind::AssistantMessage)
        .map(|(i, _)| i)
        .collect();
    assert_eq!(
        requests.len(),
        ends.len(),
        "one request per model call that answered"
    );
    for (i, end) in ends.iter().enumerate() {
        let body = build_context(&Prefix::default(), &events[..*end]).unwrap();
        let n = body.len();
        assert_eq!(
            &requests[i][requests[i].len() - n..],
            &body[..],
            "request {i} is the context the log as it stood builds"
        );
    }
}

/// Reads the log fresh each round until `pred` holds, yielding between
/// rounds so the turn's task can run.
async fn wait_until(dir: &Path, thread: ulid::Ulid, pred: impl Fn(&[Event]) -> bool) {
    for _ in 0..1000 {
        let events = ThreadLog::open(dir, thread).unwrap().read_all().unwrap();
        if pred(&events) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    panic!("the turn never produced it");
}

fn magnus(text: &str) -> Queued {
    Queued {
        author: Author::User(UserId("magnus".into())),
        blocks: vec![ContentBlock::Text(text.into())],
    }
}

fn texts(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .flat_map(|m| m.blocks.iter())
        .filter_map(|b| match b {
            ContentBlock::Text(t) => Some(t.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_message_queued_during_a_tool_call_is_in_the_very_next_request() {
    let rig = rig(vec![
        vec![
            ProviderEvent::ToolCall(slow_call("c1", 400)),
            done("tool_calls"),
        ],
        vec![ProviderEvent::TextDelta("adjusted.".into()), done("stop")],
    ]);

    // The turn is inside the tool call, then magnus posts.
    wait_until(rig.dir.path(), rig.thread, |events| {
        events.iter().any(|e| e.kind == EventKind::AssistantMessage)
    })
    .await;
    assert!(rig.outbox.send(magnus("use the other file")));

    let outcome = rig.task.await.unwrap().unwrap();
    assert_eq!(outcome.reason, "done");
    assert_eq!(outcome.iterations, 2);

    let seen = rig.seen.lock().unwrap();
    // The first request could not have seen it.
    assert!(
        !texts(&seen[0])
            .iter()
            .any(|t| t.contains("use the other file"))
    );
    // The second request ends with the tool result and then the
    // message, named as magnus's: the next model call sees it.
    let msgs = &seen[1];
    let n = msgs.len();
    assert!(
        matches!(&msgs[n - 2].blocks[0], ContentBlock::ToolResult(r) if r.id == "c1"),
        "the tool result precedes the message"
    );
    assert!(
        matches!(&msgs[n - 1].blocks[0], ContentBlock::Text(t) if t.contains("magnus")
            && t.contains("use the other file")),
        "the message is the last thing the model sees, {:?}",
        texts(msgs)
    );

    // The log: appended mid-turn, steering.
    let events = ThreadLog::open(rig.dir.path(), rig.thread)
        .unwrap()
        .read_all()
        .unwrap();
    let p: UserMessagePayload = serde_json::from_value(
        events
            .iter()
            .find(|e| e.kind == EventKind::UserMessage && e.author != steve())
            .unwrap()
            .payload
            .clone(),
    )
    .unwrap();
    assert!(p.mid_turn);
    assert!(p.steer);
}

#[tokio::test]
async fn a_message_queued_while_results_are_pending_appears_after_the_last_one() {
    let rig = rig(vec![
        vec![
            ProviderEvent::ToolCall(slow_call("c1", 100)),
            ProviderEvent::ToolCall(slow_call("c2", 400)),
            done("tool_calls"),
        ],
        vec![ProviderEvent::TextDelta("adjusted.".into()), done("stop")],
    ]);

    // magnus posts while c1 is still running: both results pending.
    wait_until(rig.dir.path(), rig.thread, |events| {
        events.iter().any(|e| e.kind == EventKind::AssistantMessage)
    })
    .await;
    assert!(rig.outbox.send(magnus("use the other file")));

    rig.task.await.unwrap().unwrap();

    let seen = rig.seen.lock().unwrap();
    let msgs = &seen[1];
    // Both results first, the message after the last of them: never
    // inside the pair.
    let results: Vec<&str> = msgs
        .iter()
        .flat_map(|m| m.blocks.iter())
        .filter_map(|b| match b {
            ContentBlock::ToolResult(r) => Some(r.id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(results, vec!["c1", "c2"]);
    let at = msgs
        .iter()
        .position(|m| {
            m.blocks
                .iter()
                .any(|b| matches!(b, ContentBlock::Text(t) if t.contains("use the other file")))
        })
        .expect("the message is in the second request");
    let last_result = msgs
        .iter()
        .rposition(|m| {
            m.blocks
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolResult(_)))
        })
        .expect("results are in the second request");
    assert!(at > last_result);
    assert_eq!(at, msgs.len() - 1, "nothing after the message");
}

#[tokio::test]
async fn replaying_the_log_gives_the_live_run_its_context() {
    // steve steers his own turn: one human author, so the projection is
    // the request verbatim, no names to add.
    let rig = rig(vec![
        vec![
            ProviderEvent::ToolCall(slow_call("c1", 400)),
            done("tool_calls"),
        ],
        vec![ProviderEvent::TextDelta("adjusted.".into()), done("stop")],
    ]);
    wait_until(rig.dir.path(), rig.thread, |events| {
        events.iter().any(|e| e.kind == EventKind::AssistantMessage)
    })
    .await;
    assert!(rig.outbox.send(Queued {
        author: steve(),
        blocks: vec![ContentBlock::Text("use the other file".into())],
    }));

    let outcome = rig.task.await.unwrap().unwrap();
    assert_eq!(outcome.reason, "done");

    let seen = rig.seen.lock().unwrap();
    assert_replay(rig.dir.path(), rig.thread, &seen);
}

#[tokio::test]
async fn a_message_posted_mid_stream_of_a_reply_with_a_tool_call_steers_the_next_step() {
    let (gate, mut parked) = gate();
    let rig = gated_rig(vec![
        vec![
            Step::Event(ProviderEvent::TextDelta("working".into())),
            Step::Gate(gate),
            Step::Event(ProviderEvent::ToolCall(slow_call("c1", 0))),
            Step::Event(done("tool_calls")),
        ],
        vec![
            Step::Event(ProviderEvent::TextDelta("adjusted.".into())),
            Step::Event(done("stop")),
        ],
    ]);

    // magnus posts between two deltas of the reply.
    parked.wait().await;
    assert!(rig.outbox.send(magnus("use the other file")));
    parked.open();

    let outcome = rig.task.await.unwrap().unwrap();
    assert_eq!(outcome.reason, "done");
    assert_eq!(outcome.iterations, 2);

    {
        let seen = rig.seen.lock().unwrap();
        // The call that was streaming could not have seen it.
        assert!(
            !texts(&seen[0])
                .iter()
                .any(|t| t.contains("use the other file"))
        );
        // The next call: the tool result, then the message, and
        // nothing after it.
        let msgs = &seen[1];
        let n = msgs.len();
        assert!(
            matches!(&msgs[n - 2].blocks[0], ContentBlock::ToolResult(r) if r.id == "c1"),
            "the tool result precedes the message"
        );
        assert!(
            matches!(&msgs[n - 1].blocks[0], ContentBlock::Text(t) if t.contains("magnus")
                && t.contains("use the other file")),
            "the message is the last thing the model sees, {:?}",
            texts(msgs)
        );
    }

    // The log: after that reply's tool result and before the call that
    // reads it, steering — where the model first sees it.
    let events = log_of(rig.dir.path(), rig.thread);
    let at = events
        .iter()
        .position(|e| e.kind == EventKind::UserMessage && e.author != steve())
        .expect("magnus's message is in the log");
    let result = events
        .iter()
        .position(|e| e.kind == EventKind::ToolResult)
        .expect("the tool result is in the log");
    let next_call = events
        .iter()
        .rposition(|e| e.kind == EventKind::AssistantMessage)
        .expect("the second reply is in the log");
    assert!(result < at, "after the reply's tool result");
    assert!(at < next_call, "before the call that reads it");
    let p: UserMessagePayload = serde_json::from_value(events[at].payload.clone()).unwrap();
    assert!(p.mid_turn);
    assert!(p.steer);

    assert_replay(rig.dir.path(), rig.thread, &rig.seen.lock().unwrap());
}

#[tokio::test]
async fn a_message_posted_mid_stream_of_a_final_reply_is_answered_by_the_next_turn() {
    let (gate, mut parked) = gate();
    let rig = gated_rig_answering(vec![
        vec![
            Step::Event(ProviderEvent::TextDelta("all done".into())),
            Step::Gate(gate),
            Step::Event(ProviderEvent::TextDelta(".".into())),
            Step::Event(done("stop")),
        ],
        vec![
            Step::Event(ProviderEvent::TextDelta("adjusted.".into())),
            Step::Event(done("stop")),
        ],
    ]);

    parked.wait().await;
    assert!(rig.outbox.send(magnus("use the other file")));
    parked.open();

    let outcome = rig.task.await.unwrap().unwrap();
    assert_eq!(outcome.reason, "done");

    {
        let seen = rig.seen.lock().unwrap();
        // The reply that was streaming never saw it.
        assert!(
            !texts(&seen[0])
                .iter()
                .any(|t| t.contains("use the other file"))
        );
        // The turn the actor starts for it reads it as the last thing.
        let msgs = &seen[1];
        assert!(
            matches!(&msgs.last().unwrap().blocks[0], ContentBlock::Text(t) if t.contains("magnus")
                && t.contains("use the other file")),
            "the message is the last thing the model sees, {:?}",
            texts(msgs)
        );
    }

    // The log: mid_turn without `steer`, before the turn_ended that
    // ended the turn — so the existing next-turn path picks it up.
    let events = log_of(rig.dir.path(), rig.thread);
    let at = events
        .iter()
        .position(|e| e.kind == EventKind::UserMessage && e.author != steve())
        .expect("magnus's message is in the log");
    let end = events
        .iter()
        .position(|e| e.kind == EventKind::TurnEnded)
        .expect("the turn ended");
    assert!(at < end, "in the log before the turn ended");
    let p: UserMessagePayload = serde_json::from_value(events[at].payload.clone()).unwrap();
    assert!(p.mid_turn);
    assert!(!p.steer);

    assert_replay(rig.dir.path(), rig.thread, &rig.seen.lock().unwrap());
}

#[tokio::test]
async fn a_message_posted_mid_stream_before_an_interrupt_is_not_lost() {
    let (gate, mut parked) = gate();
    let rig = gated_rig_answering(vec![
        vec![
            Step::Event(ProviderEvent::TextDelta("working".into())),
            Step::Gate(gate),
            Step::Event(ProviderEvent::TextDelta(" more".into())),
            Step::Event(done("stop")),
        ],
        vec![
            Step::Event(ProviderEvent::TextDelta("adjusted.".into())),
            Step::Event(done("stop")),
        ],
    ]);

    // magnus posts mid-stream, then steve interrupts before the reply
    // finishes.
    parked.wait().await;
    assert!(rig.outbox.send(magnus("wait, use the other file")));
    rig.cancel.cancel(steve());

    let outcome = rig.task.await.unwrap().unwrap();
    assert_eq!(outcome.reason, "interrupted");

    // In the log, mid_turn without `steer`, before the turn_ended the
    // interrupt wrote — not lost with the inbox.
    let events = log_of(rig.dir.path(), rig.thread);
    let at = events
        .iter()
        .position(|e| e.kind == EventKind::UserMessage && e.author != steve())
        .expect("magnus's message is in the log");
    let end = events
        .iter()
        .position(|e| e.kind == EventKind::TurnEnded)
        .expect("the turn ended");
    assert!(at < end, "in the log before the turn ended");
    let p: UserMessagePayload = serde_json::from_value(events[at].payload.clone()).unwrap();
    assert!(p.mid_turn);
    assert!(!p.steer);

    // And the turn the actor starts next answers it: the message is in
    // its context, before the note the interrupt left.
    let seen = rig.seen.lock().unwrap();
    assert_eq!(seen.len(), 2, "the interrupted call, then the answer");
    let msgs = &seen[1];
    let at = msgs
        .iter()
        .position(|m| {
            m.blocks
                .iter()
                .any(|b| matches!(b, ContentBlock::Text(t) if t.contains("use the other file")))
        })
        .expect("the next turn reads it");
    let note = msgs
        .iter()
        .position(|m| {
            m.blocks
                .iter()
                .any(|b| matches!(b, ContentBlock::Text(t) if t.contains("interrupted by steve")))
        })
        .expect("the interrupt's note is in it too");
    assert!(at < note, "the message, then the note, {:?}", texts(msgs));
    assert_replay(rig.dir.path(), rig.thread, &seen[1..]);
}

#[tokio::test]
async fn a_message_posted_mid_stream_before_a_provider_error_is_not_lost() {
    let (gate, mut parked) = gate();
    let rig = gated_rig_answering(vec![
        vec![
            Step::Event(ProviderEvent::TextDelta("working".into())),
            Step::Gate(gate),
            Step::Event(ProviderEvent::Error(ProviderError::Transport(
                "connection dropped".into(),
            ))),
        ],
        vec![
            Step::Event(ProviderEvent::TextDelta("adjusted.".into())),
            Step::Event(done("stop")),
        ],
    ]);

    parked.wait().await;
    assert!(rig.outbox.send(magnus("use the other file")));
    parked.open();

    let error = rig.task.await.unwrap().unwrap_err();
    assert!(
        matches!(error, RuntimeError::Provider(ProviderError::Transport(_))),
        "{error:?}"
    );

    // In the log before the turn_ended the error wrote, mid_turn
    // without `steer`: no call of that turn read it.
    let events = log_of(rig.dir.path(), rig.thread);
    let at = events
        .iter()
        .position(|e| e.kind == EventKind::UserMessage && e.author != steve())
        .expect("magnus's message is in the log");
    let end = events
        .iter()
        .position(|e| e.kind == EventKind::TurnEnded)
        .expect("the turn ended");
    assert!(at < end, "in the log before the turn ended");
    let p: UserMessagePayload = serde_json::from_value(events[at].payload.clone()).unwrap();
    assert!(p.mid_turn);
    assert!(!p.steer);
    let ended: TurnEndedPayload = serde_json::from_value(events[end].payload.clone()).unwrap();
    assert_eq!(
        ended.reason,
        "provider_error: transport error: connection dropped"
    );

    // And the turn the actor starts next answers it.
    let seen = rig.seen.lock().unwrap();
    assert_eq!(seen.len(), 2, "the call that errored, then the answer");
    assert!(
        matches!(&seen[1].last().unwrap().blocks[0], ContentBlock::Text(t) if t.contains("magnus")
            && t.contains("use the other file")),
        "the next turn reads it, {:?}",
        texts(&seen[1])
    );
    assert_replay(rig.dir.path(), rig.thread, &seen[1..]);
}
