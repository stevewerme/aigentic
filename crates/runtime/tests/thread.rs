//! Issue #99: the thread figure. `working` is what the model sees this
//! call; `thread` is how big the conversation really is — everything the
//! log holds, as if nothing had been stubbed or summarised — so it never
//! falls at a stub, a sweep or a summary.
//!
//! Every expected value here is derived: the recount comes from
//! `Runtime::raw_thread_tokens`, the contribution of a message from the
//! estimator the runtime itself counts with, and the served figure from
//! `calibrated_delta`, the runtime's own pricing rule.

mod common;

use std::sync::{Arc, Mutex};

use aigentic_core::{
    AgentId, Author, ContentBlock, EventKind, Message, ProviderEvent, Role, UserId,
};
use aigentic_log::{NewEvent, ThreadLog};
use aigentic_providers::estimate::estimate_tokens;
use aigentic_runtime::{
    CancelToken, CompactionSettings, DEFAULT_COMPACTION, Runtime, Signal, WindowUsage,
    calibrated_delta, inbox,
};
use aigentic_tools::ToolRegistry;
use common::{Step, call, done, steve};

/// A runtime over a temp log, plus the recount a fresh runtime makes over
/// the same log — the running sum's yardstick.
fn rig(
    dir: &std::path::Path,
    thread: ulid::Ulid,
    script: Vec<Vec<ProviderEvent>>,
    k: f64,
) -> Runtime {
    let tools: ToolRegistry = vec![Box::new(common::EchoTool(Arc::new(Mutex::new(Vec::new()))))
        as Box<dyn aigentic_core::Tool>]
    .into();
    let (provider, _seen) = common::reporting_scripted(script, k);
    Runtime::new(
        provider,
        tools,
        ThreadLog::open(dir, thread).unwrap(),
        AgentId("worker".into()),
    )
    .with_layers(aigentic_runtime::Layers::global_instructions("Be terse."))
}

/// A double that counts with the runtime's own estimator and reports
/// exactly what it counted, so the learned ratio (issue #52) is 1 and the
/// served figure is the raw one.
fn counted_script(turns: usize, calls_per_turn: usize) -> Vec<Vec<ProviderEvent>> {
    let mut script = Vec::new();
    for turn in 0..turns {
        for c in 0..calls_per_turn {
            let id = format!("t{turn}c{c}");
            let mut reply = vec![ProviderEvent::TextDelta("working".into())];
            // A reasoning blob on the first reply of every turn: the
            // projection drops it at `turn_ended`, so counting it would
            // make the figure fall there — the thing #99 is about.
            if c == 0 {
                reply.push(ProviderEvent::Blob(aigentic_core::ProviderBlob {
                    provider: "scripted".into(),
                    data: serde_json::json!({ "thinking": "r".repeat(4_000) }),
                }));
            }
            reply.push(ProviderEvent::ToolCall(call(&id, &"x".repeat(200))));
            reply.push(done("tool_calls"));
            script.push(reply);
        }
        // The reply that ends the turn: no tool calls.
        script.push(vec![
            ProviderEvent::TextDelta("all done".into()),
            done("stop"),
        ]);
    }
    script
}

/// One turn, returning every reading of the figure it emitted.
async fn turn(rt: &mut Runtime, text: &str) -> Vec<WindowUsage> {
    let mut usages = Vec::new();
    let outcome = rt
        .run_turn(
            steve(),
            vec![ContentBlock::Text(text.into())],
            &mut |signal| {
                if let Signal::Usage(u) = signal {
                    usages.push(u);
                }
            },
        )
        .await
        .unwrap();
    assert_eq!(outcome.reason, "done", "the turn ran to its end");
    usages
}

/// The recount, as a freshly opened runtime makes it.
fn recount(dir: &std::path::Path, thread: ulid::Ulid) -> u64 {
    rig(dir, thread, Vec::new(), 1.0).raw_thread_tokens()
}

/// A runtime that never summarises: the double advertises a one-thousand
/// token window, so the default settings would compact a few turns in and
/// spend a script entry on the summary call.
fn no_summary(rt: Runtime) -> Runtime {
    rt.with_compaction(CompactionSettings {
        keep_turns: 1_000,
        ..DEFAULT_COMPACTION
    })
}

/// The spec's bound: every append contributes its own rounded estimate, so a
/// reading sits within one token per message of the recount of the log it was
/// taken over — in either direction, since the ceiling never rounds down and a
/// reading can be taken before the reply carrying it is appended.
fn assert_within_bound(sum: u64, recounted: u64, messages: usize) {
    assert!(
        sum.abs_diff(recounted) <= messages as u64,
        "the running sum ({sum}) and the recount ({recounted}) differ past the \
         one-token-per-message bound ({messages} messages)"
    );
}

/// T1: after every call of a twelve-turn thread the running sum is the
/// thread's own recount, the served figure is that priced by the ratio,
/// and the figure never falls — including at every `turn_ended`, where the
/// projection drops the reasoning blobs.
#[tokio::test]
async fn the_figure_is_the_whole_thread_after_every_call() {
    let dir = tempfile::tempdir().unwrap();
    let thread = ulid::Ulid::generate();
    let mut rt = no_summary(rig(dir.path(), thread, counted_script(12, 4), 1.0));
    let mut previous = 0;
    for number in 0..12 {
        let usages = turn(&mut rt, &format!("turn {number}")).await;
        // One reading per call of the turn: the four replies with tool
        // calls and the reply that ends it.
        assert_eq!(usages.len(), 5, "one reading per call of the turn");
        for u in &usages {
            assert!(
                u.thread_tokens >= previous,
                "the figure fell from {previous} to {} in turn {number}",
                u.thread_tokens
            );
            previous = u.thread_tokens;
        }
        // The last call of a turn is the reading the recount answers: the
        // only event appended after it is `turn_ended`, which the figure
        // ignores.
        let recounted = recount(dir.path(), thread);
        let events = ThreadLog::open(dir.path(), thread)
            .unwrap()
            .read_all()
            .unwrap();
        let last = usages.last().unwrap();
        assert_within_bound(last.thread_tokens, recounted, events.len());
        // And it is the raw count priced by the ratio, not the raw count: the
        // fixture's double reports what it counts, so the ratio is one and
        // pricing only rounds — no schemas, no overhead.
        let priced = calibrated_delta(recounted, rt.eviction_ratio());
        assert!(
            last.thread_tokens.abs_diff(priced) <= events.len() as u64,
            "the served figure {} is not the raw count {recounted} priced by the \
             ratio ({} → {priced})",
            last.thread_tokens,
            rt.eviction_ratio()
        );
    }
}

/// T1: a thread the runtime trims — a `results_stubbed` batch, a
/// `context_evicted` sweep and a `compacted` restatement — never loses
/// thread. `working` falls; `thread` does not.
#[tokio::test]
async fn a_trimmed_log_does_not_shrink_the_figure() {
    let line = 900;
    let dir = tempfile::tempdir().unwrap();
    let thread = ulid::Ulid::generate();
    let mut rt =
        rig(dir.path(), thread, counted_script(3, 9), 1.0).with_compaction(CompactionSettings {
            working_set_tokens: line,
            evict_above_tokens: line,
            // Anything worth freeing breaks the prefix: this fixture wants
            // both a batch and a sweep, not the quiet case #76 pins.
            evict_min_free_percent: 0,
            keep_turns: 50,
            ..DEFAULT_COMPACTION
        });
    let mut previous = 0;
    let mut recounted_previous = 0;
    let mut window_fell = false;
    for number in 0..3 {
        let usages = turn(&mut rt, &format!("turn {number}")).await;
        let mut last_window = None;
        for u in &usages {
            if let Some(before) = last_window {
                window_fell |= u.tokens_in_window < before;
            }
            last_window = Some(u.tokens_in_window);
            assert!(
                u.thread_tokens >= previous,
                "the figure fell from {previous} to {} in turn {number}",
                u.thread_tokens
            );
            previous = u.thread_tokens;
        }
        let recounted = recount(dir.path(), thread);
        assert!(
            recounted >= recounted_previous,
            "the recount fell from {recounted_previous} to {recounted} in turn {number}"
        );
        recounted_previous = recounted;
    }
    let events = ThreadLog::open(dir.path(), thread)
        .unwrap()
        .read_all()
        .unwrap();
    for kind in [EventKind::ResultsStubbed, EventKind::ContextEvicted] {
        assert!(
            events.iter().any(|e| e.kind == kind),
            "the fixture never wrote a {kind:?} event: the run never trimmed, so \
             it proves nothing about trimming"
        );
    }
    assert!(
        window_fell,
        "`working` never fell: no batch shrank what the model sees"
    );
}

/// T1: the figure is priced when the reading is built, by the ratio the
/// calls have taught. A double that reports twice what it counts drives
/// the ratio up, and the served figure follows it, above the raw count.
#[tokio::test]
async fn the_figure_is_priced_by_the_learned_ratio() {
    let dir = tempfile::tempdir().unwrap();
    let thread = ulid::Ulid::generate();
    let mut rt = no_summary(rig(dir.path(), thread, counted_script(2, 2), 2.0));
    turn(&mut rt, "go").await;
    let usages = turn(&mut rt, "again").await;
    let recounted = recount(dir.path(), thread);
    let served = usages.last().unwrap().thread_tokens;
    assert!(
        served > recounted,
        "the served figure ({served}) is not priced above the raw count ({recounted})"
    );
    assert!(
        served <= recounted * 2 + 1,
        "the served figure ({served}) is priced past the double's own report \
         (2 × {recounted})"
    );
}

/// T1: a message the person typed mid-turn, parked in a stream, is in the
/// figure at the turn's end — `append_queued` is the runtime's other write
/// path, and it counts what the person typed.
#[tokio::test]
async fn a_message_typed_mid_turn_counts() {
    let dir = tempfile::tempdir().unwrap();
    let thread = ulid::Ulid::generate();
    let (gate, mut parked) = common::gate();
    let script = vec![
        vec![
            Step::Event(ProviderEvent::TextDelta("thinking".into())),
            Step::Gate(gate),
            Step::Event(ProviderEvent::ToolCall(call("c1", "hello"))),
            Step::Event(done("tool_calls")),
        ],
        vec![
            Step::Event(ProviderEvent::TextDelta("done".into())),
            Step::Event(done("stop")),
        ],
    ];
    let tools: ToolRegistry = vec![Box::new(common::EchoTool(Arc::new(Mutex::new(Vec::new()))))
        as Box<dyn aigentic_core::Tool>]
    .into();
    let (provider, _seen) = common::gated_reporting(script, 1.0);
    let mut runtime = Runtime::new(
        provider,
        tools,
        ThreadLog::open(dir.path(), thread).unwrap(),
        AgentId("worker".into()),
    )
    .with_layers(aigentic_runtime::Layers::global_instructions("Be terse."));
    let (outbox, rx) = inbox();
    let usages: Arc<Mutex<Vec<WindowUsage>>> = Arc::new(Mutex::new(Vec::new()));
    let observed = usages.clone();
    let task = tokio::spawn(async move {
        let cancel = CancelToken::never();
        let mut inbox = rx;
        runtime
            .run_turn_until(
                steve(),
                vec![ContentBlock::Text("go".into())],
                &cancel,
                &mut inbox,
                &mut |signal| {
                    if let Signal::Usage(u) = signal {
                        observed.lock().unwrap().push(u);
                    }
                },
            )
            .await
            .unwrap()
            .reason
    });
    parked.wait().await;
    outbox.send(aigentic_runtime::Queued {
        author: Author::User(UserId("magnus".into())),
        blocks: vec![ContentBlock::Text("y".repeat(400))],
    });
    parked.open();
    assert_eq!(task.await.unwrap(), "done");

    let events = ThreadLog::open(dir.path(), thread)
        .unwrap()
        .read_all()
        .unwrap();
    let typed = events
        .iter()
        .find(|e| {
            e.kind == EventKind::UserMessage
                && serde_json::from_value::<aigentic_log::UserMessagePayload>(e.payload.clone())
                    .map(|p| p.mid_turn)
                    .unwrap_or(false)
        })
        .expect("the message the person typed mid-turn is in the log");
    let blocks = serde_json::from_value::<aigentic_log::UserMessagePayload>(typed.payload.clone())
        .unwrap()
        .blocks;
    let counted = estimate_tokens(&[Message {
        role: Role::User,
        author: Author::User(UserId("magnus".into())),
        blocks,
    }]);
    assert!(
        counted > 50,
        "the typed message ({counted} tokens) is too small to tell a count from \
         the rounding bound"
    );

    // The recount of a fresh runtime over the same log includes the typed
    // message, so the sum of the run can only match it if `append_queued`
    // counted the message too.
    let served = *usages.lock().unwrap().last().unwrap();
    let recounted = recount(dir.path(), thread);
    assert_within_bound(served.thread_tokens, recounted, events.len());
}

/// T2: resume repairs an open turn by appending a synthetic result through
/// the same seam, so the resumed runtime counts it. Its figure is the open
/// log's own recount plus that result.
#[tokio::test]
async fn the_figure_counts_what_resume_wrote() {
    let dir = tempfile::tempdir().unwrap();
    let thread = ulid::Ulid::generate();
    let mut log = ThreadLog::open(dir.path(), thread).unwrap();
    // A turn left open: a reply with a call whose result never came.
    log.append(NewEvent {
        kind: EventKind::UserMessage,
        author: steve(),
        payload: serde_json::to_value(aigentic_log::UserMessagePayload::new(vec![
            ContentBlock::Text("go".into()),
        ]))
        .unwrap(),
        parent_event: None,
    })
    .unwrap();
    log.append(NewEvent {
        kind: EventKind::AssistantMessage,
        author: Author::Agent(AgentId("worker".into())),
        payload: serde_json::to_value(aigentic_log::AssistantMessagePayload {
            blocks: vec![ContentBlock::ToolCall(call("c1", "hello"))],
            usage: None,
            finish_reason: None,
        })
        .unwrap(),
        parent_event: None,
    })
    .unwrap();

    let mut runtime = rig(
        dir.path(),
        thread,
        vec![vec![
            ProviderEvent::TextDelta("continuing".into()),
            done("stop"),
        ]],
        1.0,
    );

    // The live runtime's figure over the open log, before the repair.
    let before = runtime.raw_thread_tokens();
    let resumed = runtime.resume(None, &mut |_| {}).unwrap();
    assert!(
        matches!(
            resumed,
            aigentic_runtime::Resumed::Interrupted {
                unanswered_calls: 1,
                ..
            }
        ),
        "one unanswered call was repaired: {resumed:?}"
    );
    let after = runtime.raw_thread_tokens();

    // The synthetic result's own estimate, from the payload resume wrote.
    let events = ThreadLog::open(dir.path(), thread)
        .unwrap()
        .read_all()
        .unwrap();
    let synthetic: Vec<Message> = events
        .iter()
        .filter_map(|e| {
            serde_json::from_value::<aigentic_log::ToolResultPayload>(e.payload.clone()).ok()
        })
        .map(|p| Message {
            role: Role::Tool,
            author: Author::System,
            blocks: vec![ContentBlock::ToolResult(p.result)],
        })
        .collect();
    assert_eq!(synthetic.len(), 1, "resume wrote one synthetic result");
    let added = estimate_tokens(&synthetic);
    // Resume also notes the interruption, and that note projects to a
    // message of its own, so the repair's own estimate is the floor.
    assert!(
        after >= before + added,
        "the recount grew from {before} to {after}, less than the synthetic \
         result's own {added}"
    );

    // The resumed runtime's running sum, read off the next call it makes:
    // the seam counted the repair, so it is the recount and not the open
    // log's seed.
    let usages = turn(&mut runtime, "and on").await;
    let recounted = runtime.raw_thread_tokens();
    assert_within_bound(
        usages.last().unwrap().thread_tokens,
        recounted,
        events.len(),
    );
    assert!(
        usages[0].thread_tokens >= before + added,
        "the resumed sum ({}) is under the open log's figure ({before}) plus the \
         repair ({added})",
        usages[0].thread_tokens
    );
}

/// T1: a summary is not thread either. `/compact` restates the older turns
/// as one pinned fact, so the context shrinks while the figure the person
/// reads does not.
#[tokio::test]
async fn a_summary_does_not_shrink_the_figure() {
    let dir = tempfile::tempdir().unwrap();
    let thread = ulid::Ulid::generate();
    // Slack past the turns: a summary of its own is a model call, and the
    // fixture's tiny window can have the runtime reach for one on its own.
    let mut script = counted_script(3, 2);
    for _ in 0..4 {
        script.push(vec![
            ProviderEvent::TextDelta("a summary".into()),
            done("stop"),
        ]);
    }
    let mut rt = rig(dir.path(), thread, script, 1.0).with_compaction(CompactionSettings {
        keep_turns: 1,
        ..DEFAULT_COMPACTION
    });
    let mut previous = 0;
    for number in 0..3 {
        let usages = turn(&mut rt, &format!("turn {number}")).await;
        for u in &usages {
            assert!(
                u.thread_tokens >= previous,
                "the figure fell from {previous} to {} in turn {number}",
                u.thread_tokens
            );
            previous = u.thread_tokens;
        }
    }
    let before = recount(dir.path(), thread);
    rt.compact_now(&mut |_| {}).await.unwrap();
    let events = ThreadLog::open(dir.path(), thread)
        .unwrap()
        .read_all()
        .unwrap();
    assert!(
        events.iter().any(|e| e.kind == EventKind::Compacted),
        "the fixture never wrote a Compacted event: nothing was summarised"
    );
    // The summary replaces turns in the context, and the recount — which
    // leaves `compacted` events out — is unmoved by it.
    assert_eq!(
        recount(dir.path(), thread),
        before,
        "a summary changed the recount of the thread"
    );
    let usages = turn(&mut rt, "after the summary").await;
    assert!(
        usages[0].thread_tokens >= previous,
        "the figure fell from {previous} to {} across a summary",
        usages[0].thread_tokens
    );
    let events = ThreadLog::open(dir.path(), thread)
        .unwrap()
        .read_all()
        .unwrap();
    assert_within_bound(
        usages.last().unwrap().thread_tokens,
        recount(dir.path(), thread),
        events.len(),
    );
}

/// The fixture's own guard: a quiet reading of a long thread stays inside
/// the rounding bound after hundreds of events.
#[tokio::test]
async fn a_long_thread_stays_within_the_bound() {
    let dir = tempfile::tempdir().unwrap();
    let thread = ulid::Ulid::generate();
    let mut rt = no_summary(rig(dir.path(), thread, counted_script(6, 12), 1.0));
    for number in 0..6 {
        let usages = turn(&mut rt, &format!("turn {number}")).await;
        let recounted = recount(dir.path(), thread);
        let events = ThreadLog::open(dir.path(), thread)
            .unwrap()
            .read_all()
            .unwrap();
        assert_within_bound(
            usages.last().unwrap().thread_tokens,
            recounted,
            events.len(),
        );
    }
    let events = ThreadLog::open(dir.path(), thread)
        .unwrap()
        .read_all()
        .unwrap();
    assert!(
        events.len() > 100,
        "the fixture is too short to say anything about a long thread"
    );
}
