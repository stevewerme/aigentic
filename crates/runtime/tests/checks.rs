//! E1, E2, E3 and the dispatcher (issue #56, T1–T6, T15).
//!
//! The weekend fixtures and the E4/E5/E7 cases are in the next commit's
//! tests; this one reads the repository itself.

use std::path::Path;
use std::process::Command;

use aigentic_log::{CheckResult, StepReport};
use aigentic_runtime::checks::git::read_commits;
use aigentic_runtime::checks::{CheckInput, ChecksError, ObservedCommit, run_checks};

/// A scratch repository with the commits a test asked for.
struct Repo {
    dir: tempfile::TempDir,
}

impl Repo {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("a temp dir");
        let repo = Self { dir };
        repo.git(&["init", "-q"]);
        repo.git(&["config", "user.email", "t@example.com"]);
        repo.git(&["config", "user.name", "Test"]);
        repo
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn git(&self, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(self.path())
            .args(args)
            .output()
            .expect("git runs");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// Write `name`, stage it and commit with `message` through
    /// `git commit -F`, the rule the trailer check exists for.
    fn commit(&self, name: &str, message: &str) -> String {
        let file = self.path().join(name);
        std::fs::write(&file, format!("{name}\n")).expect("the file");
        let msg = self.path().join("msg.txt");
        std::fs::write(&msg, message).expect("the message file");
        self.git(&["add", name]);
        self.git(&["commit", "-F", msg.to_str().expect("utf-8")]);
        self.git(&["rev-parse", "HEAD"]).trim().to_string()
    }
}

fn trailer(model: &str) -> String {
    format!("Co-Authored-By: aigentic ({model}) <332865255+aigentic-bot@users.noreply.github.com>")
}

/// The plan's model string: the provider prefix is stripped by
/// `trailer_model`, so the plan's `deepseek/deepseek-v4.1-flash` is
/// credited as `deepseek-v4.1-flash`.
const PROFILE_MODEL: &str = "deepseek/deepseek-v4.1-flash";
const TRAILER_MODEL: &str = "deepseek-v4.1-flash";

fn input<'a>(
    commits: &'a [ObservedCommit],
    named_subjects: &'a [String],
    report: &'a StepReport,
) -> CheckInput<'a> {
    CheckInput {
        commits,
        events: &[],
        report,
        named_subjects,
        model: PROFILE_MODEL,
    }
}

fn verdict(id: &str, input: &CheckInput<'_>) -> Result<(), String> {
    let outcome = run_checks(&[id.to_string()], input)
        .expect("a known id")
        .pop()
        .expect("one outcome");
    match outcome.result {
        CheckResult::Pass => Ok(()),
        other => Err(format!(
            "{other:?}: {}",
            outcome.detail.clone().unwrap_or_default()
        )),
    }
}

/// T1: `read_commits` reads sha, subject, message, trailers and paths of
/// a two-commit range, against the repository's own answers.
#[test]
fn read_commits_reads_the_range() {
    let repo = Repo::new();
    repo.commit("base.txt", "base\n");
    let base = repo.git(&["rev-parse", "HEAD"]).trim().to_string();

    let first_subject = "log: add the first field";
    let first_message = format!(
        "{first_subject}\n\nBody of the first.\n\n{}\n",
        trailer(TRAILER_MODEL)
    );
    let _first = repo.commit("one.txt", &first_message);

    let second_subject = "runtime: read the second range";
    let second_message = format!(
        "{second_subject}\n\nBody of the second.\n\n{}\n",
        trailer(TRAILER_MODEL)
    );
    let second = repo.commit("two.txt", &second_message);

    let commits = read_commits(repo.path(), &base).expect("the range reads");
    assert_eq!(commits.len(), 2, "two commits in {base}..HEAD");

    // Newest first, as `git log` prints.
    assert_eq!(commits[0].sha, second);
    assert_eq!(commits[0].subject, second_subject);
    assert!(commits[0].message.contains("Body of the second."));
    assert_eq!(
        commits[0].trailers,
        vec![(
            "Co-Authored-By".to_string(),
            format!("aigentic ({TRAILER_MODEL}) <332865255+aigentic-bot@users.noreply.github.com>")
        )]
    );
    assert_eq!(commits[0].paths, vec!["two.txt".to_string()]);

    let first_sha = repo.git(&["rev-parse", "HEAD~1"]).trim().to_string();
    assert_eq!(commits[1].sha, first_sha);
    assert_eq!(commits[1].subject, first_subject);
    assert_eq!(commits[1].paths, vec!["one.txt".to_string()]);

    // The reader is newest-first, and the plan's order is the reverse.
    let mut reversed: Vec<String> = commits.iter().map(|c| c.subject.clone()).collect();
    reversed.reverse();
    assert_eq!(
        reversed,
        vec![first_subject.to_string(), second_subject.to_string()]
    );
}

/// T2: E1 passes commits whose trailers name the profile's model.
#[test]
fn e1_passes_the_profiles_trailer() {
    let repo = Repo::new();
    repo.commit("base.txt", "base\n");
    let base = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
    repo.commit("one.txt", &format!("one\n\n{}\n", trailer(TRAILER_MODEL)));
    let commits = read_commits(repo.path(), &base).expect("the range reads");

    let report = StepReport::default();
    let subjects = vec!["one".to_string()];
    assert_eq!(verdict("e1", &input(&commits, &subjects, &report)), Ok(()));
}

/// T3: E1 fails a trailer naming another model.
#[test]
fn e1_fails_a_wrong_model() {
    let repo = Repo::new();
    repo.commit("base.txt", "base\n");
    let base = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
    repo.commit("one.txt", &format!("one\n\n{}\n", trailer("glm-4.6")));
    let commits = read_commits(repo.path(), &base).expect("the range reads");

    let report = StepReport::default();
    let subjects = vec!["one".to_string()];
    let error = verdict("e1", &input(&commits, &subjects, &report)).expect_err("a mismatch");
    assert!(error.contains("glm-4.6"), "{error}");
}

/// T4: E1 fails a trailer with no blank line before it: git parses none.
#[test]
fn e1_fails_a_trailer_with_no_blank_line() {
    let repo = Repo::new();
    repo.commit("base.txt", "base\n");
    let base = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
    repo.commit(
        "one.txt",
        &format!(
            "one\nSubject and trailer with no blank line between.\n{}\n",
            trailer(TRAILER_MODEL)
        ),
    );
    let commits = read_commits(repo.path(), &base).expect("the range reads");
    assert_eq!(commits[0].trailers, vec![], "git parses no trailer");

    let report = StepReport::default();
    let subjects = vec!["one".to_string()];
    let error = verdict("e1", &input(&commits, &subjects, &report)).expect_err("a failure");
    assert!(error.contains("no trailer parsed"), "{error}");
}

/// T5: E2 passes the reader's own subjects, fails a count mismatch (#54's
/// extra-commit shape) and a subject swap (#47's shape).
#[test]
fn e2_compares_subjects_exactly() {
    let repo = Repo::new();
    repo.commit("base.txt", "base\n");
    let base = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
    repo.commit(
        "one.txt",
        &format!("runtime: one\n\n{}\n", trailer(TRAILER_MODEL)),
    );
    repo.commit(
        "two.txt",
        &format!("runtime: two\n\n{}\n", trailer(TRAILER_MODEL)),
    );
    let commits = read_commits(repo.path(), &base).expect("the range reads");

    let report = StepReport::default();
    let named = vec!["runtime: one".to_string(), "runtime: two".to_string()];
    assert_eq!(verdict("e2", &input(&commits, &named, &report)), Ok(()));

    // Count: one named message too many.
    let named = vec![
        "runtime: one".to_string(),
        "runtime: two".to_string(),
        "runtime: three".to_string(),
    ];
    let error = verdict("e2", &input(&commits, &named, &report)).expect_err("a count mismatch");
    assert!(error.contains("2 commits, 3 named messages"), "{error}");

    // Swap: the subjects are there, in the wrong order.
    let named = vec!["runtime: two".to_string(), "runtime: one".to_string()];
    let error = verdict("e2", &input(&commits, &named, &report)).expect_err("a swap");
    assert!(error.contains("commit 1: got \"runtime: one\""), "{error}");
}

/// T6: E3 fails `.scratch/x.rs` and `.aigentic/rules.toml`, passes the
/// clean repository the other cases use.
#[test]
fn e3_refuses_scratch_and_the_rules_file() {
    let repo = Repo::new();
    repo.commit("base.txt", "base\n");
    let base = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
    repo.commit(
        "one.txt",
        &format!("runtime: one\n\n{}\n", trailer(TRAILER_MODEL)),
    );
    let commits = read_commits(repo.path(), &base).expect("the range reads");

    let report = StepReport::default();
    let named = vec!["runtime: one".to_string()];
    assert_eq!(verdict("e3", &input(&commits, &named, &report)), Ok(()));

    let scratch = vec![ObservedCommit {
        sha: commits[0].sha.clone(),
        subject: commits[0].subject.clone(),
        message: commits[0].message.clone(),
        trailers: commits[0].trailers.clone(),
        paths: vec![".scratch/x.rs".to_string()],
    }];
    let error = verdict("e3", &input(&scratch, &named, &report)).expect_err("scratch");
    assert!(error.contains(".scratch/x.rs"), "{error}");

    let rules = vec![ObservedCommit {
        paths: vec![".aigentic/rules.toml".to_string()],
        ..scratch[0].clone()
    }];
    let error = verdict("e3", &input(&rules, &named, &report)).expect_err("the rules file");
    assert!(error.contains(".aigentic/rules.toml"), "{error}");
}

/// T15: the dispatcher runs ids in order, reports each, and refuses an
/// unknown id rather than skipping it.
#[test]
fn dispatcher_runs_in_order_and_refuses_unknown_ids() {
    let repo = Repo::new();
    repo.commit("base.txt", "base\n");
    let base = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
    repo.commit(
        "one.txt",
        &format!("runtime: one\n\n{}\n", trailer(TRAILER_MODEL)),
    );
    let commits = read_commits(repo.path(), &base).expect("the range reads");

    let report = StepReport::default();
    let named = vec!["runtime: one".to_string()];
    let input = input(&commits, &named, &report);

    let ids = vec!["e1".to_string(), "e3".to_string(), "e2".to_string()];
    let outcomes = run_checks(&ids, &input).expect("known ids");
    assert_eq!(
        outcomes.iter().map(|o| o.id.clone()).collect::<Vec<_>>(),
        ids
    );
    assert!(outcomes.iter().all(|o| o.result == CheckResult::Pass));

    let error = run_checks(&["e9".to_string()], &input).expect_err("an unknown id");
    assert_eq!(error, ChecksError::UnknownId("e9".to_string()));
    assert!(error.to_string().contains("e9"), "{error}");
}
