//! Named seams. Each currently passes through; later phases fill them in
//! without touching the loop.
//!
//! - [`Runtime::policy_check`]: phase 3, per-user per-project permissions.
//! - [`Runtime::turn_queue_next`]: phase 5, queued messages and interrupts.
//! - [`Runtime::compact`]: phase 2, truncation and summary compaction.

use aigentic_core::{Author, ContentBlock, Message, ToolCall};

use crate::Runtime;

/// What policy says about one tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyDecision {
    Allowed,
    /// Recorded as an error tool result the model sees on the next iteration.
    Denied(String),
}

impl Runtime {
    /// Policy check seam. Phase 0 allows everything.
    pub fn policy_check(&self, _call: &ToolCall) -> PolicyDecision {
        PolicyDecision::Allowed
    }

    /// Turn queue seam: a message that arrived mid-turn and should either
    /// be queued for the next turn or interrupt this one. Phase 0 has no
    /// queue, so there is never one.
    pub fn turn_queue_next(&mut self) -> Option<(Author, Vec<ContentBlock>)> {
        None
    }

    /// Compaction seam. Phase 0 returns the context unchanged.
    pub fn compact(&self, context: Vec<Message>) -> Vec<Message> {
        context
    }
}
