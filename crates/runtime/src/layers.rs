//! Instruction layering: global, project, thread. Each layer narrows the
//! one above and never widens it. The global layer is the owner's
//! (`~/.config/aigentic/`); the project layer is `aigentic.toml` and its
//! folders; the thread layer is the pinned facts in the log.

use std::path::Path;

use crate::Project;
use crate::project::ProjectError;

/// The owner's layer: who the agent is, house rules, and what no project
/// may offer.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct GlobalLayer {
    pub instructions: Option<String>,
    /// Tool names, exact or with a trailing `*`.
    pub denied_tools: Vec<String>,
    pub denied_skills: Vec<String>,
}

impl GlobalLayer {
    /// Read `instructions.md`; a missing file is no instructions.
    pub fn load(
        instructions: &Path,
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
        Ok(Self {
            instructions: text.filter(|t| !t.trim().is_empty()),
            denied_tools,
            denied_skills,
        })
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

    pub fn memory_prefix(&self) -> Option<String> {
        self.project.as_ref()?.memory_prefix()
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
                },
                ..ProjectFile::default()
            },
            instructions: Some("project rules".into()),
            memory: vec![],
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
            },
            project: Some(project(&["bash", "mcp.docs.search", "read_file"])),
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
        let g = GlobalLayer::load(&path, vec![], vec![]).unwrap();
        assert_eq!(g.instructions, None);
        std::fs::write(&path, "  \n").unwrap();
        assert_eq!(
            GlobalLayer::load(&path, vec![], vec![])
                .unwrap()
                .instructions,
            None
        );
        std::fs::write(&path, "You are terse.\n").unwrap();
        assert_eq!(
            GlobalLayer::load(&path, vec![], vec![])
                .unwrap()
                .instructions
                .as_deref(),
            Some("You are terse.\n")
        );
    }
}
