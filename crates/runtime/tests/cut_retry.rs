//! A reply cut off mid-stream is asked again, once (issue #114):
//! nothing of a cut attempt ever ran or was appended, so the harness may
//! simply ask again without the person. These tests hold the boundary
//! the retry owns — after content, once per model call, never on a
//! non-transient failure, never once the turn is cancelled — and every
//! expected value comes from the script or from the log the runtime
//! wrote.

use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aigentic_core::{
    AgentId, Author, BoxFuture, Budget, CUT_STREAM, Capabilities, CompletionRequest, ContentBlock,
    Event, EventKind, Message, Provider, ProviderError, ProviderEvent, RiskClass, Tool, ToolError,
    ToolOutput, UserId,
};
use aigentic_log::{ThreadLog, TurnEndedPayload};
use aigentic_runtime::{CancelToken, Inbox, Runtime, RuntimeError};
use aigentic_tools::ToolRegistry;
use futures_core::Stream;

mod common;
use common::{call, done, harness, steve, usage};

/// The `provider_retried` payloads a log holds, in order.
fn retries(events: &[Event]) -> Vec<aigentic_log::ProviderRetriedPayload> {
    events
        .iter()
        .filter(|e| e.kind == EventKind::ProviderRetried)
        .map(|e| serde_json::from_value(e.payload.clone()).expect("a retried payload"))
        .collect()
}

/// The text of every `assistant_message` that has one, in order: a
/// reply that is only tool calls carries no text and is not counted.
fn assistant_texts(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter(|e| e.kind == EventKind::AssistantMessage)
        .map(|e| {
            let p: aigentic_log::AssistantMessagePayload =
                serde_json::from_value(e.payload.clone()).expect("an assistant payload");
            p.blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text(t) => Some(t.clone()),
                    _ => None,
                })
                .collect::<String>()
        })
        .filter(|t| !t.is_empty())
        .collect()
}

/// The ids of every `tool_result` a log holds, in order.
fn result_ids(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter(|e| e.kind == EventKind::ToolResult)
        .map(|e| {
            let p: aigentic_log::ToolResultPayload =
                serde_json::from_value(e.payload.clone()).expect("a result payload");
            p.result.id
        })
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

/// The whole log as one string, so a test can prove a discarded
/// attempt's text is nowhere in it.
fn log_text(events: &[Event]) -> String {
    serde_json::to_string(events).expect("serialisable")
}

/// A `Usage` fixture's own total, the way `Budget::spent_of` prices it
/// with no cache fields set: uncached input plus output.
fn fixture_total(input: u64, output: u64) -> u64 {
    input + output
}

/// T1: a text-only reply cut off mid-stream is asked again, once: the
/// turn ends `done`, the cut attempt's text is nowhere in the log, one
/// `provider_retried` records the retry, and the turn's own token figure
/// charges both attempts.
#[tokio::test]
async fn a_cut_after_text_is_asked_again_and_the_cut_attempt_is_dropped() {
    let mut h = harness(
        vec![
            vec![
                ProviderEvent::TextDelta("partial".into()),
                usage(100, 10),
                done(CUT_STREAM),
            ],
            vec![
                ProviderEvent::TextDelta("the answer".into()),
                usage(200, 20),
                done("stop"),
            ],
        ],
        None,
    );
    let out = h
        .runtime
        .run_turn(steve(), vec![ContentBlock::Text("go".into())], &mut |_| {})
        .await
        .expect("a retried cut recovers");
    assert_eq!(out.reason, "done");

    let events = h.runtime.log().read_all().unwrap();
    let retried = retries(&events);
    assert_eq!(retried.len(), 1, "one retry: {events:?}");
    assert_eq!(retried[0].attempt, 1);
    assert_eq!(retried[0].retries, 1);
    assert_eq!(retried[0].wait_ms, 0, "no wait: the same request again");
    assert!(
        retried[0].reason.contains("cut off mid-reply"),
        "the reason names the cut: {}",
        retried[0].reason
    );
    assert_eq!(
        assistant_texts(&events),
        vec!["the answer".to_owned()],
        "only the retry's reply is in the log"
    );
    assert!(
        !log_text(&events).contains("partial"),
        "nothing of the cut attempt is kept"
    );
    assert_eq!(
        turn_ended(&events).reason,
        "done",
        "the turn ends on the retry's success"
    );
    // The turn charges the cut attempt too (issue #114 keeps `spent.tokens`
    // charging every attempt): both usage fixtures, not the retry's alone.
    let charged = fixture_total(100, 10) + fixture_total(200, 20);
    assert_eq!(
        out.tokens, charged,
        "the cut attempt's tokens are still counted"
    );
}

/// T2: a reply holding a *complete* tool call and then cut is dropped
/// whole: that call never runs. The retry's own call does.
#[tokio::test]
async fn a_complete_tool_call_before_the_cut_never_runs() {
    let mut h = harness(
        vec![
            vec![
                ProviderEvent::ToolCall(call("c1", "first")),
                done(CUT_STREAM),
            ],
            vec![
                ProviderEvent::ToolCall(call("c2", "second")),
                done("tool_calls"),
            ],
            vec![ProviderEvent::TextDelta("all done".into()), done("stop")],
        ],
        None,
    );
    let out = h
        .runtime
        .run_turn(steve(), vec![ContentBlock::Text("go".into())], &mut |_| {})
        .await
        .expect("a retried cut recovers");
    assert_eq!(out.reason, "done");

    let events = h.runtime.log().read_all().unwrap();
    assert_eq!(
        result_ids(&events),
        vec!["c2".to_owned()],
        "the cut reply's call was never run: {events:?}"
    );
    assert_eq!(retries(&events).len(), 1);
    assert_eq!(
        assistant_texts(&events),
        vec!["all done".to_owned()],
        "no assistant message from the cut attempt"
    );
}

/// T3: a dropped connection mid-stream is asked again, once.
#[tokio::test]
async fn a_transport_error_after_content_is_asked_again() {
    let mut h = harness(
        vec![
            vec![
                ProviderEvent::TextDelta("partial".into()),
                ProviderEvent::Error(ProviderError::Transport("the connection dropped".into())),
            ],
            vec![ProviderEvent::TextDelta("the answer".into()), done("stop")],
        ],
        None,
    );
    let out = h
        .runtime
        .run_turn(steve(), vec![ContentBlock::Text("go".into())], &mut |_| {})
        .await
        .expect("a retried transport error recovers");
    assert_eq!(out.reason, "done");

    let events = h.runtime.log().read_all().unwrap();
    assert_eq!(retries(&events).len(), 1);
    assert_eq!(assistant_texts(&events), vec!["the answer".to_owned()]);
    assert!(!log_text(&events).contains("partial"));
}

/// T3 (the mix): a cut attempt, then a transport error on the retry.
/// The retry's content flag is its own, so the second failure still ends
/// the turn — one retry per model call, never two.
#[tokio::test]
async fn a_cut_then_a_transport_ends_the_turn_after_one_retry() {
    let mut h = harness(
        vec![
            vec![ProviderEvent::TextDelta("partial".into()), done(CUT_STREAM)],
            vec![
                ProviderEvent::TextDelta("half a line".into()),
                ProviderEvent::Error(ProviderError::Transport("dropped again".into())),
            ],
        ],
        None,
    );
    let err = h
        .runtime
        .run_turn(steve(), vec![ContentBlock::Text("go".into())], &mut |_| {})
        .await
        .expect_err("two failures end the turn");
    assert!(
        matches!(err, RuntimeError::Provider(ProviderError::Transport(_))),
        "the retry's own error is the one reported: {err:?}"
    );

    let events = h.runtime.log().read_all().unwrap();
    assert_eq!(
        retries(&events).len(),
        1,
        "the retry itself is not retried: {events:?}"
    );
    assert_eq!(
        assistant_texts(&events),
        Vec::<String>::new(),
        "nothing of either attempt is kept"
    );
    let text = log_text(&events);
    assert!(!text.contains("partial") && !text.contains("half a line"));
}

/// T4: a second cut ends the turn on the cut, as before #114.
#[tokio::test]
async fn a_second_cut_ends_the_turn_on_the_cut() {
    let mut h = harness(
        vec![
            vec![ProviderEvent::TextDelta("partial".into()), done(CUT_STREAM)],
            vec![done(CUT_STREAM)],
        ],
        None,
    );
    let err = h
        .runtime
        .run_turn(steve(), vec![ContentBlock::Text("go".into())], &mut |_| {})
        .await
        .expect_err("two cuts end the turn");
    assert!(
        matches!(err, RuntimeError::Provider(ProviderError::Cut)),
        "the cut is the error: {err:?}"
    );

    let events = h.runtime.log().read_all().unwrap();
    assert_eq!(retries(&events).len(), 1, "exactly one retry");
    let ended = turn_ended(&events);
    assert_eq!(
        ended.reason,
        "provider_error: the stream ended before the reply finished"
    );
    assert_eq!(ended.error, Some(ProviderError::Cut));
    assert_eq!(assistant_texts(&events), Vec::<String>::new());
}

/// One non-transient failure after content: no retry, the turn ends on
/// it, and the cut of the reply is still not trusted.
async fn not_asked_again(error: ProviderError) {
    let mut h = harness(
        vec![vec![
            ProviderEvent::TextDelta("partial".into()),
            ProviderEvent::Error(error.clone()),
        ]],
        None,
    );
    let err = h
        .runtime
        .run_turn(steve(), vec![ContentBlock::Text("go".into())], &mut |_| {})
        .await
        .expect_err("a non-transient failure ends the turn");
    assert!(
        matches!(&err, RuntimeError::Provider(e) if *e == error),
        "the error is the one reported: {err:?}"
    );

    let events = h.runtime.log().read_all().unwrap();
    assert_eq!(
        retries(&events),
        Vec::new(),
        "a malformed or refused reply is not asked again: {events:?}"
    );
    assert_eq!(assistant_texts(&events), Vec::<String>::new());
}

/// T5: an unsupported request after content is not asked again.
#[tokio::test]
async fn an_unsupported_error_after_content_is_not_asked_again() {
    not_asked_again(ProviderError::Unsupported("no such tool".into())).await;
}

/// T5: a refusal after content is not asked again.
#[tokio::test]
async fn a_refusal_after_content_is_not_asked_again() {
    not_asked_again(ProviderError::Http {
        status: 400,
        body: "bad request".into(),
    })
    .await;
}

/// T5: an unreadable stream after content is not asked again.
#[tokio::test]
async fn a_protocol_error_after_content_is_not_asked_again() {
    not_asked_again(ProviderError::Protocol("the stream made no sense".into())).await;
}

/// T6: one retry *per model call*, not one per turn: both calls of this
/// turn are cut once and both are asked again.
#[tokio::test]
async fn each_model_call_is_asked_again_once() {
    let mut h = harness(
        vec![
            vec![
                ProviderEvent::TextDelta("discarded one".into()),
                done(CUT_STREAM),
            ],
            vec![
                ProviderEvent::TextDelta("kept one".into()),
                ProviderEvent::ToolCall(call("c1", "work")),
                done("tool_calls"),
            ],
            vec![
                ProviderEvent::TextDelta("discarded two".into()),
                done(CUT_STREAM),
            ],
            vec![ProviderEvent::TextDelta("kept two".into()), done("stop")],
        ],
        None,
    );
    let out = h
        .runtime
        .run_turn(steve(), vec![ContentBlock::Text("go".into())], &mut |_| {})
        .await
        .expect("both calls recover");
    assert_eq!(out.reason, "done");

    let events = h.runtime.log().read_all().unwrap();
    let retried = retries(&events);
    assert_eq!(retried.len(), 2, "one retry per model call: {events:?}");
    assert_eq!(
        retried
            .iter()
            .map(|r| (r.attempt, r.retries))
            .collect::<Vec<_>>(),
        vec![(1, 1), (1, 1)],
        "each retry is the first of its own call"
    );
    assert_eq!(
        assistant_texts(&events),
        vec!["kept one".to_owned(), "kept two".to_owned()]
    );
    assert_eq!(result_ids(&events), vec!["c1".to_owned()]);
    let text = log_text(&events);
    assert!(!text.contains("discarded"));
}

/// A provider whose reply streams text and then fires the turn's cancel
/// token in the same poll that hands the cut over (T9): a script cannot
/// otherwise reach the window between the failure and the retry. One
/// call, one attempt.
struct CutAndCancel {
    token: CancelToken,
    calls: Arc<Mutex<u32>>,
}

impl Provider for CutAndCancel {
    fn complete(
        &self,
        _: &CompletionRequest<'_>,
    ) -> Pin<Box<dyn Stream<Item = ProviderEvent> + Send + '_>> {
        *self.calls.lock().unwrap() += 1;
        let token = self.token.clone();
        let steps: std::collections::VecDeque<ProviderEvent> =
            [ProviderEvent::TextDelta("partial".into()), done(CUT_STREAM)].into();
        Box::pin(futures_util::stream::unfold(
            (steps, token),
            move |(mut steps, token)| async move {
                let event = steps.pop_front()?;
                // The cut is handed over with the token already fired: the
                // interrupt is seen between the failure and the retry, the
                // one window the retry must respect.
                if matches!(event, ProviderEvent::Done { .. }) {
                    token.cancel(Author::User(UserId("steve".into())));
                }
                Some((event, (steps, token)))
            },
        ))
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

/// A registry holding the echo tool, so the runtime has somewhere to run
/// a call it is sent.
fn registry() -> ToolRegistry {
    vec![Box::new(Echo) as Box<dyn Tool>].into()
}

struct Echo;

impl Tool for Echo {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "echo"
    }
    fn schema(&self) -> schemars::schema::RootSchema {
        schemars::schema_for!(String)
    }
    fn risk_class(&self) -> RiskClass {
        RiskClass::Safe
    }
    fn call(&self, args: serde_json::Value) -> BoxFuture<'_, Result<ToolOutput, ToolError>> {
        let msg = args["msg"].as_str().unwrap_or_default().to_owned();
        Box::pin(async move {
            Ok(ToolOutput {
                content: format!("echo: {msg}"),
                is_error: false,
            })
        })
    }
}

/// T9: a cut after content with the turn cancelled between the error and
/// the retry ends interrupted — no second request, no `provider_retried`.
#[tokio::test]
async fn a_cancelled_turn_is_not_asked_again() {
    let dir = tempfile::tempdir().unwrap();
    let log = ThreadLog::open(dir.path(), ulid::Ulid::generate()).unwrap();
    let token = CancelToken::never();
    let calls = Arc::new(Mutex::new(0u32));
    let provider = CutAndCancel {
        token: token.clone(),
        calls: calls.clone(),
    };
    let mut runtime = Runtime::new(
        Box::new(provider),
        registry(),
        log,
        AgentId("worker".into()),
    );
    let out = runtime
        .run_turn_until(
            steve(),
            vec![ContentBlock::Text("go".into())],
            &token,
            &mut Inbox::none(),
            &mut |_| {},
        )
        .await
        .expect("an interrupted turn is not an error");

    assert_eq!(out.reason, "interrupted", "the person's interrupt wins");
    let events = runtime.log().read_all().unwrap();
    assert_eq!(
        retries(&events),
        Vec::new(),
        "a cancelled turn is not asked again: {events:?}"
    );
    assert_eq!(*calls.lock().unwrap(), 1, "no second request went out");
    assert!(!log_text(&events).contains("partial"));
    assert_eq!(turn_ended(&events).reason, "interrupted");
}

/// A budget a test sets so an iteration count is the thing under test.
fn budget(max_iterations: u32) -> Budget {
    Budget {
        max_iterations,
        max_tokens: u64::MAX,
        max_wall_time: Duration::from_secs(60),
        cache_read_price_ratio: 0.25,
    }
}

/// T10: a model call costs one iteration, however many attempts it took:
/// a cut and its retry fit inside a budget of one (issue #114).
#[tokio::test]
async fn a_cut_and_its_retry_fit_inside_one_iteration() {
    let mut h = harness(
        vec![
            vec![ProviderEvent::TextDelta("partial".into()), done(CUT_STREAM)],
            vec![ProviderEvent::TextDelta("the answer".into()), done("stop")],
        ],
        None,
    );
    h.runtime.set_budget(budget(1));
    let out = h
        .runtime
        .run_turn(steve(), vec![ContentBlock::Text("go".into())], &mut |_| {})
        .await
        .expect("a retried cut recovers");
    assert_eq!(out.reason, "done");
    assert_eq!(
        retries(&h.runtime.log().read_all().unwrap()).len(),
        1,
        "the retry happened: two `complete` calls"
    );
    assert_eq!(
        out.iterations, 1,
        "two attempts of one model call are one iteration"
    );
}

/// T11: a retry does not cost the *next* model call its iteration: with a
/// budget of two, a first call cut once (its retry asks for a tool) and a
/// second call as the final reply both fit, exactly as they do without the
/// cut (issue #114).
#[tokio::test]
async fn a_cut_does_not_cost_the_next_model_call_its_iteration() {
    let mut cut = harness(
        vec![
            vec![
                ProviderEvent::TextDelta("discarded".into()),
                done(CUT_STREAM),
            ],
            vec![
                ProviderEvent::ToolCall(call("c1", "work")),
                done("tool_calls"),
            ],
            vec![ProviderEvent::TextDelta("all done".into()), done("stop")],
        ],
        None,
    );
    let mut clean = harness(
        vec![
            vec![
                ProviderEvent::ToolCall(call("c1", "work")),
                done("tool_calls"),
            ],
            vec![ProviderEvent::TextDelta("all done".into()), done("stop")],
        ],
        None,
    );

    let mut seen = Vec::new();
    for h in [&mut cut, &mut clean] {
        h.runtime.set_budget(budget(2));
        let out = h
            .runtime
            .run_turn(steve(), vec![ContentBlock::Text("go".into())], &mut |_| {})
            .await
            .expect("two model calls fit a budget of two");
        assert_eq!(out.reason, "done", "neither turn stops on the budget");
        seen.push(out.iterations);
    }
    assert_eq!(
        seen,
        vec![2, 2],
        "two model calls, two iterations, cut or not"
    );
    assert_eq!(retries(&cut.runtime.log().read_all().unwrap()).len(), 1);
    assert_eq!(retries(&clean.runtime.log().read_all().unwrap()).len(), 0);
}

/// T12: a failing call still counts its one iteration (issue #114). The
/// cut below ends the turn on its error, exactly as a single uncut error
/// does; `run_turn` returns `Err` on that path, so the count is read from
/// the turn that ends without an error — the person's cancel between the
/// failure and the retry — over the same `spent`.
#[tokio::test]
async fn a_failing_call_still_counts_its_one_iteration() {
    let dir = tempfile::tempdir().unwrap();
    let log = ThreadLog::open(dir.path(), ulid::Ulid::generate()).unwrap();
    let token = CancelToken::never();
    let calls = Arc::new(Mutex::new(0u32));
    let provider = CutAndCancel {
        token: token.clone(),
        calls: calls.clone(),
    };
    let mut runtime = Runtime::new(
        Box::new(provider),
        registry(),
        log,
        AgentId("worker".into()),
    );
    let out = runtime
        .run_turn_until(
            steve(),
            vec![ContentBlock::Text("go".into())],
            &token,
            &mut Inbox::none(),
            &mut |_| {},
        )
        .await
        .expect("an interrupted turn is not an error");

    assert_eq!(out.reason, "interrupted", "the person's interrupt wins");
    assert_eq!(*calls.lock().unwrap(), 1, "one call, cut");
    assert_eq!(
        out.iterations, 1,
        "the call that failed counts once, as before #114"
    );
}
