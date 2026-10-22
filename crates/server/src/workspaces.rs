//! Workspaces (phase 6 section 9b): a group of projects that belong
//! together, one file each in `<config dir>/workspaces/`, per machine
//! since the paths are. The daemon reads them all, so it knows every
//! project's root and which workspace each is in. `save` writes one
//! back whole.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{ConfigError, ProjectConfig};

/// `workspaces/<name>.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Workspace {
    pub name: String,
    /// The folder whose `workspace/` subfolder holds the shared layer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
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

/// The inverse of [`expand`]: a path under `$HOME` as `~/x`, anything
/// else as given.
pub fn contract(path: &Path) -> PathBuf {
    let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) else {
        return path.to_path_buf();
    };
    match path.strip_prefix(&home) {
        Ok(rest) if rest.as_os_str().is_empty() => PathBuf::from("~"),
        Ok(rest) => Path::new("~").join(rest),
        Err(_) => path.to_path_buf(),
    }
}

/// A workspace that could not be written, or a change it refuses.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorkspaceError {
    #[error("{0:?} has no letters or digits to name a file by")]
    BadName(String),
    #[error("a workspace named {0:?} already exists")]
    Exists(String),
    #[error("no workspace named {0:?}")]
    NoWorkspace(String),
    #[error("{} is already in workspace {workspace:?}", root.display())]
    InOther { root: PathBuf, workspace: String },
    #[error("no project {0:?} in a workspace")]
    NoProject(String),
    #[error("{project:?} is in more than one workspace: {}", workspaces.join(", "))]
    Ambiguous {
        project: String,
        workspaces: Vec<String>,
    },
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("{0}")]
    Io(String),
}

/// A workspace's file name: the name lowercased, every run of other
/// characters than letters and digits one `-`, trimmed, plus `.toml`.
/// `None` when nothing is left.
pub fn file_name(name: &str) -> Option<String> {
    let mut slug = String::new();
    for c in name.chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() {
            slug.push(c);
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_end_matches('-');
    (!slug.is_empty()).then(|| format!("{slug}.toml"))
}

/// A new, empty workspace, written. Refused when its file exists or a
/// workspace has the name, compared without case. `shared` is absolute.
pub fn create(
    config_dir: &Path,
    name: &str,
    shared: Option<&Path>,
) -> Result<Workspace, WorkspaceError> {
    let file = file_name(name).ok_or_else(|| WorkspaceError::BadName(name.to_string()))?;
    let taken = dir(config_dir).join(file).exists()
        || load_all(config_dir)?
            .iter()
            .any(|w| w.name.to_lowercase() == name.to_lowercase());
    if taken {
        return Err(WorkspaceError::Exists(name.to_string()));
    }
    let w = Workspace {
        name: name.to_string(),
        shared: shared.map(Path::to_path_buf),
        projects: Vec::new(),
    };
    save(config_dir, &w)?;
    Ok(w)
}

/// Lists the absolute `root` in `workspace`, found without case. A root
/// already there is left alone; one in another workspace is refused,
/// since a project is in at most one.
pub fn add_project(
    config_dir: &Path,
    workspace: &str,
    root: &Path,
) -> Result<Workspace, WorkspaceError> {
    let all = load_all(config_dir)?;
    let mut target = all
        .iter()
        .find(|w| w.name.to_lowercase() == workspace.to_lowercase())
        .cloned()
        .ok_or_else(|| WorkspaceError::NoWorkspace(workspace.to_string()))?;
    if let Some(other) = all
        .iter()
        .find(|w| w.name != target.name && w.projects.iter().any(|p| p == root))
    {
        return Err(WorkspaceError::InOther {
            root: root.to_path_buf(),
            workspace: other.name.clone(),
        });
    }
    if !target.projects.iter().any(|p| p == root) {
        target.projects.push(root.to_path_buf());
        save(config_dir, &target)?;
    }
    Ok(target)
}

/// Drops one project from its workspace's list, matched by name or by
/// path (after [`expand`]), in `workspace` alone when given. More than
/// one match is refused, naming the workspaces. The project's folder is
/// not touched, and the last project out leaves `projects = []`.
pub fn remove_project(
    config_dir: &Path,
    workspace: Option<&str>,
    project: &str,
) -> Result<Workspace, WorkspaceError> {
    let path = expand(Path::new(project));
    let mut all = load_all(config_dir)?;
    if let Some(name) = workspace {
        all.retain(|w| w.name.to_lowercase() == name.to_lowercase());
        if all.is_empty() {
            return Err(WorkspaceError::NoWorkspace(name.to_string()));
        }
    }
    let matches: Vec<(usize, usize)> = all
        .iter()
        .enumerate()
        .flat_map(|(wi, w)| {
            w.projects
                .iter()
                .enumerate()
                .filter(|(_, p)| **p == path || project_name(p) == project)
                .map(move |(pi, _)| (wi, pi))
        })
        .collect();
    match matches.as_slice() {
        [] => Err(WorkspaceError::NoProject(project.to_string())),
        [(wi, pi)] => {
            let mut w = all.swap_remove(*wi);
            w.projects.remove(*pi);
            save(config_dir, &w)?;
            Ok(w)
        }
        _ => {
            let mut workspaces: Vec<String> = matches
                .iter()
                .map(|(wi, _)| all[*wi].name.clone())
                .collect();
            workspaces.dedup();
            Err(WorkspaceError::Ambiguous {
                project: project.to_string(),
                workspaces,
            })
        }
    }
}

/// Rewrites `workspaces/<file_name>` whole, through a temporary file and
/// a rename so a reader never sees half of it, paths under `$HOME` as
/// `~/`. Comments in a hand-edited file are not kept.
pub fn save(config_dir: &Path, w: &Workspace) -> Result<(), WorkspaceError> {
    let file = file_name(&w.name).ok_or_else(|| WorkspaceError::BadName(w.name.clone()))?;
    let dir = dir(config_dir);
    let io = |e: std::io::Error| WorkspaceError::Io(format!("{}: {e}", dir.display()));
    std::fs::create_dir_all(&dir).map_err(io)?;
    let stored = Workspace {
        name: w.name.clone(),
        shared: w.shared.as_deref().map(contract),
        projects: w.projects.iter().map(|p| contract(p)).collect(),
    };
    let body = toml::to_string(&stored).map_err(|e| WorkspaceError::Io(format!("{file}: {e}")))?;
    let text =
        format!("# A workspace. aigentic rewrites this file whole; comments are not kept.\n{body}");
    let tmp = dir.join(format!(".{file}.tmp"));
    std::fs::write(&tmp, text).map_err(io)?;
    std::fs::rename(&tmp, dir.join(&file)).map_err(io)
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
    fn a_created_workspace_loads_back_and_its_name_is_taken() {
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("notes");
        let w = create(dir.path(), "My Work", Some(&shared)).unwrap();
        assert!(dir.path().join("workspaces/my-work.toml").is_file());
        assert_eq!(load_all(dir.path()).unwrap(), vec![w]);
        assert_eq!(
            create(dir.path(), "My Work", None),
            Err(WorkspaceError::Exists("My Work".into()))
        );
        assert_eq!(
            create(dir.path(), "my work", None),
            Err(WorkspaceError::Exists("my work".into())),
            "a name is unique without case"
        );
        assert_eq!(
            create(dir.path(), "--", None),
            Err(WorkspaceError::BadName("--".into()))
        );
    }

    #[test]
    fn a_file_name_is_the_name_slugged() {
        assert_eq!(file_name("My Work").as_deref(), Some("my-work.toml"));
        assert_eq!(file_name("  a__b..C  ").as_deref(), Some("a-b-c.toml"));
        assert_eq!(file_name("Åsa 2").as_deref(), Some("åsa-2.toml"));
        assert_eq!(file_name(" / "), None);
        assert_eq!(file_name(""), None);
    }

    fn snapshot(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    out.push((p.clone(), Vec::new()));
                    stack.push(p);
                } else {
                    out.push((p.clone(), std::fs::read(&p).unwrap()));
                }
            }
        }
        out.sort();
        out
    }

    #[test]
    fn adding_and_removing_a_project_changes_only_the_list() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("cfg");
        let site = dir.path().join("site");
        std::fs::create_dir_all(site.join(".aigentic/threads")).unwrap();
        std::fs::write(site.join("aigentic.toml"), "[project]\nname = \"web\"\n").unwrap();
        std::fs::write(site.join(".aigentic/threads/t1.jsonl"), "{}\n").unwrap();
        std::fs::write(site.join("README.md"), "hi\n").unwrap();
        let before = snapshot(&site);
        create(&cfg, "work", None).unwrap();
        create(&cfg, "home", None).unwrap();

        let w = add_project(&cfg, "Work", &site).unwrap();
        assert_eq!(w.projects, vec![site.clone()]);
        assert_eq!(add_project(&cfg, "work", &site).unwrap(), w, "a no-op");
        assert_eq!(
            add_project(&cfg, "home", &site),
            Err(WorkspaceError::InOther {
                root: site.clone(),
                workspace: "work".into()
            })
        );
        assert_eq!(
            add_project(&cfg, "nope", &site),
            Err(WorkspaceError::NoWorkspace("nope".into()))
        );
        let loaded = load_all(&cfg).unwrap();
        assert_eq!(workspace_of(&loaded, &site).unwrap().name, "work");

        let w = remove_project(&cfg, None, "web").unwrap();
        assert!(w.projects.is_empty());
        let text = std::fs::read_to_string(cfg.join("workspaces/work.toml")).unwrap();
        assert!(text.contains("projects = []"), "{text}");
        assert!(
            load_all(&cfg)
                .unwrap()
                .iter()
                .all(|w| w.projects.is_empty())
        );

        add_project(&cfg, "home", &site).unwrap();
        remove_project(&cfg, Some("home"), &site.display().to_string()).unwrap();
        assert_eq!(
            remove_project(&cfg, None, "web"),
            Err(WorkspaceError::NoProject("web".into()))
        );
        assert_eq!(snapshot(&site), before, "the project's folder is untouched");
    }

    #[test]
    fn a_name_in_two_workspaces_is_refused_naming_both() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a/app");
        let b = dir.path().join("b/app");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        create(dir.path(), "one", None).unwrap();
        create(dir.path(), "two", None).unwrap();
        add_project(dir.path(), "one", &a).unwrap();
        add_project(dir.path(), "two", &b).unwrap();
        assert_eq!(
            remove_project(dir.path(), None, "app"),
            Err(WorkspaceError::Ambiguous {
                project: "app".into(),
                workspaces: vec!["one".into(), "two".into()]
            })
        );
        let w = remove_project(dir.path(), Some("two"), "app").unwrap();
        assert!(w.projects.is_empty());
        assert_eq!(load_all(dir.path()).unwrap()[0].projects, vec![a]);
    }

    /// The paths sit under `$HOME` but are never created: only the
    /// workspace file under the temporary config dir is written.
    #[test]
    fn a_hand_written_tilde_file_is_rewritten_with_tilde() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("workspaces")).unwrap();
        std::fs::write(
            dir.path().join("workspaces/mine.toml"),
            "# mine\nname = \"mine\"\nshared = \"~/no-such-notes\"\nprojects = [\"~/no-such-a\"]\n",
        )
        .unwrap();
        let home = PathBuf::from(std::env::var("HOME").unwrap());
        let ws = load_all(dir.path()).unwrap();
        assert_eq!(ws[0].projects, vec![home.join("no-such-a")]);
        add_project(dir.path(), "mine", &home.join("no-such-b")).unwrap();
        let text = std::fs::read_to_string(dir.path().join("workspaces/mine.toml")).unwrap();
        assert!(text.contains("\"~/no-such-notes\""), "{text}");
        assert!(text.contains("\"~/no-such-a\""), "{text}");
        assert!(text.contains("\"~/no-such-b\""), "{text}");
        assert!(!text.contains(&home.display().to_string()), "{text}");
        assert!(!text.contains("# mine"), "comments are not kept");
        assert_eq!(
            load_all(dir.path()).unwrap()[0].projects,
            vec![home.join("no-such-a"), home.join("no-such-b")]
        );
    }

    #[test]
    fn contract_undoes_expand() {
        let home = PathBuf::from(std::env::var("HOME").unwrap());
        assert_eq!(contract(&home.join("x/y")), PathBuf::from("~/x/y"));
        assert_eq!(contract(&home), PathBuf::from("~"));
        assert_eq!(contract(Path::new("/abs/x")), PathBuf::from("/abs/x"));
        assert_eq!(expand(&contract(&home.join("x"))), home.join("x"));
    }

    #[test]
    fn tilde_expands_to_home() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(expand(Path::new("~/x")), PathBuf::from(home).join("x"));
        assert_eq!(expand(Path::new("/abs")), PathBuf::from("/abs"));
    }
}
