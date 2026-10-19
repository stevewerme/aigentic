//! `tools/pty/drive.py` is how a step checks that the tui still draws what it
//! should. These tests pin what makes it safe to run unattended: the child's
//! cwd, HOME and XDG paths are inside a scratch tree, the config is linked and
//! left alone, a config or a directory it must not reach is refused before
//! anything exists, and a failing step names itself on one line.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use tempfile::TempDir;

const DRIVE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tools/pty/drive.py");
const SH: &str = "/bin/sh";
const SLEEP: &str = "/bin/sleep";
const WAIT: &str = "5";
const EXIT_TIME: &str = "1";
const MARKER: &str = "pty-driver-marker";
const CHILD_EXIT: i32 = 3;

/// The config every run is given. One line, and no `threads_dir`.
fn write_config(root: &Path) -> PathBuf {
    let path = root.join("config.toml");
    fs::write(&path, "default_profile = \"flash\"\n").expect("write the config");
    path
}

/// The driver with the arguments every test shares; a test adds its own after.
fn driver(python_flags: &[&str], root: &Path, config: &Path, data: &Path) -> Command {
    let mut command = Command::new("python3");
    command
        .args(python_flags)
        .arg(DRIVE)
        .arg("--config")
        .arg(config)
        .arg("--data")
        .arg(data)
        .arg("--out")
        .arg(root.join("out"))
        .arg("--wait")
        .arg(WAIT)
        .arg("--exit-time")
        .arg(EXIT_TIME);
    command
}

/// Run the driver on `root`'s step file, keeping the dumps in `<root>/out`.
fn drive(root: &Path, config: &Path, steps: &str, data: &Path, args: &[&str]) -> Output {
    let script = root.join("steps.txt");
    fs::write(&script, steps).expect("write the steps");
    driver(&[], root, config, data)
        .arg("--script")
        .arg(&script)
        .args(args)
        .output()
        .expect("python3 runs tools/pty/drive.py")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn code(output: &Output) -> i32 {
    output.status.code().expect("the driver exited by itself")
}

/// The `drive.py <key>=<value>` lines the driver prints before the child runs.
fn facts(output: &Output) -> Vec<(String, String)> {
    stdout(output)
        .lines()
        .filter_map(|line| line.strip_prefix("drive.py "))
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

fn fact<'a>(facts: &'a [(String, String)], key: &str) -> &'a str {
    facts
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.as_str())
        .unwrap_or_else(|| panic!("no drive.py {key}=<value> line: {facts:?}"))
}

/// A refusal: exit 2, one prefixed line on stderr, naming what was refused.
fn refused(output: &Output, needle: &str) {
    let stderr = stderr(output);
    assert_eq!(code(output), 2, "a refusal exits 2, stderr was {stderr:?}");
    assert_eq!(stderr.lines().count(), 1, "one line says it: {stderr:?}");
    assert!(
        stderr.starts_with("drive.py: "),
        "the line is prefixed: {stderr:?}"
    );
    assert!(
        stderr.contains(needle),
        "the line names {needle:?}: {stderr:?}"
    );
    assert!(
        !stdout(output).contains("drive.py "),
        "a refusal starts nothing, so there is no header: {}",
        stdout(output)
    );
}

/// The value of `<prefix><absolute path>` on the screen. The typed command line
/// carries the variables themselves, so only an absolute value counts.
fn value_of(screen: &str, prefix: &str) -> String {
    for line in screen.lines() {
        for token in line.split_whitespace() {
            if let Some(value) = token.strip_prefix(prefix)
                && value.starts_with('/')
            {
                return value.to_string();
            }
        }
    }
    panic!("no {prefix}<absolute path> on the screen:\n{screen}");
}

fn scratch_of(output: &Output) -> PathBuf {
    PathBuf::from(fact(&facts(output), "scratch"))
}

/// The absolute path of `python3` on PATH: `--bin` takes no bare name, so a
/// step cannot pick up another binary by accident.
fn python3() -> String {
    let path = env::var_os("PATH").expect("PATH");
    for dir in env::split_paths(&path) {
        let candidate = dir.join("python3");
        if candidate.is_file() {
            return candidate.display().to_string();
        }
    }
    panic!("python3 on PATH");
}

/// Whether the interpreter can render a screen, which the wait: and dump: steps
/// need.
fn pyte_installed() -> bool {
    Command::new("python3")
        .arg("-c")
        .arg("import pyte")
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[test]
fn the_scratch_tree_holds_every_path_the_child_writes() {
    let root = TempDir::new().expect("a temp dir");
    let config = write_config(root.path());
    // One path per line, on a wide screen: a pty wraps at its width, and a
    // wrapped path would read back as two broken tokens.
    let steps = "send: pwd; echo H=$HOME; echo D=$XDG_DATA_HOME; echo C=$XDG_CONFIG_HOME; echo T=$TMPDIR\nsleep: 1\nsend: exit 0\n";

    let output = drive(
        root.path(),
        &config,
        steps,
        root.path(),
        &["--bin", SH, "--cols", "400"],
    );
    assert_eq!(
        code(&output),
        0,
        "stdout:\n{}\nstderr:\n{}",
        stdout(&output),
        stderr(&output)
    );

    let scratch = scratch_of(&output);
    assert!(
        scratch.starts_with(
            env::temp_dir()
                .canonicalize()
                .expect("a canonical temp dir")
        ),
        "the scratch tree is under the temp dir: {scratch:?}"
    );

    let screen = fs::read_to_string(root.path().join("out/final.txt")).expect("the final dump");
    assert_eq!(
        PathBuf::from(value_of(&screen, "")),
        scratch.join("cwd"),
        "the child starts in the scratch cwd:\n{screen}"
    );
    let mut seen = Vec::new();
    for name in ["H=", "D=", "C="] {
        let path = PathBuf::from(value_of(&screen, name));
        assert!(
            path.starts_with(&scratch),
            "{name}{} is inside the scratch tree:\n{screen}",
            path.display()
        );
        assert!(
            !seen.contains(&path),
            "{name}{} is its own directory:\n{screen}",
            path.display()
        );
        seen.push(path);
    }

    // The daemon's socket path is capped, so TMPDIR is the scratch tree only
    // when a socket fits there and a directory of its own under /tmp otherwise.
    let temp = PathBuf::from(fact(&facts(&output), "tmp"));
    assert_eq!(
        PathBuf::from(value_of(&screen, "T=")),
        temp,
        "the child's TMPDIR is the one the driver made:\n{screen}"
    );
    let short_root = Path::new("/tmp").canonicalize().expect("a canonical /tmp");
    assert!(
        temp.starts_with(&scratch) || temp.parent() == Some(short_root.as_path()),
        "TMPDIR is the scratch tree or a directory of its own under /tmp: {temp:?}"
    );
    assert!(
        !scratch.exists() && !temp.exists(),
        "the run removed what it made: {scratch:?} and {temp:?}"
    );
}

#[test]
fn the_config_is_linked_into_scratch_and_unchanged() {
    let root = TempDir::new().expect("a temp dir");
    let config = write_config(root.path());
    let written = fs::read(&config).expect("read the config the test wrote");

    let output = drive(
        root.path(),
        &config,
        "send: exit 0\n",
        root.path(),
        &["--bin", SH, "--keep"],
    );
    assert_eq!(code(&output), 0, "stderr:\n{}", stderr(&output));

    let link = scratch_of(&output).join("config/aigentic/config.toml");
    let metadata = fs::symlink_metadata(&link).expect("the linked config");
    assert!(
        metadata.file_type().is_symlink(),
        "the config is a link, not a copy: {link:?}"
    );
    assert_eq!(
        fs::read_link(&link).expect("read the link"),
        config,
        "the link points at the config the test wrote"
    );
    assert_eq!(
        fs::read(&config).expect("read the config again"),
        written,
        "the run left the config byte for byte as it was"
    );

    // The run kept its tree so the link could be read; the test then removes
    // what it kept, the child's temp dir included when it is outside the tree.
    let facts = facts(&output);
    let _ = fs::remove_dir_all(fact(&facts, "tmp"));
    fs::remove_dir_all(scratch_of(&output)).expect("remove the kept scratch tree");
}

#[test]
fn a_config_naming_threads_dir_is_refused() {
    let root = TempDir::new().expect("a temp dir");
    let config = root.path().join("config.toml");
    let elsewhere = root.path().join("elsewhere");
    fs::write(
        &config,
        format!(
            "default_profile = \"flash\"\nthreads_dir = \"{}\"\n",
            elsewhere.display()
        ),
    )
    .expect("write a config with a threads_dir");

    let output = drive(
        root.path(),
        &config,
        "send: exit 0\n",
        root.path(),
        &["--bin", SH],
    );
    refused(&output, "threads_dir");
    assert!(
        !elsewhere.exists(),
        "nothing was created at the refused path {elsewhere:?}"
    );
}

#[test]
fn a_data_dir_outside_the_temp_dir_is_refused() {
    let root = TempDir::new().expect("a temp dir");
    let config = write_config(root.path());
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
    let before = fs::read_dir(repo).expect("list the manifest dir").count();

    let output = drive(root.path(), &config, "send: exit 0\n", repo, &["--bin", SH]);
    refused(&output, "--data must be");
    assert_eq!(
        fs::read_dir(repo)
            .expect("list the manifest dir again")
            .count(),
        before,
        "the refused run made nothing in {repo:?}"
    );
}

#[test]
fn bad_inputs_are_refused_with_one_line() {
    let root = TempDir::new().expect("a temp dir");
    let config = write_config(root.path());

    let binary = root.path().join("nope");
    let output = drive(
        root.path(),
        &config,
        "send: exit 0\n",
        root.path(),
        &["--bin", binary.to_str().expect("a utf-8 path")],
    );
    refused(&output, "cargo build");

    let missing = root.path().join("nope.toml");
    let output = drive(
        root.path(),
        &missing,
        "send: exit 0\n",
        root.path(),
        &["--bin", SH],
    );
    refused(&output, "--config");

    let output = drive(
        root.path(),
        &config,
        "fly: x\n",
        root.path(),
        &["--bin", SH],
    );
    refused(&output, "'fly'");
    assert!(
        stderr(&output).contains("line 1"),
        "the line number says where: {}",
        stderr(&output)
    );
}

#[test]
fn a_wait_step_without_pyte_names_the_install_line() {
    let root = TempDir::new().expect("a temp dir");
    let config = write_config(root.path());
    let script = root.path().join("steps.txt");
    fs::write(&script, "wait: x\n").expect("write a wait: step");

    // -S drops site-packages, so pyte cannot be imported whatever the machine
    // has installed.
    let output = driver(&["-S"], root.path(), &config, root.path())
        .arg("--bin")
        .arg(SH)
        .arg("--script")
        .arg(&script)
        .output()
        .expect("python3 runs the driver");
    refused(&output, "pyte");
}

#[test]
fn a_wait_step_matches_the_screen_and_dumps_it() {
    if !pyte_installed() {
        eprintln!("pyte is not installed: python3 -m pip install --user pyte");
        return;
    }
    let root = TempDir::new().expect("a temp dir");
    let config = write_config(root.path());
    let steps = format!("send: echo {MARKER}\nwait: {MARKER}\nsend: exit 0\ndump: first\n");

    let output = drive(root.path(), &config, &steps, root.path(), &["--bin", SH]);
    assert_eq!(
        code(&output),
        0,
        "stdout:\n{}\nstderr:\n{}",
        stdout(&output),
        stderr(&output)
    );

    let text = fs::read_to_string(root.path().join("out/first.txt")).expect("first.txt");
    assert!(text.contains(MARKER), "the dump's text:\n{text}");
    let raw = fs::read(root.path().join("out/first.raw")).expect("first.raw");
    assert!(
        String::from_utf8_lossy(&raw).contains(MARKER),
        "the dump's bytes hold the marker"
    );
}

#[test]
fn a_child_that_does_not_exit_fails_the_run() {
    let root = TempDir::new().expect("a temp dir");
    let config = write_config(root.path());

    let started = Instant::now();
    let output = drive(
        root.path(),
        &config,
        "",
        root.path(),
        &["--bin", SLEEP, "--", "30"],
    );
    let waited = started.elapsed();

    assert_eq!(code(&output), 1, "stderr:\n{}", stderr(&output));
    assert!(
        stderr(&output).contains("child did not exit"),
        "the line says why: {}",
        stderr(&output)
    );
    assert!(
        waited < Duration::from_secs(15),
        "the child is cut off after --exit-time, the run took {waited:?}"
    );
}

#[test]
fn a_childs_non_zero_exit_fails_the_run() {
    let root = TempDir::new().expect("a temp dir");
    let config = write_config(root.path());
    let steps = format!("send: exit {CHILD_EXIT}\n");

    let output = drive(root.path(), &config, &steps, root.path(), &["--bin", SH]);
    assert_eq!(code(&output), 1, "stdout:\n{}", stdout(&output));
    assert!(
        stderr(&output).contains(&format!("child exited {CHILD_EXIT}")),
        "the line names the child's code: {}",
        stderr(&output)
    );
}

#[test]
fn a_cwd_outside_the_temp_dir_is_refused() {
    let root = TempDir::new().expect("a temp dir");
    let config = write_config(root.path());
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
    let before = fs::read_dir(repo).expect("list the manifest dir").count();

    let output = drive(
        root.path(),
        &config,
        "send: exit 0\n",
        root.path(),
        &["--bin", SH, "--cwd", env!("CARGO_MANIFEST_DIR")],
    );
    refused(&output, "--cwd must be");
    assert_eq!(
        fs::read_dir(repo)
            .expect("list the manifest dir again")
            .count(),
        before,
        "the refused run made nothing in {repo:?}"
    );
}

/// A config that names its own key variable, so a run of the tui needs no real
/// key and can reach no model: the base URL is a closed port.
fn write_tui_config(root: &Path, key_var: &str) -> PathBuf {
    let path = root.join("config.toml");
    fs::write(
        &path,
        format!(
            "default_profile = \"pty\"\n\n[profiles.pty]\nbase_url = \"http://127.0.0.1:9\"\nmodel = \"pty-check\"\napi_key_env = \"{key_var}\"\n"
        ),
    )
    .expect("write a config with a profile of its own");
    path
}

/// The child that asks the terminal for its cursor position and prints the reply.
const ASK_CURSOR: &str = "import sys, tty\ntty.setraw(0)\nsys.stdout.write('\\x1b[6n')\nsys.stdout.flush()\nreply = b''\nwhile not reply.endswith(b'R'):\n    chunk = sys.stdin.buffer.read(1)\n    if not chunk:\n        break\n    reply += chunk\nsys.stdout.write('\\r\\nreply=%s\\r\\n' % reply.decode('utf-8', 'replace').replace('\\x1b', 'ESC'))\nsys.stdout.flush()\n";

#[test]
fn the_driver_answers_the_cursor_position_query() {
    if !pyte_installed() {
        eprintln!("pyte is not installed: python3 -m pip install --user pyte");
        return;
    }
    let root = TempDir::new().expect("a temp dir");
    let config = write_config(root.path());
    let steps = "wait: reply=ESC\\[\\d+;\\d+R\ndump: reply\n";

    // A program that asks a real terminal where the cursor is gets a reply; a
    // tui that gets none gives up waiting and exits with an error.
    let output = drive(
        root.path(),
        &config,
        steps,
        root.path(),
        &["--bin", &python3(), "--", "-c", ASK_CURSOR],
    );
    assert_eq!(
        code(&output),
        0,
        "stdout:\n{}\nstderr:\n{}",
        stdout(&output),
        stderr(&output)
    );

    let text = fs::read_to_string(root.path().join("out/reply.txt")).expect("reply.txt");
    assert!(
        text.contains("reply=ESC[1;1R"),
        "the reply is a cursor position, from the top left of a fresh screen:\n{text}"
    );
}

#[test]
fn the_driver_starts_the_tui_and_ends_on_ctrl_d() {
    if !pyte_installed() {
        eprintln!("pyte is not installed: python3 -m pip install --user pyte");
        return;
    }
    let root = TempDir::new().expect("a temp dir");
    let key_var = "AIGENTIC_PTY_TEST_KEY";
    let config = write_tui_config(root.path(), key_var);
    let steps = "wait: aigentic [0-9]\ndump: chrome\nkey: C-d\n";
    let script = root.path().join("steps.txt");
    fs::write(&script, steps).expect("write the steps");

    let output = driver(&[], root.path(), &config, root.path())
        .arg("--script")
        .arg(&script)
        .arg("--bin")
        .arg(env!("CARGO_BIN_EXE_aigentic"))
        .args(["--wait", "20", "--exit-time", "10"])
        .env(key_var, "placeholder-not-a-key")
        .output()
        .expect("python3 runs tools/pty/drive.py");
    assert_eq!(
        code(&output),
        0,
        "stdout:\n{}\nstderr:\n{}",
        stdout(&output),
        stderr(&output)
    );

    let text = fs::read_to_string(root.path().join("out/chrome.txt")).expect("chrome.txt");
    assert!(
        text.contains("aigentic "),
        "the chrome is on the screen:\n{text}"
    );
    assert!(
        !text.contains("Error:"),
        "the tui started without a terminal error:\n{text}"
    );
}
