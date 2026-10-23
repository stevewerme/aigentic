//! A background job seen from the front thread that approved it: which
//! threads are its jobs, and one live line about a job read from its own
//! log. The line is what `inspect_job` answers, so a model asked how a job
//! is going reads the log instead of guessing.

use std::io::{BufRead, BufReader};
use std::path::Path;

use aigentic_core::{ContentBlock, Event, EventKind};
use aigentic_log::{AssistantMessagePayload, ThreadStartedPayload};
use time::OffsetDateTime;
use ulid::Ulid;

use crate::harness_tools::open_checklist;

/// The jobs `front` approved, oldest first: every thread in `dir` whose
/// masthead marks it a job with `front` as its parent. The order is the
/// order they were made in, which is how the front thread numbers them.
pub fn jobs_of(dir: &Path, front: Ulid) -> Vec<Ulid> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut jobs: Vec<Ulid> = entries
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            let id: Ulid = path.file_stem()?.to_str()?.parse().ok()?;
            let file = std::fs::File::open(&path).ok()?;
            let mut first = String::new();
            BufReader::new(file).read_line(&mut first).ok()?;
            let event: Event = serde_json::from_str(&first).ok()?;
            let started: ThreadStartedPayload = serde_json::from_value(event.payload).ok()?;
            (started.job && started.parent_thread == Some(front)).then_some(id)
        })
        .collect();
    jobs.sort_unstable();
    jobs
}

/// Every event in a thread's log, or `None` when it cannot be read. A
/// line that does not parse ends the read: a torn tail is not the job's
/// state.
pub fn read_events(dir: &Path, id: Ulid) -> Option<Vec<Event>> {
    let file = std::fs::File::open(dir.join(format!("{id}.jsonl"))).ok()?;
    Some(
        BufReader::new(file)
            .lines()
            .map_while(|line| serde_json::from_str::<Event>(&line.ok()?).ok())
            .collect(),
    )
}

/// One line about job `number` (`id`) from its log, as of `now`: its
/// state, its turns, how long it has run, its tool calls, what it has
/// cost, its checklist, and what it last did.
pub fn job_line(number: usize, id: Ulid, events: &[Event], now: OffsetDateTime) -> String {
    let mut parts = vec![format!("job {number} ({id}): {}", state(events))];
    let turns = events
        .iter()
        .filter(|e| e.kind == EventKind::UserMessage)
        .count();
    parts.push(format!("turn {turns}"));
    if let Some(first) = events.first() {
        let secs = (now - first.created_at).whole_seconds().max(0);
        parts.push(format!("{}m{:02}s", secs / 60, secs % 60));
    }
    let replies: Vec<AssistantMessagePayload> = events
        .iter()
        .filter(|e| e.kind == EventKind::AssistantMessage)
        .filter_map(|e| serde_json::from_value(e.payload.clone()).ok())
        .collect();
    let calls = replies
        .iter()
        .flat_map(|r| r.blocks.iter())
        .filter(|b| matches!(b, ContentBlock::ToolCall(_)))
        .count();
    parts.push(format!("{calls} calls"));
    let cost = replies
        .iter()
        .filter_map(|r| r.usage.as_ref().and_then(|u| u.cost_usd))
        .fold(0.0, |sum, usd| sum + usd);
    parts.push(format!("${cost:.2}"));
    if let Some(list) = open_checklist(events) {
        let step = list
            .active
            .map(|text| format!(", at: {text}"))
            .unwrap_or_default();
        parts.push(format!("checklist {}/{}{step}", list.done, list.total));
    }
    if let Some(last) = replies
        .iter()
        .rev()
        .flat_map(|r| r.blocks.iter().rev())
        .find_map(|b| match b {
            ContentBlock::ToolCall(call) => Some(call),
            _ => None,
        })
    {
        parts.push(format!("last call: {}", last.name));
    }
    parts.join(" · ")
}

/// What the job is doing by its log: waiting on a person while a
/// permission request has no decision after it, running while its turn
/// is open, and otherwise how its last turn ended.
fn state(events: &[Event]) -> String {
    let last = |kind| events.iter().rev().find(|e| e.kind == kind).map(|e| e.seq);
    let requested = last(EventKind::PermissionRequested);
    let decided = last(EventKind::PermissionDecided);
    let message = last(EventKind::UserMessage);
    let ended = last(EventKind::TurnEnded);
    if requested.is_some_and(|r| decided.is_none_or(|d| d < r)) {
        return "waiting on you".into();
    }
    match (message, ended) {
        (Some(m), Some(e)) if e > m => {
            let reason = events
                .iter()
                .rev()
                .find(|e| e.kind == EventKind::TurnEnded)
                .and_then(|e| e.payload.get("reason").and_then(|r| r.as_str()))
                .unwrap_or("done");
            if reason == "done" {
                "finished".into()
            } else {
                format!("ended: {reason}")
            }
        }
        (Some(_), _) => "running".into(),
        _ => "not started".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aigentic_core::Author;
    use aigentic_log::{NewEvent, ThreadLog};
    use serde_json::json;

    /// A thread in `dir` whose masthead says it is a job of `parent`, or
    /// an ordinary thread when `parent` is `None`, with `tail` after it.
    fn thread(dir: &Path, parent: Option<Ulid>, tail: &[(EventKind, serde_json::Value)]) -> Ulid {
        let id = Ulid::generate();
        let mut log = ThreadLog::open(dir, id).unwrap();
        let started = json!({
            "project": "p",
            "root": "/p",
            "created_by": {"kind": "system"},
            "parent_thread": parent.map(|p| p.to_string()),
            "job": parent.is_some(),
        });
        for (kind, payload) in
            std::iter::once((EventKind::ThreadStarted, started)).chain(tail.iter().cloned())
        {
            log.append(NewEvent {
                kind,
                author: Author::System,
                payload,
                parent_event: None,
            })
            .unwrap();
        }
        id
    }

    fn brief() -> (EventKind, serde_json::Value) {
        (
            EventKind::UserMessage,
            json!({"blocks": [{"type": "text", "text": "list the callers"}]}),
        )
    }

    /// A front thread's jobs are its own job children, oldest first; an
    /// ordinary thread and another front's job are not among them.
    #[test]
    fn a_fronts_jobs_are_its_own_job_children_oldest_first() {
        let dir = tempfile::tempdir().unwrap();
        let front = Ulid::generate();
        let first = thread(dir.path(), Some(front), &[brief()]);
        thread(dir.path(), None, &[brief()]);
        thread(dir.path(), Some(Ulid::generate()), &[brief()]);
        let second = thread(dir.path(), Some(front), &[brief()]);
        assert_eq!(jobs_of(dir.path(), front), vec![first, second]);
    }

    /// The line says what the job is doing by its log: running with a
    /// turn open, waiting on a person while a request stands, finished
    /// once its turn ended done; and it counts calls, cost and the last
    /// call made.
    #[test]
    fn a_jobs_line_reads_its_state_calls_cost_and_last_call() {
        let dir = tempfile::tempdir().unwrap();
        let front = Ulid::generate();
        let call = (
            EventKind::AssistantMessage,
            json!({
                "blocks": [{"type": "tool_call", "id": "c1", "name": "bash", "args": {"command": "ls"}}],
                "usage": {"input_tokens": 1, "output_tokens": 1, "cost_usd": 0.25},
            }),
        );
        let running = thread(dir.path(), Some(front), &[brief(), call.clone()]);
        let events = read_events(dir.path(), running).unwrap();
        let line = job_line(1, running, &events, OffsetDateTime::now_utc());
        assert!(
            line.starts_with(&format!("job 1 ({running}): running")),
            "{line}"
        );
        assert!(line.contains("1 calls"), "{line}");
        assert!(line.contains("$0.25"), "{line}");
        assert!(line.contains("last call: bash"), "{line}");

        let asked = (EventKind::PermissionRequested, json!({}));
        let waiting = thread(dir.path(), Some(front), &[brief(), call.clone(), asked]);
        let line = job_line(
            2,
            waiting,
            &read_events(dir.path(), waiting).unwrap(),
            OffsetDateTime::now_utc(),
        );
        assert!(line.contains("): waiting on you"), "{line}");

        let done = (EventKind::TurnEnded, json!({"reason": "done"}));
        let finished = thread(dir.path(), Some(front), &[brief(), call, done]);
        let line = job_line(
            3,
            finished,
            &read_events(dir.path(), finished).unwrap(),
            OffsetDateTime::now_utc(),
        );
        assert!(line.contains("): finished"), "{line}");
    }
}
