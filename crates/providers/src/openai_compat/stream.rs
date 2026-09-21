use std::collections::{BTreeMap, VecDeque};
use std::fmt::Display;
use std::pin::Pin;

use aigentic_core::{ProviderBlob, ProviderError, ProviderEvent, ToolCall};
use bytes::Bytes;
use futures_core::Stream;
use futures_util::StreamExt;
use serde::Deserialize;

use super::PROVIDER_NAME;
use super::sse::SseParser;

/// One `chat.completion.chunk`. Only the fields the translator needs; the
/// rest is ignored so vendor extensions never break parsing.
#[derive(Debug, Deserialize)]
struct Chunk {
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<WireUsage>,
    #[serde(default)]
    error: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    #[serde(default)]
    delta: Delta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
    /// llama.cpp and DeepSeek-style servers stream thinking here.
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ToolCallDelta>,
}

#[derive(Debug, Deserialize)]
struct ToolCallDelta {
    #[serde(default)]
    index: Option<usize>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: FunctionDelta,
}

#[derive(Debug, Default, Deserialize)]
struct FunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireUsage {
    prompt_tokens: u64,
    completion_tokens: u64,
}

#[derive(Debug, Default)]
struct PendingCall {
    id: Option<String>,
    name: String,
    arguments: String,
}

/// Turns decoded SSE payloads into `ProviderEvent`s. Text is forwarded as it
/// arrives; tool calls are accumulated by index and emitted whole once the
/// choice finishes; reasoning is collected into one blob emitted before
/// `Done`.
#[derive(Debug, Default)]
pub struct Translator {
    calls: BTreeMap<usize, PendingCall>,
    reasoning: String,
    finish_reason: Option<String>,
    done: bool,
}

impl Translator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Handle one event payload.
    pub fn on_data(&mut self, data: &str) -> Vec<ProviderEvent> {
        if self.done {
            return Vec::new();
        }
        if data.trim() == "[DONE]" {
            return self.finish();
        }
        let chunk: Chunk = match serde_json::from_str(data) {
            Ok(c) => c,
            Err(e) => {
                self.done = true;
                return vec![ProviderEvent::Error(ProviderError::Protocol(format!(
                    "unparsable chunk: {e}: {data}"
                )))];
            }
        };
        if let Some(err) = chunk.error {
            self.done = true;
            return vec![ProviderEvent::Error(ProviderError::Protocol(
                err.to_string(),
            ))];
        }

        let mut out = Vec::new();
        for choice in chunk.choices {
            if let Some(text) = choice.delta.content
                && !text.is_empty()
            {
                out.push(ProviderEvent::TextDelta(text));
            }
            if let Some(r) = choice.delta.reasoning_content {
                self.reasoning.push_str(&r);
            }
            for tc in choice.delta.tool_calls {
                let index = tc.index.unwrap_or(0);
                let pending = self.calls.entry(index).or_default();
                if let Some(id) = tc.id
                    && !id.is_empty()
                {
                    pending.id = Some(id);
                }
                if let Some(name) = tc.function.name {
                    pending.name.push_str(&name);
                }
                if let Some(args) = tc.function.arguments {
                    pending.arguments.push_str(&args);
                }
            }
            if let Some(reason) = choice.finish_reason {
                out.extend(self.flush_calls());
                self.finish_reason = Some(reason);
            }
        }
        if let Some(u) = chunk.usage {
            out.push(ProviderEvent::Usage {
                input_tokens: u.prompt_tokens,
                output_tokens: u.completion_tokens,
            });
        }
        out
    }

    /// End of stream, whether by `[DONE]`, EOF or a transport error.
    pub fn finish(&mut self) -> Vec<ProviderEvent> {
        if self.done {
            return Vec::new();
        }
        self.done = true;
        let mut out = self.flush_calls();
        if !self.reasoning.is_empty() {
            out.push(ProviderEvent::Blob(ProviderBlob {
                provider: PROVIDER_NAME.to_owned(),
                data: serde_json::json!({ "reasoning_content": std::mem::take(&mut self.reasoning) }),
            }));
        }
        out.push(ProviderEvent::Done {
            finish_reason: self
                .finish_reason
                .take()
                .unwrap_or_else(|| "end_of_stream".to_owned()),
        });
        out
    }

    fn flush_calls(&mut self) -> Vec<ProviderEvent> {
        std::mem::take(&mut self.calls)
            .into_iter()
            .map(|(index, call)| {
                // Ids are preserved verbatim; a server that omits them gets a
                // deterministic stand-in so results can still be linked.
                let id = call.id.unwrap_or_else(|| format!("call_{index}"));
                let args = if call.arguments.trim().is_empty() {
                    serde_json::json!({})
                } else {
                    serde_json::from_str(&call.arguments)
                        .unwrap_or(serde_json::Value::String(call.arguments))
                };
                ProviderEvent::ToolCall(ToolCall {
                    id,
                    name: call.name,
                    args,
                })
            })
            .collect()
    }
}

struct State<S> {
    bytes: Pin<Box<S>>,
    parser: SseParser,
    translator: Translator,
    pending: VecDeque<ProviderEvent>,
    ended: bool,
}

/// Decode an SSE byte stream (as `reqwest` yields it) into provider events.
/// Generic over the error type so tests can drive it without a network.
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
                    for data in st.parser.feed(&chunk) {
                        st.pending.extend(st.translator.on_data(&data));
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
                    if let Some(data) = st.parser.finish() {
                        st.pending.extend(st.translator.on_data(&data));
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

    const TEXT: &str = include_str!("../../fixtures/text.sse");
    const TOOL_CALLS: &str = include_str!("../../fixtures/tool_calls.sse");
    const FINISH_NO_DONE: &str = include_str!("../../fixtures/finish_no_done.sse");

    /// Run a fixture through the parser and translator as one chunk.
    fn translate(fixture: &str) -> Vec<ProviderEvent> {
        let mut parser = SseParser::new();
        let mut translator = Translator::new();
        let mut out = Vec::new();
        for data in parser.feed(fixture.as_bytes()) {
            out.extend(translator.on_data(&data));
        }
        if let Some(data) = parser.finish() {
            out.extend(translator.on_data(&data));
        }
        out.extend(translator.finish());
        out
    }

    #[test]
    fn text_deltas_usage_and_finish() {
        assert_eq!(
            translate(TEXT),
            vec![
                ProviderEvent::TextDelta("Hello".into()),
                ProviderEvent::TextDelta(", world".into()),
                ProviderEvent::Usage {
                    input_tokens: 12,
                    output_tokens: 3
                },
                ProviderEvent::Done {
                    finish_reason: "stop".into()
                },
            ]
        );
    }

    #[test]
    fn tool_call_deltas_are_assembled_with_ids_preserved() {
        assert_eq!(
            translate(TOOL_CALLS),
            vec![
                ProviderEvent::TextDelta("Let me look.".into()),
                ProviderEvent::ToolCall(ToolCall {
                    id: "call_abc123".into(),
                    name: "read_file".into(),
                    args: json!({"path": "Cargo.toml"}),
                }),
                ProviderEvent::ToolCall(ToolCall {
                    id: "call_def456".into(),
                    name: "bash".into(),
                    args: json!({"command": "ls -la"}),
                }),
                ProviderEvent::Usage {
                    input_tokens: 40,
                    output_tokens: 21
                },
                ProviderEvent::Done {
                    finish_reason: "tool_calls".into()
                },
            ]
        );
    }

    #[test]
    fn finish_with_inline_usage_and_no_done_marker() {
        assert_eq!(
            translate(FINISH_NO_DONE),
            vec![
                ProviderEvent::TextDelta("partial".into()),
                ProviderEvent::Usage {
                    input_tokens: 5,
                    output_tokens: 1
                },
                ProviderEvent::Done {
                    finish_reason: "length".into()
                },
            ]
        );
    }

    #[test]
    fn a_stream_without_usage_still_completes() {
        let mut t = Translator::new();
        let mut out =
            t.on_data(r#"{"choices":[{"delta":{"content":"hi"},"finish_reason":"stop"}]}"#);
        out.extend(t.on_data("[DONE]"));
        assert_eq!(
            out,
            vec![
                ProviderEvent::TextDelta("hi".into()),
                ProviderEvent::Done {
                    finish_reason: "stop".into()
                },
            ]
        );
        assert!(!out.iter().any(|e| matches!(e, ProviderEvent::Usage { .. })));
    }

    #[test]
    fn reasoning_is_collected_into_one_blob_before_done() {
        let mut t = Translator::new();
        let mut out = t.on_data(r#"{"choices":[{"delta":{"reasoning_content":"think "}}]}"#);
        out.extend(t.on_data(r#"{"choices":[{"delta":{"reasoning_content":"hard"}}]}"#));
        out.extend(t.on_data(r#"{"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]}"#));
        out.extend(t.on_data("[DONE]"));
        assert_eq!(
            out,
            vec![
                ProviderEvent::TextDelta("ok".into()),
                ProviderEvent::Blob(ProviderBlob {
                    provider: PROVIDER_NAME.into(),
                    data: json!({"reasoning_content": "think hard"}),
                }),
                ProviderEvent::Done {
                    finish_reason: "stop".into()
                },
            ]
        );
    }

    #[test]
    fn missing_tool_call_id_gets_a_stand_in() {
        let mut t = Translator::new();
        let mut out = t.on_data(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"bash","arguments":"{}"}}]}}]}"#,
        );
        out.extend(t.on_data(r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#));
        assert_eq!(
            out,
            vec![ProviderEvent::ToolCall(ToolCall {
                id: "call_0".into(),
                name: "bash".into(),
                args: json!({}),
            })]
        );
    }

    #[test]
    fn server_error_object_and_bad_json_become_protocol_errors() {
        let mut t = Translator::new();
        let out = t.on_data(r#"{"error":{"message":"model not found"}}"#);
        assert!(
            matches!(&out[0], ProviderEvent::Error(ProviderError::Protocol(m)) if m.contains("model not found"))
        );
        assert!(
            t.on_data("anything after").is_empty(),
            "stream is closed after an error"
        );

        let mut t = Translator::new();
        assert!(matches!(
            t.on_data("not json")[0],
            ProviderEvent::Error(ProviderError::Protocol(_))
        ));
    }

    #[tokio::test]
    async fn parse_stream_is_chunk_boundary_agnostic() {
        for chunk_size in [1usize, 7, 64, 100_000] {
            let chunks: Vec<Result<Bytes, std::io::Error>> = TOOL_CALLS
                .as_bytes()
                .chunks(chunk_size)
                .map(|c| Ok(Bytes::copy_from_slice(c)))
                .collect();
            let events: Vec<ProviderEvent> = parse_stream(futures_util::stream::iter(chunks))
                .collect()
                .await;
            assert_eq!(events, translate(TOOL_CALLS), "chunk size {chunk_size}");
        }
    }

    #[tokio::test]
    async fn transport_error_mid_stream_is_reported_after_partial_output() {
        let chunks: Vec<Result<Bytes, std::io::Error>> = vec![
            Ok(Bytes::from_static(
                b"data: {\"choices\":[{\"delta\":{\"content\":\"He\"}}]}\n\n",
            )),
            Err(std::io::Error::other("connection reset")),
        ];
        let events: Vec<ProviderEvent> = parse_stream(futures_util::stream::iter(chunks))
            .collect()
            .await;
        assert_eq!(events.len(), 2);
        assert_eq!(events[0], ProviderEvent::TextDelta("He".into()));
        assert!(
            matches!(&events[1], ProviderEvent::Error(ProviderError::Transport(m)) if m.contains("connection reset"))
        );
    }
}
