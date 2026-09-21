use serde::{Deserialize, Serialize};

/// Identifier of a human participant.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UserId(pub String);

/// Identifier of an agent participant (for example `"orchestrator"`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentId(pub String);

/// Who produced a message or event. Present from phase 0 so multiplayer
/// attribution never needs a log migration.
///
/// Serialises as `{"kind":"user","id":"steve"}`, `{"kind":"agent","id":"orchestrator"}`
/// or `{"kind":"system"}`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(from = "AuthorWire", into = "AuthorWire")]
pub enum Author {
    User(UserId),
    Agent(AgentId),
    System,
}

/// Wire shape. serde's internal tagging cannot wrap a bare string in a newtype
/// variant, so the `id` field is spelled out here and converted both ways.
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum AuthorWire {
    User { id: UserId },
    Agent { id: AgentId },
    System,
}

impl From<Author> for AuthorWire {
    fn from(author: Author) -> Self {
        match author {
            Author::User(id) => AuthorWire::User { id },
            Author::Agent(id) => AuthorWire::Agent { id },
            Author::System => AuthorWire::System,
        }
    }
}

impl From<AuthorWire> for Author {
    fn from(wire: AuthorWire) -> Self {
        match wire {
            AuthorWire::User { id } => Author::User(id),
            AuthorWire::Agent { id } => Author::Agent(id),
            AuthorWire::System => Author::System,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_serialises_with_kind_and_id() {
        let author = Author::Agent(AgentId("orchestrator".into()));
        let json = serde_json::to_string(&author).unwrap();
        assert_eq!(json, r#"{"kind":"agent","id":"orchestrator"}"#);
        assert_eq!(serde_json::from_str::<Author>(&json).unwrap(), author);
    }

    #[test]
    fn user_and_system_round_trip() {
        for author in [Author::User(UserId("steve".into())), Author::System] {
            let json = serde_json::to_string(&author).unwrap();
            assert_eq!(serde_json::from_str::<Author>(&json).unwrap(), author);
        }
        assert_eq!(
            serde_json::to_string(&Author::System).unwrap(),
            r#"{"kind":"system"}"#
        );
    }
}
