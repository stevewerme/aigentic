//! The client's engine (phase 5 step 9, moved under `app/` in phase 6
//! step 3): lines in, requests out, notices rendered as they arrive.
//! The same code path whether the daemon is embedded for one user or
//! remote for many. Rendering goes through a `Printer`: the ratatui
//! shell commits each line to the terminal's scrollback and shows the
//! streaming tail, plain stdout does the same without a terminal, and
//! a vector stands in for tests. Approvals and answers (step 10) are requests like
//! any other: a permission request or an `ask_human` question becomes a
//! selectable menu (`menu.rs`) when this user's role may answer and says
//! who it waits for when not; a prompt answered on
//! another connection first is withdrawn with who decided it.

use std::collections::{HashMap, HashSet, VecDeque};

use aigentic_api::client::Client;
use aigentic_api::{Notice, ReportKind, Request, Response, SwitchReply, ThreadInfo, ThreadState};
use aigentic_runtime::aigentic_core::{Author, ContentBlock, Event, EventKind, ToolCall};
use aigentic_runtime::aigentic_log::{
    AssistantMessagePayload, CheckpointAnsweredPayload, CheckpointAskedPayload, CompactedPayload,
    CompactionStrategy, DecisionAnswer, DecisionAnsweredPayload, DecisionKind,
    DecisionProposedPayload, DecisionScope, InterruptedPayload, MemoryExtractedPayload, MemoryHome,
    MemoryRememberedPayload, PermissionDecidedPayload, PolicyRecord, RunFinishedPayload,
    SkillLoadedPayload, ToolResultPayload, TurnEndedPayload, Usage, UserMessagePayload,
};
use aigentic_runtime::harness_tools::{TaskState, open_checklist};
use aigentic_runtime::{ASKED_HUMAN, INTERRUPTED, LENGTH_STOP, NOT_RUN_OVER_LIMIT, NOT_RUN_SOLO};

use crate::app::cells::full_command;
use tokio::sync::mpsc;
use ulid::Ulid;

use crate::app::cells::{CallLine, Cell, ToolDetail, ToolState, summarise_args};
use crate::app::commands::{Command, HELP, parse_line, truncate_for_display};
use crate::app::copy::Used;
use crate::app::look::View;
use crate::app::menu::{Keyed, Kind, Menu, Pick};
use crate::front;

/// What a key on the prompt menu came to, from `ClientRepl::menu_key`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuKey {
    /// Not a menu key; the caller carries on.
    Passed,
    /// Handled: the selection moved, or the decision went out.
    Used,
    /// The composer should take the prompt's text (a deny's reason, a
    /// question's free text); the draft it held comes back after.
    Text,
}

/// A running turn's figures, from the provider's reported usage on the
/// events, never counted here (plan step 8b).
#[derive(Debug, Clone)]
pub struct TurnStats {
    pub started: std::time::Instant,
    pub tools: u32,
    /// Output across the turn's calls, reasoning included: the turn's
    /// cost in tokens, shown by `/cost`.
    pub output: u64,
    /// How many usage-bearing `AssistantMessage`s the turn has drawn,
    /// 1-based (issue #115): the ordinal a call line shows.
    calls: u32,
    /// Dollars the turn's calls were stamped with, summed by the
    /// `classify_cost` rule (issue #110): only a call that is not
    /// estimated and carries a `cost_usd` adds here.
    spent: f64,
    /// The turn's calls that were priced, and those that were not
    /// (estimated, or with no stamp).
    priced: u32,
    unpriced: u32,
    /// Prompt tokens across the turn: input, cache read and cache
    /// write.
    prompt: u64,
    /// Cache reads across the turn, the numerator of the cache share.
    cached: u64,
    /// Whether any call reported a cache read or write at all, so a
    /// backend that has no caching stays quiet instead of showing `0%`.
    cache_seen: bool,
    /// The running tool, `name argument`.
    pub current: Option<String>,
    /// Text is streaming.
    pub writing: bool,
    /// The retry the call is waiting on (issue #31): attempt, retries,
    /// reason and the instant the wait ends (issue #90). Set when
    /// `provider_retried` arrives, so the turn line counts down
    /// `retrying 2/3 · tensorx · not answering · next in 12s` while the
    /// backoff runs, and says `trying now` once the attempt is in
    /// flight, instead of a bare `thinking`. Cleared by the first
    /// content.
    pub retry: Option<RetryWait>,
}

/// The retry the call is waiting on (issue #31): the attempt, the most
/// the call could make, why, and when the wait ends (issue #90).
#[derive(Debug, Clone)]
pub struct RetryWait {
    /// Which attempt is waiting, 1-based.
    pub attempt: u32,
    /// The most retries the call could make.
    pub retries: u32,
    pub reason: String,
    /// When the backoff ends and the next attempt starts. The turn line
    /// counts down to it, and reads `trying now` once it has passed.
    pub until: std::time::Instant,
    /// The backoff as the event held it, for the `System` line (issue
    /// #115), so its `in 2.0s` need not be re-derived from `until`.
    pub wait_ms: u64,
}

impl RetryWait {
    /// The activity row's text (issue #31), countdown unchanged (issue
    /// #90): `retrying 1/3 · overloaded · next in 4s`.
    pub fn activity(&self, countdown: &str) -> String {
        format!(
            "retrying {}/{} · {} · {countdown}",
            self.attempt, self.retries, self.reason
        )
    }

    /// The developer view's `System` line (issue #115): `retry 1/3 in
    /// 2.0s: overloaded`. One helper renders both this and the activity
    /// row, so the two never drift.
    pub fn system(&self) -> String {
        format!(
            "retry {}/{} in {}: {}",
            self.attempt,
            self.retries,
            crate::app::cells::secs_ms(self.wait_ms),
            self.reason
        )
    }
}

/// `next in 12s` while the backoff runs, and `trying now` once it has
/// passed and the attempt is in flight (issue #90). `None` is an instant
/// already behind us.
fn retry_countdown(left: Option<std::time::Duration>) -> String {
    match left {
        Some(left) if !left.is_zero() => {
            format!("next in {}", crate::app::status::elapsed_short(left))
        }
        _ => "trying now".into(),
    }
}

impl TurnStats {
    /// A turn in a chosen state, for the tests that script a phase
    /// without a daemon behind them.
    #[cfg(test)]
    pub fn for_test(writing: bool, current: Option<&str>) -> Self {
        Self {
            writing,
            current: current.map(str::to_owned),
            ..Self::new()
        }
    }

    fn new() -> Self {
        Self {
            started: std::time::Instant::now(),
            tools: 0,
            output: 0,
            calls: 0,
            spent: 0.0,
            priced: 0,
            unpriced: 0,
            prompt: 0,
            cached: 0,
            cache_seen: false,
            current: None,
            writing: false,
            retry: None,
        }
    }

    /// Fold one call's usage into the turn (issue #110). The pricing
    /// rule is `stats::classify_cost`'s: an estimated call, or one the
    /// runtime stamped no price on, adds no dollars and counts as
    /// unpriced, so this line and `/cost` agree on the same calls.
    fn add_usage(&mut self, u: &aigentic_runtime::aigentic_log::Usage) {
        self.output += u.output_tokens;
        self.prompt += u.input_tokens + u.cache_read_tokens + u.cache_write_tokens;
        self.cached += u.cache_read_tokens;
        if u.cache_read_tokens > 0 || u.cache_write_tokens > 0 {
            self.cache_seen = true;
        }
        if !u.estimated
            && let Some(usd) = u.cost_usd
        {
            self.spent += usd;
            self.priced += 1;
        } else {
            self.unpriced += 1;
        }
    }

    /// Fold one call's usage in and say which call of the turn it was,
    /// 1-based (issue #115): the ordinal the call line shows.
    fn record_usage(&mut self, u: &aigentic_runtime::aigentic_log::Usage) -> u32 {
        self.add_usage(u);
        self.calls += 1;
        self.calls
    }

    /// `1m 12s · 4 tools`: how long the turn has run and how many
    /// calls it took. Sizes live in the footer, and the running line
    /// carries state and time only, so it stays one thing (issue #21).
    /// The finished summary appends the turn's own cost on top of this
    /// (`summary`, issue #110): §15 of the phase 6 plan puts the
    /// message's dollars on that line, which reverses #21's "costs in
    /// `/cost` … this line says neither" for the summary alone.
    pub fn figures(&self) -> String {
        let mut parts = vec![crate::app::status::elapsed_short(self.started.elapsed())];
        match self.tools {
            0 => {}
            1 => parts.push("1 tool".into()),
            n => parts.push(format!("{n} tools")),
        }
        parts.join(" · ")
    }

    /// The finished turn's summary (issue #110): `figures()` plus what
    /// the message cost, how many of its calls went unpriced, and how
    /// much of its prompt was cached. Each part appears only when it
    /// has something to say, so a thread whose calls carry no price
    /// reads exactly as it did before. The live line keeps `figures()`,
    /// so a running turn still shows only state and time.
    pub fn summary(&self) -> String {
        let mut out = self.figures();
        if self.priced > 0 {
            out.push_str(&format!(
                " · {}",
                crate::stats::money(Some(self.spent), None)
            ));
        }
        if self.unpriced > 0 {
            out.push_str(&format!(" · {} unpriced", self.unpriced));
        }
        // `stats`' cache share over every call, estimated included; a
        // backend that reports no cache fields drops the part rather
        // than showing `0%` every turn.
        if self.prompt > 0 && self.cache_seen {
            let share = 100.0 * self.cached as f64 / self.prompt as f64;
            out.push_str(&format!(" · {share:.0}% cached"));
        }
        out
    }

    /// What the turn is doing now — the verb only. The in-flight row
    /// above names the call; repeating its command here was noise.
    pub fn activity(&self) -> String {
        match (&self.current, self.writing) {
            (Some(_), _) => "running".into(),
            // A retry outranks "thinking": the call is not thinking, it
            // is waiting on a provider that has not answered (issue #31),
            // and the line counts the wait down (issue #90).
            (None, _) if self.retry.is_some() => {
                let r = self.retry.as_ref().expect("checked");
                let until = r.until.checked_duration_since(std::time::Instant::now());
                r.activity(&retry_countdown(until))
            }
            (None, true) => "writing".into(),
            (None, false) => "thinking".into(),
        }
    }
}

/// Where rendered lines go.
pub trait Printer {
    /// A finished line: committed, never redrawn.
    fn line(&mut self, text: &str);
    /// The assistant's streamed text not yet ended by a newline, after
    /// each delta; empty once flushed. A shell redraws it in place.
    fn tail(&mut self, _text: &str) {}
    /// A cell; `done` false means it still changes (a running tool). The
    /// default prints its head while running and the whole cell when
    /// done, which is what a pipe wants.
    /// The thread waits on this user: a shell draws the menu, a pipe
    /// prints the lines.
    fn prompt(&mut self, menu: &Menu) {
        for l in menu.plain() {
            self.line(&l);
        }
    }
    /// Housekeeping (a title, memory written): a pipe prints it, a shell
    /// keeps it out of the transcript.
    fn quiet(&mut self, text: &str) {
        self.line(text);
    }
    /// A long text to page through (`/diff`); a pipe prints it.
    fn pager(&mut self, _title: &str, text: &str) {
        for l in text.lines() {
            self.line(l);
        }
    }
    /// Put `text` on the system clipboard (`/copy`, issue #41). The
    /// default refuses: plain stdout would have to put an OSC 52 escape
    /// on the same stream as the transcript, so a pipe that does not
    /// support it gets a message instead.
    fn copy(&mut self, _text: &str) -> Result<Used, String> {
        Err("/copy needs the shell UI".into())
    }
    /// The REPL moved to another thread (`/new`, issue #89). A shell
    /// drops the live state that belonged to the old one; a pipe holds
    /// none, so the default does nothing.
    fn thread_changed(&mut self) {}
    /// The transcript view the printer wants (issue #115). The default is
    /// `Normal`: `Stdout`, `Lines` and `Copies` print HEAD's rows, so a
    /// pipe, `exec` and every scripted-turn test is unchanged. The shell's
    /// printer returns `Dev` unless `/view normal` is in force.
    fn view(&self) -> View {
        View::Normal
    }
    /// A switch the shell acts on. A pipe has no view to switch, so the
    /// default ignores it.
    fn set_view(&mut self, _view: View) {}
    fn cell(&mut self, cell: Cell, done: bool) {
        let lines = cell.plain();
        if done {
            for l in &lines {
                self.line(l);
            }
        } else if let Some(head) = lines.first() {
            self.line(head);
        }
    }
}

/// Whether the printer asks for the developer view's cells (issue #115):
/// the call line, the step headers, the system lines, and the tool
/// detail that rides on an existing head line. The shell's printer does;
/// `Stdout`, `Lines` and `Copies` do not.
fn detailed(printer: &dyn Printer) -> bool {
    printer.view().detailed()
}

/// Emit a system line (issue #115): what the harness did on its own.
/// The developer view only, so a `Normal` printer never sees the cell.
fn system_cell(out: &mut dyn Printer, line: String) {
    if detailed(out) {
        out.cell(Cell::System(line), true);
    }
}

/// A vector, for tests. Field 1 asks for the developer view's cells
/// (issue #115); `Default` is off, so every scripted turn that builds
/// `Lines::default()` prints HEAD's rows.
#[cfg(test)]
#[derive(Default)]
pub struct Lines(pub Vec<String>, pub bool);

#[cfg(test)]
impl Lines {
    /// A `Lines` that asks for the developer view, as the shell does.
    pub fn detail_on() -> Self {
        Lines(Vec::new(), true)
    }
}

#[cfg(test)]
impl Printer for Lines {
    fn line(&mut self, text: &str) {
        self.0.push(text.to_owned());
    }

    fn view(&self) -> View {
        if self.1 { View::Dev } else { View::Normal }
    }
}

/// Plain lines plus the texts handed to the clipboard, for `/copy`
/// (issue #41). The lines are `Copies::0`, the clipboard `Copies::1`.
#[cfg(test)]
#[derive(Default)]
pub struct Copies(pub Lines, pub Vec<String>);

#[cfg(test)]
impl Printer for Copies {
    fn line(&mut self, text: &str) {
        self.0.line(text);
    }

    fn copy(&mut self, text: &str) -> Result<Used, String> {
        self.1.push(text.to_owned());
        Ok(Used::Child)
    }
}

/// What the client knows about its thread.
/// The profile, model and effort a daemon names at attach (issue
/// #43): `None` profile or effort when the config sets neither.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Identity {
    pub profile: Option<String>,
    pub model: String,
    pub effort: Option<String>,
}

impl Identity {
    /// The head of the status line: `flash (deepseek-v4.1-flash)`, with
    /// `effort <n>` after the model when the profile sets one. Nothing
    /// when the model is not known.
    pub fn label(&self) -> Option<String> {
        if self.model.is_empty() || self.model == "unknown" {
            return None;
        }
        let mut who = match &self.profile {
            Some(p) => format!("{p} ({})", self.model),
            None => self.model.clone(),
        };
        if let Some(e) = &self.effort {
            who.push_str(&format!(" · effort {e}"));
        }
        Some(who)
    }
}

/// The run this REPL follows (issue #68): its lead, the issue it is for,
/// and the last `seq` drawn. `printed` is the boundary between the
/// backlog and the live notices: both sources advance it, and both skip
/// `seq <= printed`, so a lead event appended between the two is drawn
/// once (copy of `build_cmd`'s rule).
#[derive(Debug, Clone)]
pub struct Followed {
    pub lead: Ulid,
    pub issue: u64,
    pub printed: u64,
}

/// A followed run's open checkpoint. Leaving it waiting hides the prompt
/// but keeps the gate, so a later `/answer` still reaches it.
pub struct OpenGate {
    lead: Ulid,
    gate: String,
    menu: Menu,
    /// Whether the prompt is drawn and takes keys.
    shown: bool,
    /// `Continue with changes` was picked: the next plain line is the
    /// amendment.
    amending: bool,
}

pub struct ClientRepl {
    client: Client,
    thread: Ulid,
    user: String,
    /// The user's role in the project, from `Welcome`.
    role: Option<String>,
    skills: Vec<String>,
    /// Running calls by id: name, the row's text, and the pager's when
    /// they differ, for the result's cell.
    calls: HashMap<String, (String, String, Option<String>)>,
    state: ThreadState,
    mode: String,
    /// The profile, model and effort the daemon named at attach
    /// (issue #43), for the status line's head.
    identity: Identity,
    /// Streamed assistant text not yet ended by a newline.
    partial: String,
    /// The assistant's text since the last prompt this client posted
    /// (issue #41), the source of `/copy`: unlike `partial` it survives a
    /// newline, so a whole reply's fences are intact.
    reply: String,
    /// Whether streamed text has arrived since the last
    /// `AssistantMessage` or turn start (issue #114). `partial` cannot
    /// serve: the engine flushes it at a streamed tool call, so a reply
    /// cut behind a tool call leaves it empty while its text is still on
    /// screen.
    streamed_text: bool,
    /// The length of `reply` when the current call's text stream began
    /// (issue #114), so a retried cut truncates `/copy` back past exactly
    /// the discarded attempt and no further.
    reply_stream_start: usize,
    /// The call id of the request or question this client prompted for
    /// and has not answered: a decision from elsewhere withdraws it.
    prompted: Option<String>,
    /// The prompt menu while `prompted`.
    menu: Option<Menu>,
    /// The running turn's figures; `None` while idle.
    turn: Option<TurnStats>,
    /// The project the thread moved to, once it has; the shell's status
    /// line shows it over the one it started in.
    project: Option<String>,
    /// The thread's title, once one is recorded while this client is on.
    title: Option<String>,
    /// The model's checklist from its last `update_tasks`, until it is all
    /// done or the turn ends.
    tasks: Vec<aigentic_runtime::harness_tools::Task>,
    /// The `update_tasks` call ids, whose results draw nothing.
    task_calls: std::collections::HashSet<String>,
    /// The text of the last step header committed this turn (issue
    /// #115), so an identical list re-emitting prints no header, and
    /// `None` when no step is open. Cleared at turn start.
    step_header: Option<String>,
    quit: bool,
    /// The project this REPL started in (issue #68): `/build` names it,
    /// and its run is opened through it.
    home_project: String,
    /// The run this REPL follows (issue #68), if any.
    following: Option<Followed>,
    /// The followed run's open checkpoint, apart from `menu`: a chat
    /// state change clearing `menu` never withdraws it, and a chat
    /// permission prompt comes first when both are up.
    checkpoint: Option<OpenGate>,
    /// The last `Notice::Usage`: the working and thread figures for the
    /// status line (phase 6 step 3, issue #99).
    usage: Option<crate::app::status::Figures>,
    /// The turn that ran last (issue #21): tokens written and calls,
    /// shown by `/cost` — the live line carries state and time only.
    last_turn: Option<(u64, u32)>,
    /// The last turn's raw stop reason when it ended on a provider
    /// failure (issue #22): the machine text kept off the screen, shown
    /// by `/why`. Cleared by a turn that ended in the ordinary way.
    last_stop: Option<String>,
    /// The open turn's own events, for its turn-end report (issue #113).
    /// Cleared at each turn start and at its `turn_ended`.
    turn_events: TurnEvents,
    /// A post went out while idle and its turn has not been seen
    /// running yet; input at its end waits for that turn.
    awaiting_turn: bool,
    /// The `/new` question while a turn is open (issue #108), in a slot
    /// of its own: a daemon prompt (approval, question, switch) sets
    /// `menu` the moment it arrives and would overwrite it. Answered
    /// here, never sent to the daemon.
    confirm_new: Option<Menu>,
}

impl ClientRepl {
    /// The step header to draw, if one is due (issue #115): the active
    /// checklist step's text, when it differs from the last committed
    /// header's, or nothing when no step is active any more (the group
    /// closes). The text is compared, never the index, so a re-emitted
    /// identical list does not re-header and renumbering alone does not.
    /// Decided at `ToolCallStarted` from the `update_tasks` args, like
    /// the `Done` lines, so it prints even if that call's result fails.
    pub(crate) fn step_header(&mut self, out: &mut dyn Printer) {
        let active = self.tasks.iter().position(|t| t.state == TaskState::Active);
        match active {
            Some(index) => {
                let text = self.tasks[index].text.clone();
                if self.step_header.as_deref() == Some(text.as_str()) {
                    return;
                }
                self.step_header = Some(text.clone());
                if detailed(out) {
                    out.cell(
                        Cell::Step {
                            index: index + 1,
                            total: self.tasks.len(),
                            text,
                        },
                        true,
                    );
                }
            }
            None => {
                self.step_header = None;
            }
        }
    }

    // One argument per thing the REPL needs to know at birth: the wire,
    // the chat thread, who is talking, what they may do, what to show,
    // what to remember, how to count money, and the project it started
    // in (issue #68). Bundling them would only move the same list.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        client: Client,
        thread: Ulid,
        user: &str,
        role: Option<String>,
        state: ThreadState,
        mode: String,
        identity: Identity,
        project: &str,
    ) -> Self {
        Self {
            client,
            thread,
            user: user.to_owned(),
            role,
            skills: Vec::new(),
            calls: HashMap::new(),
            state,
            mode,
            identity,
            partial: String::new(),
            reply: String::new(),
            streamed_text: false,
            reply_stream_start: 0,
            prompted: None,
            menu: None,
            turn: None,
            project: None,
            title: None,
            tasks: Vec::new(),
            task_calls: std::collections::HashSet::new(),
            step_header: None,
            usage: None,
            last_turn: None,
            last_stop: None,
            turn_events: TurnEvents::default(),
            awaiting_turn: false,
            confirm_new: None,
            quit: false,
            home_project: project.to_owned(),
            following: None,
            checkpoint: None,
        }
    }

    /// The user-invoked skills, so `/<skill>` dispatches.
    pub fn with_skills(mut self, skills: Vec<String>) -> Self {
        self.skills = skills;
        self
    }

    /// The whole part of the thread's name the status line shows
    /// (issue #89): set at birth, so a resumed thread shows its title
    /// from the first frame.
    pub fn with_title(mut self, title: Option<String>) -> Self {
        self.title = title;
        self
    }

    fn may_approve(&self) -> bool {
        matches!(self.role.as_deref(), Some("approve" | "admin"))
    }

    fn may_write(&self) -> bool {
        matches!(self.role.as_deref(), Some("write" | "approve" | "admin"))
    }

    /// The loop: lines from `input` and notices from the client until
    /// `/quit`, end of input, or the daemon going away.
    pub async fn run(
        &mut self,
        mut input: mpsc::UnboundedReceiver<String>,
        mut notices: mpsc::Receiver<Notice>,
        out: &mut dyn Printer,
    ) {
        // A thread opened while it waits: the prompt is shown at once.
        let state = self.state.clone();
        self.show_state(&state, out);
        // Input at its end (a pipe closed): the turn it started still
        // finishes before the loop does.
        let mut closed = false;
        while !self.quit {
            if closed && !self.awaiting_turn && matches!(self.state, ThreadState::Idle) {
                break;
            }
            tokio::select! {
                line = input.recv(), if !closed => match line {
                    None => closed = true,
                    Some(line) => self.handle_line(&line, out).await,
                },
                notice = notices.recv() => match notice {
                    None => {
                        out.line("[the daemon closed the connection]");
                        break;
                    }
                    Some(n) => self.render(n, out),
                },
            }
        }
        self.flush_partial(out);
    }

    async fn request(&self, request: Request) -> Response {
        match self.client.request(request).await {
            Ok(r) => r,
            Err(e) => Response::Error {
                message: e.to_string(),
            },
        }
    }

    /// Print a response that is not an event: text, or why not.
    fn show(&self, response: Response, ok: &str, out: &mut dyn Printer) {
        match response {
            Response::Ok => {
                if !ok.is_empty() {
                    out.line(ok);
                }
            }
            Response::Text { text } => {
                for l in text.lines() {
                    out.line(l);
                }
            }
            Response::Refused { reason } => out.line(&format!("[refused: {reason}]")),
            Response::Error { message } => out.line(&format!("[error: {message}]")),
            Response::Threads { threads } => {
                for l in render_thread_infos(&threads).lines() {
                    out.line(l);
                }
            }
            other => out.line(&format!("[{other:?}]")),
        }
    }

    pub async fn handle_line(&mut self, line: &str, out: &mut dyn Printer) {
        // `!text` interrupts, as typing a command while the model streams
        // is the common case.
        if let Some(text) = line.strip_prefix('!') {
            let text = text.trim();
            if !text.is_empty() {
                self.post(text, true, out).await;
            }
            return;
        }
        // The `/new` question (issue #108) is answered first, before the
        // followed run's checkpoint and the chat prompt: it is the one
        // on screen, and its keys are its own. `!` and `/` lines are
        // exempt, so `!stop it` and `/new` keep working with it up.
        if !line.trim().starts_with('/')
            && let Some(keyed) = self.confirm_new.as_ref().and_then(|m| m.line(line))
        {
            self.apply_confirm(keyed, out).await;
            return;
        }
        // A followed run's checkpoint takes a typed line first (issue #68):
        // plain mode has no keys, so `1`/`stop` and `2`/`wait` are the
        // prompt's answers there. A `/` line still reaches the parser,
        // so `/build` and `/help` keep working with the prompt up. Only
        // while it is the prompt on screen, though: with the chat's
        // prompt up, a typed `1` is the chat's answer (#68's review).
        if self.prompted.is_none()
            && !line.trim_start().starts_with('/')
            && !line.trim().is_empty()
            && self.checkpoint.as_ref().is_some_and(|open| open.amending)
        {
            self.send_answer(
                aigentic_api::CheckpointAnswer::Amend,
                Some(line.trim().to_owned()),
                out,
            )
            .await;
            return;
        }
        if self.prompted.is_none()
            && !line.trim_start().starts_with('/')
            && let Some(keyed) = self
                .checkpoint
                .as_ref()
                .filter(|open| open.shown)
                .and_then(|open| open.menu.line(line))
        {
            self.apply_checkpoint(keyed, out).await;
            return;
        }
        // A pending question, request or switch proposal this client
        // prompted for takes the line first; without the role the line
        // is what it is. A switch proposal goes the same route a
        // question does (issue #82): `self.menu.line(line)`.
        match &self.state {
            ThreadState::AwaitingHuman { call_id, .. }
            | ThreadState::AwaitingSwitch { call_id, .. }
                if self.prompted.as_deref() == Some(call_id) && !line.trim().starts_with('/') =>
            {
                if let Some(keyed) = self.menu.as_ref().and_then(|m| m.line(line)) {
                    self.apply(keyed, out).await;
                    return;
                }
            }
            ThreadState::AwaitingApproval { call_id, .. }
                if self.prompted.as_deref() == Some(call_id) =>
            {
                if let Some(Keyed::Decide { pick, reason, echo }) =
                    self.menu.as_ref().and_then(|m| m.line(line))
                {
                    out.line(&echo);
                    match pick {
                        Pick::Allow { session, prefix } => {
                            self.decide(true, session, prefix, reason, out).await;
                        }
                        Pick::Deny => {
                            self.decide(false, false, None, reason, out).await;
                        }
                        // A checkpoint pick never reaches here (its own prompt
                        // handles it): issue #68. Nor a switch pick
                        // (issue #82), nor the `/new` question's two
                        // (#108): those are the checkpoint's, the
                        // switch's and `apply_confirm`'s.
                        Pick::Answer
                        | Pick::Other
                        | Pick::ContinueRun
                        | Pick::AmendRun
                        | Pick::StopRun
                        | Pick::LeaveWaiting
                        | Pick::SwitchYes
                        | Pick::SwitchNo
                        | Pick::SwitchElsewhere
                        | Pick::SwitchCorrected { .. }
                        | Pick::NewYes
                        | Pick::NewNo => {}
                    }
                    return;
                }
            }
            _ => {}
        }
        match parse_line(line, &self.skills) {
            Command::Empty => {}
            Command::Quit => self.quit = true,
            Command::Help => {
                for l in HELP.lines() {
                    out.line(l);
                }
            }
            Command::Keys => {
                for l in crate::app::keymap::KEYS.lines() {
                    out.line(l);
                }
            }
            // `/view` (issue #115): the transcript view. The default is
            // `dev`; committed cells keep the rows they were built with,
            // only cells committed from here on change.
            Command::View(name) => match name {
                None => out.line(&format!(
                    "view: {} · {}",
                    out.view().name(),
                    out.view().meaning()
                )),
                Some("dev") => {
                    out.set_view(View::Dev);
                    out.line("view: dev · every call's figures, the step, the policy");
                }
                Some("normal") => {
                    out.set_view(View::Normal);
                    out.line("view: normal · the transcript as before #115");
                }
                Some(other) => out.line(&format!(
                    "unknown view {other:?} · dev or normal"
                )),
            },
            Command::ProjectUse(name) => {
                let r = self
                    .request(Request::SwitchProject {
                        thread: self.thread,
                        project: name.to_owned(),
                    })
                    .await;
                self.show(r, "", out);
            }
            Command::Rename(title) => {
                let r = self
                    .request(Request::Rename {
                        thread: self.thread,
                        title: title.to_owned(),
                    })
                    .await;
                self.show(r, "", out);
            }
            Command::Diff => {
                let r = self
                    .request(Request::Report {
                        thread: self.thread,
                        report: ReportKind::Diff,
                    })
                    .await;
                match r {
                    Response::Text { text } => out.pager("diff", &text),
                    other => self.show(other, "", out),
                }
            }
            Command::Chat(text) => self.post(text, false, out).await,
            Command::Interrupt(text) => self.post(text, true, out).await,
            Command::Skill(name, args) => {
                let r = self
                    .request(Request::InvokeSkill {
                        thread: self.thread,
                        name: name.to_owned(),
                        args: args.to_owned(),
                    })
                    .await;
                self.show(r, "", out);
            }
            Command::Cost => self.report(ReportKind::Cost, out).await,
            // `/new` with a turn open asks first (issue #108): a y/N menu of
            // this REPL's own, never sent to the daemon. The second
            // `/new` while the question is up is ignored, here and in
            // plain mode alike.
            Command::New => {
                if self.confirm_new.is_some() {
                    return;
                }
                if self.turn_open() {
                    let menu = Menu::new_thread();
                    // Plain mode prints the question (its kind is in
                    // `prints_in_plain`): a pipe that answers `y` or `n`
                    // gets the same handling as a terminal.
                    out.prompt(&menu);
                    self.confirm_new = Some(menu);
                } else {
                    self.new_front(false, out).await;
                }
            }
            Command::Build(arg) => self.build(arg, out).await,
            Command::Answer(arg) => self.answer_checkpoint(arg, out).await,
            // The raw stop reason stays off the transcript (issue #22):
            // `/why` fetches it, engine-local, no daemon round-trip.
            Command::Why => out.line(&why_line(self.last_stop.as_deref())),
            Command::Project => self.report(ReportKind::Project, out).await,
            Command::Policy => self.report(ReportKind::Policy, out).await,
            Command::Memory => self.report(ReportKind::Memory, out).await,
            Command::Skills => self.report(ReportKind::Skills, out).await,
            Command::Who => {
                out.line(&format!(
                    "you: {} ({})",
                    self.user,
                    self.role.as_deref().unwrap_or("no role")
                ));
                self.report(ReportKind::Who, out).await;
            }
            Command::Queue => out.line(&match &self.state {
                ThreadState::Running { by, queued } => match queued {
                    0 => format!("turn running by {}", author_name(by)),
                    n => format!(
                        "turn running by {}; {n} message(s) sent, reaching the agent at its next step",
                        author_name(by)
                    ),
                },
                ThreadState::Idle => "idle; nothing queued".into(),
                ThreadState::AwaitingApproval { call, .. } => {
                    format!("waiting for an approver: {}", describe_call(call))
                }
                ThreadState::AwaitingHuman { question, .. } => {
                    format!("waiting for an answer: {question}")
                }
                ThreadState::AwaitingSwitch { project, .. } => {
                    format!("waiting for an answer: switch to {project}?")
                }
            }),
            Command::Threads => {
                // The project this thread is in: the one it moved to, else
                // the one this REPL started in, as `/new` reads it. Not the
                // first project the daemon knows (issue #86). The listing
                // itself is every project (`project: None`); `current` only
                // decides which rows carry a project suffix (issue #97).
                let current = self
                    .project
                    .clone()
                    .unwrap_or_else(|| self.home_project.clone());
                let r = self.request(Request::ListThreads { project: None }).await;
                match r {
                    Response::Threads { threads } => {
                        for l in render_thread_groups(&threads, &current).lines() {
                            out.line(l);
                        }
                    }
                    // A refusal or error prints as it always did.
                    other => self.show(other, "", out),
                }
            }
            Command::Pin(text) => {
                let r = self
                    .request(Request::Pin {
                        thread: self.thread,
                        text: text.to_owned(),
                    })
                    .await;
                self.show(r, "[pinned]", out);
            }
            Command::Remember(text) => {
                let r = self
                    .request(Request::Remember {
                        thread: self.thread,
                        text: text.to_owned(),
                    })
                    .await;
                // The event is the one line of feedback, once it
                // arrives as a notice.
                self.show(r, "", out);
            }
            Command::Compact => {
                let r = self
                    .request(Request::Compact {
                        thread: self.thread,
                    })
                    .await;
                self.show(r, "[compacted]", out);
            }
            Command::Mode(None) => out.line(&format!(
                "[mode {}: {}]",
                self.mode,
                self.mode
                    .parse()
                    .map(aigentic_server::reports::mode_meaning)
                    .unwrap_or("")
            )),
            Command::Mode(Some(name)) => {
                let r = self
                    .request(Request::SetMode {
                        thread: self.thread,
                        mode: name.to_owned(),
                    })
                    .await;
                self.show(r, "", out);
            }
            Command::Copy(arg) => {
                use crate::app::copy::{CopyChoice, count, pick};
                let outcome = match pick(&self.reply, arg) {
                    CopyChoice::Block { n, text, lines } => {
                        Ok((text, format!("copied block {n} ({})", count(lines, "line"))))
                    }
                    CopyChoice::All { text, lines } => {
                        Ok((text, format!("copied the whole reply ({})", count(lines, "line"))))
                    }
                    CopyChoice::NoReply => Err(
                        "[no assistant reply in this session yet: /copy covers turns since this session started]"
                            .to_owned(),
                    ),
                    CopyChoice::NoBlocks => {
                        Err("[the last reply has no fenced code blocks]".to_owned())
                    }
                    CopyChoice::OutOfRange { n, total } => Err(format!(
                        "[block {n} is out of range: the last reply has {}]",
                        count(total, "block")
                    )),
                    CopyChoice::NotANumber => {
                        Err("[copy takes a block number or \"all\": /copy [n|all]]".to_owned())
                    }
                };
                match outcome {
                    Ok((text, ok)) => self.finish_copy(&text, &ok, out),
                    Err(line) => out.line(&line),
                }
            }
            Command::Profile(_) => {
                out.line(
                    "[/profile is not available over the daemon: the profile is the project's]",
                );
            }
            Command::Unknown(cmd) => out.line(&format!("unknown command: {cmd}")),
        }
    }

    /// `/new` (issues #89, #108): start a new front thread and move this
    /// REPL onto it. The old thread stays listed. A turn running in it
    /// is interrupted first — `/new` asks before it does that, and
    /// `interrupted` says the answer was yes — because a turn left
    /// running in a thread this REPL no longer shows is unreachable
    /// from here. This connection closes the old thread last, so a
    /// failure anywhere before that leaves the REPL where it was.
    async fn new_front(&mut self, interrupted: bool, out: &mut dyn Printer) {
        // The new thread is in the project this REPL is in; a thread
        // that never switched is in the project it started in. The
        // folder's project is not consulted.
        let project = self
            .project
            .clone()
            .unwrap_or_else(|| self.home_project.clone());
        // 1. Ask for it. A refusal (`/new` needs `write` here) or any
        //    other reply changes nothing.
        let new = match self
            .request(Request::NewFront {
                project: project.clone(),
            })
            .await
        {
            Response::Thread { thread } => thread,
            Response::Refused { reason } => {
                let reason = crate::front::without_project(&reason, &project);
                let already = if interrupted {
                    " (the running turn was already interrupted)"
                } else {
                    ""
                };
                out.line(&format!(
                    "cannot start a new thread in {project}: {reason}{already}"
                ));
                return;
            }
            other => {
                self.show(other, "", out);
                return;
            }
        };
        // 2. Open it. Until this succeeds the REPL is wholly on the old
        //    thread, still subscribed to it: the stray new front thread
        //    is what the next launch resumes, and the line says so.
        let (state, mode, identity) = match self
            .request(Request::Open {
                thread: new.id,
                from_seq: 0,
            })
            .await
        {
            Response::Opened {
                state,
                mode,
                profile,
                model,
                effort,
                ..
            } => (
                state,
                mode,
                Identity {
                    profile,
                    model,
                    effort,
                },
            ),
            Response::Refused { reason } => {
                out.line(&format!("cannot open the new thread {}: {reason}", new.id));
                return;
            }
            other => {
                out.line(&format!("cannot open the new thread {}: {other:?}", new.id));
                return;
            }
        };
        self.land_new_front(new, state, mode, identity, interrupted, out)
            .await;
    }

    /// Move the REPL onto a thread that has just been opened (`/new`'s
    /// steps 3-8). Split out from [`Self::new_front`] so the branch a
    /// failed `Open` takes can be driven with a reply the daemon would
    /// not send.
    async fn land_new_front(
        &mut self,
        new: ThreadInfo,
        state: ThreadState,
        mode: String,
        identity: Identity,
        interrupted: bool,
        out: &mut dyn Printer,
    ) {
        let old = self.thread;
        // 3. Every per-thread field goes; the per-session ones (`client`,
        //    `user`, `role` below, `skills`, `quit`, `home_project`,
        //    `following`, `checkpoint`) stay.
        self.thread = new.id;
        self.state = state;
        self.identity = identity;
        self.project = match new.project.as_deref() {
            Some(p) if p != self.home_project => Some(p.to_owned()),
            _ => None,
        };
        self.title = new.title.clone();
        self.calls.clear();
        self.partial.clear();
        self.reply.clear();
        self.streamed_text = false;
        self.reply_stream_start = 0;
        self.prompted = None;
        self.menu = None;
        self.turn = None;
        self.tasks.clear();
        self.task_calls.clear();
        self.usage = None;
        self.last_turn = None;
        self.last_stop = None;
        self.awaiting_turn = false;
        // The `/new` question was about the old thread (issue #108): it
        // does not follow this REPL over.
        self.confirm_new = None;
        // The role is the new project's, and only the daemon knows it:
        // `Welcome` named the project the client started in.
        let project = self
            .project
            .clone()
            .unwrap_or_else(|| self.home_project.clone());
        if let Response::Projects { projects } = self.request(Request::ListProjects).await
            && let Some(role) = front::role_for(&projects, &project)
        {
            self.role = Some(role);
        }
        // 4. This connection opened the old thread; let it go. `Close`
        //    is per client, so another one on it is unaffected.
        let _ = self.request(Request::Close { thread: old }).await;
        // 5. A new thread is `manual`: a session in another mode keeps
        //    it, as `main.rs` does at launch.
        if self.mode != "manual" {
            let keep = self.mode.clone();
            let _ = self
                .request(Request::SetMode {
                    thread: new.id,
                    mode: keep,
                })
                .await;
        } else {
            self.mode = mode;
        }
        // 6. The shell drops the live state the old thread left behind.
        out.thread_changed();
        // With a turn interrupted (issue #108), say what was *sent*: this
        // connection has just closed the old thread and cannot see the
        // interrupt land. Without one, the plain line as before.
        if interrupted {
            out.line(&format!(
                "sent an interrupt to {old}; new front thread {} · the old one stays listed in /threads",
                new.id
            ));
        } else {
            out.line(&format!(
                "new front thread {} · the old one stays listed in /threads",
                new.id
            ));
        }
    }

    async fn post(&mut self, text: &str, interrupt: bool, out: &mut dyn Printer) {
        let r = self
            .request(Request::Post {
                thread: self.thread,
                blocks: vec![ContentBlock::Text(text.to_owned())],
                interrupt,
            })
            .await;
        let ok = match (&self.state, interrupt) {
            (ThreadState::Idle, _) => "",
            (_, true) => "[interrupting]",
            (_, false) => "[sent: reaches the agent at its next step]",
        };
        if matches!(r, Response::Ok) && matches!(self.state, ThreadState::Idle) {
            self.awaiting_turn = true;
            // A new prompt starts a new reply (issue #41): what `/copy`
            // copies is the answer to the last thing asked here.
            self.reply.clear();
            self.streamed_text = false;
        }
        self.show(r, ok, out);
    }

    /// `/build <n>` (issue #68): start or resume a run of that issue and
    /// follow its lead's log from here. `Build`'s reply and `Open`'s both
    /// arrive on this task, which also consumes the notices, so a lead
    /// event appended in between is processed after the backlog and
    /// skipped by `printed`.
    async fn build(&mut self, arg: Option<&str>, out: &mut dyn Printer) {
        let mut words = arg.unwrap_or("").split_whitespace();
        let issue = words
            .next()
            .and_then(|a| a.parse::<u64>().ok())
            .filter(|n| *n > 0);
        let workflow = words.next().map(str::to_owned);
        let Some(issue) = issue.filter(|_| words.next().is_none()) else {
            out.line("[usage: /build <issue number> [workflow]]");
            return;
        };
        if !self.may_approve() {
            out.line("[/build needs the approve role in this project]");
            return;
        }
        // One run at a time: following m leaves n, which keeps running
        // in the daemon.
        if self.following.is_some() {
            self.detach(out);
        }
        let project = self
            .project
            .clone()
            .unwrap_or_else(|| self.home_project.clone());
        let r = self
            .request(Request::Build {
                project,
                issue,
                workflow,
            })
            .await;
        match r {
            Response::Run { lead, resumed } => {
                let how = if resumed { "resumed" } else { "started" };
                out.cell(
                    Cell::Run(format!("run {lead} for issue #{issue} ({how})")),
                    true,
                );
                self.following = Some(Followed {
                    lead,
                    issue,
                    printed: 0,
                });
                self.follow_backlog(lead, out).await;
            }
            Response::Refused { reason } => out.line(&format!("[build refused: {reason}]")),
            other => self.show(other, "", out),
        }
    }

    /// The lead's log from its first event, drawn in order (issue #68).
    /// Nothing is answered here: a gate the backlog ends at is shown and
    /// left waiting for a person.
    async fn follow_backlog(&mut self, lead: Ulid, out: &mut dyn Printer) {
        let events = match crate::run_view::fetch_backlog(&self.client, lead).await {
            Ok(events) => events,
            Err(e) if e.downcast_ref::<crate::run_view::Refused>().is_some() => {
                out.line(&format!("[build refused: {e}]"));
                return;
            }
            Err(e) => {
                out.line(&format!("[error: {e}]"));
                return;
            }
        };
        for event in &events {
            self.render_run_event(event, out);
        }
    }

    /// One event from the followed lead (issue #68): skip what `printed`
    /// already covers, then draw the line, show the prompt a gate asks
    /// for, and end the follow at the outcome. The same rule serves the
    /// backlog and the live notices.
    fn render_run_event(
        &mut self,
        event: &aigentic_runtime::aigentic_core::Event,
        out: &mut dyn Printer,
    ) {
        let Some(followed) = self.following.as_mut() else {
            return;
        };
        if event.seq <= followed.printed {
            return;
        }
        followed.printed = event.seq;
        let lead = followed.lead;
        if let Some(text) = crate::run_view::render(event) {
            out.cell(Cell::Run(text), true);
        }
        match event.kind {
            EventKind::CheckpointAsked => {
                if let Ok(p) =
                    serde_json::from_value::<CheckpointAskedPayload>(event.payload.clone())
                {
                    let menu = Menu::checkpoint(&p.gate, &p.shown, &p.options);
                    out.prompt(&menu);
                    self.checkpoint = Some(OpenGate {
                        lead,
                        gate: p.gate,
                        menu,
                        shown: true,
                        amending: false,
                    });
                }
            }
            // Answered from another connection: the prompt goes without
            // this client sending anything. The answer event names no
            // gate (the payload has no such field), and a lead has one
            // gate up at a time, so any answer withdraws the prompt.
            EventKind::CheckpointAnswered => {
                if serde_json::from_value::<CheckpointAnsweredPayload>(event.payload.clone())
                    .is_ok()
                {
                    self.checkpoint = None;
                }
            }
            EventKind::RunFinished => {
                if let Ok(p) = serde_json::from_value::<RunFinishedPayload>(event.payload.clone()) {
                    out.cell(
                        Cell::Run(format!(
                            "run {lead} finished: {}",
                            crate::run_view::outcome_line(&p.outcome)
                        )),
                        true,
                    );
                    self.following = None;
                    self.checkpoint = None;
                }
            }
            _ => {}
        }
    }

    /// Stop following the run (issue #68): its prompt goes, and it keeps
    /// running in the daemon — paused until the next `/build n` without
    /// `--server`.
    pub fn detach(&mut self, out: &mut dyn Printer) {
        let Some(followed) = self.following.take() else {
            return;
        };
        self.checkpoint = None;
        out.line(&format!(
            "[detached from run {}: it keeps running in the daemon; without --server, \
             closing this REPL pauses it until /build {}]",
            followed.lead, followed.issue
        ));
    }

    /// Whether a run is followed, for the keys (issue #68).
    pub fn following(&self) -> bool {
        self.following.is_some()
    }

    /// The followed run's checkpoint prompt, while it is up.
    pub fn checkpoint(&self) -> Option<&Menu> {
        self.checkpoint
            .as_ref()
            .filter(|open| open.shown)
            .map(|open| &open.menu)
    }

    /// `/answer go`, `/answer amend <text>` or `/answer stop`: answer the
    /// followed run's open checkpoint by name. The daemon refuses an
    /// answer the gate didn't offer, and the prompt stays up then.
    async fn answer_checkpoint(&mut self, arg: &str, out: &mut dyn Printer) {
        if self.checkpoint.is_none() {
            out.line("[no checkpoint is waiting in this REPL: /build <n> follows a run]");
            return;
        }
        let (word, rest) = arg.split_once(char::is_whitespace).unwrap_or((arg, ""));
        let (answer, amendment) = match (word, rest.trim()) {
            ("go", "") => (aigentic_api::CheckpointAnswer::Go, None),
            ("stop", "") => (aigentic_api::CheckpointAnswer::Stop, None),
            ("amend", text) if !text.is_empty() => {
                (aigentic_api::CheckpointAnswer::Amend, Some(text.to_owned()))
            }
            _ => {
                out.line("[usage: /answer go | /answer amend <text> | /answer stop]");
                return;
            }
        };
        self.send_answer(answer, amendment, out).await;
    }

    /// Send an answer to the open checkpoint. On success the gate goes;
    /// a refusal answers nothing and the gate stays as it was.
    async fn send_answer(
        &mut self,
        answer: aigentic_api::CheckpointAnswer,
        amendment: Option<String>,
        out: &mut dyn Printer,
    ) {
        let Some((lead, gate)) = self
            .checkpoint
            .as_ref()
            .map(|open| (open.lead, open.gate.clone()))
        else {
            return;
        };
        let word = match answer {
            aigentic_api::CheckpointAnswer::Go => "go",
            aigentic_api::CheckpointAnswer::Amend => "amend",
            aigentic_api::CheckpointAnswer::Stop => "stop",
        };
        let r = self
            .request(Request::AnswerCheckpoint {
                lead,
                gate: gate.clone(),
                answer,
                amendment,
            })
            .await;
        match r {
            Response::Ok => {
                self.checkpoint = None;
                out.line(&format!("[answered {gate}: {word}]"));
            }
            other => self.show(other, "", out),
        }
    }

    /// A picked or typed answer to the checkpoint prompt. Leaving it
    /// waiting hides the prompt and sends nothing; the gate stays open,
    /// so `/answer` still answers it.
    async fn apply_checkpoint(&mut self, keyed: Keyed, out: &mut dyn Printer) {
        let Some(gate) = self.checkpoint.as_ref().map(|open| open.gate.clone()) else {
            return;
        };
        let Keyed::Decide { pick, reason, echo } = keyed else {
            return;
        };
        out.line(&echo);
        match pick {
            Pick::ContinueRun => {
                self.send_answer(aigentic_api::CheckpointAnswer::Go, None, out)
                    .await;
            }
            Pick::AmendRun => match reason {
                Some(text) => {
                    self.send_answer(aigentic_api::CheckpointAnswer::Amend, Some(text), out)
                        .await;
                }
                None => self.start_amending(out),
            },
            Pick::StopRun => {
                self.send_answer(aigentic_api::CheckpointAnswer::Stop, None, out)
                    .await;
            }
            Pick::LeaveWaiting => {
                if let Some(open) = self.checkpoint.as_mut() {
                    open.shown = false;
                    open.amending = false;
                }
                out.line(&format!(
                    "[left {gate} waiting: /answer answers it, /build <n> shows it again]"
                ));
            }
            // Neither a switch's picks nor anything else reaches here:
            // a checkpoint menu offers only the four above.
            Pick::Answer
            | Pick::Other
            | Pick::Allow { .. }
            | Pick::Deny
            | Pick::SwitchYes
            | Pick::SwitchNo
            | Pick::SwitchElsewhere
            | Pick::SwitchCorrected { .. }
            | Pick::NewYes
            | Pick::NewNo => {}
        }
    }

    /// `Continue with changes` picked without its text: the prompt goes
    /// and the next plain line is the amendment.
    fn start_amending(&mut self, out: &mut dyn Printer) {
        if let Some(open) = self.checkpoint.as_mut() {
            open.shown = false;
            open.amending = true;
            out.line(&format!(
                "[type the changes for {} and press Enter; /answer stop stops the run]",
                open.gate
            ));
        }
    }

    /// Answer the permission request this client prompted for. `p` sends
    /// a prefix (allow from now on), `Esc` a reason with the deny.
    pub async fn decide(
        &mut self,
        allow: bool,
        session: bool,
        prefix: Option<Vec<String>>,
        reason: Option<String>,
        out: &mut dyn Printer,
    ) {
        let Some(call_id) = self.prompted.clone() else {
            return;
        };
        let r = self
            .request(Request::Decide {
                thread: self.thread,
                call_id,
                allow,
                session,
                prefix,
                reason,
            })
            .await;
        self.answered(r, out);
    }

    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// The project the thread last moved to, if it has.
    pub fn project(&self) -> Option<&str> {
        self.project.as_deref()
    }

    /// The checklist the shell draws above the turn line.
    pub fn tasks(&self) -> &[aigentic_runtime::harness_tools::Task] {
        &self.tasks
    }

    /// The running turn's figures, for the turn line.
    pub fn turn(&self) -> Option<&TurnStats> {
        self.turn.as_ref()
    }

    /// The menu the shell draws while this client is prompted: the `/new`
    /// question first (issue #108 — it is the one on screen and the one
    /// keys go to), then the chat prompt, else the followed run's
    /// checkpoint prompt (issue #68).
    pub fn menu(&self) -> Option<&Menu> {
        if self.confirm_new.is_some() {
            return self.confirm_new.as_ref();
        }
        if self.prompted.is_some() {
            return self.menu.as_ref();
        }
        self.checkpoint()
    }

    /// Which prompt `menu()` draws: the chat's call or the run's gate.
    /// The shell restarts the answering keys' grace when it changes, so
    /// a key in flight from answering one prompt cannot land on the
    /// prompt that replaces it (#68's review: `1` allowed a command, and
    /// the Enter after it stopped the run).
    pub fn menu_id(&self) -> Option<String> {
        if self.confirm_new.is_some() {
            return Some("confirm:new".to_owned());
        }
        if let Some(call_id) = self.prompted.as_ref() {
            return self.menu.as_ref().map(|_| format!("chat:{call_id}"));
        }
        self.checkpoint
            .as_ref()
            .filter(|open| open.shown)
            .map(|open| format!("gate:{}:{}", open.lead, open.gate))
    }

    /// A key while the menu is up: the selection, the digits, the hidden
    /// accelerators and Enter, sending the decision with the echo of
    /// what was chosen. `composer_empty` and `settled` as `Menu::key`
    /// takes them. `Text` says the composer should take the prompt's
    /// text.
    pub async fn menu_key(
        &mut self,
        key: &crossterm::event::KeyEvent,
        composer_empty: bool,
        settled: bool,
        out: &mut dyn Printer,
    ) -> MenuKey {
        if self.confirm_new.is_some() {
            // The `/new` question takes its keys first (issue #108):
            // Esc is `No` in `Menu::key`, so it never reaches the
            // keymap's interrupt. The question has no text input.
            let keyed = {
                let Some(menu) = self.confirm_new.as_mut() else {
                    return MenuKey::Passed;
                };
                menu.key(key, composer_empty, settled)
            };
            return match keyed {
                Keyed::Passed => MenuKey::Passed,
                Keyed::Used => MenuKey::Used,
                Keyed::Text => MenuKey::Passed,
                keyed @ (Keyed::Decide { .. } | Keyed::Answer { .. }) => {
                    self.apply_confirm(keyed, out).await;
                    MenuKey::Used
                }
            };
        }
        if self.prompted.is_none() {
            // With no chat prompt the keys belong to the followed run's
            // checkpoint menu (issue #68), if one is up.
            let keyed = {
                let Some(open) = self.checkpoint.as_mut().filter(|open| open.shown) else {
                    return MenuKey::Passed;
                };
                open.menu.key(key, composer_empty, settled)
            };
            return match keyed {
                Keyed::Passed => MenuKey::Passed,
                Keyed::Used => MenuKey::Used,
                // `Continue with changes`: the composer takes the text.
                Keyed::Text => {
                    self.start_amending(out);
                    MenuKey::Used
                }
                keyed @ (Keyed::Decide { .. } | Keyed::Answer { .. }) => {
                    self.apply_checkpoint(keyed, out).await;
                    MenuKey::Used
                }
            };
        }
        let Some(menu) = self.menu.as_mut() else {
            return MenuKey::Passed;
        };
        match menu.key(key, composer_empty, settled) {
            Keyed::Passed => MenuKey::Passed,
            Keyed::Text => MenuKey::Text,
            Keyed::Used => MenuKey::Used,
            keyed @ (Keyed::Decide { .. } | Keyed::Answer { .. }) => {
                self.apply(keyed, out).await;
                MenuKey::Used
            }
        }
    }

    /// What the menu came to: a decision, or an answer to the current
    /// question — echoed, and sent when it was the last.
    async fn apply(&mut self, keyed: Keyed, out: &mut dyn Printer) {
        match keyed {
            Keyed::Decide { pick, reason, echo } => {
                out.line(&echo);
                match pick {
                    Pick::Allow { session, prefix } => {
                        self.decide(true, session, prefix, reason, out).await;
                    }
                    Pick::Deny => {
                        self.decide(false, false, None, reason, out).await;
                    }
                    // A switch proposal's three answers (issue #82).
                    Pick::SwitchYes => self.answer_switch(SwitchReply::Yes, out).await,
                    Pick::SwitchNo => self.answer_switch(SwitchReply::No, out).await,
                    Pick::SwitchCorrected { to } => {
                        self.answer_switch(SwitchReply::Corrected { to }, out).await;
                    }
                    Pick::Answer
                    | Pick::Other
                    | Pick::ContinueRun
                    | Pick::AmendRun
                    | Pick::StopRun
                    | Pick::LeaveWaiting
                    | Pick::SwitchElsewhere
                    // The `/new` question is answered by `apply_confirm`,
                    // never by a daemon prompt's kind here (#108).
                    | Pick::NewYes
                    | Pick::NewNo => {}
                }
            }
            Keyed::Answer { text, echo } => {
                out.line(&echo);
                self.answer_menu(&text, out).await;
            }
            Keyed::Passed | Keyed::Used | Keyed::Text => {}
        }
    }

    /// Answer the `/new` question (issue #108). It is the REPL's own, so
    /// nothing is sent to the daemon for the answer itself: `Yes`
    /// interrupts the turn it names, then starts the new front thread;
    /// `No` leaves the thread and the turn exactly as they were.
    async fn apply_confirm(&mut self, keyed: Keyed, out: &mut dyn Printer) {
        let Keyed::Decide { pick, .. } = keyed else {
            return;
        };
        self.confirm_new = None;
        match pick {
            Pick::NewNo => out.line("kept this thread; nothing changed"),
            Pick::NewYes => {
                // The turn may have ended while the question was up:
                // then there is nothing to interrupt, and the new front
                // thread says only that.
                let interrupted = self.turn_open();
                if interrupted {
                    self.interrupt(out).await;
                }
                self.new_front(interrupted, out).await;
            }
            _ => {}
        }
    }

    /// A turn is open on this thread (issue #108): the states that mean
    /// the daemon has one, or a post this REPL just made and has not
    /// seen the state for yet (`awaiting_turn`), which is the same
    /// window one keystroke wide.
    fn turn_open(&self) -> bool {
        !matches!(self.state, ThreadState::Idle) || self.awaiting_turn
    }

    /// Answer the current question with `contribution` (the echo is the
    /// caller's): the next one is prompted, or the whole answer goes
    /// out in one `AnswerHuman`.
    async fn answer_menu(&mut self, contribution: &str, out: &mut dyn Printer) {
        let all = self.menu.as_mut().and_then(|m| m.answer(contribution));
        match all {
            Some(all) => self.answer_human(all, out).await,
            None => {
                if let Some(menu) = self.menu.as_ref() {
                    out.prompt(menu);
                }
            }
        }
    }

    /// Send the composed answer to the question this client is prompted
    /// for.
    async fn answer_human(&mut self, text: String, out: &mut dyn Printer) {
        let Some(call_id) = self.prompted.clone() else {
            return;
        };
        let r = self
            .request(Request::AnswerHuman {
                thread: self.thread,
                call_id,
                text,
            })
            .await;
        self.answered(r, out);
    }

    /// Answer the switch proposal this client is prompted for (issues
    /// #7, #82): yes, no, or where it belongs.
    async fn answer_switch(&mut self, answer: SwitchReply, out: &mut dyn Printer) {
        let Some(call_id) = self.prompted.clone() else {
            return;
        };
        let r = self
            .request(Request::AnswerSwitch {
                thread: self.thread,
                call_id,
                answer,
            })
            .await;
        self.answered(r, out);
    }

    /// The composer's text for the prompt, Enter on its text input: a
    /// deny's reason, with none when it was empty, or a question's
    /// free-text answer, headed like any other. A prompt answered
    /// elsewhere first is gone; the text goes nowhere.
    pub async fn prompt_text(&mut self, text: Option<String>, out: &mut dyn Printer) {
        if self.menu().is_none() {
            return;
        }
        match self.menu.as_ref().map(|m| m.kind) {
            Some(Kind::Permission) => {
                let echo = match &text {
                    Some(r) => format!("↳ No: {r}"),
                    None => "↳ No".into(),
                };
                out.line(&echo);
                self.decide(false, false, None, text, out).await;
            }
            Some(Kind::Question) => {
                if let Some(t) = text.map(|t| t.trim().to_owned()).filter(|t| !t.is_empty()) {
                    let contribution = self
                        .menu
                        .as_ref()
                        .map(|m| m.free_text(&t))
                        .unwrap_or(t.clone());
                    out.line(&format!("↳ {contribution}"));
                    self.answer_menu(&contribution, out).await;
                }
            }
            // The `/new` question has no text input (issue #108): its
            // keys are digits and `y`/`n`, and there is nothing for the
            // composer to send.
            Some(Kind::NewThread) => {}
            // A checkpoint has no text input: nothing to send.
            Some(Kind::Checkpoint) => {}
            // A switch proposal's text input is where it belongs
            // (issue #82): an empty one answers `No, stay here`.
            Some(Kind::Switch) => {
                match text.map(|t| t.trim().to_owned()).filter(|t| !t.is_empty()) {
                    Some(to) => {
                        out.line(&format!("↳ No, it belongs to: {to}"));
                        self.answer_switch(SwitchReply::Corrected { to }, out).await;
                    }
                    None => {
                        out.line("↳ No, stay here");
                        self.answer_switch(SwitchReply::No, out).await;
                    }
                }
            }
            None => {}
        }
    }

    /// Ctrl-C or Esc while a turn runs: cancel it, post nothing.
    pub async fn interrupt(&mut self, out: &mut dyn Printer) {
        let r = self
            .request(Request::Interrupt {
                thread: self.thread,
            })
            .await;
        self.show(r, "[interrupting]", out);
    }

    /// The reply to our `Decide` or `AnswerHuman`: `Ok` closes the
    /// prompt (the event that follows says what was decided); a refusal
    /// says why, and a race lost to another connection reads as such.
    fn answered(&mut self, response: Response, out: &mut dyn Printer) {
        match response {
            Response::Ok => {
                self.prompted = None;
                self.menu = None;
            }
            Response::Refused { reason } if reason.contains("already decided") => {
                self.prompted = None;
                self.menu = None;
                out.line(&format!("[{reason}; someone else was first]"));
            }
            other => self.show(other, "", out),
        }
    }

    async fn report(&mut self, kind: ReportKind, out: &mut dyn Printer) {
        let r = self
            .request(Request::Report {
                thread: self.thread,
                report: kind,
            })
            .await;
        self.show(r, "", out);
        // The turn's own cost (issue #21), where token totals now
        // live: what the turn wrote and how many calls it took. The
        // live line carries state and time only.
        if matches!(kind, ReportKind::Cost)
            && let Some((written, calls)) = self.last_turn
        {
            let calls_word = if calls == 1 { "call" } else { "calls" };
            out.line(&format!(
                "turn: {} written · {} {calls_word}",
                crate::app::status::count_short(written),
                calls
            ));
        }
    }

    fn flush_partial(&mut self, out: &mut dyn Printer) {
        if !self.partial.is_empty() {
            let text = std::mem::take(&mut self.partial);
            out.cell(
                Cell::Assistant {
                    text,
                    fenced: false,
                },
                true,
            );
            out.tail("");
        }
    }

    /// A tool call ends the model's message; its text may have no
    /// trailing newline, and the next message's text would run into it
    /// (issue #41). The display already breaks a line here, so `reply`
    /// keeps the same boundary and `/copy` sees the same blocks the
    /// person did.
    fn reply_boundary(&mut self) {
        if !self.reply.is_empty() && !self.reply.ends_with('\n') {
            self.reply.push('\n');
        }
    }

    /// Put `text` on the clipboard through the printer's transport and
    /// say what happened. `OSC 52` is named because the terminal may
    /// ignore it (issue #41).
    fn finish_copy(&mut self, text: &str, ok: &str, out: &mut dyn Printer) {
        match out.copy(text) {
            Ok(Used::Osc52) => out.line(&format!("{ok} via OSC 52")),
            Ok(Used::Child) => out.line(ok),
            Err(e) => out.line(&format!("[copy failed: {e}]")),
        }
    }

    pub fn state(&self) -> &ThreadState {
        &self.state
    }

    /// The user-invoked skills, for `/` completion.
    pub fn skills(&self) -> &[String] {
        &self.skills
    }

    pub fn mode(&self) -> &str {
        &self.mode
    }

    /// Who the thread runs as, for the status line's head (issue #43).
    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    /// The last figures the daemon reported: what the model sees and
    /// the whole thread (issue #99).
    pub fn usage(&self) -> Option<crate::app::status::Figures> {
        self.usage
    }

    pub fn quit_requested(&self) -> bool {
        self.quit
    }

    /// One notice to lines. Streamed text is printed as its lines
    /// complete; the rest at the message's end.
    pub fn render(&mut self, notice: Notice, out: &mut dyn Printer) {
        // A followed run's notices go to the run view (issue #68), and
        // everything else for its lead is dropped: a lead's `State` must
        // never reach `self.state`, which drives the footer and what
        // Ctrl-C and Esc mean. Notices for a thread that is neither the
        // chat nor the followed lead are dropped too — an old lead after
        // a detach still pushes them.
        if self.route(&notice, out) {
            return;
        }
        self.render_chat(notice, out)
    }

    /// Draw a notice as the run view, or answer whether it was one
    /// (issue #68). `false` hands the notice to the chat.
    fn route(&mut self, notice: &Notice, out: &mut dyn Printer) -> bool {
        let some_lead = self.following.as_ref().map(|f| f.lead);
        let thread = match notice {
            Notice::Event { thread, .. }
            | Notice::TextDelta { thread, .. }
            | Notice::ToolCallStarted { thread, .. }
            | Notice::State { thread, .. }
            | Notice::Mode { thread, .. }
            | Notice::Model { thread, .. }
            | Notice::Usage { thread, .. }
            | Notice::Note { thread, .. } => *thread,
        };
        if thread == self.thread {
            return false;
        }
        match notice {
            Notice::Event { event, .. } if some_lead == Some(thread) => {
                self.render_run_event(event, out);
                true
            }
            Notice::Note { text, .. } if some_lead == Some(thread) => {
                out.cell(Cell::Run(format!("note: {text}")), true);
                // The runner says the run is over before its last
                // events land; the follow ends here so nothing after
                // it is drawn as live.
                if text.starts_with("run stopped") {
                    self.following = None;
                    self.checkpoint = None;
                }
                true
            }
            // Every other notice for the lead, and every notice for a
            // thread we are not on: nothing to draw.
            _ => true,
        }
    }

    fn render_chat(&mut self, notice: Notice, out: &mut dyn Printer) {
        match notice {
            Notice::Model {
                profile,
                model,
                effort,
                ..
            } => {
                // The model this client is attached to (issue #43):
                // appended at attach, so a shell that missed the frame
                // still names it.
                self.identity = Identity {
                    profile,
                    model,
                    effort,
                };
            }
            Notice::TextDelta { text, .. } => {
                // The first text of this call's stream marks where a
                // discarded attempt would start (issue #114), so a
                // runtime retry truncates `/copy` back to exactly here.
                if !self.streamed_text {
                    self.streamed_text = true;
                    self.reply_stream_start = self.reply.len();
                }
                if let Some(t) = self.turn.as_mut() {
                    // A blank block is not writing (issue #43): the
                    // model sends one before nearly every tool call,
                    // and a phase change for it would move the pane
                    // twice per call.
                    if !text.trim().is_empty() {
                        t.writing = true;
                    }
                    // Content is arriving: the call recovered (issue #31).
                    t.retry = None;
                }
                self.partial.push_str(&text);
                // `/copy` keeps the whole reply, not just the current
                // line (issue #41); nothing renders from this.
                self.reply.push_str(&text);
                while let Some(pos) = self.partial.find('\n') {
                    let line: String = self.partial.drain(..=pos).collect();
                    out.cell(
                        Cell::Assistant {
                            text: line.trim_end_matches('\n').to_owned(),
                            fenced: false,
                        },
                        true,
                    );
                }
                out.tail(&self.partial);
            }
            Notice::ToolCallStarted { call, .. }
                if call.name == aigentic_runtime::harness_tools::UPDATE_TASKS =>
            {
                self.flush_partial(out);
                self.reply_boundary();
                self.task_calls.insert(call.id.clone());
                if let Ok(args) = serde_json::from_value::<
                    aigentic_runtime::harness_tools::UpdateTasksArgs,
                >(call.args.clone())
                {
                    use aigentic_runtime::harness_tools::TaskState;
                    // What is done reaches the scrollback one line at a
                    // time; the live block shows the rest.
                    let done = args
                        .tasks
                        .iter()
                        .filter(|t| {
                            t.state == TaskState::Done
                                && !self
                                    .tasks
                                    .iter()
                                    .any(|o| o.text == t.text && o.state == TaskState::Done)
                        })
                        .map(|t| t.text.clone())
                        .collect::<Vec<_>>();
                    for text in done {
                        out.cell(Cell::Done(text), true);
                    }
                    self.tasks = args.tasks;
                    // A new active step gets a header, and its calls
                    // indent under it (issue #115).
                    self.step_header(out);
                }
            }
            Notice::ToolCallStarted { call, .. } => {
                self.flush_partial(out);
                self.reply_boundary();
                let summary = summarise_args(&call);
                let full = full_command(&call);
                if let Some(t) = self.turn.as_mut() {
                    t.tools += 1;
                    t.writing = false;
                    t.retry = None;
                    t.current = Some(format!("{} {summary}", call.name));
                }
                self.calls.insert(
                    call.id.clone(),
                    (call.name.clone(), summary.clone(), full.clone()),
                );
                out.cell(
                    Cell::Tool {
                        name: call.name,
                        summary,
                        full,
                        state: ToolState::Running,
                        output: String::new(),
                        detail: None,
                    },
                    false,
                );
            }
            Notice::Mode { mode, .. } => {
                self.mode = mode.clone();
                out.quiet(&format!("[mode {mode}]"));
            }
            Notice::State { state, .. } => {
                self.flush_partial(out);
                self.show_state(&state, out);
                if !matches!(state, ThreadState::Idle) {
                    self.awaiting_turn = false;
                    if self.turn.is_none() {
                        self.turn = Some(TurnStats::new());
                        // A fresh checklist per turn: the last one's
                        // items are already in the scrollback.
                        self.tasks.clear();
                        // A fresh turn: no call of it has streamed text
                        // yet (issue #114).
                        self.streamed_text = false;
                    }
                }
                self.state = state;
            }
            Notice::Event { event, .. } => self.render_event(&event, out),
            Notice::Usage {
                tokens_in_window,
                thread_tokens,
                ..
            } => {
                self.usage = Some(crate::app::status::Figures {
                    working: tokens_in_window,
                    thread: thread_tokens,
                })
            }
            Notice::Note { text, .. } => {
                self.flush_partial(out);
                out.line(&format!("[{text}]"));
            }
        }
    }

    /// What the thread waits for, as this user sees it: a prompt when
    /// their role may answer, else who it waits for. A wait that ends
    /// without our answer withdraws the prompt.
    pub fn show_state(&mut self, state: &ThreadState, out: &mut dyn Printer) {
        match state {
            ThreadState::AwaitingApproval {
                call_id,
                call,
                class,
                reason,
            } => {
                if self.may_approve() {
                    let menu = Menu::permission(call, *class, reason);
                    out.prompt(&menu);
                    self.menu = Some(menu);
                    self.prompted = Some(call_id.clone());
                } else {
                    out.line(&format!(
                        "[waiting for an approver: {}]",
                        describe_call(call)
                    ));
                }
            }
            ThreadState::AwaitingHuman {
                call_id,
                question,
                questions,
            } => {
                if self.may_write() {
                    // An old daemon's frame carries no questions: the
                    // plain text is the one.
                    let all = if questions.is_empty() {
                        vec![aigentic_api::AskedQuestion {
                            question: question.clone(),
                            header: None,
                            options: Vec::new(),
                            multi: false,
                        }]
                    } else {
                        questions.clone()
                    };
                    let menu = Menu::asking(all);
                    out.prompt(&menu);
                    self.menu = Some(menu);
                    self.prompted = Some(call_id.clone());
                } else {
                    out.line(&format!("[waiting for an answer: {question}]"));
                }
            }
            ThreadState::AwaitingSwitch {
                call_id,
                project,
                workspace,
                reason,
            } => {
                if self.may_write() {
                    // The in-place block (issue #82): the same prompt as
                    // a permission or a question, answered in one
                    // keystroke (ADR 0002).
                    let menu = Menu::switch(project, workspace.as_deref(), reason);
                    out.prompt(&menu);
                    self.menu = Some(menu);
                    self.prompted = Some(call_id.clone());
                } else {
                    out.line(&format!("[waiting for an answer: switch to {project}?]"));
                }
            }
            ThreadState::Running { .. } | ThreadState::Idle => {
                // A decision's or an answer's event named its author
                // before this state arrived and closed the prompt; this
                // is the fallback for a wait that ended some other way.
                if self.prompted.take().is_some() {
                    out.line("[answered elsewhere]");
                }
                self.menu = None;
            }
        }
    }

    fn render_event(
        &mut self,
        event: &aigentic_runtime::aigentic_core::Event,
        out: &mut dyn Printer,
    ) {
        // The turn's own events, kept for the turn-end report (issue
        // #113). The `turn_ended` event itself is not one of them: it is
        // what the report is read at.
        if event.kind != EventKind::TurnEnded {
            self.turn_events.push(event);
        }
        match event.kind {
            EventKind::AssistantMessage => {
                self.flush_partial(out);
                // The call's reply is settled and kept: text after this
                // belongs to the next call (issue #114).
                self.streamed_text = false;
                if let Some(t) = self.turn.as_mut() {
                    t.writing = false;
                    t.retry = None;
                }
                if let Ok(AssistantMessagePayload { usage: Some(u), .. }) =
                    serde_json::from_value(event.payload.clone())
                {
                    // The call line (issue #115): drawn after the
                    // flushed reply text and before its tool results.
                    // The ordinal is the turn's own counter of
                    // usage-bearing messages.
                    let n = match self.turn.as_mut() {
                        Some(t) => t.record_usage(&u),
                        None => 1,
                    };
                    if detailed(out) {
                        out.cell(
                            Cell::Call(CallLine::from_usage(
                                n,
                                &u,
                                self.identity.profile.as_deref(),
                            )),
                            true,
                        );
                    }
                }
            }
            EventKind::UserMessage => {
                // Our own posts echo nothing; others' are named.
                if event.author
                    != Author::User(aigentic_runtime::aigentic_core::UserId(self.user.clone()))
                    && let Ok(p) =
                        serde_json::from_value::<UserMessagePayload>(event.payload.clone())
                {
                    self.flush_partial(out);
                    let text = p
                        .blocks
                        .iter()
                        .find_map(|b| match b {
                            ContentBlock::Text(t) => Some(t.as_str()),
                            _ => None,
                        })
                        .unwrap_or("");
                    out.line(&format!("{}: {text}", author_name(&event.author)));
                }
            }
            EventKind::ToolResult => {
                if let Some(t) = self.turn.as_mut() {
                    t.current = None;
                    t.retry = None;
                }
                if let Ok(ToolResultPayload { result: r, .. }) =
                    serde_json::from_value(event.payload.clone())
                    && self.task_calls.remove(&r.id)
                {
                    return;
                }
                if let Ok(ToolResultPayload { result: r, policy }) =
                    serde_json::from_value(event.payload.clone())
                {
                    // Our question, answered on another connection: the
                    // result is that person's event.
                    if self.prompted.as_deref() == Some(r.id.as_str()) {
                        self.prompted = None;
                        let who = author_name(&event.author);
                        if who != self.user {
                            out.line(&format!("[answered by {who}]"));
                        }
                    }
                    let (name, summary, full) = self
                        .calls
                        .remove(&r.id)
                        .unwrap_or_else(|| ("tool".to_owned(), String::new(), None));
                    // What the log held about the call (issue #115): its
                    // run time (the result's `created_at` minus its
                    // parent assistant message's), the result's size and
                    // the policy that allowed it.
                    let detail = tool_detail(policy.as_ref(), r.content.len(), event, &self.turn_events);
                    if matches!(name.as_str(), "edit_file" | "write_file")
                        && !r.is_error
                        && let Some(edit) = crate::app::diff::parse_edit_result(&r.content)
                    {
                        out.cell(
                            Cell::Edit {
                                edit,
                                detail: Some(detail),
                            },
                            true,
                        );
                        return;
                    }
                    out.cell(
                        Cell::Tool {
                            name,
                            summary,
                            full,
                            state: if r.is_error {
                                ToolState::Err
                            } else {
                                ToolState::Ok
                            },
                            output: r.content,
                            detail: Some(detail),
                        },
                        true,
                    );
                }
            }
            EventKind::PermissionDecided => {
                if let Ok(p) =
                    serde_json::from_value::<PermissionDecidedPayload>(event.payload.clone())
                {
                    let what = match (p.allow, p.scope) {
                        (true, DecisionScope::Once) => "allowed",
                        (true, DecisionScope::Session) => "allowed for this session",
                        (false, _) => "denied",
                    };
                    let why = p.reason.map(|r| format!(" ({r})")).unwrap_or_default();
                    let who = author_name(&event.author);
                    if self.prompted.as_deref() == Some(p.call_id.as_str()) {
                        // Our prompt, decided on another connection.
                        self.prompted = None;
                        if who != self.user {
                            out.line(&format!("[decided by {who}]"));
                        }
                    }
                    out.line(&format!("  [{what} by {who}{why}]"));
                }
            }
            EventKind::Interrupted => {
                if let Ok(p) = serde_json::from_value::<InterruptedPayload>(event.payload.clone()) {
                    self.flush_partial(out);
                    match p.by {
                        Some(by) => out.line(&format!("[interrupted by {}]", author_name(&by))),
                        None => out.line(&format!("[interrupted: {}]", p.reason)),
                    }
                }
            }
            EventKind::TurnEnded => {
                self.flush_partial(out);
                if let Some(t) = self.turn.take() {
                    out.cell(Cell::Summary(format!("─ {}", t.summary())), true);
                    // The turn's cost, for `/cost` (issue #21): the live
                    // line no longer carries it.
                    if t.output > 0 || t.tools > 0 {
                        self.last_turn = Some((t.output, t.tools));
                    }
                }
                if let Ok(p) = serde_json::from_value::<TurnEndedPayload>(event.payload.clone()) {
                    // The raw reason, for `/why`, is kept off the screen
                    // (issue #22): a provider failure shows its plain
                    // line, and the machine text stays one command away.
                    self.last_stop = stop_reason(&p);
                    // After the summary, before the stop line, so a turn
                    // that ended for any reason still says it slept
                    // (issue #47). What stopped it, what it did and
                    // whether anything is left come from the turn's own
                    // events (issue #113).
                    for line in turn_end_report(&p, self.turn_events.as_slice()) {
                        out.line(&line);
                    }
                    self.turn_events.clear();
                }
            }
            EventKind::Compacted => {
                self.flush_partial(out);
                if let Ok(p) = serde_json::from_value::<CompactedPayload>(event.payload.clone()) {
                    match p.strategy {
                        CompactionStrategy::TruncateResults { max_bytes } => out.line(&format!(
                            "[compacted: tool results in events {}-{} truncated to {max_bytes} bytes]",
                            p.from_seq, p.to_seq
                        )),
                        CompactionStrategy::Summary { usage, .. } => out.line(&format!(
                            "[compacted: events {}-{} summarised ({} tokens in, {} out)]",
                            p.from_seq,
                            p.to_seq,
                            usage.input_tokens + usage.cache_read_tokens + usage.cache_write_tokens,
                            usage.output_tokens
                        )),
                    }
                }
            }
            EventKind::SkillLoaded => {
                self.flush_partial(out);
                if let Ok(p) = serde_json::from_value::<SkillLoadedPayload>(event.payload.clone()) {
                    out.line(&format!(
                        "[skill {} loaded ({} bytes, {})]",
                        p.name,
                        p.body.len(),
                        p.source
                    ));
                }
            }
            EventKind::ProjectSwitched => {
                self.flush_partial(out);
                if let Ok(p) = serde_json::from_value::<
                    aigentic_runtime::aigentic_log::ProjectSwitchedPayload,
                >(event.payload.clone())
                {
                    let name =
                        |n: &Option<String>| n.clone().unwrap_or_else(|| "no project".into());
                    let ws = p
                        .workspace
                        .as_ref()
                        .map(|w| format!(" ({w})"))
                        .unwrap_or_default();
                    out.line(&format!(
                        "[project: {} → {}{ws} · {}]",
                        name(&p.from),
                        name(&p.to),
                        p.root.display()
                    ));
                    self.project = p.to;
                }
            }
            EventKind::ThreadRenamed => {
                if let Ok(p) = serde_json::from_value::<
                    aigentic_runtime::aigentic_log::ThreadRenamedPayload,
                >(event.payload.clone())
                {
                    out.quiet(&format!("[title: {}]", p.title));
                    self.title = Some(p.title);
                }
            }
            // A retry (issue #31): set the turn line's reason, print no
            // line of its own. The attempt is visible the moment the
            // wait starts, so a dead endpoint never reads as a slow model.
            EventKind::ProviderRetried => {
                // A runtime retry after text was streamed (issue #114):
                // the reply on screen and in `/copy` is discarded whole,
                // so say so and cut it back past the attempt. A #90
                // adapter retry never reaches here with text shown — its
                // window is before the first content — so it stays silent,
                // as before. A backlog carries no `TextDelta` notices, so
                // the flag is false on replay and nothing is drawn.
                if self.streamed_text {
                    self.flush_partial(out);
                    out.cell(
                        Cell::Note(
                            "[the reply was cut off mid-stream; asking again — the text above is \
                             discarded]"
                                .into(),
                        ),
                        true,
                    );
                    self.reply.truncate(self.reply_stream_start);
                    self.streamed_text = false;
                }
                if let Ok(p) = serde_json::from_value::<
                    aigentic_runtime::aigentic_log::ProviderRetriedPayload,
                >(event.payload.clone())
                {
                    // The developer view's own line (issue #115): drawn
                    // after #114's note above when both apply, from the
                    // one helper the activity row also reads.
                    let wait = RetryWait {
                        attempt: p.attempt,
                        retries: p.retries,
                        reason: p.reason,
                        // The line counts down to the next attempt
                        // (issue #90); the app ticks every 250 ms.
                        until: std::time::Instant::now()
                            + std::time::Duration::from_millis(p.wait_ms),
                        wait_ms: p.wait_ms,
                    };
                    system_cell(out, wait.system());
                    if let Some(t) = self.turn.as_mut() {
                        t.retry = Some(wait);
                    }
                }
            }
            // Saturation (issue #35): the sweep can stub no deeper, so the
            // turn is as small as it will get. A line naming the ceiling,
            // so the person knows to start a fresh thread rather than read
            // a thrashing one.
            EventKind::ContextSaturated => {
                if let Ok(p) = serde_json::from_value::<
                    aigentic_runtime::aigentic_log::ContextSaturatedPayload,
                >(event.payload.clone())
                {
                    self.flush_partial(out);
                    out.line(&saturated_line(&p));
                }
            }
            EventKind::MemoryExtracted => {
                // The pipe's footer, one per written item (unchanged,
                // item 8); the shell keeps it out of the transcript.
                if let Ok(p) =
                    serde_json::from_value::<MemoryExtractedPayload>(event.payload.clone())
                {
                    for line in &p.written {
                        out.quiet(&format!("filed to memory: {} ({})", line.text, line.file));
                    }
                }
                if let Some(line) = system_line(event) {
                    self.flush_partial(out);
                    system_cell(out, line);
                }
            }
            EventKind::MemoryRemembered => {
                if let Ok(p) =
                    serde_json::from_value::<MemoryRememberedPayload>(event.payload.clone())
                {
                    if p.written {
                        out.quiet(&format!("filed to memory: {} ({})", p.text, p.file));
                    } else {
                        out.quiet(&format!("already in memory: {} ({})", p.text, p.file));
                    }
                }
                if let Some(line) = system_line(event) {
                    self.flush_partial(out);
                    system_cell(out, line);
                }
            }
            EventKind::ContextEvicted
            | EventKind::ResultsStubbed
            | EventKind::DecisionProposed
            | EventKind::DecisionAnswered => {
                // The sweep and decision lines (issue #115): what the
                // harness did on its own. Developer view only.
                if let Some(line) = system_line(event) {
                    self.flush_partial(out);
                    system_cell(out, line);
                }
            }
            EventKind::Pinned
            | EventKind::PermissionRequested
            | EventKind::ThreadStarted
            // The build runner's events (issue #53): they belong to the
            // lead thread's run view, not to any turn, so a turn view
            // prints no line for them.
            | EventKind::RunStarted
            | EventKind::StepStarted
            | EventKind::StepFinished
            | EventKind::ChecksRun
            | EventKind::RouteTaken
            | EventKind::CheckpointAsked
            | EventKind::CheckpointAnswered
            | EventKind::BudgetWarned
            | EventKind::Pushed
            | EventKind::RunFinished
            | EventKind::StepReported => {}
        }
    }
}

/// One sweep's line (issue #115): `context evicted through #42`, with
/// the calibration ratio as a percentage when the event carried one.
pub(crate) fn sweep_line(what: &str, through_seq: u64, ratio: Option<f64>) -> String {
    let mut line = format!("{what} through #{through_seq}");
    if let Some(ratio) = ratio {
        line.push_str(&format!(" · {:.0}%", ratio * 100.0));
    }
    line
}

/// The developer view's line for what the harness did on its own (issue
/// #115), or `None` for a kind that draws none. Pure over the event, so
/// each kind's wording is tested without a daemon.
/// What the report calls a line's home. The project's is unmarked: it is
/// where every line went before the other two homes existed.
fn memory_home_name(home: MemoryHome) -> &'static str {
    match home {
        MemoryHome::Project => "",
        MemoryHome::Workspace => "the workspace's ",
        MemoryHome::Person => "the person's ",
    }
}

fn system_line(event: &Event) -> Option<String> {
    match event.kind {
        EventKind::ProviderRetried => payload(
            event,
            |p: aigentic_runtime::aigentic_log::ProviderRetriedPayload| {
                RetryWait {
                    attempt: p.attempt,
                    retries: p.retries,
                    reason: p.reason,
                    until: std::time::Instant::now(),
                    wait_ms: p.wait_ms,
                }
                .system()
            },
        ),
        EventKind::ContextEvicted => payload(
            event,
            |p: aigentic_runtime::aigentic_log::ContextEvictedPayload| {
                sweep_line("context evicted", p.through_seq, p.ratio)
            },
        ),
        EventKind::ResultsStubbed => payload(
            event,
            |p: aigentic_runtime::aigentic_log::ResultsStubbedPayload| {
                sweep_line("old results stubbed", p.through_seq, p.ratio)
            },
        ),
        EventKind::MemoryExtracted => payload(event, |p: MemoryExtractedPayload| {
            let n = p.written.len();
            if n == 0 {
                "memory: nothing written".to_owned()
            } else {
                let noun = if n == 1 { "line" } else { "lines" };
                let mut line = format!("memory: {n} {noun} written");
                if !p.model.is_empty() {
                    line.push_str(&format!(" · {}", p.model));
                }
                let (stamped, estimated) = money_cells(&p.usage);
                let money = crate::stats::money(stamped, estimated);
                if money != "-" {
                    line.push_str(&format!(" · {money}"));
                }
                line
            }
        }),
        EventKind::MemoryRemembered => payload(event, |p: MemoryRememberedPayload| {
            let home = memory_home_name(p.home);
            if p.written {
                format!("remembered in {home}{}", p.file)
            } else {
                format!("already known: {home}{}", p.file)
            }
        }),
        EventKind::DecisionProposed => payload(event, |p: DecisionProposedPayload| {
            format!(
                "proposed {}: {} — {}",
                decision_kind_name(p.kind),
                p.proposal,
                p.reason
            )
        }),
        EventKind::DecisionAnswered => payload(event, |p: DecisionAnsweredPayload| {
            let mut line = format!("answered {}", decision_answer_name(p.answer));
            if let Some(correction) = p.correction {
                line.push_str(&format!(" — {correction}"));
            } else if let Some(note) = p.note {
                line.push_str(&format!(" ({note})"));
            }
            line
        }),
        _ => None,
    }
}

/// Read `event.payload` as `P` and map it; `None` when it will not parse
/// (an old or foreign line).
fn payload<P: serde::de::DeserializeOwned>(
    event: &Event,
    f: impl FnOnce(P) -> String,
) -> Option<String> {
    serde_json::from_value(event.payload.clone()).ok().map(f)
}

/// The two cells [`crate::stats::money`] takes for one usage: the stamped
/// amount, or the estimate when the log marked it guessed. `None` both
/// ways when the line holds no price, so no figure is invented.
pub(crate) fn money_cells(u: &Usage) -> (Option<f64>, Option<f64>) {
    match (u.cost_usd, u.estimated) {
        (Some(usd), false) => (Some(usd), None),
        (Some(usd), true) => (None, Some(usd)),
        (None, _) => (None, None),
    }
}

/// The word a decision kind reads as, matching `stats.rs`'s report.
fn decision_kind_name(kind: DecisionKind) -> &'static str {
    match kind {
        DecisionKind::Project => "project",
        DecisionKind::Job => "job",
        DecisionKind::Ticket => "ticket",
        DecisionKind::Knowledge => "knowledge",
        DecisionKind::Route => "route",
        DecisionKind::WorkingSet => "working_set",
    }
}

/// The word a decision answer reads as.
fn decision_answer_name(answer: DecisionAnswer) -> &'static str {
    match answer {
        DecisionAnswer::Yes => "yes",
        DecisionAnswer::No => "no",
        DecisionAnswer::Corrected => "corrected",
        DecisionAnswer::Withdrawn => "withdrawn",
    }
}

/// What the log held about one tool call (issue #115): its run time —
/// the result's `created_at` minus its `parent_event` event's
/// `created_at`, the `assistant_message` that carried the call — the
/// result's size, and the policy that allowed it. For a parallel batch
/// this is "done after the call was made", not the call's own span.
/// Never estimated: a part the log does not hold is `None`.
pub(crate) fn tool_detail(
    policy: Option<&PolicyRecord>,
    content_len: usize,
    event: &Event,
    events: &TurnEvents,
) -> ToolDetail {
    let took = event
        .parent_event
        .and_then(|id| events.at(&id))
        .and_then(|start| std::time::Duration::try_from(event.created_at - start.created_at).ok());
    let policy = policy.map(|p| match p {
        PolicyRecord::Rule { rule, decision, .. } if decision == "deny" => {
            format!("denied: rule {rule}")
        }
        PolicyRecord::Rule { rule, .. } => format!("rule {rule}"),
        PolicyRecord::Human { allow: true, .. } => "you allowed".to_owned(),
        PolicyRecord::Human { allow: false, .. } => "you denied".to_owned(),
    });
    ToolDetail {
        took,
        bytes: content_len,
        policy,
    }
}

/// The line a saturated turn shows once (issue #35): what the sweep is
/// holding, and what it is holding it under, so the person can start a
/// fresh thread instead of watching the same boundary move every call.
pub(crate) fn saturated_line(
    p: &aigentic_runtime::aigentic_log::ContextSaturatedPayload,
) -> String {
    format!(
        "[context saturated: eviction already holds {} of the {}-token ceiling; \
         start a fresh thread for the rest]",
        p.tokens_at_floor, p.ceiling
    )
}

/// The line a turn that slept shows once, right after its summary, or
/// nothing when it did not sleep (issue #47). Minutes are whole minutes
/// with the first one at a minute, and the guard's own status is
/// appended only when the machine slept *and* the guard was not simply
/// on: that is the case where the sleep was avoidable, or where the
/// program was missing.
pub(crate) fn slept_line(p: &TurnEndedPayload) -> Option<String> {
    let slept = p.slept_secs?;
    let mins = (slept / 60).max(1);
    let mut line = format!("the machine slept {mins} min during this turn");
    if slept.saturating_sub(p.slept_awaiting_secs.unwrap_or(0)) > 0 {
        match p.keep_awake.as_deref() {
            Some("on") | None => {}
            Some("off") => line.push_str("; set keep_awake = true to prevent this"),
            Some(status) => {
                line.push_str("; ");
                line.push_str(status);
            }
        }
    }
    Some(line)
}

/// The head of a `turn_ended` reason: `provider_error: http 503` is a
/// `provider_error` turn, the same grouping `stats.rs` uses.
fn reason_head(reason: &str) -> &str {
    reason.split(':').next().unwrap_or("")
}

/// The raw stop reason a payload leaves for `/why`: only a provider
/// failure has machine text worth showing (issue #22). Every other end
/// (done, asked_human, interrupted) clears it.
pub(crate) fn stop_reason(p: &TurnEndedPayload) -> Option<String> {
    (reason_head(&p.reason) == "provider_error").then(|| p.reason.clone())
}

/// What `/why` prints (issue #22): the last turn's raw stop reason, or
/// that there is nothing raw to show.
pub(crate) fn why_line(last_stop: Option<&str>) -> String {
    match last_stop {
        Some(reason) => format!("[last stop: {reason}]"),
        None => "[the last turn ended done]".to_owned(),
    }
}

/// The line for a reply the model's own output limit stopped (issue
/// #96): the model ran out of room mid-reply, no tool call of it ran,
/// and nothing failed — `continue` picks the thread up where it
/// stopped. Named here so a test asserts the constant, not a retyped
/// copy.
pub(crate) const LENGTH_STOP_TEXT: &str =
    "the reply hit the model's output limit; type continue to go on";

/// The lines a `turn_ended` payload prints after the turn's summary
/// (issues #47, #96, #113): the slept line, then — for a turn that ended
/// on anything but `done`/`asked_human`/`interrupted` — what stopped it,
/// what the turn did, and whether anything is left. Everything after the
/// first line comes from the turn's own events, so a turn that finished
/// its work and then lost its closing reply is not told to continue.
/// `turn` is the open turn's events as the caller kept them; see
/// [`TurnEvents`].
pub(crate) fn turn_end_report(p: &TurnEndedPayload, turn: &[Event]) -> Vec<String> {
    let mut lines: Vec<String> = slept_line(p).into_iter().collect();
    if p.reason == "done" || p.reason == ASKED_HUMAN || p.reason == INTERRUPTED {
        return lines;
    }
    let head = reason_head(&p.reason);
    let message = match (head, &p.error) {
        ("provider_error", Some(e)) => format!(
            "the model's reply failed: {}  (/why shows the raw error)",
            e.cause_line()
        ),
        // A reply the model's own output limit stopped (issue #96): no
        // tool ran, and nothing failed, so it says what to do next.
        (LENGTH_STOP, None) => LENGTH_STOP_TEXT.to_owned(),
        // A budget cap, by its plain name (issue #113). The payload
        // carries no figure, and the tui has no budget config to read
        // one from, so none is invented.
        ("max_wall_time", _) => "the turn hit its wall-time limit".to_owned(),
        ("max_tokens", _) => "the turn hit its token budget".to_owned(),
        ("max_iterations", _) => "the turn hit its call limit".to_owned(),
        _ => p.reason.clone(),
    };
    lines.push(format!("[turn ended: {message}]"));
    if turn.iter().any(is_turn_start) {
        let calls = named_calls(turn, REPORT_CALLS);
        let what = if calls.is_empty() {
            "no tool ran".to_owned()
        } else {
            calls
                .iter()
                .map(|(c, failed)| {
                    let text = format!("{} {}", c.name, summarise_args(c));
                    // A call whose result was an error is marked, so the
                    // line never reads as if failed work was done
                    // (issue #113 follow-up); a successful one is
                    // unchanged.
                    if *failed {
                        format!("{text} (failed)")
                    } else {
                        text
                    }
                })
                .collect::<Vec<_>>()
                .join(" · ")
        };
        lines.push(format!("[before that: {what}]"));
    } else {
        // The client attached or resumed mid-turn, so it never saw the
        // turn start: it can say what it has seen, not that nothing ran.
        lines.push(format!("[{JOINED_MID_TURN}]"));
    }
    if !p.touched.is_empty() {
        lines.push(format!("[files written: {}]", p.touched.join(", ")));
    }
    lines.push(format!("[{}]", left_line(head, turn)));
    lines
}

/// What is left of the turn, decided only from the turn's own events
/// (issue #113): an unfinished call or an open checklist first, then a
/// finished checklist, then a cap that stopped the model while it was
/// working, then the honest unknown. `continue` appears only where there
/// might be something to continue.
fn left_line(head: &str, turn: &[Event]) -> String {
    if let Some(call) = unfinished_call(turn) {
        return format!(
            "left: {} {} — type continue to carry on",
            call.name,
            summarise_args(&call)
        );
    }
    let checklist = open_checklist(turn);
    if let Some(c) = &checklist {
        if c.done < c.total {
            let step = c.active.as_deref().unwrap_or("the checklist");
            return format!("left: {step} — type continue to carry on");
        }
        // A finished checklist is done even when a cap ended the turn:
        // the cap is already named in the first line.
        return "nothing was left mid-way".to_owned();
    }
    if is_cap(head) {
        return "left: the turn was stopped while working — type continue to carry on".to_owned();
    }
    "if that wasn't the end, type continue".to_owned()
}

/// The head of a `turn_ended` reason that is a budget cap, not a failure.
fn is_cap(head: &str) -> bool {
    matches!(head, "max_wall_time" | "max_tokens" | "max_iterations")
}

/// The open turn's own events, as a client keeps them (issues #109,
/// #113): cleared at each turn start and bounded by what the turn-end
/// report needs, not by a count. A turn start is a `user_message` with
/// `mid_turn == false`, so a message typed mid-turn does not clear it.
#[derive(Debug, Default, Clone)]
pub(crate) struct TurnEvents(Vec<Event>);

impl TurnEvents {
    /// Adds an event, clearing the buffer first when it starts a turn.
    pub(crate) fn push(&mut self, event: &Event) {
        if is_turn_start(event) {
            self.0.clear();
        }
        self.0.push(event.clone());
        self.prune();
    }

    pub(crate) fn clear(&mut self) {
        self.0.clear();
    }

    pub(crate) fn as_slice(&self) -> &[Event] {
        &self.0
    }

    /// The event with this id, when the buffer still holds it (issue
    /// #115): a tool result's `took` is read from its parent assistant
    /// message's `created_at`.
    pub(crate) fn at(&self, id: &Ulid) -> Option<&Event> {
        self.0.iter().find(|e| &e.id == id)
    }

    /// Drops what the report cannot need: everything but the turn start;
    /// the newest `update_tasks` call whose result succeeded, its result,
    /// and every `update_tasks` call after it with its result; the last
    /// assistant message and every event after it; and the last
    /// `REPORT_CALLS` calls, successful or not, with their results.
    ///
    /// Keeping only the newest successful `update_tasks` call (and what
    /// follows it) bounds the checklist set however often the model
    /// updates it: `open_checklist` reads `done`, `total` and `active`
    /// from that one call alone, so the buffer's checklist stays the
    /// same as the whole turn's at a fixed size. The older calls remain
    /// in the log; only this client-side buffer drops them.
    fn prune(&mut self) {
        // Parse each event once: the calls a message holds, and the id and
        // outcome of a result. `message_calls` is then not re-run for
        // every result, which is what made a checklist-heavy turn cost
        // quadratic time.
        let mut calls: Vec<Vec<ToolCall>> = Vec::with_capacity(self.0.len());
        let mut results: Vec<Option<(String, bool)>> = Vec::with_capacity(self.0.len());
        let mut last_at: Option<usize> = None;
        for (i, e) in self.0.iter().enumerate() {
            if e.kind == EventKind::AssistantMessage {
                calls.push(message_calls(e));
                last_at = Some(i);
            } else {
                calls.push(Vec::new());
            }
            results.push(if e.kind == EventKind::ToolResult {
                serde_json::from_value::<ToolResultPayload>(e.payload.clone())
                    .ok()
                    .map(|p| (p.result.id, p.result.is_error))
            } else {
                None
            });
        }
        // The ids whose result came back without an error.
        let mut succeeded: HashSet<&str> = HashSet::new();
        for (id, is_error) in results.iter().flatten() {
            if !is_error {
                succeeded.insert(id);
            }
        }
        let ordered: Vec<&ToolCall> = calls.iter().flatten().collect();
        let mut keep: HashSet<String> = HashSet::new();
        // The newest successful `update_tasks` call, and every
        // `update_tasks` call after it (a failed one, or one still in
        // flight), each with its result.
        if let Some(at) = ordered
            .iter()
            .rposition(|c| c.name == UPDATE_TASKS && succeeded.contains(c.id.as_str()))
        {
            keep.extend(
                ordered[at..]
                    .iter()
                    .filter(|c| c.name == UPDATE_TASKS)
                    .map(|c| c.id.clone()),
            );
        }
        // The last `REPORT_CALLS` successful calls: `before that:` prefers
        // them.
        let mut good: Vec<&ToolCall> = Vec::new();
        for (id, is_error) in results.iter().flatten() {
            if *is_error {
                continue;
            }
            if let Some(c) = ordered
                .iter()
                .find(|c| c.id == *id)
                .filter(|c| c.name != UPDATE_TASKS)
            {
                good.push(c);
            }
        }
        let skip = good.len().saturating_sub(REPORT_CALLS);
        keep.extend(good[skip..].iter().map(|c| c.id.clone()));
        // The calls that ran but did not finish, so `before that:` never
        // claims that no tool ran while one did, and so the last batch's
        // unfinished call survives.
        let skip = ordered.len().saturating_sub(REPORT_CALLS);
        keep.extend(ordered[skip..].iter().map(|c| c.id.clone()));

        let mut kept = Vec::with_capacity(self.0.len());
        for (i, e) in self.0.iter().enumerate() {
            let holds = e.kind == EventKind::AssistantMessage
                && calls[i].iter().any(|c| keep.contains(&c.id));
            let answers = e.kind == EventKind::ToolResult
                && results[i].as_ref().is_some_and(|(id, _)| keep.contains(id));
            if is_turn_start(e) && i == 0 || last_at.is_some_and(|at| i >= at) || holds || answers {
                kept.push(e.clone());
            }
        }
        self.0 = kept;
    }
}

/// How many of the turn's successful calls the report names.
const REPORT_CALLS: usize = 3;

/// What a client that attached or resumed mid-turn reads in place of the
/// `before that:` line: it never saw the turn start, so it never claims
/// that no tool ran.
const JOINED_MID_TURN: &str = "this window joined mid-turn; the transcript and /why have the rest";

/// The tool name that carries the model's checklist.
const UPDATE_TASKS: &str = "update_tasks";

/// Whether an event starts a turn: a `user_message` with `mid_turn ==
/// false` (issue #109's rule), or an old line without the field.
fn is_turn_start(event: &Event) -> bool {
    event.kind == EventKind::UserMessage
        && serde_json::from_value::<UserMessagePayload>(event.payload.clone())
            .is_ok_and(|p| !p.mid_turn)
}

/// The tool calls a message holds, in order.
fn message_calls(event: &Event) -> Vec<ToolCall> {
    let Ok(p) = serde_json::from_value::<AssistantMessagePayload>(event.payload.clone()) else {
        return Vec::new();
    };
    p.blocks
        .into_iter()
        .filter_map(|b| match b {
            ContentBlock::ToolCall(c) => Some(c),
            _ => None,
        })
        .collect()
}

/// The index of the turn's last assistant message, if it has one.
fn last_message_at(turn: &[Event]) -> Option<usize> {
    turn.iter()
        .rposition(|e| e.kind == EventKind::AssistantMessage)
}

/// The turn's successful tool calls, newest last, at most `n` of them:
/// a call whose result came back without an error (issue #113). Rendering
/// goes through the transcript's own [`summarise_args`], so the report
/// and the screen never disagree.
fn successful_calls(turn: &[Event], n: usize) -> Vec<ToolCall> {
    let mut out: Vec<ToolCall> = Vec::new();
    for e in turn {
        if e.kind != EventKind::ToolResult {
            continue;
        }
        let Ok(p) = serde_json::from_value::<ToolResultPayload>(e.payload.clone()) else {
            continue;
        };
        if p.result.is_error {
            continue;
        }
        let call = turn
            .iter()
            .filter(|m| m.kind == EventKind::AssistantMessage)
            .flat_map(message_calls)
            .find(|c| c.id == p.result.id)
            // `update_tasks` draws as a checklist in the transcript, not as
            // a call, so the report does not name it either.
            .filter(|c| c.name != UPDATE_TASKS);
        if let Some(call) = call {
            out.push(call);
        }
    }
    let keep = out.len().saturating_sub(n);
    out.split_off(keep)
}

/// Every tool call the turn holds, in order, newest last, whichever way
/// its result went.
fn last_calls(turn: &[Event], n: usize) -> Vec<ToolCall> {
    let mut out: Vec<ToolCall> = turn
        .iter()
        .filter(|e| e.kind == EventKind::AssistantMessage)
        .flat_map(message_calls)
        .collect();
    let keep = out.len().saturating_sub(n);
    out.split_off(keep)
}

/// The calls `before that:` names (issue #113), each with whether its
/// result was an error (the mark a failed call carries), so the line
/// never reads as if failed work was done: the last `n` successful calls
/// other than the checklist call — the same text already shows in the
/// call's own cell — or, when nothing succeeded, what did run, so the
/// line never says no tool ran while a tool did.
fn named_calls(turn: &[Event], n: usize) -> Vec<(ToolCall, bool)> {
    let failed = failed_ids(turn);
    let good = successful_calls(turn, n);
    if !good.is_empty() {
        return good.into_iter().map(|c| (c, false)).collect();
    }
    let ran: Vec<ToolCall> = last_calls(turn, n)
        .into_iter()
        .filter(|c| c.name != UPDATE_TASKS)
        .collect();
    let ran = if ran.is_empty() {
        // Nothing but the checklist call: name it rather than deny it.
        last_calls(turn, n)
    } else {
        ran
    };
    ran.into_iter()
        .map(|c| {
            let was_failed = failed.contains(&c.id);
            (c, was_failed)
        })
        .collect()
}

/// The ids of the turn's calls whose result came back as an error, so a
/// failed call can be marked in `before that:`.
fn failed_ids(turn: &[Event]) -> HashSet<String> {
    turn.iter()
        .filter(|e| e.kind == EventKind::ToolResult)
        .filter_map(|e| serde_json::from_value::<ToolResultPayload>(e.payload.clone()).ok())
        .filter(|p| p.result.is_error)
        .map(|p| p.result.id)
        .collect()
}

/// The first call in the turn's last batch that did not run or never came
/// back (issue #113): a call with no result at all, or one whose result a
/// policy record marks as not run. Only the last batch counts: a sibling's
/// not-run result is written while the turn continues, so the model's next
/// call can recover from it.
fn unfinished_call(turn: &[Event]) -> Option<ToolCall> {
    let at = last_message_at(turn)?;
    for call in message_calls(&turn[at]) {
        let result = turn.iter().find_map(|e| {
            if e.kind != EventKind::ToolResult {
                return None;
            }
            let p = serde_json::from_value::<ToolResultPayload>(e.payload.clone()).ok()?;
            (p.result.id == call.id).then_some(p.policy)
        });
        match result {
            None => return Some(call),
            Some(policy) if not_run(&policy) => return Some(call),
            Some(_) => {}
        }
    }
    None
}

/// Whether a result's policy record says the call did not run: an
/// interrupt, a `suggest_project` sibling, the model's own output limit
/// (issue #96), or a resume's synthetic result.
fn not_run(policy: &Option<PolicyRecord>) -> bool {
    match policy {
        Some(PolicyRecord::Rule { rule, decision, .. }) => {
            rule == INTERRUPTED
                || rule == NOT_RUN_SOLO
                || rule == NOT_RUN_OVER_LIMIT
                || decision == "synthetic"
        }
        _ => false,
    }
}

pub(crate) fn author_name(author: &Author) -> String {
    match author {
        Author::User(u) => u.0.clone(),
        Author::Agent(a) => a.0.clone(),
        Author::System => "system".into(),
    }
}

fn describe_call(call: &ToolCall) -> String {
    let args = call.args.to_string();
    let args = truncate_for_display(&args, 1, 200);
    format!("{} {}", call.name, args)
}

/// The daemon's thread listing as `aigentic threads` and `/threads`
/// print it: id, date, event count, first line; newest first as listed.
pub fn render_thread_infos(threads: &[aigentic_api::ThreadInfo]) -> String {
    if threads.is_empty() {
        return "no threads".into();
    }
    let mut out = String::new();
    for t in threads {
        out.push_str(&format!(
            "{}  {}  {:>5}  {}\n",
            t.id,
            t.date,
            t.events,
            t.title.as_deref().unwrap_or(&t.first_line)
        ));
    }
    out.trim_end().to_owned()
}

/// `/threads` (issue #97): every readable thread, drawn in the order the
/// daemon sent it, under the headings its rows imply — the front thread,
/// the builds, then the rest grouped by workspace. It never re-sorts, so
/// `aigentic threads --server` still reaches for
/// [`render_thread_infos`], which is unchanged.
pub fn render_thread_groups(threads: &[aigentic_api::ThreadInfo], current: &str) -> String {
    use aigentic_api::ThreadKind;
    if threads.is_empty() {
        return "no threads".into();
    }
    let of = |pick: fn(&ThreadKind) -> bool| -> Vec<&ThreadInfo> {
        threads.iter().filter(|t| pick(&t.kind)).collect()
    };
    let front = of(|k| *k == ThreadKind::Front);
    let builds = of(|k| matches!(k, ThreadKind::Run(_)));
    let rest = of(|k| !matches!(k, ThreadKind::Front | ThreadKind::Run(_)));

    let mut out = String::new();
    if !front.is_empty() {
        out.push_str("front thread\n");
        for t in &front {
            out.push_str(&thread_group_row(t, 0, current));
        }
    }
    if !builds.is_empty() {
        out.push_str("builds\n");
        // The depth each run row was drawn at, so a child sits two spaces
        // further in than the row it names as its lead. A child whose
        // lead is not among the rows drawn is a root, at depth 0.
        let mut depth: HashMap<Ulid, usize> = HashMap::new();
        for t in &builds {
            let d = match t.kind {
                ThreadKind::Run(aigentic_api::RunThread::Child { lead, .. }) => {
                    depth.get(&lead).map(|d| d + 1).unwrap_or(0)
                }
                _ => 0,
            };
            depth.insert(t.id, d);
            out.push_str(&thread_group_row(t, d, current));
        }
    }
    if !rest.is_empty() {
        // One heading for the whole run when no workspace names any row,
        // else one at each change of workspace as the daemon grouped
        // them, contiguous.
        if rest.iter().all(|t| t.workspace.is_none()) {
            out.push_str("threads\n");
            for t in &rest {
                out.push_str(&thread_group_row(t, 0, current));
            }
        } else {
            let mut seen: Option<Option<&str>> = None;
            for t in &rest {
                let workspace = t.workspace.as_deref();
                if seen != Some(workspace) {
                    match workspace {
                        Some(w) => out.push_str(&format!("threads · {w}\n")),
                        None => out.push_str("threads · no workspace\n"),
                    }
                    seen = Some(workspace);
                }
                out.push_str(&thread_group_row(t, 0, current));
            }
        }
    }
    out.trim_end().to_owned()
}

/// One row of a group: today's columns, indented under its heading (two
/// spaces, plus two per build depth), with the `#issue` a lead carries,
/// the `[step]` a child carries when it names one, and the ` · project`
/// suffix for a row in another project (issue #97).
fn thread_group_row(t: &ThreadInfo, depth: usize, current: &str) -> String {
    use aigentic_api::{RunThread, ThreadKind};
    let mut label = String::new();
    match &t.kind {
        ThreadKind::Run(RunThread::Lead { issue }) => label.push_str(&format!("#{issue} ")),
        ThreadKind::Run(RunThread::Child {
            step: Some(step), ..
        }) => label.push_str(&format!("[{step}] ")),
        // A child with no step gets no prefix.
        ThreadKind::Run(RunThread::Child { step: None, .. }) => {}
        _ => {}
    }
    label.push_str(t.title.as_deref().unwrap_or(&t.first_line));
    let suffix = match &t.project {
        Some(p) if p != current => format!(" · {p}"),
        _ => String::new(),
    };
    format!(
        "{}{}  {}  {:>5}  {label}{suffix}\n",
        "  ".repeat(depth + 1),
        t.id,
        t.date,
        t.events
    )
}

/// `aigentic threads --server ...`: the project's threads over the API.
pub async fn list_threads_over(client: &Client, project: &str) -> anyhow::Result<String> {
    match client
        .request(Request::ListThreads {
            project: Some(project.to_owned()),
        })
        .await?
    {
        Response::Threads { threads } => Ok(render_thread_infos(&threads)),
        Response::Refused { reason } => anyhow::bail!("cannot list {project}: {reason}"),
        other => anyhow::bail!("unexpected reply listing {project}: {other:?}"),
    }
}

/// `aigentic project show --server ...`: the project report over the
/// API. The daemon renders it from a thread's runtime (the report is
/// the project's, the same for every thread of it), so the newest
/// thread is asked; a project with no thread yet has none to ask.
pub async fn project_report_over(client: &Client, project: &str) -> anyhow::Result<String> {
    let newest = match client
        .request(Request::ListThreads {
            project: Some(project.to_owned()),
        })
        .await?
    {
        Response::Threads { threads } => threads.into_iter().next().map(|t| t.id),
        Response::Refused { reason } => anyhow::bail!("cannot list {project}: {reason}"),
        other => anyhow::bail!("unexpected reply listing {project}: {other:?}"),
    };
    let Some(thread) = newest else {
        anyhow::bail!(
            "no thread in {project} on the daemon yet: the report is rendered from a thread's runtime, so start one first"
        );
    };
    match client
        .request(Request::Report {
            thread,
            report: ReportKind::Project,
        })
        .await?
    {
        Response::Text { text } => Ok(text),
        Response::Refused { reason } => anyhow::bail!("cannot report on {project}: {reason}"),
        other => anyhow::bail!("unexpected reply reporting on {project}: {other:?}"),
    }
}

/// The transcript a resumed thread's events give, for the banner: the
/// last few lines so a person sees where it was.
pub fn recent_lines(events: &[aigentic_runtime::aigentic_core::Event], n: usize) -> Vec<String> {
    let mut out: VecDeque<String> = VecDeque::new();
    for e in events {
        let text = match e.kind {
            EventKind::UserMessage => {
                serde_json::from_value::<UserMessagePayload>(e.payload.clone())
                    .ok()
                    .and_then(|p| {
                        p.blocks.into_iter().find_map(|b| match b {
                            ContentBlock::Text(t) => Some(t),
                            _ => None,
                        })
                    })
                    .map(|t| format!("{}: {}", author_name(&e.author), first_line(&t)))
            }
            _ => None,
        };
        if let Some(t) = text {
            out.push_back(t);
            if out.len() > n {
                out.pop_front();
            }
        }
    }
    out.into()
}

fn first_line(text: &str) -> String {
    let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let mut s: String = line.chars().take(72).collect();
    if s.chars().count() < line.chars().count() {
        s.push('…');
    }
    s
}

#[cfg(test)]
mod tests {
    //! The REPL against an embedded daemon with a scripted provider: the
    //! phase 3 REPL behaviour behind the client. No terminal, no network
    //! beyond the private socket.

    use super::*;
    use crate::app::rig::{config, open, project};
    use aigentic_api::client::Addr;
    use aigentic_runtime::aigentic_core::{
        CUT_STREAM, Capabilities, CompletionRequest, Event, Message, Provider, ProviderError,
        ProviderEvent, RiskClass, ToolCall as CoreToolCall, Usage,
    };
    use aigentic_server::build::{BuildError, ProviderFactory};
    use aigentic_server::{DefaultReports, Embedded, Server};
    use futures_core::Stream;
    use std::collections::VecDeque;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};

    struct Scripted(Mutex<VecDeque<Vec<ProviderEvent>>>);

    impl Provider for Scripted {
        fn complete(
            &self,
            _: &CompletionRequest<'_>,
        ) -> Pin<Box<dyn Stream<Item = ProviderEvent> + Send + '_>> {
            let events = self.0.lock().unwrap().pop_front().unwrap_or_default();
            Box::pin(futures_util::stream::iter(events))
        }
        fn count_tokens(&self, _: &[Message]) -> u64 {
            7
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                supports_tools: true,
                supports_images: false,
                supports_caching: false,
                supports_structured_output: false,
                max_context_tokens: 1000,
            }
        }
    }

    /// Builds one scripted provider and records the profile names it
    /// was asked for.
    struct Factory(Mutex<Option<Vec<Vec<ProviderEvent>>>>, Mutex<Vec<String>>);

    impl Factory {
        fn scripted(script: Vec<Vec<ProviderEvent>>) -> Arc<Self> {
            Arc::new(Self(Mutex::new(Some(script)), Mutex::new(Vec::new())))
        }
    }

    impl ProviderFactory for Factory {
        fn build(&self, profile: &str) -> Result<(Box<dyn Provider>, String), BuildError> {
            self.1.lock().unwrap().push(profile.to_owned());
            let script = self.0.lock().unwrap().take().unwrap_or_default();
            Ok((
                Box::new(Scripted(Mutex::new(script.into()))),
                "scripted".into(),
            ))
        }
    }

    fn text(t: &str) -> ProviderEvent {
        ProviderEvent::TextDelta(t.into())
    }
    fn done() -> ProviderEvent {
        ProviderEvent::Done {
            finish_reason: "stop".into(),
        }
    }

    /// A stream cut off with no end marker (issue #96/#114).
    fn cut() -> ProviderEvent {
        ProviderEvent::Done {
            finish_reason: aigentic_runtime::aigentic_core::CUT_STREAM.into(),
        }
    }
    fn call(id: &str, name: &str, args: serde_json::Value) -> ProviderEvent {
        ProviderEvent::ToolCall(ToolCall {
            id: id.into(),
            name: name.into(),
            args,
        })
    }
    fn tool_use() -> ProviderEvent {
        ProviderEvent::Done {
            finish_reason: "tool_use".into(),
        }
    }

    /// The chat thread waiting on a permission decision.
    fn approval_state(call_id: &str) -> ThreadState {
        ThreadState::AwaitingApproval {
            call_id: call_id.into(),
            call: CoreToolCall {
                id: call_id.into(),
                name: "bash".into(),
                args: serde_json::json!({ "command": "rm -rf /" }),
            },
            class: RiskClass::Exec,
            reason: "class exec: ask".into(),
        }
    }

    /// Notices until the state matches, with a moment for the other
    /// connections to have seen the same notice.
    async fn until_state(rx: &mut mpsc::Receiver<Notice>, pred: impl Fn(&ThreadState) -> bool) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let n = tokio::time::timeout_at(deadline, rx.recv())
                .await
                .expect("state in time")
                .expect("notices open");
            if let Notice::State { state, .. } = n
                && pred(&state)
            {
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    #[tokio::test]
    async fn the_repl_streams_a_reply_reports_and_quits_over_an_embedded_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let root = project(dir.path(), "proj", "");
        let cfg_dir = dir.path().join("cfg");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let script = vec![vec![text("Hej "), text("Steve!\nLine two"), done()]];
        let embedded = Server::embed_with(
            config(dir.path()),
            cfg_dir.clone(),
            root,
            "steve",
            None,
            Factory::scripted(script),
            Arc::new(DefaultReports {
                global_instructions: cfg_dir.join("instructions.md"),
            }),
        )
        .await
        .unwrap();
        assert_eq!(embedded.project, "proj");
        let (client, welcome) =
            Client::connect(&Addr::Unix(embedded.socket.clone()), &embedded.token)
                .await
                .unwrap();
        assert_eq!(welcome.user, "steve");
        let role = welcome.projects[0].role.clone();
        assert_eq!(role.as_deref(), Some("admin"));
        let (thread, state, mode) = open(&client, "proj", None).await;
        let notices = client.take_notices().unwrap();
        let mut repl = ClientRepl::new(
            client,
            thread,
            "steve",
            role,
            state,
            mode,
            Identity::default(),
            "proj",
        );
        let (tx, rx) = mpsc::unbounded_channel();
        // Lines arrive as a person would type them, with a pause for the
        // turn to finish before the reports.
        let feeder = async move {
            tx.send("hello".into()).unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            tx.send("/cost".into()).unwrap();
            tx.send("/who".into()).unwrap();
            tx.send("/mode".into()).unwrap();
            tx.send("/mode auto".into()).unwrap();
            tx.send("/queue".into()).unwrap();
            tx.send("/keys".into()).unwrap();
            tx.send("/nope".into()).unwrap();
            // The mode notice is asynchronous; let it land before quitting.
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            tx.send("/quit".into()).unwrap();
        };
        let mut out = Lines::default();
        let ((), ()) = tokio::join!(repl.run(rx, notices, &mut out), feeder);
        let lines = out.0;
        assert!(lines.contains(&"Hej Steve!".to_owned()), "{lines:#?}");
        assert!(lines.contains(&"Line two".to_owned()), "{lines:#?}");
        assert!(
            lines.iter().any(|l| l.starts_with("reported   in")),
            "{lines:#?}"
        );
        assert!(
            lines.iter().any(|l| l == "you: steve (admin)"),
            "{lines:#?}"
        );
        assert!(
            lines.iter().any(|l| l.starts_with("participants:")),
            "{lines:#?}"
        );
        assert!(
            lines.iter().any(|l| l.starts_with("[mode manual:")),
            "{lines:#?}"
        );
        assert!(lines.iter().any(|l| l == "[mode auto]"), "{lines:#?}");
        assert!(
            lines.iter().any(|l| l == "idle; nothing queued"),
            "{lines:#?}"
        );
        assert!(lines.iter().any(|l| l.starts_with("Enter ")), "{lines:#?}");
        assert!(
            lines.iter().any(|l| l == "unknown command: /nope"),
            "{lines:#?}"
        );
        drop(embedded);
    }

    /// A retry (issue #31) is a UI fact, not a transcript line: the
    /// turn line says `retrying 2/3 · not answering` while the call
    /// waits, and the paged transcript holds only the reply. The event
    /// is still in the log, so a later reader sees the retry here too.
    #[tokio::test]
    async fn a_retry_changes_the_turn_line_and_not_the_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let root = project(dir.path(), "proj", "");
        let cfg_dir = dir.path().join("cfg");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let script = vec![vec![
            ProviderEvent::Retried {
                attempt: 1,
                retries: 3,
                reason: "proj · not answering".into(),
                wait: std::time::Duration::from_secs(1),
            },
            text("Hej"),
            done(),
        ]];
        let embedded = Server::embed_with(
            config(dir.path()),
            cfg_dir.clone(),
            root,
            "steve",
            None,
            Factory::scripted(script),
            Arc::new(DefaultReports {
                global_instructions: cfg_dir.join("instructions.md"),
            }),
        )
        .await
        .unwrap();
        let (client, welcome) =
            Client::connect(&Addr::Unix(embedded.socket.clone()), &embedded.token)
                .await
                .unwrap();
        let role = welcome.projects[0].role.clone();
        let (thread, state, mode) = open(&client, "proj", None).await;
        let notices = client.take_notices().unwrap();
        let mut repl = ClientRepl::new(
            client,
            thread,
            "steve",
            role,
            state,
            mode,
            Identity::default(),
            "proj",
        );
        let (tx, rx) = mpsc::unbounded_channel();
        let (pacer, _) = Client::connect(&Addr::Unix(embedded.socket.clone()), &embedded.token)
            .await
            .unwrap();
        open(&pacer, "proj", Some(thread)).await;
        let mut paced = pacer.take_notices().unwrap();
        let feeder = async move {
            tx.send("hello".into()).unwrap();
            until_state(&mut paced, |s| *s == ThreadState::Idle).await;
            tx.send("/quit".into()).unwrap();
        };
        let mut out = Lines::default();
        let (_done, ()) = tokio::join!(repl.run(rx, notices, &mut out), feeder);
        let lines = out.0;
        assert!(
            lines.iter().any(|l| l.contains("Hej")),
            "the reply is in the transcript: {lines:#?}"
        );
        assert!(
            !lines.iter().any(|l| l.contains("not answering")),
            "a retry draws no transcript line: {lines:#?}"
        );
        // And the retry is in the log the transcript was built from.
        // Flat since #9: one log directory, not one per project.
        let events =
            aigentic_runtime::aigentic_log::ThreadLog::open(dir.path().join("threads"), thread)
                .unwrap()
                .read_all()
                .unwrap();
        assert!(
            events.iter().any(|e| e.kind == EventKind::ProviderRetried),
            "the retry is logged: {events:#?}"
        );
        drop(embedded);
    }

    /// An `ask_human` call with questions (issue #13): every question
    /// renders on the menu, one after another — a number picks an
    /// option, free text answers one without — and the whole call is
    /// answered in one go: the composed lines are the call's result.
    #[tokio::test]
    async fn ask_human_questions_are_answered_one_after_another_in_one_go() {
        let dir = tempfile::tempdir().unwrap();
        let root = project(dir.path(), "proj", "");
        let cfg_dir = dir.path().join("cfg");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let script = vec![
            vec![
                call(
                    "q1",
                    "ask_human",
                    serde_json::json!({"questions": [
                        {"question": "Which colour?", "header": "colour",
                         "options": [{"label": "Red", "description": "the warm one"},
                                     {"label": "Green"}]},
                        {"question": "Ship it?"}
                    ]}),
                ),
                tool_use(),
            ],
            vec![text("done"), done()],
        ];
        let embedded = Server::embed_with(
            config(dir.path()),
            cfg_dir.clone(),
            root,
            "steve",
            None,
            Factory::scripted(script),
            Arc::new(DefaultReports {
                global_instructions: cfg_dir.join("instructions.md"),
            }),
        )
        .await
        .unwrap();
        let addr = Addr::Unix(embedded.socket.clone());
        let (client, welcome) = Client::connect(&addr, &embedded.token).await.unwrap();
        let role = welcome.projects[0].role.clone();
        let (thread, state, mode) = open(&client, "proj", None).await;
        let notices = client.take_notices().unwrap();
        let mut repl = ClientRepl::new(
            client,
            thread,
            "steve",
            role,
            state,
            mode,
            Identity::default(),
            "proj",
        );
        let (pacer, _) = Client::connect(&addr, &embedded.token).await.unwrap();
        open(&pacer, "proj", Some(thread)).await;
        let mut paced = pacer.take_notices().unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        let feeder = async move {
            tx.send("pick one".into()).unwrap();
            until_state(
                &mut paced,
                |s| matches!(s, ThreadState::AwaitingHuman { call_id, .. } if call_id == "q1"),
            )
            .await;
            // The first question, by number; the second, without
            // options, as free text. No state notice separates them: the
            // menu holds the questions.
            tx.send("1".into()).unwrap();
            tx.send("yes, friday".into()).unwrap();
            until_state(&mut paced, |s| *s == ThreadState::Idle).await;
            tx.send("/quit".into()).unwrap();
        };
        let mut out = Lines::default();
        let ((), ()) = tokio::join!(repl.run(rx, notices, &mut out), feeder);
        let lines = out.0;
        let at = |needle: &str| {
            lines
                .iter()
                .position(|l| l == needle)
                .unwrap_or_else(|| panic!("no line {needle:?} in {lines:#?}"))
        };
        let first = at("[question] Which colour?");
        assert_eq!(lines[first + 1], "  1. Red · the warm one");
        assert_eq!(lines[first + 2], "  2. Green");
        assert_eq!(lines[first + 3], "  3. Other: type your own");
        // The choice is echoed, the second question follows, its free
        // text too, and the whole answer is the call's result.
        let echo = at("↳ colour: Red");
        let second = at("[question] Ship it?");
        let free = at("↳ yes, friday");
        assert!(first < echo && echo < second && second < free, "{lines:#?}");
        assert_eq!(lines[second + 1], "  type the answer");
        assert!(lines.contains(&"  └ colour: Red".to_owned()), "{lines:#?}");
        assert!(lines.contains(&"  └ yes, friday".to_owned()), "{lines:#?}");
        assert!(lines.contains(&"done".to_owned()), "{lines:#?}");
        drop(embedded);
    }

    /// `/copy n` across a turn's tool calls (issue #41, the real brief's
    /// shape): the first message ends in a closing fence with no
    /// newline and then calls a tool, so the next message's text would
    /// run into the fence without `reply_boundary`; the turn's last
    /// message is a plain sentence. `/copy 2` still finds both blocks.
    #[tokio::test]
    async fn copy_gets_the_nth_block_of_the_latest_turns_reply_across_tool_calls() {
        let dir = tempfile::tempdir().unwrap();
        let root = project(dir.path(), "proj", "");
        let cfg_dir = dir.path().join("cfg");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let one = "fn one() {}\nfn one_more() {}";
        let two = "let two = 2;\nlet two_more = 3;";
        // No trailing newline after the closing fence: the tool call is
        // the boundary.
        let block1 = format!("First block:\n\n```rust\n{one}\n```");
        // This fence closes before the turn's last sentence.
        let block2 = format!("Second block:\n\n```rust\n{two}\n```\n");
        let script = vec![
            vec![
                text(&block1),
                call(
                    "t1",
                    "update_tasks",
                    serde_json::json!({"tasks": [{"text": "write the tests", "state": "done"}]}),
                ),
                tool_use(),
            ],
            vec![text(&block2), text("Delivered above."), done()],
        ];
        let embedded = Server::embed_with(
            config(dir.path()),
            cfg_dir.clone(),
            root,
            "steve",
            None,
            Factory::scripted(script),
            Arc::new(DefaultReports {
                global_instructions: cfg_dir.join("instructions.md"),
            }),
        )
        .await
        .unwrap();
        let addr = Addr::Unix(embedded.socket.clone());
        let (client, welcome) = Client::connect(&addr, &embedded.token).await.unwrap();
        let role = welcome.projects[0].role.clone();
        let (thread, state, mode) = open(&client, "proj", None).await;
        let notices = client.take_notices().unwrap();
        let mut repl = ClientRepl::new(
            client,
            thread,
            "steve",
            role,
            state,
            mode,
            Identity::default(),
            "proj",
        );
        let (pacer, _) = Client::connect(&addr, &embedded.token).await.unwrap();
        open(&pacer, "proj", Some(thread)).await;
        let mut paced = pacer.take_notices().unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        let feeder = async move {
            // Before any turn there is nothing to copy.
            tx.send("/copy".into()).unwrap();
            tx.send("hello".into()).unwrap();
            until_state(&mut paced, |s| *s == ThreadState::Idle).await;
            tx.send("/copy 2".into()).unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            tx.send("/quit".into()).unwrap();
        };
        let mut caps = Copies::default();
        let ((), ()) = tokio::join!(repl.run(rx, notices, &mut caps), feeder);
        let lines = caps.0.0;
        assert!(
            lines.contains(
                &"[no assistant reply in this session yet: /copy covers turns since this session started]"
                    .to_owned()
            ),
            "the first /copy has no reply yet: {lines:#?}"
        );
        assert_eq!(
            caps.1,
            vec![two.to_owned()],
            "the second block, verbatim: {lines:#?}"
        );
        assert!(
            lines.contains(&"copied block 2 (2 lines)".to_owned()),
            "{lines:#?}"
        );
        drop(embedded);
    }

    /// `/copy` keeps the turn's reply across an `ask_human` answer
    /// (issue #41): block 1 is written before the question, block 2 in
    /// the continuation turn after it, and both survive — clearing on a
    /// state notice, or on the continuation turn, would lose block 1.
    #[tokio::test]
    async fn copy_keeps_the_reply_across_an_ask_human_answer() {
        let dir = tempfile::tempdir().unwrap();
        let root = project(dir.path(), "proj", "");
        let cfg_dir = dir.path().join("cfg");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let one = "fn before() {}";
        let two = "fn after() {}";
        let block1 = format!("Before the question:\n\n```rust\n{one}\n```");
        let block2 = format!("After the answer:\n\n```rust\n{two}\n```\n");
        let script = vec![
            vec![
                text(&block1),
                call(
                    "q1",
                    "ask_human",
                    serde_json::json!({"questions": [{"question": "Ship it?"}]}),
                ),
                tool_use(),
            ],
            vec![text(&block2), text("Done."), done()],
        ];
        let embedded = Server::embed_with(
            config(dir.path()),
            cfg_dir.clone(),
            root,
            "steve",
            None,
            Factory::scripted(script),
            Arc::new(DefaultReports {
                global_instructions: cfg_dir.join("instructions.md"),
            }),
        )
        .await
        .unwrap();
        let addr = Addr::Unix(embedded.socket.clone());
        let (client, welcome) = Client::connect(&addr, &embedded.token).await.unwrap();
        let role = welcome.projects[0].role.clone();
        let (thread, state, mode) = open(&client, "proj", None).await;
        let notices = client.take_notices().unwrap();
        let mut repl = ClientRepl::new(
            client,
            thread,
            "steve",
            role,
            state,
            mode,
            Identity::default(),
            "proj",
        );
        let (pacer, _) = Client::connect(&addr, &embedded.token).await.unwrap();
        open(&pacer, "proj", Some(thread)).await;
        let mut paced = pacer.take_notices().unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        let feeder = async move {
            tx.send("go".into()).unwrap();
            until_state(
                &mut paced,
                |s| matches!(s, ThreadState::AwaitingHuman { call_id, .. } if call_id == "q1"),
            )
            .await;
            tx.send("yes, friday".into()).unwrap();
            until_state(&mut paced, |s| *s == ThreadState::Idle).await;
            tx.send("/copy 1".into()).unwrap();
            tx.send("/copy".into()).unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            tx.send("/quit".into()).unwrap();
        };
        let mut caps = Copies::default();
        let ((), ()) = tokio::join!(repl.run(rx, notices, &mut caps), feeder);
        let lines = caps.0.0;
        assert_eq!(
            caps.1,
            vec![one.to_owned(), two.to_owned()],
            "block 1 before the question, block 2 after it: {lines:#?}"
        );
        assert!(
            lines.contains(&"copied block 1 (1 line)".to_owned())
                && lines.contains(&"copied block 2 (1 line)".to_owned()),
            "{lines:#?}"
        );
        drop(embedded);
    }

    /// Step 10 for one person: an `ask_human` question takes the next
    /// line as its answer and the turn continues; a `bash` call the rules
    /// ask about prompts, `n` denies it, and the decision prints with the
    /// user's own name. A second connection paces the typing on the
    /// thread's state, as a person would on the prompt.
    #[tokio::test]
    async fn a_question_and_a_permission_request_are_answered_from_the_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let root = project(dir.path(), "proj", "");
        let cfg_dir = dir.path().join("cfg");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let script = vec![
            vec![
                call(
                    "q1",
                    "ask_human",
                    serde_json::json!({"question": "which colour?"}),
                ),
                tool_use(),
            ],
            vec![
                text("blue it is\n"),
                call("b1", "bash", serde_json::json!({"command": "rm -rf x"})),
                tool_use(),
            ],
            vec![text("fine, not deleting"), done()],
        ];
        let embedded = Server::embed_with(
            config(dir.path()),
            cfg_dir.clone(),
            root,
            "steve",
            None,
            Factory::scripted(script),
            Arc::new(DefaultReports {
                global_instructions: cfg_dir.join("instructions.md"),
            }),
        )
        .await
        .unwrap();
        let addr = Addr::Unix(embedded.socket.clone());
        let (client, welcome) = Client::connect(&addr, &embedded.token).await.unwrap();
        let role = welcome.projects[0].role.clone();
        let (thread, state, mode) = open(&client, "proj", None).await;
        let notices = client.take_notices().unwrap();
        let mut repl = ClientRepl::new(
            client,
            thread,
            "steve",
            role,
            state,
            mode,
            Identity::default(),
            "proj",
        );
        // The pacer: a second session on the same thread.
        let (pacer, _) = Client::connect(&addr, &embedded.token).await.unwrap();
        open(&pacer, "proj", Some(thread)).await;
        let mut paced = pacer.take_notices().unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        let feeder = async move {
            tx.send("hello".into()).unwrap();
            until_state(
                &mut paced,
                |s| matches!(s, ThreadState::AwaitingHuman { call_id, .. } if call_id == "q1"),
            )
            .await;
            tx.send("blue".into()).unwrap();
            until_state(
                &mut paced,
                |s| matches!(s, ThreadState::AwaitingApproval { call_id, .. } if call_id == "b1"),
            )
            .await;
            tx.send("n".into()).unwrap();
            until_state(&mut paced, |s| *s == ThreadState::Idle).await;
            tx.send("/quit".into()).unwrap();
        };
        let mut out = Lines::default();
        let ((), ()) = tokio::join!(repl.run(rx, notices, &mut out), feeder);
        let lines = out.0;
        let at = |needle: &str| {
            lines
                .iter()
                .position(|l| l == needle)
                .unwrap_or_else(|| panic!("no line {needle:?} in {lines:#?}"))
        };
        let question = at("[question] which colour?");
        assert_eq!(lines[question + 1], "  type the answer");
        let permission = at("[permission] Run this command?");
        assert_eq!(lines[permission + 1], "  rm -rf x");
        assert_eq!(lines[permission + 2], "  1. Yes");
        assert_eq!(
            lines[permission + 3],
            "  2. Yes, and don't ask again for `rm -rf x` in this project"
        );
        assert_eq!(lines[permission + 4], "  3. No, and tell the agent why");
        // The feeder answered `n`: the choice is echoed, then the
        // decision prints.
        let echo = at("↳ No");
        let denied = at("  [denied by steve]");
        assert!(
            question < permission && permission < echo && echo < denied,
            "{lines:#?}"
        );
        assert!(
            lines.iter().any(|l| l.starts_with("• Failed ")),
            "the denied call's result: {lines:#?}"
        );
        assert!(
            lines.contains(&"fine, not deleting".to_owned()),
            "{lines:#?}"
        );
        assert!(
            !lines
                .iter()
                .any(|l| l.contains("elsewhere") || l.starts_with("[decided by")),
            "{lines:#?}"
        );
        drop(embedded);
    }

    /// `--profile` reaches the embedded daemon: every thread it builds
    /// uses that profile over the project's `[model] profile`, so item 8
    /// of the acceptance list can swap backends with the flag again.
    #[tokio::test]
    async fn the_profile_flag_wins_over_the_project_file_on_an_embedded_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let root = project(dir.path(), "proj", "[model]\nprofile = \"a\"\n");
        let cfg_dir = dir.path().join("cfg");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let factory = Factory::scripted(vec![vec![text("hi"), done()]]);
        let embedded = Server::embed_with(
            config(dir.path()),
            cfg_dir.clone(),
            root,
            "steve",
            Some("b"),
            factory.clone(),
            Arc::new(DefaultReports {
                global_instructions: cfg_dir.join("instructions.md"),
            }),
        )
        .await
        .unwrap();
        let (client, _) = Client::connect(&Addr::Unix(embedded.socket.clone()), &embedded.token)
            .await
            .unwrap();
        let (thread, _, _) = open(&client, "proj", None).await;
        let mut notices = client.take_notices().unwrap();
        let r = client
            .request(Request::Post {
                thread,
                blocks: vec![ContentBlock::Text("hello".into())],
                interrupt: false,
            })
            .await
            .unwrap();
        assert_eq!(r, Response::Ok);
        until_state(&mut notices, |s| *s == ThreadState::Idle).await;
        assert_eq!(*factory.1.lock().unwrap(), vec!["b".to_owned()]);
        drop(embedded);
    }

    /// A question answered on another connection: steve is prompted,
    /// magnus answers, and steve's prompt is withdrawn with
    /// `[answered by magnus]`, since the answer is magnus's event.
    #[tokio::test]
    async fn a_question_answered_elsewhere_is_withdrawn_with_the_answerers_name() {
        use aigentic_server::Listener;
        use aigentic_server::config::{ProjectConfig, UserConfig};

        let dir = tempfile::tempdir().unwrap();
        let root = project(
            dir.path(),
            "p",
            "[participants]\nsteve = \"admin\"\nmagnus = \"write\"\n",
        );
        let cfg_dir = dir.path().join("cfg");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let script = vec![
            vec![
                call(
                    "q1",
                    "ask_human",
                    serde_json::json!({"question": "which colour?"}),
                ),
                tool_use(),
            ],
            vec![text("blue it is"), done()],
        ];
        let server_config = aigentic_server::ServerConfig {
            listen: "unix".into(),
            idle_unload_secs: 600,
            // An embedded daemon resumes nothing on its own (#58, rule 8).
            resume_runs: false,
            users: ["steve", "magnus"]
                .iter()
                .map(|n| UserConfig {
                    name: (*n).to_owned(),
                    token_env: None,
                    token: Some(format!("tok-{n}")),
                })
                .collect(),
            projects: vec![ProjectConfig {
                name: "p".into(),
                root,
            }],
        };
        let server = Arc::new(Server::new(
            config(dir.path()),
            cfg_dir.clone(),
            server_config,
            Factory::scripted(script),
            Arc::new(DefaultReports {
                global_instructions: cfg_dir.join("instructions.md"),
            }),
        ));
        let socket = dir.path().join("d.sock");
        let task = tokio::spawn(server.clone().serve(Listener::Unix(socket.clone())));
        for _ in 0..200 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let addr = Addr::Unix(socket);
        let (steve, welcome) = Client::connect(&addr, "tok-steve").await.unwrap();
        let role = welcome.projects[0].role.clone();
        let (thread, state, mode) = open(&steve, "p", None).await;
        let notices = steve.take_notices().unwrap();
        let mut repl = ClientRepl::new(
            steve,
            thread,
            "steve",
            role,
            state,
            mode,
            Identity::default(),
            "proj",
        );
        let (magnus, _) = Client::connect(&addr, "tok-magnus").await.unwrap();
        open(&magnus, "p", Some(thread)).await;
        let mut magnus_notices = magnus.take_notices().unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        let driver = async move {
            tx.send("pick one".into()).unwrap();
            until_state(
                &mut magnus_notices,
                |s| matches!(s, ThreadState::AwaitingHuman { call_id, .. } if call_id == "q1"),
            )
            .await;
            let r = magnus
                .request(Request::AnswerHuman {
                    thread,
                    call_id: "q1".into(),
                    text: "blue".into(),
                })
                .await
                .unwrap();
            assert_eq!(r, Response::Ok);
            until_state(&mut magnus_notices, |s| *s == ThreadState::Idle).await;
            tx.send("/quit".into()).unwrap();
        };
        let mut out = Lines::default();
        let ((), ()) = tokio::join!(repl.run(rx, notices, &mut out), driver);
        let lines = out.0;
        let at = |needle: &str| {
            lines
                .iter()
                .position(|l| l == needle)
                .unwrap_or_else(|| panic!("no line {needle:?} in {lines:#?}"))
        };
        let question = at("[question] which colour?");
        let answered = at("[answered by magnus]");
        assert!(question < answered, "{lines:#?}");
        assert!(lines.contains(&"  └ blue".to_owned()), "{lines:#?}");
        assert!(lines.contains(&"blue it is".to_owned()), "{lines:#?}");
        assert!(
            !lines.iter().any(|l| l == "[answered elsewhere]"),
            "{lines:#?}"
        );
        task.abort();
    }

    /// `aigentic threads` and `project show` over the API: the listing
    /// is the daemon's, newest first with the first line; the project
    /// report comes from the newest thread's runtime, and a project
    /// without a thread says so.
    #[tokio::test]
    async fn threads_and_the_project_report_come_over_the_api() {
        let dir = tempfile::tempdir().unwrap();
        let root = project(dir.path(), "proj", "");
        let cfg_dir = dir.path().join("cfg");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let embedded = Server::embed_with(
            config(dir.path()),
            cfg_dir.clone(),
            root,
            "steve",
            None,
            Factory::scripted(vec![vec![text("hi"), done()]]),
            Arc::new(DefaultReports {
                global_instructions: cfg_dir.join("instructions.md"),
            }),
        )
        .await
        .unwrap();
        let (client, _) = Client::connect(&Addr::Unix(embedded.socket.clone()), &embedded.token)
            .await
            .unwrap();
        assert_eq!(
            list_threads_over(&client, "proj").await.unwrap(),
            "no threads"
        );
        let err = project_report_over(&client, "proj").await.unwrap_err();
        assert!(err.to_string().contains("no thread in proj"), "{err}");
        assert!(
            list_threads_over(&client, "nope")
                .await
                .unwrap_err()
                .to_string()
                .contains("cannot list nope"),
        );
        let (thread, _, _) = open(&client, "proj", None).await;
        let mut notices = client.take_notices().unwrap();
        client
            .request(Request::Post {
                thread,
                blocks: vec![ContentBlock::Text("first words".into())],
                interrupt: false,
            })
            .await
            .unwrap();
        until_state(&mut notices, |s| *s == ThreadState::Idle).await;
        let listing = list_threads_over(&client, "proj").await.unwrap();
        assert!(
            listing.starts_with(&thread.to_string()) && listing.ends_with("first words"),
            "{listing}"
        );
        // T3 (issue #97): a per-project listing is still the flat
        // `render_thread_infos`, with no group heading.
        let Response::Threads { threads } = client
            .request(Request::ListThreads {
                project: Some("proj".into()),
            })
            .await
            .unwrap()
        else {
            panic!("a threads reply")
        };
        assert!(!threads.is_empty(), "non-empty, so a heading would show");
        assert_eq!(listing, render_thread_infos(&threads));
        assert!(
            !listing.contains("front thread")
                && !listing.contains("builds")
                && !listing.contains("threads"),
            "{listing}"
        );
        let report = project_report_over(&client, "proj").await.unwrap();
        assert!(report.starts_with("project proj at "), "{report}");
        drop(embedded);
    }

    /// Step 10 for two people: steve (admin) is prompted and magnus
    /// (approve) decides first on another connection, so steve's prompt
    /// is withdrawn with `[decided by magnus]` and the decision prints
    /// as `[allowed by magnus]`; the reviewer (read) is never prompted
    /// and sees who the thread waits for.
    #[tokio::test]
    async fn a_prompt_decided_elsewhere_is_withdrawn_and_a_reader_only_watches() {
        use aigentic_server::Listener;
        use aigentic_server::config::{ProjectConfig, UserConfig};

        let dir = tempfile::tempdir().unwrap();
        let root = project(
            dir.path(),
            "p",
            "[participants]\nsteve = \"admin\"\nmagnus = \"approve\"\nreviewer = \"read\"\n",
        );
        let cfg_dir = dir.path().join("cfg");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let script = vec![
            vec![
                call("b1", "bash", serde_json::json!({"command": "printf ok"})),
                tool_use(),
            ],
            vec![text("ran"), done()],
        ];
        let server_config = aigentic_server::ServerConfig {
            listen: "unix".into(),
            idle_unload_secs: 600,
            // An embedded daemon resumes nothing on its own (#58, rule 8).
            resume_runs: false,
            users: ["steve", "magnus", "reviewer"]
                .iter()
                .map(|n| UserConfig {
                    name: (*n).to_owned(),
                    token_env: None,
                    token: Some(format!("tok-{n}")),
                })
                .collect(),
            projects: vec![ProjectConfig {
                name: "p".into(),
                root,
            }],
        };
        let server = Arc::new(Server::new(
            config(dir.path()),
            cfg_dir.clone(),
            server_config,
            Factory::scripted(script),
            Arc::new(DefaultReports {
                global_instructions: cfg_dir.join("instructions.md"),
            }),
        ));
        let socket = dir.path().join("d.sock");
        let task = tokio::spawn(server.clone().serve(Listener::Unix(socket.clone())));
        for _ in 0..200 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let addr = Addr::Unix(socket);
        let connect = |user: &'static str| {
            let addr = addr.clone();
            async move {
                Client::connect(&addr, &format!("tok-{user}"))
                    .await
                    .unwrap()
            }
        };

        let (steve, welcome) = connect("steve").await;
        let steve_role = welcome.projects[0].role.clone();
        let (thread, state, mode) = open(&steve, "p", None).await;
        let steve_notices = steve.take_notices().unwrap();
        let mut steve_repl = ClientRepl::new(
            steve,
            thread,
            "steve",
            steve_role,
            state,
            mode,
            Identity::default(),
            "proj",
        );

        let (reviewer, welcome) = connect("reviewer").await;
        let reviewer_role = welcome.projects[0].role.clone();
        assert_eq!(reviewer_role.as_deref(), Some("read"));
        let (_, state, mode) = open(&reviewer, "p", Some(thread)).await;
        let reviewer_notices = reviewer.take_notices().unwrap();
        let mut reviewer_repl = ClientRepl::new(
            reviewer,
            thread,
            "reviewer",
            reviewer_role,
            state,
            mode,
            Identity::default(),
            "proj",
        );

        let (magnus, _) = connect("magnus").await;
        open(&magnus, "p", Some(thread)).await;
        let mut magnus_notices = magnus.take_notices().unwrap();

        let (steve_tx, steve_rx) = mpsc::unbounded_channel();
        let (reviewer_tx, reviewer_rx) = mpsc::unbounded_channel();
        let driver = async move {
            steve_tx.send("go".into()).unwrap();
            until_state(
                &mut magnus_notices,
                |s| matches!(s, ThreadState::AwaitingApproval { call_id, .. } if call_id == "b1"),
            )
            .await;
            let r = magnus
                .request(Request::Decide {
                    thread,
                    call_id: "b1".into(),
                    allow: true,
                    session: false,
                    prefix: None,
                    reason: None,
                })
                .await
                .unwrap();
            assert_eq!(r, Response::Ok);
            until_state(&mut magnus_notices, |s| *s == ThreadState::Idle).await;
            // Steve's late `y` loses the race, without a stale prompt.
            steve_tx.send("y".into()).unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            steve_tx.send("/quit".into()).unwrap();
            reviewer_tx.send("/quit".into()).unwrap();
        };
        let mut steve_out = Lines::default();
        let mut reviewer_out = Lines::default();
        let ((), (), ()) = tokio::join!(
            steve_repl.run(steve_rx, steve_notices, &mut steve_out),
            reviewer_repl.run(reviewer_rx, reviewer_notices, &mut reviewer_out),
            driver
        );
        let lines = steve_out.0;
        let at = |needle: &str| {
            lines
                .iter()
                .position(|l| l.starts_with(needle))
                .unwrap_or_else(|| panic!("no line {needle:?} in {lines:#?}"))
        };
        let prompt = at("[permission] Run this command?");
        let withdrawn = at("[decided by magnus]");
        let allowed = at("  [allowed by magnus]");
        assert!(prompt < withdrawn && withdrawn + 1 == allowed, "{lines:#?}");
        assert!(lines.contains(&"  └ ok".to_owned()), "{lines:#?}");
        assert!(lines.contains(&"ran".to_owned()), "{lines:#?}");
        // The prompt was withdrawn, so the late `y` was a chat line the
        // daemon took as a post, not a decision of a closed request.
        assert!(
            !lines.iter().any(|l| l.contains("already decided")),
            "{lines:#?}"
        );
        let lines = reviewer_out.0;
        assert!(
            lines.contains(
                &"[waiting for an approver: bash {\"command\":\"printf ok\"}]".to_owned()
            ),
            "{lines:#?}"
        );
        assert!(
            lines.contains(&"  [allowed by magnus]".to_owned()),
            "{lines:#?}"
        );
        assert!(
            !lines
                .iter()
                .any(|l| l.starts_with("[permission]") || l.starts_with("[decided by")),
            "{lines:#?}"
        );
        assert!(lines.contains(&"steve: go".to_owned()), "{lines:#?}");
        task.abort();
    }

    #[tokio::test]
    async fn p_allows_the_prefix_from_now_on_and_n_with_a_reason_tells_the_model() {
        let dir = tempfile::tempdir().unwrap();
        let root = project(dir.path(), "proj", "[memory]\nenabled = false\n");
        let cfg_dir = dir.path().join("cfg");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        // `uname -a` is local (no network) and absent from the policy's
        // read-only allow list and no-op list, so it asks. Its prefix is
        // the whole two-word command (a bare flag never looks like a
        // value), so the grant still covers the riskiest segment's
        // prefix (#16).
        let probe = || call("c", "bash", serde_json::json!({"command": "uname -a"}));
        let script = vec![
            // Turn 1: uname asks; `p` allows `uname -a` from now on.
            vec![probe(), tool_use()],
            vec![text("fetched"), done()],
            // Turn 2: the same call runs without asking.
            vec![probe(), tool_use()],
            vec![text("fetched again"), done()],
            // Turn 3: a different command asks; `n why` denies with a reason.
            vec![
                call("d", "bash", serde_json::json!({"command": "rm -rf build"})),
                tool_use(),
            ],
            vec![text("ok"), done()],
        ];
        let embedded = Server::embed_with(
            config(dir.path()),
            cfg_dir.clone(),
            root.clone(),
            "steve",
            None,
            Factory::scripted(script),
            Arc::new(DefaultReports {
                global_instructions: cfg_dir.join("instructions.md"),
            }),
        )
        .await
        .unwrap();
        let (client, welcome) =
            Client::connect(&Addr::Unix(embedded.socket.clone()), &embedded.token)
                .await
                .unwrap();
        let role = welcome.projects[0].role.clone();
        let (thread, state, mode) = open(&client, "proj", None).await;
        let notices = client.take_notices().unwrap();
        // `repl.run` owns the first connection's notices, so a second
        // connection watches the same thread's state — the same pattern
        // as `a_prompt_decided_elsewhere_is_withdrawn_and_a_reader_only_watches`.
        let (watcher, _) = Client::connect(&Addr::Unix(embedded.socket.clone()), &embedded.token)
            .await
            .unwrap();
        open(&watcher, "proj", Some(thread)).await;
        let watcher_notices = watcher.take_notices().unwrap();
        let mut repl = ClientRepl::new(
            client,
            thread,
            "steve",
            role,
            state,
            mode,
            Identity::default(),
            "proj",
        );
        let (tx, rx) = mpsc::unbounded_channel();
        let feeder = async move {
            // The watcher must outlive the feeder's waits: dropping it
            // closes the notice stream.
            let _watcher = watcher;
            let mut notices = watcher_notices;
            // Every keystroke waits for the state it answers, so the
            // test never races a sleep.
            tx.send("one".into()).unwrap();
            until_state(
                &mut notices,
                |s| matches!(s, ThreadState::AwaitingApproval { call_id, .. } if call_id == "c"),
            )
            .await;
            tx.send("p".into()).unwrap();
            until_state(&mut notices, |s| *s == ThreadState::Idle).await;
            tx.send("two".into()).unwrap();
            // Running-then-Idle proves turn 2 ran to completion without
            // an ask; a bare second Idle could match a queued notice.
            until_state(&mut notices, |s| matches!(s, ThreadState::Running { .. })).await;
            until_state(&mut notices, |s| *s == ThreadState::Idle).await;
            tx.send("three".into()).unwrap();
            until_state(
                &mut notices,
                |s| matches!(s, ThreadState::AwaitingApproval { call_id, .. } if call_id == "d"),
            )
            .await;
            tx.send("n the build directory is shared".into()).unwrap();
            until_state(&mut notices, |s| *s == ThreadState::Idle).await;
            tx.send("/quit".into()).unwrap();
        };
        let mut out = Lines::default();
        let ((), ()) = tokio::join!(repl.run(rx, notices, &mut out), feeder);
        let lines = out.0;
        let asks = lines
            .iter()
            .filter(|l| l.starts_with("[permission] Run this command?"))
            .count();
        assert_eq!(asks, 2, "uname asked once, rm once: {lines:#?}");
        assert!(
            lines
                .iter()
                .any(|l| l.contains("don't ask again for `uname -a` in this project")),
            "{lines:#?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l == "↳ Yes, and don't ask again for `uname -a` in this project"),
            "the choice is echoed into the record: {lines:#?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("denied by steve: the build directory is shared")),
            "{lines:#?}"
        );
        let rules = std::fs::read_to_string(root.join(".aigentic/rules.toml")).unwrap();
        assert!(rules.contains("\"uname -a\""), "{rules}");
    }

    /// The checklist (issue #21): a finished item checks off into the
    /// scrollback, one line at a time — never the whole list at once,
    /// which the live block shows three rows of.
    #[tokio::test]
    async fn finished_tasks_check_off_into_the_scrollback() {
        let dir = tempfile::tempdir().unwrap();
        let root = project(dir.path(), "proj", "");
        let cfg_dir = dir.path().join("cfg");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let pending = |a: &str, b: &str, c: &str| {
            serde_json::json!({"tasks": [
                {"text": a, "state": "pending"},
                {"text": b, "state": "pending"},
                {"text": c, "state": "pending"},
            ]})
        };
        let script = vec![
            vec![
                text("Three things.\n"),
                call(
                    "t1",
                    "update_tasks",
                    pending("read the plan", "write the code", "run the gate"),
                ),
                tool_use(),
            ],
            vec![
                call("b1", "bash", serde_json::json!({"command": "echo hi"})),
                call(
                    "t2",
                    "update_tasks",
                    serde_json::json!({"tasks": [
                        {"text": "read the plan", "state": "done"},
                        {"text": "write the code", "state": "active"},
                        {"text": "run the gate", "state": "pending"},
                    ]}),
                ),
                tool_use(),
            ],
            vec![
                call(
                    "t3",
                    "update_tasks",
                    serde_json::json!({"tasks": [
                        {"text": "read the plan", "state": "done"},
                        {"text": "write the code", "state": "done"},
                        {"text": "run the gate", "state": "active"},
                    ]}),
                ),
                tool_use(),
            ],
            vec![
                ProviderEvent::Usage(Usage {
                    input_tokens: 4_200,
                    output_tokens: 4_000,
                    ..Default::default()
                }),
                text("All set.\n"),
                done(),
            ],
        ];
        let embedded = Server::embed_with(
            config(dir.path()),
            cfg_dir.clone(),
            root,
            "steve",
            None,
            Factory::scripted(script),
            Arc::new(DefaultReports {
                global_instructions: cfg_dir.join("instructions.md"),
            }),
        )
        .await
        .unwrap();
        let (client, welcome) =
            Client::connect(&Addr::Unix(embedded.socket.clone()), &embedded.token)
                .await
                .unwrap();
        let role = welcome.projects[0].role.clone();
        let (thread, state, mode) = open(&client, "proj", None).await;
        let notices = client.take_notices().unwrap();
        let mut repl = ClientRepl::new(
            client,
            thread,
            "steve",
            role,
            state,
            mode,
            Identity::default(),
            "proj",
        );
        let (tx, rx) = mpsc::unbounded_channel();
        let feeder = async move {
            tx.send("go".into()).unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(900)).await;
            tx.send("/cost".into()).unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            tx.send("/quit".into()).unwrap();
        };
        let mut out = Lines::default();
        let ((), ()) = tokio::join!(repl.run(rx, notices, &mut out), feeder);
        let lines = out.0;
        assert!(
            lines.contains(&"✓ read the plan".to_owned())
                && lines.contains(&"✓ write the code".to_owned()),
            "each finished item, one line: {lines:#?}"
        );
        assert!(
            !lines.iter().any(|l| l.starts_with("• Tasks")),
            "the whole list is a pager away, not printed: {lines:#?}"
        );
        // The turn's totals moved here (issue #21), off the live line:
        // what was written, how many calls it took. Harness task
        // updates are bookkeeping, not calls.
        assert!(
            lines.iter().any(|l| l == "turn: 4.0k written · 1 call"),
            "the turn's cost is reported by /cost: {lines:#?}"
        );
        drop(embedded);
    }

    /// The turn's summary line carries what the message cost, the way
    /// the rig's `[profiles.a.prices]` prices it (issue #110): the
    /// engine folds the usages the runtime stamped, and the last
    /// summary cell in the transcript says the dollars and the cache
    /// share the fixture's numbers give.
    #[tokio::test]
    async fn the_turn_summary_line_shows_the_turns_dollars_and_cache_share() {
        use aigentic_runtime::aigentic_log::Usage as LogUsage;

        let dir = tempfile::tempdir().unwrap();
        let root = project(dir.path(), "proj", "");
        let cfg_dir = dir.path().join("cfg");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let usage = |input: u64, cache_read: u64, output: u64| Usage {
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: cache_read,
            cache_write_tokens: 0,
            reasoning_tokens: None,
        };
        let first = usage(1_000, 3_000, 200);
        let second = usage(2_000, 0, 400);
        let script = vec![
            vec![
                call("b1", "bash", serde_json::json!({"command": "echo one"})),
                call("b2", "bash", serde_json::json!({"command": "echo two"})),
                ProviderEvent::Usage(first),
                tool_use(),
            ],
            vec![text("Both done.\n"), ProviderEvent::Usage(second), done()],
        ];
        let embedded = Server::embed_with(
            config(dir.path()),
            cfg_dir.clone(),
            root,
            "steve",
            None,
            Factory::scripted(script),
            Arc::new(DefaultReports {
                global_instructions: cfg_dir.join("instructions.md"),
            }),
        )
        .await
        .unwrap();
        let (client, welcome) =
            Client::connect(&Addr::Unix(embedded.socket.clone()), &embedded.token)
                .await
                .unwrap();
        let role = welcome.projects[0].role.clone();
        let (thread, state, mode) = open(&client, "proj", None).await;
        let notices = client.take_notices().unwrap();
        let mut repl = ClientRepl::new(
            client,
            thread,
            "steve",
            role,
            state,
            mode,
            Identity::default(),
            "proj",
        );
        let (tx, rx) = mpsc::unbounded_channel();
        let feeder = async move {
            tx.send("go".into()).unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(900)).await;
            tx.send("/quit".into()).unwrap();
        };
        let mut out = Lines::default();
        let ((), ()) = tokio::join!(repl.run(rx, notices, &mut out), feeder);
        let lines = out.0;
        // The rig's `[profiles.a.prices]`, the same table the runtime
        // stamped the calls with; the expected dollars come from it and
        // the fixture's usages, never by hand.
        let prices = aigentic_runtime::Prices {
            input: 3.0,
            cache_read: 0.3,
            cache_write: 3.0,
            output: 15.0,
        };
        let spent = prices.cost_usd(&LogUsage::reported(first))
            + prices.cost_usd(&LogUsage::reported(second));
        let prompt = 1_000 + 3_000 + 2_000;
        let share = 100.0 * 3_000.0 / prompt as f64;
        let expected = format!(
            "· {} · {share:.0}% cached",
            crate::stats::money(Some(spent), None)
        );
        let summary = lines
            .iter()
            .rev()
            .find(|l| l.trim_start().starts_with("─ "))
            .unwrap_or_else(|| panic!("a turn summary in {lines:#?}"));
        assert!(summary.ends_with(&expected), "{summary:?}");
        drop(embedded);
    }

    #[test]
    fn turn_figures_read_short() {
        let mut t = TurnStats::new();
        // State and time only (issue #21): the verb, the clock, the
        // calls — the command stays on the in-flight row above, token
        // totals move to `/cost`.
        assert_eq!(t.activity(), "thinking");
        t.writing = true;
        assert_eq!(t.activity(), "writing");
        t.writing = false;
        t.tools = 4;
        t.output = 1_840;
        t.current = Some("bash cargo test -p aigentic-server".into());
        assert_eq!(t.activity(), "running");
        let f = t.figures();
        assert!(f.ends_with("4 tools"), "{f}");
        assert!(!f.contains("cargo"), "{f}");
        assert!(!f.contains("1.8k"), "{f}");
        t.tools = 1;
        assert!(t.figures().ends_with("1 tool"), "{}", t.figures());
        assert_eq!(crate::app::status::count_short(950), "950");
        assert_eq!(crate::app::status::count_short(1_300_000), "1.3M");
    }

    /// The finished summary (issue #110) adds what the message cost,
    /// how many of its calls went unpriced and the cached share to the
    /// `figures()` text; the running line keeps `figures()` alone.
    #[test]
    fn the_turn_summary_adds_the_cost_the_unpriced_calls_and_the_cache_share() {
        use aigentic_runtime::aigentic_log::Usage as LogUsage;

        let stamp =
            |input: u64, cache_read: u64, cache_write: u64, output: u64, usd: Option<f64>| {
                LogUsage {
                    input_tokens: input,
                    output_tokens: output,
                    cache_read_tokens: cache_read,
                    cache_write_tokens: cache_write,
                    reasoning_tokens: None,
                    estimated: false,
                    profile: None,
                    model: None,
                    effort: None,
                    latency_ms: None,
                    ttft_ms: None,
                    cost_usd: usd,
                }
            };
        // Two priced calls, one call the runtime stamped no price on,
        // and an estimated call that carries a fabricated `cost_usd`:
        // the estimated stamp must stay out of the dollars.
        let usages = vec![
            stamp(1_000, 0, 0, 500, Some(0.25)),
            stamp(2_000, 3_000, 1_000, 200, Some(0.50)),
            stamp(100, 0, 0, 10, None),
            LogUsage {
                estimated: true,
                ..stamp(50, 0, 0, 5, Some(9.99))
            },
        ];
        let mut t = TurnStats::new();
        t.tools = 2;
        for u in &usages {
            t.add_usage(u);
        }
        // Expected values from the fixture, never by hand.
        let spent: f64 = usages
            .iter()
            .filter(|u| !u.estimated && u.cost_usd.is_some())
            .map(|u| u.cost_usd.unwrap())
            .sum();
        let unpriced = usages
            .iter()
            .filter(|u| u.estimated || u.cost_usd.is_none())
            .count();
        let prompt: u64 = usages
            .iter()
            .map(|u| u.input_tokens + u.cache_read_tokens + u.cache_write_tokens)
            .sum();
        let cached: u64 = usages.iter().map(|u| u.cache_read_tokens).sum();
        let share = 100.0 * cached as f64 / prompt as f64;
        let expected = format!(
            " · {} · {unpriced} unpriced · {share:.0}% cached",
            crate::stats::money(Some(spent), None)
        );
        assert!(t.summary().ends_with(&expected), "{}", t.summary());
        // The running line is unchanged: state, clock and calls only.
        assert!(t.figures().ends_with(" · 2 tools"), "{}", t.figures());
        assert!(!t.figures().contains('$'), "{}", t.figures());
        assert!(!t.figures().contains("cached"), "{}", t.figures());
        assert!(!t.figures().contains("unpriced"), "{}", t.figures());

        // No priced call: no dollar part at all.
        let mut free = TurnStats::new();
        free.add_usage(&stamp(100, 0, 0, 10, None));
        assert!(!free.summary().contains('$'), "{}", free.summary());
        assert!(free.summary().ends_with("1 unpriced"), "{}", free.summary());

        // A backend reporting no cache fields drops the cache part
        // rather than claiming `0%`.
        let mut plain = TurnStats::new();
        plain.add_usage(&stamp(100, 0, 0, 10, Some(0.01)));
        assert!(
            !plain.summary().contains("cached"),
            "no cache fields, no share: {}",
            plain.summary()
        );
        // And a cache write alone counts as seen, so the share shows.
        let mut writer = TurnStats::new();
        writer.add_usage(&stamp(100, 0, 50, 10, Some(0.01)));
        assert!(
            writer.summary().ends_with("0% cached"),
            "{}",
            writer.summary()
        );
    }

    /// A retry outranks "thinking" on the turn line (issue #31), and
    /// counts its wait down (issue #90): a call waiting on a dead
    /// provider says so, with the attempt, the profile that will be
    /// tried again and the time left, then `trying now` once the wait is
    /// over and the attempt is in flight.
    #[test]
    fn a_retry_shows_on_the_turn_line() {
        let mut t = TurnStats::new();
        t.retry = Some(RetryWait {
            attempt: 2,
            retries: 3,
            reason: "tensorx · not answering".into(),
            wait_ms: 0,
            until: std::time::Instant::now() + std::time::Duration::from_secs(90),
        });
        let line = t.activity();
        assert!(
            line.starts_with("retrying 2/3 · tensorx · not answering · next in "),
            "{line}"
        );
        // The countdown itself, rendered by the same helper the line
        // uses, and `trying now` once the wait is over.
        let left = std::time::Duration::from_secs(90);
        assert_eq!(
            retry_countdown(Some(left)),
            format!("next in {}", crate::app::status::elapsed_short(left))
        );
        assert_eq!(retry_countdown(None), "trying now");
        t.retry = Some(RetryWait {
            attempt: 2,
            retries: 3,
            reason: "tensorx · not answering".into(),
            wait_ms: 0,
            until: std::time::Instant::now(),
        });
        assert!(t.activity().ends_with("trying now"), "{}", t.activity());
        // Content arriving is the recovery: the reason goes away.
        t.writing = true;
        t.retry = None;
        assert_eq!(t.activity(), "writing");
        // And a running tool keeps the row above, not this line.
        t.retry = Some(RetryWait {
            attempt: 1,
            retries: 3,
            reason: "tensorx · not answering".into(),
            wait_ms: 0,
            until: std::time::Instant::now() + std::time::Duration::from_secs(1),
        });
        t.current = Some("bash cargo test".into());
        assert_eq!(t.activity(), "running");
    }

    /// A `turn_ended` payload as the runtime writes one (issue #47).
    fn slept_payload(wall: u64, slept: u64, awaiting: u64, keep: Option<&str>) -> TurnEndedPayload {
        TurnEndedPayload {
            reason: "done".into(),
            touched: Vec::new(),
            wall_secs: Some(wall),
            slept_secs: Some(slept),
            slept_awaiting_secs: Some(awaiting),
            keep_awake: keep.map(str::to_owned),
            error: None,
        }
    }

    /// A provider-error `turn_ended` payload as the runtime writes one
    /// (issue #22): the machine `reason` string plus the structured
    /// error.
    fn provider_error_payload(status: u16, body: &str) -> TurnEndedPayload {
        let error = ProviderError::Http {
            status,
            body: body.to_owned(),
        };
        TurnEndedPayload {
            reason: format!("provider_error: {error}"),
            error: Some(error),
            ..TurnEndedPayload::new("")
        }
    }

    /// T12 (issue #47): the issue's own case — 750 s of wall over no
    /// running time, the guard on. One line, whole minutes, and no
    /// advice: nothing about this nap was avoidable.
    #[test]
    fn a_slept_turn_says_the_minutes() {
        let line =
            slept_line(&slept_payload(750, 750, 0, Some("on"))).expect("a slept turn says so");
        assert_eq!(
            line,
            format!("the machine slept {} min during this turn", 750 / 60)
        );
    }

    /// T13 (issue #47): with the guard off, and the sleep somewhere
    /// other than a human wait, the line says what to do; with the whole
    /// nap inside a wait, nothing is advised, because enabling the guard
    /// could not have prevented it. Expected strings are built here from
    /// the payload's own values.
    #[test]
    fn a_slept_turn_blames_the_guard_only_when_it_could_have_helped() {
        let wall = 750;
        let line =
            slept_line(&slept_payload(wall, wall, 0, Some("off"))).expect("a slept turn says so");
        assert_eq!(
            line,
            format!(
                "the machine slept {} min during this turn; set keep_awake = true to prevent this",
                wall / 60
            )
        );
        let line = slept_line(&slept_payload(wall, wall, wall, Some("off")))
            .expect("a slept turn says so");
        assert_eq!(
            line,
            format!("the machine slept {} min during this turn", wall / 60),
            "a nap parked on a person: the guard could not have prevented it"
        );
    }

    /// T14 (issue #47): a guard that could not start names the missing
    /// tool in its own words.
    #[test]
    fn a_slept_turn_quotes_a_guard_that_could_not_start() {
        let status = "unavailable: no caffeinate on PATH";
        let line =
            slept_line(&slept_payload(750, 750, 0, Some(status))).expect("a slept turn says so");
        assert!(
            line.contains("caffeinate"),
            "the line names the missing tool: {line}"
        );
        assert_eq!(
            line,
            format!(
                "the machine slept {} min during this turn; {status}",
                750 / 60
            )
        );
    }

    /// And a turn that did not sleep says nothing at all.
    #[test]
    fn a_turn_that_did_not_sleep_says_nothing() {
        assert_eq!(
            slept_line(&TurnEndedPayload::new("done")),
            None,
            "a turn without slept_secs renders exactly as it always did"
        );
    }

    /// T7 (issue #22), reshaped by #113: a provider-error turn end names
    /// the failure in plain words plus the `/why` pointer, never the raw
    /// JSON reason, and still names the files written.
    #[test]
    fn provider_error_turn_end_renders_plain_line() {
        let mut p = provider_error_payload(503, "{\"error\": {\"message\": \"unavailable\"}}");
        p.touched = vec!["src/main.rs".into()];
        let error = p.error.as_ref().expect("the payload carries its error");
        let expected = format!(
            "[turn ended: the model's reply failed: {}  (/why shows the raw error)]",
            error.cause_line()
        );
        let lines = turn_end_report(&p, &[]);
        assert_eq!(lines[0], expected, "{lines:#?}");
        assert_eq!(lines[2], "[files written: src/main.rs]", "{lines:#?}");
        assert!(!lines[0].contains('{'), "{lines:#?}");
        assert!(!lines[0].contains("provider_error"), "{lines:#?}");
    }

    /// Issue #35: the saturation line names what the sweep holds and the
    /// ceiling, so the reader can tell the turn is as small as it will
    /// get and start a fresh thread.
    #[test]
    fn a_saturation_line_names_the_ceiling_and_the_floor() {
        let p = aigentic_runtime::aigentic_log::ContextSaturatedPayload {
            through_seq: 312,
            ratio: None,
            tokens_at_floor: 137_000,
            ceiling: 128_000,
        };
        let line = saturated_line(&p);
        assert!(line.contains("137000"), "{line}");
        assert!(line.contains("128000"), "{line}");
        assert!(line.contains("fresh thread"), "{line}");
    }

    /// T8 (issue #22): a payload without a structured error (an old log,
    /// an older daemon) keeps the raw reason — no regression.
    #[test]
    fn legacy_turn_end_without_error_falls_back() {
        let p = TurnEndedPayload::new("provider_error: transport error: connection closed");
        let lines = turn_end_report(&p, &[]);
        assert_eq!(
            lines[0], "[turn ended: provider_error: transport error: connection closed]",
            "an unreadable error shape degrades to the raw reason"
        );
        assert_eq!(lines.len(), 3, "and nothing else is known: {lines:#?}");
    }

    /// T6 (issue #96): a cut reply reads as the `Cut` plain line plus the
    /// `/why` pointer, and a length stop reads as its named constant —
    /// never the raw reason either way.
    #[test]
    fn a_cut_reply_and_a_length_stop_each_read_as_their_line() {
        let cut = TurnEndedPayload {
            reason: format!("provider_error: {CUT_STREAM}"),
            error: Some(ProviderError::Cut),
            ..TurnEndedPayload::new("")
        };
        let expected_cut = format!(
            "[turn ended: the model's reply failed: {}  (/why shows the raw error)]",
            ProviderError::Cut.cause_line()
        );
        let cut_lines = turn_end_report(&cut, &[]);
        assert_eq!(cut_lines[0], expected_cut, "{cut_lines:#?}");
        assert!(!cut_lines[0].contains("provider_error"), "{cut_lines:#?}");

        let length = TurnEndedPayload::new(LENGTH_STOP);
        let length_lines = turn_end_report(&length, &[]);
        assert_eq!(
            length_lines[0],
            format!("[turn ended: {LENGTH_STOP_TEXT}]"),
            "a length stop names what to do next"
        );
        assert!(
            !length_lines[0].contains(LENGTH_STOP),
            "and not the raw reason: {length_lines:#?}"
        );
    }

    /// T9 (issue #22): `/why` shows the raw reason after a provider
    /// failure, and a later done turn clears it.
    #[test]
    fn why_shows_raw_reason_then_done_clears_interest() {
        let failed = provider_error_payload(400, "{\"error\": {\"message\": \"bad request\"}}");
        let last = stop_reason(&failed);
        assert_eq!(last.as_deref(), Some(failed.reason.as_str()));
        assert_eq!(
            why_line(last.as_deref()),
            format!("[last stop: {}]", failed.reason)
        );

        let done = TurnEndedPayload::new("done");
        assert_eq!(stop_reason(&done), None);
        assert_eq!(
            why_line(stop_reason(&done).as_deref()),
            "[the last turn ended done]"
        );
    }

    /// T10 (issue #22): a provider failure after a nap still prints the
    /// slept line first, then the stop line.
    #[test]
    fn slept_line_stays_before_stop_line() {
        let mut p = provider_error_payload(503, "{}");
        p.slept_secs = Some(750);
        p.slept_awaiting_secs = Some(750);
        let lines = turn_end_report(&p, &[]);
        assert!(lines.len() >= 2, "{lines:#?}");
        assert!(lines[0].starts_with("the machine slept"), "{lines:#?}");
        assert!(lines[1].starts_with("[turn ended: "), "{lines:#?}");
    }

    // ---- the turn-end report (issue #113) ----------------------------

    /// One turn event for the report's fixtures, authored by the model.
    fn turn_event(seq: u64, kind: EventKind, payload: serde_json::Value) -> Event {
        Event {
            id: Ulid::generate(),
            thread_id: Ulid::generate(),
            seq,
            kind,
            author: Author::Agent(aigentic_runtime::aigentic_core::AgentId("model".into())),
            payload,
            parent_event: None,
            created_at: time::OffsetDateTime::now_utc(),
        }
    }

    /// The `user_message` that starts a turn (`mid_turn == false`).
    fn turn_start(seq: u64) -> Event {
        turn_event(
            seq,
            EventKind::UserMessage,
            serde_json::to_value(UserMessagePayload::new(Vec::new())).unwrap(),
        )
    }

    /// A model message holding `calls` and no text.
    fn call_message(seq: u64, calls: &[ToolCall]) -> Event {
        turn_event(
            seq,
            EventKind::AssistantMessage,
            serde_json::to_value(AssistantMessagePayload {
                blocks: calls.iter().cloned().map(ContentBlock::ToolCall).collect(),
                usage: None,
                finish_reason: Some("tool_calls".into()),
            })
            .unwrap(),
        )
    }

    /// The result answering `call`, with `policy`, in log order.
    fn call_result(seq: u64, call: &ToolCall, is_error: bool, policy: PolicyRecord) -> Event {
        turn_event(
            seq,
            EventKind::ToolResult,
            serde_json::to_value(ToolResultPayload::new(
                aigentic_runtime::aigentic_core::ToolResult {
                    id: call.id.clone(),
                    content: format!("{} done", call.name),
                    is_error,
                },
                policy,
            ))
            .unwrap(),
        )
    }

    /// A `bash` call; `summarise_args` renders it as its command.
    fn a_bash_call(id: &str, command: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "bash".into(),
            args: serde_json::json!({ "command": command }),
        }
    }

    /// An `update_tasks` call, as the model sends one.
    fn an_update_tasks_call(id: &str, tasks: serde_json::Value) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "update_tasks".into(),
            args: serde_json::json!({ "tasks": tasks }),
        }
    }

    /// How the transcript draws one call, as the report shares it.
    fn rendered(call: &ToolCall) -> String {
        format!("{} {}", call.name, summarise_args(call))
    }

    /// T2 (issue #113): #111's own case — the closing reply failed after
    /// the work was done and the temp file deleted. The line says what
    /// failed, what the turn did, that the files are written, and that
    /// nothing was left — and never sends the reader back for finished
    /// work.
    #[test]
    fn a_failed_closing_reply_after_finished_work_invites_no_retry() {
        let comment = a_bash_call(
            "c1",
            "gh issue comment 113 --body-file /tmp/113-spec-check.md",
        );
        let rm = a_bash_call("c2", "rm -f /tmp/113-spec-check.md");
        let tasks = an_update_tasks_call(
            "t1",
            serde_json::json!([
                { "text": "post the comment", "state": "done" },
                { "text": "delete the temp file", "state": "done" },
            ]),
        );
        let turn = vec![
            turn_start(1),
            call_message(2, std::slice::from_ref(&tasks)),
            call_result(
                3,
                &tasks,
                false,
                PolicyRecord::rule("harness tool", "allow"),
            ),
            call_message(4, std::slice::from_ref(&comment)),
            call_result(
                5,
                &comment,
                false,
                PolicyRecord::rule("bash allow-pattern gh", "allow"),
            ),
            call_message(6, std::slice::from_ref(&rm)),
            call_result(
                7,
                &rm,
                false,
                PolicyRecord::rule("bash allow-pattern rm", "allow"),
            ),
            turn_event(
                8,
                EventKind::AssistantMessage,
                serde_json::to_value(AssistantMessagePayload {
                    blocks: vec![ContentBlock::Text("posted; temp file gone".into())],
                    usage: None,
                    finish_reason: Some("end_of_stream".into()),
                })
                .unwrap(),
            ),
        ];
        let mut p = TurnEndedPayload::new("provider_error: transport error: connection closed");
        p.error = Some(ProviderError::Transport("connection closed".into()));
        p.touched = vec!["/tmp/113-spec-check.md".into()];
        let cause = p.error.as_ref().unwrap().cause_line();

        let lines = turn_end_report(&p, &turn);
        assert_eq!(
            lines,
            vec![
                format!(
                    "[turn ended: the model's reply failed: {cause}  (/why shows the raw error)]"
                ),
                format!("[before that: {} · {}]", rendered(&comment), rendered(&rm)),
                "[files written: /tmp/113-spec-check.md]".to_owned(),
                "[nothing was left mid-way]".to_owned(),
            ],
            "{lines:#?}"
        );
    }

    /// A failure that leaves the last batch unfinished is `left:`, whatever
    /// left it: the four not-run shapes the log writes.
    #[test]
    fn a_not_run_result_in_the_last_batch_is_left() {
        let call = a_bash_call("c1", "cargo test");
        let shapes = [
            PolicyRecord::rule(NOT_RUN_OVER_LIMIT, "deny"),
            PolicyRecord::rule(INTERRUPTED, "deny"),
            PolicyRecord::rule(NOT_RUN_SOLO, "deny"),
            PolicyRecord::synthetic(),
        ];
        for policy in shapes {
            let turn = vec![
                turn_start(1),
                call_message(2, std::slice::from_ref(&call)),
                call_result(3, &call, true, policy.clone()),
            ];
            let p = provider_error_payload(503, "{}");
            let lines = turn_end_report(&p, &turn);
            assert_eq!(
                lines.last().unwrap(),
                &format!("[left: {} — type continue to carry on]", rendered(&call)),
                "{policy:?}: {lines:#?}"
            );
        }
    }

    /// A sibling's not-run result is written while the turn goes on: once
    /// the turn recovered and its checklist is complete, nothing is left.
    #[test]
    fn a_not_run_sibling_that_the_turn_recovered_from_is_not_left() {
        let solo = ToolCall {
            id: "s1".into(),
            name: "suggest_project".into(),
            args: serde_json::json!({ "project": "aigentic-web" }),
        };
        let tasks = an_update_tasks_call(
            "t1",
            serde_json::json!([{ "text": "post the comment", "state": "done" }]),
        );
        let gate = a_bash_call("c1", "cargo test");
        let turn = vec![
            turn_start(1),
            call_message(2, std::slice::from_ref(&solo)),
            call_result(3, &solo, true, PolicyRecord::rule(NOT_RUN_SOLO, "deny")),
            call_message(4, std::slice::from_ref(&tasks)),
            call_result(
                5,
                &tasks,
                false,
                PolicyRecord::rule("harness tool", "allow"),
            ),
            call_message(6, std::slice::from_ref(&gate)),
            call_result(
                7,
                &gate,
                false,
                PolicyRecord::rule("bash allow-pattern", "allow"),
            ),
        ];
        let p = provider_error_payload(503, "{}");
        let lines = turn_end_report(&p, &turn);
        assert_eq!(
            lines.last().unwrap(),
            "[nothing was left mid-way]",
            "{lines:#?}"
        );
        assert!(!lines.iter().any(|l| l.contains("continue")), "{lines:#?}");
    }

    /// A client that attached or resumed mid-turn never saw the turn
    /// start, so it says so instead of claiming no tool ran.
    #[test]
    fn a_window_that_joined_mid_turn_says_so() {
        let call = a_bash_call("c1", "cargo test");
        let turn = vec![
            call_message(2, std::slice::from_ref(&call)),
            call_result(
                3,
                &call,
                false,
                PolicyRecord::rule("bash allow-pattern", "allow"),
            ),
        ];
        let p = provider_error_payload(503, "{}");
        let lines = turn_end_report(&p, &turn);
        assert_eq!(
            lines[1], "[this window joined mid-turn; the transcript and /why have the rest]",
            "{lines:#?}"
        );
        assert!(
            !lines.iter().any(|l| l.contains("no tool ran")),
            "{lines:#?}"
        );
    }

    /// A call the process never answered is named, with the advice.
    #[test]
    fn a_call_with_no_result_is_named_as_left() {
        let call = a_bash_call("c1", "gh issue comment 113 --body-file /tmp/113.md");
        let turn = vec![turn_start(1), call_message(2, std::slice::from_ref(&call))];
        let p = provider_error_payload(503, "{}");
        let lines = turn_end_report(&p, &turn);
        assert_eq!(
            lines.last().unwrap(),
            &format!("[left: {} — type continue to carry on]", rendered(&call)),
            "{lines:#?}"
        );
    }

    /// An unfinished checklist names the step it stopped on, not the last
    /// call, when the last batch is answered and fine.
    #[test]
    fn a_checklist_that_is_not_finished_names_the_active_step() {
        let tasks = an_update_tasks_call(
            "t1",
            serde_json::json!([
                { "text": "read the spec", "state": "done" },
                { "text": "write the code", "state": "done" },
                { "text": "run the gate", "state": "active" },
                { "text": "post the report", "state": "pending" },
                { "text": "push", "state": "pending" },
            ]),
        );
        let gate = a_bash_call("c1", "cargo test");
        let turn = vec![
            turn_start(1),
            call_message(2, std::slice::from_ref(&tasks)),
            call_result(
                3,
                &tasks,
                false,
                PolicyRecord::rule("harness tool", "allow"),
            ),
            call_message(4, std::slice::from_ref(&gate)),
            call_result(
                5,
                &gate,
                false,
                PolicyRecord::rule("bash allow-pattern", "allow"),
            ),
        ];
        let p = provider_error_payload(503, "{}");
        let lines = turn_end_report(&p, &turn);
        assert_eq!(
            lines.last().unwrap(),
            "[left: run the gate — type continue to carry on]",
            "{lines:#?}"
        );
        assert_eq!(
            open_checklist(&turn).unwrap().total,
            5,
            "2/5, as the fixture"
        );
    }

    /// `before that:` never says no tool ran while one did: a turn whose
    /// only tool was the checklist call, and a turn whose only call
    /// failed, both name what ran instead.
    #[test]
    fn a_turn_that_ran_a_tool_is_not_said_to_have_run_none() {
        let tasks = an_update_tasks_call(
            "t1",
            serde_json::json!([{ "text": "write the code", "state": "active" }]),
        );
        let p = provider_error_payload(503, "{}");
        let turn = vec![
            turn_start(1),
            call_message(2, std::slice::from_ref(&tasks)),
            call_result(
                3,
                &tasks,
                false,
                PolicyRecord::rule("harness tool", "allow"),
            ),
        ];
        let lines = turn_end_report(&p, &turn);
        assert_eq!(
            lines
                .iter()
                .find(|l| l.starts_with("[before that: "))
                .expect("the line is drawn"),
            &format!("[before that: {}]", rendered(&tasks)),
            "{lines:#?}"
        );

        // A call that ran and failed is named too, not denied — and
        // marked `(failed)` so the line never reads as work done
        // (supervisor extra 1).
        let call = a_bash_call("c1", "cargo test");
        let turn = vec![
            turn_start(1),
            call_message(2, std::slice::from_ref(&call)),
            call_result(3, &call, true, PolicyRecord::rule("bash", "allow")),
        ];
        let lines = turn_end_report(&p, &turn);
        assert_eq!(
            lines
                .iter()
                .find(|l| l.starts_with("[before that: "))
                .expect("the line is drawn"),
            &format!("[before that: {} (failed)]", rendered(&call)),
            "{lines:#?}"
        );
    }

    /// Supervisor extra 1 (issue #113): a call whose result was an error
    /// is marked in `before that:`; a successful call is unchanged, with
    /// no suffix.
    #[test]
    fn a_failed_call_is_marked_and_a_successful_one_is_not() {
        let p = provider_error_payload(503, "{}");
        let call = a_bash_call("c1", "cargo test");

        let failed = vec![
            turn_start(1),
            call_message(2, std::slice::from_ref(&call)),
            call_result(3, &call, true, PolicyRecord::rule("bash", "allow")),
        ];
        let lines = turn_end_report(&p, &failed);
        assert_eq!(
            lines[1],
            format!("[before that: {} (failed)]", rendered(&call)),
            "{lines:#?}"
        );

        let ok = vec![
            turn_start(1),
            call_message(2, std::slice::from_ref(&call)),
            call_result(
                3,
                &call,
                false,
                PolicyRecord::rule("bash allow-pattern", "allow"),
            ),
        ];
        let lines = turn_end_report(&p, &ok);
        assert_eq!(
            lines[1],
            format!("[before that: {}]", rendered(&call)),
            "no suffix on a success: {lines:#?}"
        );
    }

    /// A failure with nothing known either way says so, and offers the
    /// cheap retry without claiming the work is unfinished.
    #[test]
    fn a_failure_with_no_checklist_and_nothing_unfinished_is_unknown() {
        let call = a_bash_call("c1", "cargo test");
        let turn = vec![
            turn_start(1),
            call_message(2, std::slice::from_ref(&call)),
            call_result(
                3,
                &call,
                false,
                PolicyRecord::rule("bash allow-pattern", "allow"),
            ),
        ];
        let p = provider_error_payload(503, "{}");
        let lines = turn_end_report(&p, &turn);
        assert_eq!(
            lines.last().unwrap(),
            "[if that wasn't the end, type continue]",
            "{lines:#?}"
        );
    }

    /// A cap that caught a finished turn is still named, and says nothing
    /// is left: a cap can end a turn whose checklist is complete.
    #[test]
    fn a_wall_time_cap_on_finished_work_is_done_with_no_figure() {
        let tasks = an_update_tasks_call(
            "t1",
            serde_json::json!([{ "text": "post the report", "state": "done" }]),
        );
        let turn = vec![
            turn_start(1),
            call_message(2, std::slice::from_ref(&tasks)),
            call_result(
                3,
                &tasks,
                false,
                PolicyRecord::rule("harness tool", "allow"),
            ),
        ];
        let p = TurnEndedPayload::new("max_wall_time");
        let lines = turn_end_report(&p, &turn);
        assert_eq!(
            lines[0], "[turn ended: the turn hit its wall-time limit]",
            "{lines:#?}"
        );
        assert!(
            !lines[0].chars().any(|c| c.is_ascii_digit()),
            "no figure is in the payload to name: {lines:#?}"
        );
        assert_eq!(
            lines.last().unwrap(),
            "[nothing was left mid-way]",
            "{lines:#?}"
        );
        assert!(!lines.iter().any(|l| l.contains("continue")), "{lines:#?}");
    }

    /// A cap with no checklist stopped the model while it was working.
    #[test]
    fn a_wall_time_cap_while_working_is_left() {
        let call = a_bash_call("c1", "cargo test");
        let turn = vec![
            turn_start(1),
            call_message(2, std::slice::from_ref(&call)),
            call_result(
                3,
                &call,
                false,
                PolicyRecord::rule("bash allow-pattern", "allow"),
            ),
        ];
        let p = TurnEndedPayload::new("max_tokens");
        let lines = turn_end_report(&p, &turn);
        assert_eq!(
            lines[0], "[turn ended: the turn hit its token budget]",
            "{lines:#?}"
        );
        assert_eq!(
            lines.last().unwrap(),
            "[left: the turn was stopped while working — type continue to carry on]",
            "{lines:#?}"
        );
    }

    /// A length stop keeps its own text, and a turn that ran no tool says
    /// so rather than leaving the reader to guess.
    #[test]
    fn a_length_stop_keeps_its_text_and_an_empty_turn_says_no_tool_ran() {
        let turn = vec![turn_start(1)];
        let lines = turn_end_report(&TurnEndedPayload::new(LENGTH_STOP), &turn);
        assert_eq!(
            lines[0],
            format!("[turn ended: {LENGTH_STOP_TEXT}]"),
            "{lines:#?}"
        );
        assert_eq!(lines[1], "[before that: no tool ran]", "{lines:#?}");
    }

    /// A turn that ended `done`, `asked_human` or `interrupted` prints
    /// exactly what it printed before this ticket.
    #[test]
    fn a_finished_turn_prints_no_new_lines() {
        for reason in ["done", ASKED_HUMAN, INTERRUPTED] {
            let turn = vec![
                turn_start(1),
                call_message(2, &[a_bash_call("c1", "cargo test")]),
            ];
            assert!(
                turn_end_report(&TurnEndedPayload::new(reason), &turn).is_empty(),
                "{reason} says nothing new"
            );
        }
        let mut slept = slept_payload(750, 750, 750, None);
        slept.reason = INTERRUPTED.into();
        let lines = turn_end_report(&slept, &[turn_start(1)]);
        assert_eq!(lines.len(), 1, "only the slept line: {lines:#?}");
        assert!(lines[0].starts_with("the machine slept"), "{lines:#?}");
    }

    // ---- the turn buffer's bound (issue #113 fix) --------------------

    /// The report over the pruned buffer equals the report over the whole
    /// turn: pruning keeps everything `turn_end_report` reads.
    fn assert_prune_keeps_the_report(p: &TurnEndedPayload, turn: &[Event]) {
        let mut buffer = TurnEvents::default();
        for e in turn {
            buffer.push(e);
        }
        assert_eq!(
            turn_end_report(p, buffer.as_slice()),
            turn_end_report(p, turn),
            "pruning changed the report: {turn:#?}"
        );
    }

    /// A bounded turn: the buffer keeps only the newest successful
    /// `update_tasks` call pair, however many the turn made, so a
    /// checklist-heavy turn stays small. The count is derived, not
    /// measured: the turn start (1) plus the last `REPORT_CALLS` calls
    /// each with a result (2 × REPORT_CALLS = 6) = 7; the last assistant
    /// message and the newest checklist pair lie inside that set here.
    /// No timing assertion: the bound, not a clock, is the contract.
    #[test]
    fn a_checklist_heavy_turn_keeps_a_bounded_buffer() {
        let steps = serde_json::json!(
            (0..30)
                .map(|i| serde_json::json!({ "text": format!("step {i}"), "state": "done" }))
                .collect::<Vec<_>>()
        );
        let mut buffer = TurnEvents::default();
        let mut full: Vec<Event> = vec![];
        buffer.push(&turn_start(1));
        full.push(turn_start(1));
        let mut seq = 2;
        for i in 0..500 {
            let call = an_update_tasks_call(&format!("t{i}"), steps.clone());
            let message = call_message(seq, std::slice::from_ref(&call));
            buffer.push(&message);
            full.push(message);
            seq += 1;
            let result = call_result(
                seq,
                &call,
                false,
                PolicyRecord::rule("harness tool", "allow"),
            );
            buffer.push(&result);
            full.push(result);
            seq += 1;
        }
        assert_eq!(
            buffer.as_slice().len(),
            1 + 2 * REPORT_CALLS,
            "turn start + the last {REPORT_CALLS} calls, each with a result"
        );
        // The checklist the bounded buffer holds is the whole turn's: the
        // drop is safe because `open_checklist` reads the last success.
        assert_eq!(
            open_checklist(buffer.as_slice()),
            open_checklist(&full),
            "the checklist is unchanged by the bound"
        );
    }

    /// Pruning loses nothing the report reads, across the fixtures it was
    /// built for, plus the latest `update_tasks` failing after an earlier
    /// success: the newest successful call is what the checklist reads.
    #[test]
    fn a_pruned_turn_reports_the_same_as_the_whole_turn() {
        let p = provider_error_payload(503, "{}");
        let allow = || PolicyRecord::rule("bash allow-pattern", "allow");

        // A complete checklist, then two successful calls (#111's shape).
        let comment = a_bash_call("c1", "gh issue comment 113 --body-file /tmp/113.md");
        let rm = a_bash_call("c2", "rm -f /tmp/113.md");
        let done = an_update_tasks_call(
            "t1",
            serde_json::json!([{ "text": "post the comment", "state": "done" }]),
        );
        let turn = vec![
            turn_start(1),
            call_message(2, std::slice::from_ref(&done)),
            call_result(3, &done, false, PolicyRecord::rule("harness tool", "allow")),
            call_message(4, std::slice::from_ref(&comment)),
            call_result(5, &comment, false, allow()),
            call_message(6, std::slice::from_ref(&rm)),
            call_result(7, &rm, false, allow()),
        ];
        assert_prune_keeps_the_report(&p, &turn);

        // A 2/5 checklist: the buffer still names the active step.
        let tasks = an_update_tasks_call(
            "t1",
            serde_json::json!([
                { "text": "read", "state": "done" },
                { "text": "write", "state": "done" },
                { "text": "run the gate", "state": "active" },
                { "text": "post", "state": "pending" },
                { "text": "push", "state": "pending" },
            ]),
        );
        let turn = vec![
            turn_start(1),
            call_message(2, std::slice::from_ref(&tasks)),
            call_result(
                3,
                &tasks,
                false,
                PolicyRecord::rule("harness tool", "allow"),
            ),
        ];
        assert_prune_keeps_the_report(&p, &turn);

        // The latest `update_tasks` failed after an earlier success: the
        // earlier success is the checklist the report reads.
        let first = an_update_tasks_call(
            "t1",
            serde_json::json!([
                { "text": "write", "state": "done" },
                { "text": "run the gate", "state": "active" },
            ]),
        );
        let failed = an_update_tasks_call(
            "t2",
            serde_json::json!([{ "text": "run the gate", "state": "done" }]),
        );
        let turn = vec![
            turn_start(1),
            call_message(2, std::slice::from_ref(&first)),
            call_result(
                3,
                &first,
                false,
                PolicyRecord::rule("harness tool", "allow"),
            ),
            call_message(4, std::slice::from_ref(&failed)),
            call_result(5, &failed, true, PolicyRecord::rule("harness tool", "deny")),
        ];
        assert_prune_keeps_the_report(&p, &turn);
    }

    // ---- the turn-end report through the engine (issue #113) ---------

    /// One embedded daemon-repl run (the shape of
    /// `the_repl_streams_a_reply_reports_and_quits_over_an_embedded_daemon`),
    /// with `input` typed as a person would and the drawn lines back.
    async fn run_a_scripted_repl(
        dir: &std::path::Path,
        root: std::path::PathBuf,
        script: Vec<Vec<ProviderEvent>>,
        input: &str,
    ) -> (aigentic_server::Embedded, Vec<String>) {
        let cfg_dir = dir.join("cfg");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let embedded = Server::embed_with(
            config(dir),
            cfg_dir.clone(),
            root,
            "steve",
            None,
            Factory::scripted(script),
            Arc::new(DefaultReports {
                global_instructions: cfg_dir.join("instructions.md"),
            }),
        )
        .await
        .unwrap();
        let (client, welcome) =
            Client::connect(&Addr::Unix(embedded.socket.clone()), &embedded.token)
                .await
                .unwrap();
        let (thread, state, mode) = open(&client, "p", None).await;
        let notices = client.take_notices().unwrap();
        let mut repl = ClientRepl::new(
            client,
            thread,
            "steve",
            welcome.projects[0].role.clone(),
            state,
            mode,
            Identity::default(),
            "p",
        );
        let (tx, rx) = mpsc::unbounded_channel();
        let typed: Vec<String> = input.split('\n').map(str::to_owned).collect();
        let feeder = async move {
            for line in typed {
                tx.send(line).unwrap();
                // A pause so each turn finishes before the next line.
                tokio::time::sleep(std::time::Duration::from_millis(600)).await;
            }
            tx.send("/quit".into()).unwrap();
        };
        let mut out = Lines::default();
        let ((), ()) = tokio::join!(repl.run(rx, notices, &mut out), feeder);
        (embedded, out.0)
    }

    /// T3 (#113): a turn whose provider fails after two tool calls draws
    /// the report's lines after the summary cell, and what it did comes
    /// from `summarise_args`, the transcript's own rendering.
    #[tokio::test]
    async fn a_failed_turn_draws_what_it_did_and_what_is_left() {
        let dir = tempfile::tempdir().unwrap();
        let root = project(dir.path(), "p", "");
        // Two read-class calls: they run, and they need no gate.
        let (a, b) = (dir.path().join("a.md"), dir.path().join("b.md"));
        std::fs::write(&a, "one\n").unwrap();
        std::fs::write(&b, "two\n").unwrap();
        let one = call("c1", "read_file", serde_json::json!({"path": a}));
        let two = call("c2", "read_file", serde_json::json!({"path": b}));
        let script = vec![
            vec![one.clone(), tool_use()],
            vec![two.clone(), tool_use()],
            vec![ProviderEvent::Error(ProviderError::Transport(
                "connection closed".into(),
            ))],
        ];
        let (embedded, lines) = run_a_scripted_repl(dir.path(), root, script, "hello").await;
        let rendered = |e: &ProviderEvent| match e {
            ProviderEvent::ToolCall(c) => summarise_args(c),
            _ => unreachable!(),
        };
        let expected = format!(
            "[before that: read_file {} · read_file {}]",
            rendered(&one),
            rendered(&two)
        );
        assert!(lines.contains(&expected), "{lines:#?}");
        // Unknown: no checklist, and the turn's last batch answered both
        // calls. Nothing is left, so no `continue` — only the door.
        assert!(
            lines.contains(&"[if that wasn't the end, type continue]".to_owned()),
            "{lines:#?}"
        );
        let stop = format!(
            "[turn ended: the model's reply failed: {}  (/why shows the raw error)]",
            ProviderError::Transport("connection closed".into()).cause_line()
        );
        let stop_at = lines
            .iter()
            .position(|l| *l == stop)
            .expect("the stop line");
        let summary_at = lines
            .iter()
            .position(|l| l.trim_start().starts_with("─ "))
            .expect("the summary cell");
        assert!(stop_at > summary_at, "after the summary: {lines:#?}");
        let before_at = lines.iter().position(|l| *l == expected).unwrap();
        assert!(before_at > stop_at, "the stop line reads first: {lines:#?}");
        assert!(
            !lines.iter().any(|l| l.contains("type continue to retry")),
            "no old advice either: {lines:#?}"
        );
        drop(embedded);
    }

    /// T3 (#113): the buffer is cleared at the next turn start, so the
    /// second turn's report never claims the first turn's calls.
    #[tokio::test]
    async fn the_turn_events_are_cleared_at_the_next_turn_start() {
        let dir = tempfile::tempdir().unwrap();
        let root = project(dir.path(), "p", "");
        let (a, b) = (dir.path().join("a.md"), dir.path().join("b.md"));
        std::fs::write(&a, "one\n").unwrap();
        std::fs::write(&b, "two\n").unwrap();
        let one = call("c1", "read_file", serde_json::json!({"path": a}));
        let two = call("c2", "read_file", serde_json::json!({"path": b}));
        let failed = || ProviderEvent::Error(ProviderError::Transport("connection closed".into()));
        let script = vec![
            vec![one.clone(), tool_use()],
            vec![two.clone(), tool_use()],
            vec![failed()],
            vec![failed()],
            vec![failed()],
        ];
        let (embedded, lines) = run_a_scripted_repl(dir.path(), root, script, "hello\nagain").await;
        let rendered = |e: &ProviderEvent| match e {
            ProviderEvent::ToolCall(c) => summarise_args(c),
            _ => unreachable!(),
        };
        let did = format!(
            "read_file {} · read_file {}",
            rendered(&one),
            rendered(&two)
        );
        assert_eq!(
            lines
                .iter()
                .filter(|l| *l == &format!("[before that: {did}]"))
                .count(),
            1,
            "the first turn's calls, once: {lines:#?}"
        );
        assert_eq!(
            lines
                .iter()
                .filter(|l| *l == "[before that: no tool ran]")
                .count(),
            1,
            "the second turn's own, empty: {lines:#?}"
        );
        drop(embedded);
    }

    // ---- /build in the REPL (issue #68) ------------------------------

    /// A followed run's tests drive a scripted daemon
    /// (`run_view::fake_daemon`): a socket that answers `Build` and
    /// `Open` and pushes the notices a test asks for. `Lead` bundles
    /// what they share, so each test reads as the REPL's own steps.
    struct Lead {
        _dir: tempfile::TempDir,
        daemon: crate::run_view::fake_daemon::FakeDaemon,
        repl: ClientRepl,
        notices: mpsc::Receiver<Notice>,
        out: Copies,
    }

    impl Lead {
        /// A daemon whose `Build` names `lead` and whose `Open` hands
        /// back `backlog`, with a REPL attached to a chat thread.
        async fn start(lead: Ulid, backlog: Vec<Event>) -> Self {
            Self::with(lead, backlog, "admin", None, false).await
        }

        /// The same, with the role, the refusal and the `resumed` flag
        /// a test needs.
        async fn with(
            lead: Ulid,
            backlog: Vec<Event>,
            role: &'static str,
            refuse_build: Option<String>,
            resumed: bool,
        ) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let script = crate::run_view::fake_daemon::Script {
                lead,
                backlog,
                resumed,
                role,
                refuse_build,
                refuse_open: None,
            };
            let daemon = crate::run_view::fake_daemon::FakeDaemon::start(dir.path(), script).await;
            let (client, welcome) = Client::connect(&daemon.addr(), "tok").await.unwrap();
            let role = welcome.projects[0].role.clone();
            let (thread, state, mode) = open(&client, "proj", Some(Ulid::generate())).await;
            let notices = client.take_notices().unwrap();
            let repl = ClientRepl::new(
                client,
                thread,
                "steve",
                role,
                state,
                mode,
                Identity::default(),
                "proj",
            );
            Self {
                _dir: dir,
                daemon,
                repl,
                notices,
                out: Copies::default(),
            }
        }

        /// The lines drawn so far.
        fn lines(&self) -> Vec<String> {
            self.out.0.0.clone()
        }

        /// A typed line, as the shell would hand it over.
        async fn line(&mut self, line: &str) {
            self.repl.handle_line(line, &mut self.out).await;
        }

        /// Every notice that has arrived, drawn. A pause for the
        /// daemon's writes, then whatever is there.
        async fn pump(&mut self) {
            while let Ok(Some(notice)) =
                tokio::time::timeout(std::time::Duration::from_millis(200), self.notices.recv())
                    .await
            {
                self.repl.render(notice, &mut self.out);
            }
        }

        /// Push a notice as a live one.
        fn push(&self, notice: Notice) {
            self.daemon.push(notice);
        }
    }

    /// An event for a lead, at `seq`.
    fn run_event(lead: Ulid, seq: u64, kind: EventKind, payload: serde_json::Value) -> Event {
        Event {
            id: Ulid::generate(),
            thread_id: lead,
            seq,
            kind,
            author: Author::User(aigentic_runtime::aigentic_core::UserId("steve".into())),
            payload,
            parent_event: None,
            created_at: time::OffsetDateTime::now_utc(),
        }
    }

    /// A `step_started` payload, as the runner writes it.
    fn step_started(step: &str, attempt: u32, child: Ulid) -> serde_json::Value {
        serde_json::json!({
            "step": step,
            "role": "implementer",
            "profile": "flash",
            "child_thread": child,
            "attempt": attempt,
            "budget_usd": 3.0,
        })
    }

    /// A `checkpoint_asked` payload with one shown line, as the runner
    /// writes for one of its own gates: `stop` is its only answer.
    fn checkpoint_asked(gate: &str) -> serde_json::Value {
        serde_json::json!({
            "gate": gate,
            "shown": ["plan ready"],
            "options": ["stop"],
        })
    }

    /// A `run_finished` payload.
    fn run_finished(outcome: aigentic_runtime::aigentic_log::RunOutcome) -> serde_json::Value {
        serde_json::json!({ "outcome": outcome, "cost_usd": 1.5 })
    }

    /// T1: the parser and the help text. `/build 73` is the number,
    /// `/build` alone is no argument, and `/build x` is neither.
    #[test]
    fn build_parses_its_argument_and_is_in_the_help() {
        assert_eq!(
            parse_line("/build 73", &[]),
            Command::Build(Some("73")),
            "the number is the argument"
        );
        assert_eq!(
            parse_line("/build", &[]),
            Command::Build(None),
            "no argument is not an argument"
        );
        assert!(
            crate::app::commands::COMMANDS
                .iter()
                .any(|(name, _)| *name == "build"),
            "/build is offered: {:?}",
            crate::app::commands::COMMANDS
        );
        assert!(HELP.contains("/build <n>"), "the help names it: {HELP}");
    }

    /// T1: `/build x` prints the usage line and sends nothing.
    #[tokio::test]
    async fn a_bad_build_argument_prints_the_usage_line_and_sends_nothing() {
        let mut lead = Lead::start(Ulid::generate(), Vec::new()).await;
        lead.line("/build x").await;
        assert_eq!(
            lead.lines(),
            vec!["[usage: /build <issue number> [workflow]]".to_owned()],
            "the usage line, and nothing sent"
        );
        assert!(
            !lead
                .daemon
                .requests()
                .iter()
                .any(|r| matches!(r, Request::Build { .. })),
            "no Build was sent: {:?}",
            lead.daemon.requests()
        );
    }

    /// `/build <n> <workflow>` names the workflow to the daemon; a third
    /// word is a usage error.
    #[tokio::test]
    async fn build_names_the_workflow_when_one_is_given() {
        let mut lead = Lead::start(Ulid::generate(), Vec::new()).await;
        lead.line("/build 58 loop").await;
        assert!(
            lead.daemon.requests().iter().any(|r| matches!(
                r,
                Request::Build { issue: 58, workflow: Some(w), .. } if w == "loop"
            )),
            "the workflow went with the build: {:?}",
            lead.daemon.requests()
        );

        let mut lead = Lead::start(Ulid::generate(), Vec::new()).await;
        lead.line("/build 58 loop extra").await;
        assert_eq!(
            lead.lines(),
            vec!["[usage: /build <issue number> [workflow]]".to_owned()]
        );
    }

    /// `/answer` parses its word; with no checkpoint up it sends nothing
    /// and says so.
    #[tokio::test]
    async fn answer_parses_and_needs_a_checkpoint() {
        assert_eq!(parse_line("/answer go", &[]), Command::Answer("go"));
        assert_eq!(
            parse_line("/answer amend keep it small", &[]),
            Command::Answer("amend keep it small")
        );
        assert!(HELP.contains("/answer go"), "the help names it: {HELP}");
        let mut lead = Lead::start(Ulid::generate(), Vec::new()).await;
        lead.line("/answer go").await;
        assert_eq!(
            lead.lines(),
            vec!["[no checkpoint is waiting in this REPL: /build <n> follows a run]".to_owned()]
        );
        assert!(
            !lead
                .daemon
                .requests()
                .iter()
                .any(|r| matches!(r, Request::AnswerCheckpoint { .. })),
            "nothing was sent"
        );
    }

    /// T3: `/build 58` sends the project and the issue, then opens the
    /// lead at seq 0, and the backlog becomes run cells in order — the
    /// first of them saying `started` or `resumed` as the reply did.
    #[tokio::test]
    async fn build_sends_the_project_the_issue_and_opens_the_lead() {
        let lead_id = Ulid::generate();
        let backlog = vec![
            run_event(
                lead_id,
                1,
                EventKind::StepStarted,
                step_started("implement", 1, Ulid::generate()),
            ),
            run_event(
                lead_id,
                2,
                EventKind::CheckpointAsked,
                checkpoint_asked("route"),
            ),
        ];
        let dir = tempfile::tempdir().unwrap();
        let script = crate::run_view::fake_daemon::Script {
            lead: lead_id,
            backlog: backlog.clone(),
            resumed: true,
            role: "admin",
            refuse_build: None,
            refuse_open: None,
        };
        let daemon = crate::run_view::fake_daemon::FakeDaemon::start(dir.path(), script).await;
        let (client, welcome) = Client::connect(&daemon.addr(), "tok").await.unwrap();
        let role = welcome.projects[0].role.clone();
        let (thread, state, mode) = open(&client, "proj", Some(Ulid::generate())).await;
        let notices = client.take_notices().unwrap();
        let mut repl = ClientRepl::new(
            client,
            thread,
            "steve",
            role,
            state,
            mode,
            Identity::default(),
            "proj",
        );
        let mut out = Copies::default();
        repl.handle_line("/build 58", &mut out).await;

        let requests = daemon.requests();
        // Hello, the chat thread's own open, then the build and the
        // lead's open, in that order (the spec keeps them in one call).
        match &requests[2] {
            Request::Build {
                project,
                issue,
                workflow,
            } => {
                assert_eq!(project, "proj", "the project the REPL started in");
                assert_eq!(*issue, 58);
                assert!(workflow.is_none());
            }
            other => panic!("the build follows the chat's open: {other:?}"),
        }
        assert_eq!(
            requests[3],
            Request::Open {
                thread: lead_id,
                from_seq: 0
            },
            "the lead's log, from its first event"
        );

        let lines = out.0.0.clone();
        let expected_step = crate::run_view::render(&backlog[0]).unwrap();
        let expected_gate = crate::run_view::render(&backlog[1]).unwrap();
        // The prompt prints through `Menu::plain()`, so its lines come
        // from the menu the engine built, not from here.
        let prompt = crate::app::menu::Menu::checkpoint(
            "route",
            &["plan ready".to_owned()],
            &["stop".to_owned()],
        )
        .plain();
        assert_eq!(
            prompt[prompt.len() - 2..],
            [
                "  1. Leave it waiting".to_owned(),
                "  2. Stop the run".to_owned()
            ],
            "the prompt's last two lines are its rows: {prompt:?}"
        );
        assert!(
            prompt[1] == "  plan ready",
            "the prompt's body is the gate's shown lines: {prompt:?}"
        );
        assert!(
            prompt[0].ends_with("checkpoint route"),
            "the prompt's title is the gate: {prompt:?}"
        );
        let mut expected = vec![
            format!("▸ run {lead_id} for issue #58 (resumed)"),
            format!("▸ {expected_step}"),
            format!("▸ {expected_gate}"),
        ];
        expected.extend(prompt);
        assert_eq!(
            lines, expected,
            "the run cell first, then the backlog in order, then the gate's prompt"
        );
        assert!(
            repl.menu().is_some(),
            "the backlog ended at a gate, so the prompt is up"
        );
        drop(notices);
        daemon.stop();
    }

    /// T6: a backlog that ends at an unanswered gate shows the prompt
    /// and sends no answer until a pick. A backlog that ends at the
    /// outcome commits it and leaves nothing followed.
    #[tokio::test]
    async fn a_backlog_gate_is_shown_and_never_answered_on_its_own() {
        let lead_id = Ulid::generate();
        let backlog = vec![run_event(
            lead_id,
            1,
            EventKind::CheckpointAsked,
            checkpoint_asked("gate-1"),
        )];
        let mut lead = Lead::start(lead_id, backlog).await;
        lead.line("/build 58").await;
        lead.pump().await;
        assert!(
            !lead
                .daemon
                .requests()
                .iter()
                .any(|r| matches!(r, Request::AnswerCheckpoint { .. })),
            "nothing was answered: {:?}",
            lead.daemon.requests()
        );
        assert!(lead.repl.menu().is_some(), "the gate's prompt is up");
        assert!(lead.repl.following(), "the run is followed");

        // A backlog that ends at the outcome: the line, and no follow.
        let lead_id = Ulid::generate();
        let backlog = vec![run_event(
            lead_id,
            1,
            EventKind::RunFinished,
            run_finished(aigentic_runtime::aigentic_log::RunOutcome::Closed),
        )];
        let (mut repl, mut out, daemon) = repl_for(lead_id, backlog).await;
        repl.handle_line("/build 58", &mut out).await;
        let lines = out.0.0.clone();
        assert_eq!(
            lines.last().unwrap(),
            &format!(
                "▸ run {lead_id} finished: {}",
                crate::run_view::outcome_line(&aigentic_runtime::aigentic_log::RunOutcome::Closed)
            ),
            "{lines:#?}"
        );
        assert!(!repl.following(), "a finished run is not followed");
        daemon.stop();
    }

    /// T5: a live gate shows the prompt; `Stop the run` sends the
    /// answer and `Ok` withdraws it; the outcome line ends the follow,
    /// so a later event draws nothing.
    #[tokio::test]
    async fn a_live_gate_is_answered_stop_and_the_outcome_ends_the_follow() {
        let lead_id = Ulid::generate();
        let mut lead = Lead::start(lead_id, Vec::new()).await;
        lead.line("/build 58").await;
        lead.pump().await;

        lead.push(Notice::Event {
            thread: lead_id,
            event: run_event(
                lead_id,
                1,
                EventKind::StepStarted,
                step_started("s", 1, Ulid::generate()),
            ),
        });
        lead.pump().await;
        assert!(
            lead.lines().iter().any(|l| l.contains("step s attempt 1")),
            "the live event drew a cell: {:?}",
            lead.lines()
        );

        lead.push(Notice::Event {
            thread: lead_id,
            event: run_event(
                lead_id,
                2,
                EventKind::CheckpointAsked,
                checkpoint_asked("g2"),
            ),
        });
        lead.pump().await;
        let menu = lead.repl.menu().expect("the prompt is up");
        assert_eq!(menu.title, "checkpoint g2", "the gate is the title");

        // Picking `Stop the run` sends the answer; `Ok` withdraws it.
        let before = lead.daemon.requests().len();
        lead.repl
            .menu_key(&key_for('2'), true, true, &mut lead.out)
            .await;
        let sent = lead.daemon.requests()[before..].to_vec();
        match &sent[0] {
            Request::AnswerCheckpoint {
                lead: answered,
                gate,
                answer,
                amendment,
            } => {
                assert_eq!(*answered, lead_id);
                assert_eq!(gate, "g2");
                assert_eq!(*answer, aigentic_api::CheckpointAnswer::Stop);
                assert!(amendment.is_none());
            }
            other => panic!("{other:?}"),
        }
        assert!(lead.repl.menu().is_none(), "the prompt went on `Ok`");

        lead.push(Notice::Event {
            thread: lead_id,
            event: run_event(
                lead_id,
                3,
                EventKind::RunFinished,
                run_finished(aigentic_runtime::aigentic_log::RunOutcome::Stopped),
            ),
        });
        lead.pump().await;
        let expected = format!(
            "▸ run {lead_id} finished: {}",
            crate::run_view::outcome_line(&aigentic_runtime::aigentic_log::RunOutcome::Stopped)
        );
        assert!(
            lead.lines().contains(&expected),
            "the outcome line: {:?}",
            lead.lines()
        );
        assert!(!lead.repl.following(), "the follow ended");

        lead.push(Notice::Event {
            thread: lead_id,
            event: run_event(
                lead_id,
                4,
                EventKind::StepStarted,
                step_started("late", 1, Ulid::generate()),
            ),
        });
        lead.pump().await;
        assert!(
            !lead.lines().iter().any(|l| l.contains("step late")),
            "nothing after the outcome draws: {:?}",
            lead.lines()
        );
    }

    /// T4 and T17r: a notice for a third thread draws nothing; a lead's
    /// `State` never reaches this client's state; a chat-thread notice
    /// still draws; and a `seq` already printed is not drawn again.
    #[tokio::test]
    async fn only_the_chat_and_the_followed_lead_draw_and_a_leads_state_is_ignored() {
        let lead_id = Ulid::generate();
        let mut lead = Lead::start(lead_id, Vec::new()).await;
        lead.line("/build 58").await;
        lead.pump().await;
        let state_before = lead.repl.state().clone();
        let after_build = lead.lines();

        // The lead goes Running; this client must not follow it.
        lead.push(Notice::State {
            thread: lead_id,
            state: ThreadState::Running {
                by: Author::User(aigentic_runtime::aigentic_core::UserId("steve".into())),
                queued: 0,
            },
        });
        lead.pump().await;
        assert_eq!(
            lead.repl.state(),
            &state_before,
            "a lead's state never reaches the chat's"
        );
        assert_eq!(
            lead.lines(),
            after_build,
            "a state is not a run cell: {:?}",
            lead.lines()
        );

        // A third thread: nothing.
        lead.push(Notice::Event {
            thread: Ulid::generate(),
            event: run_event(
                Ulid::generate(),
                9,
                EventKind::StepStarted,
                step_started("other", 1, Ulid::generate()),
            ),
        });
        lead.pump().await;
        assert_eq!(lead.lines(), after_build, "a stranger draws nothing");

        // The chat thread's own notice still draws, exactly as before.
        let chat = lead.repl.thread;
        let mut someone_else = run_event(
            chat,
            1,
            EventKind::UserMessage,
            serde_json::json!({
                "blocks": [{ "type": "text", "text": "hello" }],
            }),
        );
        someone_else.author =
            Author::User(aigentic_runtime::aigentic_core::UserId("magnus".into()));
        // The chat's and the lead's seq counters are separate.
        let chat_seq = someone_else.seq;
        lead.push(Notice::Event {
            thread: chat,
            event: someone_else,
        });
        lead.pump().await;
        assert!(
            lead.lines().iter().any(|l| l.contains("magnus: hello")),
            "the chat still draws (seq {chat_seq}): {:?}",
            lead.lines()
        );

        // A seq already printed is not drawn twice: two notices for the
        // same event make one cell.
        let printed = lead.lines().len();
        let event = run_event(
            lead_id,
            7,
            EventKind::StepStarted,
            step_started("once", 1, Ulid::generate()),
        );
        lead.push(Notice::Event {
            thread: lead_id,
            event: event.clone(),
        });
        lead.pump().await;
        lead.push(Notice::Event {
            thread: lead_id,
            event: event.clone(),
        });
        lead.pump().await;
        assert_eq!(
            lead.lines().len(),
            printed + 1,
            "one cell for the seq, not two: {:?}",
            lead.lines()
        );
    }

    /// T5/T6: the boundary between backlog and live — an event the
    /// backlog already drew arrives live and is skipped.
    #[tokio::test]
    async fn a_live_event_the_backlog_already_drew_is_skipped() {
        let lead_id = Ulid::generate();
        let backlog = vec![run_event(
            lead_id,
            1,
            EventKind::StepStarted,
            step_started("backlog", 1, Ulid::generate()),
        )];
        let (mut repl, mut out, daemon) = repl_for(lead_id, backlog.clone()).await;
        repl.handle_line("/build 58", &mut out).await;
        let drawn = out.0.0.clone();
        repl.render(
            Notice::Event {
                thread: lead_id,
                event: backlog[0].clone(),
            },
            &mut out,
        );
        assert_eq!(
            out.0.0, drawn,
            "the same seq, from the backlog and live, is one cell"
        );
        daemon.stop();
    }

    /// `Leave it waiting` and Esc hide the prompt and send nothing, and
    /// the gate stays open: a later `/answer` still answers it.
    #[tokio::test]
    async fn leaving_the_gate_waiting_sends_nothing() {
        let lead_id = Ulid::generate();
        let backlog = vec![run_event(
            lead_id,
            1,
            EventKind::CheckpointAsked,
            checkpoint_asked("wait-here"),
        )];
        let (mut repl, mut out, daemon) = repl_for(lead_id, backlog).await;
        repl.handle_line("/build 58", &mut out).await;
        let before = daemon.requests().len();
        repl.handle_line("1", &mut out).await;
        assert_eq!(
            out.0.0.last().unwrap(),
            "[left wait-here waiting: /answer answers it, /build <n> shows it again]",
            "{:#?}",
            out.0.0
        );
        assert!(repl.menu().is_none(), "the prompt went without an answer");
        assert_eq!(daemon.requests().len(), before, "nothing was sent");
        repl.handle_line("/answer stop", &mut out).await;
        assert!(
            matches!(
                daemon.requests()[before..],
                [Request::AnswerCheckpoint {
                    answer: aigentic_api::CheckpointAnswer::Stop,
                    ..
                }]
            ),
            "the gate left waiting is still answered: {:?}",
            daemon.requests()
        );

        // Esc is the same choice, on the prompt itself.
        let lead_id = Ulid::generate();
        let backlog = vec![run_event(
            lead_id,
            1,
            EventKind::CheckpointAsked,
            checkpoint_asked("esc-gate"),
        )];
        let (mut repl, mut out, daemon) = repl_for(lead_id, backlog).await;
        repl.handle_line("/build 58", &mut out).await;
        let before = daemon.requests().len();
        let used = repl.menu_key(&key_for_esc(), true, true, &mut out).await;
        assert_eq!(used, MenuKey::Used, "Esc is the prompt's");
        assert!(repl.menu().is_none(), "the prompt went");
        assert_eq!(daemon.requests().len(), before, "nothing was sent");
        daemon.stop();
    }

    /// A workflow's own checkpoint offers what it takes: `Continue`,
    /// `Continue with changes`, `Leave it waiting` and `Stop the run`. The
    /// selection starts on the row that sends nothing, so Enter alone
    /// neither stops nor continues the run.
    #[tokio::test]
    async fn a_workflow_checkpoint_offers_go_amend_and_stop() {
        let gate = |name: &str| {
            serde_json::json!({
                "gate": name,
                "shown": ["read the spec"],
                "options": ["go", "amend", "stop"],
            })
        };
        let rows = |repl: &ClientRepl| -> Vec<String> {
            repl.menu()
                .expect("the prompt is up")
                .rows
                .iter()
                .map(|row| row.label.clone())
                .collect()
        };

        // Enter on the fresh prompt leaves the run waiting.
        let lead_id = Ulid::generate();
        let backlog = vec![run_event(
            lead_id,
            1,
            EventKind::CheckpointAsked,
            gate("decide"),
        )];
        let (mut repl, mut out, daemon) = repl_for(lead_id, backlog).await;
        repl.handle_line("/build 58", &mut out).await;
        assert_eq!(
            rows(&repl),
            [
                "Continue",
                "Continue with changes",
                "Leave it waiting",
                "Stop the run"
            ]
        );
        let before = daemon.requests().len();
        repl.menu_key(&key_for_enter(), true, true, &mut out).await;
        assert_eq!(daemon.requests().len(), before, "Enter sent nothing");
        assert!(repl.menu().is_none(), "and left the gate waiting");
        daemon.stop();

        // `Continue` sends `go`.
        let lead_id = Ulid::generate();
        let backlog = vec![run_event(
            lead_id,
            1,
            EventKind::CheckpointAsked,
            gate("decide"),
        )];
        let (mut repl, mut out, daemon) = repl_for(lead_id, backlog).await;
        repl.handle_line("/build 58", &mut out).await;
        let before = daemon.requests().len();
        repl.menu_key(&key_for('1'), true, true, &mut out).await;
        assert!(
            matches!(
                daemon.requests()[before..],
                [Request::AnswerCheckpoint {
                    answer: aigentic_api::CheckpointAnswer::Go,
                    amendment: None,
                    ..
                }]
            ),
            "{:?}",
            daemon.requests()
        );
        daemon.stop();

        // `Continue with changes` takes the next line as the amendment.
        let lead_id = Ulid::generate();
        let backlog = vec![run_event(
            lead_id,
            1,
            EventKind::CheckpointAsked,
            gate("decide"),
        )];
        let (mut repl, mut out, daemon) = repl_for(lead_id, backlog).await;
        repl.handle_line("/build 58", &mut out).await;
        let before = daemon.requests().len();
        repl.menu_key(&key_for('2'), true, true, &mut out).await;
        assert_eq!(daemon.requests().len(), before, "the text comes first");
        repl.handle_line("keep the temp dir", &mut out).await;
        match &daemon.requests()[before..] {
            [
                Request::AnswerCheckpoint {
                    answer: aigentic_api::CheckpointAnswer::Amend,
                    amendment: Some(text),
                    ..
                },
            ] => assert_eq!(text, "keep the temp dir"),
            other => panic!("the line is the amendment: {other:?}"),
        }
        daemon.stop();

        // `amend <text>` typed at the prompt is the same answer.
        let lead_id = Ulid::generate();
        let backlog = vec![run_event(
            lead_id,
            1,
            EventKind::CheckpointAsked,
            gate("decide"),
        )];
        let (mut repl, mut out, daemon) = repl_for(lead_id, backlog).await;
        repl.handle_line("/build 58", &mut out).await;
        let before = daemon.requests().len();
        repl.handle_line("amend split item 7", &mut out).await;
        match &daemon.requests()[before..] {
            [
                Request::AnswerCheckpoint {
                    answer: aigentic_api::CheckpointAnswer::Amend,
                    amendment: Some(text),
                    ..
                },
            ] => assert_eq!(text, "split item 7"),
            other => panic!("the typed amendment is sent: {other:?}"),
        }
        daemon.stop();
    }

    /// T8: a chat-thread state change leaves the gate's prompt up, and
    /// it can still be answered.
    #[tokio::test]
    async fn the_gate_survives_a_chat_state_change_and_is_still_answerable() {
        let lead_id = Ulid::generate();
        let backlog = vec![run_event(
            lead_id,
            1,
            EventKind::CheckpointAsked,
            checkpoint_asked("survivor"),
        )];
        let (mut repl, mut out, daemon) = repl_for(lead_id, backlog).await;
        repl.handle_line("/build 58", &mut out).await;
        assert!(repl.menu().is_some());

        let chat = repl.thread;
        repl.render(
            Notice::State {
                thread: chat,
                state: ThreadState::Running {
                    by: Author::User(aigentic_runtime::aigentic_core::UserId("steve".into())),
                    queued: 0,
                },
            },
            &mut out,
        );
        repl.render(
            Notice::State {
                thread: chat,
                state: ThreadState::Idle,
            },
            &mut out,
        );
        assert!(
            repl.menu().is_some(),
            "the prompt is drawn apart from the chat's own"
        );

        let before = daemon.requests().len();
        repl.menu_key(&key_for('2'), true, true, &mut out).await;
        let sent = daemon.requests()[before..].to_vec();
        assert!(
            matches!(
                sent[0],
                Request::AnswerCheckpoint {
                    answer: aigentic_api::CheckpointAnswer::Stop,
                    ..
                }
            ),
            "still answerable: {sent:?}"
        );
        daemon.stop();
    }

    /// T9: while a run is followed and the chat is idle with an empty
    /// composer, the first Ctrl-C detaches; the prompt goes with it, and
    /// later lead notices draw nothing.
    #[tokio::test]
    async fn the_first_ctrl_c_detaches_from_the_followed_run() {
        let lead_id = Ulid::generate();
        let backlog = vec![run_event(
            lead_id,
            1,
            EventKind::CheckpointAsked,
            checkpoint_asked("d-gate"),
        )];
        let mut lead = Lead::start(lead_id, backlog).await;
        lead.line("/build 58").await;
        lead.pump().await;

        let ctx = crate::app::keymap::KeyContext {
            running: false,
            composer_empty: true,
            following: true,
        };
        let action = crate::app::keymap::action_for(&key_for_ctrl_c(), ctx);
        assert_eq!(
            action,
            crate::app::keymap::Action::DetachRun,
            "Ctrl-C detaches while a run is followed"
        );
        lead.repl.detach(&mut lead.out);
        let expected = format!(
            "[detached from run {lead_id}: it keeps running in the daemon; without --server, \
             closing this REPL pauses it until /build 58]"
        );
        assert_eq!(lead.out.0.0.last().unwrap(), &expected);
        assert!(!lead.repl.following(), "the follow ended");
        assert!(lead.repl.menu().is_none(), "its prompt went too");

        // The daemon keeps pushing: nothing is drawn.
        let drawn = lead.lines().len();
        lead.push(Notice::Event {
            thread: lead_id,
            event: run_event(
                lead_id,
                2,
                EventKind::StepStarted,
                step_started("after", 1, Ulid::generate()),
            ),
        });
        lead.pump().await;
        assert_eq!(lead.lines().len(), drawn, "a detached lead is not drawn");

        // The next Ctrl-C is the ordinary one: with the composer empty
        // it arms quit, and Esc arms recall.
        let ctx = crate::app::keymap::KeyContext {
            running: false,
            composer_empty: true,
            following: false,
        };
        assert_eq!(
            crate::app::keymap::action_for(&key_for_ctrl_c(), ctx),
            crate::app::keymap::Action::QuitArm
        );
        assert_eq!(
            crate::app::keymap::action_for(&key_for_esc(), ctx),
            crate::app::keymap::Action::RecallArm
        );
    }

    /// T10: without the approve role, `/build` says so and sends
    /// nothing.
    #[tokio::test]
    async fn build_without_the_approve_role_sends_nothing() {
        let mut lead = Lead::with(Ulid::generate(), Vec::new(), "read", None, false).await;
        lead.line("/build 58").await;
        assert_eq!(
            lead.lines(),
            vec!["[/build needs the approve role in this project]".to_owned()]
        );
        assert!(
            !lead
                .daemon
                .requests()
                .iter()
                .any(|request| matches!(request, Request::Build { .. })),
            "no build went out: {:?}",
            lead.daemon.requests()
        );
    }

    /// T11: a refused `Build` prints the reason. A lead's `Note`
    /// starting `run stopped` becomes a cell and ends the follow.
    #[tokio::test]
    async fn a_refused_build_and_a_run_stopped_note() {
        let mut lead = Lead::with(
            Ulid::generate(),
            Vec::new(),
            "admin",
            Some("the issue is not open".into()),
            false,
        )
        .await;
        lead.line("/build 58").await;
        assert_eq!(
            lead.lines(),
            vec!["[build refused: the issue is not open]".to_owned()]
        );
        assert!(!lead.repl.following(), "nothing is followed");

        let lead_id = Ulid::generate();
        let mut lead = Lead::start(lead_id, Vec::new()).await;
        lead.line("/build 58").await;
        lead.pump().await;
        assert!(lead.repl.following());
        lead.push(Notice::Note {
            thread: lead_id,
            text: "run stopped: the budget went".into(),
        });
        lead.pump().await;
        assert_eq!(
            lead.lines().last().unwrap(),
            "▸ note: run stopped: the budget went"
        );
        assert!(!lead.repl.following(), "the note ended the follow");
    }

    /// T14: an answer from another session withdraws the prompt without
    /// this client sending anything.
    #[tokio::test]
    async fn an_answer_from_elsewhere_withdraws_the_prompt() {
        let lead_id = Ulid::generate();
        let backlog = vec![run_event(
            lead_id,
            1,
            EventKind::CheckpointAsked,
            checkpoint_asked("elsewhere"),
        )];
        let (mut repl, mut out, daemon) = repl_for(lead_id, backlog).await;
        repl.handle_line("/build 58", &mut out).await;
        assert!(repl.menu().is_some());
        let before = daemon.requests().len();
        repl.render(
            Notice::Event {
                thread: lead_id,
                event: run_event(
                    lead_id,
                    2,
                    EventKind::CheckpointAnswered,
                    serde_json::json!({ "answer": "stop" }),
                ),
            },
            &mut out,
        );
        assert!(repl.menu().is_none(), "the prompt went");
        assert_eq!(daemon.requests().len(), before, "this client sent nothing");
    }

    /// T13: with the prompt up in plain mode, `/help` still runs the
    /// command and an unrelated line goes to the chat.
    #[tokio::test]
    async fn a_slash_line_still_runs_and_another_line_goes_to_the_chat() {
        let lead_id = Ulid::generate();
        let backlog = vec![run_event(
            lead_id,
            1,
            EventKind::CheckpointAsked,
            checkpoint_asked("plain"),
        )];
        let (mut repl, mut out, daemon) = repl_for(lead_id, backlog).await;
        repl.handle_line("/build 58", &mut out).await;
        let before = daemon.requests().len();
        repl.handle_line("/help", &mut out).await;
        assert!(
            out.0.0.iter().any(|l| l.starts_with("/build <n>")),
            "the command still ran: {:?}",
            out.0.0
        );
        assert!(repl.menu().is_some(), "and left the prompt alone");
        assert_eq!(daemon.requests().len(), before, "and sent nothing");

        // The prompt in plain mode is `Menu::plain`'s own text.
        let prompt = repl.menu().expect("still up").plain();
        assert_eq!(prompt[0], "[checkpoint] checkpoint plain", "{prompt:?}");
        assert!(
            prompt.iter().any(|l| l.starts_with("  2. Stop the run")),
            "the picks are numbered for a typed answer: {prompt:?}"
        );

        // And a typed `2` is that pick: it sends the answer and goes.
        let before = daemon.requests().len();
        repl.handle_line("2", &mut out).await;
        let sent = daemon.requests()[before..].to_vec();
        match &sent[0] {
            Request::AnswerCheckpoint {
                lead: answered,
                gate,
                answer,
                amendment,
            } => {
                assert_eq!(*answered, lead_id);
                assert_eq!(gate, "plain");
                assert_eq!(*answer, aigentic_api::CheckpointAnswer::Stop);
                assert!(amendment.is_none());
            }
            other => panic!("{other:?}"),
        }
        assert!(repl.menu().is_none(), "the prompt went on `Ok`");

        repl.handle_line("just a thought", &mut out).await;
        let sent = daemon.requests()[before..].to_vec();
        assert!(
            matches!(sent[1], Request::Post { .. }),
            "an unrelated line is the chat's: {sent:?}"
        );
    }

    /// T15: a chat permission prompt and a checkpoint at once: the
    /// chat's comes first, and the checkpoint is shown once it ends.
    #[tokio::test]
    async fn the_chat_prompt_comes_first_and_the_gate_after_it() {
        let lead_id = Ulid::generate();
        let backlog = vec![run_event(
            lead_id,
            1,
            EventKind::CheckpointAsked,
            checkpoint_asked("second"),
        )];
        let (mut repl, mut out, daemon) = repl_for(lead_id, backlog).await;
        repl.handle_line("/build 58", &mut out).await;

        let call_id = "c1";
        repl.render(
            Notice::State {
                thread: repl.thread,
                state: approval_state(call_id),
            },
            &mut out,
        );
        assert!(repl.prompted.is_some(), "the chat's prompt was set up");
        let title = repl.menu().expect("a menu").title.clone();
        assert!(
            !title.starts_with("checkpoint"),
            "the permission prompt comes first, not the gate's: {title}"
        );

        // The chat prompt ends: the checkpoint prompt is what is left.
        repl.render(
            Notice::State {
                thread: repl.thread,
                state: ThreadState::Idle,
            },
            &mut out,
        );
        let title = repl
            .menu()
            .expect("the checkpoint prompt is still there")
            .title
            .clone();
        assert_eq!(title, "checkpoint second");
        daemon.stop();
    }

    /// T15b (#68's review): with the chat's permission prompt and the
    /// gate both up, a typed `1` is the chat's answer and never the
    /// gate's; the two prompts have different ids, so the shell's grace
    /// restarts when the gate's prompt takes the chat's place.
    #[tokio::test]
    async fn a_typed_line_answers_the_prompt_on_screen_not_the_gate() {
        let lead_id = Ulid::generate();
        let backlog = vec![run_event(
            lead_id,
            1,
            EventKind::CheckpointAsked,
            checkpoint_asked("second"),
        )];
        let (mut repl, mut out, daemon) = repl_for(lead_id, backlog).await;
        repl.handle_line("/build 58", &mut out).await;
        let gate_id = repl.menu_id().expect("the gate's prompt is up");
        assert!(gate_id.starts_with("gate:"), "{gate_id}");

        let call_id = "c1";
        repl.render(
            Notice::State {
                thread: repl.thread,
                state: approval_state(call_id),
            },
            &mut out,
        );
        let chat_id = repl.menu_id().expect("the chat's prompt is up");
        assert_eq!(chat_id, format!("chat:{call_id}"));
        assert_ne!(chat_id, gate_id, "a change of prompt is a change of id");

        repl.handle_line("1", &mut out).await;
        let requests = daemon.requests();
        assert!(
            requests
                .iter()
                .any(|r| matches!(r, Request::Decide { allow: true, .. })),
            "the `1` allowed the chat's call: {requests:?}"
        );
        assert!(
            !requests
                .iter()
                .any(|r| matches!(r, Request::AnswerCheckpoint { .. })),
            "and never answered the gate: {requests:?}"
        );
        daemon.stop();
    }

    /// T16: `/build 59` while following 58 detaches from 58 — prompt
    /// withdrawn, its later notices undrawn — then follows 59.
    #[tokio::test]
    async fn a_second_build_detaches_from_the_first_run() {
        let first = Ulid::generate();
        let mut lead = Lead::start(
            first,
            vec![run_event(
                first,
                1,
                EventKind::CheckpointAsked,
                checkpoint_asked("first-gate"),
            )],
        )
        .await;
        lead.line("/build 58").await;
        lead.pump().await;
        assert!(lead.repl.following());

        // The second run: the daemon answers `Build` with the same lead
        // (it has one), so the detach is what the log shows.
        lead.line("/build 59").await;
        lead.pump().await;
        assert!(
            lead.lines()
                .iter()
                .any(|l| l.starts_with("[detached from run ")),
            "the detach line: {:?}",
            lead.lines()
        );
        assert!(lead.repl.following(), "and then 59 is followed");
        assert!(
            lead.lines()
                .iter()
                .filter(|l| l.starts_with("▸ run "))
                .count()
                >= 2,
            "both runs have their cell: {:?}",
            lead.lines()
        );
    }

    /// T12: `AwaitingSwitch` prints the line, and `y`, `n` and `n customer
    /// X` send `Yes`, `No` and `Corrected`; an unrelated line goes to the
    /// chat as usual.
    #[tokio::test]
    async fn t12_the_repl_answers_a_switch_proposal_with_one_keystroke() {
        let (mut repl, mut out, daemon) = repl_for(Ulid::generate(), vec![]).await;
        let state = ThreadState::AwaitingSwitch {
            call_id: "c9".into(),
            project: "there".into(),
            workspace: Some("~/there".into()),
            reason: "the message is about the site".into(),
        };
        repl.render(
            Notice::State {
                thread: repl.thread,
                state: state.clone(),
            },
            &mut out,
        );
        // The in-place block replaces #7's typed line (#82's T7): the
        // menu is up, this client prompted for it, and the shell draws
        // it. The `Copies` printer shows what a pipe would print.
        assert_eq!(repl.menu().map(|m| m.kind), Some(Kind::Switch));
        assert_eq!(repl.menu_id().as_deref(), Some("chat:c9"));
        assert_eq!(
            out.0.0,
            vec![
                "[switch] switch to there?".to_owned(),
                "  the message is about the site".to_owned(),
                "  in workspace ~/there".to_owned(),
                "  1. Yes, switch to there".to_owned(),
                "  2. No, stay here".to_owned(),
                "  3. No, it belongs somewhere else…".to_owned(),
            ]
        );
        assert_eq!(repl.prompted.as_deref(), Some("c9"));

        repl.handle_line("y", &mut out).await;
        let answers: Vec<SwitchReply> = daemon
            .requests()
            .into_iter()
            .filter_map(|r| match r {
                Request::AnswerSwitch { answer, .. } => Some(answer),
                _ => None,
            })
            .collect();
        assert_eq!(answers, vec![SwitchReply::Yes]);

        // `n`, and `n <where>`.
        for (line, expected) in [
            ("n", SwitchReply::No),
            (
                "n customer X",
                SwitchReply::Corrected {
                    to: "customer X".into(),
                },
            ),
        ] {
            let (mut repl, mut out, daemon) = repl_for(Ulid::generate(), vec![]).await;
            repl.render(
                Notice::State {
                    thread: repl.thread,
                    state: state.clone(),
                },
                &mut out,
            );
            repl.handle_line(line, &mut out).await;
            let answers: Vec<SwitchReply> = daemon
                .requests()
                .into_iter()
                .filter_map(|r| match r {
                    Request::AnswerSwitch { answer, .. } => Some(answer),
                    _ => None,
                })
                .collect();
            assert_eq!(
                answers,
                vec![expected.clone()],
                "`{line}` sends {expected:?}"
            );
            daemon.stop();
        }

        // Anything else is a chat line, as before.
        let (mut repl, mut out, daemon) = repl_for(Ulid::generate(), vec![]).await;
        repl.render(
            Notice::State {
                thread: repl.thread,
                state: state.clone(),
            },
            &mut out,
        );
        repl.handle_line("good morning", &mut out).await;
        let requests = daemon.requests();
        assert!(
            !requests
                .iter()
                .any(|r| matches!(r, Request::AnswerSwitch { .. })),
            "an unrelated line answers nothing: {requests:?}"
        );
        assert!(
            requests.iter().any(|r| matches!(r, Request::Post { .. })),
            "it goes to the chat: {requests:?}"
        );
        daemon.stop();
    }

    /// Every `AnswerSwitch` the daemon was sent, in order.
    fn switch_answers(daemon: &GuardedLead) -> Vec<SwitchReply> {
        daemon
            .requests()
            .into_iter()
            .filter_map(|r| match r {
                Request::AnswerSwitch { answer, .. } => Some(answer),
                _ => None,
            })
            .collect()
    }

    /// The chat thread waiting on a switch proposal (issue #82).
    fn switch_state(call_id: &str) -> ThreadState {
        ThreadState::AwaitingSwitch {
            call_id: call_id.into(),
            project: "customer".into(),
            workspace: Some("~/customer".into()),
            reason: "the message is about the site".into(),
        }
    }

    /// T4 (#82): `AwaitingSwitch` raises the switch block under
    /// `chat:{call_id}`, and each row answers the proposal with the
    /// specified echo — `1` yes, `2` no, `3` then a destination the
    /// composer hands over.
    #[tokio::test]
    async fn a_switch_proposal_shows_the_block_and_answers_by_row() {
        let state = switch_state("c9");

        // 1: yes.
        let (mut repl, mut out, daemon) = repl_for(Ulid::generate(), vec![]).await;
        repl.render(
            Notice::State {
                thread: repl.thread,
                state: state.clone(),
            },
            &mut out,
        );
        assert_eq!(repl.menu().map(|m| m.kind), Some(Kind::Switch));
        assert_eq!(
            repl.menu().map(|m| m.title.as_str()),
            Some("switch to customer?")
        );
        assert_eq!(repl.menu_id().as_deref(), Some("chat:c9"));
        repl.menu_key(&key_for('1'), true, true, &mut out).await;
        assert_eq!(switch_answers(&daemon), vec![SwitchReply::Yes]);
        assert!(
            out.0.0.iter().any(|l| l == "↳ Yes, switch to customer"),
            "{:#?}",
            out.0.0
        );
        daemon.stop();

        // 2: no.
        let (mut repl, mut out, daemon) = repl_for(Ulid::generate(), vec![]).await;
        repl.render(
            Notice::State {
                thread: repl.thread,
                state: state.clone(),
            },
            &mut out,
        );
        repl.menu_key(&key_for('2'), true, true, &mut out).await;
        assert_eq!(switch_answers(&daemon), vec![SwitchReply::No]);
        assert!(
            out.0.0.iter().any(|l| l == "↳ No, stay here"),
            "{:#?}",
            out.0.0
        );
        daemon.stop();

        // 3: the composer takes where it belongs.
        let (mut repl, mut out, daemon) = repl_for(Ulid::generate(), vec![]).await;
        repl.render(
            Notice::State {
                thread: repl.thread,
                state: state.clone(),
            },
            &mut out,
        );
        assert_eq!(
            repl.menu_key(&key_for('3'), true, true, &mut out).await,
            MenuKey::Text,
            "3 keys open the composer"
        );
        repl.prompt_text(Some("customer X".into()), &mut out).await;
        assert_eq!(
            switch_answers(&daemon),
            vec![SwitchReply::Corrected {
                to: "customer X".into()
            }]
        );
        assert!(
            out.0
                .0
                .iter()
                .any(|l| l == "↳ No, it belongs to: customer X"),
            "{:#?}",
            out.0.0
        );

        // An empty destination answers `No`, at once.
        repl.render(
            Notice::State {
                thread: repl.thread,
                state: switch_state("c10"),
            },
            &mut out,
        );
        repl.prompt_text(None, &mut out).await;
        assert_eq!(switch_answers(&daemon).last(), Some(&SwitchReply::No));
        assert!(
            out.0.0.iter().any(|l| l == "↳ No, stay here"),
            "{:#?}",
            out.0.0
        );
        daemon.stop();
    }

    /// T4 (#82): a switch answered elsewhere withdraws the block; the
    /// state that left `AwaitingSwitch` sends nothing.
    #[tokio::test]
    async fn a_switch_answered_elsewhere_leaves_no_menu() {
        let (mut repl, mut out, daemon) = repl_for(Ulid::generate(), vec![]).await;
        repl.render(
            Notice::State {
                thread: repl.thread,
                state: switch_state("c9"),
            },
            &mut out,
        );
        assert_eq!(repl.menu().map(|m| m.kind), Some(Kind::Switch));

        repl.render(
            Notice::State {
                thread: repl.thread,
                state: ThreadState::Idle,
            },
            &mut out,
        );
        assert_eq!(repl.menu(), None, "the block goes with the state");
        assert!(
            out.0.0.iter().any(|l| l == "[answered elsewhere]"),
            "{:#?}",
            out.0.0
        );
        assert!(switch_answers(&daemon).is_empty(), "and nothing is sent");
        daemon.stop();
    }

    /// T4 (#82): without `write` the user cannot answer a switch: the
    /// waiting line, and no menu.
    #[tokio::test]
    async fn a_user_without_write_sees_the_waiting_line() {
        let (mut repl, mut out, daemon) = repl_for(Ulid::generate(), vec![]).await;
        repl.role = Some("read".into());
        repl.render(
            Notice::State {
                thread: repl.thread,
                state: switch_state("c9"),
            },
            &mut out,
        );
        assert_eq!(repl.menu(), None);
        assert_eq!(repl.prompted, None);
        assert!(
            out.0
                .0
                .iter()
                .any(|l| l == "[waiting for an answer: switch to customer?]"),
            "{:#?}",
            out.0.0
        );
        daemon.stop();
    }

    /// T5 (#82): #68's grace, for a switch. A permission prompt answered
    /// by `1` and the Enter that follows leave the switch prompt that
    /// replaces it unanswered.
    #[tokio::test]
    async fn the_enter_after_a_permission_does_not_answer_the_switch() {
        let (mut repl, mut out, daemon) = repl_for(Ulid::generate(), vec![]).await;
        repl.render(
            Notice::State {
                thread: repl.thread,
                state: approval_state("c1"),
            },
            &mut out,
        );
        assert_eq!(repl.menu().map(|m| m.kind), Some(Kind::Permission));
        let permission_id = repl.menu_id().expect("the permission prompt is up");

        // The decision went out: the switch prompt takes the screen
        // with a new id, so the answering keys restart their grace.
        repl.render(
            Notice::State {
                thread: repl.thread,
                state: switch_state("c2"),
            },
            &mut out,
        );
        let switch_id = repl.menu_id().expect("the switch prompt is up");
        assert_ne!(
            permission_id, switch_id,
            "a change of prompt is a change of id"
        );
        assert!(
            repl.menu_key(&key_for_enter(), true, false, &mut out).await == MenuKey::Passed,
            "the Enter carrying over is not an answer"
        );
        assert!(
            switch_answers(&daemon).is_empty(),
            "the switch is unanswered: {:?}",
            daemon.requests()
        );
        daemon.stop();
    }

    /// T6 (#82): plain mode prints the switch block through
    /// `Menu::plain()`, and a typed `y` or `n <where>` answers it.
    #[tokio::test]
    async fn a_pipe_prints_the_switch_block_and_answers_it() {
        let (mut repl, mut out, daemon) = repl_for(Ulid::generate(), vec![]).await;
        repl.render(
            Notice::State {
                thread: repl.thread,
                state: switch_state("c9"),
            },
            &mut out,
        );
        assert!(
            out.0.0.iter().any(|l| l == "[switch] switch to customer?"),
            "{:#?}",
            out.0.0
        );
        assert!(
            out.0
                .0
                .iter()
                .any(|l| l.trim() == "1. Yes, switch to customer"),
            "{:#?}",
            out.0.0
        );
        repl.handle_line("y", &mut out).await;
        assert_eq!(switch_answers(&daemon), vec![SwitchReply::Yes]);
        daemon.stop();

        let (mut repl, mut out, daemon) = repl_for(Ulid::generate(), vec![]).await;
        repl.render(
            Notice::State {
                thread: repl.thread,
                state: switch_state("c9"),
            },
            &mut out,
        );
        repl.handle_line("n customer X", &mut out).await;
        assert_eq!(
            switch_answers(&daemon),
            vec![SwitchReply::Corrected {
                to: "customer X".into()
            }]
        );
        daemon.stop();

        // An unrelated line is a chat line.
        let (mut repl, mut out, daemon) = repl_for(Ulid::generate(), vec![]).await;
        repl.render(
            Notice::State {
                thread: repl.thread,
                state: switch_state("c9"),
            },
            &mut out,
        );
        repl.handle_line("good morning", &mut out).await;
        let requests = daemon.requests();
        assert!(
            switch_answers(&daemon).is_empty(),
            "an unrelated line answers nothing: {requests:?}"
        );
        assert!(
            requests.iter().any(|r| matches!(r, Request::Post { .. })),
            "it goes to the chat: {requests:?}"
        );
        daemon.stop();
    }

    /// A REPL over a scripted daemon with one lead, for the tests that
    /// need the parts rather than the bundle.
    /// T5 (issue #99): a `Notice::Usage` with the thread figure reaches
    /// the status line as `working · thread`, and one from a daemon
    /// before #99 leaves `working` alone.
    #[tokio::test]
    async fn the_usage_notice_feeds_working_and_thread() {
        let (mut repl, mut out, daemon) = repl_for(Ulid::generate(), Vec::new()).await;
        let usage = |thread_tokens| Notice::Usage {
            thread: repl.thread,
            tokens_in_window: 48_000,
            window: 120_000,
            turn_elapsed_ms: None,
            queued: 0,
            thread_tokens,
        };
        let with = usage(Some(2_100_000));
        let without = usage(None);
        repl.render(with, &mut out);
        let figures = repl.usage().expect("the notice was applied");
        assert_eq!(
            figures,
            crate::app::status::Figures {
                working: 48_000,
                thread: Some(2_100_000),
            }
        );
        let status = crate::app::status::Status {
            project: "proj".into(),
            mode: "manual".into(),
            usage: Some(figures),
            ..crate::app::status::Status::default()
        };
        assert!(
            status.line().ends_with(&format!(
                "working {} · thread {}",
                crate::app::status::count_short(48_000),
                crate::app::status::count_short(2_100_000)
            )),
            "{}",
            status.line()
        );
        repl.render(without, &mut out);
        assert_eq!(repl.usage().and_then(|f| f.thread), None);
        daemon.stop();
    }

    async fn repl_for(lead: Ulid, backlog: Vec<Event>) -> (ClientRepl, Copies, GuardedLead) {
        let dir = tempfile::tempdir().unwrap();
        let script = crate::run_view::fake_daemon::Script {
            lead,
            backlog,
            resumed: false,
            role: "admin",
            refuse_build: None,
            refuse_open: None,
        };
        let daemon = crate::run_view::fake_daemon::FakeDaemon::start(dir.path(), script).await;
        let (client, welcome) = Client::connect(&daemon.addr(), "tok").await.unwrap();
        let role = welcome.projects[0].role.clone();
        let (thread, state, mode) = open(&client, "proj", Some(Ulid::generate())).await;
        let repl = ClientRepl::new(
            client,
            thread,
            "steve",
            role,
            state,
            mode,
            Identity::default(),
            "proj",
        );
        let (daemon, _dir) = (daemon, dir);
        (repl, Copies::default(), GuardedLead { daemon, _dir })
    }

    /// A daemon with the directory its socket lives in, so the two
    /// outlive each other.
    struct GuardedLead {
        daemon: crate::run_view::fake_daemon::FakeDaemon,
        _dir: tempfile::TempDir,
    }

    impl GuardedLead {
        /// Every request the daemon has answered, in order.
        fn requests(&self) -> Vec<Request> {
            self.daemon.requests()
        }

        /// Stop the daemon's task.
        fn stop(self) {
            self.daemon.stop();
        }
    }

    /// Lines plus how often `thread_changed` was called, for `/new`
    /// (issue #89, T5).
    #[derive(Default)]
    struct Recording {
        lines: Vec<String>,
        changed: usize,
    }

    impl Printer for Recording {
        fn line(&mut self, text: &str) {
            self.lines.push(text.to_owned());
        }

        fn thread_changed(&mut self) {
            self.changed += 1;
        }
    }

    /// A listing row for the group renderer's tests (issue #97): the
    /// title is `{title}`, so an expected label reads as written.
    fn thread_info(
        id: Ulid,
        project: Option<&str>,
        workspace: Option<&str>,
        kind: aigentic_api::ThreadKind,
        title: &str,
    ) -> aigentic_api::ThreadInfo {
        aigentic_api::ThreadInfo {
            id,
            project: project.map(str::to_owned),
            date: "2026-10-05".into(),
            events: 3,
            first_line: format!("{title} first"),
            state: ThreadState::Idle,
            title: Some(title.into()),
            workspace: workspace.map(str::to_owned),
            kind,
        }
    }

    /// T1 (issue #97): the renderer walks the daemon's order, writes a
    /// heading at each boundary, and builds every row from its own
    /// fields — the `#issue`/`[step]` label, the build indent, and the
    /// ` · project` suffix a row earns against `current`.
    #[test]
    fn thread_groups_draw_every_section_from_the_daemons_order() {
        use aigentic_api::{RunThread, ThreadKind};

        let front = thread_info(
            Ulid::from_parts(1, 1),
            Some("b"),
            Some("w"),
            ThreadKind::Front,
            "front",
        );
        let lead1 = thread_info(
            Ulid::from_parts(1, 2),
            Some("b"),
            None,
            ThreadKind::Run(RunThread::Lead { issue: 42 }),
            "lead one",
        );
        let child1 = thread_info(
            Ulid::from_parts(1, 3),
            Some("a"),
            None,
            ThreadKind::Run(RunThread::Child {
                lead: lead1.id,
                step: Some("build".into()),
            }),
            "child one",
        );
        let grand1 = thread_info(
            Ulid::from_parts(1, 4),
            Some("b"),
            None,
            ThreadKind::Run(RunThread::Child {
                lead: child1.id,
                step: Some("verify".into()),
            }),
            "grand one",
        );
        let lead2 = thread_info(
            Ulid::from_parts(1, 5),
            Some("b"),
            None,
            ThreadKind::Run(RunThread::Lead { issue: 7 }),
            "lead two",
        );
        let step_less = thread_info(
            Ulid::from_parts(1, 6),
            Some("b"),
            None,
            ThreadKind::Run(RunThread::Child {
                lead: lead2.id,
                step: None,
            }),
            "no step",
        );
        let orphan = thread_info(
            Ulid::from_parts(1, 7),
            Some("b"),
            None,
            ThreadKind::Run(RunThread::Child {
                lead: Ulid::from_parts(9, 9),
                step: Some("check".into()),
            }),
            "orphan",
        );
        let p1 = thread_info(
            Ulid::from_parts(1, 8),
            Some("b"),
            Some("w"),
            ThreadKind::Thread,
            "plain w here",
        );
        let p2 = thread_info(
            Ulid::from_parts(1, 9),
            Some("a"),
            Some("w"),
            ThreadKind::Thread,
            "plain w there",
        );
        let p3 = thread_info(
            Ulid::from_parts(1, 10),
            Some("c"),
            Some("v"),
            ThreadKind::Thread,
            "plain v",
        );
        let p4 = thread_info(
            Ulid::from_parts(1, 11),
            None,
            Some("v"),
            ThreadKind::Thread,
            "no project",
        );
        let p5 = thread_info(
            Ulid::from_parts(1, 12),
            Some("a"),
            None,
            ThreadKind::Thread,
            "loose there",
        );
        let p6 = thread_info(
            Ulid::from_parts(1, 13),
            Some("b"),
            None,
            ThreadKind::Thread,
            "loose here",
        );

        // The daemon's order (#86): the front row, the build forest,
        // then the rest grouped by workspace, groups contiguous.
        let rows = vec![
            front.clone(),
            lead1.clone(),
            child1.clone(),
            grand1.clone(),
            lead2.clone(),
            step_less.clone(),
            orphan.clone(),
            p1.clone(),
            p2.clone(),
            p3.clone(),
            p4.clone(),
            p5.clone(),
            p6.clone(),
        ];

        // The spec's line shape, written from each fixture row: two
        // spaces under its heading plus two per depth, the row's own
        // columns, the label, and the suffix its project earns against
        // `current` (`b`, the REPL's project).
        let row = |t: &aigentic_api::ThreadInfo, depth: usize, label: String| {
            let suffix = match &t.project {
                Some(p) if p != "b" => format!(" · {p}"),
                _ => String::new(),
            };
            format!(
                "{}{}  {}  {:>5}  {label}{suffix}",
                "  ".repeat(depth + 1),
                t.id,
                t.date,
                t.events
            )
        };
        let expected = [
            "front thread".to_owned(),
            row(&front, 0, "front".into()),
            "builds".to_owned(),
            row(&lead1, 0, "#42 lead one".into()),
            row(&child1, 1, "[build] child one".into()),
            row(&grand1, 2, "[verify] grand one".into()),
            row(&lead2, 0, "#7 lead two".into()),
            row(&step_less, 1, "no step".into()),
            row(&orphan, 0, "[check] orphan".into()),
            "threads · w".to_owned(),
            row(&p1, 0, "plain w here".into()),
            row(&p2, 0, "plain w there".into()),
            "threads · v".to_owned(),
            row(&p3, 0, "plain v".into()),
            row(&p4, 0, "no project".into()),
            "threads · no workspace".to_owned(),
            row(&p5, 0, "loose there".into()),
            row(&p6, 0, "loose here".into()),
        ];

        assert_eq!(
            render_thread_groups(&rows, "b"),
            expected.join("\n"),
            "every row keeps its place, its label and its indent"
        );
    }

    /// T2 (issue #97): a heading appears only when its section has rows,
    /// the no-workspace run collapses to `threads` when it is all there
    /// is, and an empty listing still says so.
    #[test]
    fn thread_group_headings_appear_only_with_rows() {
        use aigentic_api::ThreadKind;

        let plain = |id: u128| {
            thread_info(
                Ulid::from_parts(2, id),
                None,
                None,
                ThreadKind::Thread,
                "plain",
            )
        };

        // All of the rest in no workspace: one `threads` heading.
        let no_workspace = vec![plain(1), plain(2)];
        let drawn = render_thread_groups(&no_workspace, "b");
        assert_eq!(drawn.matches("threads").count(), 1, "{drawn}");
        assert!(!drawn.contains("no workspace"), "{drawn}");

        // A front row and nothing else: only `front thread`.
        let front_only = vec![thread_info(
            Ulid::from_parts(2, 3),
            Some("b"),
            None,
            ThreadKind::Front,
            "front",
        )];
        let drawn = render_thread_groups(&front_only, "b");
        assert_eq!(drawn.lines().next(), Some("front thread"), "{drawn}");
        assert!(!drawn.contains("builds"), "{drawn}");
        assert!(!drawn.contains("\nthreads"), "{drawn}");

        // No front row and no runs: no `front thread`, no `builds`, but
        // the rest still gets its heading.
        let neither = vec![plain(4)];
        let drawn = render_thread_groups(&neither, "b");
        assert!(!drawn.contains("front thread"), "{drawn}");
        assert!(!drawn.contains("builds"), "{drawn}");
        assert_eq!(drawn.lines().next(), Some("threads"), "{drawn}");

        // An empty listing is still `no threads`.
        assert_eq!(render_thread_groups(&[], "b"), "no threads");
    }

    /// T4 (issue #97): `/threads` lists every project, grouped — the
    /// front thread first, then the rest by workspace — and a row of
    /// another project says which one. It replaces #86's
    /// `threads_lists_the_threads_own_project`, whose rule is gone.
    #[tokio::test]
    async fn threads_lists_every_project_grouped() {
        let dir = tempfile::tempdir().unwrap();
        let a = project(dir.path(), "a", "[participants]\nsteve = \"admin\"\n");
        let b = project(dir.path(), "b", "[participants]\nsteve = \"admin\"\n");
        let addr =
            crate::app::rig::daemon(dir.path(), &[("steve", "t")], &[("a", a), ("b", b)]).await;
        let (client, welcome) = Client::connect(&addr, "t").await.unwrap();
        // A thread in each project, and this user's front thread in `b`.
        let (in_a, _, _) = open(&client, "a", None).await;
        let front = crate::front::pick_thread(&client, None, false, "b", None, None)
            .await
            .unwrap();
        // The REPL's own thread, in `b`, newer than `a`'s.
        let (in_b, state, mode) = open(&client, "b", None).await;
        let role = welcome
            .projects
            .iter()
            .find(|p| p.name == "b")
            .and_then(|p| p.role.clone());
        let notices = client.take_notices().unwrap();
        let mut repl = ClientRepl::new(
            client,
            in_b,
            "steve",
            role,
            state,
            mode,
            Identity::default(),
            "b",
        );

        let (tx, rx) = mpsc::unbounded_channel();
        tx.send("/threads".into()).unwrap();
        tx.send("/quit".into()).unwrap();
        let mut out = Recording::default();
        repl.run(rx, notices, &mut out).await;

        let lines = out.lines.join("\n");
        // One entry per rendered line: the front heading, then its row.
        assert_eq!(out.lines[0], "front thread", "{lines}");
        assert!(
            out.lines[1].contains(&front.id.to_string()),
            "the front thread follows its heading: {lines}"
        );
        let at = |needle: &str| {
            out.lines
                .iter()
                .position(|l| l.contains(needle))
                .unwrap_or_else(|| panic!("{needle} is not listed: {lines}"))
        };
        let own = at(&in_b.to_string());
        let other = at(&in_a.to_string());
        // Both are in no workspace, so one `threads` group; inside it
        // the newer row comes first, and only the other project's row
        // carries a suffix (the REPL is in `b`).
        assert!(own < other, "{lines}");
        assert!(!out.lines[own].ends_with(" · b"), "{lines}");
        assert!(out.lines[other].ends_with(" · a"), "{lines}");
    }

    /// T5 and T7: `/new` starts a new front thread, the REPL follows it
    /// into the project the old thread had moved to, and the session's
    /// mode comes with it.
    #[tokio::test]
    async fn new_moves_the_repl_to_a_new_front_thread_in_the_threads_project() {
        let dir = tempfile::tempdir().unwrap();
        let a = project(dir.path(), "a", "[participants]\nsteve = \"admin\"\n");
        let b = project(dir.path(), "b", "[participants]\nsteve = \"admin\"\n");
        let addr =
            crate::app::rig::daemon(dir.path(), &[("steve", "t")], &[("a", a), ("b", b)]).await;
        let (client, welcome) = Client::connect(&addr, "t").await.unwrap();
        // The folder is in `b`: the thread is born there, and the REPL's
        // home project is `b`.
        let role = welcome
            .projects
            .iter()
            .find(|p| p.name == "b")
            .and_then(|p| p.role.clone());
        let front = crate::front::pick_thread(&client, None, false, "b", None, None)
            .await
            .unwrap();
        let (old, state, mode) = open(&client, "b", Some(front.id)).await;
        let notices = client.take_notices().unwrap();
        let mut repl = ClientRepl::new(
            client,
            old,
            "steve",
            role,
            state,
            mode,
            Identity::default(),
            "b",
        );
        // A session in auto, as `--mode auto` or `/mode auto` leaves it.
        repl.mode = "auto".to_owned();

        let (tx, rx) = mpsc::unbounded_channel();
        let feeder = async move {
            tx.send("/project use a".into()).unwrap();
            // The switch is the thread's own event, seen as a notice: the
            // REPL only knows its new project once that lands.
            tokio::time::sleep(std::time::Duration::from_millis(600)).await;
            tx.send("/new".into()).unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(600)).await;
            tx.send("/quit".into()).unwrap();
        };
        let mut out = Recording::default();
        tokio::join!(repl.run(rx, notices, &mut out), feeder);

        let new = repl.thread;
        assert_ne!(new, old, "the REPL moved to a new thread");
        assert_eq!(
            repl.project(),
            Some("a"),
            "the new thread is in the project the old one had moved to"
        );
        assert_eq!(repl.mode(), "auto", "the session's mode came with it");
        assert_eq!(out.changed, 1, "the printer was told once");
        assert!(
            out.lines.contains(&format!(
                "new front thread {new} · the old one stays listed in /threads"
            )),
            "the line names the new id: {:?}",
            out.lines
        );

        // A second client: the new thread is the front one, in `a`, in
        // auto, and both threads are listed there.
        let (second, _) = Client::connect(&addr, "t").await.unwrap();
        let resumed = crate::front::pick_thread(&second, None, false, "a", None, None)
            .await
            .unwrap();
        assert_eq!(resumed.info.as_ref().unwrap().id, new);
        let Response::Opened { mode, .. } = second
            .request(Request::Open {
                thread: new,
                from_seq: 0,
            })
            .await
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(mode, "auto");
        let Response::Threads { threads } = second
            .request(Request::ListThreads {
                project: Some("a".into()),
            })
            .await
            .unwrap()
        else {
            panic!()
        };
        let info = threads
            .iter()
            .find(|t| t.id == new)
            .expect("the new thread");
        assert!(
            threads.iter().any(|t| t.id == old),
            "the old thread stays listed"
        );
        // What the thread moved to: `None` when it is the home project.
        assert_eq!(
            repl.project(),
            info.project.as_deref().filter(|p| *p != "b")
        );
        assert_eq!(
            repl.title().map(str::to_owned),
            info.title.clone().filter(|t| !t.is_empty())
        );
    }

    /// T6: `/new` clears the old thread's per-thread state, and the old
    /// thread's notices stop drawing.
    #[tokio::test]
    async fn new_clears_the_old_threads_approval_state_and_its_notes() {
        let dir = tempfile::tempdir().unwrap();
        let root = project(dir.path(), "p", "[participants]\nsteve = \"admin\"\n");
        let addr = crate::app::rig::daemon(dir.path(), &[("steve", "t")], &[("p", root)]).await;
        let (client, welcome) = Client::connect(&addr, "t").await.unwrap();
        let role = welcome.projects[0].role.clone();
        let (old, state, mode) = open(&client, "p", None).await;
        let mut repl = ClientRepl::new(
            client,
            old,
            "steve",
            role,
            state,
            mode,
            Identity::default(),
            "p",
        );
        // A gate up: the id of the call it waits on, and the menu drawn.
        repl.prompted = Some("c1".to_owned());
        repl.menu = Some(Menu::checkpoint(
            "c1",
            &["ls".to_owned()],
            &["stop".to_owned()],
        ));

        let mut out = Recording::default();
        repl.handle_line("/new", &mut out).await;
        assert_ne!(repl.thread, old);
        assert!(repl.prompted.is_none(), "the old thread's gate is dropped");
        assert!(repl.menu.is_none(), "and its menu with it");
        assert_eq!(out.changed, 1);

        // The old thread is no longer subscribed: its note draws nothing,
        // the new thread's does.
        let drawn = out.lines.len();
        repl.render(
            Notice::Note {
                thread: old,
                text: "old news".into(),
            },
            &mut out,
        );
        assert_eq!(out.lines.len(), drawn, "the old thread's note is dropped");
        repl.render(
            Notice::Note {
                thread: repl.thread,
                text: "new news".into(),
            },
            &mut out,
        );
        assert_eq!(out.lines.last().unwrap(), "[new news]");
    }

    /// T8a: a read-only user's `/new` is refused and nothing changes.
    #[tokio::test]
    async fn a_refused_new_leaves_the_repl_on_the_old_thread() {
        let dir = tempfile::tempdir().unwrap();
        let root = project(
            dir.path(),
            "p",
            "[participants]\nsteve = \"admin\"\nmagnus = \"read\"\n",
        );
        let addr = crate::app::rig::daemon(
            dir.path(),
            &[("steve", "t-steve"), ("magnus", "t-magnus")],
            &[("p", root)],
        )
        .await;
        let (steve, _) = Client::connect(&addr, "t-steve").await.unwrap();
        let (thread, ..) = open(&steve, "p", None).await;
        let (magnus, welcome) = Client::connect(&addr, "t-magnus").await.unwrap();
        let role = welcome.projects[0].role.clone();
        let (_, state, mode) = open(&magnus, "p", Some(thread)).await;
        let mut repl = ClientRepl::new(
            magnus,
            thread,
            "magnus",
            role,
            state,
            mode,
            Identity::default(),
            "p",
        );
        let mut out = Recording::default();
        repl.handle_line("/new", &mut out).await;
        assert_eq!(repl.thread, thread, "the REPL stays where it was");
        assert_eq!(out.changed, 0);
        let printed = out.lines.last().unwrap();
        assert!(
            printed.starts_with("cannot start a new thread in p: "),
            "{printed}"
        );
        assert!(
            printed.contains("read"),
            "the daemon's reason is printed: {printed}"
        );
        // Still subscribed: a note for the old thread draws.
        repl.render(
            Notice::Note {
                thread,
                text: "still here".into(),
            },
            &mut out,
        );
        assert_eq!(out.lines.last().unwrap(), "[still here]");
    }

    /// T8b: when `Open` of the new front thread fails, the REPL stays
    /// wholly on the old one. No real daemon refuses that `Open` — a
    /// fresh thread in a project this client may write is always
    /// openable — so a scripted daemon answers `NewFront` and refuses
    /// `Open`.
    #[tokio::test]
    async fn a_failed_open_says_so_and_leaves_the_repl_alone() {
        use aigentic_api::{Body, Frame, ProjectInfo, Response, Welcome, decode, encode};
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixListener;

        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("f.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let old = Ulid::from_parts(1, 1);
        let new = Ulid::from_parts(1, 2);
        let info = |id: Ulid| aigentic_api::ThreadInfo {
            id,
            project: Some("p".into()),
            date: "2026-10-05".into(),
            events: 0,
            first_line: String::new(),
            state: ThreadState::Idle,
            title: None,
            workspace: None,
            kind: aigentic_api::ThreadKind::Thread,
        };
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut lines = BufReader::new(read).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(frame) = decode(&line) else { break };
                let id = frame.id.unwrap_or(0);
                let Body::Request(request) = frame.body else {
                    break;
                };
                let response = match request {
                    Request::Hello { .. } => Response::Welcome(Welcome {
                        user: "steve".into(),
                        projects: vec![ProjectInfo {
                            name: "p".into(),
                            root: dir.path().join("p"),
                            role: Some("admin".into()),
                            threads: 0,
                        }],
                        server: "scripted".into(),
                    }),
                    Request::NewFront { .. } => Response::Thread { thread: info(new) },
                    Request::Open { .. } => Response::Refused {
                        reason: "the thread is gone".into(),
                    },
                    other => panic!("unscripted: {other:?}"),
                };
                let frame = Frame::response(id, response);
                write
                    .write_all(format!("{}\n", encode(&frame)).as_bytes())
                    .await
                    .unwrap();
            }
        });
        let addr = Addr::Unix(socket);
        let (client, _) = Client::connect(&addr, "t").await.unwrap();
        let mut repl = ClientRepl::new(
            client,
            old,
            "steve",
            Some("admin".into()),
            ThreadState::Idle,
            "manual".into(),
            Identity::default(),
            "p",
        );
        let mut out = Recording::default();
        repl.handle_line("/new", &mut out).await;
        assert_eq!(repl.thread, old, "the REPL stays on the old thread");
        assert_eq!(out.changed, 0, "nothing was told the thread changed");
        let printed = out.lines.last().unwrap();
        assert!(
            printed.starts_with(&format!("cannot open the new thread {new}: ")),
            "{printed}"
        );
        assert!(printed.ends_with("the thread is gone"), "{printed}");
        repl.render(
            Notice::Note {
                thread: old,
                text: "still here".into(),
            },
            &mut out,
        );
        assert_eq!(out.lines.last().unwrap(), "[still here]");
    }

    /// A key by its code, for the menu and the keymap.
    fn key_for(c: char) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char(c),
            crossterm::event::KeyModifiers::NONE,
        )
    }

    fn key_for_enter() -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        )
    }

    fn key_for_esc() -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Esc,
            crossterm::event::KeyModifiers::NONE,
        )
    }

    fn key_for_ctrl_c() -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('c'),
            crossterm::event::KeyModifiers::CONTROL,
        )
    }

    /// The `/new` question's rig (issue #108): an embedded daemon whose
    /// first turn stops at a bash call's approval and stays there until
    /// someone answers it — no timer holds it, the gate does — a REPL on
    /// the chat thread, and a second client to pace that turn.
    struct HeldTurn {
        dir: tempfile::TempDir,
        embedded: Embedded,
        repl: ClientRepl,
        notices: mpsc::Receiver<Notice>,
        pacer: Client,
        paced: mpsc::Receiver<Notice>,
        out: Copies,
    }

    impl HeldTurn {
        async fn start() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = project(dir.path(), "proj", "");
            let cfg_dir = dir.path().join("cfg");
            std::fs::create_dir_all(&cfg_dir).unwrap();
            // The first batch asks to run a command and then waits on the
            // approval; the second is whatever follows it, so a test that
            // lets the turn finish sees it end.
            let script = vec![
                vec![
                    call("b1", "bash", serde_json::json!({"command": "rm -rf x"})),
                    tool_use(),
                ],
                vec![text("fine, not deleting"), done()],
            ];
            let embedded = Server::embed_with(
                config(dir.path()),
                cfg_dir.clone(),
                root,
                "steve",
                None,
                Factory::scripted(script),
                Arc::new(DefaultReports {
                    global_instructions: cfg_dir.join("instructions.md"),
                }),
            )
            .await
            .unwrap();
            let addr = Addr::Unix(embedded.socket.clone());
            let (client, welcome) = Client::connect(&addr, &embedded.token).await.unwrap();
            let role = welcome.projects[0].role.clone();
            let (thread, state, mode) = open(&client, "proj", None).await;
            let notices = client.take_notices().unwrap();
            let repl = ClientRepl::new(
                client,
                thread,
                "steve",
                role,
                state,
                mode,
                Identity::default(),
                "proj",
            );
            let (pacer, _) = Client::connect(&addr, &embedded.token).await.unwrap();
            open(&pacer, "proj", Some(thread)).await;
            let paced = pacer.take_notices().unwrap();
            Self {
                dir,
                embedded,
                repl,
                notices,
                pacer,
                paced,
                out: Copies::default(),
            }
        }

        /// The lines the REPL printed.
        fn lines(&self) -> Vec<String> {
            self.out.0.0.clone()
        }

        /// Post a message and wait for the daemon to report the turn
        /// stopped at the bash call's approval: the turn is open, and
        /// nothing moves it until someone answers the gate.
        async fn post_and_hold(&mut self) {
            self.repl.post("hello", false, &mut self.out).await;
            until_state(
                &mut self.paced,
                |s| matches!(s, ThreadState::AwaitingApproval { call_id, .. } if call_id == "b1"),
            )
            .await;
        }

        /// The raw text of a thread's log.
        fn log_text(&self, thread: Ulid) -> String {
            let path = self
                .dir
                .path()
                .join("threads")
                .join(format!("{thread}.jsonl"));
            std::fs::read_to_string(path).unwrap_or_default()
        }

        /// The kinds in a thread's log, read from the file: a line is
        /// kept only when it parses, so a poll may catch the daemon
        /// mid-append.
        fn log_kinds(&self, thread: Ulid) -> Vec<String> {
            self.log_text(thread)
                .lines()
                .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                .filter_map(|v| v.get("kind").and_then(|k| k.as_str()).map(str::to_owned))
                .collect()
        }

        /// Wait for a kind to appear in a thread's log.
        async fn until_log_kind(&self, thread: Ulid, kind: &str, what: &str) {
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
            while !self.log_kinds(thread).iter().any(|k| k == kind) {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "{what}: no {kind} in the log of {thread}: {:?}",
                    self.log_kinds(thread)
                );
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        }

        /// Whether the daemon really has that thread, on a connection of
        /// its own.
        async fn thread_exists(&self, thread: Ulid) -> bool {
            let addr = Addr::Unix(self.embedded.socket.clone());
            let (client, _) = Client::connect(&addr, &self.embedded.token).await.unwrap();
            matches!(
                client
                    .request(Request::Open {
                        thread,
                        from_seq: 0
                    })
                    .await,
                Ok(Response::Opened { .. })
            )
        }

        /// Draw this REPL's notices until it has seen the turn end, as
        /// the shell does.
        async fn draw_until_idle(&mut self) {
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                let n = tokio::time::timeout_at(deadline, self.notices.recv())
                    .await
                    .expect("a state notice in time")
                    .expect("a notice");
                let idle = matches!(&n, Notice::State { state, .. } if *state == ThreadState::Idle);
                self.repl.render(n, &mut self.out);
                if idle {
                    return;
                }
            }
        }

        /// Draw this REPL's notices until the daemon's prompt is up, as
        /// the shell does: the approval is the menu the REPL keys now.
        async fn draw_the_prompt(&mut self) {
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
            while self.repl.menu().is_none() {
                let n = tokio::time::timeout_at(deadline, self.notices.recv())
                    .await
                    .expect("a prompt in time")
                    .expect("a notice");
                self.repl.render(n, &mut self.out);
            }
        }
    }

    /// The question as a pipe prints it.
    fn new_question() -> Vec<String> {
        Menu::new_thread().plain()
    }

    /// The lines the question drew, asserted present in `lines`.
    fn assert_question_drawn(lines: &[String]) {
        let question = new_question();
        assert!(
            question.iter().all(|l| lines.contains(l)),
            "the question is drawn: {question:?} in {lines:#?}"
        );
    }

    /// T2 (#108): idle `/new` is untouched, and with a turn running a
    /// `No` keeps the thread and the turn.
    #[tokio::test]
    async fn new_with_a_running_turn_asks_and_no_keeps_the_turn() {
        let mut rig = HeldTurn::start().await;
        rig.post_and_hold().await;
        let old = rig.repl.thread;

        rig.repl.handle_line("/new", &mut rig.out).await;
        assert_eq!(
            rig.repl.menu(),
            Some(&Menu::new_thread()),
            "the question is up"
        );
        assert_eq!(rig.repl.menu_id().as_deref(), Some("confirm:new"));
        assert_question_drawn(&rig.lines());
        assert_eq!(rig.repl.thread, old, "`/new` itself starts nothing");

        rig.repl.handle_line("n", &mut rig.out).await;
        let lines = rig.lines();
        assert!(
            lines
                .iter()
                .any(|l| l == "kept this thread; nothing changed"),
            "{lines:#?}"
        );
        assert_eq!(rig.repl.thread, old, "the REPL stayed on the thread");
        assert_eq!(rig.repl.menu(), None, "and the question is gone");
        let kinds = rig.log_kinds(old);
        assert!(
            !kinds
                .iter()
                .any(|k| k == "interrupted" || k == "turn_ended"),
            "the turn is still open: {kinds:?}"
        );
    }

    /// T2 (#108): a `Yes` interrupts the running turn, starts the new
    /// front thread and says what it sent.
    #[tokio::test]
    async fn new_with_a_running_turn_asks_and_yes_interrupts_and_starts_a_new_thread() {
        let mut rig = HeldTurn::start().await;
        rig.post_and_hold().await;
        let old = rig.repl.thread;

        rig.repl.handle_line("/new", &mut rig.out).await;
        assert_eq!(rig.repl.menu(), Some(&Menu::new_thread()));
        rig.repl.handle_line("y", &mut rig.out).await;

        let new = rig.repl.thread;
        assert_ne!(new, old, "the REPL moved to a new front thread");
        assert!(rig.thread_exists(new).await, "the daemon has {new}");
        rig.until_log_kind(old, "interrupted", "the interrupt reached the turn")
            .await;
        rig.until_log_kind(old, "turn_ended", "and the turn ended")
            .await;
        let lines = rig.lines();
        let sent = format!(
            "sent an interrupt to {old}; new front thread {new} · the old one stays listed in /threads"
        );
        assert!(lines.contains(&sent), "{lines:#?}");
        assert!(
            lines.iter().any(|l| l == "[interrupting]"),
            "the turn's own line went out first: {lines:#?}"
        );
    }

    /// T2 (#108): `/new` typed right after a message, before the daemon
    /// has reported the turn, still asks — `awaiting_turn` covers the
    /// window.
    #[tokio::test]
    async fn new_typed_after_a_message_asks_before_the_state_notice() {
        let mut rig = HeldTurn::start().await;
        let old = rig.repl.thread;
        rig.repl.post("hello", false, &mut rig.out).await;
        // The turn is running on the daemon and this REPL has not drawn
        // a notice since: only the post it just made says so.
        assert_eq!(rig.repl.state, ThreadState::Idle, "no notice was drawn");
        assert!(rig.repl.turn_open(), "but the turn this REPL started is");

        rig.repl.handle_line("/new", &mut rig.out).await;
        assert_eq!(
            rig.repl.menu(),
            Some(&Menu::new_thread()),
            "it asks before the notice: {:#?}",
            rig.lines()
        );
        rig.repl.handle_line("n", &mut rig.out).await;
        assert_eq!(rig.repl.thread, old);
    }

    /// T2 (#108): the turn ends while the question is up. The question
    /// stays, nothing is interrupted, and `Yes` starts the new thread.
    #[tokio::test]
    async fn new_while_the_question_is_up_after_the_turn_ended_starts_it_without_an_interrupt() {
        let mut rig = HeldTurn::start().await;
        rig.post_and_hold().await;
        let old = rig.repl.thread;
        rig.repl.handle_line("/new", &mut rig.out).await;
        assert_eq!(rig.repl.menu(), Some(&Menu::new_thread()));

        // The gate is answered on the pacer: the turn runs on and ends
        // behind the question.
        let r = rig
            .pacer
            .request(Request::Decide {
                thread: old,
                call_id: "b1".into(),
                allow: true,
                session: false,
                prefix: None,
                reason: None,
            })
            .await
            .unwrap();
        assert!(matches!(r, Response::Ok), "{r:?}");
        until_state(&mut rig.paced, |s| *s == ThreadState::Idle).await;
        rig.draw_until_idle().await;
        assert_eq!(rig.repl.state, ThreadState::Idle);
        assert_eq!(
            rig.repl.menu(),
            Some(&Menu::new_thread()),
            "the question stays: {:#?}",
            rig.lines()
        );

        rig.repl.handle_line("y", &mut rig.out).await;
        let new = rig.repl.thread;
        assert_ne!(new, old, "the REPL moved to a new front thread");
        rig.until_log_kind(old, "turn_ended", "the turn ended on its own")
            .await;
        let kinds = rig.log_kinds(old);
        assert!(
            !kinds.iter().any(|k| k == "interrupted"),
            "nothing was interrupted: {kinds:?}"
        );
        let lines = rig.lines();
        let sent = format!("new front thread {new} · the old one stays listed in /threads");
        assert!(lines.contains(&sent), "{lines:#?}");
    }

    /// T2 (#108): a second `/new` while the question is up is ignored,
    /// and the answer still works.
    #[tokio::test]
    async fn a_second_new_while_the_question_is_up_is_ignored() {
        let mut rig = HeldTurn::start().await;
        rig.post_and_hold().await;
        let old = rig.repl.thread;
        rig.repl.handle_line("/new", &mut rig.out).await;
        let question = rig.repl.menu().cloned();
        let drawn = rig.lines();

        rig.repl.handle_line("/new", &mut rig.out).await;
        assert_eq!(
            rig.repl.menu().cloned(),
            question,
            "the question is the same one"
        );
        assert_eq!(rig.lines(), drawn, "and it was not drawn again");
        assert_eq!(rig.repl.thread, old, "no new thread started");

        rig.repl.handle_line("n", &mut rig.out).await;
        assert!(
            rig.lines()
                .iter()
                .any(|l| l == "kept this thread; nothing changed"),
            "the answer still lands: {:#?}",
            rig.lines()
        );
    }

    /// T3 (#108): with a followed run's checkpoint also up, the question
    /// is the one `menu()` and `menu_id()` name and the one that takes
    /// its keys; a `No` hands the checkpoint back, still answerable.
    #[tokio::test]
    async fn the_question_takes_its_keys_before_a_checkpoint_and_gives_it_back() {
        let lead_id = Ulid::generate();
        let backlog = vec![run_event(
            lead_id,
            1,
            EventKind::CheckpointAsked,
            checkpoint_asked("gate-1"),
        )];
        let mut lead = Lead::start(lead_id, backlog).await;
        lead.line("/build 58").await;
        lead.pump().await;
        let checkpoint = lead.repl.menu().cloned().expect("the checkpoint is up");

        // The chat thread is running, as a notice says: `/new` asks.
        lead.push(Notice::State {
            thread: lead.repl.thread,
            state: ThreadState::Running {
                by: Author::User(aigentic_runtime::aigentic_core::UserId("steve".into())),
                queued: 0,
            },
        });
        lead.pump().await;
        lead.line("/new").await;
        assert_eq!(
            lead.repl.menu(),
            Some(&Menu::new_thread()),
            "the question is drawn"
        );
        assert_eq!(lead.repl.menu_id().as_deref(), Some("confirm:new"));

        // `n` is the checkpoint's own key too; it answers the question.
        let keyed = lead
            .repl
            .menu_key(&key_for('n'), true, true, &mut lead.out)
            .await;
        assert!(matches!(keyed, MenuKey::Used), "{keyed:?}");
        assert!(
            lead.lines()
                .iter()
                .any(|l| l == "kept this thread; nothing changed"),
            "{:#?}",
            lead.lines()
        );
        assert!(
            !lead
                .daemon
                .requests()
                .iter()
                .any(|r| matches!(r, Request::AnswerCheckpoint { .. })),
            "the checkpoint was not answered: {:?}",
            lead.daemon.requests()
        );

        // The checkpoint is the menu again, and still answerable.
        assert_eq!(
            lead.repl.menu().cloned(),
            Some(checkpoint),
            "the checkpoint is drawn again"
        );
        let before = lead.daemon.requests().len();
        let keyed = lead
            .repl
            .menu_key(&key_for('2'), true, true, &mut lead.out)
            .await;
        assert!(matches!(keyed, MenuKey::Used), "{keyed:?}");
        match &lead.daemon.requests()[before..] {
            [Request::AnswerCheckpoint { gate, .. }] => assert_eq!(gate, "gate-1"),
            other => panic!("the gate's own key answers it: {other:?}"),
        }
        lead.daemon.stop();
    }

    /// T3 (#108): a typed answer in plain mode answers the question, not
    /// a followed run's checkpoint, and the question prints.
    #[tokio::test]
    async fn a_typed_answer_answers_the_question_and_not_a_checkpoint() {
        let lead_id = Ulid::generate();
        let backlog = vec![run_event(
            lead_id,
            1,
            EventKind::CheckpointAsked,
            checkpoint_asked("gate-1"),
        )];
        let mut lead = Lead::start(lead_id, backlog).await;
        lead.line("/build 58").await;
        lead.pump().await;
        let checkpoint = lead.repl.menu().cloned().expect("the checkpoint is up");

        lead.push(Notice::State {
            thread: lead.repl.thread,
            state: ThreadState::Running {
                by: Author::User(aigentic_runtime::aigentic_core::UserId("steve".into())),
                queued: 0,
            },
        });
        lead.pump().await;
        lead.line("/new").await;
        assert_question_drawn(&lead.lines());
        assert_eq!(lead.repl.menu(), Some(&Menu::new_thread()));

        // A typed line, as a pipe hands it over: the question first.
        lead.line("n").await;
        assert!(
            lead.lines()
                .iter()
                .any(|l| l == "kept this thread; nothing changed"),
            "{:#?}",
            lead.lines()
        );
        assert_eq!(
            lead.repl.menu().cloned(),
            Some(checkpoint),
            "the checkpoint is untouched, and up"
        );
        assert!(
            !lead
                .daemon
                .requests()
                .iter()
                .any(|r| matches!(r, Request::AnswerCheckpoint { .. })),
            "and nothing was answered for it: {:?}",
            lead.daemon.requests()
        );
        lead.daemon.stop();
    }

    /// T3 (#108): a daemon approval that arrives while the question is
    /// up waits behind it — the question keeps the keys, and a `No`
    /// hands the approval back, still answerable.
    #[tokio::test]
    async fn the_question_keeps_its_keys_while_an_approval_waits_behind_it() {
        let mut rig = HeldTurn::start().await;
        rig.post_and_hold().await;
        rig.draw_the_prompt().await;
        let old = rig.repl.thread;
        let approval = rig.repl.menu().cloned().expect("the approval is up");
        assert_eq!(rig.repl.menu_id().as_deref(), Some("chat:b1"));

        rig.repl.handle_line("/new", &mut rig.out).await;
        assert_eq!(
            rig.repl.menu(),
            Some(&Menu::new_thread()),
            "the question is the one shown: {:#?}",
            rig.lines()
        );
        assert_eq!(rig.repl.menu_id().as_deref(), Some("confirm:new"));

        // `n` answers the question, never the approval behind it.
        let keyed = rig
            .repl
            .menu_key(&key_for('n'), true, true, &mut rig.out)
            .await;
        assert!(matches!(keyed, MenuKey::Used), "{keyed:?}");
        assert_eq!(
            rig.repl.menu().cloned(),
            Some(approval),
            "the approval is drawn again"
        );
        assert_eq!(
            rig.repl.menu_id().as_deref(),
            Some("chat:b1"),
            "and it is the one keyed"
        );

        // Still answerable: allow it once, and the turn runs on to its
        // own end, with nothing denied along the way.
        let keyed = rig
            .repl
            .menu_key(&key_for('1'), true, true, &mut rig.out)
            .await;
        assert!(matches!(keyed, MenuKey::Used), "{keyed:?}");
        rig.until_log_kind(old, "turn_ended", "the turn ran to its end")
            .await;
        let log = rig.log_text(old);
        assert!(
            !log.contains("denied by"),
            "the question's `n` denied nothing: {log}"
        );
        assert_eq!(rig.repl.thread, old, "and no thread moved");
    }

    /// T3 (#108): in plain mode (the rig's printer is a pipe: it prints a
    /// prompt's `plain()` text) the question prints, and a typed `n`
    /// answers the question rather than a followed run's checkpoint.
    #[tokio::test]
    async fn a_piped_new_with_a_turn_running_asks_and_a_typed_n_answers_it() {
        let mut rig = HeldTurn::start().await;
        rig.post_and_hold().await;
        rig.draw_the_prompt().await;
        let old = rig.repl.thread;

        rig.repl.handle_line("/new", &mut rig.out).await;
        let printed = rig.lines();
        let question = Menu::new_thread().plain();
        assert!(
            printed.ends_with(&question),
            "the question's own lines end the output: {printed:#?}"
        );
        assert_eq!(rig.repl.menu_id().as_deref(), Some("confirm:new"));

        // A typed line, as a pipe sends one.
        rig.repl.handle_line("n", &mut rig.out).await;
        let lines = rig.lines();
        assert!(
            lines.contains(&"kept this thread; nothing changed".to_owned()),
            "{lines:#?}"
        );
        assert_eq!(rig.repl.thread, old, "no thread moved");
        assert!(
            rig.repl.menu_id().as_deref() == Some("chat:b1"),
            "the checkpoint is still the one keyed"
        );
    }

    // ---- the retry after a cut (issue #114) ---------------------------

    /// T7 (#114): a reply cut off mid-stream is asked again and the shown
    /// text is said to be dropped: the flushed partial, then the dim note,
    /// then the retry's reply.
    #[tokio::test]
    async fn a_cut_after_streamed_text_draws_the_note_between_the_text_and_the_answer() {
        let dir = tempfile::tempdir().unwrap();
        let root = project(dir.path(), "p", "");
        let script = vec![
            vec![text("half a reply"), cut()],
            vec![text("the real reply"), done()],
        ];
        let (embedded, lines) = run_a_scripted_repl(dir.path(), root, script, "hello").await;
        drop(embedded);

        let index = |needle: &str| {
            lines
                .iter()
                .position(|l| l.contains(needle))
                .unwrap_or_else(|| panic!("no line with {needle:?}: {lines:#?}"))
        };
        let text = index("half a reply");
        let note = index("cut off mid-stream");
        let answer = index("the real reply");
        assert!(
            lines[note].contains("asking again") && lines[note].contains("discarded"),
            "the note says what happened: {:?}",
            lines[note]
        );
        assert!(
            text < note && note < answer,
            "the note comes after the text it retracts and before the answer: {lines:#?}"
        );
    }

    /// T7 (#114): `/copy` drops exactly the attempt the retry discarded
    /// and keeps the turn's earlier, kept reply — the truncation back to
    /// where the cut call's stream began, not `clear()`.
    #[tokio::test]
    async fn copy_drops_the_attempt_a_retry_discarded_and_keeps_the_kept_reply() {
        let dir = tempfile::tempdir().unwrap();
        let root = project(dir.path(), "p", "");
        let cfg_dir = dir.path().join("cfg");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let script = vec![
            vec![
                text("kept reply"),
                call("c1", "update_tasks", serde_json::json!({"tasks": []})),
                tool_use(),
            ],
            vec![text("discarded reply"), cut()],
            vec![text("the real reply"), done()],
        ];
        let embedded = Server::embed_with(
            config(dir.path()),
            cfg_dir.clone(),
            root,
            "steve",
            None,
            Factory::scripted(script),
            Arc::new(DefaultReports {
                global_instructions: cfg_dir.join("instructions.md"),
            }),
        )
        .await
        .unwrap();
        let addr = Addr::Unix(embedded.socket.clone());
        let (client, welcome) = Client::connect(&addr, &embedded.token).await.unwrap();
        let role = welcome.projects[0].role.clone();
        let (thread, state, mode) = open(&client, "p", None).await;
        let notices = client.take_notices().unwrap();
        let mut repl = ClientRepl::new(
            client,
            thread,
            "steve",
            role,
            state,
            mode,
            Identity::default(),
            "p",
        );
        let (pacer, _) = Client::connect(&addr, &embedded.token).await.unwrap();
        open(&pacer, "p", Some(thread)).await;
        let mut paced = pacer.take_notices().unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        let feeder = async move {
            tx.send("hello".into()).unwrap();
            // The turn has three model calls: let it finish them all.
            until_state(&mut paced, |s| *s == ThreadState::Idle).await;
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            tx.send("/copy all".into()).unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            tx.send("/quit".into()).unwrap();
        };
        let mut caps = Copies::default();
        let ((), ()) = tokio::join!(repl.run(rx, notices, &mut caps), feeder);
        let copied = caps
            .1
            .last()
            .unwrap_or_else(|| panic!("nothing was copied: {:#?}", caps.0.0))
            .clone();
        assert!(
            copied.contains("kept reply") && copied.contains("the real reply"),
            "the kept calls are still copyable: {copied:?}"
        );
        assert!(
            !copied.contains("discarded reply"),
            "the discarded attempt is gone: {copied:?}"
        );
    }
    // ---- the developer view (issue #115, T4/T6/T7) -------------------

    /// A printer that collects lines like `Lines` and leaves `view()`
    /// to the `Printer` trait's default — the guard that the default
    /// stays `Normal` (issue #115).
    #[derive(Default)]
    struct DefaultView(pub Vec<String>);

    impl Printer for DefaultView {
        fn line(&mut self, text: &str) {
            self.0.push(text.to_owned());
        }
    }

    /// Drive one scripted turn into `out` and hand the printer back.
    async fn run_turn<P: Printer>(script: Vec<Vec<ProviderEvent>>, mut out: P) -> P {
        let dir = tempfile::tempdir().unwrap();
        let root = project(dir.path(), "proj", "");
        let cfg_dir = dir.path().join("cfg");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let embedded = Server::embed_with(
            config(dir.path()),
            cfg_dir.clone(),
            root,
            "steve",
            None,
            Factory::scripted(script),
            Arc::new(DefaultReports {
                global_instructions: cfg_dir.join("instructions.md"),
            }),
        )
        .await
        .unwrap();
        let (client, welcome) =
            Client::connect(&Addr::Unix(embedded.socket.clone()), &embedded.token)
                .await
                .unwrap();
        let role = welcome.projects[0].role.clone();
        let (thread, state, mode) = open(&client, "proj", None).await;
        let notices = client.take_notices().unwrap();
        let mut repl = ClientRepl::new(
            client,
            thread,
            "steve",
            role,
            state,
            mode,
            Identity::default(),
            "proj",
        );
        let (tx, rx) = mpsc::unbounded_channel();
        let feeder = async move {
            tx.send("go".into()).unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
            tx.send("/quit".into()).unwrap();
        };
        let ((), ()) = tokio::join!(repl.run(rx, notices, &mut out), feeder);
        drop(embedded);
        out
    }

    /// One scripted turn's lines through a `Lines`. `detail` picks the
    /// developer view's `Lines` (T6) or the default (T7), the one every
    /// existing test uses.
    async fn dev_lines(script: Vec<Vec<ProviderEvent>>, detail: bool) -> Vec<String> {
        let out = if detail {
            Lines::detail_on()
        } else {
            Lines::default()
        };
        run_turn(script, out).await.0
    }

    /// A `update_tasks` argument value with the given `(text, state)`
    /// rows.
    fn tasks(rows: &[(&str, &str)]) -> serde_json::Value {
        serde_json::json!({
            "tasks": rows
                .iter()
                .map(|(text, state)| serde_json::json!({ "text": text, "state": state }))
                .collect::<Vec<_>>()
        })
    }

    fn usage(input: u64, cache_read: u64, output: u64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: cache_read,
            cache_write_tokens: 0,
            reasoning_tokens: None,
        }
    }

    /// T6 (issue #115): through the engine, with a `Lines` built with
    /// detail on, the turn reads in log order — the reply text, the call
    /// lines, the tool cells and the closing summary — and the call
    /// lines carry the scripted usage's figures. Through a default
    /// `Lines` the same turn is HEAD's rows: no `Call`, `Step` or
    /// `System` line, no blank-row additions.
    #[tokio::test]
    async fn the_developer_view_shows_each_call_in_log_order_and_normal_shows_none() {
        let script = vec![
            vec![
                ProviderEvent::Usage(usage(1_000, 3_000, 200)),
                text("first\n"),
                call("c1", "bash", serde_json::json!({ "command": "echo hi" })),
                tool_use(),
            ],
            vec![
                ProviderEvent::Usage(usage(2_000, 600, 300)),
                text("second\n"),
                done(),
            ],
        ];
        let dev = dev_lines(script.clone(), true).await;
        let call1 = dev
            .iter()
            .position(|l| l.contains("◦ call 1"))
            .unwrap_or_else(|| panic!("no call 1 line: {dev:#?}"));
        let call2 = dev
            .iter()
            .position(|l| l.contains("◦ call 2"))
            .unwrap_or_else(|| panic!("no call 2 line: {dev:#?}"));
        let first = dev.iter().position(|l| l.contains("first")).unwrap();
        let second = dev.iter().position(|l| l.contains("second")).unwrap();
        let tool = dev
            .iter()
            .position(|l| l.contains("bash") && l.contains("echo hi"))
            .unwrap_or_else(|| panic!("no tool cell: {dev:#?}"));
        assert!(
            first < call1,
            "the reply text comes before call 1: {dev:#?}"
        );
        assert!(call1 < tool, "call 1 comes before its tool cell: {dev:#?}");
        assert!(
            tool < second,
            "the tool cell comes before the closing text: {dev:#?}"
        );
        assert!(
            second < call2,
            "a reply's text comes before its own call line: {dev:#?}"
        );
        // The figures are the scripted usage's: the numbers the log holds.
        let line1 = &dev[call1];
        assert!(
            line1.contains("4.0k in") && line1.contains("75% cached") && line1.contains("200 out"),
            "call 1 reads the first usage: {line1:?}"
        );
        let line2 = &dev[call2];
        assert!(
            line2.contains("300 out"),
            "call 2 reads the second usage: {line2:?}"
        );

        let normal = dev_lines(script, false).await;
        for l in &normal {
            assert!(
                !l.contains("◦ call") && !l.contains('▸') && !l.contains("retry "),
                "a default Lines sees no developer cell: {l:?}"
            );
        }
        assert!(
            !normal
                .windows(2)
                .any(|w| w[0].is_empty() && w[1].is_empty()),
            "no blank-row additions: {normal:#?}"
        );
    }

    /// The trait's default `view()` is what a plain printer gets:
    /// `Stdout` (the plain REPL) and `Copies` both leave it alone
    /// (issue #115). Pin it on a printer that overrides nothing, through
    /// the same two-call turn as the dev-view test: no `Call`, `Step` or
    /// `System` cell.
    #[tokio::test]
    async fn the_default_view_keeps_a_plain_printer_on_the_normal_view() {
        assert_eq!(
            Copies::default().view(),
            View::Normal,
            "a printer with no view() of its own must print the normal view"
        );
        let script = vec![
            vec![
                ProviderEvent::Usage(usage(1_000, 3_000, 200)),
                text("first\n"),
                call("c1", "bash", serde_json::json!({ "command": "echo hi" })),
                tool_use(),
            ],
            vec![
                ProviderEvent::Usage(usage(2_000, 600, 300)),
                text("second\n"),
                done(),
            ],
        ];
        let lines = run_turn(script, DefaultView::default()).await.0;
        for l in &lines {
            assert!(
                !l.contains("◦ call") && !l.contains('▸') && !l.starts_with("· "),
                "the default view draws no developer cell: {l:?}"
            );
        }
        assert!(
            !lines.windows(2).any(|w| w[0].is_empty() && w[1].is_empty()),
            "no blank-row additions: {lines:#?}"
        );
    }

    /// T4 (issue #115): a scripted turn whose `update_tasks` moves the
    /// active step twice. Under `Dev` every change draws a header with
    /// its calls indented, and there is no blank row between a header
    /// and its first call; re-emitting the identical list and
    /// renumbering alone draw none. Under `Normal` there is no header
    /// and no indent.
    #[tokio::test]
    async fn a_moved_step_draws_a_header_with_its_calls_indented() {
        let script = vec![
            vec![
                text("planning\n"),
                call(
                    "u1",
                    "update_tasks",
                    tasks(&[("one", "active"), ("two", "pending"), ("three", "pending")]),
                ),
                tool_use(),
            ],
            vec![
                text("reading\n"),
                call("b1", "bash", serde_json::json!({ "command": "echo a" })),
                tool_use(),
            ],
            vec![
                text("moving on\n"),
                call(
                    "u2",
                    "update_tasks",
                    tasks(&[("one", "done"), ("two", "active"), ("three", "pending")]),
                ),
                tool_use(),
            ],
            // The identical list again: no new header.
            vec![
                text("again\n"),
                call(
                    "u3",
                    "update_tasks",
                    tasks(&[("one", "done"), ("two", "active"), ("three", "pending")]),
                ),
                tool_use(),
            ],
            // Renumbered (a fourth row), same active text: still none.
            vec![
                text("renumbered\n"),
                call(
                    "u4",
                    "update_tasks",
                    tasks(&[
                        ("one", "done"),
                        ("two", "active"),
                        ("three", "pending"),
                        ("four", "pending"),
                    ]),
                ),
                tool_use(),
            ],
            vec![text("done\n"), done()],
        ];
        let dev = dev_lines(script.clone(), true).await;
        let headers: Vec<&String> = dev.iter().filter(|l| l.contains('▸')).collect();
        assert_eq!(headers.len(), 2, "one header per change, no more: {dev:#?}");
        assert!(
            headers[0].contains("1/3") && headers[0].contains("one"),
            "the first header names step 1/3: {headers:?}"
        );
        assert!(
            headers[1].contains("2/3") && headers[1].contains("two"),
            "the second header names step 2/3: {headers:?}"
        );
        // A header's very next row is never blank (a call indents under it
        // rather than opening a new block; the indent itself is the
        // renderer's, covered by the look.rs step tests).
        let h = dev.iter().position(|l| l.contains('▸')).unwrap();
        assert!(
            !dev[h + 1].is_empty(),
            "no blank row between a header and the row under it: {dev:#?}"
        );
        assert!(
            !headers[0].starts_with(' '),
            "a header itself is not indented: {:?}",
            headers[0]
        );

        let normal = dev_lines(script, false).await;
        assert!(
            !normal.iter().any(|l| l.contains('▸')),
            "Normal draws no header: {normal:#?}"
        );
    }

    /// T7 (issue #115): a turn through a default `Lines` — HEAD's printer
    /// — carries no developer row and no extra blank row. The cell-level
    /// half, that `Normal` of a cell with detail equals `Normal` of the
    /// same cell with none, is pinned by
    /// `the_developer_view_adds_the_tool_detail_and_normal_is_the_bare_cell`
    /// in `look.rs`; here the whole turn is checked.
    #[tokio::test]
    async fn a_normal_turn_adds_no_developer_rows_and_no_extra_blank_rows() {
        let script = vec![
            vec![
                text("here you go\n"),
                call("c1", "bash", serde_json::json!({ "command": "echo hi" })),
                tool_use(),
            ],
            vec![text("all done\n"), done()],
        ];
        let rows = dev_lines(script, false).await;
        for l in &rows {
            assert!(
                !l.contains("◦ call") && !l.contains('▸') && !l.contains("retry "),
                "Normal prints no developer row: {l:?}"
            );
        }
        assert!(
            !rows.windows(2).any(|w| w[0].is_empty() && w[1].is_empty()),
            "Normal adds no blank rows: {rows:#?}"
        );
    }
    // ---- the developer view's pure parts (issue #115, T3/T5) ---------

    mod dev_view {
        use super::*;
        use aigentic_runtime::aigentic_core::{Author, UserId};
        use aigentic_runtime::aigentic_log::{
            ContextEvictedPayload, DecisionAnsweredPayload, DecisionProposedPayload,
            MemoryExtractedPayload, MemoryHome, MemoryLine, MemoryRememberedPayload, PolicyRecord,
            ProviderRetriedPayload, ResultsStubbedPayload, Usage,
        };
        use time::OffsetDateTime;
        use time::macros::datetime;

        fn at(seconds: i64) -> OffsetDateTime {
            datetime!(2026-10-08 12:00:00 UTC) + time::Duration::seconds(seconds)
        }

        fn ulid(n: u64) -> Ulid {
            Ulid::from_parts(n, 1)
        }

        fn event(kind: EventKind, seq: u64, payload: serde_json::Value) -> Event {
            Event {
                id: ulid(seq + 10),
                thread_id: ulid(1),
                seq,
                kind,
                author: Author::System,
                payload,
                parent_event: None,
                created_at: at(seq as i64),
            }
        }

        fn log_usage(cost: Option<f64>, model: &str) -> Usage {
            Usage {
                input_tokens: 1_000,
                output_tokens: 200,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                reasoning_tokens: None,
                estimated: false,
                profile: None,
                model: if model.is_empty() {
                    None
                } else {
                    Some(model.to_owned())
                },
                effort: None,
                latency_ms: None,
                ttft_ms: None,
                cost_usd: cost,
            }
        }

        /// T3 (issue #115): the policy words, one per shape the log holds,
        /// and `None` prints nothing.
        #[test]
        fn a_tool_detail_words_each_policy_shape() {
            let cases: &[(Option<PolicyRecord>, &str)] = &[
                (
                    Some(PolicyRecord::Rule {
                        rule: "read_only".into(),
                        decision: "allow".into(),
                        reason: None,
                    }),
                    "rule read_only",
                ),
                (
                    Some(PolicyRecord::Rule {
                        rule: "no_shell".into(),
                        decision: "deny".into(),
                        reason: None,
                    }),
                    "denied: rule no_shell",
                ),
                (
                    Some(PolicyRecord::Human {
                        event: ulid(7),
                        allow: true,
                    }),
                    "you allowed",
                ),
                (
                    Some(PolicyRecord::Human {
                        event: ulid(8),
                        allow: false,
                    }),
                    "you denied",
                ),
                (None, ""),
            ];
            let result = event(EventKind::ToolResult, 2, serde_json::json!({}));
            let events = TurnEvents::default();
            for (policy, want) in cases {
                let d = tool_detail(policy.as_ref(), 0, &result, &events);
                if want.is_empty() {
                    assert_eq!(d.policy, None, "an absent policy prints nothing");
                } else {
                    assert_eq!(d.policy.as_deref(), Some(*want));
                }
            }
        }

        /// T3 (issue #115): `took` is the result's `created_at` minus the
        /// `created_at` of the event its `parent_event` names — the
        /// assistant message that carried the call — and `bytes` is the
        /// result content's length. A parallel batch is "done after".
        #[test]
        fn a_tool_detail_reads_took_from_the_parent_and_bytes_from_the_result() {
            let parent = Event {
                id: ulid(1),
                thread_id: ulid(9),
                seq: 1,
                kind: EventKind::AssistantMessage,
                author: Author::System,
                payload: serde_json::json!({}),
                parent_event: None,
                created_at: at(0),
            };
            let mut events = TurnEvents::default();
            events.push(&parent);
            // Two results off the one message, at different times: each
            // reads its own `took`.
            let mut early = event(EventKind::ToolResult, 2, serde_json::json!({}));
            early.parent_event = Some(parent.id);
            early.created_at = at(1);
            let mut late = event(EventKind::ToolResult, 3, serde_json::json!({}));
            late.parent_event = Some(parent.id);
            late.created_at = at(3);
            let a = tool_detail(None, 1_000, &early, &events);
            let b = tool_detail(None, 12_345, &late, &events);
            assert_eq!(a.took, Some(std::time::Duration::from_secs(1)));
            assert_eq!(b.took, Some(std::time::Duration::from_secs(3)));
            assert_eq!(a.bytes, 1_000);
            assert_eq!(b.bytes, 12_345);
            // A result whose parent is not in the buffer has no duration.
            let mut orphan = event(EventKind::ToolResult, 4, serde_json::json!({}));
            orphan.parent_event = Some(ulid(999));
            let o = tool_detail(None, 5, &orphan, &TurnEvents::default());
            assert_eq!(o.took, None);
        }

        /// T5 (issue #115): one fixture event per silent kind gives exactly
        /// its line, and an event with no line stays `None`. The set is
        /// Design 7's, including the empty-`written` and no-price memory
        /// cases; `Pinned`, `PermissionRequested` and the build kinds are
        /// silent.
        #[test]
        fn a_system_line_words_each_silent_kind() {
            let cases: Vec<(EventKind, serde_json::Value, Option<&str>)> = vec![
                (
                    EventKind::ProviderRetried,
                    serde_json::to_value(ProviderRetriedPayload {
                        attempt: 1,
                        retries: 3,
                        reason: "overloaded".into(),
                        wait_ms: 2_000,
                    })
                    .unwrap(),
                    Some("retry 1/3 in 2.0s: overloaded"),
                ),
                (
                    EventKind::ContextEvicted,
                    serde_json::to_value(ContextEvictedPayload {
                        through_seq: 42,
                        ratio: None,
                    })
                    .unwrap(),
                    Some("context evicted through #42"),
                ),
                (
                    EventKind::ContextEvicted,
                    serde_json::to_value(ContextEvictedPayload {
                        through_seq: 42,
                        ratio: Some(0.8),
                    })
                    .unwrap(),
                    Some("context evicted through #42 · 80%"),
                ),
                (
                    EventKind::ResultsStubbed,
                    serde_json::to_value(ResultsStubbedPayload {
                        through_seq: 9,
                        ratio: None,
                    })
                    .unwrap(),
                    Some("old results stubbed through #9"),
                ),
                (
                    EventKind::MemoryExtracted,
                    serde_json::to_value(MemoryExtractedPayload {
                        through_seq: 5,
                        written: Vec::new(),
                        model: "flash".into(),
                        usage: log_usage(None, "flash"),
                    })
                    .unwrap(),
                    Some("memory: nothing written"),
                ),
                (
                    EventKind::MemoryExtracted,
                    serde_json::to_value(MemoryExtractedPayload {
                        through_seq: 5,
                        written: vec![
                            MemoryLine {
                                file: "decisions.md".into(),
                                home: MemoryHome::Project,
                                text: "a".into(),
                                stated_by: Author::User(UserId("steve".into())),
                                at_seq: 2,
                            },
                            MemoryLine {
                                file: "decisions.md".into(),
                                home: MemoryHome::Project,
                                text: "b".into(),
                                stated_by: Author::User(UserId("steve".into())),
                                at_seq: 2,
                            },
                        ],
                        model: "flash".into(),
                        usage: log_usage(Some(0.0141), "flash"),
                    })
                    .unwrap(),
                    Some("memory: 2 lines written · flash · $0.0141"),
                ),
                (
                    EventKind::MemoryRemembered,
                    serde_json::to_value(MemoryRememberedPayload {
                        file: "decisions.md".into(),
                        home: MemoryHome::Project,
                        text: "x".into(),
                        written: true,
                    })
                    .unwrap(),
                    Some("remembered in decisions.md"),
                ),
                (
                    EventKind::MemoryRemembered,
                    serde_json::to_value(MemoryRememberedPayload {
                        file: "decisions.md".into(),
                        home: MemoryHome::Project,
                        text: "x".into(),
                        written: false,
                    })
                    .unwrap(),
                    Some("already known: decisions.md"),
                ),
                (
                    EventKind::DecisionProposed,
                    serde_json::to_value(DecisionProposedPayload {
                        kind: aigentic_runtime::aigentic_log::DecisionKind::Project,
                        proposal: "switch to site".into(),
                        target: None,
                        reason: "it is the front".into(),
                        call_id: None,
                        stage: Default::default(),
                    })
                    .unwrap(),
                    Some("proposed project: switch to site — it is the front"),
                ),
                (
                    EventKind::DecisionAnswered,
                    serde_json::to_value(DecisionAnsweredPayload {
                        answer: aigentic_runtime::aigentic_log::DecisionAnswer::Yes,
                        correction: None,
                        note: None,
                    })
                    .unwrap(),
                    Some("answered yes"),
                ),
                // Silent kinds stay silent.
                (EventKind::Pinned, serde_json::json!({}), None),
                (EventKind::PermissionRequested, serde_json::json!({}), None),
                (EventKind::ThreadStarted, serde_json::json!({}), None),
            ];
            for (i, (kind, payload, want)) in cases.into_iter().enumerate() {
                let e = event(kind, i as u64 + 1, payload);
                assert_eq!(system_line(&e).as_deref(), want, "kind {kind:?} line");
            }
        }

        /// A remembered line names the home it went to, so a person's or a
        /// workspace's line is not read as the project's.
        #[test]
        fn the_tui_report_names_the_home() {
            for (home, remembered, already_known) in [
                (
                    MemoryHome::Project,
                    "remembered in decisions.md",
                    "already known: decisions.md",
                ),
                (
                    MemoryHome::Workspace,
                    "remembered in the workspace's decisions.md",
                    "already known: the workspace's decisions.md",
                ),
                (
                    MemoryHome::Person,
                    "remembered in the person's decisions.md",
                    "already known: the person's decisions.md",
                ),
            ] {
                for (written, want) in [(true, remembered), (false, already_known)] {
                    let payload = serde_json::to_value(MemoryRememberedPayload {
                        file: "decisions.md".into(),
                        home,
                        text: "x".into(),
                        written,
                    })
                    .unwrap();
                    let e = event(EventKind::MemoryRemembered, 1, payload);
                    assert_eq!(system_line(&e).as_deref(), Some(want), "home {home:?}");
                }
            }
        }
    }
}
