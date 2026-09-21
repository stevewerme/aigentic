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
# [profiles.anthropic.compaction]          # any profile; all optional
# trigger_fraction = 0.7                  # of the model's window
# keep_turns = 8                          # verbatim tail after a summary
# max_result_bytes = 4096                 # truncation target for old tool results
# summary_max_output_tokens = 2048

# user = "steve"                          # author id on your messages ($USER by default)
# threads_dir = "/path/to/threads"        # default ~/.local/share/aigentic/threads
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
```

Slash commands: `/cost` (input and output tokens for the thread, reported and
estimated shown separately, plus cache reads and writes, the reasoning share
and compactions), `/pin <text>` (a fact for the stable prefix, never
summarised), `/compact` (run compaction now and report what it did),
`/quit`. Anything else starting with `/` prints
`unknown command`. Ctrl-D quits; Ctrl-C clears the line.

Assistant text streams as it arrives. Tool calls print as `→ name {args}`
and their output follows, truncated to twelve lines or 1200 bytes with a
note of what was omitted. There are no permission prompts in phase 0; the
policy seam allows everything.

Thread logs are JSONL files, one per thread, under `threads_dir`. The
working directory at launch is the tools' working directory and the source
of repository instructions (`AGENTS.md`, falling back to `CLAUDE.md`).

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
