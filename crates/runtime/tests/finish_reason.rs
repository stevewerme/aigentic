//! What a reply's own end means (issue #96): a stream cut off by the
//! provider is a provider failure and nothing of it is kept, while a
//! reply stopped by the model's output limit is recorded with its reason
//! and runs none of the calls it wrote.
//!
//! Every expected value here comes from the runtime's own constants or
//! from the log it wrote — never a hand-counted literal.

use aigentic_core::{
    AgentId, ContentBlock, Event, EventKind, Message, ProviderError, ProviderEvent,
};
use aigentic_log::{AssistantMessagePayload, ThreadLog, ToolResultPayload, TurnEndedPayload};
use aigentic_runtime::{
    CompactionSettings, LENGTH_STOP, MAX_TOKENS_STOP, NOT_RUN_LENGTH, Runtime, RuntimeError,
};

mod common;
use common::{call, done, harness, scripted, steve};

/// A call the provider sent, as the stream carries it.
fn sent(id: &str, msg: &str) -> ProviderEvent {
    ProviderEvent::ToolCall(call(id, msg))
}

/// The two reasons a provider gives for a reply the model's own output
/// limit stopped: OpenAI-compatible says `length`, Anthropic `max_tokens`.
const LENGTH_REASONS: [&str; 2] = [LENGTH_STOP, MAX_TOKENS_STOP];

/// The `assistant_message` payloads a log holds, in order.
fn assistant_messages(events: &[Event]) -> Vec<AssistantMessagePayload> {
    events
        .iter()
        .filter(|e| e.kind == EventKind::AssistantMessage)
        .map(|e| serde_json::from_value(e.payload.clone()).expect("an assistant payload"))
        .collect()
}

/// The `tool_result` payloads a log holds, in order.
fn tool_results(events: &[Event]) -> Vec<ToolResultPayload> {
    events
        .iter()
        .filter(|e| e.kind == EventKind::ToolResult)
        .map(|e| serde_json::from_value(e.payload.clone()).expect("a result payload"))
        .collect()
}

/// The last `turn_ended` payload a log holds.
fn turn_ended(events: &[Event]) -> TurnEndedPayload {
    serde_json::from_value(
        events
            .iter()
            .rev()
            .find(|e| e.kind == EventKind::TurnEnded)
            .expect("the turn ended")
            .payload
            .clone(),
    )
    .expect("a turn_ended payload")
}

/// T2: a stream that ends without a reason ends the turn as a provider
/// failure — its reply may stop mid-sentence — with no assistant message
/// and no call run, even when a call arrived before the cut.
#[tokio::test]
async fn a_cut_reply_is_a_provider_failure_with_nothing_kept() {
    let mut h = harness(
        vec![
            vec![
                ProviderEvent::TextDelta("I'll start by".into()),
                done(aigentic_core::CUT_STREAM),
            ],
            // A call that arrived whole, and then the cut: it must not run.
            vec![sent("c1", "do it"), done(aigentic_core::CUT_STREAM)],
        ],
        None,
    );

    for _ in 0..2 {
        let err = h
            .runtime
            .run_turn(steve(), vec![ContentBlock::Text("go".into())], &mut |_| {})
            .await
            .expect_err("a cut reply is not a success");
        assert!(
            matches!(err, RuntimeError::Provider(ProviderError::Cut)),
            "{err:?}"
        );
    }

    let events = h.runtime.log().read_all().unwrap();
    assert!(
        assistant_messages(&events).is_empty(),
        "a cut reply left an assistant message behind"
    );
    assert!(
        tool_results(&events).is_empty(),
        "a cut reply left a tool result behind"
    );
    assert!(
        h.calls.lock().unwrap().is_empty(),
        "a tool ran from a cut reply: {:?}",
        h.calls.lock().unwrap()
    );
    let end = turn_ended(&events);
    assert!(end.reason.starts_with("provider_error:"), "{}", end.reason);
    assert!(
        end.error
            .as_ref()
            .is_some_and(|e| e.plain_line().contains("cut off")),
        "{:?}",
        end.error
    );
}

/// T2 at the compaction seam: a cut summary is a failure, so it never
/// replaces the originals it was meant to summarise.
#[tokio::test]
async fn a_cut_compaction_summary_appends_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let log = ThreadLog::open(dir.path(), ulid::Ulid::generate()).unwrap();
    let (provider, _seen) = scripted(vec![
        // Two ordinary turns, then the summary call, which is cut.
        vec![ProviderEvent::TextDelta("one".into()), done("stop")],
        vec![ProviderEvent::TextDelta("two".into()), done("stop")],
        vec![
            ProviderEvent::TextDelta("Earlier turns: ".into()),
            done(aigentic_core::CUT_STREAM),
        ],
    ]);
    let mut rt = Runtime::new(
        provider,
        aigentic_tools::ToolRegistry::empty(),
        log,
        AgentId("worker".into()),
    )
    .with_compaction(CompactionSettings {
        keep_turns: 1,
        ..aigentic_runtime::DEFAULT_COMPACTION
    });
    for i in 0..2 {
        rt.run_turn(
            steve(),
            vec![ContentBlock::Text(format!("t{i}"))],
            &mut |_| {},
        )
        .await
        .unwrap();
    }

    let err = rt
        .compact_now(&mut |_| {})
        .await
        .expect_err("a cut summary is a failure");
    assert!(
        matches!(err, RuntimeError::Provider(ProviderError::Cut)),
        "{err:?}"
    );
    assert!(
        !rt.log()
            .read_all()
            .unwrap()
            .iter()
            .any(|e| e.kind == EventKind::Compacted),
        "a cut summary was appended"
    );
}

/// T3: a length stop records its reason, answers every call it wrote with
/// a not-run result, runs none of them, and ends the turn `length` — so
/// the next request still builds. Both spellings of a length stop.
#[tokio::test]
async fn a_length_stop_runs_no_tool_and_answers_every_call() {
    for reason in LENGTH_REASONS {
        let mut h = harness(
            vec![
                vec![sent("c1", "first"), sent("c2", "second"), done(reason)],
                vec![ProviderEvent::TextDelta("done".into()), done("stop")],
            ],
            None,
        );

        let outcome = h
            .runtime
            .run_turn(steve(), vec![ContentBlock::Text("go".into())], &mut |_| {})
            .await
            .unwrap();
        assert_eq!(outcome.reason, LENGTH_STOP, "{reason}");
        assert!(
            h.calls.lock().unwrap().is_empty(),
            "a length stop ran a tool: {:?}",
            h.calls.lock().unwrap()
        );

        let events = h.runtime.log().read_all().unwrap();
        let messages = assistant_messages(&events);
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0].finish_reason.as_deref(),
            Some(reason),
            "the provider's own reason is recorded"
        );
        let results = tool_results(&events);
        assert_eq!(results.len(), 2, "{results:?}");
        let ids: Vec<&str> = results.iter().map(|r| r.result.id.as_str()).collect();
        assert_eq!(ids, vec!["c1", "c2"]);
        for r in &results {
            assert_eq!(r.result.content, NOT_RUN_LENGTH);
            assert!(r.result.is_error);
        }
        assert_eq!(turn_ended(&events).reason, LENGTH_STOP);

        // The next request builds: the runtime's projection holds a
        // message back until every call has a result, so a request that
        // carries both results means the log stayed whole.
        let second = h
            .runtime
            .run_turn(
                steve(),
                vec![ContentBlock::Text("go on".into())],
                &mut |_| {},
            )
            .await
            .unwrap();
        assert_eq!(second.reason, "done");
        let seen = h.seen.lock().unwrap();
        let carried: Vec<String> = seen
            .last()
            .expect("a request went out")
            .iter()
            .flat_map(|m: &Message| m.blocks.iter())
            .filter_map(|b| match b {
                ContentBlock::ToolResult(r) => Some(r.content.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(carried, vec![NOT_RUN_LENGTH.to_owned(); 2]);
    }
}

/// T3 without calls: the reply is recorded with its reason and the turn
/// still ends `length`.
#[tokio::test]
async fn a_length_stop_with_no_call_ends_length() {
    for reason in LENGTH_REASONS {
        let mut h = harness(
            vec![vec![
                ProviderEvent::TextDelta("half a".into()),
                done(reason),
            ]],
            None,
        );
        let outcome = h
            .runtime
            .run_turn(steve(), vec![ContentBlock::Text("go".into())], &mut |_| {})
            .await
            .unwrap();
        assert_eq!(outcome.reason, LENGTH_STOP, "{reason}");

        let events = h.runtime.log().read_all().unwrap();
        let messages = assistant_messages(&events);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].finish_reason.as_deref(), Some(reason));
        assert!(tool_results(&events).is_empty());
        assert_eq!(turn_ended(&events).reason, LENGTH_STOP);
    }
}

/// T4: a clean reply records the provider's own reason and the turn ends
/// `done`, as it always did.
#[tokio::test]
async fn a_clean_reply_records_its_reason_and_ends_done() {
    for reason in ["stop", "end_turn"] {
        let mut h = harness(
            vec![vec![ProviderEvent::TextDelta("hi".into()), done(reason)]],
            None,
        );
        let outcome = h
            .runtime
            .run_turn(steve(), vec![ContentBlock::Text("go".into())], &mut |_| {})
            .await
            .unwrap();
        assert_eq!(outcome.reason, "done");
        let events = h.runtime.log().read_all().unwrap();
        let messages = assistant_messages(&events);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].finish_reason.as_deref(), Some(reason));
        assert_eq!(turn_ended(&events).reason, "done");
    }
}
