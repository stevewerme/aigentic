//! Cells (plan section 5): what the transcript is made of. A cell is
//! rendered twice, styled for the shell and plain for a pipe, and once
//! more at full length for the pager. Tool cells copy Codex's shape:
//! a bullet, the call, the output under `└` in dim, head and tail with
//! `… +N lines`. Consecutive reads fold into one `Explored` cell.

use aigentic_runtime::aigentic_core::ToolCall;
use aigentic_runtime::harness_tools::{Task, TaskState};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::app::{diff, markdown};

/// Head and tail rows a tool result shows before `… +N lines`.
pub const RESULT_HEAD: usize = 3;
pub const RESULT_TAIL: usize = 2;
/// How many characters of a call's arguments the bullet line shows.
const ARGS_WIDTH: usize = 120;
/// Diff lines an edit cell previews (hunk lines, headers skipped).
pub const EDIT_PREVIEW: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolState {
    Running,
    Ok,
    Err,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cell {
    /// The person's own message, `> ` prefixed.
    User(String),
    /// One assistant line, light markdown; `fenced` is whether it sits
    /// inside a code fence (the shell tracks that across lines).
    Assistant { text: String, fenced: bool },
    Tool {
        name: String,
        /// The row's text: the one value that names what the call is
        /// about — for `bash`, the command's main segment.
        summary: String,
        /// The whole command when the row shows less than all of it
        /// (a `bash` chain): the pager's text.
        full: Option<String>,
        state: ToolState,
        /// The result, whole; rendering cuts it.
        output: String,
    },
    /// Folded reads: the tool name and what it looked at, in order.
    Explored(Vec<String>),
    /// An edit_file or write_file result: the diff, previewed at a few
    /// lines, whole in the pager.
    Edit(diff::Edit),
    /// A `[bracketed]` notice, a report line, anything else.
    Note(String),
    /// A turn's figures when it ends, dim.
    Summary(String),
    /// A checklist item finished: checked off into the scrollback, dim.
    Done(String),
}

/// The checklist's lines: a head with the count, then one line a step.
/// The transcript pager shows it; the live block shows [`task_compact`].
pub fn task_lines(tasks: &[Task]) -> Vec<Line<'static>> {
    let done = tasks.iter().filter(|t| t.state == TaskState::Done).count();
    let mut lines = vec![Line::from(vec![
        Span::styled("• ", Style::default().fg(Color::Cyan)),
        Span::styled("Tasks", Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(
            format!(" {done}/{}", tasks.len()),
            Style::default().add_modifier(Modifier::DIM),
        ),
    ])];
    for t in tasks {
        let (mark, style) = match t.state {
            TaskState::Done => ("✓", Style::default().add_modifier(Modifier::DIM)),
            TaskState::Active => ("▸", Style::default().add_modifier(Modifier::BOLD)),
            TaskState::Pending => ("○", Style::default()),
        };
        lines.push(Line::from(Span::styled(
            format!("  {mark} {}", t.text),
            style,
        )));
    }
    lines
}

/// The live block's task rows, at most three (issue #21): the item
/// now in hand, the one after it, and how far the list has come. What
/// is finished reaches the scrollback one line at a time
/// ([`Cell::Done`]); the whole list is a pager away.
pub fn task_compact(tasks: &[Task]) -> Vec<Line<'static>> {
    if tasks.is_empty() {
        return Vec::new();
    }
    let dim = Style::default().add_modifier(Modifier::DIM);
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let done = tasks.iter().filter(|t| t.state == TaskState::Done).count();
    let mut lines: Vec<Line<'static>> = tasks
        .iter()
        .filter(|t| t.state != TaskState::Done)
        .take(2)
        .map(|t| {
            let style = if t.state == TaskState::Active {
                bold
            } else {
                dim
            };
            Line::from(Span::styled(
                format!(
                    "{} {}",
                    if t.state == TaskState::Active {
                        "▸"
                    } else {
                        "○"
                    },
                    t.text
                ),
                style,
            ))
        })
        .collect();
    lines.push(Line::from(Span::styled(
        format!("{done}/{} done · ctrl-t for the list", tasks.len()),
        dim,
    )));
    lines
}

/// Tools whose consecutive calls fold into `Explored`.
pub fn is_read_tool(name: &str) -> bool {
    matches!(
        name,
        "read_file" | "list_dir" | "grep" | "search_knowledge" | "glob"
    )
}

/// The bullet line's argument text: the one value that names what the
/// call is about when there is one, else the JSON. A `bash` line
/// shows its main segment (issue #21) — the row says what the line
/// is, the pager keeps the chain.
pub fn summarise_args(call: &ToolCall) -> String {
    let key = match call.name.as_str() {
        "bash" => Some("command"),
        "read_file" | "write_file" | "edit_file" | "list_dir" => Some("path"),
        "grep" | "search_knowledge" => Some("pattern"),
        _ => None,
    };
    let text = key
        .and_then(|k| call.args.get(k))
        .and_then(|v| v.as_str())
        .map(|raw| {
            if call.name == "bash" {
                aigentic_runtime::aigentic_policy::main_segment(raw)
            } else {
                raw.to_owned()
            }
        })
        .unwrap_or_else(|| call.args.to_string());
    let one_line = text.replace('\n', " ");
    if one_line.chars().count() > ARGS_WIDTH {
        let cut: String = one_line.chars().take(ARGS_WIDTH).collect();
        format!("{cut}…")
    } else {
        one_line
    }
}

/// A `bash` call's whole command, for the pager; `None` for every
/// other tool, whose one value is already the whole of it.
pub fn full_command(call: &ToolCall) -> Option<String> {
    if call.name != "bash" {
        return None;
    }
    let full = call
        .args
        .get("command")
        .and_then(|v| v.as_str())?
        .replace('\n', " ")
        .trim_end()
        .to_owned();
    (full != summarise_args(call)).then_some(full)
}

impl Cell {
    /// Plain lines, for a pipe and for tests.
    pub fn plain(&self) -> Vec<String> {
        if let Cell::Assistant { text, .. } = self {
            return vec![text.clone()];
        }
        self.styled(usize::MAX)
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    /// Styled lines for the transcript; tool output cut to head and
    /// tail. `width` is only used to keep long bullet lines sane.
    pub fn styled(&self, _width: usize) -> Vec<Line<'static>> {
        match self {
            Cell::User(text) => text
                .lines()
                .enumerate()
                .map(|(i, l)| {
                    Line::from(vec![
                        Span::styled(
                            if i == 0 { "> " } else { "  " },
                            Style::default().fg(Color::Cyan),
                        ),
                        Span::styled(l.to_owned(), Style::default().add_modifier(Modifier::BOLD)),
                    ])
                })
                .collect(),
            Cell::Assistant { text, fenced } => vec![markdown::line(text, *fenced)],
            Cell::Tool {
                name,
                summary,
                full: _,
                state,
                output,
            } => {
                let mut lines = vec![tool_head(name, summary, state)];
                let rows: Vec<&str> = output.lines().collect();
                let dim = Style::default().add_modifier(Modifier::DIM);
                if rows.len() <= RESULT_HEAD + RESULT_TAIL {
                    for r in &rows {
                        lines.push(Line::from(Span::styled(format!("  └ {r}"), dim)));
                    }
                } else {
                    for r in &rows[..RESULT_HEAD] {
                        lines.push(Line::from(Span::styled(format!("  └ {r}"), dim)));
                    }
                    lines.push(Line::from(Span::styled(
                        format!(
                            "  … +{} lines (ctrl-t for all)",
                            rows.len() - RESULT_HEAD - RESULT_TAIL
                        ),
                        dim,
                    )));
                    for r in &rows[rows.len() - RESULT_TAIL..] {
                        lines.push(Line::from(Span::styled(format!("  └ {r}"), dim)));
                    }
                }
                lines
            }
            Cell::Explored(entries) => {
                let mut lines = vec![Line::from(vec![
                    Span::styled("• ", Style::default().fg(Color::Green)),
                    Span::styled("Explored", Style::default().add_modifier(Modifier::BOLD)),
                ])];
                let dim = Style::default().add_modifier(Modifier::DIM);
                for e in entries {
                    lines.push(Line::from(Span::styled(format!("  └ {e}"), dim)));
                }
                lines
            }
            Cell::Edit(edit) => {
                let mut lines = vec![edit_head(edit)];
                let body: Vec<&str> = hunk_lines(&edit.diff);
                for l in body.iter().take(EDIT_PREVIEW) {
                    lines.push(diff::line(l));
                }
                if body.len() > EDIT_PREVIEW {
                    lines.push(Line::from(Span::styled(
                        format!("  … +{} lines (ctrl-t for all)", body.len() - EDIT_PREVIEW),
                        Style::default().add_modifier(Modifier::DIM),
                    )));
                }
                lines
            }
            Cell::Done(text) => vec![Line::from(vec![
                Span::styled("✓ ", Style::default().add_modifier(Modifier::DIM)),
                Span::raw(text.clone()),
            ])],
            Cell::Summary(text) => vec![Line::from(Span::styled(
                text.clone(),
                Style::default().add_modifier(Modifier::DIM),
            ))],
            Cell::Note(text) => text
                .lines()
                .map(|l| {
                    Line::from(Span::styled(
                        l.to_owned(),
                        Style::default().fg(Color::Yellow),
                    ))
                })
                .collect(),
        }
    }

    /// Every line, nothing cut: the pager's view.
    pub fn full(&self) -> Vec<Line<'static>> {
        match self {
            Cell::Tool {
                name,
                summary,
                full,
                state,
                output,
            } => {
                let mut lines = vec![tool_head(name, full.as_deref().unwrap_or(summary), state)];
                let dim = Style::default().add_modifier(Modifier::DIM);
                for r in output.lines() {
                    lines.push(Line::from(Span::styled(format!("  └ {r}"), dim)));
                }
                lines
            }
            Cell::Edit(edit) => {
                let mut lines = vec![edit_head(edit)];
                lines.extend(diff::lines(&edit.diff));
                lines
            }
            other => other.styled(usize::MAX),
        }
    }
}

/// The diff's hunk lines: everything after the `+++` header.
fn hunk_lines(diff: &str) -> Vec<&str> {
    diff.lines()
        .skip_while(|l| l.starts_with("--- ") || l.starts_with("+++ "))
        .collect()
}

fn edit_head(edit: &diff::Edit) -> Line<'static> {
    Line::from(vec![
        Span::styled("• ", Style::default().fg(Color::Green)),
        Span::styled("Edited ", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(edit.path.clone()),
        Span::styled(
            format!(" (+{} −{})", edit.added, edit.removed),
            Style::default().add_modifier(Modifier::DIM),
        ),
    ])
}

fn tool_head(name: &str, summary: &str, state: &ToolState) -> Line<'static> {
    let (bullet, colour) = match state {
        ToolState::Running => ("◦ ", Color::Yellow),
        ToolState::Ok => ("• ", Color::Green),
        ToolState::Err => ("• ", Color::Red),
    };
    let verb = match state {
        ToolState::Running => "Running",
        ToolState::Ok => "Ran",
        ToolState::Err => "Failed",
    };
    Line::from(vec![
        Span::styled(bullet, Style::default().fg(colour)),
        Span::styled(
            format!("{verb} {name} "),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(summary.to_owned()),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "c".into(),
            name: name.into(),
            args,
        }
    }

    /// What a line says with every style ignored, as the tests read it.
    fn plain_line(l: &Line<'_>) -> String {
        l.spans.iter().map(|s| s.content.to_string()).collect()
    }

    #[test]
    fn args_summarise_to_the_one_value_that_matters() {
        assert_eq!(
            summarise_args(&call("bash", json!({"command": "cargo test\n"}))),
            "cargo test"
        );
        assert_eq!(
            summarise_args(&call("read_file", json!({"path": "src/x.rs"}))),
            "src/x.rs"
        );
        assert_eq!(summarise_args(&call("other", json!({"a": 1}))), "{\"a\":1}");
        let long = "x".repeat(ARGS_WIDTH + 10);
        assert!(summarise_args(&call("bash", json!({"command": long}))).ends_with('…'));
    }

    /// The issue's own chain: the row says the main segment, the
    /// pager the whole command (issue #21).
    #[test]
    fn a_bash_chain_says_its_main_segment_and_the_pager_the_whole() {
        let command =
            "cargo test -p aigentic-server 2>&1 | grep 'test result' | head; echo SERVER-DONE";
        let cell = call("bash", json!({"command": command}));
        assert_eq!(summarise_args(&cell), "cargo test -p aigentic-server");
        assert_eq!(full_command(&cell).as_deref(), Some(command));
        // The pager's first row is the whole command, the row's the
        // segment.
        let cell = Cell::Tool {
            name: "bash".into(),
            summary: summarise_args(&cell),
            full: full_command(&cell),
            state: ToolState::Ok,
            output: "ok".into(),
        };
        let full = cell.full();
        assert_eq!(plain_line(&full[0]), format!("• Ran bash {command}"));
        assert_eq!(cell.plain()[0], "• Ran bash cargo test -p aigentic-server");
        // `2>&1` the row leaves off is still the pager's to show; a
        // command the row already says in full has nothing extra.
        let plain = call("bash", json!({"command": "cargo test 2>&1"}));
        assert_eq!(summarise_args(&plain), "cargo test");
        assert_eq!(full_command(&plain).as_deref(), Some("cargo test 2>&1"));
        assert_eq!(
            full_command(&call("bash", json!({"command": "cargo test"}))),
            None
        );
        // Another tool's one value is the whole of it.
        assert_eq!(full_command(&call("read_file", json!({"path": "a"}))), None);
        // Setup defers to the work it sets up (the pty dump's case:
        // the row named the `cd`, not the test after it).
        assert_eq!(
            summarise_args(&call(
                "bash",
                json!({"command": "cd ~/Projects/aigentic && cargo test --workspace 2>&1"})
            )),
            "cargo test --workspace"
        );
    }

    #[test]
    fn tool_output_shows_head_and_tail() {
        let output = (1..=9)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let cell = Cell::Tool {
            name: "bash".into(),
            summary: "ls".into(),
            full: None,
            state: ToolState::Ok,
            output,
        };
        let plain = cell.plain();
        assert_eq!(plain[0], "• Ran bash ls");
        assert_eq!(plain[1], "  └ line 1");
        assert_eq!(plain[3], "  └ line 3");
        assert_eq!(plain[4], "  … +4 lines (ctrl-t for all)");
        assert_eq!(plain[5], "  └ line 8");
        assert_eq!(plain[6], "  └ line 9");
        assert_eq!(plain.len(), 7);
        assert_eq!(cell.full().len(), 10);
        let short = Cell::Tool {
            name: "bash".into(),
            summary: "x".into(),
            full: None,
            state: ToolState::Err,
            output: "a\nb".into(),
        };
        assert_eq!(short.plain(), vec!["• Failed bash x", "  └ a", "  └ b"]);
    }

    #[test]
    fn an_edit_cell_previews_then_shows_all() {
        let edit = diff::parse_edit_result(
            "--- a/f.rs\n+++ b/f.rs\n@@ -1,4 +1,4 @@\n a\n-b\n+B\n c\n d\nedited f.rs at line 2",
        )
        .unwrap();
        let cell = Cell::Edit(edit);
        let plain = cell.plain();
        assert_eq!(plain[0], "• Edited f.rs (+1 −1)");
        assert_eq!(plain[1], "@@ -1,4 +1,4 @@");
        assert_eq!(plain[3], "-b");
        assert_eq!(plain[4], "  … +3 lines (ctrl-t for all)");
        assert_eq!(cell.full().len(), 9);
    }

    #[test]
    fn tasks_render_with_marks_and_a_count() {
        let tasks = vec![
            Task {
                text: "read the plan".into(),
                state: TaskState::Done,
            },
            Task {
                text: "write the code".into(),
                state: TaskState::Active,
            },
            Task {
                text: "run the gate".into(),
                state: TaskState::Pending,
            },
        ];
        assert_eq!(
            task_lines(&tasks)
                .iter()
                .map(|l| plain_line(l))
                .collect::<Vec<_>>(),
            vec![
                "• Tasks 1/3",
                "  ✓ read the plan",
                "  ▸ write the code",
                "  ○ run the gate"
            ]
        );
        // The live block stays at three rows whatever the list does:
        // the item in hand, the one after it, how far it has come.
        assert_eq!(
            task_compact(&tasks)
                .iter()
                .map(|l| plain_line(l))
                .collect::<Vec<_>>(),
            vec![
                "▸ write the code",
                "○ run the gate",
                "1/3 done · ctrl-t for the list"
            ]
        );
    }

    #[test]
    fn the_compact_task_block_never_exceeds_three_rows() {
        let many: Vec<Task> = (0..9)
            .map(|i| Task {
                text: format!("task {i}"),
                state: if i < 3 {
                    TaskState::Done
                } else if i == 3 {
                    TaskState::Active
                } else {
                    TaskState::Pending
                },
            })
            .collect();
        let rows = task_compact(&many);
        assert_eq!(rows.len(), 3);
        let plain: Vec<_> = rows.iter().map(|l| plain_line(l)).collect();
        assert_eq!(plain[0], "▸ task 3");
        assert_eq!(plain[1], "○ task 4");
        assert_eq!(plain[2], "3/9 done · ctrl-t for the list");
        assert!(task_compact(&[]).is_empty());
    }

    #[test]
    fn a_finished_task_checks_off_into_the_scrollback() {
        let cell = Cell::Done("read the plan".into());
        assert_eq!(cell.plain(), vec!["✓ read the plan"]);
    }

    #[test]
    fn explored_and_user_render() {
        let e = Cell::Explored(vec!["read_file a.rs".into(), "grep foo".into()]);
        assert_eq!(
            e.plain(),
            vec!["• Explored", "  └ read_file a.rs", "  └ grep foo"]
        );
        let u = Cell::User("one\ntwo".into());
        assert_eq!(u.plain(), vec!["> one", "  two"]);
        assert!(is_read_tool("grep"));
        assert!(!is_read_tool("bash"));
    }
}
