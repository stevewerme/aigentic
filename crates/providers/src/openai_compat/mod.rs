//! OpenAI-compatible chat completions adapter. Covers vLLM, llama.cpp,
//! Mistral and most EU hosts: anything that serves `POST /v1/chat/completions`
//! with `stream: true`.

mod stream;
mod wire;

use std::collections::BTreeMap;
use std::pin::Pin;

use aigentic_core::{
    Capabilities, CompletionRequest, Message, Provider, ProviderError, ProviderEvent,
};
use futures_core::Stream;
use serde::{Deserialize, Serialize};

pub use stream::{Translator, parse_stream};
pub use wire::{WireMessage, from_wire, to_wire};

/// Name stamped on `ProviderBlob`s this adapter produces; only blobs with
/// this name are replayed.
pub const PROVIDER_NAME: &str = "openai_compat";

/// A reasoning effort as the endpoint takes it: a number (DeepSeek V4.1
/// takes 1-100) or a label (`low` | `medium` | `high` on OpenAI-style
/// endpoints; GLM and Kimi have their own knobs). Values pass through
/// unchecked — the endpoint is the authority, and `reasoning_effort_param`
/// exists because the accepted shape differs per host (issue #44).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum ReasoningEffort {
    /// Kept a JSON number on the wire: DeepSeek expects one.
    Int(u64),
    Label(String),
}

impl ReasoningEffort {
    /// The string a footer or a usage line names: `50` for `Int(50)`,
    /// the label itself otherwise.
    pub fn label(&self) -> String {
        match self {
            Self::Int(n) => n.to_string(),
            Self::Label(s) => s.clone(),
        }
    }
}

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
    /// The param name to send the effort under, and the effort: absent
    /// means the field is not sent at all (issue #44). The name is a
    /// dotted path, since some endpoints nest it (`thinking.effort`).
    pub reasoning_effort: Option<(String, ReasoningEffort)>,
    /// How long a call that fails before any content keeps retrying
    /// (issue #90).
    pub retry: crate::RetryPolicy,
}

/// The param name an endpoint expects when the config names none.
pub const REASONING_EFFORT_PARAM: &str = "reasoning_effort";

impl OpenAiCompatConfig {
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: None,
            model: model.into(),
            max_context_tokens: 32_768,
            supports_images: false,
            stall: crate::STALL,
            reasoning_effort: None,
            retry: crate::RetryPolicy::default(),
        }
    }

    /// Send `effort` under `param` on every request; `param` is a dotted
    /// path (`reasoning_effort`, or `thinking.effort` where the endpoint
    /// nests it).
    pub fn with_reasoning_effort(
        mut self,
        param: impl Into<String>,
        effort: ReasoningEffort,
    ) -> Self {
        self.reasoning_effort = Some((param.into(), effort));
        self
    }

    /// The effort the config asks for, or `None`.
    pub fn reasoning_effort(&self) -> Option<&ReasoningEffort> {
        self.reasoning_effort.as_ref().map(|(_, e)| e)
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

    /// How long a call that fails before any content keeps retrying.
    pub fn with_retry(mut self, retry: crate::RetryPolicy) -> Self {
        self.retry = retry;
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
            extra: self.reasoning_effort_field(),
        }
    }

    /// The configured effort under its param name, as a top-level entry.
    /// Empty when no effort is configured, so the flattened body gains
    /// nothing and absent stays absent (issue #44). A dotted param nests
    /// the effort, merging into an existing object under that key
    /// (amendment 2.1).
    fn reasoning_effort_field(&self) -> BTreeMap<String, serde_json::Value> {
        let mut extra = BTreeMap::new();
        let Some((param, effort)) = &self.config.reasoning_effort else {
            return extra;
        };
        let value = serde_json::to_value(effort).expect("an effort serialises");
        let segments: Vec<&str> = param.split('.').collect();
        let (head, tail) = segments.split_first().expect("a param has a segment");
        if tail.is_empty() {
            extra.insert((*head).to_owned(), value);
            return extra;
        }
        let mut nested = value;
        for segment in tail.iter().rev() {
            nested = serde_json::json!({ *segment: nested });
        }
        extra.insert((*head).to_owned(), nested);
        extra
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
    /// Fields whose name the config chooses — today `reasoning_effort`
    /// under `reasoning_effort_param`. Flattened, so an empty map adds
    /// no keys at all (issue #44).
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
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
        let policy = self.config.retry.clone();

        // Live retries (issue #31): the attempt loop runs in its own
        // task and sends each event into the channel, so a `Retried` is
        // observed *before* the backoff it announces — the client shows
        // `retrying 2/14` while it waits, not after the wait is over.
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut attempt = 0;
            let started = std::time::Instant::now();
            loop {
                // Nobody is listening: stop before asking the provider
                // again (issue #90).
                if tx.is_closed() {
                    return;
                }
                let mut retry_after = None;
                let mut request = client.post(&url).json(&body);
                if let Some(key) = &api_key {
                    request = request.bearer_auth(key);
                }
                let sent = tokio::select! {
                    biased;
                    // The turn dropped the stream while this request was
                    // in flight: abandon it and close the connection
                    // now, rather than run it to the stall and still be
                    // billed (issue #90). This is the wait for a
                    // response's first byte, which `forward`'s race
                    // cannot cover.
                    _ = tx.closed() => return,
                    sent = request.send() => sent,
                };
                let outcome: Result<EventStream<'static>, ProviderEvent> = match sent {
                    Err(e) => Err(ProviderEvent::Error(ProviderError::Transport(
                        e.to_string(),
                    ))),
                    Ok(resp) if !resp.status().is_success() => {
                        let status = resp.status().as_u16();
                        // The header is read before the body, which
                        // consumes the response (issue #90).
                        retry_after = crate::retry_after(resp.headers());
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
                        let wait = if retryable(&failure) {
                            policy.next_wait(attempt, started.elapsed(), retry_after)
                        } else {
                            None
                        };
                        let Some(wait) = wait else {
                            let attempts = attempt as u32 + 1;
                            let _ = tx.send(ProviderEvent::Error(
                                crate::error_of(failure).with_attempts(attempts, started.elapsed()),
                            ));
                            return;
                        };
                        let _ = tx.send(ProviderEvent::Retried {
                            attempt: attempt as u32 + 1,
                            retries: policy.retries_within(),
                            reason: crate::retry_reason(&failure),
                            wait,
                        });
                        // A dropped stream ends the wait at once
                        // (issue #90): no `sleep` outlives the turn.
                        tokio::select! {
                            biased;
                            _ = tx.closed() => return,
                            _ = tokio::time::sleep(wait) => {}
                        }
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

    /// Issue #44: the body carries `reasoning_effort` only when the
    /// config sets one. `build_request` is a `ChatRequest`'s only
    /// constructor, so this is the snapshot of an unconfigured profile
    /// and the per-key checks below say exactly what the field adds.
    #[test]
    fn a_request_without_a_reasoning_effort_has_no_field() {
        let provider = OpenAiCompat::new(OpenAiCompatConfig::new("http://x/v1", "m"));
        let request = CompletionRequest {
            messages: &[],
            tools: &[],
            max_output_tokens: Some(64),
        };
        let body = serde_json::to_value(provider.build_request(&request)).unwrap();
        assert!(
            body.get("reasoning_effort").is_none(),
            "absent means absent: {body}"
        );
    }

    /// An integer effort stays a JSON number under the default param.
    #[test]
    fn an_integer_effort_is_a_json_number_under_the_default_param() {
        let provider = OpenAiCompat::new(
            OpenAiCompatConfig::new("http://x/v1", "m")
                .with_reasoning_effort("reasoning_effort", ReasoningEffort::Int(50)),
        );
        let request = CompletionRequest {
            messages: &[],
            tools: &[],
            max_output_tokens: None,
        };
        let body = serde_json::to_value(provider.build_request(&request)).unwrap();
        assert_eq!(body["reasoning_effort"], json!(50));
        assert!(body["reasoning_effort"].is_u64(), "{body}");
    }

    /// A label effort rides the same field as its string.
    #[test]
    fn a_label_effort_is_a_string() {
        let provider = OpenAiCompat::new(
            OpenAiCompatConfig::new("http://x/v1", "m")
                .with_reasoning_effort("reasoning_effort", ReasoningEffort::Label("high".into())),
        );
        let request = CompletionRequest {
            messages: &[],
            tools: &[],
            max_output_tokens: None,
        };
        let body = serde_json::to_value(provider.build_request(&request)).unwrap();
        assert_eq!(body["reasoning_effort"], "high");
    }

    /// `reasoning_effort_param`: the field goes under the endpoint's own
    /// name, and the default name stays out of the body.
    #[test]
    fn a_configured_param_name_is_the_one_sent() {
        let provider = OpenAiCompat::new(
            OpenAiCompatConfig::new("http://x/v1", "m")
                .with_reasoning_effort("deepseek_effort", ReasoningEffort::Int(50)),
        );
        let request = CompletionRequest {
            messages: &[],
            tools: &[],
            max_output_tokens: None,
        };
        let body = serde_json::to_value(provider.build_request(&request)).unwrap();
        assert_eq!(body["deepseek_effort"], json!(50));
        assert!(body.get("reasoning_effort").is_none(), "{body}");
    }

    /// Amendment 2.1: a dotted param nests the effort, and the default
    /// top-level name stays out of the body.
    #[test]
    fn a_dotted_param_nests_the_effort() {
        let provider = OpenAiCompat::new(
            OpenAiCompatConfig::new("http://x/v1", "m")
                .with_reasoning_effort("thinking.effort", ReasoningEffort::Int(50)),
        );
        let request = CompletionRequest {
            messages: &[],
            tools: &[],
            max_output_tokens: None,
        };
        let body = serde_json::to_value(provider.build_request(&request)).unwrap();
        assert_eq!(body["thinking"]["effort"], json!(50));
        assert!(body.get("reasoning_effort").is_none(), "{body}");
    }

    /// A policy that gives up in a fifth of a second, so no test waits
    /// real seconds (issue #90). Its waits are 10, 20, then 40 ms.
    fn quick_policy() -> crate::RetryPolicy {
        crate::RetryPolicy {
            window: std::time::Duration::from_millis(200),
            backoff: vec![
                std::time::Duration::from_millis(10),
                std::time::Duration::from_millis(20),
                std::time::Duration::from_millis(40),
            ],
        }
    }

    /// The most retries a policy could give with attempts that take no
    /// time: the count and the ceiling in the tests come from the policy,
    /// so neither is hand-counted (issue #90). Real attempts take real
    /// time, so a call gives up at or below this.
    fn attempts_ceiling(policy: &crate::RetryPolicy) -> u32 {
        let mut elapsed = std::time::Duration::ZERO;
        let mut retries = 0;
        while let Some(wait) = policy.next_wait(retries as usize, elapsed, None) {
            elapsed += wait;
            retries += 1;
        }
        retries
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
        let config = OpenAiCompatConfig::new(format!("http://{addr}/v1"), "m");
        // Derived from the policy the adapter will use (issue #90), not
        // stated by hand.
        let policy = config.retry.clone();
        let provider = OpenAiCompat::new(config);
        let request = CompletionRequest {
            messages: &[],
            tools: &[],
            max_output_tokens: None,
        };
        let events: Vec<ProviderEvent> = provider.complete(&request).collect().await;
        assert!(
            matches!(events.first(), Some(ProviderEvent::Retried { attempt: 1, retries, reason, wait })
                if *retries == policy.retries_within() && reason == "overloaded" && *wait == policy.backoff[0]),
            "{events:?}"
        );
        assert!(
            matches!(events.get(1), Some(ProviderEvent::TextDelta(t)) if t == "hi"),
            "{events:?}"
        );
        assert!(!events.iter().any(|e| matches!(e, ProviderEvent::Error(_))));
    }

    /// Issue #90: a server that is down for five requests — more than
    /// the three the adapter used to try — is ridden out, and the reply
    /// arrives. Each retry is reported with the policy's own wait.
    #[tokio::test]
    async fn a_long_outage_is_ridden_out() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        const DOWN: usize = 5;
        std::thread::spawn(move || {
            for i in 0..=DOWN {
                let (mut conn, _) = listener.accept().unwrap();
                let mut buf = [0u8; 65536];
                let _ = conn.read(&mut buf);
                if i < DOWN {
                    conn.write_all(
                        b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 4\r\nconnection: close\r\n\r\nbusy",
                    )
                    .unwrap();
                } else {
                    conn.write_all(hi_reply().as_bytes()).unwrap();
                }
            }
        });
        let policy = quick_policy();
        let config =
            OpenAiCompatConfig::new(format!("http://{addr}/v1"), "m").with_retry(policy.clone());
        let provider = OpenAiCompat::new(config);
        let request = CompletionRequest {
            messages: &[],
            tools: &[],
            max_output_tokens: None,
        };
        let events: Vec<ProviderEvent> = provider.complete(&request).collect().await;
        let retries: Vec<&ProviderEvent> = events
            .iter()
            .filter(|e| matches!(e, ProviderEvent::Retried { .. }))
            .collect();
        assert_eq!(retries.len(), DOWN, "{events:?}");
        // Each retry carries its own index, the ceiling, and the wait
        // the policy gives for that index.
        for (i, event) in retries.iter().enumerate() {
            let expected = policy.next_wait(i, std::time::Duration::ZERO, None);
            assert!(
                matches!(event, ProviderEvent::Retried { attempt, retries, wait, .. }
                    if *attempt == i as u32 + 1
                        && *retries == policy.retries_within()
                        && Some(*wait) == expected),
                "{i}: {event:?}"
            );
        }
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ProviderEvent::TextDelta(t) if t == "hi")),
            "{events:?}"
        );
        assert!(!events.iter().any(|e| matches!(e, ProviderEvent::Error(_))));
    }

    /// Issue #90: a `Retry-After` that reaches past the retry window
    /// ends the call at once with that status, and no retry: the
    /// provider has said it will not be back inside the window. Without
    /// reading the header this call would have retried, so the header is
    /// what the assertion is about.
    #[tokio::test]
    async fn a_retry_after_past_the_window_ends_the_call() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut buf = [0u8; 65536];
            let _ = conn.read(&mut buf);
            conn.write_all(
                b"HTTP/1.1 429 Too Many Requests\r\nretry-after: 5\r\ncontent-length: 4\r\nconnection: close\r\n\r\nbusy",
            )
            .unwrap();
        });
        // A 200 ms window against a 5 s `Retry-After`: nothing to wait
        // for. The elapsed time is the policy's own boundary.
        let config =
            OpenAiCompatConfig::new(format!("http://{addr}/v1"), "m").with_retry(quick_policy());
        let provider = OpenAiCompat::new(config);
        let request = CompletionRequest {
            messages: &[],
            tools: &[],
            max_output_tokens: None,
        };
        let events: Vec<ProviderEvent> = provider.complete(&request).collect().await;
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(
            matches!(
                events.first(),
                Some(ProviderEvent::Error(ProviderError::Http {
                    status: 429,
                    ..
                }))
            ),
            "{events:?}"
        );
    }

    /// Issue #90 (design 4a): a stream that ends before any content is
    /// a retryable failure, and the next attempt's reply is delivered.
    #[tokio::test]
    async fn a_stream_that_ends_before_any_content_is_retried() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            // First: a well-formed stream that ends with no content at
            // all — a `Done { "end_of_stream" }` and nothing else.
            let body = "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"end_of_stream\"}]}\n\ndata: [DONE]\n\n";
            let cut = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            for reply in [cut, hi_reply()] {
                let (mut conn, _) = listener.accept().unwrap();
                let mut buf = [0u8; 65536];
                let _ = conn.read(&mut buf);
                conn.write_all(reply.as_bytes()).unwrap();
            }
        });
        let config =
            OpenAiCompatConfig::new(format!("http://{addr}/v1"), "m").with_retry(quick_policy());
        let provider = OpenAiCompat::new(config);
        let request = CompletionRequest {
            messages: &[],
            tools: &[],
            max_output_tokens: None,
        };
        let events: Vec<ProviderEvent> = provider.complete(&request).collect().await;
        assert!(
            matches!(events.first(), Some(ProviderEvent::Retried { attempt: 1, reason, .. })
                if reason == "not answering"),
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

    /// A non-blocking accept, so a loop can watch for a connection without
    /// blocking the runtime (issue #90's interrupt tests).
    fn accept_now(
        listener: &std::net::TcpListener,
    ) -> Option<(std::net::TcpStream, std::net::SocketAddr)> {
        match listener.accept() {
            Ok(conn) => Some(conn),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => None,
            Err(e) => panic!("accept: {e}"),
        }
    }

    /// The same accept, given `margin` to arrive. Failing here is the
    /// test saying nothing connected, not the test hanging.
    async fn accept_within(
        listener: &std::net::TcpListener,
        margin: std::time::Duration,
    ) -> (std::net::TcpStream, std::net::SocketAddr) {
        let started = std::time::Instant::now();
        while started.elapsed() < margin {
            if let Some(conn) = accept_now(listener) {
                return conn;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        panic!("nothing connected within {margin:?}");
    }

    /// How long the interrupt tests give a dropped call to prove itself
    /// gone. Comfortably longer than the millisecond backoff it is on,
    /// and nowhere near a real second of waiting (issue #90).
    const MARGIN: std::time::Duration = std::time::Duration::from_millis(500);

    const BUSY_503: &[u8] =
        b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 4\r\nconnection: close\r\n\r\nbusy";

    /// Issue #90 (T5a): a turn that drops the stream during a backoff
    /// ends the call. Nothing further reaches the server inside the
    /// margin, which the test's own accept must see, so the check can
    /// neither lie nor hang.
    #[tokio::test]
    async fn dropping_the_stream_during_a_backoff_stops_the_retries() {
        use futures_util::StreamExt;
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let policy = quick_policy();
        let config =
            OpenAiCompatConfig::new(format!("http://{addr}/v1"), "m").with_retry(policy.clone());
        let provider = OpenAiCompat::new(config);
        let request = CompletionRequest {
            messages: &[],
            tools: &[],
            max_output_tokens: None,
        };
        let mut stream = provider.complete(&request);
        let (mut conn, _) = accept_within(&listener, MARGIN).await;
        conn.write_all(BUSY_503).unwrap();
        // The first attempt is answered at once, so this leaves the
        // adapter inside its first backoff.
        let first = stream.next().await;
        assert!(
            matches!(first, Some(ProviderEvent::Retried { attempt: 1, .. })),
            "{first:?}"
        );
        drop(stream);
        let started = std::time::Instant::now();
        let mut extra = 0;
        while started.elapsed() < MARGIN {
            if let Some((mut conn, _)) = accept_now(&listener) {
                let _ = conn.write_all(BUSY_503);
                extra += 1;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        assert_eq!(extra, 0, "the dropped stream ended every retry");
    }

    /// Issue #90 (T5b): dropping the stream while an attempt is in
    /// flight closes the connection at once, instead of leaving the
    /// request running on for up to the stall and still being billed.
    /// The server holds its response, so nothing but the drop can end
    /// the exchange.
    #[tokio::test]
    async fn dropping_the_stream_closes_an_attempt_in_flight() {
        use std::io::Read;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = std::sync::mpsc::channel::<&'static str>();
        let server = std::thread::spawn(move || {
            let (mut conn, _) = loop {
                if let Some(conn) = accept_now(&listener) {
                    break conn;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            };
            let mut buf = [0u8; 65536];
            let _ = conn.read(&mut buf);
            tx.send("accepted").unwrap();
            // Hold: no response headers, so the attempt is in flight. The
            // client dropping the stream closes the socket; any request
            // bytes still in flight are drained first.
            conn.set_read_timeout(Some(MARGIN)).unwrap();
            let deadline = std::time::Instant::now() + MARGIN * 4;
            let mut one = [0u8; 1];
            let mut closed = false;
            while std::time::Instant::now() < deadline {
                match conn.read(&mut one) {
                    Ok(0) => {
                        closed = true;
                        break;
                    }
                    Ok(_) => {}
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) => {}
                    // A reset is the client going away too.
                    Err(_) => {
                        closed = true;
                        break;
                    }
                }
            }
            tx.send(if closed { "closed" } else { "kept" }).unwrap();
        });
        let config =
            OpenAiCompatConfig::new(format!("http://{addr}/v1"), "m").with_retry(quick_policy());
        let provider = OpenAiCompat::new(config);
        let request = CompletionRequest {
            messages: &[],
            tools: &[],
            max_output_tokens: None,
        };
        let stream = provider.complete(&request);
        // Wait for the request to be in flight before dropping.
        let started = std::time::Instant::now();
        while rx.try_recv().is_err() {
            assert!(started.elapsed() < MARGIN, "nothing connected");
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        drop(stream);
        let started = std::time::Instant::now();
        let mut end = None;
        while started.elapsed() < MARGIN {
            if let Ok(what) = rx.try_recv() {
                end = Some(what);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        assert_eq!(end, Some("closed"), "the attempt in flight was abandoned");
        let _ = server.join();
    }

    /// Every attempt failing (503) ends in one error that says how hard
    /// the adapter tried: the count and the wall time (issue #90: the
    /// count and the ceiling both come from the policy, so neither is
    /// hand-counted).
    #[tokio::test]
    async fn giving_up_says_how_long_it_tried() {
        use std::io::{Read, Write};
        let policy = quick_policy();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        // Enough connections for the ceiling the policy could allow; the
        // call will use fewer, since its attempts take real time.
        let ceiling = attempts_ceiling(&policy) as usize + 1;
        std::thread::spawn(move || {
            for _ in 0..ceiling {
                let (mut conn, _) = listener.accept().unwrap();
                let mut buf = [0u8; 65536];
                let _ = conn.read(&mut buf);
                conn.write_all(
                    b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 4\r\nconnection: close\r\n\r\nbusy",
                )
                .unwrap();
            }
        });
        let config =
            OpenAiCompatConfig::new(format!("http://{addr}/v1"), "m").with_retry(policy.clone());
        let provider = OpenAiCompat::new(config);
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
        // The window bounds the retries: the count is the events' own, and the
        // policy's ceiling is the most it could have been (issue #90).
        assert!(
            retries >= 1 && retries as u32 <= attempts_ceiling(&policy),
            "{retries} retries against a ceiling of {}",
            attempts_ceiling(&policy)
        );
        let Some(ProviderEvent::Error(e)) = events.last() else {
            panic!("expected a final error: {events:?}");
        };
        // The rule is `with_attempts`' own: prefix + the original text,
        // with the attempt count one more than the retries seen.
        let expected_prefix = format!("gave up after {} attempts over ", retries + 1);
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
