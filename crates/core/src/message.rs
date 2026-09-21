use serde::{Deserialize, Serialize};

use crate::{Author, ContentBlock, Role};

/// The canonical message every provider adapter translates to and from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    /// Multiplayer attribution, even in single-user phase 0.
    pub author: Author,
    pub blocks: Vec<ContentBlock>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::tests::all_blocks;
    use crate::{AgentId, UserId};

    #[test]
    fn message_with_every_block_round_trips() {
        let message = Message {
            role: Role::Assistant,
            author: Author::Agent(AgentId("worker".into())),
            blocks: all_blocks(),
        };
        let json = serde_json::to_string(&message).unwrap();
        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(back, message);
    }

    #[test]
    fn every_role_round_trips() {
        for role in [Role::User, Role::Assistant, Role::System, Role::Tool] {
            let message = Message {
                role,
                author: Author::User(UserId("steve".into())),
                blocks: vec![ContentBlock::Text("x".into())],
            };
            let json = serde_json::to_string(&message).unwrap();
            assert_eq!(serde_json::from_str::<Message>(&json).unwrap(), message);
        }
        assert_eq!(serde_json::to_string(&Role::Tool).unwrap(), r#""tool""#);
    }
}
