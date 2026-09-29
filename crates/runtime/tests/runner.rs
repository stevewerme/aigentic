//! The build runner's tests.

use std::sync::Mutex;

use aigentic_runtime::runner::{Forge, GhForge};

/// `PATH` is process-wide and `GhForge` finds `gh` on it, so the test that
/// puts a fake `gh` there holds this lock while it does.
static PATH_LOCK: Mutex<()> = Mutex::new(());

/// A `gh` that only knows the calls T12 makes, and records a comment body
/// next to itself instead of posting it.
const FAKE_GH: &str = r#"#!/bin/sh
here=$(dirname "$0")
case "$1 $2" in
  "issue view")
    case "$*" in
      *comments*) printf '%s' '{"comments":[{"body":"first"},{"body":"second"}]}' ;;
      *) printf '%s' '{"title":"A title","body":"A body"}' ;;
    esac
    ;;
  "issue comment")
    while [ $# -gt 0 ]; do
      if [ "$1" = "--body-file" ]; then shift; cp "$1" "$here/body.txt"; fi
      shift
    done
    ;;
  *) echo "fake gh: unexpected args: $*" >&2; exit 1 ;;
esac
"#;

fn write_fake_gh(dir: &std::path::Path) {
    let path = dir.join("gh");
    std::fs::write(&path, FAKE_GH).unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    std::fs::set_permissions(&path, perms).unwrap();
}

/// T12: `GhForge` reads the issue's title and body and posts a comment as a
/// body *file*. The fake `gh` on `PATH` pins the call shape the real one
/// gets; the body carries a quote, a backtick and a newline, which is what
/// a body on `argv` would mangle.
#[test]
fn t12_gh_forge_reads_the_issue_and_posts_a_comment() {
    let _guard = PATH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    write_fake_gh(dir.path());
    let old = std::env::var_os("PATH");
    let mut path = dir.path().as_os_str().to_os_string();
    if let Some(old) = &old {
        path.push(":");
        path.push(old);
    }
    // SAFETY: every test that reads or writes `PATH` holds `PATH_LOCK`.
    unsafe { std::env::set_var("PATH", &path) };

    let forge = GhForge::new();
    let issue = forge.issue(57).expect("a readable issue");
    assert_eq!(issue.title, "A title");
    assert_eq!(issue.body, "A body");
    assert_eq!(
        forge.comments(57).expect("readable comments"),
        vec!["first".to_string(), "second".to_string()],
        "every comment's body, oldest first"
    );

    let body = "## Implementation\n\nA `quote` and a \"mark\", on one line.";
    forge.comment(57, body).expect("a posted comment");
    let recorded = std::fs::read_to_string(dir.path().join("body.txt")).unwrap();
    assert_eq!(recorded, body, "the body arrived byte for byte");

    match old {
        // SAFETY: as above.
        Some(previous) => unsafe { std::env::set_var("PATH", previous) },
        None => unsafe { std::env::remove_var("PATH") },
    }
}
