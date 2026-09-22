//! Decisions made outside the turn (phase 5): a permission request or an
//! `ask_human` question parks the turn on a channel that any approver, on
//! any connection, can answer. The events are the same as phase 3's; only
//! who answers, and how the turn waits, changes. `CancelToken` is how an
//! interrupt reaches a running turn, carrying who sent it.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use aigentic_core::{Author, ContentBlock};
use aigentic_log::PermissionRequestedPayload;
use tokio::sync::{mpsc, oneshot, watch};

/// What a turn is waiting for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pending {
    /// A tool call the rules ask about; anyone with `approve` decides.
    Permission {
        call_id: String,
        request: PermissionRequestedPayload,
    },
    /// The `ask_human` tool; anyone with `write` answers.
    Human { call_id: String, question: String },
}

impl Pending {
    pub fn call_id(&self) -> &str {
        match self {
            Pending::Permission { call_id, .. } | Pending::Human { call_id, .. } => call_id,
        }
    }
}

/// An answer to a `Pending`, with who gave it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answered {
    Permission {
        allow: bool,
        /// A standing grant for identical calls this session.
        session: bool,
        by: Author,
    },
    Human {
        text: String,
        by: Author,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecisionError {
    #[error("nothing is pending for call {0}")]
    NotPending(String),
    #[error("call {0} was already decided")]
    AlreadyDecided(String),
    #[error("call {call_id} is waiting for a {expected}, not a {given}")]
    WrongKind {
        call_id: String,
        expected: &'static str,
        given: &'static str,
    },
}

/// The pending requests of one thread and the channel each waits on.
/// Shared between the runtime and whoever answers (the daemon's actor).
#[derive(Default)]
pub struct Decisions {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    waiting: HashMap<String, (Pending, oneshot::Sender<Answered>)>,
    decided: HashSet<String>,
}

impl Decisions {
    pub fn new() -> Self {
        Self::default()
    }

    /// Everything waiting, in no particular order (a thread has at most
    /// one, since turns are serial).
    pub fn pending(&self) -> Vec<Pending> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .waiting
            .values()
            .map(|(p, _)| p.clone())
            .collect()
    }

    /// Answer one. The first answer wins; the turn resumes with it.
    pub fn decide(&self, call_id: &str, decided: Answered) -> Result<(), DecisionError> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some((pending, _)) = inner.waiting.get(call_id) else {
            return Err(if inner.decided.contains(call_id) {
                DecisionError::AlreadyDecided(call_id.to_owned())
            } else {
                DecisionError::NotPending(call_id.to_owned())
            });
        };
        let (expected, given) = match (pending, &decided) {
            (Pending::Permission { .. }, Answered::Permission { .. })
            | (Pending::Human { .. }, Answered::Human { .. }) => {
                let (_, tx) = inner.waiting.remove(call_id).expect("present");
                inner.decided.insert(call_id.to_owned());
                // A receiver that went away (the turn was cancelled) is
                // not an error for the decider: the decision is moot.
                let _ = tx.send(decided);
                return Ok(());
            }
            (Pending::Permission { .. }, Answered::Human { .. }) => ("decision", "human answer"),
            (Pending::Human { .. }, Answered::Permission { .. }) => ("human answer", "decision"),
        };
        Err(DecisionError::WrongKind {
            call_id: call_id.to_owned(),
            expected,
            given,
        })
    }

    /// Park: the runtime registers what it waits for and awaits the
    /// receiver.
    pub(crate) fn register(&self, pending: Pending) -> oneshot::Receiver<Answered> {
        let (tx, rx) = oneshot::channel();
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.decided.remove(pending.call_id());
        inner
            .waiting
            .insert(pending.call_id().to_owned(), (pending, tx));
        rx
    }

    /// The turn stopped waiting (an interrupt); a later answer is
    /// `AlreadyDecided`.
    pub(crate) fn withdraw(&self, call_id: &str) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.waiting.remove(call_id).is_some() {
            inner.decided.insert(call_id.to_owned());
        }
    }
}

/// How an interrupt reaches a running turn. Cloned freely; `cancel`
/// from any clone fires every waiter, carrying who interrupted. A token
/// that is never cancelled is `CancelToken::never()`.
#[derive(Debug, Clone)]
pub struct CancelToken {
    tx: watch::Sender<Option<Author>>,
    rx: watch::Receiver<Option<Author>>,
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::never()
    }
}

impl CancelToken {
    pub fn never() -> Self {
        let (tx, rx) = watch::channel(None);
        Self { tx, rx }
    }

    /// Fire. A second call keeps the first author.
    pub fn cancel(&self, by: Author) {
        self.tx.send_if_modified(|slot| {
            if slot.is_none() {
                *slot = Some(by);
                true
            } else {
                false
            }
        });
    }

    /// Who cancelled, if anyone has.
    pub fn cancelled_by(&self) -> Option<Author> {
        self.rx.borrow().clone()
    }

    /// Resolves when cancelled, with who did it.
    pub async fn cancelled(&self) -> Author {
        let mut rx = self.rx.clone();
        loop {
            if let Some(by) = rx.borrow_and_update().clone() {
                return by;
            }
            if rx.changed().await.is_err() {
                // Every sender is gone: nobody can cancel any more.
                std::future::pending::<()>().await;
            }
        }
    }
}

/// A message that arrived while a turn ran (phase 5's queue). The turn
/// appends it at once as a `user_message` with `mid_turn` set, so every
/// subscriber sees it; the projection's horizon rule keeps it out of the
/// model's context until the next turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Queued {
    pub author: Author,
    pub blocks: Vec<ContentBlock>,
}

/// The sending side, held by the daemon's actor. Cloned freely.
#[derive(Debug, Clone)]
pub struct Outbox(mpsc::UnboundedSender<Queued>);

impl Outbox {
    /// Hand a message to the running turn. `false` when no turn holds
    /// the inbox any more.
    pub fn send(&self, queued: Queued) -> bool {
        self.0.send(queued).is_ok()
    }
}

/// The receiving side, held by the turn. `Inbox::none()` never yields.
#[derive(Debug)]
pub struct Inbox {
    rx: mpsc::UnboundedReceiver<Queued>,
    /// Keeps `none()` pending rather than closed.
    _keep: Option<Outbox>,
}

/// A connected pair.
pub fn inbox() -> (Outbox, Inbox) {
    let (tx, rx) = mpsc::unbounded_channel();
    (Outbox(tx), Inbox { rx, _keep: None })
}

impl Inbox {
    /// An inbox nothing is ever posted to: the single-user REPL's.
    pub fn none() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            rx,
            _keep: Some(Outbox(tx)),
        }
    }

    /// Pends until a message arrives; never resolves for `none()`.
    pub(crate) async fn recv(&mut self) -> Queued {
        match self.rx.recv().await {
            Some(q) => q,
            None => std::future::pending().await,
        }
    }

    /// What has arrived so far, without waiting.
    pub(crate) fn drain(&mut self) -> Vec<Queued> {
        let mut out = Vec::new();
        while let Ok(q) = self.rx.try_recv() {
            out.push(q);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aigentic_core::{RiskClass, ToolCall, UserId};

    fn magnus() -> Author {
        Author::User(UserId("magnus".into()))
    }

    fn request() -> PermissionRequestedPayload {
        PermissionRequestedPayload {
            call: ToolCall {
                id: "c1".into(),
                name: "bash".into(),
                args: serde_json::json!({"command": "rm -rf x"}),
            },
            class: RiskClass::Exec,
            reason: "class exec: ask".into(),
        }
    }

    #[tokio::test]
    async fn the_first_decision_wins_and_kinds_must_match() {
        let d = Decisions::new();
        assert!(d.pending().is_empty());
        assert_eq!(
            d.decide(
                "c1",
                Answered::Human {
                    text: "x".into(),
                    by: magnus()
                }
            ),
            Err(DecisionError::NotPending("c1".into()))
        );
        let rx = d.register(Pending::Permission {
            call_id: "c1".into(),
            request: request(),
        });
        assert_eq!(d.pending().len(), 1);
        assert_eq!(d.pending()[0].call_id(), "c1");
        assert!(matches!(
            d.decide(
                "c1",
                Answered::Human {
                    text: "x".into(),
                    by: magnus()
                }
            ),
            Err(DecisionError::WrongKind {
                expected: "decision",
                given: "human answer",
                ..
            })
        ));
        d.decide(
            "c1",
            Answered::Permission {
                allow: true,
                session: false,
                by: magnus(),
            },
        )
        .unwrap();
        assert!(d.pending().is_empty());
        assert_eq!(
            d.decide(
                "c1",
                Answered::Permission {
                    allow: false,
                    session: false,
                    by: magnus()
                }
            ),
            Err(DecisionError::AlreadyDecided("c1".into()))
        );
        assert_eq!(
            rx.await.unwrap(),
            Answered::Permission {
                allow: true,
                session: false,
                by: magnus()
            }
        );
        // Withdrawn: a late answer is AlreadyDecided too.
        let _rx = d.register(Pending::Human {
            call_id: "c2".into(),
            question: "?".into(),
        });
        d.withdraw("c2");
        assert_eq!(
            d.decide(
                "c2",
                Answered::Human {
                    text: "late".into(),
                    by: magnus()
                }
            ),
            Err(DecisionError::AlreadyDecided("c2".into()))
        );
    }

    #[tokio::test]
    async fn a_cancel_token_fires_every_clone_once() {
        let token = CancelToken::never();
        let other = token.clone();
        assert_eq!(token.cancelled_by(), None);
        let waiter = tokio::spawn(async move { other.cancelled().await });
        token.cancel(magnus());
        token.cancel(Author::System);
        assert_eq!(token.cancelled_by(), Some(magnus()));
        assert_eq!(waiter.await.unwrap(), magnus());
        assert_eq!(token.cancelled().await, magnus());
        // A never token never resolves.
        let never = CancelToken::never();
        let raced =
            tokio::time::timeout(std::time::Duration::from_millis(20), never.cancelled()).await;
        assert!(raced.is_err());
    }
}
