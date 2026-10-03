//! Bringing things back from the log (issue #75): the lookups a stub, a
//! truncation marker and the harness `recall` tool all share.
//!
//! Everything here is a pure function over a thread's events: no I/O, no
//! clock, no mutation, so it is as replayable as the projection is. The
//! call index and [`short_args`] live here because the projection, the
//! stub text and the tool all need the same answer to "what was this
//! call?".

use std::collections::HashMap;

use aigentic_core::{Author, ContentBlock, Event, EventKind};

use crate::payload::{
    AssistantMessagePayload, CompactedPayload, CompactionStrategy, ProjectSwitchedPayload,
    ThreadStartedPayload, ToolResultPayload, UserMessagePayload,
};
use crate::store::LogError;

/// The harness tool whose results `search` never returns: a query would
/// otherwise keep finding its own echoes, and every echo is another
/// result for the sweep to price.
const RECALL_TOOL: &str = "recall";

/// How many characters of a matching line a [`Hit`] carries.
pub const SNIPPET_MAX_CHARS: usize = 160;

/// Every `tool_call` block in the log, keyed by id: the call's name and
/// its arguments. The last block with an id wins, which is what the
/// projection's in-line map did before issue #75.
///
/// A malformed assistant payload is an error, not a silent omission, so
/// the projection keeps failing on it exactly as it did.
pub fn call_index(
    events: &[Event],
) -> Result<HashMap<String, (String, serde_json::Value)>, LogError> {
    let mut calls = HashMap::new();
    for event in events {
        if event.kind != EventKind::AssistantMessage {
            continue;
        }
        let p: AssistantMessagePayload =
            serde_json::from_value(event.payload.clone()).map_err(|source| LogError::Payload {
                seq: event.seq,
                kind: event.kind,
                source,
            })?;
        for block in &p.blocks {
            if let ContentBlock::ToolCall(c) = block {
                calls.insert(c.id.clone(), (c.name.clone(), c.args.clone()));
            }
        }
    }
    Ok(calls)
}

/// The name and arguments of the `tool_call` block with that id, if the
/// log holds one.
pub fn call_of(events: &[Event], call_id: &str) -> Option<(String, serde_json::Value)> {
    call_index(events).ok()?.remove(call_id)
}

/// A call's arguments as one short line: enough to tell two calls of the
/// same tool apart, never the payload itself. Cut only when longer than
/// 60 chars, to the first 59 plus `…`.
pub fn short_args(args: &serde_json::Value) -> String {
    let text = serde_json::to_string(args).unwrap_or_default();
    if text.chars().count() <= 60 {
        text
    } else {
        format!("{}…", text.chars().take(59).collect::<String>())
    }
}

/// The project a seq belongs to: the `to` of the last `project_switched`
/// at or before it — **even when that `to` is `None`**, the thread having
/// left its project — else `thread_started`'s project, else `None`.
///
/// `None` therefore means "outside any project", both for a thread that
/// never had one and for a switch that left one.
pub fn project_at(events: &[Event], seq: u64) -> Option<String> {
    if let Some(p) = events
        .iter()
        .filter(|e| e.kind == EventKind::ProjectSwitched && e.seq <= seq)
        .max_by_key(|e| e.seq)
        .and_then(payload_of::<ProjectSwitchedPayload>)
    {
        return p.to;
    }
    events
        .iter()
        .find(|e| e.kind == EventKind::ThreadStarted)
        .and_then(payload_of::<ThreadStartedPayload>)
        .and_then(|p| p.project)
}

/// One search result: where it is, what it is, and the line that matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub seq: u64,
    pub kind: EventKind,
    pub snippet: String,
}

/// Search the thread's own words: user-message text, assistant text and
/// tool-result content. Case-insensitive; every whitespace-separated
/// term must appear in the text, though not necessarily on one line.
/// Newest first, `limit` hits at most (hits, not bytes). A tool result
/// whose call is `recall` is never a hit. A query of no terms matches
/// nothing.
///
/// The whole thread is scanned on every call; that is the trade the tool
/// makes, and tens of thousands of events are still milliseconds.
pub fn search(events: &[Event], query: &str, limit: usize) -> Vec<Hit> {
    let terms: Vec<String> = query.split_whitespace().map(|t| t.to_lowercase()).collect();
    if terms.is_empty() {
        return Vec::new();
    }
    let calls = call_index(events).unwrap_or_default();
    let mut hits = Vec::new();
    for event in events.iter().rev() {
        if hits.len() >= limit {
            break;
        }
        let Some(text) = searchable_text(event, &calls) else {
            continue;
        };
        let lower = text.to_lowercase();
        if !terms.iter().all(|t| lower.contains(t.as_str())) {
            continue;
        }
        let Some(snippet) = matching_line(&text, &terms) else {
            continue;
        };
        hits.push(Hit {
            seq: event.seq,
            kind: event.kind,
            snippet,
        });
    }
    hits
}

/// The text a hit may come from, or `None` when the event holds none or
/// is a `recall` result.
fn searchable_text(
    event: &Event,
    calls: &HashMap<String, (String, serde_json::Value)>,
) -> Option<String> {
    match event.kind {
        EventKind::UserMessage => Some(text_of(&payload_of::<UserMessagePayload>(event)?.blocks)),
        EventKind::AssistantMessage => Some(text_of(
            &payload_of::<AssistantMessagePayload>(event)?.blocks,
        )),
        EventKind::ToolResult => {
            let p = payload_of::<ToolResultPayload>(event)?;
            if calls
                .get(&p.result.id)
                .is_some_and(|(name, _)| name == RECALL_TOOL)
            {
                return None;
            }
            Some(p.result.content)
        }
        _ => None,
    }
}

/// The line to quote: the first that holds every term where one does,
/// else the first that holds any. Trimmed and cut to
/// [`SNIPPET_MAX_CHARS`] chars plus `…` when longer.
fn matching_line(text: &str, terms: &[String]) -> Option<String> {
    let holds = |line: &str, all: bool| {
        let lower = line.to_lowercase();
        if all {
            terms.iter().all(|t| lower.contains(t.as_str()))
        } else {
            terms.iter().any(|t| lower.contains(t.as_str()))
        }
    };
    let line = text
        .lines()
        .find(|l| holds(l, true))
        .or_else(|| text.lines().find(|l| holds(l, false)))?;
    Some(cut_chars(line.trim(), SNIPPET_MAX_CHARS))
}

/// A range of the log as text: one block per event, `{seq} · {kind} ·
/// {author}` and then the event's own words. It reads the log, never the
/// projection, so a range across a `compacted` event shows the originals
/// and the summary that replaced them.
///
/// At most `max_events` blocks, oldest first; when the range holds more,
/// a last line says how many were not shown.
pub fn render_range(events: &[Event], from: u64, to: u64, max_events: usize) -> String {
    let range: Vec<&Event> = events
        .iter()
        .filter(|e| from <= e.seq && e.seq <= to)
        .collect();
    let shown = range.len().min(max_events);
    let mut blocks: Vec<String> = range[..shown].iter().map(|e| render_event(e)).collect();
    if range.len() > shown {
        blocks.push(format!(
            "({} more events in this range not shown)",
            range.len() - shown
        ));
    }
    blocks.join("\n\n")
}

/// One event's block: its header, then the words its kind carries.
fn render_event(event: &Event) -> String {
    let header = format!(
        "{} · {} · {}",
        event.seq,
        kind_name(event.kind),
        author_label(&event.author)
    );
    let mut parts: Vec<String> = Vec::new();
    match event.kind {
        EventKind::UserMessage => {
            if let Some(p) = payload_of::<UserMessagePayload>(event) {
                parts.push(text_of(&p.blocks));
            }
        }
        EventKind::AssistantMessage => {
            if let Some(p) = payload_of::<AssistantMessagePayload>(event) {
                parts.push(text_of(&p.blocks));
                for block in &p.blocks {
                    if let ContentBlock::ToolCall(c) = block {
                        parts.push(format!("→ {} {}", c.name, short_args(&c.args)));
                    }
                }
            }
        }
        EventKind::ToolResult => {
            if let Some(p) = payload_of::<ToolResultPayload>(event) {
                parts.push(p.result.content);
            }
        }
        EventKind::Compacted => {
            if let Some(p) = payload_of::<CompactedPayload>(event)
                && let CompactionStrategy::Summary { text, .. } = p.strategy
            {
                parts.push(format!("summary:\n{text}"));
            }
        }
        EventKind::ProjectSwitched => {
            if let Some(p) = payload_of::<ProjectSwitchedPayload>(event) {
                parts.push(match p.to {
                    Some(to) => format!("moved to {to}"),
                    None => "moved outside any project".into(),
                });
            }
        }
        _ => {}
    }
    parts.retain(|p| !p.is_empty());
    if parts.is_empty() {
        header
    } else {
        format!("{header}\n{}", parts.join("\n"))
    }
}

/// An event kind's serde name, `snake_case`: the label `render_range`
/// heads a block with, and the one `recall` names a non-result seq by.
pub fn kind_name(kind: EventKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// The same label the rest of the harness shows a speaker by.
fn author_label(author: &Author) -> String {
    match author {
        Author::User(id) => id.0.clone(),
        Author::Agent(id) => id.0.clone(),
        Author::System => "system".into(),
    }
}

/// The text blocks of a message, joined by newlines; images, calls,
/// results and provider blobs carry none.
fn text_of(blocks: &[ContentBlock]) -> String {
    let texts: Vec<&str> = blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    texts.join("\n")
}

/// `text` cut to `max` chars plus `…` when it is longer. Shared with the
/// projection's stub excerpt.
pub(crate) fn cut_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_owned()
    } else {
        format!("{}…", text.chars().take(max).collect::<String>())
    }
}

fn payload_of<T: serde::de::DeserializeOwned>(event: &Event) -> Option<T> {
    serde_json::from_value(event.payload.clone()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aigentic_core::{AgentId, ToolCall, UserId};
    use serde_json::json;
    use time::macros::datetime;
    use ulid::Ulid;

    use crate::payload::{CompactedPayload, CompactionStrategy};

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
    fn user(seq: u64, text: &str) -> Event {
        ev(
            seq,
            EventKind::UserMessage,
            Author::User(UserId("steve".into())),
            json!({"blocks": [{"type": "text", "text": text}]}),
        )
    }
    fn assistant(seq: u64, text: &str) -> Event {
        ev(
            seq,
            EventKind::AssistantMessage,
            Author::Agent(AgentId("worker".into())),
            json!({"blocks": [{"type": "text", "text": text}]}),
        )
    }
    fn call(seq: u64, id: &str, name: &str, args: serde_json::Value) -> Event {
        ev(
            seq,
            EventKind::AssistantMessage,
            Author::Agent(AgentId("worker".into())),
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
    fn result(seq: u64, id: &str, content: &str) -> Event {
        ev(
            seq,
            EventKind::ToolResult,
            Author::System,
            json!({"id": id, "content": content, "is_error": false}),
        )
    }
    fn started(seq: u64, project: Option<&str>) -> Event {
        let author = Author::User(UserId("steve".into()));
        let p = ThreadStartedPayload {
            project: project.map(str::to_owned),
            root: "/tmp/root".into(),
            created_by: author.clone(),
            parent_thread: None,
            step: None,
        };
        ev(
            seq,
            EventKind::ThreadStarted,
            author,
            serde_json::to_value(p).unwrap(),
        )
    }
    fn switched(seq: u64, from: Option<&str>, to: Option<&str>) -> Event {
        let p = ProjectSwitchedPayload {
            from: from.map(str::to_owned),
            to: to.map(str::to_owned),
            root: "/tmp/root".into(),
            workspace: None,
        };
        ev(
            seq,
            EventKind::ProjectSwitched,
            Author::System,
            serde_json::to_value(p).unwrap(),
        )
    }
    fn summary(seq: u64, from: u64, to: u64, text: &str) -> Event {
        let p = CompactedPayload {
            from_seq: from,
            to_seq: to,
            strategy: CompactionStrategy::Summary {
                text: text.into(),
                model: "m".into(),
                usage: Box::new(crate::payload::Usage::reported(
                    aigentic_core::Usage::default(),
                )),
            },
        };
        ev(
            seq,
            EventKind::Compacted,
            Author::Agent(AgentId("worker".into())),
            serde_json::to_value(p).unwrap(),
        )
    }

    /// T5 (issue #75): the project a seq belongs to.
    #[test]
    fn project_at_reads_the_latest_switch_at_or_before_the_seq() {
        let events = vec![
            started(0, Some("alpha")),
            user(1, "hello"),
            switched(2, Some("alpha"), Some("beta")),
            user(3, "there"),
            switched(4, Some("beta"), None),
            user(5, "outside"),
        ];
        // Before any switch: the thread's own project.
        assert_eq!(project_at(&events, 1).as_deref(), Some("alpha"));
        // At a switch's own seq: the new project, not the old one.
        assert_eq!(project_at(&events, 2).as_deref(), Some("beta"));
        assert_eq!(project_at(&events, 3).as_deref(), Some("beta"));
        // A later switch to None wins over the earlier project.
        assert_eq!(project_at(&events, 4), None);
        assert_eq!(project_at(&events, 5), None);
        assert_eq!(project_at(&events, 0).as_deref(), Some("alpha"));
    }

    #[test]
    fn project_at_without_a_thread_start_or_a_switch_is_none() {
        let events = vec![user(1, "hello")];
        assert_eq!(project_at(&events, 1), None);
        let outside = vec![started(0, None), switched(1, None, None)];
        assert_eq!(project_at(&outside, 1), None);
    }

    /// T6 (issue #75): search's rules, each on the same fixture.
    fn search_fixture() -> Vec<Event> {
        vec![
            user(0, "Deploy the STAGING host please"),
            assistant(1, "I will check the staging deploy script"),
            call(2, "c1", "bash", json!({"command": "grep deploy"})),
            result(3, "c1", "deploy.sh:12: echo staging\ndeploy.sh:13: exit 0"),
            call(4, "c2", "recall", json!({"query": "staging deploy"})),
            result(
                5,
                "c2",
                "[recalled result 3 · bash] deploy.sh:12: echo staging",
            ),
            call(6, "c3", "bash", json!({"command": "ls"})),
            result(7, "c3", "no such word here"),
        ]
    }

    #[test]
    fn search_is_case_insensitive_and_needs_every_term() {
        let events = search_fixture();
        let hits = search(&events, "STAGING deploy", 10);
        // The user line, the assistant line and the grep result hold both
        // terms; the record of the earlier recall does not count.
        assert_eq!(
            hits.iter().map(|h| h.seq).collect::<Vec<_>>(),
            vec![3, 1, 0],
            "newest first, and the recall result is skipped"
        );
        assert_eq!(hits[0].kind, EventKind::ToolResult);
        assert_eq!(hits[1].kind, EventKind::AssistantMessage);
        assert_eq!(hits[2].kind, EventKind::UserMessage);
        // Every term is required: a term no line holds matches nothing.
        assert!(search(&events, "staging absent", 10).is_empty());
    }

    #[test]
    fn search_skips_a_recall_result_even_when_its_text_matches() {
        let events = vec![
            call(0, "c1", "bash", json!({"command": "ls"})),
            result(1, "c1", "only place the word zanzibar appears"),
            call(2, "c2", "recall", json!({"handle": 1})),
            result(3, "c2", "only place the word zanzibar appears"),
        ];
        let hits = search(&events, "zanzibar", 10);
        assert_eq!(
            hits.iter().map(|h| h.seq).collect::<Vec<_>>(),
            vec![1],
            "the recall echo is never a hit, the original is"
        );
    }

    #[test]
    fn search_is_capped_at_a_number_of_hits_not_bytes() {
        let events: Vec<Event> = (0..8).map(|i| user(i, "needle here")).collect();
        let hits = search(&events, "needle", 3);
        assert_eq!(hits.len(), 3, "limit counts hits");
        assert_eq!(
            hits.iter().map(|h| h.seq).collect::<Vec<_>>(),
            vec![7, 6, 5],
            "newest first, and the cap drops the oldest"
        );
    }

    #[test]
    fn a_snippet_is_the_matching_line_trimmed_and_cut_at_160() {
        let long = format!("   {}   ", "y".repeat(200));
        let events = vec![user(0, &format!("intro\n{long}\noutro"))];
        let hits = search(&events, "yy", 10);
        let snippet = &hits[0].snippet;
        assert_eq!(snippet.chars().count(), SNIPPET_MAX_CHARS + 1, "{snippet}");
        assert!(snippet.ends_with('…'), "{snippet}");
        assert_eq!(snippet.chars().take(SNIPPET_MAX_CHARS).count(), 160);
        assert!(snippet.starts_with('y'), "trimmed: {snippet}");
        // A short line comes back whole and trimmed.
        let short = vec![user(0, "  a needle  ")];
        assert_eq!(search(&short, "needle", 10)[0].snippet, "a needle");
    }

    /// T7 (issue #75): a range of the log as text.
    #[test]
    fn render_range_heads_every_event_with_seq_kind_and_author() {
        let events = vec![
            user(0, "do it"),
            assistant(1, "on it"),
            call(2, "c1", "bash", json!({"command": "ls"})),
            result(3, "c1", "a.rs\nb.rs"),
            summary(4, 0, 3, "the thread so far"),
            switched(5, Some("alpha"), Some("beta")),
            switched(6, Some("beta"), None),
        ];
        assert_eq!(
            render_range(&events, 0, 1, 50),
            "0 · user_message · steve\ndo it\n\n1 · assistant_message · worker\non it"
        );
        assert_eq!(
            render_range(&events, 2, 3, 50),
            "2 · assistant_message · worker\n→ bash {\"command\":\"ls\"}\n\n3 · tool_result · system\na.rs\nb.rs"
        );
        // A range across the compaction shows the originals and the
        // summary block, both read from the log.
        assert_eq!(
            render_range(&events, 3, 4, 50),
            "3 · tool_result · system\na.rs\nb.rs\n\n4 · compacted · worker\nsummary:\nthe thread so far"
        );
        assert_eq!(
            render_range(&events, 5, 6, 50),
            "5 · project_switched · system\nmoved to beta\n\n6 · project_switched · system\nmoved outside any project"
        );
    }

    #[test]
    fn render_range_marks_a_kind_it_has_no_words_for_by_its_header_alone() {
        let events = vec![ev(
            0,
            EventKind::TurnEnded,
            Author::Agent(AgentId("worker".into())),
            json!({"reason": "done"}),
        )];
        assert_eq!(render_range(&events, 0, 0, 50), "0 · turn_ended · worker");
    }

    #[test]
    fn render_range_is_capped_and_says_how_many_it_left_out() {
        let events: Vec<Event> = (0..6).map(|i| user(i, "line")).collect();
        let out = render_range(&events, 0, 5, 2);
        let shown = 2;
        let omitted = 6 - shown;
        assert_eq!(
            out,
            format!(
                "0 · user_message · steve\nline\n\n1 · user_message · steve\nline\n\n({omitted} more events in this range not shown)"
            )
        );
    }

    #[test]
    fn render_range_of_an_empty_stretch_is_empty() {
        let events = vec![user(0, "hi")];
        assert_eq!(render_range(&events, 5, 9, 50), "");
    }

    /// T2 (issue #75), the `short_args` half: cut only above 60 chars.
    #[test]
    fn short_args_cuts_only_above_sixty_chars() {
        let at_cap = "x".repeat(58);
        let text = serde_json::to_string(&json!(at_cap)).unwrap();
        assert_eq!(text.chars().count(), 60, "the fixture sits on the cap");
        assert_eq!(short_args(&json!(at_cap)), text);

        let over = "x".repeat(59);
        let text = serde_json::to_string(&json!(over)).unwrap();
        assert_eq!(text.chars().count(), 61, "one over the cap");
        assert_eq!(
            short_args(&json!(over)),
            format!("{}…", text.chars().take(59).collect::<String>()),
            "the first 59 chars and a marker"
        );
    }

    /// The lookups the stub and the tool share.
    #[test]
    fn call_of_finds_the_block_with_that_id() {
        let events = vec![
            call(0, "c1", "bash", json!({"command": "ls"})),
            result(1, "c1", "a.rs"),
            call(2, "c2", "grep", json!({"pattern": "x"})),
        ];
        assert_eq!(
            call_of(&events, "c1"),
            Some(("bash".into(), json!({"command": "ls"})))
        );
        assert_eq!(call_of(&events, "nope"), None);
        assert_eq!(call_index(&events).unwrap().len(), 2);
    }
}
