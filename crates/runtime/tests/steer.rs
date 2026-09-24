//! Issue #33: a message posted while a turn runs reaches the agent at
//! its next step. It is appended mid-turn with `steer`, the projection
//! emits it where it sits — never between an assistant message and that
//! message's own tool results — so the very next model call sees it, and
//! replaying the log gives the live run's context.

mod common;

use std::path::Path;
use std::time::Duration;

use aigentic_core::{
    AgentId, Author, BoxFuture, ContentBlock, Event, EventKind, Message, ProviderEvent, RiskClass,
    Tool, ToolCall, ToolError, ToolOutput, UserId,
};
use aigentic_log::{ThreadLog, UserMessagePayload, project};
use aigentic_runtime::{
    CancelToken, Layers, Outbox, Queued, Runtime, RuntimeError, TurnOutcome, inbox,
};
use aigentic_tools::ToolRegistry;
use common::{Seen, done, scripted, steve};
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
    task: tokio::task::JoinHandle<Result<TurnOutcome, RuntimeError>>,
}

/// A turn on its own task with a live inbox: the test reads the log and
/// posts while the turn runs.
fn rig(script: Vec<Vec<ProviderEvent>>) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let thread = ulid::Ulid::generate();
    let log = ThreadLog::open(dir.path(), thread).unwrap();
    let (provider, seen) = scripted(script);
    let mut tools = ToolRegistry::empty();
    tools.register(Box::new(Slow)).unwrap();
    let mut runtime = Runtime::new(provider, tools, log, AgentId("worker".into()))
        .with_layers(Layers::global_instructions("Be terse."));
    let (outbox, mut inbox) = inbox();
    let cancel = CancelToken::never();
    let task = tokio::spawn(async move {
        runtime
            .run_turn_until(
                steve(),
                vec![ContentBlock::Text("go".into())],
                &cancel,
                &mut inbox,
                &mut |_| {},
            )
            .await
    });
    Rig {
        seen,
        dir,
        thread,
        outbox,
        task,
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

    let events = ThreadLog::open(rig.dir.path(), rig.thread)
        .unwrap()
        .read_all()
        .unwrap();
    // The log as each request was built: everything before each
    // assistant message.
    let ends: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, e)| e.kind == EventKind::AssistantMessage)
        .map(|(i, _)| i)
        .collect();
    let seen = rig.seen.lock().unwrap();
    assert_eq!(seen.len(), ends.len(), "one request per model call");
    for (i, end) in ends.iter().enumerate() {
        let body = project(&events[..*end]).unwrap().body;
        let n = body.len();
        assert_eq!(
            &seen[i][seen[i].len() - n..],
            &body[..],
            "request {i} is the projection of the log as it stood"
        );
    }
}
