//! Phase 5 step 4: decisions made outside the turn, and interrupts. A
//! permission request or an `ask_human` question parks the turn on
//! `Decisions`; a `CancelToken` ends a running turn cleanly: the model
//! call is dropped, a running tool call is killed where it stands
//! (whatever its timeout), the rest of the batch gets synthetic
//! results, `interrupted` names who, `turn_ended` says so.

mod common;

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aigentic_core::{
    Author, BoxFuture, Capabilities, CompletionRequest, ContentBlock, EventKind, Message, Provider,
    ProviderEvent, RiskClass, Tool, ToolCall, ToolError, ToolOutput, UserId,
};
use aigentic_log::{
    DecisionScope, InterruptedPayload, PermissionDecidedPayload, PolicyRecord, ThreadLog,
    ToolResultPayload, TurnEndedPayload,
};
use aigentic_policy::{Decision, Policy, Rule};
use aigentic_runtime::harness_tools::HumanQuestion;
use aigentic_runtime::{
    ASKED_HUMAN, Answered, CancelToken, DecisionError, Decisions, INTERRUPTED, Inbox, Pending,
    Runtime, Signal,
};
use aigentic_tools::ToolRegistry;
use common::{done, steve};
use futures_core::Stream;
use serde_json::json;

/// A scripted provider whose `None` entries never yield: the model call
/// that an interrupt lands on.
struct Gated {
    script: Mutex<VecDeque<Option<Vec<ProviderEvent>>>>,
    seen: Arc<Mutex<Vec<Vec<Message>>>>,
}

impl Provider for Gated {
    fn complete(
        &self,
        request: &CompletionRequest<'_>,
    ) -> Pin<Box<dyn Stream<Item = ProviderEvent> + Send + '_>> {
        self.seen.lock().unwrap().push(request.messages.to_vec());
        match self.script.lock().unwrap().pop_front().expect("script") {
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

/// A `write`-class tool (so the default rules ask) that sleeps for
/// `ms` and records that it ran.
struct Slow(Arc<Mutex<Vec<String>>>);

impl Tool for Slow {
    fn name(&self) -> &str {
        "slow"
    }
    fn description(&self) -> &str {
        "slow"
    }
    fn schema(&self) -> schemars::schema::RootSchema {
        schemars::schema_for!(String)
    }
    fn risk_class(&self) -> RiskClass {
        RiskClass::Write
    }
    fn call(&self, args: serde_json::Value) -> BoxFuture<'_, Result<ToolOutput, ToolError>> {
        let ms = args["ms"].as_u64().unwrap_or(0);
        let ran = self.0.clone();
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            ran.lock().unwrap().push(format!("slept {ms}"));
            Ok(ToolOutput {
                content: format!("slept {ms}"),
                is_error: false,
            })
        })
    }
}

fn magnus() -> Author {
    Author::User(UserId("magnus".into()))
}

fn slow(id: &str, ms: u64) -> ProviderEvent {
    ProviderEvent::ToolCall(ToolCall {
        id: id.into(),
        name: "slow".into(),
        args: json!({"ms": ms}),
    })
}

fn ask(id: &str, question: &str) -> ProviderEvent {
    ProviderEvent::ToolCall(ToolCall {
        id: id.into(),
        name: "ask_human".into(),
        args: json!({"question": question}),
    })
}

fn text(t: &str) -> ProviderEvent {
    ProviderEvent::TextDelta(t.into())
}

struct Rig {
    runtime: Runtime,
    decisions: Arc<Decisions>,
    ran: Arc<Mutex<Vec<String>>>,
    waited: Arc<Mutex<Vec<Pending>>>,
    _dir: tempfile::TempDir,
}

/// `slow` allowed outright when `allow_slow`, else the default rules ask
/// for it (class write).
fn rig(script: Vec<Option<Vec<ProviderEvent>>>, allow_slow: bool) -> Rig {
    let ran = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ToolRegistry::empty();
    registry.register(Box::new(Slow(ran.clone()))).unwrap();
    let policy = if allow_slow {
        Policy::configured(vec![Rule::tool("slow", Decision::Allow, "test")], None)
    } else {
        Policy::defaults()
    };
    let mut rig = rig_with(script, tempfile::tempdir().unwrap(), registry, policy);
    rig.ran = ran;
    rig
}

/// The rig body over a caller-built registry and policy; `dir` is where
/// the log and the tools live, and it stays alive on the `Rig`.
fn rig_with(
    script: Vec<Option<Vec<ProviderEvent>>>,
    dir: tempfile::TempDir,
    registry: ToolRegistry,
    policy: Policy,
) -> Rig {
    let log = ThreadLog::open(dir.path(), ulid::Ulid::generate()).unwrap();
    let decisions = Arc::new(Decisions::new());
    let runtime = Runtime::new(
        Box::new(Gated {
            script: Mutex::new(script.into()),
            seen: Arc::new(Mutex::new(Vec::new())),
        }),
        registry,
        log,
        aigentic_core::AgentId("worker".into()),
    )
    .with_policy(policy)
    .with_decisions(decisions.clone());
    Rig {
        runtime,
        decisions,
        ran: Arc::new(Mutex::new(Vec::new())),
        waited: Arc::new(Mutex::new(Vec::new())),
        _dir: dir,
    }
}

fn kinds(rig: &Rig) -> Vec<EventKind> {
    rig.runtime
        .log()
        .read_all()
        .unwrap()
        .iter()
        .map(|e| e.kind)
        .collect()
}

fn payload<T: serde::de::DeserializeOwned>(rig: &Rig, seq: usize, kind: EventKind) -> T {
    let events = rig.runtime.log().read_all().unwrap();
    assert_eq!(events[seq].kind, kind, "seq {seq}: {:?}", kinds(rig));
    serde_json::from_value(events[seq].payload.clone()).unwrap()
}

fn author_at(rig: &Rig, seq: usize) -> Author {
    rig.runtime.log().read_all().unwrap()[seq].author.clone()
}

/// Poll until something is pending, then answer it.
async fn when_pending(decisions: &Decisions, answer: impl FnOnce(&Pending) -> Answered) {
    for _ in 0..200 {
        if let Some(p) = decisions.pending().into_iter().next() {
            decisions.decide(p.call_id(), answer(&p)).unwrap();
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("nothing became pending");
}

/// Start a turn with the token, as the actor will.
async fn start(rig: &mut Rig, cancel: &CancelToken) -> aigentic_runtime::TurnOutcome {
    let waited = rig.waited.clone();
    rig.runtime
        .run_turn_until(
            steve(),
            vec![ContentBlock::Text("go".into())],
            cancel,
            &mut Inbox::none(),
            &mut |s| {
                if let Signal::Waiting(p) = s {
                    waited.lock().unwrap().push(p.clone());
                }
            },
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn a_decision_from_an_approver_resumes_the_turn_with_their_author() {
    let mut rig = rig(
        vec![
            Some(vec![slow("c1", 0), done("tool_use")]),
            Some(vec![slow("c2", 0), done("tool_use")]),
            Some(vec![text("ok"), done("stop")]),
        ],
        false,
    );
    let decisions = rig.decisions.clone();
    let decider = async {
        // First call: allow for the session, by magnus.
        when_pending(&decisions, |p| {
            assert!(matches!(p, Pending::Permission { call_id, .. } if call_id == "c1"));
            Answered::Permission {
                allow: true,
                session: true,
                by: magnus(),
                prefix: None,
                reason: None,
            }
        })
        .await;
        // A second answer to the same call is refused.
        assert_eq!(
            decisions.decide(
                "c1",
                Answered::Permission {
                    allow: false,
                    session: false,
                    by: magnus(),
                    prefix: None,
                    reason: None,
                }
            ),
            Err(DecisionError::AlreadyDecided("c1".into()))
        );
        // The second call is identical: the grant answers it, nothing
        // becomes pending again.
    };
    let cancel = CancelToken::never();
    let (outcome, ()) = tokio::join!(run_plain(&mut rig, &cancel), decider);
    assert_eq!(outcome.reason, "done");
    assert_eq!(
        kinds(&rig),
        vec![
            EventKind::UserMessage,
            EventKind::AssistantMessage,
            EventKind::PermissionRequested,
            EventKind::PermissionDecided,
            EventKind::ToolResult,
            EventKind::AssistantMessage,
            EventKind::PermissionRequested,
            EventKind::PermissionDecided,
            EventKind::ToolResult,
            EventKind::AssistantMessage,
            EventKind::TurnEnded,
        ]
    );
    let d: PermissionDecidedPayload = payload(&rig, 3, EventKind::PermissionDecided);
    assert_eq!(
        (d.allow, d.scope, d.reason),
        (true, DecisionScope::Session, None)
    );
    assert_eq!(author_at(&rig, 3), magnus());
    let d: PermissionDecidedPayload = payload(&rig, 7, EventKind::PermissionDecided);
    assert_eq!(
        d.scope,
        DecisionScope::Session,
        "the grant answered the second"
    );
    assert_eq!(author_at(&rig, 7), magnus());
    assert_eq!(rig.ran.lock().unwrap().len(), 2);
    let waited = rig.waited.lock().unwrap();
    assert_eq!(waited.len(), 1, "only the first call parked: {waited:?}");
    assert!(rig.decisions.pending().is_empty());
}

/// The plain path: `run_turn` with a never token.
async fn run_plain(rig: &mut Rig, _cancel: &CancelToken) -> aigentic_runtime::TurnOutcome {
    let waited = rig.waited.clone();
    rig.runtime
        .run_turn(steve(), vec![ContentBlock::Text("go".into())], &mut |s| {
            if let Signal::Waiting(p) = s {
                waited.lock().unwrap().push(p.clone());
            }
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn an_answer_to_ask_human_is_the_result_and_splits_the_turn() {
    let mut rig = rig(
        vec![Some(vec![ask("q1", "Ship it?"), done("tool_use")])],
        true,
    );
    let decisions = rig.decisions.clone();
    let answerer = when_pending(&decisions, |p| {
        assert_eq!(
            p,
            &Pending::Human {
                call_id: "q1".into(),
                question: "Ship it?".into(),
                // The old single-question shape normalises to one
                // question with no options.
                questions: vec![HumanQuestion {
                    question: "Ship it?".into(),
                    header: None,
                    options: Vec::new(),
                    multi: false,
                }],
            }
        );
        Answered::Human {
            text: "yes, ship".into(),
            by: magnus(),
        }
    });
    let cancel = CancelToken::never();
    let (outcome, ()) = tokio::join!(run_plain(&mut rig, &cancel), answerer);
    assert_eq!(outcome.reason, ASKED_HUMAN);
    let r: ToolResultPayload = payload(&rig, 2, EventKind::ToolResult);
    assert_eq!(r.result.content, "yes, ship");
    assert!(!r.result.is_error);
    assert!(matches!(
        decisions.decide(
            "q1",
            Answered::Human {
                text: "late".into(),
                by: magnus()
            }
        ),
        Err(DecisionError::AlreadyDecided(_))
    ));
}

#[tokio::test]
async fn an_interrupt_during_the_model_call_leaves_no_partial_message() {
    let mut rig = rig(vec![None], true);
    let cancel = CancelToken::never();
    let canceller = async {
        tokio::time::sleep(Duration::from_millis(30)).await;
        cancel.cancel(magnus());
    };
    let (outcome, ()) = tokio::join!(start(&mut rig, &cancel), canceller);
    assert_eq!(outcome.reason, INTERRUPTED);
    assert_eq!(
        kinds(&rig),
        vec![
            EventKind::UserMessage,
            EventKind::Interrupted,
            EventKind::TurnEnded
        ]
    );
    let i: InterruptedPayload = payload(&rig, 1, EventKind::Interrupted);
    assert_eq!(i.reason, "interrupt");
    assert_eq!(i.by, Some(magnus()));
    assert_eq!(i.after_seq, 0);
    assert!(i.unanswered_calls.is_empty());
    let t: TurnEndedPayload = payload(&rig, 2, EventKind::TurnEnded);
    assert_eq!(t.reason, INTERRUPTED);
    // A turn started after a cancel ends at once.
    let outcome = start(&mut rig, &cancel).await;
    assert_eq!(outcome.reason, INTERRUPTED);
}

#[tokio::test]
async fn an_interrupt_kills_a_running_tool_and_gives_the_rest_synthetic_results() {
    let mut rig = rig(
        vec![Some(vec![
            slow("c1", 60_000),
            slow("c2", 0),
            done("tool_use"),
        ])],
        true,
    );
    let cancel = CancelToken::never();
    let began = Instant::now();
    let canceller = async {
        tokio::time::sleep(Duration::from_millis(150)).await;
        cancel.cancel(magnus());
    };
    let (outcome, ()) = tokio::join!(start(&mut rig, &cancel), canceller);
    assert_eq!(outcome.reason, INTERRUPTED);
    // The interrupt is not held hostage by the tool's own timeout.
    assert!(
        began.elapsed() < Duration::from_secs(1),
        "{:?}",
        began.elapsed()
    );
    assert_eq!(
        kinds(&rig),
        vec![
            EventKind::UserMessage,
            EventKind::AssistantMessage,
            EventKind::ToolResult,
            EventKind::ToolResult,
            EventKind::Interrupted,
            EventKind::TurnEnded
        ]
    );
    // The running tool was killed where it stood and its result says so.
    let r: ToolResultPayload = payload(&rig, 2, EventKind::ToolResult);
    assert_eq!(r.result.id, "c1");
    assert!(r.result.is_error);
    assert!(
        r.result
            .content
            .contains("interrupted by magnus while running: the call was killed"),
        "{}",
        r.result.content
    );
    assert!(
        rig.ran.lock().unwrap().is_empty(),
        "the tool never finished"
    );
    // The second never ran and says why, with a rule record.
    let r: ToolResultPayload = payload(&rig, 3, EventKind::ToolResult);
    assert_eq!(r.result.id, "c2");
    assert!(r.result.is_error);
    assert!(
        r.result
            .content
            .contains("interrupted by magnus before this call"),
        "{}",
        r.result.content
    );
    assert_eq!(r.policy, Some(PolicyRecord::rule(INTERRUPTED, "deny")));
    let i: InterruptedPayload = payload(&rig, 4, EventKind::Interrupted);
    assert_eq!(i.unanswered_calls, vec!["c2".to_owned()]);
    assert_eq!(i.by, Some(magnus()));
    let events = rig.runtime.log().read_all().unwrap();
    assert!(aigentic_runtime::audit_tool_results(&events).is_empty());
}

/// True while a process with this pid exists (zombies included).
#[cfg(unix)]
fn alive(pid: i32) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
}

/// An interrupt kills a bash command however long its timeout is, and
/// takes everything it started with it (issue #36: the interrupt must
/// win over a 900 s command).
#[cfg(unix)]
#[tokio::test]
async fn an_interrupt_kills_a_bash_command_with_a_long_timeout_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("sleep.pid");
    let command = format!("sleep 300 & echo $! > {}; sleep 300", pid_file.display());
    let mut registry = ToolRegistry::empty();
    registry
        .register(Box::new(
            aigentic_tools::BashTool::new(aigentic_tools::Workdir::new(dir.path()))
                .with_timeout(Duration::from_secs(600)),
        ))
        .unwrap();
    let policy = Policy::configured(vec![Rule::tool("bash", Decision::Allow, "test")], None);
    let mut rig = rig_with(
        vec![Some(vec![
            ProviderEvent::ToolCall(ToolCall {
                id: "c1".into(),
                name: "bash".into(),
                args: json!({"command": command}),
            }),
            done("tool_use"),
        ])],
        dir,
        registry,
        policy,
    );
    let cancel = CancelToken::never();
    let began = Instant::now();
    let canceller = async {
        tokio::time::sleep(Duration::from_millis(150)).await;
        cancel.cancel(magnus());
    };
    let (outcome, ()) = tokio::join!(start(&mut rig, &cancel), canceller);
    assert_eq!(outcome.reason, INTERRUPTED);
    assert!(
        began.elapsed() < Duration::from_secs(1),
        "{:?}",
        began.elapsed()
    );
    let r: ToolResultPayload = payload(&rig, 2, EventKind::ToolResult);
    assert_eq!(r.result.id, "c1");
    assert!(r.result.is_error);
    assert!(
        r.result
            .content
            .contains("interrupted by magnus while running: the call was killed"),
        "{}",
        r.result.content
    );

    // The shell, the command and its background child are dead: not just
    // unrecorded, actually torn down.
    let pid: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while alive(pid) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        !alive(pid),
        "the backgrounded sleep {pid} survived the interrupt"
    );
    let events = rig.runtime.log().read_all().unwrap();
    assert!(aigentic_runtime::audit_tool_results(&events).is_empty());
}

#[tokio::test]
async fn an_interrupt_while_awaiting_approval_denies_with_the_interrupter() {
    let mut rig = rig(vec![Some(vec![slow("c1", 0), done("tool_use")])], false);
    let cancel = CancelToken::never();
    let decisions = rig.decisions.clone();
    let canceller = async {
        for _ in 0..200 {
            if !decisions.pending().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(!decisions.pending().is_empty(), "the request never parked");
        cancel.cancel(magnus());
    };
    let (outcome, ()) = tokio::join!(start(&mut rig, &cancel), canceller);
    assert_eq!(outcome.reason, INTERRUPTED);
    assert_eq!(
        kinds(&rig),
        vec![
            EventKind::UserMessage,
            EventKind::AssistantMessage,
            EventKind::PermissionRequested,
            EventKind::PermissionDecided,
            EventKind::ToolResult,
            EventKind::Interrupted,
            EventKind::TurnEnded
        ]
    );
    let d: PermissionDecidedPayload = payload(&rig, 3, EventKind::PermissionDecided);
    assert_eq!(
        (d.allow, d.scope, d.reason.as_deref()),
        (false, DecisionScope::Once, Some(INTERRUPTED))
    );
    assert_eq!(author_at(&rig, 3), magnus());
    let r: ToolResultPayload = payload(&rig, 4, EventKind::ToolResult);
    assert!(r.result.is_error);
    assert!(
        r.result
            .content
            .contains("interrupted by magnus before a decision"),
        "{}",
        r.result.content
    );
    assert!(matches!(
        r.policy,
        Some(PolicyRecord::Human { allow: false, .. })
    ));
    assert!(rig.ran.lock().unwrap().is_empty(), "the call never ran");
    // A late decision is refused: the request was withdrawn.
    assert_eq!(
        decisions.decide(
            "c1",
            Answered::Permission {
                allow: true,
                session: false,
                by: magnus(),
                prefix: None,
                reason: None,
            }
        ),
        Err(DecisionError::AlreadyDecided("c1".into()))
    );
    let waited = rig.waited.lock().unwrap();
    assert_eq!(waited.len(), 1);
}
