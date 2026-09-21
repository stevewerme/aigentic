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
cargo run -p aigentic-tui -- project setup                       # render docs/agents/*.md from [pocock]
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

## Pocock setup

`[pocock]` in `aigentic.toml` holds the answers upstream's
`setup-matt-pocock-skills` would ask for, and `aigentic project setup`
writes what that skill would write, byte for byte for the default
answers: `docs/agents/issue-tracker.md` for `issue_tracker = "github" |
"gitlab" | "local"`, `docs/agents/domain.md` (single-context), and
`docs/agents/triage-labels.md` when the `triage` skill is enabled or
`triage_labels` maps a canonical role to this tracker's label
(`triage_labels = { needs-triage = "bug:triage" }`). `docs_dir` moves the
`agents/` folder; `prs_as_requests = true` flips upstream's request-surface
flag. The `## Agent skills` block goes into `.aigentic/instructions.md` if
present, else `AGENTS.md`, replaced in place on a rerun; no other file is
touched. The files are generated: edit `aigentic.toml` and rerun, and
commit the result. A test asserts the embedded templates still equal the
vendored skill's, so a `skills update` that changes them fails until the
snapshot is regenerated.

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

### Phase 4 acceptance (docs/PLAN-phase4.md done-when 1 to 5)

Do these in this repository and in Vendela, on both backends, over a
week of daily use, and record thread ids below. Results so far follow
item 20.

16. Projects: in Vendela, `aigentic project init`, then add `[pocock]
    issue_tracker = "github"` and run `aigentic project setup`. Start
    `aigentic` in each repository and in a subdirectory of each; expect
    the banner to name the project and the layers loaded, `aigentic
    threads` to list only that project's threads, and a thread started
    in one never to mention the other's instructions, knowledge or
    memory. Resume one pre-phase-4 thread by id and expect the
    "pre-project layout" note.
17. Layers: put `[tools] denied = ["write_file"]` in `config.toml` and
    `allow = ["read_file", "write_file", "bash", "grep", "list_dir",
    "search_knowledge", "load_skill", "pin", "ask_human"]` in a
    project's `[tools]` (an allow list hides every tool it does not
    name, and a skill whose `requires` names a hidden tool refuses to
    start). `aigentic project show` and `/project` must list
    `write_file` as `denied by global`, `edit_file` as `not in project
    allow`; ask the model to write a file and expect it not to see the
    tool (an `unknown tool` refusal if it tries). Send two turns and
    check on Anthropic that the second reads the prefix from cache.
18. Knowledge: in Vendela (over the threshold) expect `index` in the
    banner and `search_knowledge` among the tools; ask a question only
    a knowledge section answers and expect a `→ search_knowledge` call
    whose result names `path#heading`, then a correct answer. Here
    (under the threshold, once `.aigentic/knowledge/` has a file) expect
    `inline` and no tool. Edit a knowledge file between turns and expect
    the next turn's mode and prefix to reflect it.
19. Memory: state a decision in a turn ("From now on we deploy on
    Fridays."). Expect `[memory: 1 lines written]`, a `memory_extracted`
    event in the log with the line and its `at_seq`, the line in
    `.aigentic/memory/decisions.md` with the date and thread id, and the
    next turn's prefix (visible in `/cost` as a cache invalidation on
    Anthropic, or by asking) to contain it. Have the model infer
    something you did not state and confirm nothing is filed. Edit the
    file by hand and expect the next prefix to change. Set
    `every_n_turns = 2` and confirm one turn is skipped.
20. Pocock: `docs/agents/issue-tracker.md` must be byte-identical to the
    vendored `issue-tracker-github.md` (`cmp`), and `/code-review` on a
    branch with `Closes #N` in a commit must fetch the issue through
    `gh` as that file says, without `/setup-matt-pocock-skills` having
    run.

Phase 4 results, in this repository and Vendela (`~/Projects/vendela`,
set up on 2026-09-21 with `aigentic project init`, a curated copy of
fourteen docs under `.aigentic/knowledge/` and `threshold_fraction =
0.02` so the index switches on under the Anthropic window too):

- Item 16 on TensorX (`z-ai/glm-5.3`), 2026-09-21: starting in Vendela's
  root printed `project vendela · layers: global, project, knowledge
  (index, 14 files) · 3 skills · 0 threads`; thread
  `01M32HQF6Q35NMDE9JDYMV7Q52` answered "what do you know about this
  project" from `AGENTS.md`, `WHERE-WE-ARE.md` and `CONTEXT.md`, and
  its `memory_extracted` event followed the turn with nothing written
  (`/cost` listed one extraction, 1530 in, 469 out). `aigentic threads`
  listed that one thread with 7 events and the first line. Starting in
  `packages/verify` still resolved to `vendela` with `1 threads`
  (`01M32HX7GJXS9XNVVH1N2YHX0G`); starting here printed `project
  aigentic · layers: global, project · 5 skills · 0 threads`
  (`01M32HYAAG3YS9YM1A4YYYH3Q2`). Two notes: the model read the docs
  with `read_file` rather than `search_knowledge`, since the knowledge
  files are copies of files it can name from `AGENTS.md`; and a thread
  appears in the listing only after its first event. `--thread
  01M3288192XFT8GWBR5K0CFJCM` (a phase 3 log, flat in `threads_dir`)
  printed the pre-project-layout note and resumed with 42 events. A
  baseline thread here on Anthropic (`01M32J4HJDCTQ1S7WH5APW3D25`)
  wrote `scratch.txt` with `write_file` behind a permission prompt and
  read 8.8k tokens from cache on its second call.
- Item 17 on Anthropic (`claude-opus-5`), 2026-09-21, in Vendela with
  `denied = ["write_file"]` globally and an allow list naming
  `write_file` but not `edit_file`: `project show` printed `write_file
  denied by global` and `edit_file not in project allow`, the banner
  said 8 tools visible, and thread `01M32JD2R0KT6MWP2NSVE78CNV` listed
  its tools without either. Two findings. First, an allow list that
  left out `pin` refused to start with "skill `code-review` requires
  tool `pin`, which is not available", so the list must name the
  harness tools and `search_knowledge`; the checklist now says so.
  Second, asked to create `scratch.txt`, the model wrote it through
  `bash` (`echo hello > scratch.txt`), which policy prompted as an exec
  call: hiding `write_file` is not a write ban while `bash` is allowed,
  which is section 5's split (narrowing decides what the model sees,
  policy what runs) and is worth remembering when an allow list is
  meant as a guard. The second call read 18.8k tokens from cache, so
  the prefix was byte-stable across turns.
- Item 18 on TensorX, 2026-09-21. In Vendela (index, 14 files, 39.9k
  tokens under `threshold_fraction = 0.02`), thread
  `01M32JXXZWBKDT3VZ53H003MJ4`: asked why a rewriting step needs an
  omission guard, the model answered correctly but read
  `docs/adr/0005-...md` with `read_file`, since the index names the
  file and the same file sits in `docs/`; told to use
  `search_knowledge`, it made four calls whose results were
  `path#heading` blocks and answered from `FOUNDATION.md#Invariants`
  with the invariant's text. A line appended to
  `.aigentic/knowledge/accolm-sequence.md` mid-thread came back from
  the first search of the next turn, so the folder is re-read at the
  turn boundary. Here, with one 33-token file, the banner said
  `knowledge (inline, 1 files)` and `/project` listed no
  `search_knowledge` (`01M32K55320P515A0SEFS14477`). Two findings.
  First, the scorer ranked the long `adr/0006#Context` section above
  `FOUNDATION.md#Invariants` for three phrasings of the question:
  term counts without length normalisation favour long sections, which
  decision 6 said a small folder would not need; an open item. Second,
  when knowledge holds one thin line the model keeps searching and then
  speculates from neighbouring sections, and a question sent to the
  wrong project (aigentic, no knowledge) got twenty tool calls and an
  invented answer rather than "not here"; model behaviour, noted for
  the memory filter's sake.
- Item 19 on TensorX, 2026-09-21, in Vendela. Thread
  `01M32K8XNBMG3MMPGMD834AEKS`: "Decision: we deploy on Fridays only,
  never on a Monday" ended with `[memory: 1 lines written]`, a
  `memory_extracted` event carrying the line at `at_seq 0` (the user
  message), and the bullet in `.aigentic/memory/decisions.md` with the
  date and thread id. The next turn answered from the prefix; an
  inference turn ("what day do you think we cut releases") and two
  others filed nothing (`/cost`: 4 extractions, 1 line). The hand edit
  (Fridays to Thursdays) did not reach the following turn: memory was
  re-read only at startup and after an extraction, one turn late. Fixed
  in `c437679` (the turn re-reads memory files like knowledge); on the
  resumed thread the edit was in the next prefix, and the model noticed
  it contradicted its pin and the docs and called `ask_human` rather
  than pick one. With `every_n_turns = 2`, thread
  `01M32MNX7B7G7PS54Y9B69A5ZD` ran a turn with no extraction event, and
  the second turn extracted once over both (`/cost`: 1 extraction, 1
  line). Two findings. First, the model read the bare "Decision: ..."
  as a request to file it in the repository (Vendela's instructions say
  decisions live in owning docs), edited two docs, ran the tests and
  committed, every step behind a permission prompt the human answered;
  the commit was dropped afterwards. A statement of fact ("For the
  record, ...") is filed by memory without that. Second, the model
  pins facts on its own alongside memory, so a fact can live in the
  thread prefix and the project files at once.
