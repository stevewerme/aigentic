//! Holding the machine awake while a turn works (issue #47). macOS
//! idle-sleeps on the power assertion, not on CPU load, so a turn
//! waiting on a provider can sleep for minutes whatever the CPU does:
//! `pmset -g log` on #40 showed 12.5 of a build's 16 minutes asleep.
//!
//! One guard serves the whole daemon. Any working turn holds it; it is
//! released when no turn is working, including while a turn waits for a
//! person, which is not work and must not keep the machine up.

use std::ffi::OsStr;
use std::io;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[cfg(unix)]
use nix::sys::signal::{Signal, killpg};
#[cfg(unix)]
use nix::unistd::Pid;

/// A guard, shared by every thread's actor.
pub trait KeepAwake: Send + Sync {
    /// Take the assertion for one working turn. The first outstanding
    /// hold starts the process that holds it; further holds only count,
    /// so one program serves the daemon and three turns do not spawn
    /// three.
    fn hold(&self);
    /// Give one hold back. The last release stops the process and its
    /// group; a release with no hold outstanding does nothing.
    fn release(&self);
    /// `"on"` while the guard is doing its job, `"off"` when the config
    /// turned it off, else `"unavailable: <reason>"`. Read when a turn
    /// ends, so a guard that failed earlier in the run is reported by
    /// the turn it affected rather than frozen at startup.
    fn status(&self) -> String;
    /// The program doing the holding, when the machine has one (issue
    /// #47): the doctor names it, so `caffeinate` in `ps` is
    /// explainable. `None` when nothing is holding.
    fn program(&self) -> Option<&'static str> {
        None
    }
}

/// A status reader a `Runtime` can hold (issue #47 amendment 2), so
/// what the guard was doing at the end of a turn reaches the log.
pub fn reader(guard: &Arc<dyn KeepAwake>) -> Arc<dyn Fn() -> Option<String> + Send + Sync> {
    let guard = guard.clone();
    Arc::new(move || Some(guard.status()))
}

/// The real guard: a child process holding this machine's power
/// assertion, in its own process group so the whole group can be killed.
pub struct ProcessGuard {
    /// Spawns the child that holds the assertion. `None` when the guard
    /// never spawns — it is off, or this machine has no program for it
    /// — and then `inert` says why. Injected, so a test drives the
    /// lifecycle without `caffeinate` and can watch the child die.
    spawner: Option<Box<dyn Fn() -> io::Result<Child> + Send + Sync>>,
    inner: Mutex<Inner>,
    /// Outstanding holds, so the first one starts the child and the
    /// last one stops it.
    holds: AtomicUsize,
    /// The program doing the holding, when a real one was found: named
    /// by the doctor so a `caffeinate` in `ps` is explainable.
    program: Option<&'static str>,
}

#[derive(Default)]
struct Inner {
    /// The live child, while a hold is outstanding.
    child: Option<Child>,
    /// The status reported when there is no spawner: `off` or
    /// `unavailable: …`.
    inert: Option<String>,
    /// The first spawn failure. A guard that could not start is not
    /// retried, so one failure is one warning, and the reason names
    /// that failure rather than staying silent.
    failure: Option<String>,
    /// How many failures were recorded. Kept for the report channel
    /// phase 5 step 9 adds; a test asserts a failure warns exactly once.
    warnings: usize,
}

impl ProcessGuard {
    /// A guard that spawns something when held: the seam a test drives.
    pub fn with_spawner(spawner: Box<dyn Fn() -> io::Result<Child> + Send + Sync>) -> Self {
        Self {
            spawner: Some(spawner),
            inner: Mutex::new(Inner::default()),
            holds: AtomicUsize::new(0),
            program: None,
        }
    }

    /// The same guard, with the program named in its status line.
    pub fn with_program(mut self, program: &'static str) -> Self {
        self.program = Some(program);
        self
    }

    /// The guard installed when the config's `keep_awake` is `false`.
    pub fn off() -> Self {
        Self::inert("off".to_owned())
    }

    /// The guard installed when this machine has no way to hold the
    /// assertion, or when the config asks for one that is not there.
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self::inert(format!("unavailable: {}", reason.into()))
    }

    fn inert(status: String) -> Self {
        Self {
            // A guard that will never hold an assertion still gets a
            // spawner, and that spawner panics: if anything ever tries to
            // start a program for an off guard, a test says so instead of
            // the noise passing silently (issue #47, T5).
            spawner: Some(Box::new(|| panic!("an off guard must never spawn"))),
            inner: Mutex::new(Inner {
                inert: Some(status),
                ..Inner::default()
            }),
            holds: AtomicUsize::new(0),
            program: None,
        }
    }

    /// How many times the guard failed to start. Zero or one: a failed
    /// guard is not retried.
    pub fn warnings(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .warnings
    }

    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl KeepAwake for ProcessGuard {
    fn hold(&self) {
        if self.holds.fetch_add(1, Ordering::SeqCst) > 0 {
            // Someone is already holding the assertion.
            return;
        }
        let Some(spawner) = &self.spawner else {
            return;
        };
        let mut inner = self.inner();
        if inner.child.is_some() || inner.inert.is_some() || inner.failure.is_some() {
            return;
        }
        match spawner() {
            Ok(child) => inner.child = Some(child),
            Err(e) => {
                inner.failure = Some(format!("could not start the keep-awake guard: {e}"));
                inner.warnings += 1;
            }
        }
    }

    fn release(&self) {
        let previous = self
            .holds
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1));
        if !matches!(previous, Ok(1)) {
            // No hold to give back, or other turns are still working.
            return;
        }
        let child = self.inner().child.take();
        if let Some(mut child) = child {
            stop(&mut child);
        }
    }

    fn status(&self) -> String {
        let inner = self.inner();
        if let Some(failure) = &inner.failure {
            return format!("unavailable: {failure}");
        }
        if let Some(inert) = &inner.inert {
            return inert.clone();
        }
        "on".to_owned()
    }

    fn program(&self) -> Option<&'static str> {
        self.program
    }
}

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        // Amendment 1: a daemon that exits mid-turn must not leave the
        // child holding the assertion, so the group goes with the guard.
        let child = self
            .inner
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .child
            .take();
        if let Some(mut child) = child {
            stop(&mut child);
        }
    }
}

/// `detect` with the search path given, so a test needs no global
/// environment and no real `caffeinate`.
pub fn detect_with(path: Option<&OsStr>, keep_awake: bool) -> Arc<dyn KeepAwake> {
    if !keep_awake {
        return Arc::new(ProcessGuard::off());
    }
    let program = if cfg!(target_os = "macos") {
        "caffeinate"
    } else if cfg!(target_os = "linux") {
        "systemd-inhibit"
    } else {
        return Arc::new(ProcessGuard::unavailable(
            "no keep-awake program on this machine",
        ));
    };
    if !on_path_in(path, program) {
        // Nothing to spawn, so nothing to spawn on the first hold
        // either: the failure is knowable now and reported every turn.
        return Arc::new(ProcessGuard::unavailable(format!("no {program} on PATH")));
    }
    Arc::new(
        ProcessGuard::with_spawner(Box::new(spawner(program)))
            .with_program(program_static(program)),
    )
}

/// The name the guard reports, as the `'static` one the trait hands the
/// doctor: the two are fixed literals, so this is a match and not an
/// allocation.
fn program_static(program: &str) -> &'static str {
    if program == "caffeinate" {
        "caffeinate"
    } else {
        "systemd-inhibit"
    }
}

/// The guard this machine gets: `off` when the config says so, else the
/// program that holds its power assertion.
pub fn detect(keep_awake: bool) -> Arc<dyn KeepAwake> {
    detect_with(std::env::var_os("PATH").as_deref(), keep_awake)
}

/// Is `program` in one of `path`'s directories? A plain `is_file` test:
/// the daemon has no shell to ask.
pub fn on_path_in(path: Option<&OsStr>, program: &str) -> bool {
    path.is_some_and(|paths| std::env::split_paths(paths).any(|dir| dir.join(program).is_file()))
}

/// `on_path_in` against the process's own `PATH`.
pub fn on_path(program: &str) -> bool {
    on_path_in(std::env::var_os("PATH").as_deref(), program)
}

/// How the child is built: it must be in its own process group, so the
/// group can be killed and no descendant survives the release; and it
/// must never touch the daemon's own standard streams.
fn command(program: &str, args: &[String]) -> Command {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd
}

/// The spawner for `program`: one child, started when the first turn
/// holds the guard and killed when the last one lets go.
fn spawner(program: &str) -> impl Fn() -> io::Result<Child> + Send + Sync {
    let program = program.to_owned();
    move || {
        if cfg!(target_os = "macos") {
            // `-i` is the assertion `pmset -g assertions` lists as
            // `PreventUserIdleSystemSleep`; `-w <pid>` ties its life to
            // this daemon's, so even a killed daemon holds nothing.
            command(
                &program,
                &["-i".into(), "-w".into(), std::process::id().to_string()],
            )
            .spawn()
        } else {
            // systemd needs a command whose life is the inhibitor's:
            // the same pid watch, in a shell loop.
            let watch = format!(
                "while kill -0 {} 2>/dev/null; do sleep 5; done",
                std::process::id()
            );
            command(
                &program,
                &[
                    "--what=idle:sleep".into(),
                    "--why=aigentic turn".into(),
                    "sh".into(),
                    "-c".into(),
                    watch,
                ],
            )
            .spawn()
        }
    }
}

/// Stop the child and everything in its group. SIGTERM first, so a
/// guard that wants to clean up can; then SIGKILL without waiting long,
/// because a survivor means the machine never idle-sleeps again.
#[cfg(unix)]
fn stop(child: &mut Child) {
    // The child was spawned with `process_group(0)`, so its pid is its
    // group's, and `killpg` reaches the whole group — the `sleep 30`
    // the shell started as well as the shell itself.
    let group = group_of(child).map(Pid::from_raw);
    if let Some(group) = group {
        let _ = killpg(group, Signal::SIGTERM);
    }
    for _ in 0..20 {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => break,
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
    if let Some(group) = group {
        let _ = killpg(group, Signal::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// A group is gone when a signal to it finds nothing to signal — the
/// check a test makes after a release (issue #47).
#[cfg(unix)]
pub fn group_gone(pid: i32) -> bool {
    use nix::errno::Errno;
    matches!(killpg(Pid::from_raw(pid), None), Err(Errno::ESRCH))
}

#[cfg(not(unix))]
fn stop(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// The child's process group: it was spawned with `process_group(0)`,
/// so its pid is its group's id.
#[cfg(unix)]
fn group_of(child: &Child) -> Option<i32> {
    child.id().try_into().ok()
}

#[cfg(not(unix))]
fn group_of(_: &Child) -> Option<i32> {
    None
}
