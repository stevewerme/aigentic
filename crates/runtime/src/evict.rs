//! In-turn eviction (issue #30): the sweep that decides what the
//! projection stubs. Compaction never touches the turn in progress —
//! summarising it would break the tool-call contract — so a long build
//! turn grows without bound. The sweep runs beside compaction at the top
//! of every loop iteration, at the same boundary, and records how far the
//! stubbing reaches.
//!
//! It appends at most one `context_evicted` event per call, and only
//! when the boundary would move a whole block of calls deeper: a sweep
//! on every result would rewrite the middle of the context each time
//! and burn the provider's prompt cache, while blocks leave a stable
//! prefix between sweeps. The originals stay in the log; the model gets
//! a stub that says how to bring a result back.

use aigentic_core::{Author, ContentBlock, Event, EventKind};
use aigentic_log::{AssistantMessagePayload, ContextEvictedPayload, ToolResultPayload};

use crate::{Runtime, RuntimeError, Signal};

/// The boundary moves this many calls at a time, never call by call.
pub const EVICT_BLOCK_CALLS: usize = 8;

impl Runtime {
    /// Stub, in projection, the open turn's results and successful
    /// edit/write arguments older than the last `keep_last_calls`
    /// calls, one block at a time. Whether anything was appended.
    pub(crate) fn evict_stale(
        &mut self,
        observe: &mut (dyn FnMut(Signal<'_>) + Send),
    ) -> Result<bool, RuntimeError> {
        let events = self.log.read_all()?;
        // The open turn: everything after the last `turn_ended`. A turn
        // that ended is compactable by the rules; this sweep only ever
        // records the turn it runs in.
        let start = events
            .iter()
            .rposition(|e| e.kind == EventKind::TurnEnded)
            .map_or(0, |i| i + 1);
        // The turn's calls that have their result, in log order; the
        // boundary is measured in these.
        let mut calls: Vec<u64> = Vec::new();
        let mut pending: Vec<String> = Vec::new();
        let mut through: Option<u64> = None;
        for e in &events[start..] {
            match e.kind {
                EventKind::AssistantMessage => {
                    let Ok(p) =
                        serde_json::from_value::<AssistantMessagePayload>(e.payload.clone())
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
                    let Ok(p) = serde_json::from_value::<ToolResultPayload>(e.payload.clone())
                    else {
                        continue;
                    };
                    if pending.contains(&p.result.id) {
                        pending.retain(|id| *id != p.result.id);
                        calls.push(e.seq);
                    }
                }
                EventKind::ContextEvicted => {
                    if let Ok(p) =
                        serde_json::from_value::<ContextEvictedPayload>(e.payload.clone())
                    {
                        through = Some(through.unwrap_or(0).max(p.through_seq));
                    }
                }
                _ => {}
            }
        }
        let evicted = match through {
            Some(seq) => calls.iter().filter(|s| **s <= seq).count(),
            None => 0,
        };
        // Pressure first (issue #32): under the line nothing is stubbed.
        // Sweeping a small context saved a few thousand tokens, broke
        // the prompt cache each time, and made a reading turn forget
        // and re-read the same files (150 calls, 21 sweeps, no edit).
        // A lower ceiling lowers the line with it.
        let line = match (
            self.compaction.evict_above_tokens,
            self.compaction.context_ceiling_tokens,
        ) {
            (0, _) => 0,
            (line, 0) => line,
            (line, ceiling) => line.min(ceiling),
        };
        if line > 0 && !self.over_the_ceiling(&events, line, None, &mut None)? {
            return Ok(false);
        }
        // How far to evict: all but the last `keep_last_calls`, cut to
        // the block line so the next sweep lands on the next block.
        let mut target = (calls.len().saturating_sub(self.compaction.keep_last_calls)
            / EVICT_BLOCK_CALLS)
            * EVICT_BLOCK_CALLS;
        // The ceiling (issue #30): what we are willing to pay for per
        // call, whatever the window. When even the projection with
        // `target` stubbed does not fit, evict deeper, a block at a
        // time and never past the last block — then stop: the boundary
        // holds until the context passes the ceiling again, so the
        // cached prefix survives between sweeps.
        let ceiling = self.compaction.context_ceiling_tokens;
        if ceiling > 0 {
            let floor = calls.len().saturating_sub(EVICT_BLOCK_CALLS);
            target = target.max(evicted);
            // Probe, a block at a time and never past the last block.
            // The whole walk shares one scratch projection: the
            // synthetic event is the last, so rewriting its payload
            // re-probes without re-cloning the log.
            let mut scratch: Option<Vec<Event>> = None;
            while target < floor
                && self.over_the_ceiling(
                    &events,
                    ceiling,
                    target.checked_sub(1).map(|i| calls[i]),
                    &mut scratch,
                )?
            {
                target = (target + EVICT_BLOCK_CALLS).min(floor);
            }
        }
        if target <= evicted {
            return Ok(false);
        }
        let through_seq = calls[target - 1];
        let payload =
            serde_json::to_value(ContextEvictedPayload { through_seq }).expect("serialisable");
        // The projection changes, so the window measure is stale.
        self.measured = None;
        self.append(
            EventKind::ContextEvicted,
            Author::System,
            payload,
            None,
            observe,
        )?;
        Ok(true)
    }

    /// Whether the context, projected as the sweep would leave it with
    /// the boundary at `through` (as it stands now, when `None`), still
    /// does not fit under `ceiling`. A probe: the synthetic event is
    /// projected, never stored, and the projection takes a boundary's
    /// deepest reach, so probing at or below the current one reads the
    /// current context. `scratch` carries the one cloned projection the
    /// walk reuses across rungs; a rung at or below the current boundary
    /// reads the log as it stands, so it clones nothing.
    fn over_the_ceiling(
        &self,
        events: &[Event],
        ceiling: u64,
        through: Option<u64>,
        scratch: &mut Option<Vec<Event>>,
    ) -> Result<bool, RuntimeError> {
        let context = match through {
            None => crate::build_context(&self.prefix(), events)?,
            Some(through_seq) => {
                let projected = scratch.get_or_insert_with(|| {
                    let mut projected = events.to_vec();
                    let last = projected.last().expect("a turn with calls has events");
                    projected.push(Event {
                        id: ulid::Ulid::generate(),
                        thread_id: last.thread_id,
                        seq: last.seq + 1,
                        kind: EventKind::ContextEvicted,
                        author: Author::System,
                        payload: serde_json::to_value(ContextEvictedPayload { through_seq })
                            .expect("serialisable"),
                        parent_event: None,
                        created_at: last.created_at,
                    });
                    projected
                });
                let last = projected.last_mut().expect("the synthetic event");
                last.payload = serde_json::to_value(ContextEvictedPayload { through_seq })
                    .expect("serialisable");
                crate::build_context(&self.prefix(), projected)?
            }
        };
        Ok(self.provider.count_tokens(&context) > ceiling)
    }
}
