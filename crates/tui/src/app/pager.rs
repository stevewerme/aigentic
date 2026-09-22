//! The transcript pager (plan section 5): every cell at full length in
//! the alternate screen. Arrows, PageUp and PageDown scroll, `g` and
//! `G` jump, `/` searches forward, `n` again, `q` or Esc leaves. Pure
//! state here; the shell draws it.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

pub struct Pager {
    title: String,
    lines: Vec<Line<'static>>,
    /// The first line shown.
    top: usize,
    /// A search being typed, or the last one.
    query: String,
    typing: bool,
}

impl Pager {
    /// Open at the end, where the newest lines are.
    pub fn new(title: &str, lines: Vec<Line<'static>>) -> Self {
        Self {
            title: title.to_owned(),
            top: lines.len(),
            lines,
            query: String::new(),
            typing: false,
        }
    }

    /// A key; `true` when the pager closes.
    pub fn key(&mut self, code: crossterm::event::KeyCode, height: usize) -> bool {
        use crossterm::event::KeyCode as K;
        let page = height.saturating_sub(1).max(1);
        if self.typing {
            match code {
                K::Esc => {
                    self.typing = false;
                    self.query.clear();
                }
                K::Enter => {
                    self.typing = false;
                    self.find_from(self.top + 1, height);
                }
                K::Backspace => {
                    self.query.pop();
                }
                K::Char(c) => self.query.push(c),
                _ => {}
            }
            return false;
        }
        match code {
            K::Char('q') | K::Esc => return true,
            K::Up | K::Char('k') => self.top = self.top.saturating_sub(1),
            K::Down | K::Char('j') => self.top += 1,
            K::PageUp => self.top = self.top.saturating_sub(page),
            K::PageDown | K::Char(' ') => self.top += page,
            K::Char('g') | K::Home => self.top = 0,
            K::Char('G') | K::End => self.top = self.lines.len(),
            K::Char('/') => {
                self.typing = true;
                self.query.clear();
            }
            K::Char('n') => self.find_from(self.top + 1, height),
            _ => {}
        }
        self.clamp(height);
        false
    }

    fn clamp(&mut self, height: usize) {
        let max_top = self.lines.len().saturating_sub(height);
        if self.top > max_top {
            self.top = max_top;
        }
    }

    /// The next line at or after `from` containing the query; wraps.
    fn find_from(&mut self, from: usize, height: usize) {
        if self.query.is_empty() || self.lines.is_empty() {
            return;
        }
        let q = self.query.to_lowercase();
        let n = self.lines.len();
        for i in 0..n {
            let idx = (from + i) % n;
            let text: String = self.lines[idx]
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect();
            if text.to_lowercase().contains(&q) {
                self.top = idx;
                self.clamp(height);
                return;
            }
        }
    }

    /// The rows to draw in `area`: the visible lines and a footer.
    pub fn view(&mut self, area: Rect) -> Vec<Line<'static>> {
        let height = area.height as usize;
        let body = height.saturating_sub(1);
        self.clamp(body.max(1));
        let mut rows: Vec<Line<'static>> = self
            .lines
            .iter()
            .skip(self.top)
            .take(body)
            .cloned()
            .collect();
        while rows.len() < body {
            rows.push(Line::raw(""));
        }
        let footer = if self.typing {
            format!("/{}", self.query)
        } else {
            format!(
                "{} · line {} of {} · q leaves · / searches · g G top bottom",
                self.title,
                (self.top + 1).min(self.lines.len().max(1)),
                self.lines.len()
            )
        };
        rows.push(Line::from(Span::styled(
            footer,
            Style::default().add_modifier(Modifier::REVERSED),
        )));
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyCode;

    fn notes(n: usize) -> Vec<Line<'static>> {
        (1..=n).map(|i| Line::raw(format!("note {i}"))).collect()
    }

    fn top_text(p: &mut Pager, h: u16) -> String {
        let rows = p.view(Rect::new(0, 0, 40, h));
        rows[0].spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn opens_at_the_end_and_scrolls() {
        let mut p = Pager::new("t", notes(20));
        assert_eq!(top_text(&mut p, 5), "note 17");
        p.key(KeyCode::Up, 5);
        assert_eq!(top_text(&mut p, 5), "note 16");
        p.key(KeyCode::Char('g'), 5);
        assert_eq!(top_text(&mut p, 5), "note 1");
        p.key(KeyCode::PageDown, 5);
        assert_eq!(top_text(&mut p, 5), "note 5");
        assert!(p.key(KeyCode::Char('q'), 5));
    }

    #[test]
    fn search_finds_forward_and_wraps() {
        let mut p = Pager::new("t", notes(20));
        p.key(KeyCode::Char('g'), 5);
        p.key(KeyCode::Char('/'), 5);
        for c in "note 7".chars() {
            p.key(KeyCode::Char(c), 5);
        }
        p.key(KeyCode::Enter, 5);
        assert_eq!(top_text(&mut p, 5), "note 7");
        p.key(KeyCode::Char('n'), 5);
        assert_eq!(top_text(&mut p, 5), "note 7", "wraps to the only match");
    }
}
