//! The vendored set: every skill hashes to its lock entry, and the review
//! doc is a snapshot of the static check, so an upstream update that adds
//! a `curl` shows up as a test failure. Set `AIGENTIC_REGEN=1` to rewrite
//! `skills.lock.toml` and `docs/skills-review.md` from the files.

use std::path::{Path, PathBuf};

use aigentic_skills::{
    Lockfile, Origin, Roots, SkillSet, blocking, lock_root, render_review, walk_root,
};

const SOURCE: &str = "https://github.com/mattpocock/skills";
const COMMIT: &str = "c55ee46073ed923f86ce59a5eb3b6d895095d1b7";

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

#[test]
fn vendored_set_matches_lock_and_review_snapshot() {
    let repo = repo();
    let lock_path = repo.join("skills.lock.toml");
    let review_path = repo.join("docs/skills-review.md");
    let root = repo.join("skills/pocock");

    let mut lock = if lock_path.exists() {
        Lockfile::load(&lock_path).unwrap()
    } else {
        Lockfile::default()
    };
    let manifests = lock_root(&root, "skills/pocock", SOURCE, COMMIT, &mut lock).unwrap();
    assert_eq!(manifests.len(), 38, "upstream has 38 skills at {COMMIT}");
    let review = render_review(&manifests, &lock);

    if std::env::var_os("AIGENTIC_REGEN").is_some() {
        lock.save(&lock_path).unwrap();
        std::fs::write(&review_path, &review).unwrap();
        return;
    }
    assert_eq!(
        Lockfile::load(&lock_path).unwrap(),
        lock,
        "skills.lock.toml is stale; regenerate with AIGENTIC_REGEN=1"
    );
    assert_eq!(
        std::fs::read_to_string(&review_path).unwrap(),
        review,
        "docs/skills-review.md is stale; regenerate with AIGENTIC_REGEN=1"
    );
}

#[test]
fn every_vendored_skill_loads_against_the_lock() {
    let repo = repo();
    let lock = Lockfile::load(&repo.join("skills.lock.toml")).unwrap();
    let names: Vec<String> = walk_root(&repo.join("skills/pocock"), Origin::Bundled)
        .unwrap()
        .into_iter()
        .map(|m| m.name)
        .collect();
    let roots = Roots {
        project: None,
        user: None,
        bundled: Some(repo.join("skills/pocock")),
    };
    let set = SkillSet::load(&names, &roots, &lock, &[]).unwrap();
    assert_eq!(set.len(), 38);
    assert!(set.get("tdd").is_some() && set.get("implement").is_some());
    assert_eq!(set.user_invoked().len(), 22);
    assert_eq!(set.model_invoked().len(), 16);
}

#[test]
fn the_acceptance_set_is_vendored_and_locked() {
    let lock = Lockfile::load(&repo().join("skills.lock.toml")).unwrap();
    for name in [
        "implement",
        "tdd",
        "code-review",
        "diagnosing-bugs",
        "grilling",
    ] {
        let e = lock
            .get(name)
            .unwrap_or_else(|| panic!("{name} not in lock"));
        assert_eq!(e.source, SOURCE);
        assert_eq!(e.commit, COMMIT);
    }
}

#[test]
fn skills_check_blocks_while_pending_skills_have_findings() {
    let repo = repo();
    let lock = Lockfile::load(&repo.join("skills.lock.toml")).unwrap();
    let manifests = walk_root(&repo.join("skills/pocock"), Origin::Bundled).unwrap();
    let blocked = blocking(&manifests, &lock);
    let pending_with_findings: Vec<String> = manifests
        .iter()
        .filter(|m| lock.get(&m.name).unwrap().review.is_pending())
        .filter(|m| !aigentic_skills::check(m).is_empty())
        .map(|m| m.name.clone())
        .collect();
    let mut expected = pending_with_findings;
    expected.sort();
    assert_eq!(blocked, expected);
}
