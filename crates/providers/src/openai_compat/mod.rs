//! OpenAI-compatible chat completions adapter. Covers vLLM, llama.cpp,
//! Mistral and most EU hosts: anything that serves `POST /v1/chat/completions`
//! with `stream: true`.

mod stream;
mod wire;

use std::pin::Pin;

use aigentic_core::{
    Capabilities, CompletionRequest, ContentBlock, Message, Provider, ProviderError, ProviderEvent,
};
use futures_core::Stream;
use futures_util::StreamExt;
use serde::Serialize;

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

/// The adapter. Holds the HTTP client and the connection settings; tools
/// and the output cap arrive with each `CompletionRequest`.
#[derive(Debug, Clone)]
pub struct OpenAiCompat {
    client: reqwest::Client,
    config: OpenAiCompatConfig,
}

impl OpenAiCompat {
    pub fn new(config: OpenAiCompatConfig) -> Self {
        Self {
            client: reqwest::Client::new(),
            config,
        }
    }

    pub fn config(&self) -> &OpenAiCompatConfig {
        &self.config
    }

    /// The request body, exposed so tests can check the wire shape without
    /// a network.
    pub fn build_request(&self, request: &CompletionRequest<'_>) -> ChatRequest {
        ChatRequest {
            model: self.config.model.clone(),
            messages: to_wire(request.messages),
            tools: request
                .tools
                .iter()
                .map(|t| WireTool {
                    kind: "function",
                    function: WireFunctionDef {
                        name: t.name.clone(),
                        description: t.description.clone(),
                        parameters: t.schema.clone(),
                    },
                })
                .collect(),
            max_tokens: request.max_output_tokens,
            stream: true,
            // Asks for a final usage chunk. Servers that ignore it simply
            // send no `Usage` event; the parser does not require one.
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
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
    fn complete(&self, request: &CompletionRequest<'_>) -> EventStream<'_> {
        let body = self.build_request(request);
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

    /// An estimate for pre-call sizing only: about four bytes per token plus
    /// a few per message. Tokenizers differ per model, so this is never used
    /// for accounting; the runtime records real usage from
    /// `ProviderEvent::Usage`.
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
    use aigentic_core::{Author, Role, ToolSpec, UserId};
    use serde_json::json;

    #[test]
    fn request_has_model_messages_tools_cap_and_streaming() {
        let provider = OpenAiCompat::new(OpenAiCompatConfig::new("http://x/v1", "m"));
        let tools = [ToolSpec {
            name: "read_file".into(),
            description: "Read".into(),
            schema: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        }];
        let messages = [Message {
            role: Role::User,
            author: Author::User(UserId("steve".into())),
            blocks: vec![ContentBlock::Text("hi".into())],
        }];
        let request = CompletionRequest {
            messages: &messages,
            tools: &tools,
            max_output_tokens: Some(512),
        };
        let body = serde_json::to_value(provider.build_request(&request)).unwrap();
        assert_eq!(body["model"], "m");
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(body["max_tokens"], 512);
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "hi");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "read_file");
        assert_eq!(body["tools"][0]["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn no_tools_and_no_cap_means_no_fields() {
        let provider = OpenAiCompat::new(OpenAiCompatConfig::new("http://x/v1", "m"));
        let request = CompletionRequest {
            messages: &[],
            tools: &[],
            max_output_tokens: None,
        };
        let body = serde_json::to_value(provider.build_request(&request)).unwrap();
        assert!(body.get("tools").is_none());
        assert!(body.get("max_tokens").is_none());
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
