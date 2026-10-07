//! Issue #102: a stale checklist in the open turn gets the model a short
//! reminder at the very end of its context. The reminder is computed
//! from the log for each call and never appended, so the log, the
//! projection, replay and the cached prefix are all unchanged, and the
//! next call's context is the previous one without the reminder.

mod common;

use aigentic_core::{
    Author, ContentBlock, Event, EventKind, Message, ProviderEvent, Role, ToolCall,
};
use aigentic_runtime::harness_tools::TASK_REMINDER_CALLS;
use aigentic_runtime::{Prefix, build_context};
use common::{call, done, harness, steve};
use serde_json::{Value, json};

const INSTRUCTIONS: &str = "Be terse.";

/// Three steps, one done: a checklist with work left, so it can go stale.
fn unfinished() -> Value {
    json!([
        {"text": "read the plan", "state": "done"},
        {"text": "write the code", "state": "active"},
        {"text": "run the gate", "state": "pending"},
    ])
}

/// The same three steps, all done.
fn finished() -> Value {
    json!([
        {"text": "read the plan", "state": "done"},
        {"text": "write the code", "state": "done"},
        {"text": "run the gate", "state": "done"},
    ])
}

fn update(id: &str, tasks: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "update_tasks".into(),
        args: json!({"tasks": tasks}),
    }
}

fn echo(i: usize) -> ToolCall {
    call(&format!("e{i}"), &format!("x{i}"))
}

/// The reminder's exact text, the one the spec fixes.
fn text(done: usize, total: usize, since: usize) -> String {
    format!(
        "Your checklist reads {done}/{total} done, last updated {since} tool calls ago. \
         If a step has started or finished since, call update_tasks now."
    )
}

/// One reply per echo, plus the two checklists and the closing stop:
/// the first `update_tasks`, `echoes` echoes, the all-done one, two
/// more echoes, then the stop.
fn replies() -> usize {
    echoes() + 5
}

/// The number of echoes between the first checklist and the second.
fn echoes() -> usize {
    TASK_REMINDER_CALLS + 2
}

/// One turn: an unfinished checklist, `echoes` echo calls, an all-done
/// checklist, two more echoes, then a plain stop. Reply 0 is the first
/// `update_tasks`, replies `1..=echoes` are the echoes, reply
/// `echoes + 1` is the all-done `update_tasks`.
fn script() -> Vec<Vec<ProviderEvent>> {
    let mut script = vec![vec![
        ProviderEvent::ToolCall(update("u1", unfinished())),
        done("tool_calls"),
    ]];
    for i in 0..echoes() {
        script.push(vec![ProviderEvent::ToolCall(echo(i)), done("tool_calls")]);
    }
    script.push(vec![
        ProviderEvent::ToolCall(update("u2", finished())),
        done("tool_calls"),
    ]);
    for i in 0..2 {
        script.push(vec![
            ProviderEvent::ToolCall(echo(100 + i)),
            done("tool_calls"),
        ]);
    }
    script.push(vec![
        ProviderEvent::TextDelta("all done".into()),
        done("stop"),
    ]);
    script
}

/// The reminder on a request, when its very last message is one.
fn reminder_of(message: &Message) -> Option<String> {
    if message.role != Role::User || message.author != Author::System {
        return None;
    }
    match message.blocks.as_slice() {
        [ContentBlock::Text(t)] => Some(t.clone()),
        _ => None,
    }
}

struct Ran {
    events: Vec<Event>,
    requests: Vec<Vec<Message>>,
}

async fn run() -> Ran {
    let mut h = harness(script(), Some(INSTRUCTIONS));
    let outcome = h
        .runtime
        .run_turn(steve(), vec![ContentBlock::Text("go".into())], &mut |_| {})
        .await
        .unwrap();
    assert_eq!(outcome.reason, "done", "the turn ends on the scripted stop");
    let events = h.runtime.log().read_all().unwrap();
    let requests = h.seen.lock().unwrap().clone();
    assert_eq!(
        requests.len(),
        replies(),
        "one request per scripted reply, and no retries"
    );
    Ran { events, requests }
}

/// T2, through a turn: the reminder appears only once the checklist has
/// been stale for `TASK_REMINDER_CALLS` completed calls, and the very
/// last message of every request carrying it is the reminder.
#[tokio::test]
async fn the_reminder_rides_at_the_end_of_the_context_until_the_checklist_is_done() {
    let ran = run().await;
    let mut carried = 0;
    for (r, request) in ran.requests.iter().enumerate() {
        // The echo results before request `r`: every reply before it,
        // but the checklists, and nothing after the all-done one.
        let since = r.saturating_sub(1).min(echoes());
        let stale = (1..=echoes() + 1).contains(&r) && since >= TASK_REMINDER_CALLS;
        let last = request.last().expect("every request has messages");
        match reminder_of(last) {
            Some(got) => {
                assert!(stale, "request {r} carries the reminder, but is not stale");
                assert_eq!(got, text(1, 3, since), "request {r}'s reminder");
                assert_eq!(last.role, Role::User);
                assert_eq!(last.author, Author::System);
                carried += 1;
            }
            None => assert!(
                !stale,
                "request {r} should carry the reminder (since {since})"
            ),
        }
        let non_reminders = request.len() - usize::from(reminder_of(last).is_some());
        assert!(
            !request[..non_reminders]
                .iter()
                .any(|m| reminder_of(m).is_some()),
            "request {r} carries the reminder only at the very end"
        );
    }
    assert_eq!(
        carried,
        echoes() + 1 - TASK_REMINDER_CALLS,
        "the requests from the K-th call on, and no others"
    );

    // No event holds it: the reminder is never appended to the log.
    for event in &ran.events {
        let json = serde_json::to_string(event).unwrap();
        assert!(
            !json.contains("Your checklist reads"),
            "an event holds the reminder: {json}"
        );
    }

    // The log is what the script implies. Eviction may add its own
    // kinds along the way (the double reports a 1000-token window); the
    // reminder's assertions do not depend on them.
    let kinds: Vec<EventKind> = ran.events.iter().map(|e| e.kind).collect();
    assert_eq!(kinds.first(), Some(&EventKind::UserMessage));
    assert_eq!(kinds.last(), Some(&EventKind::TurnEnded), "turn_ended last");
    assert_eq!(
        kinds
            .iter()
            .filter(|k| **k == EventKind::AssistantMessage)
            .count(),
        replies(),
        "one assistant message per scripted reply"
    );
    assert_eq!(
        kinds
            .iter()
            .filter(|k| **k == EventKind::ToolResult)
            .count(),
        replies() - 1,
        "one tool result per call (the closing stop makes none)"
    );
    for kind in &kinds {
        assert!(
            matches!(
                kind,
                EventKind::UserMessage
                    | EventKind::AssistantMessage
                    | EventKind::ToolResult
                    | EventKind::TurnEnded
                    | EventKind::ContextEvicted
                    | EventKind::ContextSaturated
                    | EventKind::ResultsStubbed
                    | EventKind::Compacted
                    | EventKind::ProviderRetried
            ),
            "unexpected event kind {kind:?}"
        );
    }
}

/// T3, the prefix: every request with its reminder removed is exactly
/// the context the log as it stood builds, and each is a prefix of the
/// next, so the cache holds everything but the reminder.
#[tokio::test]
async fn each_request_is_the_context_the_log_builds_without_the_reminder() {
    let ran = run().await;
    let prefix = Prefix::instructions(Some(INSTRUCTIONS));
    let ends: Vec<usize> = ran
        .events
        .iter()
        .enumerate()
        .filter(|(_, e)| e.kind == EventKind::AssistantMessage)
        .map(|(i, _)| i)
        .collect();
    assert_eq!(
        ends.len(),
        ran.requests.len(),
        "one request per model call that answered"
    );

    let stripped: Vec<Vec<Message>> = ran
        .requests
        .iter()
        .map(|request| {
            let mut messages = request.clone();
            if messages.last().is_some_and(|m| reminder_of(m).is_some()) {
                messages.pop();
            }
            messages
        })
        .collect();

    for (i, end) in ends.iter().enumerate() {
        let body = build_context(&prefix, &ran.events[..*end]).unwrap();
        assert_eq!(
            stripped[i], body,
            "request {i}, its reminder removed, is the context the log built"
        );
    }

    for i in 0..stripped.len() - 1 {
        let between = &ran.events[ends[i] + 1..ends[i + 1]];
        let rewritten = between.iter().any(|e| {
            matches!(
                e.kind,
                EventKind::ContextEvicted | EventKind::ResultsStubbed | EventKind::Compacted
            )
        });
        if rewritten {
            continue;
        }
        assert!(
            stripped[i].len() <= stripped[i + 1].len(),
            "request {i} is no longer than request {}",
            i + 1
        );
        assert_eq!(
            stripped[i][..],
            stripped[i + 1][..stripped[i].len()],
            "request {i}, its reminder removed, is a prefix of request {}",
            i + 1
        );
    }
}
