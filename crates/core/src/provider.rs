use std::pin::Pin;

use futures_core::Stream;
use serde::{Deserialize, Serialize};

use crate::{Message, ProviderBlob, ToolCall, ToolSpec};

/// What a backend can do. The runtime adapts its behaviour to these flags
/// rather than to the provider's name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub supports_tools: bool,
    pub supports_images: bool,
    pub supports_caching: bool,
    pub supports_structured_output: bool,
    pub max_context_tokens: u64,
}

/// Token usage for one model call, as the backend reports it.
///
/// `input_tokens` is the uncached remainder only; the whole prompt is
/// `input_tokens + cache_read_tokens + cache_write_tokens`. Cache fields are
/// zero on backends without caching; `reasoning_tokens` is `None` when the
/// backend does not split reasoning out of output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
    #[serde(default)]
    pub reasoning_tokens: Option<u64>,
}

impl Usage {
    /// Every token the call consumed or produced, for budgets.
    pub fn total(&self) -> u64 {
        self.input_tokens + self.cache_read_tokens + self.cache_write_tokens + self.output_tokens
    }
}

/// One item of a streamed completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderEvent {
    /// Streaming token(s) of assistant text.
    TextDelta(String),
    /// A complete tool call arriving in the stream.
    ToolCall(ToolCall),
    /// Opaque provider content (thinking, signatures) to store and replay verbatim.
    Blob(ProviderBlob),
    Usage(Usage),
    Done {
        finish_reason: String,
    },
    /// A retry about to run, yielded before its backoff wait so a client
    /// can show why the call is quiet (issue #31). `attempt` is the
    /// 1-based number of the retry that is starting, `retries` the total
    /// the adapter will make.
    Retried {
        attempt: u32,
        retries: u32,
        reason: String,
        wait: std::time::Duration,
    },
    /// The stream failed; no further items follow.
    Error(ProviderError),
}

/// The `Done` reason both adapters report when a stream ended with no
/// completion reason from the provider at all (issue #96): the
/// connection dropped, and neither a reason nor its marker arrived, so
/// the reply may stop mid-sentence or mid tool call. The runtime turns
/// it into [`ProviderError::Cut`] rather than a finished reply, and the
/// adapters likewise never flush a tool call it cut off.
pub const CUT_STREAM: &str = "end_of_stream";

/// Errors a provider adapter can surface.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
pub enum ProviderError {
    #[error("transport error: {0}")]
    Transport(String),
    #[error("http {status}: {body}")]
    Http { status: u16, body: String },
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("rate limited")]
    RateLimited,
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// The stream ended before the reply finished (issue #96): the
    /// provider closed it with no completion marker and no reason, so
    /// nothing about the reply can be trusted — a tool call it was still
    /// writing may have half its arguments. Its own variant, not a
    /// `Protocol`: nothing the provider sent is unreadable, there is
    /// simply no end to the reply.
    #[error("the stream ended before the reply finished")]
    Cut,
}

impl ProviderError {
    /// Whether waiting and trying again could work (issue #22): a
    /// dropped connection, a rate limit, an overloaded backend. The
    /// same class the adapters retry (`crates/providers/src/lib.rs`),
    /// generalised to every 5xx; a request the provider refused
    /// (a 4xx, a bad reply, an unsupported call) will not.
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Transport(_) | Self::RateLimited => true,
            Self::Http { status, .. } => *status == 429 || (500..600).contains(status),
            Self::Protocol(_) | Self::Unsupported(_) | Self::Cut => false,
        }
    }

    /// One plain sentence for a person: what happened and what to do
    /// (issue #22). The raw error stays in the log; this is derived at
    /// render time and never stored. Core does not know the profile's
    /// name, so the line never names a vendor.
    pub fn plain_line(&self) -> String {
        match self {
            Self::Transport(_) => {
                "the connection to the model failed; type continue to retry".to_owned()
            }
            Self::RateLimited => {
                "the provider is rate limiting (HTTP 429); type continue to retry".to_owned()
            }
            Self::Http { status: 429, .. } => {
                "the provider is rate limiting (HTTP 429); type continue to retry".to_owned()
            }
            Self::Http { status, .. } if (500..600).contains(status) => {
                format!(
                    "the model is temporarily unavailable (HTTP {status}); type continue to retry"
                )
            }
            Self::Http { status: 400, body } if mentions_spend_limit(body) => {
                "the provider refused: the account's spend limit is reached — top up or raise \
                 the key budget, then type continue"
                    .to_owned()
            }
            Self::Http { status, .. } => {
                format!("the provider refused the request (HTTP {status}); type continue to retry")
            }
            Self::Protocol(_) => {
                "the provider sent a reply that could not be read; type continue to retry"
                    .to_owned()
            }
            Self::Unsupported(_) => {
                "this request is unsupported by the provider; check the configuration".to_owned()
            }
            Self::Cut => "the model's reply was cut off; type continue to retry".to_owned(),
        }
    }

    /// The same error, prefixed with how hard the adapter tried: the
    /// turn line and the log then say a provider was given `attempts`
    /// attempts over `tried`, not just that it failed.
    pub fn with_attempts(self, attempts: u32, tried: std::time::Duration) -> Self {
        let prefix = format!("gave up after {attempts} attempts over {tried:?}: ");
        match self {
            Self::Transport(m) => Self::Transport(prefix + &m),
            Self::Http { status, body } => Self::Http {
                status,
                body: prefix + &body,
            },
            Self::Protocol(m) => Self::Protocol(prefix + &m),
            Self::RateLimited => Self::Transport(format!("{prefix}rate limited")),
            Self::Unsupported(m) => Self::Unsupported(m),
            // Nothing was retried: a cut is not an `Error` the adapter
            // sees, so there are no attempts to report (issue #96).
            Self::Cut => Self::Cut,
        }
    }
}

/// Whether a refusal body is the provider account out of credit or over
/// its key budget, so a 400 gets the spend-limit line rather than the
/// generic one. Vendor wording only; the status stays the gate.
fn mentions_spend_limit(body: &str) -> bool {
    let body = body.to_lowercase();
    body.contains("budget") || body.contains("spend limit")
}

/// Everything one model call needs. Tools are per call because the
/// registry (and the policy narrowing it) can change between turns.
#[derive(Debug, Clone, Copy)]
pub struct CompletionRequest<'a> {
    pub messages: &'a [Message],
    pub tools: &'a [ToolSpec],
    pub max_output_tokens: Option<u64>,
}

/// The model as a function: request in, a stream of events out.
///
/// Object-safe so the runtime can hold `Box<dyn Provider>` and swap backends
/// by configuration alone.
pub trait Provider: Send + Sync {
    fn complete(
        &self,
        request: &CompletionRequest<'_>,
    ) -> Pin<Box<dyn Stream<Item = ProviderEvent> + Send + '_>>;
    fn count_tokens(&self, context: &[Message]) -> u64;
    fn capabilities(&self) -> Capabilities;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// T1 (issue #22): a dropped connection says so in plain words — the
    /// amendment's sentence for every transport failure, a refused
    /// connection included.
    #[test]
    fn transport_line_says_connection_dropped() {
        assert_eq!(
            ProviderError::Transport("error decoding response body".into()).plain_line(),
            "the connection to the model failed; type continue to retry"
        );
    }

    /// T2 (issue #22): an overloaded backend names its status, and the
    /// adapter's `gave up after …` prefix and raw body never reach the
    /// line (the widening comment's 503).
    #[test]
    fn http_5xx_line_says_unavailable_with_status() {
        let raw = ProviderError::Http {
            status: 503,
            body: "{\"error\": {\"message\": \"The model is temporarily unavailable.\"}}".into(),
        };
        let line = raw
            .with_attempts(4, Duration::from_millis(70_800))
            .plain_line();
        assert_eq!(
            line,
            "the model is temporarily unavailable (HTTP 503); type continue to retry"
        );
        assert!(!line.contains("gave up after"), "{line}");
        assert!(!line.contains('{'), "{line}");
    }

    /// T3 (issue #22): a 400 whose body is the account over its budget
    /// gets the spend-limit line, not the raw refusal.
    #[test]
    fn budget_400_line_says_spend_limit() {
        let e = ProviderError::Http {
            status: 400,
            body: "{\"error\": {\"message\": \"Budget has been exceeded! Current cost: \
                   30.499857365, Max budget: 0.0\"}}"
                .into(),
        };
        let line = e.plain_line();
        assert_eq!(
            line,
            "the provider refused: the account's spend limit is reached — top up or raise the \
             key budget, then type continue"
        );
        assert!(line.contains("continue"), "{line}");
        assert!(!line.contains("Budget has been exceeded"), "{line}");
    }

    /// T4 (issue #22): a 400 that is not about spend still gets a plain
    /// line, never the raw JSON.
    #[test]
    fn other_400_line_uses_generic_fallback() {
        let e = ProviderError::Http {
            status: 400,
            body: "{\"error\": {\"message\": \"bad request\"}}".into(),
        };
        assert_eq!(
            e.plain_line(),
            "the provider refused the request (HTTP 400); type continue to retry"
        );
    }

    /// T5 (issue #22): transience matches the adapters' retry class,
    /// widened to every 5xx; a refusal never retries.
    #[test]
    fn is_transient_match_retry_classes() {
        let transient = [
            ProviderError::Transport("dropped".into()),
            ProviderError::Http {
                status: 429,
                body: String::new(),
            },
            ProviderError::Http {
                status: 500,
                body: String::new(),
            },
            ProviderError::Http {
                status: 503,
                body: String::new(),
            },
            ProviderError::RateLimited,
        ];
        for e in transient {
            assert!(e.is_transient(), "{e:?} is transient");
        }
        let refused = [
            ProviderError::Http {
                status: 400,
                body: String::new(),
            },
            ProviderError::Http {
                status: 401,
                body: String::new(),
            },
            ProviderError::Protocol("unreadable".into()),
            ProviderError::Unsupported("images".into()),
        ];
        for e in refused {
            assert!(!e.is_transient(), "{e:?} is not transient");
        }
    }

    /// T5 (issue #96): a cut reply round-trips through serde, never
    /// retries, and keeps its identity through `with_attempts` — no
    /// adapter retried it, so there is nothing to prefix.
    #[test]
    fn a_cut_reply_round_trips_and_never_retries() {
        let cut = ProviderError::Cut;
        let json = serde_json::to_string(&cut).unwrap();
        assert_eq!(
            serde_json::from_str::<ProviderError>(&json).unwrap(),
            ProviderError::Cut
        );
        assert!(!cut.is_transient());
        assert_eq!(
            ProviderError::Cut.with_attempts(4, Duration::from_millis(70_800)),
            ProviderError::Cut
        );
        assert_eq!(
            cut.plain_line(),
            "the model's reply was cut off; type continue to retry"
        );
    }
}
