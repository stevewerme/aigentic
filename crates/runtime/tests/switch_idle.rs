//! The project proposal raised while no turn runs (issue #85):
//! `propose_switch_idle` writes it, `answer_switch_idle` settles it, and
//! a restart withdraws it. Sibling of `suggest_project.rs`, whose rig the
//! two `apply_switch` cases below borrow: each test binary is standalone,
//! so the helpers are repeated rather than shared.
//!
//! Every expected value is the runtime's own constant or function, or the
//! payload the log holds.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use aigentic_core::{AgentId, Author, Event, EventKind, ProviderEvent, ToolCall};
use aigentic_log::{
    DecisionAnswer, DecisionAnsweredPayload, DecisionKind, DecisionProposedPayload, DecisionStage,
    NewEvent, ProjectSwitchedPayload, STARTUP_PREFIX, ThreadLog, decision_records,
    declined_at_startup, is_startup_call,
};
use aigentic_policy::Policy;
use aigentic_runtime::harness_tools::{
    NO_CONTEXT, not_answered_text, proposal_text, switch_failed,
};
use aigentic_runtime::project::ProjectFile;
use aigentic_runtime::{
    DAEMON_RESTARTED, Decisions, Layers, Project, ProjectContext, Resumed, Runtime, RuntimeError,
    Settled, SwitchAnswer, SwitchCtx,
};
use aigentic_tools::ToolRegistry;
use common::{done, scripted, steve};
use serde_json::json;
use ulid::Ulid;

const HERE: &str = "here";
const THERE: &str = "there";
const REASON: &str = "the folder's project is where this belongs";
const LISTING: &str = "Projects in reach. aigentic (here) ~/h · aigentic-web ~/t";
const SUGGEST_PROJECT: &str = "suggest_project";

// ----------------------------------------------------------- the rig

fn project(name: &str, root: &Path) -> Project {
    Project {
        name: name.into(),
        root: root.to_path_buf(),
        file: ProjectFile::default(),
        instructions: Some(format!("{name}: answer in English")),
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
        layers: Layers::default().with_project(project(name, &root)),
        policy: Policy::defaults(),
        skills: aigentic_runtime::aigentic_skills::SkillSet::default(),
        registry: ToolRegistry::empty(),
        provider,
        model_label: "target-model".into(),
        projects: Some(LISTING.into()),
        profile: None,
        effort: None,
        prices: None,
    }
}

/// A runtime over `log`, in `here`, with no decisions installed.
fn over(log: ThreadLog, dir: &Path) -> Runtime {
    let (provider, _seen) = scripted(vec![speaks("ok")]);
    Runtime::new(
        provider,
        ToolRegistry::empty(),
        log,
        AgentId("worker".into()),
    )
    .with_layers(Layers::default().with_project(project(HERE, dir)))
    .with_projects(Some(LISTING.into()))
}

/// A fresh log in `dir`, and a runtime over it. The thread id comes back
/// so a test can reopen the same log as a restart would.
fn rig(dir: &Path) -> (Runtime, Ulid) {
    let log = ThreadLog::open(dir, Ulid::generate()).unwrap();
    let thread = log.thread_id();
    (over(log, dir), thread)
}

/// The turn rig, for the two `apply_switch` cases (issue #7's shape):
/// `here` with a scripted provider.
fn turn_rig(script: Vec<Vec<ProviderEvent>>, dir: &Path) -> Runtime {
    let log = ThreadLog::open(dir, Ulid::generate()).unwrap();
    let (provider, _seen) = scripted(script);
    Runtime::new(
        provider,
        ToolRegistry::empty(),
        log,
        AgentId("worker".into()),
    )
    .with_layers(Layers::default().with_project(project(HERE, dir)))
    .with_projects(Some(LISTING.into()))
}

/// A step thread: `suggest_project` is unoffered there, but a call that
/// arrives anyway still runs (issue #7's T4).
fn project_rig(script: Vec<Vec<ProviderEvent>>, dir: &Path) -> Runtime {
    let log = ThreadLog::open(dir, Ulid::generate()).unwrap();
    let (provider, _seen) = scripted(script);
    Runtime::new(
        provider,
        ToolRegistry::empty(),
        log,
        AgentId("worker".into()),
    )
    .with_layers(Layers::default().with_project(project(HERE, dir)))
    .with_projects(Some(LISTING.into()))
    .with_step("implement", &[])
    .expect("a valid deny list")
}

fn proposing(id: &str, name: &str, reason: &str) -> Vec<ProviderEvent> {
    vec![
        ProviderEvent::ToolCall(ToolCall {
            id: id.into(),
            name: SUGGEST_PROJECT.into(),
            args: json!({"project": name, "reason": reason}),
        }),
        done("tool_use"),
    ]
}

fn speaks(text: &str) -> Vec<ProviderEvent> {
    vec![ProviderEvent::TextDelta(text.into()), done("stop")]
}

async fn quiet_turn(
    runtime: &mut Runtime,
) -> Result<aigentic_runtime::TurnOutcome, aigentic_runtime::RuntimeError> {
    runtime
        .run_turn(
            steve(),
            vec![aigentic_core::ContentBlock::Text("go".into())],
            &mut |_| {},
        )
        .await
}

/// Wait for the turn to park on a switch, and hand back the call id.
async fn when_parked(decisions: &Arc<Decisions>) -> String {
    for _ in 0..4_000 {
        if let Some(aigentic_runtime::Pending::Switch { call_id, .. }) = decisions.pending().first()
        {
            return call_id.clone();
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
        let call_id = when_parked(&decisions).await;
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
                aigentic_runtime::Answered::Switch {
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

// ------------------------------------------------------- the readers

fn log_events(runtime: &Runtime) -> Vec<Event> {
    runtime.log().read_all().unwrap()
}

fn kinds(runtime: &Runtime) -> Vec<EventKind> {
    log_events(runtime).iter().map(|e| e.kind).collect()
}

fn answers(events: &[Event]) -> Vec<(Author, DecisionAnsweredPayload)> {
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

fn proposals(events: &[Event]) -> Vec<(Ulid, DecisionProposedPayload)> {
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

fn result_of(events: &[Event], id: &str) -> aigentic_core::ToolResult {
    let event = events
        .iter()
        .rev()
        .find(|e| {
            e.kind == EventKind::ToolResult
                && serde_json::from_value::<aigentic_log::ToolResultPayload>(e.payload.clone())
                    .is_ok_and(|p| p.result.id == id)
        })
        .expect("the call has a result");
    serde_json::from_value::<aigentic_log::ToolResultPayload>(event.payload.clone())
        .expect("a tool result payload")
        .result
}

/// Raise an idle proposal, the way #92 will.
async fn propose(runtime: &mut Runtime) -> aigentic_runtime::IdleProposal {
    runtime
        .propose_switch_idle(THERE, REASON, &mut |_| {})
        .await
        .expect("an idle proposal")
}

async fn settle(
    runtime: &mut Runtime,
    proposal: Ulid,
    answer: SwitchAnswer,
    ctx: SwitchCtx,
) -> Result<Settled, RuntimeError> {
    runtime
        .answer_switch_idle(proposal, answer, steve(), ctx, &mut |_| {})
        .await
}

// --------------------------------------------------- hand-written logs

fn append(
    log: &mut ThreadLog,
    kind: EventKind,
    author: Author,
    payload: serde_json::Value,
) -> Ulid {
    append_naming(log, kind, author, payload, None)
}

fn append_naming(
    log: &mut ThreadLog,
    kind: EventKind,
    author: Author,
    payload: serde_json::Value,
    parent: Option<Ulid>,
) -> Ulid {
    log.append(NewEvent {
        kind,
        author,
        payload,
        parent_event: parent,
    })
    .expect("appended")
    .id
}

/// A `decision_proposed` with `call_id`, as `propose_switch_idle` writes
/// one when it is a start-up proposal.
fn append_proposal(log: &mut ThreadLog, call_id: &str, target: &str) -> Ulid {
    append(
        log,
        EventKind::DecisionProposed,
        Author::System,
        serde_json::to_value(DecisionProposedPayload {
            kind: DecisionKind::Project,
            proposal: proposal_text(target),
            target: Some(target.to_owned()),
            reason: REASON.to_owned(),
            call_id: Some(call_id.to_owned()),
            stage: DecisionStage::Ask,
        })
        .unwrap(),
    )
}

fn append_answer(
    log: &mut ThreadLog,
    proposal: Ulid,
    answer: DecisionAnswer,
    correction: Option<&str>,
    by: Author,
) {
    append_naming(
        log,
        EventKind::DecisionAnswered,
        by,
        serde_json::to_value(DecisionAnsweredPayload {
            answer,
            correction: correction.map(str::to_owned),
            note: None,
        })
        .unwrap(),
        Some(proposal),
    );
}

fn append_switch(log: &mut ThreadLog, to: &str) {
    append(
        log,
        EventKind::ProjectSwitched,
        Author::Agent(AgentId("worker".into())),
        serde_json::to_value(ProjectSwitchedPayload {
            from: Some(HERE.into()),
            to: Some(to.into()),
            root: PathBuf::from("/tmp").join(to),
            workspace: None,
        })
        .unwrap(),
    );
}

/// A log dir of its own, so a test's hand-written events never mix with
/// another's.
fn hand_log(dir: &Path) -> ThreadLog {
    ThreadLog::open(dir, Ulid::generate()).unwrap()
}

// ------------------------------------------------------------- T1

/// T1: an idle proposal is a `decision_proposed` by the system, with a
/// `startup-` call id, and a step thread is refused without a write.
#[tokio::test]
async fn t1_an_idle_proposal_is_the_systems_with_a_startup_call_id() {
    let dir = tempfile::tempdir().unwrap();
    let (mut runtime, _) = rig(dir.path());
    let made = propose(&mut runtime).await;

    let events = log_events(&runtime);
    let proposed = proposals(&events);
    assert_eq!(proposed.len(), 1, "one proposal");
    let event = events
        .iter()
        .find(|e| e.kind == EventKind::DecisionProposed)
        .expect("the proposal event");
    assert_eq!(event.author, Author::System, "the system proposed it");
    assert_eq!(proposed[0].1.kind, DecisionKind::Project);
    assert_eq!(proposed[0].1.target.as_deref(), Some(THERE));
    assert_eq!(proposed[0].1.reason, REASON);
    assert_eq!(proposed[0].1.stage, DecisionStage::Ask);
    assert_eq!(proposed[0].1.proposal, proposal_text(THERE));

    // What it hands back names the event, and its call id is the one in
    // the log.
    assert_eq!(made.proposal, event.id, "the event's id");
    assert_eq!(
        proposed[0].1.call_id.as_deref(),
        Some(made.call_id.as_str()),
        "the call id in the log"
    );
    assert!(made.call_id.starts_with(STARTUP_PREFIX), "{}", made.call_id);
    assert!(is_startup_call(Some(&made.call_id)));

    // A step thread has no project of its own to propose leaving, and
    // writing one there would be a proposal nobody could answer.
    let step_dir = tempfile::tempdir().unwrap();
    let mut step = project_rig(vec![speaks("ok")], step_dir.path());
    let refused = step.propose_switch_idle(THERE, REASON, &mut |_| {}).await;
    assert!(
        matches!(refused, Err(RuntimeError::StepThread(_))),
        "{refused:?}"
    );
    assert!(log_events(&step).is_empty(), "nothing is appended");
}

// ------------------------------------------------------------- T2

/// T2a: a `yes` with a context switches, then records the answer, and the
/// ack fires `Ok`.
#[tokio::test]
async fn t2a_a_yes_while_idle_switches_and_then_records_the_answer() {
    let dir = tempfile::tempdir().unwrap();
    let (mut runtime, _) = rig(dir.path());
    let made = propose(&mut runtime).await;
    let (ctx, ack) = SwitchCtx::oneshot(target_context(THERE, dir.path().to_path_buf()));
    let settled = settle(&mut runtime, made.proposal, SwitchAnswer::Yes, ctx)
        .await
        .expect("settled");
    assert_eq!(settled, Settled::Switched);
    assert_eq!(ack.await.unwrap(), Ok(()), "the context was used");
    assert_eq!(runtime.current_project(), Some(THERE.to_string()));

    assert_eq!(
        kinds(&runtime),
        vec![
            EventKind::DecisionProposed,
            EventKind::ProjectSwitched,
            EventKind::DecisionAnswered,
        ],
        "the switch lands before the answer"
    );
    let events = log_events(&runtime);
    let answered = answers(&events);
    assert_eq!(answered.len(), 1);
    assert_eq!(answered[0].0, steve());
    assert_eq!(answered[0].1.answer, DecisionAnswer::Yes);
    let parent = events
        .iter()
        .find(|e| e.kind == EventKind::DecisionAnswered)
        .unwrap()
        .parent_event;
    assert_eq!(parent, Some(made.proposal), "it answers the proposal");
}

/// T2b: `no` is recorded and the thread stays.
#[tokio::test]
async fn t2b_a_no_while_idle_is_recorded_and_the_thread_stays() {
    let dir = tempfile::tempdir().unwrap();
    let (mut runtime, _) = rig(dir.path());
    let made = propose(&mut runtime).await;
    let settled = settle(
        &mut runtime,
        made.proposal,
        SwitchAnswer::No,
        SwitchCtx::none(),
    )
    .await
    .expect("settled");
    assert_eq!(settled, Settled::Declined);
    assert_eq!(runtime.current_project(), Some(HERE.to_string()));
    assert_eq!(
        kinds(&runtime),
        vec![EventKind::DecisionProposed, EventKind::DecisionAnswered]
    );
    let answered = answers(&log_events(&runtime));
    assert_eq!(answered.len(), 1);
    assert_eq!(answered[0].0, steve());
    assert_eq!(answered[0].1.answer, DecisionAnswer::No);
}

/// T2c: a correction carries the person's words.
#[tokio::test]
async fn t2c_a_correction_while_idle_carries_the_persons_words() {
    let dir = tempfile::tempdir().unwrap();
    let (mut runtime, _) = rig(dir.path());
    let made = propose(&mut runtime).await;
    let words = "it belongs to somebody else";
    let settled = settle(
        &mut runtime,
        made.proposal,
        SwitchAnswer::Corrected(words.into()),
        SwitchCtx::none(),
    )
    .await
    .expect("settled");
    assert_eq!(settled, Settled::Corrected(words.into()));
    let answered = answers(&log_events(&runtime));
    assert_eq!(answered.len(), 1);
    assert_eq!(answered[0].1.answer, DecisionAnswer::Corrected);
    assert_eq!(answered[0].1.correction.as_deref(), Some(words));
}

/// T2d: a withdrawal is recorded by the system, with its note.
#[tokio::test]
async fn t2d_a_withdrawal_while_idle_is_recorded_by_the_system() {
    let dir = tempfile::tempdir().unwrap();
    let (mut runtime, _) = rig(dir.path());
    let made = propose(&mut runtime).await;
    let note = "a message came in";
    let settled = settle(
        &mut runtime,
        made.proposal,
        SwitchAnswer::Withdrawn(note.into()),
        SwitchCtx::none(),
    )
    .await
    .expect("settled");
    assert_eq!(settled, Settled::Withdrawn(note.into()));
    let answered = answers(&log_events(&runtime));
    assert_eq!(answered.len(), 1);
    assert_eq!(answered[0].0, Author::System);
    assert_eq!(answered[0].1.answer, DecisionAnswer::Withdrawn);
    assert_eq!(answered[0].1.note.as_deref(), Some(note));
}

/// T2e: a `yes` with no context moves nothing, and is withdrawn with
/// `switch_failed(NO_CONTEXT)`.
#[tokio::test]
async fn t2e_a_yes_with_no_context_is_withdrawn_and_moves_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (mut runtime, _) = rig(dir.path());
    let made = propose(&mut runtime).await;
    let settled = settle(
        &mut runtime,
        made.proposal,
        SwitchAnswer::Yes,
        SwitchCtx::none(),
    )
    .await
    .expect("settled");
    let note = switch_failed(NO_CONTEXT);
    assert_eq!(settled, Settled::NoContext(note.clone()));
    assert!(
        !kinds(&runtime).contains(&EventKind::ProjectSwitched),
        "nothing moved"
    );
    assert_eq!(runtime.current_project(), Some(HERE.to_string()));
    let answered = answers(&log_events(&runtime));
    assert_eq!(answered.len(), 1);
    assert_eq!(answered[0].0, Author::System);
    assert_eq!(answered[0].1.answer, DecisionAnswer::Withdrawn);
    assert_eq!(answered[0].1.note.as_deref(), Some(note.as_str()));
}

/// T2f: a second answer, and an id that names no proposal, are both
/// refused with nothing written — so the fold sees no orphan.
#[tokio::test]
async fn t2f_a_second_answer_is_refused_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (mut runtime, _) = rig(dir.path());
    let made = propose(&mut runtime).await;
    settle(
        &mut runtime,
        made.proposal,
        SwitchAnswer::No,
        SwitchCtx::none(),
    )
    .await
    .expect("settled");
    let before = log_events(&runtime);

    let again = settle(
        &mut runtime,
        made.proposal,
        SwitchAnswer::Withdrawn("a message came in".into()),
        SwitchCtx::none(),
    )
    .await;
    assert!(
        matches!(again, Err(RuntimeError::NoOpenProposal(_))),
        "{again:?}"
    );
    assert_eq!(log_events(&runtime).len(), before.len(), "nothing written");

    let stranger = Ulid::generate();
    let unknown = settle(&mut runtime, stranger, SwitchAnswer::No, SwitchCtx::none()).await;
    assert!(
        matches!(unknown, Err(RuntimeError::NoOpenProposal(_))),
        "{unknown:?}"
    );
    assert_eq!(log_events(&runtime).len(), before.len(), "nothing written");

    let fold = decision_records(&log_events(&runtime));
    assert_eq!(fold.records.len(), 1);
    assert_eq!(fold.orphans, 0, "no second answer to count");
}

/// T2g: through `apply_switch`, the two withdrawn texts stay apart — a
/// failed switch spells the note out, a `yes` with no context is the bare
/// note.
#[tokio::test]
async fn t2g_apply_switch_keeps_the_two_withdrawn_texts_apart() {
    // A `yes` with no context, inside a turn.
    let dir = tempfile::tempdir().unwrap();
    let mut runtime = turn_rig(
        vec![proposing("c1", THERE, REASON), speaks("ok")],
        dir.path(),
    );
    let decisions = Arc::new(Decisions::new());
    runtime = runtime.with_decisions(decisions.clone());
    let answer = answerer(decisions.clone(), SwitchAnswer::Yes, || None);
    let (outcome, _) = tokio::join!(quiet_turn(&mut runtime), answer);
    let outcome = outcome.expect("the turn finished");
    assert_ne!(
        outcome.reason,
        aigentic_runtime::ASKED_HUMAN,
        "a refused yes continues the turn in place"
    );
    let note = switch_failed(NO_CONTEXT);
    assert_eq!(
        result_of(&log_events(&runtime), "c1").content,
        note,
        "the bare note, as #7 wrote it"
    );

    // A `yes` the switch itself refuses: a step thread.
    let step_dir = tempfile::tempdir().unwrap();
    let mut step = project_rig(
        vec![proposing("c1", THERE, REASON), speaks("ok")],
        step_dir.path(),
    );
    let decisions = Arc::new(Decisions::new());
    step = step.with_decisions(decisions.clone());
    let root = step_dir.path().to_path_buf();
    let answer = answerer(decisions.clone(), SwitchAnswer::Yes, move || {
        Some(target_context(THERE, root.clone()))
    });
    let (outcome, _) = tokio::join!(quiet_turn(&mut step), answer);
    let outcome = outcome.expect("the turn finished");
    assert_ne!(
        outcome.reason,
        aigentic_runtime::ASKED_HUMAN,
        "a refused yes continues the turn in place"
    );
    let events = log_events(&step);
    let result = result_of(&events, "c1");
    assert!(result.is_error, "a refused yes is an error result");
    let answered = answers(&events);
    assert_eq!(answered.len(), 1);
    assert_eq!(answered[0].0, Author::System);
    assert_eq!(answered[0].1.answer, DecisionAnswer::Withdrawn);
    let failed = answered[0].1.note.clone().expect("a note");
    assert_eq!(
        result.content,
        not_answered_text(&failed),
        "the note spelled out, as #7 wrote it"
    );
    assert_ne!(failed, note, "the two notes are not the same note");
}

// ------------------------------------------------------------- T4

/// T4a: a restart withdraws an idle proposal once, with today's note.
#[tokio::test]
async fn t4a_a_restart_withdraws_an_idle_proposal_once() {
    let dir = tempfile::tempdir().unwrap();
    let (mut runtime, thread) = rig(dir.path());
    let made = propose(&mut runtime).await;
    drop(runtime);

    let reopened = ThreadLog::open(dir.path(), thread).unwrap();
    let mut again = over(reopened, dir.path());
    assert_eq!(again.resume(None, &mut |_| {}).unwrap(), Resumed::Clean);
    let events = log_events(&again);
    let answered = answers(&events);
    assert_eq!(answered.len(), 1, "one answer");
    assert_eq!(answered[0].0, Author::System);
    assert_eq!(answered[0].1.answer, DecisionAnswer::Withdrawn);
    assert_eq!(answered[0].1.note.as_deref(), Some(DAEMON_RESTARTED));
    let parent = events
        .iter()
        .find(|e| e.kind == EventKind::DecisionAnswered)
        .unwrap()
        .parent_event;
    assert_eq!(parent, Some(made.proposal), "it answers the idle proposal");

    // A second resume writes nothing: the answered proposal isn't open.
    let before = log_events(&again).len();
    assert_eq!(again.resume(None, &mut |_| {}).unwrap(), Resumed::Clean);
    assert_eq!(log_events(&again).len(), before, "no second answer");
}

/// T4b: a proposal answered before the restart is untouched by it.
#[tokio::test]
async fn t4b_an_answered_idle_proposal_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let (mut runtime, thread) = rig(dir.path());
    let made = propose(&mut runtime).await;
    settle(
        &mut runtime,
        made.proposal,
        SwitchAnswer::No,
        SwitchCtx::none(),
    )
    .await
    .expect("settled");
    drop(runtime);

    let reopened = ThreadLog::open(dir.path(), thread).unwrap();
    let mut again = over(reopened, dir.path());
    assert_eq!(again.resume(None, &mut |_| {}).unwrap(), Resumed::Clean);
    let answered = answers(&log_events(&again));
    assert_eq!(answered.len(), 1, "still one answer");
    assert_eq!(answered[0].0, steve());
    assert_eq!(answered[0].1.answer, DecisionAnswer::No);
    assert_eq!(answered[0].1.note, None, "not relabelled");
}

/// T4c: an open turn **and** an idle proposal in its tail are each
/// withdrawn exactly once, and an unanswered proposal from an earlier turn
/// is not this restart's to relabel.
#[tokio::test]
async fn t4c_an_open_turn_and_an_idle_proposal_are_each_withdrawn_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut log = hand_log(dir.path());
    let thread = log.thread_id();
    // A turn that ended, carrying a proposal nobody answered.
    append(
        &mut log,
        EventKind::UserMessage,
        steve(),
        serde_json::to_value(aigentic_log::UserMessagePayload::new(vec![
            aigentic_core::ContentBlock::Text("go".into()),
        ]))
        .unwrap(),
    );
    let earlier = append_proposal(&mut log, "c1", HERE);
    append(
        &mut log,
        EventKind::TurnEnded,
        Author::Agent(AgentId("worker".into())),
        serde_json::to_value(aigentic_log::TurnEndedPayload::new("stop")).unwrap(),
    );
    // A second turn, still open, with an idle proposal in its tail.
    append(
        &mut log,
        EventKind::UserMessage,
        steve(),
        serde_json::to_value(aigentic_log::UserMessagePayload::new(vec![
            aigentic_core::ContentBlock::Text("and again".into()),
        ]))
        .unwrap(),
    );
    let idle = append_proposal(&mut log, &format!("{STARTUP_PREFIX}hand"), THERE);
    drop(log);

    let reopened = ThreadLog::open(dir.path(), thread).unwrap();
    let mut runtime = over(reopened, dir.path());
    let resumed = runtime.resume(None, &mut |_| {}).unwrap();
    assert!(
        matches!(resumed, Resumed::Interrupted { .. }),
        "{resumed:?}"
    );

    let events = log_events(&runtime);
    let answered = answers(&events);
    assert_eq!(answered.len(), 1, "one withdrawal: {answered:?}");
    assert_eq!(answered[0].1.answer, DecisionAnswer::Withdrawn);
    assert_eq!(answered[0].1.note.as_deref(), Some(DAEMON_RESTARTED));
    let answered_parent = events
        .iter()
        .find(|e| e.kind == EventKind::DecisionAnswered)
        .unwrap()
        .parent_event;
    assert_eq!(answered_parent, Some(idle), "the idle proposal, not `c1`");
    assert_eq!(
        events
            .iter()
            .filter(|e| e.kind == EventKind::DecisionAnswered && e.parent_event == Some(earlier))
            .count(),
        0,
        "the earlier turn's proposal keeps its own history"
    );

    let before = log_events(&runtime).len();
    runtime.resume(None, &mut |_| {}).unwrap();
    assert_eq!(log_events(&runtime).len(), before, "nothing more written");
}

// ------------------------------------------------------------- T5

/// T5a: the fold marks a start-up proposal, and not a model's.
#[tokio::test]
async fn t5a_the_fold_marks_a_startup_proposal() {
    // The runtime's own idle proposal, written by `propose_switch_idle`.
    let dir = tempfile::tempdir().unwrap();
    let (mut runtime, _) = rig(dir.path());
    propose(&mut runtime).await;
    let fold = decision_records(&log_events(&runtime));
    assert_eq!(fold.records.len(), 1);
    assert!(
        fold.records[0].startup,
        "the idle proposal is a start-up one"
    );

    // A model's proposal, whose call id is a tool call's.
    let model_dir = tempfile::tempdir().unwrap();
    let mut log = hand_log(model_dir.path());
    append_proposal(&mut log, "c1", HERE);
    let fold = decision_records(&log.read_all().unwrap());
    assert_eq!(fold.records.len(), 1);
    assert!(!fold.records[0].startup, "the model's is not");
}

/// T5b: `declined_at_startup` reads a start-up `no` or correction, for
/// that target, since the last switch.
#[tokio::test]
async fn t5b_declined_at_startup_reads_a_no_or_a_correction_for_that_target() {
    for answer in [DecisionAnswer::No, DecisionAnswer::Corrected] {
        let dir = tempfile::tempdir().unwrap();
        let mut log = hand_log(dir.path());
        // No `project_switched` at all: the first start of a front thread.
        let proposal = append_proposal(&mut log, &format!("{STARTUP_PREFIX}1"), THERE);
        append_answer(&mut log, proposal, answer, Some("elsewhere"), steve());
        let events = log.read_all().unwrap();
        assert!(
            !events.iter().any(|e| e.kind == EventKind::ProjectSwitched),
            "the case under test has no switch"
        );
        assert!(declined_at_startup(&events, THERE), "{answer:?}");
        assert!(!declined_at_startup(&events, HERE), "another target");
    }
}

/// T5c: a `yes`, a model's `no`, another target, and a switch after the
/// decline all read as "not declined".
#[tokio::test]
async fn t5c_declined_at_startup_is_false_for_a_yes_a_model_and_a_later_switch() {
    // A `yes`.
    let dir = tempfile::tempdir().unwrap();
    let mut log = hand_log(dir.path());
    let proposal = append_proposal(&mut log, &format!("{STARTUP_PREFIX}1"), THERE);
    append_answer(&mut log, proposal, DecisionAnswer::Yes, None, steve());
    assert!(!declined_at_startup(&log.read_all().unwrap(), THERE));

    // A model's own `no` for the same target.
    let dir = tempfile::tempdir().unwrap();
    let mut log = hand_log(dir.path());
    let proposal = append_proposal(&mut log, "c1", THERE);
    append_answer(&mut log, proposal, DecisionAnswer::No, None, steve());
    assert!(!declined_at_startup(&log.read_all().unwrap(), THERE));

    // A start-up `no`, and then the thread switches: the memory clears.
    let dir = tempfile::tempdir().unwrap();
    let mut log = hand_log(dir.path());
    let proposal = append_proposal(&mut log, &format!("{STARTUP_PREFIX}1"), THERE);
    append_answer(&mut log, proposal, DecisionAnswer::No, None, steve());
    assert!(declined_at_startup(&log.read_all().unwrap(), THERE));
    append_switch(&mut log, HERE);
    assert!(
        !declined_at_startup(&log.read_all().unwrap(), THERE),
        "a switch clears it"
    );
}
