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

/// How long a response may go without a byte before it counts as dead.
/// Reasoning models stream thinking deltas throughout, so two minutes of
/// silence is a stall, not a slow answer.
pub(crate) const STALL: std::time::Duration = std::time::Duration::from_secs(120);

/// Retries for a reply that failed before any content streamed. Backoff
/// 1s, 3s, 8s. Overload waves observed during the phase 2 acceptance
/// lasted minutes; anything longer belongs to the user, who sees the
/// error as a turn_ended event and can re-ask.
pub(crate) const RETRIES: usize = 3;
pub(crate) const BACKOFF: [std::time::Duration; RETRIES] = [
    std::time::Duration::from_secs(1),
    std::time::Duration::from_secs(3),
    std::time::Duration::from_secs(8),
];

/// Why a call is being retried, in words a user reads on the turn line:
/// a dead connection is "not answering" (the stall case that looked like
/// a slow model in the #38 review), a status is itself, and 429 gets its
/// own label.
pub(crate) fn retry_reason(failure: &aigentic_core::ProviderEvent) -> String {
    use aigentic_core::{ProviderError, ProviderEvent};
    match failure {
        ProviderEvent::Error(ProviderError::Transport(_)) => "not answering".into(),
        ProviderEvent::Error(ProviderError::Http { status, .. }) if *status == 429 => {
            "rate limited".into()
        }
        // The statuses Anthropic names "overloaded"; any other 5xx is
        // its own number, which is more use than a guess.
        ProviderEvent::Error(ProviderError::Http {
            status: 503 | 529, ..
        }) => "overloaded".into(),
        ProviderEvent::Error(ProviderError::Http { status, .. }) => format!("http {status}"),
        other => format!("{other:?}"),
    }
}

/// The error out of an `Error` event; anything else is a protocol bug
/// and says so rather than panicking.
pub(crate) fn error_of(event: aigentic_core::ProviderEvent) -> aigentic_core::ProviderError {
    match event {
        aigentic_core::ProviderEvent::Error(e) => e,
        other => {
            aigentic_core::ProviderError::Protocol(format!("expected an error, got {other:?}"))
        }
    }
}

/// A call's arguments as the wire must carry them on replay: an object.
/// A model that emitted malformed JSON leaves its raw text in the log as a
/// string (the tool already answered with an error quoting it), and both
/// APIs reject any history whose tool call arguments are not an object, so
/// every later turn would fail. The log keeps the original; the wire gets
/// `{}`.
pub(crate) fn replay_args(args: &serde_json::Value) -> serde_json::Value {
    if args.is_object() {
        args.clone()
    } else {
        serde_json::Value::Object(serde_json::Map::new())
    }
}

/// One HTTP client configuration for every adapter. Idle pooled
/// connections are dropped after a few seconds: a tool call can take
/// longer than a server's keep-alive timeout, and reusing a connection the
/// server has already closed fails the next request with a transport
/// error instead of a reply.
///
/// Every read, headers or a body chunk, must arrive within [`STALL`]: a
/// stream that goes silent fails as a transport error instead of hanging
/// the turn (a TensorX stream once sat silent for fifteen minutes).
pub(crate) fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .pool_idle_timeout(std::time::Duration::from_secs(5))
        .read_timeout(STALL)
        .build()
        .expect("a default TLS backend is compiled in")
}
