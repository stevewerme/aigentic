//! One table from keys to actions (plan section 4), printed by `/keys`.
//! What a key does depends on whether a turn runs and whether the
//! composer holds anything, so the table takes that context.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// What the client is doing when a key arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyContext {
    /// A turn runs (or waits on someone) on the thread.
    pub running: bool,
    pub composer_empty: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
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
    /// Ctrl-U: drop the draft.
    ClearDraft,
    /// Cancel the running turn, nothing posted.
    Interrupt,
    /// The draft was empty and idle: quit on the second press within a
    /// second, else arm.
    QuitArm,
    /// Ctrl-D with an empty composer.
    Quit,
    /// Esc on an empty idle composer: the second press within a second
    /// recalls the last message sent.
    RecallArm,
    /// Alt-Up: copy the last message sent into the composer (a queued
    /// message is already in the log, so it is copied, not withdrawn).
    Recall,
    /// Ctrl-T: the transcript pager — the whole checklist at its top,
    /// then the run so far.
    Transcript,
    None,
}

/// The key table.
pub fn action_for(key: &KeyEvent, ctx: KeyContext) -> Action {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        KeyCode::Enter if shift || alt => Action::Newline,
        KeyCode::Enter => Action::Submit,
        KeyCode::Char('j') if ctrl => Action::Newline,
        KeyCode::Char('c') if ctrl => match (ctx.running, ctx.composer_empty) {
            (true, _) => Action::Interrupt,
            (false, false) => Action::ClearDraft,
            (false, true) => Action::QuitArm,
        },
        KeyCode::Esc => match (ctx.running, ctx.composer_empty) {
            (true, _) => Action::Interrupt,
            (false, false) => Action::ClearDraft,
            (false, true) => Action::RecallArm,
        },
        KeyCode::Char('d') if ctrl => {
            if ctx.composer_empty {
                Action::Quit
            } else {
                Action::None
            }
        }
        KeyCode::Char('t') if ctrl => Action::Transcript,
        KeyCode::Char('a') if ctrl => Action::Home,
        KeyCode::Char('e') if ctrl => Action::End,
        KeyCode::Char('u') if ctrl => Action::ClearDraft,
        KeyCode::Char(c) if !ctrl => Action::Insert(c),
        KeyCode::Backspace => Action::Backspace,
        KeyCode::Delete => Action::Delete,
        KeyCode::Left => Action::Left,
        KeyCode::Right => Action::Right,
        KeyCode::Up if alt => Action::Recall,
        KeyCode::Up => Action::Up,
        KeyCode::Down => Action::Down,
        KeyCode::Home => Action::Home,
        KeyCode::End => Action::End,
        _ => Action::None,
    }
}

/// `/keys`.
pub const KEYS: &str = "\
Enter            send; while a turn runs: queue for the next turn (in the log at once); a prompt menu: pick the row
!text Enter      interrupt the running turn, then send this
Shift-Enter      new line (Ctrl-J where the terminal sends plain Enter)
Ctrl-C           turn running: interrupt · draft: clear it · empty: press again within a second to quit
Esc              turn running: interrupt · draft: clear it · a permission menu: answer with a reason · empty: Esc again recalls the last message sent
Alt-Up           copy the last message sent into the composer (a queued one stays queued)
Up / Down        move in the draft; on one line, walk the history; a prompt menu: move its selection
0-9              a prompt menu: pick that row
Space            a prompt menu: toggle a row (multi-select)
Ctrl-A / Ctrl-E  start / end of the line
Ctrl-U           clear the draft
Ctrl-T           the transcript pager: the whole checklist, then the run in full
Ctrl-D           quit when the composer is empty
y / a / n        a permission menu, hidden: once / allow more / no";

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }
    const IDLE_EMPTY: KeyContext = KeyContext {
        running: false,
        composer_empty: true,
    };
    const IDLE_DRAFT: KeyContext = KeyContext {
        running: false,
        composer_empty: false,
    };
    const RUNNING: KeyContext = KeyContext {
        running: true,
        composer_empty: true,
    };

    #[test]
    fn ctrl_c_and_esc_depend_on_the_context() {
        let ctrl_c = key(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(action_for(&ctrl_c, RUNNING), Action::Interrupt);
        assert_eq!(action_for(&ctrl_c, IDLE_DRAFT), Action::ClearDraft);
        assert_eq!(action_for(&ctrl_c, IDLE_EMPTY), Action::QuitArm);
        let esc = key(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(action_for(&esc, RUNNING), Action::Interrupt);
        assert_eq!(action_for(&esc, IDLE_DRAFT), Action::ClearDraft);
        assert_eq!(action_for(&esc, IDLE_EMPTY), Action::RecallArm);
    }

    #[test]
    fn the_rest_of_the_table() {
        assert_eq!(
            action_for(&key(KeyCode::Enter, KeyModifiers::NONE), IDLE_EMPTY),
            Action::Submit
        );
        assert_eq!(
            action_for(&key(KeyCode::Enter, KeyModifiers::SHIFT), IDLE_EMPTY),
            Action::Newline
        );
        assert_eq!(
            action_for(&key(KeyCode::Char('j'), KeyModifiers::CONTROL), IDLE_EMPTY),
            Action::Newline
        );
        assert_eq!(
            action_for(&key(KeyCode::Char('d'), KeyModifiers::CONTROL), IDLE_EMPTY),
            Action::Quit
        );
        assert_eq!(
            action_for(&key(KeyCode::Char('d'), KeyModifiers::CONTROL), IDLE_DRAFT),
            Action::None
        );
        assert_eq!(
            action_for(&key(KeyCode::Up, KeyModifiers::ALT), RUNNING),
            Action::Recall
        );
        assert_eq!(
            action_for(&key(KeyCode::Up, KeyModifiers::NONE), RUNNING),
            Action::Up
        );
        assert_eq!(
            action_for(&key(KeyCode::Char('t'), KeyModifiers::CONTROL), IDLE_EMPTY),
            Action::Transcript
        );
        assert_eq!(
            action_for(&key(KeyCode::Char('X'), KeyModifiers::SHIFT), IDLE_EMPTY),
            Action::Insert('X')
        );
        assert_eq!(
            action_for(&key(KeyCode::Tab, KeyModifiers::NONE), IDLE_EMPTY),
            Action::None
        );
    }
}
