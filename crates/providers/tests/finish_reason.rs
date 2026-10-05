//! How a stream ends (issue #96): the provider's own reason is kept, a
//! marker that arrives with no reason gives the adapter's clean default,
//! and an EOF with no reason at all is a cut — the reply may stop
//! mid-sentence or mid tool call, so the adapter flushes no call.
//!
//! One table, run against both adapters through their public
//! `parse_stream`, so the two never drift apart.

use aigentic_core::{CUT_STREAM, ProviderEvent};
use bytes::Bytes;
use futures_util::StreamExt;

// Recorded from TensorX (z-ai/glm-5.3) and the Messages API
// (claude-opus-5) by `fixtures/record.sh`.
const OPENAI_TEXT: &str = include_str!("../fixtures/openai_compat/text.sse");
const OPENAI_TOOL_CALLS: &str = include_str!("../fixtures/openai_compat/tool_calls.sse");
const ANTHROPIC_TEXT: &str = include_str!("../fixtures/anthropic/text.sse");
const ANTHROPIC_TOOL_CALLS: &str = include_str!("../fixtures/anthropic/tool_calls.sse");

/// Which adapter's wire a row is written for.
#[derive(Clone, Copy)]
enum Which {
    OpenAi,
    Anthropic,
}

/// One adapter's fixture and the four ends of stream the table needs,
/// each derived from it rather than hand-built:
///
/// - `full`: the whole recording, reason and marker as they arrived;
/// - `bare`: the same with the reason taken out, the marker kept, so the
///   stream ends with a marker and nothing recorded;
/// - `marked`: the reason kept, the marker gone, so the stream dies after
///   its reason;
/// - `cut`: the recording cut off inside a tool call's arguments, before
///   any reason or marker — the case #96 is about, which used to be
///   flushed and run.
struct Wire {
    which: Which,
    /// The whole recording, and the reason it spells out.
    full: &'static str,
    reason: &'static str,
    /// The recording of a plain reply, and the reason it spells out.
    text: &'static str,
    text_reason: &'static str,
    /// What the marker means when no reason preceded it.
    clean: &'static str,
    bare: String,
    marked: String,
    cut: String,
}

/// The fixture's first `lines` lines: the stream dying there. The last
/// event is left without its terminating blank line, as a dropped
/// connection leaves it.
fn cut_after(fixture: &str, lines: usize) -> String {
    let mut out = fixture.lines().take(lines).collect::<Vec<_>>().join("\n");
    out.push('\n');
    out
}

/// Drop the line holding `needle`, leaving the events around it intact.
fn without_line(fixture: &str, needle: &str) -> String {
    let kept: Vec<&str> = fixture
        .lines()
        .filter(|line| !line.contains(needle))
        .collect();
    kept.join("\n")
}

/// Replace `needle` in the fixture's text, leaving valid JSON behind.
fn without_text(fixture: &str, needle: &str, with: &str) -> String {
    fixture.replace(needle, with)
}

fn openai() -> Wire {
    Wire {
        which: Which::OpenAi,
        full: OPENAI_TOOL_CALLS,
        reason: "tool_calls",
        text: OPENAI_TEXT,
        text_reason: "stop",
        clean: "stop",
        // The reason chunk is one whole line; `[DONE]` stays.
        bare: without_line(OPENAI_TOOL_CALLS, "\"finish_reason\""),
        marked: without_line(OPENAI_TOOL_CALLS, "data: [DONE]"),
        // Inside the second call's arguments: the chunk at 37 carries the
        // reason, and nothing has been flushed yet. Today this was
        // flushed and run as two calls.
        cut: cut_after(OPENAI_TOOL_CALLS, 35),
    }
}

fn anthropic() -> Wire {
    Wire {
        which: Which::Anthropic,
        full: ANTHROPIC_TOOL_CALLS,
        reason: "tool_use",
        text: ANTHROPIC_TEXT,
        text_reason: "end_turn",
        clean: "end_turn",
        // The reason rides a `message_delta` that also carries usage, so
        // only the value goes; `message_stop` stays.
        bare: without_text(
            ANTHROPIC_TOOL_CALLS,
            "\"stop_reason\":\"tool_use\"",
            "\"stop_reason\":null",
        ),
        marked: ANTHROPIC_TOOL_CALLS
            .split_once("event: message_stop")
            .expect("the fixture has a marker")
            .0
            .to_owned(),
        // Inside the first call's `input_json_delta`. The block at 37 is
        // where that call's arguments close: a cut there would already
        // have delivered the call whole, so the cut is taken before it.
        cut: cut_after(ANTHROPIC_TOOL_CALLS, 35),
    }
}

/// Drive one wire text through the adapter's public stream.
async fn events(which: Which, wire: &str) -> Vec<ProviderEvent> {
    // In chunks, so the cut lands mid-chunk as often as not.
    let chunks: Vec<Result<Bytes, std::io::Error>> = wire
        .as_bytes()
        .chunks(7)
        .map(|c| Ok(Bytes::copy_from_slice(c)))
        .collect();
    let stream = futures_util::stream::iter(chunks);
    match which {
        Which::OpenAi => {
            aigentic_providers::openai_compat::parse_stream(stream)
                .collect()
                .await
        }
        Which::Anthropic => {
            aigentic_providers::anthropic::parse_stream(stream)
                .collect()
                .await
        }
    }
}

/// The `Done` reason of a stream that must have ended, or a panic naming
/// what it did instead.
fn done_reason(events: &[ProviderEvent]) -> &str {
    match events.last() {
        Some(ProviderEvent::Done { finish_reason }) => finish_reason,
        other => panic!("the stream did not end: {other:?}"),
    }
}

/// (a) A reason the provider sent is kept, whether or not a marker
/// follows it.
#[tokio::test]
async fn a_reason_the_provider_sent_is_the_reason() {
    for wire in [openai(), anthropic()] {
        let out = events(wire.which, wire.full).await;
        assert_eq!(done_reason(&out), wire.reason, "{:?}", wire.full);

        let out = events(wire.which, wire.text).await;
        assert_eq!(done_reason(&out), wire.text_reason, "{:?}", wire.text);
    }
}

/// (b) The marker with no reason recorded is the adapter's clean
/// default, not a cut.
#[tokio::test]
async fn a_marker_with_no_reason_gives_the_clean_default() {
    for wire in [openai(), anthropic()] {
        let out = events(wire.which, &wire.bare).await;
        assert_eq!(done_reason(&out), wire.clean, "{:?}", wire.bare);
        assert_ne!(done_reason(&out), CUT_STREAM);
    }
}

/// (c) A reason seen, then an EOF with no marker: the reason stands.
#[tokio::test]
async fn a_reason_then_eof_with_no_marker_keeps_the_reason() {
    for wire in [openai(), anthropic()] {
        let out = events(wire.which, &wire.marked).await;
        assert_eq!(done_reason(&out), wire.reason, "{:?}", wire.marked);
    }
}

/// (d) An EOF with no reason is a cut: `end_of_stream`, and the tool
/// call it interrupted is never flushed. The text that arrived before the
/// cut is emitted exactly as the whole recording emits it.
#[tokio::test]
async fn an_eof_with_no_reason_is_a_cut_that_flushes_no_tool_call() {
    for wire in [openai(), anthropic()] {
        let whole = events(wire.which, wire.full).await;
        let out = events(wire.which, &wire.cut).await;

        assert_eq!(done_reason(&out), CUT_STREAM, "{:?}", wire.cut);
        assert!(
            !out.iter().any(|e| matches!(e, ProviderEvent::ToolCall(_))),
            "a cut flushed a tool call: {out:?}"
        );
        // Everything the cut emitted before its own `Done` is what the
        // recording emitted too: a cut loses what followed, not what had
        // already arrived. Compared against the recording's own events,
        // not a prefix of them, because the adapters order differently —
        // OpenAI emits the accumulated reasoning blob at the end,
        // Anthropic streams its text as it comes.
        let arrived = &out[..out.len() - 1];
        assert!(
            !arrived.is_empty(),
            "the cut dropped what had already arrived"
        );
        let kept: Vec<&ProviderEvent> = whole.iter().filter(|e| arrived.contains(e)).collect();
        assert_eq!(
            kept.len(),
            arrived.len(),
            "the cut invented an event: {out:?}"
        );
    }
}
