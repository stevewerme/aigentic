```text
      _              _   _
 __ _(_)__ _ ___ _ _| |_(_)__
/ _` | / _` / -_) ' \  _| / _|
\__,_|_\__, \___|_||_\__|_\__|
       |___/
```

# Aigentic

An open source agent harness in Rust that runs open-weight models, on EU
infrastructure or any OpenAI-compatible or Anthropic endpoint, and ships
as a single terminal binary. The harness owns everything around the model,
from the loop and the tools to skills, permissions and the client; the
model is a swappable backend behind one trait. One append-only event log per
thread is the source of truth, with model context, the terminal view, costs
and resume all projections of it; humans and agents share threads on equal
terms, every event attributed, and projects carry instructions, knowledge
and memory across threads.

## What works today

- A streaming terminal client: an inline shell, not a full screen —
  markdown replies, tool calls with truncated output, a file picker,
  command completion, a transcript pager and a turn line while it works
- Two provider adapters (OpenAI-compatible, Anthropic with thinking and
  effort control), config profiles, and a utility profile for side jobs
  such as thread titles and memory extraction
- The event log: resume any thread by id, compaction near the window,
  `/cost` over long threads, attributed permission decisions
- Built-in tools — read, write and edit files (edits report diffs), list,
  grep, bash, knowledge search — plus MCP servers
- A policy you own: reads and an allow-list of build and test commands run
  without asking, everything else prompts inline; permission modes when you
  want less asking
- Skills as versioned folders with a `SKILL.md`, run as slash commands,
  vendored and hashed before they load
- Projects: `aigentic.toml`, layered instructions (global, workspace,
  project), knowledge, memory; workspaces, and `/project use` to move a
  thread between projects without losing the conversation
- The multiplayer daemon: `aigentic serve`, sessions over a Unix socket or
  TCP, per-user roles checked on every request
- `aigentic exec` for scripting (exit 0 done, 1 failed, 3 a human was
  needed) and `aigentic doctor` for setup checks

## Getting started

Needs a stable Rust toolchain.

1. Install:

   ```bash
   git clone https://github.com/stevewerme/aigentic && cd aigentic
   cargo install --path crates/tui          # installs the `aigentic` binary
   ```

2. Configure the endpoint in `~/.config/aigentic/config.toml` (override
   with `--config`). The key itself is never in this file: each profile
   names the environment variable that holds it.

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
   ```

   Anthropic options (`thinking`, `effort`, …), budgets, compaction tuning
   and `utility_profile` are covered in the
   [configuration reference](crates/tui/README.md#configuration).

3. Put the key in `.env` in the directory you run from — it is loaded at
   startup and gitignored; see [.env.example](.env.example) — or export it:

   ```bash
   echo 'TENSORX_API_KEY=…' >> .env
   ```

4. Check the setup:

   ```bash
   aigentic doctor            # add --probe for one tiny completion per profile
   ```

5. Run it inside the repository you want to work on:

   ```bash
   cd ~/Projects/my-repo && aigentic
   ```

   A new thread starts and its id is printed; resume it later with
   `aigentic --thread <id>`. With no `--server` a daemon is embedded in the
   process for that directory; pass `--server unix:/path` or
   `tcp:host:port` to work against a shared daemon instead.

6. Bring in a new project with the guided setup — the config, the project
   file, `AGENTS.md`, GitHub issue labels through `gh`, knowledge links;
   every file is shown before it is written:

   ```bash
   cd ~/Projects/new-repo && aigentic init
   ```

## Depth

- [docs/PRD.md](docs/PRD.md) — the design: purpose, principles, architecture
- [AGENTS.md](AGENTS.md) — crate layout, the dependency rule, the event-log
  rule, conventions for contributors and agents
- [crates/tui/README.md](crates/tui/README.md) — the full client manual:
  keys, slash commands, policy modes, the config reference

## Developing

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

CI runs the same three on every push and pull request.

## Licence

MIT, see [LICENSE](LICENSE).
