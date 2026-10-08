//! How the shell's transcript looks (phase 6 step 8d): who is speaking,
//! where a turn starts, what is plumbing. A reply is marked with a clay
//! dot, a tool call is one line with a green or red dot, the person's
//! message is a shaded block, groups are separated by a blank line, and
//! housekeeping is dim. A pipe and the pager keep `Cell::plain` and
//! `Cell::full`; this is the terminal's view only.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::app::cells::{Cell, ToolState};
use crate::app::diff;
use crate::app::tui::wrap_line;

/// Claude Code's reply marker, in clay.
pub const MARK: &str = "⏺ ";
pub const CLAY: Color = Color::Rgb(0xd8, 0x5a, 0x30);
/// The assistant's bullet (issue #21): clay reads as red, and red is
/// for failures alone, so a reply leans violet instead.
pub const REPLY: Color = Color::Rgb(0x8b, 0x7b, 0xe0);
const USER_BG: Color = Color::Rgb(0x30, 0x30, 0x2e);
const USER_FG: Color = Color::Rgb(0xf1, 0xef, 0xe8);
/// Diff lines an edit shows before `… +N lines`.
const EDIT_PREVIEW: usize = 3;
/// Output lines a failed tool shows, from its end.
const FAILED_TAIL: usize = 3;
/// Commands whose second word is part of what they are (`git log`).
const TWO_WORD: &[&str] = &[
    "git", "cargo", "gh", "npm", "npx", "pnpm", "yarn", "docker", "kubectl", "go", "make", "bun",
    "uv", "pip", "vercel",
];

/// Which transcript the shell draws (issue #115). `Dev` is the default:
/// a developer reading the scrollback sees every figure the log holds,
/// while `Normal` prints what the shell printed before the developer
/// view existed, byte for byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum View {
    /// Calls, step headers, system lines and tool detail.
    #[default]
    Dev,
    /// The rows the shell printed before #115.
    Normal,
}

impl View {
    pub fn name(self) -> &'static str {
        match self {
            View::Dev => "dev",
            View::Normal => "normal",
        }
    }

    /// The one line `/view` prints beside the name.
    pub fn meaning(self) -> &'static str {
        match self {
            View::Dev => "dev: every call's cost, the step it served, and what the harness did",
            View::Normal => "normal: the rows the shell printed before the developer view",
        }
    }

    /// Whether the developer view's cells (a call line, a step header, a
    /// system line) are drawn and emitted at all (issue #115).
    pub fn detailed(self) -> bool {
        matches!(self, View::Dev)
    }
}

/// Which kind of block a cell belongs to; a blank line separates two
/// blocks of different kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    User,
    Assistant,
    Tools,
    /// A followed build's lines (issue #68): consecutive run cells sit
    /// together, with a blank line between them and any other group.
    Run,
    Summary,
    Note,
}

pub fn group(cell: &Cell) -> Group {
    match cell {
        Cell::User(_) => Group::User,
        Cell::Assistant { .. } => Group::Assistant,
        Cell::Tool { .. }
        | Cell::Explored(_)
        | Cell::Edit { .. }
        | Cell::Done(_)
        | Cell::Step { .. }
        | Cell::Call(_)
        | Cell::System(_) => Group::Tools,
        Cell::Run(_) => Group::Run,
        Cell::Summary(_) => Group::Summary,
        Cell::Note(_) => Group::Note,
    }
}

/// Whether a cell takes the active step's two-column indent under
/// `Dev` (issue #115). A header opens the group and the `Done` line
/// closes it, so neither is indented.
pub fn indents(cell: &Cell) -> bool {
    matches!(
        cell,
        Cell::Tool { .. } | Cell::Explored(_) | Cell::Edit { .. } | Cell::Call(_) | Cell::System(_)
    )
}

fn dim() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

fn dot(colour: Color) -> Span<'static> {
    Span::styled(MARK, Style::default().fg(colour))
}

/// Wrap `line` to `width` with `first` in front of its first row and two
/// spaces in front of the rest, so wrapped text hangs under the marker.
pub fn hang(line: Line<'static>, first: Span<'static>, width: usize) -> Vec<Line<'static>> {
    let inner = width.saturating_sub(2).max(1);
    wrap_line(&line, inner)
        .into_iter()
        .enumerate()
        .map(|(i, row)| {
            let lead = if i == 0 {
                first.clone()
            } else {
                Span::raw("  ")
            };
            let mut spans = vec![lead];
            spans.extend(row.spans);
            Line::from(spans)
        })
        .collect()
}

/// The verb a tool's one-line summary starts with.
pub fn verb(tool: &str) -> &str {
    match tool {
        "read_file" => "Read",
        "write_file" => "Wrote",
        "edit_file" => "Edited",
        "list_dir" => "Listed",
        "grep" => "Searched",
        "search_knowledge" => "Searched knowledge",
        other => other,
    }
}

/// `git log` bold, then the rest of the command; or `Read` bold, then
/// the path.
fn head_spans(tool: &str, summary: &str) -> Vec<Span<'static>> {
    if tool == "bash" {
        let words: Vec<&str> = summary.split_whitespace().collect();
        let n = if words.len() > 1 && TWO_WORD.contains(&words[0]) {
            2
        } else {
            1
        };
        let head = words.iter().take(n).copied().collect::<Vec<_>>().join(" ");
        let rest = words.iter().skip(n).copied().collect::<Vec<_>>().join(" ");
        let mut spans = vec![Span::styled(head, bold())];
        if !rest.is_empty() {
            spans.push(Span::raw(format!(" {rest}")));
        }
        spans
    } else {
        vec![
            Span::styled(verb(tool).to_owned(), bold()),
            Span::raw(format!(" {summary}")),
        ]
    }
}

fn count(lines: usize) -> String {
    match lines {
        0 => String::new(),
        1 => " · 1 line".into(),
        n => format!(" · {n} lines"),
    }
}

/// A read, for the folded list: `Read docs/PLAN.md · 612 lines`.
pub fn explored_entry(tool: &str, summary: &str, output: &str) -> String {
    format!("{} {summary}{}", verb(tool), count(output.lines().count()))
}

/// The running tool, in the viewport. Under `Dev`, while a step is
/// active, the row takes the step's indent too (issue #115), so it
/// never jumps left when it commits.
pub fn running(
    tool: &str,
    summary: &str,
    view: View,
    indent: bool,
    width: usize,
) -> Vec<Line<'static>> {
    let indent = indent && view == View::Dev;
    let width = if indent {
        width.saturating_sub(2).max(1)
    } else {
        width
    };
    let rows = hang(
        Line::from(head_spans(tool, summary)),
        Span::styled("◦ ", Style::default().fg(Color::Yellow)),
        width,
    );
    if indent { prefix_two(rows) } else { rows }
}

/// Two columns in front of every row (issue #115): the indent is a
/// prefix, and the leaf got `width - 2`, so a wrapped row still hangs
/// under its own marker rather than two past it.
fn prefix_two(rows: Vec<Line<'static>>) -> Vec<Line<'static>> {
    rows.into_iter()
        .map(|mut line| {
            line.spans.insert(0, Span::raw("  "));
            line
        })
        .collect()
}

/// A cell's rows in the shell. The assistant's `first` says whether this
/// line starts a reply (the marker) or continues one (two spaces).
/// `view` decides whether the developer rows print, and `indent` says
/// whether a step is active; under `Normal` neither shows.
pub fn render(
    cell: &Cell,
    first: bool,
    view: View,
    indent: bool,
    width: usize,
) -> Vec<Line<'static>> {
    let indent = indent && view == View::Dev && indents(cell);
    let width = if indent {
        width.saturating_sub(2).max(1)
    } else {
        width
    };
    let rows = render_cell(cell, first, width, view);
    if indent { prefix_two(rows) } else { rows }
}

fn render_cell(cell: &Cell, first: bool, width: usize, view: View) -> Vec<Line<'static>> {
    match cell {
        Cell::User(text) => {
            let style = Style::default().bg(USER_BG).fg(USER_FG);
            let inner = width.saturating_sub(2).max(1);
            text.lines()
                .flat_map(|l| {
                    wrap_line(&Line::raw(l.to_owned()), inner)
                        .into_iter()
                        .map(|row| {
                            let text: String =
                                row.spans.iter().map(|s| s.content.as_ref()).collect();
                            let used = unicode_width::UnicodeWidthStr::width(text.as_str());
                            let pad = width.saturating_sub(used + 1);
                            Line::from(Span::styled(format!(" {text}{}", " ".repeat(pad)), style))
                        })
                        .collect::<Vec<_>>()
                })
                .collect()
        }
        Cell::Assistant { text, fenced } => {
            let line = crate::app::markdown::line(text, *fenced);
            let lead = if first { dot(REPLY) } else { Span::raw("  ") };
            hang(line, lead, width)
        }
        Cell::Tool {
            name,
            summary,
            full: _,
            state,
            output,
            detail,
        } => {
            let mut spans = head_spans(name, summary);
            let failed = matches!(state, ToolState::Err);
            if failed {
                spans.push(Span::styled(" · failed", Style::default().fg(Color::Red)));
            } else {
                spans.push(Span::styled(count(output.lines().count()), dim()));
            }
            if view == View::Dev
                && let Some(detail) = detail
            {
                spans.push(Span::styled(detail.tail(), dim()));
            }
            if !failed {
                return hang(Line::from(spans), dot(Color::Green), width);
            }
            let mut lines = hang(Line::from(spans), dot(Color::Red), width);
            let rows: Vec<&str> = output.lines().collect();
            for r in rows.iter().skip(rows.len().saturating_sub(FAILED_TAIL)) {
                lines.extend(
                    hang(
                        Line::from(Span::styled((*r).to_owned(), dim())),
                        Span::styled("└ ", dim()),
                        width.saturating_sub(2),
                    )
                    .into_iter()
                    .map(|l| {
                        let mut s = vec![Span::raw("  ")];
                        s.extend(l.spans);
                        Line::from(s)
                    }),
                );
            }
            lines
        }
        Cell::Explored(entries) => entries
            .iter()
            .flat_map(|e| {
                let (main, tail) = match e.line.split_once(" · ") {
                    Some((m, t)) => (m.to_owned(), format!(" · {t}")),
                    None => (e.line.clone(), String::new()),
                };
                let (v, rest) = main.split_once(' ').unwrap_or((main.as_str(), ""));
                let mut spans = vec![
                    Span::styled(v.to_owned(), bold()),
                    Span::raw(format!(" {rest}")),
                    Span::styled(tail, dim()),
                ];
                if view == View::Dev
                    && let Some(detail) = &e.detail
                {
                    spans.push(Span::styled(detail.tail(), dim()));
                }
                hang(Line::from(spans), dot(Color::Green), width)
            })
            .collect(),
        Cell::Edit { edit, detail } => {
            let mut head = vec![
                Span::styled("Edited", bold()),
                Span::raw(format!(" {}", edit.path)),
                Span::styled(format!(" (+{} −{})", edit.added, edit.removed), dim()),
            ];
            if view == View::Dev
                && let Some(detail) = detail
            {
                head.push(Span::styled(detail.tail(), dim()));
            }
            let mut lines = hang(Line::from(head), dot(Color::Green), width);
            let body: Vec<&str> = edit
                .diff
                .lines()
                .skip_while(|l| l.starts_with("--- ") || l.starts_with("+++ "))
                .collect();
            for l in body.iter().take(EDIT_PREVIEW) {
                let mut spans = vec![Span::raw("  ")];
                spans.extend(diff::line(l).spans);
                lines.push(Line::from(spans));
            }
            if body.len() > EDIT_PREVIEW {
                lines.push(Line::from(Span::styled(
                    format!("  … +{} lines (ctrl-t)", body.len() - EDIT_PREVIEW),
                    dim(),
                )));
            }
            lines
        }
        Cell::Call(line) => match view {
            View::Dev => hang(
                Line::from(Span::styled(line.text(), dim())),
                Span::raw(""),
                width,
            ),
            View::Normal => Vec::new(),
        },
        Cell::Step { index, total, text } => match view {
            View::Dev => hang(
                Line::from(vec![
                    Span::styled("▸ ", dim()),
                    Span::styled(format!("{index}/{total} "), dim()),
                    Span::styled(text.clone(), bold()),
                ]),
                Span::raw(""),
                width,
            ),
            View::Normal => Vec::new(),
        },
        Cell::System(text) => match view {
            View::Dev => text
                .lines()
                .flat_map(|l| {
                    hang(
                        Line::from(Span::styled(format!("· {l}"), dim())),
                        Span::raw(""),
                        width,
                    )
                })
                .collect(),
            View::Normal => Vec::new(),
        },
        Cell::Done(text) => vec![Line::from(vec![
            Span::styled("✓ ", dim()),
            Span::raw(text.clone()),
        ])],
        Cell::Summary(text) => vec![Line::from(Span::styled(
            format!("  {}", text.trim_start_matches("─ ")),
            dim(),
        ))],
        Cell::Run(text) => text
            .lines()
            .map(|l| Line::from(Span::styled(format!("▸ {l}"), dim())))
            .collect(),
        Cell::Note(text) => {
            let bracketed = text.starts_with('[') && text.ends_with(']');
            let (shown, style) = if bracketed {
                (text[1..text.len() - 1].to_owned(), dim())
            } else {
                (text.clone(), Style::default())
            };
            hang(
                Line::from(Span::styled(shown, style)),
                Span::raw("  "),
                width,
            )
        }
    }
}

/// The welcome: the logo in clay, then who, where and what thread in one
/// line, then the keys worth knowing. ANSI for plain `println!`, before
/// the shell starts.
pub fn welcome(lines: &[String]) -> String {
    const LOGO: &str = r"      _              _   _
 __ _(_)__ _ ___ _ _| |_(_)__
/ _` | / _` / -_) ' \  _| / _|
\__,_|_\__, \___|_||_\__|_\__|
       |___/";
    let clay = "\x1b[38;2;216;90;48m";
    let dim = "\x1b[2m";
    let reset = "\x1b[0m";
    let mut out = String::new();
    for l in LOGO.lines() {
        out.push_str(&format!("{clay}{l}{reset}\n"));
    }
    out.push('\n');
    for (i, l) in lines.iter().enumerate() {
        if i == 0 {
            out.push_str(&format!("{l}\n"));
        } else {
            out.push_str(&format!("{dim}{l}{reset}\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::cells::{CallLine, ToolDetail};

    fn text(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn a_reply_is_marked_once_and_hangs_when_wrapped() {
        let cell = Cell::Assistant {
            text: "one two three four five".into(),
            fenced: false,
        };
        assert_eq!(
            text(&render(&cell, true, View::Normal, false, 14)),
            vec!["⏺ one two", "  three four", "  five"]
        );
        assert_eq!(
            text(&render(&cell, false, View::Normal, false, 40)),
            vec!["  one two three four five"]
        );
        // The bullet is violet (issue #21): red is for failures
        // alone, and clay reads as red.
        let lines = render(&cell, true, View::Normal, false, 40);
        assert_eq!(lines[0].spans[0].style.fg, Some(REPLY));
        assert_ne!(REPLY, CLAY);
        assert_ne!(REPLY, Color::Red);
        // A failed tool's bullet stays red.
        let failed = Cell::Tool {
            name: "bash".into(),
            summary: "cargo test".into(),
            full: None,
            state: ToolState::Err,
            output: "error".into(),
            detail: None,
        };
        let lines = render(&failed, true, View::Normal, false, 80);
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Red));
    }

    #[test]
    fn a_tool_is_one_line_and_a_failure_shows_its_tail() {
        let ok = Cell::Tool {
            name: "bash".into(),
            summary: "git log --oneline -15".into(),
            full: None,
            state: ToolState::Ok,
            output: "a\nb\nc".into(),
            detail: None,
        };
        assert_eq!(
            text(&render(&ok, false, View::Normal, false, 80)),
            vec!["⏺ git log --oneline -15 · 3 lines"]
        );
        let failed = Cell::Tool {
            name: "bash".into(),
            summary: "cargo test".into(),
            full: None,
            state: ToolState::Err,
            output: "1\n2\n3\n4\nerror: 1 failed".into(),
            detail: None,
        };
        assert_eq!(
            text(&render(&failed, false, View::Normal, false, 80)),
            vec![
                "⏺ cargo test · failed",
                "  └ 3",
                "  └ 4",
                "  └ error: 1 failed"
            ]
        );
        assert_eq!(
            explored_entry("read_file", "docs/x.md", "a\nb"),
            "Read docs/x.md · 2 lines"
        );
    }

    #[test]
    fn the_person_is_a_full_width_block_and_notes_lose_their_brackets() {
        let rows = text(&render(
            &Cell::User("hi".into()),
            false,
            View::Normal,
            false,
            10,
        ));
        assert_eq!(rows, vec![" hi       "]);
        assert_eq!(
            text(&render(
                &Cell::Note("[sent: reaches the agent at its next step]".into()),
                false,
                View::Normal,
                false,
                80
            )),
            vec!["  sent: reaches the agent at its next step"]
        );
        assert_eq!(
            text(&render(
                &Cell::Summary("─ 3s · 1 tool".into()),
                false,
                View::Normal,
                false,
                80
            )),
            vec!["  3s · 1 tool"]
        );
    }

    #[test]
    fn the_welcome_has_the_logo_then_the_lines() {
        let w = welcome(&["aigentic · steve".into(), "keys".into()]);
        assert!(w.contains("|___/"));
        assert!(w.contains("aigentic · steve\n"));
    }

    // T3 (issue #115): the developer view adds the tool detail; Normal
    // is the bare cell; the step indent shifts every row and passes
    // `width - 2` into the leaf.

    use crate::app::cells::{bytes_short, secs_ms};
    use std::time::Duration;

    fn a_tool(detail: Option<ToolDetail>) -> Cell {
        Cell::Tool {
            name: "read_file".into(),
            summary: "src/x.rs".into(),
            full: None,
            state: ToolState::Ok,
            output: "a\nb".into(),
            detail,
        }
    }

    #[test]
    fn the_developer_view_adds_the_tool_detail_and_normal_is_the_bare_cell() {
        let cell = a_tool(Some(ToolDetail {
            took: Some(Duration::from_millis(1_200)),
            bytes: 3_400,
            policy: Some("rule read_only".into()),
        }));
        let dev = text(&render(&cell, false, View::Dev, false, 80));
        let normal = text(&render(&cell, false, View::Normal, false, 80));
        let bare = text(&render(&a_tool(None), false, View::Normal, false, 80));
        // Normal's rows are the cell with no detail at all.
        assert_eq!(normal, bare);
        assert!(dev[0].contains("rule read_only"));
        assert!(dev[0].contains(&bytes_short(3_400)));
        assert!(dev[0].contains(&secs_ms(1_200)));
    }

    #[test]
    fn the_step_indent_shifts_every_row_and_passes_width_minus_two() {
        let cell = Cell::Tool {
            name: "bash".into(),
            summary: "one two three four five six seven eight".into(),
            full: None,
            state: ToolState::Ok,
            output: "x".into(),
            detail: None,
        };
        let stepped = text(&render(&cell, false, View::Dev, true, 20));
        // Every row takes two columns...
        for row in &stepped {
            assert!(row.starts_with("  "), "{row:?}");
        }
        // ...and the leaf wraps as if it had two fewer columns, so each
        // level does not shorten the wrap by two twice.
        let inner = text(&render(&cell, false, View::Normal, false, 18));
        let stripped: Vec<String> = stepped.iter().map(|r| r[2..].to_owned()).collect();
        assert_eq!(stripped, inner);
        assert!(stepped.len() > 1);
    }

    #[test]
    fn a_failed_tool_nests_its_tail_inside_the_step_indent() {
        let failed = Cell::Tool {
            name: "bash".into(),
            summary: "cargo test".into(),
            full: None,
            state: ToolState::Err,
            output: "1\n2\nerror".into(),
            detail: None,
        };
        let rows = text(&render(&failed, false, View::Dev, true, 80));
        assert_eq!(rows[0], "  ⏺ cargo test · failed");
        // The failed path's own two-column nest sits inside the step's.
        assert_eq!(rows[1], "    └ 1");
        assert_eq!(rows[3], "    └ error");
    }

    #[test]
    fn an_edit_diff_preview_lines_up_under_the_step_indent() {
        let edit = diff::parse_edit_result("--- a\n+++ b/src/x.rs\n@@\n-old\n+new\n").unwrap();
        let cell = Cell::Edit { edit, detail: None };
        let rows = text(&render(&cell, false, View::Dev, true, 80));
        assert!(rows[0].starts_with("  ⏺ Edited src/x.rs"), "{:?}", rows[0]);
        // The edit path's own two-column marker sits inside the step's.
        assert_eq!(rows[1], "    @@");
        assert_eq!(rows[2], "    -old");
        assert_eq!(rows[3], "    +new");
    }

    #[test]
    fn a_step_header_prints_only_under_the_developer_view() {
        let step = Cell::Step {
            index: 2,
            total: 5,
            text: "Read key code regions".into(),
        };
        assert_eq!(
            text(&render(&step, false, View::Dev, false, 80)),
            vec!["▸ 2/5 Read key code regions"]
        );
        assert!(render(&step, false, View::Normal, false, 80).is_empty());
        // The pager always shows it (Design 3): `full()` is not view-led.
        assert_eq!(step.plain(), vec!["▸ 2/5 Read key code regions"]);
    }

    #[test]
    fn a_call_and_a_system_line_print_only_under_the_developer_view() {
        let call = Cell::Call(CallLine {
            n: 3,
            ..Default::default()
        });
        assert!(text(&render(&call, false, View::Dev, false, 80))[0].starts_with("◦ call 3"));
        assert!(render(&call, false, View::Normal, false, 80).is_empty());

        let system = Cell::System("retry 1/3 in 2.0s: transport reset".into());
        assert_eq!(
            text(&render(&system, false, View::Dev, false, 80)),
            vec!["· retry 1/3 in 2.0s: transport reset"]
        );
        assert!(render(&system, false, View::Normal, false, 80).is_empty());
    }
}
