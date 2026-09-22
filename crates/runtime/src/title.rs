//! Thread titles (phase 6 step 9): once threads cross projects, "the
//! vendela thread" stops meaning anything, so every thread gets a title
//! after its first turn, proposed by the utility model and recorded as a
//! `thread_renamed` event; `/rename` overrides it.

use aigentic_core::{
    Author, CompletionRequest, ContentBlock, Event, EventKind, Message, Provider, ProviderEvent,
    Role,
};
use aigentic_log::{
    AssistantMessagePayload, ThreadRenamedPayload, TurnEndedPayload, UserMessagePayload,
};
use futures_util::StreamExt;

use crate::RuntimeError;

/// A title is at most this many characters.
pub const TITLE_CHARS: usize = 60;
/// How much of the first message and reply the prompt carries.
const EXCERPT_CHARS: usize = 1500;

const TITLE_PROMPT: &str = "You name conversation threads. Reply with a title of three to six words that says what the thread is about, in the language the person wrote in. No quotes, no punctuation at the end, nothing else.";

/// The last `thread_renamed` title in `events`.
pub fn title_of(events: &[Event]) -> Option<String> {
    events
        .iter()
        .rev()
        .filter(|e| e.kind == EventKind::ThreadRenamed)
        .find_map(|e| serde_json::from_value::<ThreadRenamedPayload>(e.payload.clone()).ok())
        .map(|p| p.title)
}

/// A turn ended `done`.
pub fn has_finished_turn(events: &[Event]) -> bool {
    events.iter().any(|e| {
        e.kind == EventKind::TurnEnded
            && serde_json::from_value::<TurnEndedPayload>(e.payload.clone())
                .is_ok_and(|p| p.reason == "done")
    })
}

/// One line, no wrapping quotes or trailing full stop, at most
/// `TITLE_CHARS`.
pub fn clean(raw: &str) -> String {
    let line = raw.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let line = line
        .trim()
        .trim_start_matches(['"', '\'', '`', '*', '#', ' '])
        .trim_end_matches(['"', '\'', '`', '*', '.', ' '])
        .trim_start_matches("Title:")
        .trim();
    let mut out: String = line.chars().take(TITLE_CHARS).collect();
    if line.chars().count() > TITLE_CHARS {
        out = out.trim_end().to_owned();
        out.push('…');
    }
    out
}

fn excerpt(text: &str) -> String {
    let mut s: String = text.chars().take(EXCERPT_CHARS).collect();
    if text.chars().count() > EXCERPT_CHARS {
        s.push('…');
    }
    s
}

fn first_text(events: &[Event], kind: EventKind) -> Option<String> {
    let event = events.iter().find(|e| e.kind == kind)?;
    let blocks = match kind {
        EventKind::UserMessage => {
            serde_json::from_value::<UserMessagePayload>(event.payload.clone())
                .ok()?
                .blocks
        }
        _ => {
            serde_json::from_value::<AssistantMessagePayload>(event.payload.clone())
                .ok()?
                .blocks
        }
    };
    blocks.into_iter().find_map(|b| match b {
        ContentBlock::Text(t) if !t.trim().is_empty() => Some(t),
        _ => None,
    })
}

/// Ask `provider` for a title from the first message and the first
/// reply. Empty when there is no first message.
pub async fn propose_title(
    provider: &dyn Provider,
    events: &[Event],
) -> Result<String, RuntimeError> {
    let Some(asked) = first_text(events, EventKind::UserMessage) else {
        return Ok(String::new());
    };
    let answered = first_text(events, EventKind::AssistantMessage).unwrap_or_default();
    let messages = vec![
        Message {
            role: Role::System,
            author: Author::System,
            blocks: vec![ContentBlock::Text(TITLE_PROMPT.into())],
        },
        Message {
            role: Role::User,
            author: Author::System,
            blocks: vec![ContentBlock::Text(format!(
                "The person wrote:\n{}\n\nThe assistant replied:\n{}\n\nThe title:",
                excerpt(&asked),
                excerpt(&answered)
            ))],
        },
    ];
    let request = CompletionRequest {
        messages: &messages,
        tools: &[],
        // Room for reasoning first: GLM 5.3 Flash spent 40 tokens on it and
        // returned no title; at 400 it answered in about 150.
        max_output_tokens: Some(512),
    };
    let mut text = String::new();
    let mut stream = provider.complete(&request);
    while let Some(event) = stream.next().await {
        match event {
            ProviderEvent::TextDelta(t) => text.push_str(&t),
            ProviderEvent::Error(e) => return Err(RuntimeError::Provider(e)),
            _ => {}
        }
    }
    Ok(clean(&text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_are_cleaned_to_one_short_line() {
        assert_eq!(
            clean("\"Crate dependency comparison.\"\n"),
            "Crate dependency comparison"
        );
        assert_eq!(clean("Title: Harness status check"), "Harness status check");
        assert_eq!(clean("**Bold title**"), "Bold title");
        let long = "word ".repeat(30);
        let c = clean(&long);
        assert!(c.chars().count() <= TITLE_CHARS + 1, "{c}");
        assert!(c.ends_with('…'));
        assert_eq!(clean("\n\n"), "");
    }
}
