//! Completion (plan section 4): `@` opens a fuzzy file picker over the
//! project root (the `ignore` crate walks it, `nucleo` matches), `/`
//! the command list with descriptions. Tab or Enter accepts, Esc
//! closes, Up and Down move; typing narrows.

use std::path::Path;

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher};

/// How many matches the popup shows.
pub const POPUP_ROWS: usize = 6;
/// How many files the index keeps; a bigger tree is cut, not walked to
/// the end.
const MAX_FILES: usize = 20_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Files,
    Commands,
}

/// The project's files, relative paths with `/`, walked once.
pub struct FileIndex {
    paths: Vec<String>,
}

impl FileIndex {
    pub fn walk(root: &Path) -> Self {
        let mut paths = Vec::new();
        for entry in ignore::WalkBuilder::new(root)
            .hidden(true)
            .git_ignore(true)
            .build()
            .flatten()
        {
            if entry.file_type().is_some_and(|t| t.is_file())
                && let Ok(rel) = entry.path().strip_prefix(root)
            {
                paths.push(rel.to_string_lossy().replace('\\', "/"));
                if paths.len() >= MAX_FILES {
                    break;
                }
            }
        }
        paths.sort();
        Self { paths }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.paths.len()
    }
}

/// An open popup: what is being completed and the matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Popup {
    pub kind: Kind,
    /// Where the token starts in the composer's current line (chars).
    pub start: usize,
    pub query: String,
    /// (item, description); the description is empty for files.
    pub items: Vec<(String, String)>,
    pub selected: usize,
}

impl Popup {
    pub fn up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn down(&mut self) {
        if self.selected + 1 < self.items.len() {
            self.selected += 1;
        }
    }

    /// The text that replaces the token when accepted.
    pub fn accepted(&self) -> Option<String> {
        let (item, _) = self.items.get(self.selected)?;
        Some(match self.kind {
            Kind::Files => {
                if item.contains(' ') {
                    format!("@\"{item}\" ")
                } else {
                    format!("@{item} ")
                }
            }
            Kind::Commands => format!("/{item} "),
        })
    }
}

/// The token under the cursor that a popup completes: `@query` or, at
/// the line's start, `/query`. `(start, kind, query)`.
pub fn token_at(line: &str, cursor: usize) -> Option<(usize, Kind, String)> {
    let chars: Vec<char> = line.chars().collect();
    let cursor = cursor.min(chars.len());
    let mut start = cursor;
    while start > 0 && !chars[start - 1].is_whitespace() {
        start -= 1;
    }
    let token: String = chars[start..cursor].iter().collect();
    if let Some(q) = token.strip_prefix('@') {
        return Some((start, Kind::Files, q.to_owned()));
    }
    if start == 0
        && let Some(q) = token.strip_prefix('/')
    {
        return Some((0, Kind::Commands, q.to_owned()));
    }
    None
}

/// The best matches for `query` among `candidates`, best first, at most
/// `POPUP_ROWS`. An empty query lists the first few.
pub fn matches<'a>(
    query: &str,
    candidates: impl IntoIterator<Item = (&'a str, &'a str)>,
    paths: bool,
) -> Vec<(String, String)> {
    let mut config = Config::DEFAULT;
    if paths {
        config.set_match_paths();
    }
    let mut matcher = Matcher::new(config);
    let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
    let candidates: Vec<(&str, &str)> = candidates.into_iter().collect();
    let names: Vec<&str> = candidates.iter().map(|(n, _)| *n).collect();
    let mut scored = pattern.match_list(names.iter().copied(), &mut matcher);
    if query.is_empty() {
        scored.truncate(POPUP_ROWS);
    } else {
        scored.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        scored.truncate(POPUP_ROWS);
    }
    scored
        .into_iter()
        .map(|(name, _)| {
            let desc = candidates
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, d)| (*d).to_owned())
                .unwrap_or_default();
            (name.to_owned(), desc)
        })
        .collect()
}

/// A popup for the token, or none when there is no token or no match.
pub fn open(
    line: &str,
    cursor: usize,
    files: &FileIndex,
    commands: &[(String, String)],
) -> Option<Popup> {
    let (start, kind, query) = token_at(line, cursor)?;
    let items = match kind {
        Kind::Files => matches(&query, files.paths.iter().map(|p| (p.as_str(), "")), true),
        Kind::Commands => matches(
            &query,
            commands.iter().map(|(n, d)| (n.as_str(), d.as_str())),
            false,
        ),
    };
    if items.is_empty() {
        return None;
    }
    Some(Popup {
        kind,
        start,
        query,
        items,
        selected: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_found_under_the_cursor() {
        assert_eq!(
            token_at("see @src/ma", 11),
            Some((4, Kind::Files, "src/ma".into()))
        );
        assert_eq!(token_at("/co", 3), Some((0, Kind::Commands, "co".into())));
        assert_eq!(token_at("a /co", 5), None, "a command only at the start");
        assert_eq!(token_at("plain", 5), None);
        assert_eq!(token_at("@x done", 2), Some((0, Kind::Files, "x".into())));
    }

    #[test]
    fn files_match_fuzzily_and_commands_by_prefix() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/app")).unwrap();
        std::fs::write(dir.path().join("src/app/mod.rs"), "").unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "").unwrap();
        std::fs::write(dir.path().join("README.md"), "").unwrap();
        std::fs::write(dir.path().join(".gitignore"), "target\n").unwrap();
        std::fs::create_dir_all(dir.path().join("target")).unwrap();
        std::fs::write(dir.path().join("target/x.o"), "").unwrap();
        let files = FileIndex::walk(dir.path());
        assert_eq!(files.len(), 4, "{:?}", files.paths);
        let commands = vec![
            ("cost".to_owned(), "tokens".to_owned()),
            ("compact".to_owned(), "compact now".to_owned()),
            ("keys".to_owned(), "the table".to_owned()),
        ];
        let p = open("@main", 5, &files, &commands).unwrap();
        assert_eq!(p.items[0].0, "src/main.rs");
        assert_eq!(p.accepted().as_deref(), Some("@src/main.rs "));
        let p = open("/co", 3, &files, &commands).unwrap();
        let names: Vec<&str> = p.items.iter().map(|(n, _)| n.as_str()).collect();
        assert!(
            names.contains(&"cost") && names.contains(&"compact"),
            "{names:?}"
        );
        assert!(!names.contains(&"keys"));
        assert!(open("/zzz", 4, &files, &commands).is_none());
    }
}
