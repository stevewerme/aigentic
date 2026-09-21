use aigentic_core::{Author, ContentBlock, Event, Message, Role};
use aigentic_log::{LogError, project};

/// Model context, in cache-friendly order: the stable prefix first
/// (repository instructions as a system message; project instructions,
/// knowledge and pinned facts join it in phase 4), then the thread body
/// projected from the log, oldest to newest.
pub fn build_context(
    instructions: Option<&str>,
    events: &[Event],
) -> Result<Vec<Message>, LogError> {
    let mut context = Vec::with_capacity(events.len() + 1);
    if let Some(text) = instructions {
        context.push(Message {
            role: Role::System,
            author: Author::System,
            blocks: vec![ContentBlock::Text(text.to_owned())],
        });
    }
    context.extend(project(events)?);
    Ok(context)
}
