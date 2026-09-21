//! One test per row of the resume table in docs/PLAN-phase2.md section 6.

use aigentic_core::{Author, ContentBlock, EventKind, ProviderEvent, ToolCall};
use aigentic_log::{
    AssistantMessagePayload, InterruptedPayload, NewEvent, PolicyRecord, Repair, ThreadLog,
    ToolResultPayload, TurnEndedPayload, UserMessagePayload,
};
use aigentic_runtime::{INTERRUPTED_RESULT, Resumed};
use serde_json::json;

mod common;
use common::*;

fn new_event(kind: EventKind, author: Author, payload: serde_json::Value) -> NewEvent {
    NewEvent {
        kind,
        author,
        payload,
        parent_event: None,
    }
}
fn user_ev(text: &str) -> NewEvent {
    let p = UserMessagePayload {
        blocks: vec![ContentBlock::Text(text.into())],
    };
    new_event(
        EventKind::UserMessage,
        steve(),
        serde_json::to_value(p).unwrap(),
    )
}
fn assistant_ev(blocks: Vec<ContentBlock>) -> NewEvent {
    let p = AssistantMessagePayload {
        blocks,
        usage: None,
    };
    new_event(
        EventKind::AssistantMessage,
        Author::Agent(aigentic_core::AgentId("worker".into())),
        serde_json::to_value(p).unwrap(),
    )
}
fn result_ev(id: &str) -> NewEvent {
    new_event(
        EventKind::ToolResult,
        Author::System,
        json!({"id": id, "content": "echo: ok", "is_error": false}),
    )
}
fn ended_ev() -> NewEvent {
    new_event(
        EventKind::TurnEnded,
        Author::Agent(aigentic_core::AgentId("worker".into())),
        json!({"reason": "done"}),
    )
}
fn call(id: &str) -> ContentBlock {
    ContentBlock::ToolCall(ToolCall {
        id: id.into(),
        name: "echo".into(),
        args: json!({"msg": id}),
    })
}
fn done() -> Vec<ProviderEvent> {
    vec![
        ProviderEvent::TextDelta("finished".into()),
        ProviderEvent::Done {
            finish_reason: "stop".into(),
        },
    ]
}

/// Write `events` to a fresh log and build a harness over it.
fn prepared(events: Vec<NewEvent>, script: Vec<Vec<ProviderEvent>>) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let mut log = ThreadLog::open(dir.path(), ulid::Ulid::generate()).unwrap();
    for e in events {
        log.append(e).unwrap();
    }
    harness_with_log(script, None, dir, log)
}

fn kinds(h: &Harness) -> Vec<EventKind> {
    h.runtime
        .log()
        .read_all()
        .unwrap()
        .iter()
        .map(|e| e.kind)
        .collect()
}

#[tokio::test]
async fn ends_on_turn_ended_is_clean() {
    let mut h = prepared(
        vec![
            user_ev("hi"),
            assistant_ev(vec![ContentBlock::Text("hello".into())]),
            ended_ev(),
        ],
        vec![],
    );
    assert_eq!(h.runtime.resume(None, &mut |_| {}).unwrap(), Resumed::Clean);
    assert_eq!(kinds(&h).len(), 3, "nothing appended");
}

#[tokio::test]
async fn ends_on_user_message_continues_after_a_note() {
    let mut h = prepared(vec![user_ev("hi")], vec![done()]);
    let r = h.runtime.resume(Some(17), &mut |_| {}).unwrap();
    assert_eq!(
        r,
        Resumed::Interrupted {
            after_seq: 0,
            unanswered_calls: 0,
            torn_bytes: Some(17)
        }
    );
    assert_eq!(
        kinds(&h),
        vec![EventKind::UserMessage, EventKind::Interrupted]
    );
    let events = h.runtime.log().read_all().unwrap();
    let p: InterruptedPayload = serde_json::from_value(events[1].payload.clone()).unwrap();
    assert_eq!((p.after_seq, p.unanswered_calls.len()), (0, 0));

    let outcome = h.runtime.continue_turn(&mut |_| {}).await.unwrap();
    assert_eq!(outcome.reason, "done");
    assert_eq!(
        kinds(&h),
        vec![
            EventKind::UserMessage,
            EventKind::Interrupted,
            EventKind::AssistantMessage,
            EventKind::TurnEnded
        ]
    );
    // The model saw the note.
    let seen = h.seen.lock().unwrap();
    assert_eq!(seen[0].len(), 2);
    assert!(matches!(&seen[0][1].blocks[0], ContentBlock::Text(t) if t.contains("interrupted")));
}

#[tokio::test]
async fn unanswered_tool_calls_get_synthetic_error_results() {
    let mut h = prepared(
        vec![
            user_ev("go"),
            assistant_ev(vec![call("c1"), call("c2"), call("c3")]),
            result_ev("c1"),
        ],
        vec![done()],
    );
    let r = h.runtime.resume(None, &mut |_| {}).unwrap();
    assert_eq!(
        r,
        Resumed::Interrupted {
            after_seq: 2,
            unanswered_calls: 2,
            torn_bytes: None
        }
    );
    let events = h.runtime.log().read_all().unwrap();
    assert_eq!(
        events.iter().map(|e| e.kind).collect::<Vec<_>>(),
        vec![
            EventKind::UserMessage,
            EventKind::AssistantMessage,
            EventKind::ToolResult,
            EventKind::ToolResult,
            EventKind::ToolResult,
            EventKind::Interrupted,
        ]
    );
    for (i, id) in [(3, "c2"), (4, "c3")] {
        let ToolResultPayload { result: r, policy } =
            serde_json::from_value(events[i].payload.clone()).unwrap();
        assert_eq!(policy, Some(PolicyRecord::synthetic()));
        assert_eq!(
            (r.id.as_str(), r.is_error, r.content.as_str()),
            (id, true, INTERRUPTED_RESULT)
        );
        assert_eq!(
            events[i].parent_event,
            Some(events[1].id),
            "linked to the assistant message"
        );
        assert_eq!(events[i].author, Author::System);
    }
    let p: InterruptedPayload = serde_json::from_value(events[5].payload.clone()).unwrap();
    assert_eq!(p.unanswered_calls, vec!["c2", "c3"]);

    h.runtime.continue_turn(&mut |_| {}).await.unwrap();
    assert_eq!(kinds(&h).last(), Some(&EventKind::TurnEnded));
    // Every call has a result in what the model saw: three tool messages.
    let seen = h.seen.lock().unwrap();
    let tools = seen[0]
        .iter()
        .filter(|m| m.role == aigentic_core::Role::Tool)
        .count();
    assert_eq!(tools, 3);
}

#[tokio::test]
async fn assistant_with_all_calls_unanswered() {
    let mut h = prepared(
        vec![user_ev("go"), assistant_ev(vec![call("c1")])],
        vec![done()],
    );
    let r = h.runtime.resume(None, &mut |_| {}).unwrap();
    assert!(
        matches!(
            r,
            Resumed::Interrupted {
                unanswered_calls: 1,
                after_seq: 1,
                ..
            }
        ),
        "{r:?}"
    );
    assert_eq!(
        kinds(&h),
        vec![
            EventKind::UserMessage,
            EventKind::AssistantMessage,
            EventKind::ToolResult,
            EventKind::Interrupted
        ]
    );
}

#[tokio::test]
async fn assistant_without_calls_gets_its_lost_turn_ended() {
    let mut h = prepared(
        vec![
            user_ev("hi"),
            assistant_ev(vec![ContentBlock::Text("hello".into())]),
        ],
        vec![],
    );
    assert_eq!(h.runtime.resume(None, &mut |_| {}).unwrap(), Resumed::Clean);
    let events = h.runtime.log().read_all().unwrap();
    assert_eq!(events.last().unwrap().kind, EventKind::TurnEnded);
    let p: TurnEndedPayload =
        serde_json::from_value(events.last().unwrap().payload.clone()).unwrap();
    assert_eq!(p.reason, "resumed");
}

#[tokio::test]
async fn ends_on_a_complete_tool_result_continues() {
    let mut h = prepared(
        vec![
            user_ev("go"),
            assistant_ev(vec![call("c1")]),
            result_ev("c1"),
        ],
        vec![done()],
    );
    let r = h.runtime.resume(None, &mut |_| {}).unwrap();
    assert!(
        matches!(
            r,
            Resumed::Interrupted {
                unanswered_calls: 0,
                after_seq: 2,
                ..
            }
        ),
        "{r:?}"
    );
    assert_eq!(
        kinds(&h),
        vec![
            EventKind::UserMessage,
            EventKind::AssistantMessage,
            EventKind::ToolResult,
            EventKind::Interrupted
        ]
    );
    h.runtime.continue_turn(&mut |_| {}).await.unwrap();
    assert_eq!(kinds(&h).last(), Some(&EventKind::TurnEnded));
}

#[tokio::test]
async fn a_second_crash_after_repair_does_not_stack_notes() {
    let mut h = prepared(
        vec![user_ev("go"), assistant_ev(vec![call("c1")])],
        vec![done()],
    );
    h.runtime.resume(None, &mut |_| {}).unwrap();
    let before = kinds(&h);
    let r = h.runtime.resume(None, &mut |_| {}).unwrap();
    assert!(
        matches!(
            r,
            Resumed::Interrupted {
                unanswered_calls: 0,
                ..
            }
        ),
        "{r:?}"
    );
    assert_eq!(kinds(&h), before, "nothing appended the second time");
}

#[tokio::test]
async fn torn_tail_then_resume_then_continue() {
    let dir = tempfile::tempdir().unwrap();
    let thread = ulid::Ulid::generate();
    let mut log = ThreadLog::open(dir.path(), thread).unwrap();
    log.append(user_ev("go")).unwrap();
    log.append(assistant_ev(vec![call("c1")])).unwrap();
    // Crash mid-append of the tool result.
    let text = std::fs::read_to_string(log.path()).unwrap();
    std::fs::write(
        log.path(),
        format!("{text}{{\"id\":\"01ARZ3NDEKTSV4RRFFQ69G5FAV\",\"seq\":2,\"ki"),
    )
    .unwrap();
    drop(log);

    let (log, cut) = ThreadLog::open_with(dir.path(), thread, Repair::TruncateTornTail).unwrap();
    assert!(cut.unwrap() > 0);
    let mut h = harness_with_log(vec![done()], None, dir, log);
    let r = h.runtime.resume(cut, &mut |_| {}).unwrap();
    assert!(
        matches!(
            r,
            Resumed::Interrupted {
                unanswered_calls: 1,
                torn_bytes: Some(_),
                ..
            }
        ),
        "{r:?}"
    );
    let outcome = h.runtime.continue_turn(&mut |_| {}).await.unwrap();
    assert_eq!(outcome.reason, "done");
    let events = h.runtime.log().read_all().unwrap();
    assert_eq!(
        events.iter().map(|e| e.seq).collect::<Vec<_>>(),
        (0..6).collect::<Vec<u64>>()
    );
}
