//! The exact checks of §6.1, issue #56.
//!
//! One check covers one row of §6.1's table and answers pass or fail; a
//! `fail` blocks the push. The checks are code, not model: every input is
//! a fact already in the log or in git — the commits, the child's
//! `step_reported` report, and every bash call's own segments and exit
//! status.
//!
//! [`run_checks`] is the dispatcher: §6.2 runs the named ids in order, and
//! an unknown id is an error rather than a check that passes. The runner
//! (#57) owns the ids, the terminal command, and the response to a `fail`;
//! this module owns the verdict on each id.

pub mod git;

use std::collections::HashMap;
use std::path::Path;

use aigentic_core::{ContentBlock, Event, EventKind};
use aigentic_log::{
    AssistantMessagePayload, CheckOutcome, CheckResult, StepReport, ToolResultPayload,
};
use aigentic_policy::{Kind, ScanSegment, scan};
use thiserror::Error;

use crate::workflow::render::trailer_model;

/// The address every `Co-Authored-By` trailer must carry, verbatim.
const TRAILER_ADDRESS: &str = "332865255+aigentic-bot@users.noreply.github.com";

/// One commit of the step's range, read with the plan's git invocations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedCommit {
    /// Full hex sha.
    pub sha: String,
    /// First line of the message.
    pub subject: String,
    /// The whole message, subject included.
    pub message: String,
    /// The `Co-Authored-By` trailers git parsed, name then value.
    pub trailers: Vec<(String, String)>,
    /// Every path the commit changed, repo-relative.
    pub paths: Vec<String>,
}

impl ObservedCommit {
    /// The one trailer value the rule allows, when the commit has exactly
    /// one; `Err` names what the reader found instead.
    fn trailer(&self) -> Result<&str, String> {
        let short = short_sha(&self.sha);
        let values: Vec<&str> = self
            .trailers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("Co-Authored-By"))
            .map(|(_, value)| value.as_str())
            .collect();
        match values.as_slice() {
            [] => Err(format!("{short}: no trailer parsed")),
            [only] => Ok(only),
            many => Err(format!(
                "{short}: {} Co-Authored-By trailers: {}",
                many.len(),
                many.join(", ")
            )),
        }
    }
}

/// The facts one run of the checks reads.
pub struct CheckInput<'a> {
    /// The step's own commits, newest first, as the reader returns them.
    pub commits: &'a [ObservedCommit],
    /// The child thread's events, oldest first.
    pub events: &'a [Event],
    /// The child's `finish_step` report.
    pub report: &'a StepReport,
    /// The plan's named commit messages, in the plan's order.
    pub named_subjects: &'a [String],
    /// The profile's model, e.g. `deepseek/deepseek-v4.1-flash`.
    pub model: &'a str,
    /// Tracked paths git reports modified at check time, untracked files
    /// excluded: the tested tree must be the committed tree (E4).
    pub uncommitted: &'a [String],
}

/// A check id the dispatcher does not have.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ChecksError {
    #[error("unknown check {0:?}")]
    UnknownId(String),
}

/// §6.2: run the named checks in order and report each one's outcome.
/// The ids are §6.1's, as the workflow writes them (`E1`…`E5`, `E7`); an
/// id with no check is an error, never a silent pass.
pub fn run_checks(
    ids: &[String],
    input: &CheckInput<'_>,
) -> Result<Vec<CheckOutcome>, ChecksError> {
    let mut outcomes = Vec::with_capacity(ids.len());
    for id in ids {
        // The result a check reports when it has something to say: `fail`
        // blocks the push, `flag` does not (`§6.1`, f591e51). E5 is the
        // one flag — a step cannot remove a run already in its log, so
        // the flag is what a step back cites.
        let (verdict, on_failure) = match id.as_str() {
            "E1" => (e1_commit_trailers(input), CheckResult::Fail),
            "E2" => (e2_named_subjects(input), CheckResult::Fail),
            "E3" => (e3_forbidden_paths(input), CheckResult::Fail),
            "E4" => (e4_gate(input), CheckResult::Fail),
            "E5" => (e5_full_suites(input), CheckResult::Flag),
            "E7" => (e7_named_runs(input), CheckResult::Fail),
            other => return Err(ChecksError::UnknownId(other.to_string())),
        };
        outcomes.push(outcome(id, verdict, on_failure));
    }
    Ok(outcomes)
}

fn outcome(id: &str, verdict: Result<(), String>, on_failure: CheckResult) -> CheckOutcome {
    match verdict {
        Ok(()) => CheckOutcome {
            id: id.to_string(),
            result: CheckResult::Pass,
            detail: None,
        },
        Err(detail) => CheckOutcome {
            id: id.to_string(),
            result: on_failure,
            detail: Some(detail),
        },
    }
}

/// E1: every commit carries exactly one `Co-Authored-By` trailer whose
/// value is the profile's model at the fixed address. A trailer with no
/// blank line before it is not a trailer — git does not parse it, so the
/// reader reports "no trailer parsed".
fn e1_commit_trailers(input: &CheckInput<'_>) -> Result<(), String> {
    let wanted = format!(
        "aigentic ({}) <{TRAILER_ADDRESS}>",
        trailer_model(input.model)
    );
    for commit in input.commits {
        let got = commit.trailer()?;
        if got != wanted {
            return Err(format!(
                "{}: Co-Authored-By {got:?}, expected {wanted:?}",
                short_sha(&commit.sha)
            ));
        }
    }
    Ok(())
}

/// E2: the plan's named messages, in author order, are the commit
/// subjects exactly — count, order and text.
fn e2_named_subjects(input: &CheckInput<'_>) -> Result<(), String> {
    // The reader lists newest first; the plan names oldest first.
    let authored: Vec<&ObservedCommit> = input.commits.iter().rev().collect();
    if authored.len() != input.named_subjects.len() {
        return Err(format!(
            "{} commits, {} named messages",
            authored.len(),
            input.named_subjects.len()
        ));
    }
    for (index, (commit, named)) in authored.iter().zip(input.named_subjects).enumerate() {
        if commit.subject != *named {
            return Err(format!(
                "commit {}: got {:?}, expected {:?}",
                index + 1,
                commit.subject,
                named
            ));
        }
    }
    Ok(())
}

/// E3: any path at or under `.scratch/`, or equal to
/// `.aigentic/rules.toml`, fails. The source is the commits' own paths,
/// so a path a commit did not touch is not a change of the step's.
fn e3_forbidden_paths(input: &CheckInput<'_>) -> Result<(), String> {
    for commit in input.commits {
        for path in &commit.paths {
            let path = path.trim_start_matches("./");
            if path == SCRATCH.trim_end_matches('/') || path.starts_with(SCRATCH) || path == RULES {
                return Err(format!("{}: {path}", short_sha(&commit.sha)));
            }
        }
    }
    Ok(())
}

/// Paths no step may commit, and the file the harness owns (§6.1).
pub const SCRATCH: &str = ".scratch/";
pub const RULES: &str = ".aigentic/rules.toml";

fn short_sha(sha: &str) -> &str {
    let limit = sha.len().min(8);
    &sha[..limit]
}

/// One tool call the log recorded, with the answer to it when the log has
/// one; a call whose result is outside the slice has none.
#[derive(Debug, Clone)]
struct Call {
    seq: u64,
    name: String,
    args: serde_json::Value,
    answer: Option<Answer>,
}

#[derive(Debug, Clone)]
struct Answer {
    is_error: bool,
    content: String,
}

impl Call {
    fn command(&self) -> Option<&str> {
        (self.name == "bash")
            .then(|| self.args.get("command").and_then(|value| value.as_str()))
            .flatten()
    }

    /// The E4 edit table: `edit_file` and `write_file` write, a bash call
    /// writes when any of its segments does.
    fn is_edit(&self) -> bool {
        match self.name.as_str() {
            "edit_file" | "write_file" => true,
            "bash" => self.command().is_some_and(is_an_edit),
            _ => false,
        }
    }

    /// What the call touched, for a detail that names the edit.
    fn what(&self) -> String {
        match self.name.as_str() {
            "edit_file" | "write_file" => {
                let path = self
                    .args
                    .get("path")
                    .and_then(|value| value.as_str())
                    .unwrap_or("?");
                format!("{} {path}", self.name)
            }
            "bash" => format!("bash {:?}", elide(self.command().unwrap_or(""), 120)),
            other => other.to_string(),
        }
    }
}

/// E4: the gate ran in template form — `cargo fmt`, then `cargo clippy
/// --all-targets -- -D warnings`, then `cargo test > <log> 2>&1` — each
/// before the last, all three passing, nothing edited between the format
/// and the suite, and the tested tree committed. A gate command may carry
/// one leading `cd <dir> &&` (the shared working directory's habit,
/// `## Supervisor findings` item 3); nothing else may precede it.
fn e4_gate(input: &CheckInput<'_>) -> Result<(), String> {
    let calls = calls(input.events);
    let format = calls.iter().rfind(|call| is_gate(call, GATE_FORMAT));
    let clippy = calls.iter().rfind(|call| is_gate(call, GATE_CLIPPY));
    let suite = calls
        .iter()
        .rfind(|call| call.command().is_some_and(is_gate_suite));
    let Some(suite) = suite else {
        return Err("no gate in template form".to_string());
    };

    // The suite itself must have passed: a failing gate is not a gate.
    match &suite.answer {
        Some(answer) if answer.is_error => {
            return Err(format!("the test suite failed (seq {})", suite.seq));
        }
        Some(_) => {}
        None => {
            return Err(format!(
                "the test suite has no answer in the log (seq {})",
                suite.seq
            ));
        }
    }

    let Some(format) = format else {
        return Err("no `cargo fmt` gate".to_string());
    };
    let Some(clippy) = clippy else {
        return Err("no `cargo clippy --all-targets -- -D warnings` gate".to_string());
    };
    if format.seq > clippy.seq || clippy.seq > suite.seq {
        return Err(format!(
            "gate out of order: fmt seq {}, clippy seq {}, suite seq {}",
            format.seq, clippy.seq, suite.seq
        ));
    }
    for (label, call) in [("cargo fmt", format), ("cargo clippy", clippy)] {
        if call.answer.as_ref().is_some_and(|answer| answer.is_error) {
            return Err(format!("{label} failed (seq {})", call.seq));
        }
    }

    // Nothing edited between the formatting and the suite: the suite tests
    // the formatted tree, so `cargo fmt` itself is not an edit here.
    if let Some(edit) = calls
        .iter()
        .find(|call| call.is_edit() && call.seq > format.seq && call.seq < suite.seq)
    {
        return Err(format!(
            "edit between the gate's `cargo fmt` and its `cargo test`: {} (seq {})",
            edit.what(),
            edit.seq
        ));
    }
    if let Some(edit) = calls
        .iter()
        .find(|call| call.is_edit() && call.seq > suite.seq)
    {
        return Err(format!(
            "edit after the gate: {} (seq {})",
            edit.what(),
            edit.seq
        ));
    }

    if !input.uncommitted.is_empty() {
        return Err(format!(
            "tested tree not committed: {}",
            input.uncommitted.join(", ")
        ));
    }
    Ok(())
}

/// E5: two full-suite runs with no edit between them are the same work
/// twice — reported as a `flag`, not a `fail` (§6.1, f591e51: a step
/// cannot remove a run already in its log, so the flag is what a step
/// back cites). A full suite is a `cargo test` segment that selects no
/// package and no target and names no filter, piped or chained
/// (`## Plan amendment` item 1, `## Supervisor findings` item 2); a partial
/// run is not one.
fn e5_full_suites(input: &CheckInput<'_>) -> Result<(), String> {
    let calls = calls(input.events);
    let runs: Vec<&Call> = calls
        .iter()
        .filter(|call| call.command().is_some_and(is_full_suite))
        .collect();
    for pair in runs.windows(2) {
        let (first, second) = (pair[0], pair[1]);
        let edited = calls
            .iter()
            .any(|call| call.is_edit() && call.seq > first.seq && call.seq < second.seq);
        if !edited {
            return Err(format!(
                "consecutive full-suite runs at seq {} and seq {} with no edit between",
                first.seq, second.seq
            ));
        }
    }
    Ok(())
}

/// E7: a run that names a test must have run it — at least one `passed`
/// in its own result, or the output never came back. Per name filter
/// only the latest run counts (`## Supervisor findings` item 4): a
/// debugging run that found nothing is superseded by the run that
/// repeats it, so a name is judged by the last word in the log about it,
/// not the first. The rule is #34's: `running 0 tests` is never a pass
/// unless a later run of the same filter says otherwise.
fn e7_named_runs(input: &CheckInput<'_>) -> Result<(), String> {
    let calls = calls(input.events);
    let mut latest: HashMap<String, usize> = HashMap::new();
    for (index, call) in calls.iter().enumerate() {
        let Some(name) = call.command().and_then(named_run) else {
            continue;
        };
        latest.insert(name, index);
    }
    // In the log's order, so the detail names the earliest name at fault.
    let mut judged: Vec<(usize, &String)> = latest.iter().map(|(name, at)| (*at, name)).collect();
    judged.sort_by_key(|(at, _)| *at);
    for (index, name) in judged {
        let call = &calls[index];
        let Some(answer) = &call.answer else {
            return Err(format!("{name:?}: output not in the log (redirected)"));
        };
        if !answer.content.contains("test result:") {
            return Err(format!("{name:?}: output not in the log (redirected)"));
        }
        if passed_count(&answer.content) == 0 {
            return Err(format!("running 0 tests for {name:?}"));
        }
    }
    Ok(())
}

/// Every tool call in the slice, in order, each with its result when the
/// slice holds it: one walk of the events, joined by call id.
fn calls(events: &[Event]) -> Vec<Call> {
    let mut calls: Vec<Call> = Vec::new();
    let mut at: HashMap<String, usize> = HashMap::new();
    for event in events {
        match event.kind {
            EventKind::AssistantMessage => {
                let Ok(payload) =
                    serde_json::from_value::<AssistantMessagePayload>(event.payload.clone())
                else {
                    continue;
                };
                for block in payload.blocks {
                    if let ContentBlock::ToolCall(call) = block {
                        at.insert(call.id.clone(), calls.len());
                        calls.push(Call {
                            seq: event.seq,
                            name: call.name,
                            args: call.args,
                            answer: None,
                        });
                    }
                }
            }
            EventKind::ToolResult => {
                let Ok(payload) =
                    serde_json::from_value::<ToolResultPayload>(event.payload.clone())
                else {
                    continue;
                };
                if let Some(&index) = at.get(&payload.result.id) {
                    calls[index].answer = Some(Answer {
                        is_error: payload.result.is_error,
                        content: payload.result.content,
                    });
                }
            }
            _ => {}
        }
    }
    calls
}

/// The gate's two fixed commands, verbatim, behind one optional `cd`.
const GATE_FORMAT: &str = "cargo fmt";
const GATE_CLIPPY: &str = "cargo clippy --all-targets -- -D warnings";

/// A gate command, behind the one leading `cd` a build habitually uses.
fn is_gate(call: &Call, form: &str) -> bool {
    call.command()
        .is_some_and(|command| behind_a_leading_cd(command) == form)
}

/// The command behind one leading `cd <dir> &&`, when that is all that
/// precedes it: a build changes into its directory and runs the gate
/// there (`## Supervisor findings` item 3). The directory may be a
/// substitution (`cd "$(git rev-parse --show-toplevel)"`), but it may not
/// hide another command: a `;`, a pipe, a second `&&` or a redirection in
/// it means something else came first, and nothing is stripped then.
fn behind_a_leading_cd(command: &str) -> &str {
    let Some(rest) = command.strip_prefix("cd ") else {
        return command;
    };
    let Some(at) = rest.find(" &&") else {
        return command;
    };
    let dir = &rest[..at];
    if dir.is_empty() || dir.contains([';', '|', '&', '<', '>']) {
        return command;
    }
    rest[at + 3..].trim_start()
}

/// The gate's suite form: exactly `cargo test > <one plain-word log>
/// 2>&1`, behind the optional leading `cd`, one segment and nothing
/// after it.
fn is_gate_suite(command: &str) -> bool {
    let command = behind_a_leading_cd(command.trim()).trim();
    let Some(rest) = command.strip_prefix("cargo test > ") else {
        return false;
    };
    let Some(log) = rest.strip_suffix(" 2>&1") else {
        return false;
    };
    !log.is_empty()
        && !log.contains(|c: char| c.is_whitespace())
        && !log.contains(['|', '&', ';', '<', '>'])
}

/// A full suite: a `cargo test` segment that selects no package and no
/// target and names no filter, so the whole workspace runs
/// (`## Supervisor findings` item 2). Flags that take a value
/// (`--manifest-path`, `--features`) keep their value out of the
/// filter's way; a `--list` run runs nothing and is no suite.
fn is_full_suite(command: &str) -> bool {
    scan(command)
        .iter()
        .any(|segment| match cargo_test_args(segment) {
            Some(args) => !args.lists && !args.selection && args.name.is_none(),
            None => false,
        })
}

/// What a `cargo test` segment asks for.
struct TestArgs {
    /// The name filter, when it names one. `--list` runs nothing, so it
    /// names none.
    name: Option<String>,
    /// A package or a target kind is selected: a partial run.
    selection: bool,
    /// `--list`: nothing runs.
    lists: bool,
}

/// The arguments of a `cargo test` segment, when the segment is one.
/// `named_run` and `is_full_suite` read the same walk of the words, so
/// they agree on what a package, a target and a filter are.
fn cargo_test_args(segment: &ScanSegment) -> Option<TestArgs> {
    let words = &segment.words;
    if words.len() < 2 || words[0] != "cargo" || words[1] != "test" {
        return None;
    }
    let mut args = TestArgs {
        name: None,
        selection: false,
        lists: false,
    };
    let mut after_separator = false;
    let mut skip = false;
    for word in words.iter().skip(2) {
        if skip {
            skip = false;
            continue;
        }
        if word == "--" {
            after_separator = true;
            continue;
        }
        if word.starts_with('-') {
            // A flag before `--` may take the next word as its value; a
            // flag after it is the test binary's own.
            if !after_separator && takes_a_value(word) && !word.contains('=') {
                skip = true;
            }
            if selects_a_target(word) {
                args.selection = true;
            }
            if word == "--list" {
                args.lists = true;
            }
            continue;
        }
        if args.name.is_none() {
            args.name = Some(word.clone());
        }
    }
    if args.lists {
        args.name = None;
    }
    Some(args)
}

/// A flag that picks what runs: a package or a target kind
/// (`## Supervisor findings` item 2).
fn selects_a_target(flag: &str) -> bool {
    matches!(
        flag.split('=').next().unwrap_or(flag),
        "-p" | "--package"
            | "--lib"
            | "--bin"
            | "--bins"
            | "--test"
            | "--tests"
            | "--example"
            | "--examples"
            | "--bench"
            | "--benches"
            | "--doc"
    )
}

/// The test name a `cargo test` call filters on, when it names one. A
/// call with `--list` runs nothing, so it names none (`## Plan
/// amendment 2` item 4).
fn named_run(command: &str) -> Option<String> {
    scan(command).iter().find_map(cargo_test_args)?.name
}

/// Does this flag take the next word as its value?
fn takes_a_value(flag: &str) -> bool {
    matches!(
        flag,
        "-p" | "--package"
            | "--test"
            | "--bench"
            | "--example"
            | "--bin"
            | "--features"
            | "-F"
            | "--skip"
            | "--exclude"
            | "--jobs"
            | "-j"
            | "--manifest-path"
    )
}

/// The tests the run's own `test result:` lines report as passed
/// (multi-target reads count).
fn passed_count(output: &str) -> u64 {
    output
        .lines()
        .filter_map(|line| {
            let rest = line.split("test result:").nth(1)?;
            // " ok. 1 passed; 0 failed; …" -> "1".
            let before = rest.split(" passed").next()?;
            before.split_whitespace().next_back()?.parse::<u64>().ok()
        })
        .sum()
}

/// A bash call is an edit unless every segment is harmless or read-only.
/// The command-head rows win over the classify-based ones (`## Plan
/// amendment` item 4): `cargo fmt` is allow-listed, so it classifies
/// read-only, and it is still an edit.
pub fn is_an_edit(command: &str) -> bool {
    let segments = scan(&without_heredocs(command));
    if segments.is_empty() {
        return true;
    }
    segments.iter().any(segment_is_an_edit)
}

/// The command with heredoc bodies removed: the lines between a `<<WORD`
/// opener and the `WORD` line that ends it are the redirection's data, not
/// commands of their own, so the edit table must not read an English
/// sentence there as one. A body that names `git add` would otherwise make
/// every `cat > /tmp/msg.txt <<'MSG'` call an edit.
fn without_heredocs(command: &str) -> String {
    let mut kept: Vec<String> = Vec::new();
    let mut pending: Vec<String> = Vec::new();
    for line in command.lines() {
        if let Some(terminator) = pending.first() {
            if line.trim() == terminator.as_str() {
                pending.remove(0);
            }
            continue;
        }
        let (stripped, terminators) = strip_heredocs(line);
        pending.extend(terminators);
        kept.push(stripped);
    }
    kept.join("\n")
}

/// One line with its `<<WORD` openers removed, and the terminators they
/// wait for. The opener goes too: its word is not a path a redirection
/// check should read as a write target.
fn strip_heredocs(line: &str) -> (String, Vec<String>) {
    let bytes = line.as_bytes();
    let mut kept = String::new();
    let mut terminators = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if index + 1 < bytes.len() && bytes[index] == b'<' && bytes[index + 1] == b'<' {
            let mut at = index + 2;
            if at < bytes.len() && bytes[at] == b'-' {
                at += 1;
            }
            while at < bytes.len() && bytes[at] == b' ' {
                at += 1;
            }
            let rest = &line[at..];
            let (word, after) = match rest.chars().next() {
                Some(quote @ ('\'' | '"')) => {
                    let inner = &rest[1..];
                    let word = inner.split(quote).next().unwrap_or_default();
                    let end = at + 1 + word.len() + 1;
                    (word.to_string(), end.min(bytes.len()))
                }
                _ => {
                    let word: String = rest
                        .split(|c: char| c.is_whitespace() || c == ';' || c == '|' || c == '&')
                        .next()
                        .unwrap_or_default()
                        .to_string();
                    ((word.clone()), at + word.len())
                }
            };
            if !word.is_empty() {
                terminators.push(word.trim_matches(|c| c == '\'' || c == '"').to_string());
            }
            kept.push(' ');
            index = after.max(index + 2);
            continue;
        }
        let ch = line[index..].chars().next().unwrap_or(' ');
        kept.push(ch);
        index += ch.len_utf8();
    }
    (kept, terminators)
}

fn segment_is_an_edit(segment: &ScanSegment) -> bool {
    let head = segment.words.as_slice();
    if head.starts_with(&["cargo".to_string(), "fmt".to_string()]) {
        return true;
    }
    if head.starts_with(&["git".to_string(), "add".to_string()])
        || head.starts_with(&["git".to_string(), "commit".to_string()])
        || head.starts_with(&["git".to_string(), "status".to_string()])
    {
        return false;
    }
    // A `Write` whose own command writes nothing and whose every
    // redirection lands in a temp directory is not an edit: the report
    // and pty scratch files of a real build. A write that came from the
    // command (or from a `$(…)` inside it) is always an edit — the
    // redirection only collects it (issue #56's review) — and so is a
    // relative target: it resolves inside the repo, so that way fails
    // safe.
    if segment.kind == Kind::Write
        && !segment.write_from_command
        && !segment.redirects.is_empty()
        && segment.redirects.iter().all(|target| to_temp(target))
    {
        return false;
    }
    !matches!(segment.kind, Kind::ReadOnly | Kind::Harmless)
}

fn to_temp(target: &str) -> bool {
    let path = Path::new(target);
    path.is_absolute()
        && (path.starts_with("/tmp")
            || path.starts_with("/var/folders")
            || path.starts_with(std::env::temp_dir()))
}

fn elide(text: &str, limit: usize) -> String {
    let text = text.replace('\n', " ");
    if text.chars().count() <= limit {
        return text;
    }
    let kept: String = text.chars().take(limit).collect();
    format!("{kept}…")
}
