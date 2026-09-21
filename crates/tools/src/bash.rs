use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aigentic_core::{BoxFuture, RiskClass, Tool, ToolError, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::files::parse_args;
use crate::truncate::{BoundedCapture, DEFAULT_OUTPUT_CAP};
use crate::workdir::Workdir;

/// Default wall-clock limit for one command.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// How long to keep reading pipes after the shell has exited. A background
/// child that inherited the pipe would otherwise hold the call open.
const DRAIN_GRACE: Duration = Duration::from_millis(500);

#[derive(Debug, Deserialize, JsonSchema)]
struct BashArgs {
    /// Command to run with `bash -c`. The working directory persists between
    /// calls, so `cd` takes effect for later commands.
    command: String,
}

/// Run a shell command with a persistent working directory, a timeout and a
/// cap on captured output.
#[derive(Debug, Clone)]
pub struct BashTool {
    workdir: Workdir,
    timeout: Duration,
    output_cap: usize,
}

impl BashTool {
    pub fn new(workdir: Workdir) -> Self {
        Self {
            workdir,
            timeout: DEFAULT_TIMEOUT,
            output_cap: DEFAULT_OUTPUT_CAP,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Cap per stream (stdout and stderr each), in bytes.
    pub fn with_output_cap(mut self, cap: usize) -> Self {
        self.output_cap = cap;
        self
    }

    async fn run(&self, command: &str) -> Result<ToolOutput, ToolError> {
        let cwd = self.workdir.current();
        // The shell reports its final directory into a temp file so `cd`
        // persists. A command that `exit`s early skips the report and the
        // directory is left unchanged.
        let cwd_file = tempfile::NamedTempFile::new()
            .map_err(|e| ToolError::Execution(format!("temp file: {e}")))?;
        let script = format!(
            "{command}\n__aigentic_status=$?\npwd > \"$AIGENTIC_CWD_FILE\"\nexit $__aigentic_status\n"
        );

        let mut child = Command::new("bash")
            .arg("-c")
            .arg(&script)
            .current_dir(&cwd)
            .env("AIGENTIC_CWD_FILE", cwd_file.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| ToolError::Execution(format!("spawn bash: {e}")))?;

        let stdout = capture(child.stdout.take(), self.output_cap);
        let stderr = capture(child.stderr.take(), self.output_cap);

        let (status, timed_out) = match tokio::time::timeout(self.timeout, child.wait()).await {
            Ok(Ok(status)) => (Some(status), false),
            Ok(Err(e)) => return Err(ToolError::Execution(format!("wait: {e}"))),
            Err(_) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                (None, true)
            }
        };

        // Give the readers a moment to drain, then stop waiting on them: a
        // backgrounded grandchild may keep the pipe open indefinitely.
        let _ = tokio::time::timeout(DRAIN_GRACE, async {
            let _ = stdout.task.await;
            let _ = stderr.task.await;
        })
        .await;
        stdout.task_handle.abort();
        stderr.task_handle.abort();

        if !timed_out && let Ok(reported) = std::fs::read_to_string(cwd_file.path()) {
            let reported = reported.trim_end();
            if !reported.is_empty() {
                self.workdir.set(PathBuf::from(reported));
            }
        }

        let out = stdout.buf.lock().unwrap_or_else(|e| e.into_inner());
        let err = stderr.buf.lock().unwrap_or_else(|e| e.into_inner());
        let mut content = out.render();
        if err.total() > 0 {
            if !content.is_empty() && !content.ends_with('\n') {
                content.push('\n');
            }
            content.push_str("[stderr]\n");
            content.push_str(&err.render());
        }
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }

        let is_error = match status {
            Some(s) if s.success() => false,
            Some(s) => {
                content.push_str(&format!("[exit code {}]\n", s.code().unwrap_or(-1)));
                true
            }
            None => {
                content.push_str(&format!(
                    "[timed out after {} s and was killed]\n",
                    self.timeout.as_secs_f64()
                ));
                true
            }
        };
        Ok(ToolOutput { content, is_error })
    }
}

struct Capture {
    buf: Arc<Mutex<BoundedCapture>>,
    task: tokio::task::JoinHandle<()>,
    task_handle: tokio::task::AbortHandle,
}

fn capture<R>(reader: Option<R>, cap: usize) -> Capture
where
    R: AsyncReadExt + Unpin + Send + 'static,
{
    let buf = Arc::new(Mutex::new(BoundedCapture::new(cap)));
    let sink = buf.clone();
    let task = tokio::spawn(async move {
        let Some(mut reader) = reader else { return };
        let mut chunk = [0u8; 8192];
        while let Ok(n) = reader.read(&mut chunk).await {
            if n == 0 {
                break;
            }
            sink.lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(&chunk[..n]);
        }
    });
    let task_handle = task.abort_handle();
    Capture {
        buf,
        task,
        task_handle,
    }
}

impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Run a shell command with bash. The working directory persists across calls. \
         Output is capped; long output keeps its head and tail."
    }

    fn schema(&self) -> schemars::schema::RootSchema {
        schemars::schema_for!(BashArgs)
    }

    fn risk_class(&self) -> RiskClass {
        RiskClass::Exec
    }

    fn call(&self, args: serde_json::Value) -> BoxFuture<'_, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let args: BashArgs = parse_args(args)?;
            self.run(&args.command).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Instant;

    fn tool(dir: &std::path::Path) -> BashTool {
        BashTool::new(Workdir::new(dir))
    }

    #[tokio::test]
    async fn runs_and_reports_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let t = tool(dir.path());
        let out = t.call(json!({"command": "echo hi"})).await.unwrap();
        assert_eq!(out.content, "hi\n");
        assert!(!out.is_error);

        let out = t
            .call(json!({"command": "echo oops >&2; exit 3"}))
            .await
            .unwrap();
        assert!(out.is_error);
        assert_eq!(out.content, "[stderr]\noops\n[exit code 3]\n");
    }

    #[tokio::test]
    async fn cwd_persists_between_calls() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let workdir = Workdir::new(dir.path());
        let t = BashTool::new(workdir.clone());

        t.call(json!({"command": "cd sub"})).await.unwrap();
        let out = t
            .call(json!({"command": "basename \"$PWD\""}))
            .await
            .unwrap();
        assert_eq!(out.content, "sub\n");
        assert_eq!(workdir.current().file_name().unwrap(), "sub");

        // An early `exit` skips the report; the directory stays put.
        t.call(json!({"command": "cd ..; exit 0"})).await.unwrap();
        assert_eq!(workdir.current().file_name().unwrap(), "sub");
    }

    #[tokio::test]
    async fn times_out_and_kills() {
        let dir = tempfile::tempdir().unwrap();
        let t = tool(dir.path()).with_timeout(Duration::from_millis(300));
        let start = Instant::now();
        let out = t
            .call(json!({"command": "echo before; sleep 5; echo after"}))
            .await
            .unwrap();
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "{:?}",
            start.elapsed()
        );
        assert!(out.is_error);
        assert!(out.content.starts_with("before\n"), "{}", out.content);
        assert!(
            out.content.contains("timed out after 0.3 s"),
            "{}",
            out.content
        );
        assert!(!out.content.contains("after\n"), "{}", out.content);
    }

    #[tokio::test]
    async fn large_output_is_truncated_head_and_tail() {
        let dir = tempfile::tempdir().unwrap();
        let t = tool(dir.path()).with_output_cap(1000);
        let out = t.call(json!({"command": "seq 1 100000"})).await.unwrap();
        assert!(!out.is_error);
        assert!(out.content.starts_with("1\n2\n"), "{}", out.content);
        assert!(out.content.ends_with("99999\n100000\n"), "{}", out.content);
        assert!(out.content.contains("bytes omitted"), "{}", out.content);
        assert!(out.content.len() < 1100, "{}", out.content.len());
    }

    #[tokio::test]
    async fn backgrounded_child_does_not_hang_the_call() {
        let dir = tempfile::tempdir().unwrap();
        let t = tool(dir.path()).with_timeout(Duration::from_secs(5));
        let start = Instant::now();
        let out = t
            .call(json!({"command": "sleep 30 & echo started"}))
            .await
            .unwrap();
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "{:?}",
            start.elapsed()
        );
        assert!(out.content.starts_with("started"));
    }
}
