//! The client's pure parts: the slash-command grammar and the display
//! truncation. The loop itself is `app/engine.rs`.

#[derive(Debug, PartialEq, Eq)]
pub enum Command<'a> {
    Cost,
    Quit,
    Help,
    Skills,
    Project,
    Threads,
    Pin(&'a str),
    /// File a memory line directly: `/remember <text>`; an optional
    /// first word (decision, constraint, fact) picks the file.
    Remember(&'a str),
    Compact,
    /// Post as an interrupt: the running turn ends first (`!text` too).
    Interrupt(&'a str),
    /// The participants and their roles, and who you are.
    Who,
    /// What the thread is doing and what is queued.
    Queue,
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
    /// The key table.
    Keys,
    /// The project's working-tree diff, in the pager.
    Diff,
    /// Set the thread's title.
    Rename(&'a str),
    /// Move the thread to another project.
    ProjectUse(&'a str),
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
        ("keys", _) => Command::Keys,
        ("diff", _) => Command::Diff,
        ("rename", title) if !title.is_empty() => Command::Rename(title),
        ("skills", _) => Command::Skills,
        ("project", rest) if rest.starts_with("use ") && !rest[4..].trim().is_empty() => {
            Command::ProjectUse(rest[4..].trim())
        }
        ("project", _) => Command::Project,
        ("threads", _) => Command::Threads,
        ("compact", _) => Command::Compact,
        ("interrupt", text) if !text.is_empty() => Command::Interrupt(text),
        ("who", _) => Command::Who,
        ("queue", _) => Command::Queue,
        ("mode", "") => Command::Mode(None),
        ("mode", name) => Command::Mode(Some(name)),
        ("profile", name) if !name.is_empty() => Command::Profile(name),
        ("policy", _) => Command::Policy,
        ("memory", _) => Command::Memory,
        ("pin", text) if !text.is_empty() => Command::Pin(text),
        ("remember", text) if !text.is_empty() => Command::Remember(text),
        (name, args) if skills.iter().any(|s| s == name) => Command::Skill(name, args),
        _ => Command::Unknown(trimmed),
    }
}

/// Every built-in command and a description, for `/` completion.
pub const COMMANDS: &[(&str, &str)] = &[
    (
        "cost",
        "tokens for the thread, reported and estimated separately",
    ),
    ("pin", "pin a fact to the stable prefix: /pin <text>"),
    (
        "remember",
        "file a memory line: /remember [decision|constraint|fact] <text>",
    ),
    ("compact", "run compaction now"),
    (
        "mode",
        "show the permission mode, or set it: manual, accept-edits, auto",
    ),
    (
        "interrupt",
        "end the running turn and start one with this text",
    ),
    ("who", "the participants and their roles"),
    ("queue", "what the thread is doing and what is queued"),
    (
        "policy",
        "the rule table, the bash allow patterns, the mode and the grants",
    ),
    ("memory", "the memory files as the prefix carries them"),
    (
        "skills",
        "list enabled skills; user-invoked ones are slash commands",
    ),
    (
        "project",
        "the layers, the knowledge mode and every tool's fate",
    ),
    ("threads", "this project's threads, newest first"),
    ("diff", "the project's working-tree diff, in the pager"),
    ("rename", "set the thread's title: /rename <title>"),
    ("keys", "the key table: interrupt, recall, quit"),
    ("help", "the command list"),
    ("quit", "exit (Ctrl-D too)"),
];

pub const HELP: &str = "\
/cost            tokens for the thread, reported and estimated separately
/pin <text>      pin a fact to the stable prefix
/remember <text> file a memory line; an optional first word picks the file: decision, constraint, fact
/compact         run compaction now
/mode [name]     show the permission mode, or set it: manual, accept-edits, auto
/interrupt <text> end the running turn and start one with this (or `!text`)
/who             the participants and their roles
/queue           what the thread is doing and what is queued
/policy          the rule table, the bash allow patterns, the mode and the session grants
/memory          the memory files as the prefix carries them, and the last extraction
/skills          list enabled skills; user-invoked ones are slash commands
/project         the layers, the knowledge mode and every tool's fate
/project use <n> move this thread to project <n>, keeping the conversation
/threads         this project's threads, newest first
/<skill> [args]  run a user-invoked skill
/diff            the project's working-tree diff, untracked files included
/rename <title>  set the thread's title (one is proposed after the first turn)
/keys            the key table: interrupt, recall, quit
/help            this list
/quit            exit (Ctrl-D too)
Permission prompts (approve role) are a menu: Up/Down or a digit picks, Enter confirms; Esc answers with a reason; y/a/n still work. A question with options is the same menu; free text answers too.";

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
        assert_eq!(parse_line("/keys", &none), Command::Keys);
        assert_eq!(
            parse_line("/interrupt stop now", &none),
            Command::Interrupt("stop now")
        );
        assert_eq!(parse_line("/who", &none), Command::Who);
        assert_eq!(parse_line("/queue", &none), Command::Queue);
        assert_eq!(parse_line("/mode", &none), Command::Mode(None));
        assert_eq!(
            parse_line("/mode accept-edits", &none),
            Command::Mode(Some("accept-edits"))
        );
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
            parse_line("/remember decision We deploy from main only.", &none),
            Command::Remember("decision We deploy from main only.")
        );
        assert_eq!(
            parse_line("  /remember   dates in Swedish format  ", &none),
            Command::Remember("dates in Swedish format")
        );
        assert_eq!(
            parse_line("/remember", &none),
            Command::Unknown("/remember")
        );
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
    fn a_larger_cap_shows_what_a_small_one_cuts() {
        let text: String = (0..20)
            .map(|i| format!("line {i:02} {}\n", "x".repeat(30)))
            .collect();
        let configured = truncate_for_display(&text, 3, 600);
        assert!(configured.starts_with("line 00 "), "{configured}");
        assert!(configured.contains("more bytes)"), "{configured}");
        assert_eq!(
            truncate_for_display(&text, 40, 8000),
            text,
            "the larger cap shows all of it"
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
}
