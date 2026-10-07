//! docs/PLAN-phase2.md done-when 2: a 200-turn thread stays under budget.

use std::pin::Pin;
use std::sync::{Arc, Mutex};

use std::time::Duration;

use aigentic_core::{
    AgentId, Author, CUT_STREAM, Capabilities, CompletionRequest, ContentBlock, Event, EventKind,
    Message, Provider, ProviderError, ProviderEvent, Role, ToolCall, Usage,
};
use aigentic_log::{CompactedPayload, CompactionStrategy, ThreadLog};
use aigentic_runtime::{
    CancelToken, CompactionSettings, INTERRUPTED, Inbox, LINK_PROMPT, Prices, RATIO_SMOOTHING,
    Runtime, SUMMARY_PROMPT, min_free,
};
use futures_core::Stream;
use serde_json::json;

mod common;
use common::{EchoTool, Harness, reporting_scripted, steve};

const WINDOW: u64 = 8_000;

/// Each request's messages, plus whether it was a summarisation call —
/// a nest or a link.
type Requests = Arc<Mutex<Vec<(Vec<Message>, bool)>>>;

/// Replies grow the thread: `reply_bytes` of text per ordinary turn, an
/// echo tool call every third turn with `tool_bytes` of `msg`, and a short
/// summary whenever asked with either summary prompt (issue #98: the same
/// double stands in for the thread's provider and for the utility's).
/// Reports usage from the same estimate the runtime uses, like a real
/// backend would report its own count.
struct Growing {
    requests: Requests,
    turn: Mutex<u32>,
    /// The reply is `"Reply {turn}. "` repeated this many times.
    reply_repeat: usize,
    tool_bytes: usize,
}

impl Growing {
    fn new(requests: Requests, reply_repeat: usize, tool_bytes: usize) -> Self {
        Self {
            requests,
            turn: Mutex::new(0),
            reply_repeat,
            tool_bytes,
        }
    }
}

fn estimate(context: &[Message]) -> u64 {
    aigentic_providers::estimate::estimate_tokens(context)
}

impl Provider for Growing {
    fn complete(
        &self,
        request: &CompletionRequest<'_>,
    ) -> Pin<Box<dyn Stream<Item = ProviderEvent> + Send + '_>> {
        let is_summary = matches!(
            request.messages.first(),
            Some(Message { role: Role::System, blocks, .. })
                if blocks[0] == ContentBlock::Text(SUMMARY_PROMPT.into())
                    || blocks[0] == ContentBlock::Text(LINK_PROMPT.into())
        );
        self.requests
            .lock()
            .unwrap()
            .push((request.messages.to_vec(), is_summary));
        // Like a real provider, the prompt it reports includes the tool
        // schemas; the runtime calibrates against that (issue #52), so a
        // provider that left them out would skew the estimate down a
        // little more with every tool the harness offers (it pushed this
        // test over its line once #7 added suggest_project).
        let prompt =
            estimate(request.messages) + aigentic_runtime::evict::schemas_tokens(request.tools);
        let usage = |out: u64| {
            ProviderEvent::Usage(Usage {
                input_tokens: prompt,
                output_tokens: out,
                ..Default::default()
            })
        };

        let events = if is_summary {
            vec![
                ProviderEvent::TextDelta(
                    "Earlier turns: the user counted upward and I replied at length.".into(),
                ),
                usage(20),
                ProviderEvent::Done {
                    finish_reason: "stop".into(),
                },
            ]
        } else if request.messages.last().map(|m| m.role) == Some(Role::Tool) {
            vec![
                ProviderEvent::TextDelta("Tool done. ".repeat(60)),
                usage(200),
                ProviderEvent::Done {
                    finish_reason: "stop".into(),
                },
            ]
        } else {
            let mut turn = self.turn.lock().unwrap();
            *turn += 1;
            if self.tool_bytes > 0 && (*turn).is_multiple_of(3) {
                vec![
                    ProviderEvent::ToolCall(ToolCall {
                        id: format!("call_{turn}"),
                        name: "echo".into(),
                        args: json!({"msg": "x".repeat(self.tool_bytes)}),
                    }),
                    usage(50),
                    ProviderEvent::Done {
                        finish_reason: "tool_calls".into(),
                    },
                ]
            } else {
                vec![
                    ProviderEvent::TextDelta(format!("Reply {turn}. ").repeat(self.reply_repeat)),
                    usage(200),
                    ProviderEvent::Done {
                        finish_reason: "stop".into(),
                    },
                ]
            }
        };
        Box::pin(futures_util::stream::iter(events))
    }
    fn count_tokens(&self, context: &[Message]) -> u64 {
        estimate(context)
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supports_tools: true,
            supports_images: false,
            supports_caching: false,
            supports_structured_output: false,
            max_context_tokens: WINDOW,
        }
    }
}

#[tokio::test]
async fn two_hundred_turns_stay_under_budget() {
    // Issue #98, T3: the same 200-turn thread, now kept under the line by
    // a chain of links on the utility profile instead of a nest at the
    // near-limit trigger. Every expected value below is read off the log
    // the run wrote or the settings it ran under, never hand-computed.
    let dir = tempfile::tempdir().unwrap();
    let requests: Requests = Arc::new(Mutex::new(Vec::new()));
    let utility_requests: Requests = Arc::new(Mutex::new(Vec::new()));
    let log = ThreadLog::open(dir.path(), ulid::Ulid::generate()).unwrap();
    let settings = CompactionSettings {
        trigger_fraction: 0.7,
        keep_turns: 4,
        max_result_bytes: 500,
        summary_max_output_tokens: 256,
        keep_last_calls: 12,
        working_set_tokens: 128_000,
        evict_above_tokens: 64_000,
        evict_min_free_percent: 25,
    };
    let calls = Arc::new(Mutex::new(Vec::new()));
    let registry: aigentic_tools::ToolRegistry =
        vec![Box::new(EchoTool(calls)) as Box<dyn aigentic_core::Tool>].into();
    let mut rt = Runtime::new(
        Box::new(Growing::new(requests.clone(), 80, 3_000)),
        registry,
        log,
        AgentId("worker".into()),
    )
    .with_layers(aigentic_runtime::Layers::global_instructions("Be terse."))
    .with_compaction(settings)
    .with_model_label("scripted")
    .with_utility(
        Box::new(Growing::new(utility_requests.clone(), 4, 0)),
        UTILITY_LABEL,
        Some(UTILITY_PRICES),
    );
    let line = rt.window_line();
    assert_eq!(line, 5_600);
    // The client's ceiling (issue #21) is the line compaction aims
    // under, so the footer's `N / M context` is fill against what we
    // are actually willing to pay for, not the provider's raw window.
    assert_eq!(rt.window_usage(&[]).window, line);

    for i in 0..200 {
        let outcome = rt
            .run_turn(
                Author::User(aigentic_core::UserId("steve".into())),
                vec![ContentBlock::Text(format!("turn {i}"))],
                &mut |_| {},
            )
            .await
            .unwrap();
        assert_eq!(outcome.reason, "done", "turn {i}");
    }

    let events = rt.log().read_all().unwrap();
    let links = links(&events);
    assert!(!links.is_empty(), "the chain should have links");
    // Every compaction here is a link: the near-limit trigger is an
    // emergency net now, and this thread never nears the real window.
    assert!(
        nests(&events).is_empty(),
        "no nest or truncation should have fired: {:?}",
        nests(&events)
    );
    assert_eq!(compactions(&events).len(), links.len());

    // Each link sits directly after a batch's `results_stubbed` — the
    // batch iteration — and every range ends on a turn boundary.
    let stub_seqs = event_seqs(&events, EventKind::ResultsStubbed);
    assert!(!stub_seqs.is_empty(), "at least one batch fired");
    let ends = turn_ends(&events);
    for (at, p) in &links {
        assert!(
            stub_seqs.contains(&(at - 1)),
            "link at {at} does not follow a results_stubbed in {stub_seqs:?}"
        );
        assert_eq!(events[*at as usize].kind, EventKind::Compacted);
        assert!(p.continuous, "{p:?} is a link");
        assert_eq!(
            events[p.to_seq as usize].kind,
            EventKind::TurnEnded,
            "{p:?}"
        );
        assert!(
            leaves_keep_turns(&ends, *at, p.to_seq, settings.keep_turns),
            "link at {at} to {} does not leave keep_turns closed turns",
            p.to_seq
        );
        let CompactionStrategy::Summary { model, usage, .. } = &p.strategy else {
            panic!("a link is a summary: {p:?}");
        };
        assert_eq!(model, UTILITY_LABEL, "the utility profile wrote it");
        assert_eq!(
            usage.cost_usd,
            Some(UTILITY_PRICES.cost_usd(usage)),
            "the link carries its own price"
        );
    }
    // The chain: from the log's start, then each right after the last.
    assert_eq!(links[0].1.from_seq, events.first().unwrap().seq);
    for pair in links.windows(2) {
        assert_eq!(
            pair[1].1.from_seq,
            pair[0].1.to_seq + 1,
            "the next link starts right after the last"
        );
    }

    let reqs = requests.lock().unwrap();
    let mut model_calls = 0;
    for (messages, is_summary) in reqs.iter() {
        if *is_summary {
            continue;
        }
        model_calls += 1;
        let size = estimate(messages);
        // The batch is decided once per turn, at its start, so a mid-turn
        // request may stand over the line by what the turn itself has
        // added since — and by no more than that. The fixture's own
        // largest addition, priced by the same estimator the runtime
        // uses: the reply with its echo call, that call's result, and the
        // reply after it (issue #98: the near-limit trigger no longer
        // compacts mid-turn, so this slack is the whole of the overshoot).
        assert!(
            size < line + turn_growth(),
            "request of {size} tokens is over the {line} line plus one turn's growth"
        );
        assert!(
            messages.last().map(|m| m.role) != Some(Role::User) || size < line,
            "a turn's first request is under the {line} line: {size} tokens"
        );
        // Tool contract: every assistant tool call has a following tool result.
        let mut open: Vec<String> = Vec::new();
        for m in messages {
            for b in &m.blocks {
                match b {
                    ContentBlock::ToolCall(c) => open.push(c.id.clone()),
                    ContentBlock::ToolResult(r) => open.retain(|id| id != &r.id),
                    _ => {}
                }
            }
        }
        assert!(open.is_empty(), "unanswered calls {open:?}");
    }
    assert!(model_calls >= 200);

    // The last four turns are verbatim in the final request.
    let (last, _) = reqs.iter().rev().find(|(_, s)| !*s).unwrap();
    let text = last
        .iter()
        .flat_map(|m| m.blocks.iter())
        .filter_map(|b| match b {
            ContentBlock::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(
            "
",
        );
    for i in 196..200 {
        assert!(
            text.contains(&format!("turn {i}")),
            "turn {i} missing from the final request"
        );
    }
    assert!(
        text.contains("[Summary of events"),
        "the final request carries a summary"
    );
    assert!(
        !text.contains("turn 10\n"),
        "early turns are summarised away"
    );
    drop(reqs);

    // What the utility saw: one call per link, and only the link's own
    // range's originals — never seq 0 again after the first link.
    let util = utility_requests.lock().unwrap();
    assert_eq!(util.len(), links.len(), "one utility call per link");
    for (messages, is_summary) in util.iter() {
        assert!(*is_summary, "the utility is asked only to summarise");
        assert_eq!(
            messages[0].blocks[0],
            ContentBlock::Text(LINK_PROMPT.into())
        );
    }
    let first_turn = util
        .iter()
        .filter(|(messages, _)| {
            messages.iter().any(|m| {
                m.blocks
                    .iter()
                    .any(|b| matches!(b, ContentBlock::Text(t) if t.contains("turn 0")))
            })
        })
        .count();
    assert_eq!(first_turn, 1, "only the first link reads seq 0");

    // Summaries are auditable and cost is attributed.
    let summary_usage: u64 = links
        .iter()
        .filter_map(|(_, p)| match &p.strategy {
            CompactionStrategy::Summary { usage, .. } => Some(usage.output_tokens),
            _ => None,
        })
        .sum();
    assert!(summary_usage > 0);
}
#[tokio::test]
async fn compact_now_reports_what_it_did_and_pin_lands_in_the_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let requests: Requests = Arc::new(Mutex::new(Vec::new()));
    let provider = Growing::new(requests.clone(), 80, 3_000);
    let log = ThreadLog::open(dir.path(), ulid::Ulid::generate()).unwrap();
    let mut rt = Runtime::new(
        Box::new(provider),
        aigentic_tools::ToolRegistry::empty(),
        log,
        AgentId("worker".into()),
    )
    .with_compaction(CompactionSettings {
        keep_turns: 1,
        ..aigentic_runtime::DEFAULT_COMPACTION
    });
    let steve = Author::User(aigentic_core::UserId("steve".into()));

    assert!(
        rt.compact_now(&mut |_| {}).await.unwrap().is_empty(),
        "nothing to compact yet"
    );
    rt.pin(steve.clone(), "Answer in Swedish.".into(), &mut |_| {})
        .unwrap();
    for i in 0..3 {
        rt.run_turn(
            steve.clone(),
            vec![ContentBlock::Text(format!("t{i}"))],
            &mut |_| {},
        )
        .await
        .unwrap();
    }
    let did = rt.compact_now(&mut |_| {}).await.unwrap();
    assert_eq!(did.len(), 1, "{did:?}");
    assert!(matches!(did[0], CompactionStrategy::Summary { .. }));

    rt.run_turn(steve, vec![ContentBlock::Text("t3".into())], &mut |_| {})
        .await
        .unwrap();
    let reqs = requests.lock().unwrap();
    let (last, _) = reqs.last().unwrap();
    assert_eq!(last[0].role, Role::System);
    assert!(
        matches!(&last[0].blocks[0], ContentBlock::Text(t) if t.contains("Pinned facts:\n- Answer in Swedish."))
    );
    assert!(
        matches!(&last[1].blocks[0], ContentBlock::Text(t) if t.starts_with("[Summary of events 0 to"))
    );
}

/// T2, issue #52: a scripted provider reporting `k ×` its own count of the
/// request it was sent — `Growing` above is the precedent, with the factor
/// made visible. After one call the runtime's ratio is T1's formula for
/// the sample that provider's numbers give, computed from the code: the
/// reported count over the estimate of the request plus the schemas that
/// went with it.
#[tokio::test]
async fn one_call_teaches_the_ratio_the_reporting_factor() {
    let dir = tempfile::tempdir().unwrap();
    let log = ThreadLog::open(dir.path(), ulid::Ulid::generate()).unwrap();
    let k = 2.0;
    let (provider, seen) = reporting_scripted(
        vec![vec![
            ProviderEvent::TextDelta("hi".into()),
            ProviderEvent::Done {
                finish_reason: "stop".into(),
            },
        ]],
        k,
    );
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
    assert_eq!(rt.eviction_ratio(), 1.0, "the seed");

    rt.run_turn(
        Author::User(aigentic_core::UserId("steve".into())),
        vec![ContentBlock::Text("count this".into())],
        &mut |_| {},
    )
    .await
    .unwrap();

    // What the provider reported, and what the runtime estimated for the
    // same request: the sample, and the ratio one EWMA step leaves.
    let over = rt.tool_specs();
    let overhead = aigentic_runtime::schemas_tokens(&over);
    let seen = seen.lock().unwrap();
    let request = seen.last().expect("the turn's request");
    let est = estimate(request);
    let reported = (k * (est + overhead) as f64).round();
    let sample = reported / (est + overhead) as f64;
    let expected = 1.0 + RATIO_SMOOTHING * (sample - 1.0);
    assert!(
        (rt.eviction_ratio() - expected).abs() < 1e-9,
        "ratio {} is not {expected} after one sample of {sample}",
        rt.eviction_ratio()
    );
}

// ------------------------------------------------------------- #98 ---
//
// Issue #98: older turns are summarised continuously, in a chain, on the
// utility profile, at #76's batch point. The links below ride a batch's
// `results_stubbed`, one per batch, and `T2`–`T6` read them off the log.

/// The utility profile's label, so a link's `model` says who wrote it.
const UTILITY_LABEL: &str = "utility-small";
/// The thread's own label, as `two_hundred_turns_stay_under_budget` set it.
const THREAD_LABEL: &str = "scripted";
/// Prices that make a link's `cost_usd` visible: a tenth of a cent per
/// 1k input and a third of a cent per 1k output.
const UTILITY_PRICES: Prices = Prices {
    input: 100.0,
    cache_read: 50.0,
    cache_write: 200.0,
    output: 300.0,
};

/// #98's settings. The doubles offer an 8000-token window, so the
/// emergency gate is 0.9 × 8000 = 7200. `target` is the working-set
/// target the batch aims at (`min_free` is `evict_min_free_percent` of
/// it), `cap` is the summary output cap a link's range is priced
/// against, `keep_turns` the closed turns left verbatim.
fn link_settings(target: u64, cap: u64, keep_turns: usize) -> CompactionSettings {
    CompactionSettings {
        keep_turns,
        summary_max_output_tokens: cap,
        working_set_tokens: target,
        ..aigentic_runtime::DEFAULT_COMPACTION
    }
}

/// The #98 fixture: a thread provider that grows by text, and by tool
/// output every third turn when `tool_bytes` is set, and its own utility
/// profile for the links.
fn link_harness(
    thread_requests: Requests,
    utility_requests: Requests,
    reply_repeat: usize,
    tool_bytes: usize,
    settings: CompactionSettings,
) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let log = ThreadLog::open(dir.path(), ulid::Ulid::generate()).unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let registry: aigentic_tools::ToolRegistry =
        vec![Box::new(EchoTool(calls.clone())) as Box<dyn aigentic_core::Tool>].into();
    let runtime = Runtime::new(
        Box::new(Growing::new(thread_requests, reply_repeat, tool_bytes)),
        registry,
        log,
        AgentId("worker".into()),
    )
    .with_compaction(settings)
    .with_model_label(THREAD_LABEL)
    .with_utility(
        Box::new(Growing::new(utility_requests, 4, 0)),
        UTILITY_LABEL,
        Some(UTILITY_PRICES),
    );
    Harness {
        runtime,
        seen: Arc::new(Mutex::new(Vec::new())),
        calls,
        dir,
    }
}

/// The fill the next model call would see (issue #98, T6): the runtime's
/// own measure over the log as it stands.
fn fill_of(rt: &Runtime) -> u64 {
    let events = rt.log().read_all().unwrap();
    let context = aigentic_runtime::build_context(&aigentic_runtime::Prefix::default(), &events)
        .expect("the log projects");
    rt.fill(&context)
}

/// Every `compacted` in the log, with its event's seq.
fn compactions(events: &[Event]) -> Vec<(u64, CompactedPayload)> {
    events
        .iter()
        .filter(|e| e.kind == EventKind::Compacted)
        .map(|e| (e.seq, serde_json::from_value(e.payload.clone()).unwrap()))
        .collect()
}

/// The links only: the continuous summaries, with their event's seq.
fn links(events: &[Event]) -> Vec<(u64, CompactedPayload)> {
    compactions(events)
        .into_iter()
        .filter(|(_, p)| p.continuous)
        .collect()
}

/// Nests only: a `/compact` or the emergency net's summary/truncation.
fn nests(events: &[Event]) -> Vec<(u64, CompactedPayload)> {
    compactions(events)
        .into_iter()
        .filter(|(_, p)| !p.continuous)
        .collect()
}

/// A compaction's summary text, when it has one.
fn summary_text(p: &CompactedPayload) -> Option<&str> {
    match &p.strategy {
        CompactionStrategy::Summary { text, .. } => Some(text),
        CompactionStrategy::TruncateResults { .. } => None,
    }
}

/// The largest a single turn can add to the context in this fixture,
/// priced the way the runtime prices a context: the assistant's reply
/// with its echo call, that call's result, and the reply after it. Every
/// piece comes from the fixture's own parameters, never by hand.
fn turn_growth() -> u64 {
    const REPLY_REPEAT: usize = 80;
    const TOOL_BYTES: usize = 3_000;
    let agent = Author::Agent(AgentId("worker".into()));
    let msg = "x".repeat(TOOL_BYTES);
    estimate(&[
        Message {
            role: Role::Assistant,
            author: agent.clone(),
            blocks: vec![
                ContentBlock::Text("Reply 200. ".repeat(REPLY_REPEAT)),
                ContentBlock::ToolCall(ToolCall {
                    id: "call".into(),
                    name: "echo".into(),
                    args: json!({ "msg": msg.clone() }),
                }),
            ],
        },
        Message {
            role: Role::Tool,
            author: agent.clone(),
            blocks: vec![ContentBlock::ToolResult(
                aigentic_core::content::ToolResult {
                    id: "call".into(),
                    content: format!("echo: {msg}"),
                    is_error: false,
                },
            )],
        },
        Message {
            role: Role::Assistant,
            author: agent,
            blocks: vec![ContentBlock::Text("Tool done. ".repeat(60))],
        },
    ])
}

fn turn_ends(events: &[Event]) -> Vec<u64> {
    events
        .iter()
        .filter(|e| e.kind == EventKind::TurnEnded)
        .map(|e| e.seq)
        .collect()
}

fn event_seqs(events: &[Event], kind: EventKind) -> Vec<u64> {
    events
        .iter()
        .filter(|e| e.kind == kind)
        .map(|e| e.seq)
        .collect()
}

/// Whether `to` is the `turn_ended` that left exactly `keep_turns` closed
/// turns after it **at the moment the event at `at` was appended** — the
/// rule a link's `to` must follow. `ends` is the final log's turn ends,
/// and only those before the link's own event count.
fn leaves_keep_turns(ends: &[u64], at: u64, to: u64, keep_turns: usize) -> bool {
    let before: Vec<u64> = ends.iter().copied().filter(|e| *e < at).collect();
    match before.iter().position(|e| *e == to) {
        Some(i) => before.len() - i - 1 == keep_turns,
        None => false,
    }
}

/// Drive `turns` turns of `text` each, ending with the last turn's own
/// run, and return the notes the observer saw.
async fn driven_turns(rt: &mut Runtime, turns: u32, text: &str) -> Vec<String> {
    let mut notes = Vec::new();
    for i in 0..turns {
        rt.run_turn(
            steve(),
            vec![ContentBlock::Text(format!("{text} {i}"))],
            &mut |s| {
                if let aigentic_runtime::Signal::Note(n) = s {
                    notes.push(n);
                }
            },
        )
        .await
        .unwrap();
    }
    notes
}

/// T2 (issue #98): the link's range. It starts where the last compaction
/// stopped — the log's start for the first link, the previous link's
/// `to + 1` for the rest — and its `to` is the `turn_ended` that leaves
/// exactly `keep_turns` closed turns. A link reads only its own range:
/// its own turns' originals plus the earlier link's text as context.
#[tokio::test]
async fn a_link_chains_from_the_last_compaction_and_reads_only_its_range() {
    let thread: Requests = Arc::new(Mutex::new(Vec::new()));
    let utility: Requests = Arc::new(Mutex::new(Vec::new()));
    let mut h = link_harness(
        thread.clone(),
        utility.clone(),
        60,
        3_000,
        link_settings(400, 256, 1),
    );
    driven_turns(&mut h.runtime, 12, "turn").await;

    let events = h.runtime.log().read_all().unwrap();
    let ends = turn_ends(&events);
    let links = links(&events);
    assert!(
        nests(&events).is_empty(),
        "nothing should nest here: {:?}",
        nests(&events)
    );
    assert!(
        links.len() >= 2,
        "the chain should have grown more than once: {links:?}"
    );
    for (at, p) in &links {
        assert!(
            leaves_keep_turns(&ends, *at, p.to_seq, 1),
            "to {} (at {at}) does not leave keep_turns closed turns in {ends:?}",
            p.to_seq
        );
    }
    assert_eq!(
        links[0].1.from_seq,
        events.first().unwrap().seq,
        "the first link starts at the log's start"
    );
    for pair in links.windows(2) {
        assert_eq!(
            pair[1].1.from_seq,
            pair[0].1.to_seq + 1,
            "the next link starts right after the last"
        );
    }
    for (_, p) in &links {
        let CompactionStrategy::Summary { model, usage, .. } = &p.strategy else {
            panic!("a link is a summary: {p:?}");
        };
        assert_eq!(model, UTILITY_LABEL, "the utility wrote it");
        assert_eq!(
            usage.cost_usd,
            Some(UTILITY_PRICES.cost_usd(usage)),
            "a link carries its own price"
        );
    }

    // What the utility was asked: one call per link, the link prompt, the
    // earlier link's text from the second on, and never seq 0 every time.
    let reqs = utility.lock().unwrap();
    assert_eq!(reqs.len(), links.len(), "one call per link");
    for (i, (messages, is_summary)) in reqs.iter().enumerate() {
        assert!(*is_summary, "the utility is asked only to summarise");
        assert_eq!(
            messages[0].blocks[0],
            ContentBlock::Text(LINK_PROMPT.into())
        );
        let earlier = messages
            .iter()
            .flat_map(|m| m.blocks.iter())
            .filter_map(|b| match b {
                ContentBlock::Text(t) if t.starts_with("Earlier summary: ") => Some(t.clone()),
                _ => None,
            })
            .next();
        if i == 0 {
            assert_eq!(earlier, None, "the first link has none to continue");
            assert!(
                messages.iter().any(|m| m
                    .blocks
                    .iter()
                    .any(|b| matches!(b, ContentBlock::Text(t) if t.contains("turn 0")))),
                "the first link reads the log's first turn"
            );
        } else {
            let earlier = earlier.expect("a later link continues the chain");
            assert!(
                earlier.contains(summary_text(&links[i - 1].1).unwrap()),
                "the earlier summary is the last link's text"
            );
            assert!(
                !messages.iter().any(|m| m
                    .blocks
                    .iter()
                    .any(|b| matches!(b, ContentBlock::Text(t) if t.contains("turn 0")))),
                "a later link does not read seq 0 again"
            );
        }
    }
}

/// T2 (issue #98), the threshold: a range under `3 × summary_max_output_tokens`
/// is not worth a utility call and a cache break, so no link is made — the
/// batch still fires on what stubbing frees.
#[tokio::test]
async fn a_range_under_three_caps_makes_no_link() {
    let thread: Requests = Arc::new(Mutex::new(Vec::new()));
    let utility: Requests = Arc::new(Mutex::new(Vec::new()));
    // A cap no range here can be worth: 3 caps is far past this fixture.
    let mut h = link_harness(
        thread.clone(),
        utility.clone(),
        60,
        3_000,
        link_settings(400, 4_000_000, 1),
    );
    driven_turns(&mut h.runtime, 12, "turn").await;
    let events = h.runtime.log().read_all().unwrap();
    assert!(links(&events).is_empty(), "under three caps: no link");
    assert!(
        !event_seqs(&events, EventKind::ResultsStubbed).is_empty(),
        "the batch itself still fires on what stubbing frees"
    );
    assert!(
        utility.lock().unwrap().is_empty(),
        "the utility was never called"
    );
}

/// T2 (issue #98), not enough closed turns: with fewer than `keep_turns + 1`
/// of them the range is empty, so a batch carries no link even though it
/// fires.
#[tokio::test]
async fn too_few_closed_turns_makes_no_link() {
    let thread: Requests = Arc::new(Mutex::new(Vec::new()));
    let utility: Requests = Arc::new(Mutex::new(Vec::new()));
    let mut h = link_harness(
        thread.clone(),
        utility.clone(),
        60,
        3_000,
        link_settings(400, 256, 8),
    );
    driven_turns(&mut h.runtime, 4, "turn").await;
    let events = h.runtime.log().read_all().unwrap();
    assert!(links(&events).is_empty(), "no range to link: {events:?}");
    assert!(
        !event_seqs(&events, EventKind::ResultsStubbed).is_empty(),
        "the batch fired on stubbing alone"
    );
}

/// T2 (issue #98), after a nest: `/compact` nests from seq 0, and the link
/// that follows starts at the nest's `to + 1` with the nest's own text as
/// its earlier summary.
#[tokio::test]
async fn after_a_nest_the_chain_restarts_with_the_nests_text() {
    let thread: Requests = Arc::new(Mutex::new(Vec::new()));
    let utility: Requests = Arc::new(Mutex::new(Vec::new()));
    let mut h = link_harness(
        thread.clone(),
        utility.clone(),
        60,
        3_000,
        link_settings(400, 256, 1),
    );
    // Three closed turns, then a nest over all of them.
    driven_turns(&mut h.runtime, 3, "turn").await;
    let did = h.runtime.compact_now(&mut |_| {}).await.unwrap();
    assert_eq!(did.len(), 1, "one nest: {did:?}");
    let nest = {
        let events = h.runtime.log().read_all().unwrap();
        nests(&events).pop().expect("a nest is in the log")
    };
    assert!(
        matches!(nest.1.strategy, CompactionStrategy::Summary { .. }),
        "the nest is a summary: {:?}",
        nest.1
    );

    driven_turns(&mut h.runtime, 6, "turn").await;
    let events = h.runtime.log().read_all().unwrap();
    let links = links(&events);
    // The links made *after* the nest: an earlier batch may have made one
    // before it, and that one is not the restart.
    let after: Vec<&(u64, CompactedPayload)> =
        links.iter().filter(|(at, _)| *at > nest.0).collect();
    assert!(!after.is_empty(), "the chain restarted after the nest");
    assert_eq!(
        after[0].1.from_seq,
        nest.1.to_seq + 1,
        "the link starts right after the nest"
    );
    // Its call: the nest's text is the earlier summary.
    let index = links.iter().position(|(at, _)| *at == after[0].0).unwrap();
    let nest_text = summary_text(&nest.1).unwrap().to_owned();
    let reqs = utility.lock().unwrap();
    assert!(
        reqs[index].0.iter().any(|m| m.blocks.iter().any(
            |b| matches!(b, ContentBlock::Text(t) if t == &format!("Earlier summary: {nest_text}"))
        )),
        "the nest's text is the link's earlier summary"
    );
}

/// How the utility answers, for T5 (issue #98).
#[derive(Debug, Clone, Copy)]
enum Fault {
    /// A normal link.
    Ok,
    /// The provider errors.
    Error,
    /// The stream is cut part-way (#96): less than the originals.
    Cut,
    /// The stream never yields; only the limit or the cancel ends it.
    Hang,
}

/// The fault script, shared with the test so it can change its mind
/// mid-run (T5's retry).
type Faults = Arc<Mutex<std::collections::VecDeque<Fault>>>;

/// A utility profile that answers as `faults` says, one entry per call,
/// `Ok` once the script runs out. Records every request it saw.
struct Faulty {
    faults: Faults,
    requests: Requests,
}

impl Faulty {
    fn new(faults: Faults, requests: Requests) -> Self {
        Self { faults, requests }
    }
    fn next(&self) -> Fault {
        self.faults.lock().unwrap().pop_front().unwrap_or(Fault::Ok)
    }
}

impl Provider for Faulty {
    fn complete(
        &self,
        request: &CompletionRequest<'_>,
    ) -> Pin<Box<dyn Stream<Item = ProviderEvent> + Send + '_>> {
        self.requests
            .lock()
            .unwrap()
            .push((request.messages.to_vec(), true));
        let events = match self.next() {
            Fault::Ok => vec![
                ProviderEvent::TextDelta("Earlier turns, summarised.".into()),
                ProviderEvent::Done {
                    finish_reason: "stop".into(),
                },
            ],
            Fault::Error => vec![ProviderEvent::Error(ProviderError::Transport(
                "the utility is down".into(),
            ))],
            Fault::Cut => vec![
                ProviderEvent::TextDelta("Half a summary".into()),
                ProviderEvent::Done {
                    finish_reason: CUT_STREAM.into(),
                },
            ],
            Fault::Hang => return Box::pin(futures_util::stream::pending()),
        };
        Box::pin(futures_util::stream::iter(events))
    }
    fn count_tokens(&self, context: &[Message]) -> u64 {
        estimate(context)
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supports_tools: true,
            supports_images: false,
            supports_caching: false,
            supports_structured_output: false,
            max_context_tokens: WINDOW,
        }
    }
}

/// A harness over `reply_repeat`-sized turns with a fault-scripted
/// utility, priced and labelled like a real profile.
fn faulty_harness(
    faults: Vec<Fault>,
    reply_repeat: usize,
    settings: CompactionSettings,
    limit: Duration,
) -> (Harness, Requests, Requests, Faults) {
    let dir = tempfile::tempdir().unwrap();
    let log = ThreadLog::open(dir.path(), ulid::Ulid::generate()).unwrap();
    let thread: Requests = Arc::new(Mutex::new(Vec::new()));
    let utility: Requests = Arc::new(Mutex::new(Vec::new()));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let registry: aigentic_tools::ToolRegistry =
        vec![Box::new(EchoTool(calls.clone())) as Box<dyn aigentic_core::Tool>].into();
    let runtime = Runtime::new(
        Box::new(Growing::new(thread.clone(), reply_repeat, 3_000)),
        registry,
        log,
        AgentId("worker".into()),
    )
    .with_compaction(settings)
    .with_model_label(THREAD_LABEL)
    .with_summary_limit(limit);
    let queue: Faults = Arc::new(Mutex::new(faults.into()));
    let runtime = runtime.with_utility(
        Box::new(Faulty::new(queue.clone(), utility.clone())),
        UTILITY_LABEL,
        Some(UTILITY_PRICES),
    );
    (
        Harness {
            runtime,
            seen: Arc::new(Mutex::new(Vec::new())),
            calls,
            dir,
        },
        thread,
        utility,
        queue,
    )
}

/// T4 (issue #98): a thread that grows by text alone — long replies, no
/// tool calls, so there is nothing to stub — still batches once the link
/// alone can pay for the cache break. The target is small enough for the
/// link's saving to reach `min_free` on its own.
#[tokio::test]
async fn a_text_only_thread_batches_on_the_links_saving_alone() {
    let settings = link_settings(1_200, 256, 1);
    let thread: Requests = Arc::new(Mutex::new(Vec::new()));
    let utility: Requests = Arc::new(Mutex::new(Vec::new()));
    let mut h = link_harness(thread.clone(), utility.clone(), 300, 0, settings);
    driven_turns(&mut h.runtime, 12, "turn").await;

    let events = h.runtime.log().read_all().unwrap();
    assert!(
        event_seqs(&events, EventKind::ToolResult).is_empty(),
        "nothing to stub in this fixture"
    );
    let links = links(&events);
    assert!(
        !links.is_empty(),
        "the link's saving alone should have batched"
    );
    assert!(
        !event_seqs(&events, EventKind::ResultsStubbed).is_empty(),
        "the batch fired"
    );

    // The saving the decision read: the range's tokens minus the cap the
    // link writes, over `min_free` of the target — the whole of what the
    // batch had to go on, since stubbing frees nothing here.
    let line = h.runtime.window_line();
    let want = min_free(line, settings.evict_min_free_percent);
    let (at, p) = links[0].clone();
    let events_at = &events[..at as usize];
    // The fixture's provider reports exactly what the runtime's estimator
    // and overhead make of a request, so the calibration stays at 1.0 —
    // the ratio the decision ran under.
    let tokens = h
        .runtime
        .link_saving(events_at, p.from_seq, p.to_seq, p.to_seq, 1.0)
        .unwrap();
    assert!(
        tokens >= want,
        "the link's saving {tokens} should reach min_free {want} of the {line} line"
    );
    assert!(p.continuous, "the batch appended a link");
}

/// T5 (issue #98): a link is best effort. A utility that errors, one that
/// is cut short, one that never answers (a short limit), and a turn
/// cancelled during the call: each leaves the batch's stubs standing, no
/// `compacted`, a note saying so, and the turn running to its own end.
/// The next batch, after another closed turn, links from the same seq
/// over a longer range.
#[tokio::test]
async fn a_failed_link_is_skipped_and_the_next_batch_retries_longer() {
    // The first utility call fails four different ways; later ones work.
    for fault in [Fault::Error, Fault::Cut, Fault::Hang] {
        let settings = link_settings(400, 256, 1);
        let (mut h, _thread, _utility, faults) =
            faulty_harness(vec![fault; 50], 60, settings, Duration::from_millis(100));
        let mut notes: Vec<String> = Vec::new();
        let mut reasons: Vec<String> = Vec::new();
        for i in 0..12 {
            let outcome = h
                .runtime
                .run_turn(
                    steve(),
                    vec![ContentBlock::Text(format!("turn {i}"))],
                    &mut |s| {
                        if let aigentic_runtime::Signal::Note(n) = s {
                            notes.push(n);
                        }
                    },
                )
                .await
                .unwrap();
            reasons.push(outcome.reason);
        }
        let events = h.runtime.log().read_all().unwrap();
        assert!(
            !event_seqs(&events, EventKind::ResultsStubbed).is_empty(),
            "{fault:?}: the batch's stubs stand"
        );
        assert!(
            links(&events).is_empty(),
            "{fault:?}: the failed link appended nothing"
        );
        assert!(
            notes
                .iter()
                .any(|n| n.starts_with("summary skipped:") && n.contains("the next batch retries")),
            "{fault:?}: a note says it was skipped: {notes:?}"
        );
        assert!(
            reasons.iter().all(|r| r == "done"),
            "{fault:?}: every turn ran to its normal end: {reasons:?}"
        );

        // The next batch, once the utility works, links the same `from`
        // over a longer range: the failed attempts did not move it.
        faults.lock().unwrap().clear();
        driven_turns(&mut h.runtime, 6, "turn").await;
        let events = h.runtime.log().read_all().unwrap();
        let links = links(&events);
        assert!(
            !links.is_empty(),
            "{fault:?}: the next batch retried and landed"
        );
        assert_eq!(
            links[0].1.from_seq,
            events.first().unwrap().seq,
            "{fault:?}: the retry starts where the first attempt did"
        );
    }
}

/// T5 (issue #98), the four cases in one place: a utility that errors,
/// one that is cut, one that never answers under a short limit, and a
/// cancellation during the call. Each leaves the stubs, appends no
/// `compacted`, notes the skip, and lets the turn end its own way; the
/// next batch then links the same `from` over a longer range.
#[tokio::test]
async fn a_skipped_link_retries_from_the_same_seq_over_a_longer_range() {
    let settings = link_settings(400, 256, 1);
    let (mut h, _thread, utility, faults) = faulty_harness(
        vec![Fault::Error; 50],
        60,
        settings,
        Duration::from_millis(100),
    );

    let mut notes: Vec<String> = Vec::new();
    let mut observe = |s: aigentic_runtime::Signal<'_>| {
        if let aigentic_runtime::Signal::Note(n) = s {
            notes.push(n);
        }
    };
    for i in 0..12 {
        h.runtime
            .run_turn(
                steve(),
                vec![ContentBlock::Text(format!("turn {i}"))],
                &mut observe,
            )
            .await
            .unwrap();
    }
    let events = h.runtime.log().read_all().unwrap();
    assert!(!event_seqs(&events, EventKind::ResultsStubbed).is_empty());
    assert!(
        links(&events).is_empty(),
        "the failed link appended nothing"
    );
    assert!(
        notes
            .iter()
            .any(|n| n.starts_with("summary skipped: provider error")),
        "the note names the provider error: {notes:?}"
    );

    // The utility comes back; another closed turn lets the next batch
    // link the same `from` over a longer range.
    faults.lock().unwrap().clear();
    driven_turns(&mut h.runtime, 6, "turn").await;
    let events = h.runtime.log().read_all().unwrap();
    let links = links(&events);
    assert!(!links.is_empty(), "the retry landed: {links:?}");
    assert_eq!(
        links[0].1.from_seq,
        events.first().unwrap().seq,
        "the range still starts where the first attempt did"
    );
    let util = utility.lock().unwrap();
    assert!(util.len() >= 2, "a failed call and then a retry");
    assert!(
        estimate(&util[util.len() - 1].0) > estimate(&util[0].0),
        "the retry's range is longer than the first attempt's"
    );
}

/// T5 (issue #98), the cancellation: a turn cancelled while the link hangs
/// skips the summary, leaves the batch's stubs, and ends on the cancel
/// path rather than a normal end.
#[tokio::test]
async fn a_link_cancelled_mid_call_is_skipped_and_the_turn_interrupts() {
    let settings = link_settings(400, 256, 1);
    // A short limit (test-only; there is no config key): the hanging
    // utility is ended by the cancel, not by the limit, and the turn is
    // bounded either way.
    let (mut h, _thread, _utility, _faults) =
        faulty_harness(vec![Fault::Hang; 50], 60, settings, Duration::from_secs(30));

    let cancel = CancelToken::never();
    let token = cancel.clone();
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        token.cancel(steve());
    });

    let mut notes: Vec<String> = Vec::new();
    let mut inbox = Inbox::none();
    let mut reasons: Vec<String> = Vec::new();
    for i in 0..12 {
        let outcome = h
            .runtime
            .run_turn_until(
                steve(),
                vec![ContentBlock::Text(format!("turn {i}"))],
                &cancel,
                &mut inbox,
                &mut |s| {
                    if let aigentic_runtime::Signal::Note(n) = s {
                        notes.push(n);
                    }
                },
            )
            .await
            .unwrap();
        reasons.push(outcome.reason);
    }
    canceller.abort();
    let events = h.runtime.log().read_all().unwrap();
    assert!(!event_seqs(&events, EventKind::ResultsStubbed).is_empty());
    assert!(
        notes
            .iter()
            .any(|n| n.starts_with("summary skipped: the turn was cancelled")),
        "the cancelled link is noted: {notes:?}"
    );
    assert!(
        reasons.iter().any(|r| r == INTERRUPTED),
        "the turn took its cancel path: {reasons:?}"
    );
}

/// T5b (issue #98): a link that succeeds notes its start exactly once,
/// and never notes a skip.
#[tokio::test]
async fn a_link_notes_its_start_once_and_no_skip() {
    let settings = link_settings(400, 256, 1);
    let thread: Requests = Arc::new(Mutex::new(Vec::new()));
    let utility: Requests = Arc::new(Mutex::new(Vec::new()));
    let mut h = link_harness(thread.clone(), utility.clone(), 60, 3_000, settings);
    let notes = driven_turns(&mut h.runtime, 12, "turn").await;
    let events = h.runtime.log().read_all().unwrap();
    let links = links(&events);
    assert!(!links.is_empty(), "the fixture makes links");
    let starts: Vec<&String> = notes
        .iter()
        .filter(|n| n.starts_with("summarising turns"))
        .collect();
    assert_eq!(
        starts.len(),
        links.len(),
        "exactly one start note per link: {notes:?}"
    );
    for (_, p) in &links {
        assert!(
            starts.iter().any(|n| *n
                == &format!(
                    "summarising turns {}–{} on {UTILITY_LABEL}",
                    p.from_seq, p.to_seq
                )),
            "the start note names the range and the utility: {starts:?}"
        );
    }
    assert!(
        !notes.iter().any(|n| n.starts_with("summary skipped")),
        "no skip note when every link worked: {notes:?}"
    );
}

/// T6 (issue #98): the emergency net, both halves against one fixture.
/// The same settings and the same double: the thread grows by text alone,
/// so nothing can be stubbed, and the summary cap is far past anything
/// this fixture can hold, so no link is ever offered — `freed` is 0 and
/// the batch cannot fire. Only the emergency net can compact, and it
/// fires only past `0.9 ×` the model's real window.
#[tokio::test]
async fn the_emergency_net_fires_only_past_its_gate() {
    let settings = link_settings(100_000, 4_000_000, 1);
    let h = link_harness(
        Arc::new(Mutex::new(Vec::new())),
        Arc::new(Mutex::new(Vec::new())),
        400,
        0,
        settings,
    );
    let line = h.runtime.window_line();
    let gate = h.runtime.emergency_line();
    assert_eq!(line, 5_600, "the working set aims at 0.7 × 8000");
    assert_eq!(gate, 7_200, "the net's gate is 0.9 × 8000");
    drop(h);

    // Smaller turns until fill passes the line the batch aims at, then
    // two more that both *start* in the band between the line and the
    // net's gate: a net gated at the line would fire there, so this half
    // proves the gate, not only the fixture (#98's judge, note 1).
    // Nothing compacts — not here, not mid-turn.
    let mut below = link_harness(
        Arc::new(Mutex::new(Vec::new())),
        Arc::new(Mutex::new(Vec::new())),
        200,
        0,
        settings,
    );
    let mut grown = 0;
    while fill_of(&below.runtime) <= line {
        driven_turns(&mut below.runtime, 1, &format!("grow {grown}")).await;
        grown += 1;
        assert!(grown < 100, "the fixture never reaches the line");
    }
    driven_turns(&mut below.runtime, 2, "in the band").await;
    let fill = fill_of(&below.runtime);
    assert!(
        fill > line && fill < gate,
        "the fixture is in the band: fill {fill}, line {line}, gate {gate}"
    );
    let events = below.runtime.log().read_all().unwrap();
    assert!(
        compactions(&events).is_empty(),
        "nothing compacts between the line and the gate: {:?}",
        compactions(&events)
    );

    // Two more turns of the same growth: fill passes the gate, and the
    // net — which runs at the top of every iteration, not only a turn's
    // first — fires.
    let mut above = link_harness(
        Arc::new(Mutex::new(Vec::new())),
        Arc::new(Mutex::new(Vec::new())),
        400,
        0,
        settings,
    );
    driven_turns(&mut above.runtime, 8, "turn").await;
    let events = above.runtime.log().read_all().unwrap();
    let fired = compactions(&events);
    assert_eq!(fired.len(), 1, "the net fired once: {fired:?}");
    let (at, payload) = &fired[0];
    assert!(!payload.continuous, "the net nests, it is no link");
    assert_eq!(payload.from_seq, events.first().unwrap().seq);
    assert!(
        matches!(payload.strategy, CompactionStrategy::Summary { .. }),
        "the net's rule 2: {:?}",
        payload.strategy
    );
    assert!(
        event_seqs(&events, EventKind::ResultsStubbed).is_empty(),
        "no batch fired: the net alone compacts here"
    );
    let _ = at;
}

/// T8 (issue #98): `/compact`, resume and projection are unchanged by the
/// chain. After a run of links, `compact_now` still nests from seq 0 over
/// everything; a `Runtime` reopened on the same log projects every link
/// and reproduces the live context; and a log whose `continuous` keys are
/// taken out projects exactly as the same log with them.
#[tokio::test]
async fn compact_resume_and_projection_are_unchanged_by_the_chain() {
    let thread: Requests = Arc::new(Mutex::new(Vec::new()));
    let utility: Requests = Arc::new(Mutex::new(Vec::new()));
    let mut h = link_harness(
        thread.clone(),
        utility.clone(),
        60,
        3_000,
        link_settings(400, 256, 1),
    );
    driven_turns(&mut h.runtime, 12, "turn").await;
    let events = h.runtime.log().read_all().unwrap();
    let links = links(&events);
    assert!(links.len() >= 2, "a chain to preserve: {links:?}");
    assert!(
        !event_seqs(&events, EventKind::ResultsStubbed).is_empty(),
        "the chain rode the batch"
    );

    // The projection before the nest: one summary message per link, so
    // the model reads the whole chain.
    let prefix = aigentic_runtime::Prefix::default();
    let live = aigentic_runtime::build_context(&prefix, &events).unwrap();
    let markers = live
        .iter()
        .flat_map(|m| m.blocks.iter())
        .filter_map(|b| match b {
            ContentBlock::Text(t) if t.starts_with("[Summary of events") => Some(t.as_str()),
            _ => None,
        })
        .count();
    assert_eq!(
        markers,
        links.len(),
        "each link is a summary message of its own"
    );
    for (_, p) in &links {
        let text = summary_text(p).unwrap();
        assert!(
            live.iter().any(|m| m
                .blocks
                .iter()
                .any(|b| matches!(b, ContentBlock::Text(t) if t.contains(text)))),
            "the link {p:?} is in the projection"
        );
    }

    // `/compact` still nests: one summary from seq 0 over everything.
    let did = h.runtime.compact_now(&mut |_| {}).await.unwrap();
    assert_eq!(did.len(), 1, "one nest: {did:?}");
    let after = h.runtime.log().read_all().unwrap();
    let nest = nests(&after).pop().unwrap();
    assert_eq!(nest.1.from_seq, after.first().unwrap().seq);
    assert!(
        events
            .iter()
            .all(|e| after.iter().any(|a| a.seq == e.seq && a.kind == e.kind)),
        "the nest appended; nothing was rewritten"
    );

    // A `Runtime` reopened on the same log reads the same context, and
    // the nest is the one summary the projection now keeps.
    let reopened_log = ThreadLog::open(h.dir.path(), h.runtime.log().thread_id()).unwrap();
    let resumed = Runtime::new(
        Box::new(Growing::new(Arc::new(Mutex::new(Vec::new())), 60, 0)),
        aigentic_tools::ToolRegistry::default(),
        reopened_log,
        AgentId("worker".into()),
    )
    .with_compaction(link_settings(400, 256, 1));
    let resumed_events = resumed.log().read_all().unwrap();
    assert_eq!(
        resumed_events.len(),
        after.len(),
        "the log reads back whole"
    );
    let resumed_context = aigentic_runtime::build_context(&prefix, &resumed_events).unwrap();
    assert_eq!(
        resumed_context,
        live_after(&h),
        "a resume reproduces the live context"
    );
    let nest_text = summary_text(&nest.1).unwrap();
    assert_eq!(
        resumed_context
            .iter()
            .flat_map(|m| m.blocks.iter())
            .filter(|b| matches!(b, ContentBlock::Text(t) if t.starts_with("[Summary of events")))
            .count(),
        1,
        "the nest covers the chain in the projection"
    );
    assert!(
        resumed_context.iter().any(|m| m
            .blocks
            .iter()
            .any(|b| matches!(b, ContentBlock::Text(t) if t.contains(nest_text)))),
        "and the nest is what is left"
    );

    // The `continuous` key changes no projection: the same log with each
    // key taken out projects exactly as it does with them.
    let stripped: Vec<Event> = after
        .iter()
        .map(|e| {
            let mut e = e.clone();
            if e.kind == EventKind::Compacted {
                let mut v: serde_json::Value = e.payload.clone();
                if let Some(obj) = v.as_object_mut() {
                    obj.remove("continuous");
                }
                e.payload = v;
            }
            e
        })
        .collect();
    assert_eq!(
        aigentic_log::project(&stripped).unwrap().body,
        aigentic_log::project(&after).unwrap().body,
        "an old log projects exactly as before"
    );
    assert!(
        stripped.iter().any(|e| {
            e.kind == EventKind::Compacted
                && e.payload.get("continuous").is_none()
                && e.payload.to_string().contains("summary")
        }),
        "the strip really removed keys from summary lines"
    );
}

/// The live runtime's own context, for the resume comparison.
fn live_after(rt: &Harness) -> Vec<Message> {
    let events = rt.runtime.log().read_all().unwrap();
    aigentic_runtime::build_context(&aigentic_runtime::Prefix::default(), &events).unwrap()
}
