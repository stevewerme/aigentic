# Phase 4 plan

Status: draft · Follows `docs/PRD.md` (the Projects phase) and the phase 0 to 3 plans

## 0. Goal and done-when

Phase 3 made the loop safe to hand work to. Phase 4 gives it a place to
work from: a project is a folder of plain files that carries standing
instructions, knowledge and memory across every thread in it, and the
harness reads all of it into a stable prefix. The PRD's condition is the
spine; the rest makes the three layers, the knowledge switch and memory
checkable.

Done when:

1. **Two real projects run from their own folders.** This repository and
   Vendela each have an `aigentic.toml`, a project instructions file, a
   knowledge folder and a memory folder. Starting `aigentic` in either
   directory prints the project's name, the layers it loaded and the
   thread count; `aigentic threads` lists that project's threads; a
   thread started in one never sees the other's files. Daily use for a
   week on both, on both backends.
2. **Layers narrow, never widen.** The prefix is global instructions,
   project instructions, knowledge, memory, pinned facts, skills, in that
   order, byte-stable between turns. A tool or skill the global config
   denies stays denied whatever the project allows; a test asserts it and
   `/project` shows which layer decided each tool.
3. **Knowledge switches on its own.** A knowledge folder under the
   threshold is inlined in the prefix; over it, the prefix holds a short
   index and the model gets a `search_knowledge` tool. A scripted test
   drives both sides of the threshold and asserts the prefix, the tool
   list and a retrieval hit. In Vendela, whose knowledge is over the
   threshold, the model answers a question from a retrieved section.
4. **Memory is extracted after turns and read on the next.** After a turn
   in which the user states a decision, a `memory_extracted` event is
   appended, `memory/decisions.md` gains the line, and the next turn's
   prefix contains it. A fact the model inferred but no participant
   stated is not filed. Editing the file by hand changes the next prefix.
5. **The Pocock setup is configuration.** `[pocock]` fields in
   `aigentic.toml` render the files upstream's skills read
   (`docs/agents/issue-tracker.md`, triage labels, doc location), so
   `code-review` and `triage` run without `/setup-matt-pocock-skills`.

Not in phase 4: participants and per-user permissions (phase 5), the
thread-level tool narrowing the PRD's table mentions (phase 5, with the
turn queue, since both are per-thread state a client sets), embeddings
(decided against in the PRD until misses are measured), the daemon, the
`ratatui` client, skill evals.

## 1. Layout changes

```
aigentic.toml                    the project file: [project], [model], [budget], [compaction],
                                 [tools], [skills], [policy], [[mcp_servers]], [pocock]
.aigentic/                       the project's folders, in the repository, human-edited
  instructions.md                standing instructions (falls back to AGENTS.md, then CLAUDE.md)
  knowledge/                     markdown loaded or indexed into the prefix
  memory/                        decisions.md, constraints.md, facts.md, written by extraction
~/.config/aigentic/
  config.toml                    gains [tools] denied and [global] instructions path
  instructions.md                the global layer: who the agent is, house rules
~/.local/share/aigentic/threads/<project>/   one directory per project name
crates/core/src/event.rs         EventKind gains MemoryExtracted
crates/log/src/payload.rs        MemoryExtractedPayload
crates/runtime/src/
  project.rs                     Project: the file, the folders, the layers (moved from tui)
  layers.rs                      Layers: global, project, thread; narrow-not-widen for tools and skills
  knowledge.rs                   Knowledge: inline or index, the threshold, the search
  memory.rs                      extraction after a turn, the fixed prompt, the file writer
  context.rs                     build_context takes a Prefix, not three options
crates/tools/src/knowledge.rs    search_knowledge: ranked sections over one folder, read class
crates/tui/src/
  project_cmd.rs                 `aigentic project init | setup | show`, `aigentic threads`
  repl.rs                        /project, /threads
docs/PLAN-phase4.md              this file
```

Dependency direction is unchanged. `project.rs` moves from `tui` to
`runtime` because the runtime now reads the project, not only the client;
`tui` keeps the config file and the CLI.

## 2. Signatures (no bodies)

```rust
// crates/core/src/event.rs
pub enum EventKind { /* phase 0-3 kinds */ MemoryExtracted }

// crates/log/src/payload.rs
pub struct MemoryExtractedPayload {
    pub through_seq: u64,             // events considered, inclusive
    pub written: Vec<MemoryLine>,     // what landed, so the log is the audit
    pub model: String,
    pub usage: Usage,
}
pub struct MemoryLine { pub file: String, pub text: String, pub stated_by: Author, pub at_seq: u64 }

// crates/runtime/src/project.rs
pub struct Project {
    pub name: String,                 // [project] name; the threads directory
    pub root: PathBuf,                // where aigentic.toml is
    pub file: ProjectFile,            // the parsed toml, phase 3's struct plus the new sections
    pub instructions: Option<String>, // .aigentic/instructions.md, AGENTS.md or CLAUDE.md
    pub knowledge: Knowledge,
    pub memory: Vec<(String, String)>,// (file name, contents), sorted by name
}
impl Project {
    pub fn open(cwd: &Path) -> Result<Option<Self>, ProjectError>; // walks up to the nearest aigentic.toml
    pub fn reload_memory(&mut self) -> Result<(), ProjectError>;   // after extraction, before the next turn
}
pub struct ProjectFile { /* phase 3 fields */
    pub project: ProjectSection,      // name, description
    pub model: Option<ModelSection>,  // profile: which config.toml profile this project uses
    pub budget: Option<BudgetConfig>, // overrides the profile's
    pub compaction: Option<CompactionConfig>,
    pub tools: ToolsSection,          // allow: Vec<String>; empty means every registered tool
    pub knowledge: KnowledgeSection,  // threshold_fraction (default 0.4), max_hits (default 5)
    pub memory: MemorySection,        // enabled (default true), every_n_turns (default 1)
    pub pocock: Option<PocockSection>,// issue_tracker, triage_labels, docs_dir
}

// crates/runtime/src/layers.rs
pub struct GlobalLayer { pub instructions: Option<String>, pub denied_tools: Vec<String>, pub denied_skills: Vec<String> }
pub struct Layers { pub global: GlobalLayer, pub project: Option<Project> }
impl Layers {
    /// Registry names minus the global denials, then minus what the project does not allow.
    pub fn allowed_tools(&self, registry: &[String]) -> Vec<String>;
    pub fn allowed_skills(&self, enabled: &[String]) -> Vec<String>;
    /// Which layer decided a name, for `/project`.
    pub fn decided_by(&self, name: &str) -> Layer;               // Global | Project | Registry
}

// crates/runtime/src/knowledge.rs
pub struct Knowledge { pub files: Vec<KnowledgeFile>, pub tokens: u64 }
pub struct KnowledgeFile { pub path: PathBuf, pub text: String, pub sections: Vec<Section> }
pub struct Section { pub heading: String, pub text: String, pub line: usize }
pub enum KnowledgeMode { Inline, Index }
impl Knowledge {
    pub fn load(dir: &Path, count: &dyn Fn(&str) -> u64) -> Result<Self, ProjectError>;
    pub fn mode(&self, window: u64, threshold: f32) -> KnowledgeMode;
    pub fn prefix(&self, mode: KnowledgeMode) -> String;         // the files, or one line per file
    pub fn search(&self, query: &str, max_hits: usize) -> Vec<&Section>; // ranked, see section 4
}

// crates/tools/src/knowledge.rs — the tool wraps a snapshot the runtime hands it
pub struct SearchKnowledgeTool { /* sections, max_hits */ }        // name search_knowledge, class Read

// crates/runtime/src/memory.rs
pub const MEMORY_PROMPT: &str;                                     // fixed and versioned
impl Runtime {
    /// After a turn: ask the model for stated facts since the last extraction,
    /// write them, append memory_extracted, reload the prefix.
    pub async fn extract_memory(&mut self, observe) -> Result<Option<MemoryExtractedPayload>, RuntimeError>;
}

// crates/runtime/src/context.rs
pub struct Prefix<'a> { pub global: Option<&'a str>, pub project: Option<&'a str>,
                        pub knowledge: Option<String>, pub memory: Option<String>, pub skills: Option<String> }
pub fn build_context(prefix: &Prefix<'_>, events: &[Event]) -> Result<Vec<Message>, LogError>;

impl Runtime {
    pub fn with_layers(self, layers: Layers) -> Self;              // replaces with_instructions and with_skills' policy role
    pub fn project(&self) -> Option<&Project>;
}
```

Changed phase 0 to 3 signatures: `build_context` takes a `Prefix` struct
(three call sites); `Runtime::with_instructions` is removed in favour of
`with_layers`; `ProjectFile` moves crates and gains sections, all
optional, so every phase 3 `aigentic.toml` still parses. Everything else
is additive.

## 3. The project file and its folders

**One file, one directory.** `aigentic.toml` stays where phase 3 put it
and keeps its name: it is the PRD's `project.toml` in all but name, and
`Cargo.toml` is the precedent for a root file named after the tool
(decision 1). Its folders live under `.aigentic/` in the same repository
so a Next.js or Rust tree is not cluttered with `knowledge/` at the root
(decision 2). `Project::open` walks up from the working directory to the
nearest `aigentic.toml`, so a thread started in a subdirectory still
belongs to the project, and the tools' working directory stays where the
user launched.

**Name.** `[project] name` is required once any phase 4 section is
present; a phase 3 file without it takes the directory name. The name
selects the threads directory and shows in the banner.

**Instructions.** `.aigentic/instructions.md` if present, else `AGENTS.md`,
else `CLAUDE.md`, as phase 0's loader does today. The project layer holds
what this venture is, its conventions and priorities.

**Model, budget, compaction.** `[model] profile = "tensorx"` names a
profile from `config.toml`; `--profile` on the command line still wins.
`[budget]` and `[compaction]` have the phase 3 shapes and override the
profile's values field by field.

**Tools and skills.** `[tools] allow = ["read_file", "bash", "mcp.docs.*"]`
lists what this project's model may see; empty or absent means every
registered tool. `[skills] enabled` is unchanged. Both are narrowed again
by the global layer (section 5).

**Pocock.** `[pocock] issue_tracker = "github" | "gitlab" | "local"`,
`triage_labels = [...]`, `docs_dir = "docs"`. `aigentic project setup`
renders `docs/agents/issue-tracker.md` and the labels file from these,
byte-for-byte what upstream's `setup-matt-pocock-skills` would write for
the same answers, so the vendored skills find what they read. The files
are generated, committed, and regenerated when the fields change.

## 4. Knowledge

**Loading.** Every `*.md` under `.aigentic/knowledge/`, recursively,
sorted by path. Each file is split into sections at headings (the text
before the first heading is a section titled by the file name). Token
count is the provider's `count_tokens` over the whole folder, measured
once at startup and again when a file changes (mtime, checked before each
turn).

**The switch.** `mode` is `Inline` when the folder's tokens are under
`threshold_fraction` (default 0.4) of the model's window, else `Index`.
Inline puts the files in the prefix as one system message per file,
headed by its path. Index puts one line per file (path, first heading,
section count) under a fixed heading that says to use `search_knowledge`,
and registers the tool. The switch is per project and per model window,
so the same project can be inline on a 200k model and indexed on a 32k
one; it is decided at startup and re-decided when the folder changes,
never mid-turn.

**Search.** `search_knowledge { query, max_hits? }` returns up to
`max_hits` sections, each as `path#heading` then the text, ranked by a
simple score: for each query term (lowercased, split on whitespace, stop
words dropped) the count of whole-word matches in the section, weighted
by inverse document frequency over sections, ties broken by file order.
That is BM25's shape without its length normalisation, which the PRD's
"grep or BM25 first" allows and a hundred-section folder does not need.
The tool is `read` class and confined to the snapshot the runtime built;
it never reads the filesystem itself.

**Prefix stability.** The inline files and the index line are part of the
prefix, so a knowledge edit invalidates the cache once and is stable
again, as the PRD says for memory.

## 5. Instruction layering

**Three layers, in order:** the global layer from `~/.config/aigentic/`
(`instructions.md` and `[tools] denied`, `[skills] denied` in
`config.toml`, owner only), the project layer from the project file and
folders, the thread layer as pinned facts. The prefix is global
instructions, project instructions, knowledge, memory, pins, skills,
then the body, each block a system message with a fixed heading.

**Narrow, never widen.** The registry is the universe. The global layer
subtracts its denials; the project layer keeps only what its `allow`
lists, or everything when the list is empty; the result is what the
model's specs contain and what the skill loader's `requires` is checked
against. A project cannot name a globally denied tool into existence: the
subtraction happens first, and `/project` prints every registered tool
with `allowed`, `denied by global` or `not in project allow`. Skills the
same way with `enabled` and `denied`.

**Policy is unchanged.** Narrowing decides what the model sees; policy
decides what runs. A tool outside the allow list is simply not offered,
and a call to it anyway is the unknown-tool refusal from phase 3.

## 6. Memory

**When.** After every `every_n_turns` turns that ended `done` (default 1),
before the prompt returns to the user, and never during a turn. The REPL
prints `[memory: 2 lines written]` or nothing.

**What.** One model call with the project's own model, a fixed prompt
(`MEMORY_PROMPT`, versioned like the summary prompt) and the events since
the last `memory_extracted` event's `through_seq`, projected as the model
would see them. The model returns lines tagged `decision`, `constraint`
or `fact`, each with the seq of the user or agent message that stated it.
The runtime keeps only lines whose `at_seq` points at a `user_message`
or a `skill_loaded` by a user, or an `assistant_message` whose author is
a named agent participant; anything else is dropped as inference, which
is how "only what a participant stated" is enforced rather than hoped
for. In phase 4 the only agent participant is the thread's own agent, so
in practice the filter keeps what the human said.

**Where.** `.aigentic/memory/decisions.md`, `constraints.md`, `facts.md`,
one bullet per line with the date and the thread id in a trailing
comment. A line already present byte-for-byte is not written twice.
Humans edit or delete lines directly; the harness never rewrites a line
it did not just add.

**The event.** `memory_extracted` carries `through_seq`, the lines
written with their sources, the model and usage. It is the audit and the
cursor: the next extraction starts after `through_seq`. `/cost` reports
extraction calls like compactions.

**Prefix.** Memory files are read at startup and after each extraction,
so the next turn's prefix carries the new lines and the cache is
invalidated once.

## 7. Threads per project

`threads_dir/<project name>/` holds a project's logs; a run outside any
project uses `threads_dir/_none/`. `aigentic threads` lists the current
project's threads newest first with id, date, event count and the first
user message's first line; `/threads` does the same in the REPL. The log
gains no field: the directory is the index, and a `project` field on the
first event waits for the daemon in phase 5, when threads can be created
from outside a working directory.

## 8. Configuration

```toml
# aigentic.toml, the project file. Phase 3 sections unchanged.
[project]
name = "vendela"
description = "Bostadsrättsförening admin, Next.js on Vercel"

[model]
profile = "tensorx"            # a profile in config.toml; --profile overrides

[budget]                       # override the profile's, field by field
max_tokens = 3000000

[tools]
allow = ["read_file", "edit_file", "write_file", "list_dir", "grep", "bash"]

[knowledge]
threshold_fraction = 0.4       # of the model's window; over it, index + search_knowledge
max_hits = 5

[memory]
enabled = true
every_n_turns = 1

[pocock]
issue_tracker = "github"
triage_labels = ["bug", "feature", "chore"]
docs_dir = "docs"
```

```toml
# config.toml additions
[global]
instructions = "~/.config/aigentic/instructions.md"   # default; the file is optional

[tools]
denied = ["mcp.*"]             # no project can offer these

[skills]
denied = ["wizard"]
```

CLI: `aigentic project init` writes a minimal `aigentic.toml` with
`[project] name` from the directory and creates `.aigentic/{knowledge,
memory}`; `aigentic project setup` renders the Pocock files; `aigentic
project show` prints the layers and every tool's fate; `aigentic threads`
lists this project's threads. REPL: `/project` and `/threads`.

## 9. Tests

- **Project** (runtime): `open` finds the file from a subdirectory and
  returns `None` outside one; every phase 3 fixture file still parses;
  instructions fall back in order; a name is required once a phase 4
  section is present.
- **Layers** (runtime): global denial beats project allow; empty allow
  means everything; `decided_by` names the right layer; the specs the
  model gets match `allowed_tools`.
- **Knowledge** (runtime, tools): section splitting on a fixture folder;
  the mode on both sides of the threshold with a scripted `count_tokens`;
  the inline prefix is byte-stable; the index lists every file; search
  ranks a two-term query above a one-term hit and drops stop words; the
  tool is `read` class and returns `path#heading` blocks.
- **Memory** (runtime): a scripted extraction reply with one stated
  decision, one inferred fact and one line pointing at a tool result:
  only the decision lands, the file has it once after two extractions,
  the event carries it, the next request's prefix contains it; a
  hand-edited file changes the prefix; `every_n_turns = 2` skips a turn.
- **Threads** (tui): the directory per project; the listing's shape.
- **Pocock** (tui): the rendered files equal a snapshot taken from
  upstream's setup skill for the same answers.
- Phase 0 to 3 tests unchanged apart from `build_context`'s signature and
  `with_instructions`.

No test touches the network. Extraction and knowledge counting use the
scripted provider.

## 10. Steps, one commit each

1. `core, log: memory_extracted` with its payload; projection ignores it;
   `/cost` counts it.
2. `runtime: project file and folders` — `ProjectFile` moves from `tui`
   with the new sections, `Project::open`, instructions fallback, memory
   files read.
3. `runtime: instruction layering` — `Layers`, `Prefix`, `build_context`
   over it, `with_layers`, narrow-not-widen for tools and skills, the
   global layer read by the client.
4. `runtime, tools: knowledge` — loading, sections, the threshold switch,
   `search_knowledge`.
5. `runtime: memory extraction` — the prompt, the call after a turn, the
   stated-by filter, the file writer, reload.
6. `tui: project commands` — `aigentic project init | setup | show`,
   `aigentic threads`, `/project`, `/threads`, threads per project, the
   banner, `[model] profile`.
7. `tui: pocock setup` — the rendered files and their snapshot test.
8. Acceptance: `aigentic project init` here and in Vendela, a week of
   daily use on both backends, done-when 1 to 5 by hand with thread ids
   recorded in the READMEs.

Each step passes `cargo fmt`, `cargo clippy --all-targets -- -D warnings`
and `cargo test` before its commit.

## 11. Decisions

1. **`aigentic.toml` keeps its name and place.** The PRD's `project.toml`
   was a placeholder; renaming a file every user of phase 3 already has
   buys nothing. The PRD's `projects/<name>/` directory becomes the
   repository itself with `.aigentic/` for the folders, which keeps
   "plain files where a human might look" and puts them under the
   project's own git history.
2. **Folders under `.aigentic/`, skills at `./skills`.** Skills stay
   where phase 3 resolves them; knowledge and memory are new and hidden
   from the tree's top level.
3. **Narrowing is set subtraction on the registry, before policy.** Two
   mechanisms that both say "no" would need a rule for which one the
   model sees. Narrowing hides; policy refuses what is visible.
4. **The stated-by filter is structural, not a prompt instruction.** The
   extraction model tags each line with the seq it came from; the runtime
   checks what kind of event and whose author sits at that seq. A model
   that invents a seq points at a tool result or nothing, and the line is
   dropped.
5. **Memory files are append-only from the harness's side.** The harness
   adds lines it did not find; it never edits or removes one. Humans own
   the files. Consolidation is a human's job until phase 6 shows it
   needs a tool.
6. **Search is term scoring, not embeddings.** The PRD defers embeddings
   until measured misses; an IDF-weighted term count over sections is a
   day's work and greppable. The `search_knowledge` result names the
   section, so a miss is visible in the log and can be counted.
7. **The knowledge mode is decided per model window.** The threshold is
   a fraction of the window, as the PRD says for compaction, so switching
   profiles can switch modes; the banner says which mode is active.
8. **Threads are grouped by directory, not by a log field.** The log's
   first event does not know its project today and adding a field that
   the daemon will define differently in phase 5 would be changed, not
   added. A directory per name is enough for one machine.
9. **Thread-level tool narrowing waits.** The PRD's thread layer is
   pinned facts and a temporary narrowing; the narrowing is per-thread
   state a client sets and belongs with the turn queue in phase 5.

## 12. Open items

- Whether the global layer should be able to *add* a tool a project did
  not list (an owner's audit tool, say). The rule is narrow-only in both
  directions for now.
- The Pocock `CONTEXT.md` convention: upstream's skills read that file
  by name; ours is `.aigentic/instructions.md` in the prefix. Either
  `project setup` writes a `CONTEXT.md` pointer or the skills simply do
  not find one. Start with the pointer and see whether it helps.
- `search_knowledge` over `memory/` too, once memory files grow past a
  page.
- Phase 3 leftovers: `skills check` is red until the 33 unenabled
  vendored skills are reviewed or its rule narrows to the enabled set;
  the bundled skills root is the build repository, so a single-binary
  install needs a home for `skills/`; tools accept unknown arguments
  silently.
