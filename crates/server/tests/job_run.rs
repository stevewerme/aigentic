//! An approved job runs on the daemon: a front thread in `auto` proposes
//! one, a person says yes, and the job's own thread works from the brief
//! until it ends. The front thread then holds exactly one `job_finished`
//! saying how it went and what it cost. A daemon that approves nothing
//! makes no job thread and writes no return.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aigentic_api::{Notice, Response, ThreadState};
use aigentic_runtime::aigentic_core::{
    Author, Capabilities, CompletionRequest, ContentBlock, Event, EventKind, Message, Provider,
    ProviderError, ProviderEvent, ToolCall, Usage, UserId,
};
use aigentic_runtime::aigentic_log::{
    AssistantMessagePayload, JobFinishedPayload, JobOutcome, ThreadLog, ThreadStartedPayload,
    ToolResultPayload, TurnEndedPayload, UserMessagePayload,
};
use aigentic_runtime::harness_tools::start_job_spec;
use aigentic_runtime::{INTERRUPTED, Mode, SwitchAnswer, SwitchCtx};
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

const FAILURE: &str = "the stub's model fell over";

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
    router: Router,
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
        router,
        _dir: dir,
    }
}

async fn ask(mailbox: &Mailbox, mail: impl FnOnce(oneshot::Sender<Response>) -> Mail) -> Response {
    let (reply, rx) = oneshot::channel();
    mailbox.send(mail(reply)).unwrap();
    rx.await.unwrap()
}

/// A front thread in `auto`, opened, with its notices.
async fn front(rig: &Rig) -> (Ulid, Mailbox, mpsc::UnboundedReceiver<Notice>) {
    let id = rig.table.create("p", steve(), true).await.unwrap().id;
    let (mailbox, _) = rig.table.open(id).await.unwrap();
    assert_eq!(
        ask(&mailbox, |reply| Mail::SetMode {
            mode: Mode::Auto,
            reply
        })
        .await,
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

/// Post on the front, say yes to the job it proposes, and wait for the
/// job's return to land on it.
async fn approve_and_wait(rig: &Rig) -> Ulid {
    let (id, mailbox, mut notices) = front(rig).await;
    post(&mailbox, "Who calls parse_config?").await;
    let parked = until(&mut notices, |n| {
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
        ask(&mailbox, |reply| Mail::AnswerSwitch {
            by: steve(),
            call_id,
            answer: SwitchAnswer::Yes,
            ctx: SwitchCtx::none(),
            reply,
        })
        .await,
        Response::Ok
    );
    until(
        &mut notices,
        |n| matches!(n, Notice::Event { event, .. } if event.kind == EventKind::JobFinished),
    )
    .await;
    id
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

/// The cost the runtime stamped on each of the job's model calls, summed.
fn stamped_cost(events: &[Event]) -> f64 {
    events
        .iter()
        .filter(|e| e.kind == EventKind::AssistantMessage)
        .filter_map(|e| serde_json::from_value::<AssistantMessagePayload>(e.payload.clone()).ok())
        .filter_map(|p| p.usage.and_then(|u| u.cost_usd))
        .sum()
}

fn last_turn_end(events: &[Event]) -> TurnEndedPayload {
    let ended = events
        .iter()
        .rev()
        .find(|e| e.kind == EventKind::TurnEnded)
        .expect("a turn ended");
    serde_json::from_value(ended.payload.clone()).unwrap()
}

#[tokio::test]
async fn an_approved_job_runs_and_its_return_lands_once() {
    let rig = rig(Router::new(
        proposes_a_job(),
        vec![
            call("job-call", "bash", json!({"command": "echo looking"})),
            say(JOB_WORDS),
        ],
    ));
    let front = approve_and_wait(&rig).await;

    let jobs = job_threads(&rig.threads);
    assert_eq!(jobs.len(), 1, "exactly one job thread: {jobs:?}");
    let (child, started) = &jobs[0];
    assert_eq!(started.parent_thread, Some(front));
    assert_eq!(started.step, None);
    assert_eq!(started.project.as_deref(), Some("p"));

    let child_events = events(&rig.threads, *child);
    let seed: UserMessagePayload = serde_json::from_value(
        child_events
            .iter()
            .find(|e| e.kind == EventKind::UserMessage)
            .unwrap()
            .payload
            .clone(),
    )
    .unwrap();
    assert_eq!(seed.blocks, vec![ContentBlock::Text(BRIEF.into())]);
    let ran: Vec<ToolResultPayload> = child_events
        .iter()
        .filter(|e| e.kind == EventKind::ToolResult)
        .map(|e| serde_json::from_value(e.payload.clone()).unwrap())
        .collect();
    assert_eq!(ran.len(), 1, "{ran:?}");
    assert_eq!(ran[0].result.id, "job-call");
    assert!(
        !ran[0].result.is_error,
        "an `auto` front's job runs it: {ran:?}"
    );

    let front_events = events(&rig.threads, front);
    let posted = returns(&front_events);
    assert_eq!(posted.len(), 1, "one return: {posted:?}");
    assert_eq!(posted[0].job_thread, *child);
    assert_eq!(posted[0].outcome, JobOutcome::Finished);
    assert_eq!(posted[0].summary, JOB_WORDS);
    let cost = stamped_cost(&child_events);
    assert!(cost > 0.0, "the profile is priced");
    assert_eq!(posted[0].cost_usd, cost);
}

#[tokio::test]
async fn a_job_that_falls_over_says_so() {
    let rig = rig(Router::new(
        proposes_a_job(),
        vec![vec![ProviderEvent::Error(ProviderError::Protocol(
            FAILURE.into(),
        ))]],
    ));
    let front = approve_and_wait(&rig).await;
    let posted = returns(&events(&rig.threads, front));
    assert_eq!(posted.len(), 1, "one return: {posted:?}");
    assert_eq!(posted[0].outcome, JobOutcome::Failed);
    assert!(posted[0].summary.contains(FAILURE), "{}", posted[0].summary);
}

#[tokio::test]
async fn a_job_that_never_ends_is_stopped_and_reported() {
    // The job's script is empty, so its first model call never answers.
    let rig = rig(Router::new(proposes_a_job(), Vec::new()));
    rig.table
        .with_job_limits(Duration::from_millis(20), Duration::from_millis(100));
    let front = approve_and_wait(&rig).await;

    let posted = returns(&events(&rig.threads, front));
    assert_eq!(posted.len(), 1, "one return: {posted:?}");
    assert_eq!(posted[0].outcome, JobOutcome::Stopped);
    let child_events = events(&rig.threads, posted[0].job_thread);
    assert_eq!(
        child_events.last().map(|e| e.kind),
        Some(EventKind::TurnEnded),
        "the interrupted turn is the job's last word"
    );
    assert_eq!(last_turn_end(&child_events).reason, INTERRUPTED);
}

#[tokio::test]
async fn nothing_new_happens_when_no_job_is_approved() {
    let rig = rig(Router::new(vec![say("Nothing to start.")], Vec::new()));
    let (front, mailbox, mut notices) = front(&rig).await;
    post(&mailbox, "Hello").await;
    until(
        &mut notices,
        |n| matches!(n, Notice::Event { event, .. } if event.kind == EventKind::TurnEnded),
    )
    .await;

    assert!(returns(&events(&rig.threads, front)).is_empty());
    assert!(job_threads(&rig.threads).is_empty());
    let offered = rig.router.tools.lock().unwrap().clone();
    let name = start_job_spec().name;
    assert_eq!(
        offered[0].iter().filter(|t| **t == name).count(),
        1,
        "{offered:?}"
    );
}

/// A read-only job's model is offered no tool that writes a file, and
/// keeps `bash` so it can still search.
#[tokio::test]
async fn a_read_only_jobs_thread_is_offered_no_write_tool() {
    let rig = rig(Router::new(proposes(true), vec![say(JOB_WORDS)]));
    approve_and_wait(&rig).await;
    let offered = rig.router.job_tools.lock().unwrap().clone();
    let front = rig.router.tools.lock().unwrap().clone();
    assert!(front[0].iter().any(|t| t == "write_file"), "{front:?}");
    assert!(
        !offered[0]
            .iter()
            .any(|t| t == "write_file" || t == "edit_file"),
        "{offered:?}"
    );
    assert!(offered[0].iter().any(|t| t == "bash"), "{offered:?}");
}
