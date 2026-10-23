//! A job's asks reach the front thread. A front in `manual` approves a
//! job whose command the rules would ask about; the job parks, the front
//! thread's watcher sees the ask tagged with the job, and an answer sent
//! to the job's own thread lets it finish and report back.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aigentic_api::{Notice, Response, ThreadState};
use aigentic_runtime::aigentic_core::{
    Author, Capabilities, CompletionRequest, ContentBlock, Event, EventKind, Message, Provider,
    ProviderEvent, ToolCall, Usage, UserId,
};
use aigentic_runtime::aigentic_log::{
    JobFinishedPayload, JobOutcome, ThreadLog, ThreadStartedPayload, ToolResultPayload,
};
use aigentic_runtime::{Mode, SwitchAnswer, SwitchCtx};
use aigentic_server::build::{BuildError, ProviderFactory};
use aigentic_server::config::{Config, ProjectConfig, ServerConfig, UserConfig};
use aigentic_server::{Mail, Mailbox, NoReports, ThreadTable};
use futures_core::Stream;
use serde_json::json;
use tokio::sync::{mpsc, oneshot};
use ulid::Ulid;

/// The brief the front's model writes; a request whose messages carry it
/// is the job's, so the provider answers it from the job's script.
const BRIEF: &str = "Find every caller of `parse_config` and list them.";

const JOB_WORDS: &str = "Three callers: main, reload and the test helper.";

type Script = Arc<Mutex<VecDeque<Vec<ProviderEvent>>>>;

/// Two scripts behind one provider: the job's thread reads the job's,
/// every other request the front's. An empty script never answers, so a
/// thread that asks again stays running.
#[derive(Clone)]
struct Router {
    front: Script,
    job: Script,
    /// The tool names each request offered: the front's, then the job's.
    tools: Arc<Mutex<Vec<Vec<String>>>>,
    job_tools: Arc<Mutex<Vec<Vec<String>>>>,
}

impl Router {
    fn new(front: Vec<Vec<ProviderEvent>>, job: Vec<Vec<ProviderEvent>>) -> Self {
        Self {
            front: Arc::new(Mutex::new(front.into())),
            job: Arc::new(Mutex::new(job.into())),
            tools: Arc::new(Mutex::new(Vec::new())),
            job_tools: Arc::new(Mutex::new(Vec::new())),
        }
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
        .join("\n")
}

impl Provider for Router {
    fn complete(
        &self,
        request: &CompletionRequest<'_>,
    ) -> Pin<Box<dyn Stream<Item = ProviderEvent> + Send + '_>> {
        let job = texts(request.messages).contains(BRIEF);
        let names = request.tools.iter().map(|t| t.name.clone()).collect();
        if job { &self.job_tools } else { &self.tools }
            .lock()
            .unwrap()
            .push(names);
        let script = if job { &self.job } else { &self.front };
        match script.lock().unwrap().pop_front() {
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

impl ProviderFactory for Router {
    fn build(&self, _: &str) -> Result<(Box<dyn Provider>, String), BuildError> {
        Ok((Box::new(self.clone()), "router".into()))
    }
}

fn usage() -> ProviderEvent {
    ProviderEvent::Usage(Usage {
        input_tokens: 1_000,
        output_tokens: 100,
        ..Usage::default()
    })
}

fn done(reason: &str) -> ProviderEvent {
    ProviderEvent::Done {
        finish_reason: reason.into(),
    }
}

fn say(text: &str) -> Vec<ProviderEvent> {
    vec![ProviderEvent::TextDelta(text.into()), usage(), done("stop")]
}

fn call(id: &str, name: &str, args: serde_json::Value) -> Vec<ProviderEvent> {
    vec![
        ProviderEvent::ToolCall(ToolCall {
            id: id.into(),
            name: name.into(),
            args,
        }),
        usage(),
        done("tool_use"),
    ]
}

/// The front's model: propose the job, then say it is under way.
fn proposes_a_job() -> Vec<Vec<ProviderEvent>> {
    proposes(false)
}

fn proposes(read_only: bool) -> Vec<Vec<ProviderEvent>> {
    vec![
        call(
            "job-1",
            "start_job",
            json!({"task": "list the callers", "brief": BRIEF, "read_only": read_only}),
        ),
        say("The job is under way."),
    ]
}

fn steve() -> Author {
    Author::User(UserId("steve".into()))
}

struct Rig {
    table: Arc<ThreadTable>,
    threads: PathBuf,
    _dir: tempfile::TempDir,
}

/// A daemon over one project `p`, priced, with `router` answering every
/// model call.
fn rig(router: Router) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("p");
    std::fs::create_dir_all(root.join(".aigentic")).unwrap();
    std::fs::write(
        root.join("aigentic.toml"),
        "[project]\nname = \"p\"\n[participants]\nsteve = \"admin\"\n[memory]\nenabled = false\n",
    )
    .unwrap();
    let threads = dir.path().join("threads");
    let config = Config::parse(
        "default_profile = \"a\"\n[profiles.a]\nbase_url = \"u\"\nmodel = \"m\"\napi_key_env = \"K\"\n[profiles.a.prices]\ninput = 3.0\noutput = 15.0\n",
    )
    .unwrap();
    let server = ServerConfig {
        listen: "unix".into(),
        idle_unload_secs: 3600,
        users: vec![UserConfig {
            name: "steve".into(),
            token_env: None,
            token: Some("t".into()),
        }],
        projects: vec![ProjectConfig {
            name: "p".into(),
            root,
        }],
        resume_runs: false,
    };
    let table = Arc::new(ThreadTable::new(
        Arc::new(config),
        dir.path().join("cfg"),
        Arc::new(server),
        Arc::new(router.clone()),
        Arc::new(NoReports),
        threads.clone(),
        aigentic_server::awake::detect(false),
    ));
    Rig {
        table,
        threads,
        _dir: dir,
    }
}

async fn ask(mailbox: &Mailbox, mail: impl FnOnce(oneshot::Sender<Response>) -> Mail) -> Response {
    let (reply, rx) = oneshot::channel();
    mailbox.send(mail(reply)).unwrap();
    rx.await.unwrap()
}

/// A front thread in `mode`, opened, with its notices.
async fn front(rig: &Rig, mode: Mode) -> (Ulid, Mailbox, mpsc::UnboundedReceiver<Notice>) {
    let id = rig.table.create("p", steve(), true).await.unwrap().id;
    let (mailbox, _) = rig.table.open(id).await.unwrap();
    assert_eq!(
        ask(&mailbox, |reply| Mail::SetMode { mode, reply }).await,
        Response::Ok
    );
    let (tx, rx) = mpsc::unbounded_channel();
    let (reply, opened) = oneshot::channel();
    mailbox
        .send(Mail::Subscribe {
            from_seq: 0,
            notices: tx,
            reply,
        })
        .unwrap();
    opened.await.unwrap();
    (id, mailbox, rx)
}

async fn post(mailbox: &Mailbox, text: &str) {
    assert_eq!(
        ask(mailbox, |reply| Mail::Post {
            author: steve(),
            blocks: vec![ContentBlock::Text(text.into())],
            interrupt: false,
            reply,
        })
        .await,
        Response::Ok
    );
}

/// The first notice `want` accepts, within the deadline.
async fn until(rx: &mut mpsc::UnboundedReceiver<Notice>, want: impl Fn(&Notice) -> bool) -> Notice {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let notice = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .expect("the notice in time")
            .expect("the thread is open");
        if want(&notice) {
            return notice;
        }
    }
}

/// Post on the front, and say yes to the job it proposes.
async fn approve(mailbox: &Mailbox, notices: &mut mpsc::UnboundedReceiver<Notice>) {
    post(mailbox, "Who calls parse_config?").await;
    let parked = until(notices, |n| {
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
        state: ThreadState::AwaitingSwitch { call_id, .. },
        ..
    } = parked
    else {
        unreachable!()
    };
    assert_eq!(
        ask(mailbox, |reply| Mail::AnswerSwitch {
            by: steve(),
            call_id,
            answer: SwitchAnswer::Yes,
            ctx: SwitchCtx::none(),
            reply,
        })
        .await,
        Response::Ok
    );
}

fn events(threads: &Path, id: Ulid) -> Vec<Event> {
    ThreadLog::open(threads, id).unwrap().read_all().unwrap()
}

fn returns(events: &[Event]) -> Vec<JobFinishedPayload> {
    events
        .iter()
        .filter(|e| e.kind == EventKind::JobFinished)
        .map(|e| serde_json::from_value(e.payload.clone()).unwrap())
        .collect()
}

/// Every thread in the directory whose masthead says it is a job's.
fn job_threads(threads: &Path) -> Vec<(Ulid, ThreadStartedPayload)> {
    std::fs::read_dir(threads)
        .unwrap()
        .filter_map(|entry| {
            let name = entry.unwrap().file_name().into_string().unwrap();
            name.strip_suffix(".jsonl")?.parse::<Ulid>().ok()
        })
        .filter_map(|id| {
            let started = events(threads, id)
                .into_iter()
                .find(|e| e.kind == EventKind::ThreadStarted)?;
            let payload: ThreadStartedPayload = serde_json::from_value(started.payload).ok()?;
            payload.job.then_some((id, payload))
        })
        .collect()
}

/// A job's write that the rules would ask about, in a `manual` front.
const WRITE: &str = "touch found.txt";

#[tokio::test]
async fn a_jobs_ask_reaches_the_front_and_an_answer_on_the_job_lets_it_finish() {
    let rig = rig(Router::new(
        proposes_a_job(),
        vec![
            call("job-call", "bash", json!({ "command": WRITE })),
            say(JOB_WORDS),
        ],
    ));
    let (front, mailbox, mut notices) = front(&rig, Mode::Manual).await;
    approve(&mailbox, &mut notices).await;

    let asked = until(&mut notices, |n| {
        matches!(
            n,
            Notice::Job { notice, .. }
                if matches!(**notice, Notice::State { state: ThreadState::AwaitingApproval { .. }, .. })
        )
    })
    .await;
    let Notice::Job {
        thread,
        job,
        number,
        notice,
    } = asked
    else {
        unreachable!()
    };
    assert_eq!(thread, front, "the notice is the front thread's");
    assert_eq!(number, 1, "the front's first job");
    let jobs = job_threads(&rig.threads);
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    assert_eq!(job, jobs[0].0, "tagged with the job's own thread");
    let Notice::State {
        thread: waiting_on,
        state: ThreadState::AwaitingApproval { call_id, call, .. },
    } = *notice
    else {
        unreachable!()
    };
    assert_eq!(waiting_on, job, "the job's own state, as the job sent it");
    assert_eq!(call.args["command"], WRITE);

    // Answer on the job's thread, which its driver holds open.
    let (job_box, _) = rig.table.open(job).await.unwrap();
    assert_eq!(
        ask(&job_box, |reply| Mail::Decide {
            by: steve(),
            call_id: call_id.clone(),
            allow: true,
            session: false,
            prefix: None,
            reason: None,
            reply,
        })
        .await,
        Response::Ok
    );
    rig.table.close(job);

    until(&mut notices, |n| {
        matches!(n, Notice::Job { notice, .. }
            if matches!(&**notice, Notice::Event { event, .. } if event.kind == EventKind::PermissionDecided))
    })
    .await;
    until(
        &mut notices,
        |n| matches!(n, Notice::Event { event, .. } if event.kind == EventKind::JobFinished),
    )
    .await;

    let child_events = events(&rig.threads, job);
    let ran: Vec<ToolResultPayload> = child_events
        .iter()
        .filter(|e| e.kind == EventKind::ToolResult)
        .map(|e| serde_json::from_value(e.payload.clone()).unwrap())
        .collect();
    assert_eq!(ran.len(), 1, "{ran:?}");
    assert!(!ran[0].result.is_error, "the person allowed it: {ran:?}");
    let posted = returns(&events(&rig.threads, front));
    assert_eq!(posted.len(), 1, "one return: {posted:?}");
    assert_eq!(posted[0].outcome, JobOutcome::Finished);
}

#[tokio::test]
async fn the_front_thread_sees_none_of_the_jobs_other_traffic() {
    let rig = rig(Router::new(proposes_a_job(), vec![say(JOB_WORDS)]));
    let (_, mailbox, mut notices) = front(&rig, Mode::Auto).await;
    approve(&mailbox, &mut notices).await;
    let mut forwarded = Vec::new();
    loop {
        let notice = until(&mut notices, |_| true).await;
        if let Notice::Job { notice, .. } = &notice {
            forwarded.push((**notice).clone());
        }
        if matches!(&notice, Notice::Event { event, .. } if event.kind == EventKind::JobFinished) {
            break;
        }
    }
    assert!(
        !forwarded.is_empty(),
        "the job's state changes are forwarded"
    );
    assert!(
        forwarded.iter().all(|n| matches!(n, Notice::State { .. })),
        "only state reaches the front for a job that asks nothing: {forwarded:?}"
    );
}
