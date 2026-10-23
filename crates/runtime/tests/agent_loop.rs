//! Drives the loop with a scripted provider and a recording tool, then
//! asserts the exact event sequence in the log.

use std::time::Duration;

use aigentic_core::{
    AgentId, Author, Budget, ContentBlock, EventKind, ProviderError, ProviderEvent, Role, Tool,
    ToolCall,
};
use aigentic_log::{AssistantMessagePayload, ThreadLog, ToolResultPayload, TurnEndedPayload};
use aigentic_runtime::{DEFAULT_BUDGET, MAX_USD, Prices, Runtime, RuntimeError, Signal};
use aigentic_tools::ToolRegistry;
use serde_json::json;
use std::sync::{Arc, Mutex};

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
                Signal::ToolCallStarted(_)
                | Signal::Waiting(_)
                | Signal::Usage(_)
                | Signal::JobApproved { .. }
                | Signal::Note(_) => {}
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
    h.runtime.set_budget(Budget {
        max_iterations: 2,
        max_tokens: u64::MAX,
        max_wall_time: Duration::from_secs(60),
        max_cost_usd: None,
        cache_read_price_ratio: 0.25,
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
    h.runtime.set_budget(Budget {
        max_iterations: 10,
        max_tokens: 1000,
        max_wall_time: Duration::from_secs(60),
        max_cost_usd: None,
        cache_read_price_ratio: 0.25,
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
    h.runtime.set_budget(Budget {
        max_iterations: 10,
        max_tokens: u64::MAX,
        max_wall_time: Duration::ZERO,
        max_cost_usd: None,
        cache_read_price_ratio: 0.25,
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

/// What the runtime's own arithmetic charges for a call whose reply
/// reports `usage(i, o)`: a test's cap comes from here, so the cap and
/// the stamped cost are one computation and no dollar figure is typed.
fn cost_of(prices: &Prices, i: u64, o: u64) -> f64 {
    prices.cost_usd(&aigentic_log::Usage::reported(aigentic_core::Usage {
        input_tokens: i,
        output_tokens: o,
        ..Default::default()
    }))
}

/// A reply that calls the `echo` tool and reports `usage(i, o)`. A tool
/// call rather than a plain stop, because a reply that stops ends the
/// turn before the budget is consulted: only a reply that keeps the loop
/// going lets a test reach the dollar check at the next iteration.
fn priced_tool_call(id: &str, i: u64, o: u64) -> Vec<ProviderEvent> {
    vec![
        ProviderEvent::ToolCall(call(id, "x")),
        usage(i, o),
        done("tool_calls"),
    ]
}

/// The new cap ends a turn with a reason of its own, and the call that
/// reached the cap runs to its end: the check runs before the next call,
/// not inside one.
#[tokio::test]
async fn a_turn_ends_at_its_dollar_cap() {
    let prices = Prices {
        input: 3.0,
        cache_read: 0.5,
        cache_write: 4.0,
        output: 15.0,
    };
    let cap = cost_of(&prices, 1_000, 500);
    assert!(cap > 0.0, "the price table charges for this call");

    let mut h = harness(
        vec![
            priced_tool_call("c", 1_000, 500),
            // Never called: the first one has spent the cap.
            vec![ProviderEvent::TextDelta("after".into()), done("stop")],
        ],
        None,
    );
    h.runtime.set_pricing("tensorx", Some(prices));
    h.runtime.set_budget(Budget {
        max_cost_usd: Some(cap),
        ..DEFAULT_BUDGET
    });

    let outcome = h
        .runtime
        .run_turn(steve(), vec![], &mut |_| {})
        .await
        .unwrap();
    assert_eq!(outcome.reason, MAX_USD);
    assert_eq!(
        h.seen.lock().unwrap().len(),
        1,
        "the call that spent the cap ran; the next was never made"
    );

    let events = h.runtime.log().read_all().unwrap();
    let ends: Vec<_> = events
        .iter()
        .filter(|e| e.kind == EventKind::TurnEnded)
        .collect();
    assert_eq!(ends.len(), 1, "one turn, one end");
    let end: TurnEndedPayload = serde_json::from_value(ends[0].payload.clone()).unwrap();
    assert_eq!(
        end.reason, MAX_USD,
        "the same reason string the runner's step record carries"
    );

    let u = costs(&events);
    assert_eq!(u[0], cap, "the turn's spend is what its one call cost");
    assert_eq!(
        u[0],
        prices.cost_usd(&aigentic_log::Usage::reported(aigentic_core::Usage {
            input_tokens: 1_000,
            output_tokens: 500,
            ..Default::default()
        })),
        "the logged cost is the formula over the numbers the reply reported"
    );
}

/// The cost each logged call carries, in the log's order: the spend the
/// cap is compared against, read back from the log rather than counted.
fn costs(events: &[aigentic_core::Event]) -> Vec<f64> {
    events
        .iter()
        .filter(|e| e.kind == EventKind::AssistantMessage)
        .map(|e| {
            serde_json::from_value::<AssistantMessagePayload>(e.payload.clone())
                .unwrap()
                .usage
                .expect("a stamped usage line")
                .cost_usd
                .expect("a priced call")
        })
        .collect()
}

/// The cap is the turn's own spend, whatever the script holds next: a
/// turn whose first call reached it stops there, and one whose cap is two
/// calls' price stops after the second.
#[tokio::test]
async fn a_turn_stops_when_it_spends_its_cap() {
    let prices = Prices {
        input: 3.0,
        cache_read: 0.5,
        cache_write: 4.0,
        output: 15.0,
    };
    let first = cost_of(&prices, 1_000, 500);
    let second = cost_of(&prices, 2_000, 100);

    // One call's price as the cap: the turn ends after it.
    let mut h = harness(
        vec![
            priced_tool_call("c1", 1_000, 500),
            vec![ProviderEvent::TextDelta("after".into()), done("stop")],
        ],
        None,
    );
    h.runtime.set_pricing("tensorx", Some(prices));
    h.runtime.set_budget(Budget {
        max_cost_usd: Some(first),
        ..DEFAULT_BUDGET
    });
    let outcome = h
        .runtime
        .run_turn(steve(), vec![], &mut |_| {})
        .await
        .unwrap();
    assert_eq!(outcome.reason, MAX_USD);
    assert_eq!(h.seen.lock().unwrap().len(), 1);

    // Both prices as the cap: two calls run, and the third is never made.
    let mut h = harness(
        vec![
            priced_tool_call("c1", 1_000, 500),
            priced_tool_call("c2", 2_000, 100),
            vec![ProviderEvent::TextDelta("after".into()), done("stop")],
        ],
        None,
    );
    h.runtime.set_pricing("tensorx", Some(prices));
    h.runtime.set_budget(Budget {
        max_cost_usd: Some(first + second),
        ..DEFAULT_BUDGET
    });
    let outcome = h
        .runtime
        .run_turn(steve(), vec![], &mut |_| {})
        .await
        .unwrap();
    assert_eq!(outcome.reason, MAX_USD);
    assert_eq!(
        h.seen.lock().unwrap().len(),
        2,
        "the second call is the one that reached the cap"
    );
    let events = h.runtime.log().read_all().unwrap();
    assert_eq!(
        costs(&events),
        vec![first, second],
        "the spend that met the cap is the two calls' prices"
    );
}

/// The dollar arm sits behind the three older ones: the same script and
/// the same cap are named by the iteration limit when that limit is met
/// on the same check, and by the cap when there is iteration room.
#[tokio::test]
async fn a_dollar_cap_does_not_displace_the_older_reasons() {
    let prices = Prices {
        input: 3.0,
        cache_read: 0.5,
        cache_write: 4.0,
        output: 15.0,
    };
    let per = cost_of(&prices, 1_000, 500);
    let capped = || Budget {
        max_cost_usd: Some(per * 2.0),
        ..DEFAULT_BUDGET
    };
    let script = || {
        vec![
            priced_tool_call("c1", 1_000, 500),
            priced_tool_call("c2", 1_000, 500),
            priced_tool_call("c3", 1_000, 500),
        ]
    };
    let spend = |h: &Harness| {
        costs(&h.runtime.log().read_all().unwrap())
            .iter()
            .sum::<f64>()
    };

    // No figure at all: the iteration limit names the stop.
    let forever = || vec![ProviderEvent::ToolCall(call("c", "x")), done("tool_calls")];
    let mut h = harness(vec![forever(), forever(), forever()], None);
    h.runtime.set_budget(Budget {
        max_iterations: 2,
        max_cost_usd: None,
        ..DEFAULT_BUDGET
    });
    let outcome = h
        .runtime
        .run_turn(steve(), vec![], &mut |_| {})
        .await
        .unwrap();
    assert_eq!(outcome.reason, "max_iterations");
    assert_ne!(outcome.reason, MAX_USD);

    // The same cap with iteration room: the dollar reason, after the two
    // calls whose prices it names.
    let mut h = harness(script(), None);
    h.runtime.set_pricing("tensorx", Some(prices));
    h.runtime.set_budget(capped());
    let outcome = h
        .runtime
        .run_turn(steve(), vec![], &mut |_| {})
        .await
        .unwrap();
    assert_eq!(outcome.reason, MAX_USD);
    assert_eq!(spend(&h), per * 2.0, "two calls met the cap");

    // The same script and cap with the iteration limit met on the same
    // check: the limit is named, and the cap was met as well.
    let mut h = harness(script(), None);
    h.runtime.set_pricing("tensorx", Some(prices));
    h.runtime.set_budget(Budget {
        max_iterations: 2,
        ..capped()
    });
    let outcome = h
        .runtime
        .run_turn(steve(), vec![], &mut |_| {})
        .await
        .unwrap();
    assert_eq!(outcome.reason, "max_iterations");
    assert_ne!(outcome.reason, MAX_USD);
    assert_eq!(
        spend(&h),
        per * 2.0,
        "the cap was spent and the count still named the stop"
    );
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
    // The error comes before any content, so nothing of this reply was
    // ever shown and no retry is earned (issue #114).
    let mut h = harness(
        vec![vec![ProviderEvent::Error(ProviderError::Http {
            status: 500,
            body: "boom".into(),
        })]],
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

/// A retry the provider reports is appended where it happened
/// (issue #31): between the user's message and the reply it delayed,
/// system-authored, carrying the attempt, the total and the wait.
#[tokio::test]
async fn a_provider_retry_is_logged_as_it_arrives() {
    let mut h = harness(
        vec![vec![
            ProviderEvent::Retried {
                attempt: 1,
                retries: 3,
                reason: "not answering".into(),
                wait: Duration::from_secs(1),
            },
            ProviderEvent::TextDelta("hi".into()),
            usage(5, 1),
            done("stop"),
        ]],
        None,
    );
    let mut signalled = Vec::new();
    h.runtime
        .run_turn(steve(), vec![ContentBlock::Text("hi".into())], &mut |s| {
            if let Signal::Event(e) = s {
                signalled.push(e.kind);
            }
        })
        .await
        .unwrap();

    let events = h.runtime.log().read_all().unwrap();
    let kinds: Vec<EventKind> = events.iter().map(|e| e.kind).collect();
    assert_eq!(
        kinds,
        vec![
            EventKind::UserMessage,
            EventKind::ProviderRetried,
            EventKind::AssistantMessage,
            EventKind::TurnEnded,
        ]
    );
    let retry = &events[1];
    assert_eq!(retry.author, Author::System);
    assert_eq!(retry.payload["attempt"], json!(1));
    assert_eq!(retry.payload["retries"], json!(3));
    assert_eq!(retry.payload["reason"], json!("not answering"));
    assert_eq!(retry.payload["wait_ms"], json!(1000));
    assert_eq!(signalled, kinds, "every appended event is signalled");
}

/// Issue #31: with prices set, the logged call carries the profile, the
/// model label, a measured latency and a cost that the formula
/// reproduces from the recorded numbers; the runtime does not invent a
/// price when none is set.
#[tokio::test]
async fn a_priced_call_is_stamped_and_recomputable() {
    let prices = Prices {
        input: 3.0,
        cache_read: 0.3,
        cache_write: 3.75,
        output: 15.0,
    };
    let mut h = harness(
        vec![vec![
            // Content first, so the call has a time-to-first-token: a
            // usage-only stream never starts a block (issue #31).
            ProviderEvent::TextDelta("hi".into()),
            // 1M of each kind: the cost must be the per-million sum.
            ProviderEvent::Usage(aigentic_core::Usage {
                input_tokens: 1_000_000,
                output_tokens: 1_000_000,
                cache_read_tokens: 1_000_000,
                cache_write_tokens: 1_000_000,
                ..Default::default()
            }),
            done("stop"),
        ]],
        None,
    );
    h.runtime.set_pricing("tensorx", Some(prices));
    h.runtime.set_effort(Some("50".into()));
    h.runtime
        .run_turn(steve(), vec![ContentBlock::Text("hi".into())], &mut |_| {})
        .await
        .unwrap();

    let events = h.runtime.log().read_all().unwrap();
    let message = events
        .iter()
        .find(|e| e.kind == EventKind::AssistantMessage)
        .expect("an assistant message");
    let u = serde_json::from_value::<AssistantMessagePayload>(message.payload.clone())
        .unwrap()
        .usage
        .expect("a usage line");

    assert_eq!(u.profile.as_deref(), Some("tensorx"));
    assert_eq!(u.effort.as_deref(), Some("50"), "the effort it was set to");
    assert!(!u.model.as_deref().unwrap_or_default().is_empty());
    assert!(u.latency_ms.is_some(), "latency is measured");
    assert!(u.ttft_ms.is_some(), "a scripted stream has a first event");

    // The formula, recomputed from the numbers that were recorded: the
    // answer the test asserts is the rule `Prices::cost_usd` states, not
    // a literal copied out of the implementation.
    let expected = prices.cost_usd(&u);
    assert_eq!(u.cost_usd, Some(expected));
    let per_million = (3.0 + 15.0 + 0.3 + 3.75) * 1_000_000.0 / 1_000_000.0;
    assert!(
        (expected - per_million).abs() < 1e-9,
        "{expected} != {per_million}"
    );
}

/// Without a price table nothing is claimed: `cost_usd` is `None` even
/// though the call is stamped in every other way.
#[tokio::test]
async fn an_unpriced_call_has_no_cost() {
    let mut h = harness(vec![vec![usage(10, 2), done("stop")]], None);
    h.runtime.set_pricing("free", None);
    h.runtime
        .run_turn(steve(), vec![ContentBlock::Text("hi".into())], &mut |_| {})
        .await
        .unwrap();
    let events = h.runtime.log().read_all().unwrap();
    let message = events
        .iter()
        .find(|e| e.kind == EventKind::AssistantMessage)
        .unwrap();
    let u = serde_json::from_value::<AssistantMessagePayload>(message.payload.clone())
        .unwrap()
        .usage
        .unwrap();
    assert_eq!(u.profile.as_deref(), Some("free"));
    assert_eq!(u.cost_usd, None);
    assert_eq!(u.effort, None, "no effort set, none claimed");
}

/// The runtime reads the JSONL file once, at open: a turn over a log that
/// was already on disk appends without a second file pass (issue #34).
#[tokio::test]
async fn turn_reads_the_file_only_once() {
    let dir = tempfile::tempdir().unwrap();
    let thread = ulid::Ulid::generate();
    let dirpath = dir.path().to_path_buf();

    // A first runtime seeds the log with a two-step turn.
    let log = ThreadLog::open(dir.path(), thread).unwrap();
    let mut h = harness_with_log(
        vec![
            vec![
                ProviderEvent::TextDelta("Working.".into()),
                ProviderEvent::ToolCall(call("call_1", "a")),
                done("tool_calls"),
            ],
            vec![ProviderEvent::TextDelta("Done.".into()), done("stop")],
        ],
        None,
        dir,
        log,
    );
    h.runtime
        .run_turn(steve(), vec![ContentBlock::Text("hi".into())], &mut |_| {})
        .await
        .unwrap();
    let seeded = h.runtime.log().len();
    assert!(seeded > 0, "the first turn seeded the log");

    // A new runtime over the same directory: the file is read at open...
    let fresh = ThreadLog::open(&dirpath, thread).unwrap();
    assert_eq!(
        fresh.file_reads(),
        1,
        "a healthy open reads the file exactly once"
    );
    let (provider, _seen) = scripted(vec![
        vec![
            ProviderEvent::TextDelta("More.".into()),
            ProviderEvent::ToolCall(call("call_2", "b")),
            done("tool_calls"),
        ],
        vec![ProviderEvent::TextDelta("Finished.".into()), done("stop")],
    ]);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let registry: ToolRegistry = vec![Box::new(EchoTool(calls)) as Box<dyn Tool>].into();
    let mut rt = Runtime::new(provider, registry, fresh, AgentId("worker".into()));
    rt.run_turn(
        steve(),
        vec![ContentBlock::Text("go on".into())],
        &mut |_| {},
    )
    .await
    .unwrap();

    // ...and never again, however many steps the turn takes.
    assert_eq!(
        rt.log().file_reads(),
        1,
        "no step of the turn re-reads the file"
    );
    assert!(rt.log().len() > seeded, "the turn appended events");
}
