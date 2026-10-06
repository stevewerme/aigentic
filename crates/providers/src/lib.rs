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

/// How long a call that fails before any content keeps trying, and how
/// long it waits between attempts (issue #90). An outage measured in
/// minutes no longer ends a turn: the call rides out the whole window
/// with a growing backoff, and stops the moment the window has passed or
/// the provider says it will be back later than the window has left.
///
/// Wall time is not budgeted here. `max_wall_time` is checked before each
/// call, never during one, so a call may overrun that budget by up to its
/// whole window: 1,200 s by default, against a default budget of 1,800 s,
/// and a profile may set far less. A build step's wall budget therefore
/// bounds the step only after the call returns.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// How long the call may keep retrying, measured from its first
    /// attempt. `Duration::ZERO` means no retries.
    pub window: std::time::Duration,
    /// The wait before each retry, in order. Past its end, the last value
    /// repeats.
    pub backoff: Vec<std::time::Duration>,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            window: std::time::Duration::from_secs(1_200),
            backoff: [1, 3, 8, 20, 45, 90, 120]
                .map(std::time::Duration::from_secs)
                .to_vec(),
        }
    }
}

impl RetryPolicy {
    /// A policy that never retries: the doctor's probes report the first
    /// failure as it is (issue #90).
    pub fn no_retries() -> Self {
        Self {
            window: std::time::Duration::ZERO,
            ..Self::default()
        }
    }

    /// The wait before retry `attempt` (0-based, the adapter loop's own),
    /// or `None` to give up.
    ///
    /// `elapsed` is the call's own clock. It gives `None` once `elapsed`
    /// has reached the window. Otherwise the wait is
    /// `max(backoff[attempt], retry_after)`, so the last wait may end
    /// past the window: the call rides out the whole window rather than
    /// stopping short. A `retry_after` larger than the time the window
    /// has left gives `None` at once, though: the provider has said it
    /// will not be back within the window, so its 429 or 503 becomes the
    /// error the person reads.
    pub fn next_wait(
        &self,
        attempt: usize,
        elapsed: std::time::Duration,
        retry_after: Option<std::time::Duration>,
    ) -> Option<std::time::Duration> {
        if elapsed >= self.window {
            return None;
        }
        let left = self.window - elapsed;
        if let Some(after) = retry_after
            && after > left
        {
            return None;
        }
        let backoff = self.backoff_at(attempt)?;
        Some(match retry_after {
            Some(after) => backoff.max(after),
            None => backoff,
        })
    }

    /// The wait before retry `attempt`, the last value repeating.
    fn backoff_at(&self, attempt: usize) -> Option<std::time::Duration> {
        let last = self.backoff.len().checked_sub(1)?;
        self.backoff.get(attempt.min(last)).copied()
    }

    /// The most retries the window could hold, counting waits alone: a
    /// ceiling, used as `Retried.retries`, so the turn line reads
    /// `retrying k/N` with N as the most there could be (15 for the
    /// default). It counts the waits that *start* inside the window, as
    /// `next_wait` grants them, so the last retry of a full-window
    /// outage reads `N/N`, never `N+1/N`. A zero wait holds an unbounded
    /// number, so the walk stops there rather than spin. The repeating
    /// tail is counted with one division, so an absurdly long configured
    /// window cannot make this loop.
    pub fn retries_within(&self) -> u32 {
        let mut start = std::time::Duration::ZERO;
        let mut n: u128 = 0;
        for &wait in &self.backoff {
            if wait.is_zero() || start >= self.window {
                return n.min(u32::MAX as u128) as u32;
            }
            start = start.saturating_add(wait);
            n += 1;
        }
        match self.backoff.last() {
            Some(&last) if !last.is_zero() && start < self.window => {
                n += (self.window - start).as_nanos().div_ceil(last.as_nanos());
            }
            _ => {}
        }
        n.min(u32::MAX as u128) as u32
    }
}

/// A response's `Retry-After` as whole seconds (issue #90). An HTTP date
/// and anything unparsable are ignored: the backoff is the fallback.
pub(crate) fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<std::time::Duration> {
    let value = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    value
        .trim()
        .parse::<u64>()
        .ok()
        .map(std::time::Duration::from_secs)
}

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
///
/// A stream that ends with [`CUT_STREAM`](aigentic_core::CUT_STREAM)
/// *before* any content is the same case (issue #90): nothing reached
/// the caller, so it is handed back as a retryable transport failure.
/// The same cut *after* content stays the caller's `Cut`.
///
/// The wait also races `tx.closed()`: an attempt in flight is abandoned
/// the moment the turn drops the stream, instead of running on for up to
/// `stall` and still being billed (issue #90).
pub(crate) async fn forward(
    mut events: std::pin::Pin<
        Box<dyn futures_core::Stream<Item = aigentic_core::ProviderEvent> + Send>,
    >,
    tx: &tokio::sync::mpsc::UnboundedSender<aigentic_core::ProviderEvent>,
    stall: std::time::Duration,
) -> Option<aigentic_core::ProviderEvent> {
    use aigentic_core::{CUT_STREAM, ProviderError, ProviderEvent};
    use futures_util::StreamExt;
    let mut sent = false;
    let mut deadline = tokio::time::Instant::now() + stall;
    loop {
        let next = tokio::select! {
            biased;
            _ = tx.closed() => return None,
            next = tokio::time::timeout_at(deadline, events.next()) => next,
        };
        let event = match next {
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
            // Nothing reached the caller, so the adapter may try again
            // (issue #90): a stream cut before any content is a failure,
            // not a finished reply.
            ProviderEvent::Done { finish_reason } if !sent && finish_reason == CUT_STREAM => {
                return Some(ProviderEvent::Error(ProviderError::Transport(
                    "the stream ended before any content".into(),
                )));
            }
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

/// How long a connect may take before the attempt fails as a transport
/// error and goes to backoff (issue #90). Without it, a connect that
/// never completes is bounded only by the OS, roughly one to two
/// minutes, and `read_timeout` starts after the connection.
pub(crate) const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// One HTTP client configuration for every adapter. Idle pooled
/// connections are dropped after a few seconds: a tool call can take
/// longer than a server's keep-alive timeout, and reusing a connection the
/// server has already closed fails the next request with a transport
/// error instead of a reply.
///
/// Every read, headers or a body chunk, must arrive within `stall`: a
/// stream that goes silent fails as a transport error instead of hanging
/// the turn (a TensorX stream once sat silent for fifteen minutes). The
/// `connect` timeout bounds the connect only, and is added alongside the
/// read timeout, never in its place: a loopback connect completes at
/// once, so a slow local endpoint loading a model is unaffected, and its
/// delay comes after the connection.
fn build_client(connect: std::time::Duration, stall: std::time::Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(connect)
        .pool_idle_timeout(std::time::Duration::from_secs(5))
        .read_timeout(stall)
        .build()
        .expect("a default TLS backend is compiled in")
}

/// The bounds the one shared client is built with: stated in one place,
/// so [`http_client`] and the test that pins it cannot drift apart
/// (issue #90).
fn client_bounds() -> (std::time::Duration, std::time::Duration) {
    (CONNECT_TIMEOUT, STALL)
}

pub(crate) fn http_client() -> reqwest::Client {
    let (connect, stall) = client_bounds();
    build_client(connect, stall)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aigentic_core::{CUT_STREAM, ProviderError, ProviderEvent};

    fn ms(n: u64) -> std::time::Duration {
        std::time::Duration::from_millis(n)
    }

    /// Issue #90: a policy that gives up in a fifth of a second, so a
    /// test never waits real seconds. `retries_within` is 7 for it.
    fn test_policy() -> RetryPolicy {
        RetryPolicy {
            window: ms(200),
            backoff: vec![ms(10), ms(20), ms(40)],
        }
    }

    /// `next_wait` follows the backoff with a 0-based index, the loop's
    /// own, and repeats the last wait past the end (issue #90).
    #[test]
    fn backoff_follows_its_index_and_repeats_the_last() {
        let p = test_policy();
        assert_eq!(p.next_wait(0, ms(0), None), Some(ms(10)));
        assert_eq!(p.next_wait(1, ms(10), None), Some(ms(20)));
        assert_eq!(p.next_wait(2, ms(30), None), Some(ms(40)));
        assert_eq!(p.next_wait(3, ms(70), None), Some(ms(40)));
        assert_eq!(p.next_wait(9, ms(100), None), Some(ms(40)));
    }

    /// Issue #90: a `Retry-After` larger than the backoff wins, and a
    /// smaller one does not shorten the wait: `Retry-After: 0` is not a
    /// retry with no pause.
    #[test]
    fn a_larger_retry_after_wins() {
        let p = test_policy();
        assert_eq!(p.next_wait(0, ms(0), Some(ms(5))), Some(ms(10)));
        assert_eq!(p.next_wait(0, ms(0), Some(ms(30))), Some(ms(30)));
        assert_eq!(p.next_wait(2, ms(0), Some(ms(40))), Some(ms(40)));
        assert_eq!(
            p.next_wait(0, ms(0), Some(std::time::Duration::ZERO)),
            Some(ms(10))
        );
    }

    /// Issue #90: the elapsed clock is the give-up boundary. A wait that
    /// *starts* inside the window may end past it — the call rides out
    /// the whole window rather than stopping short — and the window is
    /// reached, not passed, before giving up.
    #[test]
    fn the_window_is_the_give_up_boundary() {
        let p = test_policy();
        let inside = p.next_wait(5, ms(190), None).expect("inside the window");
        assert!(
            ms(190) + inside > p.window,
            "{inside:?} may end past the window"
        );
        assert_eq!(p.next_wait(0, p.window - ms(1), None), Some(ms(10)));
        assert_eq!(p.next_wait(0, p.window, None), None);
        assert_eq!(p.next_wait(0, ms(1_000), None), None);
        // The default window is the spec's 20 minutes.
        assert_eq!(
            RetryPolicy::default().window,
            std::time::Duration::from_secs(1_200)
        );
    }

    /// Issue #90: a `Retry-After` that reaches past the window ends the
    /// call at once — the provider has said it will not be back inside
    /// the window, so its status is the error to read. A header exactly
    /// equal to the time left is still a wait.
    #[test]
    fn a_retry_after_past_the_window_gives_up_at_once() {
        let p = test_policy();
        assert_eq!(
            p.next_wait(0, ms(50), Some(std::time::Duration::from_secs(1))),
            None
        );
        assert_eq!(p.next_wait(0, ms(50), Some(ms(150))), Some(ms(150)));
    }

    /// Issue #90: the default's `retries_within` is the number of waits
    /// `next_wait` grants over a full-window outage, derived by walking
    /// it rather than stated, so the turn line never reads `15/14`.
    #[test]
    fn retries_within_counts_the_waits_the_window_holds() {
        fn walk(p: &RetryPolicy) -> u32 {
            let mut elapsed = std::time::Duration::ZERO;
            let mut n = 0u32;
            while let Some(wait) = p.next_wait(n as usize, elapsed, None) {
                elapsed += wait;
                n += 1;
            }
            n
        }
        let t = test_policy();
        assert_eq!(t.retries_within(), walk(&t));
        assert_eq!(t.retries_within(), 7);
        let d = RetryPolicy::default();
        let derived = walk(&d);
        assert_eq!(d.retries_within(), derived);
        assert_eq!(derived, 15);
        assert!(
            d.retries_within() > 3,
            "the outage window is wider than before"
        );

        // A window no one would configure but a typo could: the count is
        // closed-form, so this returns instead of walking for ever.
        let absurd = RetryPolicy {
            window: std::time::Duration::from_secs(u64::MAX),
            ..RetryPolicy::default()
        };
        assert!(absurd.retries_within() > d.retries_within());
    }

    /// Issue #90: the doctor's policy. A zero window gives up at the
    /// first failure, whatever the elapsed time.
    #[test]
    fn no_retries_gives_up_at_the_first_failure() {
        let p = RetryPolicy::no_retries();
        assert_eq!(p.next_wait(0, std::time::Duration::ZERO, None), None);
        assert_eq!(p.next_wait(3, std::time::Duration::ZERO, None), None);
        assert_eq!(p.retries_within(), 0);
    }

    /// Issue #90: `Retry-After` is read as whole seconds only. An HTTP
    /// date and anything unparsable are ignored, so the backoff decides.
    #[test]
    fn retry_after_reads_whole_seconds_only() {
        let headers = |value: &str| {
            let mut h = reqwest::header::HeaderMap::new();
            h.insert(reqwest::header::RETRY_AFTER, value.parse().unwrap());
            h
        };
        assert_eq!(
            retry_after(&headers("1")),
            Some(std::time::Duration::from_secs(1))
        );
        assert_eq!(retry_after(&headers("0")), Some(std::time::Duration::ZERO));
        assert_eq!(
            retry_after(&headers("120")),
            Some(std::time::Duration::from_secs(120))
        );
        assert_eq!(retry_after(&headers("Wed, 21 Oct 2026 07:28:00 GMT")), None);
        assert_eq!(retry_after(&headers("soon")), None);
        assert_eq!(retry_after(&reqwest::header::HeaderMap::new()), None);
    }

    /// Issue #90: every adapter's client bounds a connect as well as a
    /// read, so an attempt that never connects fails in 15 s instead of
    /// the OS's minute or two, and goes to backoff. The timeout's
    /// behaviour is not proven end to end: a non-routable address fails
    /// fast for another reason (see the implementation report).
    #[test]
    fn the_shared_client_bounds_a_connect_and_a_read() {
        assert_eq!(CONNECT_TIMEOUT, std::time::Duration::from_secs(15));
        assert_eq!(STALL, std::time::Duration::from_secs(120));
        // The one client every adapter uses is built with both bounds:
        // the connect timeout is added alongside the read timeout, never
        // in its place.
        assert_eq!(client_bounds(), (CONNECT_TIMEOUT, STALL));
        let _ = build_client(CONNECT_TIMEOUT, STALL);
        let _ = http_client();
        // The timeout's behaviour is not proven end to end: a
        // non-routable address fails fast for another reason, so the
        // ledger records this as a pinned constant, not a live
        // connection that was made to time out.
    }

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

    /// Issue #90: a stream that ends before any content is a retryable
    /// failure, not a finished reply — nothing reached the caller, so
    /// the adapter may try again. The `Done` is never passed on.
    #[tokio::test]
    async fn a_cut_before_any_content_is_returned_for_a_retry() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let failure = forward(
            Box::pin(futures_util::stream::iter(vec![ProviderEvent::Done {
                finish_reason: CUT_STREAM.into(),
            }])),
            &tx,
            std::time::Duration::from_secs(5),
        )
        .await;
        assert!(
            matches!(&failure, Some(ProviderEvent::Error(ProviderError::Transport(m)))
                if m == "the stream ended before any content"),
            "{failure:?}"
        );
        assert!(rx.try_recv().is_err(), "nothing reached the caller");
    }

    /// Issue #90: the same cut after content has gone out is the
    /// caller's `Cut` (issue #96), passed on as the last event: a retry
    /// would repeat what the person can already see.
    #[tokio::test]
    async fn a_cut_after_content_is_passed_on() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let failure = forward(
            Box::pin(futures_util::stream::iter(vec![
                ProviderEvent::TextDelta("hi".into()),
                ProviderEvent::Done {
                    finish_reason: CUT_STREAM.into(),
                },
            ])),
            &tx,
            std::time::Duration::from_secs(5),
        )
        .await;
        assert!(failure.is_none());
        assert_eq!(
            rx.try_recv().unwrap(),
            ProviderEvent::TextDelta("hi".into())
        );
        assert_eq!(
            rx.try_recv().unwrap(),
            ProviderEvent::Done {
                finish_reason: CUT_STREAM.into()
            }
        );
    }

    /// Issue #90: a turn that drops the stream abandons the attempt in
    /// flight at once, instead of waiting out the stall and still being
    /// billed.
    #[tokio::test]
    async fn a_dropped_stream_abandons_the_wait() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let stall = std::time::Duration::from_secs(60);
        let task = tokio::spawn(async move {
            // A stream that never produces anything: without the
            // `closed()` race this would sit out the whole stall.
            forward(stalled_after(vec![]), &tx, stall).await
        });
        drop(rx);
        let ended = tokio::time::timeout(std::time::Duration::from_millis(500), task)
            .await
            .expect("the forward ends with the stream, not with the stall");
        assert!(ended.unwrap().is_none());
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
