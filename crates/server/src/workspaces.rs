//! Workspaces (phase 6 section 9b): a group of projects that belong
//! together, one file each in `<config dir>/workspaces/`, per machine
//! since the paths are. The daemon reads them all, so it knows every
//! project's root and which workspace each is in.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::config::{ConfigError, ProjectConfig};

/// `workspaces/<name>.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Workspace {
    pub name: String,
    /// The folder whose `workspace/` subfolder holds the shared layer.
    #[serde(default)]
    pub shared: Option<PathBuf>,
    /// Project roots.
    #[serde(default)]
    pub projects: Vec<PathBuf>,
}

pub fn dir(config_dir: &Path) -> PathBuf {
    config_dir.join("workspaces")
}

/// `~/x` to the home directory; anything else as given.
pub fn expand(path: &Path) -> PathBuf {
    match path.strip_prefix("~") {
        Ok(rest) => std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join(rest))
            .unwrap_or_else(|| path.to_path_buf()),
        Err(_) => path.to_path_buf(),
    }
}

/// Every workspace file, paths expanded, by name. A missing directory
/// is none; a file that does not parse is an error naming it.
pub fn load_all(config_dir: &Path) -> Result<Vec<Workspace>, ConfigError> {
    let dir = dir(config_dir);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(ConfigError::msg(format!("{}: {e}", dir.display()))),
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
        .collect();
    files.sort();
    let mut out = Vec::new();
    for f in files {
        let text = std::fs::read_to_string(&f)
            .map_err(|e| ConfigError::msg(format!("{}: {e}", f.display())))?;
        let mut w: Workspace =
            toml::from_str(&text).map_err(|e| ConfigError::msg(format!("{}: {e}", f.display())))?;
        w.shared = w.shared.as_deref().map(expand);
        w.projects = w.projects.iter().map(|p| expand(p)).collect();
        out.push(w);
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// A project's name: its `aigentic.toml`'s, else the folder's.
pub fn project_name(root: &Path) -> String {
    let file = root.join(aigentic_runtime::project::FILE_NAME);
    if file.is_file()
        && let Ok(f) = aigentic_runtime::ProjectFile::load(&file)
        && let Some(p) = f.project
    {
        return p.name;
    }
    root.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.display().to_string())
}

/// The projects the workspaces name, as `server.toml` would list them.
pub fn project_configs(workspaces: &[Workspace]) -> Vec<ProjectConfig> {
    workspaces
        .iter()
        .flat_map(|w| w.projects.iter())
        .map(|root| ProjectConfig {
            name: project_name(root),
            root: root.clone(),
        })
        .collect()
}

/// The workspace a project root is in.
pub fn workspace_of<'a>(workspaces: &'a [Workspace], root: &Path) -> Option<&'a Workspace> {
    workspaces
        .iter()
        .find(|w| w.projects.iter().any(|p| p == root))
}

/// `projects` plus every workspace project whose name is not already
/// there: `server.toml` wins on a clash.
pub fn merge(mut projects: Vec<ProjectConfig>, workspaces: &[Workspace]) -> Vec<ProjectConfig> {
    for p in project_configs(workspaces) {
        if !projects.iter().any(|q| q.name == p.name) {
            projects.push(p);
        }
    }
    projects
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_files_load_expand_and_merge() {
        let dir = tempfile::tempdir().unwrap();
        let site = dir.path().join("site");
        std::fs::create_dir_all(&site).unwrap();
        std::fs::write(site.join("aigentic.toml"), "[project]\nname = \"site\"\n").unwrap();
        let notes = dir.path().join("marketing");
        std::fs::create_dir_all(&notes).unwrap();
        std::fs::create_dir_all(dir.path().join("cfg/workspaces")).unwrap();
        std::fs::write(
            dir.path().join("cfg/workspaces/aigentic.toml"),
            format!(
                "name = \"aigentic\"\nshared = {:?}\nprojects = [{:?}, {:?}]\n",
                notes.display().to_string(),
                site.display().to_string(),
                notes.display().to_string()
            ),
        )
        .unwrap();
        let ws = load_all(&dir.path().join("cfg")).unwrap();
        assert_eq!(ws.len(), 1);
        assert_eq!(ws[0].name, "aigentic");
        let merged = merge(
            vec![ProjectConfig {
                name: "site".into(),
                root: PathBuf::from("/elsewhere"),
            }],
            &ws,
        );
        let names: Vec<&str> = merged.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["site", "marketing"],
            "server.toml wins; a folder name for no file"
        );
        assert_eq!(merged[0].root, PathBuf::from("/elsewhere"));
        assert_eq!(
            workspace_of(&ws, &site).map(|w| w.name.as_str()),
            Some("aigentic")
        );
        assert!(load_all(&dir.path().join("none")).unwrap().is_empty());
        std::fs::write(dir.path().join("cfg/workspaces/bad.toml"), "nope = 1\n").unwrap();
        assert!(load_all(&dir.path().join("cfg")).is_err());
    }

    #[test]
    fn tilde_expands_to_home() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(expand(Path::new("~/x")), PathBuf::from(home).join("x"));
        assert_eq!(expand(Path::new("/abs")), PathBuf::from("/abs"));
    }
}
