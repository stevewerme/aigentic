//! The REPL over the daemon's client (phase 5 step 9): lines in,
//! requests out, notices rendered as they arrive. The same code path
//! whether the daemon is embedded for one user or remote for many.
//! Rendering goes through a `Printer`, which is rustyline's external
//! printer at the terminal (so lines land above the prompt) and a
//! vector in tests.

use std::collections::VecDeque;

use aigentic_api::client::Client;
use aigentic_api::{Notice, ReportKind, Request, Response, ThreadState};
use aigentic_runtime::aigentic_core::{Author, ContentBlock, EventKind, ToolCall};
use aigentic_runtime::aigentic_log::{
    CompactedPayload, CompactionStrategy, DecisionScope, InterruptedPayload,
    MemoryExtractedPayload, PermissionDecidedPayload, SkillLoadedPayload, ToolResultPayload,
    TurnEndedPayload, UserMessagePayload,
};
use aigentic_runtime::{ASKED_HUMAN, INTERRUPTED};
use tokio::sync::mpsc;
use ulid::Ulid;

use crate::config::DisplaySection;
use crate::repl::{Command, HELP, parse_line, truncate_for_display};

/// Where rendered lines go.
pub trait Printer {
    fn line(&mut self, text: &str);
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

/// `/verbose`'s caps for tool results.
const VERBOSE_RESULT_LINES: usize = 40;
const VERBOSE_RESULT_BYTES: usize = 8000;

/// What the client knows about its thread.
pub struct ClientRepl {
    client: Client,
    thread: Ulid,
    user: String,
    /// The user's role in the project, from `Welcome`.
    role: Option<String>,
    skills: Vec<String>,
    display: DisplaySection,
    verbose: bool,
    state: ThreadState,
    mode: String,
    /// Streamed assistant text not yet ended by a newline.
    partial: String,
    /// Whether the last thing printed was our own streamed text; a
    /// tool call or note then starts on its own line.
    quit: bool,
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
            display: DisplaySection::default(),
            verbose: false,
            state,
            mode,
            partial: String::new(),
            quit: false,
        }
    }

    pub fn with_display(mut self, display: DisplaySection) -> Self {
        self.display = display;
        self
    }

    /// The user-invoked skills, so `/<skill>` dispatches.
    pub fn with_skills(mut self, skills: Vec<String>) -> Self {
        self.skills = skills;
        self
    }

    fn caps(&self) -> (usize, usize) {
        if self.verbose {
            (VERBOSE_RESULT_LINES, VERBOSE_RESULT_BYTES)
        } else {
            (self.display.result_lines, self.display.result_bytes)
        }
    }

    fn may_approve(&self) -> bool {
        matches!(self.role.as_deref(), Some("approve" | "admin"))
    }

    /// The loop: lines from `input` and notices from the client until
    /// `/quit`, end of input, or the daemon going away.
    pub async fn run(
        &mut self,
        mut input: mpsc::UnboundedReceiver<String>,
        mut notices: mpsc::Receiver<Notice>,
        out: &mut dyn Printer,
    ) {
        while !self.quit {
            tokio::select! {
                line = input.recv() => match line {
                    None => break,
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
                if threads.is_empty() {
                    out.line("no threads");
                }
                for t in threads {
                    out.line(&format!(
                        "{}  {}  {:>5}  {}",
                        t.id, t.date, t.events, t.first_line
                    ));
                }
            }
            other => out.line(&format!("[{other:?}]")),
        }
    }

    async fn handle_line(&mut self, line: &str, out: &mut dyn Printer) {
        // `!text` interrupts, as typing a command while the model streams
        // is the common case.
        if let Some(text) = line.strip_prefix('!') {
            let text = text.trim();
            if !text.is_empty() {
                self.post(text, true, out).await;
            }
            return;
        }
        // A pending question or request takes the line first.
        match &self.state {
            ThreadState::AwaitingHuman { call_id, .. } if !line.trim().starts_with('/') => {
                let call_id = call_id.clone();
                let r = self
                    .request(Request::AnswerHuman {
                        thread: self.thread,
                        call_id,
                        text: line.trim().to_owned(),
                    })
                    .await;
                self.show(r, "", out);
                return;
            }
            ThreadState::AwaitingApproval { call_id, .. } => {
                let answer = match line.trim().to_ascii_lowercase().as_str() {
                    "y" | "yes" => Some((true, false)),
                    "a" | "always" => Some((true, true)),
                    "n" | "no" => Some((false, false)),
                    _ => None,
                };
                if let Some((allow, session)) = answer {
                    let call_id = call_id.clone();
                    let r = self
                        .request(Request::Decide {
                            thread: self.thread,
                            call_id,
                            allow,
                            session,
                        })
                        .await;
                    self.show(r, "", out);
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
            Command::Verbose => {
                self.verbose = !self.verbose;
                let (lines, bytes) = self.caps();
                out.line(&format!(
                    "[verbose {}: {lines} lines / {bytes} bytes of tool output]",
                    if self.verbose { "on" } else { "off" }
                ));
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
        self.show(r, ok, out);
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
            out.line(&text);
        }
    }

    /// One notice to lines. Streamed text is printed as its lines
    /// complete; the rest at the message's end.
    fn render(&mut self, notice: Notice, out: &mut dyn Printer) {
        match notice {
            Notice::TextDelta { text, .. } => {
                self.partial.push_str(&text);
                while let Some(pos) = self.partial.find('\n') {
                    let line: String = self.partial.drain(..=pos).collect();
                    out.line(line.trim_end_matches('\n'));
                }
            }
            Notice::ToolCallStarted { call, .. } => {
                self.flush_partial(out);
                out.line(&format!("→ {}", describe_call(&call)));
            }
            Notice::Mode { mode, .. } => {
                self.mode = mode.clone();
                out.line(&format!("[mode {mode}]"));
            }
            Notice::State { state, .. } => {
                self.flush_partial(out);
                match &state {
                    ThreadState::AwaitingApproval {
                        call,
                        class,
                        reason,
                        ..
                    } => {
                        out.line(&format!(
                            "[permission] {} (class {}): {reason}",
                            call.name,
                            class_name(*class)
                        ));
                        out.line(&format!(
                            "  {}",
                            truncate_for_display(&call.args.to_string(), 6, 600)
                        ));
                        if self.may_approve() {
                            out.line("  allow? y once / a always this session / n no");
                        } else {
                            out.line("  waiting for an approver");
                        }
                    }
                    ThreadState::AwaitingHuman { question, .. } => {
                        out.line(&format!("[question] {question}"));
                        out.line("  type the answer");
                    }
                    ThreadState::Running { .. } | ThreadState::Idle => {}
                }
                self.state = state;
            }
            Notice::Event { event, .. } => self.render_event(&event, out),
        }
    }

    fn render_event(
        &mut self,
        event: &aigentic_runtime::aigentic_core::Event,
        out: &mut dyn Printer,
    ) {
        let (lines, bytes) = self.caps();
        match event.kind {
            EventKind::AssistantMessage => self.flush_partial(out),
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
                if let Ok(ToolResultPayload { result: r, .. }) =
                    serde_json::from_value(event.payload.clone())
                {
                    let marker = if r.is_error { "✗" } else { "✓" };
                    for line in truncate_for_display(&r.content, lines, bytes).lines() {
                        out.line(&format!("  {marker} {line}"));
                    }
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
                    out.line(&format!(
                        "  [{what} by {}{why}]",
                        author_name(&event.author)
                    ));
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
                    out.line(&format!("[memory: {} lines written]", p.written.len()));
                }
            }
            EventKind::Pinned | EventKind::PermissionRequested | EventKind::ThreadStarted => {}
        }
    }
}

fn author_name(author: &Author) -> String {
    match author {
        Author::User(u) => u.0.clone(),
        Author::Agent(a) => a.0.clone(),
        Author::System => "system".into(),
    }
}

fn class_name(class: aigentic_runtime::aigentic_core::RiskClass) -> &'static str {
    use aigentic_runtime::aigentic_core::RiskClass;
    match class {
        RiskClass::Read => "read",
        RiskClass::Write => "write",
        RiskClass::Exec => "exec",
        RiskClass::Network => "network",
        RiskClass::Safe => "safe",
    }
}

fn describe_call(call: &ToolCall) -> String {
    let args = call.args.to_string();
    let args = truncate_for_display(&args, 1, 200);
    format!("{} {}", call.name, args)
}

/// Lines typed at a terminal, read by rustyline on its own thread so the
/// async loop is never blocked; rendered lines go above the prompt
/// through its external printer. Without a terminal (a pipe, a script)
/// plain stdin lines and stdout.
pub struct TerminalInput {
    pub lines: mpsc::UnboundedReceiver<String>,
    pub printer: Box<dyn Printer + Send>,
    _thread: std::thread::JoinHandle<()>,
}

struct ExternalPrinter(Box<dyn rustyline::ExternalPrinter + Send>);

impl Printer for ExternalPrinter {
    fn line(&mut self, text: &str) {
        let _ = self.0.print(format!("{text}\n"));
    }
}

struct Stdout;

impl Printer for Stdout {
    fn line(&mut self, text: &str) {
        println!("{text}");
    }
}

impl TerminalInput {
    /// Start reading. `history` is rustyline's file.
    pub fn start(history: std::path::PathBuf) -> anyhow::Result<Self> {
        use std::io::IsTerminal;
        let (tx, lines) = mpsc::unbounded_channel();
        if !std::io::stdin().is_terminal() {
            let thread = std::thread::spawn(move || {
                use std::io::BufRead;
                for line in std::io::stdin().lock().lines() {
                    let Ok(line) = line else { break };
                    if tx.send(line).is_err() {
                        break;
                    }
                }
            });
            return Ok(Self {
                lines,
                printer: Box::new(Stdout),
                _thread: thread,
            });
        }
        let mut editor = rustyline::DefaultEditor::new()?;
        let _ = editor.load_history(&history);
        let printer = editor.create_external_printer()?;
        let thread = std::thread::spawn(move || {
            loop {
                match editor.readline("> ") {
                    Ok(line) => {
                        if !line.trim().is_empty() {
                            let _ = editor.add_history_entry(line.trim());
                        }
                        if tx.send(line).is_err() {
                            break;
                        }
                    }
                    Err(rustyline::error::ReadlineError::Interrupted) => continue,
                    Err(_) => break,
                }
            }
            let _ = editor.save_history(&history);
        });
        Ok(Self {
            lines,
            printer: Box::new(ExternalPrinter(Box::new(printer))),
            _thread: thread,
        })
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

    struct Factory(Mutex<Option<Vec<Vec<ProviderEvent>>>>);

    impl ProviderFactory for Factory {
        fn build(&self, _: &str) -> Result<(Box<dyn Provider>, String), BuildError> {
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

    #[tokio::test]
    async fn the_repl_streams_a_reply_reports_and_quits_over_an_embedded_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("aigentic.toml"), "[project]\nname = \"proj\"\n").unwrap();
        let cfg_dir = dir.path().join("cfg");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let threads = dir.path().join("threads");
        let config = Config::parse(&format!(
            "threads_dir = {:?}\nbundled_dir = {:?}\n[profiles.a]\nbase_url = \"u\"\nmodel = \"m\"\napi_key_env = \"K\"\n",
            threads.display(),
            dir.path().display()
        ))
        .unwrap();
        let script = vec![vec![text("Hej "), text("Steve!\nLine two"), done()]];
        let embedded = Server::embed_with(
            config,
            cfg_dir.clone(),
            root,
            "steve",
            Arc::new(Factory(Mutex::new(Some(script)))),
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
        let Response::Thread { thread } = client
            .request(Request::CreateThread {
                project: "proj".into(),
            })
            .await
            .unwrap()
        else {
            panic!()
        };
        let Response::Opened { state, mode, .. } = client
            .request(Request::Open {
                thread: thread.id,
                from_seq: 0,
            })
            .await
            .unwrap()
        else {
            panic!()
        };
        let notices = client.take_notices().unwrap();
        let mut repl = ClientRepl::new(client, thread.id, "steve", role, state, mode);
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
            tx.send("/verbose".into()).unwrap();
            tx.send("/nope".into()).unwrap();
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
        assert!(
            lines.iter().any(|l| l.starts_with("[verbose on:")),
            "{lines:#?}"
        );
        assert!(
            lines.iter().any(|l| l == "unknown command: /nope"),
            "{lines:#?}"
        );
        drop(embedded);
    }
}
