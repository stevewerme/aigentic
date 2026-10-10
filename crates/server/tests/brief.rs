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
    descriptions: Arc<Mutex<Vec<Vec<String>>>>,
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
        self.descriptions.lock().unwrap().push(
            request
                .tools
                .iter()
                .map(|t| t.description.clone())
                .collect(),
        );
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
    descriptions: Arc<Mutex<Vec<Vec<String>>>>,
}

impl ProviderFactory for RecordingFactory {
    fn build(&self, _: &str) -> Result<(Box<dyn Provider>, String), BuildError> {
        Ok((
            Box::new(Recording {
                script: self.script.clone(),
                seen: self.seen.clone(),
                descriptions: self.descriptions.clone(),
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
    descriptions: Arc<Mutex<Vec<Vec<String>>>>,
}

/// Write `rel` under `root`, making the folders on the way.
fn write_file(root: &std::path::Path, rel: &str, text: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
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
/// brief, `w` carries knowledge, memory and a brief, and workspace `v`
/// with project `r` sits outside it.
async fn daemon(script: Vec<Vec<ProviderEvent>>) -> Daemon {
    let dir = tempfile::tempdir().unwrap();
    let p = project(
        dir.path(),
        "p",
        "# Project P\n\nP consumes getscale's API.\n",
    );
    // A small file, so `p` keeps its knowledge inline and the search
    // tool is offered for what the sibling and the workspace hold.
    write_file(
        &p,
        ".aigentic/knowledge/notes.md",
        "# Notes\n\nP talks to the gateway.\n",
    );
    let q = project(
        dir.path(),
        "q",
        "# The marketing site\n\nNext.js on Vercel.\n",
    );
    write_file(
        &q,
        ".aigentic/knowledge/ops.md",
        "# Ops\n\nQ builds with the vercel flag set.\n",
    );
    write_file(
        &q,
        ".aigentic/memory/decisions.md",
        "# Choice\n\nWe chose vercel for marketing.\n",
    );
    let shared = dir.path().join("shared");
    std::fs::create_dir_all(shared.join("workspace")).unwrap();
    write_file(&shared, "workspace/instructions.md", "Workspace W voice.");
    write_file(
        &shared,
        "workspace/brief.md",
        "# Workspace W\n\nThe group of projects.\n",
    );
    write_file(
        &shared,
        "workspace/knowledge/notes.md",
        "# Notes\n\nW requires the gateway flag.\n",
    );
    write_file(
        &shared,
        "workspace/memory/facts.md",
        "# Fact\n\nWe keep one gateway.\n",
    );
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
    // Workspace `v` and its project `r`: in reach by role, outside what
    // a thread in `w` understands.
    let r = project(dir.path(), "r", "# Project R\n\nR is elsewhere.\n");
    write_file(
        &r,
        ".aigentic/knowledge/ops.md",
        "# R ops\n\nR deploys on its own.\n",
    );
    std::fs::create_dir_all(dir.path().join("v-shared/workspace")).unwrap();
    std::fs::write(
        cfg_dir.join("workspaces/v.toml"),
        format!(
            "name = \"v\"\nshared = {:?}\nprojects = [{:?}]\n",
            dir.path().join("v-shared").display().to_string(),
            r.display().to_string()
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
    let descriptions: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let server = Arc::new(Server::new(
        config,
        cfg_dir,
        server,
        Arc::new(RecordingFactory {
            script: Arc::new(Mutex::new(VecDeque::from(script))),
            seen: seen.clone(),
            descriptions: descriptions.clone(),
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
        descriptions,
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

/// A thread in `project` whose turn runs `script`; what the daemon saw
/// and wrote.
struct Run {
    prefix: String,
    tools: Vec<String>,
    descriptions: Vec<String>,
    results: Vec<ToolResultPayload>,
    /// Holds the daemon's temp dirs for the test's lifetime.
    _daemon: Daemon,
}

/// The `daemon` rig, with `p`'s file additionally naming `related` and
/// `r`'s file carrying `r_participants` when given.
async fn daemon_related(
    script: Vec<Vec<ProviderEvent>>,
    related: &[&str],
    r_participants: &str,
) -> Daemon {
    let rig = daemon(script).await;
    let list = related
        .iter()
        .map(|entry| format!("{entry:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    std::fs::write(
        rig.dir.path().join("p/aigentic.toml"),
        format!("[project]\nname = \"p\"\nrelated = [{list}]\n[memory]\nenabled = false\n"),
    )
    .unwrap();
    if !r_participants.is_empty() {
        std::fs::write(
            rig.dir.path().join("r/aigentic.toml"),
            format!("[project]\nname = \"r\"\n{r_participants}[memory]\nenabled = false\n"),
        )
        .unwrap();
    }
    rig
}

/// `run`, on a rig whose `p` names `related`.
async fn run_related(
    script: Vec<Vec<ProviderEvent>>,
    project: &str,
    related: &[&str],
    r_participants: &str,
) -> Run {
    let rig = daemon_related(script, related, r_participants).await;
    let (client, _welcome) = Client::connect(&Addr::Unix(rig.socket.clone()), "tok")
        .await
        .unwrap();
    let Response::Thread { thread } = client
        .request(Request::CreateThread {
            project: project.into(),
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
                blocks: vec![ContentBlock::Text("go".into())],
                interrupt: false,
            })
            .await
            .unwrap(),
        Response::Ok
    );
    until_idle(&mut notices).await;
    let log = ThreadLog::open(rig.threads_base.clone(), id).unwrap();
    let results = log
        .read_all()
        .unwrap()
        .into_iter()
        .filter_map(|e| serde_json::from_value::<ToolResultPayload>(e.payload).ok())
        .collect();
    let (messages, tools) = rig.seen.lock().unwrap()[0].clone();
    let descriptions = rig.descriptions.lock().unwrap()[0].clone();
    let prefix = systems(&messages);
    Run {
        prefix,
        tools,
        descriptions,
        results,
        _daemon: rig,
    }
}

/// One `read_brief` call and one `search_knowledge` call for `name`,
/// then a reply.
fn brief_and_search_turn(name: &str) -> Vec<Vec<ProviderEvent>> {
    vec![
        vec![
            ProviderEvent::ToolCall(ToolCall {
                id: "b1".into(),
                name: "read_brief".into(),
                args: json!({"project": name}),
            }),
            ProviderEvent::ToolCall(ToolCall {
                id: "s1".into(),
                name: "search_knowledge".into(),
                args: json!({"query": "deploys", "project": name}),
            }),
            tool_use(),
        ],
        vec![text("seen"), done()],
    ]
}

/// One thread, one posted turn, then idle.
async fn run(script: Vec<Vec<ProviderEvent>>, project: &str) -> Run {
    let rig = daemon(script).await;
    let (client, _welcome) = Client::connect(&Addr::Unix(rig.socket.clone()), "tok")
        .await
        .unwrap();
    let Response::Thread { thread } = client
        .request(Request::CreateThread {
            project: project.into(),
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
                blocks: vec![ContentBlock::Text("go".into())],
                interrupt: false,
            })
            .await
            .unwrap(),
        Response::Ok
    );
    until_idle(&mut notices).await;
    let log = ThreadLog::open(rig.threads_base.clone(), id).unwrap();
    let results = log
        .read_all()
        .unwrap()
        .into_iter()
        .filter_map(|e| serde_json::from_value::<ToolResultPayload>(e.payload).ok())
        .collect();
    let (messages, tools) = rig.seen.lock().unwrap()[0].clone();
    let descriptions = rig.descriptions.lock().unwrap()[0].clone();
    let prefix = systems(&messages);
    Run {
        prefix,
        tools,
        descriptions,
        results,
        _daemon: rig,
    }
}

/// One tool call for `args`, then a reply.
fn search_turn(args: serde_json::Value) -> Vec<Vec<ProviderEvent>> {
    vec![
        vec![
            ProviderEvent::ToolCall(ToolCall {
                id: "s1".into(),
                name: "search_knowledge".into(),
                args,
            }),
            tool_use(),
        ],
        vec![text("seen"), done()],
    ]
}

/// The description of `name` in the tool list the model was offered.
fn description_of(tools: &[String], descriptions: &[String], name: &str) -> String {
    let i = tools.iter().position(|t| t == name).expect("offered");
    descriptions[i].clone()
}

#[tokio::test]
async fn t6_a_sibling_is_searched_by_name_and_marked_read_only() {
    let run = run(search_turn(json!({"query": "vercel", "project": "q"})), "p").await;
    let content = &run.results[0].result.content;
    assert!(!run.results[0].result.is_error, "{content}");
    assert!(content.starts_with("[read-only · q]\n"), "{content}");
    assert!(
        content.contains("Q builds with the vercel flag set."),
        "{content}"
    );
    assert!(content.contains("memory/decisions.md"), "{content}");
    assert!(
        content.contains("We chose vercel for marketing."),
        "{content}"
    );
    // The sibling's knowledge was never inline: only the tool reaches it.
    assert!(
        !run.prefix.contains("Q builds with the vercel flag set."),
        "{}",
        run.prefix
    );
}

#[tokio::test]
async fn t6_the_workspace_is_searched_with_workspace_true() {
    let run = run(
        search_turn(json!({"query": "gateway", "workspace": true})),
        "p",
    )
    .await;
    let content = &run.results[0].result.content;
    assert!(!run.results[0].result.is_error, "{content}");
    assert!(
        content.starts_with("[read-only · workspace w]\n"),
        "{content}"
    );
    assert!(
        content.contains("W requires the gateway flag."),
        "{content}"
    );
    assert!(content.contains("memory/facts.md"), "{content}");
    assert!(content.contains("We keep one gateway."), "{content}");
    assert!(
        run.prefix.contains("# Workspace knowledge: w"),
        "{}",
        run.prefix
    );
}

#[tokio::test]
async fn t6_a_project_in_another_workspace_is_refused() {
    let run = run(
        search_turn(json!({"query": "deploys", "project": "r"})),
        "p",
    )
    .await;
    let result = &run.results[0].result;
    assert!(result.is_error, "{}", result.content);
    assert!(
        result
            .content
            .contains("`r` is not a project this thread understands"),
        "{}",
        result.content
    );
    // The refusal offers only the projects this thread understands.
    assert!(
        result.content.contains("searchable: p, q"),
        "{}",
        result.content
    );
    // The refused project is still named in the block, as any other
    // workspace's projects are.
    assert!(
        run.prefix.contains("Other workspaces: v: r"),
        "{}",
        run.prefix
    );
}

#[tokio::test]
async fn t6_an_inline_project_offered_the_tool_names_both_arguments() {
    let run = run(vec![vec![text("nothing to do"), done()]], "p").await;
    // `p`'s own knowledge is inline, yet a sibling's corpus offers it.
    assert!(
        run.tools.contains(&"search_knowledge".to_string()),
        "{:?}",
        run.tools
    );
    let description = description_of(&run.tools, &run.descriptions, "search_knowledge");
    for argument in ["query", "project", "workspace"] {
        assert!(description.contains(argument), "{description}");
    }
}

#[tokio::test]
async fn a_related_project_is_briefable_and_searchable_read_only() {
    let run = run_related(brief_and_search_turn("v/r"), "p", &["v/r"], "").await;
    let brief = &run.results[0].result;
    assert!(!brief.is_error, "{}", brief.content);
    assert!(
        brief.content.starts_with("[read-only · v/r]\n"),
        "{}",
        brief.content
    );
    assert!(
        brief.content.contains("R is elsewhere."),
        "{}",
        brief.content
    );
    let search = &run.results[1].result;
    assert!(!search.is_error, "{}", search.content);
    assert!(
        search.content.starts_with("[read-only · v/r]\n"),
        "{}",
        search.content
    );
    assert!(
        search.content.contains("R deploys on its own."),
        "{}",
        search.content
    );
    // The block lists the related project once, under its address and
    // with its brief's one-liner; the plain `v: r` row is gone.
    let one = aigentic_runtime::brief::one_line("# Project R\n\nR is elsewhere.\n").unwrap();
    assert!(
        run.prefix.contains(&format!("v: v/r — {one}")),
        "{}",
        run.prefix
    );
    assert!(!run.prefix.contains("v: r"), "{}", run.prefix);
    // No tool result names the related root: nothing reaches its files.
    let root = run._daemon.dir.path().join("r").display().to_string();
    for result in &run.results {
        assert!(
            !result.result.content.contains(&root),
            "{}",
            result.result.content
        );
    }
}

/// The child test `related_never_grants_a_role` re-runs, so its daemon's
/// stderr lands in the parent's pipe.
const ROLE_CHILD: &str = "AIGENTIC_129_ROLE_CHILD";

/// A related root whose own participants name someone else: the entry
/// resolves to no role, so no row, no reachable brief and no searchable
/// corpus — and the daemon says why on stderr.
#[test]
fn related_never_grants_a_role() {
    if std::env::var(ROLE_CHILD).is_ok() {
        return;
    }
    let exe = std::env::current_exe().unwrap();
    let out = std::process::Command::new(&exe)
        .args([
            "the_child_related_never_grants_a_role",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(ROLE_CHILD, "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "the child failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        stderr
    );
    assert!(stderr.contains("warn"), "{stderr}");
    assert!(stderr.contains("v/r"), "{stderr}");
}

#[tokio::test]
async fn the_child_related_never_grants_a_role() {
    if std::env::var(ROLE_CHILD).is_err() {
        return;
    }
    let run = run_related(
        brief_and_search_turn("v/r"),
        "p",
        &["v/r"],
        "[participants]\nmia = \"admin\"\n",
    )
    .await;
    for result in &run.results {
        assert!(result.result.is_error, "{}", result.result.content);
        assert!(
            !result.result.content.contains("R is elsewhere."),
            "{}",
            result.result.content
        );
        assert!(
            !result.result.content.contains("R deploys on its own."),
            "{}",
            result.result.content
        );
    }
    // `related` grants no role, so no row for the project at all.
    assert!(!run.prefix.contains("v/r"), "{}", run.prefix);
    assert!(!run.prefix.contains("v: r"), "{}", run.prefix);
}
