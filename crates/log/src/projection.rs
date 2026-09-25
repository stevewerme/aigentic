//! Events to model context. Compaction is applied here, as a projection:
//! the originals stay in the log and `compacted` events change only what
//! the model sees.

use aigentic_core::{Author, ContentBlock, Event, EventKind, Message, Role};
use time::format_description::well_known::Rfc3339;

use crate::payload::{
    AssistantMessagePayload, CompactedPayload, CompactionStrategy, ContextEvictedPayload,
    InterruptedPayload, PinnedPayload, SkillLoadedPayload, ToolResultPayload, UserMessagePayload,
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

/// An in-turn eviction: the turn it belongs to (the `turn_ended` before
/// it, `None` when that turn opens the thread) and how far it reaches.
#[derive(Debug, Clone, Copy)]
struct Eviction {
    turn: Option<u64>,
    through: u64,
}

/// What the first pass needs to know about a tool call's result.
#[derive(Debug, Clone, Copy)]
struct CallResult {
    seq: u64,
    /// The turn the result belongs to, keyed like [`Eviction::turn`].
    turn: Option<u64>,
    is_error: bool,
    /// For a successful `edit_file` / `write_file`: its result's diff
    /// shape, `-removed/+added`, for the argument stub.
    diff: Option<(u64, u64)>,
}

/// Project events into the canonical messages a provider sees, oldest
/// first, with compaction applied.
///
/// Rules, in order: pinned events lift out of the body; a summary replaces
/// its range with one user-role message from the system author (later
/// summaries win where ranges overlap); truncations shorten tool results in
/// their range; a `context_evicted` event stubs the tool results at or
/// before its `through_seq` within its own turn, and the arguments of their
/// successful `edit_file` / `write_file` calls with the diff shape, except
/// failed results and the last result of each distinct tool, which stay;
/// provider blobs are dropped from every assistant message
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
///
/// The horizon rule (phase 5): a `user_message` with `mid_turn` set and
/// `steer` unset arrived while a turn ran and is emitted only once a
/// `turn_ended` follows it, so the model sees it from the next turn and a
/// replay gives the live run's context exactly. With `steer` set (issue
/// #33) the message is instead emitted where it sits in the log, so the
/// very next model call sees it: a person can steer a running agent, not
/// only interrupt it. The hold-back above still applies to it, so it
/// never lands between an assistant message and that message's own
/// results. `thread_started` emits nothing.
pub fn project(events: &[Event]) -> Result<Projection, LogError> {
    let mut summaries: Vec<Summary> = Vec::new();
    let mut truncations: Vec<Truncation> = Vec::new();
    let mut blobs_dropped_before: Option<u64> = None;
    let mut pinned = Vec::new();
    let last_turn_end = events
        .iter()
        .rev()
        .find(|e| e.kind == EventKind::TurnEnded)
        .map(|e| e.seq);

    // In-turn eviction: what each call is, what its result is, and the
    // last result of each distinct tool per turn, so the second pass can
    // decide what the `context_evicted` events stub.
    let mut evictions: Vec<Eviction> = Vec::new();
    let mut calls: std::collections::HashMap<String, (String, serde_json::Value)> =
        std::collections::HashMap::new();
    let mut results: std::collections::HashMap<String, CallResult> =
        std::collections::HashMap::new();
    let mut last_of_tool: std::collections::HashMap<(Option<u64>, String), u64> =
        std::collections::HashMap::new();
    let mut turn: Option<u64> = None;
    let mut stubbed: std::collections::HashSet<String> = std::collections::HashSet::new();

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
            EventKind::TurnEnded => {
                turn = Some(event.seq);
            }
            EventKind::ContextEvicted => {
                let p: ContextEvictedPayload = payload(event)?;
                evictions.push(Eviction {
                    turn,
                    through: p.through_seq,
                });
            }
            EventKind::AssistantMessage => {
                let p: AssistantMessagePayload = payload(event)?;
                for block in &p.blocks {
                    if let ContentBlock::ToolCall(c) = block {
                        calls.insert(c.id.clone(), (c.name.clone(), c.args.clone()));
                    }
                }
            }
            EventKind::ToolResult => {
                let p: ToolResultPayload = payload(event)?;
                let Some((name, _)) = calls.get(&p.result.id) else {
                    continue;
                };
                let is_edit = name == "edit_file" || name == "write_file";
                results.insert(
                    p.result.id.clone(),
                    CallResult {
                        seq: event.seq,
                        turn,
                        is_error: p.result.is_error,
                        diff: (!p.result.is_error && is_edit)
                            .then(|| diff_lines(&p.result.content)),
                    },
                );
                last_of_tool.insert((turn, name.clone()), event.seq);
            }
            _ => {}
        }
    }
    for (id, r) in &results {
        if r.is_error {
            continue; // a failure is information; it stays
        }
        let Some((name, _)) = calls.get(id) else {
            continue;
        };
        // The last result of each distinct tool stays, however old.
        if last_of_tool.get(&(r.turn, name.clone())) == Some(&r.seq) {
            continue;
        }
        if evictions
            .iter()
            .any(|e| e.turn == r.turn && r.seq <= e.through)
        {
            stubbed.insert(id.clone());
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
                // Steered (issue #33): emitted where it sits in the log,
                // after the hold-back; the very next model call sees it.
                // Without `steer` (every log from before steering), the
                // horizon rule: it waits for the next turn.
                if p.mid_turn && !p.steer && last_turn_end.is_none_or(|end| event.seq >= end) {
                    continue;
                }
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
                // A provider blob (a model's reasoning) is replayed only
                // while it can matter: in the open turn, for a message
                // whose calls are not stubbed. Reasoning from a finished
                // turn, or behind the eviction boundary, is dropped; it
                // was 61% of a build thread's context (issue #35). No
                // provider needs it back: Anthropic keeps only the last
                // assistant message's thinking in a tool loop, and
                // OpenAI-compatible reasoning is informational.
                let settled = last_turn_end.is_some_and(|end| event.seq < end);
                let mut call_ids = p.blocks.iter().filter_map(|b| match b {
                    ContentBlock::ToolCall(c) => Some(&c.id),
                    _ => None,
                });
                let behind_boundary =
                    call_ids.clone().next().is_some() && call_ids.all(|id| stubbed.contains(id));
                let drop_blobs = settled
                    || behind_boundary
                    || blobs_dropped_before.is_some_and(|at| event.seq < at);
                let blocks = p
                    .blocks
                    .into_iter()
                    .map(|b| match &b {
                        // A successful edit whose result is stubbed loses
                        // its old/new strings: the diff shape is enough,
                        // and the file can be re-read.
                        ContentBlock::ToolCall(c)
                            if stubbed.contains(&c.id)
                                && (c.name == "edit_file" || c.name == "write_file") =>
                        {
                            let diff = results.get(&c.id).and_then(|r| r.diff).unwrap_or((0, 0));
                            stub_call_args(c, diff)
                        }
                        _ => b,
                    })
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
                if stubbed.contains(&result.id) {
                    if let Some((name, args)) = calls.get(&result.id) {
                        result.content = result_stub(name, args, &result.content);
                    }
                } else if let Some(max) = truncations
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
            EventKind::ProjectSwitched => {
                let p: crate::ProjectSwitchedPayload = payload(event)?;
                let name = |n: &Option<String>| n.clone().unwrap_or_else(|| "no project".into());
                let workspace = p
                    .workspace
                    .as_ref()
                    .map(|w| format!(" in workspace {w}"))
                    .unwrap_or_default();
                push(
                    &mut body,
                    &mut pending_calls,
                    &mut held,
                    Message {
                        role: Role::User,
                        author: Author::System,
                        blocks: vec![ContentBlock::Text(format!(
                            "[The thread moved from project {} to project {}{workspace}. {}'s instructions, knowledge and files no longer apply; the working directory is now {}.]",
                            name(&p.from),
                            name(&p.to),
                            name(&p.from),
                            p.root.display()
                        ))],
                    },
                );
            }
            EventKind::Interrupted => {
                let p: InterruptedPayload = payload(event)?;
                let who = match &p.by {
                    Some(Author::User(u)) => format!(" by {}", u.0),
                    Some(Author::Agent(a)) => format!(" by {}", a.0),
                    Some(Author::System) | None => String::new(),
                };
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
                            "[The previous turn was interrupted{who}: {}.{calls} Continue from here; rerun anything whose outcome is unknown.]",
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
            | EventKind::ContextEvicted
            | EventKind::PermissionRequested
            | EventKind::PermissionDecided
            | EventKind::MemoryExtracted
            | EventKind::MemoryRemembered
            | EventKind::ThreadStarted
            | EventKind::ThreadRenamed
            // A retry is a UI fact (issue #31), not context: the model
            // is told nothing about transport trouble.
            | EventKind::ProviderRetried => {}
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

/// The stub an evicted result projects to: what ran, roughly with what,
/// how big it was, and how to get it back (issue #30).
fn result_stub(name: &str, args: &serde_json::Value, content: &str) -> String {
    format!(
        "[result of {name} {} · {} lines · dropped from context; re-run it if you need it again]",
        short_args(args),
        content.lines().count()
    )
}

/// A call's arguments as one short line: enough to tell two calls of the
/// same tool apart, never the payload itself.
fn short_args(args: &serde_json::Value) -> String {
    let text = serde_json::to_string(args).unwrap_or_default();
    if text.chars().count() <= 60 {
        text
    } else {
        format!("{}…", text.chars().take(59).collect::<String>())
    }
}

/// A successful edit/write call whose result is stubbed: the arguments
/// become the diff shape, read off the result before it was stubbed.
fn stub_call_args(call: &aigentic_core::ToolCall, diff: (u64, u64)) -> ContentBlock {
    let path = call
        .args
        .get("path")
        .and_then(|p| p.as_str())
        .unwrap_or_default();
    let (removed, added) = diff;
    let stub = format!("[edited {path}: -{removed}/+{added} lines]");
    ContentBlock::ToolCall(aigentic_core::ToolCall {
        id: call.id.clone(),
        name: call.name.clone(),
        args: serde_json::json!({ "evicted": stub }),
    })
}

/// The `-removed/+added` line counts of a unified diff, headers excluded.
fn diff_lines(content: &str) -> (u64, u64) {
    let (mut removed, mut added) = (0, 0);
    for line in content.lines() {
        if line.starts_with("---") || line.starts_with("+++") {
            continue;
        }
        match line.as_bytes().first() {
            Some(b'-') => removed += 1,
            Some(b'+') => added += 1,
            _ => {}
        }
    }
    (removed, added)
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
                usage: Box::new(Usage::reported(aigentic_core::Usage::default())),
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
    fn call(seq: u64, id: &str, name: &str, args: serde_json::Value) -> Event {
        ev(
            seq,
            EventKind::AssistantMessage,
            agent(),
            serde_json::to_value(AssistantMessagePayload {
                blocks: vec![ContentBlock::ToolCall(ToolCall {
                    id: id.into(),
                    name: name.into(),
                    args,
                })],
                usage: None,
            })
            .unwrap(),
        )
    }
    fn failed(seq: u64, id: &str, content: &str) -> Event {
        ev(
            seq,
            EventKind::ToolResult,
            Author::System,
            json!({"id": id, "content": content, "is_error": true}),
        )
    }
    fn evicted(seq: u64, through: u64) -> Event {
        ev(
            seq,
            EventKind::ContextEvicted,
            Author::System,
            json!({"through_seq": through}),
        )
    }
    /// Every tool result's content, oldest first, as `name:content`.
    fn results(p: &Projection) -> Vec<(String, String)> {
        p.body
            .iter()
            .flat_map(|m| m.blocks.iter())
            .filter_map(|b| match b {
                ContentBlock::ToolResult(r) => Some((r.id.clone(), r.content.clone())),
                _ => None,
            })
            .collect()
    }
    /// The arguments of every tool call, oldest first.
    fn call_args(p: &Projection) -> Vec<serde_json::Value> {
        p.body
            .iter()
            .flat_map(|m| m.blocks.iter())
            .filter_map(|b| match b {
                ContentBlock::ToolCall(c) => Some(c.args.clone()),
                _ => None,
            })
            .collect()
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

    fn queued(seq: u64, text: &str) -> Event {
        ev(
            seq,
            EventKind::UserMessage,
            Author::User(UserId("magnus".into())),
            json!({"blocks": [{"type": "text", "text": text}], "mid_turn": true}),
        )
    }

    /// A queued message from a thread that steers (issue #33).
    fn steered(seq: u64, text: &str) -> Event {
        ev(
            seq,
            EventKind::UserMessage,
            Author::User(UserId("magnus".into())),
            json!({"blocks": [{"type": "text", "text": text}], "mid_turn": true, "steer": true}),
        )
    }

    /// An assistant message with two calls, both results pending.
    fn two_calls(seq: u64) -> Event {
        ev(
            seq,
            EventKind::AssistantMessage,
            agent(),
            serde_json::to_value(AssistantMessagePayload {
                blocks: vec![
                    ContentBlock::ToolCall(ToolCall {
                        id: "c1".into(),
                        name: "bash".into(),
                        args: json!({"command": "ls"}),
                    }),
                    ContentBlock::ToolCall(ToolCall {
                        id: "c2".into(),
                        name: "bash".into(),
                        args: json!({"command": "pwd"}),
                    }),
                ],
                usage: None,
            })
            .unwrap(),
        )
    }

    #[test]
    fn a_mid_turn_message_waits_for_the_next_turn() {
        // Turn one runs; magnus posts while it does; the turn ends.
        let mut events = vec![
            ev(
                0,
                EventKind::ThreadStarted,
                steve(),
                json!({"project": "p", "root": "/r", "created_by": {"kind": "user", "id": "steve"}}),
            ),
            user(1, "one"),
            assistant(2, "reply one", false),
            queued(3, "also this"),
        ];
        // No turn_ended yet: the queued message is not in context.
        let p = project(&events).unwrap();
        assert_eq!(
            texts(&p),
            vec!["one", "reply one"],
            "thread_started emits nothing too"
        );
        // Even after a resume note, still the same turn.
        events.push(ev(
            4,
            EventKind::Interrupted,
            Author::System,
            json!({"reason": "process exited mid-turn", "after_seq": 3, "unanswered_calls": []}),
        ));
        let p = project(&events).unwrap();
        assert_eq!(texts(&p).len(), 3);
        assert!(!texts(&p).iter().any(|t| t == "also this"));
        // The turn ends: the message is in context, in log order.
        events.push(ended(5));
        let p = project(&events).unwrap();
        let t = texts(&p);
        assert_eq!(t[0], "one");
        assert_eq!(t[1], "reply one");
        assert_eq!(t[2], "also this");
        assert_eq!(p.body[2].author, Author::User(UserId("magnus".into())));
        // A message with the flag off is never held, as before phase 5.
        events.push(user(6, "two"));
        let p = project(&events).unwrap();
        assert_eq!(texts(&p).last().map(String::as_str), Some("two"));
    }

    #[test]
    fn a_steered_message_appears_only_after_the_last_pending_result() {
        // magnus posts while both of an assistant message's results are
        // still pending. The hold-back keeps the pair intact: the
        // message lands after the last of them, never between.
        let events = vec![
            user(0, "one"),
            two_calls(1),
            steered(2, "use the other file"),
            result(3, "c1", "r1"),
            result(4, "c2", "r2"),
        ];
        let p = project(&events).unwrap();
        assert_eq!(
            results(&p),
            vec![("c1".into(), "r1".into()), ("c2".into(), "r2".into())]
        );
        assert_eq!(
            texts(&p).last().map(String::as_str),
            Some("use the other file")
        );
        assert_eq!(
            p.body.last().unwrap().author,
            Author::User(UserId("magnus".into()))
        );
    }

    #[test]
    fn a_steered_message_after_the_last_result_lands_in_place() {
        // Posted between a tool result and the next model call: exactly
        // where it sits in the log, no `turn_ended` needed.
        let events = vec![
            user(0, "one"),
            call(1, "c1", "bash", json!({"command": "ls"})),
            result(2, "c1", "r1"),
            steered(3, "use the other file"),
            ended(4),
        ];
        let p = project(&events).unwrap();
        assert_eq!(p.body.len(), 4);
        assert_eq!(p.body.last().unwrap().role, Role::User);
        assert_eq!(
            texts(&p).last().map(String::as_str),
            Some("use the other file")
        );
    }

    #[test]
    fn an_interrupt_names_who_did_it() {
        let events = vec![
            user(0, "one"),
            ev(
                1,
                EventKind::Interrupted,
                Author::System,
                json!({"reason": "interrupt", "after_seq": 0, "by": {"kind": "user", "id": "magnus"}}),
            ),
        ];
        let p = project(&events).unwrap();
        let note = texts(&p)[1].clone();
        assert!(
            note.starts_with("[The previous turn was interrupted by magnus: interrupt."),
            "{note}"
        );
    }

    #[test]
    fn phase0_shape_is_unchanged_without_compaction() {
        let p = project(&two_turns()).unwrap();
        assert_eq!(texts(&p), vec!["one", "reply one", "two", "reply two"]);
        assert_eq!(
            p.body[1].blocks.len(),
            1,
            "a finished turn's blob is dropped even with no summary (#35)"
        );
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
    fn reasoning_from_a_finished_turn_is_not_replayed() {
        let events = vec![
            user(1, "one"),
            assistant(2, "reply one", true),
            ended(3),
            user(4, "two"),
            assistant(5, "reply two", true),
        ];
        let p = project(&events).unwrap();
        assert_eq!(p.body[1].blocks.len(), 1, "finished turn: blob dropped");
        assert_eq!(p.body[3].blocks.len(), 2, "open turn: blob kept");
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

    #[test]
    fn an_old_log_without_context_evicted_replays_unchanged() {
        // Recorded before in-turn eviction: closed turns and an open one
        // alike project with every result in full, however many calls
        // the turn holds.
        let long = "x".repeat(500);
        let events = vec![
            user(0, "build it"),
            call(1, "c1", "bash", json!({"command": "cargo test"})),
            result(2, "c1", &long),
            call(3, "c2", "bash", json!({"command": "cargo build"})),
            result(4, "c2", &long),
            ended(5),
            user(6, "and keep going"),
            call(7, "c3", "read_file", json!({"path": "a.rs"})),
            result(8, "c3", &long),
        ];
        let p = project(&events).unwrap();
        assert_eq!(
            results(&p),
            vec![
                ("c1".into(), long.clone()),
                ("c2".into(), long.clone()),
                ("c3".into(), long.clone())
            ],
            "no event, no stub, open turn included"
        );
    }

    #[test]
    fn reasoning_behind_the_eviction_boundary_is_not_replayed() {
        let thinking = |seq: u64, id: &str, command: &str| {
            ev(
                seq,
                EventKind::AssistantMessage,
                agent(),
                json!({"blocks": [
                    {"type": "tool_call", "id": id, "name": "bash", "args": {"command": command}},
                    {"type": "provider_blob", "provider": "openai_compat", "data": {"reasoning_content": "long thoughts"}},
                ]}),
            )
        };
        let long = "line\n".repeat(200);
        let events = vec![
            user(0, "build it"),
            thinking(1, "c1", "cargo test"),
            result(2, "c1", &long),
            thinking(3, "c2", "cargo test"),
            result(4, "c2", &long),
            evicted(5, 2),
        ];
        let p = project(&events).unwrap();
        let blobs = |i: usize| {
            p.body[i]
                .blocks
                .iter()
                .filter(|b| matches!(b, ContentBlock::ProviderBlob(_)))
                .count()
        };
        assert_eq!(blobs(1), 0, "behind the boundary: reasoning dropped");
        assert_eq!(blobs(3), 1, "the latest call keeps its reasoning");
    }

    #[test]
    fn evicted_results_stub_but_failures_and_the_last_per_tool_stay() {
        let long = "line\n".repeat(200);
        let events = vec![
            user(0, "build it"),
            call(1, "c1", "bash", json!({"command": "cargo test"})),
            result(2, "c1", &long),
            call(3, "c2", "read_file", json!({"path": "a.rs"})),
            result(4, "c2", &long),
            call(5, "c3", "bash", json!({"command": "cargo test"})),
            result(6, "c3", &long),
            call(7, "c4", "grep", json!({"pattern": "x"})),
            failed(8, "c4", "no matches"),
            call(
                9,
                "c5",
                "write_file",
                json!({"path": "src/new.rs", "content": "one\ntwo\n"}),
            ),
            result(
                10,
                "c5",
                "--- a/src/new.rs\n+++ b/src/new.rs\n@@ -0,0 +1,2 @@\n+one\n+two\nwrote 8 bytes to src/new.rs",
            ),
            call(
                11,
                "c7",
                "write_file",
                json!({"path": "src/other.rs", "content": "x\n"}),
            ),
            result(
                12,
                "c7",
                "--- a/src/other.rs\n+++ b/src/other.rs\n@@ -0,0 +1,1 @@\n+x\nwrote 2 bytes to src/other.rs",
            ),
            evicted(13, 12),
            call(
                14,
                "c6",
                "edit_file",
                json!({"path": "src/new.rs", "old_string": "one", "new_string": "ONE"}),
            ),
            result(15, "c6", "edited src/new.rs at line 1"),
        ];
        let p = project(&events).unwrap();
        assert_eq!(
            results(&p),
            vec![
                (
                    "c1".into(),
                    "[result of bash {\"command\":\"cargo test\"} · 200 lines · dropped from context; re-run it if you need it again]".into()
                ),
                ("c2".into(), long.clone()), // last read_file of the turn
                ("c3".into(), long),         // last bash result of the turn
                ("c4".into(), "no matches".into()), // failures stay
                (
                    "c5".into(),
                    "[result of write_file {\"content\":\"one\\ntwo\\n\",\"path\":\"src/new.rs\"} · 6 lines · dropped from context; re-run it if you need it again]".into()
                ),
                // The last write_file result stays, in range though it is.
                (
                    "c7".into(),
                    "--- a/src/other.rs\n+++ b/src/other.rs\n@@ -0,0 +1,1 @@\n+x\nwrote 2 bytes to src/other.rs".into()
                ),
                ("c6".into(), "edited src/new.rs at line 1".into()), // after the boundary
            ]
        );
        // The tool contract holds: role and author unchanged, ids intact.
        assert!(p.body.iter().any(|m| m.role == Role::Tool
            && m.author == Author::System
            && matches!(&m.blocks[0], ContentBlock::ToolResult(r) if r.id == "c1" && !r.is_error)));
        // Successful edit/write arguments stub with the diff shape; the
        // last of a tool and the call after the boundary keep theirs.
        assert_eq!(
            call_args(&p),
            vec![
                json!({"command": "cargo test"}),
                json!({"path": "a.rs"}),
                json!({"command": "cargo test"}),
                json!({"pattern": "x"}),
                json!({"evicted": "[edited src/new.rs: -0/+2 lines]"}),
                json!({"path": "src/other.rs", "content": "x\n"}),
                json!({"path": "src/new.rs", "old_string": "one", "new_string": "ONE"}),
            ]
        );
    }

    #[test]
    fn a_failed_edit_keeps_its_arguments_and_only_its_own_turn_is_evicted() {
        let long = "y".repeat(300);
        let events = vec![
            user(0, "one"),
            call(1, "c1", "bash", json!({"command": "ls"})),
            result(2, "c1", &long),
            ended(3),
            user(4, "two"),
            call(
                5,
                "c2",
                "edit_file",
                json!({"path": "a.rs", "old_string": "x", "new_string": "y"}),
            ),
            failed(6, "c2", "a.rs: old_string not found"),
            call(7, "c3", "bash", json!({"command": "ls"})),
            result(8, "c3", &long),
            evicted(9, 6),
        ];
        let p = project(&events).unwrap();
        // The closed turn's result is untouched: the sweep only ever
        // records its own turn.
        assert_eq!(
            results(&p),
            vec![
                ("c1".into(), long.clone()),
                ("c2".into(), "a.rs: old_string not found".into()),
                ("c3".into(), long), // last bash result of the open turn
            ]
        );
        // A failed edit's arguments stay: the rule is successful edits.
        assert_eq!(
            call_args(&p)[1],
            json!({"path": "a.rs", "old_string": "x", "new_string": "y"})
        );
    }

    #[test]
    fn long_result_args_are_shortened_in_the_stub() {
        let events = vec![
            user(0, "go"),
            call(
                1,
                "c1",
                "write_file",
                json!({"path": "big.rs", "content": "z".repeat(400)}),
            ),
            result(2, "c1", "wrote 400 bytes to big.rs"),
            evicted(3, 2),
            call(
                4,
                "c2",
                "write_file",
                json!({"path": "z.rs", "content": "q"}),
            ),
            result(5, "c2", "wrote 1 byte to z.rs"),
        ];
        let p = project(&events).unwrap();
        let (_, stub) = results(&p)[0].clone();
        assert!(
            stub.starts_with("[result of write_file {\"content\":\"zz"),
            "{stub}"
        );
        assert!(stub.contains("…"), "the arguments are cut short: {stub}");
        assert!(
            stub.ends_with(" · dropped from context; re-run it if you need it again]"),
            "{stub}"
        );
        assert!(stub.len() < 200, "{stub}");
    }
}
