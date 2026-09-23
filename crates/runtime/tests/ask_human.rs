//! A human's answer starts a turn (docs/PLAN-phase4.md step 9): the
//! answered `ask_human` ends the turn with `asked_human`, and the
//! continuation runs on a fresh budget.

mod common;

use std::time::Duration;

use aigentic_core::{Author, Budget, ContentBlock, EventKind, ProviderEvent, ToolCall};
use aigentic_log::{PermissionRequestedPayload, ThreadLog, TurnEndedPayload};
use aigentic_runtime::harness_tools::{
    AskHumanArgs, HARNESS_INSTRUCTIONS, HumanOption, HumanQuestion,
};
use aigentic_runtime::{ASKED_HUMAN, Answer, Approver, Layers, Runtime};
use aigentic_tools::ToolRegistry;
use common::{EchoTool, call, done, scripted, steve};
use serde_json::json;
use std::sync::{Arc, Mutex};

/// Both shapes parse: the new `questions` (1–4, with options and
/// headers) and the old single `question`, which normalises to one
/// question with no options. The bounds are enforced.
#[test]
fn ask_human_takes_questions_and_the_old_shape() {
    let args: AskHumanArgs = serde_json::from_value(json!({
        "questions": [
            {
                "question": "Which colour?",
                "header": "colour",
                "options": [
                    {"label": "Red", "description": "the warm one"},
                    {"label": "Green"}
                ]
            },
            {
                "question": "Which tests?",
                "multi": true,
                "options": [{"label": "unit"}, {"label": "integration"}]
            }
        ]
    }))
    .unwrap();
    assert_eq!(
        args.questions,
        vec![
            HumanQuestion {
                question: "Which colour?".into(),
                header: Some("colour".into()),
                options: vec![
                    HumanOption {
                        label: "Red".into(),
                        description: Some("the warm one".into()),
                    },
                    HumanOption {
                        label: "Green".into(),
                        description: None,
                    },
                ],
                multi: false,
            },
            HumanQuestion {
                question: "Which tests?".into(),
                header: None,
                options: vec![
                    HumanOption {
                        label: "unit".into(),
                        description: None,
                    },
                    HumanOption {
                        label: "integration".into(),
                        description: None,
                    },
                ],
                multi: true,
            },
        ]
    );
    let old: AskHumanArgs = serde_json::from_value(json!({"question": "go?"})).unwrap();
    assert_eq!(
        old.questions,
        vec![HumanQuestion {
            question: "go?".into(),
            header: None,
            options: vec![],
            multi: false,
        }]
    );
    // Nothing to ask, or more than four: the model is told.
    assert!(
        serde_json::from_value::<AskHumanArgs>(json!({})).is_err(),
        "no question at all"
    );
    let five = (0..5)
        .map(|i| json!({"question": format!("q{i}")}))
        .collect::<Vec<_>>();
    assert!(
        serde_json::from_value::<AskHumanArgs>(json!({"questions": five})).is_err(),
        "more than four questions"
    );
    assert!(
        serde_json::from_value::<AskHumanArgs>(json!({"questions": []})).is_err(),
        "an empty questions array"
    );
}

/// The spec and the standing instructions say what the issue asks:
/// every question in the call, options when the answer is a choice, and
/// no "Other" row of the model's own.
#[test]
fn the_spec_and_instructions_ask_for_questions_in_the_call() {
    let spec = aigentic_runtime::harness_tools::harness_specs(false)
        .into_iter()
        .find(|s| s.name == "ask_human")
        .unwrap();
    assert!(
        spec.schema["properties"]["questions"].is_object(),
        "{spec:?}"
    );
    assert!(spec.description.contains("every question in the call"));
    assert!(spec.description.contains("Other"));
    assert!(HARNESS_INSTRUCTIONS.contains("ask_human"));
    assert!(HARNESS_INSTRUCTIONS.contains("every question in the call"));
    assert!(HARNESS_INSTRUCTIONS.contains("Other"));
}

/// Answers every question with "yes" and allows every prompt.
struct Yes;
impl Approver for Yes {
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

fn ask(id: &str) -> ProviderEvent {
    ProviderEvent::ToolCall(ToolCall {
        id: id.into(),
        name: "ask_human".into(),
        args: json!({"question": "go?"}),
    })
}

fn text(t: &str) -> ProviderEvent {
    ProviderEvent::TextDelta(t.into())
}

#[tokio::test]
async fn an_answered_question_ends_the_turn_and_the_continuation_has_its_own_budget() {
    let dir = tempfile::tempdir().unwrap();
    let log = ThreadLog::open(dir.path(), ulid::Ulid::generate()).unwrap();
    let (provider, _) = scripted(vec![
        vec![ask("q1"), done("tool_use")],
        vec![ProviderEvent::ToolCall(call("c1", "hi")), done("tool_use")],
        vec![text("done"), done("stop")],
    ]);
    let registry: ToolRegistry =
        vec![Box::new(EchoTool(Arc::new(Mutex::new(Vec::new())))) as Box<dyn aigentic_core::Tool>]
            .into();
    let mut rt = Runtime::new(provider, registry, log, aigentic_core::AgentId("w".into()))
        .with_layers(Layers::default())
        .with_approver(Box::new(Yes))
        .with_budget(Budget {
            max_iterations: 2,
            max_tokens: 1_000_000,
            max_wall_time: Duration::from_secs(60),
            cache_read_price_ratio: 0.25,
        });
    let first = rt
        .run_turn(
            steve(),
            vec![ContentBlock::Text("start".into())],
            &mut |_| {},
        )
        .await
        .unwrap();
    assert_eq!(first.reason, ASKED_HUMAN);
    assert_eq!(first.iterations, 1);
    assert!(rt.awaiting_continuation().unwrap());

    let second = rt.continue_turn(&mut |_| {}).await.unwrap();
    assert_eq!(
        second.reason, "done",
        "two iterations fit a fresh budget of two"
    );
    assert_eq!(second.iterations, 2);
    assert!(!rt.awaiting_continuation().unwrap());

    let events = rt.log().read_all().unwrap();
    let kinds: Vec<EventKind> = events.iter().map(|e| e.kind).collect();
    assert_eq!(
        kinds,
        vec![
            EventKind::UserMessage,
            EventKind::AssistantMessage,
            EventKind::ToolResult,
            EventKind::TurnEnded,
            EventKind::AssistantMessage,
            EventKind::ToolResult,
            EventKind::AssistantMessage,
            EventKind::TurnEnded,
        ]
    );
    let reasons: Vec<String> = events
        .iter()
        .filter(|e| e.kind == EventKind::TurnEnded)
        .map(|e| {
            serde_json::from_value::<TurnEndedPayload>(e.payload.clone())
                .unwrap()
                .reason
        })
        .collect();
    assert_eq!(reasons, vec![ASKED_HUMAN.to_owned(), "done".to_owned()]);
    let answer: aigentic_log::ToolResultPayload =
        serde_json::from_value(events[2].payload.clone()).unwrap();
    assert_eq!(answer.result.content, "yes");
}

#[tokio::test]
async fn an_unanswered_question_does_not_split_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let log = ThreadLog::open(dir.path(), ulid::Ulid::generate()).unwrap();
    let (provider, _) = scripted(vec![
        vec![ask("q1"), done("tool_use")],
        vec![text("ok then"), done("stop")],
    ]);
    let mut rt = Runtime::new(
        provider,
        ToolRegistry::empty(),
        log,
        aigentic_core::AgentId("w".into()),
    )
    .with_layers(Layers::default());
    // The default approver has no human: the result is an error and the
    // turn runs on to `done` as before.
    let outcome = rt
        .run_turn(
            steve(),
            vec![ContentBlock::Text("start".into())],
            &mut |_| {},
        )
        .await
        .unwrap();
    assert_eq!(outcome.reason, "done");
    assert_eq!(outcome.iterations, 2);
    assert!(!rt.awaiting_continuation().unwrap());
}

/// A log the old binary wrote — a single-question `ask_human`, answered
/// — replays: the runtime continues from the recorded answer, the
/// model's context carries it, and nothing re-parses the call (event
/// kinds are added, never changed; AGENTS.md).
#[tokio::test]
async fn an_old_log_with_the_single_question_shape_replays() {
    use aigentic_core::ContentBlock;
    use aigentic_log::{
        AssistantMessagePayload, NewEvent, PolicyRecord, ToolResultPayload, TurnEndedPayload,
        UserMessagePayload,
    };

    let dir = tempfile::tempdir().unwrap();
    let mut log = ThreadLog::open(dir.path(), ulid::Ulid::generate()).unwrap();
    let ev = |kind, author, payload| NewEvent {
        kind,
        author,
        payload,
        parent_event: None,
    };
    let asked = ToolCall {
        id: "q1".into(),
        name: "ask_human".into(),
        args: json!({"question": "which colour?"}),
    };
    let assistant = ev(
        EventKind::AssistantMessage,
        Author::Agent(aigentic_core::AgentId("worker".into())),
        serde_json::to_value(AssistantMessagePayload {
            blocks: vec![ContentBlock::ToolCall(asked)],
            usage: None,
        })
        .unwrap(),
    );
    let answered = ev(
        EventKind::ToolResult,
        steve(),
        serde_json::to_value(ToolResultPayload::new(
            aigentic_core::ToolResult {
                id: "q1".into(),
                content: "blue".into(),
                is_error: false,
            },
            PolicyRecord::rule("class safe", "allow"),
        ))
        .unwrap(),
    );
    let parent = log.append(assistant).unwrap().id;
    let answered = NewEvent {
        parent_event: Some(parent),
        ..answered
    };
    for e in [
        ev(
            EventKind::UserMessage,
            steve(),
            serde_json::to_value(UserMessagePayload::new(vec![ContentBlock::Text(
                "start".into(),
            )]))
            .unwrap(),
        ),
        answered,
        ev(
            EventKind::TurnEnded,
            Author::Agent(aigentic_core::AgentId("worker".into())),
            serde_json::to_value(TurnEndedPayload::new(ASKED_HUMAN)).unwrap(),
        ),
    ] {
        log.append(e).unwrap();
    }
    let mut harness =
        common::harness_with_log(vec![vec![text("done"), done("stop")]], None, dir, log);
    assert!(harness.runtime.awaiting_continuation().unwrap());
    let outcome = harness.runtime.continue_turn(&mut |_| {}).await.unwrap();
    assert_eq!(outcome.reason, "done");
    // The continuation's context carries the recorded answer, not a
    // re-asked question.
    let seen = harness.seen.lock().unwrap();
    let carried = seen.iter().any(|msgs| {
        msgs.iter().any(|m| {
            m.blocks.iter().any(|b| match b {
                ContentBlock::ToolResult(r) => r.content == "blue",
                _ => false,
            })
        })
    });
    assert!(carried, "the old log's answer is in the context");
}
