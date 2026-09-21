//! Manifest, lock, discovery and static check over the fixtures in
//! `tests/fixtures`, plus tampering in a temp copy.

use std::path::{Path, PathBuf};

use aigentic_skills::{
    FindingKind, Invocation, LockEntry, Lockfile, Manifest, Origin, Review, Roots, SkillError,
    SkillSet, check, discover, tools_referenced,
};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}
fn bundled() -> PathBuf {
    fixtures().join("bundled")
}
fn roots() -> Roots {
    Roots::new(
        &fixtures().join("project"),
        &fixtures().join("user"),
        &bundled(),
    )
}
fn manifest(name: &str) -> Manifest {
    Manifest::parse(&bundled().join("bucket").join(name), Origin::Bundled).unwrap()
}
fn lock_for(names: &[&str]) -> Lockfile {
    let mut lock = Lockfile::default();
    for name in names {
        let m = discover(
            &fixtures().join("project"),
            &fixtures().join("user"),
            &bundled(),
        )
        .unwrap()
        .into_iter()
        .find(|m| m.name == *name)
        .unwrap();
        lock.upsert(LockEntry::from_manifest(&m, format!("skills/{name}"), "local", "").unwrap());
    }
    lock
}
fn no_tools() -> Vec<String> {
    vec![]
}
fn kinds(findings: &[aigentic_skills::Finding]) -> Vec<FindingKind> {
    let mut k: Vec<FindingKind> = findings.iter().map(|f| f.kind).collect();
    k.dedup();
    k
}

// Manifest

#[test]
fn frontmatter_parses_including_disable_model_invocation() {
    let m = manifest("slash");
    assert_eq!(m.name, "slash");
    assert_eq!(m.description, "A user-invoked skill, quoted description.");
    assert_eq!(m.invocation, Invocation::User);
    assert_eq!(m.argument_hint.as_deref(), Some("What to do"));
    assert_eq!(m.body, "Run a `/plain` session.\n");
    assert!(m.files.is_empty());

    let m = manifest("plain");
    assert_eq!(m.invocation, Invocation::Model);
    assert!(m.body.starts_with("# Plain"));
    assert_eq!(m.description_line(), "plain: A skill with nothing to flag.");
}

#[test]
fn companion_files_are_listed_and_scripts_recognised() {
    let m = manifest("scripted");
    assert_eq!(
        m.files,
        vec![PathBuf::from("notes.md"), PathBuf::from("setup.sh")]
    );
    assert_eq!(m.scripts(), vec![PathBuf::from("setup.sh")]);
}

#[test]
fn a_bad_frontmatter_fails_loudly() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("SKILL.md"), "# no frontmatter\n").unwrap();
    let err = Manifest::parse(dir.path(), Origin::Project).unwrap_err();
    assert!(matches!(err, SkillError::Frontmatter { .. }), "{err}");

    std::fs::write(
        dir.path().join("SKILL.md"),
        "---\ndescription: x\n---\nbody\n",
    )
    .unwrap();
    let err = Manifest::parse(dir.path(), Origin::Project).unwrap_err();
    assert!(err.to_string().contains("missing `name`"), "{err}");

    std::fs::write(
        dir.path().join("SKILL.md"),
        "---\nname: x\ndescription:\n  multi\n---\nbody\n",
    )
    .unwrap();
    let err = Manifest::parse(dir.path(), Origin::Project).unwrap_err();
    assert!(err.to_string().contains("single-line"), "{err}");

    std::fs::write(
        dir.path().join("SKILL.md"),
        "---\nname: x\ndescription: d\nmetadata:\n  credits:\n    author: someone\n---\nbody\n",
    )
    .unwrap();
    let m = Manifest::parse(dir.path(), Origin::Project).unwrap();
    assert_eq!((m.name.as_str(), m.description.as_str()), ("x", "d"));
}

// Discovery

#[test]
fn resolution_order_is_project_then_user_then_bundled() {
    let all = discover(
        &fixtures().join("project"),
        &fixtures().join("user"),
        &bundled(),
    )
    .unwrap();
    let names: Vec<&str> = all.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "ignore-rules",
            "injection",
            "plain",
            "scripted",
            "slash",
            "tool-ref",
            "url",
            "user-only",
            "widening"
        ]
    );
    let by = |n: &str| all.iter().find(|m| m.name == n).unwrap();
    assert_eq!(by("plain").origin, Origin::Project);
    assert!(by("plain").body.contains("Project override"));
    assert_eq!(by("slash").origin, Origin::User);
    assert_eq!(by("user-only").origin, Origin::User);
    assert_eq!(by("tool-ref").origin, Origin::Bundled);
}

#[test]
fn a_missing_root_is_empty_not_an_error() {
    let all = discover(
        Path::new("/nonexistent/x"),
        Path::new("/nonexistent/y"),
        &bundled(),
    )
    .unwrap();
    assert!(all.iter().all(|m| m.origin == Origin::Bundled));
    assert_eq!(all.len(), 8);
}

// Lock

#[test]
fn lock_round_trips_through_toml() {
    let lock = lock_for(&["plain", "scripted"]);
    let text = lock.to_toml();
    let back = Lockfile::parse(&text).unwrap();
    assert_eq!(back, lock);
    let scripted = back.get("scripted").unwrap();
    assert_eq!(scripted.files.len(), 2);
    assert!(scripted.files.contains_key("setup.sh"));
    assert!(scripted.review.is_pending());
    assert_eq!(scripted.sha256.len(), 64);
}

#[test]
fn len_counts_entries_not_companion_files() {
    let empty = Lockfile::default();
    assert!(empty.is_empty());
    assert_eq!(empty.len(), 0);

    // `scripted` has two companion files; only its entry counts.
    let lock = lock_for(&["plain", "scripted"]);
    assert_eq!(lock.len(), 2);
    assert!(!lock.is_empty());

    // Upsert replaces by name, so re-adding does not grow the lock.
    let mut merged = lock.clone();
    merged.upsert(lock.get("scripted").unwrap().clone());
    assert_eq!(merged.len(), 2);
}

#[test]
fn hash_verification_passes_on_the_fixture_set_and_fails_on_one_byte() {
    let lock = lock_for(&["scripted"]);
    let set = SkillSet::load(&["scripted".into()], &roots(), &lock, &no_tools()).unwrap();
    assert!(set.get("scripted").is_some());

    // Copy to a temp dir, flip one byte in SKILL.md, reuse the same lock.
    let tmp = tempfile::tempdir().unwrap();
    let dst = tmp.path().join("scripted");
    copy_dir(&bundled().join("bucket/scripted"), &dst);
    let md = dst.join("SKILL.md");
    let mut bytes = std::fs::read(&md).unwrap();
    let last = bytes.len() - 2;
    bytes[last] ^= 0x01;
    std::fs::write(&md, bytes).unwrap();
    let roots = Roots {
        project: None,
        user: None,
        bundled: Some(tmp.path().to_path_buf()),
    };
    let err = SkillSet::load(&["scripted".into()], &roots, &lock, &no_tools()).unwrap_err();
    match err {
        SkillError::HashMismatch { name, file, .. } => {
            assert_eq!(name, "scripted");
            assert_eq!(file, "SKILL.md");
        }
        other => panic!("expected HashMismatch, got {other}"),
    }

    // A changed script is caught too.
    std::fs::copy(bundled().join("bucket/scripted/SKILL.md"), &md).unwrap();
    std::fs::write(dst.join("setup.sh"), "#!/bin/sh\necho changed\n").unwrap();
    let err = SkillSet::load(&["scripted".into()], &roots, &lock, &no_tools()).unwrap_err();
    assert!(
        matches!(&err, SkillError::HashMismatch { file, .. } if file == "setup.sh"),
        "{err}"
    );
}

#[test]
fn an_unlocked_or_missing_skill_does_not_load() {
    let lock = lock_for(&["plain"]);
    let err = SkillSet::load(&["url".into()], &roots(), &lock, &no_tools()).unwrap_err();
    assert!(
        matches!(err, SkillError::NotInLock(ref n) if n == "url"),
        "{err}"
    );
    let err = SkillSet::load(&["nope".into()], &roots(), &lock, &no_tools()).unwrap_err();
    assert!(
        matches!(err, SkillError::NotFound(ref n) if n == "nope"),
        "{err}"
    );
}

#[test]
fn a_project_override_needs_its_own_lock_entry() {
    // The lock hashes the bundled `plain`; the project override resolves
    // first and its hash differs, so it is refused rather than swapped in.
    let bundled_plain = manifest("plain");
    let mut lock = Lockfile::default();
    lock.upsert(LockEntry::from_manifest(&bundled_plain, "skills/plain", "local", "").unwrap());
    let err = SkillSet::load(&["plain".into()], &roots(), &lock, &no_tools()).unwrap_err();
    assert!(matches!(err, SkillError::HashMismatch { .. }), "{err}");

    let lock = lock_for(&["plain"]); // hashes the override, as `skills vendor` would
    let set = SkillSet::load(&["plain".into()], &roots(), &lock, &no_tools()).unwrap();
    assert_eq!(set.get("plain").unwrap().origin, Origin::Project);
}

#[test]
fn requires_mismatch_fails_loudly() {
    let mut lock = lock_for(&["tool-ref"]);
    lock.get_mut("tool-ref").unwrap().requires = vec!["bash".into(), "read_file".into()];
    let err = SkillSet::load(
        &["tool-ref".into()],
        &roots(),
        &lock,
        &["read_file".to_owned()],
    )
    .unwrap_err();
    assert!(
        matches!(&err, SkillError::MissingRequirement { skill, tool } if skill == "tool-ref" && tool == "bash"),
        "{err}"
    );
    SkillSet::load(
        &["tool-ref".into()],
        &roots(),
        &lock,
        &["read_file".to_owned(), "bash".to_owned()],
    )
    .unwrap();
}

#[test]
fn accepted_review_round_trips() {
    let mut lock = lock_for(&["plain"]);
    lock.get_mut("plain").unwrap().review = Review::Accepted {
        by: "steve".into(),
        on: "2026-09-21".into(),
    };
    let back = Lockfile::parse(&lock.to_toml()).unwrap();
    assert_eq!(back, lock);
}

// SkillSet views

#[test]
fn descriptions_are_sorted_grouped_and_stable() {
    let lock = lock_for(&["plain", "slash", "url"]);
    let set = SkillSet::load(
        &["url".into(), "slash".into(), "plain".into()],
        &roots(),
        &lock,
        &no_tools(),
    )
    .unwrap();
    let a = set.descriptions();
    let b = set.descriptions();
    assert_eq!(a, b);
    assert_eq!(
        a,
        "# Skills\n\n\
         Run by the user as slash commands; do not load them yourself:\n\
         - slash: The user's version of slash.\n\n\
         Available through the `load_skill` tool when the description fits the task. When a loaded skill's instructions refer to `/<name>` and that name is in this list, call `load_skill` with it before continuing:\n\
         - plain: The project's own version of plain.\n\
         - url: Links out."
    );
    assert_eq!(set.user_invoked().len(), 1);
    assert_eq!(set.model_invoked().len(), 2);
    assert_eq!(set.entry("url").unwrap().source_ref(), "local@");
}

// Static check: one fixture per finding kind, plus the planted injection.

#[test]
fn plain_skill_has_no_findings() {
    assert!(check(&manifest("plain")).is_empty());
}

#[test]
fn tool_references_are_found_with_lines() {
    let f = check(&manifest("tool-ref"));
    assert_eq!(kinds(&f), vec![FindingKind::ToolReference]);
    assert_eq!(
        tools_referenced(&f),
        vec!["bash", "read_file", "write_file"]
    );
    assert!(
        f.iter()
            .all(|x| x.line == 6 && x.file == Path::new("SKILL.md"))
    );
}

#[test]
fn urls_are_found() {
    let f = check(&manifest("url"));
    assert_eq!(kinds(&f), vec![FindingKind::Url]);
    assert_eq!(f[0].text, "https://example.com/docs");
    assert_eq!(f[0].line, 6);
}

#[test]
fn scripts_and_pipe_to_shell_are_found() {
    let f = check(&manifest("scripted"));
    let in_script: Vec<&aigentic_skills::Finding> = f
        .iter()
        .filter(|x| x.file == Path::new("setup.sh"))
        .collect();
    assert_eq!(in_script[0].kind, FindingKind::ShellInScript);
    assert_eq!(in_script[0].text, "script file");
    assert!(
        in_script
            .iter()
            .any(|x| x.line == 3 && x.text.contains("| sh"))
    );
    assert!(
        in_script
            .iter()
            .any(|x| x.kind == FindingKind::Url && x.line == 3)
    );
    assert!(f.iter().all(|x| x.file != Path::new("notes.md")));
    assert!(f.iter().all(|x| x.file != Path::new("SKILL.md")), "{f:#?}");
}

#[test]
fn permission_widening_is_found() {
    let f = check(&manifest("widening"));
    assert_eq!(kinds(&f), vec![FindingKind::PermissionWidening]);
    let texts: Vec<&str> = f.iter().map(|x| x.text.as_str()).collect();
    assert_eq!(texts, vec!["without asking", "sudo"]);
}

#[test]
fn ignore_rules_is_found() {
    let f = check(&manifest("ignore-rules"));
    assert_eq!(kinds(&f), vec![FindingKind::IgnoreRules]);
    assert_eq!(f[0].text, "ignore the project rules");
}

#[test]
fn the_planted_injection_is_flagged_on_every_count() {
    let f = check(&manifest("injection"));
    let has = |kind, line| f.iter().any(|x| x.kind == kind && x.line == line);
    assert!(has(FindingKind::IgnoreRules, 10), "{f:#?}");
    assert!(has(FindingKind::PermissionWidening, 10), "{f:#?}");
    assert!(has(FindingKind::PermissionWidening, 11), "{f:#?}");
    assert!(has(FindingKind::ShellInScript, 11), "{f:#?}");
    assert!(has(FindingKind::Url, 11), "{f:#?}");
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let dst = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &dst);
        } else {
            std::fs::copy(entry.path(), dst).unwrap();
        }
    }
}
