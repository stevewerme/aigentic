//! A run's status line, folded from its lead's and its step child's logs.
//!
//! While a person follows a run the status line should say where it is and
//! what its live step has done. This is a second, small reader of the facts
//! `aigentic status` already folds: the phase comes from the lead's
//! [`NextMove`], and a live step's figures come from the child's open turn.

use aigentic_core::{ContentBlock, Event, EventKind};
use aigentic_log::{AssistantMessagePayload, NextMove, StepStartedPayload, run_state};
use time::OffsetDateTime;

use crate::harness_tools::{Checklist, open_checklist};

/// Where a run is, and what its live step has done so far.
#[derive(Debug, Clone, PartialEq)]
pub struct RunStatus {
    /// The issue the run was asked for, from its `run_started`.
    pub issue: u64,
    pub phase: Phase,
    /// Seconds since the newest event the fold read, never negative.
    pub idle_secs: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Phase {
    /// The runner waits on a step's child.
    Step(Step),
    /// The run waits for a person at a checkpoint.
    Gate { gate: String },
    /// The runner is between moves; the sentence names the move.
    Move(String),
}

/// A live step's figures, read from the child's open turn.
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub step: String,
    pub attempt: u32,
    /// Seconds since this attempt's `step_started` on the lead.
    pub elapsed_secs: u64,
    /// `ToolCall` blocks in the child's open turn.
    pub calls: u32,
    /// The stamped `cost_usd` sum over the child's open turn.
    pub cost_usd: f64,
    /// The child's last `update_tasks`, through `open_checklist`.
    pub checklist: Option<Checklist>,
}

/// The run's status, or `None` when the lead's log is not a run in
/// flight. Reads no file, no clock and no price book.
pub fn run_status(lead: &[Event], child: &[Event], now: OffsetDateTime) -> Option<RunStatus> {
    let state = run_state(lead).ok()?;
    let phase = match state.next_move() {
        NextMove::NotARun | NextMove::Done { .. } => return None,
        NextMove::StartFirstStep => Phase::Move("starting the first step".to_owned()),
        NextMove::ReAwait { step, attempt, .. } => {
            let (calls, cost_usd) = open_turn_figures(child);
            Phase::Step(Step {
                elapsed_secs: elapsed_secs(lead, &step, attempt, now),
                step,
                attempt,
                calls,
                cost_usd,
                checklist: open_checklist(child),
            })
        }
        NextMove::AfterStep { step, attempt, .. } => {
            Phase::Move(format!("checking {step} attempt {attempt}"))
        }
        NextMove::ChecksDone { step, attempt, .. } => {
            Phase::Move(format!("deciding after {step} attempt {attempt}"))
        }
        NextMove::Pushed { step, .. } => Phase::Move(format!("continuing after {step}")),
        NextMove::FollowRoute { taken, .. } => Phase::Move(format!("following {taken}")),
        NextMove::AwaitingCheckpoint { gate } => Phase::Gate { gate },
        NextMove::Answered { gate, .. } => Phase::Move(format!("resuming after {gate}")),
    };
    let idle_secs = idle_secs(lead, child, &phase, now);
    Some(RunStatus {
        issue: state.issue,
        phase,
        idle_secs,
    })
}

/// Seconds from `then` to `now`, never negative: a clock skew or a stamp
/// written ahead must not panic.
fn saturating_secs(now: OffsetDateTime, then: OffsetDateTime) -> u64 {
    (now - then).whole_seconds().max(0) as u64
}

/// Seconds since this attempt's `step_started` on the lead: the newest one
/// whose step and attempt match, because an attempt is re-created on resume
/// and its own start is the honest clock.
fn elapsed_secs(lead: &[Event], step: &str, attempt: u32, now: OffsetDateTime) -> u64 {
    lead.iter()
        .rev()
        .filter(|e| e.kind == EventKind::StepStarted)
        .filter_map(|e| {
            serde_json::from_value::<StepStartedPayload>(e.payload.clone())
                .ok()
                .filter(|p| p.step == step && p.attempt == attempt)
                .map(|_| e.created_at)
        })
        .next()
        .map_or(0, |then| saturating_secs(now, then))
}

/// The newest event's stamp in `events`.
fn newest(events: &[Event]) -> Option<OffsetDateTime> {
    events.iter().map(|e| e.created_at).max()
}

/// Seconds since the newest event the fold read: the child's while a step
/// is live and the child holds anything, otherwise the lead's.
fn idle_secs(lead: &[Event], child: &[Event], phase: &Phase, now: OffsetDateTime) -> u64 {
    let source = match phase {
        Phase::Step(_) if !child.is_empty() => child,
        _ => lead,
    };
    newest(source).map_or(0, |then| saturating_secs(now, then))
}

/// The child's open turn — everything after its last `turn_ended`, or all
/// of it when it has none — as the `ToolCall` block count and the stamped
/// `cost_usd` sum. A line without a stamp adds nothing; no price book and
/// no estimate enters.
fn open_turn_figures(child: &[Event]) -> (u32, f64) {
    let start = child
        .iter()
        .rposition(|e| e.kind == EventKind::TurnEnded)
        .map_or(0, |i| i + 1);
    let mut calls = 0u32;
    let mut cost_usd = 0.0f64;
    for event in &child[start..] {
        if event.kind != EventKind::AssistantMessage {
            continue;
        }
        let Ok(payload) = serde_json::from_value::<AssistantMessagePayload>(event.payload.clone())
        else {
            continue;
        };
        calls += payload
            .blocks
            .iter()
            .filter(|b| matches!(b, ContentBlock::ToolCall(_)))
            .count() as u32;
        if let Some(cost) = payload.usage.and_then(|u| u.cost_usd) {
            cost_usd += cost;
        }
    }
    (calls, cost_usd)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aigentic_core::{AgentId, Author};
    use serde_json::json;
    use time::Duration;
    use time::macros::datetime;
    use ulid::Ulid;

    const ISSUE: u64 = 139;
    const STEP: &str = "spec";
    const CHILD: Ulid = Ulid::from_parts(1_700_000_000_000, 5);

    /// The one fixture clock: every stamp is `T0` plus these offsets.
    fn t0() -> OffsetDateTime {
        datetime!(2026-09-24 12:00:00 UTC)
    }

    fn at(secs: i64) -> OffsetDateTime {
        t0() + Duration::seconds(secs)
    }

    fn runner() -> Author {
        Author::Agent(AgentId("runner".into()))
    }

    fn lead_event(seq: u64, kind: EventKind, secs: i64, payload: serde_json::Value) -> Event {
        Event {
            id: Ulid::from_parts(1_700_000_000_000 + seq, u128::from(seq)),
            thread_id: Ulid::from_parts(1_700_000_000_000, 1),
            seq,
            kind,
            author: runner(),
            payload,
            parent_event: None,
            created_at: at(secs),
        }
    }

    fn child_event(seq: u64, kind: EventKind, secs: i64, payload: serde_json::Value) -> Event {
        let mut event = lead_event(seq, kind, secs, payload);
        event.thread_id = CHILD;
        event
    }

    fn step_started(seq: u64, secs: i64, attempt: u32, child: Ulid) -> Event {
        lead_event(
            seq,
            EventKind::StepStarted,
            secs,
            json!({
                "step": STEP,
                "role": "implementer",
                "profile": "flash",
                "child_thread": child,
                "attempt": attempt,
                "budget_usd": 3.0,
            }),
        )
    }

    fn run_started(seq: u64, secs: i64) -> Event {
        lead_event(
            seq,
            EventKind::RunStarted,
            secs,
            json!({
                "issue": ISSUE,
                "workflow": "build",
                "version": 1,
                "content_hash": "abc",
                "budget_usd": 10.0,
            }),
        )
    }

    fn step_finished(seq: u64, secs: i64) -> Event {
        lead_event(
            seq,
            EventKind::StepFinished,
            secs,
            json!({"step": STEP, "status": "done", "end_reason": "done", "cost_usd": 1.0}),
        )
    }

    fn checks_run(seq: u64, secs: i64) -> Event {
        lead_event(
            seq,
            EventKind::ChecksRun,
            secs,
            json!({"step": STEP, "checks": [{"id": "gate", "result": "pass"}]}),
        )
    }

    fn turn_open(seq: u64, secs: i64) -> Event {
        child_event(
            seq,
            EventKind::UserMessage,
            secs,
            json!({"blocks": [{"type": "text", "text": "go"}]}),
        )
    }

    fn assistant(seq: u64, secs: i64, payload: serde_json::Value) -> Event {
        child_event(seq, EventKind::AssistantMessage, secs, payload)
    }

    /// An assistant reply whose blocks and usage are the given json: the
    /// fixture's own stamp, so expectations derive from the fixture.
    fn reply(seq: u64, secs: i64, blocks: serde_json::Value, usage: serde_json::Value) -> Event {
        assistant(seq, secs, json!({"blocks": blocks, "usage": usage}))
    }

    /// A lead that is mid-step: `run_started` and one open `step_started`.
    fn lead_on_a_step() -> Vec<Event> {
        vec![run_started(0, 0), step_started(1, 0, 1, CHILD)]
    }

    #[test]
    fn a_live_step_reads_its_elapsed_calls_cost_and_idle_from_the_two_logs() {
        const ASSISTANT_AT: i64 = 30;
        const NOW: i64 = 192;
        let lead = lead_on_a_step();
        let child = vec![
            turn_open(0, 2),
            reply(
                1,
                ASSISTANT_AT,
                json!([
                    {"type": "tool_call", "id": "c1", "name": "bash", "args": {}},
                    {"type": "tool_call", "id": "c2", "name": "read_file", "args": {}},
                ]),
                json!({"input_tokens": 10, "output_tokens": 5, "cost_usd": 0.51}),
            ),
        ];

        let status = run_status(&lead, &child, at(NOW)).unwrap();

        assert_eq!(status.issue, ISSUE);
        assert_eq!(
            status.phase,
            Phase::Step(Step {
                step: STEP.into(),
                attempt: 1,
                elapsed_secs: NOW as u64,
                calls: 2,
                cost_usd: 0.51,
                checklist: None,
            })
        );
        assert_eq!(status.idle_secs, (NOW - ASSISTANT_AT) as u64);
    }

    #[test]
    fn the_checklists_position_and_item_come_from_the_childs_last_update_tasks() {
        let tasks = json!([
            {"text": "read the code", "state": "done"},
            {"text": "write the fold", "state": "active"},
            {"text": "post the report", "state": "pending"},
        ]);
        let lead = lead_on_a_step();
        let child = vec![
            turn_open(0, 0),
            reply(
                1,
                1,
                json!([{"type": "tool_call", "id": "t1", "name": "update_tasks", "args": {"tasks": tasks}}]),
                json!({"input_tokens": 1, "output_tokens": 1}),
            ),
            child_event(
                2,
                EventKind::ToolResult,
                2,
                json!({"id": "t1", "content": "ok", "is_error": false}),
            ),
        ];

        let Some(Phase::Step(step)) = run_status(&lead, &child, at(10)).map(|s| s.phase) else {
            panic!("expected a live step");
        };
        let checklist = step
            .checklist
            .expect("the last update_tasks is the checklist");
        assert_eq!(checklist.done, 1);
        assert_eq!(checklist.total, 3);
        assert_eq!(checklist.active.as_deref(), Some("write the fold"));
    }

    #[test]
    fn a_step_whose_child_has_written_nothing_still_reads_as_a_live_step() {
        const NOW: i64 = 50;
        let lead = lead_on_a_step();
        let child: Vec<Event> = Vec::new();

        let status = run_status(&lead, &child, at(NOW)).unwrap();

        assert_eq!(
            status.phase,
            Phase::Step(Step {
                step: STEP.into(),
                attempt: 1,
                elapsed_secs: NOW as u64,
                calls: 0,
                cost_usd: 0.0,
                checklist: None,
            })
        );
        assert_eq!(status.idle_secs, NOW as u64);
    }

    #[test]
    fn the_cost_is_the_stamped_sum_of_the_open_turn_only() {
        let lead = lead_on_a_step();
        let child = vec![
            // A closed turn: its stamp is not the open turn's.
            turn_open(0, 0),
            reply(
                1,
                1,
                json!([]),
                json!({"input_tokens": 1, "output_tokens": 1, "cost_usd": 5.0}),
            ),
            child_event(2, EventKind::TurnEnded, 2, json!({"reason": "done"})),
            // The open turn: one stamped line, one line with no stamp.
            turn_open(3, 3),
            reply(
                4,
                4,
                json!([{"type": "tool_call", "id": "c1", "name": "bash", "args": {}}]),
                json!({"input_tokens": 1, "output_tokens": 1, "cost_usd": 0.51}),
            ),
            reply(
                5,
                5,
                json!([{"type": "tool_call", "id": "c2", "name": "bash", "args": {}}]),
                json!({"input_tokens": 1, "output_tokens": 1}),
            ),
        ];

        let Some(Phase::Step(step)) = run_status(&lead, &child, at(10)).map(|s| s.phase) else {
            panic!("expected a live step");
        };
        assert_eq!(step.cost_usd, 0.51);
        assert_eq!(step.calls, 2);
    }

    #[test]
    fn elapsed_comes_from_the_open_attempts_step_started() {
        const SECOND_START: i64 = 200;
        const NOW: i64 = 260;
        let c2 = Ulid::from_parts(1_700_000_000_000, 6);
        let lead = vec![
            run_started(0, 0),
            step_started(1, 10, 1, CHILD),
            step_finished(2, 100),
            step_started(3, SECOND_START, 2, c2),
        ];

        let Some(Phase::Step(step)) = run_status(&lead, &[], at(NOW)).map(|s| s.phase) else {
            panic!("expected a live step");
        };
        assert_eq!(step.attempt, 2);
        assert_eq!(step.elapsed_secs, (NOW - SECOND_START) as u64);
    }

    #[test]
    fn a_run_at_a_gate_names_the_gate() {
        const ASKED_AT: i64 = 40;
        let lead = vec![
            run_started(0, 0),
            step_started(1, 0, 1, CHILD),
            step_finished(2, 10),
            checks_run(3, 20),
            lead_event(
                4,
                EventKind::CheckpointAsked,
                ASKED_AT,
                json!({"gate": "decide", "shown": [], "options": ["go"]}),
            ),
        ];

        let status = run_status(&lead, &[], at(100)).unwrap();

        assert_eq!(
            status.phase,
            Phase::Gate {
                gate: "decide".into()
            }
        );
        assert_eq!(status.idle_secs, (100 - ASKED_AT) as u64);
    }

    #[test]
    fn every_move_between_steps_has_its_own_sentence() {
        let base = || {
            vec![
                run_started(0, 0),
                step_started(1, 0, 1, CHILD),
                step_finished(2, 5),
            ]
        };
        let move_of = |lead: Vec<Event>| match run_status(&lead, &[], at(10)).map(|s| s.phase) {
            Some(Phase::Move(text)) => text,
            other => panic!("expected a move, got {other:?}"),
        };

        assert_eq!(move_of(vec![run_started(0, 0)]), "starting the first step");
        assert_eq!(move_of(base()), format!("checking {STEP} attempt 1"));

        let mut checked = base();
        checked.push(checks_run(3, 6));
        assert_eq!(move_of(checked), format!("deciding after {STEP} attempt 1"));

        let mut pushed = base();
        pushed.push(lead_event(
            3,
            EventKind::Pushed,
            6,
            json!({
                "commits": [{"sha": "abc", "subject": "spec: go"}],
                "ref_before": "0",
                "ref_after": "abc",
            }),
        ));
        assert_eq!(move_of(pushed), format!("continuing after {STEP}"));

        let mut routed = base();
        routed.push(lead_event(
            3,
            EventKind::RouteTaken,
            6,
            json!({"branch": "b", "proposed": "p", "taken": "fix"}),
        ));
        assert_eq!(move_of(routed), "following fix");

        let mut answered = base();
        answered.push(lead_event(
            3,
            EventKind::CheckpointAsked,
            6,
            json!({"gate": "decide", "shown": [], "options": []}),
        ));
        answered.push(lead_event(
            4,
            EventKind::CheckpointAnswered,
            7,
            json!({"answer": "go"}),
        ));
        assert_eq!(move_of(answered), "resuming after decide");
    }

    #[test]
    fn a_thread_that_is_not_a_run_has_no_status() {
        let plain = vec![child_event(
            0,
            EventKind::UserMessage,
            0,
            json!({"blocks": [{"type": "text", "text": "hi"}]}),
        )];
        assert_eq!(run_status(&plain, &[], at(10)), None);

        let finished = vec![
            run_started(0, 0),
            lead_event(
                1,
                EventKind::RunFinished,
                5,
                json!({"outcome": "closed", "cost_usd": 1.0}),
            ),
        ];
        assert_eq!(run_status(&finished, &[], at(10)), None);
    }
}
