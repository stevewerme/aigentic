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
    /// The first event of a thread created by the daemon (phase 5):
    /// which project it belongs to and the root its tools run in. Older
    /// logs have none and are grouped by their directory instead.
    ThreadStarted,
    /// The thread's title (phase 6): proposed by the utility model after
    /// the first turn (author `system`) or set with `/rename` (author the
    /// person). The last one wins.
    ThreadRenamed,
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
    /// Links a `tool_result` to its `assistant_message`.
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
        ];
        events[2].parent_event = Some(events[1].id);
        events[9].parent_event = Some(events[8].id);
        events
    }

    fn steve_again() -> Author {
        Author::User(UserId("steve".into()))
    }

    fn agent_again() -> Author {
        Author::Agent(AgentId("worker".into()))
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
