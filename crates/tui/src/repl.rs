use std::io::Write;
use std::path::PathBuf;

use aigentic_runtime::aigentic_core::{Author, ContentBlock, EventKind, ToolCall};
use aigentic_runtime::aigentic_log::{CompactedPayload, CompactionStrategy, ToolResultPayload};
use aigentic_runtime::{Resumed, Runtime, Signal};
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;

use crate::cost::cost_of;

/// Lines of tool output shown before truncating.
const RESULT_LINES: usize = 12;
/// Bytes of tool output shown before truncating.
const RESULT_BYTES: usize = 1200;

pub struct Repl {
    runtime: Runtime,
    user: Author,
    history: PathBuf,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Command<'a> {
    Cost,
    Quit,
    Pin(&'a str),
    Compact,
    Unknown(&'a str),
    Chat(&'a str),
    Empty,
}

pub fn parse_line(line: &str) -> Command<'_> {
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
        ("compact", _) => Command::Compact,
        ("pin", text) if !text.is_empty() => Command::Pin(text),
        _ => Command::Unknown(trimmed),
    }
}

impl Repl {
    pub fn new(runtime: Runtime, user: Author, history: PathBuf) -> Self {
        Self {
            runtime,
            user,
            history,
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
            match parse_line(&line) {
                Command::Empty => continue,
                Command::Quit => break,
                Command::Cost => {
                    let events = self.runtime.log().read_all()?;
                    println!("{}", cost_of(&events));
                }
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
        match outcome {
            Ok(o) if o.reason != "done" => println!("[turn ended: {}]", o.reason),
            Ok(_) => {}
            Err(e) => println!("[error: {e}]"),
        }
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
            if let Ok(ToolResultPayload(r)) = serde_json::from_value(event.payload.clone()) {
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
        assert_eq!(parse_line("/cost"), Command::Cost);
        assert_eq!(parse_line("  /quit  "), Command::Quit);
        assert_eq!(parse_line("/exit"), Command::Quit);
        assert_eq!(parse_line("/compact"), Command::Compact);
        assert_eq!(
            parse_line("/pin  Answer in Swedish. "),
            Command::Pin("Answer in Swedish.")
        );
        assert_eq!(parse_line("/pin"), Command::Unknown("/pin"));
        assert_eq!(parse_line("/nope arg"), Command::Unknown("/nope arg"));
        assert_eq!(parse_line("hello /cost"), Command::Chat("hello /cost"));
        assert_eq!(parse_line("   "), Command::Empty);
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
