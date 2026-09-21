use aigentic_core::{Author, ContentBlock, Event, Message, Role};
use aigentic_log::{LogError, Projection, project};

/// The stable prefix, in the order the PRD fixes: global instructions,
/// project instructions, knowledge, memory, then (from the log) pinned
/// facts, then the skills block. Every block is one system message.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Prefix<'a> {
    pub global: Option<&'a str>,
    pub project: Option<&'a str>,
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
pub fn build_context(prefix: &Prefix<'_>, events: &[Event]) -> Result<Vec<Message>, LogError> {
    let Projection { pinned, body, .. } = project(events)?;
    let mut context = Vec::with_capacity(body.len() + 6);
    if let Some(text) = prefix.global {
        context.push(system(text.to_owned()));
    }
    if let Some(text) = prefix.project {
        context.push(system(text.to_owned()));
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
            project: Some("This is Vendela."),
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
                "This is Vendela.",
                "# Knowledge\n\n...",
                "# Project memory\n\n- Use Swedish.",
                "Pinned facts:\n- Answer in Swedish.\n- Repo is aigentic.",
                "# Skills\n\n- tdd: x",
                "hej"
            ]
        );
        assert!(ctx[..6].iter().all(|m| m.role == Role::System));
        assert_eq!(ctx[6].role, Role::User);

        let ctx = build_context(&Prefix::default(), &events[1..2]).unwrap();
        assert_eq!(ctx.len(), 1, "no prefix, no pins: body only");
        let ctx = build_context(&Prefix::instructions(Some("Be terse.")), &events[1..2]).unwrap();
        assert_eq!(text(&ctx[0]), "Be terse.");
    }
}
