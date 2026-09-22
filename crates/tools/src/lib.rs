//! Built-in tools (`read_file`, `write_file`, `edit_file`, `list_dir`,
//! `grep`, `bash`) and the [`ToolRegistry`] that holds them.
//!
//! All of them share a [`Workdir`]: `bash` keeps its current directory across
//! calls (a `cd` in one call is seen by the next), and the file tools resolve
//! relative paths against it. Every tool caps what it returns with
//! [`truncate_output`], keeping the head and the tail and noting what was
//! omitted, so a single call can never flood the context.

mod bash;
pub mod diff;
mod files;
mod fs;
pub mod knowledge;
pub mod mcp;
mod registry;
mod truncate;
mod workdir;

pub use bash::BashTool;
pub use files::{ReadFileTool, WriteFileTool};
pub use fs::{DEFAULT_GREP_MATCHES, EditFileTool, GrepTool, ListDirTool};
pub use knowledge::{
    KnowledgeSnapshot, SEARCH_KNOWLEDGE, SearchKnowledgeTool, Section, search, split_sections,
};
pub use mcp::{McpError, McpServer, McpServerConfig, McpTool, McpTransport};
pub use registry::{RegistryError, ToolRegistry};
pub use truncate::{DEFAULT_OUTPUT_CAP, truncate_output};
pub use workdir::Workdir;

/// The built-in tools as a plain vector, sharing one working directory.
/// `ToolRegistry::builtin` is the same set; this form feeds
/// `Runtime::new` until phase 3 step 7 switches it to the registry.
pub fn builtin_tools(workdir: Workdir) -> Vec<Box<dyn aigentic_core::Tool>> {
    ToolRegistry::builtin(workdir).into_tools()
}
