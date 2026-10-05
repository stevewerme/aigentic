//! The test rig (issue #89): a config, a project folder and an open
//! thread, shared by the engine's and `front.rs`'s tests. Moved here
//! from the engine's tests unchanged; the assertions stayed put.

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use aigentic_api::client::{Addr, Client};
use aigentic_api::{Request, Response, ThreadState};
use aigentic_runtime::aigentic_core::{
    Capabilities, CompletionRequest, Message, Provider, ProviderEvent,
};
use aigentic_server::Listener;
use aigentic_server::build::{BuildError, ProviderFactory};
use aigentic_server::config::{ProjectConfig, ServerConfig, UserConfig};
use aigentic_server::{NoReports, Server};
use futures_core::Stream;
use ulid::Ulid;

use crate::config::Config;

/// A config with a scripted profile, threads and skills under `dir`.
pub(crate) fn config(dir: &std::path::Path) -> Config {
    Config::parse(&format!(
        "default_profile = \"a\"\nthreads_dir = {:?}\nbundled_dir = {:?}\n[profiles.a]\nbase_url = \"u\"\nmodel = \"m\"\napi_key_env = \"K\"\n[profiles.b]\nbase_url = \"u\"\nmodel = \"m\"\napi_key_env = \"K\"\n",
        dir.join("threads").display(),
        dir.display()
    ))
    .unwrap()
}

pub(crate) fn project(dir: &std::path::Path, name: &str, participants: &str) -> std::path::PathBuf {
    let root = dir.join(name);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("aigentic.toml"),
        format!("[project]\nname = \"{name}\"\n{participants}"),
    )
    .unwrap();
    root
}

/// Create a thread in `project`, open it, and hand back what a REPL
/// needs.
pub(crate) async fn open(
    client: &Client,
    project: &str,
    thread: Option<Ulid>,
) -> (Ulid, ThreadState, String) {
    let id = match thread {
        Some(id) => id,
        None => {
            let Response::Thread { thread } = client
                .request(Request::CreateThread {
                    project: project.into(),
                })
                .await
                .unwrap()
            else {
                panic!()
            };
            thread.id
        }
    };
    let Response::Opened { state, mode, .. } = client
        .request(Request::Open {
            thread: id,
            from_seq: 0,
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    (id, state, mode)
}
/// A provider that is never called: the tests that use [`daemon`] pick,
/// open and move threads, they never run a turn.
pub(crate) struct Silent;

impl Provider for Silent {
    fn complete(
        &self,
        _: &CompletionRequest<'_>,
    ) -> Pin<Box<dyn Stream<Item = ProviderEvent> + Send + '_>> {
        Box::pin(futures_util::stream::empty())
    }
    fn count_tokens(&self, _: &[Message]) -> u64 {
        0
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

struct SilentFactory;

impl ProviderFactory for SilentFactory {
    fn build(&self, _: &str) -> Result<(Box<dyn Provider>, String), BuildError> {
        Ok((Box::new(Silent), "silent".into()))
    }
}

/// A daemon with the users and projects a test names, serving on a
/// socket under `dir`. `embed_with` gives one user and one project; the
/// front thread needs two of either (a folder in `b` resuming a thread
/// in `a`, a read-only user).
pub(crate) async fn daemon(
    dir: &std::path::Path,
    users: &[(&str, &str)],
    projects: &[(&str, PathBuf)],
) -> Addr {
    let cfg_dir = dir.join("cfg");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    let socket = dir.join("s.sock");
    let server = Arc::new(Server::new(
        config(dir),
        cfg_dir,
        ServerConfig {
            listen: "unix".to_owned(),
            idle_unload_secs: 24 * 3600,
            resume_runs: false,
            users: users
                .iter()
                .map(|(name, token)| UserConfig {
                    name: (*name).into(),
                    token_env: None,
                    token: Some((*token).into()),
                })
                .collect(),
            projects: projects
                .iter()
                .map(|(name, root)| ProjectConfig {
                    name: (*name).into(),
                    root: root.clone(),
                })
                .collect(),
        },
        Arc::new(SilentFactory),
        Arc::new(NoReports),
    ));
    tokio::spawn(server.clone().serve(Listener::Unix(socket.clone())));
    for _ in 0..200 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    Addr::Unix(socket)
}
