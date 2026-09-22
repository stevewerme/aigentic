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
pub mod markdown;
pub mod pager;
pub mod status;
pub mod tui;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use aigentic_api::{Notice, ThreadState};
use crossterm::event::{Event, EventStream, KeyEventKind};
use futures_util::StreamExt;
use tokio::sync::mpsc;

use crate::app::cells::{Cell, ToolState, is_read_tool};
use crate::app::completion::{FileIndex, Popup};
use crate::app::composer::Composer;
use crate::app::engine::{ClientRepl, Printer, PromptBlock};
use crate::app::keymap::{Action, KeyContext, action_for};
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
        }
    }

    fn commit(&mut self, cell: Cell) {
        let width = self.shell.width();
        self.pending.extend(cell.styled(width));
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
            lines.extend(Cell::Explored(self.explored.clone()).styled(width));
        }
        if let Some(cell) = &self.running {
            lines.extend(cell.styled(width));
        }
        if !self.tail.is_empty() {
            lines.extend(wrap_line(&markdown::line(&self.tail, self.fenced), width));
        }
        lines
    }
}

impl Printer for ShellOut {
    fn line(&mut self, text: &str) {
        self.flush_explored();
        self.commit(Cell::Note(text.to_owned()));
    }

    fn prompt(&mut self, _block: &PromptBlock) {
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
                    state,
                    ..
                },
                true,
            ) => {
                self.running = None;
                if is_read_tool(name) && *state == ToolState::Ok {
                    self.explored.push(format!("{name} {summary}"));
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

/// The prompt block's lines.
fn block_lines(block: &PromptBlock, width: usize) -> Vec<Line<'static>> {
    let head = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().add_modifier(Modifier::DIM);
    let lines = match block {
        PromptBlock::Permission {
            tool,
            class,
            reason,
            summary,
            prefix,
        } => {
            let p = match prefix {
                Some(p) => format!(" · p allow `{}` from now on", p.join(" ")),
                None => String::new(),
            };
            vec![
                Line::from(Span::styled(
                    format!("permission · {tool} ({class}) · {reason}"),
                    head,
                )),
                Line::from(Span::raw(format!("  {summary}"))),
                Line::from(Span::styled(
                    format!("  y once · a this session{p} · n deny · esc deny with a reason"),
                    dim,
                )),
            ]
        }
        PromptBlock::Question { question } => vec![
            Line::from(Span::styled(format!("question · {question}"), head)),
            Line::from(Span::styled("  type the answer and press Enter", dim)),
        ],
    };
    lines.iter().flat_map(|l| wrap_line(l, width)).collect()
}

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
    let mut running_since: Option<Instant> = None;
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(TICK);
    // Ctrl-C and Esc arm on an empty idle composer; the second press
    // within the window quits or recalls.
    let mut armed: Option<(Action, Instant)> = None;
    // The last message sent (not a command), for Esc-Esc and Alt-Up.
    let mut last_sent: Option<String> = None;
    // Esc on a permission prompt: the composer takes the reason; the
    // draft it held comes back after.
    let mut reason_draft: Option<String> = None;

    // A thread opened while it waits: the prompt is shown at once.
    let state = engine.state().clone();
    engine.show_state(&state, &mut out);

    loop {
        // Everything the last step produced, then the pane.
        out.flush()?;
        let state = engine.state().clone();
        status.apply_state(&state);
        status.mode = engine.mode().to_owned();
        status.usage = engine.usage();
        running_since = match (&state, running_since) {
            (ThreadState::Idle, _) => None,
            (_, Some(t)) => Some(t),
            (_, None) => Some(Instant::now()),
        };
        status.elapsed = running_since.map(|t| t.elapsed());
        if let Some((_, at)) = armed
            && at.elapsed() > ARM_WINDOW
        {
            armed = None;
        }
        let block = engine
            .prompt_block()
            .map(|b| block_lines(b, out.shell.width()))
            .unwrap_or_default();
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
                Some(_) if reason_draft.is_none() => {
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
            _ if reason_draft.is_some() => {
                Some("deny with a reason · Enter sends · Esc cancels".to_owned())
            }
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
                        // A permission prompt takes single keys first.
                        if let Some(PromptBlock::Permission { prefix, .. }) =
                            engine.prompt_block().cloned()
                        {
                            use crossterm::event::KeyCode as K;
                            if let Some(draft) = reason_draft.clone() {
                                match key.code {
                                    K::Enter => {
                                        let reason = composer.take().unwrap_or_default();
                                        composer.set_text(&draft);
                                        reason_draft = None;
                                        engine
                                            .decide(false, false, None, Some(reason), &mut out)
                                            .await;
                                        continue;
                                    }
                                    K::Esc => {
                                        composer.set_text(&draft);
                                        reason_draft = None;
                                        continue;
                                    }
                                    _ => {}
                                }
                            } else {
                                let plain = key.modifiers.is_empty()
                                    || key.modifiers == crossterm::event::KeyModifiers::SHIFT;
                                let handled = match key.code {
                                    K::Char('y') if plain => Some((true, false, None)),
                                    K::Char('a') if plain => Some((true, true, None)),
                                    K::Char('p') if plain => {
                                        Some((true, prefix.is_none(), prefix.clone()))
                                    }
                                    K::Char('n') if plain => Some((false, false, None)),
                                    _ => None,
                                };
                                if let Some((allow, session, prefix)) = handled {
                                    engine.decide(allow, session, prefix, None, &mut out).await;
                                    continue;
                                }
                                if key.code == K::Esc {
                                    reason_draft = Some(composer.text());
                                    composer.clear();
                                    continue;
                                }
                            }
                        }
                        // An open popup takes the navigation keys.
                        if let Some(p) = popup.as_mut() {
                            use crossterm::event::KeyCode as K;
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
                                let lines: Vec<Line<'static>> =
                                    out.transcript.iter().flat_map(Cell::full).collect();
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
