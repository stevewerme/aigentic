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
estimated shown separately, plus cache reads and writes and the reasoning
share), `/quit`. Anything else starting with `/` prints
`unknown command`. Ctrl-D quits; Ctrl-C clears the line.

Assistant text streams as it arrives. Tool calls print as `→ name {args}`
and their output follows, truncated to twelve lines or 1200 bytes with a
note of what was omitted. There are no permission prompts in phase 0; the
policy seam allows everything.

Thread logs are JSONL files, one per thread, under `threads_dir`. The
working directory at launch is the tools' working directory and the source
of repository instructions (`AGENTS.md`, falling back to `CLAUDE.md`).

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

Status: all seven steps passed on 2026-09-21 against TensorX
(`https://api.tensorx.ai/v1`, model `z-ai/glm-5.3`). The resumed thread
answered from context with no tool calls. Re-run after changes to the
loop, the tools or the adapter.
