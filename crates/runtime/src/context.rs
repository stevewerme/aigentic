use aigentic_core::{Author, ContentBlock, Event, Message, Role};
use aigentic_log::{LogError, Projection, project};

/// Model context, in cache-friendly order: the stable prefix first
/// (repository instructions, then pinned facts, each as a system message;
/// project instructions and knowledge join it in phase 4), then the thread
/// body projected from the log with compaction applied, oldest to newest.
pub fn build_context(
    instructions: Option<&str>,
    events: &[Event],
) -> Result<Vec<Message>, LogError> {
    let Projection { pinned, body, .. } = project(events)?;
    let mut context = Vec::with_capacity(body.len() + 2);
    if let Some(text) = instructions {
        context.push(system(text.to_owned()));
    }
    if !pinned.is_empty() {
        let facts = pinned
            .iter()
            .map(|p| format!("- {p}"))
            .collect::<Vec<_>>()
            .join("\n");
        context.push(system(format!("Pinned facts:\n{facts}")));
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

    #[test]
    fn prefix_is_instructions_then_pins_then_body() {
        let events = vec![
            ev(0, EventKind::Pinned, json!({"text": "Answer in Swedish."})),
            ev(
                1,
                EventKind::UserMessage,
                json!({"blocks": [{"type": "text", "text": "hej"}]}),
            ),
            ev(2, EventKind::Pinned, json!({"text": "Repo is aigentic."})),
        ];
        let ctx = build_context(Some("Be terse."), &events).unwrap();
        assert_eq!(ctx.len(), 3);
        assert_eq!(ctx[0].role, Role::System);
        assert_eq!(ctx[0].blocks, vec![ContentBlock::Text("Be terse.".into())]);
        assert_eq!(
            ctx[1].blocks,
            vec![ContentBlock::Text(
                "Pinned facts:\n- Answer in Swedish.\n- Repo is aigentic.".into()
            )]
        );
        assert_eq!(ctx[2].role, Role::User);

        let ctx = build_context(None, &events[1..2]).unwrap();
        assert_eq!(ctx.len(), 1, "no instructions, no pins: body only");
    }
}
