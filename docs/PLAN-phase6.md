# Phase 6 plan

Status: grilled and settled on 2026-09-22 (18 questions, section 11 records the answers); steps 1 to 9 landed the same day · Follows `docs/PRD.md` (the Terminal application and Projects sections) and the phase 0 to 5 plans · Renumbers the PRD's build order: the client is phase 6, sandboxing phase 7, the orchestrator phase 8; multiplayer acceptance (phase 5 step 11) waits behind all three

## 0. Goal and done-when

Phases 0 to 5 built the runtime and took it out of the process. What a
person meets every day is still the phase 1 client: one line in, plain
text out, three lines of every tool result, Ctrl-C that redraws the
prompt, and a project the thread is chained to at birth. Phase 6 makes
the single-person experience the product.

The shape it aims for is a conversation, not a workspace. A person's
day crosses the harness, the marketing site and the marketing plan
without opening three conversations, the way a talk between two people
crosses topics. So a thread is the person's, threads are cheap, and a
project is secondary: the context the thread is currently in, proposed
by the model when the talk moves, confirmed by the person, never
switched silently. What the harness learned in one project is
available as understanding in the others; what it may touch is one
project at a time.

The client bar is Codex CLI's terminal client (`openai/codex`,
`codex-rs/tui`, read on 2026-09-22); where this plan copies it, it says
so, and where it differs, section 11 says why. One principle holds
through every phase: the product runs on open-weight models through
TensorX; Anthropic is the reference backend for comparison and never on
the product path.

Done when:

1. **One thread, three projects, one day.** From one thread Steve works
   on the harness (this repository), the marketing site (a second
   repository) and marketing planning (a folder of notes and assets,
   no repository). When a message belongs to another project the model
   proposes the switch before its turn, the client asks, and a `y`
   moves the thread; `/project use <name>` does the same by hand. Each
   proposal and each answer is in the log. After a switch the
   instructions, knowledge, memory, skills, policy and working
   directory are the new project's, the transcript is intact, and the
   projection carries one note naming both projects. Killing the
   daemon and resuming lands in the project the thread was last in.
2. **Understanding crosses projects, touching does not.** In the
   marketing folder the model can answer what the harness does and
   where the site stands, from the related projects' briefs in its
   prefix and their memory and knowledge through `search_knowledge`,
   and cannot read or write their files. A fact about Steve stated
   anywhere lands in the person-level memory; a fact about the project
   lands in that project's.
3. **Start is resume.** `aigentic` with no flags resumes the most
   recent thread wherever it is; the banner names the thread's title
   and project and, when the directory belongs to another project,
   asks whether to switch. `--new` starts a thread in the directory's
   project. Every thread has a title after its first turn.
4. **Interrupt is a reflex.** Ctrl-C during a turn cancels the model
   call within a second and the status row says so; Ctrl-C when idle
   with an empty composer quits on the second press within a second.
   Esc-Esc puts the last user message back in the composer. A message
   typed during a turn shows as queued above the composer with a hint
   that `!` sends it now as an interrupt; Alt-Up pulls it back.
5. **You always know what it is doing and what it costs.** A status
   line under the composer shows model, mode, project, context used as
   a percentage, the running turn's elapsed seconds and the queue depth,
   fed by the daemon, never counted by the client.
6. **Edits are readable, output is expandable.** Every `edit_file` and
   `write_file` shows a coloured unified diff, previewed at three lines
   with the whole diff one key away. Every bash result shows the first
   and last lines with `… +N lines`; Ctrl-T opens the transcript with
   everything. Consecutive reads, lists and greps fold into one
   `Explored` cell. `/diff` shows the project's working-tree diff.
7. **Approvals ask rarely and answer richly.** The prompt is a block
   above the composer; the composer keeps its text. Keys: `y` once,
   `a` this session, `p` this command prefix from now on (persisted as
   a readable rules file), `n` deny, `Esc` deny with a typed reason
   that reaches the model. A working session on this repository in
   manual mode asks fewer than five times and never twice for the same
   argv, counted from `permission_requested` events.
8. **Non-interactive mode.** `aigentic exec "prompt"` (or a prompt on
   stdin) runs one turn to completion with no prompt ever shown, prints
   progress to stderr and the final assistant message to stdout, exits
   0 on `turn_ended`, 1 on failure, 3 when it completed but denied at
   least one request, 130 on interrupt. `--json` prints the thread's
   events as JSON lines instead. A script and a second `aigentic`
   thread can both drive it.
9. **Nothing lost.** The phase 3 to 5 acceptance items 1 to 20 in the
   tui README rerun in the new client with the same outcomes, and every
   pre-phase-6 log opens, replays and lists.

Not in phase 6: OS sandboxing for bash (phase 7: Seatbelt on macOS,
bubblewrap or Landlock on Linux, the escalate-and-justify path), the
orchestrator and any turn that touches two projects (phase 8), a
classifier that proposes switches without the main model (later, from
the proposal log; section 12), full markdown and syntax highlighting
inside diffs (light markdown only), image paste, a side pane for pins,
Vim mode, an external editor key, mouse capture, and multiplayer
acceptance (phase 5 step 11, after phase 7).

## 1. Layout changes

```
crates/tui/src/
  main.rs            # flags (--new added, --project as initial), embed-or-connect, `exec`
  exec.rs            # NEW: the non-interactive runner over the client
  app/               # NEW: the ratatui client, replaces client_repl.rs + repl.rs
    mod.rs           # App: event loop over crossterm events + api::Push
    tui.rs           # inline viewport, scrollback commit, resize replay
    cells.rs         # Cell enum and its rendering to lines
    stream.rs        # assistant text: commit at newlines, redraw the tail
    composer.rs      # multi-line editor, paste, history, queued messages
    completion.rs    # @ files (ignore + nucleo), / commands
    status.rs        # the status line
    blocks.rs        # approval, question and switch-proposal blocks
    pager.rs         # Ctrl-T transcript and /diff, in the alternate screen
    keymap.rs        # one table: key -> Action, printed by /keys
    commands.rs      # slash commands (the phase 5 set, plus /project use,
                     # /diff, /rename, /keys, /new)
  markdown.rs        # NEW: light markdown to styled lines
  diff.rs            # NEW: unified diff to styled lines
crates/tui/Cargo.toml  # + ratatui, crossterm, nucleo, ignore, similar;
                       # - rustyline
crates/api/src/lib.rs  # Push::Usage, Push::ProjectSwitched, Push::AwaitingSwitch,
                       # Request::SwitchProject, AnswerSwitch, Rename, New;
                       # Decision::DeniedWithReason, Decision::AllowPrefix
crates/core/src/event.rs        # ProjectProposed, ProjectSwitched, ThreadRenamed
crates/log/src/payload.rs       # the three payloads
crates/log/src/projection.rs    # the switch as a one-line system note
crates/runtime/src/runtime.rs   # set_project, usage(), utility provider
crates/runtime/src/project.rs   # `related`, brief(), person-level memory path
crates/runtime/src/memory.rs    # the person-vs-project rule, brief updates
crates/runtime/src/title.rs     # NEW: auto-title after the first turn
crates/tools/src/fs.rs          # edit_file/write_file results carry a diff
crates/tools/src/registry.rs    # keeps the Workdir, exposes it; suggest_project
crates/policy/src/rules.rs      # NEW: persisted prefix rules, default allow list
crates/server/src/build.rs      # ProjectContext, apply_project, utility profile
crates/server/src/actor.rs      # Mail::SwitchProject, AwaitingSwitch state, usage pushes
crates/server/src/threads.rs    # flat threads dir, project_of and title from the log
crates/server/src/migrate.rs    # NEW: threads/<project>/<id> -> threads/<id>
docs/PLAN-phase6.md
crates/tui/README.md            # keys, exec, the phase 6 acceptance section
```

`client_repl.rs` and `repl.rs` go; their tests move behind `app/` where
they still apply (slash parsing, truncation, notices) and the rest are
rewritten as cell-rendering tests over recorded pushes.

## 2. Signatures (no bodies)

```rust
// crates/api/src/lib.rs
pub enum Request {
    // ... phase 5 ...
    /// Change the thread's current project by hand. Refused unless the
    /// user has `write` in the target; refused while a turn runs.
    SwitchProject { thread: ThreadId, project: String },
    /// Answer the model's proposal (Push::AwaitingSwitch).
    AnswerSwitch { thread: ThreadId, accept: bool },
    Rename { thread: ThreadId, title: String },
    /// The most recent thread the user has a role in, for `aigentic` with no flags.
    Latest,
}
pub enum Decision {
    Allow, AllowSession, Deny,
    /// `p`: allow this command prefix from now on; the daemon persists it.
    AllowPrefix { prefix: Vec<String> },
    /// `Esc` then text: deny, and the text becomes the next user message.
    DeniedWithReason { reason: String },
}
pub enum Push {
    // ... phase 5 ...
    /// After every model call, switch and queue change: what the status line shows.
    Usage { tokens_in_window: u64, window: u64, turn_elapsed_ms: Option<u64>, queue: usize },
    /// The model proposed a switch; the turn is paused until AnswerSwitch.
    AwaitingSwitch { project: String, reason: String },
    ProjectSwitched { from: Option<String>, to: Option<String>, root: PathBuf, by: Author },
    Titled { title: String },
}

// crates/core/src/event.rs
pub enum EventKind { /* ... */ ProjectProposed, ProjectSwitched, ThreadRenamed }

// crates/log/src/payload.rs
pub struct ProjectProposedPayload { pub project: String, pub reason: String, pub accepted: Option<bool> }
pub struct ProjectSwitchedPayload { pub from: Option<String>, pub to: Option<String>, pub root: PathBuf }
pub struct ThreadRenamedPayload { pub title: String }

// crates/runtime/src/project.rs
pub struct ProjectFile { /* ... */ pub related: Vec<String> }
impl Project {
    pub fn brief(&self) -> Option<String>;                 // .aigentic/brief.md, one screen
}
pub fn person_memory_path(config_dir: &Path) -> PathBuf;   // ~/.config/aigentic/memory.md

// crates/runtime/src/runtime.rs
impl Runtime {
    /// Swap the project half of the layers: instructions, knowledge, memory,
    /// skills, policy root, workdir, MCP servers, budget, related briefs.
    /// Resets `measured`.
    pub async fn set_project(&mut self, ctx: ProjectContext) -> Result<(), RuntimeError>;
    pub fn usage(&self) -> Usage;
    /// The provider for side jobs (titles, memory, briefs): the utility
    /// profile when configured, else the thread's.
    pub fn utility(&self) -> &dyn Provider;
}

// crates/runtime/src/title.rs
pub async fn propose_title(utility: &dyn Provider, first_turn: &[Event]) -> Result<String, ProviderError>;

// crates/runtime/src/memory.rs — the extractor's rule: a fact about the
// person goes to person_memory_path, everything else to the current
// project; at the end of a turn that wrote files, refresh the brief.

// crates/tools — a built-in the model may call first in a turn:
// suggest_project { project: String, reason: String }. The runtime does not
// run it; the actor turns it into AwaitingSwitch and pauses the turn.

// crates/server/src/build.rs
pub struct ProjectContext { pub project: Option<Project>, pub root: PathBuf,
    pub policy: Policy, pub skills: Vec<Skill>, pub mcp: Vec<McpServer>, pub budget: Budget,
    pub related: Vec<RelatedProject> }                       // brief + knowledge + memory, read-only
pub fn project_context(cfg: &ProjectConfig, all: &[ProjectConfig], profiles: &Profiles)
    -> Result<ProjectContext, BuildError>;

// crates/server/src/threads.rs
impl ThreadTable {
    /// The last `project_switched`, else `thread_started`'s project.
    pub fn project_of(&self, thread: ThreadId) -> Result<Option<String>, ThreadError>;
    pub fn title_of(&self, thread: ThreadId) -> Result<Option<String>, ThreadError>;
    pub fn latest_for(&self, user: &str) -> Result<Option<ThreadId>, ThreadError>;
}

// crates/tools/src/fs.rs — the result text of edit_file/write_file starts
// with a unified diff (`--- a/path`, `+++ b/path`) followed by the summary.

// crates/policy/src/rules.rs
pub struct Rules { pub allow_prefixes: Vec<Vec<String>> }
impl Rules {
    pub fn load(global: &Path, project: Option<&Path>) -> Rules;   // ~/.config/aigentic/rules.toml, .aigentic/rules.toml
    pub fn allows(&self, argv: &[String]) -> bool;
    pub fn add_prefix(path: &Path, prefix: &[String]) -> io::Result<()>;
}
pub fn default_bash_allow() -> &'static [&'static str];             // cargo build|test|fmt|clippy|check, git status|diff|log|show|branch, ls, cat, rg, grep, find, head, tail, wc, pwd, which

// crates/tui/src/exec.rs
pub struct ExecArgs { pub prompt: Option<String>, pub json: bool, pub output_last: Option<PathBuf>,
    pub mode: Mode, pub project: Option<String>, pub thread: Option<ThreadId> }
pub async fn run(client: Client, args: ExecArgs) -> anyhow::Result<i32>;

// crates/tui/src/app/cells.rs
pub enum Cell {
    User { author: String, text: String },
    Assistant { lines: Vec<Line> },                      // committed markdown
    Tool { name: String, args: String, state: ToolState, output: Output }, // Running | Ok | Err
    Explored { calls: Vec<(String, String)> },           // folded reads
    Edit { path: PathBuf, added: usize, removed: usize, diff: Vec<Line> },
    Note(String),                                        // [bracketed] notices, switches
    Other { author: String, text: String },
}
pub struct Output { pub head: Vec<String>, pub tail: Vec<String>, pub hidden: usize }

// crates/tui/src/app/tui.rs
pub struct Tui { /* inline viewport: active cell + bottom pane; finished cells go to scrollback */ }
impl Tui {
    pub fn commit(&mut self, cell: &Cell);               // insert_history: scroll region + lines
    pub fn draw(&mut self, active: Option<&Cell>, pane: &BottomPane);
    pub fn on_resize(&mut self, cells: &[Cell]);         // clear + replay at the new width
    pub fn alt_screen<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T; // pager only
}

// crates/tui/src/app/composer.rs
pub struct Composer { /* textarea, history, queued: Vec<Queued>, large_pastes */ }
pub enum Submit { Send(String), Queue(String), Interrupt(String), Command(Command) }

// crates/tui/src/app/keymap.rs
pub enum Action { Submit, Newline, Interrupt, QuitOrClear, RecallLast, Transcript, PullQueued,
    Complete, HistoryUp, HistoryDown, Approve(Decision), AnswerSwitch(bool), /* ... */ }
pub fn action_for(key: KeyEvent, ctx: KeyContext) -> Option<Action>;
```

Config additions (`config.toml`): `utility_profile = "flash"` naming a
profile like any other; for the acceptance week
`z-ai/glm-5.3-flash` on TensorX. Project file: `[project] related =
["aigentic", "aigentic-site"]`.

## 3. The shell: inline viewport, not a full screen

Copied from Codex (`codex-rs/tui/src/tui.rs`, `insert_history.rs`).
The client draws only the bottom of the terminal: the active cell (the
assistant text still streaming or the tool still running), the queued
messages, any block (approval, question, switch proposal), the
composer, the status line. A finished cell is written into the
terminal's own scrollback once, above the viewport, through a scroll
region, so the transcript is the terminal's: it scrolls with the mouse
wheel, survives quitting, and copies with the terminal's selection. The
alternate screen is entered only for the pager (Ctrl-T transcript,
`/diff`).

Streaming: assistant deltas accumulate; whole lines are rendered
through `markdown.rs` and committed as they complete, the unterminated
tail is redrawn in the active slot each frame. Resize clears the
client-written history and replays every cell at the new width, capped
at the last 200 cells so a long thread does not stall the terminal.

Non-TTY stdin (a pipe) keeps today's plain line mode; that is what the
phase 3 to 5 README items and `exec` use.

## 4. Start, composer, keys, queue

`aigentic` with no flags asks the daemon for `Latest` and resumes it;
the banner reads `title · project · mode` and, when the working
directory belongs to a different project, adds
`you are in <name>: switch? y/n`. `--new` starts a thread in the
directory's project; `--thread` resumes a named one; `--project` sets
the first project of a new thread only.

Enter submits, Shift-Enter and Ctrl-J insert a newline. Bracketed paste
inserts as-is; a paste over 1000 characters becomes a
`[pasted N chars]` placeholder expanded on submit. Up and Down walk the
history only when the composer is empty or holds the recalled entry.
`@` opens a file picker over the current project's root (the `ignore`
crate for the walk, `nucleo` for the fuzzy match; a root without a
repository is walked the same way); `/` opens the command list with
descriptions. Tab completes either.

Keys, one table in `keymap.rs`, printed by `/keys`:

| Key | Idle | Turn running |
| --- | --- | --- |
| Enter | send | queue (in the log at once, in context next turn) |
| `!text` Enter | send | interrupt: cancel, then send |
| Ctrl-C | clear draft; empty draft: quit on second press within 1 s | interrupt |
| Esc | close popup or clear draft | interrupt |
| Esc Esc | recall last user message into the composer | same |
| Alt-Up | — | pull the last queued message back into the composer |
| Ctrl-T | transcript pager | same |
| Ctrl-D | quit when the composer is empty | — |
| y / a / p / n / Esc | answer the approval block | same |
| y / n | answer a switch proposal | same |

Queued messages render above the composer as `queued (2) · ! sends now`
with each message's first line. The daemon already holds the queue;
the client shows what `Push::State` reports.

## 5. Cells and the pager

Every push becomes a `Cell`. Assistant text goes through light
markdown: `**bold**`, inline code, fenced blocks in a dim box, bullets.
Tool cells copy Codex's shape: `•` bullet (running: animated, then
green or red), the command highlighted, output under `└` in dim, five
lines head-and-tail with `… +N lines (ctrl-t)`. Reads, lists and greps
that follow each other fold into one `Explored` cell listing the paths.
Edit cells show `• Edited path (+a −r)` and a three-line diff preview
with added lines on a green background and removed on red; the pager
shows the whole diff. The `/verbose` toggle goes; the pager replaces
it. `[bracketed]` notices stay for interrupts, compaction, skills
loaded, memory written, titles, and project switches.

The pager is a scrollable alternate-screen view of every cell at full
length, with `/` search, `q` to leave. `/diff` opens it on
`git diff` plus `git diff --no-index /dev/null <untracked>` run by the
daemon in the project root (a project without a repository reports
"no repository").

## 6. Status line and usage

`Push::Usage` after every model call, every switch and every queue
change: `tokens_in_window` is the runtime's last measured prompt size
plus the turn's output so far, `window` the provider's context window.
The line reads

```
z-ai/glm-5.3 · manual · vendela · 31% context · 12s · queued 1
```

The percentage is `(tokens_in_window − baseline) / (window − baseline)`
with the baseline the measured empty prefix, clamped, the same idea as
Codex's 12 000-token baseline but measured rather than assumed. Elapsed
shows only while a turn runs. `/cost` keeps the full breakdown.

## 7. Approvals and rules

The prompt is a block above the composer:

```
permission · bash (exec) · the rules ask about network access
  curl -s https://api.example.com/health
  y once · a this session · p allow `curl -s` from now on · n deny · esc deny with a reason
```

`p` sends `Decision::AllowPrefix` with the argv prefix up to the first
argument that looks like a value; the daemon appends it to the project's
`.aigentic/rules.toml` when the project has a file, else to
`~/.config/aigentic/rules.toml`, says which in the block, and the
runtime reloads rules before the next call. `Esc` opens a one-line
reason field; the reason is a `permission_decided` with the reason and
then a `user_message` with the same text, so the model sees why.
Decisions made elsewhere withdraw the block as in phase 5. The
`ask_human` question block works the same way with a text field.

The default bash allow list moves from the tui's config default into
`policy::rules::default_bash_allow()` and grows to the build, test,
format, lint and read-only git commands, plus the read-only coreutils.
`[policy] bash_allow` in the project file still replaces it. Done-when
7 is measured by counting `permission_requested` events per thread per
session and checking no argv repeats.

## 8. `aigentic exec`

```
aigentic exec [PROMPT] [--json] [-o FILE] [--mode MODE] [--project NAME] [--thread ID] [--server ADDR]
```

Copied from `codex exec` with two differences. Approvals: a request the
rules would ask about is denied automatically with the reason
`non-interactive`, recorded, and returned to the model, which may work
around it or stop; `--mode auto` runs it instead. The last line on
stderr and the JSON summary say how many requests were denied, and the
exit code is 3 when any were, so a script can treat 3 as "needs a
human". The prompt on stdin when the argument is absent. Human mode
prints cells as plain text to stderr (the same renderer with styling
off) and the final assistant message to stdout; `-o` writes it to a
file as well. `--json` prints every `Push` as one JSON line to stdout
and nothing else. Ctrl-C interrupts and exits 130. A switch proposal
under `exec` is declined and logged. Without `--project` and without a
project file in the working directory `exec` refuses and says which
projects exist.

## 9. Projects underneath the thread

The inversion. A thread belongs to the person who started it and
carries a current project; a project is a root the daemon knows (from
`server.toml`, or the working directory when embedded) that contributes
layers to whatever thread is in it. The log is the only state.

**Proposal and switch.** The model's prefix lists the projects the
user has a role in, one line each with the brief's first line. When a
message belongs elsewhere the model calls `suggest_project` before
anything else; the actor appends `project_proposed`, pushes
`AwaitingSwitch`, and pauses the turn. `y` appends the answer,
switches, and runs the turn in the new project; `n` appends the answer
and runs the turn where it was. The proposal log (project, reason,
accepted) is what a later classifier is judged against; it is kept
from day one. `/project use <name>` and the start-up directory prompt
do the same switch by hand. A switch while idle only; the request is
refused during a turn.

**What a switch does.** The actor builds a `ProjectContext` for the
target and calls `Runtime::set_project`: layers, knowledge reload,
memory, skills, policy root, workdir, MCP servers dropped and
connected, budget, related briefs, `measured = None`; then pushes
`ProjectSwitched` and `Usage`. Pins and compaction summaries stay: they
are in the log. The projection renders the switch as one system line,
`[project: aigentic → marketing; aigentic's instructions and files no
longer apply]`, so the model knows why what it saw a minute ago is
gone.

**Understanding across projects.** `[project] related = [...]` names
the projects this one should understand. For each, the prefix carries
its `.aigentic/brief.md` (one screen: what it is, where it stands,
maintained by the memory writer at the end of turns that changed the
project and editable by hand) and `search_knowledge` indexes its
knowledge folder and memory files, marked read-only by project name in
each hit. No tool reaches a related project's files; that is phase 8.

**Memory.** The extractor's rule: a fact about the person (preferences,
habits, how they want to be addressed) goes to the person-level file
in the config directory, part of the global layer in every project;
everything else goes to the current project's memory. Extraction and
brief updates run on the utility provider.

**Threads.** `ThreadTable::project_of` reads the log, not the
directory. Threads move to one flat directory, `threads/<ulid>.jsonl`;
`migrate.rs` moves `threads/<project>/<ulid>.jsonl` up on first start
(the `thread_started` event already names the project). `/threads`
lists every thread the user has a role in, newest first, with title
and current project; `aigentic threads` in a directory lists that
project's. After a thread's first turn the utility provider proposes a
title, appended as `thread_renamed` by the system author; `/rename`
overrides; `/new` starts a fresh thread in the current project without
leaving the client. `_none` goes; a thread started nowhere in
particular is in the embedded daemon's directory project like any
other.

**Projects without a repository.** `[[projects]]` in `server.toml`
names any root; the embedded daemon names the working directory's
basename. Such a root gets the global layer, a default policy rooted
there, no skills, knowledge if `.aigentic/knowledge/` exists.
`aigentic project init` there writes a file with `kind = "notes"`,
which only changes what `doctor` expects (no repository, no GitHub).

## 10. Steps, one commit each

1. `tools, api, server: diffs in edit results and usage pushes` —
   `edit_file`/`write_file` results begin with a unified diff;
   `Runtime::usage`, `Push::Usage` after model calls and queue changes;
   tests on the result format and the push timing.
2. `tui: exec` — `exec.rs` over the client with the plain renderer,
   `--json`, `-o`, exit codes including 3, the non-interactive denial;
   a test that runs it against the embedded daemon with a scripted
   provider.
3. `tui: the ratatui shell` — `app/` with `tui.rs`, `cells.rs`,
   `stream.rs`, `composer.rs` (multi-line, paste, history), `status.rs`,
   `commands.rs` carrying every phase 5 command; rustyline removed;
   README items 1 to 20 pass by hand. Rendering tests over recorded
   pushes into a `TestBackend`.
4. `tui: keys` — `keymap.rs`, Ctrl-C and Esc semantics, Esc-Esc recall,
   queued messages above the composer, Alt-Up, `/keys`.
5. `tui: cells` — light markdown, tool head-and-tail, `Explored`
   folding, the Ctrl-T pager; `/verbose` removed.
6. `tui, server: diffs` — `diff.rs`, edit cells, `/diff` as a
   `Report` run in the project root.
7. `policy, api, tui: approvals and rules` — `rules.rs` with the
   default allow list and persisted prefixes, `AllowPrefix` and
   `DeniedWithReason`, the approval and question blocks.
8. `tui: completion` — `@` files and `/` commands with `ignore` and
   `nucleo`.
8b. `tui: the turn line` — while a turn runs, a line above the composer:
    what it is doing (the running tool and its argument, else writing or
    thinking), the turn's clock, tools called, the last prompt's size
    and its cached share, output so far, and `esc interrupts`; when the
    turn ends, one dim summary line in the transcript with the same
    figures. Figures are the provider's reported usage from the events,
    never counted by the client. The status line drops its clock.
8c. `runtime, api, tui: the task list` — a built-in `update_tasks` tool
    the model calls with its whole checklist (text and state: pending,
    active, done); no new event, since the call already carries the
    whole list in the log; the list drawn above the
    composer while the turn runs and committed to the transcript when it
    changes to all done. The system prompt asks for it on any task of
    three steps or more: as landed, a fixed harness block after the
    person's global instructions, set by the daemon's build (the tool's
    description alone got zero calls from GLM 5.3 on an explicit
    four-step task; the block got four and five). Its purpose is to keep a turn going until the
    work is finished and to show where it is.
9. `runtime, server: utility profile and titles` — `utility_profile`
   in config, `Runtime::utility`, `title.rs`, `thread_renamed`,
   `/rename`, `Push::Titled`; memory extraction moved to the utility
   provider.
10. `core, log, runtime, server: project_switched` — the kinds and
    payloads, `ProjectContext` and `apply_project` split out of
    `build_thread`, `Runtime::set_project`, `Mail::SwitchProject`,
    `Request::SwitchProject` with the role check, the projection note;
    actor tests that switch between two temp projects and check the
    prefix, workdir, policy root and skills.
11. `tools, server, tui: the proposal` — `suggest_project`,
    `project_proposed`, `AwaitingSwitch`, `AnswerSwitch`, the block,
    the projects list in the prefix; `exec` declines.
12. `runtime: related projects and memory` — `related`, briefs inline,
    related knowledge and memory in `search_knowledge` marked by
    project, brief updates, the person-level memory rule.
13. `server, tui: one threads directory and start-is-resume` — flat
    layout, `project_of` and `title_of` from the log, `migrate.rs`,
    `Latest`, `--new`, `/new`, `/threads` across projects, the
    directory prompt in the banner, `kind = "notes"` in `project init`
    and `doctor`; old fixtures still list.
13b. **The working set** (section 13; grilled before it is built, after
    step 13): the context is kept near a token target whatever the
    window, used tool results become stubs with a handle, a `recall`
    tool brings any of them or an older turn back from the log,
    summaries of older turns are kept continuously on the utility
    model, and broad exploration can run in a child context that returns
    only its conclusion. Probably three or four commits; the grilling
    sets the steps.
14. `docs: phase 6 acceptance` — the tui README's phase 6 section:
    done-when 1 to 9 by hand with thread ids, then a week of daily use
    on the three projects from one thread, alternating backends by day
    as phase 4 did, recording per day the ask count, the proposals and
    whether each was right.

Steps 1 and 2 land first so `exec` exists before the shell changes; 3
to 8c are the client; 9 to 13 the inversion; 13b the working set; 14 the
close, a week of use with all of it. Each step
passes `cargo fmt`, `cargo clippy --all-targets -- -D warnings` and
`cargo test` before its commit.

## 11. Decisions

Settled in the grilling of 2026-09-22 (Q1 to Q18) and in the draft.

1. **Threads are cheap and yours; start is resume.** (Q1, Q16) A
   thread is the person's conversation, any thread can cross projects,
   and `aigentic` resumes the latest. One months-long thread stays
   possible without a change to the log, but v1 does not bet on it.
2. **Never a silent switch.** (Q2) A switch changes policy, tools and
   working directory; an accidental one has teeth. The model proposes,
   the person confirms; the directory prompt at start asks too.
3. **The model proposes; a classifier later, from the log.** (Q9)
   The main model already sees the project list, so proposing costs
   nothing extra. Every proposal and answer is logged so a small
   classifier (an embedding model, `qwen/qwen3-embedding-8b` is served)
   can be trained and judged later without a second guess at the data.
4. **One project at a time; understanding is declared.** (Q11, Q14)
   No turn touches two roots until the sandbox exists. Understanding
   crosses through `related`: briefs inline, memory and knowledge
   searchable, files untouchable. Declared rather than all-projects,
   so vendela's memory never lands in the marketing context.
5. **The switch leaves a note.** (Q10) One projection line naming both
   projects explains to the model why files it saw are gone.
6. **Memory: person global, everything else current project.** (Q12)
   Cross-project tagging waits for evidence from the acceptance week.
7. **Order: client, sandbox, orchestrator, multiplayer.** (Q4, Q8) The
   orchestrator is the first thing that runs unattended, and the
   sandbox is what makes that acceptable. Its first weeks may run on
   the VM before that.
8. **Inline viewport, terminal scrollback.** (Q5) Copied from Codex over
   the PRD's "thread pane": the terminal already scrolls, searches and
   copies better than any pane, the transcript survives quitting, and
   the client stays thin. The alternate screen is for the pager only.
9. **`exec` denies rather than asks, and says so.** (Q6) Codex forces
   its approval policy to `never`, which runs everything. Here the
   safer default is to deny what would have asked and let the model
   react; exit 3 marks a run that needed a human; `--mode auto` is the
   explicit opt-in, made reasonable by phase 7.
10. **Approvals target: fewer than five per session, never twice.**
    (Q7) Per session, not per day, counted from the log.
11. **Prefix rules are files; project first.** (Q13) A `p` answer is a
    line in a TOML file a person can read, edit and commit; project
    rules travel with the project, the global file is personal.
12. **Titles are automatic.** (Q15) Once threads cross projects, "the
    vendela thread" stops meaning anything; the utility model names it
    after the first turn and `/rename` overrides.
13. **A utility model, open-weight.** (Q17, Q18) Side jobs (titles,
    memory, briefs) run on `utility_profile`. The grilling chose
    `z-ai/glm-5.3-flash` for being the driver's family; measured at step
    9 on TensorX it took 6 to 48 s per title with 37 to 112 reasoning
    tokens, and turning reasoning off had no effect, so the utility
    profile is `deepseek/deepseek-v4.1-flash` (0.8 to 1.6 s, no
    reasoning, good titles). Titles run only when a utility profile is
    set; memory falls back to the thread's profile. Both side jobs are
    capped at 60 s so a slow model cannot hold a thread's mailbox. Anthropic is the reference backend for
    comparison and never on the product path.
14. **Ctrl-C interrupts; no revert.** Codex's Esc-Esc reverts the thread
    to before a turn. The log here is append-only by design, so
    Esc-Esc recalls the text and nothing is undone.
15. **The daemon computes usage.** The status line never counts tokens;
    `Push::Usage` carries what the runtime measured, so a second client
    and `exec --json` see the same number.
16. **One flat threads directory.** The per-project directory was the
    index; with the project in the log it is a lie the moment a thread
    switches.
17. **Light markdown now, highlighting later.** Bold, code and bullets
    are what the model uses in every message; tables and headings are
    rare in a conversation and stay raw.
18. **`ratatui` plus `crossterm`, no `tui-textarea`.** The composer is a
    few hundred lines and owning it keeps the key table in one place.
19. **The client stays a subscriber.** Nothing in `app/` holds state the
    daemon does not; the cells are a projection of pushes, and a second
    client attached to the same thread renders the same transcript.

## 12. Open items

- The classifier (decision 3): when the proposal log holds a few
  hundred answers, try an embedding-similarity proposer run by the
  daemon before the turn, and compare its accuracy against the model's
  on the same log. If it wins, it proposes and the model stops.
- Whether `Explored` folding should also fold consecutive successful
  bash reads (`cat`, `rg`). Start with the file tools only.
- The `p` prefix heuristic (stop at the first argument that looks like
  a value) will be wrong for some commands; the rules file being
  editable is the mitigation until a better rule appears.
- Memory across projects: whether facts should be tagged with the
  project they are about rather than the one the thread is in. Watch
  during step 14.
- Brief and memory quality on the utility model: if DeepSeek V4.1
  Flash writes poor briefs or memory lines, fall back to the thread's
  profile for those only. The Qwen flash models are not candidates on
  TensorX: 40 s and more per title, or no answer within 512 tokens.
- Side jobs run in the actor after a turn, so a post made right then
  waits for them (about a second on the utility model). If that shows,
  move them off the actor.
- Whether `exec --json`'s output is the raw `Push` or a simplified
  event schema like Codex's `ThreadItem`. Raw first; a stable schema
  when a second consumer exists.
- A token ceiling for compaction and inline knowledge (superseded by the
  working-set target in section 13). Both are fractions
  of the window, and GLM 5.3 on TensorX has a 1 048 576-token window, so a
  thread compacts only near 734k tokens and knowledge stays inline up to
  about 315k: correct, but every call then carries a very long prompt.
  Candidate: the lower of the fraction and a fixed ceiling (150k?). Decide
  from the acceptance week's cost and latency.
- The PRD's build-order table still lists the orchestrator as phase 6;
  update it when phase 6 closes, with the renumbering in section 0.

## 13. The working set (to be grilled)

The aim is that a person never manages the context window. Agents hit
it today because forgetting is lossy and permanent; here the log keeps
everything, so the harness can forget freely as long as it can bring
things back. A first read of where tokens go (the first phase 6 thread
on GLM 5.3): the final call's prompt was 12 269 tokens of which 12 224
were cache reads, and one file read was two thirds of the thread. So
the levers, in order of effect:

1. A working-set target (around 100k to 150k tokens) instead of a
   fraction of the window. It replaces section 12's ceiling question.
2. Tool results evicted to a one-line stub with a handle once used:
   `read_file docs/PLAN-phase6.md · 8k tokens · result 14`.
3. `recall`: a tool that returns a stubbed result, a range of the log,
   or a search over it. This is what makes eviction lossless.
4. Summaries of older turns kept continuously on the utility model,
   not one large compaction near the limit.
5. Exploration in a child context (a sub-thread with read tools only)
   returning its conclusion.

The status line then reads `working 48k · thread 310k`, information
rather than a warning. Questions for the grilling: which results are
evicted and when (after the turn that used them, or by age), whether the
stub carries a short summary, how `recall` is scoped across a project
switch, whether the child context is phase 6 or phase 8, and how the
prompt cache survives eviction (a changed early message invalidates
everything after it, so eviction should happen in batches at a
compaction boundary, not message by message).
