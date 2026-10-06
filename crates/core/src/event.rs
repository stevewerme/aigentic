use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use ulid::Ulid;

use crate::Author;

/// Kind of a log event. Kinds are added, never changed, so old logs always
/// replay.
///
/// Tool calls are not an event kind: they live as `ToolCall` blocks inside
/// the `assistant_message` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    UserMessage,
    AssistantMessage,
    ToolResult,
    TurnEnded,
    /// Replaces a range of events in the projection; the originals stay.
    Compacted,
    /// A fact for the stable prefix; never summarised.
    Pinned,
    /// The process died mid-turn; appended on resume, never edited in.
    Interrupted,
    /// The body of a skill entered the thread.
    SkillLoaded,
    /// A tool call needs a human.
    PermissionRequested,
    /// A human answered; the author is who answered.
    PermissionDecided,
    /// Facts stated in the thread were written to the project's memory
    /// files; carries what was written and the cursor for the next run.
    MemoryExtracted,
    /// `/remember <text>`: a person filed a memory line themselves, no
    /// model call (issue #14's reliable path). The event's author is who
    /// asked; the payload carries the file, the text and whether the
    /// line landed or was already present.
    MemoryRemembered,
    /// The first event of a thread created by the daemon (phase 5):
    /// which project it belongs to and the root its tools run in. Older
    /// logs have none and are grouped by their directory instead.
    ThreadStarted,
    /// The thread's title (phase 6): proposed by the utility model after
    /// the first turn (author `system`) or set with `/rename` (author the
    /// person). The last one wins.
    ThreadRenamed,
    /// The thread moved to another project (phase 6 step 10): from where
    /// to where, the new working root, and the workspace. The projection
    /// tells the model; a reload builds the thread in the last one.
    ProjectSwitched,
    /// In-turn eviction (issue #30): tool results and successful
    /// edit/write arguments at or before `through_seq`, within the turn
    /// the event was appended in, are stubbed in projection. The
    /// originals stay in the log; the sweep appends one per block of
    /// calls, so the cached prefix is stable between sweeps.
    ContextEvicted,
    /// The eviction sweep cannot fit the turn (issue #35): the
    /// projection at the deepest legal boundary — the floor, the newest
    /// calls — is still over the working-set target, so no sweep the
    /// turn could make would bring it down, and the thread is the one to
    /// hand to a fresh one. Appended once per turn, in the turn it
    /// happened in, and never projected into model context.
    ContextSaturated,
    /// A closed turn's used results are stubbed in a batch (issue #76):
    /// every successful tool result of every turn closed at or before
    /// `through_seq` is stubbed in projection, with the handle to recall
    /// it. Appended at a turn's first loop iteration, so the history
    /// changes once at a turn's start and the cached prefix breaks once
    /// per batch rather than once per message. The originals stay in the
    /// log.
    ResultsStubbed,
    /// A provider retry (issue #31), appended live as the adapter starts
    /// waiting, so a turn that hangs on a dead endpoint reads as retries
    /// and not as a slow model. Author `system`; the payload carries the
    /// attempt, the total, why, and the wait in ms.
    ProviderRetried,
    /// The build runner began a run (layer 2, issue #53). Author
    /// `agent:runner`; the payload names the issue, the workflow and its
    /// version and content hash, and the provisional issue budget. It is
    /// the first lead-thread event of a run, so a replay that finds none
    /// is not looking at a run at all.
    RunStarted,
    /// The runner entered a step (layer 2, issue #53). Author
    /// `agent:runner`; the payload names the step, role, profile, the
    /// child thread and the attempt. Written before the child is
    /// awaited, so a replay re-awaits instead of starting the step
    /// twice. Every re-entry (send-back, continue, fresh thread) is a
    /// new event with `attempt + 1`.
    StepStarted,
    /// A step's child turn ended (layer 2, issue #53). Author
    /// `agent:runner`; the payload carries the status, the end reason,
    /// the cost and the child's `step_reported` event id. Written after
    /// the child ended and before the checks run.
    StepFinished,
    /// The runner ran a step's exact checks (layer 2, issue #53). Author
    /// `agent:runner`; the payload carries each check's result. A `fail`
    /// blocks the push, so a replay after this event decides send-back
    /// or push from it rather than re-running the checks.
    ChecksRun,
    /// The runner took a route at a branch point (layer 2, issue #53).
    /// Author `agent:runner`; the payload carries the branch, what was
    /// proposed, the preconditions with their results, what was taken
    /// and why a fallback was needed. Replay follows the logged decision:
    /// preconditions read git state that a crash can change.
    RouteTaken,
    /// The runner asked a human at a checkpoint (layer 2, issue #53).
    /// Author `agent:runner`; the payload carries the gate, what was
    /// shown and the options. Written before the wait, so a replay waits
    /// again instead of acting on an answer nobody gave.
    CheckpointAsked,
    /// A human answered a checkpoint (layer 2, issue #53). The author is
    /// who answered; the payload carries the answer, an amendment and
    /// the reveal marks. Written before the runner acts on it.
    CheckpointAnswered,
    /// The runner crossed a budget level (layer 2, issue #53). Author
    /// `agent:runner`; the payload carries the scope, what was spent and
    /// the limit, so a restart re-warns only at levels not yet reached.
    /// Not a decision: replay sees the move of the event before it.
    BudgetWarned,
    /// The runner pushed the run's commits and installed the binary
    /// (layer 2, issue #53). Author `agent:runner`; the payload carries
    /// the commits, the remote ref before and after and the installed
    /// binary's commit. Appended after push and install, which are
    /// idempotent, so a replay resumes after the push.
    Pushed,
    /// The run ended (layer 2, issue #53). Author `agent:runner`; the
    /// payload carries the outcome, the cost and the release impact.
    /// The last lead-thread event of a run.
    RunFinished,
    /// A child step reported through `finish_step` (layer 2, issue #53).
    /// Lives in the child thread, not the lead one; the payload is the
    /// section 4 field table, every field optional because each step
    /// fills a subset. Written by the tool — a later ticket's — and read
    /// by the runner's `step_finished`.
    StepReported,
    /// The harness proposes a decision of some kind (issue #74). Author:
    /// the agent or system that proposed it. Appended **before** the turn
    /// parks (the `CheckpointAsked` rule), so a replay or resume finds the
    /// open proposal rather than acting on an answer nobody gave.
    DecisionProposed,
    /// The answer to a `DecisionProposed` (issue #74), with
    /// `parent_event = Some(<the proposal's event id>)`. Author: whoever
    /// answered — a user for a person's answer, `Author::System` for a
    /// proposal the system closes (`withdrawn`).
    DecisionAnswered,
}

/// One line of a thread's append-only log. The log is the source of truth;
/// model context and UI are projections of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub id: Ulid,
    pub thread_id: Ulid,
    /// Position in thread, gapless.
    pub seq: u64,
    pub kind: EventKind,
    pub author: Author,
    /// Kind-specific JSON.
    pub payload: serde_json::Value,
    /// Links a `tool_result` to its `assistant_message`, and a
    /// `decision_answered` to its `decision_proposed` (issue #74).
    pub parent_event: Option<Ulid>,
    /// Serialised as RFC 3339.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::tests::all_blocks;
    use crate::{AgentId, Message, Role, UserId};
    use serde_json::json;
    use time::macros::datetime;

    fn event(seq: u64, kind: EventKind, author: Author, payload: serde_json::Value) -> Event {
        Event {
            id: Ulid::from_parts(1_700_000_000_000 + seq, u128::from(seq)),
            thread_id: Ulid::from_parts(1_700_000_000_000, 42),
            seq,
            kind,
            author,
            payload,
            parent_event: None,
            created_at: datetime!(2026-09-21 12:00:00 UTC),
        }
    }

    /// One event per kind, in the order a real turn produces them.
    fn one_of_each_kind() -> Vec<Event> {
        let steve = Author::User(UserId("steve".into()));
        let agent = Author::Agent(AgentId("worker".into()));
        let assistant = Message {
            role: Role::Assistant,
            author: agent.clone(),
            blocks: all_blocks(),
        };
        let mut events = vec![
            event(
                0,
                EventKind::UserMessage,
                steve,
                json!({"blocks": [{"type": "text", "text": "hi"}]}),
            ),
            event(
                1,
                EventKind::AssistantMessage,
                agent.clone(),
                serde_json::to_value(&assistant).unwrap(),
            ),
            event(
                2,
                EventKind::ToolResult,
                Author::System,
                json!({"id": "call_1", "content": "[workspace]", "is_error": false}),
            ),
            event(
                3,
                EventKind::TurnEnded,
                agent.clone(),
                json!({"reason": "done"}),
            ),
            event(
                4,
                EventKind::Compacted,
                agent,
                json!({"from_seq": 0, "to_seq": 3, "strategy": {"kind": "truncate_results", "max_bytes": 4096}}),
            ),
            event(
                5,
                EventKind::Pinned,
                steve_again(),
                json!({"text": "Use Swedish."}),
            ),
            event(
                6,
                EventKind::Interrupted,
                Author::System,
                json!({"reason": "process exited mid-turn", "after_seq": 5, "unanswered_calls": []}),
            ),
            event(
                7,
                EventKind::SkillLoaded,
                steve_again(),
                json!({"name": "tdd", "hash": "abc", "source": "github.com/mattpocock/skills@c55ee46", "body": "# TDD", "invoked_by": "user"}),
            ),
            event(
                8,
                EventKind::PermissionRequested,
                Author::System,
                json!({"call": {"id": "call_2", "name": "bash", "args": {"command": "rm -rf build"}}, "class": "exec", "reason": "class exec: ask"}),
            ),
            event(
                9,
                EventKind::PermissionDecided,
                steve_again(),
                json!({"call_id": "call_2", "allow": true, "scope": "once"}),
            ),
            event(
                10,
                EventKind::MemoryExtracted,
                agent_again(),
                json!({"through_seq": 9, "written": [{"file": "decisions.md", "text": "Use Swedish.", "stated_by": {"kind": "user", "id": "steve"}, "at_seq": 0}],
                       "model": "m", "usage": {"input_tokens": 10, "output_tokens": 2}}),
            ),
            event(
                11,
                EventKind::ThreadStarted,
                steve_again(),
                json!({"project": "vendela", "root": "/srv/vendela", "created_by": {"kind": "user", "id": "steve"}}),
            ),
            // The build runner's kinds (issue #53), in the order a run
            // appends them.
            event(
                12,
                EventKind::RunStarted,
                runner(),
                json!({"issue": 53, "workflow": "build", "version": 1, "content_hash": "abc", "budget_usd": 10.0}),
            ),
            event(
                13,
                EventKind::StepStarted,
                runner(),
                json!({"step": "plan", "role": "planner", "profile": "kimi",
                       "child_thread": child().to_string(), "attempt": 1, "budget_usd": 3.0}),
            ),
            event(
                14,
                EventKind::StepFinished,
                runner(),
                json!({"step": "plan", "status": "done", "end_reason": "done",
                       "cost_usd": 0.4, "reported_event": child().to_string()}),
            ),
            event(
                15,
                EventKind::ChecksRun,
                runner(),
                json!({"step": "plan", "checks": [{"id": "E1", "result": "pass", "detail": null}]}),
            ),
            event(
                16,
                EventKind::RouteTaken,
                runner(),
                json!({"branch": "plan_gate", "proposed": "implement", "taken": "implement",
                       "preconditions": [], "fallback_reason": null}),
            ),
            event(
                17,
                EventKind::CheckpointAsked,
                runner(),
                json!({"gate": "plan_gate_blind", "shown": ["plan"], "options": ["go", "amend", "stop"]}),
            ),
            event(
                18,
                EventKind::CheckpointAnswered,
                steve_again(),
                json!({"answer": "go", "amendment": null, "marks": []}),
            ),
            event(
                19,
                EventKind::BudgetWarned,
                runner(),
                json!({"scope": "issue", "spent_usd": 8.0, "limit_usd": 10.0}),
            ),
            event(
                20,
                EventKind::Pushed,
                runner(),
                json!({"commits": [{"sha": "abc123", "subject": "core: add kinds"}],
                       "ref_before": "abc000", "ref_after": "abc123", "installed": "abc123"}),
            ),
            event(
                21,
                EventKind::RunFinished,
                runner(),
                json!({"outcome": "closed", "cost_usd": 0.9, "release_impact": "none"}),
            ),
            event(
                22,
                EventKind::StepReported,
                agent_again(),
                json!({"status": "done", "body": "## Plan", "commits": []}),
            ),
            // Decisions (issue #74): the harness proposes, a person
            // answers, and the answer names the proposal.
            event(
                23,
                EventKind::DecisionProposed,
                Author::System,
                json!({"kind": "project", "proposal": "switch to getscale/site",
                       "target": "getscale/site", "reason": "the brief names it",
                       "stage": "ask"}),
            ),
            event(
                24,
                EventKind::DecisionAnswered,
                steve_again(),
                json!({"answer": "yes"}),
            ),
        ];
        events[2].parent_event = Some(events[1].id);
        events[9].parent_event = Some(events[8].id);
        events[24].parent_event = Some(events[23].id);
        // A closed-turn batch (issue #76) sits at the end so the indices
        // the wire-shape test reads stay put.
        events.push(event(
            25,
            EventKind::ResultsStubbed,
            Author::System,
            json!({"through_seq": 3}),
        ));
        events
    }

    fn steve_again() -> Author {
        Author::User(UserId("steve".into()))
    }

    fn agent_again() -> Author {
        Author::Agent(AgentId("worker".into()))
    }

    /// The build runner (issue #53) is an agent like any other.
    fn runner() -> Author {
        Author::Agent(AgentId("runner".into()))
    }

    fn child() -> Ulid {
        Ulid::from_parts(1_700_000_000_000, 7)
    }

    /// The wire-name rule: `CamelCase` variant names become `snake_case`,
    /// so the expected name is derived from the variant and not written
    /// out by hand next to it.
    fn snake_case(name: &str) -> String {
        let mut out = String::new();
        for (i, c) in name.char_indices() {
            if c.is_uppercase() {
                if i != 0 {
                    out.push('_');
                }
                out.extend(c.to_lowercase());
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn every_kind_round_trips_as_jsonl() {
        for event in one_of_each_kind() {
            let line = serde_json::to_string(&event).unwrap();
            assert!(!line.contains('\n'), "an event must fit on one line");
            let back: Event = serde_json::from_str(&line).unwrap();
            assert_eq!(back, event, "round trip failed for {line}");
        }
    }

    /// T1 (issue #53): the eleven new kinds round-trip and land on the
    /// wire under the enum's snake_case rule — the name comes from the
    /// variant, not from a hand-written pair.
    #[test]
    fn the_runner_kinds_round_trip_under_the_snake_case_rule() {
        let kinds = [
            EventKind::RunStarted,
            EventKind::StepStarted,
            EventKind::StepFinished,
            EventKind::ChecksRun,
            EventKind::RouteTaken,
            EventKind::CheckpointAsked,
            EventKind::CheckpointAnswered,
            EventKind::BudgetWarned,
            EventKind::Pushed,
            EventKind::RunFinished,
            EventKind::StepReported,
        ];
        let mut names: Vec<String> = Vec::new();
        for kind in kinds {
            let wire = serde_json::to_value(kind).unwrap();
            let expected = snake_case(&format!("{kind:?}"));
            assert_eq!(wire, serde_json::Value::String(expected.clone()));
            // And the kind reads back the same way in a whole event.
            let event = event(
                0,
                kind,
                runner(),
                json!({"any": "payload survives whatever the kind"}),
            );
            let line = serde_json::to_string(&event).unwrap();
            assert_eq!(serde_json::from_str::<Event>(&line).unwrap(), event);
            names.push(expected);
        }
        assert_eq!(names.len(), 11);
        let mut unique = names.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), names.len(), "wire names are distinct");
    }

    /// T1 (issue #74): the two decision kinds round-trip and land on the
    /// wire under the enum's snake_case rule, derived the same way as the
    /// runner kinds above — never a hand-written name pair.
    #[test]
    fn the_decision_kinds_round_trip_under_the_snake_case_rule() {
        let kinds = [EventKind::DecisionProposed, EventKind::DecisionAnswered];
        let mut names: Vec<String> = Vec::new();
        for kind in kinds {
            let wire = serde_json::to_value(kind).unwrap();
            let expected = snake_case(&format!("{kind:?}"));
            assert_eq!(wire, serde_json::Value::String(expected.clone()));
            let event = event(
                0,
                kind,
                steve_again(),
                json!({"any": "payload survives whatever the kind"}),
            );
            let line = serde_json::to_string(&event).unwrap();
            assert_eq!(serde_json::from_str::<Event>(&line).unwrap(), event);
            names.push(expected);
        }
        assert_eq!(names, vec!["decision_proposed", "decision_answered"]);
    }

    /// T2 (issue #76): the batch kind round-trips as a whole event and
    /// lands on the wire under the enum's snake_case rule, derived from
    /// the variant, never a hand-written pair.
    #[test]
    fn the_results_stubbed_kind_round_trips_under_the_snake_case_rule() {
        let kind = EventKind::ResultsStubbed;
        let wire = serde_json::to_value(kind).unwrap();
        assert_eq!(
            wire,
            serde_json::Value::String(snake_case(&format!("{kind:?}")))
        );
        let event = event(0, kind, Author::System, json!({"through_seq": 3}));
        let line = serde_json::to_string(&event).unwrap();
        assert_eq!(serde_json::from_str::<Event>(&line).unwrap(), event);
    }

    #[test]
    fn wire_shape_matches_prd_field_table() {
        let events = one_of_each_kind();
        let value = serde_json::to_value(&events[2]).unwrap();
        assert_eq!(value["kind"], "tool_result");
        assert_eq!(
            serde_json::to_value(&events[4]).unwrap()["kind"],
            "compacted"
        );
        assert_eq!(serde_json::to_value(&events[5]).unwrap()["kind"], "pinned");
        assert_eq!(
            serde_json::to_value(&events[6]).unwrap()["kind"],
            "interrupted"
        );
        assert_eq!(
            serde_json::to_value(&events[7]).unwrap()["kind"],
            "skill_loaded"
        );
        assert_eq!(
            serde_json::to_value(&events[8]).unwrap()["kind"],
            "permission_requested"
        );
        assert_eq!(
            serde_json::to_value(&events[9]).unwrap()["kind"],
            "permission_decided"
        );
        assert_eq!(
            serde_json::to_value(&events[10]).unwrap()["kind"],
            "memory_extracted"
        );
        assert_eq!(
            serde_json::to_value(&events[11]).unwrap()["kind"],
            "thread_started"
        );
        assert_eq!(value["seq"], 2);
        assert_eq!(value["author"], json!({"kind": "system"}));
        assert_eq!(value["created_at"], "2026-09-21T12:00:00Z");
        assert_eq!(value["parent_event"], events[1].id.to_string());
        assert_eq!(value["id"], events[2].id.to_string());
    }

    #[test]
    fn missing_parent_event_serialises_as_null_and_reads_back() {
        let value = serde_json::to_value(&one_of_each_kind()[0]).unwrap();
        assert!(value["parent_event"].is_null());
        let back: Event = serde_json::from_value(value).unwrap();
        assert_eq!(back.parent_event, None);
    }
}
