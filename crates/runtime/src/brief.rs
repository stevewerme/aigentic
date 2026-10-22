//! Briefs: a project's `.aigentic/brief.md` and a workspace's
//! `<shared>/workspace/brief.md`, hand-written Markdown that says what a
//! project or workspace is and where it stands. The whole brief goes
//! inline in the prefix, capped; its first non-empty line is the
//! one-liner the projects block prints for a sibling, and `read_brief`
//! opens a sibling's whole brief on demand.
//!
//! The recommended shape is a `## Foundation` section (what it is and
//! who it is for, the stack, the commands, hard rules, relationships as
//! edges) then a `## Current state` section (what changed, what is open,
//! what is next, a date). Only `## Current state` is parsed, by
//! [`splice_current_state`], which replaces that section and leaves every
//! other byte alone; [`write_brief_atomically`] puts the result on disk.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::aigentic_skills::hash_bytes;
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

/// The heading [`splice_current_state`] looks for, matched against a
/// trimmed line ignoring ASCII case.
pub const CURRENT_STATE_HEADING: &str = "## Current state";

/// Bytes of the current-state body a splice keeps, its date line
/// included.
pub const CURRENT_STATE_CAP: usize = 1_200;

/// A brief's hash: the lowercase hex SHA-256 of its bytes, the value
/// [`write_brief_atomically`] compares against.
pub fn brief_hash(text: &str) -> String {
    hash_bytes(text.as_bytes())
}

/// `text` with its `## Current state` section's body replaced by
/// `Updated <date>`, a blank line, then `reply` trimmed with every line
/// starting `#` dropped, cut at [`CURRENT_STATE_CAP`] bytes on a line
/// boundary.
///
/// The section runs from the first line equal to the heading (trimmed,
/// ASCII case-insensitive) to the next line starting `## ` outside a
/// fenced code block, or the end. The heading line and every byte before
/// and after the section are copied unchanged, so CRLF, a missing final
/// newline and non-ASCII outside it survive. The body's lines end the way
/// the heading line does. A brief without the heading gets
/// `\n## Current state\n\n<body>\n` appended.
pub fn splice_current_state(text: &str, reply: &str, date: &str) -> String {
    let body = current_state_body(reply, date);
    let Some(section) = current_state_section(text) else {
        return format!("{text}\n{CURRENT_STATE_HEADING}\n\n{body}\n");
    };
    let heading = &text[section.start..section.body_start];
    let eol = if heading.ends_with("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let mut out = String::with_capacity(text.len() + body.len());
    out.push_str(&text[..section.body_start]);
    if !heading.ends_with('\n') {
        out.push_str(eol);
    }
    out.push_str(eol);
    out.push_str(&body.replace('\n', eol));
    out.push_str(eol);
    if section.end < text.len() {
        // A blank line between the body and the heading that follows.
        out.push_str(eol);
    }
    out.push_str(&text[section.end..]);
    out
}

/// Byte offsets of the current-state section: where its heading line
/// starts, where the line after the heading starts, and where the
/// section ends (the next `## ` line or the end of the text).
struct Section {
    start: usize,
    body_start: usize,
    end: usize,
}

fn current_state_section(text: &str) -> Option<Section> {
    let mut found: Option<(usize, usize)> = None;
    let mut fenced = false;
    let mut offset = 0;
    for raw in text.split_inclusive('\n') {
        let start = offset;
        offset += raw.len();
        let line = raw.trim_end_matches(['\n', '\r']);
        if is_fence(line) {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        match found {
            None if line.trim().eq_ignore_ascii_case(CURRENT_STATE_HEADING) => {
                found = Some((start, offset));
            }
            Some((start_at, body_start)) if line.starts_with("## ") => {
                return Some(Section {
                    start: start_at,
                    body_start,
                    end: start,
                });
            }
            _ => {}
        }
    }
    found.map(|(start, body_start)| Section {
        start,
        body_start,
        end: text.len(),
    })
}

/// A line that opens or closes a fenced code block. Opener and closer
/// are not matched by character, which is enough to keep a fenced
/// `## ` line from ending the section.
fn is_fence(line: &str) -> bool {
    let line = line.trim_start();
    line.starts_with("```") || line.starts_with("~~~")
}

/// The body that goes under the heading, `\n`-joined with no final
/// newline. A reply's headings are dropped so a splice can never end its
/// own section early or add a section to the brief.
fn current_state_body(reply: &str, date: &str) -> String {
    let kept: Vec<&str> = reply
        .trim()
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .collect();
    let reply = kept.join("\n");
    let mut body = format!("Updated {date}");
    for (i, line) in reply.trim().lines().enumerate() {
        let sep = if i == 0 { "\n\n" } else { "\n" };
        if body.len() + sep.len() + line.len() > CURRENT_STATE_CAP {
            break;
        }
        body.push_str(sep);
        body.push_str(line);
    }
    body
}

/// What [`write_brief_atomically`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
    /// The new text replaced the brief.
    Written,
    /// The brief already held the new text; nothing was written.
    Unchanged,
    /// The brief is missing, is not a regular file, or no longer hashes
    /// to `expected_hash`; nothing was written.
    ChangedUnderneath,
}

/// Replace the brief at `path` with `new_text`, but only if it is still a
/// regular file whose [`brief_hash`] is `expected_hash`. A symlink counts
/// as changed and is never followed.
///
/// The new text goes to `<stem>.tmp.<pid>.<ulid>` beside the brief,
/// created with `create_new`, given the brief's permissions and synced,
/// then renamed over it, so a reader sees the old brief or the new one,
/// never half of either. The temp file is removed if any step fails. The
/// caller is the harness itself, not a tool, so no policy is consulted;
/// it only ever passes `project_brief_path(root)`.
pub fn write_brief_atomically(
    path: &Path,
    expected_hash: &str,
    new_text: &str,
) -> io::Result<WriteOutcome> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Ok(WriteOutcome::ChangedUnderneath);
        }
        Err(e) => return Err(e),
    };
    if !meta.is_file() {
        return Ok(WriteOutcome::ChangedUnderneath);
    }
    let current = std::fs::read(path)?;
    if hash_bytes(&current) != expected_hash {
        return Ok(WriteOutcome::ChangedUnderneath);
    }
    if current == new_text.as_bytes() {
        return Ok(WriteOutcome::Unchanged);
    }
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "brief".to_owned());
    let tmp = path.with_file_name(format!(
        "{stem}.tmp.{}.{}",
        std::process::id(),
        ulid::Ulid::generate()
    ));
    let written = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(new_text.as_bytes())?;
        file.set_permissions(meta.permissions())?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(WriteOutcome::Written)
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

    const BODY: &str = "Updated 2026-10-22\n\nShipped the splice.";

    /// Bytes before and after the section are the caller's; only the
    /// body under the heading is the splice's to rewrite.
    #[test]
    fn a_middle_section_is_replaced_and_the_bytes_around_it_are_kept() {
        let text = "# Web\n\n## Foundation\n\nNext.js.\n\n## current STATE  \n\nold\nolder\n\n## Next\n\nmore\n";
        let out = splice_current_state(text, "Shipped the splice.", "2026-10-22");
        assert_eq!(
            out,
            format!(
                "# Web\n\n## Foundation\n\nNext.js.\n\n## current STATE  \n\n{BODY}\n\n## Next\n\nmore\n"
            )
        );
    }

    #[test]
    fn a_section_at_the_end_is_replaced() {
        let text = "# Web\n\n## Current state\n\nold\n";
        assert_eq!(
            splice_current_state(text, "  Shipped the splice.\n\n", "2026-10-22"),
            format!("# Web\n\n## Current state\n\n{BODY}\n")
        );
    }

    #[test]
    fn a_brief_without_the_section_gets_it_appended() {
        let text = "# Web\n\n## Foundation\n\nNext.js.\n";
        assert_eq!(
            splice_current_state(text, "Shipped the splice.", "2026-10-22"),
            format!("{text}\n## Current state\n\n{BODY}\n")
        );
    }

    /// A heading in the reply would end the section early on the next
    /// splice, or add a section the brief's author never wrote.
    #[test]
    fn heading_lines_in_the_reply_are_dropped() {
        let reply = "# Current state\n\nShipped the splice.\n## Next\n  ### Open\nThe write.";
        let out = splice_current_state("", reply, "2026-10-22");
        assert_eq!(
            out,
            "\n## Current state\n\nUpdated 2026-10-22\n\nShipped the splice.\nThe write.\n"
        );
    }

    #[test]
    fn an_over_long_reply_is_cut_on_a_line_boundary() {
        let line = "x".repeat(99);
        let reply = vec![line.as_str(); 30].join("\n");
        let body = current_state_body(&reply, "2026-10-22");
        assert!(body.len() <= CURRENT_STATE_CAP, "{} bytes", body.len());
        assert!(body.len() > CURRENT_STATE_CAP - 100, "{} bytes", body.len());
        assert!(body.lines().skip(2).all(|l| l == line), "{body}");

        // A first line over the cap on its own leaves only the date.
        let wide = "y".repeat(CURRENT_STATE_CAP);
        assert_eq!(
            current_state_body(&wide, "2026-10-22"),
            "Updated 2026-10-22"
        );
    }

    #[test]
    fn a_fenced_heading_does_not_end_the_section() {
        let text = "## Current state\n\n```md\n## not a heading\n```\nold\n\n## Next\n\nkept\n";
        assert_eq!(
            splice_current_state(text, "Shipped the splice.", "2026-10-22"),
            format!("## Current state\n\n{BODY}\n\n## Next\n\nkept\n")
        );

        // A fenced copy of the heading is not the section either.
        let text = "~~~\n## Current state\n~~~\n";
        assert_eq!(
            splice_current_state(text, "Shipped the splice.", "2026-10-22"),
            format!("{text}\n## Current state\n\n{BODY}\n")
        );
    }

    #[test]
    fn crlf_is_kept_and_the_body_follows_the_heading_line_ending() {
        let text = "# Web\r\n\r\n## Current state\r\n\r\nold\r\n\r\n## Next\r\nkept\r\n";
        assert_eq!(
            splice_current_state(text, "a\nb", "2026-10-22"),
            "# Web\r\n\r\n## Current state\r\n\r\nUpdated 2026-10-22\r\n\r\na\r\nb\r\n\r\n## Next\r\nkept\r\n"
        );
    }

    #[test]
    fn a_missing_final_newline_is_kept_outside_the_section() {
        // After the section: the suffix keeps its missing newline.
        let text = "## Current state\nold\n## Next\nkept";
        assert_eq!(
            splice_current_state(text, "Shipped the splice.", "2026-10-22"),
            format!("## Current state\n\n{BODY}\n\n## Next\nkept")
        );
        // A heading on the last line with no newline gets one.
        assert_eq!(
            splice_current_state(
                "# Web\n## Current state",
                "Shipped the splice.",
                "2026-10-22"
            ),
            format!("# Web\n## Current state\n\n{BODY}\n")
        );
        // A brief with no section and no final newline: appended as is.
        assert_eq!(
            splice_current_state("# Web", "Shipped the splice.", "2026-10-22"),
            format!("# Web\n## Current state\n\n{BODY}\n")
        );
    }

    #[test]
    fn non_ascii_around_and_in_the_section_survives() {
        let text = "# Webbplats för Åsa\n\n## Current state\n\ngammalt\n\n## Nästa 🚀\n\nfärdigt\n";
        assert_eq!(
            splice_current_state(text, "Klart: översättning ✅", "2026-10-22"),
            "# Webbplats för Åsa\n\n## Current state\n\nUpdated 2026-10-22\n\nKlart: översättning ✅\n\n## Nästa 🚀\n\nfärdigt\n"
        );
    }

    fn brief_file(text: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = project_brief_path(dir.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        (dir, path)
    }

    fn siblings(path: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_brief_matching_the_hash_is_written_and_no_temp_file_is_left() {
        let (_dir, path) = brief_file("old\n");
        let outcome = write_brief_atomically(&path, &brief_hash("old\n"), "new\n").unwrap();
        assert_eq!(outcome, WriteOutcome::Written);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new\n");
        assert_eq!(siblings(&path), vec![BRIEF_FILE.to_owned()]);
    }

    #[test]
    fn equal_text_is_unchanged() {
        let (_dir, path) = brief_file("same\n");
        let outcome = write_brief_atomically(&path, &brief_hash("same\n"), "same\n").unwrap();
        assert_eq!(outcome, WriteOutcome::Unchanged);
    }

    /// The brief changed between the read and the write, so the
    /// author's edit wins and the refresh's text is dropped.
    #[test]
    fn a_changed_file_writes_nothing() {
        let (_dir, path) = brief_file("edited by hand\n");
        let outcome = write_brief_atomically(&path, &brief_hash("old\n"), "new\n").unwrap();
        assert_eq!(outcome, WriteOutcome::ChangedUnderneath);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "edited by hand\n");
        assert_eq!(siblings(&path), vec![BRIEF_FILE.to_owned()]);

        std::fs::remove_file(&path).unwrap();
        let outcome = write_brief_atomically(&path, &brief_hash("old\n"), "new\n").unwrap();
        assert_eq!(outcome, WriteOutcome::ChangedUnderneath, "a missing brief");
        assert!(!path.exists());
    }

    /// A symlink is never followed, even when its target hashes right,
    /// so the write can never land outside the project.
    #[test]
    fn a_symlink_writes_nothing() {
        let (dir, path) = brief_file("old\n");
        std::fs::remove_file(&path).unwrap();
        let real = dir.path().join("elsewhere.md");
        std::fs::write(&real, "old\n").unwrap();
        std::os::unix::fs::symlink(&real, &path).unwrap();

        let outcome = write_brief_atomically(&path, &brief_hash("old\n"), "new\n").unwrap();
        assert_eq!(outcome, WriteOutcome::ChangedUnderneath);
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "old\n");
        assert!(std::fs::symlink_metadata(&path).unwrap().is_symlink());
    }

    #[test]
    fn the_brief_keeps_its_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, path) = brief_file("old\n");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        let outcome = write_brief_atomically(&path, &brief_hash("old\n"), "new\n").unwrap();
        assert_eq!(outcome, WriteOutcome::Written);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o640);
    }
}
