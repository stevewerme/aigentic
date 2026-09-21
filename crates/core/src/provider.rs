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
    /// The stream failed; no further items follow.
    Error(ProviderError),
}

/// Errors a provider adapter can surface.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
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
