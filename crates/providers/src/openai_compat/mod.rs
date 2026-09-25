//! OpenAI-compatible chat completions adapter. Covers vLLM, llama.cpp,
//! Mistral and most EU hosts: anything that serves `POST /v1/chat/completions`
//! with `stream: true`.

mod stream;
mod wire;

use std::pin::Pin;

use aigentic_core::{
    Capabilities, CompletionRequest, Message, Provider, ProviderError, ProviderEvent,
};
use futures_core::Stream;
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
    /// How long a started reply may produce no model output before it is
    /// treated as dead ([`crate::STALL`] by default; issue #42).
    pub stall: std::time::Duration,
}

impl OpenAiCompatConfig {
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: None,
            model: model.into(),
            max_context_tokens: 32_768,
            supports_images: false,
            stall: crate::STALL,
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

    pub fn with_stall(mut self, stall: std::time::Duration) -> Self {
        self.stall = stall;
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
            client: crate::http_client(),
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

/// Whether a failure before any content is worth another attempt: the
/// connection failed or stalled, the server was rate-limited or broke.
/// A 4xx other than 429 is our request's fault and would fail again.
fn retryable(failure: &ProviderEvent) -> bool {
    match failure {
        ProviderEvent::Error(ProviderError::Transport(_)) => true,
        ProviderEvent::Error(ProviderError::Http { status, .. }) => {
            *status == 429 || *status >= 500
        }
        _ => false,
    }
}

impl Provider for OpenAiCompat {
    fn complete(&self, request: &CompletionRequest<'_>) -> EventStream<'_> {
        let body = self.build_request(request);
        let url = format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        );
        let client = self.client.clone();
        let api_key = self.config.api_key.clone();
        let stall = self.config.stall;

        // Live retries (issue #31): the attempt loop runs in its own
        // task and sends each event into the channel, so a `Retried` is
        // observed *before* the backoff it announces — the client shows
        // `retrying 2/3` while it waits, not after the wait is over.
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut attempt = 0;
            let started = std::time::Instant::now();
            loop {
                let mut request = client.post(&url).json(&body);
                if let Some(key) = &api_key {
                    request = request.bearer_auth(key);
                }
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
                        // `forward` below retries a stream that dies or
                        // stalls before its first event; no unbounded wait
                        // on that event here (issue #42).
                        Ok(resp) => {
                            Ok(Box::pin(parse_stream(resp.bytes_stream())) as EventStream<'static>)
                        }
                    };
                // A stream that stalls or breaks before passing anything on
                // is retried like a refused request (issue #42).
                let outcome = match outcome {
                    Ok(events) => match crate::forward(events, &tx, stall).await {
                        None => return,
                        Some(failure) => Err(failure),
                    },
                    Err(failure) => Err(failure),
                };
                match outcome {
                    Ok(()) => return,
                    Err(failure) => {
                        if attempt >= crate::RETRIES || !retryable(&failure) {
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
    use aigentic_core::{Author, ContentBlock, Role, ToolSpec, UserId};
    use futures_util::StreamExt;
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

    /// A server that answers the first request 503 and the second with a
    /// one-word stream: the adapter retries and the caller sees only text.
    #[tokio::test]
    async fn a_failure_before_content_is_retried() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let replies = [
                "HTTP/1.1 503 Service Unavailable\r\ncontent-length: 4\r\nconnection: close\r\n\r\nbusy".to_owned(),
                {
                    let body = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    )
                },
            ];
            for reply in replies {
                let (mut conn, _) = listener.accept().unwrap();
                let mut buf = [0u8; 65536];
                let _ = conn.read(&mut buf);
                conn.write_all(reply.as_bytes()).unwrap();
            }
        });
        let provider = OpenAiCompat::new(OpenAiCompatConfig::new(format!("http://{addr}/v1"), "m"));
        let request = CompletionRequest {
            messages: &[],
            tools: &[],
            max_output_tokens: None,
        };
        let events: Vec<ProviderEvent> = provider.complete(&request).collect().await;
        assert!(
            matches!(events.first(), Some(ProviderEvent::Retried { attempt: 1, retries, reason, wait })
                if *retries == crate::RETRIES as u32 && reason == "overloaded" && *wait == crate::BACKOFF[0]),
            "{events:?}"
        );
        assert!(
            matches!(events.get(1), Some(ProviderEvent::TextDelta(t)) if t == "hi"),
            "{events:?}"
        );
        assert!(!events.iter().any(|e| matches!(e, ProviderEvent::Error(_))));
    }

    /// A server that answers one request with a 200 and then only SSE
    /// keep-alive comments (a byte-alive stream with no model output) for
    /// `hold`, then closes; `then` is written to the next connection, if
    /// any. Returns the base URL.
    fn keep_alive_server(
        first_output: Option<&'static str>,
        hold: std::time::Duration,
        then: Option<String>,
    ) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut buf = [0u8; 65536];
            let _ = conn.read(&mut buf);
            conn.write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
            )
            .unwrap();
            if let Some(line) = first_output {
                conn.write_all(line.as_bytes()).unwrap();
            }
            let until = std::time::Instant::now() + hold;
            while std::time::Instant::now() < until {
                // The client hangs up when it gives up on the stall; the
                // retry then arrives on a new connection.
                if conn.write_all(b": keep-alive\n\n").is_err() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            drop(conn);
            if let Some(reply) = then {
                let (mut conn, _) = listener.accept().unwrap();
                let _ = conn.read(&mut buf);
                conn.write_all(reply.as_bytes()).unwrap();
            }
        });
        format!("http://{addr}/v1")
    }

    fn hi_reply() -> String {
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    /// Issue #42: keep-alives are not output. A reply that stays
    /// byte-alive without producing anything stalls, and since nothing
    /// reached the caller it is retried, with the retry reported.
    #[tokio::test]
    async fn a_byte_alive_stream_without_output_is_retried() {
        let stall = std::time::Duration::from_millis(300);
        let base = keep_alive_server(None, stall * 4, Some(hi_reply()));
        let provider = OpenAiCompat::new(OpenAiCompatConfig::new(base, "m").with_stall(stall));
        let request = CompletionRequest {
            messages: &[],
            tools: &[],
            max_output_tokens: None,
        };
        let events: Vec<ProviderEvent> = provider.complete(&request).collect().await;
        assert!(
            matches!(events.first(), Some(ProviderEvent::Retried { attempt: 1, reason, .. }) if reason == "not answering"),
            "{events:?}"
        );
        assert!(
            matches!(events.get(1), Some(ProviderEvent::TextDelta(t)) if t == "hi"),
            "{events:?}"
        );
        assert!(
            !events.iter().any(|e| matches!(e, ProviderEvent::Error(_))),
            "{events:?}"
        );
    }

    /// Issue #42: once output has gone out, a stall ends the stream with
    /// an error naming the silence, and is not retried (a retry would
    /// repeat what the caller already has).
    #[tokio::test]
    async fn a_stall_after_output_ends_without_a_retry() {
        let stall = std::time::Duration::from_millis(300);
        let base = keep_alive_server(
            Some("data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n"),
            stall * 4,
            None,
        );
        let provider = OpenAiCompat::new(OpenAiCompatConfig::new(base, "m").with_stall(stall));
        let request = CompletionRequest {
            messages: &[],
            tools: &[],
            max_output_tokens: None,
        };
        let events: Vec<ProviderEvent> = provider.complete(&request).collect().await;
        assert!(
            matches!(events.first(), Some(ProviderEvent::TextDelta(t)) if t == "hi"),
            "{events:?}"
        );
        let expected = format!("no model output for {} s", stall.as_secs());
        assert!(
            matches!(events.last(), Some(ProviderEvent::Error(ProviderError::Transport(m))) if *m == expected),
            "{events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, ProviderEvent::Retried { .. })),
            "{events:?}"
        );
    }

    /// The retry is reported *while* the call waits (issue #31): the
    /// second response is held for two seconds, and the `Retried` event
    /// must arrive long before it is written, so a hanging provider
    /// reads as retries on the turn line, not as a slow model.
    #[tokio::test]
    async fn a_retry_is_reported_before_its_wait_ends() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            // First request: 503 at once.
            let (mut conn, _) = listener.accept().unwrap();
            let mut buf = [0u8; 65536];
            let _ = conn.read(&mut buf);
            conn.write_all(
                b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 4\r\nconnection: close\r\n\r\nbusy",
            )
            .unwrap();
            drop(conn);
            // Second request: take the request, then hold the reply for
            // two seconds — the 1 s backoff plus the 8 s one are real
            // sleeps here, so this is the wait the client must see.
            let (mut conn, _) = listener.accept().unwrap();
            let _ = conn.read(&mut buf);
            std::thread::sleep(std::time::Duration::from_secs(2));
            let body = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
            let _ = conn.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
        });
        let provider = OpenAiCompat::new(OpenAiCompatConfig::new(format!("http://{addr}/v1"), "m"));
        let request = CompletionRequest {
            messages: &[],
            tools: &[],
            max_output_tokens: None,
        };
        let mut stream = provider.complete(&request);
        // The 503 is answered at once, so the first item is the retry and
        // it must be here well inside the backoff: 1.5 s is more than the
        // 1 s sleep but far less than the 2 s the second reply is held.
        let first = tokio::time::timeout(std::time::Duration::from_millis(1500), stream.next())
            .await
            .expect("the retry is reported before its wait ends")
            .expect("a first event");
        assert!(
            matches!(first, ProviderEvent::Retried { attempt: 1, .. }),
            "{first:?}"
        );
        let rest: Vec<ProviderEvent> = stream.collect().await;
        assert!(
            rest.iter()
                .any(|e| matches!(e, ProviderEvent::TextDelta(t) if t == "hi")),
            "{rest:?}"
        );
    }

    /// Every attempt failing (503) ends in one error that says how hard
    /// the adapter tried: the count and the wall time.
    #[tokio::test]
    async fn giving_up_says_how_long_it_tried() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for _ in 0..=crate::RETRIES {
                let (mut conn, _) = listener.accept().unwrap();
                let mut buf = [0u8; 65536];
                let _ = conn.read(&mut buf);
                conn.write_all(
                    b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 4\r\nconnection: close\r\n\r\nbusy",
                )
                .unwrap();
            }
        });
        let provider = OpenAiCompat::new(OpenAiCompatConfig::new(format!("http://{addr}/v1"), "m"));
        let request = CompletionRequest {
            messages: &[],
            tools: &[],
            max_output_tokens: None,
        };
        let events: Vec<ProviderEvent> = provider.complete(&request).collect().await;
        let retries = events
            .iter()
            .filter(|e| matches!(e, ProviderEvent::Retried { .. }))
            .count();
        assert_eq!(retries, crate::RETRIES);
        let Some(ProviderEvent::Error(e)) = events.last() else {
            panic!("expected a final error: {events:?}");
        };
        // The rule is `with_attempts`' own: prefix + the original text.
        let expected_prefix = format!("gave up after {} attempts over ", crate::RETRIES + 1);
        assert!(e.to_string().contains(&expected_prefix), "{e}");
        assert!(e.to_string().contains("http 503"), "{e}");
    }

    #[test]
    fn only_transient_failures_are_retried() {
        let http = |status| {
            ProviderEvent::Error(ProviderError::Http {
                status,
                body: String::new(),
            })
        };
        assert!(retryable(&ProviderEvent::Error(ProviderError::Transport(
            "stalled".into()
        ))));
        assert!(retryable(&http(429)) && retryable(&http(502)));
        assert!(!retryable(&http(400)) && !retryable(&http(403)));
    }
}
