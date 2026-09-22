//! Where skills and their locks live for one run, and the enabled set a
//! thread loads. Moved from the terminal binary in phase 5 step 7, since
//! the daemon loads skills per thread.

use std::path::{Path, PathBuf};

use aigentic_runtime::aigentic_skills::{Lockfile, Origin, Roots, SkillError};

pub const LOCK_FILE: &str = "skills.lock.toml";

/// Where skills and their locks live for one run.
#[derive(Debug, Clone)]
pub struct SkillPaths {
    /// The working directory: `./skills` and `./skills.lock.toml`.
    pub project: PathBuf,
    /// `~/.config/aigentic`: `skills/` and `skills.lock.toml` there.
    pub user: PathBuf,
    /// The repository the binary was built from, or `bundled_dir` in the
    /// config: `skills/` and `skills.lock.toml` there.
    pub bundled: PathBuf,
}

impl SkillPaths {
    pub fn new(cwd: &Path, config_dir: &Path, bundled: Option<&Path>) -> Self {
        Self {
            project: cwd.to_path_buf(),
            user: config_dir.to_path_buf(),
            bundled: bundled.map_or_else(default_bundled_dir, Path::to_path_buf),
        }
    }

    pub fn roots(&self) -> Roots {
        Roots {
            project: Some(self.project.join("skills")),
            user: Some(self.user.join("skills")),
            bundled: Some(self.bundled.join("skills")),
        }
    }

    /// The three lockfiles merged, closer winning by name. A missing file
    /// is empty.
    pub fn lock(&self) -> Result<Lockfile, SkillError> {
        let mut merged = Lockfile::default();
        for dir in [&self.bundled, &self.user, &self.project] {
            let path = dir.join(LOCK_FILE);
            if path.exists() {
                let lock = Lockfile::load(&path)?;
                for entry in lock.skills {
                    merged.upsert(entry);
                }
            }
        }
        Ok(merged)
    }

    pub fn dir_for(&self, origin: Origin) -> &Path {
        match origin {
            Origin::Project => &self.project,
            Origin::User => &self.user,
            Origin::Bundled => &self.bundled,
        }
    }
}

/// The repository this binary was built from.
pub fn default_bundled_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap_or_else(|_| Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
}

/// Load the enabled set for the REPL. A hash mismatch or an unlocked
/// skill refuses, naming the skill.
pub fn load_enabled(
    enabled: &[String],
    paths: &SkillPaths,
    tools: &[String],
) -> Result<aigentic_runtime::aigentic_skills::SkillSet, SkillError> {
    let lock = paths.lock()?;
    aigentic_runtime::aigentic_skills::SkillSet::load(enabled, &paths.roots(), &lock, tools)
}
