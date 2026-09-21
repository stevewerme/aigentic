//! OpenAI-compatible chat completions adapter. Covers vLLM, llama.cpp,
//! Mistral and most EU hosts: anything that serves `POST /v1/chat/completions`
//! with `stream: true`.

mod sse;
mod stream;
mod wire;

use std::pin::Pin;

use aigentic_core::{
    Capabilities, ContentBlock, Message, Provider, ProviderError, ProviderEvent, Tool,
};
use futures_core::Stream;
use futures_util::StreamExt;
use serde::Serialize;

pub use sse::SseParser;
pub use stream::{Translator, parse_stream};
pub use wire::{WireMessage, from_wire, to_wire};

/// Name stamped on `ProviderBlob`s this adapter produces; only blobs with
/// this name are replayed.
pub const PROVIDER_NAME: &str = "openai_compat";

/// Connection settings. Switching backends is a base URL, a key and a model.
#[derive(Debug, Clone)]
pub struct OpenAiCompatConfig {
    /// Base URL up to and including the API version, e.g. `http://127.0.0.1:8080/v1`.
    pub base_url: String,
    pub api_key: Option<String>,
    pub model: String,
    /// Context window advertised through `Capabilities`; the adapter has no
    /// way to discover it.
    pub max_context_tokens: u64,
    pub supports_images: bool,
}

impl OpenAiCompatConfig {
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: None,
            model: model.into(),
            max_context_tokens: 32_768,
            supports_images: false,
        }
    }

    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }

    pub fn with_max_context_tokens(mut self, tokens: u64) -> Self {
        self.max_context_tokens = tokens;
        self
    }

    pub fn with_images(mut self, supported: bool) -> Self {
        self.supports_images = supported;
        self
    }
}

/// A tool as advertised to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    /// JSON Schema for the arguments.
    pub parameters: serde_json::Value,
}

impl ToolDefinition {
    pub fn from_tool(tool: &dyn Tool) -> Self {
        Self {
            name: tool.name().to_owned(),
            description: tool.description().to_owned(),
            parameters: serde_json::to_value(tool.schema()).expect("schema is serialisable"),
        }
    }
}

/// The adapter. Holds the HTTP client, the connection settings and the
/// tool definitions to advertise on every call.
#[derive(Debug, Clone)]
pub struct OpenAiCompat {
    client: reqwest::Client,
    config: OpenAiCompatConfig,
    tools: Vec<ToolDefinition>,
}

impl OpenAiCompat {
    pub fn new(config: OpenAiCompatConfig) -> Self {
        Self {
            client: reqwest::Client::new(),
            config,
            tools: Vec::new(),
        }
    }

    /// Tools to advertise. `Provider::complete` takes only the context, so
    /// the tool list is part of the adapter's configuration.
    pub fn with_tools(mut self, tools: Vec<ToolDefinition>) -> Self {
        self.tools = tools;
        self
    }

    pub fn config(&self) -> &OpenAiCompatConfig {
        &self.config
    }

    /// The request body for `context`, exposed so tests can check the wire
    /// shape without a network.
    pub fn build_request(&self, context: &[Message]) -> ChatRequest {
        ChatRequest {
            model: self.config.model.clone(),
            messages: to_wire(context),
            tools: self
                .tools
                .iter()
                .map(|t| WireTool {
                    kind: "function",
                    function: WireFunctionDef {
                        name: t.name.clone(),
                        description: t.description.clone(),
                        parameters: t.parameters.clone(),
                    },
                })
                .collect(),
            stream: true,
            stream_options: StreamOptions {
                include_usage: true,
            },
        }
    }
}

/// `POST /chat/completions` body.
#[derive(Debug, Clone, Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<WireTool>,
    pub stream: bool,
    pub stream_options: StreamOptions,
}

#[derive(Debug, Clone, Serialize)]
pub struct WireTool {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub function: WireFunctionDef,
}

#[derive(Debug, Clone, Serialize)]
pub struct WireFunctionDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct StreamOptions {
    pub include_usage: bool,
}

type EventStream<'a> = Pin<Box<dyn Stream<Item = ProviderEvent> + Send + 'a>>;

impl Provider for OpenAiCompat {
    fn complete(&self, context: &[Message]) -> EventStream<'_> {
        let body = self.build_request(context);
        let url = format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        );
        let mut request = self.client.post(url).json(&body);
        if let Some(key) = &self.config.api_key {
            request = request.bearer_auth(key);
        }

        let response = async move {
            let events: EventStream<'static> = match request.send().await {
                Err(e) => Box::pin(futures_util::stream::once(async move {
                    ProviderEvent::Error(ProviderError::Transport(e.to_string()))
                })),
                Ok(resp) if !resp.status().is_success() => {
                    let status = resp.status().as_u16();
                    let body = resp.text().await.unwrap_or_default();
                    Box::pin(futures_util::stream::once(async move {
                        ProviderEvent::Error(ProviderError::Http { status, body })
                    }))
                }
                Ok(resp) => Box::pin(parse_stream(resp.bytes_stream())),
            };
            events
        };
        Box::pin(futures_util::stream::once(response).flatten())
    }

    /// A heuristic: about four bytes per token plus a few per message. Real
    /// tokenizers differ per model; compaction thresholds are fractions of
    /// the window, so an estimate is enough for phase 0.
    fn count_tokens(&self, context: &[Message]) -> u64 {
        let bytes: usize = context
            .iter()
            .map(|m| {
                4 + m
                    .blocks
                    .iter()
                    .map(|b| match b {
                        ContentBlock::Text(t) => t.len(),
                        ContentBlock::ToolCall(c) => c.name.len() + c.args.to_string().len(),
                        ContentBlock::ToolResult(r) => r.content.len(),
                        ContentBlock::Image(i) => i.data.len() / 4,
                        ContentBlock::ProviderBlob(b) => b.data.to_string().len(),
                    })
                    .sum::<usize>()
            })
            .sum();
        (bytes as u64).div_ceil(4)
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supports_tools: true,
            supports_images: self.config.supports_images,
            supports_caching: false,
            supports_structured_output: false,
            max_context_tokens: self.config.max_context_tokens,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aigentic_core::{Author, Role, UserId};
    use serde_json::json;

    #[test]
    fn request_has_model_messages_tools_and_streaming() {
        let provider = OpenAiCompat::new(OpenAiCompatConfig::new("http://x/v1", "m")).with_tools(
            vec![ToolDefinition {
                name: "read_file".into(),
                description: "Read".into(),
                parameters: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
            }],
        );
        let context = [Message {
            role: Role::User,
            author: Author::User(UserId("steve".into())),
            blocks: vec![ContentBlock::Text("hi".into())],
        }];
        let body = serde_json::to_value(provider.build_request(&context)).unwrap();
        assert_eq!(body["model"], "m");
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "hi");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "read_file");
        assert_eq!(body["tools"][0]["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn no_tools_means_no_tools_field() {
        let provider = OpenAiCompat::new(OpenAiCompatConfig::new("http://x/v1", "m"));
        let body = serde_json::to_value(provider.build_request(&[])).unwrap();
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn capabilities_follow_config() {
        let provider = OpenAiCompat::new(
            OpenAiCompatConfig::new("http://x/v1", "m")
                .with_max_context_tokens(8192)
                .with_images(true),
        );
        let caps = provider.capabilities();
        assert!(caps.supports_tools && caps.supports_images);
        assert_eq!(caps.max_context_tokens, 8192);
    }
}
