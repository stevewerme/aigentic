//! Issue #91: the front-thread lock across processes. Two `aigentic`
//! processes started together on one threads directory must make one
//! front thread, not two (issue #88's `FrontLock`). Threads in one test
//! process cannot race at the dangerous point, so this test runs itself
//! again as two child processes: each embeds a daemon the way plain
//! `aigentic` does, waits at a shared barrier file, and sends `Front`.
//! Nothing here touches a real threads directory.

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aigentic_api::client::{Addr, Client};
use aigentic_api::{FrontOutcome, Request, Response};
use aigentic_runtime::aigentic_core::{
    Capabilities, CompletionRequest, Message, Provider, ProviderEvent,
};
use aigentic_server::build::{BuildError, ProviderFactory};
use aigentic_server::config::Config;
use aigentic_server::{NoReports, Server};
use futures_core::Stream;

/// Set on a child: the shared test directory.
const CHILD_DIR: &str = "AIGENTIC_FRONT_RACE_DIR";
/// The rounds the parent runs. Without the lock the first already fails
/// (two ids, both `first`); three guard against a lucky interleaving.
const ROUNDS: usize = 3;
/// What a child prints before its result, for the parent to find.
const RESULT: &str = "FRONT-RACE-RESULT";

struct Quiet;

impl Provider for Quiet {
    fn complete(
        &self,
        _: &CompletionRequest<'_>,
    ) -> Pin<Box<dyn Stream<Item = ProviderEvent> + Send + '_>> {
        Box::pin(futures_util::stream::iter(vec![ProviderEvent::Done {
            finish_reason: "stop".into(),
        }]))
    }
    fn count_tokens(&self, _: &[Message]) -> u64 {
        1
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supports_tools: false,
            supports_images: false,
            supports_caching: false,
            supports_structured_output: false,
            max_context_tokens: 1000,
        }
    }
}

struct QuietFactory;

impl ProviderFactory for QuietFactory {
    fn build(&self, _: &str) -> Result<(Box<dyn Provider>, String), BuildError> {
        Ok((Box::new(Quiet), "scripted".into()))
    }
}

/// The project folder both children start in.
fn project_root(dir: &Path) -> PathBuf {
    dir.join("p")
}

/// The barrier both children wait at; the parent creates it.
fn barrier(dir: &Path) -> PathBuf {
    dir.join("go")
}

/// The logs in `base` whose first line marks a front thread.
fn front_logs(base: &Path) -> usize {
    std::fs::read_dir(base)
        .map(|d| {
            d.filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("jsonl"))
                .filter(|p| {
                    std::fs::read_to_string(p)
                        .unwrap_or_default()
                        .lines()
                        .next()
                        .is_some_and(|line| line.contains("\"front\":true"))
                })
                .count()
        })
        .unwrap_or(0)
}

/// The child's half: a no-op in a normal run. With `CHILD_DIR` set it
/// embeds a daemon over the shared directory, waits for the barrier,
/// sends `Front` and prints the answer for the parent.
#[tokio::test]
async fn front_race_child() {
    let Ok(dir) = std::env::var(CHILD_DIR) else {
        return;
    };
    let dir = PathBuf::from(dir);
    let cfg_dir = dir.join("cfg");
    let config = Config::parse(&format!(
        "threads_dir = {:?}\n[profiles.a]\nbase_url = \"u\"\nmodel = \"m\"\napi_key_env = \"K\"\n",
        dir.join("threads").display()
    ))
    .unwrap();
    let embedded = Server::embed_with(
        config,
        cfg_dir,
        project_root(&dir),
        "steve",
        None,
        Arc::new(QuietFactory),
        Arc::new(NoReports),
    )
    .await
    .unwrap();
    let (client, _) = Client::connect(&Addr::Unix(embedded.socket.clone()), &embedded.token)
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !barrier(&dir).exists() {
        assert!(Instant::now() < deadline, "the barrier never opened");
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let reply = client
        .request(Request::Front {
            project: embedded.project.clone(),
            here: None,
        })
        .await
        .unwrap();
    let Response::Front { thread, outcome } = reply else {
        panic!("not a front reply: {reply:?}");
    };
    let outcome = match outcome {
        FrontOutcome::First => "first",
        FrontOutcome::Resumed => "resumed",
        FrontOutcome::Replaced { .. } => "replaced",
    };
    println!("{RESULT} {} {outcome}", thread.id);
}

/// The parent's half: two child processes on one threads directory make
/// one front thread, round after round.
#[test]
fn two_processes_make_one_front_thread() {
    if std::env::var(CHILD_DIR).is_ok() {
        return;
    }
    let exe = std::env::current_exe().unwrap();
    for round in 0..ROUNDS {
        let dir = tempfile::tempdir().unwrap();
        let root = project_root(dir.path());
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("aigentic.toml"), "[project]\nname = \"p\"\n").unwrap();
        std::fs::create_dir_all(dir.path().join("cfg")).unwrap();

        let children: Vec<_> = (0..2)
            .map(|_| {
                Command::new(&exe)
                    .args([
                        "front_race_child",
                        "--exact",
                        "--nocapture",
                        "--test-threads=1",
                    ])
                    .env(CHILD_DIR, dir.path())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap()
            })
            .collect();
        // Both children have their daemons up and wait at the barrier
        // well within this; then both go at once.
        std::thread::sleep(Duration::from_millis(1500));
        std::fs::write(barrier(dir.path()), "").unwrap();

        let mut results: Vec<(String, String)> = Vec::new();
        for child in children {
            let out = child.wait_with_output().unwrap();
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(
                out.status.success(),
                "round {round}: a child failed: {stdout}\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
            let line = stdout
                .lines()
                // The harness prints `test … ... ` before it on the same line.
                .find_map(|l| l.split_once(RESULT).map(|(_, rest)| rest))
                .unwrap_or_else(|| panic!("round {round}: no result in {stdout}"));
            let mut parts = line.split_whitespace();
            results.push((
                parts.next().unwrap().to_owned(),
                parts.next().unwrap().to_owned(),
            ));
        }
        assert_eq!(
            results[0].0, results[1].0,
            "round {round}: one front thread: {results:?}"
        );
        let mut outcomes: Vec<&str> = results.iter().map(|(_, o)| o.as_str()).collect();
        outcomes.sort_unstable();
        assert_eq!(outcomes, ["first", "resumed"], "round {round}: {results:?}");
        assert_eq!(
            front_logs(&dir.path().join("threads")),
            1,
            "round {round}: exactly one front log"
        );
    }
}
