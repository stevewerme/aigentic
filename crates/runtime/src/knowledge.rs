//! A project's knowledge folder: every `*.md` under `.aigentic/knowledge/`,
//! inlined in the prefix when small, indexed with a `search_knowledge`
//! tool when over a fraction of the model's window. Decided at startup
//! and when the folder changes, never mid-turn.

use std::path::{Path, PathBuf};

use aigentic_tools::{Section, split_sections};
use walkdir::WalkDir;

use crate::project::ProjectError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnowledgeFile {
    /// Relative to the folder, `/` separators.
    pub path: String,
    pub text: String,
    pub sections: Vec<Section>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KnowledgeMode {
    Inline,
    Index,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Knowledge {
    pub files: Vec<KnowledgeFile>,
    /// The provider's count over the whole folder.
    pub tokens: u64,
    /// Paths, sizes and mtimes; a change means reload.
    fingerprint: Vec<(String, u64, Option<std::time::SystemTime>)>,
}

impl Knowledge {
    /// Read the folder; a missing one is empty. `count` is the provider's
    /// token count for a text.
    pub fn load(dir: &Path, count: &dyn Fn(&str) -> u64) -> Result<Self, ProjectError> {
        let mut files = Vec::new();
        let mut fingerprint = Vec::new();
        if dir.is_dir() {
            for entry in WalkDir::new(dir).sort_by_file_name() {
                let entry = entry.map_err(|e| ProjectError::Io {
                    path: dir.to_path_buf(),
                    source: e.into(),
                })?;
                if !entry.file_type().is_file()
                    || entry.path().extension().and_then(|e| e.to_str()) != Some("md")
                {
                    continue;
                }
                let rel = entry
                    .path()
                    .strip_prefix(dir)
                    .expect("walked under dir")
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/");
                let text =
                    std::fs::read_to_string(entry.path()).map_err(|source| ProjectError::Io {
                        path: entry.path().to_path_buf(),
                        source,
                    })?;
                let meta = entry.metadata().ok();
                fingerprint.push((
                    rel.clone(),
                    meta.as_ref().map_or(0, |m| m.len()),
                    meta.and_then(|m| m.modified().ok()),
                ));
                let sections = split_sections(&rel, &text);
                files.push(KnowledgeFile {
                    path: rel,
                    text,
                    sections,
                });
            }
        }
        let tokens = files.iter().map(|f| count(&f.text)).sum();
        Ok(Self {
            files,
            tokens,
            fingerprint,
        })
    }

    /// Whether the folder on disk differs from what was loaded.
    pub fn changed(&self, dir: &Path) -> bool {
        let mut now = Vec::new();
        if dir.is_dir() {
            for entry in WalkDir::new(dir).sort_by_file_name().into_iter().flatten() {
                if !entry.file_type().is_file()
                    || entry.path().extension().and_then(|e| e.to_str()) != Some("md")
                {
                    continue;
                }
                let rel = entry
                    .path()
                    .strip_prefix(dir)
                    .expect("walked under dir")
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/");
                let meta = entry.metadata().ok();
                now.push((
                    rel,
                    meta.as_ref().map_or(0, |m| m.len()),
                    meta.and_then(|m| m.modified().ok()),
                ));
            }
        }
        now != self.fingerprint
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Inline under `threshold` of `window`, else index.
    pub fn mode(&self, window: u64, threshold: f32) -> KnowledgeMode {
        let line = (window as f64 * f64::from(threshold)).round() as u64;
        if self.tokens <= line {
            KnowledgeMode::Inline
        } else {
            KnowledgeMode::Index
        }
    }

    /// The prefix block: the files themselves, or one line per file with
    /// a pointer to `search_knowledge`. `None` for an empty folder.
    pub fn prefix(&self, mode: KnowledgeMode) -> Option<String> {
        if self.files.is_empty() {
            return None;
        }
        let mut out = String::new();
        match mode {
            KnowledgeMode::Inline => {
                out.push_str("# Project knowledge\n");
                for f in &self.files {
                    out.push_str(&format!("\n## {}\n\n{}\n", f.path, f.text.trim_end()));
                }
            }
            KnowledgeMode::Index => {
                out.push_str(
                    "# Project knowledge (index)\n\nThe folder is too large to include; use the `search_knowledge` tool to read the sections you need.\n\n",
                );
                for f in &self.files {
                    let first = f
                        .sections
                        .iter()
                        .find(|s| s.heading != f.path)
                        .map_or("(no heading)", |s| s.heading.as_str());
                    out.push_str(&format!(
                        "- {}: {first} ({} sections)\n",
                        f.path,
                        f.sections.len()
                    ));
                }
            }
        }
        Some(out.trim_end().to_owned())
    }

    /// Every section in folder order, for the tool's snapshot.
    pub fn sections(&self) -> Vec<Section> {
        self.files.iter().flat_map(|f| f.sections.clone()).collect()
    }

    pub fn paths(&self) -> Vec<PathBuf> {
        self.files.iter().map(|f| PathBuf::from(&f.path)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("ops")).unwrap();
        std::fs::write(dir.path().join("intro.md"), "# Intro\n\nHello.\n").unwrap();
        std::fs::write(
            dir.path().join("ops/deploy.md"),
            "# Deploys\n\nFridays.\n\n## Rollback\n\nvercel rollback.\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("notes.txt"), "ignored").unwrap();
        dir
    }

    #[test]
    fn loads_sorted_md_files_recursively_and_counts_tokens() {
        let dir = folder();
        let k = Knowledge::load(dir.path(), &|t| t.len() as u64).unwrap();
        assert_eq!(
            k.paths(),
            vec![PathBuf::from("intro.md"), PathBuf::from("ops/deploy.md")]
        );
        assert_eq!(k.tokens, 16 + 51);
        assert_eq!(k.sections().len(), 3);
        assert!(!k.changed(dir.path()));
        std::fs::write(dir.path().join("new.md"), "# New\n").unwrap();
        assert!(k.changed(dir.path()));
        let empty = Knowledge::load(&dir.path().join("nope"), &|_| 1).unwrap();
        assert!(empty.is_empty());
        assert_eq!(empty.prefix(KnowledgeMode::Inline), None);
    }

    #[test]
    fn mode_and_both_prefixes() {
        let dir = folder();
        let k = Knowledge::load(dir.path(), &|t| t.len() as u64).unwrap();
        assert_eq!(k.mode(1000, 0.4), KnowledgeMode::Inline, "67 <= 400");
        assert_eq!(k.mode(100, 0.4), KnowledgeMode::Index, "67 > 40");
        assert_eq!(
            k.prefix(KnowledgeMode::Inline).unwrap(),
            "# Project knowledge\n\n## intro.md\n\n# Intro\n\nHello.\n\n## ops/deploy.md\n\n# Deploys\n\nFridays.\n\n## Rollback\n\nvercel rollback."
        );
        let index = k.prefix(KnowledgeMode::Index).unwrap();
        assert!(index.starts_with("# Project knowledge (index)"));
        assert!(index.contains("`search_knowledge`"));
        assert!(
            index
                .ends_with("- intro.md: Intro (1 sections)\n- ops/deploy.md: Deploys (2 sections)"),
            "{index}"
        );
    }
}
