//! `finish_step`, the step deny overlay and the step context, in the loop
//! (issue #55). Sibling of `policy.rs`: the same rig, scripted turns.

mod common;

use std::sync::{Arc, Mutex};

use aigentic_core::{
    AgentId, Author, BoxFuture, ContentBlock, EventKind, ProviderEvent, RiskClass, Tool, ToolCall,
    ToolError, ToolOutput, UserId,
};
use aigentic_log::{
    CommitRef, Handoff, PermissionRequestedPayload, PolicyRecord, ReportStatus, StepReport,
    ThreadLog, ToolResultPayload,
};
use aigentic_policy::{Decision, Policy, Rule};
use aigentic_runtime::{Answer, Approver, ProjectContext, Runtime, RuntimeError, STEP_REPORTED};
use aigentic_tools::ToolRegistry;
use common::{done, scripted};
use serde_json::json;

/// The step every rig here runs (the runtime's `with_step` name).
const STEP: &str = "implement";

/// A `bash` that records its commands instead of running them, so a test
/// can see whether a denied command was executed at all.
struct RecordingBash(Arc<Mutex<Vec<String>>>);

impl Tool for RecordingBash {
    fn name(&self) -> &str {
        "bash"
    }
    fn description(&self) -> &str {
        "bash"
    }
    fn schema(&self) -> schemars::schema::RootSchema {
        schemars::schema_for!(String)
    }
    fn risk_class(&self) -> RiskClass {
        RiskClass::Exec
    }
    fn call(&self, args: serde_json::Value) -> BoxFuture<'_, Result<ToolOutput, ToolError>> {
        let command = args["command"].as_str().unwrap_or("?").to_owned();
        self.0.lock().unwrap().push(command.clone());
        Box::pin(async move {
            Ok(ToolOutput {
                content: format!("ran: {command}"),
                is_error: false,
            })
        })
    }
}

/// Allows every prompt, silently.
struct Yes;

impl Approver for Yes {
    fn author(&self) -> Author {
        steve()
    }
    fn ask(&mut self, _: &PermissionRequestedPayload) -> Answer {
        Answer::Allow
    }
    fn ask_human(&mut self, _: &str) -> Option<String> {
        None
    }
}

fn steve() -> Author {
    Author::User(UserId("steve".into()))
}

fn bash(id: &str, command: &str) -> ProviderEvent {
    ProviderEvent::ToolCall(ToolCall {
        id: id.into(),
        name: "bash".into(),
        args: json!({"command": command}),
    })
}

fn finish(id: &str, args: serde_json::Value) -> ProviderEvent {
    ProviderEvent::ToolCall(ToolCall {
        id: id.into(),
        name: "finish_step".into(),
        args,
    })
}

fn text(t: &str) -> ProviderEvent {
    ProviderEvent::TextDelta(t.into())
}

struct Rig {
    runtime: Runtime,
    ran: Arc<Mutex<Vec<String>>>,
    dir: Arc<tempfile::TempDir>,
}

/// A runtime over a recording `bash`. `step` is the deny list when the
/// thread is a step thread, `None` for a plain one.
fn rig(script: Vec<Vec<ProviderEvent>>, step: Option<&[&str]>) -> Rig {
    let dir = Arc::new(tempfile::tempdir().unwrap());
    let log = ThreadLog::open(dir.path(), ulid::Ulid::generate()).unwrap();
    step_rig(script, step, log, dir)
}

/// A runtime resumed over a log the test already wrote to.
fn step_rig(
    script: Vec<Vec<ProviderEvent>>,
    step: Option<&[&str]>,
    log: ThreadLog,
    dir: Arc<tempfile::TempDir>,
) -> Rig {
    let (provider, _seen) = scripted(script);
    let ran = Arc::new(Mutex::new(Vec::new()));
    let registry: ToolRegistry = vec![Box::new(RecordingBash(ran.clone())) as Box<dyn Tool>].into();
    let mut runtime = Runtime::new(provider, registry, log, AgentId("implementer".into()))
        .with_policy(Policy::defaults())
        .with_approver(Box::new(Yes));
    if let Some(deny) = step {
        let deny: Vec<String> = deny.iter().map(|d| (*d).to_owned()).collect();
        runtime = runtime.with_step(STEP, &deny).expect("a known deny list");
    }
    Rig { runtime, ran, dir }
}

fn events(r: &Rig) -> Vec<aigentic_core::Event> {
    r.runtime.log().read_all().unwrap()
}

fn kinds(r: &Rig) -> Vec<EventKind> {
    events(r).iter().map(|e| e.kind).collect()
}

fn count(r: &Rig, kind: EventKind) -> usize {
    kinds(r).iter().filter(|k| **k == kind).count()
}

/// Every `tool_result` payload, in the order the calls produced them.
fn results(r: &Rig) -> Vec<ToolResultPayload> {
    events(r)
        .iter()
        .filter(|e| e.kind == EventKind::ToolResult)
        .map(|e| serde_json::from_value(e.payload.clone()).unwrap())
        .collect()
}

/// The step's report, the one `step_reported` payload.
fn report(r: &Rig, nth: usize) -> StepReport {
    let events = events(r);
    let payload = events
        .iter()
        .filter(|e| e.kind == EventKind::StepReported)
        .nth(nth)
        .unwrap_or_else(|| panic!("no report {nth}"));
    serde_json::from_value(payload.payload.clone()).unwrap()
}

/// The last `turn_ended` event's reason.
fn end_reason(r: &Rig) -> String {
    let events = events(r);
    let last = events
        .iter()
        .rev()
        .find(|e| e.kind == EventKind::TurnEnded)
        .expect("a turn ended");
    last.payload["reason"]
        .as_str()
        .expect("a reason")
        .to_owned()
}

/// T9: `finish_step` is offered exactly in a step thread, and its spec
/// sits where the plain list's sort puts it.
#[tokio::test]
async fn t9_finish_step_is_offered_only_in_a_step_thread() {
    let plain = rig(vec![], None);
    let plain_names: Vec<String> = plain
        .runtime
        .tool_specs()
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert!(
        !plain_names.contains(&"finish_step".to_owned()),
        "{plain_names:?}"
    );

    let r = rig(vec![], Some(&["git push"]));
    let names: Vec<String> = r.runtime.tool_specs().into_iter().map(|s| s.name).collect();
    // The expected list is the plain one's, without `suggest_project`
    // (a proposal needs a person, so a step thread never offers it, #7),
    // plus the new name, sorted.
    let mut expected: Vec<String> = plain_names
        .iter()
        .filter(|n| n.as_str() != "suggest_project")
        .cloned()
        .collect();
    expected.push("finish_step".into());
    expected.sort();
    assert_eq!(names, expected);
    assert_eq!(r.runtime.step(), Some(STEP));
    assert_eq!(plain.runtime.step(), None);
}

/// T9b (## Plan amendment item 2): a plain thread refuses the call at the
/// arm — no event, no `step_reported` turn end — because a model can
/// always emit a tool it was not offered.
#[tokio::test]
async fn t9b_a_plain_thread_refuses_finish_step_with_no_event() {
    let mut r = rig(
        vec![
            vec![
                finish("c1", json!({"status": "done", "body": "x"})),
                done("tool_use"),
            ],
            vec![text("ok"), done("stop")],
        ],
        None,
    );
    r.runtime
        .run_turn(steve(), vec![ContentBlock::Text("go".into())], &mut |_| {})
        .await
        .unwrap();
    let first = &results(&r)[0];
    assert!(first.result.is_error, "{}", first.result.content);
    assert!(
        first
            .result
            .content
            .contains("only offered to a step thread"),
        "{}",
        first.result.content
    );
    assert_eq!(count(&r, EventKind::StepReported), 0);
    assert_eq!(end_reason(&r), "done");
}

/// T10: one report, one `step_reported` event, the §4 payload the call
/// carried, written before the call's result.
#[tokio::test]
async fn t10_a_report_writes_one_step_reported_event() {
    let mut r = rig(
        vec![
            vec![
                finish(
                    "c1",
                    json!({
                        "status": "done",
                        "body": "wrote the thing",
                        "commits": [{"sha": "abc1234", "subject": "runtime: do the thing"}],
                    }),
                ),
                done("tool_use"),
            ],
            vec![text("ok"), done("stop")],
        ],
        Some(&["git push"]),
    );
    r.runtime
        .run_turn(steve(), vec![ContentBlock::Text("go".into())], &mut |_| {})
        .await
        .unwrap();
    assert_eq!(count(&r, EventKind::StepReported), 1);
    let events = events(&r);
    let reported = events
        .iter()
        .find(|e| e.kind == EventKind::StepReported)
        .expect("the report");
    assert_eq!(
        reported.author,
        Author::Agent(AgentId("implementer".into()))
    );
    assert_eq!(
        report(&r, 0),
        StepReport {
            status: Some(ReportStatus::Done),
            body: Some("wrote the thing".into()),
            commits: Some(vec![CommitRef {
                sha: "abc1234".into(),
                subject: "runtime: do the thing".into(),
            }]),
            ..StepReport::default()
        }
    );
    assert!(
        reported.seq
            < events
                .iter()
                .find(|e| e.kind == EventKind::ToolResult)
                .unwrap()
                .seq,
        "the report is written before the call's result"
    );
    assert_eq!(
        results(&r)[0].result.content,
        format!("step reported for {STEP}; this ends the turn")
    );
    assert_eq!(end_reason(&r), STEP_REPORTED);
}

/// T11: other calls in the batch run first, and the turn ends on the
/// report — every result precedes `turn_ended`.
#[tokio::test]
async fn t11_other_calls_run_first_and_the_turn_ends_on_the_report() {
    let mut r = rig(
        vec![vec![
            ProviderEvent::ToolCall(ToolCall {
                id: "c1".into(),
                name: "pin".into(),
                args: json!({"text": "Use Swedish."}),
            }),
            finish("c2", json!({"status": "done", "body": "x"})),
            done("tool_use"),
        ]],
        Some(&["git push"]),
    );
    r.runtime
        .run_turn(steve(), vec![ContentBlock::Text("go".into())], &mut |_| {})
        .await
        .unwrap();
    let events = events(&r);
    let last = events.len() - 1;
    assert_eq!(events[last].kind, EventKind::TurnEnded);
    assert_eq!(events[last].payload["reason"], STEP_REPORTED);
    assert_eq!(count(&r, EventKind::Pinned), 1, "the pin ran first");
    assert_eq!(results(&r).len(), 2, "both calls got a result");
    assert_eq!(end_reason(&r), STEP_REPORTED);
}

/// T12: the three refusals are error results with no event and no report
/// turn end; the corrected call after them lands.
#[tokio::test]
async fn t12_a_bad_call_is_an_error_result_with_no_event() {
    let handoff = json!({"done": "the loop", "next": "the tests", "dirty": ["a.rs"]});
    let mut r = rig(
        vec![
            vec![
                finish("c1", json!({"status": "done", "bogus": 1})),
                finish("c2", json!({"body": "no status"})),
                finish("c3", json!({"status": "partial", "body": "mid"})),
                done("tool_use"),
            ],
            vec![
                finish(
                    "c4",
                    json!({"status": "partial", "body": "mid", "handoff": handoff}),
                ),
                done("tool_use"),
            ],
        ],
        Some(&["git push"]),
    );
    r.runtime
        .run_turn(steve(), vec![ContentBlock::Text("go".into())], &mut |_| {})
        .await
        .unwrap();
    let results = results(&r);
    assert_eq!(results.len(), 4);
    assert!(results[0].result.is_error, "{}", results[0].result.content);
    assert!(
        results[0].result.content.contains("invalid arguments")
            && results[0].result.content.contains("bogus"),
        "the typo is named back: {}",
        results[0].result.content
    );
    assert_eq!(
        results[1].result.content,
        "status is required: done | partial (partial needs handoff)"
    );
    assert_eq!(
        results[2].result.content,
        "a partial report must carry handoff { done, next, dirty }"
    );
    assert!(!results[3].result.is_error, "the corrected call lands");
    // One report, from the corrected call, and the turn ends on it.
    assert_eq!(count(&r, EventKind::StepReported), 1);
    assert_eq!(
        report(&r, 0),
        StepReport {
            status: Some(ReportStatus::Partial),
            body: Some("mid".into()),
            handoff: Some(Handoff {
                done: "the loop".into(),
                next: "the tests".into(),
                dirty: vec!["a.rs".into()],
            }),
            ..StepReport::default()
        }
    );
    assert_eq!(end_reason(&r), STEP_REPORTED);
}

/// T13: one report per turn — the same batch's second call is refused
/// with the arm's text, and the turn still ends `step_reported`.
#[tokio::test]
async fn t13_one_report_per_turn() {
    let mut r = rig(
        vec![vec![
            finish("c1", json!({"status": "done", "body": "first"})),
            finish("c2", json!({"status": "done", "body": "second"})),
            done("tool_use"),
        ]],
        Some(&["git push"]),
    );
    r.runtime
        .run_turn(steve(), vec![ContentBlock::Text("go".into())], &mut |_| {})
        .await
        .unwrap();
    assert_eq!(count(&r, EventKind::StepReported), 1);
    let results = results(&r);
    assert_eq!(
        results[0].result.content,
        format!("step reported for {STEP}; this ends the turn")
    );
    assert_eq!(
        results[1].result.content,
        "finish_step already ran; the report was recorded"
    );
    assert_eq!(report(&r, 0).body.as_deref(), Some("first"));
    assert_eq!(end_reason(&r), STEP_REPORTED);
}

/// T15 (## Plan amendment 2 item 1): a send-back is a new user message, so
/// the same thread reports twice — once per turn.
#[tokio::test]
async fn t15_a_send_back_reports_again_in_the_same_thread() {
    let mut r = rig(
        vec![
            vec![
                finish("c1", json!({"status": "done", "body": "first"})),
                done("tool_use"),
            ],
            // The runner's send-back: a new turn in the same thread, with
            // the handoff the template asks for.
            vec![
                finish(
                    "c2",
                    json!({
                        "status": "partial",
                        "body": "second",
                        "handoff": {"done": "a", "next": "b", "dirty": []},
                    }),
                ),
                done("tool_use"),
            ],
        ],
        Some(&["git push"]),
    );
    r.runtime
        .run_turn(steve(), vec![ContentBlock::Text("go".into())], &mut |_| {})
        .await
        .unwrap();
    assert_eq!(end_reason(&r), STEP_REPORTED);
    assert_eq!(report(&r, 0).body.as_deref(), Some("first"));

    // The send-back reports again — same thread, new turn.
    let sent_back = r
        .runtime
        .run_turn(
            Author::User(UserId("runner".into())),
            vec![ContentBlock::Text("please fix the ledger".into())],
            &mut |_| {},
        )
        .await
        .unwrap();
    assert_eq!(sent_back.reason, STEP_REPORTED);
    assert_eq!(count(&r, EventKind::StepReported), 2);
    let second = report(&r, 1);
    assert_eq!(second.body.as_deref(), Some("second"));
    assert_eq!(
        second.status,
        Some(ReportStatus::Partial),
        "the send-back's report is the new turn's"
    );
}

/// T16 (## Plan amendment 2 item 1): the rule is derived from the log, so
/// a resumed runtime refuses a report its current turn already holds — and
/// accepts one after a new user message.
#[tokio::test]
async fn t16_a_resumed_runtime_reads_the_rule_from_the_log() {
    let dir = Arc::new(tempfile::tempdir().unwrap());
    let thread = ulid::Ulid::generate();
    let log = ThreadLog::open(dir.path(), thread).unwrap();
    // The turn the daemon died in: it holds a report.
    let mut first = step_rig(
        vec![vec![
            finish(
                "c1",
                json!({"status": "done", "body": "before the restart"}),
            ),
            done("tool_use"),
        ]],
        Some(&["git push"]),
        log,
        dir,
    );
    first
        .runtime
        .run_turn(steve(), vec![ContentBlock::Text("go".into())], &mut |_| {})
        .await
        .unwrap();
    assert_eq!(count(&first, EventKind::StepReported), 1);
    let dir = first.dir.clone();
    drop(first);

    // A rebuilt runtime, same log, same turn continued.
    let log = ThreadLog::open(dir.path(), thread).unwrap();
    let mut resumed = step_rig(
        vec![
            vec![
                finish("c2", json!({"status": "done", "body": "a second report"})),
                done("tool_use"),
            ],
            vec![text("ok"), done("stop")],
            vec![
                finish(
                    "c3",
                    json!({"status": "done", "body": "after the send-back"}),
                ),
                done("tool_use"),
            ],
        ],
        Some(&["git push"]),
        log,
        dir,
    );
    // The current turn already reported: the call is refused.
    let continued = resumed.runtime.continue_turn(&mut |_| {}).await.unwrap();
    assert_eq!(continued.reason, "done");
    assert_eq!(
        results(&resumed).last().unwrap().result.content,
        "finish_step already ran; the report was recorded"
    );
    assert_eq!(count(&resumed, EventKind::StepReported), 1);

    // A user message opens a new turn: the call lands.
    let again = resumed
        .runtime
        .run_turn(
            Author::User(UserId("runner".into())),
            vec![ContentBlock::Text("one more thing".into())],
            &mut |_| {},
        )
        .await
        .unwrap();
    assert_eq!(again.reason, STEP_REPORTED);
    assert_eq!(count(&resumed, EventKind::StepReported), 2);
    assert_eq!(
        report(&resumed, 1).body.as_deref(),
        Some("after the send-back")
    );
}

/// T8 through `policy_check` (## Plan amendment 2 item 2): a rule that
/// allows the call cannot rescue an entry the overlay denies. The overlay
/// is asked before `decide`, so `Policy`'s own verdict is never reached.
#[tokio::test]
async fn t8_an_allow_rule_cannot_rescue_an_entry() {
    let mut r = rig(
        vec![
            vec![bash("c1", "git push"), done("tool_use")],
            vec![text("ok"), done("stop")],
        ],
        Some(&["git push"]),
    );
    r.runtime = r.runtime.with_policy(Policy::configured(
        vec![Rule::tool("bash", Decision::Allow, "always")],
        None,
    ));
    r.runtime
        .run_turn(steve(), vec![ContentBlock::Text("go".into())], &mut |_| {})
        .await
        .unwrap();
    let first = &results(&r)[0];
    assert!(first.result.is_error, "{}", first.result.content);
    assert_eq!(
        first.policy,
        Some(PolicyRecord::rule_with_reason(
            "step deny git push",
            "deny",
            "the runner pushes once after its checks (PLAN-layer2 §6.4); commit and call finish_step",
        ))
    );
    assert!(r.ran.lock().unwrap().is_empty(), "the command never ran");
}

/// T14: the overlay holds under `Mode::Auto`, where nothing asks — and a
/// plain runtime in auto runs the same calls, which is the contrast.
#[tokio::test]
async fn t14_the_overlay_holds_under_auto() {
    let commands = ["git push", "cargo test && git push"];
    let mut step_thread = rig(
        vec![
            vec![
                bash("c1", commands[0]),
                bash("c2", commands[1]),
                done("tool_use"),
            ],
            vec![text("ok"), done("stop")],
        ],
        Some(&["git push"]),
    );
    step_thread.runtime.set_mode(aigentic_runtime::Mode::Auto);
    step_thread
        .runtime
        .run_turn(steve(), vec![ContentBlock::Text("go".into())], &mut |_| {})
        .await
        .unwrap();
    let results = results(&step_thread);
    assert_eq!(results.len(), 2);
    for (i, id) in ["c1", "c2"].into_iter().enumerate() {
        assert!(
            results[i].result.is_error,
            "{id}: {}",
            results[i].result.content
        );
        assert_eq!(
            results[i].policy,
            Some(PolicyRecord::rule_with_reason(
                "step deny git push",
                "deny",
                "the runner pushes once after its checks (PLAN-layer2 §6.4); commit and call finish_step",
            )),
            "{id}"
        );
    }
    assert!(
        step_thread.ran.lock().unwrap().is_empty(),
        "the recording bash never ran"
    );
    assert_eq!(
        count(&step_thread, EventKind::PermissionRequested),
        0,
        "auto asks nothing, and the deny does not ask either"
    );

    // The same calls, no step context, auto: they run.
    let mut plain = rig(
        vec![
            vec![
                bash("c1", commands[0]),
                bash("c2", commands[1]),
                done("tool_use"),
            ],
            vec![text("ok"), done("stop")],
        ],
        None,
    );
    plain.runtime.set_mode(aigentic_runtime::Mode::Auto);
    plain
        .runtime
        .run_turn(steve(), vec![ContentBlock::Text("go".into())], &mut |_| {})
        .await
        .unwrap();
    assert_eq!(
        *plain.ran.lock().unwrap(),
        commands,
        "the same commands run without a step context"
    );
}

/// T17 (## Plan amendment 2 item 2): `with_policy` replaces the policy
/// wholesale and never the overlay, which lives on the step context.
#[tokio::test]
async fn t17_with_policy_keeps_the_overlay() {
    let mut r = rig(
        vec![
            vec![bash("c1", "git push"), done("tool_use")],
            vec![text("ok"), done("stop")],
        ],
        Some(&["git push"]),
    );
    // The fresh policy replaces the old one wholesale; the overlay
    // the step context holds still decides first.
    r.runtime = r.runtime.with_policy(Policy::defaults());
    r.runtime
        .run_turn(steve(), vec![ContentBlock::Text("go".into())], &mut |_| {})
        .await
        .unwrap();
    let first = &results(&r)[0];
    assert!(first.result.is_error, "{}", first.result.content);
    assert_eq!(
        first.policy,
        Some(PolicyRecord::rule_with_reason(
            "step deny git push",
            "deny",
            "the runner pushes once after its checks (PLAN-layer2 §6.4); commit and call finish_step",
        ))
    );
    assert!(r.ran.lock().unwrap().is_empty());
}

/// T18 (## Plan amendment 2 item 2): a step thread keeps its context — a
/// project switch is refused rather than silently dropping the overlay.
#[tokio::test]
async fn t18_a_step_thread_refuses_a_project_switch() {
    let mut r = rig(vec![], Some(&["git push"]));
    let (provider, _seen) = scripted(vec![]);
    let other = ProjectContext {
        name: Some("other".into()),
        workspace: None,
        root: r.dir.path().to_path_buf(),
        layers: aigentic_runtime::Layers::default(),
        policy: Policy::defaults(),
        skills: aigentic_runtime::aigentic_skills::SkillSet::default(),
        registry: ToolRegistry::empty(),
        provider,
        model_label: "test".into(),
        projects: None,
        project_rows: Vec::new(),
        profile: None,
        effort: None,
        prices: None,
    };
    let error = r
        .runtime
        .set_project(other, steve(), &mut |_| {})
        .expect_err("a step thread refuses the move");
    assert!(
        matches!(&error, RuntimeError::StepThread(message) if message.contains(STEP)),
        "{error}"
    );
    assert!(
        !kinds(&r).contains(&EventKind::ProjectSwitched),
        "nothing was recorded and nothing moved"
    );
    assert_eq!(r.runtime.step(), Some(STEP));
}

/// ## Plan amendment 2 item 3: `finish_step` is the harness's contract in
/// a step thread, not a project tool — a project's allow list never hides
/// it there, while the tools it does list stay the only other ones.
#[tokio::test]
async fn t9c_finish_step_survives_a_project_allow_list() {
    // Offered: the allow list names `bash` alone, and the step thread
    // still gets `finish_step`.
    let offered = allowed(rig(vec![], Some(&["git push"])), &["bash"]);
    let names: Vec<String> = offered
        .runtime
        .tool_specs()
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert_eq!(names, vec!["bash".to_owned(), "finish_step".to_owned()]);
    assert!(
        !offered.runtime.tool_visible("pin"),
        "the allow list still hides the other harness tools"
    );

    // And run: the report lands and ends the turn.
    let mut ran = allowed(
        rig(
            vec![vec![
                finish("c1", json!({"status": "done", "body": "x"})),
                done("tool_use"),
            ]],
            Some(&["git push"]),
        ),
        &["bash"],
    );
    let outcome = ran
        .runtime
        .run_turn(steve(), vec![ContentBlock::Text("go".into())], &mut |_| {})
        .await
        .unwrap();
    assert_eq!(outcome.reason, STEP_REPORTED);
    assert_eq!(count(&ran, EventKind::StepReported), 1);
}

/// Give the runtime a project whose `[tools] allow` list is exactly
/// `allow`. `with_layers` keeps the step context, which is the point.
fn allowed(r: Rig, allow: &[&str]) -> Rig {
    use aigentic_runtime::project::{ProjectFile, ToolsSection};
    use aigentic_runtime::{Layers, Project};

    let project = Project {
        name: "p".into(),
        root: r.dir.path().to_path_buf(),
        file: ProjectFile {
            tools: ToolsSection {
                allow: allow.iter().map(|a| (*a).to_owned()).collect(),
                ..ToolsSection::default()
            },
            ..ProjectFile::default()
        },
        instructions: None,
        memory: vec![],
        unknown: vec![],
    };
    let runtime = r
        .runtime
        .with_layers(Layers::default().with_project(project));
    Rig {
        runtime,
        ran: r.ran,
        dir: r.dir,
    }
}

/// `## Plan amendment` item 4: when one assistant message carries both
/// `ask_human` and `finish_step`, the turn ends `step_reported`, not
/// `asked_human` — the report is in the log and the runner's table has
/// no "asked" to match it.
#[tokio::test]
async fn t19_a_report_wins_over_an_answer_in_one_batch() {
    /// Answers every question, so `answered` would be true too.
    struct AnsweringYes;
    impl Approver for AnsweringYes {
        fn author(&self) -> Author {
            steve()
        }
        fn ask(&mut self, _: &PermissionRequestedPayload) -> Answer {
            Answer::Allow
        }
        fn ask_human(&mut self, _: &str) -> Option<String> {
            Some("yes".into())
        }
    }

    let calls = vec![
        ProviderEvent::ToolCall(ToolCall {
            id: "c1".into(),
            name: "ask_human".into(),
            args: json!({"question": "go?"}),
        }),
        finish("c2", json!({"status": "done", "body": "x"})),
        done("tool_use"),
    ];
    let mut scripted_r = rig(vec![calls], Some(&["git push"]));
    scripted_r.runtime = scripted_r.runtime.with_approver(Box::new(AnsweringYes));
    let outcome = scripted_r
        .runtime
        .run_turn(steve(), vec![ContentBlock::Text("go".into())], &mut |_| {})
        .await
        .unwrap();
    assert_eq!(outcome.reason, STEP_REPORTED);
    assert_eq!(count(&scripted_r, EventKind::StepReported), 1);
    assert_eq!(
        end_reason(&scripted_r),
        STEP_REPORTED,
        "not asked_human: the report is the step's end"
    );
}

/// T7, runtime half: `with_step` propagates the parse error, names the
/// entry, and leaves the runtime without a step context.
#[tokio::test]
async fn t7_with_step_propagates_a_bad_entry() {
    let dir = Arc::new(tempfile::tempdir().unwrap());
    let (provider, _seen) = scripted(vec![]);
    let log = ThreadLog::open(dir.path(), ulid::Ulid::generate()).unwrap();
    let runtime = Runtime::new(
        provider,
        ToolRegistry::empty(),
        log,
        AgentId("implementer".into()),
    );
    let error = match runtime.with_step(STEP, &["git obliterate".to_owned()]) {
        Ok(_) => panic!("an unknown entry is refused"),
        Err(error) => error,
    };
    assert_eq!(error.entry, "git obliterate");
    assert!(error.to_string().contains("git obliterate"), "{error}");
}
