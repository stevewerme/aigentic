# aigentic-providers

Provider adapters for the Aigentic harness. One adapter so far:
`OpenAiCompat`, which speaks the OpenAI chat completions API with streaming
SSE and tool calls. That covers vLLM, llama.cpp, Mistral and most EU hosts.
Written against raw HTTP with `reqwest` and `serde`; no vendor SDK.

## Configuration

```rust
use aigentic_core::{CompletionRequest, Provider, ToolSpec};
use aigentic_providers::{OpenAiCompat, OpenAiCompatConfig};

let provider = OpenAiCompat::new(
    OpenAiCompatConfig::new("http://127.0.0.1:8080/v1", "qwen2.5-coder")
        .with_api_key("sk-...")            // optional
        .with_max_context_tokens(32_768)   // advertised via Capabilities
        .with_images(false),
);

// Tools and the output cap are per call; the runtime builds the list from
// its registry with `ToolSpec::from(&*tool)`.
let tools: Vec<ToolSpec> = registry.iter().map(|t| ToolSpec::from(t.as_ref())).collect();
let stream = provider.complete(&CompletionRequest {
    messages: &context,
    tools: &tools,
    max_output_tokens: Some(4096),
});
```

## What leaks through the abstraction, and where it goes

| Concern | Handling |
| --- | --- |
| Tool call shape | Separate `tool_calls` field on the assistant message; arguments are a JSON string on the wire and a `serde_json::Value` in canonical form. Ids are preserved verbatim; a server that omits them gets `call_<index>`. |
| Tool result placement | One `tool` role message per `ToolResult` block, linked by `tool_call_id`. Translated one way only, log to wire. `is_error` has no wire representation; a failed result is sent with its content prefixed by `[error] `. |
| Images | `image_url` parts with `data:<media_type>;base64,<data>` URLs. |
| Thinking / reasoning | `reasoning_content` deltas are collected into one `ProviderBlob` (provider `openai_compat`) emitted before `Done`. On replay, blobs with that name are flattened into the assistant message; other adapters' blobs are dropped. |
| Attribution | Author ids ride on the optional `name` field. |
| Token counting | `count_tokens` is an estimate for pre-call sizing only (about four bytes per token). The runtime records real usage from `ProviderEvent::Usage`. |
| Usage | `stream_options: { include_usage: true }` is sent so usage arrives in the final chunk. Usage is emitted whenever the server sends it (a trailing chunk on OpenAI and vLLM, on the finish chunk on llama.cpp); a server that sends none produces no `Usage` event and the stream still completes. |

## Stream handling

`SseParser` is an incremental SSE decoder (chunk boundaries anywhere, CRLF,
comments, multi-line data). `Translator` turns each chunk into
`ProviderEvent`s: text is forwarded as it arrives, tool calls are assembled
by index and emitted whole when the choice finishes, `Done` carries the
finish reason and is emitted on `[DONE]` or EOF. A non-2xx response, a
transport failure or an `error` object in the stream becomes a single
`ProviderEvent::Error` and closes the stream.

Tests run the parser and translator over recorded streams in `fixtures/`:
a text reply, a two-tool-call reply and a `length` stop, all captured raw
from TensorX (`z-ai/glm-5.3`) by `fixtures/record.sh`. The recordings show
what the documented format leaves out: reasoning streams as
`reasoning_content` deltas before any text, the first chunk omits
`finish_reason` rather than sending `null`, and a short `max_tokens` can be
spent entirely on reasoning with no visible text. Re-record with:

```bash
cd crates/providers/fixtures && ./record.sh https://api.tensorx.ai/v1 z-ai/glm-5.3
```

The key is read from `TENSORX_API_KEY` (or the variable named by
`AIGENTIC_API_KEY_ENV`) and never printed. Wire translation is checked by
round-trip tests. No test touches the network.

## Manual check against llama.cpp

Not automated; run it when touching the adapter or upgrading `reqwest`.

1. Start a server with a tool-capable model and the Jinja chat template
   (needed for tool calls):

   ```bash
   llama-server -hf Qwen/Qwen2.5-Coder-7B-Instruct-GGUF --port 8080 --jinja
   ```

2. Confirm it answers:

   ```bash
   curl -s http://127.0.0.1:8080/v1/models
   ```

3. Stream a plain completion. Expect text deltas, then `Usage`, then
   `Done { finish_reason: "stop" }`:

   ```bash
   cargo run -p aigentic-providers --example stream -- "Say hello in five words."
   ```

4. Trigger a tool call. Expect a `ToolCall` with a non-empty id and parsed
   JSON args, then `Done { finish_reason: "tool_calls" }`:

   ```bash
   cargo run -p aigentic-providers --example stream -- "What time is it in Stockholm? Use the tool."
   ```

5. Point at a hosted OpenAI-compatible provider with
   `AIGENTIC_BASE_URL`, `AIGENTIC_MODEL` and `AIGENTIC_API_KEY` and repeat
   steps 3 and 4.

Status: step 5 (a hosted OpenAI-compatible provider) passed on 2026-09-21
against TensorX with `z-ai/glm-5.3`: streamed text, multi-step tool calls
and usage all arrived as expected, via the `aigentic` REPL's acceptance run
(see `crates/tui/README.md`). Steps 1 to 4 against a local llama.cpp have
not yet been run; no local install exists on this machine.
