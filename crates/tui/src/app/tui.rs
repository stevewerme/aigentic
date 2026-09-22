//! The inline shell (plan section 3, copied in shape from Codex's
//! `tui.rs`): the client draws only the bottom of the terminal, and
//! every finished line goes into the terminal's own scrollback through
//! `insert_before`, so the transcript scrolls, searches and copies like
//! any other terminal output and survives quitting. The viewport holds
//! the streaming tail, the composer and the status line.

use std::io::Stdout;

use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;
use ratatui::{TerminalOptions, Viewport};
use unicode_width::UnicodeWidthChar;

use crate::app::composer::{Composer, MAX_ROWS};

/// The inline viewport's height: tail rows, the composer, the status
/// line. Fixed for now (ratatui's inline viewport does not grow).
pub const VIEWPORT_ROWS: u16 = 10;

/// What the bottom pane shows.
pub struct Pane<'a> {
    /// What is still changing: pending reads, a running tool, the
    /// assistant's text since its last newline. Already wrapped.
    pub active: &'a [Line<'static>],
    pub composer: &'a Composer,
    pub status: &'a str,
    /// A one-line hint shown above the composer (queued, Ctrl-C again).
    pub hint: Option<&'a str>,
    /// The prompt block (a permission request, a question), above the
    /// hint. Already wrapped.
    pub block: &'a [Line<'static>],
    /// The completion popup, between the hint and the composer.
    pub popup: &'a [Line<'static>],
}

/// The shell over a real terminal.
pub struct Shell {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    enhanced_keys: bool,
    stopped: bool,
}

impl Shell {
    /// Raw mode, bracketed paste, keyboard enhancement where the
    /// terminal has it, and an inline viewport at the cursor.
    pub fn start() -> anyhow::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = std::io::stdout();
        crossterm::execute!(stdout, EnableBracketedPaste)?;
        let enhanced_keys = crossterm::execute!(
            stdout,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )
        .is_ok();
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(VIEWPORT_ROWS),
            },
        )?;
        Ok(Self {
            terminal,
            enhanced_keys,
            stopped: false,
        })
    }

    /// Put the terminal back. Called on quit and by `Drop`.
    pub fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        let _ = self.terminal.draw(|f| f.render_widget(Blank, f.area()));
        let mut out = std::io::stdout();
        if self.enhanced_keys {
            let _ = crossterm::execute!(out, PopKeyboardEnhancementFlags);
        }
        let _ = crossterm::execute!(out, DisableBracketedPaste);
        let _ = disable_raw_mode();
        // Leave the cursor on a fresh line below the transcript.
        let top = self.terminal.get_frame().area().y;
        let _ = self.terminal.set_cursor_position(Position::new(0, top));
        println!();
    }

    /// The terminal's width in columns.
    pub fn width(&self) -> usize {
        self.terminal
            .size()
            .map(|s| usize::from(s.width.max(1)))
            .unwrap_or(80)
    }

    /// Commit one finished line to the scrollback, wrapped to the width.
    pub fn commit(&mut self, line: Line<'static>) -> anyhow::Result<()> {
        let width = self.width();
        let rows = wrap_line(&line, width);
        let height = u16::try_from(rows.len().max(1)).unwrap_or(u16::MAX);
        self.terminal.insert_before(height, |buf| {
            for (i, row) in rows.iter().enumerate() {
                let y = buf.area.y + u16::try_from(i).unwrap_or(u16::MAX);
                if y >= buf.area.bottom() {
                    break;
                }
                row.clone()
                    .render(Rect::new(buf.area.x, y, buf.area.width, 1), buf);
            }
        })?;
        Ok(())
    }

    /// The pager, in the alternate screen, until `draw` says to leave.
    /// The inline viewport is untouched underneath and comes back as
    /// the terminal restores its main screen.
    pub fn alternate<F>(&mut self, mut draw: F) -> anyhow::Result<()>
    where
        F: FnMut(&mut Terminal<CrosstermBackend<Stdout>>) -> anyhow::Result<bool>,
    {
        use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
        let mut out = std::io::stdout();
        crossterm::execute!(out, EnterAlternateScreen)?;
        let mut full = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
        let result = loop {
            match draw(&mut full) {
                Ok(true) => break Ok(()),
                Ok(false) => {}
                Err(e) => break Err(e),
            }
        };
        let _ = crossterm::execute!(out, LeaveAlternateScreen);
        result
    }

    /// Draw the bottom pane.
    pub fn draw(&mut self, pane: &Pane<'_>) -> anyhow::Result<()> {
        self.terminal.draw(|f| {
            let area = f.area();
            let (rows, cursor) = layout(pane, area);
            for (i, line) in rows.into_iter().enumerate() {
                let y = area.y + u16::try_from(i).unwrap_or(u16::MAX);
                if y >= area.bottom() {
                    break;
                }
                line.render(Rect::new(area.x, y, area.width, 1), f.buffer_mut());
            }
            if let Some((x, y)) = cursor {
                f.set_cursor_position(Position::new(area.x + x, area.y + y));
            }
        })?;
        Ok(())
    }
}

impl Drop for Shell {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Blank;

impl Widget for Blank {
    fn render(self, _: Rect, _: &mut ratatui::buffer::Buffer) {}
}

/// The pane's rows, bottom-aligned in `area`, and the cursor position
/// within it. Pure, so a test can check it against a `TestBackend`.
pub fn layout(pane: &Pane<'_>, area: Rect) -> (Vec<Line<'static>>, Option<(u16, u16)>) {
    let width = area.width.max(1) as usize;
    let height = area.height as usize;
    let mut rows: Vec<Line<'static>> = Vec::new();

    // The composer: `> ` on the first line, two spaces after.
    let all = pane.composer.lines();
    let (crow, ccol) = pane.composer.cursor();
    let shown = all.len().min(MAX_ROWS);
    // Keep the cursor's row visible when the draft is taller than shown.
    let first = if crow >= shown { crow + 1 - shown } else { 0 };
    let mut composer_rows: Vec<Line<'static>> = Vec::new();
    let mut cursor = None;
    for (i, line) in all.iter().enumerate().skip(first).take(shown) {
        let prefix = if i == 0 { "> " } else { "  " };
        let visible: String = fit(line, width.saturating_sub(2));
        composer_rows.push(Line::from(vec![
            Span::styled(prefix, Style::default().fg(Color::Cyan)),
            Span::raw(visible),
        ]));
        if i == crow {
            let x = 2 + display_width(&line.chars().take(ccol).collect::<String>());
            cursor = Some((
                u16::try_from(x.min(width.saturating_sub(1))).unwrap_or(u16::MAX),
                u16::try_from(composer_rows.len() - 1).unwrap_or(u16::MAX),
            ));
        }
    }

    let status = Line::from(Span::styled(
        fit(pane.status, width),
        Style::default().add_modifier(Modifier::DIM),
    ));
    let hint = pane.hint.map(|h| {
        Line::from(Span::styled(
            fit(h, width),
            Style::default().fg(Color::Yellow),
        ))
    });

    // Rows left for the tail after the composer, the hint and the status.
    let fixed =
        composer_rows.len() + usize::from(hint.is_some()) + 1 + pane.block.len() + pane.popup.len();
    let tail_rows_avail = height.saturating_sub(fixed);
    let skip = pane.active.len().saturating_sub(tail_rows_avail);
    let tail_rows: Vec<Line<'static>> = if tail_rows_avail == 0 {
        Vec::new()
    } else {
        pane.active.iter().skip(skip).cloned().collect()
    };

    // Bottom-align: blank rows first.
    let used = tail_rows.len() + fixed;
    let blank = height.saturating_sub(used);
    for _ in 0..blank {
        rows.push(Line::raw(""));
    }
    rows.extend(tail_rows);
    rows.extend(pane.block.iter().cloned());
    let composer_top = rows.len();
    if let Some(h) = hint {
        rows.push(h);
    }
    rows.extend(pane.popup.iter().cloned());
    let composer_top = composer_top + usize::from(pane.hint.is_some()) + pane.popup.len();
    rows.extend(composer_rows);
    rows.push(status);
    let cursor = cursor.map(|(x, y)| (x, u16::try_from(composer_top).unwrap_or(u16::MAX) + y));
    (rows, cursor)
}

fn display_width(s: &str) -> usize {
    s.chars().map(|c| c.width().unwrap_or(0)).sum()
}

/// The first `width` columns of `s`.
fn fit(s: &str, width: usize) -> String {
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if w + cw > width {
            break;
        }
        out.push(c);
        w += cw;
    }
    out
}

/// Wrap plain text at the width by display columns; an empty text is
/// one empty row. No word wrapping: a break falls where the column
/// runs out, which keeps code and paths honest.
#[cfg(test)]
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    for line in text.split('\n') {
        let mut row = String::new();
        let mut w = 0;
        for c in line.chars() {
            let cw = c.width().unwrap_or(0);
            if w + cw > width && !row.is_empty() {
                rows.push(std::mem::take(&mut row));
                w = 0;
            }
            row.push(c);
            w += cw;
        }
        rows.push(row);
    }
    rows
}

/// Wrap a styled line at the width, keeping each span's style across
/// the break. An empty line is one empty row.
pub fn wrap_line(line: &Line<'static>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut rows: Vec<Line<'static>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut w = 0;
    for span in &line.spans {
        let mut piece = String::new();
        for c in span.content.chars() {
            let cw = c.width().unwrap_or(0);
            if w + cw > width && w > 0 {
                if !piece.is_empty() {
                    current.push(Span::styled(std::mem::take(&mut piece), span.style));
                }
                rows.push(Line::from(std::mem::take(&mut current)));
                w = 0;
            }
            piece.push(c);
            w += cw;
        }
        if !piece.is_empty() {
            current.push(Span::styled(piece, span.style));
        }
    }
    rows.push(Line::from(current));
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn composer_with(text: &str) -> Composer {
        let mut c = Composer::default();
        c.insert_str(text);
        c
    }

    fn render(pane: &Pane<'_>, width: u16, height: u16) -> (Vec<String>, Option<(u16, u16)>) {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut cursor = None;
        terminal
            .draw(|f| {
                let area = f.area();
                let (rows, c) = layout(pane, area);
                cursor = c;
                for (i, line) in rows.into_iter().enumerate() {
                    line.render(
                        Rect::new(area.x, area.y + i as u16, area.width, 1),
                        f.buffer_mut(),
                    );
                }
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let rows = (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buf[(x, y)].symbol().to_owned())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect();
        (rows, cursor)
    }

    #[test]
    fn the_pane_is_bottom_aligned_with_composer_then_status() {
        let composer = composer_with("hello");
        let pane = Pane {
            active: &[],
            composer: &composer,
            status: "manual · p · context ?",
            hint: None,
            block: &[],
            popup: &[],
        };
        let (rows, cursor) = render(&pane, 40, 5);
        assert_eq!(rows[0], "");
        assert_eq!(rows[1], "");
        assert_eq!(rows[2], "");
        assert_eq!(rows[3], "> hello");
        assert_eq!(rows[4], "manual · p · context ?");
        assert_eq!(cursor, Some((7, 3)));
    }

    #[test]
    fn the_tail_wraps_above_the_composer_and_shows_its_last_rows() {
        let composer = composer_with("");
        let active: Vec<Line<'static>> = wrap("abcdefghij1234567890xyz", 10)
            .into_iter()
            .map(Line::raw)
            .collect();
        let pane = Pane {
            active: &active,
            composer: &composer,
            status: "s",
            hint: Some("queued 1 · ! sends now"),
            block: &[],
            popup: &[],
        };
        // 4 rows: one tail row fits above hint, composer and status.
        let (rows, cursor) = render(&pane, 10, 4);
        assert_eq!(rows[0], "xyz");
        assert_eq!(rows[1], "queued 1 ·");
        assert_eq!(rows[2], ">");
        assert_eq!(rows[3], "s");
        assert_eq!(cursor, Some((2, 2)));
    }

    #[test]
    fn a_multi_line_draft_takes_rows_and_the_cursor_follows() {
        let mut composer = composer_with("one");
        composer.newline();
        composer.insert_str("two");
        composer.up();
        let pane = Pane {
            active: &[],
            composer: &composer,
            status: "s",
            hint: None,
            block: &[],
            popup: &[],
        };
        let (rows, cursor) = render(&pane, 20, 4);
        assert_eq!(rows[1], "> one");
        assert_eq!(rows[2], "  two");
        assert_eq!(cursor, Some((5, 1)));
    }

    #[test]
    fn wrap_line_keeps_span_styles_across_the_break() {
        let line = Line::from(vec![
            Span::raw("abc"),
            Span::styled("defgh", Style::default().fg(Color::Red)),
        ]);
        let rows = wrap_line(&line, 4);
        assert_eq!(rows.len(), 2);
        let texts: Vec<String> = rows
            .iter()
            .map(|r| r.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(texts, vec!["abcd", "efgh"]);
        assert_eq!(rows[1].spans[0].style.fg, Some(Color::Red));
        assert_eq!(wrap_line(&Line::raw(""), 4).len(), 1);
    }

    #[test]
    fn wrap_breaks_by_columns_and_keeps_empty_lines() {
        assert_eq!(wrap("abcdef", 4), vec!["abcd", "ef"]);
        assert_eq!(wrap("a\n\nb", 4), vec!["a", "", "b"]);
        assert_eq!(wrap("", 4), vec![""]);
        assert_eq!(wrap("日本語", 4), vec!["日本", "語"]);
    }
}
