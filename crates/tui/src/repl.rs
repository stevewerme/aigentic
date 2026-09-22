use std::io::Write;
use std::path::PathBuf;

use aigentic_runtime::aigentic_core::{Author, ContentBlock, EventKind, ToolCall};
use aigentic_runtime::aigentic_log::{
    CompactedPayload, CompactionStrategy, DecisionScope, MemoryExtractedPayload,
    PermissionDecidedPayload, SkillLoadedPayload, ToolResultPayload,
};
use aigentic_runtime::aigentic_policy::Decision;
use aigentic_runtime::{ASKED_HUMAN, Mode, Resumed, Runtime, Signal};
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;

use crate::config::{Config, DisplaySection, Profile};
use crate::cost::cost_of;
use crate::project_cmd::{list_threads, render_threads, report};

/// What `/verbose` shows of a tool result: lines and bytes.
const VERBOSE_RESULT_LINES: usize = 40;
const VERBOSE_RESULT_BYTES: usize = 8000;

pub struct Repl {
    runtime: Runtime,
    user: Author,
    history: PathBuf,
    /// This project's threads, for `/threads`.
    threads_dir: PathBuf,
    /// The global layer's file, named in `/project`.
    global_instructions: PathBuf,
    /// The `[display]` caps for tool results.
    display: DisplaySection,
    /// The config, for `/profile`; absent in tests.
    config: Option<Config>,
    /// The profile the runtime's provider was built from.
    profile_name: String,
    /// Whether `/verbose` widened the caps for this session.
    verbose: bool,
}

/// The caps the terminal shows of a tool result right now.
#[derive(Clone, Copy)]
struct Caps {
    lines: usize,
    bytes: usize,
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
    /// Toggle the session between the configured tool-output cap and a
    /// larger one.
    Verbose,
    /// Print the permission mode, or set it when a name is given.
    Mode(Option<&'a str>),
    /// Swap the provider to a profile from the config, between turns.
    Profile(&'a str),
    /// The rule table, the bash allow patterns, the mode and the grants.
    Policy,
    /// The memory files as the prefix carries them.
    Memory,
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
        ("verbose", _) => Command::Verbose,
        ("mode", "") => Command::Mode(None),
        ("mode", name) => Command::Mode(Some(name)),
        ("profile", name) if !name.is_empty() => Command::Profile(name),
        ("policy", _) => Command::Policy,
        ("memory", _) => Command::Memory,
        ("pin", text) if !text.is_empty() => Command::Pin(text),
        (name, args) if skills.iter().any(|s| s == name) => Command::Skill(name, args),
        _ => Command::Unknown(trimmed),
    }
}

const HELP: &str = "\
/cost            tokens for the thread, reported and estimated separately
/pin <text>      pin a fact to the stable prefix
/compact         run compaction now
/verbose         toggle tool output between the configured cap and 40 lines / 8000 bytes
/mode [name]     show the permission mode, or set it: manual, accept-edits, auto
/profile <name>  swap the provider to that profile from the config, for the next turn on
/policy          the rule table, the bash allow patterns, the mode and the session grants
/memory          the memory files as the prefix carries them, and the last extraction
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
            display: DisplaySection::default(),
            config: None,
            profile_name: String::new(),
            verbose: false,
        }
    }

    /// The config and the profile the provider came from, for `/profile`.
    pub fn with_config(mut self, config: Config, profile_name: &str) -> Self {
        self.config = Some(config);
        self.profile_name = profile_name.to_owned();
        self
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

    /// The `[display]` caps, from `config.toml`.
    pub fn with_display(mut self, display: DisplaySection) -> Self {
        self.display = display;
        self
    }

    /// What the terminal shows of a tool result: `/verbose`'s larger caps
    /// while it is on, the configured ones while it is off.
    fn caps(&self) -> Caps {
        if self.verbose {
            Caps {
                lines: VERBOSE_RESULT_LINES,
                bytes: VERBOSE_RESULT_BYTES,
            }
        } else {
            Caps {
                lines: self.display.result_lines,
                bytes: self.display.result_bytes,
            }
        }
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
                    let caps = self.caps();
                    match self
                        .runtime
                        .compact_now(&mut |s| render(s, &mut at_line_start, caps))
                        .await
                    {
                        Ok(did) if did.is_empty() => println!("[nothing to compact]"),
                        Ok(_) => {}
                        Err(e) => println!("[error: {e}]"),
                    }
                }
                Command::Verbose => {
                    self.verbose = !self.verbose;
                    let caps = self.caps();
                    println!(
                        "[verbose {}: {} lines / {} bytes of tool output]",
                        if self.verbose { "on" } else { "off" },
                        caps.lines,
                        caps.bytes
                    );
                }
                Command::Mode(None) => {
                    println!("[mode {}]", self.runtime.mode());
                }
                Command::Mode(Some(name)) => match name.parse::<Mode>() {
                    Ok(mode) => {
                        self.runtime.set_mode(mode);
                        println!("[mode {mode}: {}]", mode_meaning(mode));
                    }
                    Err(e) => println!("[{e}]"),
                },
                Command::Profile(name) => {
                    let _ = editor.add_history_entry(line.trim());
                    match self.switch_profile(name) {
                        Ok(line) => println!("{line}"),
                        Err(e) => println!("[error: {e}]"),
                    }
                }
                Command::Policy => println!("{}", policy_report(&self.runtime)),
                Command::Memory => match self.runtime.log().read_all() {
                    Ok(events) => println!("{}", memory_report(&self.runtime, &events)),
                    Err(e) => println!("[error: {e}]"),
                },
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

    /// `/profile <name>`: build the provider for `name` from the config
    /// with its key from the environment and swap it in. Budget and
    /// compaction settings stay the session's; the new banner line comes
    /// back.
    fn switch_profile(&mut self, name: &str) -> anyhow::Result<String> {
        let Some(config) = &self.config else {
            anyhow::bail!("no config loaded; /profile needs one");
        };
        let (profile_name, profile) = config.select(Some(name))?;
        let api_key = profile.api_key()?;
        let provider = profile.build_provider(api_key);
        self.runtime
            .set_provider(provider, &profile.model)
            .map_err(|e| anyhow::anyhow!("reloading knowledge: {e}"))?;
        self.profile_name = profile_name.to_owned();
        Ok(banner_line(profile_name, profile, self.runtime.mode()))
    }

    async fn run_skill(&mut self, name: &str, args: &str) {
        let mut at_line_start = true;
        let caps = self.caps();
        let outcome = self
            .runtime
            .invoke_skill(self.user.clone(), name, args, &mut |signal| {
                render(signal, &mut at_line_start, caps)
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
        let caps = self.caps();
        let mut outcome = outcome;
        loop {
            match outcome {
                Ok(o) if o.reason == ASKED_HUMAN => {
                    let mut at_line_start = true;
                    outcome = self
                        .runtime
                        .continue_turn(&mut |signal| render(signal, &mut at_line_start, caps))
                        .await;
                    if !at_line_start {
                        println!();
                    }
                }
                Ok(o) if o.reason != "done" => {
                    return if o.touched.is_empty() {
                        println!("[turn ended: {}]", o.reason)
                    } else {
                        println!("[turn ended: {}; wrote {}]", o.reason, o.touched.join(", "))
                    };
                }
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
        let caps = self.caps();
        let outcome = match text {
            Some(text) => {
                let blocks = vec![ContentBlock::Text(text.to_owned())];
                self.runtime
                    .run_turn(self.user.clone(), blocks, &mut |signal| {
                        render(signal, &mut at_line_start, caps)
                    })
                    .await
            }
            None => {
                self.runtime
                    .continue_turn(&mut |signal| render(signal, &mut at_line_start, caps))
                    .await
            }
        };
        if !at_line_start {
            println!();
        }
        self.settle(outcome).await;
    }
}

/// The first banner line: profile, model, endpoint and the mode unless
/// it is `manual`. Never the key.
pub fn banner_line(profile_name: &str, profile: &Profile, mode: Mode) -> String {
    format!(
        "aigentic · profile {profile_name} · {} · {}{}",
        profile.model,
        profile.endpoint(),
        mode_banner(mode)
    )
}

fn author_name(author: &Author) -> &str {
    match author {
        Author::User(u) => u.0.as_str(),
        Author::Agent(a) => a.0.as_str(),
        Author::System => "system",
    }
}

/// `/policy`: the rule table in order with each rule's name, decision
/// and reason, the bash allow patterns, the mode, and the session grants
/// with who gave them.
pub fn policy_report(runtime: &Runtime) -> String {
    let policy = runtime.policy();
    let mut out = String::from("policy rules, first match wins\n");
    let width = policy
        .rules
        .iter()
        .map(|r| r.name().len())
        .max()
        .unwrap_or(0);
    for (i, rule) in policy.rules.iter().enumerate() {
        let decision = match rule.decision {
            Decision::Allow => "allow",
            Decision::Ask => "ask",
            Decision::Deny => "deny",
        };
        out.push_str(&format!(
            "  {:>2}. {:<width$}  {decision:<5}  {}\n",
            i + 1,
            rule.name(),
            rule.reason
        ));
    }
    out.push_str(&format!(
        "bash allow patterns: {}\n",
        if policy.bash_allow.is_empty() {
            "none".to_owned()
        } else {
            policy.bash_allow.join(", ")
        }
    ));
    out.push_str(&format!(
        "mode {}: {}\n",
        runtime.mode(),
        mode_meaning(runtime.mode())
    ));
    let grants = runtime.session_grants();
    if grants.is_empty() {
        out.push_str("session grants: none");
    } else {
        out.push_str("session grants (this session only)\n");
        for g in grants {
            let what = match &g.command {
                Some(c) => format!("bash {c:?}"),
                None => g.tool.clone(),
            };
            out.push_str(&format!("  {what}  by {}\n", author_name(&g.author)));
        }
    }
    out.trim_end().to_owned()
}

/// `/memory`: the memory files with line counts, the `through_seq` of
/// the last extraction in `events`, then the block as the prefix carries
/// it.
pub fn memory_report(
    runtime: &Runtime,
    events: &[aigentic_runtime::aigentic_core::Event],
) -> String {
    let Some(project) = runtime.layers().project.as_ref() else {
        return "no project: memory needs an aigentic.toml".to_owned();
    };
    let files: Vec<String> = project
        .memory
        .iter()
        .map(|(name, text)| {
            let lines = text.lines().filter(|l| !l.trim().is_empty()).count();
            format!("{name} ({lines} lines)")
        })
        .collect();
    let mut out = format!(
        "memory files in {}: {}\n",
        project.memory_dir().display(),
        if files.is_empty() {
            "none".to_owned()
        } else {
            files.join(", ")
        }
    );
    let last = events
        .iter()
        .rev()
        .find(|e| e.kind == EventKind::MemoryExtracted)
        .and_then(|e| serde_json::from_value::<MemoryExtractedPayload>(e.payload.clone()).ok());
    match last {
        Some(p) => out.push_str(&format!(
            "last extraction: through seq {}, {} lines written by {}\n",
            p.through_seq,
            p.written.len(),
            p.model
        )),
        None => out.push_str("last extraction: none in this thread\n"),
    }
    match project.memory_prefix() {
        Some(block) => {
            out.push_str("as the prefix carries it:\n");
            out.push_str(&block);
        }
        None => out.push_str("nothing in the prefix: every file is empty"),
    }
    out.trim_end().to_owned()
}

/// What the mode does, for `/mode` and the banner.
pub fn mode_meaning(mode: Mode) -> &'static str {
    match mode {
        Mode::Manual => "every ask goes to you",
        Mode::AcceptEdits => "writes run without asking; the shell still asks",
        Mode::Auto => "anything the rules would ask about runs; denials stand",
    }
}

/// The banner's mode note: nothing for `manual`.
pub fn mode_banner(mode: Mode) -> String {
    match mode {
        Mode::Manual => String::new(),
        other => format!(" · mode {other}"),
    }
}

fn render(signal: Signal<'_>, at_line_start: &mut bool, caps: Caps) {
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
                for line in truncate_for_display(&r.content, caps.lines, caps.bytes).lines() {
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
        assert_eq!(parse_line("/verbose", &none), Command::Verbose);
        assert_eq!(parse_line("/mode", &none), Command::Mode(None));
        assert_eq!(
            parse_line("/mode accept-edits", &none),
            Command::Mode(Some("accept-edits"))
        );
        assert_eq!(mode_banner(Mode::Manual), "");
        assert_eq!(mode_banner(Mode::Auto), " · mode auto");
        assert_eq!(
            parse_line("/profile anthropic", &none),
            Command::Profile("anthropic")
        );
        assert_eq!(parse_line("/profile", &none), Command::Unknown("/profile"));
        assert_eq!(parse_line("/policy", &none), Command::Policy);
        assert_eq!(parse_line("/memory", &none), Command::Memory);
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
    fn the_verbose_cap_shows_what_the_configured_cap_cuts() {
        let text: String = (0..20)
            .map(|i| format!("line {i:02} {}\n", "x".repeat(30)))
            .collect();
        let configured = truncate_for_display(&text, 3, 600);
        assert!(configured.starts_with("line 00 "), "{configured}");
        assert!(configured.contains("more bytes)"), "{configured}");
        assert_eq!(
            truncate_for_display(&text, 40, 8000),
            text,
            "the verbose cap shows all of it"
        );
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

    use aigentic_runtime::aigentic_core::{
        AgentId, Capabilities, CompletionRequest, Message, Provider, ProviderEvent,
    };
    use aigentic_runtime::aigentic_log::{NewEvent, ThreadLog};
    use aigentic_runtime::aigentic_tools::{ToolRegistry, Workdir};
    use aigentic_runtime::{Layers, Project};
    use futures_core::Stream;
    use std::pin::Pin;

    struct Silent;
    impl Provider for Silent {
        fn complete(
            &self,
            _: &CompletionRequest<'_>,
        ) -> Pin<Box<dyn Stream<Item = ProviderEvent> + Send + '_>> {
            Box::pin(futures_util::stream::empty())
        }
        fn count_tokens(&self, _: &[Message]) -> u64 {
            1
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

    fn fixture(dir: &std::path::Path) -> Runtime {
        std::fs::write(
            dir.join("aigentic.toml"),
            "[project]\nname = \"p\"\n[policy]\nbash_allow = [\"ls\", \"cargo test\"]\n",
        )
        .unwrap();
        let mem = dir.join(".aigentic/memory");
        std::fs::create_dir_all(&mem).unwrap();
        std::fs::write(
            mem.join("decisions.md"),
            "- Use Swedish.\n- Keep it short.\n",
        )
        .unwrap();
        std::fs::write(mem.join("facts.md"), "\n").unwrap();
        let project = Project::open_root(dir).unwrap();
        let log = ThreadLog::open(dir, ulid::Ulid::generate()).unwrap();
        Runtime::new(
            Box::new(Silent),
            ToolRegistry::builtin(Workdir::new(dir)),
            log,
            AgentId("a".into()),
        )
        .with_policy(project.file.policy())
        .with_layers(Layers::default().with_project(project))
    }

    #[test]
    fn the_policy_report_lists_rules_patterns_mode_and_grants() {
        let dir = tempfile::tempdir().unwrap();
        let mut rt = fixture(dir.path());
        let text = policy_report(&rt);
        assert!(
            text.starts_with("policy rules, first match wins\n   1. class safe"),
            "{text}"
        );
        assert!(text.contains("harness self-management"), "{text}");
        assert!(
            text.contains(&format!(
                "deny   {}",
                aigentic_runtime::aigentic_policy::MEMORY_REASON
            )),
            "{text}"
        );
        assert!(
            text.contains("\nbash allow patterns: ls, cargo test\n"),
            "{text}"
        );
        assert!(
            text.contains("\nmode manual: every ask goes to you\n"),
            "{text}"
        );
        assert!(text.ends_with("session grants: none"), "{text}");
        rt.set_mode(Mode::Auto);
        let text = policy_report(&rt);
        assert!(text.contains("\nmode auto: "), "{text}");
    }

    #[test]
    fn the_memory_report_counts_files_and_names_the_last_extraction() {
        let dir = tempfile::tempdir().unwrap();
        let rt = fixture(dir.path());
        let text = memory_report(&rt, &[]);
        assert!(
            text.starts_with(&format!(
                "memory files in {}: decisions.md (2 lines), facts.md (0 lines)\nlast extraction: none in this thread\nas the prefix carries it:\n# Project memory",
                dir.path().join(".aigentic/memory").display()
            )),
            "{text}"
        );
        assert!(
            text.contains("\n## decisions.md\n\n- Use Swedish.\n- Keep it short."),
            "{text}"
        );
        assert!(
            !text.contains("## facts.md"),
            "an empty file is not in the prefix: {text}"
        );

        let mut log = ThreadLog::open(dir.path(), ulid::Ulid::generate()).unwrap();
        log.append(NewEvent {
            kind: EventKind::MemoryExtracted,
            author: Author::System,
            payload: serde_json::json!({
                "through_seq": 7,
                "written": [{"file": "decisions.md", "text": "- Use Swedish.", "stated_by": {"kind": "user", "id": "steve"}, "at_seq": 3}],
                "model": "m",
                "usage": {"input_tokens": 1, "output_tokens": 1}
            }),
            parent_event: None,
        })
        .unwrap();
        let events = log.read_all().unwrap();
        let text = memory_report(&rt, &events);
        assert!(
            text.contains("last extraction: through seq 7, 1 lines written by m\n"),
            "{text}"
        );

        let bare = Runtime::new(
            Box::new(Silent),
            ToolRegistry::empty(),
            ThreadLog::open(dir.path(), ulid::Ulid::generate()).unwrap(),
            AgentId("a".into()),
        );
        assert!(memory_report(&bare, &[]).starts_with("no project"));
    }
}
