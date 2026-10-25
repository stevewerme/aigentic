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

/// A project `name` whose `[project] related` names `entries`; the
/// participants stay empty, so the daemon's owner holds a role.
fn related_project(dir: &Path, name: &str, entries: &[&str]) -> PathBuf {
    let root = dir.join(name);
    std::fs::create_dir_all(root.join(".aigentic")).unwrap();
    let list = entries
        .iter()
        .map(|e| format!("{e:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    std::fs::write(
        root.join("aigentic.toml"),
        format!("[project]\nname = {name:?}\nrelated = [{list}]\n[memory]\nenabled = false\n"),
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
    // Every distinct root the daemon was given: `merge` lists one project
    // per root, whatever names the workspaces carry.
    let given: std::collections::BTreeSet<PathBuf> = server_cfg
        .projects
        .iter()
        .map(|p| p.root.clone())
        .chain(
            workspaces
                .iter()
                .flat_map(|(_, roots)| roots.iter().cloned()),
        )
        .collect();
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let server = Arc::new(
        Server::new(
            config,
            cfg_dir,
            server_cfg,
            Arc::new(RecordingFactory { seen: seen.clone() }),
            Arc::new(NoReports),
        )
        .unwrap(),
    );
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
    assert_eq!(names.len(), given.len(), "{names:?}");
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

/// A log for a thread that started as `project` (the daemon's own name,
/// `None` for the legacy `_none`) at `root`, in the legacy layout: the
/// form every log written before a project was qualified has.
fn log_started_at(base: &Path, folder: &str, project: Option<&str>, root: &Path) -> Ulid {
    let threads = base.join(folder);
    std::fs::create_dir_all(&threads).unwrap();
    let id = Ulid::generate();
    let mut log = ThreadLog::open(&threads, id).unwrap();
    log.append(NewEvent {
        kind: EventKind::ThreadStarted,
        author: Author::User(UserId("steve".into())),
        payload: to_value(ThreadStartedPayload {
            project: project.map(str::to_owned),
            root: root.to_path_buf(),
            created_by: Author::User(UserId("steve".into())),
            parent_thread: None,
            step: None,
            front: false,
            job: false,
        })
        .unwrap(),
        parent_event: None,
    })
    .unwrap();
    id
}

/// Append a `project_switched` to `id`'s log, as a client that sent
/// `to` wrote it.
fn log_switched(base: &Path, folder: &str, id: Ulid, to: &str, root: &Path) {
    let mut log = ThreadLog::open(base.join(folder), id).unwrap();
    log.append(NewEvent {
        kind: EventKind::ProjectSwitched,
        author: Author::User(UserId("steve".into())),
        payload: to_value(aigentic_runtime::aigentic_log::ProjectSwitchedPayload {
            from: None,
            to: Some(to.to_owned()),
            root: root.to_path_buf(),
            workspace: None,
        })
        .unwrap(),
        parent_event: None,
    })
    .unwrap();
}

/// Append a `run_started` to `id`'s log, which is what makes it a run's
/// lead.
fn log_run_started(base: &Path, folder: &str, id: Ulid, issue: u64) {
    let mut log = ThreadLog::open(base.join(folder), id).unwrap();
    log.append(NewEvent {
        kind: EventKind::RunStarted,
        author: Author::System,
        payload: to_value(aigentic_runtime::aigentic_log::RunStartedPayload {
            issue,
            workflow: "build".into(),
            version: 1,
            content_hash: "x".into(),
            budget_usd: 3.0,
        })
        .unwrap(),
        parent_event: None,
    })
    .unwrap();
}

/// The listed project whose root is `root`, found the way the daemon's
/// own `merge` leaves its list: by root.
fn listed_at(server: &Arc<Server>, root: &Path) -> Option<String> {
    server
        .server
        .projects
        .iter()
        .find(|p| p.root == root)
        .map(|p| p.name.clone())
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
        "Projects in reach. This thread is in w1/p1 (w1).\n\
         w1: w1/p1 (here) {} · w1/p2 {}\n\
         Other workspaces: w2: w2/q1, w2/q2",
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
        "Projects in reach. This thread is in w2/q1 (w2).\n\
         w2: w2/q1 (here) {} · w2/q2 {}\n\
         Other workspaces: w1: w1/p1, w1/p2",
        q1.display(),
        q2.display()
    );
    assert_eq!(second, expected);
    assert_eq!(
        d.server.threads.shown_projects(id, Some("q1")),
        Some(expected.clone())
    );
    assert!(!second.contains("This thread is in w1/p1"), "{second}");
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
        "Projects in reach. This thread is in w2/q2 (w2).\n\
         w2: w2/q1 {} · w2/q2 (here) {}\n\
         Other workspaces: w1: w1/p1, w1/p2",
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
        block.contains("Other workspaces: w2: w2/p2"),
        "steve's reach, not mia's: {block}"
    );

    // mia's own thread has mia's reach: p1 alone.
    let own = created(&mut d.mia, "p1").await;
    let mark = d.seen.lock().unwrap().len();
    post(&mut d.mia, &mut mia_notices, own, "mine").await;
    let block = block_since(&d.seen, mark).expect("mia's block");
    let expected = format!(
        "Projects in reach. This thread is in w1/p1 (w1).\nw1: w1/p1 (here) {}",
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
        "Projects in reach. This thread is in one/p1 (one).\n\
         one: one/ghost {} · one/p1 (here) {} · one/shared {}\n\
         Other workspaces: two: two/dup, two/q1\n\
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
    // server.toml's `dup` and the workspace project whose folder is
    // also called `dup` are two projects: the qualified name keeps both.
    assert_eq!(block.matches("dup").count(), 2, "{block}");
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
        job: false,
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

/// a hand-edited workspace file with two roots of one leaf name
/// stops the daemon: `from_configs` and `embed_with` both refuse, naming
/// both roots.
#[tokio::test]
async fn a_hand_edited_duplicate_stops_the_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let one = project(dir.path(), "web", "");
    // A second folder carrying the same `[project] name`.
    let leaf = aigentic_server::workspaces::project_name(&one);
    let two = dir.path().join("web-again");
    std::fs::create_dir_all(&two).unwrap();
    std::fs::write(
        two.join("aigentic.toml"),
        format!("[project]\nname = {leaf:?}\n[memory]\nenabled = false\n"),
    )
    .unwrap();
    assert_eq!(leaf, aigentic_server::workspaces::project_name(&two));
    let cfg_dir = dir.path().join("cfg");
    std::fs::create_dir_all(cfg_dir.join("workspaces")).unwrap();
    let list = format!(
        "{:?}, {:?}",
        one.display().to_string(),
        two.display().to_string()
    );
    std::fs::write(
        cfg_dir.join("workspaces/w.toml"),
        format!("name = \"w\"\nprojects = [{list}]\n"),
    )
    .unwrap();
    let config = Config::parse(&format!(
        "threads_dir = {:?}\n[profiles.a]\nbase_url = \"u\"\nmodel = \"m\"\napi_key_env = \"K\"\n",
        dir.path().join("threads").display()
    ))
    .unwrap();
    let server_cfg = ServerConfig {
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

    let Err(refused) = Server::from_configs(config.clone(), cfg_dir.clone(), server_cfg.clone())
    else {
        panic!("the daemon started over two roots of one name");
    };
    let missing = |e: &aigentic_server::ServerError| -> String {
        let Some(aigentic_server::ServerError::Workspace(
            aigentic_server::workspaces::WorkspaceError::NameTaken { roots, .. },
        )) = Some(e)
        else {
            panic!("not a clash: {e}");
        };
        for root in [&one, &two] {
            assert!(roots.contains(root), "{roots:?} misses {}", root.display());
        }
        e.to_string()
    };
    let message = missing(&refused);
    for root in [&one, &two] {
        assert!(message.contains(&root.display().to_string()), "{message}");
    }

    // A folder of its own, outside the workspace: the ad hoc project must
    // not stand in for one of the two.
    let bare = dir.path().join("bare");
    std::fs::create_dir_all(&bare).unwrap();
    let Err(embedded) = Server::embed_with(
        config,
        cfg_dir,
        bare,
        "steve",
        None,
        Arc::new(RecordingFactory {
            seen: Arc::new(Mutex::new(Vec::new())),
        }),
        Arc::new(NoReports),
    )
    .await
    else {
        panic!("the embedded daemon started over two roots of one name");
    };
    missing(&embedded);
}

/// a setup with no workspace is what it was: the same names in the
/// same order, `project()` still resolving them, and a block carrying
/// exactly those names.
#[tokio::test]
async fn a_setup_with_no_workspace_is_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let notes = project(dir.path(), "notes", "");
    let projects = vec![ProjectConfig {
        name: aigentic_server::workspaces::project_name(&notes),
        root: notes.clone(),
    }];
    assert_eq!(
        aigentic_server::workspaces::merge(projects.clone(), &[]),
        Ok(projects.clone()),
        "no workspace: the server.toml list, unchanged"
    );

    let mut d = daemon(dir.path(), projects, &[]).await;
    let name = &d.server.server.projects[0].name;
    assert_eq!(
        d.server.server.project(name).map(|p| p.root.clone()),
        Some(notes.clone())
    );
    let mut notices = d.steve.take_notices().unwrap();
    let id = created(&mut d.steve, name).await;
    let mark = d.seen.lock().unwrap().len();
    post(&mut d.steve, &mut notices, id, "hi").await;
    let block = block_since(&d.seen, mark).expect("the block");
    let expected = format!(
        "Projects in reach. This thread is in {name} (no workspace).\n\
         In no workspace: {name} (here) {}",
        notes.display()
    );
    assert_eq!(block, expected, "the names the model gets are unchanged");
}

/// a workspace project is named as its workspace qualifies it, in
/// the block the model reads as in the daemon's list.
#[tokio::test]
async fn the_projects_block_names_a_workspace_project_as_qualified() {
    let dir = tempfile::tempdir().unwrap();
    let web = project(dir.path(), "web", "");
    let workspace = "getscale";
    let leaf = aigentic_server::workspaces::project_name(&web);
    let mut d = daemon(dir.path(), Vec::new(), &[(workspace, vec![web.clone()])]).await;
    let name = format!("{workspace}/{leaf}");
    let mut notices = d.steve.take_notices().unwrap();
    let id = created(&mut d.steve, &name).await;
    let mark = d.seen.lock().unwrap().len();
    post(&mut d.steve, &mut notices, id, "hi").await;
    let block = block_since(&d.seen, mark).expect("the block");
    let expected = format!(
        "Projects in reach. This thread is in {name} ({workspace}).\n\
         {workspace}: {name} (here) {}",
        web.display()
    );
    assert_eq!(block, expected);
}

/// a collapsed project is named by its own name in a `related`
/// row, even though the entry addressing it carries the workspace's, and
/// that name reaches its root.
#[tokio::test]
async fn a_collapsed_project_keeps_its_listed_name_through_related() {
    let dir = tempfile::tempdir().unwrap();
    // Workspace `aigentic` holding a project of the same name: listed as
    // `aigentic`, not `aigentic/aigentic`.
    let collapsed = dir.path().join("aigentic");
    std::fs::create_dir_all(collapsed.join(".aigentic")).unwrap();
    std::fs::write(
        collapsed.join("aigentic.toml"),
        format!(
            "[project]\nname = {:?}\n[memory]\nenabled = false\n",
            collapsed.file_name().unwrap().to_str().unwrap()
        ),
    )
    .unwrap();
    let workspace = collapsed.file_name().unwrap().to_str().unwrap().to_owned();
    let entry = format!("{workspace}/{workspace}");
    let p1 = related_project(dir.path(), "p1", &[&entry]);
    let mut d = daemon(
        dir.path(),
        Vec::new(),
        &[
            (&workspace, vec![collapsed.clone()]),
            ("w1", vec![p1.clone()]),
        ],
    )
    .await;

    let mut notices = d.steve.take_notices().unwrap();
    let id = created(&mut d.steve, "w1/p1").await;
    let mark = d.seen.lock().unwrap().len();
    post(&mut d.steve, &mut notices, id, "hi").await;
    let block = block_since(&d.seen, mark).expect("the block");
    assert!(
        block.contains(&format!("Other workspaces: {workspace}: {workspace}")),
        "{block}"
    );
    assert!(!block.contains(&entry), "not the entry's own form: {block}");
    assert_eq!(
        d.server.threads.project_root(&workspace),
        Some(collapsed.clone()),
        "the listed name reaches the collapsed project's root"
    );
}

/// T6 (issue #129): a project `[project] related` declares shows on the
/// other-workspaces line under its `<workspace>/<project>` address, with
/// its brief, once — and without the key that line is unchanged.
#[tokio::test]
async fn a_related_project_shows_on_the_other_workspaces_line_with_its_brief() {
    // The declared link: `p1` names `v/r`, and `r` carries a brief.
    let dir = tempfile::tempdir().unwrap();
    let p2 = project(dir.path(), "p2", "");
    let q1 = project(dir.path(), "q1", "");
    let r = project(dir.path(), "r", "");
    let brief = "R's own brief.\n\nWhy it matters.\n";
    std::fs::write(r.join(".aigentic/brief.md"), brief).unwrap();
    let p1 = related_project(dir.path(), "p1", &["v/r"]);
    let mut d = daemon(
        dir.path(),
        vec![],
        &[
            ("v", vec![r.clone()]),
            ("w1", vec![p1.clone(), p2.clone()]),
            ("w2", vec![q1.clone()]),
        ],
    )
    .await;

    let mut notices = d.steve.take_notices().unwrap();
    let id = created(&mut d.steve, "p1").await;
    let mark = d.seen.lock().unwrap().len();
    post(&mut d.steve, &mut notices, id, "hi").await;
    let block = block_since(&d.seen, mark).expect("the block");
    // The one-liner comes from the fixture's brief, by the same rule the
    // block uses; the address is the workspace name and project name the
    // workspace file and the project file carry.
    let one = aigentic_runtime::brief::one_line(brief).unwrap();
    let expected = format!(
        "Projects in reach. This thread is in w1/p1 (w1).\n\
         w1: w1/p1 (here) {} · w1/p2 {}\n\
         Other workspaces: v: v/r — {one} · w2: w2/q1",
        p1.display(),
        p2.display()
    );
    assert_eq!(block, expected);
    // The project is listed once, under its address: no plain `r` row.
    assert_eq!(block.matches("v/r").count(), 1, "{block}");
    assert!(!block.contains("v: r"), "{block}");
    println!("the listing with a declared link:\n{block}");

    // The same rig without the key: line 3 lists the project's own name
    // and no brief, and nothing else differs.
    let plain = tempfile::tempdir().unwrap();
    let p2 = project(plain.path(), "p2", "");
    let q1 = project(plain.path(), "q1", "");
    let r = project(plain.path(), "r", "");
    std::fs::write(r.join(".aigentic/brief.md"), brief).unwrap();
    let p1 = project(plain.path(), "p1", "");
    let mut d = daemon(
        plain.path(),
        vec![],
        &[
            ("v", vec![r.clone()]),
            ("w1", vec![p1.clone(), p2.clone()]),
            ("w2", vec![q1.clone()]),
        ],
    )
    .await;
    let mut notices = d.steve.take_notices().unwrap();
    let id = created(&mut d.steve, "p1").await;
    let mark = d.seen.lock().unwrap().len();
    post(&mut d.steve, &mut notices, id, "hi").await;
    let block = block_since(&d.seen, mark).expect("the block");
    let expected = format!(
        "Projects in reach. This thread is in w1/p1 (w1).\n\
         w1: w1/p1 (here) {} · w1/p2 {}\n\
         Other workspaces: v: v/r · w2: w2/q1",
        p1.display(),
        p2.display()
    );
    assert_eq!(block, expected);
    println!("the same rig without the key:\n{block}");
}

/// A thread whose log names `web`, at a root the daemon lists as
/// `getscale/web`, follows its root to that name: the log is not
/// rewritten, and the thread counts under the row its root is listed as.
#[tokio::test]
async fn an_old_thread_follows_its_root_to_its_new_name() {
    let dir = tempfile::tempdir().unwrap();
    let web = project(dir.path(), "web", "");
    let d = daemon(dir.path(), vec![], &[("getscale", vec![web.clone()])]).await;
    let listed = listed_at(&d.server, &web).expect("the root is listed");

    let id = log_started_at(&d.threads_base, "web", Some("web"), &web);
    assert_eq!(
        d.server.threads.project_of(id).as_deref(),
        Some(listed.as_str()),
        "the logged name follows its root"
    );
    let rows = d.server.threads.projects();
    let (_, _, count) = rows
        .iter()
        .find(|(name, _, _)| *name == listed)
        .expect("the row");
    assert_eq!(*count, 1, "counted under {listed}: {rows:?}");
}

/// A logged switch to a bare `web` is resolved by the root the switch
/// recorded, and a live switch sent as `web` counts the thread under the
/// listed name at once, before any rescan.
#[tokio::test]
async fn a_switched_name_is_canonicalised() {
    // Two `web`s, each in its own workspace: only the switch's root can
    // say which one it meant.
    let dir = tempfile::tempdir().unwrap();
    let ours = project(dir.path(), "web-ours", "");
    let theirs = project(dir.path(), "web-theirs", "");
    // The projects' own `[project] name` is what a workspace entry
    // qualifies, so both are called `web` on disk.
    for root in [&ours, &theirs] {
        std::fs::write(root.join("aigentic.toml"), "[project]\nname = \"web\"\n").unwrap();
    }
    let d = daemon(
        dir.path(),
        vec![],
        &[
            ("getscale", vec![ours.clone()]),
            ("acme", vec![theirs.clone()]),
        ],
    )
    .await;
    let target = listed_at(&d.server, &theirs).expect("the root is listed");
    let id = log_started_at(&d.threads_base, "web", Some("web"), &ours);
    log_switched(&d.threads_base, "web", id, "web", &theirs);
    assert_eq!(
        d.server.threads.project_of(id).as_deref(),
        Some(target.as_str()),
        "the switch's root decides between two `web`s"
    );

    // One `web`: a switch sent as the bare leaf lands on the listed name
    // in the live table, so the count is right before any rescan.
    let one = tempfile::tempdir().unwrap();
    let web = project(one.path(), "web", "");
    let mut live = daemon(one.path(), vec![], &[("getscale", vec![web.clone()])]).await;
    let listed = listed_at(&live.server, &web).expect("the root is listed");
    let id = created(&mut live.steve, &listed).await;
    assert_eq!(
        live.steve
            .request(Request::SwitchProject {
                thread: id,
                project: "web".into(),
            })
            .await
            .unwrap(),
        Response::Ok
    );
    assert_eq!(
        live.server.threads.project_of(id).as_deref(),
        Some(listed.as_str()),
        "a live switch to the bare leaf"
    );
    let rows = live.server.threads.projects();
    let (_, _, count) = rows
        .iter()
        .find(|(name, _, _)| *name == listed)
        .expect("the row");
    assert_eq!(*count, 1, "counted under {listed}: {rows:?}");
}

/// A legacy `_none` thread takes its project's listed name by root; at a
/// root in no workspace it keeps the leaf (and counts nowhere), and a
/// leaf another root's project answers still yields no project.
#[tokio::test]
async fn a_legacy_none_thread_counts_under_its_projects_listed_name() {
    let dir = tempfile::tempdir().unwrap();
    let web = project(dir.path(), "web", "");
    let loose = dir.path().join("loose");
    std::fs::create_dir_all(&loose).unwrap();
    // A project called `other` at a root that is not this log's root.
    let elsewhere = dir.path().join("elsewhere-other");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let other = dir.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    let d = daemon(
        dir.path(),
        vec![ProjectConfig {
            name: "other".into(),
            root: elsewhere,
        }],
        &[("getscale", vec![web.clone()])],
    )
    .await;
    let listed = listed_at(&d.server, &web).expect("the root is listed");

    let id = log_started_at(&d.threads_base, "_none", Some("_none"), &web);
    assert_eq!(
        d.server.threads.project_of(id).as_deref(),
        Some(listed.as_str()),
        "the root names the project"
    );
    let rows = d.server.threads.projects();
    let (_, _, count) = rows
        .iter()
        .find(|(name, _, _)| *name == listed)
        .expect("the row");
    assert_eq!(*count, 1, "counted under {listed}: {rows:?}");

    let loose_id = log_started_at(&d.threads_base, "_none", Some("_none"), &loose);
    assert_eq!(
        d.server.threads.project_of(loose_id).as_deref(),
        Some("loose"),
        "no workspace: the leaf stands"
    );
    let rows = d.server.threads.projects();
    let total: u64 = rows.iter().map(|(_, _, n)| n).sum();
    assert_eq!(total, 1, "the leaf counts in no row: {rows:?}");

    let clashing = log_started_at(&d.threads_base, "_none", Some("_none"), &other);
    assert_eq!(
        d.server.threads.project_of(clashing),
        None,
        "another root's `other` is not this thread's project"
    );
}

/// A thread made by the bare leaf name is filed under the name the daemon
/// lists the project by: its row counts it, and a listing by either name
/// finds it. The log keeps the name it was asked for.
#[tokio::test]
async fn a_thread_created_by_a_leaf_name_is_filed_under_the_listed_name() {
    let dir = tempfile::tempdir().unwrap();
    let web = project(dir.path(), "web", "");
    let d = daemon(dir.path(), vec![], &[("getscale", vec![web.clone()])]).await;
    let listed = listed_at(&d.server, &web).expect("the root is listed");
    assert_ne!(listed, "web", "a workspace project is qualified");

    let Response::Thread { thread } = d
        .steve
        .request(Request::CreateThread {
            project: "web".into(),
        })
        .await
        .unwrap()
    else {
        panic!("a thread")
    };
    let id = thread.id;
    assert_eq!(
        d.server.threads.project_of(id).as_deref(),
        Some(listed.as_str()),
        "the leaf is filed under the listed name"
    );
    let rows = d.server.threads.projects();
    let (_, _, count) = rows
        .iter()
        .find(|(name, _, _)| *name == listed)
        .expect("the row");
    assert_eq!(*count, 1, "counted under {listed}: {rows:?}");

    // Either name lists it: the leaf resolves to the same project.
    for name in [listed.as_str(), "web"] {
        let Response::Threads { threads } = d
            .steve
            .request(Request::ListThreads {
                project: Some(name.into()),
            })
            .await
            .unwrap()
        else {
            panic!("a listing")
        };
        assert!(threads.iter().any(|t| t.id == id), "listed by {name}");
    }

    let log_dir = d.server.threads.dir_of(id).expect("a log");
    let events = ThreadLog::open(&log_dir, id).unwrap().read_all().unwrap();
    let started: ThreadStartedPayload =
        serde_json::from_value(events.first().expect("thread_started").payload.clone()).unwrap();
    assert_eq!(
        started.project.as_deref(),
        Some("web"),
        "the log records the name it was asked for"
    );
}

/// A front thread opened by the bare leaf name is filed under the listed
/// name, and adds no second row.
#[tokio::test]
async fn a_front_opened_by_a_leaf_name_is_filed_under_the_listed_name() {
    let dir = tempfile::tempdir().unwrap();
    let web = project(dir.path(), "web", "");
    let d = daemon(dir.path(), vec![], &[("getscale", vec![web.clone()])]).await;
    let listed = listed_at(&d.server, &web).expect("the root is listed");

    let Response::Front { thread, .. } = d
        .steve
        .request(Request::Front {
            project: "web".into(),
            here: Some("web".into()),
            asked: None,
        })
        .await
        .unwrap()
    else {
        panic!("a front")
    };
    assert_eq!(
        d.server.threads.project_of(thread.id).as_deref(),
        Some(listed.as_str()),
        "a front sent as the leaf is filed under the listed name"
    );
    let rows = d.server.threads.projects();
    let (_, _, count) = rows
        .iter()
        .find(|(name, _, _)| *name == listed)
        .expect("the row");
    assert_eq!(*count, 1, "the front counts under {listed}: {rows:?}");
    assert_eq!(
        rows.iter().filter(|(name, _, _)| *name == listed).count(),
        1,
        "one row per project: {rows:?}"
    );
}

/// A `Build` sent as a workspace project's leaf name finds the unfinished
/// run that name's project already has, rather than starting a second one.
#[tokio::test]
async fn a_second_build_by_a_leaf_name_resumes_the_run_it_made() {
    let dir = tempfile::tempdir().unwrap();
    let web = project(dir.path(), "web", "");
    let d = daemon(dir.path(), vec![], &[("getscale", vec![web.clone()])]).await;
    let listed = listed_at(&d.server, &web).expect("the root is listed");

    // A lead made earlier, by the leaf name its folder carries, and left
    // unfinished: exactly what a first `Build` leaves behind.
    let lead = log_started_at(&d.threads_base, "web", Some("web"), &web);
    log_run_started(&d.threads_base, "web", lead, 7);

    // Both spellings of the project find that one lead.
    for asked in ["web", listed.as_str()] {
        let (found, resumed) = d
            .server
            .threads
            .build_run(asked, 7, None, Author::User(UserId("steve".into())))
            .await
            .unwrap_or_else(|e| panic!("a build by {asked}: {e:?}"));
        assert_eq!(found, lead, "build by {asked} resumes the run it made");
        assert!(resumed, "build by {asked} resumed rather than started");
    }

    let rows = d.server.threads.projects();
    let (_, _, count) = rows
        .iter()
        .find(|(name, _, _)| *name == listed)
        .expect("the row");
    assert_eq!(*count, 1, "one thread under {listed}: {rows:?}");
}
