//! Shared test doubles: a scripted provider and a recording echo tool.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use aigentic_core::{
    AgentId, Author, BoxFuture, Capabilities, CompletionRequest, Message, Provider, ProviderEvent,
    RiskClass, Tool, ToolCall, ToolError, ToolOutput, UserId,
};
use aigentic_log::ThreadLog;
use aigentic_runtime::{Layers, Runtime, audit_tool_results};
use aigentic_tools::ToolRegistry;
use futures_core::Stream;
use serde_json::json;
use tokio::sync::oneshot;

/// Every request's messages, as the provider saw them.
pub type Seen = Arc<Mutex<Vec<Vec<Message>>>>;

/// One step of a `gated` script: an event, or a gate the stream parks
/// at, so a test can post into the turn's inbox mid-call (issue #33).
pub enum Step {
    Event(ProviderEvent),
    Gate(Gate),
}

/// The stream's side of a [`Step::Gate`]: signals its [`Parked`] when
/// the stream reaches it, then holds until the test opens it.
pub struct Gate {
    parked: Mutex<Option<oneshot::Sender<()>>>,
    open: Arc<AtomicBool>,
}

impl Gate {
    /// Signal the test, then hold the stream until it opens.
    async fn hold(&self) {
        if let Some(parked) = self.parked.lock().unwrap().take() {
            let _ = parked.send(());
        }
        while !self.open.load(Ordering::Relaxed) {
            tokio::task::yield_now().await;
        }
    }
}

/// The test's side of a [`Gate`]: `wait` resolves once the stream has
/// parked — the call is live, mid-reply — and `open` lets it resume.
pub struct Parked {
    parked: Option<oneshot::Receiver<()>>,
    open: Arc<AtomicBool>,
}

impl Parked {
    pub async fn wait(&mut self) {
        if let Some(parked) = self.parked.take() {
            let _ = parked.await;
        }
    }
    pub fn open(&self) {
        self.open.store(true, Ordering::Relaxed);
    }
}

/// A gate and the test's handle on it.
pub fn gate() -> (Gate, Parked) {
    let (tx, rx) = oneshot::channel();
    let open = Arc::new(AtomicBool::new(false));
    (
        Gate {
            parked: Mutex::new(Some(tx)),
            open: open.clone(),
        },
        Parked {
            parked: Some(rx),
            open,
        },
    )
}

/// How a scripted double counts, and what it reports (issue #52).
#[derive(Clone, Copy)]
enum Count {
    /// The fixed 7 the older tests were written against, and no usage
    /// reported: a provider whose numbers say nothing about tokenizers.
    Fixed,
    /// The runtime's own estimator, with no usage reported, so a test can
    /// see what the sweep does when it has nothing to calibrate against.
    Estimated,
    /// The estimator, reporting `k ×` its own count of the request it was
    /// sent, tools included — the runtime's model of a backend whose
    /// tokenizer is `k` times the estimate's.
    Reported(f64),
}

impl Count {
    fn tokens(&self, context: &[Message]) -> u64 {
        match self {
            Count::Fixed => 7,
            Count::Estimated | Count::Reported(_) => estimate(context),
        }
    }

    /// What the reply reports for this request, when it reports at all.
    fn reported(&self, request: &CompletionRequest<'_>) -> Option<u64> {
        match self {
            Count::Fixed | Count::Estimated => None,
            Count::Reported(k) => Some(
                (*k * (estimate(request.messages) + aigentic_runtime::schemas_tokens(request.tools))
                    as f64)
                    .round() as u64,
            ),
        }
    }
}

/// Replays scripted responses in order and records every request's messages.
pub struct ScriptedProvider {
    script: Mutex<VecDeque<Vec<Step>>>,
    seen: Seen,
    count: Count,
}

pub fn scripted(script: Vec<Vec<ProviderEvent>>) -> (Box<dyn Provider>, Seen) {
    gated(
        script
            .into_iter()
            .map(|reply| reply.into_iter().map(Step::Event).collect())
            .collect(),
    )
}

fn plain(script: Vec<Vec<ProviderEvent>>) -> VecDeque<Vec<Step>> {
    script
        .into_iter()
        .map(|reply| reply.into_iter().map(Step::Event).collect())
        .collect()
}

/// `scripted`, but the provider counts with the runtime's own estimator
/// instead of the fixed 7. Its replies report nothing, so the runtime's
/// calibration never learns anything from it (issue #52, T4).
pub fn counting_scripted(script: Vec<Vec<ProviderEvent>>) -> (Box<dyn Provider>, Seen) {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let p = ScriptedProvider {
        script: Mutex::new(plain(script)),
        seen: seen.clone(),
        count: Count::Estimated,
    };
    (Box::new(p), seen)
}

/// `scripted`, but the provider counts with the runtime's own estimator and
/// every reply reports `k ×` its own count of the request it was sent,
/// tools included (issue #52): a faithful stand-in for a backend whose
/// tokenizer is `k` times the estimate's, so a test can watch the
/// calibration learn to price a context as that backend counts it.
pub fn reporting_scripted(script: Vec<Vec<ProviderEvent>>, k: f64) -> (Box<dyn Provider>, Seen) {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let p = ScriptedProvider {
        script: Mutex::new(plain(script)),
        seen: seen.clone(),
        count: Count::Reported(k),
    };
    (Box::new(p), seen)
}

/// `scripted`, but a reply is a list of steps, and a step can be a
/// [`gate`]: the stream parks there until the test opens it, so a
/// message can land between two deltas of one reply.
pub fn gated(script: Vec<Vec<Step>>) -> (Box<dyn Provider>, Seen) {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let p = ScriptedProvider {
        script: Mutex::new(script.into()),
        seen: seen.clone(),
        count: Count::Fixed,
    };
    (Box::new(p), seen)
}

impl Provider for ScriptedProvider {
    fn complete(
        &self,
        request: &CompletionRequest<'_>,
    ) -> Pin<Box<dyn Stream<Item = ProviderEvent> + Send + '_>> {
        self.seen.lock().unwrap().push(request.messages.to_vec());
        let mut steps: VecDeque<Step> = self
            .script
            .lock()
            .unwrap()
            .pop_front()
            .expect("script exhausted")
            .into();
        if let Some(counted) = self.count.reported(request) {
            steps.push_back(Step::Event(usage(counted, 1)));
        }
        // One step per poll: a gate parks inside its own poll, yielding
        // while it holds, so the turn keeps running around it.
        Box::pin(futures_util::stream::unfold(
            steps,
            |mut steps| async move {
                loop {
                    match steps.pop_front() {
                        None => return None,
                        Some(Step::Gate(g)) => g.hold().await,
                        Some(Step::Event(e)) => return Some((e, steps)),
                    }
                }
            },
        ))
    }
    fn count_tokens(&self, context: &[Message]) -> u64 {
        self.count.tokens(context)
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

/// Echoes its `msg` argument and records every call.
pub struct EchoTool(pub Arc<Mutex<Vec<serde_json::Value>>>);

impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "echo"
    }
    fn schema(&self) -> schemars::schema::RootSchema {
        schemars::schema_for!(String)
    }
    fn risk_class(&self) -> RiskClass {
        RiskClass::Safe
    }
    fn call(&self, args: serde_json::Value) -> BoxFuture<'_, Result<ToolOutput, ToolError>> {
        self.0.lock().unwrap().push(args.clone());
        Box::pin(async move {
            match args["msg"].as_str() {
                Some(m) => Ok(ToolOutput {
                    content: format!("echo: {m}"),
                    is_error: false,
                }),
                None => Err(ToolError::InvalidArgs("msg missing".into())),
            }
        })
    }
}

pub fn call(id: &str, msg: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "echo".into(),
        args: json!({"msg": msg}),
    }
}

pub fn done(reason: &str) -> ProviderEvent {
    ProviderEvent::Done {
        finish_reason: reason.into(),
    }
}

pub fn usage(i: u64, o: u64) -> ProviderEvent {
    ProviderEvent::Usage(aigentic_core::Usage {
        input_tokens: i,
        output_tokens: o,
        ..Default::default()
    })
}

/// The runtime's own estimator, so a double can count the way it does.
fn estimate(context: &[Message]) -> u64 {
    aigentic_providers::estimate::estimate_tokens(context)
}

pub struct Harness {
    pub runtime: Runtime,
    pub seen: Seen,
    pub calls: Arc<Mutex<Vec<serde_json::Value>>>,
    pub dir: tempfile::TempDir,
}

/// The audit behind done-when 2 runs over every log a harness produced:
/// no `tool_result` without a policy record, or the test fails at drop.
impl Drop for Harness {
    fn drop(&mut self) {
        if std::thread::panicking() {
            return;
        }
        let events = self.runtime.log().read_all().expect("log reads back");
        let missing = audit_tool_results(&events);
        assert!(
            missing.is_empty(),
            "tool_result events without a policy record at seqs {missing:?}"
        );
    }
}

pub fn harness(script: Vec<Vec<ProviderEvent>>, instructions: Option<&str>) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let log = ThreadLog::open(dir.path(), ulid::Ulid::generate()).unwrap();
    harness_with_log(script, instructions, dir, log)
}

/// A harness over a log the test has already written to.
pub fn harness_with_log(
    script: Vec<Vec<ProviderEvent>>,
    instructions: Option<&str>,
    dir: tempfile::TempDir,
    log: ThreadLog,
) -> Harness {
    let (provider, seen) = scripted(script);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let registry: ToolRegistry = vec![Box::new(EchoTool(calls.clone())) as Box<dyn Tool>].into();
    let runtime = Runtime::new(provider, registry, log, AgentId("worker".into()))
        .with_layers(instructions.map_or_else(Layers::default, Layers::global_instructions));
    Harness {
        runtime,
        seen,
        calls,
        dir,
    }
}

/// A harness whose turn clock is scripted (issue #47): the first
/// reading — the turn's start — is the bases, and every later reading is
/// the bases plus the deltas. So a test can make wall time move while
/// running time stands still, exactly as an idle sleep does.
pub fn harness_with_clock(
    script: Vec<Vec<ProviderEvent>>,
    instructions: Option<&str>,
    running: std::time::Duration,
    wall: std::time::Duration,
) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let log = ThreadLog::open(dir.path(), ulid::Ulid::generate()).unwrap();
    let (provider, seen) = scripted(script);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let registry: ToolRegistry = vec![Box::new(EchoTool(calls.clone())) as Box<dyn Tool>].into();
    let base_running = std::time::Instant::now();
    let base_wall = std::time::SystemTime::now();
    let reads = std::sync::atomic::AtomicUsize::new(0);
    let runtime = Runtime::new(provider, registry, log, AgentId("worker".into()))
        .with_layers(instructions.map_or_else(Layers::default, Layers::global_instructions))
        .with_clock(Arc::new(move || {
            if reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                (base_running, base_wall)
            } else {
                (base_running + running, base_wall + wall)
            }
        }));
    Harness {
        runtime,
        seen,
        calls,
        dir,
    }
}

pub fn steve() -> Author {
    Author::User(UserId("steve".into()))
}
