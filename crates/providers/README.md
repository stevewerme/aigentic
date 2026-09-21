# aigentic-providers

Provider adapters for the Aigentic harness, written against raw HTTP with
`reqwest` and `serde`; no vendor SDK.

- `OpenAiCompat` speaks the OpenAI chat completions API with streaming SSE
  and tool calls. That covers vLLM, llama.cpp, Mistral and most EU hosts.
- `Anthropic` speaks the Messages API: tool calls and results as content
  blocks, explicit cache breakpoints, thinking blocks replayed with their
  signature. It is the second, deliberately different backend that keeps
  the `Provider` trait honest.

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

Tests run the parser and translator over recorded streams in
`fixtures/openai_compat/`:
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

## Anthropic adapter

```rust
use aigentic_providers::{Anthropic, AnthropicConfig, Thinking};

let provider = Anthropic::new(
    AnthropicConfig::new(api_key, "claude-opus-5")
        .with_effort("high")             // optional: low | medium | high | xhigh | max
        .with_thinking(Thinking::Adaptive) // default; Off omits the field
        .with_cache(true),               // default; emits two cache_control breakpoints
);
```

| Concern | Handling |
| --- | --- |
| Tool calls | `tool_use` blocks on the assistant message; `input` is a JSON object; ids preserved |
| Tool results | `tool_result` blocks inside a **user** message, all results for one assistant message in one user message. `is_error` is native. One way only, log to wire |
| Roles | Must alternate; same-role neighbours are merged. Only the leading run of system messages becomes the top-level `system` array |
| Thinking | Adaptive by default. Each thinking block becomes a `ProviderBlob` (provider `anthropic`) holding the whole block, replayed verbatim in position. Other adapters' blobs are dropped, and this adapter's blobs are dropped by them |
| Caching | Breakpoint on the last system block (covers tools) and on the last block of the last user message. `Usage` carries `cache_read_tokens` and `cache_write_tokens`; the minimum cacheable prefix on Opus 5 is 512 tokens, so a toy prompt never caches |
| Stop reasons | `end_turn`, `tool_use`, `max_tokens`, `refusal`, `pause_turn` pass through as the finish reason. On `refusal` an unfinished tool call is dropped, never run |
| Overload | HTTP 429/503/529, or a stream whose first event is `overloaded_error`, is retried three times (1s, 3s, 8s) before any content has streamed. Anything after content starts is not retried; the runtime records the error |
| Append-only | The API checks that earlier turns are unchanged when thinking blocks are replayed. The log is append-only, so this holds; phase 2 compaction must replace the whole body, never rewrite the middle |

Fixtures under `fixtures/anthropic/` are recorded from `claude-opus-5`.
Re-record with a key:

```bash
cd crates/providers/fixtures && ANTHROPIC_API_KEY=... ./record.sh anthropic claude-opus-5
```

The recording sends a system prompt above the cache minimum and sends the
tool-call request twice, keeping the second, so `tool_calls.sse` shows
`cache_read_input_tokens > 0`.

### Manual check against the Messages API

1. Record the fixtures as above and run `cargo test -p aigentic-providers`.
2. Point the REPL at an `anthropic` profile (see `crates/tui/README.md`) and
   run the phase 0 acceptance steps: read, edit, `cargo test`, `/cost`,
   `/quit`, resume.
3. On the second turn `/cost` must show `cache read > 0`.
4. Resume a thread recorded on TensorX with `--profile anthropic` and ask
   what happened earlier; then the reverse.

Set `AIGENTIC_DUMP_REQUESTS=<dir>` to write every request body there as
JSON (never the key); diffing two consecutive bodies is how a silent cache
invalidator is found.

Status: all four steps passed on 2026-09-21 with `claude-opus-5`. Fixtures
were recorded; the parser needed no change. A thread produced entirely on
TensorX resumed on Anthropic and answered from context with no tool call;
the next turn showed `cache read 10381, write 5642`; resuming the same
thread on TensorX afterwards, with Anthropic thinking blobs now in the log,
produced a correct five-point summary from memory. Phase 1 done-when met.
