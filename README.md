# Aigentic

An open source agent harness in Rust that runs open-weight models on EU
infrastructure and ships as a single terminal binary. The harness owns the
loop, the tools, the event log, and later skills, permissions and projects.
The model is a swappable backend behind one trait.

Three ideas shape it. **Monothreading:** one ordered event log per thread is
the source of truth; model context and the UI are projections of it.
**Multiplayer:** several humans and agents share a thread on equal terms,
with every event attributed. **Projects:** a scoping layer that carries
instructions, knowledge and memory across threads.

The design is in [docs/PRD.md](docs/PRD.md); the current phase's plan is in
[docs/PLAN-phase0.md](docs/PLAN-phase0.md); conventions for contributors
and agents are in [AGENTS.md](AGENTS.md).

## Status

| Phase | Builds | State |
| --- | --- | --- |
| 0 Loop | Canonical types, OpenAI-compatible adapter, three tools, streaming REPL | Done, accepted live 2026-09-21 |
| 1 Two providers | Anthropic adapter, capability struct, caching contract | Done, accepted live 2026-09-21 |
| 2 Event log | Resume, compaction, `/cost` over long threads | Next |
| 3 Skills and policy | Skill loader, risk classes, permission prompts, MCP client | |
| 4 Projects | `project.toml`, instruction layering, knowledge, memory | |
| 5 Server and multiplayer | Daemon, socket API, turn queue, per-user permissions | |
| 6 Orchestrator | Portfolio project coordinating work across projects | |

Phase 0 is usable but has no permission prompts: the `bash` tool runs
whatever the model asks, in the directory you launch from. Use it in
repositories you would let an agent loose in.

## Quick start

Needs a stable Rust toolchain and any OpenAI-compatible endpoint. Hosted EU
providers and a local llama.cpp server both work.

1. Configure the endpoint. The API key is never in this file, only the
   name of the environment variable that holds it:

   ```bash
   mkdir -p ~/.config/aigentic && cat > ~/.config/aigentic/config.toml <<'TOML'
   base_url = "https://api.tensorx.ai/v1"
   model = "z-ai/glm-5.3"
   api_key_env = "TENSORX_API_KEY"
   TOML
   ```

2. Provide the key, either exported in your shell or in a `.env` file in
   the directory you run from (see [.env.example](.env.example)).

3. Run it from inside the repository you want to work on:

   ```bash
   cargo run -p aigentic-tui --
   ```

   A new thread id is printed. `--thread <id>` resumes it later by
   replaying its log. `/cost` shows tokens, `/quit` exits.

## Layout

One Cargo workspace. `core` holds the canonical types and traits and depends
on nothing; every other crate depends on `core`; `runtime` is the only crate
that knows the rest; `tui` talks to the runtime API only. Details, the
dependency rule and the event-log rule are in [AGENTS.md](AGENTS.md).

```
crates/core       canonical message, event and tool types; Provider and Tool traits
crates/log        append-only JSONL event store and projection
crates/providers  OpenAI-compatible adapter (Anthropic in phase 1)
crates/tools      read_file, write_file, bash
crates/runtime    the agent loop and its seams
crates/tui        the `aigentic` binary, a streaming REPL
crates/skills     phase 3
crates/policy     phase 3
```

## Developing

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

CI runs the same three on every push and pull request.

## Licence

MIT, see [LICENSE](LICENSE).
