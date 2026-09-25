//! Provider adapters. Each adapter translates canonical messages to one
//! backend's wire format and its streamed reply back into
//! [`ProviderEvent`](aigentic_core::ProviderEvent)s, and nothing else. Written
//! against raw HTTP with `reqwest` and `serde`; no vendor SDKs.

pub mod anthropic;
pub mod estimate;
pub mod openai_compat;
pub mod sse;

pub use anthropic::{Anthropic, AnthropicConfig, Thinking};
pub use openai_compat::{
    OpenAiCompat, OpenAiCompatConfig, REASONING_EFFORT_PARAM, ReasoningEffort,
};

/// How long a response may go without a byte, or without model output,
/// before it counts as dead. Reasoning models stream thinking deltas
/// throughout, so two minutes of silence is a stall, not a slow answer.
/// The byte clock is reqwest's read timeout; the output clock is
/// [`forward`], because a stream can stay byte-alive on keep-alive
/// comments while producing nothing (issue #42).
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

/// Pass a started stream's events to `tx`, watching for a model that has
/// gone quiet (issue #42). Only model output resets the clock: a text,
/// tool-call or blob event, usage or a finish. An empty text delta does
/// not, and keep-alive comments never become events at all, so a stream
/// that stays byte-alive while producing nothing ends after `stall`.
///
/// Returns the failure when the stream stalled or broke before anything
/// was passed on: nothing reached the caller, so the adapter may retry
/// it like a refused request. Once an event has gone out, a stall or a
/// break is passed on as the stream's last event and `None` comes back:
/// retrying then would repeat output the caller already has.
pub(crate) async fn forward(
    mut events: std::pin::Pin<
        Box<dyn futures_core::Stream<Item = aigentic_core::ProviderEvent> + Send>,
    >,
    tx: &tokio::sync::mpsc::UnboundedSender<aigentic_core::ProviderEvent>,
    stall: std::time::Duration,
) -> Option<aigentic_core::ProviderEvent> {
    use aigentic_core::{ProviderError, ProviderEvent};
    use futures_util::StreamExt;
    let mut sent = false;
    let mut deadline = tokio::time::Instant::now() + stall;
    loop {
        let event = match tokio::time::timeout_at(deadline, events.next()).await {
            Ok(Some(event)) => event,
            Ok(None) => return None,
            Err(_) => {
                let stalled = ProviderEvent::Error(ProviderError::Transport(format!(
                    "no model output for {} s",
                    stall.as_secs()
                )));
                if !sent {
                    return Some(stalled);
                }
                let _ = tx.send(stalled);
                return None;
            }
        };
        match &event {
            ProviderEvent::TextDelta(t) if t.is_empty() => continue,
            ProviderEvent::Error(ProviderError::Transport(_)) if !sent => return Some(event),
            ProviderEvent::Error(_) => {
                let _ = tx.send(event);
                return None;
            }
            _ => {}
        }
        deadline = tokio::time::Instant::now() + stall;
        sent = true;
        let _ = tx.send(event);
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

#[cfg(test)]
mod tests {
    use super::*;
    use aigentic_core::{ProviderError, ProviderEvent};

    fn stalled_after(
        first: Vec<ProviderEvent>,
    ) -> std::pin::Pin<Box<dyn futures_core::Stream<Item = ProviderEvent> + Send>> {
        use futures_util::StreamExt;
        Box::pin(futures_util::stream::iter(first).chain(futures_util::stream::pending()))
    }

    /// Issue #42: silence before any output is handed back as a
    /// retryable failure, and an empty text delta does not count as
    /// output (it neither resets the clock nor reaches the caller).
    #[tokio::test]
    async fn silence_before_output_is_returned_for_a_retry() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let stall = std::time::Duration::from_millis(100);
        let failure = forward(
            stalled_after(vec![ProviderEvent::TextDelta(String::new())]),
            &tx,
            stall,
        )
        .await;
        assert!(
            matches!(
                failure,
                Some(ProviderEvent::Error(ProviderError::Transport(_)))
            ),
            "{failure:?}"
        );
        assert!(rx.try_recv().is_err(), "nothing reached the caller");
    }

    /// Issue #42: silence after output is passed on as the last event.
    #[tokio::test]
    async fn silence_after_output_is_passed_on() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let stall = std::time::Duration::from_millis(100);
        let failure = forward(
            stalled_after(vec![ProviderEvent::TextDelta("hi".into())]),
            &tx,
            stall,
        )
        .await;
        assert!(failure.is_none());
        assert_eq!(
            rx.try_recv().unwrap(),
            ProviderEvent::TextDelta("hi".into())
        );
        assert!(matches!(
            rx.try_recv().unwrap(),
            ProviderEvent::Error(ProviderError::Transport(_))
        ));
    }
}
