//! `[policy] allow_paths` end to end: the key parses, `~` and `~/…`
//! expand to `$HOME`, a relative entry is relative to the project root,
//! an absolute one stands, and an entry naming a *user* is refused with
//! a warning rather than quietly read as `$HOME/name`, which would widen
//! the boundary past the home directory.

use std::path::PathBuf;

use aigentic_runtime::project::ProjectFile;

/// One test only: faking `HOME` is process-wide, so no second test in
/// this file may run while it is faked.
#[test]
fn allow_paths_expands_home_and_refuses_a_user_entry() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&root).unwrap();

    let text = "[policy]\n\
         allow_paths = [\"~\", \"~/somewhere-temp\", \"rel/dir\", \"/abs/not-yet\", \"~nobody/x\"]\n";
    let (file, unknown) = ProjectFile::parse_with(text).unwrap();
    assert_eq!(
        unknown,
        Vec::<String>::new(),
        "the key parses with no warning"
    );

    let previous = std::env::var_os("HOME");
    // SAFETY: this file holds one test, so no other thread reads `HOME`
    // while it is faked.
    unsafe { std::env::set_var("HOME", &home) };
    let (paths, warnings) = file.allow_paths(&root);
    match previous {
        Some(previous) => unsafe { std::env::set_var("HOME", previous) },
        None => unsafe { std::env::remove_var("HOME") },
    }

    assert_eq!(
        paths,
        vec![
            home.clone(),
            home.join("somewhere-temp"),
            root.join("rel/dir"),
            PathBuf::from("/abs/not-yet"),
        ],
        "every entry but the user one resolves"
    );
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].contains("~nobody/x"),
        "the warning names the entry it refuses: {warnings:?}"
    );
}
