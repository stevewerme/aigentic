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

/// The longest one command may run, whatever was asked; a per-call
/// `timeout_secs` and the project's `bash_timeout_secs` are both capped
/// at this.
pub const MAX_TIMEOUT_SECS: u64 = 900;

/// How long the process group gets after SIGTERM before it is SIGKILLed.
const TERM_GRACE: Duration = Duration::from_secs(2);

#[derive(Debug, Deserialize, JsonSchema)]
struct BashArgs {
    /// Command to run with `bash -c`. The working directory persists between
    /// calls, so `cd` takes effect for later commands.
    command: String,
    /// Wall-clock limit for this command in seconds, 1 to 900; anything
    /// larger is capped at 900. Omitted takes the project's default
    /// (`bash_timeout_secs` in aigentic.toml), else 120. Long builds and
    /// test suites should pass a larger value.
    timeout_secs: Option<u64>,
}

/// Run a shell command with a persistent working directory, a timeout and a
/// cap on captured output.
///
/// The shell runs in its own process group and the whole group is brought
/// down when the call ends, whether the command finished or timed out.
/// Nothing a command starts outlives the call.
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

    /// The call's wall-clock limit: its `timeout_secs`, capped at
    /// [`MAX_TIMEOUT_SECS`], else the tool's default (the project's
    /// `bash_timeout_secs`, else [`DEFAULT_TIMEOUT`]).
    fn effective_timeout(&self, asked: Option<u64>) -> Duration {
        asked.map_or(self.timeout, |secs| {
            Duration::from_secs(secs.clamp(1, MAX_TIMEOUT_SECS))
        })
    }

    async fn run(&self, command: &str, timeout: Duration) -> Result<ToolOutput, ToolError> {
        let cwd = self.workdir.current();
        // The shell reports its final directory so `cd` persists. It writes
        // a temp file in the same directory and renames it over the real
        // one, so a reader never sees a partial path. A command that `exit`s
        // early skips the report and the directory is left unchanged.
        let cwd_dir =
            tempfile::tempdir().map_err(|e| ToolError::Execution(format!("temp dir: {e}")))?;
        let cwd_file = cwd_dir.path().join("cwd");
        let script = format!(
            "{command}\n\
             __aigentic_status=$?\n\
             pwd > \"$AIGENTIC_CWD_FILE.tmp\" && mv -f \"$AIGENTIC_CWD_FILE.tmp\" \"$AIGENTIC_CWD_FILE\"\n\
             exit $__aigentic_status\n"
        );

        let mut cmd = Command::new("bash");
        cmd.arg("-c")
            .arg(&script)
            .current_dir(&cwd)
            .env("AIGENTIC_CWD_FILE", &cwd_file)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        group::configure(&mut cmd);
        let mut child = cmd
            .spawn()
            .map_err(|e| ToolError::Execution(format!("spawn bash: {e}")))?;
        let group = group::Group::of(&child);
        // An interrupt drops the call's future mid-run; the group is torn
        // down from `Drop` so it never outlives the call that way either.
        let mut kill_on_drop = group::KillOnDrop::arm(group);

        let mut stdout = capture(child.stdout.take(), self.output_cap);
        let mut stderr = capture(child.stderr.take(), self.output_cap);

        let status = match tokio::time::timeout(timeout, child.wait()).await {
            Ok(Ok(status)) => Some(status),
            Ok(Err(e)) => return Err(ToolError::Execution(format!("wait: {e}"))),
            Err(_) => None,
        };
        let timed_out = status.is_none();

        // Bring the whole group down: SIGTERM, up to TERM_GRACE for the shell
        // to exit and the pipes to close, then SIGKILL. Once the group is
        // dead every writer is gone, so the readers reach EOF.
        group.terminate(&mut child);
        let settled = tokio::time::timeout(TERM_GRACE, async {
            let _ = child.wait().await;
            let _ = (&mut stdout.task).await;
            let _ = (&mut stderr.task).await;
        })
        .await
        .is_ok();
        if !settled {
            group.kill(&mut child);
            let _ = child.wait().await;
            let _ = (&mut stdout.task).await;
            let _ = (&mut stderr.task).await;
        }
        // This path has torn the group down itself.
        kill_on_drop.disarm();

        if !timed_out && let Ok(reported) = std::fs::read_to_string(&cwd_file) {
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
                // Name the fix, so the model passes a larger value next
                // time instead of retrying the same 120 s death.
                content.push_str(&format!(
                    "[timed out after {} s; the command and everything it started were \
                     killed. Pass timeout_secs (up to {MAX_TIMEOUT_SECS}) for long builds \
                     or test suites]\n",
                    timeout.as_secs_f64()
                ));
                true
            }
        };
        Ok(ToolOutput { content, is_error })
    }
}

/// Process-group handling. On Unix the shell gets its own group so that
/// everything it starts can be signalled together.
#[cfg(unix)]
mod group {
    use nix::sys::signal::{Signal, killpg};
    use nix::unistd::Pid;
    use tokio::process::{Child, Command};

    pub(super) fn configure(cmd: &mut Command) {
        // pgid 0 means "a new group whose id is the child's pid".
        cmd.process_group(0);
    }

    #[derive(Debug, Clone, Copy)]
    pub(super) struct Group(Option<Pid>);

    impl Group {
        pub(super) fn of(child: &Child) -> Self {
            Self(child.id().map(|pid| Pid::from_raw(pid as i32)))
        }

        pub(super) fn terminate(self, _child: &mut Child) {
            if let Some(pgid) = self.0 {
                // ESRCH just means the group is already gone.
                let _ = killpg(pgid, Signal::SIGTERM);
            }
        }

        pub(super) fn kill(self, _child: &mut Child) {
            if let Some(pgid) = self.0 {
                let _ = killpg(pgid, Signal::SIGKILL);
            }
        }
    }

    /// The interrupt path: the runtime drops the call's future mid-run,
    /// so the group is torn down from `Drop`. SIGTERM now, SIGKILL after
    /// TERM_GRACE, the same two steps the timeout path takes; every path
    /// that tears the group down itself disarms the guard first.
    pub(super) struct KillOnDrop(Option<Pid>);

    impl KillOnDrop {
        pub(super) fn arm(group: Group) -> Self {
            Self(group.0)
        }

        pub(super) fn disarm(&mut self) {
            self.0 = None;
        }
    }

    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            let Some(pgid) = self.0.take() else { return };
            let _ = killpg(pgid, Signal::SIGTERM);
            // The grace needs a runtime to sleep in; without one, kill
            // at once rather than risk a survivor.
            match tokio::runtime::Handle::try_current() {
                Ok(handle) => {
                    handle.spawn(async move {
                        tokio::time::sleep(super::TERM_GRACE).await;
                        let _ = killpg(pgid, Signal::SIGKILL);
                    });
                }
                Err(_) => {
                    let _ = killpg(pgid, Signal::SIGKILL);
                }
            }
        }
    }
}

/// STUB for non-Unix platforms: no process groups, so only the shell itself
/// is killed and anything it started may outlive the call. Windows support
/// (job objects) is out of scope for phase 0.
#[cfg(not(unix))]
mod group {
    use tokio::process::{Child, Command};

    pub(super) fn configure(_cmd: &mut Command) {}

    #[derive(Debug, Clone, Copy)]
    pub(super) struct Group;

    impl Group {
        pub(super) fn of(_child: &Child) -> Self {
            Self
        }

        pub(super) fn terminate(self, child: &mut Child) {
            let _ = child.start_kill();
        }

        pub(super) fn kill(self, child: &mut Child) {
            let _ = child.start_kill();
        }
    }

    /// STUB: the shell itself is killed by `kill_on_drop` when the future
    /// is dropped; there is no group to signal.
    #[derive(Debug, Clone, Copy)]
    pub(super) struct KillOnDrop;

    impl KillOnDrop {
        pub(super) fn arm(_group: Group) -> Self {
            Self
        }

        pub(super) fn disarm(&mut self) {}
    }
}

struct Capture {
    buf: Arc<Mutex<BoundedCapture>>,
    task: tokio::task::JoinHandle<()>,
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
    Capture { buf, task }
}

impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Run a shell command with bash. The working directory persists across calls. \
         Output is capped; long output keeps its head and tail. Background processes \
         do not outlive the call. The call ends after `timeout_secs` seconds (default \
         120, at most 900); pass a larger value for long builds or test suites."
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
            let timeout = self.effective_timeout(args.timeout_secs);
            self.run(&args.command, timeout).await
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

    fn canon(p: impl AsRef<std::path::Path>) -> PathBuf {
        std::fs::canonicalize(p).unwrap()
    }

    /// True while a process with this pid exists (zombies included).
    #[cfg(unix)]
    fn alive(pid: i32) -> bool {
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
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
    async fn cd_tmp_updates_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let workdir = Workdir::new(dir.path());
        let t = BashTool::new(workdir.clone());
        let out = t.call(json!({"command": "cd /tmp && true"})).await.unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(canon(workdir.current()), canon("/tmp"));
    }

    #[tokio::test]
    async fn cd_then_timeout_leaves_cwd_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let workdir = Workdir::new(dir.path());
        let t = BashTool::new(workdir.clone()).with_timeout(Duration::from_millis(300));
        let out = t
            .call(json!({"command": "cd /tmp && sleep 60"}))
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("timed out"), "{}", out.content);
        assert_eq!(canon(workdir.current()), canon(dir.path()));
    }

    #[tokio::test]
    async fn times_out_with_partial_output() {
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
        // The message names the fix, not just the death.
        assert!(
            out.content.contains("Pass timeout_secs (up to 900)"),
            "{}",
            out.content
        );
        assert!(!out.content.contains("after\n"), "{}", out.content);
    }

    #[tokio::test]
    async fn timeout_secs_raises_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        // The tool's own default would kill this command.
        let t = tool(dir.path()).with_timeout(Duration::from_millis(300));
        let out = t
            .call(json!({"command": "sleep 1; echo done", "timeout_secs": 5}))
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(out.content, "done\n");

        // Without the argument the small default applies.
        let out = t
            .call(json!({"command": "sleep 1; echo done"}))
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(
            out.content.contains("timed out after 0.3 s"),
            "{}",
            out.content
        );
    }

    #[test]
    fn timeout_secs_is_capped_at_900() {
        let dir = tempfile::tempdir().unwrap();
        let t = tool(dir.path()).with_timeout(Duration::from_millis(300));
        assert_eq!(
            t.effective_timeout(Some(10_000)),
            Duration::from_secs(MAX_TIMEOUT_SECS)
        );
        // Zero is not a useful limit; one second is the floor.
        assert_eq!(t.effective_timeout(Some(0)), Duration::from_secs(1));
        // No argument: the project default the tool was built with.
        assert_eq!(t.effective_timeout(None), Duration::from_millis(300));
        let t = tool(dir.path());
        assert_eq!(t.effective_timeout(None), DEFAULT_TIMEOUT);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kills_backgrounded_grandchildren() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("sleep.pid");
        let t = tool(dir.path()).with_timeout(Duration::from_millis(300));
        let start = Instant::now();
        let out = t
            .call(json!({"command": format!(
                "sleep 60 & echo $! > {}; sleep 60",
                pid_file.display()
            )}))
            .await
            .unwrap();
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "{:?}",
            start.elapsed()
        );
        assert!(out.is_error);
        assert!(out.content.contains("timed out"), "{}", out.content);

        let pid: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // The orphan is reparented and reaped by init; allow a moment for that.
        let deadline = Instant::now() + Duration::from_secs(2);
        while alive(pid) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!alive(pid), "backgrounded sleep {pid} survived the timeout");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn backgrounded_child_does_not_outlive_a_finished_call() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("sleep.pid");
        let t = tool(dir.path()).with_timeout(Duration::from_secs(10));
        let start = Instant::now();
        let out = t
            .call(json!({"command": format!(
                "sleep 60 & echo $! > {}; echo started",
                pid_file.display()
            )}))
            .await
            .unwrap();
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "{:?}",
            start.elapsed()
        );
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.starts_with("started"), "{}", out.content);

        let pid: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while alive(pid) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!alive(pid), "backgrounded sleep {pid} survived the call");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn background_job_returns_promptly_with_all_output_and_no_survivor() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("sleep.pid");
        let t = tool(dir.path());
        let start = Instant::now();
        let out = t
            .call(json!({"command": format!(
                "echo start; sleep 60 & echo $! > {}; echo done",
                pid_file.display()
            )}))
            .await
            .unwrap();
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "{:?}",
            start.elapsed()
        );
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(out.content, "start\ndone\n");

        let pid: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while alive(pid) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!alive(pid), "backgrounded sleep {pid} survived the call");
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

    /// An interrupt drops the call's future mid-run (the runtime does);
    /// the group must be torn down from that path too.
    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_the_call_mid_run_kills_the_group() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("sleep.pid");
        let t = tool(dir.path()).with_timeout(Duration::from_secs(600));
        let mut fut = t.call(json!({"command": format!(
            "sleep 300 & echo $! > {}; sleep 300",
            pid_file.display()
        )}));
        let start = Instant::now();
        let out = tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(100)) => None,
            out = &mut fut => Some(out),
        };
        assert!(out.is_none(), "the call should still have been running");
        drop(fut);
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "{:?}",
            start.elapsed()
        );

        let pid: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while alive(pid) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!alive(pid), "backgrounded sleep {pid} survived the drop");
    }
}
