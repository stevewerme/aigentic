//! Memory extraction: after a turn, ask the project's model what the
//! participants stated, file it under `.aigentic/memory/`, append
//! `memory_extracted`, reload the prefix. See `docs/PLAN-phase4.md`
//! section 6.
//!
//! The "only what a participant stated" rule is structural, not a prompt
//! instruction (decision 4): the model tags each line with the seq it came
//! from, and the runtime keeps a line only when that seq is a
//! `user_message` or a user's `skill_loaded`. A line that points at an
//! assistant message, a tool result or nothing is dropped as inference.

use std::io::Write as _;

use aigentic_core::{
    Author, CompletionRequest, ContentBlock, Event, EventKind, Message, ProviderEvent, Role, Usage,
};
use aigentic_log::{
    AssistantMessagePayload, MemoryExtractedPayload, MemoryLine, SkillLoadedPayload,
    ToolResultPayload, TurnEndedPayload, UserMessagePayload,
};
use futures_util::StreamExt;
use time::format_description::well_known::Rfc3339;

use crate::{ProjectError, Runtime, RuntimeError, Signal};

/// Fixed and versioned, like `SUMMARY_PROMPT`; the filter below does the
/// enforcing, this only shapes the reply.
pub const MEMORY_PROMPT: &str = "You maintain a project's memory files. Read the transcript that follows; \
each entry is tagged with its seq number, kind and author. Extract only what a participant \
explicitly stated: a decision they made, a constraint they set, or a fact they told you about \
the project. Do not infer, summarise or restate what the assistant or a tool said; do not file \
transient details of the current task. Reply with one line per item, nothing else, in the form \
`<kind> @<seq>: <text>` where kind is decision, constraint or fact, seq is the entry that stated \
it, and text is one short sentence in the participant's own terms. Reply `none` when there is \
nothing to file.";

const MEMORY_REQUEST: &str = "Extract the memory lines now.";

/// The three files, by kind tag.
pub const MEMORY_FILES: [(&str, &str); 3] = [
    ("decision", "decisions.md"),
    ("constraint", "constraints.md"),
    ("fact", "facts.md"),
];

/// Tool results longer than this are shortened in the transcript; a
/// result is never a source, so its full text only costs tokens.
const RESULT_HEAD: usize = 400;

/// One line the model proposed, before the filter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Proposed {
    pub(crate) file: String,
    pub(crate) at_seq: u64,
    pub(crate) text: String,
}

impl Runtime {
    /// After a turn: if the project keeps memory and enough turns have
    /// ended `done` since the last extraction, run one model call over
    /// the events since the cursor, file the lines that pass the
    /// stated-by filter, append `memory_extracted` and reload the prefix.
    /// `None` when nothing ran; `Some` with an empty `written` when the
    /// model found nothing new (the cursor still moves).
    pub async fn extract_memory(
        &mut self,
        observe: &mut dyn FnMut(Signal<'_>),
    ) -> Result<Option<MemoryExtractedPayload>, RuntimeError> {
        let Some(project) = self.layers.project.as_ref() else {
            return Ok(None);
        };
        if !project.file.memory.enabled {
            return Ok(None);
        }
        let every = project.file.memory.every_n_turns.max(1);
        let memory_dir = project.memory_dir();

        let events = self.log.read_all()?;
        let Some(through_seq) = events.last().map(|e| e.seq) else {
            return Ok(None);
        };
        let cursor = last_cursor(&events);
        let since: Vec<&Event> = events
            .iter()
            .filter(|e| cursor.is_none_or(|c| e.seq > c))
            .collect();
        if done_turns(&since) < u64::from(every) {
            return Ok(None);
        }

        let transcript = transcript(&since);
        let messages = vec![
            system(MEMORY_PROMPT),
            Message {
                role: Role::User,
                author: Author::System,
                blocks: vec![ContentBlock::Text(transcript)],
            },
            Message {
                role: Role::User,
                author: Author::System,
                blocks: vec![ContentBlock::Text(MEMORY_REQUEST.into())],
            },
        ];
        let request = CompletionRequest {
            messages: &messages,
            tools: &[],
            max_output_tokens: Some(1024),
        };
        let (mut text, mut usage) = (String::new(), Usage::default());
        let mut stream = self.provider.complete(&request);
        while let Some(event) = stream.next().await {
            match event {
                ProviderEvent::TextDelta(t) => text.push_str(&t),
                ProviderEvent::Usage(u) => usage = u,
                ProviderEvent::Error(e) => return Err(RuntimeError::Provider(e)),
                _ => {}
            }
        }
        drop(stream);

        let kept = filter_stated(parse_reply(&text), &since, &self.agent);
        let date = time::OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .unwrap_or_default();
        let date = date.get(..10).unwrap_or(&date).to_owned();
        let thread = self.log.thread_id().to_string();
        let written = write_lines(&memory_dir, &kept, &date, &thread)?;

        let payload = MemoryExtractedPayload {
            through_seq,
            written,
            model: self.model_label.clone(),
            usage: aigentic_log::Usage::reported(usage),
        };
        let value = serde_json::to_value(&payload).expect("serialisable");
        self.append(
            EventKind::MemoryExtracted,
            Author::Agent(self.agent.clone()),
            value,
            None,
            observe,
        )?;
        if let Some(project) = self.layers.project.as_mut() {
            project.reload_memory()?;
        }
        self.measured = None;
        Ok(Some(payload))
    }
}

/// `through_seq` of the latest `memory_extracted`, the cursor the next
/// extraction starts after.
fn last_cursor(events: &[Event]) -> Option<u64> {
    events
        .iter()
        .rev()
        .find(|e| e.kind == EventKind::MemoryExtracted)
        .and_then(|e| serde_json::from_value::<MemoryExtractedPayload>(e.payload.clone()).ok())
        .map(|p| p.through_seq)
}

/// Turns that ended `done` among `events`.
fn done_turns(events: &[&Event]) -> u64 {
    events
        .iter()
        .filter(|e| e.kind == EventKind::TurnEnded)
        .filter(|e| {
            serde_json::from_value::<TurnEndedPayload>(e.payload.clone())
                .is_ok_and(|p| p.reason == "done")
        })
        .count() as u64
}

/// The events as the model sees them for extraction: one entry per
/// message-bearing event, tagged `[seq N] kind author:` so a line can name
/// its source. Provider blobs are dropped; tool results are shortened.
pub(crate) fn transcript(events: &[&Event]) -> String {
    let mut out = String::new();
    for e in events {
        let who = author_label(&e.author);
        let entry = match e.kind {
            EventKind::UserMessage => {
                serde_json::from_value::<UserMessagePayload>(e.payload.clone())
                    .ok()
                    .map(|p| format!("user {who}: {}", text_of(&p.blocks)))
            }
            EventKind::AssistantMessage => {
                serde_json::from_value::<AssistantMessagePayload>(e.payload.clone())
                    .ok()
                    .map(|p| format!("assistant {who}: {}", text_of(&p.blocks)))
            }
            EventKind::ToolResult => serde_json::from_value::<ToolResultPayload>(e.payload.clone())
                .ok()
                .map(|p| format!("tool_result: {}", head(&p.result.content))),
            EventKind::SkillLoaded => {
                serde_json::from_value::<SkillLoadedPayload>(e.payload.clone())
                    .ok()
                    .map(|p| format!("skill_loaded {who}: {}", p.name))
            }
            _ => None,
        };
        if let Some(entry) = entry {
            out.push_str(&format!("[seq {}] {entry}\n", e.seq));
        }
    }
    out
}

fn author_label(author: &Author) -> String {
    match author {
        Author::User(id) => id.0.clone(),
        Author::Agent(id) => id.0.clone(),
        Author::System => "system".into(),
    }
}

fn text_of(blocks: &[ContentBlock]) -> String {
    let mut parts = Vec::new();
    for b in blocks {
        match b {
            ContentBlock::Text(t) => parts.push(t.trim().to_owned()),
            ContentBlock::ToolCall(c) => parts.push(format!("[calls {}]", c.name)),
            _ => {}
        }
    }
    parts.join(" ")
}

fn head(s: &str) -> String {
    if s.len() <= RESULT_HEAD {
        return s.replace('\n', " ");
    }
    let mut end = RESULT_HEAD;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}… ({} bytes omitted)",
        s[..end].replace('\n', " "),
        s.len() - end
    )
}

/// Parse `<kind> @<seq>: <text>` lines; anything else is ignored.
pub(crate) fn parse_reply(text: &str) -> Vec<Proposed> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw
            .trim()
            .trim_start_matches(['-', '*', '•'])
            .trim_start_matches('`')
            .trim_end_matches('`')
            .trim();
        let Some((head, body)) = line.split_once(':') else {
            continue;
        };
        let mut parts = head.split_whitespace();
        let (Some(kind), Some(seq)) = (parts.next(), parts.next()) else {
            continue;
        };
        let Some((_, file)) = MEMORY_FILES
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(kind))
        else {
            continue;
        };
        let seq = seq
            .trim_start_matches(['@', '#'])
            .trim_start_matches("seq=");
        let Ok(at_seq) = seq.parse::<u64>() else {
            continue;
        };
        let text = body.trim();
        if text.is_empty() {
            continue;
        }
        out.push(Proposed {
            file: (*file).to_owned(),
            at_seq,
            text: text.to_owned(),
        });
    }
    out
}

/// Keep only lines whose seq, within the events considered, is a
/// `user_message` or a `skill_loaded` authored by a user, or (phase 5)
/// an `assistant_message` by an agent other than this thread's own,
/// since that is a participant stating something. Everything else is
/// inference and is dropped.
pub(crate) fn filter_stated(
    proposed: Vec<Proposed>,
    events: &[&Event],
    own: &aigentic_core::AgentId,
) -> Vec<MemoryLine> {
    proposed
        .into_iter()
        .filter_map(|p| {
            let e = events.iter().find(|e| e.seq == p.at_seq)?;
            let stated = match (&e.kind, &e.author) {
                (EventKind::UserMessage, Author::User(_))
                | (EventKind::SkillLoaded, Author::User(_)) => true,
                (EventKind::AssistantMessage, Author::Agent(a)) => a != own,
                _ => false,
            };
            stated.then(|| MemoryLine {
                file: p.file,
                text: p.text,
                stated_by: e.author.clone(),
                at_seq: p.at_seq,
            })
        })
        .collect()
}

/// Append each line to its file as `- <text> <!-- <date> thread <id> -->`,
/// skipping a line whose text is already present. Returns what landed.
/// The harness never rewrites a line it did not just add.
fn write_lines(
    dir: &std::path::Path,
    lines: &[MemoryLine],
    date: &str,
    thread: &str,
) -> Result<Vec<MemoryLine>, ProjectError> {
    let mut written = Vec::new();
    if lines.is_empty() {
        return Ok(written);
    }
    std::fs::create_dir_all(dir).map_err(|source| ProjectError::Io {
        path: dir.to_path_buf(),
        source,
    })?;
    for line in lines {
        let path = dir.join(&line.file);
        let existing = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(source) => return Err(ProjectError::Io { path, source }),
        };
        let present = existing
            .lines()
            .map(bullet_text)
            .chain(
                written
                    .iter()
                    .filter(|w: &&MemoryLine| w.file == line.file)
                    .map(|w| w.text.as_str()),
            )
            .any(|t| t == line.text);
        if present {
            continue;
        }
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)
            .map_err(|source| ProjectError::Io {
                path: path.clone(),
                source,
            })?;
        let prefix = if existing.is_empty() || existing.ends_with('\n') {
            ""
        } else {
            "\n"
        };
        writeln!(
            file,
            "{prefix}- {} <!-- {date} thread {thread} -->",
            line.text
        )
        .map_err(|source| ProjectError::Io {
            path: path.clone(),
            source,
        })?;
        written.push(line.clone());
    }
    Ok(written)
}

/// The text of a bullet line without its marker and trailing comment.
fn bullet_text(line: &str) -> &str {
    let line = line.trim();
    let line = line
        .strip_prefix("- ")
        .or_else(|| line.strip_prefix("* "))
        .unwrap_or(line);
    match line.rfind("<!--") {
        Some(i) => line[..i].trim_end(),
        None => line,
    }
}

fn system(text: &str) -> Message {
    Message {
        role: Role::System,
        author: Author::System,
        blocks: vec![ContentBlock::Text(text.to_owned())],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aigentic_core::{AgentId, UserId};
    use serde_json::json;
    use ulid::Ulid;

    fn event(seq: u64, kind: EventKind, author: Author, payload: serde_json::Value) -> Event {
        Event {
            id: Ulid::from_parts(1_700_000_000_000 + seq, u128::from(seq)),
            thread_id: Ulid::from_parts(1_700_000_000_000, 1),
            seq,
            kind,
            author,
            payload,
            parent_event: None,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn steve() -> Author {
        Author::User(UserId("steve".into()))
    }

    #[test]
    fn replies_parse_leniently_and_unknown_kinds_are_dropped() {
        let got = parse_reply(
            "decision @3: Use Swedish.\n- fact #5: The repo is aigentic.\nConstraint seq=7: No tokio in core\nrumour @2: nope\nnone\n",
        );
        assert_eq!(
            got,
            vec![
                Proposed {
                    file: "decisions.md".into(),
                    at_seq: 3,
                    text: "Use Swedish.".into()
                },
                Proposed {
                    file: "facts.md".into(),
                    at_seq: 5,
                    text: "The repo is aigentic.".into()
                },
                Proposed {
                    file: "constraints.md".into(),
                    at_seq: 7,
                    text: "No tokio in core".into()
                },
            ]
        );
        assert!(parse_reply("none").is_empty());
    }

    #[test]
    fn the_filter_keeps_user_messages_and_user_skills_only() {
        let events = [
            event(0, EventKind::UserMessage, steve(), json!({"blocks": []})),
            event(
                1,
                EventKind::AssistantMessage,
                Author::Agent(AgentId("worker".into())),
                json!({"blocks": []}),
            ),
            event(
                2,
                EventKind::ToolResult,
                Author::System,
                json!({"id": "c", "content": "", "is_error": false}),
            ),
            event(
                3,
                EventKind::SkillLoaded,
                steve(),
                json!({"name": "tdd", "hash": "", "source": "", "body": "", "invoked_by": "user"}),
            ),
            event(
                4,
                EventKind::SkillLoaded,
                Author::Agent(AgentId("worker".into())),
                json!({"name": "tdd", "hash": "", "source": "", "body": "", "invoked_by": "model"}),
            ),
            // Another agent posting into the thread is a participant.
            event(
                5,
                EventKind::AssistantMessage,
                Author::Agent(AgentId("orchestrator".into())),
                json!({"blocks": []}),
            ),
        ];
        let refs: Vec<&Event> = events.iter().collect();
        let proposed = (0..7)
            .map(|s| Proposed {
                file: "facts.md".into(),
                at_seq: s,
                text: format!("line {s}"),
            })
            .collect();
        let kept = filter_stated(proposed, &refs, &AgentId("worker".into()));
        assert_eq!(
            kept.iter().map(|l| l.at_seq).collect::<Vec<_>>(),
            vec![0, 3, 5]
        );
        assert_eq!(kept[0].stated_by, steve());
        assert_eq!(
            kept[2].stated_by,
            Author::Agent(AgentId("orchestrator".into()))
        );
    }

    #[test]
    fn the_transcript_tags_every_entry_with_its_seq() {
        let events = [
            event(
                0,
                EventKind::UserMessage,
                steve(),
                json!({"blocks": [{"type": "text", "text": "We ship Friday."}]}),
            ),
            event(
                1,
                EventKind::AssistantMessage,
                Author::Agent(AgentId("w".into())),
                json!({"blocks": [{"type": "text", "text": "ok"}, {"type": "tool_call", "id": "c", "name": "bash", "args": {}}]}),
            ),
            event(
                2,
                EventKind::ToolResult,
                Author::System,
                json!({"id": "c", "content": "a\nb", "is_error": false}),
            ),
            event(
                3,
                EventKind::TurnEnded,
                Author::Agent(AgentId("w".into())),
                json!({"reason": "done"}),
            ),
        ];
        let refs: Vec<&Event> = events.iter().collect();
        assert_eq!(
            transcript(&refs),
            "[seq 0] user steve: We ship Friday.\n[seq 1] assistant w: ok [calls bash]\n[seq 2] tool_result: a b\n"
        );
        assert_eq!(done_turns(&refs), 1);
    }

    #[test]
    fn writing_is_append_only_and_skips_present_text() {
        let dir = tempfile::tempdir().unwrap();
        let mem = dir.path().join("memory");
        let line = |text: &str| MemoryLine {
            file: "decisions.md".into(),
            text: text.into(),
            stated_by: steve(),
            at_seq: 0,
        };
        let w = write_lines(
            &mem,
            &[line("Use Swedish."), line("Use Swedish.")],
            "2026-09-21",
            "T",
        )
        .unwrap();
        assert_eq!(w.len(), 1);
        let w = write_lines(
            &mem,
            &[line("Use Swedish."), line("Ship Friday.")],
            "2026-09-22",
            "U",
        )
        .unwrap();
        assert_eq!(
            w.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(),
            vec!["Ship Friday."]
        );
        assert_eq!(
            std::fs::read_to_string(mem.join("decisions.md")).unwrap(),
            "- Use Swedish. <!-- 2026-09-21 thread T -->\n- Ship Friday. <!-- 2026-09-22 thread U -->\n"
        );
        // A hand-written file without a trailing newline is not corrupted.
        std::fs::write(mem.join("facts.md"), "- by hand").unwrap();
        let mut f = line("A fact.");
        f.file = "facts.md".into();
        write_lines(&mem, &[f], "2026-09-22", "U").unwrap();
        assert_eq!(
            std::fs::read_to_string(mem.join("facts.md")).unwrap(),
            "- by hand\n- A fact. <!-- 2026-09-22 thread U -->\n"
        );
    }
}
