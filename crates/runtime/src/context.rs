use std::collections::BTreeSet;

use aigentic_core::{Author, ContentBlock, Event, Message, Role};
use aigentic_log::{LogError, Projection, project};

/// The stable prefix, in the order the PRD fixes: global instructions,
/// project instructions, knowledge, memory, then (from the log) pinned
/// facts, then the skills block. Every block is one system message.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Prefix<'a> {
    pub global: Option<&'a str>,
    /// The harness's own standing instructions (phase 6 step 8c): how to
    /// use its tools. Fixed text, after the person's global block.
    pub harness: Option<&'a str>,
    pub project: Option<&'a str>,
    /// One line under the project instructions when the project names
    /// participants (phase 5): who is in it and their roles, so the
    /// model knows whom it may ask to approve.
    pub participants: Option<String>,
    pub knowledge: Option<String>,
    pub memory: Option<String>,
    pub skills: Option<String>,
}

impl<'a> Prefix<'a> {
    /// A prefix with only global instructions; what phase 0 to 3 built.
    pub fn instructions(text: Option<&'a str>) -> Self {
        Self {
            global: text,
            ..Self::default()
        }
    }
}

/// Model context, in cache-friendly order: the prefix blocks, the pinned
/// facts, the skills block, then the thread body projected from the log
/// with compaction applied, oldest to newest.
///
/// Names (phase 5): when the log holds more than one distinct human
/// author, every user message's text is prefixed `<name>: ` so the model
/// can tell people apart and address them. Counted over the whole log,
/// so the prefix is stable once a second person has written; a
/// single-human thread projects exactly as before.
pub fn build_context(prefix: &Prefix<'_>, events: &[Event]) -> Result<Vec<Message>, LogError> {
    let Projection {
        pinned, mut body, ..
    } = project(events)?;
    if human_authors(events).len() > 1 {
        for message in &mut body {
            let name = match (&message.role, &message.author) {
                (Role::User, Author::User(user)) => user.0.clone(),
                _ => continue,
            };
            name_message(message, &name);
        }
    }
    let mut context = Vec::with_capacity(body.len() + 7);
    if let Some(text) = prefix.global {
        context.push(system(text.to_owned()));
    }
    if let Some(text) = prefix.harness {
        context.push(system(text.to_owned()));
    }
    if let Some(text) = prefix.project {
        context.push(system(text.to_owned()));
    }
    if let Some(text) = &prefix.participants {
        context.push(system(text.clone()));
    }
    if let Some(text) = &prefix.knowledge {
        context.push(system(text.clone()));
    }
    if let Some(text) = &prefix.memory {
        context.push(system(text.clone()));
    }
    if !pinned.is_empty() {
        let facts = pinned
            .iter()
            .map(|p| format!("- {p}"))
            .collect::<Vec<_>>()
            .join("\n");
        context.push(system(format!("Pinned facts:\n{facts}")));
    }
    if let Some(text) = &prefix.skills {
        context.push(system(text.clone()));
    }
    context.extend(body);
    Ok(context)
}

/// The distinct human authors in the log.
pub fn human_authors(events: &[Event]) -> BTreeSet<String> {
    events
        .iter()
        .filter_map(|e| match &e.author {
            Author::User(u) => Some(u.0.clone()),
            _ => None,
        })
        .collect()
}

/// `name: ` before the message's first text block, or as a new first
/// block when it has none.
fn name_message(message: &mut Message, name: &str) {
    match message
        .blocks
        .iter_mut()
        .find(|b| matches!(b, ContentBlock::Text(_)))
    {
        Some(ContentBlock::Text(t)) => *t = format!("{name}: {t}"),
        _ => message
            .blocks
            .insert(0, ContentBlock::Text(format!("{name}:"))),
    }
}

fn system(text: String) -> Message {
    Message {
        role: Role::System,
        author: Author::System,
        blocks: vec![ContentBlock::Text(text)],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aigentic_core::{EventKind, UserId};
    use serde_json::json;
    use time::OffsetDateTime;
    use ulid::Ulid;

    fn ev(seq: u64, kind: EventKind, payload: serde_json::Value) -> Event {
        Event {
            id: Ulid::generate(),
            thread_id: Ulid::generate(),
            seq,
            kind,
            author: Author::User(UserId("steve".into())),
            payload,
            parent_event: None,
            created_at: OffsetDateTime::now_utc(),
        }
    }

    fn text(m: &Message) -> &str {
        match &m.blocks[0] {
            ContentBlock::Text(t) => t,
            _ => panic!("text"),
        }
    }

    #[test]
    fn prefix_order_is_global_project_knowledge_memory_pins_skills_body() {
        let events = vec![
            ev(0, EventKind::Pinned, json!({"text": "Answer in Swedish."})),
            ev(
                1,
                EventKind::UserMessage,
                json!({"blocks": [{"type": "text", "text": "hej"}]}),
            ),
            ev(2, EventKind::Pinned, json!({"text": "Repo is aigentic."})),
        ];
        let prefix = Prefix {
            global: Some("You are terse."),
            harness: Some("Keep a checklist."),
            project: Some("This is Vendela."),
            participants: None,
            knowledge: Some("# Knowledge\n\n...".into()),
            memory: Some("# Project memory\n\n- Use Swedish.".into()),
            skills: Some("# Skills\n\n- tdd: x".into()),
        };
        let ctx = build_context(&prefix, &events).unwrap();
        let texts: Vec<&str> = ctx.iter().map(text).collect();
        assert_eq!(
            texts,
            vec![
                "You are terse.",
                "Keep a checklist.",
                "This is Vendela.",
                "# Knowledge\n\n...",
                "# Project memory\n\n- Use Swedish.",
                "Pinned facts:\n- Answer in Swedish.\n- Repo is aigentic.",
                "# Skills\n\n- tdd: x",
                "hej"
            ]
        );
        assert!(ctx[..7].iter().all(|m| m.role == Role::System));
        assert_eq!(ctx[7].role, Role::User);

        let ctx = build_context(&Prefix::default(), &events[1..2]).unwrap();
        assert_eq!(ctx.len(), 1, "no prefix, no pins: body only");
        let ctx = build_context(&Prefix::instructions(Some("Be terse.")), &events[1..2]).unwrap();
        assert_eq!(text(&ctx[0]), "Be terse.");
    }

    fn by(seq: u64, who: &str, text: &str) -> Event {
        let mut e = ev(
            seq,
            EventKind::UserMessage,
            json!({"blocks": [{"type": "text", "text": text}]}),
        );
        e.author = Author::User(UserId(who.into()));
        e
    }

    #[test]
    fn names_appear_only_once_a_second_human_has_written() {
        // One human: byte for byte as before.
        let solo = vec![by(0, "steve", "hej")];
        let ctx = build_context(&Prefix::default(), &solo).unwrap();
        assert_eq!(text(&ctx[0]), "hej");
        assert_eq!(ctx[0].author, Author::User(UserId("steve".into())));

        // Two humans: every user message is named, the author kept, and
        // system-authored user-role messages (a summary, a note) are not.
        let mut two = vec![by(0, "steve", "hej"), by(1, "magnus", "hi")];
        two.push(Event {
            author: Author::System,
            ..ev(
                2,
                EventKind::Interrupted,
                json!({"reason": "process exited mid-turn", "after_seq": 1}),
            )
        });
        two.push(by(3, "steve", "again"));
        let ctx = build_context(&Prefix::default(), &two).unwrap();
        let texts: Vec<&str> = ctx.iter().map(text).collect();
        assert_eq!(texts[0], "steve: hej");
        assert_eq!(texts[1], "magnus: hi");
        assert!(
            texts[2].starts_with("[The previous turn was interrupted"),
            "{}",
            texts[2]
        );
        assert_eq!(texts[3], "steve: again");
        assert_eq!(ctx[1].author, Author::User(UserId("magnus".into())));
        assert_eq!(
            human_authors(&two).into_iter().collect::<Vec<_>>(),
            vec!["magnus", "steve"]
        );

        // A message without a text block gets one.
        let mut m = Message {
            role: Role::User,
            author: Author::User(UserId("x".into())),
            blocks: vec![ContentBlock::ProviderBlob(aigentic_core::ProviderBlob {
                provider: "test".into(),
                data: json!({}),
            })],
        };
        name_message(&mut m, "magnus");
        assert_eq!(m.blocks.len(), 2);
        assert!(matches!(&m.blocks[0], ContentBlock::Text(t) if t == "magnus:"));
    }

    #[test]
    fn the_participants_line_sits_under_the_project_instructions() {
        let events = vec![by(0, "steve", "hej")];
        let prefix = Prefix {
            global: Some("g"),
            project: Some("p"),
            participants: Some(
                "Participants in this project: magnus (approve), steve (admin)".into(),
            ),
            ..Prefix::default()
        };
        let ctx = build_context(&prefix, &events).unwrap();
        let texts: Vec<&str> = ctx.iter().map(text).collect();
        assert_eq!(
            texts,
            vec![
                "g",
                "p",
                "Participants in this project: magnus (approve), steve (admin)",
                "hej"
            ]
        );
        assert_eq!(ctx[2].role, Role::System);
    }
}
