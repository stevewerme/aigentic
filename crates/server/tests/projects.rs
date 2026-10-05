//! The projects block (issue #81): the model sees which projects exist
//! and which workspace each is in — the thread's own workspace in
//! detail, every other as one line — the block follows a switch across
//! workspaces, and a thread whose creator is not a person, or holds no
//! role anywhere, gets no block at all.

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aigentic_api::client::{Addr, Client};
use aigentic_api::{Notice, Request, Response, ThreadState};
use aigentic_runtime::aigentic_core::{
    AgentId, Author, Capabilities, CompletionRequest, ContentBlock, EventKind, Message, Provider,
    ProviderEvent, UserId,
};
use aigentic_runtime::aigentic_log::{NewEvent, ThreadLog, ThreadStartedPayload};
use aigentic_server::build::{BuildError, ProviderFactory};
use aigentic_server::config::{Config, ProjectConfig, ServerConfig, UserConfig};
use aigentic_server::{Listener, NoReports, Server};
use futures_core::Stream;
use serde_json::to_value;
use tokio::sync::mpsc;
use ulid::Ulid;

type Seen = Arc<Mutex<Vec<Vec<Message>>>>;

/// A provider that records every request, so the test can read the
/// prefix the model was given, and always answers `ok` (a script that
/// runs dry would hang the turn).
struct Recording {
    seen: Seen,
}

impl Provider for Recording {
    fn complete(
        &self,
        request: &CompletionRequest<'_>,
    ) -> Pin<Box<dyn Stream<Item = ProviderEvent> + Send + '_>> {
        self.seen.lock().unwrap().push(request.messages.to_vec());
        Box::pin(futures_util::stream::iter(vec![
            ProviderEvent::TextDelta("ok".into()),
            ProviderEvent::Done {
                finish_reason: "stop".into(),
            },
        ]))
    }
    fn count_tokens(&self, _: &[Message]) -> u64 {
        7
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supports_tools: false,
            supports_images: false,
            supports_caching: false,
            supports_structured_output: false,
            max_context_tokens: 100_000,
        }
    }
}

struct RecordingFactory {
    seen: Seen,
}

impl ProviderFactory for RecordingFactory {
    fn build(&self, _: &str) -> Result<(Box<dyn Provider>, String), BuildError> {
        Ok((
            Box::new(Recording {
                seen: self.seen.clone(),
            }),
            "scripted".into(),
        ))
    }
}

/// A project folder `dir/name`: its file, its instructions, its root.
fn project(dir: &Path, name: &str, participants: &str) -> PathBuf {
    let root = dir.join(name);
    std::fs::create_dir_all(root.join(".aigentic")).unwrap();
    std::fs::write(
        root.join("aigentic.toml"),
        format!("[project]\nname = \"{name}\"\n{participants}[memory]\nenabled = false\n"),
    )
    .unwrap();
    std::fs::write(
        root.join(".aigentic/instructions.md"),
        format!("This is {name}."),
    )
    .unwrap();
    root
}

struct Daemon {
    server: Arc<Server>,
    steve: Client,
    mia: Client,
    seen: Seen,
    threads_base: PathBuf,
}

/// A daemon over `dir` with `server_projects` in its `server.toml` and
/// one workspace file per `(name, roots)` pair.
async fn daemon(
    dir: &Path,
    server_projects: Vec<ProjectConfig>,
    workspaces: &[(&str, Vec<PathBuf>)],
) -> Daemon {
    let cfg_dir = dir.join("cfg");
    std::fs::create_dir_all(cfg_dir.join("workspaces")).unwrap();
    for (name, roots) in workspaces {
        let list = roots
            .iter()
            .map(|r| format!("{:?}", r.display().to_string()))
            .collect::<Vec<_>>()
            .join(", ");
        std::fs::write(
            cfg_dir.join(format!("workspaces/{name}.toml")),
            format!("name = {name:?}\nprojects = [{list}]\n"),
        )
        .unwrap();
    }
    let threads_base = dir.join("threads");
    let config = Config::parse(&format!(
        "threads_dir = {:?}\nbundled_dir = {:?}\n[profiles.a]\nbase_url = \"u\"\nmodel = \"m\"\napi_key_env = \"K\"\n",
        threads_base.display(),
        dir.display()
    ))
    .unwrap();
    let server_cfg = ServerConfig {
        listen: "unix".into(),
        idle_unload_secs: 3600,
        users: vec![
            UserConfig {
                name: "steve".into(),
                token_env: None,
                token: Some("tok".into()),
            },
            UserConfig {
                name: "mia".into(),
                token_env: None,
                token: Some("tok2".into()),
            },
        ],
        projects: server_projects,
        resume_runs: false,
    };
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let server = Arc::new(Server::new(
        config,
        cfg_dir,
        server_cfg,
        Arc::new(RecordingFactory { seen: seen.clone() }),
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
    let (steve, welcome) = Client::connect(&Addr::Unix(socket.clone()), "tok")
        .await
        .unwrap();
    let names: Vec<&str> = welcome.projects.iter().map(|p| p.name.as_str()).collect();
    assert!(names.contains(&"p1"), "{names:?}");
    let (mia, _) = Client::connect(&Addr::Unix(socket), "tok2").await.unwrap();
    Daemon {
        server,
        steve,
        mia,
        seen,
        threads_base,
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

/// Create a thread in `project` and open it for `client`.
async fn created(client: &mut Client, project: &str) -> Ulid {
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
    id
}

/// Post one message as `client` and wait for the turn to end.
async fn post(client: &mut Client, notices: &mut mpsc::Receiver<Notice>, thread: Ulid, text: &str) {
    assert_eq!(
        client
            .request(Request::Post {
                thread,
                blocks: vec![ContentBlock::Text(text.into())],
                interrupt: false,
            })
            .await
            .unwrap(),
        Response::Ok
    );
    until_idle(notices).await;
}

/// The projects block in the requests recorded since `from`: the one
/// block of the prefix that starts the listing.
fn block_since(seen: &Seen, from: usize) -> Option<String> {
    let seen = seen.lock().unwrap();
    seen.iter()
        .skip(from)
        .flat_map(|messages| messages.iter())
        .flat_map(|m| m.blocks.iter())
        .find_map(|b| match b {
            ContentBlock::Text(t) if t.starts_with("Projects in reach.") => Some(t.clone()),
            _ => None,
        })
}

/// Every `.jsonl` under `base`, relative to it.
fn logs_under(base: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![base.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|x| x == "jsonl") {
                out.push(path.strip_prefix(base).unwrap().display().to_string());
            }
        }
    }
    out.sort();
    out
}

/// T5 (issue #81): two successive switches across workspaces. After
/// each, the block the model is given names the new workspace as the
/// thread's own — and is exactly what `shown_projects` renders — and no
/// log exists outside the thread's home.
#[tokio::test]
async fn the_listing_follows_two_switches_and_opens_no_stray_log() {
    let dir = tempfile::tempdir().unwrap();
    let p1 = project(dir.path(), "p1", "");
    let p2 = project(dir.path(), "p2", "");
    let q1 = project(dir.path(), "q1", "");
    let q2 = project(dir.path(), "q2", "");
    let mut d = daemon(
        dir.path(),
        vec![],
        &[
            ("w1", vec![p1.clone(), p2.clone()]),
            ("w2", vec![q1.clone(), q2.clone()]),
        ],
    )
    .await;

    let mut notices = d.steve.take_notices().unwrap();
    let id = created(&mut d.steve, "p1").await;
    let mark = d.seen.lock().unwrap().len();
    post(&mut d.steve, &mut notices, id, "hi").await;
    let first = block_since(&d.seen, mark).expect("the block in the first build");
    let expected = format!(
        "Projects in reach. This thread is in p1 (w1).\n\
         w1: p1 (here) {} · p2 {}\n\
         Other workspaces: w2: q1, q2",
        p1.display(),
        p2.display()
    );
    assert_eq!(first, expected);
    assert_eq!(
        d.server.threads.shown_projects(id, Some("p1")),
        Some(expected.clone())
    );
    println!("first build:\n{first}");

    // Move to the other workspace, and read the block the model gets
    // after the move.
    assert_eq!(
        d.steve
            .request(Request::SwitchProject {
                thread: id,
                project: "q1".into()
            })
            .await
            .unwrap(),
        Response::Ok
    );
    let mark = d.seen.lock().unwrap().len();
    post(&mut d.steve, &mut notices, id, "where am I?").await;
    let second = block_since(&d.seen, mark).expect("the block after the switch");
    let expected = format!(
        "Projects in reach. This thread is in q1 (w2).\n\
         w2: q1 (here) {} · q2 {}\n\
         Other workspaces: w1: p1, p2",
        q1.display(),
        q2.display()
    );
    assert_eq!(second, expected);
    assert_eq!(
        d.server.threads.shown_projects(id, Some("q1")),
        Some(expected.clone())
    );
    assert!(!second.contains("This thread is in p1"), "{second}");
    println!("after the first switch:\n{second}");

    // A second switch, within the same workspace: the mark moves.
    assert_eq!(
        d.steve
            .request(Request::SwitchProject {
                thread: id,
                project: "q2".into()
            })
            .await
            .unwrap(),
        Response::Ok
    );
    let mark = d.seen.lock().unwrap().len();
    post(&mut d.steve, &mut notices, id, "and now?").await;
    let third = block_since(&d.seen, mark).expect("the block after the second switch");
    let expected = format!(
        "Projects in reach. This thread is in q2 (w2).\n\
         w2: q1 {} · q2 (here) {}\n\
         Other workspaces: w1: p1, p2",
        q1.display(),
        q2.display()
    );
    assert_eq!(third, expected);
    println!("after the second switch:\n{third}");

    // The log never left its home, and since #9 the home is the one
    // flat threads directory: the switches open none.
    assert_eq!(logs_under(&d.threads_base), vec![format!("{id}.jsonl")]);
}

/// T6 (issue #81): the block is the creator's, not the opener's; an
/// agent's thread and one whose creator holds no role anywhere get no
/// block at all.
#[tokio::test]
async fn the_listing_belongs_to_the_creator_and_only_a_person_has_one() {
    let dir = tempfile::tempdir().unwrap();
    let p1 = project(
        dir.path(),
        "p1",
        "[participants]\nsteve = \"admin\"\nmia = \"write\"\n",
    );
    // mia has no role in p2: once a project's table names anyone, it
    // alone decides.
    let p2 = project(dir.path(), "p2", "[participants]\nsteve = \"admin\"\n");
    let mut d = daemon(
        dir.path(),
        vec![],
        &[("w1", vec![p1.clone()]), ("w2", vec![p2.clone()])],
    )
    .await;

    // steve's thread, opened and posted by mia.
    let mut steve_notices = d.steve.take_notices().unwrap();
    let id = created(&mut d.steve, "p1").await;
    let mut mia_notices = d.mia.take_notices().unwrap();
    assert!(matches!(
        d.mia
            .request(Request::Open {
                thread: id,
                from_seq: 0
            })
            .await
            .unwrap(),
        Response::Opened { .. }
    ));
    let mark = d.seen.lock().unwrap().len();
    post(&mut d.mia, &mut mia_notices, id, "mia here").await;
    let block = block_since(&d.seen, mark).expect("the creator's block");
    assert_eq!(
        block,
        d.server.threads.shown_projects(id, Some("p1")).unwrap(),
        "the block is the creator's"
    );
    assert!(
        block.contains("Other workspaces: w2: p2"),
        "steve's reach, not mia's: {block}"
    );

    // mia's own thread has mia's reach: p1 alone.
    let own = created(&mut d.mia, "p1").await;
    let mark = d.seen.lock().unwrap().len();
    post(&mut d.mia, &mut mia_notices, own, "mine").await;
    let block = block_since(&d.seen, mark).expect("mia's block");
    let expected = format!(
        "Projects in reach. This thread is in p1 (w1).\nw1: p1 (here) {}",
        p1.display()
    );
    assert_eq!(block, expected);
    // No row names p2. The check reads the block without p1's root:
    // that root is a temp path, and a temp path can contain `p2`
    // (`…/.tmp2iLLNa/p1`), which made this fail at random (#94).
    let names = block.replace(&p1.display().to_string(), "");
    assert!(!names.contains("p2"), "{block}");

    // A thread an agent started: `runner` is no person.
    let agent = hand_written(
        &d.threads_base,
        "p1",
        &p1,
        Author::Agent(AgentId("runner".into())),
    );
    let mark = d.seen.lock().unwrap().len();
    open_and_post(&mut d.steve, &mut steve_notices, agent).await;
    assert_eq!(
        block_since(&d.seen, mark),
        None,
        "an agent's thread has no block"
    );

    // A creator with no role anywhere: no reach, no block.
    let nobody = hand_written(
        &d.threads_base,
        "p1",
        &p1,
        Author::User(UserId("nobody".into())),
    );
    let mark = d.seen.lock().unwrap().len();
    open_and_post(&mut d.steve, &mut steve_notices, nobody).await;
    assert_eq!(block_since(&d.seen, mark), None, "no reach, no block");
}

/// T7 (issue #81): a root that does not exist is still listed, a name
/// two projects share belongs to the first (`server.toml`), and a root
/// named by two workspace files is labelled with the first.
#[tokio::test]
async fn the_listing_keeps_a_missing_root_and_the_first_of_two_claims() {
    let dir = tempfile::tempdir().unwrap();
    let p1 = project(dir.path(), "p1", "");
    // Named by a workspace, never created.
    let ghost = dir.path().join("ghost");
    let shared = project(dir.path(), "shared", "");
    let q1 = project(dir.path(), "q1", "");
    // The folder name makes this one "dup" too; server.toml's wins.
    let dup_folder = dir.path().join("dup");
    std::fs::create_dir_all(&dup_folder).unwrap();
    // server.toml's "dup", a root of its own.
    let dup_a = dir.path().join("dup-a");
    std::fs::create_dir_all(&dup_a).unwrap();
    let mut d = daemon(
        dir.path(),
        vec![ProjectConfig {
            name: "dup".into(),
            root: dup_a.clone(),
        }],
        &[
            ("one", vec![p1.clone(), ghost.clone(), shared.clone()]),
            ("two", vec![q1.clone(), dup_folder.clone(), shared.clone()]),
        ],
    )
    .await;

    let mut notices = d.steve.take_notices().unwrap();
    let id = created(&mut d.steve, "p1").await;
    let mark = d.seen.lock().unwrap().len();
    post(&mut d.steve, &mut notices, id, "hi").await;
    let block = block_since(&d.seen, mark).expect("the block");
    let expected = format!(
        "Projects in reach. This thread is in p1 (one).\n\
         one: ghost {} · p1 (here) {} · shared {}\n\
         Other workspaces: two: q1\n\
         In no workspace: dup",
        ghost.display(),
        p1.display(),
        shared.display()
    );
    assert_eq!(
        block, expected,
        "the missing root, the first label, the first name"
    );
    // The root that does not exist is listed as stored.
    assert!(!ghost.exists());
    // `shared` is labelled `one`, the first file naming it, so `two`
    // holds q1 alone.
    assert_eq!(
        block
            .lines()
            .filter(|l| l.starts_with("Other workspaces:"))
            .count(),
        1,
        "{block}"
    );
    // `dup` is in the block once, at server.toml's root: the workspace
    // folder of the same name is not listed at all.
    assert_eq!(block.matches("dup").count(), 1, "{block}");
    let current = d.server.threads.shown_projects(id, Some("dup")).unwrap();
    assert!(
        current.contains(&format!("dup (here) {}", dup_a.display())),
        "{current}"
    );
    println!("the listing for the thread:\n{block}");
    println!("the listing with the duplicated name as current:\n{current}");
}

/// A log for a thread, written by hand: `author` started it.
///
/// This deliberately writes under `<base>/<project>/`, the legacy
/// layout, **after** the daemon exists (issue #9): it is the case a
/// thread turns up in a legacy subdirectory the index never scanned, so
/// the miss path has to find it. Don't move it flat.
fn hand_written(base: &Path, project: &str, root: &Path, author: Author) -> Ulid {
    let threads = base.join(project);
    std::fs::create_dir_all(&threads).unwrap();
    let id = Ulid::generate();
    let mut log = ThreadLog::open(&threads, id).unwrap();
    let payload = ThreadStartedPayload {
        project: Some(project.to_owned()),
        root: root.to_path_buf(),
        created_by: author.clone(),
        parent_thread: None,
        step: None,
        // never a front thread (issue #84)
        front: false,
    };
    log.append(NewEvent {
        kind: EventKind::ThreadStarted,
        author,
        payload: to_value(payload).unwrap(),
        parent_event: None,
    })
    .unwrap();
    id
}

/// Open a hand-written thread and post one message.
async fn open_and_post(client: &mut Client, notices: &mut mpsc::Receiver<Notice>, thread: Ulid) {
    assert!(matches!(
        client
            .request(Request::Open {
                thread,
                from_seq: 0
            })
            .await
            .unwrap(),
        Response::Opened { .. }
    ));
    post(client, notices, thread, "hi").await;
}
