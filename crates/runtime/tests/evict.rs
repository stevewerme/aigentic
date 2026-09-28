//! Issue #30: a long turn's context stays bounded. A build is one turn
//! of hundreds of calls; nothing else shrinks it, so the sweep stubs, in
//! projection, the results and successful edit arguments older than the
//! last few calls.
//!
//! Issue #35: once the floor — the prefix, the last block of calls and
//! the open turn's own reasoning — sits over the ceiling, the sweep can
//! no longer fit the turn at all. It then holds the boundary until the
//! turn has added `EVICT_MIN_FREE_PERCENT` of the ceiling in evictable
//! material, and says so once per turn.

use std::pin::Pin;
use std::sync::{Arc, Mutex};

use aigentic_core::{
    AgentId, Author, Capabilities, CompletionRequest, ContentBlock, Event, EventKind, Message,
    Provider, ProviderEvent, Role, ToolCall, Usage,
};
use aigentic_log::{ContextEvictedPayload, ThreadLog, project_body};
use aigentic_runtime::{
    Answer, Approver, DEFAULT_COMPACTION, Decision, EVICT_MIN_FREE_PERCENT, RATIO_SMOOTHING,
    Runtime, calibrated, min_free, next_ratio,
};
use futures_core::Stream;

/// Every request's messages.
type Seen = Arc<Mutex<Vec<Vec<Message>>>>;

/// Keeps the turn going: one tool call per reply for `calls` iterations
/// (a read, a write and an edit per three), then a bare text reply.
/// Reports usage as the runtime's own model of a request's cost (issue
/// #52): the estimate of the messages plus the schemas that went with
/// them, so this double behaves like a backend whose tokenizer is the
/// estimator — the ratio it teaches stays 1.0 and the tests below
/// measure the rule, not the calibration's warm-up.
struct Building {
    seen: Seen,
    iteration: Mutex<u32>,
    calls: u32,
    /// Lines in the read fixture and in each written file; sizes the
    /// turn's material.
    lines: usize,
    /// Whether it reports usage at all (issue #52, T4): a backend whose
    /// replies carry no counts leaves the runtime nothing to calibrate
    /// against.
    usage: bool,
}

fn estimate(context: &[Message]) -> u64 {
    aigentic_providers::estimate::estimate_tokens(context)
}

impl Provider for Building {
    fn complete(
        &self,
        request: &CompletionRequest<'_>,
    ) -> Pin<Box<dyn Stream<Item = ProviderEvent> + Send + '_>> {
        self.seen.lock().unwrap().push(request.messages.to_vec());
        let prompt = estimate(request.messages) + aigentic_runtime::schemas_tokens(request.tools);
        let mut iteration = self.iteration.lock().unwrap();
        *iteration += 1;
        let counts = |output: u64| {
            self.usage.then_some(ProviderEvent::Usage(Usage {
                input_tokens: prompt,
                output_tokens: output,
                ..Default::default()
            }))
        };
        let events = if *iteration > self.calls {
            let mut events = vec![ProviderEvent::TextDelta("done building".into())];
            events.extend(counts(10));
            events.push(ProviderEvent::Done {
                finish_reason: "stop".into(),
            });
            events
        } else {
            let i = *iteration;
            let (name, args) = match i % 3 {
                1 => ("read_file", serde_json::json!({"path": "big.txt"})),
                2 => (
                    "write_file",
                    serde_json::json!({
                        "path": format!("out/{i}.txt"),
                        "content": format!("file {i} line 0\n{}", "filler line\n".repeat(self.lines)),
                    }),
                ),
                _ => (
                    "edit_file",
                    serde_json::json!({
                        "path": format!("out/{}.txt", i - 1),
                        "old_string": format!("file {} line 0", i - 1),
                        "new_string": format!("file {} line 0 edited\nmore context", i - 1),
                    }),
                ),
            };
            let mut events = vec![ProviderEvent::ToolCall(ToolCall {
                id: format!("call_{i}"),
                name: name.into(),
                args,
            })];
            events.extend(counts(20));
            events.push(ProviderEvent::Done {
                finish_reason: "tool_calls".into(),
            });
            events
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
            // TensorX-shaped: a 1M window, so trigger_fraction compaction
            // would sit at 700k and never run. The sweep is all there is.
            max_context_tokens: 1_048_576,
        }
    }
}

/// Counts tokens and nothing else: the replay in this file never calls
/// the model.
struct Counting;

impl Provider for Counting {
    fn complete(
        &self,
        _: &CompletionRequest<'_>,
    ) -> Pin<Box<dyn Stream<Item = ProviderEvent> + Send + '_>> {
        unimplemented!("the replay never calls the model")
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
            max_context_tokens: 1_048_576,
        }
    }
}

/// Allows every prompt: the tools are real and write inside a tempdir.
struct Yes;

impl Approver for Yes {
    fn author(&self) -> Author {
        Author::User(aigentic_core::UserId("steve".into()))
    }
    fn ask(&mut self, _: &aigentic_log::PermissionRequestedPayload) -> Answer {
        Answer::Allow
    }
    fn ask_human(&mut self, _: &str) -> Option<String> {
        None
    }
}

/// What one scripted turn left behind.
struct Ran {
    /// Every request the provider answered, in order.
    seen: Seen,
    /// The log, event for event: the whole thread, which for one turn is
    /// exactly what the turn appended.
    events: Vec<Event>,
    /// Every event's kind, in order, as it was appended: `marks[i]` is
    /// `events[i].kind`.
    marks: Vec<EventKind>,
    /// The runtime, for a turn on the same thread.
    runtime: Runtime,
}

impl Ran {
    /// How many sweeps the turn appended.
    fn sweeps(&self) -> usize {
        self.marks
            .iter()
            .filter(|k| **k == EventKind::ContextEvicted)
            .count()
    }

    /// How many saturation lines the turn appended.
    fn saturations(&self) -> usize {
        self.marks
            .iter()
            .filter(|k| **k == EventKind::ContextSaturated)
            .count()
    }

    /// The index of each sweep's own event.
    fn sweep_marks(&self) -> Vec<usize> {
        self.marks
            .iter()
            .enumerate()
            .filter(|(_, k)| **k == EventKind::ContextEvicted)
            .map(|(i, _)| i)
            .collect()
    }
}

/// Runs one `Building` turn and returns what it left behind.
async fn run_turn(calls: u32, lines: usize, settings: aigentic_runtime::CompactionSettings) -> Ran {
    run_turn_with(calls, lines, settings, true).await
}

/// `run_turn`, with the fixture's usage reports switchable (T4).
async fn run_turn_with(
    calls: u32,
    lines: usize,
    settings: aigentic_runtime::CompactionSettings,
    usage: bool,
) -> Ran {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("out")).unwrap();
    std::fs::write(dir.path().join("big.txt"), "seed line\n".repeat(lines)).unwrap();
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let provider = Building {
        seen: seen.clone(),
        iteration: Mutex::new(0),
        calls,
        lines,
        usage,
    };
    let registry = aigentic_tools::ToolRegistry::builtin(
        aigentic_tools::Workdir::new(dir.path()),
        aigentic_tools::DEFAULT_TIMEOUT,
    );
    let log = ThreadLog::open(dir.path(), ulid::Ulid::generate()).unwrap();
    // The log outlives the helper; the tempdir is kept, not dropped.
    let kept = dir.keep();
    let mut runtime = Runtime::new(Box::new(provider), registry, log, AgentId("worker".into()))
        .with_approver(Box::new(Yes))
        .with_compaction(settings)
        .with_budget(aigentic_core::Budget {
            max_iterations: calls + 20,
            max_tokens: u64::MAX,
            max_wall_time: std::time::Duration::from_secs(300),
            cache_read_price_ratio: 0.25,
        })
        .with_model_label("scripted");
    let mut marks = Vec::new();
    let outcome = runtime
        .run_turn(
            Author::User(aigentic_core::UserId("steve".into())),
            vec![ContentBlock::Text("build it".into())],
            &mut |s| {
                if let aigentic_runtime::Signal::Event(e) = s {
                    marks.push(e.kind);
                }
            },
        )
        .await
        .unwrap();
    assert_eq!(outcome.reason, "done", "{} iterations", outcome.iterations);
    let file = std::fs::read_dir(&kept)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .expect("the log file");
    let events: Vec<Event> = std::fs::read_to_string(file)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(
        marks.len(),
        events.len(),
        "every event of the log was appended in this turn and reported"
    );
    Ran {
        seen,
        events,
        marks,
        runtime,
    }
}

fn flat_text(messages: &[Message]) -> String {
    messages
        .iter()
        .flat_map(|m| m.blocks.iter())
        .map(|b| match b {
            ContentBlock::Text(t) => t.clone(),
            ContentBlock::ToolResult(r) => r.content.clone(),
            ContentBlock::ToolCall(c) => c.args.to_string(),
            _ => String::new(),
        })
        .collect()
}

/// The seqs of the turn's completed calls, oldest first. This fixture is
/// a single turn, and every result in it belongs to one of its calls.
fn turn_calls(events: &[Event]) -> Vec<u64> {
    events
        .iter()
        .filter(|e| e.kind == EventKind::ToolResult)
        .map(|e| e.seq)
        .collect()
}

/// The `through_seq` of each `context_evicted` in the log, in order.
fn sweep_seqs(events: &[Event]) -> Vec<u64> {
    events
        .iter()
        .filter(|e| e.kind == EventKind::ContextEvicted)
        .map(|e| {
            serde_json::from_value::<ContextEvictedPayload>(e.payload.clone())
                .expect("an eviction payload")
                .through_seq
        })
        .collect()
}

/// Each turn's completed calls, in log order: one count per `turn_ended`
/// segment (plus a trailing segment if the log does not end on one).
/// `Runtime::sweep_decisions` returns one decision per call in this same
/// order, so these counts bucket its flat list back into turns.
fn turn_call_counts(events: &[Event]) -> Vec<usize> {
    let mut counts = Vec::new();
    let mut start = 0;
    while start < events.len() {
        let end = events[start..]
            .iter()
            .position(|e| e.kind == EventKind::TurnEnded)
            .map_or(events.len(), |i| start + i + 1);
        counts.push(
            events[start..end]
                .iter()
                .filter(|e| e.kind == EventKind::ToolResult)
                .count(),
        );
        start = end;
    }
    counts
}

/// Estimated tokens of the projection with the boundary moved to
/// `through_seq`, over the log as it stands: the sweep's own probe, run
/// from the test instead of the runtime.
fn tokens_with_boundary(events: &[Event], through_seq: u64) -> u64 {
    let mut projected = events.to_vec();
    let last = projected.last().expect("a turn has events").clone();
    projected.push(Event {
        id: ulid::Ulid::generate(),
        thread_id: last.thread_id,
        seq: last.seq + 1,
        kind: EventKind::ContextEvicted,
        author: Author::System,
        payload: serde_json::to_value(ContextEvictedPayload {
            through_seq,
            ratio: None,
        })
        .unwrap(),
        parent_event: None,
        created_at: last.created_at,
    });
    estimate(&project_body(&projected).unwrap())
}

/// Every token of the turn's material, before any sweep stubs it: the
/// ceiling on what all the sweeps together can free.
fn unswept_material(events: &[Event]) -> u64 {
    let unswept: Vec<Event> = events
        .iter()
        .filter(|e| {
            !matches!(
                e.kind,
                EventKind::ContextEvicted | EventKind::ContextSaturated
            )
        })
        .cloned()
        .collect();
    estimate(&project_body(&unswept).unwrap())
}

/// The most tokens any one call adds to the projection: the material the
/// schedule has to wait for on top of the floor. Measured from the log,
/// not hand-computed.
fn largest_call_growth(events: &[Event]) -> u64 {
    let unswept = |end: u64| {
        let up_to: Vec<Event> = events
            .iter()
            .filter(|e| {
                e.seq <= end
                    && !matches!(
                        e.kind,
                        EventKind::ContextEvicted | EventKind::ContextSaturated
                    )
            })
            .cloned()
            .collect();
        estimate(&project_body(&up_to).unwrap())
    };
    let mut largest = 0;
    let mut previous = 0;
    for seq in turn_calls(events) {
        let now = unswept(seq);
        largest = largest.max(now.saturating_sub(previous));
        previous = now;
    }
    largest
}

#[tokio::test]
async fn a_two_hundred_call_turn_with_large_results_and_edits_stays_under_128k() {
    // The pressure line off, so the sweep reconsiders at every call: the
    // mechanics, with the gate (issue #35) as the only thing spacing the
    // sweeps.
    let count_only = aigentic_runtime::CompactionSettings {
        evict_above_tokens: 0,
        ..DEFAULT_COMPACTION
    };
    let mut ran = run_turn(200, 400, count_only).await;
    let sweeps = ran.sweeps();
    // A sweep must pay for its cache break, so no turn can hold more of
    // them than it has blocks of `min_free` material to give. The old
    // once-per-eight-calls cadence is gone: that was the thrash.
    let bound = unswept_material(&ran.events)
        / min_free(
            DEFAULT_COMPACTION.context_ceiling_tokens,
            EVICT_MIN_FREE_PERCENT,
        )
        + 1;
    assert!(
        sweeps >= 3 && (sweeps as u64) <= bound,
        "{sweeps} sweeps, bound {bound}"
    );

    // Every call stayed under the ceiling of tokens we are willing to
    // pay for, and the stubs did the shrinking.
    {
        let requests = ran.seen.lock().unwrap();
        assert!(requests.len() >= 200, "{}", requests.len());
        for (i, messages) in requests.iter().enumerate() {
            let size = estimate(messages);
            assert!(size < 128_000, "request {i} was {size} tokens");
        }
        let flat = flat_text(requests.last().unwrap());
        // Results stub to one line each; 200 calls leave at most the
        // last 12 plus the last of each distinct tool in full.
        let stubs = flat.matches("dropped from context; re-run it").count();
        assert!((150..=200).contains(&stubs), "{stubs} stubs");
        // Calls stub their arguments inside their own keys too: the
        // fixture's long-argument call is the every-third write_file
        // (`read_file`'s path and `edit_file`'s three short strings stay
        // under the cap), and its `content` keeps only a shortened head
        // with the `… [+N chars]` marker. The last of each tool and the
        // calls above the floor keep their arguments, so the count sits
        // between half and all of them.
        let long_args = (1..=200u32).filter(|i| i % 3 == 2).count();
        let shortened_args = flat.matches("… [+").count();
        assert!(
            long_args / 2 < shortened_args && shortened_args <= long_args,
            "{shortened_args} argument stubs of {long_args} long-argument calls"
        );
    }

    // The stubs persist after the turn closes: a later, small turn still
    // projects the long one under the same bound.
    ran.runtime
        .run_turn(
            Author::User(aigentic_core::UserId("steve".into())),
            vec![ContentBlock::Text("anything new?".into())],
            &mut |_| {},
        )
        .await
        .unwrap();
    {
        let requests = ran.seen.lock().unwrap();
        let last = requests.last().unwrap();
        let size = estimate(last);
        assert!(size < 128_000, "the closed turn still projects at {size}");
        assert_eq!(last.last().map(|m| m.role), Some(Role::User));
    }
}

/// T5, issue #35: a turn whose floor sits below the ceiling — so the
/// sweep can still fit it — is swept and held to the ceiling as before.
/// The amendment's "unchanged" holds for what this test pins: every
/// request under the ceiling, the boundary pushed past the last-calls
/// window, and (the rule's addition) every sweep paying for its cache
/// break. What is no longer the same is the cadence: the sweeps are
/// spaced by `min_free` of material, not by the eight-call block.
#[tokio::test]
async fn the_ceiling_drives_the_sweep_deeper_than_the_last_calls_window() {
    // A write costs about 8.5k tokens and a read about 3.9k, so the
    // last 12 calls sit over a 44k ceiling while the last block of
    // eight — which the sweep never stubs past — fits under it: the
    // sweep must evict into the last-calls window, block by block,
    // and never past the last block.
    let ran = run_turn(
        60,
        1_400,
        aigentic_runtime::CompactionSettings {
            context_ceiling_tokens: 44_000,
            ..DEFAULT_COMPACTION
        },
    )
    .await;
    let ceiling = 44_000;
    let must_free = min_free(ceiling, EVICT_MIN_FREE_PERCENT);
    // T5's premise, measured rather than assumed: the floor is under
    // the ceiling, so this turn is never saturated.
    let calls = turn_calls(&ran.events);
    let floor = calls[calls.len() - 8];
    let at_floor = tokens_with_boundary(&ran.events, floor);
    assert!(
        at_floor < ceiling,
        "the fixture's floor must fit under the ceiling: {at_floor} at {floor}"
    );
    assert_eq!(ran.saturations(), 0, "{:?}", ran.marks);

    let requests = ran.seen.lock().unwrap();
    for (i, messages) in requests.iter().enumerate() {
        let size = estimate(messages);
        assert!(size < ceiling, "request {i} was {size} tokens");
    }
    let flat = flat_text(requests.last().unwrap());
    // Deeper than the last-calls rule alone would go (48 of 60): the
    // ceiling pushed the boundary past it.
    let stubs = flat.matches("dropped from context; re-run it").count();
    assert!(stubs >= 50, "only {stubs} calls stubbed");

    // The gate applies to a fitting turn too: every sweep freed enough
    // to pay for itself, and no two sweeps without a `min_free` of
    // evictable growth between them.
    // Every sweep freed at least `min_free`, and that freed measure *is*
    // the evictable material the turn has added since the last sweep:
    // the boundary only moves on a sweep, so what a sweep takes out is
    // exactly what has become evictable behind it.
    let tokens_at = |end: usize| estimate(&project_body(&ran.events[..end]).unwrap());
    for (n, mark) in ran.sweep_marks().iter().enumerate() {
        let freed = tokens_at(*mark).saturating_sub(tokens_at(mark + 1));
        assert!(
            freed >= must_free,
            "sweep {} freed only {freed} of the {must_free} it must free",
            n + 1
        );
    }
}

#[tokio::test]
async fn a_small_context_is_never_swept() {
    // Forty calls of reading stay far under the 64k line: nothing is
    // stubbed, so the turn keeps everything it read (issue #32).
    let ran = run_turn(40, 300, DEFAULT_COMPACTION).await;
    assert_eq!(ran.sweeps(), 0);
    let requests = ran.seen.lock().unwrap();
    let flat = flat_text(requests.last().unwrap());
    assert_eq!(flat.matches("dropped from context; re-run it").count(), 0);
}

#[tokio::test]
async fn the_cached_prefix_is_stable_between_sweeps() {
    let count_only = aigentic_runtime::CompactionSettings {
        evict_above_tokens: 0,
        ..DEFAULT_COMPACTION
    };
    // Long enough, and with big enough calls, that the gate lets several
    // sweeps through: issue #35 spaces them by `min_free`, not by block.
    let ran = run_turn(120, 900, count_only).await;
    let sweeps = ran.sweeps();
    assert!(
        sweeps >= 3,
        "the turn must sweep to test stability, got {sweeps}"
    );
    let requests = ran.seen.lock().unwrap();
    // Between sweeps the context only grows at the end: each request is
    // the previous one plus what the turn appended, so the provider's
    // cached prefix survives. A sweep is the only thing allowed to break
    // it, and it breaks exactly the one request that follows it.
    let mut broken = 0;
    for pair in requests.windows(2) {
        let (before, after) = (&pair[0], &pair[1]);
        let extends = after.len() >= before.len() && after[..before.len()] == before[..];
        if !extends {
            broken += 1;
        }
    }
    assert_eq!(broken, sweeps, "only sweeps may break the prefix");
}

/// T3, issue #52: the calibration is a rescaling of the ceiling, not a
/// tuned number. The rule prices a context as `calibrated(est)`; a context
/// the estimator prices at `e` and the provider counted at `c` teaches the
/// ratio `c/e`, and with that ratio the price is `c` — so a ceiling `C` is
/// compared with what the provider counted for the context, the same
/// comparison the estimate makes against `C` scaled by the gap. The gap
/// is the fixture's (a provider that counted 30 for a context the
/// estimator makes 18 of); every expected price is the code's own.
#[test]
fn the_calibration_prices_a_context_as_the_provider_counted_it() {
    // 72 bytes of text: the estimator's own rate, not a hand-computed 18.
    let text = "x".repeat(72);
    let context = vec![Message {
        role: Role::User,
        author: Author::User(aigentic_core::UserId("steve".into())),
        blocks: vec![ContentBlock::Text(text)],
    }];
    let est = estimate(&context);
    let counted = 30;
    assert!(
        counted > est,
        "the fixture's provider counts more than the estimator does"
    );
    let gap = counted as f64 / est as f64;

    // Warm-up: the seed 1.0 moves `RATIO_SMOOTHING` of the way to the
    // sample, so the price reaches the count after several calls, not one.
    let mut ratio = 1.0;
    let mut calls = 0;
    while calibrated(est, 0, ratio) != counted {
        ratio = next_ratio(ratio, counted, est, 0);
        calls += 1;
        assert!(
            calls < 1_000,
            "the price stopped at {} short of the count {counted}",
            calibrated(est, 0, ratio)
        );
    }
    assert!(calls > 1, "one call already priced it: {calls}");

    // With the ratio at the gap, the line the rule draws over a ceiling
    // `c` is the estimate's own line over `c` scaled by the gap.
    let c = est * 64;
    for factor in [8u64, 128] {
        let e = est * factor;
        assert_eq!(
            calibrated(e, 0, ratio) > c,
            (e as f64) > (c as f64) / gap,
            "the price of {e} against {c}"
        );
    }
}

/// T4, issue #52: a turn whose provider reports no usage never calibrates
/// — the ratio keeps its seed — and its sweeps still run on the estimate
/// alone. The assertions are `the_cached_prefix_is_stable_between_sweeps`'s
/// own, run against a provider that reports nothing, so the default path
/// cannot drift once calibration exists.
#[tokio::test]
async fn a_turn_that_reports_no_usage_keeps_the_ratio_at_its_seed() {
    let count_only = aigentic_runtime::CompactionSettings {
        evict_above_tokens: 0,
        ..DEFAULT_COMPACTION
    };
    let ran = run_turn_with(120, 900, count_only, false).await;
    assert_eq!(
        ran.runtime.eviction_ratio(),
        1.0,
        "no reported usage, nothing learned"
    );
    assert_ne!(
        ran.runtime.eviction_ratio(),
        1.0 + RATIO_SMOOTHING,
        "the seed is the untouched starting point"
    );
    let sweeps = ran.sweeps();
    assert!(
        sweeps >= 3,
        "the turn must sweep to test stability, got {sweeps}"
    );
    let requests = ran.seen.lock().unwrap();
    // Between sweeps the context only grows at the end, as in the
    // reporting fixture: the sweep is the only thing that breaks the
    // provider's cached prefix.
    let mut broken = 0;
    for pair in requests.windows(2) {
        let (before, after) = (&pair[0], &pair[1]);
        let extends = after.len() >= before.len() && after[..before.len()] == before[..];
        if !extends {
            broken += 1;
        }
    }
    assert_eq!(broken, sweeps, "only sweeps may break the prefix");
}

/// The settings of the floor-over-ceiling fixture (T1, T2): a 30k
/// ceiling, with the default 64k pressure line lowered to it by the
/// runtime.
fn saturated() -> aigentic_runtime::CompactionSettings {
    aigentic_runtime::CompactionSettings {
        context_ceiling_tokens: 30_000,
        ..DEFAULT_COMPACTION
    }
}

/// T1, issue #35. The plan's name, kept, with the amendment's rule: the
/// turn goes on sweeping at a spaced interval (once per `min_free` of new
/// material) rather than once per block of calls, and never stops — but
/// "sweeps once" is not "every call": the fixture's 60 calls sweep far
/// fewer times than that. The fixture is the plan's: 1_400-line calls at
/// a 30k ceiling, whose last block of eight (the floor the sweep may
/// never stub past) is over the ceiling, so no boundary the sweep can
/// reach fits the turn.
#[tokio::test]
async fn a_context_whose_floor_is_above_the_ceiling_sweeps_once_not_every_call() {
    let ran = run_turn(60, 1_400, saturated()).await;
    let calls = turn_calls(&ran.events);
    assert_eq!(calls.len(), 60);
    let ceiling = saturated().context_ceiling_tokens;
    let must_free = min_free(ceiling, EVICT_MIN_FREE_PERCENT);
    let floor = calls[calls.len() - 8];
    let at_floor = tokens_with_boundary(&ran.events, floor);
    assert!(
        at_floor > ceiling,
        "the fixture must have a floor over the ceiling: {at_floor} at {floor}"
    );

    // One line per spell, and the walk finds the floor by call 28.
    assert_eq!(ran.saturations(), 1, "{:?}", ran.marks);
    let spell = ran
        .marks
        .iter()
        .position(|k| *k == EventKind::ContextSaturated)
        .expect("a saturation line");
    let call_at_spell = ran.marks[..spell]
        .iter()
        .filter(|k| **k == EventKind::ToolResult)
        .count();
    assert!(
        call_at_spell <= 12 + 2 * 8,
        "the saturation came at call {call_at_spell}"
    );

    // Every sweep paid for its cache break, and the boundary only moved
    // again once the turn had added EVICT_MIN_FREE of evictable material.
    // Both are measured from the log's own projections.
    let sweeps = ran.sweep_marks();
    assert!(sweeps.len() >= 2, "only {} sweeps", sweeps.len());
    assert!(sweeps.len() < 60, "the turn swept every call");
    // Every sweep paid for its cache break, and that freed measure *is*
    // the evictable material that has arrived since the last sweep (the
    // boundary only moves on a sweep), so the turn cannot hold more
    // sweeps than it has blocks of `min_free` to give.
    let tokens_at = |end: usize| estimate(&project_body(&ran.events[..end]).unwrap());
    for (n, mark) in sweeps.iter().enumerate() {
        let freed = tokens_at(*mark).saturating_sub(tokens_at(mark + 1));
        assert!(
            freed >= must_free,
            "sweep {} freed only {freed} of the {must_free} it must free",
            n + 1
        );
    }

    // Held or not, the context stays within a hold of the floor.
    let requests = ran.seen.lock().unwrap();
    let prefix = estimate(&requests[0][..requests[0].len() - 1]);
    let bound = prefix + at_floor + must_free + largest_call_growth(&ran.events);
    let last = estimate(requests.last().unwrap());
    assert!(
        last <= bound,
        "the last request was {last} tokens, over the {bound} the floor allows"
    );
}

/// T2, issue #35. Same fixture: the hold appends nothing, and the
/// saturation line is not projected (see the log crate's T3), so the
/// only thing that may break the provider's cached prefix is a sweep.
#[tokio::test]
async fn the_boundary_holds_while_saturated() {
    let ran = run_turn(60, 1_400, saturated()).await;
    assert_eq!(ran.saturations(), 1, "{:?}", ran.marks);
    assert!(ran.sweeps() > 0, "the fixture must sweep");

    let requests = ran.seen.lock().unwrap();
    let mut broken = 0;
    for pair in requests.windows(2) {
        let (before, after) = (&pair[0], &pair[1]);
        let extends = after.len() >= before.len() && after[..before.len()] == before[..];
        if !extends {
            broken += 1;
        }
    }
    assert_eq!(
        broken,
        ran.sweeps(),
        "the boundary held: only a sweep may break the prefix"
    );
}

/// Issue #35, review item 3: what `evict_min_free_percent = 0` actually
/// does. With no minimum, `freed < min_free(ceiling, 0) = 0` never holds,
/// so the boundary moves to the floor the moment a call advances it — one
/// sweep per call past the block, however little each frees. That is not
/// the pre-#35 rule: the sweep still targets the floor; it just never
/// waits for the move to be worth anything.
#[tokio::test]
async fn zero_evict_min_free_percent_sweeps_as_soon_as_the_floor_advances() {
    let zero = aigentic_runtime::CompactionSettings {
        evict_min_free_percent: 0,
        ..saturated()
    };
    let ran = run_turn(60, 1_400, zero).await;
    let calls = turn_calls(&ran.events);
    let seqs = sweep_seqs(&ran.events);
    assert!(seqs.len() >= 2, "the fixture must sweep: {:?}", ran.marks);
    // Every sweep moves the boundary by exactly one call: the floor
    // advanced by one, and nothing waited for it to be worth it.
    for pair in seqs.windows(2) {
        let from = calls
            .iter()
            .position(|s| *s == pair[0])
            .expect("a sweep's call");
        let to = calls
            .iter()
            .position(|s| *s == pair[1])
            .expect("a sweep's call");
        assert_eq!(
            to,
            from + 1,
            "the boundary jumped from call {from} to call {to}"
        );
    }
    // The gated default on the same fixture sweeps far less often.
    let gated = run_turn(60, 1_400, saturated()).await;
    assert!(
        seqs.len() > gated.sweeps(),
        "zero swept {} times, the gated default {}",
        seqs.len(),
        gated.sweeps()
    );
}

/// T3's fixture lives in the log crate (a `context_saturated` event
/// changes no projection); T4's is a payload round-trip there too.
///
/// The reference check, issue #35: the rule replayed over the real build
/// thread that thrashed. Ignored by default; run it with
/// `AIGENTIC_REPLAY_LOG=<thread>.jsonl cargo test -p aigentic-runtime
/// --test evict replay -- --ignored --nocapture`.
///
/// The run recorded the provider's own token counts, and on this log the
/// estimate under-counts them by about 1.4x, so replaying at the default
/// 128k would put the replay in the fitting regime while the run itself
/// was pressed against the ceiling. The replay's ceiling is therefore the
/// run's 128k in the estimate's units, both numbers read from the log:
/// the biggest recorded input against the estimate of the same context.
/// Every assertion below is the rule's, not a tuned number.
#[tokio::test]
// The plan's name, the thread's ULID as written in it.
#[allow(non_snake_case)]
#[ignore = "needs a real thread log; set AIGENTIC_REPLAY_LOG"]
async fn thread_01M3GS3QP6_replays_without_thrash() {
    let path = std::env::var("AIGENTIC_REPLAY_LOG").expect("set AIGENTIC_REPLAY_LOG");
    let text = std::fs::read_to_string(&path).expect("the thread log");
    let events: Vec<Event> = text
        .lines()
        .map(|l| serde_json::from_str(l).expect("a log line"))
        .collect();

    // What the provider counted at its fullest call, against what the
    // estimate says of the same context: the run's own ceiling in the
    // estimate's units.
    let (seq, counted) = events
        .iter()
        .filter_map(|e| {
            let p: aigentic_log::AssistantMessagePayload =
                serde_json::from_value(e.payload.clone()).ok()?;
            Some((e.seq, p.usage?.input_tokens))
        })
        .max_by_key(|(_, tokens)| *tokens)
        .expect("a recorded call");
    let at = events.iter().position(|e| e.seq == seq).expect("the call");
    let estimated = estimate(&project_body(&events[..=at]).unwrap());
    let ceiling = DEFAULT_COMPACTION.context_ceiling_tokens * estimated / counted;
    let must_free = min_free(ceiling, EVICT_MIN_FREE_PERCENT);

    let dir = tempfile::tempdir().unwrap();
    let log = ThreadLog::open(dir.path(), ulid::Ulid::generate()).unwrap();
    let mut runtime = Runtime::new(
        Box::new(Counting),
        aigentic_tools::ToolRegistry::builtin(
            aigentic_tools::Workdir::new(dir.path()),
            aigentic_tools::DEFAULT_TIMEOUT,
        ),
        log,
        AgentId("worker".into()),
    )
    .with_compaction(aigentic_runtime::CompactionSettings {
        context_ceiling_tokens: ceiling,
        ..DEFAULT_COMPACTION
    });
    let clock = std::time::Instant::now();
    let decisions = runtime.sweep_decisions(&events).unwrap();
    let sweeps: Vec<u64> = decisions
        .iter()
        .filter_map(|d| match d {
            Decision::Sweep { freed, .. } => Some(*freed),
            _ => None,
        })
        .collect();
    // `sweep_decisions` returns one decision per completed call, turn by
    // turn, so each turn's call count buckets the flat list back into
    // turns. A `Saturated` *decision* is per call; the runtime appends a
    // `context_saturated` *event* at most once per turn (the shipped
    // rule), so a turn with any saturated decision contributes one spell.
    let turn_counts = turn_call_counts(&events);
    let mut buckets = Vec::new();
    let mut cursor = 0;
    let mut saturation_decisions = 0usize;
    let mut spells = 0usize;
    for (turn, count) in turn_counts.iter().enumerate() {
        let slice = &decisions[cursor..cursor + count];
        cursor += count;
        let swept = slice
            .iter()
            .filter(|d| matches!(d, Decision::Sweep { .. }))
            .count();
        let saturated = slice
            .iter()
            .filter(|d| matches!(d, Decision::Saturated { .. }))
            .count();
        saturation_decisions += saturated;
        if saturated > 0 {
            spells += 1;
        }
        buckets.push((turn, *count, swept, saturated, usize::from(saturated > 0)));
    }
    let in_log = events
        .iter()
        .filter(|e| e.kind == EventKind::ContextEvicted)
        .count();

    // Every sweep paid for its cache break, and no sweep could come
    // before the turn had added that much evictable material again: the
    // freed material of a sweep is all of it that had not been stubbed
    // before, so the sweeps are bounded by the turn's own material.
    for (i, freed) in sweeps.iter().enumerate() {
        assert!(
            *freed >= must_free,
            "replayed sweep {} freed only {freed} of the {must_free} it must free",
            i + 1
        );
    }
    let material = unswept_material(&events);
    let bound = material / must_free + 1;
    println!(
        "{} calls replayed in {:?} over {} turns: {} sweeps, the log has {in_log}; \
         {saturation_decisions} saturation decisions (one per call), {spells} saturation spells \
         (events the runtime would append, once per turn — the shipped rule); ceiling {ceiling} \
         est (the run counted {counted} over {estimated} est), material {material}, bound {bound}",
        decisions.len(),
        clock.elapsed(),
        turn_counts.len(),
        sweeps.len(),
    );
    for (turn, count, swept, saturated, sp) in &buckets {
        println!(
            "  turn {turn}: calls={count} sweeps={swept} saturation_decisions={saturated} spells={sp}"
        );
    }
    assert!(
        (sweeps.len() as u64) <= bound,
        "{} sweeps over the {bound} the log's material allows",
        sweeps.len()
    );
    assert!(
        sweeps.len() < in_log,
        "the replay thrashes as much as the log did: {} vs {in_log}",
        sweeps.len()
    );
    assert!(
        spells > 0,
        "the replay must reach the regime the issue is about"
    );
}

/// Issue #51: the projection's argument stubs stay inside each tool's own
/// schema, so a model that copies what it sees in history (the #17 lesson)
/// still sends a call the tool accepts.
#[test]
fn argument_stubs_still_deserialise_as_their_tools_arguments() {
    let command = format!(
        "{}\necho done\ncd crates/log && cargo test\n",
        "cargo test --workspace ".repeat(12)
    );
    let bash = ToolCall {
        id: "c1".into(),
        name: "bash".into(),
        args: serde_json::json!({"command": command, "timeout_secs": 900}),
    };
    let ContentBlock::ToolCall(stub) = aigentic_log::shorten_call_args(&bash) else {
        panic!("a tool call projects as a tool call");
    };
    assert_eq!(stub.name, "bash");
    assert_eq!(stub.args["timeout_secs"], serde_json::json!(900));
    assert!(
        stub.args["command"].as_str().unwrap().chars().count()
            < bash.args["command"].as_str().unwrap().chars().count(),
        "the stub is shorter: {}",
        stub.args["command"]
    );
    let _: aigentic_tools::BashArgs =
        serde_json::from_value(stub.args.clone()).expect("the bash stub still fits BashArgs");

    let update = ToolCall {
        id: "c2".into(),
        name: "update_tasks".into(),
        args: serde_json::json!({"tasks": [
            {"text": "read the plan", "state": "done"},
            {"text": "implement the argument stubs ".repeat(12), "state": "active"},
        ]}),
    };
    let ContentBlock::ToolCall(stub) = aigentic_log::shorten_call_args(&update) else {
        panic!("a tool call projects as a tool call");
    };
    let parsed: aigentic_runtime::harness_tools::UpdateTasksArgs =
        serde_json::from_value(stub.args.clone())
            .expect("the update_tasks stub still fits UpdateTasksArgs");
    assert_eq!(parsed.tasks.len(), 2, "the list keeps its length");
    assert_eq!(
        parsed.tasks[0].state,
        aigentic_runtime::harness_tools::TaskState::Done
    );
    assert_eq!(
        parsed.tasks[1].state,
        aigentic_runtime::harness_tools::TaskState::Active
    );
}

/// Estimated tokens of one text, through the same estimator the runtime
/// uses; used for the argument-share numbers below.
fn estimate_text(text: &str) -> u64 {
    estimate(&[Message {
        role: Role::User,
        author: Author::System,
        blocks: vec![ContentBlock::Text(text.into())],
    }])
}

/// Issue #51's reference check: the projection's floor at the deepest
/// boundary the real build thread recorded, before and after the
/// argument stubs. Ignored by default; run it with
/// `AIGENTIC_REPLAY_LOG=<thread>.jsonl cargo test -p aigentic-runtime
/// --test evict floor -- --ignored --nocapture`.
///
/// "Before" is recomputed from the projection: every call whose projected
/// arguments differ from the log's own is restored by id from the log,
/// then estimated again. Nothing below is a typed literal, and the only
/// assertions are directional.
#[tokio::test]
// The plan's name, the thread's ULID as written in it.
#[allow(non_snake_case)]
#[ignore = "needs a real thread log; set AIGENTIC_REPLAY_LOG"]
async fn thread_01M3GS3QP6_floor_shrinks_at_deepest_boundary() {
    let path = std::env::var("AIGENTIC_REPLAY_LOG").expect("set AIGENTIC_REPLAY_LOG");
    let text = std::fs::read_to_string(&path).expect("the thread log");
    let events: Vec<Event> = text
        .lines()
        .map(|l| serde_json::from_str(l).expect("a log line"))
        .collect();

    // The deepest boundary in the log: the eviction with the greatest
    // `through_seq`, and the log truncated at that event.
    let (at, through) = events
        .iter()
        .enumerate()
        .filter(|(_, e)| e.kind == EventKind::ContextEvicted)
        .map(|(i, e)| {
            (
                i,
                serde_json::from_value::<ContextEvictedPayload>(e.payload.clone())
                    .expect("an eviction payload")
                    .through_seq,
            )
        })
        .max_by_key(|(_, through)| *through)
        .expect("the log has an eviction");
    let truncated = &events[..=at];

    // Every call the log holds, by id: what the projection is compared
    // against.
    let full: std::collections::HashMap<String, (String, serde_json::Value)> = truncated
        .iter()
        .filter(|e| e.kind == EventKind::AssistantMessage)
        .filter_map(|e| {
            serde_json::from_value::<aigentic_log::AssistantMessagePayload>(e.payload.clone()).ok()
        })
        .flat_map(|p| p.blocks)
        .filter_map(|b| match b {
            ContentBlock::ToolCall(c) => Some((c.id, (c.name, c.args))),
            _ => None,
        })
        .collect();

    let after = project_body(truncated).unwrap();
    let mut before = after.clone();
    let mut stubbed = 0usize;
    for message in &mut before {
        for block in &mut message.blocks {
            let ContentBlock::ToolCall(c) = block else {
                continue;
            };
            let Some((_, args)) = full.get(&c.id) else {
                continue;
            };
            if &c.args != args {
                stubbed += 1;
                c.args = args.clone();
            }
        }
    }

    // The bash `command` share, measured with the same estimator over the
    // commands the two projections carry.
    let bash_share = |messages: &[Message]| {
        let text: String = messages
            .iter()
            .flat_map(|m| m.blocks.iter())
            .filter_map(|b| match b {
                ContentBlock::ToolCall(c) if c.name == "bash" => {
                    Some(c.args.get("command")?.as_str()?.to_owned())
                }
                _ => None,
            })
            .collect();
        estimate_text(&text)
    };
    let (floor_after, floor_before) = (estimate(&after), estimate(&before));
    let (bash_after, bash_before) = (bash_share(&after), bash_share(&before));

    println!(
        "boundary at event {at} (through_seq {through}): {} calls shortened; \
         floor {floor_before} est before, {floor_after} est after ({} saved); \
         bash command share {bash_before} est before, {bash_after} est after",
        stubbed,
        floor_before.saturating_sub(floor_after),
    );

    assert!(stubbed > 0, "the deepest boundary stubs calls");
    assert!(
        floor_after < floor_before,
        "the argument stubs must lower the floor: {floor_after} vs {floor_before}"
    );
    assert!(
        bash_after < bash_before,
        "the bash share must shrink: {bash_after} vs {bash_before}"
    );
}
