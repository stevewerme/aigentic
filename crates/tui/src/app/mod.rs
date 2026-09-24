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
use crate::app::engine::{ClientRepl, MenuKey, Printer};
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
        Self {
            shell,
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
        let width = self.shell.width();
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

    /// The viewport's changing part, wrapped to the width.
    fn active_lines(&self) -> Vec<Line<'static>> {
        let width = self.shell.width();
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
            lines.extend(look::render(&tail, first, width));
        }
        lines
    }
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
        self.tail = text.to_owned();
    }

    fn cell(&mut self, cell: Cell, done: bool) {
        match (&cell, done) {
            (Cell::Assistant { text, .. }, _) => {
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

/// The live area's rows (issue #21): the call in flight and the
/// compact task list, held to [`LIVE_ROWS`] with blanks above —
/// bottom-anchored, so the pane's height never changes as rows
/// appear. Each row is one line: the reserve is a height, not a
/// floor, and the full command is the pager's to show.
fn live_rows(
    running: Option<&Cell>,
    tasks: &[aigentic_runtime::harness_tools::Task],
    width: usize,
) -> Vec<Line<'static>> {
    let one = |l: Line<'static>| -> Line<'static> {
        wrap_line(&l, width).into_iter().next().unwrap_or(l)
    };
    let mut live: Vec<Line<'static>> = Vec::new();
    if let Some(Cell::Tool { name, summary, .. }) = running {
        live.push(one(look::running(name, summary, width)
            .into_iter()
            .next()
            .unwrap_or_default()));
    }
    live.extend(cells::task_compact(tasks).into_iter().map(one));
    while live.len() < LIVE_ROWS {
        live.insert(0, Line::raw(""));
    }
    live
}

/// The height a turn reserves for its live area: three task rows and
/// the in-flight row. The blank row off the transcript and the turn
/// line are counted where they render.
const LIVE_ROWS: usize = 3 + 1;

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
        // Everything the last step produced, then the pane.
        out.flush()?;
        let state = engine.state().clone();
        status.apply_state(&state);
        status.mode = engine.mode().to_owned();
        if let Some(p) = engine.project() {
            status.project = p.to_owned();
        }
        status.title = engine.title().map(str::to_owned);
        status.usage = engine.usage();
        // The turn line carries the clock while a turn runs.
        status.elapsed = None;
        let activity = engine.turn().map(|t| {
            let width = out.shell.width();
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
        // The turn reserves its live height from its start (issue #21):
        // the in-flight row, three task rows and the turn line,
        // bottom-anchored, so rows fill in without anything below them
        // moving. Blanks hold the place until they do; at idle there
        // is nothing to reserve.
        let mut block: Vec<Line<'static>> = Vec::new();
        if !matches!(state, ThreadState::Idle) {
            block.extend(live_rows(
                out.running.as_ref(),
                engine.tasks(),
                out.shell.width(),
            ));
        }
        block.extend(
            engine
                .menu()
                .map(|m| block_lines(m, out.shell.width()))
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
        let active = out.active_lines();
        let pane = Pane {
            active: &active,
            composer: &composer,
            status: &status.line(),
            hint: hint.as_deref(),
            block: &block,
            popup: &popup_lines,
            activity,
        };
        out.shell
            .fit(needed_rows(&pane), matches!(state, ThreadState::Idle))?;
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
    use aigentic_runtime::aigentic_core::{RiskClass, ToolCall};
    use serde_json::json;

    fn text(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
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

    /// A turn reserves its live height from its start (issue #21): the
    /// in-flight row and three task rows, bottom-anchored, so rows
    /// fill in without the pane's height ever moving.
    #[test]
    fn a_turn_reserves_its_live_height() {
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
        // The turn starts empty: the reserve is blank rows.
        assert_eq!(text(&live_rows(None, &[], 80)), vec!["", "", "", ""]);
        // Rows fill from the bottom — the count row comes with any
        // list — and the height does not move.
        let rows = live_rows(
            Some(&bash),
            &[task("write the code", TaskState::Active)],
            80,
        );
        assert_eq!(
            text(&rows),
            vec![
                "",
                "◦ cargo test --workspace",
                "▸ write the code",
                "0/1 done · ctrl-t for the list",
            ]
        );
        // Full: the call in flight and three task rows, no blanks.
        let tasks = vec![
            task("one", TaskState::Active),
            task("two", TaskState::Pending),
            task("three", TaskState::Pending),
        ];
        assert_eq!(
            text(&live_rows(Some(&bash), &tasks, 80)),
            vec![
                "◦ cargo test --workspace",
                "▸ one",
                "○ two",
                "0/3 done · ctrl-t for the list",
            ]
        );
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
