//! T5 (issue #123), through a thread: a two-project workspace where both
//! projects and the workspace carry a brief. The model's first request
//! carries the workspace brief, the project's own brief and the
//! sibling's one-liner, and a scripted `read_brief` call returns the
//! sibling's whole brief. A scripted provider stands in for the model;
//! the daemon is the real one, so the briefs come from the real build
//! path, not from a hand-built `Runtime`.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aigentic_api::client::{Addr, Client};
use aigentic_api::{Notice, Request, Response, ThreadState};
use aigentic_runtime::aigentic_core::{
    Capabilities, CompletionRequest, ContentBlock, Message, Provider, ProviderEvent, ToolCall,
};
use aigentic_runtime::aigentic_log::{ThreadLog, ToolResultPayload};
use aigentic_server::build::{BuildError, ProviderFactory};
use aigentic_server::config::{Config, ServerConfig, UserConfig};
use aigentic_server::{Listener, NoReports, Server};
use futures_core::Stream;
use serde_json::json;
use tokio::sync::mpsc;

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

/// What one model request carried: its messages, and the names of the
/// tools it was offered.
type RequestSeen = (Vec<Message>, Vec<String>);

/// Scripted, and every request kept, so the test can read what the model
/// saw — the prefix and the tool list.
struct Recording {
    script: Arc<Mutex<VecDeque<Vec<ProviderEvent>>>>,
    seen: Arc<Mutex<Vec<RequestSeen>>>,
}

impl Provider for Recording {
    fn complete(
        &self,
        request: &CompletionRequest<'_>,
    ) -> Pin<Box<dyn Stream<Item = ProviderEvent> + Send + '_>> {
        self.seen.lock().unwrap().push((
            request.messages.to_vec(),
            request.tools.iter().map(|t| t.name.clone()).collect(),
        ));
        let events = self.script.lock().unwrap().pop_front().unwrap_or_default();
        Box::pin(futures_util::stream::iter(events))
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
            max_context_tokens: 1000,
        }
    }
}

/// Every thread built gets the same script, in order.
struct RecordingFactory {
    script: Arc<Mutex<VecDeque<Vec<ProviderEvent>>>>,
    seen: Arc<Mutex<Vec<RequestSeen>>>,
}

impl ProviderFactory for RecordingFactory {
    fn build(&self, _: &str) -> Result<(Box<dyn Provider>, String), BuildError> {
        Ok((
            Box::new(Recording {
                script: self.script.clone(),
                seen: self.seen.clone(),
            }),
            "scripted".into(),
        ))
    }
}

struct Daemon {
    socket: std::path::PathBuf,
    threads_base: std::path::PathBuf,
    dir: tempfile::TempDir,
    seen: Arc<Mutex<Vec<RequestSeen>>>,
}

/// A project `name` under `dir` with `brief` as its `.aigentic/brief.md`
/// and memory off, so no extraction call follows a turn.
fn project(dir: &std::path::Path, name: &str, brief: &str) -> std::path::PathBuf {
    let root = dir.join(name);
    std::fs::create_dir_all(root.join(".aigentic")).unwrap();
    std::fs::write(
        root.join("aigentic.toml"),
        format!("[project]\nname = {name:?}\n[memory]\nenabled = false\n"),
    )
    .unwrap();
    std::fs::write(root.join(".aigentic/brief.md"), brief).unwrap();
    root
}

/// A daemon over a two-project workspace `w`: `p` and `q` both carry a
/// brief, and so does `w`.
async fn daemon(script: Vec<Vec<ProviderEvent>>) -> Daemon {
    let dir = tempfile::tempdir().unwrap();
    let p = project(
        dir.path(),
        "p",
        "# Project P\n\nP consumes getscale's API.\n",
    );
    let q = project(
        dir.path(),
        "q",
        "# The marketing site\n\nNext.js on Vercel.\n",
    );
    let shared = dir.path().join("shared");
    std::fs::create_dir_all(shared.join("workspace")).unwrap();
    std::fs::write(
        shared.join("workspace/instructions.md"),
        "Workspace W voice.",
    )
    .unwrap();
    std::fs::write(
        shared.join("workspace/brief.md"),
        "# Workspace W\n\nThe group of projects.\n",
    )
    .unwrap();
    let cfg_dir = dir.path().join("cfg");
    std::fs::create_dir_all(cfg_dir.join("workspaces")).unwrap();
    std::fs::write(
        cfg_dir.join("workspaces/w.toml"),
        format!(
            "name = \"w\"\nshared = {:?}\nprojects = [{:?}, {:?}]\n",
            shared.display().to_string(),
            p.display().to_string(),
            q.display().to_string()
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
    let server = ServerConfig {
        listen: "unix".into(),
        idle_unload_secs: 3600,
        users: vec![UserConfig {
            name: "steve".into(),
            token_env: None,
            token: Some("tok".into()),
        }],
        projects: Vec::new(),
        resume_runs: false,
    };
    let seen: Arc<Mutex<Vec<RequestSeen>>> = Arc::new(Mutex::new(Vec::new()));
    let server = Arc::new(Server::new(
        config,
        cfg_dir,
        server,
        Arc::new(RecordingFactory {
            script: Arc::new(Mutex::new(VecDeque::from(script))),
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
    Daemon {
        socket,
        threads_base,
        dir,
        seen,
    }
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

/// Every system text of a request, joined.
fn systems(messages: &[Message]) -> String {
    messages
        .iter()
        .filter_map(|m| match m.blocks.first() {
            Some(ContentBlock::Text(t)) => Some(t.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn t5_a_thread_carries_both_briefs_names_the_sibling_and_can_read_it() {
    let script = vec![
        vec![
            ProviderEvent::ToolCall(ToolCall {
                id: "c1".into(),
                name: "read_brief".into(),
                args: json!({"project": "q"}),
            }),
            tool_use(),
        ],
        vec![text("read it"), done()],
    ];
    let rig = daemon(script).await;
    let (client, welcome) = Client::connect(&Addr::Unix(rig.socket.clone()), "tok")
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
        panic!("a thread")
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
    assert_eq!(
        client
            .request(Request::Post {
                thread: id,
                blocks: vec![ContentBlock::Text("what does q do?".into())],
                interrupt: false,
            })
            .await
            .unwrap(),
        Response::Ok
    );
    until_idle(&mut notices).await;

    let (messages, tools) = {
        let seen = rig.seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "one brief read, one reply");
        seen[0].clone()
    };
    let prefix = systems(&messages);
    assert!(prefix.contains("Workspace W voice."), "{prefix}");
    assert!(
        prefix.contains("# Workspace brief: w\n\n# Workspace W\n\nThe group of projects."),
        "{prefix}"
    );
    assert!(
        prefix.contains("# Project brief: p\n\n# Project P\n\nP consumes getscale's API."),
        "{prefix}"
    );
    // The sibling is one line, beside its root, in the projects block.
    let q_root = rig.dir.path().join("q");
    assert!(
        prefix.contains(&format!("q {} — The marketing site", q_root.display())),
        "{prefix}"
    );
    // The sibling's whole brief is never inline: only its first line.
    assert!(!prefix.contains("Next.js on Vercel."), "{prefix}");
    // A sibling with a brief offers the tool.
    assert!(tools.contains(&"read_brief".to_string()), "{tools:?}");

    // The scripted call returned the sibling's whole brief.
    let log = ThreadLog::open(rig.threads_base.clone(), id).unwrap();
    let result = log
        .read_all()
        .unwrap()
        .into_iter()
        .find_map(|e| {
            (e.kind == aigentic_runtime::aigentic_core::EventKind::ToolResult)
                .then(|| serde_json::from_value::<ToolResultPayload>(e.payload).unwrap())
        })
        .expect("a tool result");
    assert_eq!(result.result.id, "c1");
    assert!(!result.result.is_error, "{}", result.result.content);
    assert!(
        result.result.content.starts_with("[read-only · q]"),
        "{}",
        result.result.content
    );
    assert!(
        result.result.content.contains("Next.js on Vercel."),
        "{}",
        result.result.content
    );
}
