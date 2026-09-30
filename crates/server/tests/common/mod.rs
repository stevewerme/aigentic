// Each test binary uses a different part of this module, so an item that
// one binary never touches is not dead: it is the other binary's.
#![allow(dead_code)]

//! What the daemon's test binaries share: a scripted provider, the
//! provider factory that hands one out per built thread, and the small
//! helpers a script writes its events with. Both `server.rs` (#55) and
//! `runs.rs` (#58) drive a real daemon with these.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use aigentic_runtime::aigentic_core::{
    Capabilities, CompletionRequest, Message, Provider, ProviderEvent, ToolCall,
};
use aigentic_server::build::{BuildError, ProviderFactory};
use futures_core::Stream;
use serde_json::json;

/// Every thread gets the same script, in order; `None` pends.
#[allow(dead_code)]
pub struct Scripted(Mutex<VecDeque<Option<Vec<ProviderEvent>>>>);

impl Provider for Scripted {
    fn complete(
        &self,
        _: &CompletionRequest<'_>,
    ) -> Pin<Box<dyn Stream<Item = ProviderEvent> + Send + '_>> {
        match self.0.lock().unwrap().pop_front().flatten() {
            Some(events) => Box::pin(futures_util::stream::iter(events)),
            None => Box::pin(futures_util::stream::pending()),
        }
    }
    fn count_tokens(&self, _: &[Message]) -> u64 {
        7
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supports_tools: true,
            supports_images: false,
            supports_caching: false,
            supports_structured_output: false,
            max_context_tokens: 1000,
        }
    }
}

/// One script per thread built, in order.
#[allow(dead_code)]
pub type Scripts = VecDeque<Vec<Option<Vec<ProviderEvent>>>>;

#[allow(dead_code)]
pub struct Factory(Arc<Mutex<Scripts>>);

impl Factory {
    /// A factory that hands out `scripts`, one per thread built.
    pub fn new(scripts: Scripts) -> Self {
        Factory(Arc::new(Mutex::new(scripts)))
    }
}

impl ProviderFactory for Factory {
    fn build(
        &self,
        _: &str,
    ) -> Result<(Box<dyn aigentic_runtime::aigentic_core::Provider>, String), BuildError> {
        let script = self.0.lock().unwrap().pop_front().unwrap_or_default();
        Ok((
            Box::new(Scripted(Mutex::new(script.into()))),
            "scripted".into(),
        ))
    }
}

pub fn text(t: &str) -> ProviderEvent {
    ProviderEvent::TextDelta(t.into())
}
pub fn done() -> ProviderEvent {
    ProviderEvent::Done {
        finish_reason: "stop".into(),
    }
}
pub fn bash(id: &str, command: &str) -> ProviderEvent {
    ProviderEvent::ToolCall(ToolCall {
        id: id.into(),
        name: "bash".into(),
        args: json!({"command": command}),
    })
}
pub fn tool_use() -> ProviderEvent {
    ProviderEvent::Done {
        finish_reason: "tool_use".into(),
    }
}
