//! The tool registry: built-ins now, MCP-backed tools in the next step.
//! The runtime looks tools up here by name and sends `specs()` to the
//! model; the harness tools it answers itself are added by the runtime.

use aigentic_core::{Tool, ToolSpec};

use crate::bash::BashTool;
use crate::files::{ReadFileTool, WriteFileTool};
use crate::fs::{EditFileTool, GrepTool, ListDirTool};
use crate::workdir::Workdir;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegistryError {
    #[error("a tool named `{0}` is already registered")]
    Duplicate(String),
}

pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolRegistry")
            .field("tools", &self.names())
            .finish()
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::empty()
    }
}

impl ToolRegistry {
    pub fn empty() -> Self {
        Self { tools: Vec::new() }
    }

    /// `read_file`, `write_file`, `edit_file`, `list_dir`, `grep` and
    /// `bash`, sharing one working directory.
    pub fn builtin(workdir: Workdir) -> Self {
        let mut registry = Self::empty();
        for tool in [
            Box::new(ReadFileTool::new(workdir.clone())) as Box<dyn Tool>,
            Box::new(WriteFileTool::new(workdir.clone())),
            Box::new(EditFileTool::new(workdir.clone())),
            Box::new(ListDirTool::new(workdir.clone())),
            Box::new(GrepTool::new(workdir.clone())),
            Box::new(BashTool::new(workdir)),
        ] {
            registry
                .register(tool)
                .expect("built-in tool names are distinct");
        }
        registry
    }

    /// Add a tool; a second tool with the same name is refused.
    pub fn register(&mut self, tool: Box<dyn Tool>) -> Result<(), RegistryError> {
        if self.get(tool.name()).is_some() {
            return Err(RegistryError::Duplicate(tool.name().to_owned()));
        }
        self.tools.push(tool);
        Ok(())
    }

    /// Every tool as the model sees it, sorted by name so the request is
    /// byte-stable between turns.
    pub fn specs(&self) -> Vec<ToolSpec> {
        let mut specs: Vec<ToolSpec> = self.tools.iter().map(|t| ToolSpec::from(&**t)).collect();
        specs.sort_by(|a, b| a.name.cmp(&b.name));
        specs
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.iter().find(|t| t.name() == name).map(|t| &**t)
    }

    /// Sorted names; what a skill's `requires` is checked against.
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.tools.iter().map(|t| t.name().to_owned()).collect();
        names.sort();
        names
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &dyn Tool> {
        self.tools.iter().map(|t| &**t)
    }

    pub fn into_tools(self) -> Vec<Box<dyn Tool>> {
        self.tools
    }
}

impl From<Vec<Box<dyn Tool>>> for ToolRegistry {
    fn from(tools: Vec<Box<dyn Tool>>) -> Self {
        let mut registry = Self::empty();
        for tool in tools {
            // Later duplicates are dropped rather than failing a conversion.
            let _ = registry.register(tool);
        }
        registry
    }
}

impl FromIterator<Box<dyn Tool>> for ToolRegistry {
    fn from_iter<I: IntoIterator<Item = Box<dyn Tool>>>(iter: I) -> Self {
        Self::from(iter.into_iter().collect::<Vec<_>>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aigentic_core::RiskClass;

    #[test]
    fn builtin_has_six_tools_with_sorted_specs_and_classes() {
        let dir = tempfile::tempdir().unwrap();
        let r = ToolRegistry::builtin(Workdir::new(dir.path()));
        assert_eq!(
            r.names(),
            vec![
                "bash",
                "edit_file",
                "grep",
                "list_dir",
                "read_file",
                "write_file"
            ]
        );
        let specs = r.specs();
        assert_eq!(
            specs.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            r.names()
        );
        assert!(specs.iter().all(|s| s.schema["properties"].is_object()));
        let class = |n: &str| r.get(n).unwrap().risk_class();
        assert_eq!(class("read_file"), RiskClass::Read);
        assert_eq!(class("list_dir"), RiskClass::Read);
        assert_eq!(class("grep"), RiskClass::Read);
        assert_eq!(class("write_file"), RiskClass::Write);
        assert_eq!(class("edit_file"), RiskClass::Write);
        assert_eq!(class("bash"), RiskClass::Exec);
        assert!(r.get("nope").is_none());
    }

    #[test]
    fn duplicate_names_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let w = Workdir::new(dir.path());
        let mut r = ToolRegistry::builtin(w.clone());
        let err = r.register(Box::new(ReadFileTool::new(w))).unwrap_err();
        assert_eq!(err, RegistryError::Duplicate("read_file".into()));
        assert_eq!(r.len(), 6);
    }
}
