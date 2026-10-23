//! The background job proposal: `start_job` parks the turn, a person
//! answers, and nothing moves — a `yes` only records that a job was
//! approved, which is what makes it different from `suggest_project`.
//!
//! Every expected result text is the runtime's own constant or function,
//! never a retyped copy.

mod common;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use aigentic_core::{AgentId, Author, ContentBlock, EventKind, ProviderEvent, ToolCall};
use aigentic_log::{
    DecisionAnswer, DecisionAnsweredPayload, DecisionKind, DecisionProposedPayload, DecisionStage,
    JobProposed, ThreadLog, ToolResultPayload, decision_records,
};
use aigentic_policy::Policy;
use aigentic_runtime::harness_tools::{
    BRIEF_REQUIRED, JOB_NO_TARGET, JOB_REASON, NO_ONE_TO_ANSWER, NOT_IN_PROJECT,
    NOT_RUN_JOB_SIBLING, NOT_RUN_JOB_SOLO, START_JOB, START_JOB_DESCRIPTION, STEP_REFUSED,
    StartJobArgs, TASK_REQUIRED, TURN_INTERRUPTED, corrected_text, harness_names, harness_specs,
    harness_specs_with, is_harness_tool, job_approved_text, job_declined_text, job_proposal_text,
    not_answered_text, start_job_spec,
};
use aigentic_runtime::project::ProjectFile;
use aigentic_runtime::{
    ASKED_HUMAN, Answered, CancelToken, DecisionError, Decisions, INTERRUPTED, Inbox, Layers,
    Pending, Project, ProjectContext, ProjectRow, Runtime, Signal, SwitchAnswer, SwitchCtx,
};
use aigentic_tools::ToolRegistry;
use common::{done, scripted, steve};
use serde_json::json;
use ulid::Ulid;

const HERE: &str = "here";
const THERE: &str = "there";
const HERE_RULES: &str = "here: answer in English";
const THERE_RULES: &str = "there: answer in Swedish";
const LISTING: &str = "Projects in reach. aigentic (here) ~/h · aigentic-web ~/t";
const TASK: &str = "close #77";
const BRIEF: &str = "#77 is ready: build it, gate it, push";

fn reach() -> (Option<String>, Vec<ProjectRow>) {
    (Some(LISTING.into()), Vec::new())
}

/// The call the model makes.
fn propose(id: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: START_JOB.into(),
        args,
    }
}

fn asking(id: &str) -> Vec<ProviderEvent> {
    vec![
        ProviderEvent::ToolCall(propose(id, json!({"task": TASK, "brief": BRIEF}))),
        done("tool_use"),
    ]
}

fn speaks(text: &str) -> Vec<ProviderEvent> {
    vec![ProviderEvent::TextDelta(text.into()), done("stop")]
}

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

/// The context a `yes` hands over: a job's answer takes none, so this
/// exists only to prove it.
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

/// A runtime over its own log, with no decisions installed and, unless
/// `in_project`, no project either.
fn rig(script: Vec<Vec<ProviderEvent>>, dir: &std::path::Path, in_project: bool) -> Runtime {
    let log = ThreadLog::open(dir, Ulid::generate()).unwrap();
    let (provider, _seen) = scripted(script);
    let registry: ToolRegistry =
        vec![Box::new(common::EchoTool(Arc::new(Mutex::new(vec![]))))
            as Box<dyn aigentic_core::Tool>]
        .into();
    let runtime = Runtime::new(provider, registry, log, AgentId("worker".into()))
        .with_projects(Some(LISTING.into()), Vec::new());
    if in_project {
        runtime.with_layers(Layers::default().with_project(project(HERE, dir, HERE_RULES)))
    } else {
        runtime.with_layers(Layers::default())
    }
}

fn here(script: Vec<Vec<ProviderEvent>>, dir: &std::path::Path) -> Runtime {
    rig(script, dir, true)
}

fn go() -> Vec<ContentBlock> {
    vec![ContentBlock::Text("go".into())]
}

async fn quiet_turn(
    runtime: &mut Runtime,
) -> Result<aigentic_runtime::TurnOutcome, aigentic_runtime::RuntimeError> {
    runtime.run_turn(steve(), go(), &mut |_| {}).await
}

fn log_events(runtime: &Runtime) -> Vec<aigentic_core::Event> {
    runtime.log().read_all().unwrap()
}

fn kinds(runtime: &Runtime) -> Vec<EventKind> {
    log_events(runtime).iter().map(|e| e.kind).collect()
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

fn policy_rule(events: &[aigentic_core::Event], id: &str) -> String {
    let event = events
        .iter()
        .find(|e| {
            e.kind == EventKind::ToolResult
                && serde_json::from_value::<ToolResultPayload>(e.payload.clone())
                    .is_ok_and(|p| p.result.id == id)
        })
        .expect("the call has a result");
    match serde_json::from_value::<ToolResultPayload>(event.payload.clone())
        .expect("a tool result payload")
        .policy
    {
        Some(aigentic_log::PolicyRecord::Rule { rule, .. }) => rule,
        other => panic!("a rule record: {other:?}"),
    }
}

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

/// The proposal is in the log before the wait fires, with the work it
/// asked for, and the waiting entry names the same call.
#[tokio::test]
async fn start_job_parks_the_turn_with_a_job_proposal() {
    let dir = tempfile::tempdir().unwrap();
    let mut runtime = here(vec![asking("c1"), speaks("ok")], dir.path());
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
    let waiting = decisions.clone();
    let parked = tokio::spawn(async move { when_parked(&waiting).await });
    let answer = answerer(decisions.clone(), SwitchAnswer::No, || None);
    let (outcome, _) = tokio::join!(runtime.run_turn(steve(), go(), &mut observer), answer);
    let (call_id, project, reason) = parked.await.unwrap();
    assert_eq!(outcome.unwrap().reason, ASKED_HUMAN);
    assert_eq!(
        seen,
        vec!["proposed".to_string(), format!("waiting:{HERE}")],
        "the event is written first, then the wait fires"
    );
    assert_eq!(call_id, "c1", "the waiting entry names the call");
    assert_eq!(project, HERE);
    assert_eq!(reason, JOB_REASON);

    let events = log_events(&runtime);
    let proposed = proposals(&events);
    assert_eq!(proposed.len(), 1);
    let p = &proposed[0].1;
    assert_eq!(p.kind, DecisionKind::Job);
    assert_eq!(p.call_id.as_deref(), Some("c1"));
    assert_eq!(
        p.job,
        Some(JobProposed {
            task: TASK.into(),
            brief: BRIEF.into(),
            read_only: false,
        })
    );
    assert_eq!(p.proposal, job_proposal_text(TASK));
    assert_eq!(p.target.as_deref(), Some(HERE));
    assert_eq!(p.reason, JOB_REASON);
    assert_eq!(p.stage, DecisionStage::Ask);
}

/// A `yes` approves the job and moves nothing — no `project_switched`,
/// the thread still in `here` — and a handed-over context is refused.
#[tokio::test]
async fn a_yes_answers_the_proposal_and_switches_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mut runtime = here(vec![asking("c1"), speaks("ok")], dir.path());
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
        Some(Err(JOB_NO_TARGET.to_owned())),
        "a job's answer takes no context"
    );

    let events = log_events(&runtime);
    let answered = answers(&events);
    assert_eq!(answered.len(), 1);
    assert_eq!(answered[0].0, steve());
    assert_eq!(answered[0].1.answer, DecisionAnswer::Yes);
    assert_eq!(answered[0].1.correction, None);
    let result = result_of(&events, "c1");
    assert!(!result.is_error, "an approval is not an error");
    assert_eq!(result.content, job_approved_text(TASK));
    assert!(!kinds(&runtime).contains(&EventKind::ProjectSwitched));
    assert_eq!(runtime.current_project(), Some(HERE.to_string()));
    let fold = decision_records(&events);
    assert_eq!(fold.records.len(), 1);
    assert_eq!(
        fold.records[0].answer.as_ref().map(|a| a.answer),
        Some(DecisionAnswer::Yes)
    );
}

/// A `no` leaves the thread where it was and says so.
#[tokio::test]
async fn a_no_leaves_the_thread_where_it_is() {
    let dir = tempfile::tempdir().unwrap();
    let mut runtime = here(vec![asking("c1"), speaks("ok")], dir.path());
    let decisions = Arc::new(Decisions::new());
    runtime = runtime.with_decisions(decisions.clone());
    let answer = answerer(decisions.clone(), SwitchAnswer::No, || None);

    let (outcome, _) = tokio::join!(quiet_turn(&mut runtime), answer);
    assert_eq!(outcome.unwrap().reason, ASKED_HUMAN);

    let events = log_events(&runtime);
    let answered = answers(&events);
    assert_eq!(answered.len(), 1);
    assert_eq!(answered[0].1.answer, DecisionAnswer::No);
    let result = result_of(&events, "c1");
    assert!(!result.is_error);
    assert_eq!(result.content, job_declined_text(TASK));
    assert!(!kinds(&runtime).contains(&EventKind::ProjectSwitched));
    assert_eq!(runtime.current_project(), Some(HERE.to_string()));
}

/// A correction names the project the job belongs to, and moves
/// nothing.
#[tokio::test]
async fn a_correction_names_the_project_it_belongs_to() {
    let dir = tempfile::tempdir().unwrap();
    let mut runtime = here(vec![asking("c1"), speaks("ok")], dir.path());
    let decisions = Arc::new(Decisions::new());
    runtime = runtime.with_decisions(decisions.clone());
    let answer = answerer(
        decisions.clone(),
        SwitchAnswer::Corrected(THERE.into()),
        || None,
    );

    let (outcome, _) = tokio::join!(quiet_turn(&mut runtime), answer);
    assert_eq!(outcome.unwrap().reason, ASKED_HUMAN);

    let events = log_events(&runtime);
    let answered = answers(&events);
    assert_eq!(answered.len(), 1);
    assert_eq!(answered[0].1.answer, DecisionAnswer::Corrected);
    assert_eq!(answered[0].1.correction.as_deref(), Some(THERE));
    assert_eq!(result_of(&events, "c1").content, corrected_text(THERE));
    assert!(!kinds(&runtime).contains(&EventKind::ProjectSwitched));
    assert_eq!(runtime.current_project(), Some(HERE.to_string()));
}

/// An interrupt while parked withdraws the proposal, by System, with
/// the interrupted note.
#[tokio::test]
async fn an_interrupt_withdraws_a_job_proposal() {
    let dir = tempfile::tempdir().unwrap();
    let mut runtime = here(vec![asking("c1"), speaks("ok")], dir.path());
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
}

/// With no `Decisions` the proposal is written and closed at once —
/// a bare runtime has nobody to ask.
#[tokio::test]
async fn a_job_proposal_with_no_one_to_answer_is_written_and_withdrawn() {
    let dir = tempfile::tempdir().unwrap();
    let mut runtime = here(vec![asking("c1"), speaks("ok")], dir.path());
    let outcome = runtime.run_turn(steve(), go(), &mut |_| {}).await.unwrap();
    assert_ne!(outcome.reason, ASKED_HUMAN, "the turn continues in place");

    let events = log_events(&runtime);
    assert_eq!(proposals(&events).len(), 1, "the proposal is still written");
    let fold = decision_records(&events);
    assert_eq!(fold.orphans, 0);
    let answer = fold.records[0].answer.as_ref().expect("answered at once");
    assert_eq!(answer.answer, DecisionAnswer::Withdrawn);
    assert_eq!(answer.by, Author::System);
    let answered = answers(&events);
    assert_eq!(answered[0].1.note.as_deref(), Some(NO_ONE_TO_ANSWER));
    let result = result_of(&events, "c1");
    assert!(result.is_error);
    assert_eq!(result.content, not_answered_text(NO_ONE_TO_ANSWER));
}

/// A thread in no project has nowhere to run a job, and is refused
/// without writing anything.
#[tokio::test]
async fn a_thread_in_no_project_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut runtime = rig(vec![asking("c1"), speaks("ok")], dir.path(), false);
    runtime = runtime.with_decisions(Arc::new(Decisions::new()));
    runtime.run_turn(steve(), go(), &mut |_| {}).await.unwrap();

    let events = log_events(&runtime);
    assert!(proposals(&events).is_empty(), "nothing is proposed");
    let result = result_of(&events, "c1");
    assert!(result.is_error);
    assert_eq!(result.content, NOT_IN_PROJECT);
}

/// A step thread proposes from the front thread only, and is refused.
#[tokio::test]
async fn a_step_thread_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut runtime = here(vec![asking("c1"), speaks("ok")], dir.path())
        .with_step("implement", &[])
        .expect("a valid deny list");
    runtime = runtime.with_decisions(Arc::new(Decisions::new()));
    let outcome = runtime.run_turn(steve(), go(), &mut |_| {}).await.unwrap();
    assert_ne!(outcome.reason, ASKED_HUMAN);

    let events = log_events(&runtime);
    assert!(proposals(&events).is_empty(), "nothing is proposed");
    let result = result_of(&events, "c1");
    assert!(result.is_error);
    assert_eq!(result.content, STEP_REFUSED);
}

/// A blank task or brief is refused before anything is written, and
/// nothing waits afterwards.
#[tokio::test]
async fn an_empty_task_or_brief_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let blank_task = vec![
        ProviderEvent::ToolCall(propose("c1", json!({"task": "   ", "brief": BRIEF}))),
        done("tool_use"),
    ];
    let blank_brief = vec![
        ProviderEvent::ToolCall(propose("c2", json!({"task": TASK, "brief": ""}))),
        done("tool_use"),
    ];
    let mut runtime = here(
        vec![blank_task, speaks("ok"), blank_brief, speaks("ok")],
        dir.path(),
    );
    let decisions = Arc::new(Decisions::new());
    runtime = runtime.with_decisions(decisions.clone());
    runtime.run_turn(steve(), go(), &mut |_| {}).await.unwrap();
    runtime.run_turn(steve(), go(), &mut |_| {}).await.unwrap();

    let events = log_events(&runtime);
    assert!(proposals(&events).is_empty(), "nothing is proposed");
    let task = result_of(&events, "c1");
    assert!(task.is_error);
    assert_eq!(task.content, TASK_REQUIRED);
    let brief = result_of(&events, "c2");
    assert!(brief.is_error);
    assert_eq!(brief.content, BRIEF_REQUIRED);
    assert!(decisions.pending().is_empty(), "nothing waits");
    assert_eq!(
        decisions.decide(
            "c1",
            Answered::Switch {
                answer: SwitchAnswer::No,
                by: steve(),
                ctx: SwitchCtx::none(),
            }
        ),
        Err(DecisionError::NotPending("c1".into()))
    );
}

/// An unknown argument is refused as `invalid arguments: …`, with no
/// event.
#[tokio::test]
async fn an_unknown_argument_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut runtime = here(
        vec![
            vec![
                ProviderEvent::ToolCall(propose(
                    "c1",
                    json!({"task": TASK, "brief": BRIEF, "mode": "fast"}),
                )),
                done("tool_use"),
            ],
            speaks("ok"),
        ],
        dir.path(),
    );
    runtime = runtime.with_decisions(Arc::new(Decisions::new()));
    runtime.run_turn(steve(), go(), &mut |_| {}).await.unwrap();

    let events = log_events(&runtime);
    assert!(proposals(&events).is_empty(), "nothing is proposed");
    let result = result_of(&events, "c1");
    assert!(result.is_error);
    assert!(
        result.content.starts_with("invalid arguments: "),
        "{}",
        result.content
    );
}

/// A proposal stands alone — the calls after it in its batch do not
/// run, and say why.
#[tokio::test]
async fn calls_after_a_job_proposal_do_not_run() {
    let dir = tempfile::tempdir().unwrap();
    let mut runtime = here(
        vec![
            vec![
                ProviderEvent::ToolCall(propose("c1", json!({"task": TASK, "brief": BRIEF}))),
                ProviderEvent::ToolCall(ToolCall {
                    id: "c2".into(),
                    name: "echo".into(),
                    args: json!({"msg": "after"}),
                }),
                done("tool_use"),
            ],
            speaks("ok"),
        ],
        dir.path(),
    );
    let decisions = Arc::new(Decisions::new());
    runtime = runtime.with_decisions(decisions.clone());
    let answer = answerer(decisions.clone(), SwitchAnswer::No, || None);
    let (outcome, _) = tokio::join!(quiet_turn(&mut runtime), answer);
    assert_eq!(outcome.unwrap().reason, ASKED_HUMAN);

    let events = log_events(&runtime);
    let sibling = result_of(&events, "c2");
    assert!(sibling.is_error, "a refusal is an error result");
    assert_eq!(sibling.content, NOT_RUN_JOB_SIBLING);
    assert_eq!(policy_rule(&events, "c2"), NOT_RUN_JOB_SOLO);
    assert_eq!(proposals(&events).len(), 1, "the proposal stands");
}

/// The harness list carries no `start_job` spec: that list is what a
/// thread's kind does not decide. `Runtime::tool_specs` offers it in an
/// ordinary thread and in no other.
#[tokio::test]
async fn the_harness_list_carries_no_start_job_spec() {
    for offered in [
        harness_specs_with(true, true, true, true),
        harness_specs(true, true, true),
    ] {
        assert!(
            !offered.iter().any(|s| s.name == START_JOB),
            "start_job is not in a model's tool list"
        );
    }
    assert!(!harness_names().iter().any(|name| name == START_JOB));
    assert!(is_harness_tool(START_JOB));
}

/// The spec asks for a task and a brief, and takes nothing else.
#[tokio::test]
async fn the_start_job_spec_asks_for_a_task_and_a_brief() {
    let spec = start_job_spec();
    assert_eq!(spec.name, START_JOB);
    assert_eq!(spec.description, START_JOB_DESCRIPTION);
    let schema = &spec.schema;
    assert_eq!(schema["additionalProperties"], json!(false));
    let properties = schema["properties"].as_object().expect("a properties map");
    let mut names: Vec<&str> = properties.keys().map(String::as_str).collect();
    names.sort_unstable();
    assert_eq!(names, vec!["brief", "read_only", "task"]);
    assert_eq!(properties["read_only"]["type"], json!("boolean"));
    let required = schema["required"].as_array().expect("a required list");
    let mut required: Vec<&str> = required.iter().map(|v| v.as_str().unwrap()).collect();
    required.sort_unstable();
    assert_eq!(required, vec!["brief", "task"]);

    // The arguments: both required, an unknown key refused, and the flag
    // defaulting to "may change files".
    let parsed = serde_json::from_value::<StartJobArgs>(json!({"task": TASK, "brief": BRIEF}));
    assert_eq!(
        parsed.unwrap(),
        StartJobArgs {
            task: TASK.into(),
            brief: BRIEF.into(),
            read_only: false,
        }
    );
    assert!(
        serde_json::from_value::<StartJobArgs>(json!({"task": TASK, "brief": BRIEF, "extra": 1}))
            .is_err()
    );
}
