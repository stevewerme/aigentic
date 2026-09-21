//! A human's answer starts a turn (docs/PLAN-phase4.md step 9): the
//! answered `ask_human` ends the turn with `asked_human`, and the
//! continuation runs on a fresh budget.

mod common;

use std::time::Duration;

use aigentic_core::{Author, Budget, ContentBlock, EventKind, ProviderEvent, ToolCall};
use aigentic_log::{PermissionRequestedPayload, ThreadLog, TurnEndedPayload};
use aigentic_runtime::{ASKED_HUMAN, Answer, Approver, Layers, Runtime};
use aigentic_tools::ToolRegistry;
use common::{EchoTool, call, done, scripted, steve};
use serde_json::json;
use std::sync::{Arc, Mutex};

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
