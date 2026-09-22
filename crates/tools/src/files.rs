use aigentic_core::{BoxFuture, RiskClass, Tool, ToolError, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::truncate::{DEFAULT_OUTPUT_CAP, truncate_output};
use crate::workdir::Workdir;

#[derive(Debug, Deserialize, JsonSchema)]
struct ReadFileArgs {
    /// Path to read, absolute or relative to the working directory.
    path: String,
}

/// Read a UTF-8 text file.
#[derive(Debug, Clone)]
pub struct ReadFileTool {
    workdir: Workdir,
    output_cap: usize,
}

impl ReadFileTool {
    pub fn new(workdir: Workdir) -> Self {
        Self {
            workdir,
            output_cap: DEFAULT_OUTPUT_CAP,
        }
    }

    /// Cap on returned bytes; larger files are truncated head-and-tail.
    pub fn with_output_cap(mut self, cap: usize) -> Self {
        self.output_cap = cap;
        self
    }
}

impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read a UTF-8 text file. Large files are returned truncated (head and tail kept)."
    }

    fn schema(&self) -> schemars::schema::RootSchema {
        schemars::schema_for!(ReadFileArgs)
    }

    fn risk_class(&self) -> RiskClass {
        RiskClass::Read
    }

    fn call(&self, args: serde_json::Value) -> BoxFuture<'_, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let args: ReadFileArgs = parse_args(args)?;
            let path = self.workdir.resolve(&args.path);
            let bytes = tokio::fs::read(&path)
                .await
                .map_err(|e| ToolError::Execution(format!("{}: {e}", path.display())))?;
            let text = String::from_utf8(bytes).map_err(|_| {
                ToolError::Execution(format!("{}: not valid UTF-8", path.display()))
            })?;
            Ok(ToolOutput {
                content: truncate_output(&text, self.output_cap),
                is_error: false,
            })
        })
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WriteFileArgs {
    /// Path to write, absolute or relative to the working directory.
    /// Parent directories are created.
    path: String,
    /// Full file content; the file is replaced.
    content: String,
}

/// Write (create or replace) a text file.
#[derive(Debug, Clone)]
pub struct WriteFileTool {
    workdir: Workdir,
}

impl WriteFileTool {
    pub fn new(workdir: Workdir) -> Self {
        Self { workdir }
    }
}

impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        "Create or replace a text file with the given content, creating parent directories."
    }

    fn schema(&self) -> schemars::schema::RootSchema {
        schemars::schema_for!(WriteFileArgs)
    }

    fn risk_class(&self) -> RiskClass {
        RiskClass::Write
    }

    fn call(&self, args: serde_json::Value) -> BoxFuture<'_, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let args: WriteFileArgs = parse_args(args)?;
            let path = self.workdir.resolve(&args.path);
            let io = |e: std::io::Error| ToolError::Execution(format!("{}: {e}", path.display()));
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await.map_err(io)?;
            }
            // What was there before, for the diff: nothing for a new file.
            let before = match tokio::fs::read(&path).await {
                Ok(bytes) => String::from_utf8(bytes).unwrap_or_default(),
                Err(_) => String::new(),
            };
            tokio::fs::write(&path, args.content.as_bytes())
                .await
                .map_err(io)?;
            let diff = crate::diff::unified(&path, &before, &args.content);
            Ok(ToolOutput {
                content: truncate_output(
                    &format!(
                        "{diff}wrote {} bytes to {}",
                        args.content.len(),
                        path.display()
                    ),
                    DEFAULT_OUTPUT_CAP,
                ),
                is_error: false,
            })
        })
    }
}

pub(crate) fn parse_args<T: serde::de::DeserializeOwned>(
    args: serde_json::Value,
) -> Result<T, ToolError> {
    serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn write_file_result_starts_with_a_diff() {
        let dir = tempfile::tempdir().unwrap();
        let write = WriteFileTool::new(Workdir::new(dir.path()));
        let out = write
            .call(json!({"path": "n.txt", "content": "one\n"}))
            .await
            .unwrap();
        let shown = dir.path().join("n.txt").display().to_string();
        assert!(
            out.content
                .starts_with(&format!("--- a/{shown}\n+++ b/{shown}\n@@")),
            "{}",
            out.content
        );
        assert!(out.content.contains("+one\n"), "{}", out.content);
        assert!(
            out.content.ends_with(&format!("wrote 4 bytes to {shown}")),
            "{}",
            out.content
        );
        // Rewriting with the same content: no diff, just the summary.
        let out = write
            .call(json!({"path": "n.txt", "content": "one\n"}))
            .await
            .unwrap();
        assert_eq!(out.content, format!("wrote 4 bytes to {shown}"));
    }

    #[tokio::test]
    async fn write_then_read_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let workdir = Workdir::new(dir.path());
        let write = WriteFileTool::new(workdir.clone());
        let read = ReadFileTool::new(workdir);

        let content = "fn main() {}\n// héj\n";
        let out = write
            .call(json!({"path": "src/nested/main.rs", "content": content}))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("\nwrote "), "{}", out.content);

        let out = read
            .call(json!({"path": "src/nested/main.rs"}))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(out.content, content);
    }

    #[tokio::test]
    async fn missing_file_is_an_execution_error() {
        let dir = tempfile::tempdir().unwrap();
        let read = ReadFileTool::new(Workdir::new(dir.path()));
        let err = read.call(json!({"path": "nope.txt"})).await.unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)), "{err}");
    }

    #[tokio::test]
    async fn bad_args_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let read = ReadFileTool::new(Workdir::new(dir.path()));
        assert!(matches!(
            read.call(json!({"file": "x"})).await.unwrap_err(),
            ToolError::InvalidArgs(_)
        ));
    }

    #[tokio::test]
    async fn large_file_is_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let big: String = (1..=1000).map(|i| format!("{i}\n")).collect();
        std::fs::write(dir.path().join("big.txt"), &big).unwrap();
        let read = ReadFileTool::new(Workdir::new(dir.path())).with_output_cap(200);
        let out = read.call(json!({"path": "big.txt"})).await.unwrap();
        assert!(out.content.contains("bytes omitted"));
        assert!(out.content.ends_with("1000\n"));
    }

    #[test]
    fn schemas_name_their_fields() {
        let dir = tempfile::tempdir().unwrap();
        let w = Workdir::new(dir.path());
        let s = serde_json::to_value(ReadFileTool::new(w.clone()).schema()).unwrap();
        assert!(s["properties"]["path"].is_object());
        let s = serde_json::to_value(WriteFileTool::new(w).schema()).unwrap();
        assert!(s["properties"]["content"].is_object());
        assert_eq!(s["required"], json!(["content", "path"]));
    }
}
