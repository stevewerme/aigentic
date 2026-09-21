//! How a client answers a permission request. Phase 3 blocks the turn on
//! a synchronous call into the client; phase 5 turns this into an event
//! any approver can answer. The events are already the right shape.

use aigentic_core::Author;
use aigentic_log::PermissionRequestedPayload;

/// A human's answer to one request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// This call only.
    Allow,
    /// This call and every identical one until the process exits.
    AllowForSession,
    Deny,
}

pub trait Approver: Send {
    /// Who the answers are attributed to; the `permission_decided` author.
    fn author(&self) -> Author;

    /// Blocks the turn until answered. Non-interactive clients deny.
    fn ask(&mut self, request: &PermissionRequestedPayload) -> Answer;

    /// The `ask_human` tool: the human's typed answer, or `None` when no
    /// human is available (the model is told so and should treat it as a
    /// deny).
    fn ask_human(&mut self, _question: &str) -> Option<String> {
        None
    }
}

/// The default: no human, everything that asks is denied.
#[derive(Debug, Clone, Copy, Default)]
pub struct DenyAll;

impl Approver for DenyAll {
    fn author(&self) -> Author {
        Author::System
    }

    fn ask(&mut self, _request: &PermissionRequestedPayload) -> Answer {
        Answer::Deny
    }
}
