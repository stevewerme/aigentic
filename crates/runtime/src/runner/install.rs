//! The install seam: putting the just-built binary on `PATH`.
//!
//! [`CargoInstaller`] runs `cargo install --path crates/tui --force` in the
//! run's repository, then asks the installed binary its version. The point
//! is the version it answers with: a pushed commit is not real until the
//! binary that will run the next thread carries that commit, so the runner
//! compares the answer with the commit it just pushed.

use std::process::Command;

use super::RunnerError;
use super::git::Repo;

/// Every way installing can fail.
#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    /// `cargo install` or the installed binary ran and said no.
    #[error("{args} failed: {message}")]
    Command {
        /// The command, space-joined, for the message.
        args: String,
        /// What it wrote on stderr.
        message: String,
    },
    /// Neither command could be run at all.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// No home directory, so no `CARGO_HOME` to fall back to.
    #[error("no CARGO_HOME and no HOME to build one from")]
    NoHome,
}

/// Build and install the binary a run's next thread will use.
pub trait Installer: Send + Sync {
    /// Install the binary from `repo` and return what the installed binary
    /// prints for `--version`.
    fn install(&self, repo: &dyn Repo) -> Result<String, RunnerError>;
}

impl<T: Installer + ?Sized> Installer for Box<T> {
    fn install(&self, repo: &dyn Repo) -> Result<String, RunnerError> {
        (**self).install(repo)
    }
}

/// The real installer: `cargo install --path crates/tui --force`, then the
/// installed binary's `--version`.
#[derive(Debug, Clone, Copy, Default)]
pub struct CargoInstaller;

impl CargoInstaller {
    /// An installer that runs `cargo` and the binary it installs from
    /// `PATH`.
    pub fn new() -> Self {
        Self
    }
}

impl Installer for CargoInstaller {
    fn install(&self, repo: &dyn Repo) -> Result<String, RunnerError> {
        let root = repo.root().to_string_lossy().into_owned();
        run(
            "cargo",
            &["install", "--path", "crates/tui", "--force"],
            &root,
        )?;
        let bin = cargo_home()?.join("bin").join("aigentic");
        Ok(run(&bin.to_string_lossy(), &["--version"], &root)?)
    }
}

impl From<InstallError> for RunnerError {
    fn from(err: InstallError) -> Self {
        RunnerError::Install(err.to_string())
    }
}

/// The cargo install root: `CARGO_HOME`, or `~/.cargo`.
fn cargo_home() -> Result<std::path::PathBuf, InstallError> {
    if let Some(home) = std::env::var_os("CARGO_HOME") {
        return Ok(std::path::PathBuf::from(home));
    }
    let home = std::env::var_os("HOME").ok_or(InstallError::NoHome)?;
    Ok(std::path::PathBuf::from(home).join(".cargo"))
}

/// Run one command in `cwd` and return its stdout, trimmed.
fn run(program: &str, args: &[&str], cwd: &str) -> Result<String, InstallError> {
    let out = Command::new(program).args(args).current_dir(cwd).output()?;
    if !out.status.success() {
        return Err(InstallError::Command {
            args: format!("{program} {}", args.join(" ")),
            message: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// An installer in memory: it answers with one configured version string,
/// or with the head it was asked about.
///
/// It lives in the library, and not with the tests, because the tests that
/// need it are a separate crate: an integration test cannot see a
/// `#[cfg(test)]` item.
#[derive(Debug, Clone)]
pub struct FakeInstaller {
    version: Version,
    calls: std::sync::Arc<std::sync::Mutex<usize>>,
}

/// What `install` answers.
#[derive(Debug, Clone)]
enum Version {
    /// The string it was built with.
    Fixed(String),
    /// `aigentic <package version> (<head>)`, read from the repo it is
    /// asked about: what a real install of the commit just pushed prints,
    /// so a test need not know the commit's hash in advance.
    Head,
}

impl FakeInstaller {
    /// A fake that answers `version` every time.
    pub fn new(version: impl Into<String>) -> Self {
        Self {
            version: Version::Fixed(version.into()),
            calls: Default::default(),
        }
    }

    /// A fake whose answer names the commit it is asked to install: an
    /// install that matches HEAD, as a real one does.
    pub fn echoing_head() -> Self {
        Self {
            version: Version::Head,
            calls: Default::default(),
        }
    }

    /// How many times `install` has been called.
    pub fn calls(&self) -> usize {
        *self.calls.lock().expect("installer calls mutex")
    }
}

impl Installer for FakeInstaller {
    fn install(&self, repo: &dyn Repo) -> Result<String, RunnerError> {
        *self.calls.lock().expect("installer calls mutex") += 1;
        Ok(match &self.version {
            Version::Fixed(version) => version.clone(),
            Version::Head => {
                let head = repo.head()?;
                let short: String = head.chars().take(12).collect();
                format!("aigentic {} ({short})", env!("CARGO_PKG_VERSION"))
            }
        })
    }
}
