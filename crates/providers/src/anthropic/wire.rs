//! Canonical context to Messages API request. One way only: the stream
//! translator builds canonical blocks directly, and tool results never
//! come back from the wire.
//!
//! What leaks and where it goes: tool calls are `tool_use` content blocks
//! on the assistant message; tool results are `tool_result` blocks inside
//! a **user** message, all results for one assistant message in a single
//! user message; images are base64 sources; `ProviderBlob`s stamped with
//! this adapter's name (thinking blocks) are replayed verbatim in their
//! original position and blobs from other adapters are dropped; the
//! leading run of system messages becomes the top-level `system` array;
//! authors are not represented (no `name` field on this API).

use aigentic_core::{CompletionRequest, ContentBlock, Message, Role};
use serde::Serialize;
use serde_json::{Value, json};

use super::{AnthropicConfig, PROVIDER_NAME, Thinking};

#[derive(Debug, Clone, Serialize)]
pub struct MessagesRequest {
    pub model: String,
    pub max_tokens: u64,
    pub stream: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub system: Vec<Value>,
    pub messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<WireTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_config: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WireMessage {
    pub role: &'static str,
    /// Content blocks as JSON objects; heterogeneous and, for blobs, opaque.
    pub content: Vec<Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WireTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

fn cache_control() -> Value {
    json!({"type": "ephemeral"})
}

/// Build the request. Breakpoints, when `config.cache` is set: one on the
/// last system block (caches tools and system together, since tools render
/// first) and one on the last block of the last user message.
pub fn to_wire(request: &CompletionRequest<'_>, config: &AnthropicConfig) -> MessagesRequest {
    let messages = request.messages;
    let leading_system = messages
        .iter()
        .take_while(|m| m.role == Role::System)
        .count();

    let mut system: Vec<Value> = messages[..leading_system]
        .iter()
        .map(|m| json!({"type": "text", "text": joined_text(&m.blocks)}))
        .collect();

    let mut out: Vec<WireMessage> = Vec::new();
    for m in &messages[leading_system..] {
        for (role, blocks) in translate(m) {
            if blocks.is_empty() {
                continue;
            }
            match out.last_mut() {
                // The API requires alternation: same-role neighbours merge.
                Some(prev) if prev.role == role => prev.content.extend(blocks),
                _ => out.push(WireMessage {
                    role,
                    content: blocks,
                }),
            }
        }
    }

    if config.cache {
        if let Some(last) = system.last_mut() {
            last["cache_control"] = cache_control();
        }
        if let Some(last) = out.last_mut()
            && last.role == "user"
            && let Some(block) = last.content.last_mut()
        {
            block["cache_control"] = cache_control();
        }
    }

    MessagesRequest {
        model: config.model.clone(),
        max_tokens: request
            .max_output_tokens
            .unwrap_or(config.max_output_tokens),
        stream: true,
        system,
        messages: out,
        tools: request
            .tools
            .iter()
            .map(|t| WireTool {
                name: t.name.clone(),
                description: t.description.clone(),
                input_schema: t.schema.clone(),
            })
            .collect(),
        thinking: match config.thinking {
            Thinking::Adaptive => Some(json!({"type": "adaptive"})),
            Thinking::Off => None,
        },
        output_config: config.effort.as_ref().map(|e| json!({"effort": e})),
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

fn tool_result_blocks(blocks: &[ContentBlock]) -> Vec<Value> {
    blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolResult(r) => Some(json!({
                "type": "tool_result",
                "tool_use_id": r.id,
                "content": r.content,
                "is_error": r.is_error,
            })),
            _ => None,
        })
        .collect()
}

/// One canonical message to one or two wire messages (a message carrying
/// tool results yields a trailing user message for them).
fn translate(m: &Message) -> Vec<(&'static str, Vec<Value>)> {
    match m.role {
        // A system message after the body starts has no place on this API
        // until phase 5 renders it into the prefix; dropped.
        Role::System => Vec::new(),
        Role::User => {
            let mut blocks: Vec<Value> = m
                .blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text(t) => Some(json!({"type": "text", "text": t})),
                    ContentBlock::Image(i) => Some(json!({
                        "type": "image",
                        "source": {"type": "base64", "media_type": i.media_type, "data": i.data},
                    })),
                    _ => None,
                })
                .collect();
            blocks.extend(tool_result_blocks(&m.blocks));
            vec![("user", blocks)]
        }
        Role::Assistant => {
            let blocks: Vec<Value> = m
                .blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text(t) => Some(json!({"type": "text", "text": t})),
                    ContentBlock::ToolCall(c) => Some(json!({
                        "type": "tool_use", "id": c.id, "name": c.name, "input": c.args,
                    })),
                    ContentBlock::ProviderBlob(blob)
                        if blob.provider == PROVIDER_NAME && blob.data.is_object() =>
                    {
                        Some(blob.data.clone())
                    }
                    _ => None,
                })
                .collect();
            vec![
                ("assistant", blocks),
                ("user", tool_result_blocks(&m.blocks)),
            ]
        }
        Role::Tool => vec![("user", tool_result_blocks(&m.blocks))],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aigentic_core::{
        AgentId, Author, Image, ProviderBlob, ToolCall, ToolResult, ToolSpec, UserId,
    };

    fn msg(role: Role, blocks: Vec<ContentBlock>) -> Message {
        let author = match role {
            Role::User => Author::User(UserId("steve".into())),
            Role::Assistant => Author::Agent(AgentId("worker".into())),
            _ => Author::System,
        };
        Message {
            role,
            author,
            blocks,
        }
    }

    fn thinking_blob() -> ContentBlock {
        ContentBlock::ProviderBlob(ProviderBlob {
            provider: PROVIDER_NAME.into(),
            data: json!({"type": "thinking", "thinking": "", "signature": "sig123"}),
        })
    }

    fn sample() -> Vec<Message> {
        vec![
            msg(Role::System, vec![ContentBlock::Text("Be terse.".into())]),
            msg(
                Role::User,
                vec![
                    ContentBlock::Text("What is this?".into()),
                    ContentBlock::Image(Image {
                        media_type: "image/png".into(),
                        data: "iVBORw0KGgo=".into(),
                    }),
                ],
            ),
            msg(
                Role::Assistant,
                vec![
                    thinking_blob(),
                    ContentBlock::Text("Checking.".into()),
                    ContentBlock::ToolCall(ToolCall {
                        id: "toolu_1".into(),
                        name: "read_file".into(),
                        args: json!({"path": "Cargo.toml"}),
                    }),
                    ContentBlock::ToolCall(ToolCall {
                        id: "toolu_2".into(),
                        name: "bash".into(),
                        args: json!({"command": "ls"}),
                    }),
                    ContentBlock::ProviderBlob(ProviderBlob {
                        provider: "openai_compat".into(),
                        data: json!({"reasoning_content": "hmm"}),
                    }),
                ],
            ),
            msg(
                Role::Tool,
                vec![ContentBlock::ToolResult(ToolResult {
                    id: "toolu_1".into(),
                    content: "[workspace]".into(),
                    is_error: false,
                })],
            ),
            msg(
                Role::Tool,
                vec![ContentBlock::ToolResult(ToolResult {
                    id: "toolu_2".into(),
                    content: "No such file".into(),
                    is_error: true,
                })],
            ),
        ]
    }

    fn build(messages: &[Message], config: &AnthropicConfig) -> Value {
        let tools = [ToolSpec {
            name: "read_file".into(),
            description: "Read".into(),
            schema: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        }];
        let request = CompletionRequest {
            messages,
            tools: &tools,
            max_output_tokens: None,
        };
        serde_json::to_value(to_wire(&request, config)).unwrap()
    }

    #[test]
    fn wire_shape_matches_the_messages_api() {
        let config = AnthropicConfig::new("k", "claude-opus-5").with_effort("high");
        let w = build(&sample(), &config);

        assert_eq!(w["model"], "claude-opus-5");
        assert_eq!(w["max_tokens"], 64_000);
        assert_eq!(w["stream"], true);
        assert_eq!(w["thinking"], json!({"type": "adaptive"}));
        assert_eq!(w["output_config"], json!({"effort": "high"}));
        assert_eq!(w["tools"][0]["name"], "read_file");
        assert_eq!(w["tools"][0]["input_schema"]["type"], "object");

        // System array with the first breakpoint.
        assert_eq!(w["system"][0]["type"], "text");
        assert_eq!(w["system"][0]["text"], "Be terse.");
        assert_eq!(
            w["system"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );

        let m = w["messages"].as_array().unwrap();
        assert_eq!(
            m.len(),
            3,
            "user, assistant, user (merged tool results): {m:#?}"
        );
        assert_eq!(m[0]["role"], "user");
        assert_eq!(
            m[0]["content"][0],
            json!({"type": "text", "text": "What is this?"})
        );
        assert_eq!(m[0]["content"][1]["type"], "image");
        assert_eq!(m[0]["content"][1]["source"]["media_type"], "image/png");

        assert_eq!(m[1]["role"], "assistant");
        let a = m[1]["content"].as_array().unwrap();
        assert_eq!(
            a.len(),
            4,
            "own blob, text, two tool_use; foreign blob dropped: {a:#?}"
        );
        assert_eq!(
            a[0],
            json!({"type": "thinking", "thinking": "", "signature": "sig123"})
        );
        assert_eq!(a[1], json!({"type": "text", "text": "Checking."}));
        assert_eq!(
            a[2],
            json!({"type": "tool_use", "id": "toolu_1", "name": "read_file", "input": {"path": "Cargo.toml"}})
        );
        assert_eq!(a[3]["id"], "toolu_2");

        assert_eq!(m[2]["role"], "user");
        let r = m[2]["content"].as_array().unwrap();
        assert_eq!(r.len(), 2, "both tool results in one user message");
        assert_eq!(r[0]["type"], "tool_result");
        assert_eq!(r[0]["tool_use_id"], "toolu_1");
        assert_eq!(r[0]["is_error"], false);
        assert_eq!(r[1]["tool_use_id"], "toolu_2");
        assert_eq!(r[1]["content"], "No such file");
        assert_eq!(r[1]["is_error"], true, "is_error is native here, no prefix");
        assert_eq!(
            r[1]["cache_control"],
            json!({"type": "ephemeral"}),
            "second breakpoint"
        );
        assert!(r[0].get("cache_control").is_none());
    }

    #[test]
    fn exactly_two_breakpoints_or_none() {
        let count = |w: &Value| w.to_string().matches("cache_control").count();
        let on = build(&sample(), &AnthropicConfig::new("k", "m"));
        assert_eq!(count(&on), 2);
        let off = build(&sample(), &AnthropicConfig::new("k", "m").with_cache(false));
        assert_eq!(count(&off), 0);
    }

    #[test]
    fn no_breakpoint_on_a_trailing_assistant_message() {
        let messages = vec![
            msg(Role::User, vec![ContentBlock::Text("hi".into())]),
            msg(Role::Assistant, vec![ContentBlock::Text("hello".into())]),
        ];
        let w = build(&messages, &AnthropicConfig::new("k", "m"));
        assert_eq!(w.to_string().matches("cache_control").count(), 0);
        assert!(w.get("system").is_none());
    }

    #[test]
    fn thinking_off_omits_the_field_and_request_cap_wins() {
        let config = AnthropicConfig::new("k", "m").with_thinking(Thinking::Off);
        let messages = vec![msg(Role::User, vec![ContentBlock::Text("hi".into())])];
        let request = CompletionRequest {
            messages: &messages,
            tools: &[],
            max_output_tokens: Some(512),
        };
        let w = serde_json::to_value(to_wire(&request, &config)).unwrap();
        assert!(w.get("thinking").is_none());
        assert!(w.get("output_config").is_none());
        assert!(w.get("tools").is_none());
        assert_eq!(w["max_tokens"], 512);
    }

    #[test]
    fn same_role_neighbours_merge_and_empty_messages_vanish() {
        let messages = vec![
            msg(Role::User, vec![ContentBlock::Text("a".into())]),
            msg(Role::User, vec![ContentBlock::Text("b".into())]),
            msg(
                Role::Assistant,
                vec![ContentBlock::ProviderBlob(ProviderBlob {
                    provider: "openai_compat".into(),
                    data: json!({"reasoning_content": "x"}),
                })],
            ),
            msg(Role::User, vec![ContentBlock::Text("c".into())]),
        ];
        let w = build(&messages, &AnthropicConfig::new("k", "m").with_cache(false));
        let m = w["messages"].as_array().unwrap();
        assert_eq!(m.len(), 1, "{m:#?}");
        assert_eq!(m[0]["content"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn only_the_leading_system_run_becomes_system() {
        let messages = vec![
            msg(Role::System, vec![ContentBlock::Text("one".into())]),
            msg(Role::System, vec![ContentBlock::Text("two".into())]),
            msg(Role::User, vec![ContentBlock::Text("hi".into())]),
            msg(Role::System, vec![ContentBlock::Text("late".into())]),
        ];
        let w = build(&messages, &AnthropicConfig::new("k", "m"));
        let s = w["system"].as_array().unwrap();
        assert_eq!(s.len(), 2);
        assert!(s[0].get("cache_control").is_none());
        assert_eq!(s[1]["cache_control"], json!({"type": "ephemeral"}));
        assert!(!w["messages"].to_string().contains("late"));
    }
}
