//! Skills: versioned folders of instructions the harness loads at runtime.
//!
//! A skill is data, not code. `SKILL.md` carries frontmatter and a body
//! ([`Manifest`]); `skills.lock.toml` carries our metadata and content
//! hashes ([`Lockfile`]); [`discover`] resolves project, user and bundled
//! roots with closer-wins; [`check`] produces findings for a human
//! reviewer; [`SkillSet`] is the enabled, hash-verified set a runtime
//! offers to the model. Nothing here touches the network.

mod check;
mod discover;
mod lock;
mod manifest;

use std::collections::BTreeMap;
use std::path::PathBuf;

pub use check::{
    Finding, FindingKind, IGNORE_RULES, PATTERN_VERSION, PERMISSION_WIDENING, SHELL_PATTERNS,
    TOOL_NAMES, check, check_text, tools_referenced,
};
pub use discover::{Roots, discover, discover_roots, walk_root};
pub use lock::{LockEntry, Lockfile, Review, hash_bytes, hash_file, hash_manifest};
pub use manifest::{Invocation, Manifest, Origin};

/// Heading of the prefix line that lists enabled skills.
pub const PREFIX_HEADING: &str = "# Skills";

#[derive(Debug, thiserror::Error)]
pub enum SkillError {
    #[error(
        "skill `{name}`: {file} hash mismatch (lock {expected}, disk {actual}); refusing to load"
    )]
    HashMismatch {
        name: String,
        file: String,
        expected: String,
        actual: String,
    },
    #[error("skill `{0}` has no entry in skills.lock.toml; run `aigentic skills vendor`")]
    NotInLock(String),
    #[error("skill `{0}` is enabled but not found in any skills root")]
    NotFound(String),
    #[error("skill `{skill}` requires tool `{tool}`, which is not available")]
    MissingRequirement { skill: String, tool: String },
    #[error("{path}: {message}")]
    Frontmatter { path: PathBuf, message: String },
    #[error("{path}: {message}")]
    Lockfile { path: PathBuf, message: String },
    #[error("skill `{name}` found twice in one root: {first} and {second}")]
    Duplicate {
        name: String,
        first: PathBuf,
        second: PathBuf,
    },
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// The enabled skills, each verified against its lock entry.
#[derive(Debug, Clone, Default)]
pub struct SkillSet {
    skills: BTreeMap<String, (Manifest, LockEntry)>,
}

impl SkillSet {
    /// Resolve every name in `enabled`, verify its hashes against `lock`,
    /// and check its `requires` against `tools`. Any failure is an error:
    /// a tampered or unlocked skill never loads silently.
    pub fn load(
        enabled: &[String],
        roots: &Roots,
        lock: &Lockfile,
        tools: &[String],
    ) -> Result<Self, SkillError> {
        let found = discover_roots(roots)?;
        let mut skills = BTreeMap::new();
        for name in enabled {
            let manifest = found
                .iter()
                .find(|m| &m.name == name)
                .cloned()
                .ok_or_else(|| SkillError::NotFound(name.clone()))?;
            let entry = lock
                .get(name)
                .cloned()
                .ok_or_else(|| SkillError::NotInLock(name.clone()))?;
            entry.verify(&manifest)?;
            if let Some(tool) = entry.requires.iter().find(|t| !tools.contains(t)) {
                return Err(SkillError::MissingRequirement {
                    skill: name.clone(),
                    tool: tool.clone(),
                });
            }
            skills.insert(name.clone(), (manifest, entry));
        }
        Ok(Self { skills })
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    pub fn len(&self) -> usize {
        self.skills.len()
    }

    /// The prefix block: a fixed heading, one `name: description` line per
    /// skill in name order, and how each kind is invoked. Byte-stable
    /// between turns for the same set.
    pub fn descriptions(&self) -> String {
        let mut out = format!("{PREFIX_HEADING}\n\n");
        let user: Vec<&Manifest> = self.user_invoked();
        let model: Vec<&Manifest> = self.model_invoked();
        if !user.is_empty() {
            out.push_str("Run by the user as slash commands; do not load them yourself:\n");
            for m in user {
                out.push_str(&format!("- {}\n", m.description_line()));
            }
            out.push('\n');
        }
        if !model.is_empty() {
            out.push_str(
                "Available through the `load_skill` tool when the description fits the task:\n",
            );
            for m in model {
                out.push_str(&format!("- {}\n", m.description_line()));
            }
        }
        out.trim_end().to_owned()
    }

    pub fn user_invoked(&self) -> Vec<&Manifest> {
        self.iter()
            .filter(|m| m.invocation == Invocation::User)
            .collect()
    }

    pub fn model_invoked(&self) -> Vec<&Manifest> {
        self.iter()
            .filter(|m| m.invocation == Invocation::Model)
            .collect()
    }

    pub fn get(&self, name: &str) -> Option<&Manifest> {
        self.skills.get(name).map(|(m, _)| m)
    }

    /// The lock entry a `skill_loaded` event records `hash` and `source` from.
    pub fn entry(&self, name: &str) -> Option<&LockEntry> {
        self.skills.get(name).map(|(_, e)| e)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Manifest> {
        self.skills.values().map(|(m, _)| m)
    }
}
