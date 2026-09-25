//! The client (phase 6): the engine that turns lines into requests and
//! notices into lines, and the two ways to drive it. At a terminal, the
//! ratatui shell: an inline viewport over the terminal's scrollback,
//! the composer, the status line. Without one (a pipe, a script), plain
//! lines in and out, which is what the README's acceptance items and
//! `exec` use.

pub mod cells;
pub mod commands;
pub mod completion;
pub mod composer;
pub mod diff;
pub mod engine;
pub mod keymap;
pub mod look;
pub mod markdown;
pub mod menu;
pub mod pager;
pub mod status;
pub mod tui;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use aigentic_api::{Notice, ThreadState};
use crossterm::event::{Event, EventStream, KeyCode as K, KeyEventKind};
use futures_util::StreamExt;
use tokio::sync::mpsc;

use crate::app::cells::{Cell, ToolState, is_read_tool};
use crate::app::completion::{FileIndex, Popup};
use crate::app::composer::Composer;
use crate::app::engine::{ClientRepl, MenuKey, Printer, TurnStats};
use crate::app::keymap::{Action, KeyContext, action_for};
use crate::app::menu::{Menu, Pick};
use crate::app::pager::Pager;
use crate::app::status::Status;
use crate::app::tui::{Pane, Shell, needed_rows, wrap_line};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// How often the status line's clock is redrawn while a turn runs.
const TICK: Duration = Duration::from_millis(250);

/// Run the client: the shell at a terminal, plain lines otherwise.
pub async fn run(
    engine: ClientRepl,
    notices: mpsc::Receiver<Notice>,
    history: PathBuf,
    project: String,
    root: PathBuf,
) -> anyhow::Result<()> {
    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        run_shell(engine, notices, history, project, root).await
    } else {
        run_plain(engine, notices).await
    }
}

/// Plain mode: stdin lines, stdout lines.
async fn run_plain(mut engine: ClientRepl, notices: mpsc::Receiver<Notice>) -> anyhow::Result<()> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    engine.run(rx, notices, &mut Stdout).await;
    println!("bye");
    Ok(())
}

struct Stdout;

impl Printer for Stdout {
    fn line(&mut self, text: &str) {
        println!("{text}");
    }
}

/// How many cells the pager keeps.
const TRANSCRIPT_KEEP: usize = 2000;

/// The shell's printer. Finished cells go to the scrollback; what still
/// changes (pending reads, a running tool, the assistant's unfinished
/// line) stays in the viewport. Consecutive reads fold into one
/// `Explored` cell, committed when something else arrives.
struct ShellOut {
    shell: Shell,
    /// The width lines are wrapped to, read from the shell while the
    /// loop draws: a resize takes effect on the next pass.
    width: usize,
    /// Every committed cell, for the pager.
    transcript: Vec<Cell>,
    /// Committed lines the next draw flushes.
    pending: Vec<Line<'static>>,
    /// Reads not yet committed as one `Explored`.
    explored: Vec<String>,
    /// The running tool, if any.
    running: Option<Cell>,
    /// The assistant's text since its last newline.
    tail: String,
    /// Inside a code fence in the assistant's text.
    fenced: bool,
    /// A text to page through as soon as the loop gets to it.
    page: Option<(String, String)>,
    /// The kind of the last committed block, for spacing and the reply
    /// marker.
    last: Option<look::Group>,
}

impl ShellOut {
    fn new(shell: Shell) -> Self {
        let width = shell.width();
        Self {
            shell,
            width,
            transcript: Vec::new(),
            pending: Vec::new(),
            explored: Vec::new(),
            running: None,
            tail: String::new(),
            fenced: false,
            page: None,
            last: None,
        }
    }

    fn commit(&mut self, cell: Cell) {
        // What was looked at so far comes first: the scrollback keeps
        // the order the work happened in.
        self.flush_explored();
        let width = self.width;
        let group = look::group(&cell);
        // A blank line between blocks of different kinds.
        if self.last.is_some_and(|last| last != group) {
            self.pending.push(Line::raw(""));
        }
        let first = self.last != Some(look::Group::Assistant);
        self.pending.extend(look::render(&cell, first, width));
        self.last = Some(group);
        self.transcript.push(cell);
        if self.transcript.len() > TRANSCRIPT_KEEP {
            self.transcript.remove(0);
        }
    }

    /// Whether `text` draws nothing (issue #43): a text block that is
    /// empty once trimmed. Such a block has no cell, no separator and
    /// no tail. A blank *inside* a block (`a\n\nb`) is the model's own
    /// spacing and keeps its row: that block is not whitespace-only,
    /// and its rows are the renderer's to make.
    fn ghost(text: &str) -> bool {
        text.trim().is_empty()
    }

    fn flush_explored(&mut self) {
        if !self.explored.is_empty() {
            let entries = std::mem::take(&mut self.explored);
            self.commit(Cell::Explored(entries));
        }
    }

    fn flush(&mut self) -> anyhow::Result<()> {
        for line in std::mem::take(&mut self.pending) {
            self.shell.commit(line)?;
        }
        Ok(())
    }

    /// The viewport's changing part, wrapped to the width. While
    /// writing, the tail slot is exactly [`TAIL_ROWS`] rows, blank
    /// padded: the pane holds still between newlines (issue #43).
    fn active_lines(&self, phase: LivePhase) -> Vec<Line<'static>> {
        let width = self.width;
        let mut lines = Vec::new();
        if !self.explored.is_empty() {
            lines.extend(look::render(
                &Cell::Explored(self.explored.clone()),
                false,
                width,
            ));
        }
        if !self.tail.is_empty() {
            let first = self.last != Some(look::Group::Assistant);
            let tail = Cell::Assistant {
                text: self.tail.clone(),
                fenced: self.fenced,
            };
            if first && self.last.is_some() && lines.is_empty() {
                lines.push(Line::raw(""));
            }
            lines.extend(tail_rows(&tail, first, width));
        }
        if phase == LivePhase::Writing {
            while lines.len() < TAIL_ROWS {
                lines.push(Line::raw(""));
            }
        }
        lines
    }
}

/// The assistant's unfinished text, as the viewport shows it: its last
/// [`TAIL_ROWS`] rows only (issue #39). The paragraph's earlier rows
/// are the scrollback's once its line completes, so keeping them in
/// the pane would only hold its height up and, with the line gone,
/// pad the difference with blanks.
fn tail_rows(cell: &Cell, first: bool, width: usize) -> Vec<Line<'static>> {
    let rendered = look::render(cell, first, width);
    let skip = rendered.len().saturating_sub(TAIL_ROWS);
    rendered.into_iter().skip(skip).collect()
}

impl Printer for ShellOut {
    fn line(&mut self, text: &str) {
        self.flush_explored();
        self.commit(Cell::Note(text.to_owned()));
    }

    fn quiet(&mut self, _text: &str) {
        // Housekeeping (a title, memory written) lives in the footer.
    }

    fn prompt(&mut self, _menu: &Menu) {
        // Drawn above the composer from the engine's state, not
        // committed to the transcript.
        self.flush_explored();
    }

    fn pager(&mut self, title: &str, text: &str) {
        // Drawn by the loop after this step.
        self.flush_explored();
        self.page = Some((title.to_owned(), text.to_owned()));
    }

    fn tail(&mut self, text: &str) {
        // A tail blank after trimming draws no rows (issue #43), as a
        // blank cell draws no cell.
        self.tail = if Self::ghost(text) {
            String::new()
        } else {
            text.to_owned()
        };
    }

    fn cell(&mut self, cell: Cell, done: bool) {
        match (&cell, done) {
            (Cell::Assistant { text, .. }, _) => {
                // A block that is blank after trimming draws nothing:
                // no cell, no separator, no tail (issue #43).
                if Self::ghost(text) {
                    return;
                }
                self.flush_explored();
                let fence = markdown::is_fence(text);
                let cell = Cell::Assistant {
                    text: text.clone(),
                    fenced: self.fenced,
                };
                self.commit(cell);
                if fence {
                    self.fenced = !self.fenced;
                }
            }
            (Cell::Tool { name, .. }, false) => {
                if !is_read_tool(name) {
                    self.flush_explored();
                }
                self.running = Some(cell);
            }
            (
                Cell::Tool {
                    name,
                    summary,
                    full: _,
                    state,
                    output,
                },
                true,
            ) => {
                self.running = None;
                if is_read_tool(name) && *state == ToolState::Ok {
                    self.explored
                        .push(look::explored_entry(name, summary, output));
                } else {
                    self.flush_explored();
                    self.commit(cell);
                }
            }
            (_, _) => {
                self.flush_explored();
                self.commit(cell);
            }
        }
    }
}

/// The popup's lines: the matches, the selected one highlighted, a
/// description after a command.
fn popup_lines(popup: &Popup) -> Vec<Line<'static>> {
    popup
        .items
        .iter()
        .enumerate()
        .map(|(i, (item, desc))| {
            let style = if i == popup.selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            let mut spans = vec![Span::styled(format!("  {item}"), style)];
            if !desc.is_empty() {
                spans.push(Span::styled(
                    format!("  {desc}"),
                    Style::default().add_modifier(Modifier::DIM),
                ));
            }
            Line::from(spans)
        })
        .collect()
}

/// The prompt menu's lines: the header in plain words, what is asked
/// about in full (wrapped, capped), and the rows to pick from with the
/// selected one marked. Already wrapped; the pane draws them as they
/// are.
fn block_lines(menu: &Menu, width: usize) -> Vec<Line<'static>> {
    let head = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().add_modifier(Modifier::DIM);
    let mut lines = vec![Line::from(Span::styled(format!(" {}", menu.title), head))];
    lines.extend(body_lines(menu, width));
    let inner = width.saturating_sub(6).max(1);
    for (i, row) in menu.rows.iter().enumerate() {
        let selected = i == menu.selected;
        let style = if selected {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let mark = if menu.multi && row.pick != Pick::Other {
            if menu.picked[i] { "[x] " } else { "[ ] " }
        } else {
            ""
        };
        let mut label = Span::styled(format!("{mark}{}", row.label), style);
        let mut spans = vec![std::mem::replace(&mut label, Span::raw(""))];
        if let Some(d) = &row.desc {
            spans.push(Span::styled(format!("  {d}"), dim));
        }
        let mut rows = wrap_line(&Line::from(spans), inner);
        for (r, line) in rows.iter_mut().enumerate() {
            let lead = if r > 0 {
                Span::raw(" ".repeat(6 + mark.len()))
            } else if selected {
                Span::styled(format!(" ❯ {}. ", i + 1), head)
            } else {
                Span::raw(format!("   {}. ", i + 1))
            };
            line.spans.insert(0, lead);
        }
        // The deny's Esc hint, dim, out by the right edge.
        if row.pick == Pick::Deny
            && let Some(first) = rows.first_mut()
        {
            let used: usize = first
                .spans
                .iter()
                .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
                .sum();
            let pad = width
                .saturating_sub(used + unicode_width::UnicodeWidthStr::width("(esc)"))
                .max(2);
            first.spans.push(Span::raw(" ".repeat(pad)));
            first.spans.push(Span::styled("(esc)", dim));
        }
        lines.extend(rows);
    }
    if let Some(note) = &menu.note {
        lines.extend(hang(note, 3, width, dim));
    }
    lines
}

/// `text` wrapped to `width - indent`, every row indented, so wrapped
/// rows hang under the first.
fn hang(text: &str, indent: usize, width: usize, style: Style) -> Vec<Line<'static>> {
    let inner = width.saturating_sub(indent).max(1);
    text.split('\n')
        .flat_map(|l| wrap_line(&Line::from(Span::styled(l.to_owned(), style)), inner))
        .map(|mut row| {
            let mut spans = vec![Span::raw(" ".repeat(indent))];
            spans.append(&mut row.spans);
            Line::from(spans)
        })
        .collect()
}

/// Rows the body may take before it is capped head and tail, as tool
/// output is.
const BODY_ROWS: usize = 8;

/// The body wrapped and hanging; a very long one keeps its head and
/// tail with a note of what fell out.
fn body_lines(menu: &Menu, width: usize) -> Vec<Line<'static>> {
    if menu.body.is_empty() {
        return Vec::new();
    }
    let dim = Style::default().add_modifier(Modifier::DIM);
    let mut rows = hang(&menu.body, 3, width, Style::default());
    if rows.len() > BODY_ROWS {
        let kept = BODY_ROWS - 3;
        let omitted = rows.len() - kept - 2;
        let mut capped: Vec<_> = rows.drain(..kept).collect();
        capped.push(Line::from(Span::styled(
            format!("   … {omitted} rows"),
            dim,
        )));
        capped.extend(rows.into_iter().skip(omitted));
        rows = capped;
    }
    rows
}

/// The transcript pager's lines: the whole checklist while one lasts
/// (the live block shows three rows of it), then every cell in full —
/// a blank row between blocks of different kinds, none inside one, as
/// the scrollback reads (issue #21).
fn transcript_lines(
    transcript: &[Cell],
    tasks: &[aigentic_runtime::harness_tools::Task],
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    if !tasks.is_empty() {
        lines.extend(cells::task_lines(tasks));
        lines.push(Line::raw(""));
    }
    let mut last: Option<look::Group> = None;
    for cell in transcript {
        if last.is_some_and(|g| g != look::group(cell)) {
            lines.push(Line::raw(""));
        }
        last = Some(look::group(cell));
        lines.extend(cell.full());
    }
    lines
}

/// The turn's phase, which sets the live area's height (issue #43).
/// The pane holds still inside a phase and moves only when the phase
/// changes: the rows that happen to exist this tick no longer decide
/// it, so the tail draining to the scrollback on a newline and the
/// in-flight cell flickering between calls cost nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LivePhase {
    /// No turn: no live rows at all.
    Idle,
    /// The call is out or waiting to retry: nothing is in flight yet,
    /// and text is not streaming (issue #31 keeps the retry line on the
    /// activity row).
    Thinking,
    /// Text is streaming: the tail slot holds its two rows.
    Writing,
    /// A tool is in flight: the row is reserved.
    Tool,
}

/// The phase this tick is in, from the state and the turn's figures.
/// Sticky: a tick that says nothing new keeps the phase it found, so
/// back-to-back tool calls are one Tool phase and a fresh turn starts
/// in Thinking. That is what stops the pane moving mid-phase.
fn next_phase(prev: LivePhase, state: &ThreadState, turn: Option<&TurnStats>) -> LivePhase {
    if matches!(state, ThreadState::Idle) {
        return LivePhase::Idle;
    }
    match turn {
        Some(t) if t.current.is_some() => LivePhase::Tool,
        // Text is arriving: the pane is writing whatever it was doing
        // a tick ago.
        Some(t) if t.writing => LivePhase::Writing,
        // A turn with neither says nothing new: keep the phase, except
        // that writing has stopped (the flag is cleared when a tool
        // starts or a message ends), which leaves the reply stretch.
        // Tool stays stuck through the gap between two calls, so the
        // in-flight row is never unreserved mid-turn (issue #43).
        _ => match prev {
            LivePhase::Idle | LivePhase::Writing => LivePhase::Thinking,
            other => other,
        },
    }
}

/// The live area's rows (issue #43), by phase: the in-flight tool row
/// is always reserved while a tool is out, the tail slot is always
/// [`TAIL_ROWS`] rows while writing, and the compact task list is
/// added as #21 draws it (at most three rows). Each row is one line:
/// the full command is the pager's to show.
fn live_block(
    phase: LivePhase,
    running: Option<&Cell>,
    tasks: &[aigentic_runtime::harness_tools::Task],
    width: usize,
) -> Vec<Line<'static>> {
    let one = |l: Line<'static>| -> Line<'static> {
        wrap_line(&l, width).into_iter().next().unwrap_or(l)
    };
    let mut live: Vec<Line<'static>> = Vec::new();
    match phase {
        LivePhase::Idle => return live,
        // The row stays even with nothing to put in it: the call is
        // between its start and its cell.
        LivePhase::Tool => {
            live.push(match running {
                Some(Cell::Tool { name, summary, .. }) => one(look::running(name, summary, width)
                    .into_iter()
                    .next()
                    .unwrap_or_default()),
                _ => Line::raw(""),
            });
        }
        LivePhase::Writing | LivePhase::Thinking => {
            if let Some(Cell::Tool { name, summary, .. }) = running {
                live.push(one(look::running(name, summary, width)
                    .into_iter()
                    .next()
                    .unwrap_or_default()));
            }
        }
    }
    live.extend(cells::task_compact(tasks).into_iter().map(one));
    live
}

/// How many rows of the assistant's unfinished text the viewport keeps
/// (issue #39): enough to read the line being written, not the
/// paragraph behind it, which is already in the scrollback.
const TAIL_ROWS: usize = 2;

/// The pager over `lines` in the alternate screen until it is closed.
fn page(shell: &mut Shell, title: &str, lines: Vec<Line<'static>>) -> anyhow::Result<()> {
    let mut pager = Pager::new(title, lines);
    shell.alternate(|term| {
        term.draw(|f| {
            let area = f.area();
            let rows = pager.view(area);
            for (i, line) in rows.into_iter().enumerate() {
                let y = area.y + i as u16;
                if y >= area.bottom() {
                    break;
                }
                f.render_widget(line, ratatui::layout::Rect::new(area.x, y, area.width, 1));
            }
        })?;
        let height = term.size()?.height as usize;
        match crossterm::event::read()? {
            Event::Key(k) if k.kind != KeyEventKind::Release => Ok(pager.key(k.code, height)),
            _ => Ok(false),
        }
    })
}

/// A permission prompt ignores single-key answers this long after it
/// appears, so a `y`, `a` or `n` typed into a sentence does not answer it.
const PROMPT_GRACE: Duration = Duration::from_millis(500);

/// A second press of Ctrl-C or Esc within this long completes it.
const ARM_WINDOW: Duration = Duration::from_secs(1);

async fn run_shell(
    mut engine: ClientRepl,
    mut notices: mpsc::Receiver<Notice>,
    history: PathBuf,
    project: String,
    root: PathBuf,
) -> anyhow::Result<()> {
    let mut out = ShellOut::new(Shell::start()?);
    // The turn's phase, kept across ticks: the live area's height
    // changes at phase boundaries only (issue #43).
    let mut phase = LivePhase::Idle;
    // The file index for `@`, walked once off the loop.
    let mut files_task = Some(tokio::task::spawn_blocking(move || FileIndex::walk(&root)));
    let mut files: Option<FileIndex> = None;
    let commands: Vec<(String, String)> = commands::COMMANDS
        .iter()
        .map(|(n, d)| ((*n).to_owned(), (*d).to_owned()))
        .chain(
            engine
                .skills()
                .iter()
                .map(|s| (s.clone(), "a user-invoked skill".to_owned())),
        )
        .collect();
    let mut popup: Option<Popup> = None;
    let mut composer = Composer::with_history(&history);
    let mut status = Status {
        project,
        mode: engine.mode().to_owned(),
        ..Status::default()
    };
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(TICK);
    // Ctrl-C and Esc arm on an empty idle composer; the second press
    // within the window quits or recalls.
    let mut armed: Option<(Action, Instant)> = None;
    // The last message sent (not a command), for Esc-Esc and Alt-Up.
    let mut last_sent: Option<String> = None;
    // Esc on a permission prompt, or Other on a question: the composer
    // becomes the prompt's text input; the draft it held comes back
    // after.
    let mut prompt_draft: Option<String> = None;
    // When the current prompt block first showed: single-key answers wait
    // PROMPT_GRACE so a key meant for the draft does not answer it.
    let mut block_since: Option<Instant> = None;

    // A thread opened while it waits: the prompt is shown at once.
    let state = engine.state().clone();
    engine.show_state(&state, &mut out);

    loop {
        // Everything the last step produced, then the pane. The window
        // may have been resized since the last pass.
        out.width = out.shell.width();
        out.flush()?;
        let state = engine.state().clone();
        status.apply_state(&state);
        status.mode = engine.mode().to_owned();
        status.identity = Some(engine.identity().clone());
        if let Some(p) = engine.project() {
            status.project = p.to_owned();
        }
        status.title = engine.title().map(str::to_owned);
        status.usage = engine.usage();
        // The turn line carries the clock while a turn runs.
        status.elapsed = None;
        let activity = engine.turn().map(|t| {
            let width = out.width;
            let text = format!("{} · {} · esc interrupts", t.activity(), t.figures());
            let line = Line::from(vec![
                Span::styled("✻ ", Style::default().fg(look::CLAY)),
                Span::styled(text, Style::default().add_modifier(Modifier::DIM)),
            ]);
            wrap_line(&line, width).into_iter().next().unwrap_or(line)
        });
        if let Some((_, at)) = armed
            && at.elapsed() > ARM_WINDOW
        {
            armed = None;
        }
        block_since = match (engine.menu(), block_since) {
            (None, _) => None,
            (Some(_), Some(t)) => Some(t),
            (Some(_), None) => Some(Instant::now()),
        };
        // The prompt went away while its reason input was open: the
        // draft comes back (the reason is moot).
        if prompt_draft.is_some() && engine.menu().is_none() {
            let draft = prompt_draft.take().unwrap();
            composer.set_text(&draft);
        }
        // The live area is sized by the turn's phase (issue #43), not
        // by the rows that happen to exist this tick: the in-flight row
        // stays reserved while a tool is out, the tail slot holds its
        // two rows while writing. The one blank row the layout puts
        // above them stays one.
        phase = next_phase(phase, &state, engine.turn());
        let mut block: Vec<Line<'static>> =
            live_block(phase, out.running.as_ref(), engine.tasks(), out.width);
        block.extend(
            engine
                .menu()
                .map(|m| block_lines(m, out.width))
                .unwrap_or_default(),
        );
        if files.is_none()
            && let Some(task) = files_task.as_mut()
            && task.is_finished()
        {
            files = files_task.take().unwrap().await.ok();
        }
        // The popup follows the composer: open, narrow or close on the
        // token under the cursor.
        popup = {
            let (line, col) = composer.current_line();
            match completion::token_at(line, col) {
                Some(_) if prompt_draft.is_none() => {
                    let index = files.as_ref();
                    match index {
                        Some(index) => {
                            completion::open(line, col, index, &commands).map(|mut p| {
                                if let Some(old) = &popup
                                    && old.kind == p.kind
                                    && old.query == p.query
                                {
                                    p.selected = old.selected.min(p.items.len().saturating_sub(1));
                                }
                                p
                            })
                        }
                        None => None,
                    }
                }
                _ => None,
            }
        };
        let popup_lines = popup.as_ref().map(popup_lines).unwrap_or_default();
        let hint = match (&armed, &state) {
            _ if prompt_draft.is_some() => Some(
                match engine.menu().map(|m| m.kind) {
                    Some(crate::app::menu::Kind::Question) => "answer: Enter sends · Esc cancels",
                    _ => "deny with a reason · Enter sends · Esc cancels",
                }
                .to_owned(),
            ),
            (Some((Action::QuitArm, _)), _) => Some("Ctrl-C again to quit".to_owned()),
            (Some((Action::RecallArm, _)), _) => {
                Some("Esc again to recall the last message".to_owned())
            }
            (_, ThreadState::Running { queued, .. }) if *queued > 0 => Some(format!(
                "queued {queued} · ! sends now · Alt-Up copies the last back"
            )),
            _ => None,
        };
        let active = out.active_lines(phase);
        let pane = Pane {
            active: &active,
            composer: &composer,
            status: &status.line(),
            hint: hint.as_deref(),
            block: &block,
            popup: &popup_lines,
            activity,
        };
        out.shell.fit(needed_rows(&pane))?;
        out.shell.draw(&pane)?;
        if let Some((title, text)) = out.page.take() {
            let lines = if title == "diff" {
                diff::lines(&text)
            } else {
                text.lines().map(|l| Line::raw(l.to_owned())).collect()
            };
            page(&mut out.shell, &title, lines)?;
        }
        if engine.quit_requested() {
            break;
        }

        tokio::select! {
            event = events.next() => {
                let Some(Ok(event)) = event else { break };
                match event {
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        // The prompt's reason input takes Enter and Esc;
                        // everything else types.
                        if let Some(draft) = prompt_draft.clone() {
                            match key.code {
                                K::Enter => {
                                    let reason = composer
                                        .take()
                                        .map(|r| r.trim().to_owned())
                                        .filter(|r| !r.is_empty());
                                    composer.set_text(&draft);
                                    prompt_draft = None;
                                    engine.prompt_text(reason, &mut out).await;
                                    continue;
                                }
                                K::Esc => {
                                    composer.set_text(&draft);
                                    prompt_draft = None;
                                    continue;
                                }
                                _ => {}
                            }
                        }
                        // An open popup takes the navigation keys.
                        if let Some(p) = popup.as_mut() {
                            match key.code {
                                K::Up => {
                                    p.up();
                                    continue;
                                }
                                K::Down => {
                                    p.down();
                                    continue;
                                }
                                K::Tab | K::Enter => {
                                    if let Some(text) = p.accepted() {
                                        composer.replace_before_cursor(p.start, &text);
                                    }
                                    popup = None;
                                    continue;
                                }
                                K::Esc => {
                                    // Close it by breaking the token.
                                    composer.insert_char(' ');
                                    composer.backspace();
                                    popup = None;
                                    continue;
                                }
                                _ => {}
                            }
                        }
                        // A prompt menu takes its keys: the selection
                        // always, the answering keys once the grace has
                        // passed.
                        if engine.menu().is_some() {
                            let settled =
                                block_since.is_some_and(|t| t.elapsed() >= PROMPT_GRACE);
                            match engine
                                .menu_key(&key, composer.is_empty(), settled, &mut out)
                                .await
                            {
                                MenuKey::Passed => {}
                                MenuKey::Used => continue,
                                MenuKey::Text => {
                                    prompt_draft = Some(composer.text());
                                    composer.clear();
                                    continue;
                                }
                            }
                        }
                        let ctx = KeyContext {
                            running: !matches!(engine.state(), ThreadState::Idle),
                            composer_empty: composer.is_empty(),
                        };
                        let action = action_for(&key, ctx);
                        // An armed key completes on its second press.
                        let was_armed = armed.take().map(|(a, _)| a);
                        match action {
                            Action::Submit => {
                                if let Some(text) = composer.take() {
                                    // The composer clears, so the message
                                    // itself goes to the transcript here.
                                    out.cell(Cell::User(text.clone()), true);
                                    if !text.starts_with('/') {
                                        last_sent =
                                            Some(text.trim_start_matches('!').trim().to_owned());
                                    }
                                    engine.handle_line(&text, &mut out).await;
                                }
                            }
                            Action::Newline => composer.newline(),
                            Action::Insert(c) => composer.insert_char(c),
                            Action::Backspace => composer.backspace(),
                            Action::Delete => composer.delete(),
                            Action::Left => composer.left(),
                            Action::Right => composer.right(),
                            Action::Up => composer.up(),
                            Action::Down => composer.down(),
                            Action::Home => composer.home(),
                            Action::End => composer.end(),
                            Action::ClearDraft => composer.clear(),
                            Action::Interrupt => engine.interrupt(&mut out).await,
                            Action::QuitArm => {
                                if was_armed == Some(Action::QuitArm) {
                                    break;
                                }
                                armed = Some((Action::QuitArm, Instant::now()));
                            }
                            Action::RecallArm => {
                                if was_armed == Some(Action::RecallArm) {
                                    if let Some(text) = &last_sent {
                                        composer.set_text(text);
                                    }
                                } else {
                                    armed = Some((Action::RecallArm, Instant::now()));
                                }
                            }
                            Action::Recall => {
                                if let Some(text) = &last_sent {
                                    composer.set_text(text);
                                }
                            }
                            Action::Quit => break,
                            Action::Transcript => {
                                let lines = transcript_lines(
                                    &out.transcript,
                                    engine.tasks(),
                                );
                                page(&mut out.shell, "transcript", lines)?;
                            }
                            Action::None => {}
                        }
                    }
                    Event::Paste(text) => composer.paste(&text),
                    _ => {}
                }
            }
            notice = notices.recv() => match notice {
                None => {
                    out.line("[the daemon closed the connection]");
                    out.flush()?;
                    break;
                }
                Some(n) => engine.render(n, &mut out),
            },
            _ = tick.tick() => {}
        }
    }
    out.flush()?;
    out.shell.stop();
    println!("bye");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::menu::Menu;
    use aigentic_runtime::aigentic_core::{Author, RiskClass, ToolCall};
    use serde_json::json;

    fn text(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    /// A `ShellOut` over a test terminal, wrapped to `width` columns.
    fn out_at(width: usize) -> ShellOut {
        ShellOut::new(Shell::test(width as u16, 24))
    }

    fn bash(command: &str) -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            args: json!({"command": command}),
        }
    }

    fn approval(command: &str) -> Menu {
        Menu::permission(
            &bash(command),
            RiskClass::Exec,
            "class exec: anything else in a shell",
        )
    }

    /// The pager opens with the whole checklist — the live block shows
    /// three rows of it — then the run in full.
    #[test]
    fn the_transcript_pager_opens_with_the_whole_checklist() {
        use aigentic_runtime::harness_tools::{Task, TaskState};
        let tasks = vec![
            Task {
                text: "read the plan".into(),
                state: TaskState::Done,
            },
            Task {
                text: "write the code".into(),
                state: TaskState::Active,
            },
        ];
        let lines = transcript_lines(&[Cell::User("one".into())], &tasks);
        assert_eq!(
            text(&lines),
            vec![
                "• Tasks 1/2",
                "  ✓ read the plan",
                "  ▸ write the code",
                "",
                "> one"
            ]
        );
        // No checklist, no head: the pager is the run alone.
        assert_eq!(
            text(&transcript_lines(&[Cell::User("one".into())], &[])),
            vec!["> one"]
        );
    }

    /// The live block says the call and up to three tasks (issue #21),
    /// and reserves nothing in the phases that have no rows to show
    /// (issue #43).
    #[test]
    fn the_live_block_says_the_call_and_up_to_three_tasks() {
        use aigentic_runtime::harness_tools::{Task, TaskState};
        let task = |text: &str, state: TaskState| Task {
            text: text.into(),
            state,
        };
        let bash = Cell::Tool {
            name: "bash".into(),
            summary: "cargo test --workspace".into(),
            full: None,
            state: ToolState::Running,
            output: String::new(),
        };
        // Idle and Thinking reserve nothing: no turn, no rows.
        assert!(live_block(LivePhase::Idle, None, &[], 80).is_empty());
        assert!(live_block(LivePhase::Thinking, None, &[], 80).is_empty());
        // Rows are exactly what is in flight plus the list's rows.
        assert_eq!(
            text(&live_block(
                LivePhase::Tool,
                Some(&bash),
                &[task("write the code", TaskState::Active)],
                80
            )),
            vec![
                "◦ cargo test --workspace",
                "▸ write the code",
                "0/1 done · ctrl-t for the list",
            ]
        );
        let tasks = vec![
            task("one", TaskState::Active),
            task("two", TaskState::Pending),
            task("three", TaskState::Pending),
        ];
        assert_eq!(
            text(&live_block(LivePhase::Tool, Some(&bash), &tasks, 80)),
            vec![
                "◦ cargo test --workspace",
                "▸ one",
                "○ two",
                "0/3 done · ctrl-t for the list",
            ]
        );
        // The cap holds with a longer list (#21): three rows of tasks.
        let long = (0..5)
            .map(|i| task(&format!("task {i}"), TaskState::Pending))
            .collect::<Vec<_>>();
        assert_eq!(
            text(&live_block(LivePhase::Tool, Some(&bash), &long, 80)).len(),
            4,
            "one call row and three task rows"
        );
    }

    /// While writing, the tail slot holds its two rows whether or not
    /// the stream has filled them (issue #43): the pane's height is a
    /// function of the phase, so a wave of slivers does not make it
    /// breathe.
    #[test]
    fn a_writing_tail_holds_two_rows_blank_padded() {
        let mut out = out_at(80);
        let blanks = |rows: &[Line<'static>]| rows.iter().filter(|l| l.spans.is_empty()).count();
        for text in ["a", "one two three four five six seven eight nine"] {
            out.tail(text);
            let rows = out.active_lines(LivePhase::Writing);
            assert_eq!(
                rows.len(),
                TAIL_ROWS,
                "the slot holds TAIL_ROWS while writing: {text:?}"
            );
            assert!(
                rows.len() - blanks(&rows) >= 1,
                "the stream's own rows show, padding behind: {text:?}"
            );
        }
        // Idle draws nothing at all.
        out.tail("");
        assert!(out.active_lines(LivePhase::Idle).is_empty());
    }

    /// The pane changes height only at phase boundaries (issue #43):
    /// the issue's scripted turn — a call out, five streamed lines
    /// whose tail cycles 0→1→2→0, three tool calls with a tick between
    /// each, and idle. Writing holds the same height throughout, and so
    /// does the whole tool stretch including the gap ticks; only a
    /// phase change moves it. Expected heights come from the same rule
    /// the pane draws by, never from a literal.
    #[test]
    fn the_pane_changes_height_only_at_phase_boundaries() {
        let running = || ThreadState::Running {
            by: Author::User(aigentic_runtime::aigentic_core::UserId("steve".into())),
            queued: 0,
        };
        let mut out = out_at(80);
        let bash = |summary: &str| Cell::Tool {
            name: "bash".into(),
            summary: summary.into(),
            full: None,
            state: ToolState::Running,
            output: String::new(),
        };
        // What the pane's own height is: the live block's rows plus
        // the tail slot. The layout's one blank row above them and the
        // separator the scrollback handoff adds are not part of this
        // count — the issue's rule is about the live rows.
        fn height(phase: LivePhase, running: Option<&Cell>, tail: &ShellOut) -> usize {
            let live = live_block(phase, running, &[], tail.width).len();
            let slot = if tail.tail.trim().is_empty() {
                0
            } else {
                tail_rows(
                    &Cell::Assistant {
                        text: tail.tail.clone(),
                        fenced: tail.fenced,
                    },
                    false,
                    tail.width,
                )
                .len()
            };
            live + if phase == LivePhase::Writing {
                slot.max(TAIL_ROWS)
            } else {
                slot
            }
        }

        // A message goes out: the call is in flight before it streams.
        let mut phase = next_phase(LivePhase::Idle, &running(), None);
        assert_eq!(phase, LivePhase::Thinking);
        assert_eq!(
            height(phase, None, &out),
            0,
            "nothing to show yet, so no rows"
        );

        // Five lines of the reply, the tail cycling after each: the
        // tail slot keeps Writing at the same height throughout.
        let mut writing = None;
        for text in ["Sure", " here", " is", " the", " answer."] {
            let turn = TurnStats::for_test(true, None);
            let next = next_phase(phase, &running(), Some(&turn));
            assert_eq!(next, LivePhase::Writing, "text is arriving: {text:?}");
            out.tail(text);
            out.cell(
                Cell::Assistant {
                    text: text.into(),
                    fenced: false,
                },
                true,
            );
            let rows = height(next, None, &out);
            match writing {
                None => writing = Some(rows),
                Some(first) => assert_eq!(rows, first, "height held while writing at {text:?}"),
            }
            phase = next;
            // A tick with no tool and no fresh text stays in Writing.
            assert_eq!(
                next_phase(phase, &running(), Some(&turn)),
                LivePhase::Writing
            );
        }

        // Three calls, a tick between each: one Tool stretch, one
        // height, the in-flight row reserved all the way through.
        let mut tool_height = None;
        for call in ["cargo fmt", "cargo clippy", "cargo test"] {
            let cell = bash(call);
            let turn = TurnStats::for_test(false, Some(call));
            let next = next_phase(phase, &running(), Some(&turn));
            assert_eq!(next, LivePhase::Tool, "a tool is out: {call}");
            let rows = height(next, Some(&cell), &out);
            match tool_height {
                None => tool_height = Some(rows),
                Some(first) => assert_eq!(rows, first, "height held across calls at {call}"),
            }
            phase = next;
            // A tick that lands between calls: still Tool, same rows.
            let gap = TurnStats::for_test(false, None);
            assert_eq!(
                next_phase(phase, &running(), Some(&gap)),
                LivePhase::Tool,
                "the gap tick keeps the phase: {call}"
            );
            assert_eq!(
                height(LivePhase::Tool, None, &out),
                rows,
                "the in-flight row stays reserved in the gap: {call}"
            );
        }

        // A `"\n\n"` delta between two calls is not writing (amendment
        // 2): it is not a phase change, so the height does not move.
        let blank = TurnStats::for_test(false, None);
        assert_eq!(
            next_phase(phase, &running(), Some(&blank)),
            LivePhase::Tool,
            "a blank block does not start Writing"
        );
        assert_eq!(height(LivePhase::Tool, None, &out), tool_height.unwrap());

        // Idle: the tail is committed and cleared, and the live rows
        // go.
        out.tail("");
        phase = next_phase(phase, &ThreadState::Idle, None);
        assert_eq!(phase, LivePhase::Idle);
        assert_eq!(height(phase, None, &out), 0);
    }

    /// A streaming paragraph shows only its last two rows (issue #39):
    /// the rows before them are already the scrollback's when the line
    /// completes, so they stay there rather than holding the pane up.
    #[test]
    fn a_streaming_paragraph_shows_only_its_last_two_rows() {
        let cell = Cell::Assistant {
            text: "one two three four five six seven".into(),
            fenced: false,
        };
        let full = text(&look::render(&cell, false, 12));
        let last = tail_rows(&cell, false, 12);
        assert_eq!(text(&last), full[full.len() - 2..].to_vec());
        assert_eq!(last.len(), 2);
        assert!(
            text(&last).last().unwrap().ends_with("seven"),
            "the newest row is the one kept: {:?}",
            text(&last)
        );
        // The reply's first row marker is elision's business, not the
        // cap's: the cap still keeps two rows.
        assert_eq!(tail_rows(&cell, true, 12).len(), 2);
    }

    /// The pane's own rows: what the viewport draws above the composer,
    /// which a ghost block must not reach.
    fn pane_rows(out: &ShellOut) -> Vec<String> {
        text(&out.active_lines(LivePhase::Writing))
    }

    /// A whitespace-only text block draws nothing (issue #43): no
    /// cell, no streaming tail, no group separator. The log keeps it
    /// as sent — the block reaches us either way.
    #[test]
    fn a_whitespace_only_text_block_draws_nothing() {
        let mut out = out_at(80);
        let width = out.width;
        let assistant = |block: &str, first: bool| {
            text(&look::render(
                &Cell::Assistant {
                    text: block.into(),
                    fenced: false,
                },
                first,
                width,
            ))
        };
        // Three blocks, then the blank ones a transcript carries
        // between tool calls.
        let lines = [
            "Checking the log now.",
            "Running the gate.",
            "Two tests added.",
        ];
        for line in lines {
            out.cell(
                Cell::Assistant {
                    text: line.into(),
                    fenced: false,
                },
                true,
            );
        }
        let wanted = out.pending.clone();
        for blank in ["\n\n", "   ", ""] {
            out.cell(
                Cell::Assistant {
                    text: blank.into(),
                    fenced: false,
                },
                true,
            );
        }
        // The scrollback is the three blocks and nothing for a blank
        // one: the same rows the three alone would push, the first
        // still carrying the reply marker.
        assert_eq!(out.transcript.len(), 3);
        assert_eq!(out.pending, wanted);
        assert_eq!(
            text(&out.pending),
            [
                assistant(lines[0], true),
                assistant(lines[1], false),
                assistant(lines[2], false),
            ]
            .concat()
        );
        // No tail either: a blank reason for the tail draws no ghost
        // row. What is left is the Writing slot's own blank padding,
        // which is every row empty — no bullet, nothing to read.
        out.tail("\n\n");
        assert!(out.tail.is_empty());
        assert!(
            pane_rows(&out).iter().all(String::is_empty),
            "the pane's rows: {:?}",
            pane_rows(&out)
        );
        // The next real block commits with no separator row: the
        // dropped blocks left the last group where it was, so a note
        // after the assistant's line is a new group with one blank row
        // before it and nothing more.
        let before = out.pending.len();
        out.cell(Cell::Note("[title set]".into()), true);
        let note = text(&look::render(
            &Cell::Note("[title set]".into()),
            false,
            width,
        ));
        assert_eq!(
            text(&out.pending[before..]),
            [vec![String::new()], note].concat(),
            "one separator row, no ghost rows"
        );
    }

    /// The rule is a blank *block*, not every blank row: an interior
    /// blank of a real block is the model's own spacing, and the row
    /// the renderer makes for it stays.
    #[test]
    fn a_blank_inside_a_real_block_still_draws() {
        let mut out = out_at(80);
        let block = "a\n\nb";
        out.cell(
            Cell::Assistant {
                text: block.into(),
                fenced: false,
            },
            true,
        );
        let rendered = look::render(
            &Cell::Assistant {
                text: block.into(),
                fenced: false,
            },
            true,
            out.width,
        );
        assert_eq!(out.pending, rendered, "the block draws as it renders");
        assert_eq!(out.transcript.len(), 1, "the block is kept as one cell");
        assert_eq!(
            out.pending.last().map(|l| l.spans.len()).unwrap_or(0),
            2,
            "the block's own rows are the renderer's: {rendered:?}"
        );
    }

    /// The issue's `"\n\n"` fixture: a blank block between tool calls
    /// adds no bullet row, and repeated blank blocks leave the live
    /// pane exactly where they found it.
    #[test]
    fn a_blank_block_between_tool_calls_adds_no_row() {
        let mut out = out_at(80);
        let tool = |state| Cell::Tool {
            name: "bash".into(),
            summary: "cargo fmt".into(),
            full: None,
            state,
            output: "ok".into(),
        };
        let blank = || Cell::Assistant {
            text: "\n\n".into(),
            fenced: false,
        };
        out.cell(
            Cell::Assistant {
                text: "Running the gate.".into(),
                fenced: false,
            },
            true,
        );
        out.cell(blank(), true);
        out.cell(tool(ToolState::Running), false);
        // Writing, with a blank block: the in-flight tool row stays
        // reserved and the blank one moves nothing.
        let writing = text(&out.active_lines(LivePhase::Tool));
        out.cell(blank(), true);
        assert_eq!(text(&out.active_lines(LivePhase::Tool)), writing);
        // The tool finishes and a blank block follows: still no row,
        // and the pane is the tool's own rows only.
        out.cell(tool(ToolState::Ok), true);
        out.cell(blank(), true);
        let idle = text(&out.active_lines(LivePhase::Idle));
        assert!(idle.is_empty(), "no residual live rows: {idle:?}");
        // The blank blocks left no trace in the transcript either: the
        // three things sent, and nothing between them.
        out.cell(blank(), true);
        assert_eq!(out.transcript.len(), 2, "the reply and the tool");
    }

    /// The rows the issue draws (issue #21), at 100 columns: a `bash`
    /// row says its chain's main segment and the pager the whole
    /// command; a checked-off task stays inside the tool group's rows;
    /// one blank row between a reply and the next tool group, none
    /// inside the group.
    #[test]
    fn rows_say_one_thing_each_with_one_blank_between_groups() {
        let chain =
            "cargo test -p aigentic-server 2>&1 | grep 'test result' | head; echo SERVER-DONE";
        let tool = Cell::Tool {
            name: "bash".into(),
            summary: "cargo test -p aigentic-server".into(),
            full: Some(chain.to_owned()),
            state: ToolState::Ok,
            output: "test result: ok".into(),
        };
        let done = Cell::Done("read the plan".into());
        let reply = Cell::Assistant {
            text: "Green.".into(),
            fenced: false,
        };
        let lines = transcript_lines(
            &[
                Cell::User("gate it".into()),
                reply.clone(),
                tool.clone(),
                done,
                reply,
            ],
            &[],
        );
        assert_eq!(
            text(&lines),
            vec![
                "> gate it",
                "",
                "Green.",
                "",
                // The pager's row says the whole chain (issue #21).
                "• Ran bash cargo test -p aigentic-server 2>&1 | grep 'test result' | head; \
                 echo SERVER-DONE",
                "  └ test result: ok",
                "✓ read the plan", // no blank before it: same group
                "",
                "Green.",
            ]
        );
        // The pager's row for the chain says the whole command.
        let full = tool.full();
        assert!(
            text(&full).iter().any(|l| l.contains("echo SERVER-DONE")),
            "the whole chain, not the segment: {full:?}"
        );
    }

    /// The screen the issue draws: the header in plain words, the
    /// command in full under it, the rows numbered with the selected one
    /// marked, and Esc named on the deny.
    #[test]
    fn the_approval_menu_renders_as_the_issue_draws_it() {
        let menu = approval("sed -n '/^## 4\\./,/^## 10\\./p' docs/PLAN-phase6.md");
        let lines = text(&block_lines(&menu, 76));
        assert_eq!(
            lines,
            vec![
                " Run this command?".to_owned(),
                "   sed -n '/^## 4\\./,/^## 10\\./p' docs/PLAN-phase6.md".to_owned(),
                " ❯ 1. Yes".to_owned(),
                "   2. Yes, and don't ask again for `sed -n` in this project".to_owned(),
                format!("   3. No, and tell the agent why{}(esc)", " ".repeat(39)),
            ]
        );
    }

    /// The full text wraps and hangs; nothing is cut to one line, and a
    /// very long one is capped head and tail, as tool output is.
    #[test]
    fn the_body_wraps_and_a_very_long_one_is_capped() {
        let menu = approval("grep pattern crates/tui/src");
        let lines = text(&block_lines(&menu, 40));
        assert_eq!(
            lines,
            vec![
                " Run this command?".to_owned(),
                "   grep pattern crates/tui/src".to_owned(),
                " ❯ 1. Yes".to_owned(),
                "   2. Yes, and don't ask again for `grep".to_owned(),
                "      pattern` in this project".to_owned(),
                "   3. No, and tell the agent why   (esc)".to_owned(),
            ]
        );
        let command = (1..=12)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let menu = approval(&command);
        let lines = text(&block_lines(&menu, 40));
        assert_eq!(
            lines[..9],
            [
                // The chain asks as its riskiest segment: the last line
                // here (issue #16).
                " Run this command? (includes line12)",
                "   line1",
                "   line2",
                "   line3",
                "   line4",
                "   line5",
                "   … 5 rows",
                "   line11",
                "   line12",
            ]
        );
    }

    /// A question with options on the same widget: Other last, the
    /// description dim, and a multi question's rows as checkboxes.
    #[test]
    fn a_question_with_options_renders_as_the_issue_draws_it() {
        let mut menu = Menu::asking(vec![
            aigentic_api::AskedQuestion {
                question: "Which colour?".into(),
                header: Some("colour".into()),
                options: vec![
                    aigentic_api::AskedOption {
                        label: "Red".into(),
                        description: Some("the warm one".into()),
                    },
                    aigentic_api::AskedOption {
                        label: "Green".into(),
                        description: None,
                    },
                ],
                multi: false,
            },
            aigentic_api::AskedQuestion {
                question: "Which tests?".into(),
                header: None,
                options: vec![
                    aigentic_api::AskedOption {
                        label: "unit".into(),
                        description: None,
                    },
                    aigentic_api::AskedOption {
                        label: "integration".into(),
                        description: None,
                    },
                ],
                multi: true,
            },
        ]);
        assert_eq!(
            text(&block_lines(&menu, 60)),
            vec![
                " Which colour?",
                " ❯ 1. Red  the warm one",
                "   2. Green",
                "   3. Other: type your own",
            ]
        );
        // The next question, multi: checkboxes, one picked.
        menu.answer("colour: Red");
        menu.picked[1] = true;
        assert_eq!(
            text(&block_lines(&menu, 60)),
            vec![
                " Which tests?",
                " ❯ 1. [ ] unit",
                "   2. [x] integration",
                "   3. Other: type your own",
                "   space toggles · enter sends · a pipe: `1 2`",
            ]
        );
    }

    /// A question without options: the question, and the composer takes
    /// the answer. A rule's own reason is a dim line under the options;
    /// a class-generated one is not shown.
    #[test]
    fn a_question_is_the_question_and_a_rules_reason_stays() {
        let menu = Menu::asking(vec![aigentic_api::AskedQuestion {
            question: "which colour?".into(),
            header: None,
            options: vec![],
            multi: false,
        }]);
        assert_eq!(
            text(&block_lines(&menu, 40)),
            vec![" which colour?", "   type the answer"]
        );
        let call = ToolCall {
            id: "c2".into(),
            name: "mcp.docs.search".into(),
            args: json!({"query": "phase 6"}),
        };
        let menu = Menu::permission(&call, RiskClass::Network, "mcp.docs: the plan says ask");
        let lines = block_lines(&menu, 76);
        let last = lines.last().unwrap();
        assert_eq!(
            last.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>(),
            "   mcp.docs: the plan says ask"
        );
        assert!(last.spans[1].style.add_modifier.contains(Modifier::DIM));
        assert!(menu.note.is_some());
        let menu = approval("rm -rf build");
        assert_eq!(menu.note, None, "the class reason adds nothing");
    }
}
