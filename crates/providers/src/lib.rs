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
