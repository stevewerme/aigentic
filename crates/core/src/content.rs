use serde::{Deserialize, Serialize};

/// A tool invocation requested by the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub args: serde_json::Value,
}

/// The outcome of a [`ToolCall`], linked by `id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    /// Links to the `ToolCall::id` it answers.
    pub id: String,
    pub content: String,
    pub is_error: bool,
}

/// An inline image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Image {
    /// MIME type, e.g. `"image/png"`.
    pub media_type: String,
    /// Base64-encoded image data.
    pub data: String,
}

/// Opaque provider-specific content (thinking blocks, signatures, ...).
/// Only the adapter named in `provider` replays it; every other adapter drops it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderBlob {
    /// Adapter that produced it; only that adapter replays it.
    pub provider: String,
    pub data: serde_json::Value,
}

/// One unit of message content.
///
/// Serialises with a `type` tag: `{"type":"text","text":"hi"}`,
/// `{"type":"tool_call","id":..,"name":..,"args":..}`, and so on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "ContentBlockWire", into = "ContentBlockWire")]
pub enum ContentBlock {
    Text(String),
    ToolCall(ToolCall),
    ToolResult(ToolResult),
    Image(Image),
    ProviderBlob(ProviderBlob),
}

/// Wire shape. Internal tagging cannot wrap a bare string, so `Text` gets an
/// explicit `text` field; the struct variants flatten naturally.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlockWire {
    Text { text: String },
    ToolCall(ToolCall),
    ToolResult(ToolResult),
    Image(Image),
    ProviderBlob(ProviderBlob),
}

impl From<ContentBlock> for ContentBlockWire {
    fn from(block: ContentBlock) -> Self {
        match block {
            ContentBlock::Text(text) => ContentBlockWire::Text { text },
            ContentBlock::ToolCall(v) => ContentBlockWire::ToolCall(v),
            ContentBlock::ToolResult(v) => ContentBlockWire::ToolResult(v),
            ContentBlock::Image(v) => ContentBlockWire::Image(v),
            ContentBlock::ProviderBlob(v) => ContentBlockWire::ProviderBlob(v),
        }
    }
}

impl From<ContentBlockWire> for ContentBlock {
    fn from(wire: ContentBlockWire) -> Self {
        match wire {
            ContentBlockWire::Text { text } => ContentBlock::Text(text),
            ContentBlockWire::ToolCall(v) => ContentBlock::ToolCall(v),
            ContentBlockWire::ToolResult(v) => ContentBlock::ToolResult(v),
            ContentBlockWire::Image(v) => ContentBlock::Image(v),
            ContentBlockWire::ProviderBlob(v) => ContentBlock::ProviderBlob(v),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    /// One instance of every variant, shared with the message and event tests.
    pub(crate) fn all_blocks() -> Vec<ContentBlock> {
        vec![
            ContentBlock::Text("hello".into()),
            ContentBlock::ToolCall(ToolCall {
                id: "call_1".into(),
                name: "read_file".into(),
                args: json!({"path": "Cargo.toml"}),
            }),
            ContentBlock::ToolResult(ToolResult {
                id: "call_1".into(),
                content: "[workspace]".into(),
                is_error: false,
            }),
            ContentBlock::Image(Image {
                media_type: "image/png".into(),
                data: "iVBORw0KGgo=".into(),
            }),
            ContentBlock::ProviderBlob(ProviderBlob {
                provider: "anthropic".into(),
                data: json!({"type": "thinking", "signature": "abc"}),
            }),
        ]
    }

    #[test]
    fn every_variant_round_trips() {
        for block in all_blocks() {
            let json = serde_json::to_string(&block).unwrap();
            let back: ContentBlock = serde_json::from_str(&json).unwrap();
            assert_eq!(back, block, "round trip failed for {json}");
        }
    }

    #[test]
    fn wire_shape_is_type_tagged() {
        let text = serde_json::to_value(ContentBlock::Text("hi".into())).unwrap();
        assert_eq!(text, json!({"type": "text", "text": "hi"}));

        let call = serde_json::to_value(ContentBlock::ToolCall(ToolCall {
            id: "c".into(),
            name: "n".into(),
            args: json!({}),
        }))
        .unwrap();
        assert_eq!(
            call,
            json!({"type": "tool_call", "id": "c", "name": "n", "args": {}})
        );
    }
}
