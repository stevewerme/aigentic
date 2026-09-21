//! `skills.lock.toml`: one entry per skill with its source, commit,
//! content hashes and review state. A vendored file is never edited; our
//! metadata lives here. The loader refuses a skill whose hash does not
//! match its entry.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{Invocation, Manifest, SkillError};

/// Whether a human has read the static check's findings and accepted them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Review {
    Accepted { by: String, on: String },
    Pending,
}

impl Review {
    pub fn is_pending(&self) -> bool {
        matches!(self, Review::Pending)
    }
}

/// TOML: `review = "pending"` or `review = { by = "steve", on = "2026-09-21" }`.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum ReviewWire {
    Accepted { by: String, on: String },
    Pending(PendingTag),
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum PendingTag {
    Pending,
}

impl Serialize for Review {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let wire = match self {
            Review::Accepted { by, on } => ReviewWire::Accepted {
                by: by.clone(),
                on: on.clone(),
            },
            Review::Pending => ReviewWire::Pending(PendingTag::Pending),
        };
        wire.serialize(s)
    }
}

impl<'de> Deserialize<'de> for Review {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(match ReviewWire::deserialize(d)? {
            ReviewWire::Accepted { by, on } => Review::Accepted { by, on },
            ReviewWire::Pending(_) => Review::Pending,
        })
    }
}

/// One skill's lock entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockEntry {
    pub name: String,
    /// Folder relative to the lockfile, e.g. `skills/pocock/engineering/tdd`.
    pub path: String,
    /// Upstream repository, e.g. `https://github.com/mattpocock/skills`;
    /// `local` for a skill written here.
    pub source: String,
    pub commit: String,
    /// SHA-256 of `SKILL.md`, hex. Verifiable against upstream with
    /// `sha256sum`.
    pub sha256: String,
    /// SHA-256 of every other file in the folder, keyed by relative path.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub files: BTreeMap<String, String>,
    pub invocation: Invocation,
    /// Tools the skill needs; filled from the static check's tool
    /// references and confirmed by the reviewer. A missing one fails at load.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<String>,
    pub review: Review,
}

impl LockEntry {
    /// A `Pending` entry for a manifest as it is on disk.
    pub fn from_manifest(
        manifest: &Manifest,
        path: impl Into<String>,
        source: impl Into<String>,
        commit: impl Into<String>,
    ) -> Result<Self, SkillError> {
        let (sha256, files) = hash_manifest(manifest)?;
        Ok(Self {
            name: manifest.name.clone(),
            path: path.into(),
            source: source.into(),
            commit: commit.into(),
            sha256,
            files,
            invocation: manifest.invocation,
            requires: Vec::new(),
            review: Review::Pending,
        })
    }

    /// Refuse unless every file on disk hashes to what the entry records.
    pub fn verify(&self, manifest: &Manifest) -> Result<(), SkillError> {
        let (sha256, files) = hash_manifest(manifest)?;
        if sha256 != self.sha256 {
            return Err(SkillError::HashMismatch {
                name: self.name.clone(),
                file: "SKILL.md".into(),
                expected: self.sha256.clone(),
                actual: sha256,
            });
        }
        for (file, actual) in &files {
            match self.files.get(file) {
                Some(expected) if expected == actual => {}
                Some(expected) => {
                    return Err(SkillError::HashMismatch {
                        name: self.name.clone(),
                        file: file.clone(),
                        expected: expected.clone(),
                        actual: actual.clone(),
                    });
                }
                None => {
                    return Err(SkillError::HashMismatch {
                        name: self.name.clone(),
                        file: file.clone(),
                        expected: "(not in lock)".into(),
                        actual: actual.clone(),
                    });
                }
            }
        }
        if let Some(missing) = self.files.keys().find(|f| !files.contains_key(*f)) {
            return Err(SkillError::HashMismatch {
                name: self.name.clone(),
                file: missing.clone(),
                expected: self.files[missing].clone(),
                actual: "(missing on disk)".into(),
            });
        }
        Ok(())
    }

    /// `source@commit`, as recorded on `skill_loaded` events.
    pub fn source_ref(&self) -> String {
        format!("{}@{}", self.source, self.commit)
    }
}

/// The whole lockfile.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lockfile {
    #[serde(default, rename = "skill")]
    pub skills: Vec<LockEntry>,
}

impl Lockfile {
    pub fn load(path: &Path) -> Result<Self, SkillError> {
        let text = std::fs::read_to_string(path).map_err(|source| SkillError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(&text).map_err(|message| SkillError::Lockfile {
            path: path.to_path_buf(),
            message,
        })
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let lock: Self = toml::from_str(text).map_err(|e| e.to_string())?;
        let mut seen = std::collections::HashSet::new();
        for entry in &lock.skills {
            if !seen.insert(&entry.name) {
                return Err(format!("duplicate entry for skill `{}`", entry.name));
            }
        }
        Ok(lock)
    }

    pub fn to_toml(&self) -> String {
        let mut out = String::from(
            "# Skills lockfile. Vendored files are byte-identical to upstream; this file\n\
             # holds our metadata: source, commit, content hashes, invocation and review.\n\
             # Managed by `aigentic skills vendor | update`; a human sets `review`.\n",
        );
        let mut sorted = self.clone();
        sorted.skills.sort_by(|a, b| a.name.cmp(&b.name));
        out.push_str(&toml::to_string_pretty(&sorted).expect("lockfile is serialisable"));
        out
    }

    pub fn save(&self, path: &Path) -> Result<(), SkillError> {
        std::fs::write(path, self.to_toml()).map_err(|source| SkillError::Io {
            path: path.to_path_buf(),
            source,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// Number of entries, one per skill; companion files do not count.
    pub fn len(&self) -> usize {
        self.skills.len()
    }

    pub fn get(&self, name: &str) -> Option<&LockEntry> {
        self.skills.iter().find(|e| e.name == name)
    }

    pub fn get_mut(&mut self, name: &str) -> Option<&mut LockEntry> {
        self.skills.iter_mut().find(|e| e.name == name)
    }

    /// Add or replace by name.
    pub fn upsert(&mut self, entry: LockEntry) {
        match self.get_mut(&entry.name) {
            Some(existing) => *existing = entry,
            None => self.skills.push(entry),
        }
    }
}

/// Hex SHA-256 of a file.
pub fn hash_file(path: &Path) -> Result<String, SkillError> {
    let bytes = std::fs::read(path).map_err(|source| SkillError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(hash_bytes(&bytes))
}

pub fn hash_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// `(sha256 of SKILL.md, sha256 of every other file by relative path)`.
pub fn hash_manifest(
    manifest: &Manifest,
) -> Result<(String, BTreeMap<String, String>), SkillError> {
    let sha256 = hash_file(&manifest.path.join("SKILL.md"))?;
    let mut files = BTreeMap::new();
    for rel in &manifest.files {
        let key = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        files.insert(key, hash_file(&manifest.path.join(rel))?);
    }
    Ok((sha256, files))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_wire_is_a_string_or_a_table() {
        let pending = Lockfile {
            skills: vec![LockEntry {
                name: "a".into(),
                path: "skills/a".into(),
                source: "local".into(),
                commit: "".into(),
                sha256: "00".into(),
                files: BTreeMap::new(),
                invocation: Invocation::Model,
                requires: vec![],
                review: Review::Pending,
            }],
        };
        let text = pending.to_toml();
        assert!(text.contains("review = \"pending\""), "{text}");
        assert_eq!(Lockfile::parse(&text).unwrap(), pending);

        let mut accepted = pending.clone();
        accepted.skills[0].review = Review::Accepted {
            by: "steve".into(),
            on: "2026-09-21".into(),
        };
        accepted.skills[0].requires = vec!["bash".into()];
        let text = accepted.to_toml();
        assert!(
            text.contains("[skill.review]") || text.contains("review = {"),
            "{text}"
        );
        assert_eq!(Lockfile::parse(&text).unwrap(), accepted);
    }

    #[test]
    fn unknown_fields_and_duplicates_are_rejected() {
        let err = Lockfile::parse("[[skill]]\nname = \"a\"\nbogus = 1\n").unwrap_err();
        assert!(err.contains("bogus"), "{err}");
        let two = "[[skill]]\nname = \"a\"\npath = \"p\"\nsource = \"s\"\ncommit = \"c\"\nsha256 = \"0\"\ninvocation = \"model\"\nreview = \"pending\"\n";
        let err = Lockfile::parse(&format!("{two}{two}")).unwrap_err();
        assert!(err.contains("duplicate"), "{err}");
    }

    #[test]
    fn sha256_matches_the_reference_vector() {
        assert_eq!(
            hash_bytes(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
