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

use aigentic_core::Event;
use aigentic_log::{CheckOutcome, CheckResult, StepReport};
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
}

/// A check id the dispatcher does not have.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ChecksError {
    #[error("unknown check {0:?}")]
    UnknownId(String),
}

/// §6.2: run the named checks in order and report each one's outcome.
pub fn run_checks(
    ids: &[String],
    input: &CheckInput<'_>,
) -> Result<Vec<CheckOutcome>, ChecksError> {
    let mut outcomes = Vec::with_capacity(ids.len());
    for id in ids {
        let verdict = match id.as_str() {
            "e1" => e1_commit_trailers(input),
            "e2" => e2_named_subjects(input),
            "e3" => e3_forbidden_paths(input),
            other => return Err(ChecksError::UnknownId(other.to_string())),
        };
        outcomes.push(outcome(id, verdict));
    }
    Ok(outcomes)
}

fn outcome(id: &str, verdict: Result<(), String>) -> CheckOutcome {
    match verdict {
        Ok(()) => CheckOutcome {
            id: id.to_string(),
            result: CheckResult::Pass,
            detail: None,
        },
        Err(detail) => CheckOutcome {
            id: id.to_string(),
            result: CheckResult::Fail,
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
