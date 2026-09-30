//! The git seam: what a writing step's repository looks like from here.
//!
//! [`GitRepo`] shells out to `git`, so nothing in this crate links a git
//! library, and every read a check needs is a plain command. It is a
//! trait so the runner can be driven without a repository at all.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Every way reading or moving a repository can fail.
#[derive(Debug, thiserror::Error)]
pub enum RepoError {
    /// `git` ran and said no.
    #[error("git {args} failed: {message}")]
    Command {
        /// The arguments, space-joined, for the message.
        args: String,
        /// What `git` wrote on stderr.
        message: String,
    },
    /// `git` could not be run at all.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Where a run's commits are read from, and where they go.
pub trait Repo {
    /// The working tree the run's commits live in.
    fn root(&self) -> &Path;
    /// The repository's HEAD, as a full sha.
    fn head(&self) -> Result<String, RepoError>;
    /// The branch HEAD is on.
    fn branch(&self) -> Result<String, RepoError>;
    /// The remote's head for `branch`: the first field of
    /// `git ls-remote origin refs/heads/<branch>`, or `None` when the
    /// remote has no such branch.
    fn remote_head(&self, branch: &str) -> Result<Option<String>, RepoError>;
    /// Push `branch` to the remote. A plain push: never `--force`, never
    /// `-f`, so a remote that moved during the step is never overwritten.
    fn push(&self, branch: &str) -> Result<(), RepoError>;
}

impl<T: Repo + ?Sized> Repo for Box<T> {
    fn root(&self) -> &Path {
        (**self).root()
    }

    fn head(&self) -> Result<String, RepoError> {
        (**self).head()
    }

    fn branch(&self) -> Result<String, RepoError> {
        (**self).branch()
    }

    fn remote_head(&self, branch: &str) -> Result<Option<String>, RepoError> {
        (**self).remote_head(branch)
    }

    fn push(&self, branch: &str) -> Result<(), RepoError> {
        (**self).push(branch)
    }
}

/// The real repository: the `git` binary, run in the run's working tree.
#[derive(Debug, Clone)]
pub struct GitRepo {
    root: PathBuf,
}

impl GitRepo {
    /// A repository whose working tree is `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl Repo for GitRepo {
    fn root(&self) -> &Path {
        &self.root
    }

    fn head(&self) -> Result<String, RepoError> {
        Ok(git(&self.root, &["rev-parse", "HEAD"])?.trim().to_owned())
    }

    fn branch(&self) -> Result<String, RepoError> {
        Ok(git(&self.root, &["rev-parse", "--abbrev-ref", "HEAD"])?
            .trim()
            .to_owned())
    }

    fn remote_head(&self, branch: &str) -> Result<Option<String>, RepoError> {
        let refspec = format!("refs/heads/{branch}");
        let text = git(&self.root, &["ls-remote", "origin", &refspec])?;
        Ok(text
            .split_whitespace()
            .next()
            .filter(|sha| !sha.is_empty())
            .map(str::to_owned))
    }

    fn push(&self, branch: &str) -> Result<(), RepoError> {
        git(&self.root, &["push", "origin", branch]).map(|_| ())
    }
}

/// Run one `git` command in `root` and return its stdout.
fn git(root: &Path, args: &[&str]) -> Result<String, RepoError> {
    let out = Command::new("git").current_dir(root).args(args).output()?;
    if !out.status.success() {
        return Err(RepoError::Command {
            args: args.join(" "),
            message: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}
