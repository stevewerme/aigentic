//! Canonical types and traits for the Aigentic harness.
//!
//! This crate depends on no other workspace crate and no async runtime. It
//! defines the message, event and tool vocabulary that every other crate
//! speaks, plus the object-safe [`Provider`] and [`Tool`] traits that
//! adapters implement. See `docs/PRD.md` and `docs/PLAN-phase0.md`.

pub mod author;
pub mod budget;
pub mod content;
pub mod event;
pub mod message;
pub mod provider;
pub mod role;
pub mod tool;

pub use author::{AgentId, Author, UserId};
pub use budget::Budget;
pub use content::{ContentBlock, Image, ProviderBlob, ToolCall, ToolResult};
pub use event::{Event, EventKind};
pub use message::Message;
pub use provider::{Capabilities, CompletionRequest, Provider, ProviderError, ProviderEvent};
pub use role::Role;
pub use tool::{RiskClass, Tool, ToolError, ToolOutput, ToolSpec};

use std::future::Future;
use std::pin::Pin;

/// A boxed, sendable future. Defined here so `core` needs only `futures-core`.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
