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
//!
//! Issue #14 added two gates before and after that one. Before the model:
//! the cue gate — a user message is extracted only when it states
//! something durable ("for the record", "from now on", "always", …);
//! every other user message, which is where one-off task instructions
//! live, is skipped before the model sees the transcript. After the
//! model: the scope tag — the model tags each line `durable` or `task`,
//! and the runtime drops `task` lines, because a participant stating a
//! task instruction ("Create ~/x, then run the tests") is attribution the
//! seq filter passes but not memory: it is obsolete the moment the task
//! is done. A line that restates the project's name or something the
//! instructions or knowledge layers already carry is dropped too. The
//! reliable path is `/remember`: a person files a line themselves, no
//! model in between.

use std::io::Write as _;

use aigentic_core::{
    Author, CompletionRequest, ContentBlock, Event, EventKind, Message, ProviderEvent, Role, Usage,
};
use aigentic_log::{
    AssistantMessagePayload, MemoryExtractedPayload, MemoryLine, MemoryRememberedPayload,
    SkillLoadedPayload, ToolResultPayload, TurnEndedPayload, UserMessagePayload,
};
use futures_util::StreamExt;
use time::format_description::well_known::Rfc3339;

use crate::{ProjectError, Runtime, RuntimeError, Signal};

/// Fixed and versioned, like `SUMMARY_PROMPT`; the filter below does the
/// enforcing, this only shapes the reply.
pub const MEMORY_PROMPT: &str = "You maintain a project's memory files. Read the transcript that follows; \
each entry is tagged with its seq number, kind and author. Extract only what a participant \
explicitly stated: a decision they made, a constraint they set, or a fact they told you about \
the project. Do not infer, summarise or restate what the assistant or a tool said. For each item \
also judge its scope. durable means it still matters in a different thread a month from now: how \
the person wants you to work on this project in general, what the project is, what it always \
uses or avoids. task means it only helps finish the current task: steps to take (Create \
~/Projects/x with…, Run aigentic project init), how this one piece of work should be done (Show \
the diff; don't commit, Label the bugs section…), or observations about the task's own objects \
(Item 10 is already decided). An instruction the agent carries out and is then done with is \
task however the person phrased it; a standing rule or a fact about the project is durable. When \
unsure, say task: a line not filed costs nothing, a wrong line is loaded by every later thread. \
Reply with one line per item, nothing else, in the form `<kind> @<seq> <scope>: <text>` where \
kind is decision, constraint or fact, seq is the entry that stated it, scope is durable or \
task, and text is one short sentence in the participant's own terms. Reply `none` when there is \
nothing to file.";

const MEMORY_REQUEST: &str = "Extract the memory lines now.";

/// The three files, by kind tag.
pub const MEMORY_FILES: [(&str, &str); 3] = [
    ("decision", "decisions.md"),
    ("constraint", "constraints.md"),
    ("fact", "facts.md"),
];

/// The cue gate (issue #14, the primary filter): a user message is
/// offered to the extraction model only when it states something
/// durable — one of these cues, in any case. `/remember` needs no
/// entry of its own: the word covers it. Everything else is skipped
/// before the model runs, so an instruction for the current task
/// ("Create ~/x, then run the tests") never even reaches it.
pub const CUES: &[&str] = &[
    "for the record",
    "remember",
    "from now on",
    "going forward",
    "always",
    "never",
    "in this project",
    "our convention",
    "we decided",
];

/// Does this user message state something durable, per the cue gate?
pub(crate) fn eligible(text: &str) -> bool {
    let lower = text.to_lowercase();
    CUES.iter().any(|cue| lower.contains(cue))
}

/// What the project already carries outside memory: its name, and the
/// instructions and knowledge layers. A proposed line that restates any
/// of it is not new (issue #14).
#[derive(Debug, Clone, Default)]
pub(crate) struct Known {
    pub(crate) name: String,
    pub(crate) text: String,
}

impl Known {
    /// Does the line restate the project's name, or something the
    /// instructions or knowledge layers already say?
    pub(crate) fn restates(&self, line: &str) -> bool {
        let line = normalise(line);
        let name = normalise(&self.name);
        if name.chars().count() >= 3 && line.contains(&name) {
            return true;
        }
        let text = normalise(&self.text);
        !text.is_empty() && text.contains(&line)
    }
}

/// Lowercase, drop punctuation, collapse whitespace: the shape two
/// restatements of one thing share.
fn normalise(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Tool results longer than this are shortened in the transcript; a
/// result is never a source, so its full text only costs tokens.
const RESULT_HEAD: usize = 400;

/// One line the model proposed, before the filter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Proposed {
    pub(crate) file: String,
    pub(crate) at_seq: u64,
    /// The model's own scope verdict; `task` lines are dropped even
    /// when the attribution holds, because they are instructions for
    /// the current task, not memory (issue #14).
    pub(crate) durable: bool,
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
        observe: &mut (dyn FnMut(Signal<'_>) + Send),
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
            // Reasoning models spend part of this before the reply.
            max_output_tokens: Some(2048),
        };
        let (mut text, mut usage) = (String::new(), Usage::default());
        let mut stream = self.utility().complete(&request);
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
        // Not new memory either: a line that restates the project's
        // name or something the instructions or knowledge layers
        // already carry.
        let known = self.known();
        let kept: Vec<_> = kept
            .into_iter()
            .filter(|l| !known.restates(&l.text))
            .collect();
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

    /// The project's name and its instructions and knowledge layers,
    /// for the restatement filter: a line that restates any of them is
    /// not new memory.
    fn known(&self) -> Known {
        let Some(project) = self.layers.project.as_ref() else {
            return Known::default();
        };
        let mut text = project.instructions.clone().unwrap_or_default();
        if let Ok(entries) = std::fs::read_dir(project.knowledge_dir()) {
            for entry in entries.flatten() {
                if let Ok(md) = std::fs::read_to_string(entry.path()) {
                    text.push('\n');
                    text.push_str(&md);
                }
            }
        }
        Known {
            name: project.name.clone(),
            text,
        }
    }

    /// `/remember <text>` (issue #14): the person files a line
    /// themselves — no model call, no filter, because the command is
    /// the statement. An optional first word (`decision`, `constraint`
    /// or `fact`) picks the file; without one the line lands in
    /// `facts.md`. Appends a `memory_remembered` event (the audit, and
    /// what the client prints) and reloads the prefix.
    pub fn remember(
        &mut self,
        author: Author,
        text: &str,
        observe: &mut (dyn FnMut(Signal<'_>) + Send),
    ) -> Result<(), RuntimeError> {
        let Some(project) = self.layers.project.as_ref() else {
            return Err(RuntimeError::NoProject);
        };
        let memory_dir = project.memory_dir();

        let (file, text) = split_kind(text);
        let (file, text) = (file.to_owned(), text.to_owned());
        // The line's own audit trail: the seq of the event this
        // method appends once the write has decided what it says.
        let at_seq = self.log.read_all()?.last().map_or(0, |e| e.seq + 1);
        let date = time::OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .unwrap_or_default();
        let date = date.get(..10).unwrap_or(&date).to_owned();
        let thread = self.log.thread_id().to_string();
        let line = MemoryLine {
            file: file.clone(),
            text: text.clone(),
            stated_by: author.clone(),
            at_seq,
        };
        let written = write_lines(&memory_dir, std::slice::from_ref(&line), &date, &thread)?;

        let payload = MemoryRememberedPayload {
            file,
            text,
            written: !written.is_empty(),
        };
        self.append(
            EventKind::MemoryRemembered,
            author,
            serde_json::to_value(&payload).expect("serialisable"),
            None,
            observe,
        )?;
        if let Some(project) = self.layers.project.as_mut() {
            project.reload_memory()?;
        }
        self.measured = None;
        Ok(())
    }
}

/// Split an optional kind word off the front of a `/remember` line:
/// `decision`, `constraint` or `fact` picks the file, the rest is the
/// line. Without one the whole text is a fact.
pub(crate) fn split_kind(text: &str) -> (&'static str, &str) {
    let trimmed = text.trim();
    if let Some((word, rest)) = trimmed.split_once(char::is_whitespace)
        && !rest.trim().is_empty()
        && let Some((_, file)) = MEMORY_FILES
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(word))
    {
        return (file, rest.trim());
    }
    ("facts.md", trimmed)
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
                    // The cue gate: an ineligible user message is skipped
                    // before the model sees the transcript at all.
                    .filter(|p| eligible(&text_of(&p.blocks)))
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

/// Parse `<kind> @<seq> <scope>: <text>` lines; anything else is
/// ignored, including a missing or unknown scope (issue #14: a line
/// the model did not classify is not filed).
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
        let (Some(kind), Some(seq), Some(scope)) = (parts.next(), parts.next(), parts.next())
        else {
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
        let durable = match scope.trim_matches([',', '.']) {
            s if s.eq_ignore_ascii_case("durable") => true,
            s if s.eq_ignore_ascii_case("task") => false,
            _ => continue,
        };
        let text = body.trim();
        if text.is_empty() {
            continue;
        }
        out.push(Proposed {
            file: (*file).to_owned(),
            at_seq,
            durable,
            text: text.to_owned(),
        });
    }
    out
}

/// Keep only lines whose seq, within the events considered, is a
/// `user_message` or a `skill_loaded` authored by a user, or (phase 5)
/// an `assistant_message` by an agent other than this thread's own,
/// since that is a participant stating something — and only when the
/// model called the line `durable`: a task instruction the participant
/// did state is still not memory (issue #14). A user message must also
/// pass the cue gate, so a line pointing at an ineligible one — which
/// the model never saw, and can only have invented — is dropped.
/// Everything else is dropped as inference.
pub(crate) fn filter_stated(
    proposed: Vec<Proposed>,
    events: &[&Event],
    own: &aigentic_core::AgentId,
) -> Vec<MemoryLine> {
    proposed
        .into_iter()
        .filter_map(|p| {
            if !p.durable {
                return None;
            }
            let e = events.iter().find(|e| e.seq == p.at_seq)?;
            let stated = match (&e.kind, &e.author) {
                (EventKind::UserMessage, Author::User(_)) => {
                    serde_json::from_value::<UserMessagePayload>(e.payload.clone())
                        .ok()
                        .map(|p| eligible(&text_of(&p.blocks)))
                        .unwrap_or(false)
                }
                (EventKind::SkillLoaded, Author::User(_)) => true,
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
            "decision @3 durable: Use Swedish.\n- fact #5 Durable: The repo is aigentic.\nConstraint seq=7 task: No tokio in core\nrumour @2 durable: nope\nfact @4 standing: neither scope\ndecision @6: no scope word\nnone\n",
        );
        assert_eq!(
            got,
            vec![
                Proposed {
                    file: "decisions.md".into(),
                    at_seq: 3,
                    durable: true,
                    text: "Use Swedish.".into()
                },
                Proposed {
                    file: "facts.md".into(),
                    at_seq: 5,
                    durable: true,
                    text: "The repo is aigentic.".into()
                },
                Proposed {
                    file: "constraints.md".into(),
                    at_seq: 7,
                    durable: false,
                    text: "No tokio in core".into()
                },
            ]
        );
        assert!(parse_reply("none").is_empty());
    }

    #[test]
    fn remember_lines_split_their_optional_kind_word() {
        assert_eq!(
            split_kind("decision Ship on Fridays"),
            ("decisions.md", "Ship on Fridays")
        );
        assert_eq!(
            split_kind("  Constraint   no tokio in core "),
            ("constraints.md", "no tokio in core")
        );
        assert_eq!(
            split_kind("Use Swedish in the UI"),
            ("facts.md", "Use Swedish in the UI")
        );
        // Not a kind word: the line is a fact about decisions.
        assert_eq!(
            split_kind("decisions are hard to make"),
            ("facts.md", "decisions are hard to make")
        );
        // A kind word with nothing after it is not a kind.
        assert_eq!(split_kind("fact"), ("facts.md", "fact"));
    }

    /// The 2026-09-23 spin (issue #14): a user message that carries a
    /// cue passes the gate, and the instructions in it are still
    /// task-scoped — the scope tag drops them, not the gate.
    #[test]
    fn task_instructions_do_not_survive_even_when_the_user_stated_them() {
        let events = [event(
            0,
            EventKind::UserMessage,
            steve(),
            json!({"blocks": [{"type": "text", "text": "Remember: create ~/Projects/aigentic-web, run aigentic project init, show the diff; don't commit."}]}),
        )];
        let refs: Vec<&Event> = events.iter().collect();
        let reply = "decision @0 task: Create ~/Projects/aigentic-web with Next.js + shadcn.\n\
decision @0 task: Run `aigentic project init` in that directory.\n\
constraint @0 task: Show the diff; don't commit.\n\
fact @0 durable: The project is called aigentic.\n";
        let kept = filter_stated(parse_reply(reply), &refs, &AgentId("worker".into()));
        assert_eq!(
            kept.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(),
            vec!["The project is called aigentic."]
        );
    }

    #[test]
    fn the_cue_gate_admits_only_durable_statements() {
        assert!(eligible("For the record, we deploy from main only."));
        assert!(eligible("From now on, dates in Swedish format."));
        assert!(eligible("Always answer in Swedish."));
        assert!(eligible("NEVER run git reset."));
        assert!(eligible("Remember: no tokio in core."));
        assert!(eligible("In this project we ship on Fridays."));
        assert!(eligible("Going forward, our convention is trunk-based."));
        assert!(eligible("We decided to ship on Fridays."));
        // `/remember` is covered by the word "remember".
        assert!(eligible("/remember we ship on Fridays"));
        assert!(!eligible("Create ~/Projects/aigentic-web with Next.js."));
        assert!(!eligible("Show the diff; don't commit."));
        assert!(!eligible("Item 10 is already decided."));
        assert!(!eligible(""));
    }

    /// Every line the 2026-09-23 spin actually filed (issue #14), as
    /// the user messages they came from. None states anything durable,
    /// so none passes the cue gate: not to the model, and not through
    /// it.
    #[test]
    fn the_spin_corpus_yields_nothing() {
        const CORPUS: &[&str] = &[
            "Create nothing until steve says go.",
            "Verify every command, flag and config key against the code and --help before writing.",
            "Show the diff; don't commit.",
            "Create ~/Projects/aigentic-web with Next.js + shadcn.",
            "Run `aigentic project init` in that directory.",
            "Create a private GitHub repo with `gh repo create`.",
            "Switch this thread to it with `/project use aigentic-web`.",
            "Label bug for the Bugs section, enhancement for UX and Features, documentation for Model behaviour.",
            "Give item 10 ready-for-agent as well as its other labels.",
            "Cross-reference related items in the bodies: 2 and 3, 8 and 9, 14 and 15.",
            "Skip the \"Not harness issues\" section and the already-fixed items.",
            "Skip items 8 and 9 since they are already filed together as #13, and point anything that would cross-reference 8 or 9 to #13 instead.",
            "Create the other 16 issues, then add the back-references to the first issue of each pair.",
            "Rewrite README.md as a front page — a 3–4 sentence TL;DR, a short list of what works today, and Getting started with install, config.toml with a profile, the key in .env, aigentic doctor, aigentic in a repo, and aigentic init for a new project.",
            "Drop the phase table and status narrative from README, linking docs/PRD.md and AGENTS.md for depth.",
            "Tighten the opening to three sentences without dashes, add a short Developing section with the three gate commands and that CI runs them, then commit as docs: README as a front page and push.",
            "Item 10 is already decided.",
            "2 and 3 are one policy change.",
            "8 and 9 share one widget.",
            "14 and 15 are both skills onboarding.",
            "The project is called aigentic.",
            "The README's opening should be three sentences without dashes and include a short Developing section covering the three gate commands and that CI runs them.",
        ];
        for source in CORPUS {
            let events = [event(
                0,
                EventKind::UserMessage,
                steve(),
                json!({"blocks": [{"type": "text", "text": source}]}),
            )];
            let refs: Vec<&Event> = events.iter().collect();
            assert!(!eligible(source), "{source}");
            // Skipped before the model: the transcript has no entry.
            assert!(transcript(&refs).is_empty(), "{source}");
            // And through it: a durable line pointing at the message is
            // inference the model could only have invented.
            let kept = filter_stated(
                vec![Proposed {
                    file: "facts.md".into(),
                    at_seq: 0,
                    durable: true,
                    text: (*source).to_owned(),
                }],
                &refs,
                &AgentId("worker".into()),
            );
            assert!(kept.is_empty(), "{source}");
        }
    }

    #[test]
    fn lines_restating_the_name_or_the_layers_are_not_new() {
        let known = Known {
            name: "aigentic".into(),
            text: "Answer in Swedish.\n# Knowledge\nThe gate is fmt, clippy, test.".into(),
        };
        assert!(known.restates("The project is called aigentic."));
        assert!(known.restates("Answer in Swedish."));
        // Punctuation and case do not save a restatement.
        assert!(known.restates("The gate is: fmt, clippy, test."));
        assert!(!known.restates("We deploy from main only."));
        // A one-letter name matches nothing on its own.
        let short = Known {
            name: "m".into(),
            text: String::new(),
        };
        assert!(!short.restates("The project is called m."));
    }

    #[test]
    fn the_filter_keeps_user_messages_and_user_skills_only() {
        let events = [
            event(
                0,
                EventKind::UserMessage,
                steve(),
                json!({"blocks": [{"type": "text", "text": "For the record, this stands."}]}),
            ),
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
        let mut proposed = (0..7)
            .map(|s| Proposed {
                file: "facts.md".into(),
                at_seq: s,
                durable: true,
                text: format!("line {s}"),
            })
            .collect::<Vec<_>>();
        // The same user message, tagged task: attribution holds, scope
        // does not.
        proposed.push(Proposed {
            file: "facts.md".into(),
            at_seq: 0,
            durable: false,
            text: "task line 0".into(),
        });
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
                json!({"blocks": [{"type": "text", "text": "For the record, we ship Friday."}]}),
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
            "[seq 0] user steve: For the record, we ship Friday.\n[seq 1] assistant w: ok [calls bash]\n[seq 2] tool_result: a b\n"
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
