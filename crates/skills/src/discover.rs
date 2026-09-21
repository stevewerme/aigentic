//! Resolution: project `./skills/<name>/`, then user
//! `~/.config/aigentic/skills/<name>/`, then bundled. Same name closer wins.
//! Each root is walked recursively, so the bundled tree keeps upstream's
//! bucket structure (`pocock/engineering/tdd/`).

use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use crate::{Manifest, Origin, SkillError};

/// The three roots, in resolution order. A missing directory is empty.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Roots {
    pub project: Option<PathBuf>,
    pub user: Option<PathBuf>,
    pub bundled: Option<PathBuf>,
}

impl Roots {
    pub fn new(project: &Path, user: &Path, bundled: &Path) -> Self {
        Self {
            project: Some(project.to_path_buf()),
            user: Some(user.to_path_buf()),
            bundled: Some(bundled.to_path_buf()),
        }
    }
}

/// Every skill reachable from the roots, closer origin winning by name,
/// sorted by name.
pub fn discover(project: &Path, user: &Path, bundled: &Path) -> Result<Vec<Manifest>, SkillError> {
    discover_roots(&Roots::new(project, user, bundled))
}

pub fn discover_roots(roots: &Roots) -> Result<Vec<Manifest>, SkillError> {
    let mut found: Vec<Manifest> = Vec::new();
    for (root, origin) in [
        (&roots.project, Origin::Project),
        (&roots.user, Origin::User),
        (&roots.bundled, Origin::Bundled),
    ] {
        let Some(root) = root else { continue };
        for manifest in walk_root(root, origin)? {
            if !found.iter().any(|m| m.name == manifest.name) {
                found.push(manifest);
            }
        }
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(found)
}

/// Every folder under `root` holding a `SKILL.md`. Two folders with the
/// same skill name in one root is an error: nothing decides between them.
pub fn walk_root(root: &Path, origin: Origin) -> Result<Vec<Manifest>, SkillError> {
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut out: Vec<Manifest> = Vec::new();
    for entry in WalkDir::new(root).sort_by_file_name() {
        let entry = entry.map_err(|e| SkillError::Io {
            path: root.to_path_buf(),
            source: e.into(),
        })?;
        if entry.file_type().is_file() && entry.file_name() == "SKILL.md" {
            let dir = entry.path().parent().expect("a file has a parent");
            let manifest = Manifest::parse(dir, origin)?;
            if let Some(other) = out.iter().find(|m| m.name == manifest.name) {
                return Err(SkillError::Duplicate {
                    name: manifest.name,
                    first: other.path.clone(),
                    second: manifest.path,
                });
            }
            out.push(manifest);
        }
    }
    Ok(out)
}
