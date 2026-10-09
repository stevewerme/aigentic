//! Instruction layering: global, project, thread. Each layer narrows the
//! one above and never widens it. The global layer is the owner's
//! (`~/.config/aigentic/`); the project layer is `aigentic.toml` and its
//! folders; the thread layer is the pinned facts in the log.

use std::path::{Path, PathBuf};

use crate::Project;
use crate::project::ProjectError;

/// The heading of the person's memory block in the prefix. The person is
/// unnamed here: the config names a user, not the person the memory is
/// about.
pub const PERSON_MEMORY_HEADING: &str = "# Person memory";

/// The owner's layer: who the agent is, house rules, and what no project
/// may offer.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct GlobalLayer {
    pub instructions: Option<String>,
    /// The person's memory folder, kept so the runtime can reload it;
    /// `None` when nobody configured one.
    pub memory_dir: Option<PathBuf>,
    /// The person's memory files, name and text, in name order.
    pub memory: Vec<(String, String)>,
    /// The daemon's owner: the only person whose lines the person's
    /// memory is written from. `None` when the config lists no user, and
    /// then there is no person memory at all.
    pub owner: Option<String>,
    /// Tool names, exact or with a trailing `*`.
    pub denied_tools: Vec<String>,
    pub denied_skills: Vec<String>,
}

impl GlobalLayer {
    /// Read `instructions.md` and the person's memory folder; a missing
    /// file or folder is empty.
    pub fn load(
        instructions: &Path,
        memory_dir: Option<&Path>,
        owner: Option<&str>,
        denied_tools: Vec<String>,
        denied_skills: Vec<String>,
    ) -> Result<Self, ProjectError> {
        let text = match std::fs::read_to_string(instructions) {
            Ok(text) => Some(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(source) => {
                return Err(ProjectError::Io {
                    path: instructions.to_path_buf(),
                    source,
                });
            }
        };
        let memory = match memory_dir {
            Some(dir) => crate::project::read_md_files(dir)?,
            None => Vec::new(),
        };
        Ok(Self {
            instructions: text.filter(|t| !t.trim().is_empty()),
            memory_dir: memory_dir.map(Path::to_path_buf),
            memory,
            owner: owner.map(str::to_owned),
            denied_tools,
            denied_skills,
        })
    }

    /// Re-read the person's memory folder, if this layer has one.
    pub fn reload_memory(&mut self) -> Result<(), ProjectError> {
        let Some(dir) = &self.memory_dir else {
            return Ok(());
        };
        self.memory = crate::project::read_md_files(dir)?;
        Ok(())
    }

    /// The person's memory, as the block the prefix carries right after
    /// the global instructions: one `## <file>` per non-empty file, and
    /// nothing when the folder has none.
    pub(crate) fn memory_block(&self) -> Option<String> {
        let parts: Vec<String> = self
            .memory
            .iter()
            .filter(|(_, t)| !t.trim().is_empty())
            .map(|(n, t)| format!("## {n}\n\n{}", t.trim_end()))
            .collect();
        if parts.is_empty() {
            return None;
        }
        Some(format!("{PERSON_MEMORY_HEADING}\n\n{}", parts.join("\n\n")))
    }
}

/// A workspace's layer (phase 6 step 10): what every project in it
/// shares. Its files live in `<shared>/workspace/`: `instructions.md`,
/// `brief.md`, `memory/*.md` and `knowledge/*.md`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct WorkspaceLayer {
    pub name: String,
    /// The folder the layer was read from, kept so the runtime can reload
    /// and stat the knowledge beside the project's.
    pub shared: Option<std::path::PathBuf>,
    pub instructions: Option<String>,
    /// `brief.md` beside `instructions.md` (issue #123), read in `load`
    /// and kept here, so the prefix is a getter, not a live read. A
    /// missing, empty or non-regular file is `None`.
    pub brief: Option<String>,
    /// Memory files, name and text, in name order.
    pub memory: Vec<(String, String)>,
    /// The workspace's knowledge folder, loaded and reloaded by the
    /// runtime. The whole shared dir is trusted, so no boundary walk.
    pub knowledge: Option<crate::knowledge::Knowledge>,
}

impl WorkspaceLayer {
    /// The folder under `shared` that holds the workspace's own files.
    pub fn dir(shared: &Path) -> std::path::PathBuf {
        shared.join("workspace")
    }

    /// The folder the workspace's knowledge lives in, under the shared
    /// dir the layer was read from.
    pub fn knowledge_dir(&self) -> Option<std::path::PathBuf> {
        self.shared
            .as_ref()
            .map(|shared| Self::dir(shared).join(crate::project::KNOWLEDGE_DIR))
    }

    /// Read the layer; missing files are an empty layer.
    pub fn load(name: &str, shared: Option<&Path>) -> Result<Self, ProjectError> {
        let mut layer = Self {
            name: name.to_owned(),
            ..Self::default()
        };
        let Some(shared) = shared else {
            return Ok(layer);
        };
        layer.shared = Some(shared.to_path_buf());
        let read = |path: &Path| match std::fs::read_to_string(path) {
            Ok(t) => Ok(Some(t)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(ProjectError::Io {
                path: path.to_path_buf(),
                source,
            }),
        };
        layer.instructions =
            read(&Self::dir(shared).join("instructions.md"))?.filter(|t| !t.trim().is_empty());
        layer.brief = crate::brief::read_file(&crate::brief::workspace_brief_path(shared));
        layer.memory = Self::read_memory(shared)?;
        Ok(layer)
    }

    /// `memory/*.md` beside the workspace's instructions; a missing
    /// folder is empty.
    fn read_memory(shared: &Path) -> Result<Vec<(String, String)>, ProjectError> {
        let read = |path: &Path| match std::fs::read_to_string(path) {
            Ok(t) => Ok(Some(t)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(ProjectError::Io {
                path: path.to_path_buf(),
                source,
            }),
        };
        let mut memory = Vec::new();
        if let Ok(entries) = std::fs::read_dir(Self::dir(shared).join(crate::project::MEMORY_DIR)) {
            let mut files: Vec<_> = entries
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "md"))
                .collect();
            files.sort();
            for f in files {
                if let Some(text) = read(&f)? {
                    let name = f
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    memory.push((name, text));
                }
            }
        }
        Ok(memory)
    }

    /// Re-read the workspace's memory folder, if this layer has one.
    pub fn reload_memory(&mut self) -> Result<(), ProjectError> {
        let Some(shared) = &self.shared else {
            return Ok(());
        };
        self.memory = Self::read_memory(shared)?;
        Ok(())
    }

    /// The workspace's brief (issue #123), for the prefix block.
    pub fn brief(&self) -> Option<&str> {
        self.brief.as_deref()
    }

    /// The block the prefix carries after the global one.
    pub fn instructions_block(&self) -> Option<String> {
        let text = self.instructions.as_deref()?;
        Some(format!("# Workspace {}\n\n{}", self.name, text.trim_end()))
    }

    fn memory_block(&self) -> Option<String> {
        let parts: Vec<String> = self
            .memory
            .iter()
            .filter(|(_, t)| !t.trim().is_empty())
            .map(|(n, t)| format!("## {n}\n\n{}", t.trim_end()))
            .collect();
        if parts.is_empty() {
            return None;
        }
        Some(format!(
            "# Workspace memory ({})\n\n{}",
            self.name,
            parts.join("\n\n")
        ))
    }
}

/// Which layer settled a tool's or skill's fate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decided {
    /// Offered to the model.
    Allowed,
    /// The global layer denies it; no project can bring it back.
    DeniedByGlobal,
    /// The project lists what it allows and this is not on the list.
    NotInProjectAllow,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Layers {
    pub global: GlobalLayer,
    /// Between the global and project layers (phase 6 step 10).
    pub workspace: Option<WorkspaceLayer>,
    pub project: Option<Project>,
}

impl Layers {
    /// Only global instructions: what a run outside any project has, and
    /// what tests use for a one-line prefix.
    pub fn global_instructions(text: impl Into<String>) -> Self {
        Self {
            global: GlobalLayer {
                instructions: Some(text.into()),
                ..GlobalLayer::default()
            },
            workspace: None,
            project: None,
        }
    }

    pub fn with_project(mut self, project: Project) -> Self {
        self.project = Some(project);
        self
    }

    /// The registry's names minus the global denials, then only what the
    /// project allows when it lists anything. Order is kept.
    pub fn allowed_tools(&self, names: &[String]) -> Vec<String> {
        names
            .iter()
            .filter(|n| self.decided_tool(n) == Decided::Allowed)
            .cloned()
            .collect()
    }

    /// The enabled skills minus the global denials.
    pub fn allowed_skills(&self, enabled: &[String]) -> Vec<String> {
        enabled
            .iter()
            .filter(|n| self.decided_skill(n) == Decided::Allowed)
            .cloned()
            .collect()
    }

    pub fn decided_tool(&self, name: &str) -> Decided {
        if self.global.denied_tools.iter().any(|p| matches(p, name)) {
            return Decided::DeniedByGlobal;
        }
        if let Some(project) = &self.project {
            let allow = &project.file.tools.allow;
            if !allow.is_empty() && !allow.iter().any(|p| matches(p, name)) {
                return Decided::NotInProjectAllow;
            }
        }
        Decided::Allowed
    }

    pub fn decided_skill(&self, name: &str) -> Decided {
        if self.global.denied_skills.iter().any(|p| matches(p, name)) {
            return Decided::DeniedByGlobal;
        }
        Decided::Allowed
    }

    pub fn project_instructions(&self) -> Option<&str> {
        self.project.as_ref()?.instructions.as_deref()
    }

    pub fn workspace_instructions(&self) -> Option<String> {
        self.workspace.as_ref()?.instructions_block()
    }

    /// Re-read every home this thread has: the person's, the workspace's
    /// and the project's. Called at turn start, so a file edited by hand
    /// or by another thread shows in the next turn.
    pub fn reload_memory(&mut self) -> Result<(), ProjectError> {
        self.global.reload_memory()?;
        if let Some(workspace) = &mut self.workspace {
            workspace.reload_memory()?;
        }
        if let Some(project) = &mut self.project {
            project.reload_memory()?;
        }
        Ok(())
    }

    /// The workspace's memory, then the project's, as one block.
    pub fn memory_prefix(&self) -> Option<String> {
        let parts: Vec<String> = [
            self.workspace
                .as_ref()
                .and_then(WorkspaceLayer::memory_block),
            self.project.as_ref().and_then(Project::memory_prefix),
        ]
        .into_iter()
        .flatten()
        .collect();
        (!parts.is_empty()).then(|| parts.join("\n\n"))
    }
}

/// Exact name, or a trailing `*` prefix (`mcp.*`).
pub fn matches(pattern: &str, name: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => pattern == name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::{ProjectFile, ToolsSection};
    use std::path::PathBuf;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    fn project(allow: &[&str]) -> Project {
        Project {
            name: "p".into(),
            root: PathBuf::from("."),
            file: ProjectFile {
                tools: ToolsSection {
                    allow: names(allow),
                    ..ToolsSection::default()
                },
                ..ProjectFile::default()
            },
            instructions: Some("project rules".into()),
            memory: vec![],
            unknown: vec![],
        }
    }

    #[test]
    fn empty_allow_means_everything_and_global_denial_beats_project_allow() {
        let registry = names(&["bash", "read_file", "mcp.docs.search", "pin"]);
        let layers = Layers::default().with_project(project(&[]));
        assert_eq!(layers.allowed_tools(&registry), registry);

        let layers = Layers {
            global: GlobalLayer {
                instructions: None,
                denied_tools: names(&["mcp.*", "bash"]),
                denied_skills: names(&["wizard"]),
                ..GlobalLayer::default()
            },
            project: Some(project(&["bash", "mcp.docs.search", "read_file"])),
            workspace: None,
        };
        assert_eq!(layers.allowed_tools(&registry), names(&["read_file"]));
        assert_eq!(layers.decided_tool("bash"), Decided::DeniedByGlobal);
        assert_eq!(
            layers.decided_tool("mcp.docs.search"),
            Decided::DeniedByGlobal
        );
        assert_eq!(layers.decided_tool("pin"), Decided::NotInProjectAllow);
        assert_eq!(layers.decided_tool("read_file"), Decided::Allowed);
        assert_eq!(
            layers.allowed_skills(&names(&["tdd", "wizard"])),
            names(&["tdd"])
        );
        assert_eq!(layers.decided_skill("wizard"), Decided::DeniedByGlobal);
    }

    #[test]
    fn project_allow_supports_globs_and_keeps_registry_order() {
        let registry = names(&["write_file", "mcp.docs.b", "mcp.docs.a", "bash"]);
        let layers = Layers::default().with_project(project(&["mcp.docs.*", "bash"]));
        assert_eq!(
            layers.allowed_tools(&registry),
            names(&["mcp.docs.b", "mcp.docs.a", "bash"])
        );
    }

    #[test]
    fn global_layer_loads_or_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("instructions.md");
        let g = GlobalLayer::load(&path, None, None, vec![], vec![]).unwrap();
        assert_eq!(g.instructions, None);
        std::fs::write(&path, "  \n").unwrap();
        assert_eq!(
            GlobalLayer::load(&path, None, None, vec![], vec![])
                .unwrap()
                .instructions,
            None
        );
        std::fs::write(&path, "You are terse.\n").unwrap();
        assert_eq!(
            GlobalLayer::load(&path, None, None, vec![], vec![])
                .unwrap()
                .instructions
                .as_deref(),
            Some("You are terse.\n")
        );
    }
}
