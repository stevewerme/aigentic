//! A thread moves between projects (phase 6 step 10): a thread started in
//! project `p`, inside workspace `w`, moves to `q`, outside it. After the
//! move the model sees q's instructions and not p's or w's, the note
//! about the move, and the earlier transcript; tools run in q's root; the
//! log records `project_switched`; unloaded and reopened, the thread is
//! rebuilt in q. A switch to an unknown project is refused.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aigentic_api::client::{Addr, Client};
use aigentic_api::{Notice, Request, Response, ThreadState};
use aigentic_runtime::aigentic_core::{
    Capabilities, CompletionRequest, ContentBlock, EventKind, Message, Provider, ProviderEvent,
    ToolCall,
};
use aigentic_runtime::aigentic_log::ThreadLog;
use aigentic_server::build::{BuildError, ProviderFactory};
use aigentic_server::config::{Config, ServerConfig, UserConfig};
use aigentic_server::{Listener, NoReports, Server};
use futures_core::Stream;
use serde_json::json;
use tokio::sync::mpsc;

type Seen = Arc<Mutex<Vec<Vec<Message>>>>;

/// One script shared by every provider the factory builds, so a switch
/// (which builds a new provider) keeps reading the same script.
struct Scripted {
    script: Arc<Mutex<VecDeque<Vec<ProviderEvent>>>>,
    seen: Seen,
}

impl Provider for Scripted {
    fn complete(
        &self,
        request: &CompletionRequest<'_>,
    ) -> Pin<Box<dyn Stream<Item = ProviderEvent> + Send + '_>> {
        self.seen.lock().unwrap().push(request.messages.to_vec());
        match self.script.lock().unwrap().pop_front() {
            Some(events) => Box::pin(futures_util::stream::iter(events)),
            None => Box::pin(futures_util::stream::pending()),
        }
    }
    fn count_tokens(&self, _: &[Message]) -> u64 {
        7
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supports_tools: true,
            supports_images: false,
            supports_caching: false,
            supports_structured_output: false,
            max_context_tokens: 100_000,
        }
    }
}

struct Factory {
    script: Arc<Mutex<VecDeque<Vec<ProviderEvent>>>>,
    seen: Seen,
}

impl ProviderFactory for Factory {
    fn build(&self, _: &str) -> Result<(Box<dyn Provider>, String), BuildError> {
        Ok((
            Box::new(Scripted {
                script: self.script.clone(),
                seen: self.seen.clone(),
            }),
            "scripted".into(),
        ))
    }
}

fn text(t: &str) -> ProviderEvent {
    ProviderEvent::TextDelta(t.into())
}
fn done() -> ProviderEvent {
    ProviderEvent::Done {
        finish_reason: "stop".into(),
    }
}
fn tool_use() -> ProviderEvent {
    ProviderEvent::Done {
        finish_reason: "tool_use".into(),
    }
}

fn texts(messages: &[Message]) -> String {
    messages
        .iter()
        .flat_map(|m| m.blocks.iter())
        .filter_map(|b| match b {
            ContentBlock::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n---\n")
}

fn project(dir: &std::path::Path, name: &str, instructions: &str) -> PathBuf {
    let root = dir.join(name);
    std::fs::create_dir_all(root.join(".aigentic")).unwrap();
    std::fs::write(
        root.join("aigentic.toml"),
        format!("[project]\nname = \"{name}\"\n[memory]\nenabled = false\n"),
    )
    .unwrap();
    std::fs::write(root.join(".aigentic/instructions.md"), instructions).unwrap();
    root
}

async fn until_idle(rx: &mut mpsc::Receiver<Notice>) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut ran = false;
    loop {
        let n = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .expect("idle in time")
            .expect("open");
        if let Notice::State { state, .. } = n {
            match state {
                ThreadState::Idle if ran => return,
                ThreadState::Idle => {}
                _ => ran = true,
            }
        }
    }
}

#[tokio::test]
async fn a_thread_moves_to_another_project_and_stays_there_across_a_reload() {
    let dir = tempfile::tempdir().unwrap();
    let p = project(dir.path(), "p", "This is project P.");
    let q = project(dir.path(), "q", "This is project Q.");
    let shared = dir.path().join("shared");
    std::fs::create_dir_all(shared.join("workspace")).unwrap();
    std::fs::write(
        shared.join("workspace/instructions.md"),
        "Workspace W voice.",
    )
    .unwrap();
    let cfg_dir = dir.path().join("cfg");
    std::fs::create_dir_all(cfg_dir.join("workspaces")).unwrap();
    std::fs::write(
        cfg_dir.join("workspaces/w.toml"),
        format!(
            "name = \"w\"\nshared = {:?}\nprojects = [{:?}]\n",
            shared.display().to_string(),
            p.display().to_string()
        ),
    )
    .unwrap();
    let threads_base = dir.path().join("threads");
    let config = Config::parse(&format!(
        "threads_dir = {:?}\nbundled_dir = {:?}\n[profiles.a]\nbase_url = \"u\"\nmodel = \"m\"\napi_key_env = \"K\"\n",
        threads_base.display(),
        dir.path().display()
    ))
    .unwrap();
    // p and q come from nowhere but the workspace file and server.toml:
    // p only from the workspace, q only from server.toml.
    let server = ServerConfig {
        listen: "unix".into(),
        idle_unload_secs: 3600,
        users: vec![UserConfig {
            name: "steve".into(),
            token_env: None,
            token: Some("tok".into()),
        }],
        projects: vec![aigentic_server::config::ProjectConfig {
            name: "q".into(),
            root: q.clone(),
        }],
    };
    let script = Arc::new(Mutex::new(VecDeque::from(vec![
        vec![text("hello from p"), done()],
        vec![
            ProviderEvent::ToolCall(ToolCall {
                id: "c1".into(),
                name: "bash".into(),
                args: json!({"command": "pwd"}),
            }),
            tool_use(),
        ],
        vec![text("in q now"), done()],
        vec![text("still q"), done()],
    ])));
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let server = Arc::new(Server::new(
        config,
        cfg_dir,
        server,
        Arc::new(Factory {
            script,
            seen: seen.clone(),
        }),
        Arc::new(NoReports),
    ));
    let socket = dir.path().join("d.sock");
    tokio::spawn(server.clone().serve(Listener::Unix(socket.clone())));
    for _ in 0..200 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let (client, welcome) = Client::connect(&Addr::Unix(socket.clone()), "tok")
        .await
        .unwrap();
    let names: Vec<&str> = welcome.projects.iter().map(|p| p.name.as_str()).collect();
    assert!(names.contains(&"p") && names.contains(&"q"), "{names:?}");

    let Response::Thread { thread } = client
        .request(Request::CreateThread {
            project: "p".into(),
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    let id = thread.id;
    assert!(matches!(
        client
            .request(Request::Open {
                thread: id,
                from_seq: 0
            })
            .await
            .unwrap(),
        Response::Opened { .. }
    ));
    let mut notices = client.take_notices().unwrap();
    let post = |t: &str| Request::Post {
        thread: id,
        blocks: vec![ContentBlock::Text(t.into())],
        interrupt: false,
    };
    assert_eq!(client.request(post("hi")).await.unwrap(), Response::Ok);
    until_idle(&mut notices).await;
    {
        let seen = seen.lock().unwrap();
        let first = texts(&seen[0]);
        assert!(first.contains("This is project P."), "{first}");
        assert!(first.contains("Workspace W voice."), "{first}");
    }

    // An unknown project is refused; q is taken.
    assert!(matches!(
        client
            .request(Request::SwitchProject {
                thread: id,
                project: "nope".into()
            })
            .await
            .unwrap(),
        Response::Refused { .. } | Response::Error { .. }
    ));
    assert_eq!(
        client
            .request(Request::SwitchProject {
                thread: id,
                project: "q".into()
            })
            .await
            .unwrap(),
        Response::Ok
    );
    assert_eq!(
        client.request(post("where are you?")).await.unwrap(),
        Response::Ok
    );
    until_idle(&mut notices).await;
    {
        let seen = seen.lock().unwrap();
        let after = texts(&seen[1]);
        assert!(after.contains("This is project Q."), "{after}");
        assert!(!after.contains("This is project P."), "{after}");
        assert!(!after.contains("Workspace W voice."), "{after}");
        assert!(
            after.contains("moved from project p to project q"),
            "{after}"
        );
        assert!(
            after.contains("hello from p"),
            "the transcript stays: {after}"
        );
        // The bash call ran in q's root.
        let with_result = texts(&seen[2]);
        let q_real = std::fs::canonicalize(&q).unwrap();
        assert!(
            with_result.contains(&q_real.display().to_string())
                || with_result.contains(&q.display().to_string()),
            "{with_result}"
        );
    }
    let kinds: Vec<EventKind> = ThreadLog::open(threads_base.join("p"), id)
        .unwrap()
        .read_all()
        .unwrap()
        .iter()
        .map(|e| e.kind)
        .collect();
    assert!(kinds.contains(&EventKind::ProjectSwitched), "{kinds:?}");

    // Unload and reopen: rebuilt in q.
    client.request(Request::Close { thread: id }).await.unwrap();
    let unloaded = server.threads.sweep(Duration::ZERO).await;
    assert_eq!(unloaded, vec![id]);
    assert!(matches!(
        client
            .request(Request::Open {
                thread: id,
                from_seq: 0
            })
            .await
            .unwrap(),
        Response::Opened { .. }
    ));
    assert_eq!(client.request(post("again")).await.unwrap(), Response::Ok);
    until_idle(&mut notices).await;
    let seen = seen.lock().unwrap();
    let reloaded = texts(seen.last().unwrap());
    assert!(reloaded.contains("This is project Q."), "{reloaded}");
    assert!(!reloaded.contains("This is project P."), "{reloaded}");
}
