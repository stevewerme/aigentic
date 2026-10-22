//! `aigentic workspace new | add | remove | list`: the workspace files in
//! `<config dir>/workspaces/`, edited through the daemon crate's
//! `workspaces`. Flags only, never a prompt. See `docs/PLAN-phase6.md`
//! section 9b.

use std::io::Write;
use std::path::{Component, Path, PathBuf};

use aigentic_server::workspaces::{self, Workspace};

#[derive(Debug, clap::Subcommand)]
pub enum WorkspaceCommand {
    /// Create an empty workspace.
    New {
        name: String,
        /// The folder whose `workspace/` subfolder holds the shared layer.
        #[arg(long, value_name = "DIR")]
        shared: Option<PathBuf>,
    },
    /// List an existing folder as a project in a workspace.
    Add {
        path: PathBuf,
        /// Which workspace; may be omitted when there is exactly one.
        #[arg(long, value_name = "NAME")]
        workspace: Option<String>,
    },
    /// Drop a project, by name or path, from its workspace's list. Nothing
    /// on disk is touched.
    Remove {
        project: String,
        /// Look only in this workspace.
        #[arg(long, value_name = "NAME")]
        workspace: Option<String>,
    },
    /// Every workspace with its shared folder and projects.
    List,
}

/// Runs one subcommand against `config_dir`, relative paths taken from
/// `cwd`. Results go to `out`, refusals to `err`; returns the exit code,
/// 1 on a refusal or on a workspace file that does not parse.
pub fn run(
    command: WorkspaceCommand,
    config_dir: &Path,
    cwd: &Path,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> std::io::Result<i32> {
    let result = match command {
        WorkspaceCommand::New { name, shared } => {
            new(config_dir, &name, shared.map(|s| absolute(cwd, &s)))
        }
        WorkspaceCommand::Add { path, workspace } => {
            add(config_dir, &absolute(cwd, &path), workspace.as_deref())
        }
        WorkspaceCommand::Remove { project, workspace } => {
            remove(config_dir, &project, workspace.as_deref())
        }
        WorkspaceCommand::List => {
            let (text, code) = list(config_dir);
            write!(out, "{text}")?;
            return Ok(code);
        }
    };
    match result {
        Ok(text) => {
            writeln!(out, "{text}")?;
            Ok(0)
        }
        Err(e) => {
            writeln!(err, "{e}")?;
            Ok(1)
        }
    }
}

fn new(config_dir: &Path, name: &str, shared: Option<PathBuf>) -> Result<String, String> {
    let w = workspaces::create(config_dir, name, shared.as_deref()).map_err(|e| e.to_string())?;
    let file = workspaces::dir(config_dir).join(workspaces::file_name(&w.name).unwrap_or_default());
    let mut text = format!("created workspace {} in {}", w.name, file.display());
    if let Some(shared) = &w.shared {
        text.push_str(&format!("\nshared  {}", shared.display()));
    }
    Ok(text)
}

fn add(config_dir: &Path, root: &Path, workspace: Option<&str>) -> Result<String, String> {
    if !root.is_dir() {
        return Err(format!("{} is not a folder", root.display()));
    }
    let all = workspaces::load_all(config_dir).map_err(|e| e.to_string())?;
    let name = match (workspace, all.as_slice()) {
        (Some(name), _) => name.to_string(),
        (None, [only]) => only.name.clone(),
        (None, []) => {
            return Err(
                "no workspace yet: create one with `aigentic workspace new <name>`".to_string(),
            );
        }
        (None, many) => {
            let names: Vec<&str> = many.iter().map(|w| w.name.as_str()).collect();
            return Err(format!(
                "more than one workspace; pass --workspace with one of: {}",
                names.join(", ")
            ));
        }
    };
    let already = workspaces::workspace_of(&all, root)
        .is_some_and(|w| w.name.to_lowercase() == name.to_lowercase());
    let w = workspaces::add_project(config_dir, &name, root).map_err(|e| e.to_string())?;
    let project = workspaces::project_name(root);
    Ok(if already {
        format!("{project} ({}) is already in {}", root.display(), w.name)
    } else {
        format!("added {project} ({}) to {}", root.display(), w.name)
    })
}

fn remove(config_dir: &Path, project: &str, workspace: Option<&str>) -> Result<String, String> {
    let w =
        workspaces::remove_project(config_dir, workspace, project).map_err(|e| e.to_string())?;
    Ok(format!(
        "{project} ({}): removed from the list; nothing on disk was touched",
        w.name
    ))
}

/// The listing and its exit code: 1 when a file does not parse, which is
/// printed as `<file>: <error>` in its place.
fn list(config_dir: &Path) -> (String, i32) {
    let each = match workspaces::load_each(config_dir) {
        Ok(each) => each,
        Err(e) => return (format!("{e}\n"), 1),
    };
    if each.is_empty() {
        return (
            format!(
                "no workspaces in {}\n",
                workspaces::dir(config_dir).display()
            ),
            0,
        );
    }
    let mut code = 0;
    let mut loaded: Vec<Workspace> = Vec::new();
    let mut broken = String::new();
    for (_, w) in each {
        match w {
            Ok(w) => loaded.push(w),
            // The error already names the file.
            Err(e) => {
                broken.push_str(&format!("{e}\n"));
                code = 1;
            }
        }
    }
    loaded.sort_by(|a, b| a.name.cmp(&b.name));
    let mut text = String::new();
    for w in &loaded {
        text.push_str(&format!("{}\n", w.name));
        let shared = w
            .shared
            .as_deref()
            .map_or("(none)".to_string(), with_missing);
        text.push_str(&format!("  shared  {shared}\n"));
        if w.projects.is_empty() {
            text.push_str("  (no projects)\n");
        }
        for root in &w.projects {
            text.push_str(&format!(
                "  {}  {}\n",
                workspaces::project_name(root),
                with_missing(root)
            ));
        }
    }
    text.push_str(&broken);
    (text, code)
}

fn with_missing(path: &Path) -> String {
    if path.exists() {
        path.display().to_string()
    } else {
        format!("{} (missing)", path.display())
    }
}

/// `path` against `cwd`, with `.` and `..` resolved by name rather than by
/// the file system, so a symlinked folder is stored as it was typed.
fn absolute(cwd: &Path, path: &Path) -> PathBuf {
    let joined = cwd.join(workspaces::expand(path));
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs one subcommand, returning the exit code, stdout and stderr.
    fn exec(command: WorkspaceCommand, cfg: &Path, cwd: &Path) -> (i32, String, String) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run(command, cfg, cwd, &mut out, &mut err).unwrap();
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
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
    fn new_add_list_and_remove_work_against_a_config_dir() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("cfg");
        let home = dir.path().join("projects");
        let site = home.join("site");
        std::fs::create_dir_all(&site).unwrap();
        std::fs::write(site.join("aigentic.toml"), "[project]\nname = \"web\"\n").unwrap();
        std::fs::create_dir_all(home.join("notes")).unwrap();

        let (code, out, _) = exec(
            WorkspaceCommand::New {
                name: "Brand".into(),
                shared: Some("notes".into()),
            },
            &cfg,
            &home,
        );
        assert_eq!(code, 0);
        assert!(out.starts_with("created workspace Brand in "), "{out}");
        assert!(cfg.join("workspaces/brand.toml").is_file());

        let (code, out, err) = exec(
            WorkspaceCommand::Add {
                path: "./site/../site".into(),
                workspace: None,
            },
            &cfg,
            &home,
        );
        assert_eq!((code, err.as_str()), (0, ""));
        assert_eq!(out, format!("added web ({}) to Brand\n", site.display()));
        let (_, out, _) = exec(
            WorkspaceCommand::Add {
                path: site.clone(),
                workspace: Some("brand".into()),
            },
            &cfg,
            &home,
        );
        assert!(out.contains("is already in Brand"), "{out}");
        let (code, _, err) = exec(
            WorkspaceCommand::Add {
                path: "nope".into(),
                workspace: None,
            },
            &cfg,
            &home,
        );
        assert_eq!(code, 1);
        assert!(err.contains("is not a folder"), "{err}");
        let gone = home.join("gone");
        std::fs::create_dir_all(&gone).unwrap();
        exec(
            WorkspaceCommand::Add {
                path: gone.clone(),
                workspace: None,
            },
            &cfg,
            &home,
        );
        std::fs::remove_dir(&gone).unwrap();

        let (code, out, _) = exec(WorkspaceCommand::List, &cfg, &home);
        assert_eq!(code, 0);
        assert_eq!(
            out,
            format!(
                "Brand\n  shared  {}\n  web  {}\n  gone  {} (missing)\n",
                home.join("notes").display(),
                site.display(),
                gone.display()
            )
        );

        let (code, out, _) = exec(
            WorkspaceCommand::Remove {
                project: "web".into(),
                workspace: None,
            },
            &cfg,
            &home,
        );
        assert_eq!(code, 0);
        assert_eq!(
            out,
            "web (Brand): removed from the list; nothing on disk was touched\n"
        );
        let (code, _, err) = exec(
            WorkspaceCommand::Remove {
                project: "web".into(),
                workspace: None,
            },
            &cfg,
            &home,
        );
        assert_eq!(code, 1);
        assert_eq!(err, "no project \"web\" in a workspace\n");
    }

    #[test]
    fn remove_leaves_the_folder_its_files_and_the_thread_logs_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("cfg");
        let site = dir.path().join("site");
        std::fs::create_dir_all(site.join(".aigentic/memory")).unwrap();
        std::fs::write(site.join("aigentic.toml"), "[project]\nname = \"web\"\n").unwrap();
        std::fs::write(site.join(".aigentic/memory/a.md"), "remembered\n").unwrap();
        std::fs::write(site.join("index.html"), "<p>hi</p>\n").unwrap();
        let threads = dir.path().join("threads");
        std::fs::create_dir_all(&threads).unwrap();
        std::fs::write(threads.join("01J000000000000000000000.jsonl"), "{}\n").unwrap();
        exec(
            WorkspaceCommand::New {
                name: "w".into(),
                shared: None,
            },
            &cfg,
            dir.path(),
        );
        exec(
            WorkspaceCommand::Add {
                path: site.clone(),
                workspace: None,
            },
            &cfg,
            dir.path(),
        );
        let site_before = snapshot(&site);
        let threads_before = snapshot(&threads);

        let (code, _, _) = exec(
            WorkspaceCommand::Remove {
                project: site.display().to_string(),
                workspace: Some("w".into()),
            },
            &cfg,
            dir.path(),
        );
        assert_eq!(code, 0);
        assert_eq!(snapshot(&site), site_before);
        assert_eq!(snapshot(&threads), threads_before);
        assert!(workspaces::load_all(&cfg).unwrap()[0].projects.is_empty());
    }

    #[test]
    fn add_without_a_workspace_and_two_of_them_fails_naming_both() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("cfg");
        for name in ["alpha", "beta"] {
            exec(
                WorkspaceCommand::New {
                    name: name.into(),
                    shared: None,
                },
                &cfg,
                dir.path(),
            );
        }
        let (code, out, err) = exec(
            WorkspaceCommand::Add {
                path: ".".into(),
                workspace: None,
            },
            &cfg,
            dir.path(),
        );
        assert_eq!((code, out.as_str()), (1, ""));
        assert_eq!(
            err,
            "more than one workspace; pass --workspace with one of: alpha, beta\n"
        );
        assert!(
            workspaces::load_all(&cfg)
                .unwrap()
                .iter()
                .all(|w| w.projects.is_empty())
        );
    }

    #[test]
    fn add_without_any_workspace_says_how_to_make_one() {
        let dir = tempfile::tempdir().unwrap();
        let (code, _, err) = exec(
            WorkspaceCommand::Add {
                path: ".".into(),
                workspace: None,
            },
            &dir.path().join("cfg"),
            dir.path(),
        );
        assert_eq!(code, 1);
        assert!(err.contains("aigentic workspace new"), "{err}");
    }

    #[test]
    fn a_file_that_does_not_parse_is_listed_by_name_and_exits_1() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("cfg");
        let (code, out, _) = exec(WorkspaceCommand::List, &cfg, dir.path());
        assert_eq!(code, 0);
        assert!(out.starts_with("no workspaces in "), "{out}");
        exec(
            WorkspaceCommand::New {
                name: "good".into(),
                shared: None,
            },
            &cfg,
            dir.path(),
        );
        let bad = cfg.join("workspaces/bad.toml");
        std::fs::write(&bad, "nope = 1\n").unwrap();
        let (code, out, _) = exec(WorkspaceCommand::List, &cfg, dir.path());
        assert_eq!(code, 1);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[..3], ["good", "  shared  (none)", "  (no projects)"]);
        assert!(
            lines[3].starts_with(&format!("{}: ", bad.display())),
            "{out}"
        );
    }

    #[test]
    fn a_relative_path_is_made_absolute_by_name() {
        let cwd = Path::new("/a/b");
        assert_eq!(absolute(cwd, Path::new(".")), PathBuf::from("/a/b"));
        assert_eq!(
            absolute(cwd, Path::new("../c/./d")),
            PathBuf::from("/a/c/d")
        );
        assert_eq!(absolute(cwd, Path::new("/x/y")), PathBuf::from("/x/y"));
    }
}
