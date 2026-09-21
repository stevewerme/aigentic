# aigentic-tui

The `aigentic` binary: a plain streaming REPL over the runtime. It is not a
full-screen application; the terminal keeps its normal scrollback, and
`rustyline` provides line editing and history.

## Configuration

`~/.config/aigentic/config.toml` (override with `--config`). One profile per
backend; `--profile` picks one, `default_profile` picks otherwise:

```toml
default_profile = "tensorx"

[profiles.tensorx]
provider = "openai_compat"                # any OpenAI-compatible endpoint
base_url = "https://api.tensorx.ai/v1"
model = "z-ai/glm-5.3"
api_key_env = "TENSORX_API_KEY"          # NAME of the variable holding the key
# max_context_tokens = 131072

[profiles.anthropic]
provider = "anthropic"
model = "claude-opus-5"
api_key_env = "ANTHROPIC_API_KEY"
# base_url = "https://api.anthropic.com"
# thinking = "adaptive"                   # or "off"
# effort = "high"                         # low | medium | high | xhigh | max
# max_output_tokens = 64000
# cache = true
# [profiles.anthropic.budget]              # any profile; all optional; per turn
# max_iterations = 50
# max_tokens = 2000000                     # every call's input + output over the turn
# max_wall_time_secs = 1800
# [profiles.anthropic.compaction]          # any profile; all optional
# trigger_fraction = 0.7                  # of the model's window
# keep_turns = 8                          # verbatim tail after a summary
# max_result_bytes = 4096                 # truncation target for old tool results
# summary_max_output_tokens = 2048

# user = "steve"                          # author id on your messages ($USER by default)
# threads_dir = "/path/to/threads"        # default ~/.local/share/aigentic/threads
# bundled_dir = "/path/to/aigentic"       # holds skills/ and skills.lock.toml; default: the build repo
```

The phase 0 flat form (top-level `base_url`, `model`, `api_key_env`) still
works and is read as a single profile named `default`.

The key is read only from the named environment variable, never from the
config file, and is never logged or printed. Unknown fields are rejected, so
a pasted `api_key` line fails to load. A `.env` file in the current
directory is loaded at startup if present; see `.env.example` at the repo
root. For a local llama.cpp server that needs no key, set the variable to
any non-empty value.

## Usage

```bash
cargo run -p aigentic-tui --                                     # new thread; prints its id
cargo run -p aigentic-tui -- --thread <ULID>                     # resume by replaying the log
cargo run -p aigentic-tui -- --thread <ULID> --profile anthropic # same thread, other backend
cargo run -p aigentic-tui -- project init                        # aigentic.toml + .aigentic/{knowledge,memory}
cargo run -p aigentic-tui -- project show                        # layers, knowledge mode, every tool's fate
cargo run -p aigentic-tui -- threads                             # this project's threads, newest first
```

The profile is `--profile`, else the project's `[model] profile`, else the
config's `default_profile`. The banner names the project, the layers it
loaded (global, project, knowledge with its mode, memory), the skill and
tool counts and how many threads the project has.

Slash commands: `/cost` (input and output tokens for the thread, reported and
estimated shown separately, plus cache reads and writes, the reasoning share,
compactions and memory extractions), `/pin <text>` (a fact for the stable
prefix, never summarised), `/compact` (run compaction now and report what it
did), `/skills` (the enabled set), `/project` (the same report as `project
show`, over the live registry so MCP tools are included), `/threads`,
`/<skill> [args]` for every enabled user-invoked skill, `/help`, `/quit`.
Anything else starting with `/` prints `unknown command`. Ctrl-D quits;
Ctrl-C clears the line.

After a turn that ends `done`, memory extraction runs with the project's
model and prints `[memory: N lines written]` when anything new landed in
`.aigentic/memory/`; `[memory] enabled = false` turns it off.

Assistant text streams as it arrives. Tool calls print as `→ name {args}`
and their output follows, truncated to twelve lines or 1200 bytes with a
note of what was omitted.

## Project file, policy, skills and MCP

`aigentic.toml` in the working directory (see the one at the repo root)
enables skills by name, prepends policy rules or replaces the bash allow
patterns, and declares MCP servers. Without it: no skills, the default
policy (`docs/PLAN-phase3.md` section 4), no servers.

A call the policy asks about prints the tool, its class, the reason and
the arguments, then prompts: `y` runs it once, `a` runs it and every
identical call (same tool; for `bash`, the same command) until the process
exits, `n`, Ctrl-C or Ctrl-D denies. Every answer is a `permission_decided`
event with your user id; a session grant is still an event each time it
is used. Rule decisions are recorded on the tool result. With stdin not a
terminal every prompt is denied and `ask_human` returns "no human
available".

Skills resolve from `./skills`, then `~/.config/aigentic/skills`, then
the bundled `skills/` in `bundled_dir`; each root has its own
`skills.lock.toml`, merged closer-wins. The enabled set is hash-verified
at startup: a changed file or a missing lock entry refuses to start,
naming the skill. A user-invoked skill is a slash command; a model-invoked
one is loaded through the `load_skill` tool. Both print
`[skill <name> loaded]`.

MCP servers connect at startup; each server's tools print with their
descriptions (the server's own text, shown so you see what the model
sees) and register as `mcp.<server>.<tool>` with the server's class,
`network` unless set. A server that fails to connect is reported and
skipped.

## Skills CLI

```bash
aigentic skills list
```

```bash
aigentic skills check
```

```bash
aigentic skills vendor https://github.com/mattpocock/skills@c55ee46 --into user
```

```bash
aigentic skills update
```

`list` shows name, invocation, origin, review state and source. `check`
verifies every hash and runs the static check; it exits non-zero on a
mismatch or while a skill with findings is still `pending`, and a human
clears it by setting `review = { by = "...", on = "..." }` in the lock.
`vendor` clones at the commit (the one place, with `update`, that touches
the network), copies the repository's `skills/` under `skills/<repo>/`,
writes pending lock entries and prints the findings. `update` re-fetches
each recorded source at its head, shows `diff -ru` per changed skill with
the new findings, and applies only what you accept; an applied skill is
pending again.

Thread logs are JSONL files, one per thread, under
`threads_dir/<project name>/` (`threads_dir/_none/` outside a project).
The nearest `aigentic.toml` at or above the working directory is the
project; the working directory at launch stays the tools' working
directory. Project instructions are `.aigentic/instructions.md`, else
`AGENTS.md`; no other product's file is read.

Compaction runs before each model call when the window is past
`trigger_fraction`: old tool results are truncated first, then everything
but the last `keep_turns` turns is summarised by the same model. Both are
`compacted` events in the log; the originals stay. The REPL prints a line
for each.

Resume after a crash: a torn last line is cut (and reported), tool calls
that never got a result receive synthetic error results, an `interrupted`
event is appended, and the turn continues before the first prompt.

## Manual acceptance

Run this against a real repository whenever the loop, the tools or the
adapter change.

1. `cd` into a Rust repository with tests. Ensure the config and the key's
   environment variable are set.
2. Start a new thread and note the id it prints:

   ```bash
   cargo run -p aigentic-tui --
   ```

3. Ask it to read a file: "Read Cargo.toml and tell me the package name."
   Expect a `→ read_file` line, truncated output and a streamed answer.
4. Ask it to make an edit: "Add a doc comment to the first function in
   src/lib.rs." Expect `→ read_file` then `→ write_file`, and the change on
   disk.
5. Ask it to run the tests: "Run cargo test." Expect `→ bash` with truncated
   output and a summary.
6. `/cost`, then `/quit`.
7. Restart with the same thread:

   ```bash
   cargo run -p aigentic-tui -- --thread <ULID>
   ```

   Expect "resumed with N events". Ask "What did you change earlier?" and
   confirm the answer reflects steps 3 to 5 without re-reading anything.
8. Phase 1: resume the same thread with `--profile anthropic` and ask the
   same question. Expect an answer from context. Send one more message and
   check `/cost` shows `cache read > 0`. Then resume once more with the
   TensorX profile and confirm it still answers from context.
9. Phase 2, kill mid-tool: ask "Run `sleep 30` with bash, then tell me the
   time." and `kill -9` the process while the sleep runs. Restart with the
   same `--thread`. Expect the interrupted line, no prompt until the turn
   finishes, and an answer that acknowledges the tool result is unknown.
10. Phase 2, kill mid-stream: ask for a long answer and `kill -9` while it
    streams. Restart; expect the turn to continue and end cleanly.
11. Phase 2, compaction: set `trigger_fraction = 0.05` on the profile,
    hold a conversation until `[compacted: ... summarised ...]` prints,
    then ask about something from before the summary. Check `/cost` lists
    the compaction and, on Anthropic, that the following turn's cache
    write is roughly the summary plus the tail and the one after reads it.

12. Phase 3, tampering: append a newline to a vendored `SKILL.md` that is
    enabled and start the REPL. Expect a refusal naming the skill and the
    hash mismatch; `aigentic skills check` reports the same. Restore the
    file.
13. Phase 3, policy: ask for an edit and expect a `[permission]` prompt
    for `edit_file`; answer `a`, then ask for a second edit and expect
    `[allowed for this session by <you>]` with no prompt. Ask it to run
    `cargo test` and expect no prompt. Ask it to run `rm -rf target` and
    answer `n`; expect the model to see the denial.
14. Phase 3, skills: `/implement <a small ticket>`. Expect
    `[skill implement loaded]`, a `→ load_skill {"name":"tdd"}` call,
    `[skill tdd loaded]`, a failing test, a passing one, `cargo test`.
15. Phase 3, MCP: declare the echo server from `aigentic.toml`'s comment,
    start the REPL, expect the three tools and descriptions printed, ask
    it to echo something, expect a prompt (class network) and the result.

Status: steps 1 to 7 passed on 2026-09-21 against TensorX
(`https://api.tensorx.ai/v1`, model `z-ai/glm-5.3`). Step 8 passed the same
day: the thread resumed on `claude-opus-5`, answered from context, made a
tool call on the next turn with `cache read > 0`, and then resumed again on
TensorX with a correct summary from memory. Steps 9 to 11 passed on
2026-09-21: a `kill -9` during a 30-second bash command resumed with a
synthetic result and an interrupted note, and the model reran the command
saying the first outcome was unknown; a kill mid-stream lost only the
in-flight call and the turn finished on restart; with `trigger_fraction =
0.05` on Anthropic a summary and a truncation both fired, the model
answered pre-summary facts from the summary alone, `/cost` listed both, and
the cache read the summary plus tail on the next call. The run also met an
Anthropic overload wave lasting minutes, recorded as `provider_error` turn
endings; the adapter now retries such replies three times. Re-run after
changes to the loop, the tools or an adapter.

Phase 3 passed on 2026-09-21, in this repository with the five skills in
its `aigentic.toml`:

- Done-when 1 on TensorX (`z-ai/glm-5.3`), thread
  `01M3288192XFT8GWBR5K0CFJCM`: `/implement Add a Lockfile::len() method
  to the skills crate with a test` loaded `implement`, wrote the failing
  test, made it pass, ran the crate's tests; sixteen tool results, ten by
  rule and six by the human, every one with its record. Two findings: the
  model did TDD from memory instead of calling `load_skill` for `tdd`
  after reading "Use /tdd", and the turn hit the 200k token budget on its
  eleventh iteration. The prefix block now tells the model to call
  `load_skill` for a `/<name>` it sees, and the default budget rose to 2M
  tokens and 50 iterations with `[profiles.<name>.budget]` to override.
- Done-when 1 on Anthropic (`claude-opus-5`), thread
  `01M328Z40PB7HEH28C9SC45R0X`: `/implement Add a Lockfile::names()
  method ...` loaded `implement`, called `load_skill` for `tdd`, then hit
  a 400 because the projection had placed the skill body between the
  `tool_use` and its `tool_result`. Fixed in the projection (a message
  produced while calls are unanswered waits for the results); the thread
  resumed, went red then green, ran fmt, clippy and the full suite, loaded
  `code-review` on its own, reviewed the diff and committed `dff1de8`.
  Eleven prompts answered by `steve`, fourteen results all recorded,
  147k of 165k input tokens read from cache.
- Done-when 2: both logs above audited, no `tool_result` without a policy
  record; the test harness audits every runtime test log at drop; the
  loop has one execution call site, `execute` in `turn.rs`, behind
  `policy_check`.
- Done-when 3: a newline appended to the vendored `tdd/SKILL.md` refused
  startup with the hash mismatch naming the skill and both hashes;
  `aigentic skills check` reported the same. An unlocked project skill
  refused with "no entry in skills.lock.toml".
- Done-when 4: the planted injection fixture is flagged on every count
  (`crates/skills/tests`), and `aigentic skills check` exits 1 while the
  33 unreviewed vendored skills with findings stay pending.
- Done-when 5 on TensorX, thread `01M329YYKP7PTX0VB6RCZ6NE18`: the echo
  server from the tools crate declared in `aigentic.toml` printed its
  three tools and descriptions at connect, `mcp.echo.echo` prompted as
  class `network`, and the result `echo: phase 3 acceptance` is in the
  log with the human's decision event.
