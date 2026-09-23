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

use std::collections::{HashMap, VecDeque};

use aigentic_api::client::Client;
use aigentic_api::{Notice, ReportKind, Request, Response, ThreadState};
use aigentic_runtime::aigentic_core::{Author, ContentBlock, EventKind, ToolCall};
use aigentic_runtime::aigentic_log::{
    AssistantMessagePayload, CompactedPayload, CompactionStrategy, DecisionScope,
    InterruptedPayload, MemoryExtractedPayload, PermissionDecidedPayload, SkillLoadedPayload,
    ToolResultPayload, TurnEndedPayload, UserMessagePayload,
};
use aigentic_runtime::{ASKED_HUMAN, INTERRUPTED};
use tokio::sync::mpsc;
use ulid::Ulid;

use crate::app::cells::{Cell, ToolState, summarise_args};
use crate::app::commands::{Command, HELP, parse_line, truncate_for_display};
use crate::app::menu::{Keyed, Kind, Menu, Pick};

/// What a key on the prompt menu came to, from `ClientRepl::menu_key`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuKey {
    /// Not a menu key; the caller carries on.
    Passed,
    /// Handled: the selection moved, or the decision went out.
    Used,
    /// The composer should take the deny's reason; the draft it held
    /// comes back after.
    Reason,
}

/// A running turn's figures, from the provider's reported usage on the
/// events, never counted here (plan step 8b).
#[derive(Debug, Clone)]
pub struct TurnStats {
    pub started: std::time::Instant,
    pub tools: u32,
    /// The last call's prompt: input plus cache reads and writes.
    pub prompt: u64,
    /// Of that, read from the cache.
    pub cached: u64,
    /// Output across the turn's calls, reasoning included.
    pub output: u64,
    /// The running tool, `name argument`.
    pub current: Option<String>,
    /// Text is streaming.
    pub writing: bool,
}

impl TurnStats {
    fn new() -> Self {
        Self {
            started: std::time::Instant::now(),
            tools: 0,
            prompt: 0,
            cached: 0,
            output: 0,
            current: None,
            writing: false,
        }
    }

    /// `1m 12s · 4 tools · 42k in, 38k cached · 1.8k out`
    pub fn figures(&self) -> String {
        let mut parts = vec![crate::app::status::elapsed_short(self.started.elapsed())];
        match self.tools {
            0 => {}
            1 => parts.push("1 tool".into()),
            n => parts.push(format!("{n} tools")),
        }
        if self.prompt > 0 {
            let cached = if self.cached >= self.prompt {
                ", all cached".to_owned()
            } else if self.cached > 0 {
                format!(", {} cached", count_short(self.cached))
            } else {
                String::new()
            };
            parts.push(format!("{} in{cached}", count_short(self.prompt)));
        }
        if self.output > 0 {
            parts.push(format!("{} out", count_short(self.output)));
        }
        parts.join(" · ")
    }

    /// What the turn is doing now.
    pub fn activity(&self) -> String {
        match (&self.current, self.writing) {
            (Some(tool), _) => format!("running {tool}"),
            (None, true) => "writing".into(),
            (None, false) => "thinking".into(),
        }
    }
}

/// `950`, `1.8k`, `42k`, `1.3M`: token counts, decimal.
pub fn count_short(n: u64) -> String {
    if n < 1000 {
        n.to_string()
    } else if n < 10_000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else if n < 1_000_000 {
        format!("{}k", n / 1000)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
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
    /// The checklist changed. A pipe prints it; a shell draws it from
    /// the engine instead.
    fn tasks(&mut self, tasks: &[aigentic_runtime::harness_tools::Task]) {
        for l in Cell::Tasks(tasks.to_vec()).plain() {
            self.line(&l);
        }
    }
    /// A long text to page through (`/diff`); a pipe prints it.
    fn pager(&mut self, _title: &str, text: &str) {
        for l in text.lines() {
            self.line(l);
        }
    }
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

/// A vector, for tests.
#[cfg(test)]
#[derive(Default)]
pub struct Lines(pub Vec<String>);

#[cfg(test)]
impl Printer for Lines {
    fn line(&mut self, text: &str) {
        self.0.push(text.to_owned());
    }
}

/// What the client knows about its thread.
pub struct ClientRepl {
    client: Client,
    thread: Ulid,
    user: String,
    /// The user's role in the project, from `Welcome`.
    role: Option<String>,
    skills: Vec<String>,
    /// Running calls by id: name and summary, for the result's cell.
    calls: HashMap<String, (String, String)>,
    state: ThreadState,
    mode: String,
    /// Streamed assistant text not yet ended by a newline.
    partial: String,
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
    quit: bool,
    /// The last `Notice::Usage`: window fill and window, for the
    /// status line (phase 6 step 3); kept, not yet shown.
    usage: Option<(u64, u64)>,
    /// A post went out while idle and its turn has not been seen
    /// running yet; input at its end waits for that turn.
    awaiting_turn: bool,
}

impl ClientRepl {
    pub fn new(
        client: Client,
        thread: Ulid,
        user: &str,
        role: Option<String>,
        state: ThreadState,
        mode: String,
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
            partial: String::new(),
            prompted: None,
            menu: None,
            turn: None,
            project: None,
            title: None,
            tasks: Vec::new(),
            task_calls: std::collections::HashSet::new(),
            usage: None,
            awaiting_turn: false,
            quit: false,
        }
    }

    /// The user-invoked skills, so `/<skill>` dispatches.
    pub fn with_skills(mut self, skills: Vec<String>) -> Self {
        self.skills = skills;
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
        // A pending question or request this client prompted for takes
        // the line first; without the role the line is what it is.
        match &self.state {
            ThreadState::AwaitingHuman { call_id, .. }
                if self.prompted.as_deref() == Some(call_id) && !line.trim().starts_with('/') =>
            {
                let call_id = call_id.clone();
                let r = self
                    .request(Request::AnswerHuman {
                        thread: self.thread,
                        call_id,
                        text: line.trim().to_owned(),
                    })
                    .await;
                self.answered(r, out);
                return;
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
                ThreadState::Running { by, queued } => format!(
                    "turn running by {}; {queued} message(s) queued for the next turn",
                    author_name(by)
                ),
                ThreadState::Idle => "idle; nothing queued".into(),
                ThreadState::AwaitingApproval { call, .. } => {
                    format!("waiting for an approver: {}", describe_call(call))
                }
                ThreadState::AwaitingHuman { question, .. } => {
                    format!("waiting for an answer: {question}")
                }
            }),
            Command::Threads => {
                // The thread's project is what the daemon told us on open;
                // the listing needs the name, which `ListProjects` gives.
                let r = self.request(Request::ListProjects).await;
                let project = match r {
                    Response::Projects { projects } => projects.into_iter().next().map(|p| p.name),
                    _ => None,
                };
                match project {
                    Some(project) => {
                        let r = self.request(Request::ListThreads { project }).await;
                        self.show(r, "", out);
                    }
                    None => out.line("[no project to list threads for]"),
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
            Command::Profile(_) => {
                out.line(
                    "[/profile is not available over the daemon: the profile is the project's]",
                );
            }
            Command::Unknown(cmd) => out.line(&format!("unknown command: {cmd}")),
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
            (_, false) => "[queued for the next turn]",
        };
        if matches!(r, Response::Ok) && matches!(self.state, ThreadState::Idle) {
            self.awaiting_turn = true;
        }
        self.show(r, ok, out);
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

    /// Commit the checklist to the transcript and clear it.
    fn settle_tasks(&mut self, out: &mut dyn Printer) {
        if !self.tasks.is_empty() {
            out.cell(Cell::Tasks(std::mem::take(&mut self.tasks)), true);
        }
    }

    /// The running turn's figures, for the turn line.
    pub fn turn(&self) -> Option<&TurnStats> {
        self.turn.as_ref()
    }

    /// The menu the shell draws while this client is prompted.
    pub fn menu(&self) -> Option<&Menu> {
        self.prompted.as_ref().and(self.menu.as_ref())
    }

    /// A key while the menu is up: the selection, the digits, the hidden
    /// accelerators and Enter, sending the decision with the echo of
    /// what was chosen. `composer_empty` and `settled` as `Menu::key`
    /// takes them. `Reason` says the composer should take the deny's
    /// reason.
    pub async fn menu_key(
        &mut self,
        key: &crossterm::event::KeyEvent,
        composer_empty: bool,
        settled: bool,
        out: &mut dyn Printer,
    ) -> MenuKey {
        if self.prompted.is_none() {
            return MenuKey::Passed;
        }
        let Some(menu) = self.menu.as_mut() else {
            return MenuKey::Passed;
        };
        match menu.key(key, composer_empty, settled) {
            Keyed::Passed => MenuKey::Passed,
            Keyed::Used => MenuKey::Used,
            Keyed::Reason => MenuKey::Reason,
            Keyed::Decide { pick, reason, echo } => {
                out.line(&echo);
                match pick {
                    Pick::Allow { session, prefix } => {
                        self.decide(true, session, prefix, reason, out).await;
                    }
                    Pick::Deny => {
                        self.decide(false, false, None, reason, out).await;
                    }
                }
                MenuKey::Used
            }
        }
    }

    /// The composer's text for the prompt, Enter on the reason input:
    /// a deny's reason, with none when it was empty. A prompt answered
    /// elsewhere first is gone; the text goes nowhere.
    pub async fn prompt_text(&mut self, reason: Option<String>, out: &mut dyn Printer) {
        if self.menu().is_none() {
            return;
        }
        if self
            .menu
            .as_ref()
            .is_some_and(|m| m.kind == Kind::Permission)
        {
            let echo = match &reason {
                Some(r) => format!("↳ No: {r}"),
                None => "↳ No".into(),
            };
            out.line(&echo);
            self.decide(false, false, None, reason, out).await;
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

    /// The last window fill the daemon reported: tokens in the window
    /// and the window itself.
    pub fn usage(&self) -> Option<(u64, u64)> {
        self.usage
    }

    pub fn quit_requested(&self) -> bool {
        self.quit
    }

    /// One notice to lines. Streamed text is printed as its lines
    /// complete; the rest at the message's end.
    pub fn render(&mut self, notice: Notice, out: &mut dyn Printer) {
        match notice {
            Notice::TextDelta { text, .. } => {
                if let Some(t) = self.turn.as_mut() {
                    t.writing = true;
                }
                self.partial.push_str(&text);
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
                self.task_calls.insert(call.id.clone());
                if let Ok(args) = serde_json::from_value::<
                    aigentic_runtime::harness_tools::UpdateTasksArgs,
                >(call.args.clone())
                {
                    use aigentic_runtime::harness_tools::TaskState;
                    self.tasks = args.tasks;
                    out.tasks(&self.tasks);
                    if !self.tasks.is_empty()
                        && self.tasks.iter().all(|t| t.state == TaskState::Done)
                    {
                        self.settle_tasks(out);
                    }
                }
            }
            Notice::ToolCallStarted { call, .. } => {
                self.flush_partial(out);
                let summary = summarise_args(&call);
                if let Some(t) = self.turn.as_mut() {
                    t.tools += 1;
                    t.writing = false;
                    t.current = Some(format!("{} {summary}", call.name));
                }
                self.calls
                    .insert(call.id.clone(), (call.name.clone(), summary.clone()));
                out.cell(
                    Cell::Tool {
                        name: call.name,
                        summary,
                        state: ToolState::Running,
                        output: String::new(),
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
                    }
                }
                self.state = state;
            }
            Notice::Event { event, .. } => self.render_event(&event, out),
            Notice::Usage {
                tokens_in_window,
                window,
                ..
            } => self.usage = Some((tokens_in_window, window)),
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
            ThreadState::AwaitingHuman { call_id, question } => {
                if self.may_write() {
                    let menu = Menu::question(question);
                    out.prompt(&menu);
                    self.menu = Some(menu);
                    self.prompted = Some(call_id.clone());
                } else {
                    out.line(&format!("[waiting for an answer: {question}]"));
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
        match event.kind {
            EventKind::AssistantMessage => {
                self.flush_partial(out);
                if let Some(t) = self.turn.as_mut() {
                    t.writing = false;
                    if let Ok(AssistantMessagePayload { usage: Some(u), .. }) =
                        serde_json::from_value(event.payload.clone())
                    {
                        t.prompt = u.input_tokens + u.cache_read_tokens + u.cache_write_tokens;
                        t.cached = u.cache_read_tokens;
                        t.output += u.output_tokens;
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
                }
                if let Ok(ToolResultPayload { result: r, .. }) =
                    serde_json::from_value(event.payload.clone())
                    && self.task_calls.remove(&r.id)
                {
                    return;
                }
                if let Ok(ToolResultPayload { result: r, .. }) =
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
                    let (name, summary) = self
                        .calls
                        .remove(&r.id)
                        .unwrap_or_else(|| ("tool".to_owned(), String::new()));
                    if matches!(name.as_str(), "edit_file" | "write_file")
                        && !r.is_error
                        && let Some(edit) = crate::app::diff::parse_edit_result(&r.content)
                    {
                        out.cell(Cell::Edit(edit), true);
                        return;
                    }
                    out.cell(
                        Cell::Tool {
                            name,
                            summary,
                            state: if r.is_error {
                                ToolState::Err
                            } else {
                                ToolState::Ok
                            },
                            output: r.content,
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
                self.settle_tasks(out);
                if let Some(t) = self.turn.take() {
                    out.cell(Cell::Summary(format!("─ {}", t.figures())), true);
                }
                if let Ok(p) = serde_json::from_value::<TurnEndedPayload>(event.payload.clone()) {
                    if p.reason == "done" || p.reason == ASKED_HUMAN || p.reason == INTERRUPTED {
                        return;
                    }
                    if p.touched.is_empty() {
                        out.line(&format!("[turn ended: {}]", p.reason));
                    } else {
                        out.line(&format!(
                            "[turn ended: {}; wrote {}]",
                            p.reason,
                            p.touched.join(", ")
                        ));
                    }
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
            EventKind::MemoryExtracted => {
                if let Ok(p) =
                    serde_json::from_value::<MemoryExtractedPayload>(event.payload.clone())
                    && !p.written.is_empty()
                {
                    out.quiet(&format!("[memory: {} lines written]", p.written.len()));
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
            EventKind::Pinned
            | EventKind::PermissionRequested
            | EventKind::ThreadStarted
            | EventKind::ContextEvicted => {}
        }
    }
}

pub fn author_name(author: &Author) -> String {
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

/// `aigentic threads --server ...`: the project's threads over the API.
pub async fn list_threads_over(client: &Client, project: &str) -> anyhow::Result<String> {
    match client
        .request(Request::ListThreads {
            project: project.to_owned(),
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
            project: project.to_owned(),
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
    use crate::config::Config;
    use aigentic_api::client::Addr;
    use aigentic_runtime::aigentic_core::{
        Capabilities, CompletionRequest, Message, Provider, ProviderEvent,
    };
    use aigentic_server::build::{BuildError, ProviderFactory};
    use aigentic_server::{DefaultReports, Server};
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

    /// A config with a scripted profile, threads and skills under `dir`.
    fn config(dir: &std::path::Path) -> Config {
        Config::parse(&format!(
            "default_profile = \"a\"\nthreads_dir = {:?}\nbundled_dir = {:?}\n[profiles.a]\nbase_url = \"u\"\nmodel = \"m\"\napi_key_env = \"K\"\n[profiles.b]\nbase_url = \"u\"\nmodel = \"m\"\napi_key_env = \"K\"\n",
            dir.join("threads").display(),
            dir.display()
        ))
        .unwrap()
    }

    fn project(dir: &std::path::Path, name: &str, participants: &str) -> std::path::PathBuf {
        let root = dir.join(name);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("aigentic.toml"),
            format!("[project]\nname = \"{name}\"\n{participants}"),
        )
        .unwrap();
        root
    }

    /// Create a thread in `project`, open it, and hand back what a REPL
    /// needs.
    async fn open(
        client: &Client,
        project: &str,
        thread: Option<Ulid>,
    ) -> (Ulid, ThreadState, String) {
        let id = match thread {
            Some(id) => id,
            None => {
                let Response::Thread { thread } = client
                    .request(Request::CreateThread {
                        project: project.into(),
                    })
                    .await
                    .unwrap()
                else {
                    panic!()
                };
                thread.id
            }
        };
        let Response::Opened { state, mode, .. } = client
            .request(Request::Open {
                thread: id,
                from_seq: 0,
            })
            .await
            .unwrap()
        else {
            panic!()
        };
        (id, state, mode)
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
        let mut repl = ClientRepl::new(client, thread, "steve", role, state, mode);
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
        let mut repl = ClientRepl::new(client, thread, "steve", role, state, mode);
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
        let mut repl = ClientRepl::new(steve, thread, "steve", role, state, mode);
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
        let mut steve_repl = ClientRepl::new(steve, thread, "steve", steve_role, state, mode);

        let (reviewer, welcome) = connect("reviewer").await;
        let reviewer_role = welcome.projects[0].role.clone();
        assert_eq!(reviewer_role.as_deref(), Some("read"));
        let (_, state, mode) = open(&reviewer, "p", Some(thread)).await;
        let reviewer_notices = reviewer.take_notices().unwrap();
        let mut reviewer_repl =
            ClientRepl::new(reviewer, thread, "reviewer", reviewer_role, state, mode);

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
        let curl = || {
            call(
                "c",
                "bash",
                serde_json::json!({"command": "curl -s https://example.com"}),
            )
        };
        let script = vec![
            // Turn 1: curl asks; `p` allows curl -s from now on.
            vec![curl(), tool_use()],
            vec![text("fetched"), done()],
            // Turn 2: the same call runs without asking.
            vec![curl(), tool_use()],
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
        let mut repl = ClientRepl::new(client, thread, "steve", role, state, mode);
        let (tx, rx) = mpsc::unbounded_channel();
        let feeder = async move {
            let pause = || tokio::time::sleep(std::time::Duration::from_millis(400));
            tx.send("one".into()).unwrap();
            pause().await;
            tx.send("p".into()).unwrap();
            pause().await;
            tx.send("two".into()).unwrap();
            pause().await;
            tx.send("three".into()).unwrap();
            pause().await;
            tx.send("n the build directory is shared".into()).unwrap();
            pause().await;
            tx.send("/quit".into()).unwrap();
        };
        let mut out = Lines::default();
        let ((), ()) = tokio::join!(repl.run(rx, notices, &mut out), feeder);
        let lines = out.0;
        let asks = lines
            .iter()
            .filter(|l| l.starts_with("[permission] Run this command?"))
            .count();
        assert_eq!(asks, 2, "curl asked once, rm once: {lines:#?}");
        assert!(
            lines
                .iter()
                .any(|l| l.contains("don't ask again for `curl -s` in this project")),
            "{lines:#?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l == "↳ Yes, and don't ask again for `curl -s` in this project"),
            "the choice is echoed into the record: {lines:#?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("denied by steve: the build directory is shared")),
            "{lines:#?}"
        );
        let rules = std::fs::read_to_string(root.join(".aigentic/rules.toml")).unwrap();
        assert!(rules.contains("\"curl -s\""), "{rules}");
    }

    #[test]
    fn turn_figures_read_short() {
        let mut t = TurnStats::new();
        assert_eq!(t.activity(), "thinking");
        t.tools = 4;
        t.prompt = 42_310;
        t.cached = 38_004;
        t.output = 1_840;
        t.current = Some("bash cargo test".into());
        let f = t.figures();
        assert!(
            f.ends_with("4 tools · 42k in, 38k cached · 1.8k out"),
            "{f}"
        );
        assert_eq!(t.activity(), "running bash cargo test");
        assert_eq!(count_short(950), "950");
        assert_eq!(count_short(1_300_000), "1.3M");
    }
}
