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

use aigentic_core::{Author, ContentBlock, EventKind};
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
        // How far to evict: all but the last `keep_last_calls`, cut to
        // the block line so the next sweep lands on the next block.
        let target = (calls.len().saturating_sub(self.compaction.keep_last_calls)
            / EVICT_BLOCK_CALLS)
            * EVICT_BLOCK_CALLS;
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
}
