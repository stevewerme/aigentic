//! How the shell's transcript looks (phase 6 step 8d): who is speaking,
//! where a turn starts, what is plumbing. A reply is marked with a clay
//! dot, a tool call is one line with a green or red dot, the person's
//! message is a shaded block, groups are separated by a blank line, and
//! housekeeping is dim. A pipe and the pager keep `Cell::plain` and
//! `Cell::full`; this is the terminal's view only.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::app::cells::{Cell, ToolState, task_lines};
use crate::app::diff;
use crate::app::tui::wrap_line;

/// Claude Code's reply marker, in clay.
pub const MARK: &str = "⏺ ";
pub const CLAY: Color = Color::Rgb(0xd8, 0x5a, 0x30);
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

/// Which kind of block a cell belongs to; a blank line separates two
/// blocks of different kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    User,
    Assistant,
    Tools,
    Summary,
    Note,
}

pub fn group(cell: &Cell) -> Group {
    match cell {
        Cell::User(_) => Group::User,
        Cell::Assistant { .. } => Group::Assistant,
        Cell::Tool { .. } | Cell::Explored(_) | Cell::Edit(_) | Cell::Tasks(_) => Group::Tools,
        Cell::Summary(_) => Group::Summary,
        Cell::Note(_) => Group::Note,
    }
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

/// The running tool, in the viewport.
pub fn running(tool: &str, summary: &str, width: usize) -> Vec<Line<'static>> {
    hang(
        Line::from(head_spans(tool, summary)),
        Span::styled("◦ ", Style::default().fg(Color::Yellow)),
        width,
    )
}

/// A cell's rows in the shell. The assistant's `first` says whether this
/// line starts a reply (the marker) or continues one (two spaces).
pub fn render(cell: &Cell, first: bool, width: usize) -> Vec<Line<'static>> {
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
            let lead = if first { dot(CLAY) } else { Span::raw("  ") };
            hang(line, lead, width)
        }
        Cell::Tool {
            name,
            summary,
            state,
            output,
        } => {
            let mut spans = head_spans(name, summary);
            match state {
                ToolState::Err => {
                    spans.push(Span::styled(" · failed", Style::default().fg(Color::Red)));
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
                _ => {
                    spans.push(Span::styled(count(output.lines().count()), dim()));
                    hang(Line::from(spans), dot(Color::Green), width)
                }
            }
        }
        Cell::Explored(entries) => entries
            .iter()
            .flat_map(|e| {
                let (main, tail) = match e.split_once(" · ") {
                    Some((m, t)) => (m.to_owned(), format!(" · {t}")),
                    None => (e.clone(), String::new()),
                };
                let (v, rest) = main.split_once(' ').unwrap_or((main.as_str(), ""));
                let line = Line::from(vec![
                    Span::styled(v.to_owned(), bold()),
                    Span::raw(format!(" {rest}")),
                    Span::styled(tail, dim()),
                ]);
                hang(line, dot(Color::Green), width)
            })
            .collect(),
        Cell::Edit(edit) => {
            let head = Line::from(vec![
                Span::styled("Edited", bold()),
                Span::raw(format!(" {}", edit.path)),
                Span::styled(format!(" (+{} −{})", edit.added, edit.removed), dim()),
            ]);
            let mut lines = hang(head, dot(Color::Green), width);
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
        Cell::Tasks(tasks) => {
            let mut lines = task_lines(tasks);
            if let Some(first) = lines.first_mut() {
                first.spans[0] = dot(Color::Cyan);
            }
            lines
        }
        Cell::Summary(text) => vec![Line::from(Span::styled(
            format!("  {}", text.trim_start_matches("─ ")),
            dim(),
        ))],
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
            text(&render(&cell, true, 14)),
            vec!["⏺ one two", "  three four", "  five"]
        );
        assert_eq!(
            text(&render(&cell, false, 40)),
            vec!["  one two three four five"]
        );
    }

    #[test]
    fn a_tool_is_one_line_and_a_failure_shows_its_tail() {
        let ok = Cell::Tool {
            name: "bash".into(),
            summary: "git log --oneline -15".into(),
            state: ToolState::Ok,
            output: "a\nb\nc".into(),
        };
        assert_eq!(
            text(&render(&ok, false, 80)),
            vec!["⏺ git log --oneline -15 · 3 lines"]
        );
        let failed = Cell::Tool {
            name: "bash".into(),
            summary: "cargo test".into(),
            state: ToolState::Err,
            output: "1\n2\n3\n4\nerror: 1 failed".into(),
        };
        assert_eq!(
            text(&render(&failed, false, 80)),
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
        let rows = text(&render(&Cell::User("hi".into()), false, 10));
        assert_eq!(rows, vec![" hi       "]);
        assert_eq!(
            text(&render(
                &Cell::Note("[sent: reaches the agent at its next step]".into()),
                false,
                80
            )),
            vec!["  sent: reaches the agent at its next step"]
        );
        assert_eq!(
            text(&render(&Cell::Summary("─ 3s · 1 tool".into()), false, 80)),
            vec!["  3s · 1 tool"]
        );
    }

    #[test]
    fn the_welcome_has_the_logo_then_the_lines() {
        let w = welcome(&["aigentic · steve".into(), "keys".into()]);
        assert!(w.contains("|___/"));
        assert!(w.contains("aigentic · steve\n"));
    }
}
