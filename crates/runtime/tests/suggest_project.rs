//! The project proposal (issue #7): `suggest_project` parks the turn, a
//! person answers, and a `yes` switches the project. Sibling of
//! `ask_human.rs`: the same rig, the same parking, a different answer.
//!
//! Every expected result text is the runtime's own constant or function
//! (the spec's rule for these tests), never a retyped copy.

mod common;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use aigentic_core::{AgentId, Author, ContentBlock, EventKind, ProviderEvent, RiskClass, ToolCall};
use aigentic_log::{
    AssistantMessagePayload, DecisionAnswer, DecisionAnsweredPayload, DecisionKind,
    DecisionProposedPayload, DecisionStage, NewEvent, ThreadLog, ToolResultPayload,
    UserMessagePayload, decision_records,
};
use aigentic_policy::Policy;
use aigentic_runtime::harness_tools::{
    HARNESS_CLASS, HARNESS_INSTRUCTIONS, NOT_RUN_SIBLING, SuggestProjectArgs, TURN_INTERRUPTED,
    already_in_text, corrected_text, declined_text, harness_names, harness_specs, is_harness_tool,
    not_answered_text, switched_text,
};
use aigentic_runtime::project::ProjectFile;
use aigentic_runtime::{
    ASKED_HUMAN, Answered, CancelToken, DAEMON_RESTARTED, Decisions, INTERRUPTED, Inbox, Layers,
    Pending, Project, ProjectContext, ProjectRow, Resumed, Runtime, Signal, SwitchAnswer,
    SwitchCtx,
};
use aigentic_tools::{DEFAULT_TIMEOUT, ToolRegistry, Workdir};
use common::{done, scripted, steve};
use serde_json::json;
use ulid::Ulid;

const HERE: &str = "here";
const THERE: &str = "there";
const HERE_RULES: &str = "here: answer in English";
const THERE_RULES: &str = "there: answer in Swedish";
const LISTING: &str = "Projects in reach. aigentic (here) ~/h · aigentic-web ~/t";

/// The reach these fixtures carry: the daemon's rendered block, and no
/// rows behind it (the pair `with_projects` takes, issue #123). No row
/// means no sibling's brief is reachable, which is what these tests,
/// about other things, want.
fn reach() -> (Option<String>, Vec<ProjectRow>) {
    (Some(LISTING.into()), Vec::new())
}

const SUGGEST_PROJECT: &str = "suggest_project";

/// The call the model makes, and the reply that carries it.
fn propose(id: &str, project: &str, reason: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: SUGGEST_PROJECT.into(),
        args: json!({"project": project, "reason": reason}),
    }
}

fn proposing(id: &str, project: &str, reason: &str) -> Vec<ProviderEvent> {
    vec![
        ProviderEvent::ToolCall(propose(id, project, reason)),
        done("tool_use"),
    ]
}

fn speaks(text: &str) -> Vec<ProviderEvent> {
    vec![ProviderEvent::TextDelta(text.into()), done("stop")]
}

/// A project layer with instructions of its own, so a prefix can be told
/// from the other one's.
fn project(name: &str, root: &std::path::Path, instructions: &str) -> Project {
    Project {
        name: name.into(),
        root: root.to_path_buf(),
        file: ProjectFile::default(),
        instructions: Some(instructions.into()),
        memory: Vec::new(),
        unknown: Vec::new(),
    }
}

/// The context a `yes` moves the thread into: the target's own layers.
fn target_context(name: &str, root: PathBuf) -> ProjectContext {
    let (provider, _seen) = scripted(vec![speaks("there now")]);
    ProjectContext {
        name: Some(name.into()),
        workspace: Some(format!("~/{name}")),
        root: root.clone(),
        layers: Layers::default().with_project(project(name, &root, THERE_RULES)),
        policy: Policy::defaults(),
        skills: aigentic_runtime::aigentic_skills::SkillSet::default(),
        registry: ToolRegistry::empty(),
        provider,
        model_label: "target-model".into(),
        projects: reach().0,
        project_rows: reach().1,
        profile: None,
        effort: None,
        prices: None,
    }
}

/// A runtime in `here`, over its own log, with no decisions installed.
fn rig(script: Vec<Vec<ProviderEvent>>, dir: &std::path::Path) -> Runtime {
    let log = ThreadLog::open(dir, Ulid::generate()).unwrap();
    let (provider, _seen) = scripted(script);
    let registry: ToolRegistry =
        vec![Box::new(common::EchoTool(Arc::new(Mutex::new(vec![]))))
            as Box<dyn aigentic_core::Tool>]
        .into();
    Runtime::new(provider, registry, log, AgentId("worker".into()))
        .with_layers(Layers::default().with_project(project(HERE, dir, HERE_RULES)))
        .with_projects(Some(LISTING.into()), Vec::new())
}

fn go() -> Vec<ContentBlock> {
    vec![ContentBlock::Text("go".into())]
}

/// One turn with nobody watching: the observer closure lives for the
/// whole call, which it does not in a `join!` arm.
async fn quiet_turn(
    runtime: &mut Runtime,
) -> Result<aigentic_runtime::TurnOutcome, aigentic_runtime::RuntimeError> {
    runtime.run_turn(steve(), go(), &mut |_| {}).await
}

/// An event kind's wire name.
fn name(kind: EventKind) -> String {
    serde_json::to_value(kind)
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned()
}

/// Every event's kind, in order.
fn kinds(runtime: &Runtime) -> Vec<EventKind> {
    runtime
        .log()
        .read_all()
        .unwrap()
        .iter()
        .map(|e| e.kind)
        .collect()
}

fn log_events(runtime: &Runtime) -> Vec<aigentic_core::Event> {
    runtime.log().read_all().unwrap()
}

fn result_of(events: &[aigentic_core::Event], id: &str) -> aigentic_core::ToolResult {
    let event = events
        .iter()
        .rev()
        .find(|e| {
            e.kind == EventKind::ToolResult
                && serde_json::from_value::<ToolResultPayload>(e.payload.clone())
                    .is_ok_and(|p| p.result.id == id)
        })
        .expect("the call has a result");
    serde_json::from_value::<ToolResultPayload>(event.payload.clone())
        .expect("a tool result payload")
        .result
}

/// Every answer in the log, with its author.
fn answers(events: &[aigentic_core::Event]) -> Vec<(Author, DecisionAnsweredPayload)> {
    events
        .iter()
        .filter(|e| e.kind == EventKind::DecisionAnswered)
        .map(|e| {
            (
                e.author.clone(),
                serde_json::from_value::<DecisionAnsweredPayload>(e.payload.clone())
                    .expect("a decision answer payload"),
            )
        })
        .collect()
}

fn proposals(events: &[aigentic_core::Event]) -> Vec<(Ulid, DecisionProposedPayload)> {
    events
        .iter()
        .filter(|e| e.kind == EventKind::DecisionProposed)
        .map(|e| {
            (
                e.id,
                serde_json::from_value::<DecisionProposedPayload>(e.payload.clone())
                    .expect("a decision proposal payload"),
            )
        })
        .collect()
}

/// Wait for the turn to park on a switch, and hand back the call, the
/// project and the reason. Panics rather than hanging for ever.
async fn when_parked(decisions: &Arc<Decisions>) -> (String, String, String) {
    for _ in 0..4_000 {
        if let Some(Pending::Switch {
            call_id,
            project,
            reason,
        }) = decisions.pending().first()
        {
            return (call_id.clone(), project.clone(), reason.clone());
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("the turn never parked on a proposal");
}

/// Answer the next proposal a test expects, from a task of its own.
fn answerer(
    decisions: Arc<Decisions>,
    answer: SwitchAnswer,
    context: impl FnOnce() -> Option<ProjectContext> + Send + 'static,
) -> tokio::task::JoinHandle<Option<Result<(), String>>> {
    tokio::spawn(async move {
        let (call_id, ..) = when_parked(&decisions).await;
        let (ctx, ack) = match context() {
            Some(target) => {
                let (ctx, ack) = SwitchCtx::oneshot(target);
                (ctx, Some(ack))
            }
            None => (SwitchCtx::none(), None),
        };
        decisions
            .decide(
                &call_id,
                Answered::Switch {
                    answer,
                    by: steve(),
                    ctx,
                },
            )
            .expect("the wait is live");
        match ack {
            Some(ack) => Some(ack.await.expect("the ack is fired")),
            None => None,
        }
    })
}

// --------------------------------------------------------------- tests

/// T1: the spec is offered in an ordinary thread and not in a step
/// thread, the name list knows it, it is a `safe` harness tool, its
/// arguments are the two required ones, an unknown key is refused, and
/// the standing instructions name it.
#[tokio::test]
async fn t1_suggest_project_is_a_safe_harness_tool_offered_in_ordinary_threads() {
    let ordinary = harness_specs(true, false, true);
    let spec = ordinary
        .iter()
        .find(|s| s.name == SUGGEST_PROJECT)
        .expect("an ordinary thread sees suggest_project");
    let schema = serde_json::to_value(&spec.schema).unwrap();
    let required: Vec<&str> = schema["required"]
        .as_array()
        .expect("a required list")
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(required, vec!["project", "reason"]);

    assert!(
        !harness_specs(true, true, false)
            .iter()
            .any(|s| s.name == SUGGEST_PROJECT),
        "a step thread never sees the spec"
    );
    assert!(harness_names().contains(&SUGGEST_PROJECT.to_string()));
    assert!(is_harness_tool(SUGGEST_PROJECT));
    assert_eq!(HARNESS_CLASS, RiskClass::Safe);
    assert!(HARNESS_INSTRUCTIONS.contains(SUGGEST_PROJECT));

    // The arguments: both are required, and an unknown key is refused.
    let parsed = serde_json::from_value::<SuggestProjectArgs>(json!({
        "project": THERE,
        "reason": "belongs there",
    }));
    assert!(parsed.is_ok());
    assert!(
        serde_json::from_value::<SuggestProjectArgs>(json!({
            "project": THERE,
            "reason": "belongs there",
            "extra": 1,
        }))
        .is_err(),
        "an unknown key is refused"
    );
    assert!(
        serde_json::from_value::<SuggestProjectArgs>(json!({"project": THERE})).is_err(),
        "the reason is required"
    );
}

/// T2: `decision_proposed` is in the log **before** the wait fires.
#[tokio::test]
async fn t2_the_proposal_is_appended_before_the_wait_fires() {
    let dir = tempfile::tempdir().unwrap();
    let mut runtime = rig(
        vec![proposing("c1", THERE, "the message is theirs")],
        dir.path(),
    );
    let decisions = Arc::new(Decisions::new());
    runtime = runtime.with_decisions(decisions.clone());

    let mut seen: Vec<String> = Vec::new();
    let mut observer = |signal: Signal<'_>| match signal {
        Signal::Event(event) if event.kind == EventKind::DecisionProposed => {
            seen.push("proposed".into())
        }
        Signal::Waiting(Pending::Switch { project, .. }) => seen.push(format!("waiting:{project}")),
        _ => {}
    };
    let answer = answerer(decisions.clone(), SwitchAnswer::No, || None);
    let (outcome, _) = tokio::join!(runtime.run_turn(steve(), go(), &mut observer), answer);
    assert_eq!(outcome.unwrap().reason, ASKED_HUMAN);
    assert_eq!(
        seen,
        vec!["proposed".to_string(), format!("waiting:{THERE}")],
        "the event is written first, then the wait fires"
    );

    let events = log_events(&runtime);
    let proposed = proposals(&events);
    assert_eq!(proposed.len(), 1);
    assert_eq!(proposed[0].1.kind, DecisionKind::Project);
    assert_eq!(proposed[0].1.target.as_deref(), Some(THERE));
    assert_eq!(proposed[0].1.reason, "the message is theirs");
    assert_eq!(proposed[0].1.call_id.as_deref(), Some("c1"));
    assert_eq!(proposed[0].1.stage, DecisionStage::Ask);
    assert_eq!(proposed[0].1.proposal, "switch to there");
}

/// T3: a `yes` writes `project_switched`, then `decision_answered`
/// (authored by the answerer, `parent_event` the proposal), then the
/// result; the turn ends `asked_human`; the ack is `Ok`; and the next
/// turn runs in the new project.
#[tokio::test]
async fn t3_a_yes_switches_the_project_and_the_next_turn_is_in_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut runtime = rig(
        vec![
            proposing("c1", THERE, "belongs to the other thread"),
            speaks("done there"),
        ],
        dir.path(),
    );
    let decisions = Arc::new(Decisions::new());
    runtime = runtime.with_decisions(decisions.clone());
    let root = dir.path().to_path_buf();
    let answer = answerer(decisions.clone(), SwitchAnswer::Yes, move || {
        Some(target_context(THERE, root.clone()))
    });

    let (outcome, ack) = tokio::join!(quiet_turn(&mut runtime), answer);
    assert_eq!(outcome.unwrap().reason, ASKED_HUMAN);
    assert_eq!(
        ack.unwrap(),
        Some(Ok(())),
        "the session that built the context learns it was used"
    );

    let events = log_events(&runtime);
    let order: Vec<EventKind> = kinds(&runtime)
        .into_iter()
        .skip_while(|k| *k != EventKind::ProjectSwitched)
        .take(3)
        .collect();
    assert_eq!(
        order,
        vec![
            EventKind::ProjectSwitched,
            EventKind::DecisionAnswered,
            EventKind::ToolResult,
        ],
        "the switch is recorded before the yes that asked for it"
    );

    let proposed = proposals(&events);
    let answered = events
        .iter()
        .find(|e| e.kind == EventKind::DecisionAnswered)
        .unwrap();
    assert_eq!(answered.parent_event, Some(proposed[0].0));
    assert_eq!(answered.author, steve());
    let payload: DecisionAnsweredPayload =
        serde_json::from_value(answered.payload.clone()).unwrap();
    assert_eq!(payload.answer, DecisionAnswer::Yes);
    assert_eq!(payload.correction, None);
    assert_eq!(payload.note, None);
    let result = result_of(&events, "c1");
    assert!(!result.is_error, "a yes is not an error result");
    assert_eq!(result.content, switched_text(THERE));
    assert!(aigentic_runtime::audit_tool_results(&events).is_empty());

    // The next turn runs in the new project: its tools are the target's
    // and its prefix carries the target's instructions.
    assert_eq!(runtime.current_project(), Some(THERE.to_string()));
    let specs = runtime.tool_specs();
    assert!(
        !specs.iter().any(|s| s.name == "echo"),
        "the next turn carries the new project's tools"
    );
    assert!(specs.iter().any(|s| s.name == SUGGEST_PROJECT));
    runtime.continue_turn(&mut |_| {}).await.unwrap();
    assert_eq!(runtime.current_project(), Some(THERE.to_string()));

    // The reference check the issue asks for: the event kinds in order,
    // from the proposal to the continued turn's own events. Run this test
    // with `--nocapture` to read the list.
    for event in log_events(&runtime)
        .iter()
        .skip_while(|e| e.kind != EventKind::DecisionProposed)
    {
        eprintln!("{:>3}  {}", event.seq, name(event.kind));
    }
}

/// T3 continued: the new project's prefix is what the model is sent, so
/// read it from the request the target's provider saw.
#[tokio::test]
async fn t3_the_next_turn_carries_the_new_project_s_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let mut runtime = rig(
        vec![
            proposing("c1", THERE, "belongs to the other thread"),
            speaks("done there"),
        ],
        dir.path(),
    );
    let decisions = Arc::new(Decisions::new());
    runtime = runtime.with_decisions(decisions.clone());
    // The context whose provider a test can look behind.
    let (provider, seen) = scripted(vec![speaks("there now")]);
    let root = dir.path().to_path_buf();
    let ctx = ProjectContext {
        provider,
        ..target_context(THERE, root)
    };
    let answer = tokio::spawn({
        let decisions = decisions.clone();
        async move {
            let (call_id, ..) = when_parked(&decisions).await;
            let (ctx, ack) = SwitchCtx::oneshot(ctx);
            decisions
                .decide(
                    &call_id,
                    Answered::Switch {
                        answer: SwitchAnswer::Yes,
                        by: steve(),
                        ctx,
                    },
                )
                .expect("live");
            let _ = ack.await;
        }
    });
    let (outcome, _) = tokio::join!(quiet_turn(&mut runtime), answer);
    outcome.unwrap();
    runtime.continue_turn(&mut |_| {}).await.unwrap();

    let requests = seen.lock().unwrap().clone();
    let last = requests.last().expect("the target provider was asked");
    let mut prefix = String::new();
    for message in last.iter() {
        if message.role == aigentic_core::Role::System {
            for block in &message.blocks {
                if let ContentBlock::Text(text) = block {
                    prefix.push_str(text);
                }
            }
        }
    }
    assert!(
        prefix.contains(THERE_RULES),
        "the new project's instructions are in the prefix: {prefix}"
    );
    assert!(!prefix.contains(HERE_RULES));
    assert!(
        prefix.contains(LISTING),
        "the listing came with the context"
    );
}

/// T4: a `yes` whose `set_project` refuses — the step thread, where the
/// call is unoffered but still executes — is withdrawn, an error result,
/// and the ack carries the reason. The turn continues in place.
#[tokio::test]
async fn t4_a_yes_that_cannot_switch_is_withdrawn_and_the_turn_continues() {
    let dir = tempfile::tempdir().unwrap();
    let log = ThreadLog::open(dir.path(), Ulid::generate()).unwrap();
    let (provider, _seen) = scripted(vec![proposing("c1", THERE, "belongs there"), speaks("ok")]);
    let mut runtime = Runtime::new(
        provider,
        ToolRegistry::empty(),
        log,
        AgentId("worker".into()),
    )
    .with_layers(Layers::default().with_project(project(HERE, dir.path(), HERE_RULES)))
    .with_projects(Some(LISTING.into()), Vec::new())
    .with_step("implement", &[])
    .expect("a valid deny list");
    let decisions = Arc::new(Decisions::new());
    runtime = runtime.with_decisions(decisions.clone());
    let root = dir.path().to_path_buf();
    let answer = answerer(decisions.clone(), SwitchAnswer::Yes, move || {
        Some(target_context(THERE, root.clone()))
    });

    let (outcome, ack) = tokio::join!(quiet_turn(&mut runtime), answer);
    let outcome = outcome.unwrap();
    let note = ack
        .unwrap()
        .expect("the ack says Err")
        .expect_err("the switch failed");
    assert!(note.starts_with("switch failed: "), "{note}");
    assert!(note.contains("step thread"), "{note}");
    assert_ne!(
        outcome.reason, ASKED_HUMAN,
        "the error continues the turn in place"
    );

    let events = log_events(&runtime);
    assert!(!kinds(&runtime).contains(&EventKind::ProjectSwitched));
    let answered = answers(&events);
    assert_eq!(answered.len(), 1);
    assert_eq!(answered[0].0, Author::System);
    assert_eq!(answered[0].1.answer, DecisionAnswer::Withdrawn);
    assert_eq!(answered[0].1.note.as_deref(), Some(note.as_str()));
    let result = result_of(&events, "c1");
    assert!(result.is_error, "a refused yes is an error result");
    assert_eq!(result.content, not_answered_text(&note));
    assert_eq!(runtime.current_project(), Some(HERE.to_string()));
}

/// T5: `no` and `corrected` are recorded, with the runtime's own result
/// texts; `corrected` carries the person's words.
#[tokio::test]
async fn t5_no_is_recorded_and_says_where_the_thread_stays() {
    let dir = tempfile::tempdir().unwrap();
    let mut runtime = rig(
        vec![proposing("c1", THERE, "belongs there"), speaks("ok")],
        dir.path(),
    );
    let decisions = Arc::new(Decisions::new());
    runtime = runtime.with_decisions(decisions.clone());
    let answer = answerer(decisions.clone(), SwitchAnswer::No, || None);
    let (outcome, _) = tokio::join!(quiet_turn(&mut runtime), answer);
    assert_eq!(outcome.unwrap().reason, ASKED_HUMAN);

    let events = log_events(&runtime);
    let answered = answers(&events);
    assert_eq!(answered.len(), 1);
    assert_eq!(answered[0].0, steve());
    assert_eq!(answered[0].1.answer, DecisionAnswer::No);
    assert_eq!(answered[0].1.correction, None);
    let result = result_of(&events, "c1");
    assert!(!result.is_error, "a no is not an error: the thread goes on");
    assert_eq!(result.content, declined_text(HERE));
    assert_eq!(runtime.current_project(), Some(HERE.to_string()));
    assert!(!kinds(&runtime).contains(&EventKind::ProjectSwitched));
    assert!(aigentic_runtime::audit_tool_results(&events).is_empty());
}

/// T5: `corrected` records the person's words and hands them to the model.
#[tokio::test]
async fn t5_corrected_records_the_place_the_person_named() {
    let dir = tempfile::tempdir().unwrap();
    let mut runtime = rig(
        vec![proposing("c1", THERE, "belongs there"), speaks("ok")],
        dir.path(),
    );
    let decisions = Arc::new(Decisions::new());
    runtime = runtime.with_decisions(decisions.clone());
    let answer = answerer(
        decisions.clone(),
        SwitchAnswer::Corrected("customer X".into()),
        || None,
    );
    let (outcome, _) = tokio::join!(quiet_turn(&mut runtime), answer);
    assert_eq!(outcome.unwrap().reason, ASKED_HUMAN);

    let events = log_events(&runtime);
    let answered = answers(&events);
    assert_eq!(answered[0].1.answer, DecisionAnswer::Corrected);
    assert_eq!(answered[0].1.correction.as_deref(), Some("customer X"));
    assert_eq!(
        result_of(&events, "c1").content,
        corrected_text("customer X")
    );
    assert_eq!(runtime.current_project(), Some(HERE.to_string()));
}

/// T6: a cancel while the proposal waits withdraws it, by System, with
/// the interrupted note and an error result.
#[tokio::test]
async fn t6_cancel_withdraws_the_proposal_by_system() {
    let dir = tempfile::tempdir().unwrap();
    let mut runtime = rig(
        vec![proposing("c1", THERE, "belongs there"), speaks("ok")],
        dir.path(),
    );
    let decisions = Arc::new(Decisions::new());
    runtime = runtime.with_decisions(decisions.clone());
    let cancel = CancelToken::never();
    let canceller = cancel.clone();
    let waiting = decisions.clone();
    let interrupter = tokio::spawn(async move {
        when_parked(&waiting).await;
        canceller.cancel(steve());
    });

    let mut inbox = Inbox::none();
    let outcome = runtime
        .run_turn_until(steve(), go(), &cancel, &mut inbox, &mut |_| {})
        .await
        .unwrap();
    interrupter.await.unwrap();
    assert_eq!(outcome.reason, INTERRUPTED);
    assert!(decisions.pending().is_empty(), "the wait is gone");

    let events = log_events(&runtime);
    let answered = answers(&events);
    assert_eq!(answered.len(), 1);
    assert_eq!(answered[0].0, Author::System);
    assert_eq!(answered[0].1.answer, DecisionAnswer::Withdrawn);
    assert_eq!(answered[0].1.note.as_deref(), Some(TURN_INTERRUPTED));
    let result = result_of(&events, "c1");
    assert!(result.is_error);
    assert_eq!(result.content, not_answered_text(TURN_INTERRUPTED));
    assert_eq!(runtime.current_project(), Some(HERE.to_string()));
}

/// T6: with no `Decisions` installed the proposal is written and closed
/// at once — a bare runtime has nobody to ask.
#[tokio::test]
async fn t6_a_bare_runtime_has_no_one_to_answer() {
    let dir = tempfile::tempdir().unwrap();
    let mut runtime = rig(
        vec![proposing("c1", THERE, "belongs there"), speaks("ok")],
        dir.path(),
    );
    let outcome = runtime.run_turn(steve(), go(), &mut |_| {}).await.unwrap();
    assert_ne!(outcome.reason, ASKED_HUMAN, "the turn continues in place");

    let events = log_events(&runtime);
    let proposed = proposals(&events);
    assert_eq!(proposed.len(), 1, "the proposal is still written");
    let answered = answers(&events);
    assert_eq!(answered.len(), 1);
    assert_eq!(answered[0].0, Author::System);
    assert_eq!(answered[0].1.answer, DecisionAnswer::Withdrawn);
    assert_eq!(
        answered[0].1.note.as_deref(),
        Some(aigentic_runtime::harness_tools::NO_ONE_TO_ANSWER)
    );
    let result = result_of(&events, "c1");
    assert!(result.is_error);
    assert_eq!(
        result.content,
        not_answered_text(aigentic_runtime::harness_tools::NO_ONE_TO_ANSWER)
    );
}

/// T6: a person's `withdrawn` is an error result, and the turn carries on
/// in place — it does not end `asked_human`.
#[tokio::test]
async fn t6_a_withdrawn_answer_continues_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let mut runtime = rig(
        vec![proposing("c1", THERE, "belongs there"), speaks("ok")],
        dir.path(),
    );
    let decisions = Arc::new(Decisions::new());
    runtime = runtime.with_decisions(decisions.clone());
    let answer = answerer(
        decisions.clone(),
        SwitchAnswer::Withdrawn("ask me later".into()),
        || None,
    );
    let (outcome, _) = tokio::join!(quiet_turn(&mut runtime), answer);
    assert_eq!(outcome.unwrap().reason, "done", "the turn went on in place");

    let events = log_events(&runtime);
    let answered = answers(&events);
    assert_eq!(answered[0].1.answer, DecisionAnswer::Withdrawn);
    assert_eq!(answered[0].1.note.as_deref(), Some("ask me later"));
    let result = result_of(&events, "c1");
    assert!(result.is_error);
    assert_eq!(result.content, not_answered_text("ask me later"));
    assert_eq!(runtime.current_project(), Some(HERE.to_string()));
}

/// T7: in a batch, the calls before the proposal run and the calls after
/// it get the not-run result and do not run.
#[tokio::test]
async fn t7_calls_after_the_proposal_do_not_run() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), "fn main() {}\n").unwrap();
    let log = ThreadLog::open(dir.path(), Ulid::generate()).unwrap();
    let (provider, _seen) = scripted(vec![
        vec![
            ProviderEvent::ToolCall(ToolCall {
                id: "c1".into(),
                name: "read_file".into(),
                args: json!({"path": "a.rs"}),
            }),
            ProviderEvent::ToolCall(propose("c2", THERE, "belongs there")),
            ProviderEvent::ToolCall(ToolCall {
                id: "c3".into(),
                name: "list_dir".into(),
                args: json!({"path": "."}),
            }),
            done("tool_use"),
        ],
        speaks("ok"),
    ]);
    let registry = ToolRegistry::builtin(Workdir::new(dir.path()), DEFAULT_TIMEOUT);
    let mut runtime = Runtime::new(provider, registry, log, AgentId("worker".into()))
        .with_layers(Layers::default().with_project(project(HERE, dir.path(), HERE_RULES)))
        // `with_root` (issue #124): `read_file` and `list_dir` here are
        // inside the rig's tempdir, so no boundary ask parks the turn.
        .with_policy(aigentic_policy::Policy::defaults().with_root(dir.path(), dir.path()));
    let decisions = Arc::new(Decisions::new());
    runtime = runtime.with_decisions(decisions.clone());
    let answer = answerer(decisions.clone(), SwitchAnswer::No, || None);
    let (outcome, _) = tokio::join!(quiet_turn(&mut runtime), answer);
    assert_eq!(outcome.unwrap().reason, ASKED_HUMAN);

    let events = log_events(&runtime);
    let before = result_of(&events, "c1");
    assert!(!before.is_error, "the call before it ran: {before:?}");
    assert!(before.content.contains("fn main"), "{}", before.content);
    let after = result_of(&events, "c3");
    assert!(after.is_error, "the call after it did not run");
    assert_eq!(after.content, NOT_RUN_SIBLING);
    // No `list_dir` result beyond the synthetic one: nothing ran after
    // the proposal.
    assert_eq!(
        events
            .iter()
            .filter(|e| e.kind == EventKind::ToolResult)
            .count(),
        3
    );
    assert_eq!(runtime.current_project(), Some(HERE.to_string()));
    assert!(aigentic_runtime::audit_tool_results(&events).is_empty());
}

/// T7: a proposal naming the project the thread is already in is refused
/// at once: no decision event, no park, and the turn goes on.
#[tokio::test]
async fn t7_a_proposal_naming_the_current_project_is_refused_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut runtime = rig(
        vec![proposing("c1", HERE, "we are already here"), speaks("ok")],
        dir.path(),
    );
    let decisions = Arc::new(Decisions::new());
    runtime = runtime.with_decisions(decisions.clone());
    let outcome = runtime.run_turn(steve(), go(), &mut |_| {}).await.unwrap();
    assert_eq!(
        outcome.reason, "done",
        "nothing parks: the turn answers its own reply"
    );
    assert!(decisions.pending().is_empty());

    let events = log_events(&runtime);
    assert!(proposals(&events).is_empty(), "no decision is logged");
    assert!(answers(&events).is_empty());
    assert!(!kinds(&runtime).contains(&EventKind::ProjectSwitched));
    let result = result_of(&events, "c1");
    assert!(result.is_error);
    assert_eq!(result.content, already_in_text(HERE));
    assert_eq!(runtime.current_project(), Some(HERE.to_string()));
}

/// T7b: killed while parked — the log ends at the call and the proposal.
/// Resume closes the proposal with the daemon-restarted note, and the
/// proposal is not answerable afterwards.
#[tokio::test]
async fn t7b_resume_closes_an_unanswered_proposal() {
    let dir = tempfile::tempdir().unwrap();
    let thread = Ulid::generate();
    let thread_id = thread;
    // A turn that dies with the proposal on the wire: run one until it
    // parks, then drop the runtime without answering.
    let log = ThreadLog::open(dir.path(), thread_id).unwrap();
    let (provider, _seen) = scripted(vec![proposing("c1", THERE, "belongs there")]);
    let decisions = Arc::new(Decisions::new());
    let mut runtime = Runtime::new(
        provider,
        ToolRegistry::empty(),
        log,
        AgentId("worker".into()),
    )
    .with_layers(Layers::default().with_project(project(HERE, dir.path(), HERE_RULES)))
    .with_decisions(decisions.clone());
    let running = tokio::spawn(async move {
        let _ = runtime.run_turn(steve(), go(), &mut |_| {}).await;
    });
    when_parked(&decisions).await;
    running.abort();
    let _ = running.await;

    // The log is what a killed daemon leaves: the call and the proposal,
    // with no answer and no `turn_ended`.
    let log = ThreadLog::open(dir.path(), thread_id).unwrap();
    let (provider, _seen) = scripted(vec![]);
    let restarted = Arc::new(Decisions::new());
    let mut runtime = Runtime::new(
        provider,
        ToolRegistry::empty(),
        log,
        AgentId("worker".into()),
    )
    .with_layers(Layers::default().with_project(project(HERE, dir.path(), HERE_RULES)))
    .with_decisions(restarted.clone());
    let resumed = runtime.resume(None, &mut |_| {}).unwrap();
    assert!(
        matches!(
            resumed,
            Resumed::Interrupted {
                unanswered_calls: 1,
                ..
            }
        ),
        "{resumed:?}"
    );

    let events = log_events(&runtime);
    let proposed = proposals(&events);
    assert_eq!(proposed.len(), 1);
    let answered = answers(&events);
    assert_eq!(answered.len(), 1);
    assert_eq!(answered[0].0, Author::System);
    assert_eq!(answered[0].1.answer, DecisionAnswer::Withdrawn);
    assert_eq!(answered[0].1.note.as_deref(), Some(DAEMON_RESTARTED));
    let answer_event = events
        .iter()
        .find(|e| e.kind == EventKind::DecisionAnswered)
        .unwrap();
    assert_eq!(answer_event.parent_event, Some(proposed[0].0));
    // Closed before the turn's own repair, so a resume that crashes again
    // writes no second answer.
    assert_eq!(
        kinds(&runtime),
        vec![
            EventKind::UserMessage,
            EventKind::AssistantMessage,
            EventKind::DecisionProposed,
            EventKind::DecisionAnswered,
            EventKind::ToolResult,
            EventKind::Interrupted,
        ]
    );

    // Not answerable after the restart: nothing is waiting on it.
    assert!(restarted.pending().is_empty());
    let refused = restarted.decide(
        "c1",
        Answered::Switch {
            answer: SwitchAnswer::Yes,
            by: steve(),
            ctx: SwitchCtx::none(),
        },
    );
    assert!(refused.is_err(), "nothing is waiting on it: {refused:?}");

    // A second resume adds no answer of its own.
    runtime.resume(None, &mut |_| {}).unwrap();
    assert_eq!(answers(&log_events(&runtime)).len(), 1);
}

/// T8: #74's fold pairs every answer with its proposal, and a person's
/// answer is told from a withdrawal.
#[tokio::test]
async fn t8_the_fold_pairs_every_answer_and_tells_a_person_from_a_withdrawal() {
    // A `yes`, answered by a person.
    let dir = tempfile::tempdir().unwrap();
    let mut runtime = rig(
        vec![proposing("c1", THERE, "belongs there"), speaks("ok")],
        dir.path(),
    );
    let decisions = Arc::new(Decisions::new());
    runtime = runtime.with_decisions(decisions.clone());
    let root = dir.path().to_path_buf();
    let answer = answerer(decisions.clone(), SwitchAnswer::Yes, move || {
        Some(target_context(THERE, root.clone()))
    });
    let (_, _) = tokio::join!(quiet_turn(&mut runtime), answer);
    let person = decision_records(&log_events(&runtime));
    assert_eq!(person.orphans, 0, "every answer has its proposal");
    assert_eq!(person.records.len(), 1);
    let yes = person.records[0].answer.clone().expect("answered");
    assert_eq!(yes.answer, DecisionAnswer::Yes);
    assert_eq!(yes.by, steve());

    // And a proposal nobody could answer: withdrawn, by the system.
    let dir = tempfile::tempdir().unwrap();
    let mut runtime = rig(
        vec![proposing("c1", THERE, "belongs there"), speaks("ok")],
        dir.path(),
    );
    runtime.run_turn(steve(), go(), &mut |_| {}).await.unwrap();
    let nobody = decision_records(&log_events(&runtime));
    assert_eq!(nobody.orphans, 0);
    assert_eq!(nobody.records.len(), 1);
    let closed = nobody.records[0].answer.clone().expect("answered");
    assert_eq!(closed.answer, DecisionAnswer::Withdrawn);
    assert_eq!(closed.by, Author::System);
    assert_ne!(closed.by, steve());
}

/// A log with exactly the events a killed-while-parked daemon leaves:
/// kept for the hand-written fixture above, where a test wants the shape
/// without running a turn.
#[allow(dead_code)]
fn killed_while_parked_log(dir: &std::path::Path, thread: Ulid) -> ThreadLog {
    let mut log = ThreadLog::open(dir, thread).unwrap();
    let parent = log
        .append(NewEvent {
            kind: EventKind::AssistantMessage,
            author: Author::Agent(AgentId("worker".into())),
            payload: serde_json::to_value(AssistantMessagePayload {
                blocks: vec![ContentBlock::ToolCall(propose(
                    "c1",
                    THERE,
                    "belongs there",
                ))],
                usage: None,
                finish_reason: None,
            })
            .unwrap(),
            parent_event: None,
        })
        .unwrap();
    log.append(NewEvent {
        kind: EventKind::DecisionProposed,
        author: Author::Agent(AgentId("worker".into())),
        payload: serde_json::to_value(DecisionProposedPayload {
            kind: DecisionKind::Project,
            proposal: "switch to there".into(),
            target: Some(THERE.into()),
            reason: "belongs there".into(),
            call_id: Some("c1".into()),
            stage: DecisionStage::Ask,
        })
        .unwrap(),
        parent_event: Some(parent.id),
    })
    .unwrap();
    log.append(NewEvent {
        kind: EventKind::UserMessage,
        author: steve(),
        payload: serde_json::to_value(UserMessagePayload::new(go())).unwrap(),
        parent_event: None,
    })
    .unwrap();
    log
}

/// T13: two contexts that are not the same `Arc` compare unequal — the
/// `PartialEq` `Answered::Switch` needs, without comparing project
/// contents.
#[test]
fn switch_ctx_compares_by_identity() {
    let dir = tempfile::tempdir().unwrap();
    let (a, _ack_a) = SwitchCtx::oneshot(target_context("p", dir.path().to_path_buf()));
    let (b, _ack_b) = SwitchCtx::oneshot(target_context("p", dir.path().to_path_buf()));
    let twin = a.clone();
    assert_eq!(a, twin, "a clone is the same context");
    assert_ne!(a, b, "two contexts are not one");
    assert_ne!(a, SwitchCtx::none(), "an empty context is not a context");
}
