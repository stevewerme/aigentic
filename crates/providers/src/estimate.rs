use aigentic_core::{ContentBlock, Message};

/// An estimate for pre-call sizing only: about four bytes per token plus a
/// few per message. Tokenizers differ per model, so this is never used for
/// accounting; the runtime records real usage from `ProviderEvent::Usage`.
pub fn estimate_tokens(context: &[Message]) -> u64 {
    let bytes: usize = context
        .iter()
        .map(|m| {
            4 + m
                .blocks
                .iter()
                .map(|b| match b {
                    ContentBlock::Text(t) => t.len(),
                    ContentBlock::ToolCall(c) => c.name.len() + c.args.to_string().len(),
                    ContentBlock::ToolResult(r) => r.content.len(),
                    ContentBlock::Image(i) => i.data.len() / 4,
                    ContentBlock::ProviderBlob(b) => b.data.to_string().len(),
                })
                .sum::<usize>()
        })
        .sum();
    (bytes as u64).div_ceil(4)
}
