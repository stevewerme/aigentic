//! Built-in tools: `read_file`, `write_file` and `bash`.
//!
//! All three share a [`Workdir`]: `bash` keeps its current directory across
//! calls (a `cd` in one call is seen by the next), and the file tools resolve
//! relative paths against it. Every tool caps what it returns with
//! [`truncate_output`], keeping the head and the tail and noting what was
//! omitted, so a single call can never flood the context.

mod bash;
mod files;
mod truncate;
mod workdir;

pub use bash::BashTool;
pub use files::{ReadFileTool, WriteFileTool};
pub use truncate::{DEFAULT_OUTPUT_CAP, truncate_output};
pub use workdir::Workdir;

/// The three phase-0 tools, sharing one working directory.
pub fn builtin_tools(workdir: Workdir) -> Vec<Box<dyn aigentic_core::Tool>> {
    vec![
        Box::new(ReadFileTool::new(workdir.clone())),
        Box::new(WriteFileTool::new(workdir.clone())),
        Box::new(BashTool::new(workdir)),
    ]
}
