//! What one build's `/jobs` row says: its state, its cost, and
//! whether the count a usage line carries agrees with the rows.

use aigentic_api::JobState;
use aigentic_runtime::aigentic_core::{Author, EventKind};
use aigentic_runtime::aigentic_log::{NewEvent, Repair, ThreadLog};
use aigentic_server::runs::{Working, job_facts};
use serde_json::json;
use tempfile::TempDir;
use ulid::Ulid;

/// A lead whose log carries `events`, written through the store so the
/// shapes are the ones a real run writes.
fn lead_log(events: &[(EventKind, serde_json::Value)]) -> (TempDir, Ulid) {
    let dir = tempfile::tempdir().unwrap();
    let lead = Ulid::generate();
    let (mut log, _cut) = ThreadLog::open_with(dir.path(), lead, Repair::TruncateTornTail).unwrap();
    for (kind, payload) in events {
        log.append(NewEvent {
            kind: *kind,
            author: Author::System,
            payload: payload.clone(),
            parent_event: None,
        })
        .unwrap();
    }
    (dir, lead)
}

fn thread_started() -> (EventKind, serde_json::Value) {
    (
        EventKind::ThreadStarted,
        json!({
            "project": "p",
            "root": "/tmp/p",
            "created_by": {"kind": "system"},
            "parent_thread": null,
            "step": null,
            "front": false,
        }),
    )
}

fn step_finished(step: &str, cost_usd: f64) -> (EventKind, serde_json::Value) {
    (
        EventKind::StepFinished,
        json!({
            "step": step,
            "status": "done",
            "end_reason": "done",
            "cost_usd": cost_usd,
        }),
    )
}

fn run_finished(cost_usd: f64) -> (EventKind, serde_json::Value) {
    (
        EventKind::RunFinished,
        json!({
            "outcome": "closed",
            "cost_usd": cost_usd,
            "release_impact": "patch",
        }),
    )
}

fn run_errored(reason: &str, cost_usd: f64) -> (EventKind, serde_json::Value) {
    (
        EventKind::RunErrored,
        json!({"reason": reason, "cost_usd": cost_usd}),
    )
}

/// A lead this daemon does not drive states what its log
/// says — `Done` for a finished end, `Stopped` with the reason's first line
/// for an errored one, and nothing for a run that never ended.
#[test]
fn a_rows_state_follows_the_run_and_its_logs_end() {
    let (finished_dir, finished) = lead_log(&[
        thread_started(),
        step_finished("spec", 0.5),
        run_finished(1.5),
    ]);
    let (errored_dir, errored) = lead_log(&[
        thread_started(),
        run_errored("the provider refused\nthe second line is dropped", 0.25),
    ]);
    let (open_dir, open) = lead_log(&[thread_started(), step_finished("spec", 0.25)]);

    assert_eq!(
        job_facts(finished_dir.path(), finished, false).0,
        Some(JobState::Done)
    );
    assert_eq!(
        job_facts(errored_dir.path(), errored, false).0,
        Some(JobState::Stopped {
            reason: "the provider refused".into()
        }),
        "the reason is the first line, not the whole text"
    );
    assert_eq!(job_facts(open_dir.path(), open, false).0, None);
}

/// A driven build's cost is what its finished steps added
/// up to, and a build nobody drove costs what its own end says. Nothing
/// priced is `None`, not zero.
#[test]
fn a_rows_cost_is_the_steps_or_the_end_it_wrote() {
    let (driven_dir, driven) = lead_log(&[
        thread_started(),
        step_finished("spec", 0.5),
        step_finished("implementer", 0.5),
        run_finished(1.5),
    ]);
    let (ended_dir, ended) = lead_log(&[thread_started(), run_finished(2.25)]);
    let (errored_dir, errored) = lead_log(&[thread_started(), run_errored("stopped", 0.75)]);
    let (unpriced_dir, unpriced) = lead_log(&[thread_started()]);

    assert_eq!(
        job_facts(driven_dir.path(), driven, true).1,
        Some(1.0),
        "the two steps' own costs, not the run's own end"
    );
    assert_eq!(job_facts(ended_dir.path(), ended, false).1, Some(2.25));
    assert_eq!(
        job_facts(errored_dir.path(), errored, false).1,
        Some(0.75),
        "an errored end still names what the run cost"
    );
    assert_eq!(
        job_facts(unpriced_dir.path(), unpriced, false).1,
        None,
        "a lead with no priced step and no end shows no cost at all"
    );
}

/// The count answers from the same rule the rows do. Two
/// builds at work count two, a build at a gate counts one while it waits,
/// and one that stopped — or whose state is unknown — counts nothing.
#[test]
fn the_count_is_the_rows_it_counts() {
    let working = Working::default();
    let at_work = Ulid::generate();
    let at_gate = Ulid::generate();
    let stopped = Ulid::generate();
    let unknown = Ulid::generate();
    for lead in [at_work, at_gate, stopped, unknown] {
        working.register(lead);
    }
    assert_eq!(working.count(), 4, "all four are driven to begin with");

    working.note_state(at_gate, Some(JobState::WaitingOnYou));
    assert_eq!(
        working.count(),
        4,
        "a build waiting on a person is still work"
    );

    working.note_state(
        stopped,
        Some(JobState::Stopped {
            reason: "the gate was refused".into(),
        }),
    );
    working.note_state(unknown, None);
    assert_eq!(
        working.count(),
        2,
        "the stopped row and the stateless row are not counted"
    );

    working.release(at_work);
    assert_eq!(working.count(), 1, "a released build stops counting");
}

/// A build resumed today from a lead minted yesterday is listed under
/// yesterday, so it does not count today either: the count and the rows
/// date a build by the same id.
#[test]
fn a_lead_from_another_day_counts_as_its_row_lists_it() {
    let working = Working::default();
    let yesterday = Ulid::from_datetime(
        std::time::SystemTime::now() - std::time::Duration::from_secs(24 * 60 * 60),
    );
    let today = Ulid::generate();
    working.register(yesterday);
    working.register(today);
    let listed = [yesterday, today]
        .into_iter()
        .filter(|lead| aigentic_server::runs::minted_today(*lead))
        .count() as u32;
    assert_eq!(working.count(), listed);
    assert_eq!(listed, 1, "only today's lead is today's row");
}

fn run_started() -> (EventKind, serde_json::Value) {
    (
        EventKind::RunStarted,
        json!({
            "issue": 7,
            "workflow": "loop",
            "version": 1,
            "content_hash": "abc",
            "budget_usd": 10.0,
        }),
    )
}

fn step_started(step: &str) -> (EventKind, serde_json::Value) {
    (
        EventKind::StepStarted,
        json!({
            "step": step,
            "role": step,
            "profile": "flash",
            "child_thread": Ulid::generate(),
            "attempt": 1,
            "budget_usd": 0.0,
        }),
    )
}

/// A build this daemon drives states what the run's own fold says: at work
/// in a step, or waiting on a person at a gate. The same log read as a
/// lead nobody drives states nothing, since it has no end yet.
#[test]
fn a_driven_builds_state_is_its_runs_fold() {
    let (step_dir, in_step) = lead_log(&[thread_started(), run_started(), step_started("spec")]);
    let (gate_dir, at_gate) = lead_log(&[
        thread_started(),
        run_started(),
        step_started("spec"),
        step_finished("spec", 0.5),
        (
            EventKind::RouteTaken,
            json!({"branch": "next", "preconditions": [], "proposed": "decide", "taken": "decide"}),
        ),
        (
            EventKind::CheckpointAsked,
            json!({"gate": "decide", "options": ["go", "amend", "stop"], "shown": ["decide"]}),
        ),
    ]);

    assert_eq!(
        job_facts(step_dir.path(), in_step, true).0,
        Some(JobState::Running)
    );
    assert_eq!(
        job_facts(gate_dir.path(), at_gate, true).0,
        Some(JobState::WaitingOnYou)
    );
    assert_eq!(job_facts(step_dir.path(), in_step, false).0, None);
    assert_eq!(job_facts(gate_dir.path(), at_gate, false).0, None);
}
