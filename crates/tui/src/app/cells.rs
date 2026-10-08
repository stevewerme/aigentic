//! Cells (plan section 5): what the transcript is made of. A cell is
//! rendered twice, styled for the shell and plain for a pipe, and once
//! more at full length for the pager. Tool cells copy Codex's shape:
//! a bullet, the call, the output under `└` in dim, head and tail with
//! `… +N lines`. Consecutive reads fold into one `Explored` cell.

use aigentic_runtime::aigentic_core::ToolCall;
use aigentic_runtime::aigentic_log::Usage;
use aigentic_runtime::harness_tools::{Task, TaskState};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use std::time::Duration;

use crate::app::{diff, markdown, status};
use crate::stats;

/// Head and tail rows a tool result shows before `… +N lines`.
pub const RESULT_HEAD: usize = 3;
pub const RESULT_TAIL: usize = 2;
/// How many characters of a call's arguments the bullet line shows.
const ARGS_WIDTH: usize = 120;
/// Diff lines an edit cell previews (hunk lines, headers skipped).
pub const EDIT_PREVIEW: usize = 3;
/// Milliseconds as `1.2s`. One vocabulary for a call's latency, its
/// first token and a tool's run time (issue #115).
pub fn secs_ms(ms: u64) -> String {
    format!("{:.1}s", ms as f64 / 1000.0)
}

/// A size in bytes: `512 B`, `3.4 KB`, `128 KB`, `1.3 MB`. Decimal
/// thousands, the vocabulary [`status::count_short`] uses, so a result
/// size and a token count are read the same way.
pub fn bytes_short(n: usize) -> String {
    if n < 1000 {
        format!("{n} B")
    } else if n < 10_000 {
        format!("{:.1} KB", n as f64 / 1000.0)
    } else if n < 1_000_000 {
        format!("{} KB", n / 1000)
    } else {
        format!("{:.1} MB", n as f64 / 1_000_000.0)
    }
}

/// What the log holds about one tool call, for the developer view
/// (issue #115). Built from the events themselves, never estimated:
/// a part the log does not hold is `None`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ToolDetail {
    /// The result's `created_at` minus that of the assistant message
    /// its `parent_event` names. For a batch run in parallel the parent
    /// is the message that carried the batch, so this is "done after"
    /// it, not each call's own run time.
    pub took: Option<Duration>,
    /// The result content's length.
    pub bytes: usize,
    /// Who or what allowed the call, already worded for the reader:
    /// `rule read_only`, `denied: rule …`, `you allowed`, `you denied`.
    pub policy: Option<String>,
}

impl ToolDetail {
    /// ` · 1.2s · 3.4 KB · rule read_only`: the dim tail a head line
    /// carries under the developer view, empty when the log held
    /// nothing.
    pub fn tail(&self) -> String {
        let mut parts = Vec::new();
        if let Some(took) = self.took {
            parts.push(secs_ms(took.as_millis() as u64));
        }
        parts.push(bytes_short(self.bytes));
        if let Some(policy) = &self.policy {
            parts.push(policy.clone());
        }
        format!(" · {}", parts.join(" · "))
    }
}

/// One model call's line (issue #115): which call it was, what ran it,
/// what it read and wrote, how long it took and what it cost. Every
/// part appears only when the log holds it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CallLine {
    /// The call's 1-based ordinal in its turn.
    pub n: u32,
    /// The profile that ran the call, set only when it differs from the
    /// one this client is attached to (a title or memory call). The
    /// attached profile stays unnamed.
    pub profile: Option<String>,
    /// The model that ran the call.
    pub model: Option<String>,
    /// Prompt tokens: input plus both cache counts, the way the turn
    /// summary counts them.
    pub prompt_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: Option<u64>,
    /// Read cache as a share of the prompt, `None` when the log held
    /// no cache fields — the rule the turn summary uses.
    pub cache_pct: Option<u32>,
    pub latency_ms: Option<u64>,
    pub ttft_ms: Option<u64>,
    pub cost_usd: Option<f64>,
    pub estimated: bool,
}

impl CallLine {
    /// The line for one call's usage. `attached` is the profile this
    /// client is attached to; a call on any other profile is named.
    pub fn from_usage(n: u32, u: &Usage, attached: Option<&str>) -> Self {
        let prompt = u.input_tokens + u.cache_read_tokens + u.cache_write_tokens;
        let cache_seen = u.cache_read_tokens > 0 || u.cache_write_tokens > 0;
        let cache_pct = (prompt > 0 && cache_seen)
            .then(|| (100.0 * u.cache_read_tokens as f64 / prompt as f64).round() as u32);
        let profile = match (u.profile.as_deref(), attached) {
            (Some(p), Some(a)) if p == a => None,
            (Some(p), _) => Some(p.to_owned()),
            (None, _) => None,
        };
        Self {
            n,
            profile,
            model: u.model.clone(),
            prompt_tokens: prompt,
            output_tokens: u.output_tokens,
            reasoning_tokens: u.reasoning_tokens,
            cache_pct,
            latency_ms: u.latency_ms,
            ttft_ms: u.ttft_ms,
            cost_usd: u.cost_usd,
            estimated: u.estimated,
        }
    }

    /// The line after `call N`: the model, tokens, cache, time and
    /// dollars, each part only when it has something to say.
    pub fn body(&self) -> String {
        let mut parts = Vec::new();
        if let Some(profile) = &self.profile {
            parts.push(profile.clone());
        }
        if let Some(model) = &self.model {
            parts.push(model.clone());
        }
        parts.push(format!("{} in", status::count_short(self.prompt_tokens)));
        if let Some(pct) = self.cache_pct {
            parts.push(format!("{pct}% cached"));
        }
        parts.push(format!("{} out", self.output_tokens));
        if let Some(reasoning) = self.reasoning_tokens.filter(|r| *r > 0) {
            parts.push(format!("{reasoning} reasoning"));
        }
        if let Some(ms) = self.latency_ms {
            let mut took = secs_ms(ms);
            if let Some(ttft) = self.ttft_ms {
                took.push_str(&format!(" (first token {})", secs_ms(ttft)));
            }
            parts.push(took);
        }
        match (self.cost_usd, self.estimated) {
            (Some(usd), false) => parts.push(stats::money(Some(usd), None)),
            (Some(usd), true) => parts.push(stats::money(None, Some(usd))),
            (None, _) => parts.push("unpriced".into()),
        }
        parts.join(" · ")
    }

    /// The whole line: `◦ call 3 · …`.
    pub fn text(&self) -> String {
        format!("◦ call {} · {}", self.n, self.body())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolState {
    Running,
    Ok,
    Err,
}

#[derive(Debug, Clone, PartialEq)]
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
        /// What the log held about the call (issue #115): the run time,
        /// the result's size and the policy that allowed it. `None`
        /// when the engine had nothing to read.
        detail: Option<ToolDetail>,
    },
    /// Folded reads: the tool name and what it looked at, in order.
    Explored(Vec<ExploredRow>),
    /// An edit_file or write_file result: the diff, previewed at a few
    /// lines, whole in the pager; the same call detail as a `Tool`.
    Edit {
        edit: diff::Edit,
        detail: Option<ToolDetail>,
    },
    /// One model call's line (issue #115), dim. Developer view only.
    Call(CallLine),
    /// A checklist step the calls under it served (issue #115):
    /// `▸ 2/5 Read key code regions`. Developer view only.
    Step {
        index: usize,
        total: usize,
        text: String,
    },
    /// What the harness did on its own (issue #115): a retry, a sweep,
    /// a memory write, a decision. Dim, `·` led, so it never reads as
    /// the model's words. Developer view only.
    System(String),
    /// A `[bracketed]` notice, a report line, anything else.
    Note(String),
    /// A turn's figures when it ends, dim.
    Summary(String),
    /// A checklist item finished: checked off into the scrollback, dim.
    Done(String),
    /// A lead's line, from the shared run view (issue #68): dim, each
    /// line `▸ ` prefixed. The text may hold several lines.
    Run(String),
}

/// One folded read: what it looked at, and what the log held about the
/// call (issue #115). A row built from a bare string is a fixture's or
/// an old caller's, with no detail.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExploredRow {
    /// `read_file src/x.rs`.
    pub line: String,
    pub detail: Option<ToolDetail>,
}

impl From<String> for ExploredRow {
    fn from(line: String) -> Self {
        Self { line, detail: None }
    }
}

impl From<&str> for ExploredRow {
    fn from(line: &str) -> Self {
        Self::from(line.to_owned())
    }
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
                detail: _,
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
                    lines.push(Line::from(Span::styled(format!("  └ {}", e.line), dim)));
                }
                lines
            }
            Cell::Edit { edit, detail: _ } => {
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
            Cell::Call(line) => vec![Line::from(Span::styled(
                line.text(),
                Style::default().add_modifier(Modifier::DIM),
            ))],
            Cell::Step { index, total, text } => vec![Line::from(vec![
                Span::styled("▸ ", Style::default().add_modifier(Modifier::DIM)),
                Span::styled(
                    format!("{index}/{total} "),
                    Style::default().add_modifier(Modifier::DIM),
                ),
                Span::styled(text.clone(), Style::default().add_modifier(Modifier::BOLD)),
            ])],
            Cell::System(text) => text
                .lines()
                .map(|l| {
                    Line::from(Span::styled(
                        format!("· {l}"),
                        Style::default().add_modifier(Modifier::DIM),
                    ))
                })
                .collect(),
            Cell::Done(text) => vec![Line::from(vec![
                Span::styled("✓ ", Style::default().add_modifier(Modifier::DIM)),
                Span::raw(text.clone()),
            ])],
            Cell::Summary(text) => vec![Line::from(Span::styled(
                text.clone(),
                Style::default().add_modifier(Modifier::DIM),
            ))],
            Cell::Run(text) => text
                .lines()
                .map(|l| {
                    Line::from(Span::styled(
                        format!("▸ {l}"),
                        Style::default().add_modifier(Modifier::DIM),
                    ))
                })
                .collect(),
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
                detail,
            } => {
                let head = head_with_detail(
                    tool_head(name, full.as_deref().unwrap_or(summary), state),
                    detail.as_ref(),
                );
                let mut lines = vec![head];
                let dim = Style::default().add_modifier(Modifier::DIM);
                for r in output.lines() {
                    lines.push(Line::from(Span::styled(format!("  └ {r}"), dim)));
                }
                lines
            }
            Cell::Edit { edit, detail } => {
                let mut lines = vec![head_with_detail(edit_head(edit), detail.as_ref())];
                lines.extend(diff::lines(&edit.diff));
                lines
            }
            other => other.styled(usize::MAX),
        }
    }
}

/// The head line with the call's detail appended, dim (issue #115):
/// the pager and the developer view both read it this way, so a tool's
/// run time, size and policy sit on its own row.
pub(crate) fn head_with_detail(
    mut head: Line<'static>,
    detail: Option<&ToolDetail>,
) -> Line<'static> {
    if let Some(detail) = detail {
        head.spans.push(Span::styled(
            detail.tail(),
            Style::default().add_modifier(Modifier::DIM),
        ));
    }
    head
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
            detail: None,
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
            detail: None,
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
            detail: None,
        };
        assert_eq!(short.plain(), vec!["• Failed bash x", "  └ a", "  └ b"]);
    }

    #[test]
    fn an_edit_cell_previews_then_shows_all() {
        let edit = diff::parse_edit_result(
            "--- a/f.rs\n+++ b/f.rs\n@@ -1,4 +1,4 @@\n a\n-b\n+B\n c\n d\nedited f.rs at line 2",
        )
        .unwrap();
        let cell = Cell::Edit { edit, detail: None };
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

    // T2 (issue #115): the call line is a pure function of a `Usage`.
    // Every expected string is derived from the shared formatters and
    // the fixture, never written by hand.

    /// A `Usage` with every field the log can hold (issue #115).
    fn full_usage() -> Usage {
        Usage {
            input_tokens: 5_300,
            output_tokens: 812,
            cache_read_tokens: 35_700,
            cache_write_tokens: 0,
            reasoning_tokens: Some(300),
            estimated: false,
            profile: Some("flash".into()),
            model: Some("deepseek-v4.1-flash".into()),
            effort: Some("high".into()),
            latency_ms: Some(2_400),
            ttft_ms: Some(900),
            cost_usd: Some(0.0141),
        }
    }

    /// The line derived from the fixture, so a formatter change moves
    /// the expectation with it.
    fn expected_call_text(n: u32, u: &Usage, profile: Option<&str>) -> String {
        let prompt = u.input_tokens + u.cache_read_tokens + u.cache_write_tokens;
        let mut parts = Vec::new();
        if let Some(p) = profile {
            parts.push(p.to_owned());
        }
        parts.push(u.model.clone().unwrap());
        parts.push(format!("{} in", status::count_short(prompt)));
        let pct = (100.0 * u.cache_read_tokens as f64 / prompt as f64).round() as u32;
        parts.push(format!("{pct}% cached"));
        parts.push(format!("{} out", u.output_tokens));
        parts.push(format!("{} reasoning", u.reasoning_tokens.unwrap()));
        let mut took = secs_ms(u.latency_ms.unwrap());
        took.push_str(&format!(" (first token {})", secs_ms(u.ttft_ms.unwrap())));
        parts.push(took);
        parts.push(stats::money(u.cost_usd, None));
        format!("◦ call {n} · {}", parts.join(" · "))
    }

    #[test]
    fn a_call_line_with_every_field_reads_its_figures() {
        let u = full_usage();
        let line = CallLine::from_usage(3, &u, Some("flash"));
        // The attached profile is not named.
        assert_eq!(line.profile, None);
        assert_eq!(line.text(), expected_call_text(3, &u, None));
    }

    #[test]
    fn a_side_profile_is_named_and_the_attached_one_is_not() {
        let u = full_usage();
        // The attached profile differs: the call names its own.
        let side = CallLine::from_usage(1, &u, Some("kimi"));
        assert_eq!(side.profile.as_deref(), Some("flash"));
        assert!(side.text().contains("flash · deepseek-v4.1-flash"));
        // The attached profile matches: nothing extra.
        let mine = CallLine::from_usage(1, &u, Some("flash"));
        assert_eq!(mine.profile, None);
        assert!(!mine.text().contains("kimi"));
    }

    #[test]
    fn each_optional_part_drops_when_the_log_lacks_it() {
        let mut u = full_usage();
        u.reasoning_tokens = None;
        assert!(
            !CallLine::from_usage(1, &u, None)
                .text()
                .contains("reasoning")
        );

        u = full_usage();
        u.latency_ms = None;
        let text = CallLine::from_usage(1, &u, None).text();
        assert!(!text.contains("s (first token"));
        assert!(!text.contains("2.4s"));

        u = full_usage();
        u.ttft_ms = None;
        let text = CallLine::from_usage(1, &u, None).text();
        assert!(text.contains(&secs_ms(2_400)));
        assert!(!text.contains("first token"));

        u = full_usage();
        u.model = None;
        assert!(
            !CallLine::from_usage(1, &u, None)
                .text()
                .contains("deepseek")
        );

        // No cache fields at all: no share is claimed.
        u = full_usage();
        u.cache_read_tokens = 0;
        u.cache_write_tokens = 0;
        assert!(
            !CallLine::from_usage(1, &u, None)
                .text()
                .contains("% cached")
        );
    }

    #[test]
    fn an_estimated_call_says_so_and_an_unpriced_one_says_that() {
        let mut u = full_usage();
        u.estimated = true;
        assert!(
            CallLine::from_usage(1, &u, None)
                .text()
                .contains(&stats::money(None, Some(0.0141)))
        );

        u = full_usage();
        u.cost_usd = None;
        assert!(
            CallLine::from_usage(1, &u, None)
                .text()
                .contains("unpriced")
        );
    }

    #[test]
    fn reasoning_tokens_of_zero_adds_nothing() {
        let mut u = full_usage();
        u.reasoning_tokens = Some(0);
        assert!(
            !CallLine::from_usage(1, &u, None)
                .text()
                .contains("reasoning")
        );
    }

    // T3 (issue #115): the tool detail's tail is bytes, took and the
    // policy word, each only when the log held it.

    #[test]
    fn a_tool_detail_tail_reads_its_parts() {
        let d = ToolDetail {
            took: Some(std::time::Duration::from_millis(1_200)),
            bytes: 3_400,
            policy: Some("rule read_only".into()),
        };
        assert_eq!(
            d.tail(),
            format!(
                " · {} · {} · rule read_only",
                secs_ms(1_200),
                bytes_short(3_400)
            )
        );
    }

    #[test]
    fn a_tool_detail_tail_drops_what_the_log_lacked() {
        let d = ToolDetail {
            took: None,
            bytes: 42,
            policy: None,
        };
        assert_eq!(d.tail(), format!(" · {}", bytes_short(42)));
    }
}
