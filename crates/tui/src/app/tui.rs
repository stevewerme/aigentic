//! The inline shell (plan section 3, copied in shape from Codex's
//! `tui.rs`): the client draws only the bottom of the terminal, and
//! every finished line goes into the terminal's own scrollback through
//! `insert_before`, so the transcript scrolls, searches and copies like
//! any other terminal output and survives quitting. The viewport holds
//! the streaming tail, the composer and the status line.

#[cfg(test)]
use std::cell::RefCell;
use std::io::Stdout;
#[cfg(test)]
use std::rc::Rc;

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

/// The viewport's height starts here: the boxed composer, the status
/// and the blank row that ends the transcript.
pub const MIN_ROWS: u16 = 5;
/// Rows of the changing part (streaming text, a running tool, pending
/// reads) the viewport shows at most; the rest is in the scrollback.
pub const MAX_ACTIVE_ROWS: usize = 12;

/// The rows the pane needs: what `layout` draws, the active part
/// capped, plus the blank row that ends the transcript.
pub fn needed_rows(pane: &Pane<'_>) -> u16 {
    // The draft's rows and the box's two borders.
    let composer = pane.composer.lines().len().min(MAX_ROWS) + 2;
    let rows = pane.active.len().min(MAX_ACTIVE_ROWS)
        + pane.block.len()
        + pane.popup.len()
        + usize::from(pane.hint.is_some())
        + usize::from(pane.activity.is_some())
        + composer
        // The blank row that ends the transcript, and the status line.
        + 2;
    u16::try_from(rows).unwrap_or(u16::MAX)
}

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
    /// The turn line while a turn runs, right above the hint.
    pub activity: Option<Line<'static>>,
}

/// What a shell draws on: the terminal, or a `TestBackend` when a test
/// drives one. An enum rather than a type parameter so `Shell` stays a
/// single concrete type.
enum Screen {
    Terminal(CrosstermBackend<Stdout>),
    /// A test's screen, shared with the backends `fit` rebuilds so the
    /// buffer the test reads is the one every draw writes.
    #[cfg(test)]
    Test(Rc<RefCell<ratatui::backend::TestBackend>>),
}

impl std::io::Write for Screen {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Terminal(inner) => std::io::Write::write(inner, buf),
            // A `TestBackend` keeps its own buffer; nothing goes out.
            #[cfg(test)]
            Self::Test(_) => Ok(buf.len()),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Terminal(inner) => std::io::Write::flush(inner),
            #[cfg(test)]
            Self::Test(_) => Ok(()),
        }
    }
}

impl ratatui::backend::Backend for Screen {
    type Error = std::io::Error;

    fn draw<'a, I>(&mut self, content: I) -> std::io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
    {
        match self {
            Self::Terminal(inner) => ratatui::backend::Backend::draw(inner, content),
            #[cfg(test)]
            Self::Test(inner) => {
                let _ = ratatui::backend::Backend::draw(&mut *inner.borrow_mut(), content);
                Ok(())
            }
        }
    }

    fn append_lines(&mut self, n: u16) -> std::io::Result<()> {
        match self {
            Self::Terminal(inner) => inner.append_lines(n),
            #[cfg(test)]
            Self::Test(inner) => {
                let _ = inner.borrow_mut().append_lines(n);
                Ok(())
            }
        }
    }

    fn hide_cursor(&mut self) -> std::io::Result<()> {
        match self {
            Self::Terminal(inner) => inner.hide_cursor(),
            #[cfg(test)]
            Self::Test(inner) => {
                let _ = inner.borrow_mut().hide_cursor();
                Ok(())
            }
        }
    }

    fn show_cursor(&mut self) -> std::io::Result<()> {
        match self {
            Self::Terminal(inner) => inner.show_cursor(),
            #[cfg(test)]
            Self::Test(inner) => {
                let _ = inner.borrow_mut().show_cursor();
                Ok(())
            }
        }
    }

    fn get_cursor_position(&mut self) -> std::io::Result<Position> {
        match self {
            Self::Terminal(inner) => inner.get_cursor_position(),
            // A `TestBackend` always answers.
            #[cfg(test)]
            Self::Test(inner) => Ok(inner
                .borrow_mut()
                .get_cursor_position()
                .expect("a test position")),
        }
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> std::io::Result<()> {
        match self {
            Self::Terminal(inner) => inner.set_cursor_position(position),
            #[cfg(test)]
            Self::Test(inner) => {
                let _ = inner.borrow_mut().set_cursor_position(position);
                Ok(())
            }
        }
    }

    fn clear(&mut self) -> std::io::Result<()> {
        match self {
            Self::Terminal(inner) => inner.clear(),
            #[cfg(test)]
            Self::Test(inner) => {
                let _ = inner.borrow_mut().clear();
                Ok(())
            }
        }
    }

    fn clear_region(&mut self, clear_type: ratatui::backend::ClearType) -> std::io::Result<()> {
        match self {
            Self::Terminal(inner) => inner.clear_region(clear_type),
            #[cfg(test)]
            Self::Test(inner) => {
                let _ = inner.borrow_mut().clear_region(clear_type);
                Ok(())
            }
        }
    }

    fn size(&self) -> std::io::Result<ratatui::layout::Size> {
        match self {
            Self::Terminal(inner) => inner.size(),
            // A `TestBackend` always knows its size.
            #[cfg(test)]
            Self::Test(inner) => Ok(inner.borrow().size().expect("a test size")),
        }
    }

    fn window_size(&mut self) -> std::io::Result<ratatui::backend::WindowSize> {
        match self {
            Self::Terminal(inner) => inner.window_size(),
            #[cfg(test)]
            Self::Test(inner) => {
                let size = inner.borrow().size().expect("a test size");
                Ok(ratatui::backend::WindowSize {
                    columns_rows: size,
                    pixels: size,
                })
            }
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Terminal(inner) => ratatui::backend::Backend::flush(inner),
            #[cfg(test)]
            Self::Test(inner) => {
                let _ = ratatui::backend::Backend::flush(&mut *inner.borrow_mut());
                Ok(())
            }
        }
    }

    fn scroll_region_up(&mut self, region: std::ops::Range<u16>, n: u16) -> std::io::Result<()> {
        match self {
            Self::Terminal(inner) => inner.scroll_region_up(region, n),
            #[cfg(test)]
            Self::Test(inner) => {
                let _ = inner.borrow_mut().scroll_region_up(region, n);
                Ok(())
            }
        }
    }

    fn scroll_region_down(&mut self, region: std::ops::Range<u16>, n: u16) -> std::io::Result<()> {
        match self {
            Self::Terminal(inner) => inner.scroll_region_down(region, n),
            #[cfg(test)]
            Self::Test(inner) => {
                let _ = inner.borrow_mut().scroll_region_down(region, n);
                Ok(())
            }
        }
    }
}

/// The crossterm backend, remembering where it last put the cursor.
/// ratatui asks the terminal for the cursor position when it places an
/// inline viewport; the answer arrives on the input, where the key-event
/// stream is waiting for keys and can hold it until the query times out.
/// After the first real query the shell always knows the position, since
/// every draw ends by setting it, so the question is answered here.
pub struct Tracked {
    inner: Screen,
    known: Option<Position>,
}

impl Tracked {
    fn new() -> Self {
        Self {
            inner: Screen::Terminal(CrosstermBackend::new(std::io::stdout())),
            known: None,
        }
    }

    /// A tracked backend over a `TestBackend`: a test drives the shell
    /// with no terminal attached.
    #[cfg(test)]
    fn test(width: u16, height: u16) -> Self {
        Self {
            inner: Screen::Test(Rc::new(RefCell::new(ratatui::backend::TestBackend::new(
                width, height,
            )))),
            known: Some(Position::new(0, 0)),
        }
    }

    /// A backend of the same kind, for the `Terminal` `fit` builds: the
    /// terminal, or the same test screen, so a test stays attached to
    /// the buffer every draw writes.
    fn rebuild(&self) -> Self {
        let inner = match &self.inner {
            Screen::Terminal(_) => Screen::Terminal(CrosstermBackend::new(std::io::stdout())),
            #[cfg(test)]
            Screen::Test(screen) => Screen::Test(Rc::clone(screen)),
        };
        Self {
            inner,
            known: self.known,
        }
    }

    /// A test's screen, for a test to read.
    #[cfg(test)]
    fn test_screen(&self) -> Option<Rc<RefCell<ratatui::backend::TestBackend>>> {
        match &self.inner {
            Screen::Terminal(_) => None,
            Screen::Test(screen) => Some(Rc::clone(screen)),
        }
    }
}

impl ratatui::backend::Backend for Tracked {
    type Error = std::io::Error;

    fn draw<'a, I>(&mut self, content: I) -> std::io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
    {
        self.inner.draw(content)
    }
    fn append_lines(&mut self, n: u16) -> std::io::Result<()> {
        if let Some(p) = self.known.as_mut() {
            // The screen's height, so a test backend clamps to its own
            // screen rather than to a terminal it has not got.
            let bottom = self
                .inner
                .size()
                .map(|s| s.height.saturating_sub(1))
                .unwrap_or(u16::MAX);
            p.y = (p.y + n).min(bottom);
            p.x = 0;
        }
        self.inner.append_lines(n)
    }
    fn hide_cursor(&mut self) -> std::io::Result<()> {
        self.inner.hide_cursor()
    }
    fn show_cursor(&mut self) -> std::io::Result<()> {
        self.inner.show_cursor()
    }
    fn get_cursor_position(&mut self) -> std::io::Result<Position> {
        match self.known {
            Some(p) => Ok(p),
            None => {
                let p = self.inner.get_cursor_position()?;
                self.known = Some(p);
                Ok(p)
            }
        }
    }
    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> std::io::Result<()> {
        let p = position.into();
        self.known = Some(p);
        self.inner.set_cursor_position(p)
    }
    fn clear(&mut self) -> std::io::Result<()> {
        self.inner.clear()
    }
    fn clear_region(&mut self, clear_type: ratatui::backend::ClearType) -> std::io::Result<()> {
        self.inner.clear_region(clear_type)
    }
    fn size(&self) -> std::io::Result<ratatui::layout::Size> {
        self.inner.size()
    }
    fn window_size(&mut self) -> std::io::Result<ratatui::backend::WindowSize> {
        self.inner.window_size()
    }
    fn flush(&mut self) -> std::io::Result<()> {
        ratatui::backend::Backend::flush(&mut self.inner)
    }
    fn scroll_region_up(&mut self, region: std::ops::Range<u16>, n: u16) -> std::io::Result<()> {
        self.inner.scroll_region_up(region, n)
    }
    fn scroll_region_down(&mut self, region: std::ops::Range<u16>, n: u16) -> std::io::Result<()> {
        self.inner.scroll_region_down(region, n)
    }
}

/// The shell over a real terminal.
pub struct Shell {
    terminal: Terminal<Tracked>,
    enhanced_keys: bool,
    stopped: bool,
    /// The inline viewport's height now.
    rows: u16,
    /// The window size the viewport was last fitted to.
    last_size: (u16, u16),
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
        let terminal = Terminal::with_options(
            Tracked::new(),
            TerminalOptions {
                viewport: Viewport::Inline(MIN_ROWS),
            },
        )?;
        Ok(Self {
            terminal,
            enhanced_keys,
            stopped: false,
            rows: MIN_ROWS,
            last_size: crossterm::terminal::size()?,
        })
    }

    /// The terminal's size in columns and rows, from the backend the
    /// shell draws on: the same query a terminal answers, and a test
    /// backend's fixed screen.
    fn size(&self) -> anyhow::Result<(u16, u16)> {
        let size = ratatui::backend::Backend::size(self.terminal.backend())?;
        Ok((size.width, size.height))
    }

    /// The height `fit` would move to, or `None` when it would not.
    fn target(&self, wanted: u16) -> anyhow::Result<Option<u16>> {
        let screen = self.size()?.1;
        let wanted = wanted.clamp(MIN_ROWS, screen.saturating_sub(1).max(MIN_ROWS));
        Ok((wanted != self.rows).then_some(wanted))
    }

    /// Size the viewport to `wanted` rows (clamped to the screen).
    ///
    /// The viewport tracks the pane's content (issue #39): it grows and
    /// shrinks as rows come and go, so the layout's one blank row above
    /// the live area stays one. A resized window is refitted at the same
    /// height. ratatui's inline viewport has a fixed height, so a new one
    /// is made at the old one's top, over rows the old one did not cover.
    ///
    /// Growing upwards over the transcript would erase it, so growth
    /// scrolls first: `shortfall` line feeds at the last row move the
    /// whole screen up, which puts the `shortfall` topmost screen rows
    /// into the terminal's own history, and the transcript rows that sat
    /// directly above the viewport move up with them and stay visible.
    /// The old viewport's rows move up too; clearing from the old top,
    /// now `shortfall` rows higher, wipes exactly those and the blank
    /// rows below, so no transcript row is ever inside the cleared band.
    /// Shrinking (and growth with room below the old top) leaves
    /// `shortfall` at zero: nothing scrolls, the clear starts at the old
    /// top, and the new viewport anchors there.
    pub fn fit(&mut self, wanted: u16) -> anyhow::Result<()> {
        let size = self.size()?;
        let resized = size != self.last_size;
        let rows = match self.target(wanted)? {
            Some(rows) => rows,
            None if resized => self.rows.min(size.1.saturating_sub(1).max(MIN_ROWS)),
            None => return Ok(()),
        };
        let height = size.1;
        let old_top = self.terminal.get_frame().area().y;
        // How far the new viewport would run past the bottom of the
        // screen: 0 when it shrinks, or when the old top leaves room.
        let shortfall = old_top.saturating_add(rows).saturating_sub(height);
        let mut backend = self.terminal.backend().rebuild();
        {
            use ratatui::backend::{Backend, ClearType};
            if shortfall > 0 {
                backend.set_cursor_position(Position::new(0, height.saturating_sub(1)))?;
                backend.append_lines(shortfall)?;
            }
            backend.set_cursor_position(Position::new(0, old_top.saturating_sub(shortfall)))?;
            backend.clear_region(ClearType::AfterCursor)?;
            Backend::flush(&mut backend)?;
        }
        self.terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(rows),
            },
        )?;
        self.rows = rows;
        self.last_size = size;
        Ok(())
    }

    /// A shell over a `TestBackend`: a test drives the scrollback with
    /// no terminal attached.
    #[cfg(test)]
    pub fn test(width: u16, height: u16) -> Self {
        Self {
            terminal: Terminal::new(Tracked::test(width, height)).expect("a test terminal"),
            enhanced_keys: false,
            stopped: false,
            rows: height,
            last_size: (width, height),
        }
    }

    /// A shell over a `TestBackend` with an inline viewport `rows` tall:
    /// a test drives the live area itself, and `fit` can move it.
    #[cfg(test)]
    pub fn test_inline(width: u16, height: u16, rows: u16) -> Self {
        Self {
            terminal: Terminal::with_options(
                Tracked::test(width, height),
                TerminalOptions {
                    viewport: Viewport::Inline(rows),
                },
            )
            .expect("a test terminal"),
            enhanced_keys: false,
            stopped: false,
            rows,
            last_size: (width, height),
        }
    }

    /// The live area's top row.
    #[cfg(test)]
    fn frame_top(&mut self) -> u16 {
        self.terminal.get_frame().area().y
    }

    /// The screen's rows, top to bottom, trailing blanks trimmed.
    #[cfg(test)]
    fn screen_lines(&self) -> Vec<String> {
        self.with_screen(|screen| rows_of(screen.buffer()))
    }

    /// The rows that have scrolled off the top, oldest first.
    #[cfg(test)]
    fn history_lines(&self) -> Vec<String> {
        self.with_screen(|screen| rows_of(screen.scrollback()))
    }

    /// Read the test's screen: the one `fit` rebuilds stay attached to.
    #[cfg(test)]
    fn with_screen<R>(&self, f: impl FnOnce(&ratatui::backend::TestBackend) -> R) -> R {
        let screen = self
            .terminal
            .backend()
            .test_screen()
            .expect("a test screen");
        let screen = screen.borrow();
        f(&screen)
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

/// A buffer's rows, top to bottom, with trailing blanks trimmed: a
/// test's view of the screen or of the history above it.
#[cfg(test)]
fn rows_of(buf: &ratatui::buffer::Buffer) -> Vec<String> {
    (0..buf.area.height)
        .map(|y| {
            (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol().to_owned())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect()
}

/// The pane's rows, bottom-aligned in `area`, and the cursor position
/// within it. Pure, so a test can check it against a `TestBackend`.
pub fn layout(pane: &Pane<'_>, area: Rect) -> (Vec<Line<'static>>, Option<(u16, u16)>) {
    let width = area.width.max(1) as usize;
    let height = area.height as usize;
    let mut rows: Vec<Line<'static>> = Vec::new();

    // The composer, in a rounded box: `> ` on the first line, two spaces
    // after, the text between `│ ` and ` │`.
    let all = pane.composer.lines();
    let (crow, ccol) = pane.composer.cursor();
    let shown = all.len().min(MAX_ROWS);
    // Keep the cursor's row visible when the draft is taller than shown.
    let first = if crow >= shown { crow + 1 - shown } else { 0 };
    let frame = Style::default().add_modifier(Modifier::DIM);
    let inner = width.saturating_sub(6);
    let rule = "─".repeat(width.saturating_sub(2));
    let mut composer_rows: Vec<Line<'static>> =
        vec![Line::from(Span::styled(format!("╭{rule}╮"), frame))];
    let mut cursor = None;
    for (i, line) in all.iter().enumerate().skip(first).take(shown) {
        let prefix = if i == 0 { "> " } else { "  " };
        let visible: String = fit(line, inner);
        let pad = inner.saturating_sub(display_width(&visible));
        composer_rows.push(Line::from(vec![
            Span::styled("│ ", frame),
            Span::styled(prefix, Style::default().fg(Color::Cyan)),
            Span::raw(visible),
            Span::raw(" ".repeat(pad)),
            Span::styled(" │", frame),
        ]));
        if i == crow {
            let x = 4 + display_width(&line.chars().take(ccol).collect::<String>());
            cursor = Some((
                u16::try_from(x.min(width.saturating_sub(3))).unwrap_or(u16::MAX),
                u16::try_from(composer_rows.len() - 1).unwrap_or(u16::MAX),
            ));
        }
    }
    composer_rows.push(Line::from(Span::styled(format!("╰{rule}╯"), frame)));

    let status = Line::from(Span::styled(
        fit(&format!("  {}", pane.status), width),
        Style::default().add_modifier(Modifier::DIM),
    ));
    let hint = pane.hint.map(|h| {
        Line::from(Span::styled(
            fit(h, width),
            Style::default().fg(Color::Yellow),
        ))
    });

    // The rule and the fixed rows come first; the tail takes what is
    // left, its last rows when that is little.
    let fixed = composer_rows.len()
        + usize::from(hint.is_some())
        + usize::from(pane.activity.is_some())
        + 1
        + pane.block.len()
        + pane.popup.len();
    let tail_rows_avail = height.saturating_sub(fixed + 1);
    let skip = pane.active.len().saturating_sub(tail_rows_avail);
    let tail_rows: Vec<Line<'static>> = if tail_rows_avail == 0 {
        Vec::new()
    } else {
        pane.active.iter().skip(skip).cloned().collect()
    };

    // Bottom-align: blank rows first, then the one blank row that
    // ends the transcript (issue #21: a rule read as a separator
    // between things that were already separate).
    let used = tail_rows.len() + fixed + 1;
    let blank = height.saturating_sub(used);
    for _ in 0..blank {
        rows.push(Line::raw(""));
    }
    rows.push(Line::raw(""));
    rows.extend(tail_rows);
    rows.extend(pane.block.iter().cloned());
    if let Some(a) = &pane.activity {
        rows.push(a.clone());
    }
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
/// the break. Breaks at the last space that fits, dropping it; a word
/// longer than the width breaks where the column runs out. An empty line
/// is one empty row.
pub fn wrap_line(line: &Line<'static>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let cells: Vec<(char, Style)> = line
        .spans
        .iter()
        .flat_map(|s| s.content.chars().map(move |c| (c, s.style)))
        .collect();
    let mut rows: Vec<Vec<(char, Style)>> = Vec::new();
    let mut row: Vec<(char, Style)> = Vec::new();
    let mut w = 0;
    for cell in cells {
        let cw = cell.0.width().unwrap_or(0);
        if w + cw > width && !row.is_empty() {
            match row.iter().rposition(|(c, _)| *c == ' ') {
                // Break after the last word that fits; the space goes.
                Some(space) if space > 0 && cell.0 != ' ' => {
                    let rest = row.split_off(space + 1);
                    row.pop();
                    rows.push(std::mem::take(&mut row));
                    row = rest;
                }
                _ => rows.push(std::mem::take(&mut row)),
            }
            w = row.iter().map(|(c, _)| c.width().unwrap_or(0)).sum();
            if cell.0 == ' ' && row.is_empty() {
                continue; // a break at a space: no leading space
            }
        }
        row.push(cell);
        w += cw;
    }
    rows.push(row);
    rows.into_iter()
        .map(|cells| {
            let mut spans: Vec<Span<'static>> = Vec::new();
            for (c, style) in cells {
                match spans.last_mut() {
                    Some(last) if last.style == style => last.content.to_mut().push(c),
                    _ => spans.push(Span::styled(c.to_string(), style)),
                }
            }
            Line::from(spans)
        })
        .collect()
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
            activity: None,
        };
        let (rows, cursor) = render(&pane, 20, 6);
        assert_eq!(rows[0], "");
        assert_eq!(rows[1], "", "the one blank row off the transcript");
        assert_eq!(rows[2], "╭──────────────────╮");
        assert_eq!(rows[3], "│ > hello          │");
        assert_eq!(rows[4], "╰──────────────────╯");
        assert_eq!(rows[5], "  manual · p · conte");
        assert_eq!(cursor, Some((9, 3)));
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
            activity: None,
        };
        // 7 rows: the blank row that ends the transcript, one tail row
        // above the hint, the boxed composer and the status.
        let (rows, cursor) = render(&pane, 10, 7);
        assert_eq!(rows[0], "");
        assert_eq!(rows[1], "xyz");
        assert_eq!(rows[2], "queued 1 ·");
        assert_eq!(rows[3], "╭────────╮");
        assert_eq!(rows[4], "│ >      │");
        assert_eq!(rows[5], "╰────────╯");
        assert_eq!(rows[6], "  s");
        assert_eq!(cursor, Some((4, 4)));
    }

    #[test]
    fn the_live_area_sits_one_blank_under_the_transcript() {
        let composer = composer_with("");
        let active: Vec<Line<'static>> = ["  five six", "  seven"]
            .into_iter()
            .map(|s| Line::raw(s.to_owned()))
            .collect();
        let block: Vec<Line<'static>> = ["◦ bash", "▸ code", "0/1 done"]
            .into_iter()
            .map(|s| Line::raw(s.to_owned()))
            .collect();
        let activity = Some(Line::raw("✻ running"));
        let pane = Pane {
            active: &active,
            composer: &composer,
            status: "s",
            hint: None,
            block: &block,
            popup: &[],
            activity: activity.clone(),
        };
        // The blank row that ends the transcript, the active rows, the
        // block, the turn line, the empty boxed composer and the
        // status (issue #39: the pane is its content, so the blank row
        // above it is one, not the padding a peak height left).
        let composer_rows = composer.lines().len().min(MAX_ROWS) + 2;
        let wanted =
            active.len() + block.len() + usize::from(activity.is_some()) + composer_rows + 2;
        assert_eq!(usize::from(needed_rows(&pane)), wanted);
        let (rows, _) = render(&pane, 30, needed_rows(&pane));
        assert_eq!(rows.len(), wanted);
        assert_eq!(rows[0], "", "exactly one blank row above the live rows");
        assert_eq!(rows[1], "  five six");
        assert_eq!(rows[2], "  seven");
        assert_eq!(rows[3], "◦ bash");
        assert_eq!(rows[6], "✻ running");
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
            activity: None,
        };
        let (rows, cursor) = render(&pane, 20, 6);
        assert_eq!(rows[0], "");
        assert_eq!(rows[1], "╭──────────────────╮");
        assert_eq!(rows[2], "│ > one            │");
        assert_eq!(rows[3], "│   two            │");
        assert_eq!(cursor, Some((7, 2)));
    }

    #[test]
    fn needed_rows_count_every_part_and_cap_the_active_one() {
        let composer = composer_with("one");
        let idle = Pane {
            active: &[],
            composer: &composer,
            status: "s",
            hint: None,
            block: &[],
            popup: &[],
            activity: None,
        };
        assert_eq!(
            needed_rows(&idle),
            5,
            "one draft row, its box, the status, the blank row"
        );
        let active: Vec<Line<'static>> = (0..30).map(|i| Line::raw(i.to_string())).collect();
        let block = vec![Line::raw("b1"), Line::raw("b2")];
        let busy = Pane {
            active: &active,
            composer: &composer,
            status: "s",
            hint: Some("h"),
            block: &block,
            popup: &[],
            activity: None,
        };
        assert_eq!(
            needed_rows(&busy) as usize,
            MAX_ACTIVE_ROWS + 2 + 1 + 3 + 1 + 1,
            "the tail, two block rows, the hint, the box, the status, the blank row"
        );
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
    fn wrap_line_breaks_at_words_and_long_words_by_column() {
        let rows = |t: &str, w: usize| -> Vec<String> {
            wrap_line(&Line::raw(t.to_owned()), w)
                .iter()
                .map(|r| r.spans.iter().map(|s| s.content.as_ref()).collect())
                .collect()
        };
        assert_eq!(
            rows("the current plan is", 12),
            vec!["the current", "plan is"]
        );
        assert_eq!(rows("abcdefghij kl", 4), vec!["abcd", "efgh", "ij", "kl"]);
        assert_eq!(rows("", 4), vec![""]);
    }

    #[test]
    fn wrap_breaks_by_columns_and_keeps_empty_lines() {
        assert_eq!(wrap("abcdef", 4), vec!["abcd", "ef"]);
        assert_eq!(wrap("a\n\nb", 4), vec!["a", "", "b"]);
        assert_eq!(wrap("", 4), vec![""]);
        assert_eq!(wrap("日本語", 4), vec!["日本", "語"]);
    }

    /// An inline shell on a `height`-row screen (issue #48) with a
    /// transcript committed below a `MIN_ROWS` viewport: the pane's
    /// rows, one per line, as long as it takes to bottom-anchor the
    /// live area, plus the one directly above it. Returns the shell,
    /// the committed lines in order, and the live area's top row.
    fn anchored_shell(width: u16, height: u16) -> (Shell, Vec<String>, u16) {
        let mut shell = Shell::test_inline(width, height, MIN_ROWS);
        // One commit per row the live area has to move down to reach
        // the bottom, and one more for the line above it.
        let n = usize::from(height - MIN_ROWS) + 1;
        let lines: Vec<String> = (1..=n).map(|i| format!("line {i}")).collect();
        for line in &lines {
            shell.commit(Line::raw(line.clone())).expect("a commit");
        }
        let top = shell.frame_top();
        (shell, lines, top)
    }

    #[test]
    fn growth_scrolls_the_transcript_into_history() {
        let (mut shell, lines, old_top) = anchored_shell(40, 15);
        let d = 2; // a small growth: two rows more than the minimum
        let screen_before = shell.screen_lines();
        let history_before = shell.history_lines();
        assert_eq!(
            screen_before[usize::from(old_top) - 1],
            lines[lines.len() - 1],
            "the pane's last row sits directly above the live area"
        );

        shell.fit(MIN_ROWS + d).expect("a fit");

        assert_eq!(shell.rows, MIN_ROWS + d);
        assert_eq!(shell.frame_top(), old_top - d, "the live area grows up");
        let screen_after = shell.screen_lines();
        let history_after = shell.history_lines();

        // Nothing the test committed is gone, and the order holds: read
        // the screen and the history as one transcript, the committed
        // lines are all there, in order (issue #48: the rows that
        // vanished from the screen).
        let seen: Vec<&String> = history_after
            .iter()
            .chain(screen_after.iter())
            .filter(|row| lines.contains(row))
            .collect();
        let expected: Vec<&String> = lines.iter().collect();
        assert_eq!(seen, expected, "every committed line, in order");

        // The rows that left the screen are its topmost `d`, now at the
        // end of the history: the whole screen moved up by `d`.
        assert_eq!(
            &history_after[history_before.len()..],
            &screen_before[..usize::from(d)]
        );

        // The pane's last line is still on screen, one row above the
        // live area's new top.
        assert_eq!(
            screen_after[usize::from(old_top - d) - 1],
            lines[lines.len() - 1]
        );
    }

    #[test]
    fn growth_with_room_below_scrolls_nothing() {
        let height = 15;
        let mut shell = Shell::test_inline(40, height, MIN_ROWS);
        let lines: Vec<String> = (1..=2).map(|i| format!("line {i}")).collect();
        for line in &lines {
            shell.commit(Line::raw(line.clone())).expect("a commit");
        }
        let old_top = shell.frame_top();
        assert!(
            old_top + MIN_ROWS < height,
            "the live area is not at the bottom of the screen"
        );
        let history_before = shell.history_lines();

        shell.fit(MIN_ROWS + 3).expect("a fit");

        assert_eq!(
            shell.frame_top(),
            old_top,
            "the live area stays where it is"
        );
        assert_eq!(shell.rows, MIN_ROWS + 3);
        assert_eq!(
            shell.history_lines(),
            history_before,
            "nothing scrolled: the room below the pane paid for it"
        );
        let screen = shell.screen_lines();
        for (i, line) in lines.iter().enumerate() {
            assert_eq!(&screen[i], line, "the transcript is still on screen");
        }
    }

    #[test]
    fn a_shrinking_viewport_leaves_the_history_alone() {
        let (mut shell, lines, old_top) = anchored_shell(40, 15);
        let d = 2;
        shell.fit(MIN_ROWS + d).expect("a fit");
        let top_before = shell.frame_top();
        let history_before = shell.history_lines();

        shell.fit(MIN_ROWS).expect("a fit");

        assert_eq!(shell.rows, MIN_ROWS);
        assert_eq!(
            shell.frame_top(),
            top_before,
            "a shrink keeps the live area's top and pulls its bottom up"
        );
        assert_eq!(
            shell.history_lines(),
            history_before,
            "a shrink scrolls nothing"
        );
        assert_eq!(top_before, old_top - d, "the tall fit grew the live area");
        assert_eq!(
            shell.screen_lines()[usize::from(top_before) - 1],
            lines[lines.len() - 1]
        );
    }
}
