//! Anthropic Messages API adapter, written against raw HTTP. The second,
//! deliberately different backend: tool calls and results are content
//! blocks, cache breakpoints are explicit, and thinking blocks come back
//! with a signature that must be replayed verbatim.

mod stream;
mod wire;

use std::pin::Pin;

use aigentic_core::{
    Capabilities, CompletionRequest, Message, Provider, ProviderError, ProviderEvent,
};
use futures_core::Stream;
use futures_util::StreamExt;

pub use stream::{Translator, parse_stream};
pub use wire::{MessagesRequest, to_wire};

/// Name stamped on `ProviderBlob`s this adapter produces; only blobs with
/// this name are replayed.
pub const PROVIDER_NAME: &str = "anthropic";
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
pub const API_VERSION: &str = "2023-06-01";
/// `max_tokens` is required by the API. Streaming means a large cap costs
/// nothing unless used, and a small one truncates tool inputs mid-JSON.
pub const DEFAULT_MAX_OUTPUT_TOKENS: u64 = 64_000;

/// Thinking configuration. Current models take `adaptive` or nothing;
/// token budgets are rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Thinking {
    /// Send `{"type": "adaptive"}`.
    #[default]
    Adaptive,
    /// Omit the field. Accepted at effort `high` or below.
    Off,
}

#[derive(Debug, Clone)]
pub struct AnthropicConfig {
    pub base_url: String,
    /// Sent as `x-api-key`. Never logged.
    pub api_key: String,
    /// e.g. `claude-opus-5`, used exactly as published.
    pub model: String,
    pub max_context_tokens: u64,
    /// `max_tokens` when the request carries no `max_output_tokens`.
    pub max_output_tokens: u64,
    pub thinking: Thinking,
    /// `low` | `medium` | `high` | `xhigh` | `max`.
    pub effort: Option<String>,
    /// Emit `cache_control` breakpoints.
    pub cache: bool,
}

impl AnthropicConfig {
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.into(),
            api_key: api_key.into(),
            model: model.into(),
            max_context_tokens: 1_000_000,
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
            thinking: Thinking::Adaptive,
            effort: None,
            cache: true,
        }
    }

    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    pub fn with_max_context_tokens(mut self, tokens: u64) -> Self {
        self.max_context_tokens = tokens;
        self
    }

    pub fn with_max_output_tokens(mut self, tokens: u64) -> Self {
        self.max_output_tokens = tokens;
        self
    }

    pub fn with_thinking(mut self, thinking: Thinking) -> Self {
        self.thinking = thinking;
        self
    }

    pub fn with_effort(mut self, effort: impl Into<String>) -> Self {
        self.effort = Some(effort.into());
        self
    }

    pub fn with_cache(mut self, cache: bool) -> Self {
        self.cache = cache;
        self
    }
}

/// The adapter.
#[derive(Debug, Clone)]
pub struct Anthropic {
    client: reqwest::Client,
    config: AnthropicConfig,
}

impl Anthropic {
    pub fn new(config: AnthropicConfig) -> Self {
        Self {
            client: crate::http_client(),
            config,
        }
    }

    pub fn config(&self) -> &AnthropicConfig {
        &self.config
    }

    /// The request body, exposed so tests can check the wire shape without
    /// a network.
    pub fn build_request(&self, request: &CompletionRequest<'_>) -> MessagesRequest {
        to_wire(request, &self.config)
    }
}

type EventStream<'a> = Pin<Box<dyn Stream<Item = ProviderEvent> + Send + 'a>>;

/// Debugging aid: with `AIGENTIC_DUMP_REQUESTS=<dir>` every request body
/// is written there as `<unix-millis>.json`. Never includes the key.
/// Diffing two consecutive bodies is how a silent cache invalidator is
/// found.
fn dump_request(body: &MessagesRequest) {
    let Some(dir) = std::env::var_os("AIGENTIC_DUMP_REQUESTS") else {
        return;
    };
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let path = std::path::Path::new(&dir).join(format!("{millis}.json"));
    if let Ok(json) = serde_json::to_string_pretty(body) {
        let _ = std::fs::write(path, json);
    }
}

fn retryable_status(status: u16) -> bool {
    matches!(status, 429 | 503 | 529)
}

fn is_overloaded(event: &ProviderEvent) -> bool {
    matches!(event, ProviderEvent::Error(ProviderError::Protocol(m)) if m.contains("overloaded_error"))
}

impl Provider for Anthropic {
    fn complete(&self, request: &CompletionRequest<'_>) -> EventStream<'_> {
        let body = self.build_request(request);
        dump_request(&body);
        let url = format!("{}/v1/messages", self.config.base_url.trim_end_matches('/'));
        let client = self.client.clone();
        let api_key = self.config.api_key.clone();

        // Live retries (issue #31), as in the OpenAI adapter: the loop
        // runs in its own task so a `Retried` reaches the caller before
        // the backoff it announces.
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut attempt = 0;
            let started = std::time::Instant::now();
            loop {
                let request = client
                    .post(&url)
                    .header("x-api-key", &api_key)
                    .header("anthropic-version", API_VERSION)
                    .json(&body);
                let outcome: Result<EventStream<'static>, ProviderEvent> =
                    match request.send().await {
                        Err(e) => Err(ProviderEvent::Error(ProviderError::Transport(
                            e.to_string(),
                        ))),
                        Ok(resp) if !resp.status().is_success() => {
                            let status = resp.status().as_u16();
                            let body = resp.text().await.unwrap_or_default();
                            Err(ProviderEvent::Error(ProviderError::Http { status, body }))
                        }
                        Ok(resp) => {
                            // Peek the first event: an overload arrives as an
                            // `error` event on a 200 stream.
                            let mut events = Box::pin(parse_stream(resp.bytes_stream()).peekable());
                            match events.as_mut().peek().await {
                                Some(first) if is_overloaded(first) => Err(first.clone()),
                                _ => Ok(Box::pin(events) as EventStream<'static>),
                            }
                        }
                    };
                match outcome {
                    Ok(mut events) => {
                        while let Some(event) = events.next().await {
                            let _ = tx.send(event);
                        }
                        return;
                    }
                    Err(failure) => {
                        // Retries cover an overloaded or rate-limited reply before any
                        // content: HTTP 429/503/529, or a first `overloaded_error` event.
                        let retry = attempt < crate::RETRIES
                            && match &failure {
                                ProviderEvent::Error(ProviderError::Http { status, .. }) => {
                                    retryable_status(*status)
                                }
                                other => is_overloaded(other),
                            };
                        if !retry {
                            let attempts = attempt as u32 + 1;
                            let _ = tx.send(ProviderEvent::Error(
                                crate::error_of(failure).with_attempts(attempts, started.elapsed()),
                            ));
                            return;
                        }
                        let _ = tx.send(ProviderEvent::Retried {
                            attempt: attempt as u32 + 1,
                            retries: crate::RETRIES as u32,
                            reason: crate::retry_reason(&failure),
                            wait: crate::BACKOFF[attempt],
                        });
                        tokio::time::sleep(crate::BACKOFF[attempt]).await;
                        attempt += 1;
                    }
                }
            }
        });
        Box::pin(futures_util::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|event| (event, rx))
        }))
    }

    fn count_tokens(&self, context: &[Message]) -> u64 {
        crate::estimate::estimate_tokens(context)
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supports_tools: true,
            supports_images: true,
            supports_caching: self.config.cache,
            supports_structured_output: false,
            max_context_tokens: self.config.max_context_tokens,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overload_detection() {
        assert!(is_overloaded(&ProviderEvent::Error(
            ProviderError::Protocol(r#"{"type":"overloaded_error","message":"Overloaded"}"#.into())
        )));
        assert!(!is_overloaded(&ProviderEvent::Error(
            ProviderError::Protocol("bad json".into())
        )));
        assert!(!is_overloaded(&ProviderEvent::TextDelta("x".into())));
        assert!(retryable_status(529) && retryable_status(429) && !retryable_status(400));
    }

    #[test]
    fn capabilities_follow_config() {
        let a = Anthropic::new(AnthropicConfig::new("k", "claude-opus-5").with_cache(false));
        let caps = a.capabilities();
        assert!(caps.supports_tools && caps.supports_images);
        assert!(!caps.supports_caching);
        assert_eq!(caps.max_context_tokens, 1_000_000);
    }
}
