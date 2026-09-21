# Phase 2 plan

Status: draft · Follows `docs/PRD.md` (the Event log phase), `docs/PLAN-phase0.md` and `docs/PLAN-phase1.md`

## 0. Goal and done-when

Phases 0 and 1 made the log the source of truth and proved it replays on
two backends. Phase 2 makes it survive: a thread that is killed mid-turn
resumes without hand repair, and a thread that runs for hundreds of turns
stays inside the model's window without losing what matters. Everything
here is an event; nothing is deleted or edited in place.

Done when:

1. **Kill mid-turn, resume cleanly.** `kill -9` the process while a bash
   tool is running, and separately while the model is streaming. On
   restart with `--thread`, the REPL repairs the log with events (never by
   rewriting), tells the user what happened, and continues the turn without
   a new user message. Works on both backends.
2. **A 200-turn thread stays under budget.** With a scripted provider and a
   small window, an integration test drives 200 turns and asserts every
   request's context stays below the configured fraction of the window, at
   least one summary compaction happened, and the latest turns are always
   verbatim in context.
3. **Compaction is auditable.** `compacted` events carry the range they
   replace, the strategy, and the summary text with the model and usage
   that produced it. `/cost` reports compactions and the tokens they cost.
4. **Cache survives compaction as well as it can.** After a compaction the
   next request's cache write is roughly the summary plus the retained
   tail; the request after that reads it all. Verified on Anthropic through
   `/cost`.

Not in phase 2: SQLite (see decision 1), the model-side `pin` tool (phase 3,
with the tool registry and policy), project-level compaction settings
(phase 4; phase 2 settings live on the profile), the turn queue.

## 1. Layout changes

```
crates/core/src/event.rs         EventKind gains Compacted, Pinned, Interrupted
crates/log/src/payload.rs        payloads for the three new kinds
crates/log/src/projection.rs     compaction-aware; returns prefix facts and body
crates/log/src/store.rs          torn-tail repair, open_turn detection
crates/runtime/src/compaction.rs the compaction seam, implemented
crates/runtime/src/resume.rs     interrupted-turn repair and continue_turn
crates/runtime/src/turn.rs       run_turn = append user message + drive; drive shared
crates/tui/src/repl.rs           /pin, /compact, resume notices
crates/tui/src/config.rs         [compaction] table on a profile
crates/tui/src/cost.rs           compaction lines
```

Dependency direction is unchanged.

## 2. Signatures (no bodies)

```rust
// crates/core/src/event.rs
pub enum EventKind {
    UserMessage,
    AssistantMessage,
    ToolResult,
    TurnEnded,
    Compacted,      // replaces a range in the projection; originals stay
    Pinned,         // a fact for the stable prefix; never summarised
    Interrupted,    // the process died mid-turn; appended on resume
}

// crates/log/src/payload.rs
pub enum CompactionStrategy {
    /// Tool results in the range are shortened to head and tail in the
    /// projection. Reclaims most of a full context and costs no model call.
    TruncateResults { max_bytes: usize },
    /// Events in the range are replaced by one summary message.
    Summary { text: String, model: String, usage: Usage },
}
pub struct CompactedPayload {
    pub from_seq: u64,               // inclusive
    pub to_seq: u64,                 // inclusive; always a turn boundary
    pub strategy: CompactionStrategy,
}
pub struct PinnedPayload { pub text: String }          // author is who pinned it
pub struct InterruptedPayload {
    pub reason: String,              // "process exited mid-turn"
    pub after_seq: u64,              // last event that was fully written
    pub unanswered_calls: Vec<String>, // tool call ids given synthetic results
}

// crates/log/src/projection.rs
pub struct Projection {
    /// Pinned facts, oldest first, for the stable prefix.
    pub pinned: Vec<String>,
    /// The thread body with compaction applied.
    pub body: Vec<Message>,
    /// Seq of the latest summary compaction, if any; the runtime uses it
    /// to decide what "the last N turns" means.
    pub compacted_through: Option<u64>,
}
pub fn project(events: &[Event]) -> Result<Projection, LogError>;

// crates/log/src/store.rs
pub enum Repair { Refuse, TruncateTornTail }
impl ThreadLog {
    /// `open` with `Repair::TruncateTornTail` cuts a torn last line at the
    /// byte offset the error reports and returns how many bytes it dropped.
    pub fn open_with(dir, thread_id, repair: Repair) -> Result<(Self, Option<u64>), LogError>;
    /// Events after the last `turn_ended`, if the log does not end on one.
    pub fn open_turn(events: &[Event]) -> Option<&[Event]>;
}

// crates/runtime/src/runtime.rs
pub struct CompactionSettings {
    pub trigger_fraction: f32,       // of Capabilities::max_context_tokens; default 0.7
    pub keep_turns: usize,           // verbatim tail; default 8
    pub max_result_bytes: usize,     // truncation target; default 4096
    pub summary_max_output_tokens: u64, // default 2048
}
impl Runtime {
    pub fn with_compaction(self, settings: CompactionSettings) -> Self;
}

// crates/runtime/src/resume.rs
pub enum Resumed {
    Clean,
    /// The log ended mid-turn. Repair events have been appended; the
    /// caller should `continue_turn`.
    Interrupted { after_seq: u64, unanswered_calls: usize, torn_bytes: Option<u64> },
}
impl Runtime {
    /// Called once after `open`. Appends synthetic error tool results for
    /// unanswered calls and an `interrupted` event; never edits.
    pub fn resume(&mut self, torn_bytes: Option<u64>, observe: &mut dyn FnMut(Signal<'_>)) -> Result<Resumed, RuntimeError>;
}

// crates/runtime/src/turn.rs
impl Runtime {
    pub async fn run_turn(&mut self, author, blocks, observe) -> Result<TurnOutcome, RuntimeError>; // unchanged
    /// The loop without a new user message: what `run_turn` does after the
    /// append, and what resume uses.
    pub async fn continue_turn(&mut self, observe) -> Result<TurnOutcome, RuntimeError>;
    /// Manual compaction (`/compact`). Returns what it did.
    pub async fn compact_now(&mut self, observe) -> Result<Vec<CompactionStrategy>, RuntimeError>;
    pub fn pin(&mut self, author: Author, text: String, observe) -> Result<Event, RuntimeError>;
}

// crates/runtime/src/seams.rs — the seam changes shape and gains a body
impl Runtime {
    /// Was `fn compact(&self, Vec<Message>) -> Vec<Message>`. Now runs
    /// before each model call, appends compaction events when the window
    /// is under pressure, and returns whether it did.
    pub async fn compact(&mut self, observe) -> Result<bool, RuntimeError>;
}
```

The changed phase 0 signature is the compaction seam: it must write events
and call the model, so it becomes async, takes `&mut self`, and no longer
takes or returns a context. `project` returns a `Projection` instead of a
`Vec<Message>`; the runtime is its only caller. Everything else is
additive.

## 3. Projection rules

`project` walks the events once and applies, in this order:

1. **Pinned events** are lifted out of the body into `pinned`, oldest
   first. They never appear in the body and are never in a summary range.
2. **Summary compactions** replace their range with one user-role message
   from `Author::System` whose text is the summary, prefixed by a fixed
   marker line so the model knows what it is. Every summary starts at seq
   0 and takes the previous summary's text as part of its input, so a
   later summary always nests over an earlier one and the projection holds
   exactly one summary message. (Chaining summaries from the previous
   `to_seq + 1` was tried first: each compaction then reclaimed one turn
   while the summaries themselves accumulated, and the 200-turn test
   crossed the line.)
3. **Truncation compactions** shorten every tool result in their range
   using the same head-and-tail rule the tools crate uses for display,
   with the payload's `max_bytes`.
4. **Provider blobs are dropped from every assistant message that predates
   the latest summary compaction.** Retained turns keep their text and tool
   calls. This is what makes keep-tail compaction legal on Anthropic, whose
   history check rejects replayed thinking blocks whose prefix changed.
   Text and tool calls are explicitly allowed to stay; thinking is not.
5. **Interrupted events** become a short user-role note from
   `Author::System`: the previous turn was interrupted after which tool
   calls, and their results are synthetic. The model decides whether to
   redo them.
6. Everything else projects as in phase 0.

The runtime's `build_context` order becomes: repository instructions,
then one system message holding the pinned facts (omitted when empty),
then the body. Both prefix messages are byte-stable between turns unless a
pin is added, which is a deliberate cache reset.

## 4. Compaction

Runs at the top of every loop iteration, before the budget check and the
model call, and on `/compact`.

**Measure.** The window fill is the previous call's reported prompt size
(`input + cache_read + cache_write` from its `Usage`), which is exact,
falling back to `count_tokens` over the projected context when no usage was
reported or the log has grown since (a tool result was appended). Compare
against `trigger_fraction × max_context_tokens`.

**Rule 1, truncate.** If over the line and any tool result in the
compactable range is longer than `max_result_bytes`, append one
`compacted{TruncateResults}` covering seq 0 through the end of the last
turn outside the keep window. Re-measure with `count_tokens`. Most full
contexts end here.

**Rule 2, summarise.** If still over the line, pick the range: from seq 0
through the `turn_ended` that leaves exactly `keep_turns` complete turns
after it. If fewer than `keep_turns + 1` turns exist, or that boundary has
not moved since the last summary, do nothing and let the budget stop the
turn; summarising the turn in progress would break the tool-call contract,
and summarising the same range twice would only spend tokens. Call
the same provider with the fixed summary prompt (section 5), no tools,
`summary_max_output_tokens`. Append `compacted{Summary}` with the text,
model and usage. The summary's own usage counts toward `/cost` under its
own line.

**Rule 3, pins.** Pinned events are outside the body, so no rule touches
them.

**Rule 4, same model.** The summariser is `self.provider`. A provider error
during summarisation ends the turn with `turn_ended{provider_error}` like
any other, and no compaction event is written.

**Never mid-turn.** Compaction runs only at an iteration boundary, after all
tool results for the previous assistant message are in the log, so no
summary range ever splits a call from its result.

**Cache.** A compaction rewrites the prefix once. The next request writes
the summary plus the tail; the one after reads it. That is the expected
cost of staying inside the window, and `/cost` makes it visible.

## 5. The summary prompt

Fixed text, versioned in code, sent as the system message of the
summarisation call with the range's projected messages as the body and one
final user message asking for the summary:

> Summarise the conversation so far for an agent that will continue it
> without seeing the original. Keep: the user's goals and constraints, every
> decision and its reason, file paths touched and what changed in each,
> commands run and their outcomes, open questions and anything the user
> asked to remember. Drop: greetings, restated tool output, reasoning that
> led nowhere. Write in the past tense, as facts, under 600 words.

The marker line placed before the summary in the projection:

> [Summary of turns N to M, written by <model> on <date>; the originals are
> in the log]

## 6. Resume

`ThreadLog::open_with(.., Repair::TruncateTornTail)` handles a crash during
an append: the torn bytes are by definition an unfinished write, so cutting
them loses nothing that was ever acknowledged. The REPL prints what it cut.
`Repair::Refuse` keeps today's behaviour for tools that must not touch the
file.

`Runtime::resume` then looks at the open turn, if any:

| Log ends with | Repair |
| --- | --- |
| `turn_ended` | nothing; `Resumed::Clean` |
| `user_message` | the model call was lost; `interrupted`; continue |
| `assistant_message` with tool calls, some or all without results | synthetic `tool_result` per missing id with `is_error: true` and content "interrupted before a result was recorded; rerun if needed"; then `interrupted`; continue |
| `assistant_message` without tool calls | the `turn_ended` was lost; append it with reason `resumed`; `Resumed::Clean` |
| `tool_result` | the next model call was lost; `interrupted`; continue |

The REPL, on `Resumed::Interrupted`, prints one line and calls
`continue_turn` immediately, so the user sees the turn finish rather than
a prompt. A bash command that was running when the process died is gone
with its process group (phase 0's teardown), so its synthetic result is
honest: nothing is known about whether it completed.

## 7. Configuration and REPL

Per profile, all optional:

```toml
[profiles.tensorx.compaction]
trigger_fraction = 0.7
keep_turns = 8
max_result_bytes = 4096
summary_max_output_tokens = 2048
```

REPL additions: `/pin <text>` appends a `pinned` event with the user as
author; `/compact` runs the rules regardless of pressure and reports what
happened; `/cost` gains

```
compactions  2 (1 truncate, 1 summary)   summary tokens in 12345 out 512
```

On resume the banner adds `repaired torn tail (N bytes)` and
`interrupted after seq N; continuing` when applicable.

## 8. Tests

- **Projection** (log crate): pins lift to the prefix; a summary replaces
  its range and nests over an earlier one; truncation shortens only results
  in range; blobs vanish before the latest summary and survive after it;
  an interrupted event becomes a note. Table-driven over hand-built event
  lists.
- **Torn tail repair**: the phase 0 truncated-line test gains a sibling that
  opens with `TruncateTornTail`, asserts the byte count, and appends
  successfully afterwards.
- **Resume table** (runtime): one integration test per row of section 6,
  each building the log by hand, calling `resume`, and asserting the exact
  events appended and the `Resumed` value. A `continue_turn` after each.
- **200 turns** (runtime): scripted provider whose responses grow, window of
  8,000 tokens, `trigger_fraction 0.7`, `keep_turns 4`. Assert every
  request's `count_tokens` is under the line, at least one summary event
  exists, the last four turns are verbatim in the final request, no request
  ever contains a tool call without its result, and no summary range splits
  a turn.
- **Compaction on both adapters** (providers): a projected context with a
  summary message and blob-free retained turns serialises on both without
  error, and the Anthropic breakpoints land on the summary's user message
  when it is last.
- **Cost**: compaction lines from a log with two compactions.
- Everything from phases 0 and 1 unchanged apart from the two signatures.

No test touches the network.

## 9. Steps, one commit each

1. `core: compacted, pinned and interrupted event kinds` with their payloads
   in the log crate.
2. `log: compaction-aware projection and torn-tail repair`.
3. `runtime: resume and continue_turn` with the resume table tests.
4. `runtime: compaction` with the 200-turn test; the seam gets its body.
5. `tui: /pin, /compact, resume notices, compaction settings`.
6. Acceptance: the two kill scenarios on TensorX and Anthropic, then a long
   session with `trigger_fraction = 0.05` to force compaction on a real
   thread and check `/cost` and the cache signature. Record the result in
   the READMEs.

Each step passes `cargo fmt`, `cargo clippy --all-targets -- -D warnings`
and `cargo test` before its commit.

## 10. Decisions

1. **No SQLite in phase 2.** The PRD's phase row says "SQLite log", but its
   storage section and the phase 0 plan make JSONL the truth and SQLite a
   derived index for cross-thread search. Nothing in this phase needs
   cross-thread search. The index arrives with the first feature that does
   (project memory extraction in phase 4, or the orchestrator's status
   reads in phase 6), and it is rebuilt from the JSONL, never written to
   directly.
2. **Keep-tail compaction, with blobs stripped from the tail.** This
   refines phase 1 decision 4, which said whole-body only. The Anthropic
   history check permits retained turns to keep text and tool calls if
   their thinking blocks are removed; the projection removes every provider
   blob before the latest summary. The PRD's "keep the last N turns raw"
   therefore holds for everything except provider-internal state.
3. **Compaction is a projection, not a rewrite.** Originals stay; the
   projection applies `compacted` events. A bug in compaction is fixed by
   appending a better compaction, and old logs replay under new rules.
4. **Truncation is a compaction event too**, not a projection-time default,
   so it happens once at a known seq and the prefix stays stable until
   then. The tools' own 32 KiB cap still applies at capture time.
5. **Pins go in the prefix, not the body.** That is where the PRD's context
   order puts pinned facts, it keeps them out of every summary range for
   free, and it means a pin costs one cache reset rather than a permanent
   growth in the body.
6. **Resume continues the turn automatically.** "Resume cleanly" means the
   user gets their answer, not a repaired log and a prompt. The repair is
   always visible as events and a printed line.
7. **The summary is a user-role message from the system author**, not a
   system message, because Anthropic requires the body to start with a
   user turn and OpenAI-compatible servers treat mid-body system messages
   inconsistently.
8. **Window fill is measured from reported usage first**, estimated second.
   The estimate is a fallback, never the primary signal, so the trigger is
   as accurate as the backend's own accounting.

## 11. Open items

- Whether `/compact` should also allow a user-supplied focus ("keep
  everything about the migration") appended to the fixed prompt. Cheap to
  add; deferred until someone wants it.
- The summary prompt is English. Swedish threads will get Swedish
  summaries anyway if the model follows the conversation's language; verify
  during acceptance.
- Server-side compaction exists on the Anthropic API. Not used: the PRD
  wants compaction as auditable events in our log, on every backend.
