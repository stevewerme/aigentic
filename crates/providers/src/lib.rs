//! Provider adapters. Each adapter translates canonical messages to one
//! backend's wire format and its streamed reply back into
//! [`ProviderEvent`](aigentic_core::ProviderEvent)s, and nothing else. Written
//! against raw HTTP with `reqwest` and `serde`; no vendor SDKs.

pub mod anthropic;
pub mod estimate;
pub mod openai_compat;
pub mod sse;

pub use anthropic::{Anthropic, AnthropicConfig, Thinking};
pub use openai_compat::{OpenAiCompat, OpenAiCompatConfig};

/// One HTTP client configuration for every adapter. Idle pooled
/// connections are dropped after a few seconds: a tool call can take
/// longer than a server's keep-alive timeout, and reusing a connection the
/// server has already closed fails the next request with a transport
/// error instead of a reply.
pub(crate) fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .pool_idle_timeout(std::time::Duration::from_secs(5))
        .build()
        .expect("a default TLS backend is compiled in")
}
