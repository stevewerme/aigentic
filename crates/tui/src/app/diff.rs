//! Unified diffs to styled lines (plan section 5): added lines on a
//! green background, removed on red, hunk headers cyan, file headers
//! dim. The tools already put a diff at the head of every edit result
//! (step 1); this renders it.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// What an edit result parses into: the path, the counts and the diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    pub path: String,
    pub added: usize,
    pub removed: usize,
    pub diff: String,
    /// The summary line after the diff (`edited … at line N`).
    pub summary: String,
}

/// An edit tool's result: the unified diff up to the summary line. None
/// when the result carries no diff (nothing changed, or an error).
pub fn parse_edit_result(content: &str) -> Option<Edit> {
    if !content.starts_with("--- ") {
        return None;
    }
    let mut path = String::new();
    let mut added = 0;
    let mut removed = 0;
    let mut diff = String::new();
    let mut summary = String::new();
    for line in content.lines() {
        if let Some(p) = line.strip_prefix("+++ b/") {
            path = p.to_owned();
        }
        let is_diff_line = line.starts_with("--- ")
            || line.starts_with("+++ ")
            || line.starts_with("@@")
            || line.starts_with('+')
            || line.starts_with('-')
            || line.starts_with(' ')
            || line.starts_with('\\')
            || line.is_empty();
        if is_diff_line && summary.is_empty() {
            if line.starts_with('+') && !line.starts_with("+++ ") {
                added += 1;
            } else if line.starts_with('-') && !line.starts_with("--- ") {
                removed += 1;
            }
            diff.push_str(line);
            diff.push('\n');
        } else {
            summary = line.to_owned();
        }
    }
    Some(Edit {
        path,
        added,
        removed,
        diff,
        summary,
    })
}

/// One diff line, styled.
pub fn line(text: &str) -> Line<'static> {
    let style = if text.starts_with("+++ ") || text.starts_with("--- ") {
        Style::default().add_modifier(Modifier::DIM)
    } else if text.starts_with("@@") {
        Style::default().fg(Color::Cyan)
    } else if text.starts_with('+') {
        Style::default()
            .fg(Color::Green)
            .bg(Color::Rgb(0x21, 0x3a, 0x2b))
    } else if text.starts_with('-') {
        Style::default()
            .fg(Color::Red)
            .bg(Color::Rgb(0x4a, 0x22, 0x1d))
    } else {
        Style::default()
    };
    Line::from(Span::styled(text.to_owned(), style))
}

/// A whole diff, styled.
pub fn lines(diff: &str) -> Vec<Line<'static>> {
    diff.lines().map(line).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const RESULT: &str = "--- a/src/x.rs\n+++ b/src/x.rs\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\nedited /tmp/src/x.rs at line 2";

    #[test]
    fn an_edit_result_parses_into_path_counts_and_diff() {
        let e = parse_edit_result(RESULT).unwrap();
        assert_eq!(e.path, "src/x.rs");
        assert_eq!((e.added, e.removed), (1, 1));
        assert!(e.diff.ends_with(" c\n"), "{}", e.diff);
        assert_eq!(e.summary, "edited /tmp/src/x.rs at line 2");
        assert!(parse_edit_result("wrote 4 bytes to f").is_none());
    }

    #[test]
    fn lines_are_coloured_by_kind() {
        let ls = lines("--- a/f\n+++ b/f\n@@ -1 +1 @@\n-old\n+new\n same");
        assert_eq!(ls[2].spans[0].style.fg, Some(Color::Cyan));
        assert_eq!(ls[3].spans[0].style.fg, Some(Color::Red));
        assert_eq!(ls[4].spans[0].style.fg, Some(Color::Green));
        assert_eq!(ls[5].spans[0].style.fg, None);
    }
}
