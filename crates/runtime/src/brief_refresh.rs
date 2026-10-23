//! Keeping a project's brief current: after a batch of turns that wrote
//! files, one call on the utility model rewrites the brief's
//! `## Current state` section from the turns since the last refresh, and
//! a `brief_refreshed` event records it. The rest of the brief is the
//! person's and is never touched.

use aigentic_core::{
    Author, CompletionRequest, ContentBlock, Event, EventKind, Message, ProviderEvent, Role, Usage,
};
use aigentic_log::{
    AssistantMessagePayload, BriefRefreshedPayload, TurnEndedPayload, UserMessagePayload,
};
use futures_util::StreamExt;
use time::format_description::well_known::Rfc3339;

use crate::brief::{
    WriteOutcome, brief_hash, project_brief_path, splice_current_state, write_brief_atomically,
};
use crate::{Runtime, RuntimeError, Signal};

/// What the refresh asks of the model.
pub const BRIEF_PROMPT: &str = "You keep a project's brief current. Below are the brief as it stands and the turns since it was last refreshed. Write only the body of the brief's current-state section: where the project stands now and what is next, in plain Markdown, about 150 words. No heading, no date: the harness adds both. Say only what the turns show.";

/// Bytes of one message's text the record keeps.
pub const TURN_TEXT_CAP: usize = 1_000;

/// Bytes of the whole record sent, the most recent turns kept.
pub const RECORD_CAP: usize = 24_000;

impl Runtime {
    /// After a turn: rewrite the project brief's current-state section
    /// when a turn since the last refresh wrote files. Runs only when the
    /// thread has a project, its `[brief] refresh` is on and `brief.md`
    /// is a regular file. `None` when nothing ran; an error from the
    /// provider appends nothing, so the next batch tries again. A brief
    /// that changed under the call is left alone and nothing is
    /// recorded; one the reply leaves as it was is still recorded.
    pub async fn refresh_project_brief(
        &mut self,
        observe: &mut (dyn FnMut(Signal<'_>) + Send),
    ) -> Result<Option<BriefRefreshedPayload>, RuntimeError> {
        let Some(project) = self.layers.project.as_ref() else {
            return Ok(None);
        };
        if !project.file.brief.refresh {
            return Ok(None);
        }
        let path = project_brief_path(&project.root);
        let is_file = std::fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_file());
        if !is_file {
            return Ok(None);
        }

        let events = self.log.read_all()?;
        let Some(through_seq) = events.last().map(|e| e.seq) else {
            return Ok(None);
        };
        let cursor = last_refresh(&events);
        let since: Vec<&Event> = events
            .iter()
            .filter(|e| cursor.is_none_or(|c| e.seq > c))
            .collect();
        if !wrote_files(&since) {
            return Ok(None);
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Ok(None);
        };
        let old_hash = brief_hash(&text);

        let messages = vec![
            Message {
                role: Role::System,
                author: Author::System,
                blocks: vec![ContentBlock::Text(BRIEF_PROMPT.to_owned())],
            },
            Message {
                role: Role::User,
                author: Author::System,
                blocks: vec![ContentBlock::Text(format!(
                    "The brief:\n\n{text}\n\nThe turns since it was refreshed:\n\n{}",
                    record(&since)
                ))],
            },
        ];
        let request = CompletionRequest {
            messages: &messages,
            tools: &[],
            max_output_tokens: Some(1024),
        };
        let (mut reply, mut usage) = (String::new(), Usage::default());
        let mut stream = self.utility().complete(&request);
        while let Some(event) = stream.next().await {
            match event {
                ProviderEvent::TextDelta(t) => reply.push_str(&t),
                ProviderEvent::Usage(u) => usage = u,
                ProviderEvent::Error(e) => return Err(RuntimeError::Provider(e)),
                _ => {}
            }
        }
        drop(stream);

        let date = time::OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .unwrap_or_default();
        let date = date.get(..10).unwrap_or(&date).to_owned();
        let new_text = splice_current_state(&text, &reply, &date);
        let written = write_brief_atomically(&path, &old_hash, &new_text).map_err(|source| {
            crate::ProjectError::Io {
                path: path.clone(),
                source,
            }
        })?;
        let new_hash = match written {
            WriteOutcome::ChangedUnderneath => return Ok(None),
            WriteOutcome::Unchanged => old_hash.clone(),
            WriteOutcome::Written => brief_hash(&new_text),
        };

        // The utility profile's label and prices when one is configured,
        // the thread's own otherwise, as memory extraction reports them.
        let model = self
            .utility_label
            .clone()
            .unwrap_or_else(|| self.model_label.clone());
        let prices = if self.utility.is_some() {
            self.utility_prices
        } else {
            self.prices
        };
        let mut usage = aigentic_log::Usage::reported(usage);
        usage.cost_usd = prices.map(|p| p.cost_usd(&usage));
        let payload = BriefRefreshedPayload {
            through_seq,
            old_hash,
            new_hash,
            model,
            usage,
        };
        self.append(
            EventKind::BriefRefreshed,
            Author::System,
            serde_json::to_value(&payload).expect("serialisable"),
            None,
            observe,
        )?;
        Ok(Some(payload))
    }
}

/// The last refresh's cursor, or `None` when the brief was never
/// refreshed in this thread.
fn last_refresh(events: &[Event]) -> Option<u64> {
    events
        .iter()
        .rev()
        .find(|e| e.kind == EventKind::BriefRefreshed)
        .and_then(|e| serde_json::from_value::<BriefRefreshedPayload>(e.payload.clone()).ok())
        .map(|p| p.through_seq)
}

/// Whether a turn among `events` ended having written a file. A change
/// made only through `bash` is not in `touched`, so it does not count.
fn wrote_files(events: &[&Event]) -> bool {
    events
        .iter()
        .filter(|e| e.kind == EventKind::TurnEnded)
        .filter_map(|e| serde_json::from_value::<TurnEndedPayload>(e.payload.clone()).ok())
        .any(|p| !p.touched.is_empty())
}

/// What the model reads of the turns: per turn the person's text, the
/// replies' text and the calls they made, then the files it wrote and
/// how it ended. Each text is cut at [`TURN_TEXT_CAP`] bytes and the
/// whole at [`RECORD_CAP`], keeping the most recent turns.
pub fn record(events: &[&Event]) -> String {
    let mut turns: Vec<String> = Vec::new();
    let mut turn = String::new();
    for event in events {
        match event.kind {
            EventKind::UserMessage => {
                if let Ok(p) = serde_json::from_value::<UserMessagePayload>(event.payload.clone()) {
                    turn.push_str(&format!("Person: {}\n", capped(&texts(&p.blocks))));
                }
            }
            EventKind::AssistantMessage => {
                if let Ok(p) =
                    serde_json::from_value::<AssistantMessagePayload>(event.payload.clone())
                {
                    let said = texts(&p.blocks);
                    if !said.trim().is_empty() {
                        turn.push_str(&format!("Reply: {}\n", capped(&said)));
                    }
                    let calls: Vec<&str> = p
                        .blocks
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::ToolCall(call) => Some(call.name.as_str()),
                            _ => None,
                        })
                        .collect();
                    if !calls.is_empty() {
                        turn.push_str(&format!("Calls: {}\n", calls.join(", ")));
                    }
                }
            }
            EventKind::TurnEnded => {
                if let Ok(p) = serde_json::from_value::<TurnEndedPayload>(event.payload.clone()) {
                    if !p.touched.is_empty() {
                        turn.push_str(&format!("Wrote: {}\n", p.touched.join(", ")));
                    }
                    turn.push_str(&format!("Ended: {}\n", p.reason));
                }
                turns.push(std::mem::take(&mut turn));
            }
            _ => {}
        }
    }
    if !turn.is_empty() {
        turns.push(turn);
    }
    // The most recent turns first, until the cap, then back in order.
    let mut kept: Vec<String> = Vec::new();
    let mut size = 0;
    for turn in turns.into_iter().rev() {
        if size + turn.len() > RECORD_CAP {
            break;
        }
        size += turn.len() + 1;
        kept.push(turn);
    }
    kept.reverse();
    kept.join("\n")
}

fn texts(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `text` cut at [`TURN_TEXT_CAP`] bytes on a character boundary.
fn capped(text: &str) -> String {
    if text.len() <= TURN_TEXT_CAP {
        return text.to_owned();
    }
    let mut end = TURN_TEXT_CAP;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}
