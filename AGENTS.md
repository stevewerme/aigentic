# Aigentic: notes for agents and contributors

Aigentic is an open source agent harness in Rust. `docs/PRD.md` and the
`docs/PLAN-*.md` files are binding; read them before changing anything.

## Crate layout

One Cargo workspace, edition 2024, MIT licence. Directories under `crates/`
use the short names from the plan; package names carry an `aigentic-` prefix
because bare `core` and `log` collide with the std `core` crate and the `log`
crate.

| Directory | Package | Owns |
| --- | --- | --- |
| `crates/core` | `aigentic-core` | Canonical message, event and tool types; `Provider` and `Tool` traits |
| `crates/log` | `aigentic-log` | JSONL event store, projections, resume |
| `crates/providers` | `aigentic-providers` | One adapter per backend (OpenAI-compatible first, then Anthropic) |
| `crates/tools` | `aigentic-tools` | Built-in tools; MCP client in phase 3 |
| `crates/runtime` | `aigentic-runtime` | Agent loop, scheduler, context builder, compaction |
| `crates/tui` | `aigentic-tui` (binary `aigentic`) | Terminal client: streaming REPL first, ratatui later |
| `crates/skills` | `aigentic-skills` | Skill discovery and loading (phase 3, empty until then) |
| `crates/policy` | `aigentic-policy` | Permission rules and prompts (phase 3, empty until then) |

## The dependency rule

`core` depends on no workspace crate and no async runtime (no `tokio`). It may
use only `serde`, `serde_json`, `schemars`, `ulid`, `futures-core`, `time` and
`thiserror`. Every other crate depends on `core`. `runtime` is the only crate
that knows about `log`, `providers`, `tools`, `skills` and `policy`. `tui`
depends on the runtime API only. Never add an edge that breaks this: the
model is a function behind the `Provider` trait, and `core` must never import
a provider SDK.

`schemars` is pinned to 0.8 because `Tool::schema` returns
`schemars::schema::RootSchema`, which schemars 1.x removed.

## The event log is the source of truth

A thread is an append-only, ordered sequence of `Event`s, stored as JSONL
(one file per thread, one event per line). Model context, the terminal view,
cost reports and resume are all projections of that log. Nothing is mutated in
place; compaction appends a summary event and keeps the originals. Event kinds
are added, never changed, so old logs always replay. If you need new state,
add an event kind, not a side table.

## Commands

All three must pass before a change is finished:

```bash
cargo fmt
```

```bash
cargo clippy -- -D warnings
```

```bash
cargo test
```

## Tool behaviour

- **Background processes never outlive a bash call.** The shell runs in its
  own process group, and the group is torn down (SIGTERM, then SIGKILL) when
  the call ends, whether the command finished or timed out. The order on
  normal exit is: wait for the shell to exit, kill the group, then drain
  stdout and stderr to EOF. A `detach` mechanism for long-running processes
  is a phase 3 addition alongside policy, not something to bolt on earlier.
- Every tool caps what it returns, keeping the head and the tail of long
  output and stating how many bytes were omitted.
- The working directory is shared by the built-in tools and persists across
  bash calls; a `cd` in a call that times out is discarded.

## Conventions

- `thiserror` enums in `core` (`ProviderError`, `ToolError`); `anyhow` only in the `tui` binary.
- Serde wire shapes: enums are `snake_case`; `Author` is tagged by `kind`, `ContentBlock` by `type`; `Event.created_at` is RFC 3339.
- Skills and tools are data, not code: a skill is a versioned folder loaded at runtime.
