//! Canonical `Message` <-> OpenAI chat message translation.
//!
//! What leaks and where it goes: tool calls are a separate field on the
//! assistant message; tool results are their own `tool` role messages;
//! images are `image_url` parts with data URLs; `ProviderBlob`s stamped
//! with this adapter's name are flattened back into the assistant message
//! (`reasoning_content` and friends); blobs from other adapters are dropped.
//! Author names ride on the optional `name` field.
//!
//! Tool results are translated one way only, log to wire. `is_error` has
//! no wire representation; a failed result is sent with its content
//! prefixed by `[error] ` so the model can tell, and nothing reads it back.

use aigentic_core::{
    AgentId, Author, ContentBlock, Image, Message, ProviderBlob, Role, ToolCall, UserId,
};
use serde::{Deserialize, Serialize};

use super::PROVIDER_NAME;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum WireMessage {
    System {
        content: String,
    },
    User {
        content: UserContent,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    Assistant {
        #[serde(default)]
        content: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<WireToolCall>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// Provider-specific fields replayed verbatim (from a `ProviderBlob`).
        #[serde(flatten)]
        extra: serde_json::Map<String, serde_json::Value>,
    },
    Tool {
        tool_call_id: String,
        content: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UserContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text { text: String },
    ImageUrl { image_url: ImageUrl },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageUrl {
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: WireFunctionCall,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireFunctionCall {
    pub name: String,
    /// JSON-encoded arguments, as the API specifies.
    pub arguments: String,
}

fn author_name(author: &Author) -> Option<String> {
    match author {
        Author::User(UserId(id)) | Author::Agent(AgentId(id)) => Some(id.clone()),
        Author::System => None,
    }
}

fn joined_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Prefix on the content of a tool result whose `is_error` is set.
pub const ERROR_PREFIX: &str = "[error] ";

fn tool_result_messages(blocks: &[ContentBlock]) -> impl Iterator<Item = WireMessage> + '_ {
    blocks.iter().filter_map(|b| match b {
        ContentBlock::ToolResult(r) => Some(WireMessage::Tool {
            tool_call_id: r.id.clone(),
            content: if r.is_error {
                format!("{ERROR_PREFIX}{}", r.content)
            } else {
                r.content.clone()
            },
        }),
        _ => None,
    })
}

/// Canonical context to wire messages. One canonical message can become
/// several wire messages (a message carrying tool results).
pub fn to_wire(context: &[Message]) -> Vec<WireMessage> {
    let mut out = Vec::with_capacity(context.len());
    for m in context {
        match m.role {
            Role::System => out.push(WireMessage::System {
                content: joined_text(&m.blocks),
            }),
            Role::User => {
                let parts: Vec<ContentPart> = m
                    .blocks
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text(t) => Some(ContentPart::Text { text: t.clone() }),
                        ContentBlock::Image(i) => Some(ContentPart::ImageUrl {
                            image_url: ImageUrl {
                                url: format!("data:{};base64,{}", i.media_type, i.data),
                            },
                        }),
                        _ => None,
                    })
                    .collect();
                let content = match parts.as_slice() {
                    [ContentPart::Text { text }] => UserContent::Text(text.clone()),
                    [] => UserContent::Text(String::new()),
                    _ => UserContent::Parts(parts),
                };
                out.push(WireMessage::User {
                    content,
                    name: author_name(&m.author),
                });
                out.extend(tool_result_messages(&m.blocks));
            }
            Role::Assistant => {
                let text = joined_text(&m.blocks);
                let tool_calls = m
                    .blocks
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::ToolCall(c) => Some(WireToolCall {
                            id: c.id.clone(),
                            kind: "function".into(),
                            function: WireFunctionCall {
                                name: c.name.clone(),
                                arguments: c.args.to_string(),
                            },
                        }),
                        _ => None,
                    })
                    .collect();
                let mut extra = serde_json::Map::new();
                for b in &m.blocks {
                    if let ContentBlock::ProviderBlob(ProviderBlob { provider, data }) = b
                        && provider == PROVIDER_NAME
                        && let serde_json::Value::Object(fields) = data
                    {
                        extra.extend(fields.clone());
                    }
                }
                out.push(WireMessage::Assistant {
                    content: (!text.is_empty()).then_some(text),
                    tool_calls,
                    name: author_name(&m.author),
                    extra,
                });
                out.extend(tool_result_messages(&m.blocks));
            }
            Role::Tool => out.extend(tool_result_messages(&m.blocks)),
        }
    }
    out
}

fn parse_data_url(url: &str) -> Option<Image> {
    let rest = url.strip_prefix("data:")?;
    let (media_type, data) = rest.split_once(";base64,")?;
    Some(Image {
        media_type: media_type.to_owned(),
        data: data.to_owned(),
    })
}

/// One wire message back to canonical. Returns `None` for `tool` messages:
/// tool results only travel log to wire, never back.
pub fn from_wire(wire: &WireMessage) -> Option<Message> {
    Some(match wire {
        WireMessage::System { content } => Message {
            role: Role::System,
            author: Author::System,
            blocks: vec![ContentBlock::Text(content.clone())],
        },
        WireMessage::User { content, name } => {
            let blocks = match content {
                UserContent::Text(t) => vec![ContentBlock::Text(t.clone())],
                UserContent::Parts(parts) => parts
                    .iter()
                    .map(|p| match p {
                        ContentPart::Text { text } => ContentBlock::Text(text.clone()),
                        ContentPart::ImageUrl { image_url } => parse_data_url(&image_url.url)
                            .map(ContentBlock::Image)
                            .unwrap_or_else(|| ContentBlock::Text(image_url.url.clone())),
                    })
                    .collect(),
            };
            Message {
                role: Role::User,
                author: Author::User(UserId(name.clone().unwrap_or_else(|| "user".into()))),
                blocks,
            }
        }
        WireMessage::Assistant {
            content,
            tool_calls,
            name,
            extra,
        } => {
            let mut blocks = Vec::new();
            if let Some(text) = content
                && !text.is_empty()
            {
                blocks.push(ContentBlock::Text(text.clone()));
            }
            blocks.extend(tool_calls.iter().map(|c| {
                ContentBlock::ToolCall(ToolCall {
                    id: c.id.clone(),
                    name: c.function.name.clone(),
                    args: serde_json::from_str(&c.function.arguments)
                        .unwrap_or(serde_json::Value::String(c.function.arguments.clone())),
                })
            }));
            if !extra.is_empty() {
                blocks.push(ContentBlock::ProviderBlob(ProviderBlob {
                    provider: PROVIDER_NAME.to_owned(),
                    data: serde_json::Value::Object(extra.clone()),
                }));
            }
            Message {
                role: Role::Assistant,
                author: Author::Agent(AgentId(name.clone().unwrap_or_else(|| "assistant".into()))),
                blocks,
            }
        }
        WireMessage::Tool { .. } => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aigentic_core::ToolResult;
    use serde_json::json;

    fn sample() -> Vec<Message> {
        vec![
            Message {
                role: Role::System,
                author: Author::System,
                blocks: vec![ContentBlock::Text("You are terse.".into())],
            },
            Message {
                role: Role::User,
                author: Author::User(UserId("steve".into())),
                blocks: vec![
                    ContentBlock::Text("What is this?".into()),
                    ContentBlock::Image(Image {
                        media_type: "image/png".into(),
                        data: "iVBORw0KGgo=".into(),
                    }),
                ],
            },
            Message {
                role: Role::Assistant,
                author: Author::Agent(AgentId("worker".into())),
                blocks: vec![
                    ContentBlock::Text("Checking.".into()),
                    ContentBlock::ToolCall(ToolCall {
                        id: "call_abc123".into(),
                        name: "read_file".into(),
                        args: json!({"path": "Cargo.toml"}),
                    }),
                    ContentBlock::ProviderBlob(ProviderBlob {
                        provider: PROVIDER_NAME.into(),
                        data: json!({"reasoning_content": "hmm"}),
                    }),
                ],
            },
        ]
    }

    fn tool_results() -> Message {
        Message {
            role: Role::Tool,
            author: Author::System,
            blocks: vec![
                ContentBlock::ToolResult(ToolResult {
                    id: "call_abc123".into(),
                    content: "[workspace]".into(),
                    is_error: false,
                }),
                ContentBlock::ToolResult(ToolResult {
                    id: "call_def456".into(),
                    content: "No such file".into(),
                    is_error: true,
                }),
            ],
        }
    }

    #[test]
    fn canonical_to_wire_json_and_back_round_trips() {
        let original = sample();
        let wire = to_wire(&original);
        let json = serde_json::to_string(&wire).unwrap();
        let parsed: Vec<WireMessage> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, wire);
        let back: Vec<Message> = parsed.iter().filter_map(from_wire).collect();
        assert_eq!(back, original);
    }

    #[test]
    fn tool_results_go_one_way_with_an_error_prefix() {
        let wire = to_wire(&[tool_results()]);
        assert_eq!(
            serde_json::to_value(&wire).unwrap(),
            json!([
                {"role": "tool", "tool_call_id": "call_abc123", "content": "[workspace]"},
                {"role": "tool", "tool_call_id": "call_def456", "content": "[error] No such file"},
            ])
        );
        assert!(wire.iter().all(|w| from_wire(w).is_none()));
    }

    #[test]
    fn wire_shape_matches_the_openai_api() {
        let wire = serde_json::to_value(to_wire(&sample())).unwrap();
        assert_eq!(
            wire[0],
            json!({"role": "system", "content": "You are terse."})
        );
        assert_eq!(wire[1]["role"], "user");
        assert_eq!(wire[1]["name"], "steve");
        assert_eq!(
            wire[1]["content"][0],
            json!({"type": "text", "text": "What is this?"})
        );
        assert_eq!(
            wire[1]["content"][1]["image_url"]["url"],
            "data:image/png;base64,iVBORw0KGgo="
        );
        assert_eq!(wire[2]["role"], "assistant");
        assert_eq!(wire[2]["content"], "Checking.");
        assert_eq!(wire[2]["tool_calls"][0]["id"], "call_abc123");
        assert_eq!(wire[2]["tool_calls"][0]["type"], "function");
        assert_eq!(wire[2]["tool_calls"][0]["function"]["name"], "read_file");
        assert_eq!(
            wire[2]["tool_calls"][0]["function"]["arguments"],
            r#"{"path":"Cargo.toml"}"#
        );
        assert_eq!(wire[2]["reasoning_content"], "hmm");
    }

    #[test]
    fn foreign_blobs_are_dropped_and_plain_text_user_is_a_string() {
        let ctx = vec![
            Message {
                role: Role::User,
                author: Author::User(UserId("steve".into())),
                blocks: vec![ContentBlock::Text("hi".into())],
            },
            Message {
                role: Role::Assistant,
                author: Author::Agent(AgentId("worker".into())),
                blocks: vec![ContentBlock::ProviderBlob(ProviderBlob {
                    provider: "anthropic".into(),
                    data: json!({"thinking": "x"}),
                })],
            },
        ];
        let wire = serde_json::to_value(to_wire(&ctx)).unwrap();
        assert_eq!(wire[0]["content"], "hi");
        assert_eq!(
            wire[1],
            json!({"role": "assistant", "content": null, "name": "worker"})
        );
    }

    #[test]
    fn assistant_without_name_parses() {
        let wire: WireMessage =
            serde_json::from_str(r#"{"role":"assistant","content":"ok"}"#).unwrap();
        let m = from_wire(&wire).unwrap();
        assert_eq!(m.author, Author::Agent(AgentId("assistant".into())));
        assert_eq!(m.blocks, vec![ContentBlock::Text("ok".into())]);
    }
}
