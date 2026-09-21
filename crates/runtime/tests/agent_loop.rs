//! Drives the loop with a scripted provider and a recording tool, then
//! asserts the exact event sequence in the log.

use std::time::Duration;

use aigentic_core::{
    AgentId, Author, Budget, ContentBlock, EventKind, ProviderError, ProviderEvent, Role, ToolCall,
};
use aigentic_log::{AssistantMessagePayload, ToolResultPayload, TurnEndedPayload};
use aigentic_runtime::{RuntimeError, Signal};
use serde_json::json;

mod common;
use common::*;

#[tokio::test]
async fn tool_call_round_trip_produces_the_exact_event_sequence() {
    let mut h = harness(
        vec![
            vec![
                ProviderEvent::TextDelta("Let me ".into()),
                ProviderEvent::TextDelta("check.".into()),
                ProviderEvent::ToolCall(call("call_1", "a")),
                ProviderEvent::ToolCall(call("call_2", "b")),
                usage(10, 5),
                done("tool_calls"),
            ],
            vec![
                ProviderEvent::TextDelta("Both echoed.".into()),
                usage(30, 4),
                done("stop"),
            ],
        ],
        Some("Be terse."),
    );
    let mut deltas = String::new();
    let mut signalled = Vec::new();
    let outcome = h
        .runtime
        .run_turn(
            steve(),
            vec![ContentBlock::Text("hi".into())],
            &mut |s| match s {
                Signal::TextDelta(t) => deltas.push_str(t),
                Signal::Event(e) => signalled.push(e.kind),
                Signal::ToolCallStarted(_) => {}
            },
        )
        .await
        .unwrap();

    assert_eq!(outcome.reason, "done");
    assert_eq!((outcome.iterations, outcome.tokens), (2, 49));
    assert_eq!(deltas, "Let me check.Both echoed.");

    let events = h.runtime.log().read_all().unwrap();
    let kinds: Vec<EventKind> = events.iter().map(|e| e.kind).collect();
    assert_eq!(
        kinds,
        vec![
            EventKind::UserMessage,
            EventKind::AssistantMessage,
            EventKind::ToolResult,
            EventKind::ToolResult,
            EventKind::AssistantMessage,
            EventKind::TurnEnded,
        ]
    );
    assert_eq!(signalled, kinds, "every appended event is signalled");
    assert_eq!(
        events.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![0, 1, 2, 3, 4, 5]
    );

    assert_eq!(events[0].author, steve());
    assert_eq!(
        events[0].payload,
        json!({"blocks": [{"type": "text", "text": "hi"}]})
    );

    assert_eq!(events[1].author, Author::Agent(AgentId("worker".into())));
    let asst: AssistantMessagePayload = serde_json::from_value(events[1].payload.clone()).unwrap();
    assert_eq!(
        asst.blocks,
        vec![
            ContentBlock::Text("Let me check.".into()),
            ContentBlock::ToolCall(call("call_1", "a")),
            ContentBlock::ToolCall(call("call_2", "b")),
        ]
    );
    assert_eq!(
        asst.usage
            .map(|u| (u.input_tokens, u.output_tokens, u.estimated)),
        Some((10, 5, false))
    );

    for (event, id, content) in [
        (&events[2], "call_1", "echo: a"),
        (&events[3], "call_2", "echo: b"),
    ] {
        assert_eq!(event.author, Author::System);
        assert_eq!(event.parent_event, Some(events[1].id));
        let r: ToolResultPayload = serde_json::from_value(event.payload.clone()).unwrap();
        assert_eq!(
            (
                r.result.id.as_str(),
                r.result.content.as_str(),
                r.result.is_error
            ),
            (id, content, false)
        );
    }
    assert_eq!(
        h.calls.lock().unwrap().as_slice(),
        &[json!({"msg": "a"}), json!({"msg": "b"})]
    );

    let asst: AssistantMessagePayload = serde_json::from_value(events[4].payload.clone()).unwrap();
    assert_eq!(asst.blocks, vec![ContentBlock::Text("Both echoed.".into())]);
    let end: TurnEndedPayload = serde_json::from_value(events[5].payload.clone()).unwrap();
    assert_eq!(end.reason, "done");

    // Context: instructions first, then the projection; the second call sees the tool results.
    let seen = h.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0][0].role, Role::System);
    assert_eq!(
        seen[0][0].blocks,
        vec![ContentBlock::Text("Be terse.".into())]
    );
    assert_eq!(seen[0].len(), 2);
    let roles: Vec<Role> = seen[1].iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        vec![
            Role::System,
            Role::User,
            Role::Assistant,
            Role::Tool,
            Role::Tool
        ]
    );
}

#[tokio::test]
async fn budget_stop_is_an_event_with_the_reason() {
    let forever = || vec![ProviderEvent::ToolCall(call("c", "x")), done("tool_calls")];
    let mut h = harness(vec![forever(), forever(), forever()], None);
    h.runtime = h.runtime.with_budget(Budget {
        max_iterations: 2,
        max_tokens: u64::MAX,
        max_wall_time: Duration::from_secs(60),
    });

    let outcome = h
        .runtime
        .run_turn(steve(), vec![], &mut |_| {})
        .await
        .unwrap();
    assert_eq!(outcome.reason, "max_iterations");
    assert_eq!(outcome.iterations, 2);
    assert_eq!(
        outcome.tokens, 28,
        "no usage reported: count_tokens estimate for input and output, per call"
    );

    let events = h.runtime.log().read_all().unwrap();
    let kinds: Vec<EventKind> = events.iter().map(|e| e.kind).collect();
    assert_eq!(
        kinds,
        vec![
            EventKind::UserMessage,
            EventKind::AssistantMessage,
            EventKind::ToolResult,
            EventKind::AssistantMessage,
            EventKind::ToolResult,
            EventKind::TurnEnded,
        ]
    );
    let end: TurnEndedPayload = serde_json::from_value(events[5].payload.clone()).unwrap();
    assert_eq!(end.reason, "max_iterations");
    for i in [1, 3] {
        let asst: AssistantMessagePayload =
            serde_json::from_value(events[i].payload.clone()).unwrap();
        assert_eq!(
            asst.usage
                .map(|u| (u.input_tokens, u.output_tokens, u.estimated)),
            Some((7, 7, true))
        );
    }
}

#[tokio::test]
async fn token_budget_stops_after_the_call_that_crosses_it() {
    let mut h = harness(
        vec![vec![
            ProviderEvent::ToolCall(call("c", "x")),
            usage(900, 200),
            done("tool_calls"),
        ]],
        None,
    );
    h.runtime = h.runtime.with_budget(Budget {
        max_iterations: 10,
        max_tokens: 1000,
        max_wall_time: Duration::from_secs(60),
    });
    let outcome = h
        .runtime
        .run_turn(steve(), vec![], &mut |_| {})
        .await
        .unwrap();
    assert_eq!(
        (outcome.reason.as_str(), outcome.tokens),
        ("max_tokens", 1100)
    );
    // Tools ran before the stop: the call has its result, so the log resumes.
    let kinds: Vec<EventKind> = h
        .runtime
        .log()
        .read_all()
        .unwrap()
        .iter()
        .map(|e| e.kind)
        .collect();
    assert_eq!(
        kinds,
        vec![
            EventKind::UserMessage,
            EventKind::AssistantMessage,
            EventKind::ToolResult,
            EventKind::TurnEnded,
        ]
    );
}

#[tokio::test]
async fn wall_time_budget_is_named_in_the_turn_ended_event() {
    let mut h = harness(
        vec![vec![
            ProviderEvent::ToolCall(call("c", "x")),
            done("tool_calls"),
        ]],
        None,
    );
    h.runtime = h.runtime.with_budget(Budget {
        max_iterations: 10,
        max_tokens: u64::MAX,
        max_wall_time: Duration::ZERO,
    });
    let outcome = h
        .runtime
        .run_turn(steve(), vec![], &mut |_| {})
        .await
        .unwrap();
    assert_eq!(outcome.reason, "max_wall_time");
    let events = h.runtime.log().read_all().unwrap();
    let end: TurnEndedPayload =
        serde_json::from_value(events.last().unwrap().payload.clone()).unwrap();
    assert_eq!(end.reason, "max_wall_time");
}

#[tokio::test]
async fn unknown_tool_and_tool_error_become_error_results_not_crashes() {
    let mut h = harness(
        vec![
            vec![
                ProviderEvent::ToolCall(ToolCall {
                    id: "u".into(),
                    name: "nope".into(),
                    args: json!({}),
                }),
                ProviderEvent::ToolCall(ToolCall {
                    id: "e".into(),
                    name: "echo".into(),
                    args: json!({}),
                }),
                done("tool_calls"),
            ],
            vec![ProviderEvent::TextDelta("ok".into()), done("stop")],
        ],
        None,
    );
    h.runtime
        .run_turn(steve(), vec![], &mut |_| {})
        .await
        .unwrap();
    let events = h.runtime.log().read_all().unwrap();
    let r: ToolResultPayload = serde_json::from_value(events[2].payload.clone()).unwrap();
    assert!(r.result.is_error && r.result.content.contains("unknown tool: nope"));
    let r: ToolResultPayload = serde_json::from_value(events[3].payload.clone()).unwrap();
    assert!(r.result.is_error && r.result.content.contains("msg missing"));
    assert_eq!(events.last().unwrap().kind, EventKind::TurnEnded);
}

#[tokio::test]
async fn provider_error_ends_the_turn_with_an_event_and_an_error() {
    let mut h = harness(
        vec![vec![
            ProviderEvent::TextDelta("partial".into()),
            ProviderEvent::Error(ProviderError::Http {
                status: 500,
                body: "boom".into(),
            }),
        ]],
        None,
    );
    let err = h
        .runtime
        .run_turn(steve(), vec![], &mut |_| {})
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        RuntimeError::Provider(ProviderError::Http { status: 500, .. })
    ));
    let events = h.runtime.log().read_all().unwrap();
    let kinds: Vec<EventKind> = events.iter().map(|e| e.kind).collect();
    assert_eq!(kinds, vec![EventKind::UserMessage, EventKind::TurnEnded]);
    let end: TurnEndedPayload = serde_json::from_value(events[1].payload.clone()).unwrap();
    assert!(
        end.reason.starts_with("provider_error: http 500"),
        "{}",
        end.reason
    );
}

#[tokio::test]
async fn a_second_turn_resumes_from_the_log() {
    let mut h = harness(
        vec![
            vec![ProviderEvent::TextDelta("one".into()), done("stop")],
            vec![ProviderEvent::TextDelta("two".into()), done("stop")],
        ],
        None,
    );
    h.runtime
        .run_turn(
            steve(),
            vec![ContentBlock::Text("first".into())],
            &mut |_| {},
        )
        .await
        .unwrap();
    h.runtime
        .run_turn(
            steve(),
            vec![ContentBlock::Text("second".into())],
            &mut |_| {},
        )
        .await
        .unwrap();
    let seen = h.seen.lock().unwrap();
    let roles: Vec<Role> = seen[1].iter().map(|m| m.role).collect();
    assert_eq!(roles, vec![Role::User, Role::Assistant, Role::User]);
    assert_eq!(h.runtime.log().len(), 6);
}
