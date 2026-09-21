//! Messages API streaming events to `ProviderEvent`s.
//!
//! Text is forwarded as it arrives. Tool inputs accumulate as
//! `input_json_delta` fragments and are emitted whole on
//! `content_block_stop`. Thinking blocks accumulate text and signature and
//! become one `Blob` each. Usage is assembled from `message_start` (input
//! and cache counts) and `message_delta` (output count).

use std::collections::{BTreeMap, VecDeque};
use std::fmt::Display;
use std::pin::Pin;

use aigentic_core::{ProviderBlob, ProviderError, ProviderEvent, ToolCall, Usage};
use bytes::Bytes;
use futures_core::Stream;
use futures_util::StreamExt;
use serde_json::{Map, Value};

use super::PROVIDER_NAME;
use crate::sse::SseParser;

#[derive(Debug)]
enum Open {
    Text,
    ToolUse {
        id: String,
        name: String,
        json: String,
    },
    Thinking {
        thinking: String,
        signature: String,
    },
    /// `redacted_thinking` and anything unknown: replayed verbatim as a blob.
    Opaque(Value),
}

#[derive(Debug, Default)]
pub struct Translator {
    open: BTreeMap<usize, Open>,
    usage: Usage,
    stop_reason: Option<String>,
    done: bool,
}

fn str_of(v: &Value, key: &str) -> String {
    v[key].as_str().unwrap_or_default().to_owned()
}

fn u64_of(v: &Value, key: &str) -> u64 {
    v[key].as_u64().unwrap_or(0)
}

impl Translator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Handle one SSE payload. The event type is taken from `data.type`.
    pub fn on_data(&mut self, data: &str) -> Vec<ProviderEvent> {
        if self.done {
            return Vec::new();
        }
        let v: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(e) => {
                self.done = true;
                return vec![ProviderEvent::Error(ProviderError::Protocol(format!(
                    "unparsable event: {e}: {data}"
                )))];
            }
        };
        let index = v["index"].as_u64().unwrap_or(0) as usize;
        match v["type"].as_str().unwrap_or("") {
            "message_start" => {
                let u = &v["message"]["usage"];
                self.usage.input_tokens = u64_of(u, "input_tokens");
                self.usage.cache_write_tokens = u64_of(u, "cache_creation_input_tokens");
                self.usage.cache_read_tokens = u64_of(u, "cache_read_input_tokens");
                Vec::new()
            }
            "content_block_start" => {
                let block = &v["content_block"];
                let open = match block["type"].as_str().unwrap_or("") {
                    "text" => Open::Text,
                    "tool_use" => Open::ToolUse {
                        id: str_of(block, "id"),
                        name: str_of(block, "name"),
                        json: String::new(),
                    },
                    "thinking" => Open::Thinking {
                        thinking: str_of(block, "thinking"),
                        signature: str_of(block, "signature"),
                    },
                    _ => Open::Opaque(block.clone()),
                };
                self.open.insert(index, open);
                // A text block may start with content already present.
                match &v["content_block"]["text"] {
                    Value::String(t) if !t.is_empty() => vec![ProviderEvent::TextDelta(t.clone())],
                    _ => Vec::new(),
                }
            }
            "content_block_delta" => {
                let delta = &v["delta"];
                match (
                    delta["type"].as_str().unwrap_or(""),
                    self.open.get_mut(&index),
                ) {
                    ("text_delta", _) => match &delta["text"] {
                        Value::String(t) if !t.is_empty() => {
                            vec![ProviderEvent::TextDelta(t.clone())]
                        }
                        _ => Vec::new(),
                    },
                    ("input_json_delta", Some(Open::ToolUse { json, .. })) => {
                        json.push_str(delta["partial_json"].as_str().unwrap_or(""));
                        Vec::new()
                    }
                    ("thinking_delta", Some(Open::Thinking { thinking, .. })) => {
                        thinking.push_str(delta["thinking"].as_str().unwrap_or(""));
                        Vec::new()
                    }
                    ("signature_delta", Some(Open::Thinking { signature, .. })) => {
                        signature.push_str(delta["signature"].as_str().unwrap_or(""));
                        Vec::new()
                    }
                    _ => Vec::new(),
                }
            }
            "content_block_stop" => self.close(index),
            "message_delta" => {
                if let Some(reason) = v["delta"]["stop_reason"].as_str() {
                    self.stop_reason = Some(reason.to_owned());
                }
                self.usage.output_tokens = u64_of(&v["usage"], "output_tokens");
                vec![ProviderEvent::Usage(self.usage)]
            }
            "message_stop" => self.finish(),
            "error" => {
                self.done = true;
                vec![ProviderEvent::Error(ProviderError::Protocol(
                    v["error"].to_string(),
                ))]
            }
            // `ping` and anything unknown.
            _ => Vec::new(),
        }
    }

    fn close(&mut self, index: usize) -> Vec<ProviderEvent> {
        let Some(open) = self.open.remove(&index) else {
            return Vec::new();
        };
        match open {
            Open::Text => Vec::new(),
            Open::ToolUse { id, name, json } => {
                let args = if json.trim().is_empty() {
                    Value::Object(Map::new())
                } else {
                    serde_json::from_str(&json).unwrap_or(Value::String(json))
                };
                vec![ProviderEvent::ToolCall(ToolCall { id, name, args })]
            }
            Open::Thinking {
                thinking,
                signature,
            } => vec![blob(serde_json::json!({
                "type": "thinking", "thinking": thinking, "signature": signature,
            }))],
            Open::Opaque(block) => vec![blob(block)],
        }
    }

    /// End of stream by `message_stop`, EOF or a transport error. On a
    /// `refusal` an unfinished tool call is dropped rather than run.
    pub fn finish(&mut self) -> Vec<ProviderEvent> {
        if self.done {
            return Vec::new();
        }
        self.done = true;
        let refused = self.stop_reason.as_deref() == Some("refusal");
        let indices: Vec<usize> = self.open.keys().copied().collect();
        let mut out = Vec::new();
        for i in indices {
            if refused && matches!(self.open.get(&i), Some(Open::ToolUse { .. })) {
                self.open.remove(&i);
                continue;
            }
            out.extend(self.close(i));
        }
        out.push(ProviderEvent::Done {
            finish_reason: self
                .stop_reason
                .take()
                .unwrap_or_else(|| "end_of_stream".to_owned()),
        });
        out
    }
}

fn blob(data: Value) -> ProviderEvent {
    ProviderEvent::Blob(ProviderBlob {
        provider: PROVIDER_NAME.to_owned(),
        data,
    })
}

struct State<S> {
    bytes: Pin<Box<S>>,
    parser: SseParser,
    translator: Translator,
    pending: VecDeque<ProviderEvent>,
    ended: bool,
}

/// Decode a Messages API SSE byte stream into provider events.
pub fn parse_stream<S, E>(bytes: S) -> impl Stream<Item = ProviderEvent> + Send
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: Display + Send,
{
    let state = State {
        bytes: Box::pin(bytes),
        parser: SseParser::new(),
        translator: Translator::new(),
        pending: VecDeque::new(),
        ended: false,
    };
    futures_util::stream::unfold(state, |mut st| async move {
        loop {
            if let Some(event) = st.pending.pop_front() {
                return Some((event, st));
            }
            if st.ended {
                return None;
            }
            match st.bytes.next().await {
                Some(Ok(chunk)) => {
                    for event in st.parser.feed(&chunk) {
                        st.pending.extend(st.translator.on_data(&event.data));
                    }
                }
                Some(Err(e)) => {
                    st.pending
                        .push_back(ProviderEvent::Error(ProviderError::Transport(
                            e.to_string(),
                        )));
                    st.ended = true;
                }
                None => {
                    if let Some(event) = st.parser.finish() {
                        st.pending.extend(st.translator.on_data(&event.data));
                    }
                    st.pending.extend(st.translator.finish());
                    st.ended = true;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // HAND-WRITTEN to the documented Messages API event shapes; replaced by
    // `record.sh anthropic <model>` recordings as the first act of the phase 1
    // acceptance run. That replacement must not change any test below.
    const TEXT: &str = include_str!("../../fixtures/anthropic/text.sse");
    const TOOL_CALLS: &str = include_str!("../../fixtures/anthropic/tool_calls.sse");
    const MAX_TOKENS: &str = include_str!("../../fixtures/anthropic/max_tokens.sse");

    fn translate(fixture: &str) -> Vec<ProviderEvent> {
        let mut parser = SseParser::new();
        let mut t = Translator::new();
        let mut out = Vec::new();
        for event in parser.feed(fixture.as_bytes()) {
            out.extend(t.on_data(&event.data));
        }
        if let Some(event) = parser.finish() {
            out.extend(t.on_data(&event.data));
        }
        out.extend(t.finish());
        out
    }

    #[test]
    fn text_reply_with_thinking_first_turn() {
        let events = translate(TEXT);
        assert_eq!(
            events,
            vec![
                ProviderEvent::Blob(ProviderBlob {
                    provider: PROVIDER_NAME.into(),
                    data: json!({"type": "thinking", "thinking": "", "signature": "EqQBCgIYAhIM"}),
                }),
                ProviderEvent::TextDelta("Hello".into()),
                ProviderEvent::TextDelta(", world.".into()),
                ProviderEvent::Usage(Usage {
                    input_tokens: 12,
                    output_tokens: 9,
                    cache_read_tokens: 0,
                    cache_write_tokens: 640,
                    reasoning_tokens: None,
                }),
                ProviderEvent::Done {
                    finish_reason: "end_turn".into()
                },
            ]
        );
    }

    #[test]
    fn two_tool_calls_second_turn_reads_cache() {
        let events = translate(TOOL_CALLS);
        assert_eq!(events.len(), 5, "{events:#?}");
        assert!(matches!(&events[0], ProviderEvent::Blob(_)));
        assert_eq!(
            events[1],
            ProviderEvent::ToolCall(ToolCall {
                id: "toolu_01A".into(),
                name: "read_file".into(),
                args: json!({"path": "Cargo.toml"}),
            })
        );
        assert_eq!(
            events[2],
            ProviderEvent::ToolCall(ToolCall {
                id: "toolu_01B".into(),
                name: "bash".into(),
                args: json!({"command": "ls -la"}),
            })
        );
        assert_eq!(
            events[3],
            ProviderEvent::Usage(Usage {
                input_tokens: 31,
                output_tokens: 84,
                cache_read_tokens: 640,
                cache_write_tokens: 0,
                reasoning_tokens: None,
            })
        );
        assert_eq!(
            events[4],
            ProviderEvent::Done {
                finish_reason: "tool_use".into()
            }
        );
    }

    #[test]
    fn max_tokens_stop_keeps_partial_text() {
        let events = translate(MAX_TOKENS);
        assert_eq!(events[0], ProviderEvent::TextDelta("The sea".into()));
        assert!(matches!(events[1], ProviderEvent::Usage(_)));
        assert_eq!(
            events[2],
            ProviderEvent::Done {
                finish_reason: "max_tokens".into()
            }
        );
    }

    #[test]
    fn eof_without_message_stop_completes_the_same_way() {
        let cut = TOOL_CALLS.rsplit_once("event: message_stop").unwrap().0;
        assert_eq!(translate(cut), translate(TOOL_CALLS));
    }

    #[test]
    fn refusal_drops_an_unfinished_tool_call() {
        let mut t = Translator::new();
        let mut out = t.on_data(
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_x","name":"bash","input":{}}}"#,
        );
        out.extend(t.on_data(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"command\": \"rm"}}"#,
        ));
        out.extend(t.on_data(
            r#"{"type":"message_delta","delta":{"stop_reason":"refusal"},"usage":{"output_tokens":5}}"#,
        ));
        out.extend(t.on_data(r#"{"type":"message_stop"}"#));
        assert!(
            !out.iter().any(|e| matches!(e, ProviderEvent::ToolCall(_))),
            "{out:#?}"
        );
        assert_eq!(
            out.last(),
            Some(&ProviderEvent::Done {
                finish_reason: "refusal".into()
            })
        );
    }

    #[test]
    fn redacted_thinking_is_replayed_verbatim() {
        let mut t = Translator::new();
        let mut out = t.on_data(
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"redacted_thinking","data":"opaque"}}"#,
        );
        out.extend(t.on_data(r#"{"type":"content_block_stop","index":0}"#));
        assert_eq!(
            out,
            vec![ProviderEvent::Blob(ProviderBlob {
                provider: PROVIDER_NAME.into(),
                data: json!({"type": "redacted_thinking", "data": "opaque"}),
            })]
        );
    }

    #[test]
    fn error_event_and_bad_json_close_the_stream() {
        let mut t = Translator::new();
        let out = t.on_data(
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
        );
        assert!(
            matches!(&out[0], ProviderEvent::Error(ProviderError::Protocol(m)) if m.contains("Overloaded"))
        );
        assert!(t.on_data(r#"{"type":"message_stop"}"#).is_empty());

        let mut t = Translator::new();
        assert!(matches!(
            t.on_data("nope")[0],
            ProviderEvent::Error(ProviderError::Protocol(_))
        ));
    }

    #[test]
    fn ping_is_ignored() {
        let mut t = Translator::new();
        assert!(t.on_data(r#"{"type":"ping"}"#).is_empty());
    }

    #[tokio::test]
    async fn parse_stream_is_chunk_boundary_agnostic() {
        for fixture in [TEXT, TOOL_CALLS, MAX_TOKENS] {
            for chunk_size in [1usize, 7, 64, 100_000] {
                let chunks: Vec<Result<Bytes, std::io::Error>> = fixture
                    .as_bytes()
                    .chunks(chunk_size)
                    .map(|c| Ok(Bytes::copy_from_slice(c)))
                    .collect();
                let events: Vec<ProviderEvent> = parse_stream(futures_util::stream::iter(chunks))
                    .collect()
                    .await;
                assert_eq!(events, translate(fixture), "chunk size {chunk_size}");
            }
        }
    }
}
