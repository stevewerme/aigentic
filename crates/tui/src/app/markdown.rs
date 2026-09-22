//! Light markdown (plan decision 17): bold, inline code, fenced code
//! blocks and bullets, which is what the model uses in every message.
//! Headings and tables stay raw. Line by line: the caller tracks
//! whether a fence is open, since lines are committed as they arrive.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// Whether `line` opens or closes a fence. The caller flips its state
/// and renders the fence line itself as a dim marker.
pub fn is_fence(line: &str) -> bool {
    line.trim_start().starts_with("```")
}

/// One line, styled. Inside a fence every line is code.
pub fn line(text: &str, fenced: bool) -> Line<'static> {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let code = Style::default().fg(Color::Cyan);
    if is_fence(text) {
        let lang = text.trim().trim_start_matches('`').trim();
        let marker = if lang.is_empty() {
            "│".to_owned()
        } else {
            format!("│ {lang}")
        };
        return Line::from(Span::styled(marker, dim));
    }
    if fenced {
        return Line::from(vec![
            Span::styled("│ ", dim),
            Span::styled(text.to_owned(), code),
        ]);
    }
    let mut spans = Vec::new();
    let mut rest = text;
    // A bullet at the start becomes a dot.
    let trimmed = rest.trim_start();
    let indent = &rest[..rest.len() - trimmed.len()];
    if let Some(after) = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
    {
        spans.push(Span::raw(format!("{indent}• ")));
        rest = after;
    }
    spans.extend(inline(rest, code));
    Line::from(spans)
}

/// `**bold**` and `` `code` `` within a line; everything else raw.
fn inline(text: &str, code: Style) -> Vec<Span<'static>> {
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let mut spans = Vec::new();
    let mut plain = String::new();
    let mut rest = text;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("**")
            && let Some(end) = after.find("**")
            && end > 0
        {
            flush(&mut plain, &mut spans);
            spans.push(Span::styled(after[..end].to_owned(), bold));
            rest = &after[end + 2..];
            continue;
        }
        if let Some(after) = rest.strip_prefix('`')
            && let Some(end) = after.find('`')
            && end > 0
        {
            flush(&mut plain, &mut spans);
            spans.push(Span::styled(after[..end].to_owned(), code));
            rest = &after[end + 1..];
            continue;
        }
        let c = rest.chars().next().expect("non-empty");
        plain.push(c);
        rest = &rest[c.len_utf8()..];
    }
    flush(&mut plain, &mut spans);
    spans
}

fn flush(plain: &mut String, spans: &mut Vec<Span<'static>>) {
    if !plain.is_empty() {
        spans.push(Span::raw(std::mem::take(plain)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(line: &Line<'_>) -> Vec<(String, bool, bool)> {
        line.spans
            .iter()
            .map(|s| {
                (
                    s.content.to_string(),
                    s.style.add_modifier.contains(Modifier::BOLD),
                    s.style.fg == Some(Color::Cyan),
                )
            })
            .collect()
    }

    #[test]
    fn bold_code_and_bullets() {
        let l = line("- use **cargo** and `fmt` now", false);
        assert_eq!(
            texts(&l),
            vec![
                ("• ".into(), false, false),
                ("use ".into(), false, false),
                ("cargo".into(), true, false),
                (" and ".into(), false, false),
                ("fmt".into(), false, true),
                (" now".into(), false, false),
            ]
        );
    }

    #[test]
    fn unbalanced_markers_stay_raw() {
        let l = line("a ** b ` c", false);
        assert_eq!(texts(&l), vec![("a ** b ` c".into(), false, false)]);
    }

    #[test]
    fn fences_and_fenced_lines() {
        assert!(is_fence("```rust"));
        assert!(!is_fence("`x`"));
        let open = line("```rust", false);
        assert_eq!(texts(&open)[0].0, "│ rust");
        let inner = line("let **x** = 1;", true);
        assert_eq!(texts(&inner)[1], ("let **x** = 1;".into(), false, true));
    }
}
