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
    Answer, Approver, DEFAULT_COMPACTION, Decision, EVICT_MIN_FREE_PERCENT, Runtime, min_free,
};
use futures_core::Stream;

/// Every request's messages.
type Seen = Arc<Mutex<Vec<Vec<Message>>>>;

/// Keeps the turn going: one tool call per reply for `calls` iterations
/// (a read, a write and an edit per three), then a bare text reply.
/// Reports usage from the same estimate the runtime uses.
struct Building {
    seen: Seen,
    iteration: Mutex<u32>,
    calls: u32,
    /// Lines in the read fixture and in each written file; sizes the
    /// turn's material.
    lines: usize,
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
        let prompt = estimate(request.messages);
        let mut iteration = self.iteration.lock().unwrap();
        *iteration += 1;
        let events = if *iteration > self.calls {
            vec![
                ProviderEvent::TextDelta("done building".into()),
                ProviderEvent::Usage(Usage {
                    input_tokens: prompt,
                    output_tokens: 10,
                    ..Default::default()
                }),
                ProviderEvent::Done {
                    finish_reason: "stop".into(),
                },
            ]
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
            vec![
                ProviderEvent::ToolCall(ToolCall {
                    id: format!("call_{i}"),
                    name: name.into(),
                    args,
                }),
                ProviderEvent::Usage(Usage {
                    input_tokens: prompt,
                    output_tokens: 20,
                    ..Default::default()
                }),
                ProviderEvent::Done {
                    finish_reason: "tool_calls".into(),
                },
            ]
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
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("out")).unwrap();
    std::fs::write(dir.path().join("big.txt"), "seed line\n".repeat(lines)).unwrap();
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let provider = Building {
        seen: seen.clone(),
        iteration: Mutex::new(0),
        calls,
        lines,
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
        payload: serde_json::to_value(ContextEvictedPayload { through_seq }).unwrap(),
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
        let edits = flat.matches("lines]").count();
        assert!((80..=200).contains(&edits), "{edits} edit stubs");
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
    let runtime = Runtime::new(
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
    let saturations = decisions
        .iter()
        .filter(|d| matches!(d, Decision::Saturated { .. }))
        .count();
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
        "{} calls replayed in {:?}: {} sweeps, the log has {in_log}; {saturations} saturations \
         spells, ceiling {ceiling} est (the run counted {counted} over {estimated} est), \
         material {material}, bound {bound}",
        decisions.len(),
        clock.elapsed(),
        sweeps.len(),
    );
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
        saturations > 0,
        "the replay must reach the regime the issue is about"
    );
}
