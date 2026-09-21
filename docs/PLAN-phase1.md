# Phase 1 plan

Status: draft · Follows `docs/PRD.md` (the Two providers phase) and `docs/PLAN-phase0.md`

## 0. Goal and done-when

Phase 0 proved the loop against one backend. Phase 1 adds the second,
deliberately different one, so the `Provider` trait, the canonical
`ContentBlock` and the `ProviderBlob` replay mechanism are tested against a
wire format that disagrees with OpenAI's on every point the PRD flags:
tool calls and results as content blocks, explicit cache breakpoints, and
thinking blocks that must be replayed verbatim with a signature.

Done when:

1. The same thread log runs on TensorX and on Anthropic by changing only the
   config file. Both backends read, edit and run tests in a real repo through
   the same REPL.
2. On Anthropic, the second turn of a thread reports
   `cache_read_input_tokens > 0` in `/cost`, proving the stable-prefix
   contract holds end to end.
3. A thread started on one backend continues on the other. Each adapter
   replays its own blobs and drops the other's; nothing crashes and nothing
   is silently mutated.
4. Fixtures for both adapters are recordings, not hand-written.

Not in phase 1: structured output, images from the REPL, server-side tools,
compaction (phase 2), permissions (phase 3), the `ratatui` client.

## 1. Layout changes

```
crates/core/src/provider.rs      Usage struct gains cache and reasoning fields
crates/log/src/payload.rs        Usage payload mirrors it
crates/providers/src/
  sse.rs                         SseParser moves up: shared by both adapters
  openai_compat/                 unchanged except the Usage fields
  anthropic/
    mod.rs                       AnthropicConfig, Anthropic, Provider impl
    wire.rs                      canonical <-> Messages API translation
    stream.rs                    Messages API SSE events -> ProviderEvent
crates/providers/fixtures/
  openai_compat/*.sse            phase 0 recordings, moved into a subdir
  anthropic/*.sse                recordings, see section 7
  record.sh                      gains an `anthropic` mode
crates/tui/src/config.rs         named profiles, `--profile`
```

Dependency direction is unchanged. `core` gains no dependencies.

## 2. Signatures (no bodies)

```rust
// crates/core/src/provider.rs
/// Token usage for one model call. Cache fields are zero on backends
/// without caching; reasoning is `None` when the backend does not split it.
pub struct Usage {
    pub input_tokens: u64,          // uncached remainder only, as the APIs report it
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: Option<u64>,
}

pub enum ProviderEvent {
    TextDelta(String),
    ToolCall(ToolCall),
    Blob(ProviderBlob),
    Usage(Usage),                   // was Usage { input_tokens, output_tokens }
    Done { finish_reason: String },
    Error(ProviderError),
}

// crates/log/src/payload.rs
pub struct Usage {                  // the persisted form; same fields plus
    /* fields of core::Usage */
    pub estimated: bool,
}

// crates/providers/src/anthropic/mod.rs
pub const PROVIDER_NAME: &str = "anthropic";

pub enum Thinking {
    Adaptive,                       // default: `{"type":"adaptive"}`
    Off,                            // omit the field; accepted at effort high or below
}

pub struct AnthropicConfig {
    pub base_url: String,           // default "https://api.anthropic.com"
    pub api_key: String,            // sent as `x-api-key`
    pub model: String,              // e.g. "claude-opus-5"
    pub max_context_tokens: u64,    // advertised via Capabilities; 1_000_000 for Opus 5
    pub max_output_tokens: u64,     // `max_tokens` is required by the API; default 64_000 when
                                    // the request carries no max_output_tokens
    pub thinking: Thinking,
    pub effort: Option<String>,     // "low" | "medium" | "high" | "xhigh" | "max"
    pub cache: bool,                // emit cache_control breakpoints (default true)
}

pub struct Anthropic { /* reqwest::Client, AnthropicConfig */ }

impl Provider for Anthropic {
    fn complete(&self, request: &CompletionRequest<'_>) -> Pin<Box<dyn Stream<Item = ProviderEvent> + Send + '_>>;
    fn count_tokens(&self, context: &[Message]) -> u64;   // same heuristic as openai_compat; the
                                                          // trait is sync, so no network call
    fn capabilities(&self) -> Capabilities;               // tools, images, caching: true;
                                                          // structured_output: false in phase 1
}

// crates/providers/src/anthropic/wire.rs
pub struct MessagesRequest { model, max_tokens, stream: true, system: Vec<SystemBlock>,
                             messages: Vec<WireMessage>, tools: Vec<WireTool>,
                             thinking: Option<..>, output_config: Option<..> }
pub fn to_wire(request: &CompletionRequest<'_>, config: &AnthropicConfig) -> MessagesRequest;
// one way only; there is no from_wire for assistant content because the
// stream translator builds canonical blocks directly.

// crates/providers/src/anthropic/stream.rs
pub struct Translator { /* per-index block accumulators */ }
impl Translator {
    pub fn on_event(&mut self, event_type: &str, data: &str) -> Vec<ProviderEvent>;
    pub fn finish(&mut self) -> Vec<ProviderEvent>;
}
pub fn parse_stream<S, E>(bytes: S) -> impl Stream<Item = ProviderEvent> + Send;

// crates/providers/src/sse.rs
pub struct SseParser;               // as today, but `feed` yields (event_type, data) pairs;
                                    // openai_compat ignores the type.
```

The only change to a phase 0 signature is `ProviderEvent::Usage`, which
becomes a struct variant carrying cache and reasoning counts. Everything
else is additive.

## 3. Wire mapping: canonical to Messages API

Endpoint `POST {base_url}/v1/messages`. Headers `x-api-key`,
`anthropic-version: 2023-06-01`, `content-type: application/json`. No beta
headers in phase 1.

| Canonical | Wire | Note |
| --- | --- | --- |
| Leading `Role::System` messages | top-level `system: [{type:"text", text}]` | Only the leading run; a system message after the thread body starts is a phase 5 concern and is dropped with a warning in phase 1 |
| `Role::User` text | `{role:"user", content:[{type:"text", text}]}` | Always the array form, never the string form, so breakpoints have a block to sit on |
| `Image` | `{type:"image", source:{type:"base64", media_type, data}}` | |
| `Role::Assistant` `Text` | `{type:"text", text}` | |
| `ToolCall{id,name,args}` | `{type:"tool_use", id, name, input: args}` | `input` is a JSON object, not a string; ids preserved verbatim |
| `ProviderBlob{provider:"anthropic"}` | the block verbatim | Thinking blocks: `{type:"thinking", thinking, signature}`. Replayed unchanged, in their original position, empty `thinking` text included |
| `ProviderBlob{provider: other}` | dropped | |
| `Role::Tool` `ToolResult` | `{type:"tool_result", tool_use_id, content, is_error}` inside a **user** message | `is_error` is native here, so no `[error] ` prefix. Still one way, log to wire |
| Author | not represented | No `name` field on this API. Attribution in the prefix is phase 5 |

Two structural rules the adapter enforces because the API does:

- **Roles must alternate.** Consecutive canonical messages with the same
  wire role are merged into one wire message. All `Role::Tool` messages that
  answer one assistant message merge into a single user message holding
  every `tool_result` block, in call order. Splitting them across messages
  teaches the model to stop calling tools in parallel.
- **Every `tool_use` needs a `tool_result`.** The runtime already guarantees
  this (the budget check runs only before a model call). The adapter does
  not repair a violating context; it sends it and lets the API's 400 surface
  as `ProviderError::Http`.

Tools: `{name, description, input_schema}` from `ToolSpec`, in the order the
runtime passes them. The runtime sorts tool specs by name before building
the request so the list is deterministic (see section 4).

## 4. The caching contract

Prompt caching is a prefix match over the rendered bytes in the order
`tools`, `system`, `messages`. Anything that changes earlier in the prefix
invalidates everything after it. The contract splits responsibility:

**The runtime guarantees**

1. The instructions system message is byte-identical between turns of a
   thread (it is read once at startup, not per turn).
2. Tool specs are sorted by name, and their JSON is serialised
   deterministically (schemars output is stable for a given struct).
3. The thread body is append-only. Compaction (phase 2) replaces the whole
   body with one summary rather than rewriting the middle, which is also
   what Anthropic's preserved-thinking check requires.

**The adapter decides where to mark**

- Breakpoint 1 on the last `system` block. Because tools render before
  system, this caches tools and instructions together.
- Breakpoint 2 on the last content block of the last user message (the
  newest turn, or the newest tool results). Each request then reads the
  whole prior conversation and writes only the delta.
- Two of the allowed four breakpoints; the other two stay free for project
  knowledge and pinned facts in phase 4.
- Default TTL (5 minutes). A REPL turn starts well within five minutes of
  the previous one, so the 1-hour TTL would only double the write cost.
- Minimum cacheable prefix on Opus 5 is 512 tokens. Below that the API
  silently writes nothing, which is why done-when item 2 checks the second
  turn of a real thread and not a toy prompt.

**Verification** is the `usage` object, nothing else. `Usage` carries
`cache_read_tokens` and `cache_write_tokens`; `/cost` shows them on their
own line. The healthy signature on turn N is reads roughly equal to the whole
prior prefix and writes roughly equal to the last assistant output plus the
new input. Reads stuck at zero across turns mean a silent invalidator, and
the fix is to diff two consecutive request bodies with the breakpoints
stripped.

## 5. Streaming translation

The Messages API stream is typed SSE: `event: <type>` then `data: <json>`,
with `data.type` repeating the event type. The shared `SseParser` yields
both; the translator keys on `data.type`.

| Event | Translator action |
| --- | --- |
| `message_start` | Remember `usage.input_tokens`, `cache_creation_input_tokens`, `cache_read_input_tokens` |
| `content_block_start` `text` | Open a text block at `index` |
| `content_block_start` `tool_use` | Open a tool call at `index` with `id` and `name`; `input` arrives as deltas |
| `content_block_start` `thinking` | Open a thinking block at `index` |
| `content_block_delta` `text_delta` | Emit `TextDelta` immediately |
| `content_block_delta` `input_json_delta` | Append `partial_json` to the open tool call |
| `content_block_delta` `thinking_delta` / `signature_delta` | Append to the open thinking block |
| `content_block_stop` | Tool call: parse accumulated JSON (empty means `{}`), emit `ToolCall`. Thinking: emit `Blob{provider:"anthropic", data: the whole block}`. Text: nothing, already streamed |
| `message_delta` | Remember `stop_reason` and `usage.output_tokens`; emit `Usage` combining both halves |
| `message_stop` | Emit `Done{finish_reason: stop_reason}` |
| `ping` | Ignore |
| `error` | Emit `Error(Protocol)` and close |

Stop reasons pass through as strings: `end_turn`, `tool_use`, `max_tokens`,
`refusal`, `pause_turn`. The runtime treats any of them the same way it
treats `stop` today: no tool calls means the turn is over. A `refusal` with
a partial tool call in flight must not run that tool; the translator drops
an unfinished tool call on `refusal` and the runtime sees only text.

Non-2xx responses and transport failures become one `Error` event, as in
phase 0.

## 6. Configuration

`config.toml` grows named profiles so a comparison run is one flag:

```toml
default_profile = "tensorx"

[profiles.tensorx]
provider = "openai_compat"
base_url = "https://api.tensorx.ai/v1"
model = "z-ai/glm-5.3"
api_key_env = "TENSORX_API_KEY"

[profiles.anthropic]
provider = "anthropic"
model = "claude-opus-5"
api_key_env = "ANTHROPIC_API_KEY"
# base_url = "https://api.anthropic.com"
# thinking = "adaptive"     # or "off"
# effort = "high"
# max_context_tokens = 1000000
```

`aigentic --profile anthropic --thread <id>` resumes a thread on the other
backend. The phase 0 flat layout keeps working: a file with top-level
`base_url`/`model`/`api_key_env` is read as a single `openai_compat`
profile named `default`. Keys stay in environment variables named by
`api_key_env`; unknown fields are still rejected.

`/cost` output gains a line:

```
cache      read     123456  write      4321   (anthropic only; zero elsewhere)
```

## 7. Tests and fixtures

- **Stream translation** on recorded fixtures under `fixtures/anthropic/`:
  a text reply with thinking, a reply with two parallel `tool_use` blocks,
  and a `max_tokens` stop. `record.sh anthropic <model>` captures them with
  the same three prompts as the OpenAI recordings. Until an Anthropic key
  exists, the fixtures are hand-written to the documented event shapes and
  the test module says so in a comment; replacing them is the first act of
  the acceptance run and must not change any test.
- **Chunk-boundary agnosticism** over all Anthropic fixtures, as for
  `openai_compat`.
- **Wire shape**: a canonical context with system, user text plus image,
  assistant text plus two tool calls plus an `anthropic` thinking blob plus
  an `openai_compat` reasoning blob, then two tool results, one with
  `is_error`. Assert the JSON: system array, alternation, both tool results
  in one user message, native `is_error`, own blob replayed in position,
  foreign blob absent.
- **Breakpoints**: exactly two `cache_control` markers, on the last system
  block and the last block of the last user message; none when
  `cache: false`.
- **Cross-backend replay**: the same canonical context through both
  adapters' `to_wire`; each drops the other's blob. Lives in
  `crates/providers/tests/`.
- **Usage**: `ProviderEvent::Usage` from a recording carries non-zero
  `cache_read_tokens` on the second-turn fixture (record the second turn of
  a real thread, not a one-shot).
- **Config**: flat phase 0 file still parses; profile selection; a profile
  with `provider = "anthropic"` and a `base_url` override.
- **Runtime**: the existing integration tests pass unchanged apart from the
  `Usage` shape.

No test touches the network.

## 8. Steps, one commit each

1. `core: usage with cache and reasoning counts` — the `Usage` struct,
   `ProviderEvent::Usage(Usage)`, log payload, runtime and `/cost` updated.
   `openai_compat` fills `reasoning_tokens` from
   `completion_tokens_details.reasoning_tokens`, which the GLM recordings
   already contain.
2. `providers: shared sse parser` — move `SseParser` up one level and yield
   event types; no behaviour change for `openai_compat`.
3. `providers: anthropic adapter` — wire, stream, config, fixtures
   (hand-written, marked), tests, README section with the manual check.
4. `tui: config profiles` — `--profile`, backward-compatible flat config,
   `/cost` cache line.
5. Acceptance run with a real key: record fixtures, commit them as
   `providers: recorded anthropic fixtures`, then run the four done-when
   items and record the result in both READMEs.

Each step passes `cargo fmt`, `cargo clippy --all-targets -- -D warnings`
and `cargo test` before its commit, as in phase 0.

## 9. Decisions

1. **Raw HTTP, no SDK**, as for `openai_compat`. There is no official Rust
   SDK, and the PRD wants adapters written against the wire.
2. **Adaptive thinking on by default**, `display` left at the API default
   (omitted). Thinking blocks come back with empty text but a signature, and
   the blob stores the whole block so replay is exact. `budget_tokens` is
   not supported; it is rejected by current models.
3. **Thinking blobs are replayed unchanged, including across models.** The
   API drops what the target model cannot read, unbilled. The adapter never
   strips them, because removing blocks from the middle of a history can
   trigger signature errors, and never edits them.
4. **Append-only is now an API requirement, not only a design principle.**
   Anthropic's preserved-thinking check rejects a history whose earlier
   turns changed. This binds phase 2: compaction must be simple compaction
   (one summary replaces the whole body), never keep-tail with thinking
   blocks retained. Recorded here so phase 2 does not rediscover it.
5. **Two breakpoints, 5-minute TTL**, per section 4. Revisit when project
   knowledge lands in phase 4.
6. **`max_tokens` defaults to 64,000** when the request carries no
   `max_output_tokens`. Streaming means a large cap costs nothing unless
   used; a small cap truncates tool inputs mid-JSON.
7. **Tool results stay one way.** `is_error` is native on this API, but the
   projection still never reads tool results back from the wire; the log is
   the truth.
8. **Model id is configuration**, default `claude-opus-5` in the example
   profile. Ids are used exactly as published, never with date suffixes.
   Refusal fallbacks, effort tuning and fast mode are out of scope; a
   `refusal` stop reason simply ends the turn.
9. **The `count_tokens` endpoint is not used.** The trait method is
   synchronous and the heuristic is enough for pre-call sizing; real counts
   come from `Usage`, now including cache splits.

## 10. Open items

- Whether to fold `openai_compat`'s `reasoning_content` blob and Anthropic's
  thinking blob into one canonical `Thinking` block. Decided against for
  phase 1: the blob was designed for exactly this, and one more backend is
  not enough evidence. Reconsider in phase 2 if `/cost` or compaction need
  to see inside them.
- Author attribution on Anthropic, where there is no `name` field. Phase 5
  will render names into the text of the prefix; until then Anthropic
  threads are single-author in practice.
- Images from the REPL. The adapter supports them; the client has no way to
  attach one. Not needed for the done-when.
