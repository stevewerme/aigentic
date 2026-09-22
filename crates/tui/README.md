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

# [display]                               # how much of a tool result the terminal shows
# result_lines = 3                        # lines before truncating
# result_bytes = 600                      # bytes before truncating
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
cargo run -p aigentic-tui --                                     # new thread over a daemon embedded for this directory
cargo run -p aigentic-tui -- --server tcp:vm:7420 --project vendela   # the same client against a remote daemon; token in AIGENTIC_TOKEN
cargo run -p aigentic-tui -- --thread <ULID>                     # resume by replaying the log
cargo run -p aigentic-tui -- --thread <ULID>                     # the backend is the project's [model] profile (see phase 5 item 26)
cargo run -p aigentic-tui -- project init                        # aigentic.toml + .aigentic/{knowledge,memory}
cargo run -p aigentic-tui -- project setup                       # render docs/agents/*.md from [pocock]
cargo run -p aigentic-tui -- project show                        # layers, knowledge mode, every tool's fate
cargo run -p aigentic-tui -- threads                             # this project's threads, newest first
cargo run -p aigentic-tui -- doctor [--probe]                    # config, keys, threads dir, project, skills, gh; exit 1 on a fail
cargo run -p aigentic-tui -- init                                # guided setup: config, project, GitHub, knowledge links
cargo run -p aigentic-tui -- serve [--listen unix:/path|tcp:host:port]   # the daemon (phase 5): server.toml's users and projects
cargo run -p aigentic-tui -- serve --new-token magnus            # a fresh token for a user, printed once, never stored
```

The daemon reads `server.toml` beside `config.toml`: `listen` (`unix`, `unix:/path`
or `tcp:host:port`; plain TCP with tokens, so put a reverse proxy or an SSH
tunnel in front of it on a network), `idle_unload_secs`, `[[users]]` with
`name` and `token_env` (the variable in the daemon's environment holding that
user's token; the first user is the owner), and `[[projects]]` with `name` and
`root` (the checkout on the daemon's machine). A session says hello with its
token, is told which projects it has a role in, and every request is checked
against `[participants]` in the project's `aigentic.toml` before it reaches the
thread. A line over 4 MiB closes the session.

`init` is the guided setup, in the spirit of upstream's
`setup-matt-pocock-skills`: explore, show, confirm, write. It needs a
terminal. Four sections, each showing what exists and proposing a
default that Enter accepts: the config (created from the example when
absent, the default profile, whether each key variable is set); the
project (name, description, `[model] profile` into `aigentic.toml`, the
`.aigentic/` folders, and an `AGENTS.md` with a heading and a Commands
section when no instructions file exists); GitHub (when origin is on
GitHub and `gh auth status` passes: `[pocock] issue_tracker = "github"`,
the `project setup` files, and `gh label create` for each of the five
triage labels the repo lacks, one confirmation for the batch); and
knowledge (symlinks into `.aigentic/knowledge/` for `docs/` and
`CONTEXT.md`). Every file is shown before it is written, nothing reaches
GitHub without a yes, and a rerun shows current values as defaults and
writes only what changed. Hand edits elsewhere in `aigentic.toml` are
kept.

`doctor` prints one line per check (`ok`, `fail` or `skip`, then the
message) and exits 1 when any fails. It names each profile's key variable
and says whether it is set, never the value. `--probe` adds one
completion per profile with a one-token cap and reports the model and
latency; it is the only check that uses the network.

The profile is `--profile`, else the project's `[model] profile`, else the
config's `default_profile`. The banner names the project, the layers it
loaded (global, project, knowledge with its mode, memory), the skill and
tool counts and how many threads the project has.

Since phase 5 the REPL is a client of the daemon: with no `--server` it
starts one in the process for this directory over a private socket, so a
single user sees what phase 4 showed; with `--server` it talks to a remote
daemon as a named user and other people's messages arrive live under their
names. A permission request or an `ask_human` question prints as a state,
and `y`, `a`, `n` or a plain line answers it when your role allows; when
someone else answers first, the prompt is withdrawn with their name and
the decision prints with it. Streamed text prints as its lines complete.
`/profile` is gone: the profile is the project's on the daemon.

Slash commands: `/cost` (input and output tokens for the thread, reported and
estimated shown separately, plus cache reads and writes, the reasoning share,
compactions and memory extractions), `/pin <text>` (a fact for the stable
prefix, never summarised), `/compact` (run compaction now and report what it
did), `/verbose` (toggle the session between the configured cap and 40 lines
/ 8000 bytes; it prints which is on), `/mode [name]` (show or set the
permission mode, below), `/who` (the participants and their roles, and who
you are), `/queue` (what the thread is doing and what is queued),
`/interrupt <text>` or a line starting with `!` (end the running turn and
start one with this), `/policy` (the rule table in order
with each rule's name, decision and reason, the bash allow patterns, the
mode, and the session grants with who gave them), `/memory` (the memory
files with line counts, the `through_seq` of the last extraction, and
the block as the prefix carries it), `/skills` (the enabled set), `/project`
(the same report as `project show`, over the live registry so MCP tools are
included), `/threads`, `/<skill> [args]` for every enabled user-invoked
skill, `/help`, `/quit`. Anything else starting with `/` prints `unknown
command`. Ctrl-D quits; Ctrl-C clears the line.

The permission mode is session state, like a session grant: never
persisted, never an event. `--mode` sets it at start and `/mode <name>`
changes it between turns; the banner shows it unless it is `manual`.
`manual` (default) sends every ask to you. `accept-edits` runs
`write`-class calls that would have asked; the shell still asks. `auto`
runs anything the rules would ask about. A rule's `deny` (the memory
files, a project's own deny rules) stands in every mode, and each call a
mode lets through carries the record `mode accept-edits` or `mode auto`
on its tool result, so the log still says why it ran.

An answered `ask_human` ends the turn with reason `asked_human` and the
REPL continues at once: what follows the answer is a new turn with its
own budget and `/cost` line. A thread quit or killed right after an
answer continues on the next start, after `[the human's answer is
recorded; continuing]`.

After a turn that ends `done`, memory extraction runs with the project's
model and prints `[memory: N lines written]` when anything new landed in
`.aigentic/memory/`; `[memory] enabled = false` turns it off.

Assistant text streams as it arrives. Tool calls print as `→ name {args}`
and their output follows, truncated to `[display]`'s `result_lines` or
`result_bytes` (3 lines / 600 bytes by default) with a note of what was
omitted; `/verbose` widens the session to 40 lines / 8000 bytes.

## Project file, policy, skills and MCP

`aigentic.toml` in the working directory (see the one at the repo root)
enables skills by name, prepends policy rules or replaces the bash allow
patterns, and declares MCP servers. Without it: no skills, the default
policy (`docs/PLAN-phase3.md` section 4), no servers.

Two default rows refuse `write_file` and `edit_file` under
`.aigentic/memory/` with "memory is written by extraction; edit it
outside the thread"; a `[policy] rules` entry may carry its own
`path_prefix`, matched against the call's `path` argument relative to
the project root. `bash` is not covered: narrowing and policy decide
what the model sees and what runs, not what a shell can reach.

A call the policy asks about is a thread state, `AwaitingApproval`, that
every session on the thread sees. A user with `approve` (or `admin`) gets
the tool, its class, the reason and the arguments, then the prompt: `y`
runs it once, `a` runs it and every identical call (same tool; for `bash`,
the same command) while the daemon keeps the thread loaded, `n` denies.
Anyone else sees `[waiting for an approver: bash rm -rf x]` and cannot
answer. The answer is a `Decide` request; the first one wins, and a prompt
answered on another connection first is withdrawn with `[decided by
magnus]` before the decision prints as `[allowed by magnus]`, `[allowed
for this session by magnus]` or `[denied by magnus]`. Every decision is
a `permission_decided` event with the decider's user id; a session grant
is still an event each time it is used. Rule decisions are recorded on
the tool result. An `ask_human` question works the same way through
`AwaitingHuman`: a `write` user gets `[question] ...` and the next plain
line answers it, a `read` user sees `[waiting for an answer: ...]`, and a
question answered elsewhere prints `[answered elsewhere]` (the answer is
a tool result, which names nobody). A thread opened while it waits shows
the prompt at once. There is no timeout: a request waits until someone
decides or interrupts, and an interrupt denies it with the interrupter's
name and the reason `interrupted`. With stdin not a terminal the prompt
still waits for a line; nothing is denied by default.

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
- Item 20 on TensorX, 2026-09-21, in Vendela, thread
  `01M32MXNY0SP78PKV482HTEF41`. `docs/agents/issue-tracker.md` was
  byte-identical to the vendored `issue-tracker-github.md` (`cmp`), with
  `/setup-matt-pocock-skills` never run. Asked in prose to review since
  `HEAD~1` (the skill is model-invoked, so `/code-review` is not a
  slash command), the model called `load_skill` for `code-review`, read
  `docs/agents/issue-tracker.md`, ran `gh issue list` and `gh issue view
  49 --comments` as that file says, matched the issue by title although
  the commit said `Closes #N` with the placeholder left in, and wrote
  the two-axis report with #49 as the spec source. It ended with an
  `ask_human` about closing the issue. It also noticed the knowledge
  copy of `accolm-sequence.md` had drifted from `docs/`, the cost of
  copying files into `.aigentic/knowledge/`.

- Item 19 on Anthropic (`claude-opus-5`), 2026-09-21, in Vendela, thread
  `01M32P070950D9TZY75V0981JZ`: "For the record, our on-call rotation
  changes on Wednesdays" was pinned by the model and filed by extraction
  (`[memory: 1 lines written]`); the recall turn answered from the
  prefix after a grep and a knowledge search found nothing else; the
  guess turn filed nothing; the line edited by hand to Fridays was in
  the next turn's prefix (that call wrote 66.7k tokens to cache where
  the earlier ones had read, the one invalidation the PRD describes),
  and the model raised the contradiction with its own pin and asked
  which stood. `/cost`: 4 extractions, 1 line. A TensorX rerun of the
  same item without the profile flag (`01M32NT8DHQS6X097NPBZRT5H1`)
  showed a new finding: the prefix's memory block names the files, so
  the model located `.aigentic/memory/facts.md` and appended the fact
  itself with `edit_file`, and extraction then filed the same fact in
  other words, which byte-level dedup cannot catch. Open item: the
  memory heading should say the harness writes these files, and a
  default policy rule should refuse writes under `.aigentic/memory/`.

- Item 18 on Anthropic, 2026-09-21, in Vendela, thread
  `01M32PEREWQWNBC0SGX131NV9R`: unlike GLM, Opus called
  `search_knowledge` on its own for the omission-guard question and the
  first hit was `adr/0005-...md#Scope`; it then read the ADR from
  `docs/` for the full text. The invariant question ranked
  `adr/0006#Context` above `FOUNDATION.md#Invariants` again (a second
  query hit the file's top section and the answer quoted invariant 7
  correctly). A line appended to the knowledge copy of
  `accolm-sequence.md` mid-thread came back from the first search of
  the next turn; the model then diffed the copy against `docs/`, called
  the appended lines test residue, and proposed gitignoring
  `.aigentic/`, which the PRD's plain-files rule says not to do. The
  residue was removed by hand afterwards.

- Item 16 on Anthropic, 2026-09-21: Vendela's root printed the
  `claude-opus-5` banner with the same layers, and thread
  `01M32PB9EHZJVGZVW3N4814ZPZ` answered "what is this project" from two
  `search_knowledge` calls made unprompted. Resuming the phase 3 thread
  `01M328Z40PB7HEH28C9SC45R0X` printed the pre-project-layout note and
  57 events. That thread belongs to this repository, and started from
  Vendela it resumed under Vendela's layers: the flat layout has no
  project field (plan decision 8), so the fallback cannot tell. Noted,
  not fixed.
- Item 20 on Anthropic, 2026-09-21, in Vendela, thread
  `01M32PY61X42J4P34QFCVEDH9K`: the model ran `gh issue view 50` from
  the commit message before loading `code-review`, then called
  `load_skill`, separated the committed diff from the uncommitted
  `docs/agents` rewrites it had first conflated, ran the tests, and
  reported both axes with #50 as the spec. No sub-agents exist here, so
  the two axes ran in sequence, which it said.

Days of use, for done-when 1:

- 2026-09-21, here, thread `01M32SZ1SVD0MDGYS77H0558TP`: "what's next
  in the plan, and the Pocock skills feel unused" became a review of
  the 33 pending vendored skills, 24 accepted and 9 declined, and the
  model went on to implement a `rejected` review state after an
  `ask_human` answer chose the design. The turn hit the 2M budget
  mid-edit with the tree uncompilable; that produced step 9's turn split
  and `touched` list. Finished by hand as `bfaeef7` and `10f44c7`.
- 2026-09-22, Steve's notes after a day and a half on both projects:
  tool output takes over the terminal even at twelve lines; a folded
  view needs the next client, not this REPL. A mode selector (auto,
  accept edits, manual) is wanted for the same reason: the prompts are
  the other half of the noise. Onboarding is the weakest surface:
  profile, key, project, GitHub through `gh`, and the scaffolding
  `[pocock]` renders should be one guided command, in the spirit of
  upstream's setup skill, and the `gh` integration should be able to
  set up issues and labels. Those became step 10: `doctor`, the
  `[display]` cap with `/verbose`, the modes, `init`, and `/profile`,
  `/policy`, `/memory`, all landed the same day.
- 2026-09-22, here on TensorX, thread `01M33SJ982MED4WCDE6063DTDD`:
  "according to our planning, what's next in the pipeline?" read the
  plan and the git log and answered with the step 9 table and the three
  next acts, one turn, no edits.
- 2026-09-22, here on TensorX, thread `01M33XR1N5EYAYJJS0VW6C40BH`:
  "What's next?" then "Yes" had the harness implement step 10 b itself:
  it read the config, the REPL and the README, edited all five files
  including the plan's status line, ran `cargo fmt`, clippy and the
  tests to green, reviewed its own diff, checked the commit-message
  convention from `git log`, and wrote the message to a file after a
  heredoc failed. The turn ended `max_tokens` before `git commit`, with
  `[turn ended: max_tokens; wrote ...]` naming the six files (step 9's
  `touched`). Committed by hand as `88bff66` with its message unchanged.
  Every edit went through a `y` prompt until `a` for the session; that
  is the noise the modes address.

Phase 4 closed on 2026-09-22. Done-when 2 to 5 hold on both backends
as of 2026-09-21 (items 16 to 20 above, each on TensorX `z-ai/glm-5.3`
and Anthropic `claude-opus-5`), with one harness fix along the way
(`c437679`, memory re-read at turn start) and one checklist correction
(item 17's allow list). Done-when 1, as amended to two days of real
use: the thread logs under `~/.local/share/aigentic/threads/` hold 5
threads for this project and 16 for Vendela. Day one (2026-09-21) had
real tasks in both projects on both backends, apart from this
repository on Anthropic, where the only thread is item 16's scripted
baseline: here on TensorX the skills review and `rejected` state
(`01M32SZ1SVD0MDGYS77H0558TP`); in Vendela on TensorX and on Anthropic
the code reviews along both axes (`01M32MXNY0SP78PKV482HTEF41`,
`01M32PY61X42J4P34QFCVEDH9K`) besides the scripted items. Day two
(2026-09-22) was this repository on TensorX only, the two threads above.
So the two days are recorded with one gap: no real task here on
Anthropic, and no second day in Vendela. Both are the first days of
phase 5 rather than a reason to hold the phase, since every harness
change the days produced has landed and the remaining question is
model behaviour, not harness behaviour. Open items the run produced
landed as step 9 (length normalisation, the memory heading and rule,
symlinked knowledge, the banner's count, the narrowing note) and step
10; the two that did not (the flat-layout fallback's missing project,
`read_file` preferred over `search_knowledge` for files in the
repository) are phase 5 open items in `docs/PLAN-phase4.md` section 12.

### Phase 5 acceptance (docs/PLAN-phase5.md done-when 1 to 5)

Two people, two machines, one daemon. Everything below is a by-hand run;
record thread ids and findings under "Results" as phase 4 did. Items 21
to 25 are done-when 1 to 5; item 26 is the rerun of items 1 to 20
through the socket. The three findings step 10's tests turned up are
folded into the items so the run does not trip on them.

**The daemon on the VM.** One machine holds the checkouts and runs
`aigentic serve`; nothing else needs a checkout, a key or the skills.

1. Build and install the one binary on the VM
   (`cargo install --path crates/tui --force`), then clone this
   repository there too: `config.toml`'s `bundled_dir` must point at
   that checkout, because the vendored `skills/` and
   `skills.lock.toml` live in it and the daemon builds every thread
   (the phase 4 open item on a single-binary home for bundled skills
   is answered this way for phase 5).
2. `~/.config/aigentic/config.toml` on the VM holds the profiles and
   `bundled_dir`; the key variables live in the daemon's environment
   only. A client machine's `config.toml` needs nothing but
   `[display]`, and `user` for the embedded case.
3. `~/.config/aigentic/server.toml`:

   ```toml
   listen = "tcp:127.0.0.1:7420"        # loopback; clients reach it over an SSH tunnel
   idle_unload_secs = 600

   [[users]]
   name = "steve"                       # first user: the daemon's owner
   token_env = "AIGENTIC_TOKEN_STEVE"

   [[users]]
   name = "magnus"
   token_env = "AIGENTIC_TOKEN_MAGNUS"

   [[projects]]
   name = "aigentic"
   root = "/srv/aigentic"

   [[projects]]
   name = "vendela"
   root = "/srv/vendela"
   ```

4. Mint one token per user with `aigentic serve --new-token magnus`;
   it prints once and is never stored. Put each in the named variable
   in the daemon's environment (a systemd unit's `EnvironmentFile=`
   with mode 0600 is the plain answer; the same file holds the model
   keys) and hand each person theirs out of band. Never paste one into
   a chat, a shell history or a file in a checkout.
5. In each project's `aigentic.toml`, `[participants]` names everyone
   with a role, the owner included: a table that names anyone gives
   the owner nothing unless listed (an absent table is the only case
   where the owner is admin by default). For done-when 3 the roles
   below assume `steve = "admin"`, `magnus = "approve"` and a third
   name with `"write"`, then `"read"`.
6. Start the daemon (`aigentic serve`, or the unit) and check its
   banner counts the users and projects and names the listener. From each
   client machine open a tunnel, `ssh -N -L 7420:127.0.0.1:7420 vm`,
   export the person's token as `AIGENTIC_TOKEN`, and run
   `aigentic --server tcp:127.0.0.1:7420 --project aigentic`. The
   banner must say `as <name> (<role>)` with the daemon's version.
   Plain TCP carries the tokens, so nothing listens off loopback
   without a proxy in front (plan decision 9).

21. Done-when 1, attribution. Both people open the same thread (the
    second with `--thread <ULID>` after the first's banner prints it).
    Each posts a message; the other's arrives live as `<name>: ...`
    and the model's reply names both. Then ask for something the
    rules prompt on, and let the person who did not ask decide: pick a
    shell command off the default allow list (`printf`, not `echo`;
    the list in `docs/PLAN-phase3.md` section 4 runs without asking),
    or an edit under `manual`. The asker sees the prompt and does not
    answer; the other sees the same prompt and answers `y`; the asker's
    terminal prints `[decided by magnus]` then `[allowed by magnus]`,
    and the log's `permission_decided` event carries
    `{"kind":"user","id":"magnus"}`. Also try the reverse: the
    asker answers first and the other's prompt is withdrawn the same
    way. A `read` user on the thread sees `[waiting for an approver:
    ...]` and no prompt.
22. Done-when 2, queue and interrupt. Ask for a long answer; while it
    streams, the other person posts a line: it prints in both
    terminals at once as `<name>: ...` with `[queued for the next
    turn]` for the poster, `/queue` counts it, and the model does not
    address it until the next turn starts (which it does on its own
    when the running one ends). Then, during another long answer, post
    `!stop, do X instead`: within a second the stream stops,
    `[interrupted by <name>]` prints, the log has `interrupted {
    reason: "interrupt", by }`, and the new turn answers the interrupt.
    Then interrupt while a tool runs (`bash sleep 20`): the tool's
    result is recorded before the interrupt, in the log and on screen.
    Then interrupt while a prompt waits: the request is denied with
    the interrupter's name and `(interrupted)`. Then `kill -9` the
    daemon mid-turn and restart it: the first `--thread` resume shows
    the phase 2 synthetic results and the interrupted note over the
    socket, and the thread continues.
23. Done-when 3, roles. With the third name as `"read"`: their post is
    `[refused: ...]` naming the role, and the log (on the VM) gains no
    event. As `"write"`: their post lands, their `y` on a prompt is
    `[refused: ...]`, their answer to an `ask_human` question lands.
    As `"approve"`: `/mode auto` sets the mode and every subscriber
    prints `[mode auto]`; a `write` user's `/mode` is refused. Roles
    change in the project file on the VM, which is read on every
    request, so the next request picks the change up.
24. Done-when 4, the client alone. On a laptop with no daemon
    reachable, `aigentic` in a checkout embeds one: the banner says
    `embedded daemon`, and a prompt answered by yourself prints
    `[allowed by <you>]`. Item 26 is the rest of this done-when.
25. Done-when 5, thread project. From a client machine with no
    checkout, `aigentic --server ... --project vendela` creates a
    thread; on the VM `aigentic threads` in `/srv/vendela` lists it
    (that command reads the log directory, so it runs where the logs
    are: it is not over the API yet), and the log's first line is
    `thread_started` with the project name, the root the daemon used
    and `created_by`. Resume one pre-phase-5 thread by id (a flat
    phase 3 log and a phase 4 directory log) through the embedded
    daemon and expect both to open and replay.
26. Items 1 to 20 through the socket, on the VM and embedded. The
    outcomes are the same; the mechanics differ in four places. Item
    2's thread is created over the API and the banner says `embedded
    daemon`. Item 8's backend swap has no flag: `--profile` is
    accepted and ignored on the REPL path since step 9 (the embedded
    daemon builds the thread from the project's `[model] profile`), so
    set that line in `aigentic.toml` between resumes; the flag still
    selects for `project show`. Items 9 and 10 kill the daemon rather
    than the client: with the embedded daemon that is the same
    process, so they read as before; on the VM they are item 22's last
    step. Item 13's `a` grant lasts while the daemon keeps the thread
    loaded (`idle_unload_secs` after the last session closes, never
    while a prompt waits), not until the process exits, and the
    prompt's Ctrl-C deny is gone: `n`. Everything else, `/cost` and
    `/project` included, is rendered by the daemon and reads the same.

Then the two days of use phase 4 asked for, this time with two people
in Vendela and here, on both backends, and the daemon on the VM for
both days. Note as before what the harness got wrong and what the
model did, and which of those became a commit.

Results: not yet run as of 2026-09-22. Steps 1 to 10 are on `main`
(the last is `e9c1450`); items 21 to 26 wait for the VM and the second
person. What step 10's tests already hold, over a private socket with a
scripted provider and no network: an `ask_human` question answered from
the prompt with the turn continuing, a `bash` request denied with
`[denied by steve]`, and over a three-user daemon the prompt on
`steve`'s terminal withdrawn with `[decided by magnus]` when magnus
decided first while the `read` user saw only `[waiting for an
approver: bash {"command":"printf ok"}]` and the decision. Three
findings from writing those tests are in the items above: `echo` is on
the default allow list; a `[participants]` table must list the owner;
and `ask_human`'s answering author is not in the log (the tool result
is authored `system`), so a question answered elsewhere prints
`[answered elsewhere]` with no name until the runtime records it. A
fourth, from writing item 26: `--profile` is a no-op on the REPL path.
