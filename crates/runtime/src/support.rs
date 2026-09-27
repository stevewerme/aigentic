//! Helpers the loop calls; none of them is the loop.

use std::time::{Duration, Instant, SystemTime};

use aigentic_core::{Author, ContentBlock, Event, EventKind, Message, Role};
use aigentic_log::{NewEvent, TurnEndedPayload, Usage, UserMessagePayload};

use crate::decisions::Queued;
use crate::{Runtime, RuntimeError, Signal, TurnOutcome};

/// A gap between wall time and running time at or above this is reported
/// as sleep (issue #47): clock jitter and NTP steps stay silent.
pub(crate) const SLEPT_THRESHOLD_SECS: u64 = 30;

/// Split a turn's clocks into the seconds it slept and the part of that
/// spent parked on a human (issue #47). `slept` is `Some` only above the
/// threshold; `slept_awaiting` is `Some` only when `slept` is (it is the
/// wait breakdown of the same sleep, and the value the turn line's
/// suffix condition subtracts). Saturating, so a clock stepped
/// backwards never reports a negative nap.
pub(crate) fn slept_split(
    wall: Duration,
    running: Duration,
    wall_waited: Duration,
    waited: Duration,
) -> (Option<u64>, Option<u64>) {
    let slept = wall.saturating_sub(running);
    if slept.as_secs() < SLEPT_THRESHOLD_SECS {
        return (None, None);
    }
    let awaiting = wall_waited.saturating_sub(waited);
    (Some(slept.as_secs()), Some(awaiting.as_secs()))
}

/// What a turn has used so far, checked against the `Budget`.
pub(crate) struct Spent {
    pub(crate) started: Instant,
    /// The same start on the wall clock (issue #47): the two together
    /// tell a sleep from work, because running time stops in sleep.
    pub(crate) wall_started: SystemTime,
    /// Time parked on a human (a permission prompt, an `ask_human`),
    /// which the wall-time budget does not count: a turn that waited
    /// twenty minutes for approvals has not worked for twenty minutes.
    pub(crate) waited: Duration,
    /// The same wait on the wall clock (issue #47): the part of a sleep
    /// that happened inside a human wait could not have been prevented
    /// by the keep-awake guard.
    pub(crate) wall_waited: Duration,
    pub(crate) iterations: u32,
    pub(crate) tokens: u64,
}

impl Runtime {
    /// Usage for a call whose provider reported none: `count_tokens` over
    /// the context for input and over the produced blocks for output,
    /// flagged `estimated`.
    pub(crate) fn estimate_usage(
        &self,
        context: &[Message],
        agent: &Author,
        blocks: &[ContentBlock],
    ) -> Usage {
        let produced = Message {
            role: Role::Assistant,
            author: agent.clone(),
            blocks: blocks.to_vec(),
        };
        Usage::estimated(aigentic_core::Usage {
            input_tokens: self.provider.count_tokens(context),
            output_tokens: self.provider.count_tokens(std::slice::from_ref(&produced)),
            ..Default::default()
        })
    }

    pub(crate) fn budget_reason(&self, spent: &Spent) -> Option<&'static str> {
        if spent.iterations >= self.budget.max_iterations {
            Some("max_iterations")
        } else if spent.tokens >= self.budget.max_tokens {
            Some("max_tokens")
        } else if (self.clock)()
            .0
            .saturating_duration_since(spent.started)
            .saturating_sub(spent.waited)
            >= self.budget.max_wall_time
        {
            Some("max_wall_time")
        } else {
            None
        }
    }

    /// `turn_ended`, with the reason a turn stopped. `held` is what a
    /// stream held (issue #33): the turn is over, so no call of this
    /// turn can read those messages where they sit. They go in first,
    /// `steer` unset, before `turn_ended` — which is what lets the
    /// projection emit them, so the turn the actor starts next answers
    /// them — and nothing posted mid-stream is lost.
    pub(crate) fn end_turn(
        &mut self,
        reason: &str,
        spent: &Spent,
        held: &mut Vec<Queued>,
        observe: &mut (dyn FnMut(Signal<'_>) + Send),
    ) -> Result<TurnOutcome, RuntimeError> {
        for queued in std::mem::take(held) {
            append_queued(&mut self.log, queued, observe, false)?;
        }
        let touched = self.touched_this_turn()?;
        let (now, wall_now) = (self.clock)();
        let running = now.saturating_duration_since(spent.started);
        let wall = wall_now
            .duration_since(spent.wall_started)
            .unwrap_or_default();
        let (slept_secs, slept_awaiting_secs) =
            slept_split(wall, running, spent.wall_waited, spent.waited);
        let payload = serde_json::to_value(TurnEndedPayload {
            reason: reason.to_owned(),
            touched: touched.clone(),
            wall_secs: Some(wall.as_secs()),
            slept_secs,
            slept_awaiting_secs,
            keep_awake: self.keep_awake.as_ref().and_then(|f| f()),
        })
        .expect("serialisable");
        self.append(
            EventKind::TurnEnded,
            Author::Agent(self.agent.clone()),
            payload,
            None,
            observe,
        )?;
        Ok(TurnOutcome {
            reason: reason.to_owned(),
            iterations: spent.iterations,
            tokens: spent.tokens,
            elapsed: spent.started.elapsed(),
            touched,
        })
    }

    /// Paths of `write_file` and `edit_file` calls since the last
    /// `turn_ended` whose result is not an error, in order, each once.
    pub(crate) fn touched_this_turn(&self) -> Result<Vec<String>, RuntimeError> {
        let events = self.log.read_all()?;
        let start = events
            .iter()
            .rposition(|e| e.kind == EventKind::TurnEnded)
            .map_or(0, |i| i + 1);
        let mut pending: Vec<(String, String)> = Vec::new();
        let mut touched: Vec<String> = Vec::new();
        for e in &events[start..] {
            match e.kind {
                EventKind::AssistantMessage => {
                    let Ok(p) = serde_json::from_value::<aigentic_log::AssistantMessagePayload>(
                        e.payload.clone(),
                    ) else {
                        continue;
                    };
                    for b in p.blocks {
                        if let ContentBlock::ToolCall(c) = b
                            && (c.name == "write_file" || c.name == "edit_file")
                            && let Some(path) = c.args.get("path").and_then(|v| v.as_str())
                        {
                            pending.push((c.id, path.to_owned()));
                        }
                    }
                }
                EventKind::ToolResult => {
                    let Ok(p) = serde_json::from_value::<aigentic_log::ToolResultPayload>(
                        e.payload.clone(),
                    ) else {
                        continue;
                    };
                    if p.result.is_error {
                        continue;
                    }
                    if let Some(i) = pending.iter().position(|(id, _)| *id == p.result.id) {
                        let (_, path) = pending.remove(i);
                        if !touched.contains(&path) {
                            touched.push(path);
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(touched)
    }

    pub(crate) fn append(
        &mut self,
        kind: EventKind,
        author: Author,
        payload: serde_json::Value,
        parent_event: Option<ulid::Ulid>,
        observe: &mut (dyn FnMut(Signal<'_>) + Send),
    ) -> Result<Event, RuntimeError> {
        let event = self.log.append(NewEvent {
            kind,
            author,
            payload,
            parent_event,
        })?;
        observe(Signal::Event(&event));
        Ok(event)
    }
}

impl Runtime {
    /// A retry's reason as shown in the log: the profile label first, so
    /// the turn line reads `retrying 2/3 · tensorx · not answering`
    /// (issue #31). Without a profile it is the reason alone.
    pub(crate) fn retry_reason(&self, reason: &str) -> String {
        match &self.profile {
            Some(profile) => format!("{profile} · {reason}"),
            None => reason.to_owned(),
        }
    }
}

/// Whether an event is the call's first content: a delta, a tool call or
/// a blob. Usage and done arrive after the content they describe, so a
/// stream that only sends those never sets a time to first token.
pub(crate) fn block_start(e: &aigentic_core::ProviderEvent) -> bool {
    use aigentic_core::ProviderEvent as P;
    matches!(e, P::TextDelta(_) | P::ToolCall(_) | P::Blob(_))
}

pub(crate) fn flush_text(text: &mut String, blocks: &mut Vec<ContentBlock>) {
    if !text.is_empty() {
        blocks.push(ContentBlock::Text(std::mem::take(text)));
    }
}

/// One queued message into the log, as `append` would but on the log
/// alone, so it can run while a stream borrows the provider or the
/// registry. `steer` is set where a model call will read the message —
/// the safe points, after a reply's last tool result and before the
/// call that reads it — so the projection emits it where it sits; a
/// call already in flight cannot be steered, so a message that arrives
/// mid-stream waits, `steer` unset, for the turn that answers it.
pub(crate) fn append_queued(
    log: &mut aigentic_log::ThreadLog,
    queued: Queued,
    observe: &mut (dyn FnMut(Signal<'_>) + Send),
    steer: bool,
) -> Result<(), RuntimeError> {
    let payload = UserMessagePayload {
        blocks: queued.blocks,
        mid_turn: true,
        steer,
    };
    let event = log.append(NewEvent {
        kind: EventKind::UserMessage,
        author: queued.author,
        payload: serde_json::to_value(payload).expect("serialisable"),
        parent_event: None,
    })?;
    observe(Signal::Event(&event));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T7 (issue #47): the wall-minus-running split. The plan's numbers:
    /// 980 s of wall over 230 s of running time is 750 s of sleep, and a
    /// gap below the threshold (5 s here, 29 s in the issue's jitter
    /// case) is silence. Every expectation is written as the arithmetic
    /// the rule states, never as a computed literal.
    #[test]
    fn slept_split_reports_only_a_real_sleep() {
        let secs = Duration::from_secs;
        let (wall, running) = (secs(980), secs(230));
        assert_eq!(
            slept_split(wall, running, Duration::ZERO, Duration::ZERO),
            (Some((wall - running).as_secs()), Some(0))
        );
        // A 5 s gap: under the 30 s threshold, so nothing is reported.
        let tiny = secs(5);
        assert_eq!(
            slept_split(running + tiny, running, Duration::ZERO, Duration::ZERO),
            (None, None)
        );
        // Exactly at the threshold is reported (at or above).
        let at = Duration::from_secs(SLEPT_THRESHOLD_SECS);
        assert_eq!(
            slept_split(running + at, running, Duration::ZERO, Duration::ZERO),
            (Some(at.as_secs()), Some(0))
        );
        // Work, not sleep: wall and running agree.
        assert_eq!(
            slept_split(running, running, Duration::ZERO, Duration::ZERO),
            (None, None)
        );
        // A clock that steps backwards is clamped, not negative.
        assert_eq!(
            slept_split(running, wall, Duration::ZERO, Duration::ZERO),
            (None, None)
        );
    }

    /// T8 (issue #47): the wait breakdown. Wall time during a human wait
    /// that running time did not see is the part of the sleep the guard
    /// could not have prevented (wall_waited = 400 s, waited = 100 s →
    /// 300 s here); equal waits report zero, and a wall wait below the
    /// running one saturates rather than going negative.
    #[test]
    fn slept_split_splits_the_wait_out_of_the_sleep() {
        let secs = Duration::from_secs;
        let (wall, running) = (secs(980), secs(230));
        let (wall_waited, waited) = (secs(400), secs(100));
        assert_eq!(
            slept_split(wall, running, wall_waited, waited),
            (
                Some((wall - running).as_secs()),
                Some((wall_waited - waited).as_secs())
            )
        );
        // A wait that both clocks saw equally: no sleep hides in it.
        assert_eq!(
            slept_split(wall, running, waited, waited),
            (Some((wall - running).as_secs()), Some(0))
        );
        // A wall wait below the running one saturates at zero.
        assert_eq!(
            slept_split(wall, running, Duration::ZERO, waited),
            (Some((wall - running).as_secs()), Some(0))
        );
        // And the breakdown is Some only when the sleep is.
        assert_eq!(
            slept_split(running, running, wall_waited, waited),
            (None, None)
        );
    }
}
