//! Briefs (issue #123): a project's `.aigentic/brief.md` and a
//! workspace's `<shared>/workspace/brief.md`, hand-written Markdown that
//! says what a project or workspace is and where it stands. The whole
//! brief goes inline in the prefix, capped; its first non-empty line is
//! the one-liner the projects block prints for a sibling, and
//! `read_brief` opens a sibling's whole brief on demand.
//!
//! The recommended shape is a `## Foundation` section (what it is and
//! who it is for, the stack, the commands, hard rules, relationships as
//! edges) then a `## Current state` section (what changed, what is open,
//! what is next, a date). It is documented, not enforced: nothing here
//! parses those headings.

use std::path::{Path, PathBuf};

use crate::project::DOT_DIR;

/// The brief's file name, in a project's `.aigentic/` and in a
/// workspace's `workspace/` folder beside its `instructions.md`.
pub const BRIEF_FILE: &str = "brief.md";

/// Bytes of a project's brief kept inline in the prefix, note included.
pub const PROJECT_BRIEF_CAP: usize = 2_000;

/// Bytes of a workspace's brief kept inline in the prefix, note included.
pub const WORKSPACE_BRIEF_CAP: usize = 1_200;

/// Characters of a brief's first line kept for the projects block, where
/// the line stands beside three others on one row.
pub const ONE_LINE_CAP: usize = 160;

/// The brief path under a project root.
pub fn project_brief_path(root: &Path) -> PathBuf {
    root.join(DOT_DIR).join(BRIEF_FILE)
}

/// The brief path under a workspace's `shared` root.
pub fn workspace_brief_path(shared: &Path) -> PathBuf {
    shared.join("workspace").join(BRIEF_FILE)
}

/// A brief file's text, or `None` when it is missing, whitespace only, or
/// not a regular file. A symlink is refused rather than followed
/// (`symlink_metadata`, not `metadata`), so a link never carries a brief
/// from outside the project.
pub fn read_file(path: &Path) -> Option<String> {
    if !std::fs::symlink_metadata(path).ok()?.is_file() {
        return None;
    }
    non_empty(std::fs::read_to_string(path).ok()?)
}

fn non_empty(text: String) -> Option<String> {
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

/// A project's brief: `<root>/.aigentic/brief.md`.
pub fn project_brief(root: &Path) -> Option<String> {
    read_file(&project_brief_path(root))
}

/// A brief's one-liner: its first non-empty line, with leading `#` and
/// spaces stripped, cut at [`ONE_LINE_CAP`] characters.
pub fn one_line(text: &str) -> Option<String> {
    let line = text
        .lines()
        .map(|line| line.trim_start_matches('#').trim())
        .find(|line| !line.is_empty())?;
    Some(line.chars().take(ONE_LINE_CAP).collect())
}

/// The workspace brief's prefix block: its heading, then the text, cut at
/// [`WORKSPACE_BRIEF_CAP`] bytes with the note inside the cap. A
/// workspace has no name to point `read_brief` at, so its note names no
/// tool.
pub fn workspace_block(name: &str, text: &str) -> String {
    let body = capped(text, WORKSPACE_BRIEF_CAP, None);
    format!("# Workspace brief: {name}\n\n{body}")
}

/// The project brief's prefix block: its heading, then the text, cut at
/// [`PROJECT_BRIEF_CAP`] bytes with the note inside the cap. The note
/// names `read_brief` only when `read_brief` is offered, since a note
/// pointing at a tool the model cannot see would be a dead end.
pub fn project_block(name: &str, text: &str, read_brief: bool) -> String {
    let body = capped(text, PROJECT_BRIEF_CAP, read_brief.then_some(name));
    format!("# Project brief: {name}\n\n{body}")
}

/// `text` cut at `cap` bytes, keeping the head, with a note when it was
/// cut. The note counts inside the cap — the text is cut at `cap` minus
/// the note — so the text never exceeds `cap`, and the cut backs off to a
/// character boundary so it never lands mid-character.
fn capped(text: &str, cap: usize, project: Option<&str>) -> String {
    if text.len() <= cap {
        return text.to_owned();
    }
    let note = match project {
        Some(name) => format!("… (brief cut at {cap} bytes; read_brief(\"{name}\") has it all)"),
        None => format!("… (brief cut at {cap} bytes)"),
    };
    let mut end = cap.saturating_sub(note.len()).min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{note}", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T1 (issue #123): the first non-empty line is the one-liner, with
    /// leading `#` and spaces stripped, and a long line is cut at exactly
    /// `ONE_LINE_CAP` characters — characters, not bytes, so a line of
    /// multi-byte ones survives whole.
    #[test]
    fn the_one_line_is_the_first_non_empty_line_with_its_hashes_stripped() {
        assert_eq!(one_line("").as_deref(), None);
        assert_eq!(one_line("\n \n\n").as_deref(), None);
        assert_eq!(
            one_line("\n\n#   The marketing site  \n\nNext.js.\n").as_deref(),
            Some("The marketing site")
        );
        assert_eq!(
            one_line("plain first line").as_deref(),
            Some("plain first line")
        );

        let long: String = "å".repeat(ONE_LINE_CAP + 40);
        let cut = one_line(&format!("# {long}")).unwrap();
        assert_eq!(cut.chars().count(), ONE_LINE_CAP);
        assert_eq!(cut, "å".repeat(ONE_LINE_CAP));
        assert_eq!(cut.len(), ONE_LINE_CAP * 2, "counted in chars, not bytes");
    }

    /// T1: a cut landing mid-character backs off to a character boundary,
    /// so the text is valid UTF-8 and inside the cap; the note is inside
    /// the cap, and a project's note names its project.
    #[test]
    fn a_cut_lands_on_a_character_boundary_with_the_note_inside_the_cap() {
        // Every character is two bytes, so a cap that is odd cannot land
        // on a boundary on its own.
        let text = "å".repeat(PROJECT_BRIEF_CAP);
        let body = capped(&text, 101, Some("web"));
        assert!(body.len() <= 101, "{} bytes", body.len());
        assert!(
            body.ends_with("… (brief cut at 101 bytes; read_brief(\"web\") has it all)"),
            "{body}"
        );
        // The cut text is whole characters: the head before the note has
        // an even byte length.
        let head = body.split('…').next().unwrap();
        assert_eq!(head.len() % 2, 0, "{head:?}");

        // Text within the cap is returned unchanged, note and all.
        assert_eq!(capped("short", 101, Some("web")), "short");
    }

    /// T1: a missing file, an empty one and a whitespace-only one are
    /// `None`; a `brief.md` that is not a regular file is refused, and a
    /// symlink to a real brief is refused rather than followed.
    #[test]
    fn a_brief_is_none_when_missing_empty_or_not_a_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let path = project_brief_path(root);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        assert_eq!(project_brief(root), None, "no file");

        std::fs::write(&path, "").unwrap();
        assert_eq!(project_brief(root), None, "empty");
        std::fs::write(&path, "  \n\t\n").unwrap();
        assert_eq!(project_brief(root), None, "whitespace only");

        std::fs::write(&path, "# Brief\n\nIt is a site.\n").unwrap();
        assert_eq!(
            project_brief(root).as_deref(),
            Some("# Brief\n\nIt is a site.\n")
        );

        // A directory named `brief.md` is not a regular file.
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert_eq!(project_brief(root), None, "a directory is not a brief");

        // A symlink to a real brief, inside the project: refused too.
        std::fs::remove_dir(&path).unwrap();
        let real = root.join("elsewhere.md");
        std::fs::write(&real, "borrowed\n").unwrap();
        std::os::unix::fs::symlink(&real, &path).unwrap();
        assert_eq!(project_brief(root), None, "a symlink is not a brief");
    }

    /// T2: the two blocks carry their headings, and a workspace's cut
    /// note names `read_brief` never, a project's only when it is
    /// offered.
    #[test]
    fn the_blocks_are_headed_and_a_cut_names_read_brief_only_when_offered() {
        assert_eq!(
            project_block("web", "# Foundation\n\nThe marketing site.\n", true),
            "# Project brief: web\n\n# Foundation\n\nThe marketing site.\n"
        );

        let text = "x".repeat(PROJECT_BRIEF_CAP * 2);
        let offered = project_block("web", &text, true);
        assert!(
            offered.ends_with("… (brief cut at 2000 bytes; read_brief(\"web\") has it all)"),
            "{offered}"
        );
        let unoffered = project_block("web", &text, false);
        assert!(
            unoffered.ends_with("… (brief cut at 2000 bytes)"),
            "{unoffered}"
        );
        assert!(!unoffered.contains("read_brief"));

        let ws = workspace_block("aigentic", &text);
        assert!(ws.starts_with("# Workspace brief: aigentic\n\n"), "{ws}");
        assert!(ws.ends_with("… (brief cut at 1200 bytes)"), "{ws}");
        assert!(!ws.contains("read_brief"));
    }
}
