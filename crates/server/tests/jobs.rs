//! What one build's `/jobs` row says (issue #79): its state, its cost, and
//! whether the count a usage line carries agrees with the rows.

use aigentic_api::JobState;
use aigentic_runtime::aigentic_core::{Author, EventKind};
use aigentic_runtime::aigentic_log::{NewEvent, Repair, ThreadLog};
use aigentic_server::runs::{Working, job_facts};
use serde_json::json;
use tempfile::TempDir;
use time::OffsetDateTime;
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

/// T5 (issue #79): a lead this daemon does not drive states what its log
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

/// T6 (issue #79): a driven build's cost is what its finished steps added
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

/// T8 (issue #79): the count answers from the same rule the rows do. Two
/// builds at work count two, a build at a gate counts one while it waits,
/// and one that stopped — or whose state is unknown — counts nothing.
#[test]
fn the_count_is_the_rows_it_counts() {
    let working = Working::default();
    let at_work = Ulid::generate();
    let at_gate = Ulid::generate();
    let stopped = Ulid::generate();
    let unknown = Ulid::generate();
    let now = OffsetDateTime::now_utc();
    for lead in [at_work, at_gate, stopped, unknown] {
        working.register(lead, now);
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
