//! Policy in the loop: rule decisions on the result, human decisions as
//! events, session grants, refusals the model sees, and harness tools.

mod common;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use aigentic_core::{
    Author, BoxFuture, ContentBlock, EventKind, ProviderEvent, RiskClass, Tool, ToolCall,
    ToolError, ToolOutput, UserId,
};
use aigentic_log::{
    DecisionScope, PermissionDecidedPayload, PermissionRequestedPayload, PolicyRecord, ThreadLog,
    ToolResultPayload,
};
use aigentic_policy::{Decision, Policy, Rule};
use aigentic_runtime::{Answer, Approver, Runtime};
use aigentic_tools::ToolRegistry;
use common::{ScriptedProvider, done, scripted};
use serde_json::json;

/// A `write`-class tool that records its calls.
struct Touch(Arc<Mutex<Vec<String>>>);

impl Tool for Touch {
    fn name(&self) -> &str {
        "touch"
    }
    fn description(&self) -> &str {
        "touch"
    }
    fn schema(&self) -> schemars::schema::RootSchema {
        schemars::schema_for!(String)
    }
    fn risk_class(&self) -> RiskClass {
        RiskClass::Write
    }
    fn call(&self, args: serde_json::Value) -> BoxFuture<'_, Result<ToolOutput, ToolError>> {
        let path = args["path"].as_str().unwrap_or("?").to_owned();
        self.0.lock().unwrap().push(path.clone());
        Box::pin(async move {
            Ok(ToolOutput {
                content: format!("touched {path}"),
                is_error: false,
            })
        })
    }
}

/// Answers from a script and records what it was asked.
struct ScriptedApprover {
    answers: VecDeque<Answer>,
    asked: Arc<Mutex<Vec<PermissionRequestedPayload>>>,
    human_answer: Option<String>,
}

impl Approver for ScriptedApprover {
    fn author(&self) -> Author {
        steve()
    }
    fn ask(&mut self, request: &PermissionRequestedPayload) -> Answer {
        self.asked.lock().unwrap().push(request.clone());
        self.answers.pop_front().expect("approver script exhausted")
    }
    fn ask_human(&mut self, _question: &str) -> Option<String> {
        self.human_answer.clone()
    }
}

fn steve() -> Author {
    Author::User(UserId("steve".into()))
}

fn touch(id: &str, path: &str) -> ProviderEvent {
    ProviderEvent::ToolCall(ToolCall {
        id: id.into(),
        name: "touch".into(),
        args: json!({"path": path}),
    })
}

fn bash(id: &str, command: &str) -> ProviderEvent {
    ProviderEvent::ToolCall(ToolCall {
        id: id.into(),
        name: "bash".into(),
        args: json!({"command": command}),
    })
}

fn text(t: &str) -> ProviderEvent {
    ProviderEvent::TextDelta(t.into())
}

struct Rig {
    runtime: Runtime,
    touched: Arc<Mutex<Vec<String>>>,
    asked: Arc<Mutex<Vec<PermissionRequestedPayload>>>,
    _dir: tempfile::TempDir,
}

fn rig(script: Vec<Vec<ProviderEvent>>, answers: Vec<Answer>, policy: Policy) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let log = ThreadLog::open(dir.path(), ulid::Ulid::generate()).unwrap();
    let (provider, _seen) = scripted(script);
    let touched = Arc::new(Mutex::new(Vec::new()));
    let asked = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ToolRegistry::builtin(aigentic_tools::Workdir::new(dir.path()));
    registry.register(Box::new(Touch(touched.clone()))).unwrap();
    let runtime = Runtime::new(
        provider,
        registry,
        log,
        aigentic_core::AgentId("worker".into()),
    )
    .with_policy(policy)
    .with_approver(Box::new(ScriptedApprover {
        answers: answers.into(),
        asked: asked.clone(),
        human_answer: Some("yes, go ahead".into()),
    }));
    Rig {
        runtime,
        touched,
        asked,
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

fn result_at(rig: &Rig, seq: usize) -> ToolResultPayload {
    let events = rig.runtime.log().read_all().unwrap();
    assert_eq!(events[seq].kind, EventKind::ToolResult, "seq {seq}");
    serde_json::from_value(events[seq].payload.clone()).unwrap()
}

fn audit(rig: &Rig) {
    let events = rig.runtime.log().read_all().unwrap();
    let missing = aigentic_runtime::audit_tool_results(&events);
    assert!(missing.is_empty(), "no policy record at {missing:?}");
}

#[tokio::test]
async fn a_rule_allow_is_recorded_on_the_result_with_no_events() {
    let mut r = rig(
        vec![
            vec![bash("c1", "echo hi"), done("tool_use")],
            vec![text("ok"), done("stop")],
        ],
        vec![],
        Policy::defaults(),
    );
    r.runtime
        .run_turn(steve(), vec![ContentBlock::Text("go".into())], &mut |_| {})
        .await
        .unwrap();
    assert_eq!(
        kinds(&r),
        vec![
            EventKind::UserMessage,
            EventKind::AssistantMessage,
            EventKind::ToolResult,
            EventKind::AssistantMessage,
            EventKind::TurnEnded,
        ]
    );
    let p = result_at(&r, 2);
    assert_eq!(
        p.policy,
        Some(PolicyRecord::rule("bash allow-pattern echo", "allow"))
    );
    assert_eq!(p.result.content.trim_end(), "hi");
    assert!(r.asked.lock().unwrap().is_empty(), "nobody was asked");
    audit(&r);
}

#[tokio::test]
async fn the_approver_answers_allow_session_and_deny_with_exact_events() {
    let mut r = rig(
        vec![
            vec![touch("c1", "a"), done("tool_use")],
            vec![touch("c2", "b"), touch("c3", "c"), done("tool_use")],
            // A different write tool: the session grant on `touch` covers
            // every later `touch`, so the deny must land on another tool.
            vec![
                ProviderEvent::ToolCall(ToolCall {
                    id: "c4".into(),
                    name: "write_file".into(),
                    args: json!({"path": "d", "content": ""}),
                }),
                done("tool_use"),
            ],
            vec![text("done"), done("stop")],
        ],
        vec![Answer::Allow, Answer::AllowForSession, Answer::Deny],
        Policy::defaults(),
    );
    r.runtime
        .run_turn(steve(), vec![ContentBlock::Text("go".into())], &mut |_| {})
        .await
        .unwrap();
    let events = r.runtime.log().read_all().unwrap();
    let k: Vec<EventKind> = events.iter().map(|e| e.kind).collect();
    assert_eq!(
        k,
        vec![
            EventKind::UserMessage,
            EventKind::AssistantMessage,
            EventKind::PermissionRequested, // c1
            EventKind::PermissionDecided,   // allow once
            EventKind::ToolResult,
            EventKind::AssistantMessage,
            EventKind::PermissionRequested, // c2
            EventKind::PermissionDecided,   // allow for session
            EventKind::ToolResult,
            EventKind::PermissionRequested, // c3: still an event...
            EventKind::PermissionDecided,   // ...answered by the grant, no prompt
            EventKind::ToolResult,
            EventKind::AssistantMessage,
            EventKind::PermissionRequested, // c4
            EventKind::PermissionDecided,   // deny
            EventKind::ToolResult,
            EventKind::AssistantMessage,
            EventKind::TurnEnded,
        ]
    );
    // Only three prompts reached the human; c3 was covered by the grant.
    let asked = r.asked.lock().unwrap();
    assert_eq!(
        asked.iter().map(|a| a.call.id.as_str()).collect::<Vec<_>>(),
        vec!["c1", "c2", "c4"]
    );
    assert_eq!(asked[0].class, RiskClass::Write);
    assert_eq!(asked[0].reason, "class write: changes the workspace");
    drop(asked);

    let requested = &events[2];
    assert_eq!(requested.author, Author::System);
    let req: PermissionRequestedPayload =
        serde_json::from_value(requested.payload.clone()).unwrap();
    assert_eq!(req.call.id, "c1");

    let decided = &events[3];
    assert_eq!(decided.author, steve(), "attributed to who answered");
    assert_eq!(decided.parent_event, Some(requested.id));
    let d: PermissionDecidedPayload = serde_json::from_value(decided.payload.clone()).unwrap();
    assert_eq!(
        d,
        PermissionDecidedPayload {
            call_id: "c1".into(),
            allow: true,
            scope: DecisionScope::Once
        }
    );
    assert_eq!(
        result_at(&r, 4).policy,
        Some(PolicyRecord::Human {
            event: decided.id,
            allow: true
        })
    );

    // Session grant: c2's decision is Session; c3's references it.
    let d2: PermissionDecidedPayload = serde_json::from_value(events[7].payload.clone()).unwrap();
    assert_eq!(d2.scope, DecisionScope::Session);
    let d3: PermissionDecidedPayload = serde_json::from_value(events[10].payload.clone()).unwrap();
    assert_eq!(
        (d3.call_id.as_str(), d3.allow, d3.scope),
        ("c3", true, DecisionScope::Session)
    );
    assert_eq!(
        events[10].parent_event,
        Some(events[7].id),
        "references the first grant"
    );
    assert_eq!(events[10].author, steve());

    // Deny: an error result the model sees, with the human record.
    let d4: PermissionDecidedPayload = serde_json::from_value(events[14].payload.clone()).unwrap();
    assert!(!d4.allow);
    let p = result_at(&r, 15);
    assert!(p.result.is_error);
    assert_eq!(
        p.result.content,
        "denied by policy: the human declined this call"
    );
    assert_eq!(
        p.policy,
        Some(PolicyRecord::Human {
            event: events[14].id,
            allow: false
        })
    );
    assert_eq!(
        *r.touched.lock().unwrap(),
        vec!["a", "b", "c"],
        "d never ran"
    );
    audit(&r);
}

#[tokio::test]
async fn a_session_grant_for_bash_is_per_exact_command() {
    let mut r = rig(
        vec![
            vec![bash("c1", "rm -f a"), done("tool_use")],
            vec![
                bash("c2", "rm -f a"),
                bash("c3", "rm -f b"),
                done("tool_use"),
            ],
            vec![text("done"), done("stop")],
        ],
        vec![Answer::AllowForSession, Answer::Deny],
        Policy::defaults(),
    );
    r.runtime
        .run_turn(steve(), vec![ContentBlock::Text("go".into())], &mut |_| {})
        .await
        .unwrap();
    let asked = r.asked.lock().unwrap();
    assert_eq!(
        asked.iter().map(|a| a.call.id.as_str()).collect::<Vec<_>>(),
        vec!["c1", "c3"],
        "c2 repeats c1's command and is covered; c3 differs and asks"
    );
    drop(asked);
    audit(&r);
}

#[tokio::test]
async fn a_rule_deny_refuses_without_asking() {
    let mut r = rig(
        vec![
            vec![touch("c1", "a"), done("tool_use")],
            vec![text("done"), done("stop")],
        ],
        vec![],
        Policy::configured(vec![Rule::tool("touch", Decision::Deny, "never")], None),
    );
    r.runtime
        .run_turn(steve(), vec![ContentBlock::Text("go".into())], &mut |_| {})
        .await
        .unwrap();
    assert_eq!(
        kinds(&r),
        vec![
            EventKind::UserMessage,
            EventKind::AssistantMessage,
            EventKind::ToolResult,
            EventKind::AssistantMessage,
            EventKind::TurnEnded,
        ]
    );
    let p = result_at(&r, 2);
    assert_eq!(p.result.content, "denied by policy: tool touch (never)");
    assert!(p.result.is_error);
    assert_eq!(
        p.policy,
        Some(PolicyRecord::rule_with_reason(
            "tool touch",
            "deny",
            "never"
        ))
    );
    assert!(r.touched.lock().unwrap().is_empty());
    audit(&r);
}

#[tokio::test]
async fn no_approver_means_deny() {
    let dir = tempfile::tempdir().unwrap();
    let log = ThreadLog::open(dir.path(), ulid::Ulid::generate()).unwrap();
    let (provider, _) = scripted(vec![
        vec![touch("c1", "a"), done("tool_use")],
        vec![text("done"), done("stop")],
    ]);
    let touched = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ToolRegistry::empty();
    registry.register(Box::new(Touch(touched.clone()))).unwrap();
    let mut rt = Runtime::new(provider, registry, log, aigentic_core::AgentId("w".into()));
    rt.run_turn(steve(), vec![ContentBlock::Text("go".into())], &mut |_| {})
        .await
        .unwrap();
    let events = rt.log().read_all().unwrap();
    assert_eq!(events[3].kind, EventKind::PermissionDecided);
    assert_eq!(events[3].author, Author::System);
    let p: ToolResultPayload = serde_json::from_value(events[4].payload.clone()).unwrap();
    assert!(p.result.is_error);
    assert!(touched.lock().unwrap().is_empty());
    assert!(aigentic_runtime::audit_tool_results(&events).is_empty());
}

#[tokio::test]
async fn an_unknown_tool_is_refused_with_a_rule_record() {
    let mut r = rig(
        vec![
            vec![
                ProviderEvent::ToolCall(ToolCall {
                    id: "c1".into(),
                    name: "nope".into(),
                    args: json!({}),
                }),
                done("tool_use"),
            ],
            vec![text("done"), done("stop")],
        ],
        vec![],
        Policy::defaults(),
    );
    r.runtime
        .run_turn(steve(), vec![ContentBlock::Text("go".into())], &mut |_| {})
        .await
        .unwrap();
    let p = result_at(&r, 2);
    assert!(p.result.is_error && p.result.content.contains("unknown tool: nope"));
    assert_eq!(p.policy, Some(PolicyRecord::rule("unknown tool", "deny")));
    audit(&r);
}

#[tokio::test]
async fn pin_and_ask_human_are_harness_tools_in_the_specs() {
    let mut r = rig(
        vec![
            vec![
                ProviderEvent::ToolCall(ToolCall {
                    id: "c1".into(),
                    name: "pin".into(),
                    args: json!({"text": "Use Swedish."}),
                }),
                ProviderEvent::ToolCall(ToolCall {
                    id: "c2".into(),
                    name: "ask_human".into(),
                    args: json!({"question": "Proceed?"}),
                }),
                done("tool_use"),
            ],
            vec![text("done"), done("stop")],
        ],
        vec![],
        Policy::defaults(),
    );
    let names: Vec<String> = r.runtime.tool_specs().into_iter().map(|s| s.name).collect();
    assert_eq!(
        names,
        vec![
            "ask_human",
            "bash",
            "edit_file",
            "grep",
            "list_dir",
            "pin",
            "read_file",
            "touch",
            "write_file"
        ]
    );
    r.runtime
        .run_turn(steve(), vec![ContentBlock::Text("go".into())], &mut |_| {})
        .await
        .unwrap();
    let events = r.runtime.log().read_all().unwrap();
    let k: Vec<EventKind> = events.iter().map(|e| e.kind).collect();
    assert_eq!(
        k,
        vec![
            EventKind::UserMessage,
            EventKind::AssistantMessage,
            EventKind::Pinned,
            EventKind::ToolResult,
            EventKind::ToolResult,
            EventKind::AssistantMessage,
            EventKind::TurnEnded,
        ]
    );
    assert_eq!(
        events[2].author,
        Author::Agent(aigentic_core::AgentId("worker".into()))
    );
    let pin = result_at(&r, 3);
    assert_eq!(pin.result.content, "pinned");
    assert_eq!(pin.policy, Some(PolicyRecord::rule("class safe", "allow")));
    let ask = result_at(&r, 4);
    assert_eq!(ask.result.content, "yes, go ahead");
    assert!(!ask.result.is_error);
    // The pin is in the next request's prefix.
    audit(&r);
}

#[tokio::test]
async fn ask_human_without_a_human_is_an_error_result() {
    let dir = tempfile::tempdir().unwrap();
    let log = ThreadLog::open(dir.path(), ulid::Ulid::generate()).unwrap();
    let (provider, _) = scripted(vec![
        vec![
            ProviderEvent::ToolCall(ToolCall {
                id: "c1".into(),
                name: "ask_human".into(),
                args: json!({"question": "Proceed?"}),
            }),
            done("tool_use"),
        ],
        vec![text("done"), done("stop")],
    ]);
    let mut rt = Runtime::new(
        provider,
        ToolRegistry::empty(),
        log,
        aigentic_core::AgentId("w".into()),
    );
    rt.run_turn(steve(), vec![ContentBlock::Text("go".into())], &mut |_| {})
        .await
        .unwrap();
    let events = rt.log().read_all().unwrap();
    let p: ToolResultPayload = serde_json::from_value(events[2].payload.clone()).unwrap();
    assert!(p.result.is_error);
    assert!(p.result.content.contains("no human available"));
}

// Keep the unused-import lint quiet for the helper type we re-export.
#[allow(dead_code)]
fn _uses(_: ScriptedProvider) {}

#[tokio::test]
async fn a_tool_the_layers_hide_is_not_offered_and_is_refused_as_unknown() {
    use aigentic_runtime::{GlobalLayer, Layers};
    let mut r = rig(
        vec![
            vec![touch("c1", "a"), done("tool_use")],
            vec![text("done"), done("stop")],
        ],
        vec![Answer::Allow],
        Policy::defaults(),
    );
    // Rig has no Drop, so the runtime can move out and back.
    r.runtime = r.runtime.with_layers(Layers {
        global: GlobalLayer {
            instructions: None,
            denied_tools: vec!["touch".into(), "mcp.*".into()],
            denied_skills: vec![],
        },
        project: None,
    });
    let names: Vec<String> = r.runtime.tool_specs().into_iter().map(|s| s.name).collect();
    assert!(!names.contains(&"touch".to_owned()), "{names:?}");
    assert!(names.contains(&"bash".to_owned()));
    r.runtime
        .run_turn(steve(), vec![ContentBlock::Text("go".into())], &mut |_| {})
        .await
        .unwrap();
    let p = result_at(&r, 2);
    assert!(p.result.is_error && p.result.content.contains("unknown tool: touch"));
    assert_eq!(p.policy, Some(PolicyRecord::rule("unknown tool", "deny")));
    assert!(
        r.asked.lock().unwrap().is_empty(),
        "never reached the approver"
    );
    assert!(r.touched.lock().unwrap().is_empty());
    audit(&r);
}
