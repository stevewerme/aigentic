//! The git reader: the commits a step made, read with git's own
//! invocations so that the parsing rules are git's, not ours.
//!
//! `read_commits` returns the commits of `base..HEAD`, newest first (the
//! order `git log` prints). A trailer with no blank line before it is not
//! a trailer: `git interpret-trailers --parse` decides, which is what
//! makes E1's blank-line requirement git's rule rather than ours.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use super::ObservedCommit;

/// Read `base..HEAD` in `repo`: sha, subject, message, trailers, paths.
pub fn read_commits(repo: &Path, base: &str) -> std::io::Result<Vec<ObservedCommit>> {
    read_commits_between(repo, base, "HEAD")
}

/// Read `base..topic`, the same way. The end is named so a replay can read
/// a thread's own range rather than everything up to today's HEAD.
pub fn read_commits_between(
    repo: &Path,
    base: &str,
    topic: &str,
) -> std::io::Result<Vec<ObservedCommit>> {
    let listed = git(
        repo,
        &["log", "--format=%H%x1f%s", &format!("{base}..{topic}")],
    )?;
    let mut commits = Vec::new();
    for line in listed.lines() {
        let Some((sha, subject)) = line.split_once('\u{1f}') else {
            continue;
        };
        let message = git(repo, &["show", "-s", "--format=%B", sha])?;
        let message = message.trim_matches('\n').to_string();
        let trailers = interpret_trailers(parse_trailers(repo, &message)?);
        let paths = git(
            repo,
            &["diff-tree", "--no-commit-id", "--name-only", "-r", sha],
        )?
        .lines()
        .map(|path| path.trim().to_string())
        .filter(|path| !path.is_empty())
        .collect();
        commits.push(ObservedCommit {
            sha: sha.to_string(),
            subject: subject.to_string(),
            message,
            trailers,
            paths,
        });
    }
    Ok(commits)
}

/// The tracked paths git reports modified now: untracked files excluded,
/// so `.scratch/` and `.aigentic/rules.toml` never show up here.
pub fn read_uncommitted(repo: &Path) -> std::io::Result<Vec<String>> {
    Ok(
        git(repo, &["status", "--porcelain", "--untracked-files=no"])?
            .lines()
            .filter_map(|line| {
                let (_, path) = line.split_at(line.len().min(3));
                let path = path.trim();
                (!path.is_empty()).then(|| path.to_string())
            })
            .collect(),
    )
}

/// `git interpret-trailers --parse`'s output for one message.
fn parse_trailers(repo: &Path, message: &str) -> std::io::Result<String> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["interpret-trailers", "--parse"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin.write_all(message.as_bytes())?;
        stdin.write_all(b"\n")?;
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(std::io::Error::other(stderr));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The name/value pairs of the parsed trailer block. `--parse` prints the
/// trailers alone — and prints nothing when a trailer has no blank line
/// before it, which is the rule E1 leans on.
fn interpret_trailers(parsed: String) -> Vec<(String, String)> {
    let mut trailers = Vec::new();
    for line in parsed.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() || name.contains(' ') {
            continue;
        }
        trailers.push((name.to_string(), value.trim().to_string()));
    }
    trailers
}

/// Run one git command in the worktree and return its stdout.
fn git(repo: &Path, args: &[&str]) -> std::io::Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(std::io::Error::other(stderr));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
