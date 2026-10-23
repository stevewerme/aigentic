//! What a background job's own thread may do: it asks nobody, it stays in
//! its project, it never pushes, and a read-only job changes nothing.
//!
//! The interesting half of each test is the same call in a plain thread, so
//! the refusal is the job's rule and not a change to the policy table.

mod common;

use std::path::Path;
use std::sync::{Arc, Mutex};

use aigentic_core::{AgentId, ContentBlock, EventKind, ProviderEvent, RiskClass, ToolCall};
use aigentic_log::{PolicyRecord, ThreadLog, ToolResultPayload};
use aigentic_policy::{Decision, Policy, Rule};
use aigentic_runtime::harness_tools::{
    ASK_HUMAN, JOB_ASKS_NOBODY, JOB_STARTS_NO_JOB, JOB_STAYS_PUT, RECALL, START_JOB,
    SUGGEST_PROJECT,
};
use aigentic_runtime::{
    JOB_DENY, JOB_RULE_ASK, JOB_RULE_BOUNDARY, JOB_RULE_READ_ONLY, JobSpec, Mode, Runtime, Verdict,
};
use aigentic_tools::ToolRegistry;
use common::{done, scripted, steve};
use serde_json::json;
use ulid::Ulid;

const TASK: &str = "close #77";
const BRIEF: &str = "#77 is ready: build it, gate it, push";

/// A runtime over its own log, in `mode`, with a write-class rule the
/// project allows outright when `allow_write`, and a policy rooted in a
/// directory of its own so the boundary is real.
fn rig(script: Vec<Vec<ProviderEvent>>, dir: &Path, mode: Mode, allow_write: bool) -> Runtime {
    let log = ThreadLog::open(dir, Ulid::generate()).unwrap();
    let (provider, _seen) = scripted(script);
    let mut registry = ToolRegistry::builtin(
        aigentic_tools::Workdir::new(dir),
        aigentic_tools::DEFAULT_TIMEOUT,
    );
    let echo = common::EchoTool(Arc::new(Mutex::new(vec![])));
    registry.register(Box::new(echo)).unwrap();
    let mut policy = Policy::defaults().with_root(dir, dir);
    if allow_write {
        policy.rules.insert(
            0,
            Rule::class(
                RiskClass::Write,
                Decision::Allow,
                "this project writes freely",
            ),
        );
    }
    let mut runtime = Runtime::new(provider, registry, log, AgentId("worker".into()))
        .with_policy(policy)
        .with_projects(
            Some("Projects in reach. aigentic (here) ~/h".into()),
            Vec::new(),
        )
        .with_layers(aigentic_runtime::Layers::default());
    runtime.set_mode(mode);
    runtime
}

fn as_job(runtime: Runtime, read_only: bool, mode: Mode) -> Runtime {
    runtime
        .with_job(JobSpec {
            front: Ulid::generate(),
            read_only,
            mode,
        })
        .expect("the loop's deny list parses")
}

fn call(name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        id: "c1".into(),
        name: name.into(),
        args,
    }
}

fn kinds(runtime: &Runtime) -> Vec<EventKind> {
    runtime
        .log()
        .read_all()
        .unwrap()
        .iter()
        .map(|e| e.kind)
        .collect()
}

async fn decide(
    runtime: &mut Runtime,
    call: &ToolCall,
    class: RiskClass,
) -> aigentic_runtime::Verdict {
    let cancel = aigentic_runtime::CancelToken::never();
    runtime
        .policy_check(call, class, &cancel, &mut |_| {})
        .await
        .unwrap()
}

fn rule_of(verdict: &Verdict) -> &str {
    match verdict.record() {
        PolicyRecord::Rule { rule, .. } => rule,
        other => panic!("a rule record: {other:?}"),
    }
}

/// The rule a record names, or `None` when a person decided instead.
fn rule_name(verdict: &Verdict) -> Option<&str> {
    match verdict.record() {
        PolicyRecord::Rule { rule, .. } => Some(rule),
        PolicyRecord::Human { .. } => None,
    }
}

/// A job has nobody to ask, so what the rules would ask about is refused
/// rather than parked in a request no one will ever answer.
#[tokio::test]
async fn a_jobs_ask_is_refused_and_nothing_is_left_waiting() {
    let dir = tempfile::tempdir().unwrap();
    let mut job = as_job(
        rig(vec![], dir.path(), Mode::Manual, false),
        false,
        Mode::Manual,
    );
    let before = kinds(&job);
    let verdict = decide(
        &mut job,
        &call("write_file", json!({"path": "notes.md"})),
        RiskClass::Write,
    )
    .await;
    assert!(matches!(verdict, Verdict::Refuse(_)), "{verdict:?}");
    assert_eq!(rule_of(&verdict), JOB_RULE_ASK);
    let after = kinds(&job);
    assert_eq!(after, before, "a job writes no request: {after:?}");

    // The same call in a plain thread in `manual` does park a request: the
    // job's refusal is its own rule, not a change to the seam.
    let mut plain = rig(vec![], dir.path(), Mode::Manual, false);
    let plain_before = kinds(&plain);
    let _ = decide(
        &mut plain,
        &call("write_file", json!({"path": "notes.md"})),
        RiskClass::Write,
    )
    .await;
    let mut plain_after = kinds(&plain);
    plain_after.drain(..plain_before.len());
    assert_eq!(
        plain_after,
        vec![EventKind::PermissionRequested, EventKind::PermissionDecided],
        "an ordinary ask is a request and its answer: {plain_after:?}"
    );
}

/// A read-only job changes nothing, whatever mode the front was in, and it
/// runs no shell command the rules would ask about.
#[tokio::test]
async fn a_read_only_job_writes_nothing_and_runs_no_shell_write() {
    let dir = tempfile::tempdir().unwrap();
    let write = call("write_file", json!({"path": "notes.md"}));
    // What the rules would ask about, and what a project rule allows
    // outright: a read-only job refuses both.
    for allow_write in [false, true] {
        let mut job = as_job(
            rig(vec![], dir.path(), Mode::Auto, allow_write),
            true,
            Mode::Auto,
        );
        let verdict = decide(&mut job, &write, RiskClass::Write).await;
        assert!(matches!(verdict, Verdict::Refuse(_)), "{verdict:?}");
        assert_eq!(rule_of(&verdict), JOB_RULE_READ_ONLY);
    }
    let mut job = as_job(rig(vec![], dir.path(), Mode::Auto, false), true, Mode::Auto);

    // The same call in a job that may write runs under the mode's rule, so
    // the flag refused it and not the mode.
    let mut writing = as_job(
        rig(vec![], dir.path(), Mode::Auto, false),
        false,
        Mode::Auto,
    );
    let verdict = decide(&mut writing, &write, RiskClass::Write).await;
    assert!(matches!(verdict, Verdict::Run(_)), "{verdict:?}");
    assert_eq!(rule_of(&verdict), Mode::Auto.rule_name());

    // A shell write the rules would ask about: refused in a read-only job
    // even in `auto`, run by the mode in one that may write.
    let shell = call("bash", json!({"command": "touch made.txt"}));
    let verdict = decide(&mut job, &shell, RiskClass::Exec).await;
    assert!(matches!(verdict, Verdict::Refuse(_)), "{verdict:?}");
    assert_eq!(rule_of(&verdict), JOB_RULE_ASK);
    let verdict = decide(&mut writing, &shell, RiskClass::Exec).await;
    assert!(matches!(verdict, Verdict::Run(_)), "{verdict:?}");
    assert_eq!(rule_of(&verdict), Mode::Auto.rule_name());

    // A read still runs.
    let read = call("read_file", json!({"path": "notes.md"}));
    let verdict = decide(&mut job, &read, RiskClass::Read).await;
    assert!(matches!(verdict, Verdict::Run(_)), "{verdict:?}");
}

/// A job runs in the project it was approved for: a path outside is refused
/// outright, where a plain thread's outside path is a person's to answer.
#[tokio::test]
async fn a_job_stays_inside_its_project() {
    let dir = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let outside = call(
        "read_file",
        json!({"path": elsewhere.path().join("notes.md").display().to_string()}),
    );

    let mut job = as_job(rig(vec![], dir.path(), Mode::Auto, true), false, Mode::Auto);
    let verdict = decide(&mut job, &outside, RiskClass::Read).await;
    assert!(matches!(verdict, Verdict::Refuse(_)), "{verdict:?}");
    assert_eq!(rule_of(&verdict), JOB_RULE_BOUNDARY);

    let mut plain = rig(vec![], dir.path(), Mode::Auto, true);
    let verdict = decide(&mut plain, &outside, RiskClass::Read).await;
    assert!(
        !matches!(verdict, Verdict::Run(_)),
        "an ordinary thread's outside path is not run by the mode: {verdict:?}"
    );
    assert_ne!(rule_name(&verdict), Some(JOB_RULE_BOUNDARY));
}

/// A job's model is invited to none of the three tools that need a person,
/// and a call it makes anyway is refused with a reason it can act on.
#[tokio::test]
async fn a_job_is_offered_no_tool_that_starts_asks_or_moves_and_is_told_why() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = as_job(rig(vec![], dir.path(), Mode::Auto, true), false, Mode::Auto);
    let specs = runtime.tool_specs();
    for name in [START_JOB, SUGGEST_PROJECT, ASK_HUMAN] {
        assert!(
            !specs.iter().any(|s| s.name == name),
            "{name} is not offered to a job"
        );
    }
    assert!(specs.iter().any(|s| s.name == RECALL), "the rest stays");

    // Each of the three, called anyway: an error result carrying its own
    // reason, and nothing else written.
    let asks = [
        (
            call(ASK_HUMAN, json!({"question": "which branch?"})),
            JOB_ASKS_NOBODY,
        ),
        (
            call(START_JOB, json!({"task": TASK, "brief": BRIEF})),
            JOB_STARTS_NO_JOB,
        ),
        (
            call(
                SUGGEST_PROJECT,
                json!({"project": "aigentic", "reason": "it is there"}),
            ),
            JOB_STAYS_PUT,
        ),
    ];
    for (ask, reason) in asks {
        let script = vec![
            vec![ProviderEvent::ToolCall(ask.clone()), done("tool_use")],
            vec![
                ProviderEvent::TextDelta("nothing else to do".into()),
                done("stop"),
            ],
        ];
        let mut runtime = as_job(rig(script, dir.path(), Mode::Auto, true), false, Mode::Auto);
        let before = kinds(&runtime);
        quiet_turn(&mut runtime).await;
        let mut after = kinds(&runtime);
        after.drain(..before.len());
        // The turn's own events, and the refusal's one result: no
        // proposal, no request, no question.
        assert_eq!(
            after,
            vec![
                EventKind::UserMessage,
                EventKind::AssistantMessage,
                EventKind::ToolResult,
                EventKind::AssistantMessage,
                EventKind::TurnEnded,
            ],
            "{} is refused and the turn goes on",
            ask.name
        );
        let result = result_of(&runtime);
        assert!(result.is_error, "{}", ask.name);
        assert_eq!(result.content, reason);
    }
}

/// The loop's deny list binds a job in every mode, and a plain thread is not
/// bound by it: no mode, rule or grant is enough to talk past it.
#[tokio::test]
async fn the_loops_deny_list_binds_a_job_in_every_mode() {
    let dir = tempfile::tempdir().unwrap();
    for entry in JOB_DENY.iter().filter(|e| e.starts_with("git ")) {
        let mut job = as_job(rig(vec![], dir.path(), Mode::Auto, true), false, Mode::Auto);
        let verdict = decide(
            &mut job,
            &call("bash", json!({"command": entry})),
            RiskClass::Exec,
        )
        .await;
        assert!(
            matches!(verdict, Verdict::Refuse(_)),
            "{entry}: {verdict:?}"
        );
    }

    // `git push` in a plain thread in `auto` is the mode's to run.
    let mut plain = rig(vec![], dir.path(), Mode::Auto, true);
    let verdict = decide(
        &mut plain,
        &call("bash", json!({"command": "git push"})),
        RiskClass::Exec,
    )
    .await;
    assert!(!matches!(verdict, Verdict::Refuse(_)), "{verdict:?}");

    // The `.env` entry: refused in a job, not refused by an ordinary
    // thread's own verdict for the same command.
    let env = call("bash", json!({"command": "cp .env /tmp/x"}));
    let mut job = as_job(rig(vec![], dir.path(), Mode::Auto, true), false, Mode::Auto);
    let verdict = decide(&mut job, &env, RiskClass::Exec).await;
    assert!(matches!(verdict, Verdict::Refuse(_)), "{verdict:?}");
    let mut plain = rig(vec![], dir.path(), Mode::Auto, true);
    let verdict = decide(&mut plain, &env, RiskClass::Exec).await;
    assert!(!matches!(verdict, Verdict::Refuse(_)), "{verdict:?}");
}

fn result_of(runtime: &Runtime) -> aigentic_core::ToolResult {
    let event = runtime
        .log()
        .read_all()
        .unwrap()
        .into_iter()
        .rev()
        .find(|e| e.kind == EventKind::ToolResult)
        .expect("the call has a result");
    serde_json::from_value::<ToolResultPayload>(event.payload)
        .expect("a tool result payload")
        .result
}

/// The worker's turn: a `go` from the person, and whatever the script says.
async fn quiet_turn(runtime: &mut Runtime) {
    runtime
        .run_turn(steve(), vec![ContentBlock::Text("go".into())], &mut |_| {})
        .await
        .expect("the turn finishes");
}
