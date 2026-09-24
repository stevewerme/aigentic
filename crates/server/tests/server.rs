//! Sessions over a Unix socket (phase 5 step 7): hello with a bad token
//! closes, a good one lists only the user's projects; a read user's post
//! is refused and the log unchanged; a write user's decision is refused;
//! two sessions on one thread both get every notice in order; a session
//! reconnecting with `from_seq` gets exactly what it missed;
//! `CreateThread` writes `thread_started` first; idle unload never
//! while awaiting approval. A scripted provider stands in for the model.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aigentic_api::client::{Addr, Client, ClientError};
use aigentic_api::{Notice, Request, Response, ThreadState};
use aigentic_runtime::aigentic_core::{
    Capabilities, CompletionRequest, ContentBlock, EventKind, Message, Provider, ProviderEvent,
    ToolCall,
};
use aigentic_runtime::aigentic_log::{ThreadLog, ThreadStartedPayload};
use aigentic_server::build::{BuildError, ProviderFactory};
use aigentic_server::config::{Config, ProjectConfig, ServerConfig, UserConfig};
use aigentic_server::{Listener, NoReports, Server};
use futures_core::Stream;
use serde_json::json;
use tokio::sync::mpsc;

/// Every thread gets the same script, in order; `None` pends.
struct Scripted(Mutex<VecDeque<Option<Vec<ProviderEvent>>>>);

impl Provider for Scripted {
    fn complete(
        &self,
        _: &CompletionRequest<'_>,
    ) -> Pin<Box<dyn Stream<Item = ProviderEvent> + Send + '_>> {
        match self.0.lock().unwrap().pop_front().flatten() {
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
            max_context_tokens: 1000,
        }
    }
}

/// One script per thread built, in order.
type Scripts = VecDeque<Vec<Option<Vec<ProviderEvent>>>>;

struct Factory(Arc<Mutex<Scripts>>);

impl ProviderFactory for Factory {
    fn build(
        &self,
        _: &str,
    ) -> Result<(Box<dyn aigentic_runtime::aigentic_core::Provider>, String), BuildError> {
        let script = self.0.lock().unwrap().pop_front().unwrap_or_default();
        Ok((
            Box::new(Scripted(Mutex::new(script.into()))),
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
fn bash(id: &str, command: &str) -> ProviderEvent {
    ProviderEvent::ToolCall(ToolCall {
        id: id.into(),
        name: "bash".into(),
        args: json!({"command": command}),
    })
}
fn tool_use() -> ProviderEvent {
    ProviderEvent::Done {
        finish_reason: "tool_use".into(),
    }
}

struct Daemon {
    socket: PathBuf,
    server: Arc<Server>,
    threads_base: PathBuf,
    _dir: tempfile::TempDir,
    _task: tokio::task::JoinHandle<Result<(), aigentic_server::ServerError>>,
}

/// A daemon with users steve (owner), magnus and reviewer, a project
/// `p` whose file names magnus `approve` and reviewer `read`, and a
/// project `q` with no participants (the owner's alone).
async fn daemon(scripts: Vec<Vec<Option<Vec<ProviderEvent>>>>, idle_secs: u64) -> Daemon {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("p");
    std::fs::create_dir_all(&p).unwrap();
    std::fs::write(
        p.join("aigentic.toml"),
        "[project]\nname = \"p\"\n[participants]\nmagnus = \"approve\"\nreviewer = \"read\"\n[memory]\nenabled = false\n",
    )
    .unwrap();
    let q = dir.path().join("q");
    std::fs::create_dir_all(&q).unwrap();
    // Memory off: these tests script every model call, and extraction
    // after a turn would be one more.
    std::fs::write(
        q.join("aigentic.toml"),
        "[project]\nname = \"q\"\n[memory]\nenabled = false\n",
    )
    .unwrap();
    let cfg_dir = dir.path().join("cfg");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    let threads_base = dir.path().join("threads");
    let config = Config::parse(&format!(
        "threads_dir = {:?}\nbundled_dir = {:?}\n[profiles.a]\nbase_url = \"u\"\nmodel = \"m\"\napi_key_env = \"K\"\n",
        threads_base.display(),
        dir.path().display()
    ))
    .unwrap();
    let server = ServerConfig {
        listen: "unix".into(),
        idle_unload_secs: idle_secs,
        users: ["steve", "magnus", "reviewer"]
            .iter()
            .map(|n| UserConfig {
                name: (*n).to_owned(),
                token_env: None,
                token: Some(format!("tok-{n}")),
            })
            .collect(),
        projects: vec![
            ProjectConfig {
                name: "p".into(),
                root: p,
            },
            ProjectConfig {
                name: "q".into(),
                root: q,
            },
        ],
    };
    let server = Arc::new(Server::new(
        config,
        cfg_dir,
        server,
        Arc::new(Factory(Arc::new(Mutex::new(scripts.into())))),
        Arc::new(NoReports),
    ));
    let socket = dir.path().join("d.sock");
    let task = tokio::spawn(server.clone().serve(Listener::Unix(socket.clone())));
    for _ in 0..200 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    Daemon {
        socket,
        server,
        threads_base,
        _dir: dir,
        _task: task,
    }
}

impl Daemon {
    async fn connect(&self, user: &str) -> (Client, aigentic_api::Welcome) {
        Client::connect(&Addr::Unix(self.socket.clone()), &format!("tok-{user}"))
            .await
            .unwrap()
    }

    fn log(&self, project: &str, thread: ulid::Ulid) -> Vec<EventKind> {
        ThreadLog::open(self.threads_base.join(project), thread)
            .unwrap()
            .read_all()
            .unwrap()
            .iter()
            .map(|e| e.kind)
            .collect()
    }
}

async fn until_state(
    rx: &mut mpsc::Receiver<Notice>,
    pred: impl Fn(&ThreadState) -> bool,
) -> Vec<Notice> {
    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let n = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .expect("state in time")
            .expect("open");
        let hit = matches!(&n, Notice::State { state, .. } if pred(state));
        seen.push(n);
        if hit {
            return seen;
        }
    }
}

#[tokio::test]
async fn hello_names_the_user_and_lists_their_projects() {
    let d = daemon(vec![], 600).await;
    let err = Client::connect(&Addr::Unix(d.socket.clone()), "tok-nobody")
        .await
        .unwrap_err();
    assert!(
        matches!(err, ClientError::Refused(ref r) if r == "unknown token"),
        "{err}"
    );

    let (_, welcome) = d.connect("steve").await;
    assert_eq!(welcome.user, "steve");
    let names: Vec<(String, Option<String>)> = welcome
        .projects
        .iter()
        .map(|p| (p.name.clone(), p.role.clone()))
        .collect();
    // Steve owns q (no participants) and is not named in p's table.
    assert_eq!(names, vec![("q".into(), Some("admin".into()))]);
    assert!(welcome.server.starts_with("aigentic "));

    let (client, welcome) = d.connect("reviewer").await;
    assert_eq!(
        welcome
            .projects
            .iter()
            .map(|p| (p.name.as_str(), p.role.as_deref()))
            .collect::<Vec<_>>(),
        vec![("p", Some("read"))]
    );
    // A protocol from the future is refused with both numbers.
    let (c2, _) = d.connect("steve").await;
    let r = c2
        .request(Request::Hello {
            protocol: 99,
            token: "x".into(),
        })
        .await
        .unwrap();
    assert!(matches!(r, Response::Refused { .. }));
    drop(client);
}

#[tokio::test]
async fn roles_are_checked_before_the_log_and_thread_started_comes_first() {
    let d = daemon(vec![vec![Some(vec![text("hi"), done()])]], 600).await;
    let (magnus, _) = d.connect("magnus").await;
    let (reviewer, _) = d.connect("reviewer").await;

    // reviewer may list and open, not create or post.
    assert!(matches!(
        reviewer
            .request(Request::ListThreads {
                project: "p".into()
            })
            .await
            .unwrap(),
        Response::Threads { threads } if threads.is_empty()
    ));
    let r = reviewer
        .request(Request::CreateThread {
            project: "p".into(),
        })
        .await
        .unwrap();
    assert!(
        matches!(r, Response::Refused { ref reason } if reason.contains("needs write")),
        "{r:?}"
    );

    let Response::Thread { thread } = magnus
        .request(Request::CreateThread {
            project: "p".into(),
        })
        .await
        .unwrap()
    else {
        panic!("thread")
    };
    assert_eq!(thread.project.as_deref(), Some("p"));
    assert_eq!(d.log("p", thread.id), vec![EventKind::ThreadStarted]);
    let events = ThreadLog::open(d.threads_base.join("p"), thread.id)
        .unwrap()
        .read_all()
        .unwrap();
    let started: ThreadStartedPayload = serde_json::from_value(events[0].payload.clone()).unwrap();
    assert_eq!(started.project.as_deref(), Some("p"));
    assert!(started.root.ends_with("p"));

    // A read user's post: refused, and the log unchanged.
    let r = reviewer
        .request(Request::Post {
            thread: thread.id,
            blocks: vec![ContentBlock::Text("hi".into())],
            interrupt: false,
        })
        .await
        .unwrap();
    assert!(matches!(r, Response::Refused { .. }), "{r:?}");
    assert_eq!(d.log("p", thread.id), vec![EventKind::ThreadStarted]);
    // A write-level action by an approver runs; a decide by a read user
    // is refused before anything is looked at.
    let r = reviewer
        .request(Request::Decide {
            thread: thread.id,
            call_id: "c".into(),
            allow: true,
            session: false,
            prefix: None,
            reason: None,
        })
        .await
        .unwrap();
    assert!(matches!(r, Response::Refused { ref reason } if reason.contains("needs approve")));
    // A thread nobody has, or a project not on the daemon.
    let r = magnus
        .request(Request::Open {
            thread: ulid::Ulid::generate(),
            from_seq: 0,
        })
        .await
        .unwrap();
    assert!(matches!(r, Response::Refused { .. }), "{r:?}");
    let r = magnus
        .request(Request::ListThreads {
            project: "zzz".into(),
        })
        .await
        .unwrap();
    assert!(matches!(r, Response::Refused { .. }), "{r:?}");
    // Listed under its project once created.
    let Response::Threads { threads } = magnus
        .request(Request::ListThreads {
            project: "p".into(),
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(threads.len(), 1);
    assert_eq!(threads[0].id, thread.id);
}

#[tokio::test]
async fn two_sessions_see_every_notice_in_order_and_a_reconnect_catches_up() {
    let d = daemon(
        vec![vec![
            Some(vec![bash("c1", "rm -rf x"), tool_use()]),
            Some(vec![text("after"), done()]),
        ]],
        600,
    )
    .await;
    let (steve, _) = d.connect("steve").await;
    // q is the owner's; a second user with a token but no role in q is
    // refused there, so both sessions here are steve's.
    let (steve2, _) = d.connect("steve").await;
    let Response::Thread { thread } = steve
        .request(Request::CreateThread {
            project: "q".into(),
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    let mut a = steve.take_notices().unwrap();
    let mut b = steve2.take_notices().unwrap();
    for c in [&steve, &steve2] {
        let r = c
            .request(Request::Open {
                thread: thread.id,
                from_seq: 0,
            })
            .await
            .unwrap();
        assert!(
            matches!(&r, Response::Opened { state: ThreadState::Idle, events, .. } if events.len() == 1),
            "{r:?}"
        );
    }
    assert_eq!(
        steve
            .request(Request::Post {
                thread: thread.id,
                blocks: vec![ContentBlock::Text("go".into())],
                interrupt: false,
            })
            .await
            .unwrap(),
        Response::Ok
    );
    let seen_a = until_state(&mut a, |s| {
        matches!(s, ThreadState::AwaitingApproval { .. })
    })
    .await;
    let seen_b = until_state(&mut b, |s| {
        matches!(s, ThreadState::AwaitingApproval { .. })
    })
    .await;
    assert_eq!(seen_a, seen_b, "both sessions, same notices, same order");
    let seqs: Vec<u64> = seen_a
        .iter()
        .filter_map(|n| match n {
            Notice::Event { event, .. } => Some(event.seq),
            _ => None,
        })
        .collect();
    assert_eq!(seqs, vec![1, 2, 3], "user, assistant, permission_requested");
    // The second session decides; the first sees the decision by steve
    // and the thread finish.
    assert_eq!(
        steve2
            .request(Request::Decide {
                thread: thread.id,
                call_id: "c1".into(),
                allow: false,
                session: false,
                prefix: None,
                reason: None,
            })
            .await
            .unwrap(),
        Response::Ok
    );
    until_state(&mut a, |s| *s == ThreadState::Idle).await;
    drop(steve2);
    // A third session reconnects from seq 4: exactly the events after
    // the permission request.
    let (late, _) = d.connect("steve").await;
    let Response::Opened { state, events, .. } = late
        .request(Request::Open {
            thread: thread.id,
            from_seq: 4,
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(state, ThreadState::Idle);
    assert_eq!(
        events.iter().map(|e| e.kind).collect::<Vec<_>>(),
        vec![
            EventKind::PermissionDecided,
            EventKind::ToolResult,
            EventKind::AssistantMessage,
            EventKind::TurnEnded
        ]
    );
    assert_eq!(
        events[0].author,
        aigentic_runtime::aigentic_core::Author::User(aigentic_runtime::aigentic_core::UserId(
            "steve".into()
        ))
    );
}

#[tokio::test]
async fn idle_threads_unload_but_never_while_awaiting_approval() {
    let d = daemon(
        vec![
            vec![
                Some(vec![bash("c1", "rm -rf x"), tool_use()]),
                Some(vec![text("x"), done()]),
            ],
            vec![Some(vec![text("hi"), done()])],
        ],
        0,
    )
    .await;
    let (steve, _) = d.connect("steve").await;
    let mut notices = steve.take_notices().unwrap();
    let Response::Thread { thread } = steve
        .request(Request::CreateThread {
            project: "q".into(),
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    steve
        .request(Request::Open {
            thread: thread.id,
            from_seq: 0,
        })
        .await
        .unwrap();
    steve
        .request(Request::Post {
            thread: thread.id,
            blocks: vec![ContentBlock::Text("go".into())],
            interrupt: false,
        })
        .await
        .unwrap();
    until_state(&mut notices, |s| {
        matches!(s, ThreadState::AwaitingApproval { .. })
    })
    .await;
    // Closed by its only session and idle for "0 seconds": still not
    // unloaded, because it waits for an approver.
    steve
        .request(Request::Close { thread: thread.id })
        .await
        .unwrap();
    assert!(d.server.threads.sweep(Duration::ZERO).await.is_empty());
    assert_eq!(d.server.threads.open_count(), 1);
    // Decide (opens it for the request only), let it finish, then sweep.
    steve
        .request(Request::Decide {
            thread: thread.id,
            call_id: "c1".into(),
            allow: true,
            session: false,
            prefix: None,
            reason: None,
        })
        .await
        .unwrap();
    for _ in 0..100 {
        if d.log("q", thread.id).last() == Some(&EventKind::TurnEnded) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(
        d.server.threads.sweep(Duration::ZERO).await,
        vec![thread.id]
    );
    assert_eq!(d.server.threads.open_count(), 0);
    // Reopening builds a fresh actor over the same log.
    let Response::Opened { events, .. } = steve
        .request(Request::Open {
            thread: thread.id,
            from_seq: 0,
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(events.last().map(|e| e.kind), Some(EventKind::TurnEnded));
}

/// The same daemon over TCP on a loopback port the system picks.
#[tokio::test]
async fn tcp_on_a_loopback_port_serves_the_same_sessions() {
    let d = daemon(vec![vec![Some(vec![text("hi"), done()])]], 600).await;
    let bound = d
        .server
        .clone()
        .listen(Listener::Tcp("127.0.0.1".into(), 0))
        .await
        .unwrap();
    let port = bound.port().unwrap();
    assert!(port > 0);
    assert_eq!(bound.addr(), format!("tcp:127.0.0.1:{port}"));
    let task = tokio::spawn(bound.serve());
    let addr = Addr::Tcp("127.0.0.1".into(), port);
    let err = Client::connect(&addr, "tok-nobody").await.unwrap_err();
    assert!(matches!(err, ClientError::Refused(_)), "{err}");
    let (steve, welcome) = Client::connect(&addr, "tok-steve").await.unwrap();
    assert_eq!(welcome.user, "steve");
    let Response::Thread { thread } = steve
        .request(Request::CreateThread {
            project: "q".into(),
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    let mut notices = steve.take_notices().unwrap();
    steve
        .request(Request::Open {
            thread: thread.id,
            from_seq: 0,
        })
        .await
        .unwrap();
    steve
        .request(Request::Post {
            thread: thread.id,
            blocks: vec![ContentBlock::Text("go".into())],
            interrupt: false,
        })
        .await
        .unwrap();
    until_state(&mut notices, |s| *s == ThreadState::Idle).await;
    assert_eq!(
        d.log("q", thread.id),
        vec![
            EventKind::ThreadStarted,
            EventKind::UserMessage,
            EventKind::AssistantMessage,
            EventKind::TurnEnded
        ]
    );
    // Listener strings parse both ways; nonsense is refused.
    assert_eq!(
        Listener::parse("tcp:0.0.0.0:7420").unwrap(),
        Listener::Tcp("0.0.0.0".into(), 7420)
    );
    assert!(matches!(
        Listener::parse("unix:/tmp/x.sock").unwrap(),
        Listener::Unix(_)
    ));
    for bad in ["tcp:7420", "tcp::1", "tcp:h:x", "unix:", "http://x"] {
        assert!(Listener::parse(bad).is_err(), "{bad}");
    }
    task.abort();
}

/// A line over the cap closes the session rather than growing memory;
/// the daemon goes on serving others.
#[tokio::test]
async fn an_oversized_line_closes_only_that_session() {
    use tokio::io::AsyncWriteExt;
    let d = daemon(vec![], 600).await;
    let mut raw = tokio::net::UnixStream::connect(&d.socket).await.unwrap();
    let huge = vec![b'x'; aigentic_server::session::MAX_LINE_BYTES + 1024];
    // The daemon may close mid-write; that is the point.
    let _ = raw.write_all(&huge).await;
    let _ = raw.write_all(b"\n").await;
    let mut probe = [0u8; 1];
    use tokio::io::AsyncReadExt;
    let closed = tokio::time::timeout(Duration::from_secs(5), raw.read(&mut probe))
        .await
        .expect("closed in time");
    assert!(matches!(closed, Ok(0) | Err(_)), "{closed:?}");
    // Others still connect.
    let (_, welcome) = d.connect("steve").await;
    assert_eq!(welcome.user, "steve");
}

/// `/remember` (issue #14): a write user files a line with no model
/// call; the event is the audit and the file is the state.
#[tokio::test]
async fn remember_files_a_line_with_no_model_call() {
    let d = daemon(vec![], 600).await;
    let (magnus, _) = d.connect("magnus").await;
    let Response::Thread { thread } = magnus
        .request(Request::CreateThread {
            project: "p".into(),
        })
        .await
        .unwrap()
    else {
        panic!("thread")
    };
    let r = magnus
        .request(Request::Remember {
            thread: thread.id,
            text: "decision We deploy from main only.".into(),
        })
        .await
        .unwrap();
    assert!(matches!(r, Response::Ok), "{r:?}");
    assert_eq!(
        d.log("p", thread.id),
        vec![EventKind::ThreadStarted, EventKind::MemoryRemembered]
    );
    let events = ThreadLog::open(d.threads_base.join("p"), thread.id)
        .unwrap()
        .read_all()
        .unwrap();
    let p: aigentic_runtime::aigentic_log::MemoryRememberedPayload =
        serde_json::from_value(events[1].payload.clone()).unwrap();
    assert_eq!(p.file, "decisions.md");
    assert_eq!(p.text, "We deploy from main only.");
    assert!(p.written);
    let root = d.threads_base.parent().unwrap().join("p");
    let decisions = std::fs::read_to_string(root.join(".aigentic/memory/decisions.md")).unwrap();
    assert!(
        decisions.contains("- We deploy from main only"),
        "{decisions}"
    );

    // A read user may not.
    let (reviewer, _) = d.connect("reviewer").await;
    let r = reviewer
        .request(Request::Remember {
            thread: thread.id,
            text: "no matter".into(),
        })
        .await
        .unwrap();
    assert!(
        matches!(r, Response::Refused { ref reason } if reason.contains("needs write")),
        "{r:?}"
    );
    assert_eq!(decisions.lines().count(), 1, "{decisions}");
}
