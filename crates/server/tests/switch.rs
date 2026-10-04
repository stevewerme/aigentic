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
use aigentic_api::{Notice, Request, Response, SwitchReply, ThreadState};
use aigentic_runtime::aigentic_core::{
    Capabilities, CompletionRequest, ContentBlock, EventKind, Message, Provider, ProviderEvent,
    ToolCall,
};
use aigentic_runtime::aigentic_log::ThreadLog;
use aigentic_server::build::{BuildError, ProviderFactory};
use aigentic_server::config::{Config, ServerConfig, UserConfig};
use aigentic_server::{Listener, NoReports, Server};
use futures_core::Stream;
use futures_util::StreamExt;
use serde_json::json;
use tokio::sync::Notify;
use tokio::sync::mpsc;
use ulid::Ulid;

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

/// The first notice the predicate accepts, within the deadline.
async fn until_notice(rx: &mut mpsc::Receiver<Notice>, want: impl Fn(&Notice) -> bool) -> Notice {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let n = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .expect("the notice in time")
            .expect("open");
        if want(&n) {
            return n;
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
        resume_runs: false,
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
    let Response::Opened {
        profile,
        model,
        effort,
        ..
    } = client
        .request(Request::Open {
            thread: id,
            from_seq: 0,
        })
        .await
        .unwrap()
    else {
        panic!("an open reply")
    };
    // The footer's head comes from the daemon at attach (issue #43):
    // the profile the thread's provider was built from, the label its
    // factory returned, and the effort the profile names — none here,
    // since a scripted factory's profile sets none.
    assert_eq!(profile.as_deref(), Some("a"));
    assert_eq!(model, "scripted");
    assert_eq!(effort, None);
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
    // The switch rebinds the provider, so the subscribers are told who
    // the thread now runs as (issue #43).
    let announced = until_notice(&mut notices, |n| matches!(n, Notice::Model { .. })).await;
    let Notice::Model {
        profile,
        model,
        effort,
        ..
    } = announced
    else {
        panic!("a model notice")
    };
    assert_eq!(profile.as_deref(), Some("a"));
    assert_eq!(model, "scripted");
    assert_eq!(effort, None);
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
    let kinds: Vec<EventKind> = ThreadLog::open(threads_base.clone(), id)
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

// ------------------------------------------------ the proposal (#7, T9-T13)

/// A project folder `dir/name` with a participants table of its own: the
/// file the target's `write` check reads.
fn project_with(dir: &std::path::Path, name: &str, participants: &str) -> PathBuf {
    let root = dir.join(name);
    std::fs::create_dir_all(root.join(".aigentic")).unwrap();
    std::fs::write(
        root.join("aigentic.toml"),
        format!("[project]\nname = \"{name}\"\n{participants}[memory]\nenabled = false\n"),
    )
    .unwrap();
    std::fs::write(
        root.join(".aigentic/instructions.md"),
        format!("This is project {name}."),
    )
    .unwrap();
    root
}

/// The model's proposal, as the provider emits it.
fn proposal(call_id: &str, project: &str, reason: &str) -> ProviderEvent {
    ProviderEvent::ToolCall(ToolCall {
        id: call_id.into(),
        name: "suggest_project".into(),
        args: json!({"project": project, "reason": reason}),
    })
}

/// Every event of a thread's log, read where the switch left it.
fn events_under(base: &std::path::Path, id: Ulid) -> Vec<aigentic_runtime::aigentic_core::Event> {
    // Logs live flat in the threads directory since #9: the switch
    // changes where the thread is built, not where its log sits.
    ThreadLog::open(base, id)
        .expect("the log opens")
        .read_all()
        .expect("the log reads")
}

/// The answers a log carries, in order.
fn answers(
    events: &[aigentic_runtime::aigentic_core::Event],
) -> Vec<aigentic_runtime::aigentic_log::DecisionAnsweredPayload> {
    events
        .iter()
        .filter(|e| e.kind == EventKind::DecisionAnswered)
        .map(|e| serde_json::from_value(e.payload.clone()).expect("an answer payload"))
        .collect()
}

struct Rig {
    server: Arc<Server>,
    socket: PathBuf,
    steve: Client,
    magnus: Client,
    seen: Seen,
    threads_base: PathBuf,
}

/// A daemon over `dir` with `p` and `q` in workspace `w1`, steve the
/// owner and magnus `write` in `p` but only `read` in `q`: everything a
/// proposal touches.
async fn rig(dir: &std::path::Path, script: Vec<Vec<ProviderEvent>>) -> Rig {
    let p = project_with(
        dir,
        "p",
        &format!(
            "[participants]\nsteve = \"admin\"\nmagnus = {}\n",
            json!("write")
        ),
    );
    let q = project_with(
        dir,
        "q",
        &format!(
            "[participants]\nsteve = \"admin\"\nmagnus = {}\n",
            json!("read")
        ),
    );
    let cfg_dir = dir.join("cfg");
    std::fs::create_dir_all(cfg_dir.join("workspaces")).unwrap();
    std::fs::write(
        cfg_dir.join("workspaces/w1.toml"),
        format!(
            "name = \"w1\"\nprojects = [{:?}, {:?}]\n",
            p.display().to_string(),
            q.display().to_string()
        ),
    )
    .unwrap();
    let threads_base = dir.join("threads");
    let config = Config::parse(&format!(
        "threads_dir = {:?}\nbundled_dir = {:?}\n[profiles.a]\nbase_url = \"u\"\nmodel = \"m\"\napi_key_env = \"K\"\n",
        threads_base.display(),
        dir.display()
    ))
    .unwrap();
    let server = ServerConfig {
        listen: "unix".into(),
        idle_unload_secs: 3600,
        users: vec![
            UserConfig {
                name: "steve".into(),
                token_env: None,
                token: Some("tok".into()),
            },
            UserConfig {
                name: "magnus".into(),
                token_env: None,
                token: Some("tok2".into()),
            },
        ],
        projects: vec![aigentic_server::config::ProjectConfig {
            name: "q".into(),
            root: q.clone(),
        }],
        resume_runs: false,
    };
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let server = Arc::new(Server::new(
        config,
        cfg_dir,
        server,
        Arc::new(Factory {
            script: Arc::new(Mutex::new(VecDeque::from(script))),
            seen: seen.clone(),
        }),
        Arc::new(NoReports),
    ));
    let socket = dir.join("d.sock");
    tokio::spawn(server.clone().serve(Listener::Unix(socket.clone())));
    for _ in 0..200 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let (steve, _) = Client::connect(&Addr::Unix(socket.clone()), "tok")
        .await
        .unwrap();
    let (magnus, _) = Client::connect(&Addr::Unix(socket.clone()), "tok2")
        .await
        .unwrap();
    Rig {
        server,
        socket,
        steve,
        magnus,
        seen,
        threads_base,
    }
}

/// A thread in `p`, opened by both clients, with steve's notices taken.
async fn thread_in_p(rig: &mut Rig) -> (Ulid, mpsc::Receiver<Notice>) {
    let created = rig
        .steve
        .request(Request::CreateThread {
            project: "p".into(),
        })
        .await
        .unwrap();
    let Response::Thread { thread } = created else {
        panic!("a thread: {created:?}")
    };
    let id = thread.id;
    for client in [&mut rig.steve, &mut rig.magnus] {
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
    }
    let notices = rig.steve.take_notices().unwrap();
    (id, notices)
}

/// Post one message as steve and stop when the turn parks on the
/// proposal: the four fields the client is told.
async fn post_and_park(
    rig: &mut Rig,
    notices: &mut mpsc::Receiver<Notice>,
    id: Ulid,
    typed: &str,
) -> (String, String, Option<String>, String) {
    assert_eq!(
        rig.steve
            .request(Request::Post {
                thread: id,
                blocks: vec![ContentBlock::Text(typed.into())],
                interrupt: false,
            })
            .await
            .unwrap(),
        Response::Ok
    );
    let notice = until_notice(notices, |n| {
        matches!(
            n,
            Notice::State {
                state: ThreadState::AwaitingSwitch { .. },
                ..
            }
        )
    })
    .await;
    let Notice::State {
        state:
            ThreadState::AwaitingSwitch {
                call_id,
                project,
                workspace,
                reason,
            },
        ..
    } = notice
    else {
        panic!("the proposal state")
    };
    (call_id, project, workspace, reason)
}

#[tokio::test]
async fn t9_a_proposal_reaches_the_client_and_a_yes_switches_the_project() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = rig(
        dir.path(),
        vec![
            vec![proposal("c1", "q", "the message is about q"), tool_use()],
            vec![text("in q now"), done()],
            vec![text("still q"), done()],
        ],
    )
    .await;
    let (id, mut notices) = thread_in_p(&mut rig).await;

    let (call_id, project, workspace, reason) =
        post_and_park(&mut rig, &mut notices, id, "hi").await;
    assert_eq!(call_id, "c1");
    assert_eq!(project, "q");
    assert_eq!(workspace.as_deref(), Some("w1"), "the target's workspace");
    assert_eq!(reason, "the message is about q");

    // The proposal is in the log before anyone answers it.
    let them = events_under(&rig.threads_base, id);
    let proposed: Vec<aigentic_runtime::aigentic_log::DecisionProposedPayload> = them
        .iter()
        .filter(|e| e.kind == EventKind::DecisionProposed)
        .map(|e| serde_json::from_value(e.payload.clone()).expect("a proposal payload"))
        .collect();
    assert_eq!(proposed.len(), 1);
    assert_eq!(proposed[0].target.as_deref(), Some("q"));

    // A yes, from a person with `write` in q.
    assert_eq!(
        rig.steve
            .request(Request::AnswerSwitch {
                thread: id,
                call_id: call_id.clone(),
                answer: SwitchReply::Yes,
            })
            .await
            .unwrap(),
        Response::Ok
    );
    // The switch rebinds the provider, so the subscribers are told who
    // the thread now runs as (issue #43).
    until_notice(&mut notices, |n| matches!(n, Notice::Model { .. })).await;
    until_idle(&mut notices).await;

    assert_eq!(rig.server.threads.project_of(id).as_deref(), Some("q"));
    let kinds: Vec<EventKind> = events_under(&rig.threads_base, id)
        .iter()
        .map(|e| e.kind)
        .collect();
    let switched = kinds
        .iter()
        .position(|k| *k == EventKind::ProjectSwitched)
        .expect("the switch is logged");
    let answered = kinds
        .iter()
        .position(|k| *k == EventKind::DecisionAnswered)
        .expect("the answer is logged");
    assert!(switched < answered, "{kinds:?}");

    // The next turn's prefix names q as current, read from the daemon's
    // own renderer rather than typed out here.
    let mark = rig.seen.lock().unwrap().len();
    assert_eq!(
        rig.steve
            .request(Request::Post {
                thread: id,
                blocks: vec![ContentBlock::Text("where am I?".into())],
                interrupt: false,
            })
            .await
            .unwrap(),
        Response::Ok
    );
    until_idle(&mut notices).await;
    let after = {
        let seen = rig.seen.lock().unwrap();
        texts(&seen[mark])
    };
    let expected = rig
        .server
        .threads
        .shown_projects(id, Some("q"))
        .expect("q is in reach");
    assert!(after.contains(&expected), "{after}");
    assert_eq!(rig.server.threads.project_of(id).as_deref(), Some("q"));
}

#[tokio::test]
async fn t9_a_no_leaves_the_project_and_the_prefix_alone() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = rig(
        dir.path(),
        vec![
            vec![proposal("c1", "q", "belongs elsewhere"), tool_use()],
            vec![text("staying in p"), done()],
        ],
    )
    .await;
    let (id, mut notices) = thread_in_p(&mut rig).await;
    let (call_id, ..) = post_and_park(&mut rig, &mut notices, id, "hi").await;

    assert_eq!(
        rig.steve
            .request(Request::AnswerSwitch {
                thread: id,
                call_id,
                answer: SwitchReply::No,
            })
            .await
            .unwrap(),
        Response::Ok
    );
    until_idle(&mut notices).await;

    assert_eq!(rig.server.threads.project_of(id).as_deref(), Some("p"));
    let them = events_under(&rig.threads_base, id);
    assert!(
        !them.iter().any(|e| e.kind == EventKind::ProjectSwitched),
        "nothing switched"
    );
    assert_eq!(
        answers(&them)[0].answer,
        aigentic_runtime::aigentic_log::DecisionAnswer::No
    );
    let last = {
        let seen = rig.seen.lock().unwrap();
        texts(seen.last().unwrap())
    };
    let expected = rig.server.threads.shown_projects(id, Some("p")).unwrap();
    assert!(last.contains(&expected), "{last}");
}

#[tokio::test]
async fn t9_a_yes_without_write_in_the_target_is_refused_and_stays_pending() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = rig(
        dir.path(),
        vec![
            vec![proposal("c1", "q", "belongs elsewhere"), tool_use()],
            vec![text("in q now"), done()],
        ],
    )
    .await;
    let (id, mut notices) = thread_in_p(&mut rig).await;
    let (call_id, ..) = post_and_park(&mut rig, &mut notices, id, "hi").await;

    // Magnus writes in p but only reads q: a yes cannot take the thread
    // there.
    let refused = rig
        .magnus
        .request(Request::AnswerSwitch {
            thread: id,
            call_id: call_id.clone(),
            answer: SwitchReply::Yes,
        })
        .await
        .unwrap();
    assert!(
        matches!(refused, Response::Refused { .. }),
        "no write in q: {refused:?}"
    );
    assert_eq!(rig.server.threads.project_of(id).as_deref(), Some("p"));
    assert!(
        answers(&events_under(&rig.threads_base, id)).is_empty(),
        "the proposal is still open"
    );

    // It is still the one the owner can answer.
    assert_eq!(
        rig.steve
            .request(Request::AnswerSwitch {
                thread: id,
                call_id,
                answer: SwitchReply::Yes,
            })
            .await
            .unwrap(),
        Response::Ok
    );
    until_idle(&mut notices).await;
    assert_eq!(rig.server.threads.project_of(id).as_deref(), Some("q"));
}

#[tokio::test]
async fn t9_an_answer_while_idle_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = rig(dir.path(), vec![vec![text("hi"), done()]]).await;
    let (id, _notices) = thread_in_p(&mut rig).await;
    let refused = rig
        .steve
        .request(Request::AnswerSwitch {
            thread: id,
            call_id: "c1".into(),
            answer: SwitchReply::Yes,
        })
        .await
        .unwrap();
    assert!(
        matches!(refused, Response::Refused { .. } | Response::Error { .. }),
        "nothing is pending: {refused:?}"
    );
    assert_eq!(rig.server.threads.project_of(id).as_deref(), Some("p"));
}

/// A provider whose first reply holds until the test lets go, so the turn
/// is live while the test sends what it sends.
struct Held {
    started: Arc<Notify>,
    release: Arc<Notify>,
    first: Mutex<bool>,
    seen: Seen,
}

impl Provider for Held {
    fn complete(
        &self,
        request: &CompletionRequest<'_>,
    ) -> Pin<Box<dyn Stream<Item = ProviderEvent> + Send + '_>> {
        self.seen.lock().unwrap().push(request.messages.to_vec());
        let held = std::mem::take(&mut *self.first.lock().unwrap());
        if !held {
            return Box::pin(futures_util::stream::iter(vec![text("done"), done()]));
        }
        let (started, release) = (self.started.clone(), self.release.clone());
        let head = futures_util::stream::once(async move {
            started.notify_one();
            release.notified().await;
            text("after the hold")
        });
        Box::pin(head.chain(futures_util::stream::iter(vec![done()])))
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

struct HeldFactory {
    started: Arc<Notify>,
    release: Arc<Notify>,
    seen: Seen,
}

impl ProviderFactory for HeldFactory {
    fn build(&self, _: &str) -> Result<(Box<dyn Provider>, String), BuildError> {
        Ok((
            Box::new(Held {
                started: self.started.clone(),
                release: self.release.clone(),
                first: Mutex::new(true),
                seen: self.seen.clone(),
            }),
            "scripted".into(),
        ))
    }
}

#[tokio::test]
async fn t9_a_switch_project_mid_turn_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let p = project_with(dir.path(), "p", "");
    let q = project_with(dir.path(), "q", "");
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
        idle_unload_secs: 3600,
        users: vec![UserConfig {
            name: "steve".into(),
            token_env: None,
            token: Some("tok".into()),
        }],
        projects: vec![
            aigentic_server::config::ProjectConfig {
                name: "p".into(),
                root: p.clone(),
            },
            aigentic_server::config::ProjectConfig {
                name: "q".into(),
                root: q.clone(),
            },
        ],
        resume_runs: false,
    };
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let server = Arc::new(Server::new(
        config,
        cfg_dir,
        server,
        Arc::new(HeldFactory {
            started: started.clone(),
            release: release.clone(),
            seen,
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
    let (client, _) = Client::connect(&Addr::Unix(socket), "tok").await.unwrap();
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

    // A turn that holds: the reply is on its way, the turn is running.
    assert_eq!(
        client
            .request(Request::Post {
                thread: id,
                blocks: vec![ContentBlock::Text("hi".into())],
                interrupt: false,
            })
            .await
            .unwrap(),
        Response::Ok
    );
    tokio::time::timeout(Duration::from_secs(5), started.notified())
        .await
        .expect("the turn started");
    let refused = client
        .request(Request::SwitchProject {
            thread: id,
            project: "q".into(),
        })
        .await
        .unwrap();
    // The actor refuses a manual switch while a turn runs; a proposal
    // answered by a person is the path that works mid-turn (issue #7).
    let Response::Error { message } = refused else {
        panic!("a turn is running: {refused:?}")
    };
    assert!(message.contains("a turn is running"), "{message}");
    assert_eq!(server.threads.project_of(id).as_deref(), Some("p"));
    release.notify_one();
    until_idle(&mut notices).await;
}

#[tokio::test]
async fn t9_the_second_client_to_answer_is_refused_and_the_first_stands() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = rig(
        dir.path(),
        vec![
            vec![proposal("c1", "q", "belongs elsewhere"), tool_use()],
            vec![text("in q now"), done()],
        ],
    )
    .await;
    let (id, mut notices) = thread_in_p(&mut rig).await;
    let (call_id, ..) = post_and_park(&mut rig, &mut notices, id, "hi").await;

    assert_eq!(
        rig.steve
            .request(Request::AnswerSwitch {
                thread: id,
                call_id: call_id.clone(),
                answer: SwitchReply::Yes,
            })
            .await
            .unwrap(),
        Response::Ok
    );
    // Magnus has `write` in q too, but the wait is gone.
    let refused = rig
        .magnus
        .request(Request::AnswerSwitch {
            thread: id,
            call_id,
            answer: SwitchReply::Yes,
        })
        .await
        .unwrap();
    assert!(
        matches!(refused, Response::Refused { .. }),
        "the first answer stands: {refused:?}"
    );
    until_idle(&mut notices).await;
    assert_eq!(rig.server.threads.project_of(id).as_deref(), Some("q"));
    assert_eq!(
        answers(&events_under(&rig.threads_base, id)).len(),
        1,
        "one answer, not two"
    );
}

#[tokio::test]
async fn t9_a_cancelled_turn_leaves_the_answer_refused_and_the_entry_alone() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = rig(
        dir.path(),
        vec![
            vec![proposal("c1", "q", "belongs elsewhere"), tool_use()],
            vec![text("in p still"), done()],
        ],
    )
    .await;
    let (id, mut notices) = thread_in_p(&mut rig).await;
    let (call_id, ..) = post_and_park(&mut rig, &mut notices, id, "hi").await;

    // The turn is interrupted while the proposal waits: the park closes,
    // so a later yes has nothing to answer and the session never waits on
    // an ack that cannot come.
    assert_eq!(
        rig.steve
            .request(Request::Post {
                thread: id,
                blocks: vec![ContentBlock::Text("stop".into())],
                interrupt: true,
            })
            .await
            .unwrap(),
        Response::Ok
    );
    until_idle(&mut notices).await;
    let refused = rig
        .steve
        .request(Request::AnswerSwitch {
            thread: id,
            call_id,
            answer: SwitchReply::Yes,
        })
        .await
        .unwrap();
    assert!(
        matches!(refused, Response::Refused { .. }),
        "the turn left: {refused:?}"
    );
    assert_eq!(rig.server.threads.project_of(id).as_deref(), Some("p"));
    let them = events_under(&rig.threads_base, id);
    assert!(
        !them.iter().any(|e| e.kind == EventKind::ProjectSwitched),
        "no switch happened"
    );
    assert_eq!(
        answers(&them)[0].answer,
        aigentic_runtime::aigentic_log::DecisionAnswer::Withdrawn,
        "the interrupted turn closed the proposal"
    );
}

#[tokio::test]
async fn t10_the_thread_leaves_awaiting_switch_after_the_answer() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = rig(
        dir.path(),
        vec![
            vec![proposal("c1", "q", "belongs elsewhere"), tool_use()],
            vec![text("staying"), done()],
        ],
    )
    .await;
    let (id, mut notices) = thread_in_p(&mut rig).await;
    let (call_id, ..) = post_and_park(&mut rig, &mut notices, id, "hi").await;
    assert_eq!(
        rig.steve
            .request(Request::AnswerSwitch {
                thread: id,
                call_id,
                answer: SwitchReply::No,
            })
            .await
            .unwrap(),
        Response::Ok
    );
    let left = until_notice(&mut notices, |n| {
        matches!(
            n,
            Notice::State { state, .. }
                if !matches!(state, ThreadState::AwaitingSwitch { .. })
        )
    })
    .await;
    let Notice::State { state, .. } = left else {
        panic!("a state")
    };
    assert!(
        matches!(state, ThreadState::Running { .. } | ThreadState::Idle),
        "the wait has ended: {state:?}"
    );
}

#[tokio::test]
async fn t13_a_protocol_2_hello_is_refused_with_both_numbers() {
    use aigentic_api::{Frame, PROTOCOL_VERSION};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let dir = tempfile::tempdir().unwrap();
    let rig = rig(dir.path(), vec![vec![text("hi"), done()]]).await;
    // A raw connection, because the only frame that may carry a stale
    // protocol number is the first one: a client that says hello twice is
    // refused for that instead.
    let stream = tokio::net::UnixStream::connect(&rig.socket).await.unwrap();
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    let hello = Frame::request(
        1,
        Request::Hello {
            protocol: 2,
            token: "tok".into(),
        },
    );
    write
        .write_all(format!("{}\n", aigentic_api::encode(&hello)).as_bytes())
        .await
        .unwrap();
    let line = lines.next_line().await.unwrap().expect("a reply");
    let frame = aigentic_api::decode(&line).unwrap();
    let aigentic_api::Body::Response(Response::Refused { reason }) = frame.body else {
        panic!("a refusal: {line}")
    };
    assert!(reason.contains('2'), "{reason}");
    assert!(reason.contains(&PROTOCOL_VERSION.to_string()), "{reason}");
    assert!(
        reason.contains(&2.to_string()),
        "the client's number: {reason}"
    );
}
