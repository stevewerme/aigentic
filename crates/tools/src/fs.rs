//! `list_dir`, `grep` and `edit_file`: what `implement` and `tdd` expect
//! of an editor beyond read and write.

use std::path::{Path, PathBuf};

use aigentic_core::{BoxFuture, RiskClass, Tool, ToolError, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::files::parse_args;
use crate::truncate::{DEFAULT_OUTPUT_CAP, truncate_output};
use crate::workdir::Workdir;

/// Default cap on `grep` matches, before the byte cap applies.
pub const DEFAULT_GREP_MATCHES: usize = 200;

// list_dir

#[derive(Debug, Deserialize, JsonSchema)]
struct ListDirArgs {
    /// Directory to list, absolute or relative to the working directory.
    /// Defaults to the working directory.
    #[serde(default)]
    path: Option<String>,
}

/// List one directory: entries sorted by name, directories with a
/// trailing slash, files with their size.
#[derive(Debug, Clone)]
pub struct ListDirTool {
    workdir: Workdir,
    output_cap: usize,
}

impl ListDirTool {
    pub fn new(workdir: Workdir) -> Self {
        Self {
            workdir,
            output_cap: DEFAULT_OUTPUT_CAP,
        }
    }

    pub fn with_output_cap(mut self, cap: usize) -> Self {
        self.output_cap = cap;
        self
    }
}

impl Tool for ListDirTool {
    fn name(&self) -> &str {
        "list_dir"
    }

    fn description(&self) -> &str {
        "List a directory's entries, sorted: `name/` for directories, `name  <bytes>` for files. Not recursive."
    }

    fn schema(&self) -> schemars::schema::RootSchema {
        schemars::schema_for!(ListDirArgs)
    }

    fn risk_class(&self) -> RiskClass {
        RiskClass::Read
    }

    fn call(&self, args: serde_json::Value) -> BoxFuture<'_, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let args: ListDirArgs = parse_args(args)?;
            let path = self.workdir.resolve(args.path.as_deref().unwrap_or("."));
            let io = |e: std::io::Error| ToolError::Execution(format!("{}: {e}", path.display()));
            let mut dir = tokio::fs::read_dir(&path).await.map_err(io)?;
            let mut lines = Vec::new();
            while let Some(entry) = dir.next_entry().await.map_err(io)? {
                let name = entry.file_name().to_string_lossy().into_owned();
                let meta = entry.metadata().await.map_err(io)?;
                if meta.is_dir() {
                    lines.push(format!("{name}/"));
                } else {
                    lines.push(format!("{name}  {}", meta.len()));
                }
            }
            lines.sort();
            let text = if lines.is_empty() {
                "(empty)".to_owned()
            } else {
                lines.join("\n") + "\n"
            };
            Ok(ToolOutput {
                content: truncate_output(&text, self.output_cap),
                is_error: false,
            })
        })
    }
}

// grep

#[derive(Debug, Deserialize, JsonSchema)]
struct GrepArgs {
    /// Regular expression (Rust `regex` syntax) to search for.
    pattern: String,
    /// File or directory to search, absolute or relative to the working
    /// directory. Defaults to the working directory.
    #[serde(default)]
    path: Option<String>,
    /// Restrict to files whose path matches this glob, e.g. `*.rs`.
    #[serde(default)]
    glob: Option<String>,
    /// Match case-insensitively.
    #[serde(default)]
    ignore_case: bool,
}

/// Search files for a regex, ripgrep-style: recursive, `.gitignore` and
/// hidden files respected, binary files skipped, `path:line:text` output.
#[derive(Debug, Clone)]
pub struct GrepTool {
    workdir: Workdir,
    output_cap: usize,
    max_matches: usize,
}

impl GrepTool {
    pub fn new(workdir: Workdir) -> Self {
        Self {
            workdir,
            output_cap: DEFAULT_OUTPUT_CAP,
            max_matches: DEFAULT_GREP_MATCHES,
        }
    }

    pub fn with_output_cap(mut self, cap: usize) -> Self {
        self.output_cap = cap;
        self
    }

    pub fn with_max_matches(mut self, max: usize) -> Self {
        self.max_matches = max;
        self
    }
}

impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search files for a regular expression, recursively, respecting .gitignore and skipping binary files. Output is `path:line:text`, capped."
    }

    fn schema(&self) -> schemars::schema::RootSchema {
        schemars::schema_for!(GrepArgs)
    }

    fn risk_class(&self) -> RiskClass {
        RiskClass::Read
    }

    fn call(&self, args: serde_json::Value) -> BoxFuture<'_, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let args: GrepArgs = parse_args(args)?;
            let root = self.workdir.resolve(args.path.as_deref().unwrap_or("."));
            let base = self.workdir.current();
            let max_matches = self.max_matches;
            let cap = self.output_cap;
            let text =
                tokio::task::spawn_blocking(move || grep_sync(&args, &root, &base, max_matches))
                    .await
                    .map_err(|e| ToolError::Execution(format!("grep task failed: {e}")))??;
            Ok(ToolOutput {
                content: truncate_output(&text, cap),
                is_error: false,
            })
        })
    }
}

fn grep_sync(
    args: &GrepArgs,
    root: &Path,
    base: &Path,
    max_matches: usize,
) -> Result<String, ToolError> {
    let regex = regex::RegexBuilder::new(&args.pattern)
        .case_insensitive(args.ignore_case)
        .build()
        .map_err(|e| ToolError::InvalidArgs(format!("bad pattern: {e}")))?;
    let glob = args
        .glob
        .as_deref()
        .map(|g| {
            let mut b = ignore::overrides::OverrideBuilder::new(root);
            b.add(g)
                .map_err(|e| ToolError::InvalidArgs(format!("bad glob: {e}")))?;
            b.build()
                .map_err(|e| ToolError::InvalidArgs(format!("bad glob: {e}")))
        })
        .transpose()?;
    if !root.exists() {
        return Err(ToolError::Execution(format!(
            "{}: no such file or directory",
            root.display()
        )));
    }
    let mut builder = ignore::WalkBuilder::new(root);
    // Honour .gitignore even outside a git checkout (the default needs a
    // .git directory before it reads one).
    builder.require_git(false);
    builder.sort_by_file_path(|a, b| a.cmp(b));
    if let Some(glob) = glob {
        builder.overrides(glob);
    }
    let mut out = String::new();
    let mut matches = 0usize;
    let mut files_searched = 0usize;
    'files: for entry in builder.build() {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Ok(bytes) = std::fs::read(entry.path()) else {
            continue;
        };
        if bytes.contains(&0) {
            continue; // binary
        }
        let text = String::from_utf8_lossy(&bytes);
        files_searched += 1;
        let shown = entry.path().strip_prefix(base).unwrap_or(entry.path());
        for (i, line) in text.lines().enumerate() {
            if regex.is_match(line) {
                if matches == max_matches {
                    out.push_str(&format!(
                        "[... more matches omitted; showing the first {max_matches} ...]\n"
                    ));
                    break 'files;
                }
                matches += 1;
                out.push_str(&format!("{}:{}:{}\n", shown.display(), i + 1, line));
            }
        }
    }
    if matches == 0 {
        out = format!("no matches in {files_searched} files");
    }
    Ok(out)
}

// edit_file

#[derive(Debug, Deserialize, JsonSchema)]
struct EditFileArgs {
    /// File to edit, absolute or relative to the working directory.
    path: String,
    /// Exact text to find. Must occur exactly once.
    old_string: String,
    /// Text to put in its place.
    new_string: String,
}

/// Replace one exact occurrence of a string in a file. Refuses when the
/// string is absent or occurs more than once, so an edit never lands in
/// the wrong place.
#[derive(Debug, Clone)]
pub struct EditFileTool {
    workdir: Workdir,
}

impl EditFileTool {
    pub fn new(workdir: Workdir) -> Self {
        Self { workdir }
    }
}

impl Tool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn description(&self) -> &str {
        "Replace an exact string in a file with another. `old_string` must occur exactly once; include enough surrounding lines to make it unique."
    }

    fn schema(&self) -> schemars::schema::RootSchema {
        schemars::schema_for!(EditFileArgs)
    }

    fn risk_class(&self) -> RiskClass {
        RiskClass::Write
    }

    fn call(&self, args: serde_json::Value) -> BoxFuture<'_, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let args: EditFileArgs = parse_args(args)?;
            if args.old_string.is_empty() {
                return Err(ToolError::InvalidArgs(
                    "old_string must not be empty".into(),
                ));
            }
            let path: PathBuf = self.workdir.resolve(&args.path);
            let io = |e: std::io::Error| ToolError::Execution(format!("{}: {e}", path.display()));
            let bytes = tokio::fs::read(&path).await.map_err(io)?;
            let text = String::from_utf8(bytes).map_err(|_| {
                ToolError::Execution(format!("{}: not valid UTF-8", path.display()))
            })?;
            match text.matches(&args.old_string).count() {
                0 => {
                    return Err(ToolError::Execution(format!(
                        "{}: old_string not found",
                        path.display()
                    )));
                }
                1 => {}
                n => {
                    return Err(ToolError::Execution(format!(
                        "{}: old_string occurs {n} times; include more context to make it unique",
                        path.display()
                    )));
                }
            }
            let edited = text.replacen(&args.old_string, &args.new_string, 1);
            tokio::fs::write(&path, edited.as_bytes())
                .await
                .map_err(io)?;
            let line = text[..text.find(&args.old_string).expect("counted once")]
                .matches('\n')
                .count()
                + 1;
            Ok(ToolOutput {
                content: format!("edited {} at line {line}", path.display()),
                is_error: false,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> (tempfile::TempDir, Workdir) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        std::fs::create_dir_all(p.join("src/inner")).unwrap();
        std::fs::create_dir_all(p.join("target")).unwrap();
        std::fs::write(p.join("src/main.rs"), "fn main() {\n    hello();\n}\n").unwrap();
        std::fs::write(
            p.join("src/inner/lib.rs"),
            "pub fn hello() {}\n// Hello again\n",
        )
        .unwrap();
        std::fs::write(p.join("README.md"), "# hello\n").unwrap();
        std::fs::write(p.join("target/out.rs"), "hello ignored\n").unwrap();
        std::fs::write(p.join(".gitignore"), "target/\n").unwrap();
        std::fs::write(p.join("bin.dat"), b"hello\0world").unwrap();
        let w = Workdir::new(p);
        (dir, w)
    }

    #[tokio::test]
    async fn list_dir_sorts_and_marks_directories() {
        let (_d, w) = fixture();
        let out = ListDirTool::new(w.clone()).call(json!({})).await.unwrap();
        assert_eq!(
            out.content,
            ".gitignore  8\nREADME.md  8\nbin.dat  11\nsrc/\ntarget/\n"
        );
        let out = ListDirTool::new(w.clone())
            .call(json!({"path": "src/inner"}))
            .await
            .unwrap();
        assert_eq!(out.content, "lib.rs  33\n");
        let err = ListDirTool::new(w)
            .call(json!({"path": "nope"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)));
    }

    #[tokio::test]
    async fn list_dir_caps_output() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..500 {
            std::fs::write(dir.path().join(format!("file-{i:04}.txt")), "x").unwrap();
        }
        let out = ListDirTool::new(Workdir::new(dir.path()))
            .with_output_cap(300)
            .call(json!({}))
            .await
            .unwrap();
        assert!(out.content.contains("bytes omitted"), "{}", out.content);
        assert!(out.content.starts_with("file-0000.txt"));
        assert!(out.content.ends_with("file-0499.txt  1\n"));
    }

    #[tokio::test]
    async fn grep_finds_lines_respects_gitignore_and_skips_binaries() {
        let (_d, w) = fixture();
        let out = GrepTool::new(w)
            .call(json!({"pattern": "hello"}))
            .await
            .unwrap();
        assert_eq!(
            out.content,
            "README.md:1:# hello\nsrc/inner/lib.rs:1:pub fn hello() {}\nsrc/main.rs:2:    hello();\n"
        );
    }

    #[tokio::test]
    async fn grep_options_and_errors() {
        let (_d, w) = fixture();
        let g = GrepTool::new(w);
        let out = g
            .call(json!({"pattern": "hello", "ignore_case": true, "glob": "*.rs"}))
            .await
            .unwrap();
        assert_eq!(
            out.content,
            "src/inner/lib.rs:1:pub fn hello() {}\nsrc/inner/lib.rs:2:// Hello again\nsrc/main.rs:2:    hello();\n"
        );
        let out = g
            .call(json!({"pattern": "hello", "path": "src/main.rs"}))
            .await
            .unwrap();
        assert_eq!(out.content, "src/main.rs:2:    hello();\n");
        let out = g.call(json!({"pattern": "zzz"})).await.unwrap();
        assert_eq!(out.content, "no matches in 3 files");
        assert!(matches!(
            g.call(json!({"pattern": "("})).await.unwrap_err(),
            ToolError::InvalidArgs(_)
        ));
        assert!(matches!(
            g.call(json!({"pattern": "x", "path": "nope"}))
                .await
                .unwrap_err(),
            ToolError::Execution(_)
        ));
    }

    #[tokio::test]
    async fn grep_caps_matches_and_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let text: String = (0..1000).map(|i| format!("match {i}\n")).collect();
        std::fs::write(dir.path().join("big.txt"), text).unwrap();
        let out = GrepTool::new(Workdir::new(dir.path()))
            .with_max_matches(10)
            .call(json!({"pattern": "match"}))
            .await
            .unwrap();
        assert_eq!(out.content.lines().count(), 11, "{}", out.content);
        assert!(out.content.ends_with("showing the first 10 ...]\n"));

        let out = GrepTool::new(Workdir::new(dir.path()))
            .with_output_cap(200)
            .call(json!({"pattern": "match"}))
            .await
            .unwrap();
        assert!(out.content.contains("bytes omitted"));
    }

    #[tokio::test]
    async fn edit_file_replaces_exactly_one_occurrence() {
        let (d, w) = fixture();
        let e = EditFileTool::new(w);
        let out = e
            .call(json!({"path": "src/main.rs", "old_string": "    hello();\n", "new_string": "    hello();\n    bye();\n"}))
            .await
            .unwrap();
        assert_eq!(
            out.content,
            format!(
                "edited {} at line 2",
                d.path().join("src/main.rs").display()
            )
        );
        assert_eq!(
            std::fs::read_to_string(d.path().join("src/main.rs")).unwrap(),
            "fn main() {\n    hello();\n    bye();\n}\n"
        );
    }

    #[tokio::test]
    async fn edit_file_refuses_zero_and_many_matches() {
        let (_d, w) = fixture();
        let e = EditFileTool::new(w);
        let err = e
            .call(json!({"path": "src/main.rs", "old_string": "nope", "new_string": "x"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not found"), "{err}");
        let err = e
            .call(json!({"path": "src/inner/lib.rs", "old_string": "ello", "new_string": "x"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("occurs 2 times"), "{err}");
        let err = e
            .call(json!({"path": "src/main.rs", "old_string": "", "new_string": "x"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)));
        let err = e
            .call(json!({"path": "missing.rs", "old_string": "a", "new_string": "b"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)));
    }
}
