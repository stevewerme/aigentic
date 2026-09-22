//! The client (phase 6): the engine that turns lines into requests and
//! notices into lines, and the two ways to drive it. At a terminal, the
//! ratatui shell: an inline viewport over the terminal's scrollback,
//! the composer, the status line. Without one (a pipe, a script), plain
//! lines in and out, which is what the README's acceptance items and
//! `exec` use.

pub mod commands;
pub mod composer;
pub mod engine;
pub mod status;
pub mod tui;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use aigentic_api::{Notice, ThreadState};
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use tokio::sync::mpsc;

use crate::app::composer::Composer;
use crate::app::engine::{ClientRepl, Printer};
use crate::app::status::Status;
use crate::app::tui::{Pane, Shell};

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

/// The shell's printer: finished lines to the scrollback, the tail kept
/// for the next draw. Lines that arrive before the shell is up, or
/// while a draw failed, are not lost: they wait in `pending`.
struct ShellOut {
    shell: Shell,
    tail: String,
    pending: Vec<String>,
}

impl Printer for ShellOut {
    fn line(&mut self, text: &str) {
        self.pending.push(text.to_owned());
    }
    fn tail(&mut self, text: &str) {
        self.tail = text.to_owned();
    }
}

impl ShellOut {
    fn flush(&mut self) -> anyhow::Result<()> {
        for line in std::mem::take(&mut self.pending) {
            self.shell.commit(&line)?;
        }
        Ok(())
    }
}

/// What a key asks for (step 4 grows this into the full table).
#[derive(Debug, PartialEq, Eq)]
enum Action {
    Submit,
    Newline,
    Insert(char),
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    ClearLine,
    /// Ctrl-C: clear the draft, or quit when it is already empty.
    ClearOrQuit,
    /// Ctrl-D: quit when the composer is empty.
    QuitIfEmpty,
    None,
}

fn action_for(key: &KeyEvent) -> Action {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        KeyCode::Enter if shift || alt => Action::Newline,
        KeyCode::Enter => Action::Submit,
        KeyCode::Char('j') if ctrl => Action::Newline,
        KeyCode::Char('c') if ctrl => Action::ClearOrQuit,
        KeyCode::Char('d') if ctrl => Action::QuitIfEmpty,
        KeyCode::Char('a') if ctrl => Action::Home,
        KeyCode::Char('e') if ctrl => Action::End,
        KeyCode::Char('u') if ctrl => Action::ClearLine,
        KeyCode::Char(c) if !ctrl => Action::Insert(c),
        KeyCode::Backspace => Action::Backspace,
        KeyCode::Delete => Action::Delete,
        KeyCode::Left => Action::Left,
        KeyCode::Right => Action::Right,
        KeyCode::Up => Action::Up,
        KeyCode::Down => Action::Down,
        KeyCode::Home => Action::Home,
        KeyCode::End => Action::End,
        _ => Action::None,
    }
}

async fn run_shell(
    mut engine: ClientRepl,
    mut notices: mpsc::Receiver<Notice>,
    history: PathBuf,
    project: String,
) -> anyhow::Result<()> {
    let mut out = ShellOut {
        shell: Shell::start()?,
        tail: String::new(),
        pending: Vec::new(),
    };
    let mut composer = Composer::with_history(&history);
    let mut status = Status {
        project,
        mode: engine.mode().to_owned(),
        ..Status::default()
    };
    let mut running_since: Option<Instant> = None;
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(TICK);

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
        let hint = match &state {
            ThreadState::Running { queued, .. } if *queued > 0 => {
                Some(format!("queued {queued} · ! sends now"))
            }
            _ if engine.prompting() => Some("answer on the line: y / a / n, or the text".into()),
            _ => None,
        };
        let pane = Pane {
            tail: &out.tail,
            composer: &composer,
            status: &status.line(),
            hint: hint.as_deref(),
        };
        out.shell.draw(&pane)?;
        if engine.quit_requested() {
            break;
        }

        tokio::select! {
            event = events.next() => {
                let Some(Ok(event)) = event else { break };
                match event {
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        match action_for(&key) {
                            Action::Submit => {
                                if let Some(text) = composer.take() {
                                    // The composer clears, so the message
                                    // itself goes to the transcript here.
                                    for (i, line) in text.lines().enumerate() {
                                        let prefix = if i == 0 { "> " } else { "  " };
                                        out.line(&format!("{prefix}{line}"));
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
                            Action::ClearLine => composer.clear(),
                            Action::ClearOrQuit => {
                                if composer.is_empty() {
                                    break;
                                }
                                composer.clear();
                            }
                            Action::QuitIfEmpty => {
                                if composer.is_empty() {
                                    break;
                                }
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

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn keys_map_to_actions() {
        assert_eq!(
            action_for(&key(KeyCode::Enter, KeyModifiers::NONE)),
            Action::Submit
        );
        assert_eq!(
            action_for(&key(KeyCode::Enter, KeyModifiers::SHIFT)),
            Action::Newline
        );
        assert_eq!(
            action_for(&key(KeyCode::Char('j'), KeyModifiers::CONTROL)),
            Action::Newline
        );
        assert_eq!(
            action_for(&key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Action::ClearOrQuit
        );
        assert_eq!(
            action_for(&key(KeyCode::Char('d'), KeyModifiers::CONTROL)),
            Action::QuitIfEmpty
        );
        assert_eq!(
            action_for(&key(KeyCode::Char('x'), KeyModifiers::NONE)),
            Action::Insert('x')
        );
        assert_eq!(
            action_for(&key(KeyCode::Char('X'), KeyModifiers::SHIFT)),
            Action::Insert('X')
        );
        assert_eq!(
            action_for(&key(KeyCode::Tab, KeyModifiers::NONE)),
            Action::None
        );
    }
}
