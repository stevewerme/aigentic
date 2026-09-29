//! E1, E2, E3 and the dispatcher (issue #56, T1–T6, T15).
//!
//! The weekend fixtures and the E4/E5/E7 cases are in the next commit's
//! tests; this one reads the repository itself.

use std::process::Command;

use aigentic_log::{CheckResult, StepReport};
use aigentic_runtime::checks::git::{read_commits, read_commits_between};
use aigentic_runtime::checks::{CheckInput, ChecksError, ObservedCommit, run_checks};
use std::path::Path;

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
    input_with(commits, &[], named_subjects, report, &[])
}

fn input_with<'a>(
    commits: &'a [ObservedCommit],
    events: &'a [aigentic_core::Event],
    named_subjects: &'a [String],
    report: &'a StepReport,
    uncommitted: &'a [String],
) -> CheckInput<'a> {
    CheckInput {
        commits,
        events,
        report,
        named_subjects,
        model: PROFILE_MODEL,
        uncommitted,
    }
}

fn verdict(id: &str, input: &CheckInput<'_>) -> Result<(), String> {
    let outcome = run_checks(&[id.to_string()], input)
        .expect("a known id")
        .pop()
        .expect("one outcome");
    match outcome.result {
        CheckResult::Pass => Ok(()),
        other => Err(outcome
            .detail
            .clone()
            .unwrap_or_else(|| format!("{other:?} without a detail"))),
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

// ---- E4, E5, E7 over the child log (T7-T14, T16-T23) -------------------

use aigentic_core::{Event, EventKind};
use serde_json::json;
use time::OffsetDateTime;
use ulid::Ulid;

/// A hand-built child log: one `assistant_message` per call, one
/// `tool_result` per answer, joined by call id as the runtime joins them.
struct Log {
    events: Vec<Event>,
    next_id: u64,
}

impl Log {
    fn new() -> Self {
        Self {
            events: Vec::new(),
            next_id: 0,
        }
    }

    fn call(&mut self, seq: u64, name: &str, args: serde_json::Value) -> String {
        self.next_id += 1;
        let id = format!("call_{}", self.next_id);
        let payload =
            json!({"blocks": [{"type": "tool_call", "id": id, "name": name, "args": args}]});
        self.push(seq, EventKind::AssistantMessage, payload);
        id
    }

    fn bash(&mut self, seq: u64, command: &str) -> String {
        self.call(seq, "bash", json!({"command": command}))
    }

    fn write_file(&mut self, seq: u64, path: &str) -> String {
        self.call(seq, "write_file", json!({"path": path, "content": "x"}))
    }

    fn answer(&mut self, seq: u64, id: &str, is_error: bool, content: &str) {
        self.push(
            seq,
            EventKind::ToolResult,
            json!({"id": id, "content": content, "is_error": is_error}),
        );
    }

    fn ok(&mut self, seq: u64, id: &str, content: &str) {
        self.answer(seq, id, false, content);
    }

    fn push(&mut self, seq: u64, kind: EventKind, payload: serde_json::Value) {
        self.events.push(Event {
            id: Ulid::generate(),
            thread_id: Ulid::generate(),
            seq,
            kind,
            author: serde_json::from_value(json!({"kind": "agent", "id": "assistant"}))
                .expect("an author"),
            payload,
            parent_event: None,
            created_at: OffsetDateTime::now_utc(),
        });
    }

    fn take(self) -> Vec<Event> {
        self.events
    }
}

/// The three template gate calls, all passing.
fn healthy_gate(log: &mut Log) {
    let fmt = log.bash(1, "cargo fmt");
    log.ok(2, &fmt, "");
    let clippy = log.bash(3, "cargo clippy --all-targets -- -D warnings");
    log.ok(4, &clippy, "Finished `dev` profile");
    let suite = log.bash(5, "cargo test > /tmp/issue56-gate.log 2>&1");
    log.ok(
        6,
        &suite,
        "test result: ok. 12 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out",
    );
}

/// The fixture lines, parsed through the core types.
fn fixture_events(name: &str) -> Vec<Event> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/checks")
        .join(name);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("{}: {err}", path.display()))
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("a log line"))
        .collect()
}

fn verdict_of(events: &[Event], id: &str) -> Result<(), String> {
    let report = StepReport::default();
    let named: Vec<String> = Vec::new();
    verdict(id, &input_with(&[], events, &named, &report, &[]))
}

/// T7: the gate form is the template's, and nothing else.
#[test]
fn e4_gate_form_matcher_accepts_the_template_and_rejects_the_rest() {
    let mut healthy = Log::new();
    healthy_gate(&mut healthy);
    assert_eq!(verdict_of(&healthy.take(), "e4"), Ok(()));

    let rejected = [
        // The echo form: not the template, whatever else it is.
        (
            "cd repo && cargo test > /tmp/log 2>&1; echo \"exit=$?\"",
            "no gate in template form",
        ),
        // A piped suite (#40's usual shape).
        ("cargo test 2>&1 | tail -25", "no gate in template form"),
        // Bare: no log, so not the template's T.
        ("cargo test", "no gate in template form"),
    ];
    for (command, wanted) in rejected {
        let mut log = Log::new();
        let fmt = log.bash(1, "cargo fmt");
        log.ok(2, &fmt, "");
        let clippy = log.bash(3, "cargo clippy --all-targets -- -D warnings");
        log.ok(4, &clippy, "");
        let suite = log.bash(5, command);
        log.ok(6, &suite, "test result: ok. 12 passed; 0 failed");
        let got = verdict_of(&log.take(), "e4").expect_err(command);
        assert!(got.contains(wanted), "{command}: {got}");
    }

    // A clippy gate missing `--all-targets` is not the gate call.
    let mut log = Log::new();
    let fmt = log.bash(1, "cargo fmt");
    log.ok(2, &fmt, "");
    let clippy = log.bash(3, "cargo clippy -- -D warnings");
    log.ok(4, &clippy, "");
    let suite = log.bash(5, "cargo test > /tmp/issue56-gate.log 2>&1");
    log.ok(6, &suite, "test result: ok. 12 passed; 0 failed");
    let got = verdict_of(&log.take(), "e4").expect_err("no clippy gate");
    assert!(got.contains("cargo clippy"), "{got}");
}

/// T8: one case per row of E4's edit table, read from the classifier and
/// then through the check for the row the gate turns on.
#[test]
fn e4_edit_table_row_by_row() {
    use aigentic_runtime::checks::is_an_edit;

    let rows = [
        ("cargo fmt", true),
        ("cargo fmt --check", true),
        ("touch crates/runtime/src/new.rs", true),
        ("echo hi > crates/runtime/src/file.rs", true),
        ("python3 - <<'PY'\nprint(1)\nPY", true),
        ("cargo test", false),
        ("cargo clippy --all-targets -- -D warnings", false),
        ("git log --oneline -5", false),
        ("git status --short", false),
        ("git add crates/runtime/src/checks/mod.rs", false),
        ("git commit -F /tmp/msg.txt", false),
        ("cat > /tmp/56-msg.txt <<'MSG'\nhi\nMSG", false),
        ("git rev-parse --show-toplevel", false),
    ];
    for (command, wanted) in rows {
        assert_eq!(is_an_edit(command), wanted, "{command}");
    }

    // And the row the gate turns on, through E4: an edit between the
    // format and the suite fails, a read-only call does not.
    for (command, is_edit) in [
        ("touch crates/runtime/src/new.rs", true),
        ("git log --oneline -5", false),
    ] {
        let mut log = Log::new();
        let format = log.bash(1, "cargo fmt");
        log.ok(2, &format, "");
        let candidate = log.bash(3, command);
        log.ok(4, &candidate, "");
        let clippy = log.bash(5, "cargo clippy --all-targets -- -D warnings");
        log.ok(6, &clippy, "");
        let suite = log.bash(7, "cargo test > /tmp/issue56-gate.log 2>&1");
        log.ok(8, &suite, "test result: ok. 12 passed; 0 failed");

        let got = verdict_of(&log.take(), "e4");
        if is_edit {
            let detail = got.expect_err(command);
            assert!(detail.contains("edit between"), "{command}: {detail}");
        } else {
            assert_eq!(got, Ok(()), "{command} is not an edit");
        }
    }
}

/// T9: E4 passes a healthy synthetic gate (`## Plan amendment` item 3:
/// no weekend log holds one in template form).
#[test]
fn e4_passes_a_healthy_gate() {
    let mut log = Log::new();
    healthy_gate(&mut log);
    assert_eq!(verdict_of(&log.take(), "e4"), Ok(()));
}

/// T10a: #53's verbatim fixture has no gate in template form; the suite
/// there is chained and echo-suffixed.
#[test]
fn e4_fails_53s_fixture_for_want_of_a_template_gate() {
    let events = fixture_events("53-01M3MP2DRANHTMM2K9XNM52Z6Q.jsonl");
    let got = verdict_of(&events, "e4").expect_err("no template gate");
    assert_eq!(got, "no gate in template form");
}

/// T10b: a synthetic template gate followed by an edit fails E4, and the
/// detail names the edit — the case E4-strict exists for.
#[test]
fn e4_fails_an_edit_after_the_gate_naming_the_edit() {
    let mut log = Log::new();
    healthy_gate(&mut log);
    let edit = log.write_file(7, "crates/runtime/src/checks/mod.rs");
    log.ok(8, &edit, "wrote");
    let got = verdict_of(&log.take(), "e4").expect_err("an edit after the gate");
    assert!(got.contains("edit after the gate"), "{got}");
    assert!(got.contains("crates/runtime/src/checks/mod.rs"), "{got}");
}

/// T11: #54's verbatim fixture, the echo form.
#[test]
fn e4_fails_54s_fixture_for_want_of_a_template_gate() {
    let events = fixture_events("54-01M3MSEFNXYE8RE64074P3YVEP.jsonl");
    let got = verdict_of(&events, "e4").expect_err("the echo form");
    assert_eq!(got, "no gate in template form");
}

/// T14: a named run whose output never came back fails E7 with the
/// redirected-output detail.
#[test]
fn e7_fails_when_the_output_never_came_back() {
    let mut log = Log::new();
    let call = log.bash(
        1,
        "cargo test -p aigentic-runtime --test checks e4_passes_a_healthy_gate > /tmp/out 2>&1",
    );
    log.ok(2, &call, "");
    let got = verdict_of(&log.take(), "e7").expect_err("no output");
    assert!(got.contains("output not in the log (redirected)"), "{got}");
}

/// T13: #34's verbatim fixture ran 0 tests; the fixture's own result line
/// with one pass is the healthy variant.
#[test]
fn e7_fails_the_zero_test_run_and_passes_one_pass() {
    let events = fixture_events("34-01M3KD9HZXX1YX7SFRRR5N6Y80.jsonl");
    let got = verdict_of(&events, "e7").expect_err("0 tests");
    assert!(got.starts_with("running 0 tests for"), "{got}");

    // The fixture's own line, with its zero replaced by a run's one pass.
    let mut passing = events.clone();
    for event in &mut passing {
        if event.kind == EventKind::ToolResult {
            let text = event.payload["content"].as_str().unwrap_or_default();
            event.payload["content"] = json!(text.replace("0 passed", "1 passed"));
        }
    }
    assert_eq!(verdict_of(&passing, "e7"), Ok(()));
}

/// T12: #40's verbatim fixture ran the full suite twice with no edit
/// between; one edit call between the two runs is the healthy variant.
#[test]
fn e5_fails_40s_consecutive_suite_runs_and_passes_with_an_edit() {
    let events = fixture_events("40-01M3E6TA3P6SA7Q6AS5SE8G8CG.jsonl");
    let got = verdict_of(&events, "e5").expect_err("consecutive runs");
    assert!(got.contains("seq 141 and seq 143"), "{got}");

    // The first two full-suite runs only: with three in the slice one edit
    // cannot separate both pairs, and E5's rule is about the pair.
    let first_two: Vec<Event> = events
        .iter()
        .filter(|event| event.seq <= 143)
        .cloned()
        .collect();
    let mut log = Log::new();
    log.events = first_two;
    let edit = log.write_file(142, "crates/runtime/src/turn.rs");
    log.ok(143, &edit, "wrote");
    assert_eq!(verdict_of(&log.take(), "e5"), Ok(()));
}

/// T21 (`## Plan amendment 2` item 1): a gate whose suite failed is not a
/// gate.
#[test]
fn e4_fails_when_the_suite_failed() {
    let mut log = Log::new();
    let fmt = log.bash(1, "cargo fmt");
    log.ok(2, &fmt, "");
    let clippy = log.bash(3, "cargo clippy --all-targets -- -D warnings");
    log.ok(4, &clippy, "");
    let suite = log.bash(5, "cargo test > /tmp/issue56-gate.log 2>&1");
    log.answer(6, &suite, true, "test result: FAILED. 11 passed; 1 failed");
    let got = verdict_of(&log.take(), "e4").expect_err("a failing suite");
    assert_eq!(got, "the test suite failed (seq 5)");
}

/// T22 (`## Plan amendment 2` item 2): E4 fails when a tracked path is
/// left modified after the gate, naming it; `.scratch/` is untracked and
/// never counts.
#[test]
fn e4_fails_a_tested_tree_that_is_not_the_committed_tree() {
    let repo = Repo::new();
    repo.commit("base.txt", "base\n");
    let base = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
    repo.commit(
        "one.txt",
        &format!("runtime: one\n\n{}\n", trailer(TRAILER_MODEL)),
    );
    let commits = read_commits(repo.path(), &base).expect("the range reads");

    let mut log = Log::new();
    healthy_gate(&mut log);
    let events = log.take();

    let report = StepReport::default();
    let named: Vec<String> = Vec::new();
    assert_eq!(
        verdict_of(&events, "e4"),
        Ok(()),
        "the clean repo passes before the edit"
    );

    std::fs::write(repo.path().join("one.txt"), "changed after the gate\n").expect("the edit");
    std::fs::create_dir_all(repo.path().join(".scratch")).expect("the scratch dir");
    std::fs::write(repo.path().join(".scratch/work.txt"), "untracked\n").expect("the scratch file");

    let uncommitted = aigentic_runtime::checks::git::read_uncommitted(repo.path())
        .expect("the working tree reads");
    assert_eq!(uncommitted, vec!["one.txt".to_string()]);
    let got = verdict(
        "e4",
        &input_with(&commits, &events, &named, &report, &uncommitted),
    )
    .expect_err("an uncommitted tracked path");
    assert_eq!(got, "tested tree not committed: one.txt");
}

/// T23 (`## Plan amendment 2` item 4): a name filter counts before and
/// after `--`, whichever way the call is piped; `--list` is not a run.
#[test]
fn e7_reads_the_named_run_forms() {
    let forms = [
        "cd repo && cargo test -p aigentic-runtime --test checks form 2>&1 | tail -30",
        "cargo test -- form",
        "cargo test form --nocapture",
    ];
    for command in forms {
        let mut log = Log::new();
        let call = log.bash(1, command);
        log.ok(
            2,
            &call,
            "test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out",
        );
        let got = verdict_of(&log.take(), "e7").expect_err(command);
        assert!(got.starts_with("running 0 tests for"), "{command}: {got}");
    }

    // `--list` runs nothing, so E7 has no name to check.
    let mut log = Log::new();
    let call = log.bash(1, "cargo test --list -p aigentic-runtime form");
    log.ok(
        2,
        &call,
        "test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out",
    );
    assert_eq!(verdict_of(&log.take(), "e7"), Ok(()));
}

/// T16-T20: the weekend logs, replayed from the real thread the fixture's
/// name carries. Ignored by default; run each with
/// `AIGENTIC_REPLAY_LOG=<thread>.jsonl cargo test -p aigentic-runtime
/// --test checks replay -- --ignored --nocapture`.
fn replay_events() -> Vec<Event> {
    let path = std::env::var("AIGENTIC_REPLAY_LOG").expect("set AIGENTIC_REPLAY_LOG");
    std::fs::read_to_string(&path)
        .expect("the thread log")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("a log line"))
        .collect()
}

/// T16: #34's thread (`01M3KD9HZXX1YX7SFRRR5N6Y80`): a named run that
/// reported `running 0 tests` and was read as a pass.
#[test]
#[ignore = "needs a real thread log; set AIGENTIC_REPLAY_LOG"]
fn thread_ulid_01m3kd9hzxx1yx7sfrrr5n6y80_replays_the_zero_test_run() {
    let events = replay_events();
    let got = verdict_of(&events, "e7").expect_err("#34's zero-test run");
    assert!(got.starts_with("running 0 tests for"), "{got}");
}

/// T17: #40's thread (`01M3E6TA3P6SA7Q6AS5SE8G8CG`): full suites
/// re-run with no edit between.
#[test]
#[ignore = "needs a real thread log; set AIGENTIC_REPLAY_LOG"]
fn thread_ulid_01m3e6ta3p6sa7q6as5se8g8cg_replays_the_repeated_suites() {
    let events = replay_events();
    let got = verdict_of(&events, "e5").expect_err("#40's repeated suite");
    assert!(got.contains("consecutive full-suite runs"), "{got}");
}

/// T18: #47's thread (`01M3GS3QP6R9XVTX6J1SNN3415`): the E2 replay reads
/// the repository's own history, `cf94253~1..9d737bf`, against the plan's
/// four named messages — commit 1's subject differs.
#[test]
#[ignore = "needs the repository's own history"]
fn thread_ulid_01m3gs3qp6r9xvtx6j1snn3415_replays_the_renamed_commits() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let commits = read_commits_between(&repo, "cf94253~1", "9d737bf").expect("the range reads");
    let named: Vec<String> = [
        "log: add slept and keep-awake fields to turn_ended",
        "runtime: measure wall beside running and report slept turns",
        "server: hold one keep-awake guard while turns work, behind keep_awake",
        "tui: report slept turns on the turn line, in stats and in doctor",
    ]
    .iter()
    .map(|subject| subject.to_string())
    .collect();
    let report = StepReport::default();
    // Oldest first is the plan's order: the reader gives newest first.
    let oldest_first: Vec<&ObservedCommit> = commits.iter().rev().collect();
    assert_eq!(named.len(), oldest_first.len(), "the range is four commits");
    assert_eq!(
        oldest_first[0].subject,
        "log: add slept, wall and keep-awake fields to turn_ended"
    );
    assert_eq!(
        named[0],
        "log: add slept and keep-awake fields to turn_ended"
    );
    assert_eq!(
        oldest_first[3].subject, named[3],
        "commit 4 matches the plan"
    );

    let got = verdict("e2", &input(&commits, &named, &report)).expect_err("commit 1 differs");
    assert!(got.starts_with("commit 1:"), "{got}");
    assert!(
        got.contains("log: add slept and keep-awake fields"),
        "{got}"
    );
}

/// T19: #53's thread (`01M3MP2DRANHTMM2K9XNM52Z6Q`): no gate in template
/// form, and the doc comment edited after the suite.
#[test]
#[ignore = "needs a real thread log; set AIGENTIC_REPLAY_LOG"]
fn thread_ulid_01m3mp2dranhtmm2k9xnm52z6q_replays_the_missing_gate() {
    let events = replay_events();
    let got = verdict_of(&events, "e4").expect_err("#53's chained gate");
    assert_eq!(got, "no gate in template form");
}

/// T20: #54's thread (`01M3MSEFNXYE8RE64074P3YVEP`): the echo-form gate.
#[test]
#[ignore = "needs a real thread log; set AIGENTIC_REPLAY_LOG"]
fn thread_ulid_01m3msefnxye8re64074p3yvep_replays_the_echo_form_gate() {
    let events = replay_events();
    let got = verdict_of(&events, "e4").expect_err("#54's echo-form gate");
    assert_eq!(got, "no gate in template form");
}
