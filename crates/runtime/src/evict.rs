//! In-turn eviction (issue #30): the sweep that decides what the
//! projection stubs. Compaction never touches the turn in progress —
//! summarising it would break the tool-call contract — so a long build
//! turn grows without bound. The sweep runs beside compaction at the top
//! of every loop iteration, at the same boundary, and records how far the
//! stubbing reaches.
//!
//! It appends at most one `context_evicted` event per call. The
//! boundary is what carries the cached prefix: moving it rewrites the
//! context from that point on and burns the provider's prompt cache, so
//! a sweep has to pay for itself. The originals stay in the log; the
//! model gets a stub that says how to bring a result back.
//!
//! A sweep pays for itself by freeing material (issue #35). It stubs
//! everything older than a floor — the last `EVICT_BLOCK_CALLS` calls,
//! the newest thing the turn is working on, plus the prefix and the open
//! turn's own part, which stubbing cannot reach — so `floor` is the
//! deepest legal boundary and the most a sweep can ever free. When that
//! is less than `EVICT_MIN_FREE_PERCENT` of the ceiling, the boundary
//! stays where it is: the cache break costs more than the relief. It
//! moves again once the turn has added that much evictable material
//! behind the boundary, which is once per `min_free` of new material
//! instead of once per block of calls.
//!
//! When even the floor is over `context_ceiling_tokens` the turn cannot
//! be fitted by eviction at all, and only the handoff to a fresh thread
//! can help. The sweep says so once per turn — a spell and a turn
//! coincide, since the floor only grows within a turn, so a later
//! `context_evicted` can never unsay it — with a `context_saturated`
//! event, and goes on evicting what it can: the alternative — holding
//! until a probe fits again — never holds, since the floor only grows.

use aigentic_core::{Author, ContentBlock, Event, EventKind, ToolSpec};
use aigentic_log::{
    AssistantMessagePayload, ContextEvictedPayload, ContextSaturatedPayload, ToolResultPayload,
};

use crate::{CompactionSettings, Runtime, RuntimeError, Signal};

/// The newest calls the sweep never stubs, the floor: the turn is
/// working on these, and they are what the model still needs intact. The
/// deepest legal boundary is this many calls from the end.
pub const EVICT_BLOCK_CALLS: usize = 8;

/// A sweep must free at least a quarter of the ceiling to be worth
/// breaking the cached prefix for (issue #35): 32k at the 128k default.
pub const EVICT_MIN_FREE_PERCENT: u32 = 25;

/// What a sweep must free from the projection to be worth its cache
/// break: `percent` of the ceiling.
pub const fn min_free(ceiling: u64, percent: u32) -> u64 {
    ceiling * percent as u64 / 100
}

/// How much of the distance to a new sample the calibration moves per
/// call (issue #52): an EWMA, slow enough that one absurd reported count
/// cannot rewrite what every size comparison is priced in.
pub const RATIO_SMOOTHING: f64 = 0.2;

/// The calibration's floor and ceiling (issue #52). The tokenizer factor
/// between the estimate and a real count sits well inside these, so a
/// ratio at either edge is a bug in the sample, not in the model.
pub const RATIO_MIN: f64 = 0.5;
pub const RATIO_MAX: f64 = 2.5;

/// One step of the calibration (issue #52): the reported prompt for a
/// context over `estimate + overhead` — what the estimator makes of the
/// messages, and what the tool schemas cost on the wire — pulled
/// [`RATIO_SMOOTHING`] of the way from `ratio` towards that sample, and
/// clamped to [`RATIO_MIN`]..[`RATIO_MAX`]. A request with nothing in it
/// says nothing about the tokenizer and leaves the ratio alone.
pub fn next_ratio(ratio: f64, reported: u64, estimate: u64, overhead: u64) -> f64 {
    if estimate + overhead == 0 {
        return ratio;
    }
    let sample = reported as f64 / (estimate + overhead) as f64;
    (ratio + RATIO_SMOOTHING * (sample - ratio)).clamp(RATIO_MIN, RATIO_MAX)
}

/// What a request's tool schemas cost on the wire, at the estimator's own
/// bytes-per-token rate (issue #52): `estimate_tokens` is given the
/// messages alone, so this is the part of a request it does not cover.
pub fn schemas_tokens(specs: &[ToolSpec]) -> u64 {
    let bytes = serde_json::to_string(specs).map_or(0, |json| json.len());
    (bytes as u64).div_ceil(4)
}

/// An estimate of a whole context in reported tokens (issue #52): the
/// calibration scales the messages and the schemas alike, so the result
/// is comparable with a ceiling, which is stated in reported units.
pub fn calibrated(estimate: u64, overhead: u64, ratio: f64) -> u64 {
    ((estimate + overhead) as f64 * ratio).round() as u64
}

/// The same for the difference between two contexts, where the schemas
/// cancel (issue #52): what an appended delta is worth in reported
/// tokens, given a prompt size in front of it that is already real.
pub fn calibrated_delta(estimate: u64, ratio: f64) -> u64 {
    (estimate as f64 * ratio).round() as u64
}

/// What the sweep decided for the open turn (issue #35). The rule lives
/// in [`Runtime::decide`], so the replay check in the test suite can ask
/// the runtime what it would have done with a real thread's log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Move the boundary to `through_seq` — the deepest legal block —
    /// and record it as a `context_evicted`. `freed` is what the move
    /// takes out of the projection, in reported tokens, the ceiling's
    /// units (issue #52), and is at least [`min_free`].
    Sweep { through_seq: u64, freed: u64 },
    /// Move nothing and append nothing: a sweep now would either free
    /// less than [`min_free`] or has nowhere to go.
    Hold,
    /// The same hold, with the deepest legal boundary itself still over
    /// the ceiling: this turn cannot be fitted by eviction at all, which
    /// is worth saying once per turn (the floor only grows within a
    /// turn, so a spell and a turn coincide).
    Saturated {
        through_seq: u64,
        tokens_at_floor: u64,
        ceiling: u64,
    },
}

/// The open turn as the sweep reads it: its completed calls in log order
/// (the seq of each result), the deepest boundary a `context_evicted` of
/// this turn recorded, and whether this turn already said it could not
/// be fitted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Scan {
    calls: Vec<u64>,
    through: Option<u64>,
    /// Whether the open turn already appended a `context_saturated`: set
    /// by any such event in the turn, cleared only by its `turn_ended`,
    /// so the event is once per turn.
    saturated: bool,
}

impl Scan {
    /// The open turn: everything after the last `turn_ended`. A turn that
    /// ended is compactable by the rules; this sweep only ever records the
    /// turn it runs in.
    fn of(events: &[Event]) -> Scan {
        let start = events
            .iter()
            .rposition(|e| e.kind == EventKind::TurnEnded)
            .map_or(0, |i| i + 1);
        let mut scan = Scan {
            calls: completed_calls(&events[start..]),
            ..Scan::default()
        };
        for e in &events[start..] {
            match e.kind {
                EventKind::ContextEvicted => {
                    if let Ok(p) =
                        serde_json::from_value::<ContextEvictedPayload>(e.payload.clone())
                    {
                        scan.through = Some(scan.through.unwrap_or(0).max(p.through_seq));
                    }
                }
                // Sticky for the rest of the turn: any `context_saturated`
                // in it means the turn has already said this, and only
                // the next `turn_ended` clears it. A later
                // `context_evicted` does not re-arm — the floor only grows
                // within a turn, so a spell and a turn coincide.
                EventKind::ContextSaturated => scan.saturated = true,
                _ => {}
            }
        }
        scan
    }
}

/// The calls of `events` that have their result, in log order: the seq of
/// each result whose own tool call precedes it. The boundary is measured
/// in these.
fn completed_calls(events: &[Event]) -> Vec<u64> {
    let mut calls = Vec::new();
    let mut pending: Vec<String> = Vec::new();
    for e in events {
        match e.kind {
            EventKind::AssistantMessage => {
                let Ok(p) = serde_json::from_value::<AssistantMessagePayload>(e.payload.clone())
                else {
                    continue;
                };
                for block in p.blocks {
                    if let ContentBlock::ToolCall(c) = block {
                        pending.push(c.id);
                    }
                }
            }
            EventKind::ToolResult => {
                let Ok(p) = serde_json::from_value::<ToolResultPayload>(e.payload.clone()) else {
                    continue;
                };
                if pending.contains(&p.result.id) {
                    pending.retain(|id| *id != p.result.id);
                    calls.push(e.seq);
                }
            }
            _ => {}
        }
    }
    calls
}

/// The boundary that stubs `stubbed` of the turn's calls: the last one's
/// seq, or zero when the turn has no call for it to name.
fn boundary(calls: &[u64], stubbed: usize) -> u64 {
    if stubbed == 0 { 0 } else { calls[stubbed - 1] }
}

/// The boundary to probe with `stubbed` calls behind it; `None` reads the
/// log as it stands, which is the same projection when nothing is stubbed.
fn probe(calls: &[u64], stubbed: usize) -> Option<u64> {
    (stubbed > 0).then(|| calls[stubbed - 1])
}

/// A `context_evicted` for `through_seq`, made to look like it follows
/// `after`: the sweep's probe, never appended to a log. Its `ratio` stays
/// `None`: the check writes its own probes and never appends, so only the
/// event the sweep itself records carries the calibration it decided in
/// (issue #52).
fn synthetic(after: &Event, through_seq: u64) -> Event {
    Event {
        id: ulid::Ulid::generate(),
        thread_id: after.thread_id,
        seq: after.seq + 1,
        kind: EventKind::ContextEvicted,
        author: Author::System,
        payload: serde_json::to_value(ContextEvictedPayload {
            through_seq,
            ratio: None,
        })
        .expect("serialisable"),
        parent_event: None,
        created_at: after.created_at,
    }
}

impl Runtime {
    /// Stub, in projection, the open turn's results and successful
    /// edit/write arguments older than the floor; hold the boundary when
    /// a sweep cannot free enough to pay for itself, and say so when the
    /// floor itself is over the ceiling (issue #35). Whether anything was
    /// appended.
    pub(crate) fn evict_stale(
        &mut self,
        observe: &mut (dyn FnMut(Signal<'_>) + Send),
    ) -> Result<bool, RuntimeError> {
        let events = self.log.events();
        let scan = Scan::of(events);
        let (kind, payload) = match self.decide(self.compaction, events, &scan, self.ratio)? {
            Decision::Hold => return Ok(false),
            Decision::Sweep { through_seq, .. } => (
                EventKind::ContextEvicted,
                serde_json::to_value(ContextEvictedPayload {
                    through_seq,
                    ratio: Some(self.ratio),
                })
                .expect("serialisable"),
            ),
            // Once per turn, not once per sweep: the turn has already
            // said it, and only the next `turn_ended` clears the flag — a
            // `context_evicted` does not re-arm it, because the floor only
            // grows within a turn, so a spell and a turn coincide.
            Decision::Saturated { .. } if scan.saturated => return Ok(false),
            Decision::Saturated {
                through_seq,
                tokens_at_floor,
                ceiling,
            } => (
                EventKind::ContextSaturated,
                serde_json::to_value(ContextSaturatedPayload {
                    through_seq,
                    tokens_at_floor,
                    ceiling,
                    ratio: Some(self.ratio),
                })
                .expect("serialisable"),
            ),
        };
        if kind == EventKind::ContextEvicted {
            // The projection changes, so the window measure is stale.
            self.measured = None;
        }
        self.append(kind, Author::System, payload, None, observe)?;
        Ok(true)
    }

    /// What the sweep should do with the open turn, given the turn as the
    /// log holds it (issue #35). Pure: the caller appends what this
    /// decides. `ratio` is the caller's calibration (issue #52): every
    /// size it compares is an estimate in reported tokens, so the ceiling
    /// — which is stated in reported tokens — is a comparable number
    /// whether the caller is a live turn or a replay.
    fn decide(
        &self,
        settings: CompactionSettings,
        events: &[Event],
        scan: &Scan,
        ratio: f64,
    ) -> Result<Decision, RuntimeError> {
        let calls = &scan.calls;
        let evicted = match scan.through {
            Some(seq) => calls.iter().filter(|s| **s <= seq).count(),
            None => 0,
        };
        // Pressure first (issue #32): under the line nothing is stubbed.
        // Sweeping a small context saved a few thousand tokens, broke
        // the prompt cache each time, and made a reading turn forget
        // and re-read the same files (150 calls, 21 sweeps, no edit).
        // A lower ceiling lowers the line with it.
        let line = match (settings.evict_above_tokens, settings.context_ceiling_tokens) {
            (0, _) => 0,
            (line, 0) => line,
            (line, ceiling) => line.min(ceiling),
        };
        if line > 0 && !self.over_the_ceiling(events, line, None, &mut None, ratio)? {
            return Ok(Decision::Hold);
        }
        let ceiling = settings.context_ceiling_tokens;
        if ceiling == 0 {
            // Nothing to measure against, so `keep_last_calls` sets the
            // depth as it always did: all but those calls, cut to the
            // block line so the next sweep lands on the next block.
            let target = (calls.len().saturating_sub(settings.keep_last_calls) / EVICT_BLOCK_CALLS)
                * EVICT_BLOCK_CALLS;
            if target <= evicted {
                return Ok(Decision::Hold);
            }
            let freed = self.freed_by(events, calls, evicted, target, ratio)?;
            return Ok(Decision::Sweep {
                through_seq: calls[target - 1],
                freed,
            });
        }
        // The ceiling (issue #30) is what we are willing to pay for per
        // call, whatever the window. The sweep never stubs past the floor
        // — the last `EVICT_BLOCK_CALLS` calls, the newest thing the turn
        // is working on — and the floor is the deepest legal boundary, so
        // it is the most a sweep can relieve, and the only relief worth
        // measuring (issue #35). The ceiling, not the window, is what
        // decides the depth: `keep_last_calls` protects more calls than
        // the floor does, so with a ceiling set the floor is what keeps
        // the newest calls whole.
        let floor = calls.len().saturating_sub(EVICT_BLOCK_CALLS);
        if floor <= evicted {
            return Ok(Decision::Hold);
        }
        let mut scratch: Option<Vec<Event>> = None;
        let at_floor = self.context_tokens(events, probe(calls, floor), &mut scratch, ratio)?;
        let before = self.context_tokens(events, probe(calls, evicted), &mut scratch, ratio)?;
        let freed = before.saturating_sub(at_floor);
        if freed < min_free(ceiling, settings.evict_min_free_percent) {
            // The cache break costs more than the relief is worth: hold
            // the boundary where it is. The relief grows by exactly what
            // slides out of the last block, so the next sweep comes when
            // the turn has added that much evictable material — once per
            // `min_free` instead of once per block. Once the deepest legal
            // boundary is itself over the ceiling the turn cannot be
            // fitted by eviction at all, and that is worth saying: the
            // right move there is a fresh thread, not another sweep.
            if at_floor > ceiling {
                return Ok(Decision::Saturated {
                    through_seq: boundary(calls, floor),
                    tokens_at_floor: at_floor,
                    ceiling,
                });
            }
            return Ok(Decision::Hold);
        }
        Ok(Decision::Sweep {
            through_seq: calls[floor - 1],
            freed,
        })
    }

    /// Estimated tokens a move of the boundary from `from` calls stubbed
    /// to `to` would take out of the projection.
    fn freed_by(
        &self,
        events: &[Event],
        calls: &[u64],
        from: usize,
        to: usize,
        ratio: f64,
    ) -> Result<u64, RuntimeError> {
        let mut scratch = None;
        let before = self.context_tokens(events, probe(calls, from), &mut scratch, ratio)?;
        let after = self.context_tokens(events, probe(calls, to), &mut scratch, ratio)?;
        Ok(before.saturating_sub(after))
    }

    /// Whether the context, projected as the sweep would leave it with
    /// the boundary at `through` (as it stands now, when `None`), still
    /// does not fit under `ceiling`.
    fn over_the_ceiling(
        &self,
        events: &[Event],
        ceiling: u64,
        through: Option<u64>,
        scratch: &mut Option<Vec<Event>>,
        ratio: f64,
    ) -> Result<bool, RuntimeError> {
        Ok(self.context_tokens(events, through, scratch, ratio)? > ceiling)
    }

    /// Estimated tokens of the context as the sweep would leave it with
    /// the boundary at `through` (as it stands now, when `None`), in
    /// reported tokens so it can be held against the ceiling (issue
    /// #52). A probe: the synthetic event is projected, never stored, and
    /// the projection takes a boundary's deepest reach, so probing at or
    /// below the current one reads the current context. `scratch` carries
    /// the one cloned projection the ceiling walk reuses across rungs; a
    /// rung at or below the current boundary reads the log as it stands,
    /// so it clones nothing.
    fn context_tokens(
        &self,
        events: &[Event],
        through: Option<u64>,
        scratch: &mut Option<Vec<Event>>,
        ratio: f64,
    ) -> Result<u64, RuntimeError> {
        let Some(through_seq) = through else {
            let context = crate::build_context(&self.prefix(), events)?;
            let estimate = self.provider.count_tokens(&context);
            return Ok(calibrated(estimate, self.overhead, ratio));
        };
        let projected = scratch.get_or_insert_with(|| events.to_vec());
        // The walk's own projection from the rung before it: the synthetic
        // event is already the last one, so rewriting its payload re-probes
        // without cloning the log again.
        let rewrite = projected
            .last()
            .is_some_and(|e| e.kind == EventKind::ContextEvicted);
        if rewrite {
            let last = projected.last_mut().expect("checked above");
            last.payload = serde_json::to_value(ContextEvictedPayload {
                through_seq,
                ratio: None,
            })
            .expect("serialisable");
        } else if let Some(last) = projected.last().cloned() {
            projected.push(synthetic(&last, through_seq));
        }
        let context = crate::build_context(&self.prefix(), projected)?;
        let estimate = self.provider.count_tokens(&context);
        Ok(calibrated(estimate, self.overhead, ratio))
    }

    /// The calibration the last call learned (issue #52), for the check
    /// that replays a thread: the ratio `sweep_decisions` priced its
    /// decisions with. Doc-hidden, like the replay it belongs to.
    #[doc(hidden)]
    pub fn eviction_ratio(&self) -> f64 {
        self.ratio
    }

    /// Every decision the rule would take on `events`, one per completed
    /// call of each turn, as if this run had swept the log rather than
    /// the run that wrote it: the sweep itself decides, so a replay can
    /// never disagree with the runtime. Each probe projects the log as it
    /// stood at that call with *this* replay's boundary for the turn in
    /// hand — the earlier turns' own evictions stay, since a replay of the
    /// last turn happens on top of them.
    ///
    /// The ratio is learned from the log, by the same named rule the turn
    /// loop uses (issue #52): every call's own reported usage — read off
    /// its `assistant_message` — against the estimate of the probe
    /// context just built, so a replayed thread is priced out of the log
    /// alone, with no manual calibration. It starts at this runtime's
    /// ratio (1.0 unless the runtime itself has run turns) and is left
    /// there for the caller to read. Doc-hidden: the replay check in the
    /// test suite is its only caller.
    #[doc(hidden)]
    pub fn sweep_decisions(&mut self, events: &[Event]) -> Result<Vec<Decision>, RuntimeError> {
        let mut out = Vec::new();
        let mut ratio = self.ratio;
        let mut start = 0;
        while start < events.len() {
            let end = events[start..]
                .iter()
                .position(|e| e.kind == EventKind::TurnEnded)
                .map_or(events.len(), |i| start + i + 1);
            let turn = &events[start..end];
            let calls = completed_calls(turn);
            let mut through: Option<u64> = None;
            for n in 1..=calls.len() {
                let at = events[start..end]
                    .iter()
                    .position(|e| e.kind == EventKind::ToolResult && e.seq == calls[n - 1])
                    .map(|i| start + i)
                    .expect("a completed call has its result");
                let mut probe: Vec<Event> = events[..=at]
                    .iter()
                    .filter(|e| {
                        !(e.seq >= turn[0].seq
                            && matches!(
                                e.kind,
                                EventKind::ContextEvicted | EventKind::ContextSaturated
                            ))
                    })
                    .cloned()
                    .collect();
                if let Some(through_seq) = through {
                    probe.push(synthetic(
                        probe.last().expect("the log's first event"),
                        through_seq,
                    ));
                }
                if let Some(reported) = reported_for(&probe) {
                    let context = crate::build_context(&self.prefix(), &probe)?;
                    let estimate = self.provider.count_tokens(&context);
                    ratio = next_ratio(ratio, reported, estimate, self.overhead);
                }
                let scan = Scan {
                    calls: calls[..n].to_vec(),
                    through,
                    saturated: false,
                };
                let decision = self.decide(self.compaction, &probe, &scan, ratio)?;
                if let Decision::Sweep { through_seq, .. } = decision {
                    through = Some(through_seq);
                }
                out.push(decision);
            }
            start = end;
        }
        self.ratio = ratio;
        Ok(out)
    }
}

/// What the call this probe ends at reported as its prompt size (issue
/// #52): the nearest `assistant_message` before it, whose usage is the
/// provider's own count of the request that made the call, cached tokens
/// included, exactly as the turn loop sums them.
fn reported_for(probe: &[Event]) -> Option<u64> {
    probe.iter().rev().find_map(|e| match e.kind {
        EventKind::AssistantMessage => {
            let payload: AssistantMessagePayload =
                serde_json::from_value(e.payload.clone()).ok()?;
            payload
                .usage
                .map(|u| u.input_tokens + u.cache_read_tokens + u.cache_write_tokens)
        }
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T1 (issue #52): the calibration's own arithmetic, with the
    /// expected values taken from the named rule, `ratio + S·(sample −
    /// ratio)` — never a hand-computed one.
    #[test]
    fn next_ratio_follows_the_named_rule_and_its_clamp() {
        // One sample: the seed moved a fraction of the way towards it.
        for k in [0.75_f64, 1.0, 2.0] {
            let est = 1_000;
            let overhead = 250;
            let reported = ((est + overhead) as f64 * k).round() as u64;
            let expected =
                1.0 + RATIO_SMOOTHING * (reported as f64 / (est + overhead) as f64 - 1.0);
            let got = next_ratio(1.0, reported, est, overhead);
            assert!(
                (got - expected).abs() < 1e-9,
                "k={k}: got {got}, rule says {expected}"
            );
        }

        // Repeated samples converge to the sample, and the clamp holds
        // them at the edges rather than just past them.
        let absurd_high = (RATIO_MAX * 100.0) as u64 + 1;
        let absurd_low = 0;
        let mut high = 1.0;
        let mut low = 1.0;
        for _ in 0..200 {
            high = next_ratio(high, absurd_high, 1, 0);
            low = next_ratio(low, absurd_low, 1, 0);
        }
        assert_eq!(high, RATIO_MAX, "100x samples converge to the ceiling");
        assert_eq!(low, RATIO_MIN, "0x samples converge to the floor");

        // An empty number tells the calibration nothing: no request had
        // any content to price, so the ratio stays where it was.
        assert_eq!(next_ratio(1.3, 5_000, 0, 0), 1.3);
    }
}
