use serde::{Deserialize, Serialize};

/// The conversational role of a [`Message`](crate::Message).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
    System,
    /// Wire-format concept: only the providers crate may branch on this variant.
    Tool,
}
