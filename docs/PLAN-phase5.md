# Phase 5 plan

Status: parked on 2026-09-22 with steps 1 to 10 landed (the last as `e9c1450`) and the daemon deployed on a VM the same day (Ubuntu 24.04, `aigentic serve` under systemd, two users and two projects, both doctors green, the client banner `as steve (admin)` over an SSH tunnel). Step 11 is deferred: multiplayer acceptance with a second person waits until the single-player experience is solid (v1 first, multiplayer after); its checklist stays in the tui README's phase 5 section. Follows `docs/PRD.md` (the Server and multiplayer phase) and the phase 0 to 4 plans

## 0. Goal and done-when

Phase 4 gave the loop a place to work from. Phase 5 takes the runtime
out of the process: a daemon owns every thread, clients append events
and subscribe to the stream over a socket, and the terminal client is
the first subscriber. That is what makes a second person possible, and
it is also what the orchestrator (phase 6) and the `ratatui` client
(after phase 5) will sit on. The PRD's condition is the spine; the rest
makes the queue, the interrupt, the roles and the attribution checkable.

Done when:

1. **Two people in one thread from two machines, approvals attributed.**
   One daemon on a machine that holds the project checkouts (a Scaleway
   VM or a laptop reachable over SSH); Steve on one machine and a second
   person on another, both connected to it as named users. Both post
   into one thread; each sees the other's message arrive live under its
   author's name; the model addresses them by name. A tool call the
   rules ask about is answered by the person who did not cause it, and
   the `permission_decided` event carries that person's author, visible
   in both terminals and in the log.
2. **Queue and interrupt.** A message posted while a turn runs is in the
   log at once, visible to everyone, and in the model's context from the
   next turn, not before. A message posted as an interrupt cancels the
   in-flight model call within a second, records an `interrupted` event
   naming who and why, and starts a new turn that includes it. A tool
   already running finishes or hits its timeout and its result is
   recorded either way. Killing the daemon mid-turn and restarting it
   resumes the thread as phase 2 did, over the socket.
3. **Permissions are per user per project.** Roles `read`, `write`,
   `approve`, `admin` in the project file. A `read` user's post is
   refused with the reason; a `write` user's decision is refused; an
   `approve` user's decision runs the call. The permission mode is per
   thread, set by an approver, shown to every subscriber. A test holds
   each refusal and the log never gains an event from a refused request.
4. **The terminal client is a subscriber.** `aigentic` with no daemon
   reachable starts one in the process over a Unix socket and behaves as
   phase 4 did: banner, prompts, slash commands, `--thread` resume. The
   phase 3 and 4 acceptance items 1 to 20 in the tui README rerun
   through the socket with the same outcomes. With `--server`, the same
   client talks to a remote daemon and the only differences are latency
   and the other names in the thread.
5. **Threads know their project.** A thread created over the API from a
   machine without a checkout is listed under its project and its first
   event records the project and the root the daemon used. Every
   pre-phase-5 log still opens and replays.

Not in phase 5: the orchestrator's cross-thread tools (`post_task`,
`await_result`, phase 6), the `ratatui` client (after phase 5, once
this API has held for a few weeks), a web client, Postgres (decision
2), TLS inside the daemon (open item; a reverse proxy or an SSH tunnel
carries it), delegated per-user tool credentials (PRD default: service
identity through phase 5), and thread-level tool narrowing beyond pins
(open item since phase 4; it needs the turn queue, which lands here, so
it is the first phase 6 candidate).

## 1. Layout changes

```
crates/api/                      aigentic-api: the wire types, depends on core only
  src/lib.rs                     Frame, Request, Response, Notice; JSON lines; PROTOCOL_VERSION
  src/client.rs                  Client: connect, hello, request/response, subscribe (tokio)
crates/server/                   aigentic-server: the daemon, depends on runtime and api
  src/lib.rs                     Server::serve(listener, ServerConfig); embed() for the in-process case
  src/config.rs                  server.toml: listen, users, projects
  src/session.rs                 one connection: hello, auth, the request loop, the subscription
  src/threads.rs                 ThreadTable: one actor per open thread, lazily started, idle unload
  src/actor.rs                   ThreadActor: mailbox, states, queue, interrupt, approvals
  src/auth.rs                    tokens, roles, what each request needs
crates/runtime/src/
  turn.rs                        continue_turn takes a CancelToken; the horizon rule for mid-turn messages
  seams.rs                       turn_queue_next removed (the actor owns the queue); policy_check parks on Decisions
  decisions.rs                   Decisions: pending approvals and human questions as events plus a channel
  context.rs                     author names on user messages when the thread has more than one human
crates/core/src/event.rs         EventKind gains ThreadStarted
crates/log/src/payload.rs        ThreadStartedPayload, UserMessagePayload.mid_turn, InterruptedPayload.by
crates/policy/src/roles.rs       Role, Participants, what a request needs
crates/tui/src/
  main.rs                        `aigentic serve`; the REPL connects (or embeds) instead of building a Runtime
  client_repl.rs                 the REPL over Client: frames in, lines out; /who, /queue, /interrupt; y/a/n and answers while waiting
  repl.rs                        only the pure parts remain: the command grammar and display truncation
crates/server/src/reports.rs     the text reports (/cost, /project, /policy, /memory, /skills, /who), moved from the tui; `DefaultReports`
  approve.rs                     prompts from awaiting_approval notices; answers are requests
~/.config/aigentic/server.toml   the daemon's config (section 8)
aigentic.toml                    gains [participants]
docs/PLAN-phase5.md              this file
```

Dependency direction: `api` depends on `core` only, so a client in any
language or crate needs nothing else. `server` depends on `runtime` and
`api`. `tui` depends on `api` for the client and on `server` only to
embed a daemon for the single-user case; it no longer constructs a
`Runtime`, which keeps the API honest, as the PRD asks. `runtime` gains
no crate edge. The phase 4 rule that `core` has no async runtime holds:
`api`'s client module is the first thing outside `runtime` and `tui`
that uses `tokio`, and it is feature-gated (`client`) so the types
compile without it.

## 2. Signatures (no bodies)

```rust
// crates/core/src/event.rs
pub enum EventKind { /* phase 0-4 kinds */ ThreadStarted }

// crates/log/src/payload.rs
pub struct ThreadStartedPayload { pub project: Option<String>, pub root: PathBuf, pub created_by: Author }
pub struct UserMessagePayload { pub blocks: Vec<ContentBlock>,
    #[serde(default)] pub mid_turn: bool }          // arrived while a turn ran: in context from the next turn
pub struct InterruptedPayload { pub reason: String,  // "process died" (phase 2) | "interrupt"
    #[serde(default)] pub by: Option<Author> }

// crates/policy/src/roles.rs
pub enum Role { Read, Write, Approve, Admin }         // each includes the ones before it
pub struct Participants(BTreeMap<String, Role>);      // [participants] in aigentic.toml; empty means the owner only
impl Participants {
    pub fn role(&self, user: &str, owner: &str) -> Option<Role>;    // empty table: the owner is admin, nobody else
    pub fn allows(&self, user: &str, owner: &str, needs: Role) -> bool;
}
pub fn needs(request: &Request) -> Option<Role>;   // None for Hello and ListProjects; policy takes an edge to api for this

// crates/api/src/lib.rs — one JSON object per line, both directions
pub const PROTOCOL_VERSION: u32 = 1;
pub struct Frame { pub id: Option<u64>, pub body: Body }          // id present on requests and their responses
pub enum Body { Request(Request), Response(Response), Notice(Notice) }
pub enum Request {
    Hello { protocol: u32, token: String },
    ListProjects,
    ListThreads { project: String },
    CreateThread { project: String },
    Open { thread: Ulid, from_seq: u64 },                            // events since, then a live subscription
    Close { thread: Ulid },
    Post { thread: Ulid, blocks: Vec<ContentBlock>, interrupt: bool },
    InvokeSkill { thread: Ulid, name: String, args: String },
    Decide { thread: Ulid, call_id: String, allow: bool, session: bool },   // session: a standing grant (log's DecisionScope, mirrored so api needs no edge to log)
    AnswerHuman { thread: Ulid, call_id: String, text: String },
    Pin { thread: Ulid, text: String },
    Compact { thread: Ulid },
    SetMode { thread: Ulid, mode: String },
    Report { thread: Ulid, report: ReportKind },                     // Cost | Project | Policy | Memory | Skills
}
pub enum Response {                                                  // internally tagged, so every variant is a struct
    Welcome(Welcome),                                                // Welcome { user, projects, server }
    Projects { projects: Vec<ProjectInfo> }, Threads { threads: Vec<ThreadInfo> }, Thread { thread: ThreadInfo },
    Opened { state: ThreadState, events: Vec<Event> },
    Ok, Text { text: String },
    Refused { reason: String },                                      // a role or a rule said no; nothing was appended
    Error { message: String },
}
pub enum Notice {                                                    // pushed to every subscriber of a thread
    Event { thread: Ulid, event: Event },
    TextDelta { thread: Ulid, text: String },                        // streamed assistant text, not an event
    ToolCallStarted { thread: Ulid, call: ToolCall },
    State { thread: Ulid, state: ThreadState },
}
pub enum ThreadState { Idle, Running { by: Author, queued: u32 },
                       AwaitingApproval { call_id: String, call: ToolCall, class: RiskClass, reason: String },   // the log's request, mirrored
                       AwaitingHuman { call_id: String, question: String } }
pub struct ProjectInfo { pub name: String, pub root: PathBuf, pub role: Option<String>, pub threads: u64 }   // role as its name: no edge to policy
pub struct ThreadInfo { pub id: Ulid, pub project: Option<String>, pub date: String, pub events: u64, pub first_line: String, pub state: ThreadState }

// crates/api/src/client.rs (feature "client")
pub struct Client { /* framed stream, pending responses by id, notice channel */ }
impl Client {
    pub async fn connect(addr: &Addr, token: &str) -> Result<(Self, Welcome), ClientError>;   // Addr::Unix(path) | Addr::Tcp(host, port)
    pub async fn request(&self, request: Request) -> Result<Response, ClientError>;
    pub fn take_notices(&self) -> Option<Receiver<Notice>>;         // once; the client is Clone and every clone shares the connection
}

// crates/runtime/src/decisions.rs
pub struct Decisions { /* pending permission requests and human questions, by call id; one oneshot each */ }
pub enum Pending { Permission { call_id, request: PermissionRequestedPayload }, Human { call_id, question } }
pub enum Answered { Permission { allow: bool, session: bool, by: Author }, Human { text: String, by: Author } }   // `Decided` is taken by layers
impl Decisions {
    pub fn pending(&self) -> Vec<Pending>;
    pub fn decide(&self, call_id: &str, answered: Answered) -> Result<(), DecisionError>;   // NotPending, AlreadyDecided, WrongKind
}
pub struct CancelToken;   // Clone; cancel(by: Author), cancelled_by(), async cancelled() -> Author; never()
impl Runtime {
    /// Replaces `with_approver` for the daemon: an Ask parks the turn on the channel;
    /// `decide` from any approver resumes it. The phase 3 Approver stays for tests and
    /// non-interactive runs (DenyAll).
    pub fn with_decisions(self, decisions: Arc<Decisions>) -> Self;
    /// A turn that can be cancelled: the in-flight model call is dropped, a running tool
    /// is awaited (its timeout bounds it), the rest of its batch gets synthetic results,
    /// `interrupted` is appended with `by`, then `turn_ended` with reason `interrupted`.
    pub async fn run_turn_until(&mut self, author, blocks, cancel: &CancelToken, inbox: &mut Inbox, observe) -> Result<TurnOutcome, RuntimeError>;
    pub async fn continue_turn_until(&mut self, cancel: &CancelToken, inbox: &mut Inbox, observe) -> Result<TurnOutcome, RuntimeError>;
    pub async fn invoke_skill_until(&mut self, author, name, args, cancel, inbox, observe) -> Result<TurnOutcome, RuntimeError>;
}
// The queue's log side lives in the runtime, because the turn owns the log writer: the actor hands a
// `Queued { author, blocks }` to the turn's `Inbox` through its `Outbox`, and the turn appends it as a
// `mid_turn` user message at its next safe point, including while the model streams or a tool runs
// (disjoint field borrows: the stream or the tool borrows the provider or the registry, the log is another field).
pub struct Queued { pub author: Author, pub blocks: Vec<ContentBlock> }
pub fn inbox() -> (Outbox, Inbox);   Inbox::none() for the single-user REPL
pub enum Signal<'a> { /* phase 0-4 */ Waiting(&'a Pending) }   // the turn parked; a client shows what it waits for
// crates/log/src/payload.rs: PermissionDecidedPayload gains `reason: Option<String>` (`interrupted` on the deny an interrupt writes)

// crates/runtime/src/turn.rs — the horizon rule, a projection rule so replay is exact
// A user_message with mid_turn = true is in context only once a turn_ended follows it.

// crates/runtime/src/context.rs
// Message text for a user_message is prefixed `<name>: ` when the log's human authors number more than one.

// crates/server/src/actor.rs
pub struct ThreadActor { /* Runtime, mailbox rx, the shared subscribers, log mirror and state */ }
pub enum Mail { Post { author, blocks, interrupt, reply }, InvokeSkill {..}, Decide {..}, Answer {..}, Pin {..}, Compact {..}, SetMode {..}, Report {..},
                Subscribe { from_seq, notices, reply: (ThreadState, Vec<Event>) } }   // a dropped receiver unsubscribes
pub trait Reports { fn render(&self, runtime: &Runtime, events: &[Event], kind: ReportKind) -> String; }   // step 9 wires the tui's renderers; NoReports until then
impl ThreadActor {
    pub fn new(runtime, torn: Option<u64>, reports: Arc<dyn Reports>) -> Result<(Self, Mailbox), RuntimeError>;   // installs Decisions, runs phase 2's resume
    pub async fn run(self);   // the loop: mail while idle; while a turn runs, Post goes to the inbox (and cancels on interrupt), Decide and Answer
                              // to Decisions, Subscribe is answered from the mirror; Pin, Compact, SetMode, Report and InvokeSkill are refused
                              // with a reason until the turn ends. An answered question or queued messages start the next turn at once.
}
// The observer counts a queued message when its event lands, so a subscriber sees the event before the state that counts it.
// The actor's future is Send: the runtime's observer closures are `FnMut(Signal) + Send` and `Approver: Send + Sync`.

// crates/server/src/serve.rs
pub struct Server { config: Arc<Config>, config_dir, server: Arc<ServerConfig>, threads: Arc<ThreadTable> }
impl Server {
    pub fn new(config: Config, config_dir, server: ServerConfig, providers: Arc<dyn ProviderFactory>, reports: Arc<dyn Reports>) -> Self;
    pub fn from_configs(config, config_dir, server) -> Self;                         // providers from config.toml's profiles
    pub async fn listen(self: Arc<Self>, listener: Listener) -> Result<Bound, ServerError>;   // Unix or Tcp; Bound::port() reports an ephemeral port
    pub async fn serve(self: Arc<Self>, listener: Listener) -> Result<(), ServerError>;    // listen then Bound::serve; sweeps idle threads
    pub async fn embed(config, config_dir, root, user) -> Result<Embedded, ServerError>;   // a private Unix socket and an in-memory token
}
// crates/server/src/build.rs: `build_thread` is what the tui's main did through phase 4 (project, provider, layers, skills, policy, log);
// `ProviderFactory` (default `Profiles`) is the seam tests script. config.toml's types moved here (`config::Config`, `Profile`)
// and `SkillPaths` too, since the daemon builds every thread; the tui re-exports them. `ThreadTable::sweep` unloads a thread only
// when no session has it open, it is idle, and the clock passed; the session checks roles before any mailbox is touched.
```

Changed phase 0 to 4 signatures: `Runtime::continue_turn` becomes a
thin wrapper over `continue_turn_until` with a token that never fires;
`build_context` reads the author names, so its output for a thread with
two humans changes (single-human threads project byte for byte as
before, which a test holds). `with_approver` stays. Everything else is
additive: new event kind, new payload fields with defaults, new crates.

## 3. The daemon and its socket

**One binary.** `aigentic serve` runs the daemon; there is no second
executable. It listens on a Unix socket by default
(`$XDG_RUNTIME_DIR/aigentic.sock`, else
`~/.local/share/aigentic/aigentic.sock`) and on TCP when `server.toml`
or `--listen tcp:0.0.0.0:7420` says so. The daemon needs the project
checkouts on its own disk, because the tools run there; a client needs
nothing but the socket.

**The protocol is JSON lines.** One JSON object per line, requests with
an `id` and their responses carrying it back, notices without. It is
greppable with the same tools as the log, needs no schema compiler, and
a client is a socket plus `serde_json`, which is what keeps a web client
"a weekend". gRPC was the PRD's other option and is decision 1.
`PROTOCOL_VERSION` is sent in `Hello`; a mismatch is refused with both
numbers.

**Sessions.** A connection sends `Hello` first with a token; the daemon
answers `Welcome` with the user's name and the projects it may see, or
closes. Every later request is checked against the user's role in the
thread's project (section 6). A session may open several threads; a
notice names its thread.

**Thread actors.** The daemon holds one `ThreadActor` per open thread, a
tokio task owning that thread's `Runtime` and log writer, started on the
first `Open` or `Post` and unloaded after an idle period with no
subscribers (default 10 minutes; the log is the state, so unloading
loses nothing). The actor is the single writer the PRD's monothreading
means: mail arrives in order, one turn runs at a time, and every
append goes through it. Threads run in parallel with each other; nothing
runs in parallel inside one.

**Subscriptions.** `Open { from_seq }` returns the events since that seq
and subscribes; from then on every appended event, every text delta,
every tool-call start and every state change reaches the session as a
`Notice`. A client that reconnects sends its last seq and misses
nothing. Text deltas are not events (the assistant message is appended
whole at the end of the call, as today), so a subscriber that joins
mid-call sees the tail of the stream and then the full event.

**Providers.** The daemon builds one provider per profile in
`config.toml` at start (keys from its own environment, never from a
client) and each thread uses its project's `[model] profile`; a client's
`/profile` is `SetProfile` in a later step if the acceptance wants it,
and phase 5 leaves it out: the profile is the project's.

**Resume.** Starting the daemon opens nothing; the first `Open` of a
thread runs phase 2's resume (torn tail, synthetic results, the
interrupted note) inside the actor before answering, so a daemon killed
mid-turn recovers exactly as the in-process binary did.

## 4. Authors and attribution

**Users are named in `server.toml`** (`[[users]] name = "magnus"`,
section 8) and the author of everything a session appends is
`Author::User(UserId(name))`. The single-user case keeps the phase 0
behaviour: the embedded daemon has one user, the config's `user` (else
`$USER`).

**The model sees names.** The context projection prefixes a
`user_message`'s text with `<name>: ` when the thread's log holds more
than one distinct human author, counted over the whole log so the
prefix is stable within a thread once a second person has written.
Single-human threads project exactly as before (a test holds the
bytes), so caches and the phase 0 to 4 fixtures are untouched. The
prefix gains one line under the project instructions when the thread
has participants: who is in the thread and their roles, so the model
knows whom it may ask to approve.

**Agents are authors too.** The thread's own agent is
`Author::Agent(AgentId("assistant"))` as today. The orchestrator posting
into a project thread in phase 6 is another `Agent` author and will use
the same `Post`; nothing here special-cases it. Memory extraction's
stated-by rule (phase 4 section 6) accepts a `user_message` by any
`User` author and, from this phase, an `assistant_message` by an `Agent`
author that is not the thread's own, since that is a participant
stating something, not the model inferring it.

**Approvals are attributed** as they already are: `permission_decided`
carries the deciding session's author; the tool result's policy record
points at that event. The client prints `[allowed by magnus]`.

## 5. The turn queue and interrupts

**States.** A thread is `Idle`, `Running`, `AwaitingApproval` or
`AwaitingHuman`, the PRD's diagram with the `ask_human` wait made
explicit. The state is a `Notice` to every subscriber whenever it
changes, and `ThreadInfo` carries it in listings.

**Queue.** A `Post` while `Running` is appended at once as a
`user_message` with `mid_turn = true` (the log is the truth; every
subscriber sees it as an event immediately) and the reply says
`Ok`. The projection's horizon rule keeps it out of the model's context
until a `turn_ended` follows it; when the running turn ends, the actor
starts the next one with everything queued in context, one turn for all
of them. The rule is a function of the log alone, so replay and resume
give the same context the live run had. `Running { queued }` counts
them for the client.

**Interrupt.** A `Post { interrupt: true }` while `Running` appends the
message the same way, then fires the actor's cancel token. The runtime
drops the in-flight model call (partial text is discarded, as a crash
would discard it; nothing partial is appended), kills a tool call that
is already executing — waiting would bound the interrupt by the tool's
own timeout, which `bash`'s `timeout_secs` can raise to 900 s — records
that tool's result as interrupted, appends `interrupted { reason:
"interrupt", by }`, and ends the turn. The actor then starts a new turn
whose context includes the interrupting message and any queued ones. An
interrupt while `AwaitingApproval` or `AwaitingHuman` cancels the wait:
the pending request is decided `deny` with the interrupter as author and
the reason `interrupted`, so the log explains why the call never ran.

**Respond when addressed.** The PRD makes "respond to every message or
only when addressed" a project setting. Phase 5 ships the default the
PRD names and no setting: every `Post` from a `write` user starts or
queues a turn. A message meant for another person and not the model is a
phase 6 question with the orchestrator, when threads gain more than one
agent and addressing matters; the open item records it.

**Budgets** are unchanged and per turn; a queued batch is one turn with
one budget.

## 6. Permissions per user and project

**Roles** are `read` (open, subscribe, reports), `write` (post, invoke a
skill, pin, answer `ask_human`), `approve` (decide permission requests,
set the mode, compact) and `admin` (everything, plus editing the project
file, which happens in git and not over the API in phase 5). Each role
includes the ones before it. They live in the project file:

```toml
[participants]
steve = "admin"
magnus = "approve"
reviewer = "read"
```

An absent section means the daemon's owner (the first user in
`server.toml`) is `admin` and nobody else has a role, so a project is
private until its file says otherwise; the file is in git, so adding a
person is a reviewed change, which is what "plain files where a human
might look" asks for.

**Checks happen before the log.** A request that lacks its role gets
`Refused { reason }` and nothing is appended, so a refused post is not
in anyone's context. The check is a pure function
`auth::needs(&Request) -> Role` against `Participants::allows`, tested
row by row.

**Approvals.** `policy_check`'s `Ask` no longer blocks on a client
callback. The runtime appends `permission_requested` as today and parks
on `Decisions`; the actor publishes `AwaitingApproval` and any session
with `approve` may `Decide`. The first decision wins; a second is
refused as `AlreadyDecided`. The decision is appended with its author,
the session grant is recorded with that author, and the turn resumes.
There is no timeout: a thread can wait for a person overnight, and the
state tells a client who joins what it is waiting for. `ask_human` works
the same way through `AwaitingHuman` and `AnswerHuman` from a `write`
user, then the phase 4 turn split as before.

**The mode** (phase 4 step 10 c) is per thread on the daemon, `Manual`
when the actor starts, set by `SetMode` from an `approve` user, shown in
`State` and the banner to everyone. It is still never an event; a
restarted daemon is `Manual` again.

**The phase 3 `Approver` stays** for the runtime's tests and for a
`DenyAll` non-interactive run; the daemon never installs one.

## 7. The terminal client as a subscriber

**Connect or embed.** `aigentic` looks for the local socket; when nothing
listens it starts the daemon in the process (`Server::embed`, a private
socket in a temp dir, one user) and connects to that, so the phase 4
experience is unchanged and the code path is the one a remote client
uses. `--server unix:/path` or `--server tcp:host:port` connects
elsewhere; the token comes from `--token-env NAME` (default
`AIGENTIC_TOKEN`), never from an argument, never printed.

**The REPL over frames.** The loop reads a line and sends `Post`; it
renders notices as phase 4 rendered signals: text deltas stream, tool
calls print `→ name {args}`, results print truncated under the client's
own `[display]` caps, events from other authors print as
`magnus: <text>`. `AwaitingApproval` shows the inline `y / a / n`
prompt when the client's user may approve and `[waiting for an approver:
bash rm -rf x]` when not; the answer is a `Decide` request, and a
prompt answered elsewhere first is withdrawn with `[decided by magnus]`.
`AwaitingHuman` shows the question the same way.

**New commands.** `/who` lists the thread's participants with roles and
who is connected; `/queue` lists queued messages; `/interrupt <text>`
posts with the flag (`!` as the first character of a line does the
same, since typing a slash command while the model streams is the
common case); `/mode` and the reports go over `SetMode` and `Report`,
so the text the user sees is rendered by the daemon and identical in
every client. `/profile` is dropped in this phase (section 3);
`/verbose` stays client-side.

**Single-user parity** is done-when 4: items 1 to 20 of the tui README
rerun through the socket. The `ratatui` client waits until this API has
held for a few weeks of two-person use.

## 8. Configuration

```toml
# ~/.config/aigentic/server.toml — read by `aigentic serve` and by the embedded daemon
listen = "unix"                       # "unix" | "tcp:0.0.0.0:7420"; the socket path is derived (section 3)
idle_unload_secs = 600

[[users]]
name = "steve"                        # the first user is the daemon's owner
token_env = "AIGENTIC_TOKEN_STEVE"    # the variable holding this user's token, in the daemon's environment

[[users]]
name = "magnus"
token_env = "AIGENTIC_TOKEN_MAGNUS"

[[projects]]
name = "aigentic"                     # must equal [project] name in the checkout's aigentic.toml
root = "/srv/aigentic"

[[projects]]
name = "vendela"
root = "/srv/vendela"
```

Tokens are opaque strings the owner generates (`aigentic serve
--new-token magnus` prints one once and never stores it); the daemon
compares against the environment variable's value in constant time and
never logs a token. Profiles and the global layer stay in `config.toml`
on the daemon's machine; a client's `config.toml` provides only
`[display]`, `user` for the embedded case, and `--server` defaults under
a new `[client] server = "tcp:host:port"`, `token_env`.

```toml
# aigentic.toml additions
[participants]
steve = "admin"
magnus = "approve"
```

CLI: `aigentic serve [--listen ...] [--config ...]`, `aigentic
[--server ...] [--token-env NAME]`, `aigentic threads` and `aigentic
project show` over the API when a server is given, else as today.

## 9. Tests

- **Wire** (api): every `Request`, `Response` and `Notice` round-trips
  through JSON lines; a frame from a newer protocol with an unknown
  variant fails to parse with the version in the error; the client
  matches responses to requests by id out of order.
- **Horizon rule** (log, runtime): a `mid_turn` message between an
  assistant message and its `turn_ended` is absent from the projection
  until the next `turn_ended`, present after it; a phase 4 log projects
  byte for byte as before.
- **Names** (runtime): two human authors give `steve: ` and `magnus: `
  prefixes and the participants line; one human gives neither.
- **Actor** (server): with a scripted provider, a post during a turn
  queues and the next turn's request holds both messages; an interrupt
  during a model call ends the turn with `interrupted { by }` and no
  partial assistant message, and the new turn's context holds the
  interrupting message; an interrupt during a running tool kills it and
  records its result first; an interrupt while awaiting approval denies
  the request with the interrupter as author and the reason `interrupted`.
- **Decisions** (runtime): an `Ask` parks the turn; `decide` from an
  approver resumes it with the right author on `permission_decided`; a
  second decision is `AlreadyDecided`; a session grant recorded through
  the channel is reused on the next identical call.
- **Roles** (policy, server): the request-to-role table, row by row;
  a `read` user's `Post` is `Refused` and the log length is unchanged;
  a `write` user's `Decide` is `Refused`; an absent `[participants]`
  gives the owner `admin` and others nothing.
- **Sessions** (server): `Hello` with a bad token closes; with a good one
  lists only projects where the user has a role; two sessions on one
  thread both receive every notice in seq order; a session that
  reconnects with `from_seq` receives exactly the missed events.
- **Resume** (server): kill the actor's task mid-turn in a test, reopen
  the thread, expect phase 2's synthetic results and the interrupted
  note over the socket.
- **Embed** (tui): the REPL over an embedded daemon runs the phase 3
  scripted-provider REPL tests unchanged (they move behind the client).
- **Thread project** (log, server): `CreateThread` writes
  `thread_started` first and the listing groups by it; a flat
  pre-phase-4 log and a phase 4 directory log both open.
- Phase 0 to 4 tests unchanged apart from `build_context`'s output for
  two-human fixtures and the REPL tests' transport.

No test touches the network beyond a Unix socket in a temp dir; TCP is
tested with a loopback listener on an ephemeral port.

## 10. Steps, one commit each

1. `core, log: thread_started, mid-turn and interrupted-by` — the kind,
   the three payload changes with defaults, the horizon rule in the
   projection, tests on old fixtures.
2. `api: wire types and the client` — `crates/api` with the frames,
   JSON lines codec, `PROTOCOL_VERSION`, and the `client` feature;
   round-trip tests.
3. `policy: roles and participants` — `Role`, `Participants`,
   `[participants]` in `ProjectFile`, `needs(&Request)`; the table
   test.
4. `runtime: decisions channel and cancellable turns` — `Decisions`,
   `with_decisions`, `continue_turn_until` with the cancel token, the
   interrupt semantics for a model call, a running tool and a pending
   request; `turn_queue_next` removed (the actor owns the queue) and
   `seams.rs` says so.
5. `runtime: names in context` — the author prefix and the
   participants line; memory's stated-by rule extended to foreign
   agents.
6. `server: thread actor` — `crates/server` with `ThreadActor`, the
   states, queue and interrupt over the runtime, subscriptions; the
   actor tests with a scripted provider and no socket.
7. `server: sessions over a unix socket` — `ServerConfig`, `Hello`,
   the request loop, roles enforced before the log, `ThreadTable` with
   idle unload, `Server::serve` and `embed`; `aigentic serve`.
8. `server: tcp and tokens` — the TCP listener, `token_env` users,
   constant-time compare, `--new-token`; the loopback test.
9. `tui: the REPL over the client` — `client_repl.rs`, embed-or-connect,
   notices rendered, other authors named, `/who`, `/queue`,
   `/interrupt` and `!`, reports over `Report`; the phase 3 REPL tests
   moved behind the client.
10. `tui: approvals and answers over the API` — the prompt from
    `AwaitingApproval` and `AwaitingHuman`, `Decide` and `AnswerHuman`,
    withdrawal when decided elsewhere, `[allowed by magnus]`.
11. `docs: phase 5 acceptance` — the daemon on the VM, items in the tui
    README: done-when 1 to 5 by hand with thread ids, the README's
    items 1 to 20 rerun through the socket, then the same two days of
    use phase 4 asked for, this time with two people.

Steps 1 to 5 are runtime work testable without a socket and land first;
6 to 8 are the daemon; 9 and 10 the client; 11 the close. Each step
passes `cargo fmt`, `cargo clippy --all-targets -- -D warnings` and
`cargo test` before its commit. `AGENTS.md`'s crate table gains `api`
and `server` in step 2 and 6.

## 11. Decisions

1. **JSON lines, not gRPC.** The PRD allowed either. A line protocol is
   readable with the tools the log is read with, needs no code
   generation, and makes a client a socket plus a JSON parser. If a
   typed schema is wanted later, the `Request` enum is it.
2. **The log stays JSONL; no Postgres in phase 5.** The PRD ties
   Postgres to "more than one machine needs the log". With a daemon,
   only the daemon's machine needs the log; clients hold nothing. Two
   people from two machines is met by one daemon, so Postgres waits for
   a second daemon or a hosted multi-tenant service, both non-goals for
   version 1. The single-writer actor is the same design either way.
3. **One binary, `aigentic serve`.** The PRD's "daemon is a single
   binary" is read literally: the client binary is the daemon binary.
   Fewer artifacts to install on the VM and no version skew between
   client and server on one machine.
4. **The single-user client embeds a daemon.** Rather than keep an
   in-process `Runtime` path in `tui` beside the socket path, there is
   one path. The cost is a Unix socket for a solo user; the gain is that
   every feature is exercised over the API daily.
5. **Queued messages are in the log at once, out of context until the
   next turn.** Appending on arrival keeps the log the truth and shows
   the message to everyone; the horizon rule is a projection rule, so
   replay reproduces the live context exactly. The alternative, holding
   mail in memory until the turn ends, would lose it on a crash.
6. **An interrupt drops the model call and kills a running tool.** A model
   call has no side effects and its partial text is not worth an event;
   a tool's outcome must be recorded, which the PRD says in as many
   words — but waiting would bound the interrupt by the tool's own
   timeout, and `bash`'s can be raised to 900 s. The call is dropped,
   `bash` tears its process group down from the drop, and the result
   event records the interrupt.
7. **Roles are in the project file, tokens in the daemon's config.**
   Who may do what in a project is a project fact and belongs in git
   with the project; who a token belongs to is a deployment fact and
   belongs with the daemon. Neither file holds a secret: tokens are
   environment variables named in the config, like API keys.
8. **No approval timeout.** A pending request waits until a person
   decides or interrupts. A timeout would be a policy decision made by
   the clock; the state notice tells everyone what is waited for.
9. **TLS is the proxy's job in phase 5.** A Caddy or nginx in front, or
   an SSH tunnel, carries TCP; the daemon speaks plain TCP with tokens.
   In-daemon TLS is an open item, taken up when a deployment without a
   proxy is wanted.
10. **The mode is per thread, set by approvers.** It is the same session
    state phase 4 made it, now shared by everyone in the thread, since
    what runs without asking concerns every participant.

## 12. Open items

Inherited from `docs/PLAN-phase4.md` section 12, with what this phase
does to each:

- Thread-level tool narrowing (PRD's thread layer): the queue lands
  here, the narrowing waits for phase 6 with `post_task`, so a task
  event can carry the narrowing it wants.
- `loop-me` between an edit mode and a routine: phase 6 with the
  orchestrator's schedule.
- `git-guardrails` as a default `bash` deny list ahead of the allow
  patterns, overridable per project: not tied to this phase; a small
  policy commit whenever wanted.
- `requires` seeding reads verbs as tools: skills crate, whenever.
- "Answer, but stop" for `ask_human`: `AnswerHuman` could carry
  `continue: bool`; decided when the two-person use shows whether it is
  wanted.
- `search_knowledge` over `memory/`: unchanged.
- Single-binary home for bundled `skills/` and silent unknown tool
  arguments: the VM deployment in step 11 forces the first one, so it
  is handled there (`bundled_dir` in the daemon's config pointing at a
  checkout of this repository is the minimal answer).
- Pre-phase-5 threads have no project in their log: `thread_started`
  fixes it for new threads; old ones keep the directory as their index
  and the flat ones stay unlisted.
- `read_file` preferred over `search_knowledge` for files in the
  repository: count `search_knowledge` calls per thread from the logs
  during step 11's days of use before deciding anything.
- Budget stops from whole-file reads: a project instruction here and in
  Vendela to read ranges, tried during step 11; a `read_file` outline
  mode if instructions do not hold.
- Done-when 1's gap from phase 4 (no real Anthropic task in this
  repository, no second day in Vendela): the first days of step 11.

New in this phase:

- In-daemon TLS (decision 9).
- Postgres, and with it cross-thread search: when a second daemon or a
  hosted service is wanted (decision 2).
- "Respond when addressed" as a project setting: phase 6, when a thread
  can hold more than one agent.
- Per-user delegated credentials for tools: PRD default is a service
  identity; unchanged.
- `SetProfile` over the API: left out; the profile is the project's.
  Revisit if two people want different backends in one thread, which
  would also make the model label per turn.
- Idle unload versus a thread whose turn waits for approval overnight:
  the actor must not unload while `AwaitingApproval`; step 7 must test
  it.
- The `ratatui` client and a web client: after this API has held.

From steps 10 and 11, before the acceptance run:

- `ask_human`'s answering author was dropped: the harness tool kept
  only the text of `Answered::Human`, and the tool result was authored
  `System`, so the log did not say who answered and a client could
  print only `[answered elsewhere]`. Fixed after step 11 under section
  4's attribution rule: an answered `ask_human`'s `tool_result` event
  is authored by the person who answered (the phase 3 `Approver`'s
  author in the single-process case), every other tool result stays
  the system's, and the client prints `[answered by magnus]`. No wire
  or payload change; old logs read as before.
- `--profile` was accepted and ignored on the REPL path from step 9:
  `Server::embed` built the thread from the project's `[model] profile`
  and the flag reached only `project show`. Fixed after step 11: the
  flag is `embed`'s profile, which `ThreadTable` passes to
  `build_thread` as its override; with `--server` it is refused, since
  a served daemon picks each thread's profile from its project
  (section 3). `embed` gains the parameter; section 2's signature is
  out of date by that one argument.
- `aigentic threads` and `project show` read local files and ignored
  `--server`, so section 8's "over the API when a server is given" was
  not true until after step 11. Now with `--server` they go over
  `ListThreads` and `Report { Project }`; the report is rendered from
  the project's newest thread, since the daemon renders reports from a
  thread's runtime and the project report is the same for every thread
  of a project, and a project with no thread yet says so. Without
  `--server` both read local files as before.
- `echo` is on the default bash allow list (phase 3 section 4), so an
  acceptance item that wants a prompt must pick something off it.
- A `[participants]` table that names anyone gives the owner no role
  unless listed, by section 6's rule; the VM's project files must list
  the owner. `aigentic doctor` has a `participants` line for it since
  after step 11.
