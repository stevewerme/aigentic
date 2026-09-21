use std::io::Write;
use std::path::PathBuf;

use aigentic_runtime::aigentic_core::{Author, ContentBlock, EventKind, ToolCall};
use aigentic_runtime::aigentic_log::{
    CompactedPayload, CompactionStrategy, DecisionScope, PermissionDecidedPayload,
    SkillLoadedPayload, ToolResultPayload,
};
use aigentic_runtime::{ASKED_HUMAN, Resumed, Runtime, Signal};
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;

use crate::cost::cost_of;
use crate::project_cmd::{list_threads, render_threads, report};

/// Lines of tool output shown before truncating.
const RESULT_LINES: usize = 12;
/// Bytes of tool output shown before truncating.
const RESULT_BYTES: usize = 1200;

pub struct Repl {
    runtime: Runtime,
    user: Author,
    history: PathBuf,
    /// This project's threads, for `/threads`.
    threads_dir: PathBuf,
    /// The global layer's file, named in `/project`.
    global_instructions: PathBuf,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Command<'a> {
    Cost,
    Quit,
    Help,
    Skills,
    Project,
    Threads,
    Pin(&'a str),
    Compact,
    /// A user-invoked skill: its name and the rest of the line.
    Skill(&'a str, &'a str),
    Unknown(&'a str),
    Chat(&'a str),
    Empty,
}

/// Dispatch a line. `skills` are the slash commands the enabled
/// user-invoked skills add; built-in commands win on a name clash.
pub fn parse_line<'a>(line: &'a str, skills: &[String]) -> Command<'a> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Command::Empty;
    }
    let Some(rest) = trimmed.strip_prefix('/') else {
        return Command::Chat(trimmed);
    };
    let (head, tail) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    match (head, tail.trim()) {
        ("cost", _) => Command::Cost,
        ("quit" | "exit", _) => Command::Quit,
        ("help", _) => Command::Help,
        ("skills", _) => Command::Skills,
        ("project", _) => Command::Project,
        ("threads", _) => Command::Threads,
        ("compact", _) => Command::Compact,
        ("pin", text) if !text.is_empty() => Command::Pin(text),
        (name, args) if skills.iter().any(|s| s == name) => Command::Skill(name, args),
        _ => Command::Unknown(trimmed),
    }
}

const HELP: &str = "\
/cost            tokens for the thread, reported and estimated separately
/pin <text>      pin a fact to the stable prefix
/compact         run compaction now
/skills          list enabled skills; user-invoked ones are slash commands
/project         the layers, the knowledge mode and every tool's fate
/threads         this project's threads, newest first
/<skill> [args]  run a user-invoked skill
/help            this list
/quit            exit (Ctrl-D too)
Permission prompts: y once, a for the session, n or Ctrl-C to deny.";

impl Repl {
    pub fn new(runtime: Runtime, user: Author, history: PathBuf) -> Self {
        Self {
            runtime,
            user,
            history,
            threads_dir: PathBuf::new(),
            global_instructions: PathBuf::new(),
        }
    }

    pub fn with_project_paths(
        mut self,
        threads_dir: PathBuf,
        global_instructions: PathBuf,
    ) -> Self {
        self.threads_dir = threads_dir;
        self.global_instructions = global_instructions;
        self
    }

    /// Start the REPL. `resumed` is what `Runtime::resume` found; an
    /// interrupted turn is finished before the first prompt.
    pub async fn run(&mut self, resumed: Resumed) -> anyhow::Result<()> {
        if let Resumed::Interrupted {
            after_seq,
            unanswered_calls,
            ..
        } = resumed
        {
            println!(
                "[turn interrupted after event {after_seq}; {unanswered_calls} tool call(s) got synthetic results; continuing]"
            );
            self.finish_turn(None).await;
        } else if self.runtime.awaiting_continuation().unwrap_or(false) {
            println!("[the human's answer is recorded; continuing]");
            self.finish_turn(None).await;
        }
        let mut editor = DefaultEditor::new()?;
        let _ = editor.load_history(&self.history);
        loop {
            let line = match editor.readline("> ") {
                Ok(line) => line,
                Err(ReadlineError::Interrupted) => continue,
                Err(ReadlineError::Eof) => break,
                Err(e) => return Err(e.into()),
            };
            let slash: Vec<String> = self
                .runtime
                .skills()
                .user_invoked()
                .iter()
                .map(|m| m.name.clone())
                .collect();
            match parse_line(&line, &slash) {
                Command::Empty => continue,
                Command::Quit => break,
                Command::Help => println!("{HELP}"),
                Command::Skills => {
                    let set = self.runtime.skills();
                    if set.is_empty() {
                        println!("no skills enabled; list them under [skills] in aigentic.toml");
                    }
                    for m in set.user_invoked() {
                        let hint = m.argument_hint.as_deref().unwrap_or("");
                        println!("/{:<24} {}  {}", m.name, m.description, hint);
                    }
                    for m in set.model_invoked() {
                        println!(" {:<24} {}  (model-invoked)", m.name, m.description);
                    }
                }
                Command::Skill(name, args) => {
                    let _ = editor.add_history_entry(line.trim());
                    let name = name.to_owned();
                    let args = args.to_owned();
                    self.run_skill(&name, &args).await;
                }
                Command::Cost => {
                    let events = self.runtime.log().read_all()?;
                    println!("{}", cost_of(&events));
                }
                Command::Project => {
                    println!("{}", report(&self.runtime, &self.global_instructions));
                }
                Command::Threads => match list_threads(&self.threads_dir) {
                    Ok(threads) => println!("{}", render_threads(&threads, &self.threads_dir)),
                    Err(e) => println!("[error: {e}]"),
                },
                Command::Pin(text) => {
                    let _ = editor.add_history_entry(line.trim());
                    match self
                        .runtime
                        .pin(self.user.clone(), text.to_owned(), &mut |_| {})
                    {
                        Ok(_) => println!("[pinned]"),
                        Err(e) => println!("[error: {e}]"),
                    }
                }
                Command::Compact => {
                    let mut at_line_start = true;
                    match self
                        .runtime
                        .compact_now(&mut |s| render(s, &mut at_line_start))
                        .await
                    {
                        Ok(did) if did.is_empty() => println!("[nothing to compact]"),
                        Ok(_) => {}
                        Err(e) => println!("[error: {e}]"),
                    }
                }
                Command::Unknown(cmd) => println!("unknown command: {cmd}"),
                Command::Chat(text) => {
                    let _ = editor.add_history_entry(text);
                    self.finish_turn(Some(text)).await;
                }
            }
        }
        let _ = editor.save_history(&self.history);
        Ok(())
    }

    async fn run_skill(&mut self, name: &str, args: &str) {
        let mut at_line_start = true;
        let outcome = self
            .runtime
            .invoke_skill(self.user.clone(), name, args, &mut |signal| {
                render(signal, &mut at_line_start)
            })
            .await;
        if !at_line_start {
            println!();
        }
        self.settle(outcome).await;
    }

    /// The end of a turn: continue after a human's answer (a new turn with
    /// its own budget), extract memory after `done`, or say why it stopped.
    async fn settle(
        &mut self,
        outcome: anyhow::Result<aigentic_runtime::TurnOutcome, aigentic_runtime::RuntimeError>,
    ) {
        let mut outcome = outcome;
        loop {
            match outcome {
                Ok(o) if o.reason == ASKED_HUMAN => {
                    let mut at_line_start = true;
                    outcome = self
                        .runtime
                        .continue_turn(&mut |signal| render(signal, &mut at_line_start))
                        .await;
                    if !at_line_start {
                        println!();
                    }
                }
                Ok(o) if o.reason != "done" => return println!("[turn ended: {}]", o.reason),
                Ok(_) => return self.after_done().await,
                Err(e) => return println!("[error: {e}]"),
            }
        }
    }

    /// After a turn that ended `done`: memory extraction, never during a
    /// turn. Prints `[memory: N lines written]` or nothing.
    async fn after_done(&mut self) {
        match self.runtime.extract_memory(&mut |_| {}).await {
            Ok(Some(p)) if !p.written.is_empty() => {
                println!("[memory: {} lines written]", p.written.len());
            }
            Ok(_) => {}
            Err(e) => println!("[memory: {e}]"),
        }
    }

    /// A new turn for `text`, or the continuation of an interrupted one.
    async fn finish_turn(&mut self, text: Option<&str>) {
        let mut at_line_start = true;
        let outcome = match text {
            Some(text) => {
                let blocks = vec![ContentBlock::Text(text.to_owned())];
                self.runtime
                    .run_turn(self.user.clone(), blocks, &mut |signal| {
                        render(signal, &mut at_line_start)
                    })
                    .await
            }
            None => {
                self.runtime
                    .continue_turn(&mut |signal| render(signal, &mut at_line_start))
                    .await
            }
        };
        if !at_line_start {
            println!();
        }
        self.settle(outcome).await;
    }
}

fn render(signal: Signal<'_>, at_line_start: &mut bool) {
    match signal {
        Signal::TextDelta(t) => {
            print!("{t}");
            let _ = std::io::stdout().flush();
            *at_line_start = t.ends_with('\n');
        }
        Signal::ToolCallStarted(call) => {
            if !*at_line_start {
                println!();
            }
            println!("→ {}", describe_call(call));
            *at_line_start = true;
        }
        Signal::Event(event) if event.kind == EventKind::ToolResult => {
            if let Ok(ToolResultPayload { result: r, .. }) =
                serde_json::from_value(event.payload.clone())
            {
                let marker = if r.is_error { "✗" } else { "✓" };
                for line in truncate_for_display(&r.content, RESULT_LINES, RESULT_BYTES).lines() {
                    println!("  {marker} {line}");
                }
            }
            *at_line_start = true;
        }
        Signal::Event(event) if event.kind == EventKind::Compacted => {
            if !*at_line_start {
                println!();
            }
            if let Ok(p) = serde_json::from_value::<CompactedPayload>(event.payload.clone()) {
                match p.strategy {
                    CompactionStrategy::TruncateResults { max_bytes } => println!(
                        "[compacted: tool results in events {}-{} truncated to {max_bytes} bytes]",
                        p.from_seq, p.to_seq
                    ),
                    CompactionStrategy::Summary { usage, .. } => println!(
                        "[compacted: events {}-{} summarised ({} tokens in, {} out)]",
                        p.from_seq,
                        p.to_seq,
                        usage.input_tokens + usage.cache_read_tokens + usage.cache_write_tokens,
                        usage.output_tokens
                    ),
                }
            }
            *at_line_start = true;
        }
        Signal::Event(event) if event.kind == EventKind::PermissionDecided => {
            if let Ok(p) = serde_json::from_value::<PermissionDecidedPayload>(event.payload.clone())
            {
                let who = match &event.author {
                    Author::User(u) => u.0.as_str(),
                    Author::Agent(a) => a.0.as_str(),
                    Author::System => "system",
                };
                let what = match (p.allow, p.scope) {
                    (true, DecisionScope::Once) => "allowed",
                    (true, DecisionScope::Session) => "allowed for this session",
                    (false, _) => "denied",
                };
                println!("  [{what} by {who}]");
            }
            *at_line_start = true;
        }
        Signal::Event(event) if event.kind == EventKind::SkillLoaded => {
            if !*at_line_start {
                println!();
            }
            if let Ok(p) = serde_json::from_value::<SkillLoadedPayload>(event.payload.clone()) {
                println!(
                    "[skill {} loaded ({} bytes, {})]",
                    p.name,
                    p.body.len(),
                    p.source
                );
            }
            *at_line_start = true;
        }
        Signal::Event(_) => {}
    }
}

fn describe_call(call: &ToolCall) -> String {
    let args = call.args.to_string();
    let args = truncate_for_display(&args, 1, 200);
    format!("{} {}", call.name, args)
}

/// Keep the first `max_lines` lines and `max_bytes` bytes; note the rest.
pub fn truncate_for_display(text: &str, max_lines: usize, max_bytes: usize) -> String {
    let mut shown = String::new();
    for (lines, line) in text.split_inclusive('\n').enumerate() {
        if lines >= max_lines || shown.len() + line.len() > max_bytes {
            break;
        }
        shown.push_str(line);
    }
    if shown.len() == text.len() {
        return shown;
    }
    let mut cut = shown.len();
    if cut == 0 {
        cut = max_bytes.min(text.len());
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        shown.push_str(&text[..cut]);
    }
    if !shown.ends_with('\n') {
        shown.push('\n');
    }
    shown.push_str(&format!("… ({} more bytes)", text.len() - cut));
    shown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slash_commands_dispatch() {
        let none: Vec<String> = vec![];
        assert_eq!(parse_line("/cost", &none), Command::Cost);
        assert_eq!(parse_line("  /quit  ", &none), Command::Quit);
        assert_eq!(parse_line("/exit", &none), Command::Quit);
        assert_eq!(parse_line("/help", &none), Command::Help);
        assert_eq!(parse_line("/skills", &none), Command::Skills);
        assert_eq!(parse_line("/compact", &none), Command::Compact);
        assert_eq!(parse_line("/project", &none), Command::Project);
        assert_eq!(parse_line("/threads", &none), Command::Threads);
        assert_eq!(
            parse_line("/pin  Answer in Swedish. ", &none),
            Command::Pin("Answer in Swedish.")
        );
        assert_eq!(parse_line("/pin", &none), Command::Unknown("/pin"));
        assert_eq!(
            parse_line("/nope arg", &none),
            Command::Unknown("/nope arg")
        );
        assert_eq!(
            parse_line("hello /cost", &none),
            Command::Chat("hello /cost")
        );
        assert_eq!(parse_line("   ", &none), Command::Empty);
    }

    #[test]
    fn skill_commands_come_from_the_enabled_set() {
        let skills = vec!["implement".to_owned(), "cost".to_owned()];
        assert_eq!(
            parse_line("/implement fix the off-by-one in cost.rs", &skills),
            Command::Skill("implement", "fix the off-by-one in cost.rs")
        );
        assert_eq!(
            parse_line("/implement", &skills),
            Command::Skill("implement", "")
        );
        assert_eq!(parse_line("/tdd x", &skills), Command::Unknown("/tdd x"));
        assert_eq!(
            parse_line("/cost", &skills),
            Command::Cost,
            "a built-in wins over a skill of the same name"
        );
    }

    #[test]
    fn short_output_is_untouched() {
        assert_eq!(truncate_for_display("a\nb\n", 12, 1200), "a\nb\n");
        assert_eq!(truncate_for_display("", 12, 1200), "");
    }

    #[test]
    fn long_output_is_cut_by_lines_and_bytes() {
        let many: String = (1..=50).map(|i| format!("{i}\n")).collect();
        let out = truncate_for_display(&many, 3, 1200);
        assert!(out.starts_with("1\n2\n3\n"), "{out}");
        assert!(
            out.ends_with(&format!("… ({} more bytes)", many.len() - 6)),
            "{out}"
        );

        let wide = "x".repeat(500);
        let out = truncate_for_display(&wide, 12, 100);
        assert!(out.starts_with(&"x".repeat(100)), "{out}");
        assert!(out.contains("… (400 more bytes)"), "{out}");
    }
}
