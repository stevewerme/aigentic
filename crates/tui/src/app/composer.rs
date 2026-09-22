//! The composer (phase 6 step 3, plan section 4): a small multi-line
//! editor owned here rather than a dependency, so the key table stays
//! in one place. Enter submits, a newline key adds a line; bracketed
//! paste inserts as-is, and a large paste becomes a placeholder
//! expanded on submit; Up and Down walk the history only when the
//! composer is empty or holds the recalled entry.

use std::path::{Path, PathBuf};

/// A paste at or over this many characters is shown as a placeholder.
pub const LARGE_PASTE_CHARS: usize = 1000;

/// How many rows the composer may take before it scrolls.
pub const MAX_ROWS: usize = 6;

/// How many history entries are kept in the file.
const HISTORY_KEEP: usize = 1000;

#[derive(Debug)]
pub struct Composer {
    lines: Vec<String>,
    /// Cursor: line index and char index within it.
    row: usize,
    col: usize,
    history: Vec<String>,
    /// While walking the history: the index shown, and the draft that
    /// was in the composer before the walk.
    walk: Option<(usize, String)>,
    /// Placeholders for large pastes and what they stand for.
    pastes: Vec<(String, String)>,
    history_path: Option<PathBuf>,
}

impl Default for Composer {
    fn default() -> Self {
        Self {
            lines: vec![String::new()],
            row: 0,
            col: 0,
            history: Vec::new(),
            walk: None,
            pastes: Vec::new(),
            history_path: None,
        }
    }
}

impl Composer {
    /// A composer whose history is read from and appended to `path`.
    /// One entry per line; a newline inside an entry is stored as `\n`.
    /// Lines starting with `#` are skipped (rustyline's old header).
    pub fn with_history(path: &Path) -> Self {
        let history = std::fs::read_to_string(path)
            .map(|s| {
                s.lines()
                    .filter(|l| !l.is_empty() && !l.starts_with('#'))
                    .map(unescape)
                    .collect()
            })
            .unwrap_or_default();
        Self {
            history,
            history_path: Some(path.to_owned()),
            ..Self::default()
        }
    }

    pub fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    /// The text as typed, placeholders included.
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// (row, col) of the cursor in chars.
    pub fn cursor(&self) -> (usize, usize) {
        (self.row, self.col)
    }

    pub fn insert_char(&mut self, c: char) {
        self.walk = None;
        let line = &mut self.lines[self.row];
        let at = byte_at(line, self.col);
        line.insert(at, c);
        self.col += 1;
    }

    /// Typed or pasted text; newlines split lines.
    pub fn insert_str(&mut self, text: &str) {
        for c in text.chars() {
            if c == '\n' {
                self.newline();
            } else if c != '\r' {
                self.insert_char(c);
            }
        }
    }

    /// A bracketed paste: inserted as-is, or as a placeholder when large.
    pub fn paste(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n");
        if text.chars().count() >= LARGE_PASTE_CHARS {
            let n = self.pastes.len() + 1;
            let placeholder = format!("[pasted {} chars #{n}]", text.chars().count());
            self.insert_str(&placeholder);
            self.pastes.push((placeholder, text));
        } else {
            self.insert_str(&text);
        }
    }

    pub fn newline(&mut self) {
        self.walk = None;
        let line = &mut self.lines[self.row];
        let at = byte_at(line, self.col);
        let rest = line.split_off(at);
        self.lines.insert(self.row + 1, rest);
        self.row += 1;
        self.col = 0;
    }

    pub fn backspace(&mut self) {
        self.walk = None;
        if self.col > 0 {
            let line = &mut self.lines[self.row];
            let at = byte_at(line, self.col - 1);
            line.remove(at);
            self.col -= 1;
        } else if self.row > 0 {
            let line = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.lines[self.row].chars().count();
            self.lines[self.row].push_str(&line);
        }
    }

    pub fn delete(&mut self) {
        self.walk = None;
        let len = self.lines[self.row].chars().count();
        if self.col < len {
            let line = &mut self.lines[self.row];
            let at = byte_at(line, self.col);
            line.remove(at);
        } else if self.row + 1 < self.lines.len() {
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&next);
        }
    }

    pub fn left(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.lines[self.row].chars().count();
        }
    }

    pub fn right(&mut self) {
        if self.col < self.lines[self.row].chars().count() {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    pub fn home(&mut self) {
        self.col = 0;
    }

    pub fn end(&mut self) {
        self.col = self.lines[self.row].chars().count();
    }

    /// Up: the line above, else the previous history entry when the
    /// composer is empty or shows a recalled entry.
    pub fn up(&mut self) {
        if self.row > 0 {
            self.row -= 1;
            self.col = self.col.min(self.lines[self.row].chars().count());
            return;
        }
        let at = match &self.walk {
            Some((i, _)) => *i,
            None if self.is_empty() => self.history.len(),
            None => return,
        };
        if at == 0 {
            return;
        }
        let draft = match self.walk.take() {
            Some((_, draft)) => draft,
            None => self.text(),
        };
        self.set_text(&self.history[at - 1].clone());
        self.walk = Some((at - 1, draft));
    }

    /// Down: the line below, else the next history entry, else the draft.
    pub fn down(&mut self) {
        if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = self.col.min(self.lines[self.row].chars().count());
            return;
        }
        let Some((i, draft)) = self.walk.take() else {
            return;
        };
        if i + 1 < self.history.len() {
            self.set_text(&self.history[i + 1].clone());
            self.walk = Some((i + 1, draft));
        } else {
            self.set_text(&draft);
        }
    }

    /// Replace the whole text; the cursor goes to the end.
    pub fn set_text(&mut self, text: &str) {
        self.lines = text.split('\n').map(str::to_owned).collect();
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.row = self.lines.len() - 1;
        self.col = self.lines[self.row].chars().count();
    }

    pub fn clear(&mut self) {
        self.set_text("");
        self.pastes.clear();
        self.walk = None;
    }

    /// Submit: the text with pastes expanded, recorded in the history;
    /// the composer is empty after. `None` when there is nothing.
    pub fn take(&mut self) -> Option<String> {
        let typed = self.text();
        if typed.trim().is_empty() {
            self.clear();
            return None;
        }
        let mut text = typed.clone();
        for (placeholder, full) in self.pastes.drain(..) {
            text = text.replacen(&placeholder, &full, 1);
        }
        if self.history.last() != Some(&typed) {
            self.history.push(typed.clone());
            if let Some(path) = &self.history_path {
                let _ = append_history(path, &typed);
            }
        }
        self.clear();
        Some(text)
    }

    /// The most recent history entry, for Esc-Esc (step 4).
    #[cfg(test)]
    pub fn last_entry(&self) -> Option<&str> {
        self.history.last().map(String::as_str)
    }
}

fn byte_at(line: &str, col: usize) -> usize {
    line.char_indices().nth(col).map_or(line.len(), |(i, _)| i)
}

fn escape(entry: &str) -> String {
    entry.replace('\\', "\\\\").replace('\n', "\\n")
}

fn unescape(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some(other) => out.push(other),
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Append one entry; trim the file to the last `HISTORY_KEEP` entries
/// now and then.
fn append_history(path: &Path, entry: &str) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(f, "{}", escape(entry))?;
    let content = std::fs::read_to_string(path)?;
    let lines: Vec<&str> = content.lines().collect();
    if lines.len() > HISTORY_KEEP + 200 {
        let keep = &lines[lines.len() - HISTORY_KEEP..];
        std::fs::write(path, keep.join("\n") + "\n")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_newlines_and_backspace_across_lines() {
        let mut c = Composer::default();
        c.insert_str("ab");
        c.newline();
        c.insert_str("cd");
        assert_eq!(c.text(), "ab\ncd");
        assert_eq!(c.cursor(), (1, 2));
        c.home();
        c.backspace();
        assert_eq!(c.text(), "abcd");
        assert_eq!(c.cursor(), (0, 2));
        c.insert_char('é');
        assert_eq!(c.text(), "abécd");
        c.left();
        c.delete();
        assert_eq!(c.text(), "abcd");
    }

    #[test]
    fn take_returns_the_text_and_records_history() {
        let mut c = Composer::default();
        c.insert_str("hello");
        assert_eq!(c.take().as_deref(), Some("hello"));
        assert!(c.is_empty());
        assert_eq!(c.last_entry(), Some("hello"));
        c.insert_str("   ");
        assert_eq!(c.take(), None);
    }

    #[test]
    fn history_walks_only_from_an_empty_or_recalled_composer() {
        let mut c = Composer::default();
        c.insert_str("one");
        c.take();
        c.insert_str("two");
        c.take();
        c.up();
        assert_eq!(c.text(), "two");
        c.up();
        assert_eq!(c.text(), "one");
        c.up();
        assert_eq!(c.text(), "one");
        c.down();
        assert_eq!(c.text(), "two");
        c.down();
        assert_eq!(c.text(), "");
        // A draft is kept while walking and restored after.
        c.insert_str("dra");
        c.up();
        assert_eq!(c.text(), "dra", "a draft does not walk");
        c.clear();
        c.up();
        c.insert_char('!');
        assert_eq!(c.text(), "two!");
        assert!(c.walk.is_none(), "typing ends the walk");
    }

    #[test]
    fn up_and_down_move_within_a_multi_line_draft_first() {
        let mut c = Composer::default();
        c.insert_str("first\nsecond");
        c.up();
        assert_eq!(c.cursor(), (0, 5));
        c.down();
        assert_eq!(c.cursor(), (1, 5));
    }

    #[test]
    fn a_large_paste_is_a_placeholder_until_submit() {
        let mut c = Composer::default();
        let big = "x".repeat(LARGE_PASTE_CHARS);
        c.insert_str("see: ");
        c.paste(&big);
        assert_eq!(
            c.text(),
            format!("see: [pasted {LARGE_PASTE_CHARS} chars #1]")
        );
        let sent = c.take().unwrap();
        assert_eq!(sent, format!("see: {big}"));
        // History keeps the placeholder, not the paste.
        assert!(c.last_entry().unwrap().contains("[pasted"));
        let mut c = Composer::default();
        c.paste("small\r\npaste");
        assert_eq!(c.text(), "small\npaste");
    }

    #[test]
    fn history_round_trips_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history");
        std::fs::write(&path, "#V2\nold entry\n").unwrap();
        let mut c = Composer::with_history(&path);
        c.insert_str("two\nlines");
        c.take();
        let c2 = Composer::with_history(&path);
        assert_eq!(
            c2.history,
            vec!["old entry".to_owned(), "two\nlines".to_owned()]
        );
    }
}
