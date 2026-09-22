//! Unified diffs for the file tools' results, so a client can show an
//! edit as a patch and the model sees exactly what changed.

use std::path::Path;

/// A unified diff of `old` to `new` with `--- a/path` / `+++ b/path`
/// headers, three lines of context, ending in a newline. Empty when
/// nothing changed.
pub fn unified(path: &Path, old: &str, new: &str) -> String {
    if old == new {
        return String::new();
    }
    let shown = path.display().to_string();
    similar::TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .header(&format!("a/{shown}"), &format!("b/{shown}"))
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_and_hunk() {
        let d = unified(Path::new("src/x.rs"), "a\nb\nc\n", "a\nB\nc\n");
        assert!(d.starts_with("--- a/src/x.rs\n+++ b/src/x.rs\n@@"), "{d}");
        assert!(d.contains("-b\n+B\n"), "{d}");
        assert!(d.ends_with('\n'));
    }

    #[test]
    fn unchanged_is_empty() {
        assert_eq!(unified(Path::new("f"), "same\n", "same\n"), "");
    }

    #[test]
    fn new_file_is_all_additions() {
        let d = unified(Path::new("f"), "", "one\ntwo\n");
        assert!(d.contains("+one\n+two\n"), "{d}");
    }
}
