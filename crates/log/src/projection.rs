//! Events to model context. Compaction is applied here, as a projection:
//! the originals stay in the log and `compacted` events change only what
//! the model sees.

use aigentic_core::{Author, ContentBlock, Event, EventKind, Message, Role};
use time::format_description::well_known::Rfc3339;

use crate::payload::{
    AssistantMessagePayload, CompactedPayload, CompactionStrategy, InterruptedPayload,
    PinnedPayload, SkillLoadedPayload, ToolResultPayload, UserMessagePayload,
};
use crate::store::LogError;

/// What the runtime builds context from.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Projection {
    /// Pinned facts, oldest first, for the stable prefix.
    pub pinned: Vec<String>,
    /// The thread body with compaction applied.
    pub body: Vec<Message>,
    /// `to_seq` of the latest summary compaction, if any.
    pub compacted_through: Option<u64>,
}

/// A summary compaction as it applies to the projection.
#[derive(Debug, Clone)]
struct Summary {
    /// Seq of the `compacted` event itself; later ones win on overlap.
    at: u64,
    from: u64,
    to: u64,
    message: Message,
}

/// A truncation compaction.
#[derive(Debug, Clone, Copy)]
struct Truncation {
    from: u64,
    to: u64,
    max_bytes: usize,
}

/// Project events into the canonical messages a provider sees, oldest
/// first, with compaction applied.
///
/// Rules, in order: pinned events lift out of the body; a summary replaces
/// its range with one user-role message from the system author (later
/// summaries win where ranges overlap); truncations shorten tool results in
/// their range; provider blobs are dropped from every assistant message
/// that predates the latest summary compaction; an interrupted event
/// becomes a short note; a loaded skill becomes a user-role message from
/// the system author (a marker line, then the body); `turn_ended`,
/// `compacted`, `permission_requested` and `permission_decided` emit
/// nothing (a refused call is visible as its error tool result), as does
/// `memory_extracted` (the memory files are read into the prefix).
///
/// Every provider requires an assistant message's tool results to follow
/// it immediately, so a message produced between an assistant message and
/// its results (a `skill_loaded` from `load_skill`, say) is held back and
/// emitted after the last of those results.
pub fn project(events: &[Event]) -> Result<Projection, LogError> {
    let mut summaries: Vec<Summary> = Vec::new();
    let mut truncations: Vec<Truncation> = Vec::new();
    let mut blobs_dropped_before: Option<u64> = None;
    let mut pinned = Vec::new();

    for event in events {
        match event.kind {
            EventKind::Compacted => {
                let p: CompactedPayload = payload(event)?;
                match p.strategy {
                    CompactionStrategy::TruncateResults { max_bytes } => {
                        truncations.push(Truncation {
                            from: p.from_seq,
                            to: p.to_seq,
                            max_bytes,
                        });
                    }
                    CompactionStrategy::Summary { text, model, .. } => {
                        let date = event.created_at.format(&Rfc3339).unwrap_or_default();
                        let date = date.get(..10).unwrap_or(&date).to_owned();
                        summaries.push(Summary {
                            at: event.seq,
                            from: p.from_seq,
                            to: p.to_seq,
                            message: Message {
                                role: Role::User,
                                author: Author::System,
                                blocks: vec![ContentBlock::Text(format!(
                                    "{}\n\n{text}",
                                    summary_marker(p.from_seq, p.to_seq, &model, &date)
                                ))],
                            },
                        });
                        blobs_dropped_before = Some(event.seq);
                    }
                }
            }
            EventKind::Pinned => {
                let p: PinnedPayload = payload(event)?;
                pinned.push(p.text);
            }
            _ => {}
        }
    }

    let mut body: Vec<Message> = Vec::with_capacity(events.len());
    let mut emitted: Vec<u64> = Vec::new(); // summaries (by `at`) already placed
    // Tool call ids of the last assistant message still without a result,
    // and the messages held back until they all have one.
    let mut pending_calls: Vec<String> = Vec::new();
    let mut held: Vec<Message> = Vec::new();
    let push = |body: &mut Vec<Message>,
                pending: &mut Vec<String>,
                held: &mut Vec<Message>,
                message: Message| {
        match &message.role {
            Role::Tool => {
                for block in &message.blocks {
                    if let ContentBlock::ToolResult(r) = block {
                        pending.retain(|id| id != &r.id);
                    }
                }
                body.push(message);
                if pending.is_empty() {
                    body.append(held);
                }
            }
            Role::Assistant => {
                // A new assistant message ends the previous call group
                // whatever was answered (resume synthesises the rest).
                body.append(held);
                pending.clear();
                for block in &message.blocks {
                    if let ContentBlock::ToolCall(c) = block {
                        pending.push(c.id.clone());
                    }
                }
                body.push(message);
            }
            _ if !pending.is_empty() => held.push(message),
            _ => body.push(message),
        }
    };
    for event in events {
        // Covered by a summary? The latest-appended one that contains this seq wins.
        if let Some(summary) = summaries
            .iter()
            .filter(|s| s.from <= event.seq && event.seq <= s.to)
            .max_by_key(|s| s.at)
        {
            if !emitted.contains(&summary.at) {
                emitted.push(summary.at);
                push(
                    &mut body,
                    &mut pending_calls,
                    &mut held,
                    summary.message.clone(),
                );
            }
            continue;
        }
        match event.kind {
            EventKind::UserMessage => {
                let p: UserMessagePayload = payload(event)?;
                push(
                    &mut body,
                    &mut pending_calls,
                    &mut held,
                    Message {
                        role: Role::User,
                        author: event.author.clone(),
                        blocks: p.blocks,
                    },
                );
            }
            EventKind::AssistantMessage => {
                let p: AssistantMessagePayload = payload(event)?;
                let drop_blobs = blobs_dropped_before.is_some_and(|at| event.seq < at);
                let blocks = p
                    .blocks
                    .into_iter()
                    .filter(|b| !(drop_blobs && matches!(b, ContentBlock::ProviderBlob(_))))
                    .collect();
                push(
                    &mut body,
                    &mut pending_calls,
                    &mut held,
                    Message {
                        role: Role::Assistant,
                        author: event.author.clone(),
                        blocks,
                    },
                );
            }
            EventKind::ToolResult => {
                let ToolResultPayload { mut result, .. } = payload(event)?;
                if let Some(max) = truncations
                    .iter()
                    .filter(|t| t.from <= event.seq && event.seq <= t.to)
                    .map(|t| t.max_bytes)
                    .min()
                {
                    result.content = truncate_middle(&result.content, max);
                }
                push(
                    &mut body,
                    &mut pending_calls,
                    &mut held,
                    Message {
                        role: Role::Tool,
                        author: event.author.clone(),
                        blocks: vec![ContentBlock::ToolResult(result)],
                    },
                );
            }
            EventKind::Interrupted => {
                let p: InterruptedPayload = payload(event)?;
                let calls = if p.unanswered_calls.is_empty() {
                    String::new()
                } else {
                    format!(
                        " Tool calls {} received no real result; their results below are synthetic.",
                        p.unanswered_calls.join(", ")
                    )
                };
                push(
                    &mut body,
                    &mut pending_calls,
                    &mut held,
                    Message {
                        role: Role::User,
                        author: Author::System,
                        blocks: vec![ContentBlock::Text(format!(
                            "[The previous turn was interrupted: {}.{calls} Continue from here; rerun anything whose outcome is unknown.]",
                            p.reason
                        ))],
                    },
                );
            }
            EventKind::SkillLoaded => {
                let p: SkillLoadedPayload = payload(event)?;
                push(
                    &mut body,
                    &mut pending_calls,
                    &mut held,
                    Message {
                        role: Role::User,
                        author: Author::System,
                        blocks: vec![ContentBlock::Text(format!(
                            "{}\n\n{}",
                            skill_marker(&p.name),
                            p.body
                        ))],
                    },
                );
            }
            EventKind::TurnEnded
            | EventKind::Compacted
            | EventKind::Pinned
            | EventKind::PermissionRequested
            | EventKind::PermissionDecided
            | EventKind::MemoryExtracted => {}
        }
    }

    body.append(&mut held);

    Ok(Projection {
        pinned,
        body,
        compacted_through: summaries.iter().map(|s| s.to).max(),
    })
}

/// The line placed before a summary so the model knows what it is.
pub fn summary_marker(from_seq: u64, to_seq: u64, model: &str, date: &str) -> String {
    format!(
        "[Summary of events {from_seq} to {to_seq}, written by {model} on {date}; the originals are in the log]"
    )
}

/// The line placed before a loaded skill's body so the model knows what it is.
pub fn skill_marker(name: &str) -> String {
    format!("[Skill `{name}` loaded; follow it for this task]")
}

fn payload<T: serde::de::DeserializeOwned>(event: &Event) -> Result<T, LogError> {
    serde_json::from_value(event.payload.clone()).map_err(|source| LogError::Payload {
        seq: event.seq,
        kind: event.kind,
        source,
    })
}

/// Keep the head and the tail of `text` within `max_bytes`, noting what
/// was omitted. The same rule the tools crate applies at capture time;
/// duplicated here because `log` may not depend on `tools`.
pub fn truncate_middle(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut head = max_bytes / 2;
    while !text.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail_start = text.len() - (max_bytes - head);
    while !text.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let omitted = tail_start - head;
    format!(
        "{}\n[... {omitted} bytes omitted by compaction ...]\n{}",
        &text[..head],
        &text[tail_start..]
    )
}

/// Convenience for callers that only want the body.
pub fn project_body(events: &[Event]) -> Result<Vec<Message>, LogError> {
    Ok(project(events)?.body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::payload::Usage;
    use aigentic_core::{AgentId, ToolCall, ToolResult, UserId};
    use serde_json::json;
    use time::macros::datetime;
    use ulid::Ulid;

    fn ev(seq: u64, kind: EventKind, author: Author, payload: serde_json::Value) -> Event {
        Event {
            id: Ulid::from_parts(1_700_000_000_000 + seq, u128::from(seq)),
            thread_id: Ulid::from_parts(1_700_000_000_000, 1),
            seq,
            kind,
            author,
            payload,
            parent_event: None,
            created_at: datetime!(2026-09-21 12:00:00 UTC),
        }
    }
    fn steve() -> Author {
        Author::User(UserId("steve".into()))
    }
    fn agent() -> Author {
        Author::Agent(AgentId("worker".into()))
    }
    fn user(seq: u64, text: &str) -> Event {
        ev(
            seq,
            EventKind::UserMessage,
            steve(),
            json!({"blocks": [{"type": "text", "text": text}]}),
        )
    }
    fn assistant(seq: u64, text: &str, with_blob: bool) -> Event {
        let mut blocks = vec![json!({"type": "text", "text": text})];
        if with_blob {
            blocks.push(json!({"type": "provider_blob", "provider": "anthropic", "data": {"type": "thinking"}}));
        }
        ev(
            seq,
            EventKind::AssistantMessage,
            agent(),
            json!({"blocks": blocks}),
        )
    }
    fn result(seq: u64, id: &str, content: &str) -> Event {
        ev(
            seq,
            EventKind::ToolResult,
            Author::System,
            json!({"id": id, "content": content, "is_error": false}),
        )
    }
    fn ended(seq: u64) -> Event {
        ev(
            seq,
            EventKind::TurnEnded,
            agent(),
            json!({"reason": "done"}),
        )
    }
    fn summary(seq: u64, from: u64, to: u64, text: &str) -> Event {
        let p = CompactedPayload {
            from_seq: from,
            to_seq: to,
            strategy: CompactionStrategy::Summary {
                text: text.into(),
                model: "m".into(),
                usage: Usage::reported(aigentic_core::Usage::default()),
            },
        };
        ev(
            seq,
            EventKind::Compacted,
            agent(),
            serde_json::to_value(p).unwrap(),
        )
    }
    fn truncation(seq: u64, from: u64, to: u64, max: usize) -> Event {
        let p = CompactedPayload {
            from_seq: from,
            to_seq: to,
            strategy: CompactionStrategy::TruncateResults { max_bytes: max },
        };
        ev(
            seq,
            EventKind::Compacted,
            agent(),
            serde_json::to_value(p).unwrap(),
        )
    }
    fn texts(p: &Projection) -> Vec<String> {
        p.body
            .iter()
            .map(|m| match &m.blocks[0] {
                ContentBlock::Text(t) => t.clone(),
                ContentBlock::ToolResult(r) => format!("result:{}", r.content),
                other => format!("{other:?}"),
            })
            .collect()
    }

    /// Two complete turns, then a pin.
    fn two_turns() -> Vec<Event> {
        vec![
            user(0, "one"),
            assistant(1, "reply one", true),
            ended(2),
            user(3, "two"),
            assistant(4, "reply two", true),
            ended(5),
            ev(
                6,
                EventKind::Pinned,
                steve(),
                json!({"text": "Use Swedish."}),
            ),
        ]
    }

    #[test]
    fn phase0_shape_is_unchanged_without_compaction() {
        let p = project(&two_turns()).unwrap();
        assert_eq!(texts(&p), vec!["one", "reply one", "two", "reply two"]);
        assert_eq!(p.body[1].blocks.len(), 2, "blobs survive with no summary");
        assert_eq!(p.compacted_through, None);
    }

    #[test]
    fn pins_lift_into_the_prefix() {
        let p = project(&two_turns()).unwrap();
        assert_eq!(p.pinned, vec!["Use Swedish."]);
        assert!(!texts(&p).iter().any(|t| t.contains("Swedish")));
    }

    #[test]
    fn a_summary_replaces_its_range_and_strips_blobs_before_it() {
        let mut events = two_turns();
        events.push(summary(7, 0, 2, "Turn one happened."));
        let p = project(&events).unwrap();
        let t = texts(&p);
        assert_eq!(t.len(), 3, "{t:#?}");
        assert!(
            t[0].starts_with("[Summary of events 0 to 2, written by m on 2026-09-21"),
            "{}",
            t[0]
        );
        assert!(t[0].ends_with("Turn one happened."));
        assert_eq!(t[1], "two");
        assert_eq!(t[2], "reply two");
        assert_eq!(p.body[0].role, Role::User);
        assert_eq!(p.body[0].author, Author::System);
        assert_eq!(
            p.body[2].blocks.len(),
            1,
            "retained turn keeps text, loses its blob"
        );
        assert_eq!(p.compacted_through, Some(2));
    }

    #[test]
    fn a_later_summary_nests_over_an_earlier_one() {
        let mut events = two_turns();
        events.push(summary(7, 0, 2, "first"));
        events.push(user(8, "three"));
        events.push(assistant(9, "reply three", true));
        events.push(ended(10));
        events.push(summary(11, 0, 5, "first and second"));
        let p = project(&events).unwrap();
        let t = texts(&p);
        assert_eq!(t.len(), 3, "{t:#?}");
        assert!(t[0].ends_with("first and second"));
        assert!(
            !t.iter().any(|x| x.ends_with("\nfirst")),
            "earlier summary is gone"
        );
        assert_eq!(t[1], "three");
        assert_eq!(
            p.body[2].blocks.len(),
            1,
            "turn three predates the latest summary event: blob dropped"
        );
        assert_eq!(p.compacted_through, Some(5));
    }

    #[test]
    fn a_chained_summary_keeps_the_earlier_one() {
        let mut events = two_turns();
        events.push(summary(7, 0, 2, "A"));
        events.push(summary(8, 3, 5, "B"));
        let p = project(&events).unwrap();
        let t = texts(&p);
        assert_eq!(t.len(), 2, "{t:#?}");
        assert!(t[0].ends_with("A"));
        assert!(t[1].ends_with("B"));
    }

    #[test]
    fn truncation_shortens_only_results_in_range() {
        let long = "x".repeat(1000);
        let events = vec![
            user(0, "go"),
            assistant(1, "calling", false),
            result(2, "c1", &long),
            ended(3),
            user(4, "again"),
            assistant(5, "calling", false),
            result(6, "c2", &long),
            ended(7),
            truncation(8, 0, 3, 100),
        ];
        let p = project(&events).unwrap();
        let t = texts(&p);
        assert!(t[2].contains("omitted by compaction"), "{}", t[2]);
        assert!(t[2].len() < 200);
        assert_eq!(t[5], format!("result:{long}"), "out of range: untouched");
    }

    #[test]
    fn interrupted_becomes_a_note() {
        let events = vec![
            user(0, "go"),
            ev(
                1,
                EventKind::Interrupted,
                Author::System,
                json!({"reason": "process exited mid-turn", "after_seq": 0, "unanswered_calls": ["c1"]}),
            ),
        ];
        let p = project(&events).unwrap();
        assert_eq!(p.body[1].role, Role::User);
        assert_eq!(p.body[1].author, Author::System);
        let note = &texts(&p)[1];
        assert!(
            note.contains("interrupted: process exited mid-turn"),
            "{note}"
        );
        assert!(note.contains("c1"), "{note}");
    }

    #[test]
    fn a_loaded_skill_is_a_system_message_with_marker_and_body() {
        let events = vec![
            ev(
                0,
                EventKind::SkillLoaded,
                steve(),
                json!({"name": "tdd", "hash": "abc", "source": "s", "body": "# TDD\n\nRed, green, refactor.", "invoked_by": "user"}),
            ),
            user(1, "implement the thing"),
        ];
        let p = project(&events).unwrap();
        assert_eq!(p.body.len(), 2);
        assert_eq!(p.body[0].role, Role::User);
        assert_eq!(p.body[0].author, Author::System);
        let t = texts(&p);
        assert_eq!(
            t[0],
            "[Skill `tdd` loaded; follow it for this task]\n\n# TDD\n\nRed, green, refactor."
        );
        assert_eq!(t[1], "implement the thing");
    }

    #[test]
    fn permission_events_emit_nothing_and_a_policy_record_is_invisible() {
        let events = vec![
            user(0, "go"),
            assistant(1, "calling", false),
            ev(
                2,
                EventKind::PermissionRequested,
                Author::System,
                json!({"call": {"id": "c1", "name": "bash", "args": {}}, "class": "exec", "reason": "class exec: ask"}),
            ),
            ev(
                3,
                EventKind::PermissionDecided,
                steve(),
                json!({"call_id": "c1", "allow": true, "scope": "once"}),
            ),
            ev(
                4,
                EventKind::ToolResult,
                Author::System,
                json!({"id": "c1", "content": "ok", "is_error": false, "policy": {"kind": "rule", "rule": "class read", "decision": "allow"}}),
            ),
            ended(5),
        ];
        let p = project(&events).unwrap();
        assert_eq!(texts(&p), vec!["go", "calling", "result:ok"]);
        assert_eq!(p.body[2].role, Role::Tool);
    }

    #[test]
    fn a_skill_loaded_between_a_call_and_its_result_moves_after_the_result() {
        let events = vec![
            user(0, "go"),
            ev(
                1,
                EventKind::AssistantMessage,
                agent(),
                serde_json::to_value(AssistantMessagePayload {
                    blocks: vec![
                        ContentBlock::ToolCall(ToolCall {
                            id: "c1".into(),
                            name: "load_skill".into(),
                            args: json!({"name": "tdd"}),
                        }),
                        ContentBlock::ToolCall(ToolCall {
                            id: "c2".into(),
                            name: "read_file".into(),
                            args: json!({}),
                        }),
                    ],
                    usage: None,
                })
                .unwrap(),
            ),
            ev(
                2,
                EventKind::SkillLoaded,
                agent(),
                json!({"name": "tdd", "hash": "h", "source": "s", "body": "# TDD", "invoked_by": "model"}),
            ),
            result(3, "c1", "loaded"),
            result(4, "c2", "contents"),
            assistant(5, "reply", false),
            ended(6),
        ];
        let p = project(&events).unwrap();
        let roles: Vec<Role> = p.body.iter().map(|m| m.role).collect();
        assert_eq!(
            roles,
            vec![
                Role::User,
                Role::Assistant,
                Role::Tool,
                Role::Tool,
                Role::User,
                Role::Assistant
            ]
        );
        assert!(texts(&p)[4].starts_with("[Skill `tdd` loaded"));
        assert_eq!(texts(&p)[2], "result:loaded");
    }

    #[test]
    fn memory_extracted_emits_nothing() {
        let events = vec![
            user(0, "go"),
            assistant(1, "ok", false),
            ended(2),
            ev(
                3,
                EventKind::MemoryExtracted,
                agent(),
                json!({"through_seq": 2, "written": [], "model": "m", "usage": {"input_tokens": 1, "output_tokens": 1}}),
            ),
        ];
        assert_eq!(texts(&project(&events).unwrap()), vec!["go", "ok"]);
    }

    #[test]
    fn a_skill_loaded_with_no_open_calls_stays_in_place() {
        let events = vec![
            ev(
                0,
                EventKind::SkillLoaded,
                steve(),
                json!({"name": "implement", "hash": "h", "source": "s", "body": "# I", "invoked_by": "user"}),
            ),
            user(1, "do it"),
            assistant(2, "reply", false),
            ended(3),
        ];
        let p = project(&events).unwrap();
        let roles: Vec<Role> = p.body.iter().map(|m| m.role).collect();
        assert_eq!(roles, vec![Role::User, Role::User, Role::Assistant]);
        assert!(texts(&p)[0].starts_with("[Skill `implement` loaded"));
    }

    #[test]
    fn a_summary_covers_a_loaded_skill_like_any_body_event() {
        let events = vec![
            ev(
                0,
                EventKind::SkillLoaded,
                steve(),
                json!({"name": "tdd", "hash": "abc", "source": "s", "body": "long body", "invoked_by": "user"}),
            ),
            user(1, "one"),
            assistant(2, "reply one", false),
            ended(3),
            user(4, "two"),
            summary(5, 0, 3, "Loaded tdd and did one."),
        ];
        let p = project(&events).unwrap();
        let t = texts(&p);
        assert_eq!(t.len(), 2, "{t:#?}");
        assert!(t[0].ends_with("Loaded tdd and did one."));
        assert_eq!(t[1], "two");
    }

    #[test]
    fn truncate_middle_keeps_head_and_tail_on_char_boundaries() {
        assert_eq!(truncate_middle("short", 100), "short");
        let text = "é".repeat(100);
        let out = truncate_middle(&text, 21);
        assert!(!out.contains('\u{FFFD}'));
        assert!(out.contains("omitted by compaction"));
        assert!(out.starts_with("ééééé\n"), "{out}");
    }

    #[test]
    fn a_full_tool_turn_still_projects_to_role_tool() {
        let events = vec![
            user(0, "go"),
            ev(
                1,
                EventKind::AssistantMessage,
                agent(),
                serde_json::to_value(AssistantMessagePayload {
                    blocks: vec![ContentBlock::ToolCall(ToolCall {
                        id: "c1".into(),
                        name: "echo".into(),
                        args: json!({}),
                    })],
                    usage: None,
                })
                .unwrap(),
            ),
            result(2, "c1", "ok"),
            ended(3),
        ];
        let p = project(&events).unwrap();
        assert_eq!(p.body[2].role, Role::Tool);
        assert_eq!(
            p.body[2].blocks,
            vec![ContentBlock::ToolResult(ToolResult {
                id: "c1".into(),
                content: "ok".into(),
                is_error: false
            })]
        );
    }
}
