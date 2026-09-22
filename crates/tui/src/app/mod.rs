//! The client (phase 6): the engine that turns lines into requests and
//! notices into lines, and the two ways to drive it. At a terminal, the
//! ratatui shell: an inline viewport over the terminal's scrollback,
//! the composer, the status line. Without one (a pipe, a script), plain
//! lines in and out, which is what the README's acceptance items and
//! `exec` use.

pub mod cells;
pub mod commands;
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
use crate::app::composer::Composer;
use crate::app::engine::{ClientRepl, Printer};
use crate::app::keymap::{Action, KeyContext, action_for};
use crate::app::pager::Pager;
use crate::app::status::Status;
use crate::app::tui::{Pane, Shell, wrap_line};
use ratatui::text::Line;

/// How often the status line's clock is redrawn while a turn runs.
const TICK: Duration = Duration::from_millis(250);

/// Run the client: the shell at a terminal, plain lines otherwise.
pub async fn run(
    engine: ClientRepl,
    notices: mpsc::Receiver<Notice>,
    history: PathBuf,
    project: String,
) -> anyhow::Result<()> {
    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        run_shell(engine, notices, history, project).await
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
) -> anyhow::Result<()> {
    let mut out = ShellOut::new(Shell::start()?);
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
        let hint = match (&armed, &state) {
            (Some((Action::QuitArm, _)), _) => Some("Ctrl-C again to quit".to_owned()),
            (Some((Action::RecallArm, _)), _) => {
                Some("Esc again to recall the last message".to_owned())
            }
            (_, ThreadState::Running { queued, .. }) if *queued > 0 => Some(format!(
                "queued {queued} · ! sends now · Alt-Up copies the last back"
            )),
            _ if engine.prompting() => Some("answer on the line: y / a / n, or the text".into()),
            _ => None,
        };
        let active = out.active_lines();
        let pane = Pane {
            active: &active,
            composer: &composer,
            status: &status.line(),
            hint: hint.as_deref(),
        };
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
