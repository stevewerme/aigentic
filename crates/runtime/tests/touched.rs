//! A `turn_ended` names the files the turn wrote (docs/PLAN-phase4.md
//! step 9), so a stop on a budget says which files it was in.

mod common;

use aigentic_core::{ContentBlock, EventKind, ProviderEvent, RiskClass, ToolCall};
use aigentic_log::{ThreadLog, TurnEndedPayload};
use aigentic_policy::{Decision, Policy, Rule};
use aigentic_runtime::{Layers, Runtime};
use aigentic_tools::{ToolRegistry, Workdir};
use common::{done, scripted, steve};
use serde_json::json;

fn call(id: &str, name: &str, args: serde_json::Value) -> ProviderEvent {
    ProviderEvent::ToolCall(ToolCall {
        id: id.into(),
        name: name.into(),
        args,
    })
}

#[tokio::test]
async fn the_turn_end_lists_files_written_without_error_once_each() {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let log = ThreadLog::open(dir.path(), ulid::Ulid::generate()).unwrap();
    let (provider, _) = scripted(vec![
        vec![
            call(
                "c1",
                "write_file",
                json!({"path": "a.rs", "content": "fn a() {}\n"}),
            ),
            call(
                "c2",
                "write_file",
                json!({"path": "b.rs", "content": "fn b() {}\n"}),
            ),
            call("c3", "read_file", json!({"path": "a.rs"})),
            done("tool_use"),
        ],
        vec![
            call(
                "c4",
                "edit_file",
                json!({"path": "a.rs", "old_string": "fn a()", "new_string": "fn a2()"}),
            ),
            call(
                "c5",
                "edit_file",
                json!({"path": "missing.rs", "old_string": "x", "new_string": "y"}),
            ),
            done("tool_use"),
        ],
        vec![ProviderEvent::TextDelta("done".into()), done("stop")],
    ]);
    let mut rt = Runtime::new(
        provider,
        ToolRegistry::builtin(Workdir::new(&work)),
        log,
        aigentic_core::AgentId("w".into()),
    )
    .with_layers(Layers::default())
    .with_policy(Policy::configured(
        vec![Rule::class(RiskClass::Write, Decision::Allow, "test")],
        None,
    ));
    let outcome = rt
        .run_turn(
            steve(),
            vec![ContentBlock::Text("write".into())],
            &mut |_| {},
        )
        .await
        .unwrap();
    assert_eq!(outcome.reason, "done");
    assert_eq!(
        outcome.touched,
        vec!["a.rs", "b.rs"],
        "a.rs once, the failed edit not at all"
    );
    let events = rt.log().read_all().unwrap();
    let end = events
        .iter()
        .rfind(|e| e.kind == EventKind::TurnEnded)
        .unwrap();
    let p: TurnEndedPayload = serde_json::from_value(end.payload.clone()).unwrap();
    assert_eq!(p.touched, vec!["a.rs", "b.rs"]);
    assert_eq!(end.payload["touched"], json!(["a.rs", "b.rs"]));
}

#[tokio::test]
async fn a_turn_without_writes_carries_no_list_and_old_lines_read_back() {
    let dir = tempfile::tempdir().unwrap();
    let log = ThreadLog::open(dir.path(), ulid::Ulid::generate()).unwrap();
    let (provider, _) = scripted(vec![vec![
        ProviderEvent::TextDelta("hi".into()),
        done("stop"),
    ]]);
    let mut rt = Runtime::new(
        provider,
        ToolRegistry::empty(),
        log,
        aigentic_core::AgentId("w".into()),
    )
    .with_layers(Layers::default());
    let outcome = rt
        .run_turn(steve(), vec![ContentBlock::Text("hi".into())], &mut |_| {})
        .await
        .unwrap();
    assert!(outcome.touched.is_empty());
    let events = rt.log().read_all().unwrap();
    let end = events.last().unwrap();
    assert_eq!(
        end.payload,
        json!({"reason": "done"}),
        "no field when empty"
    );
    let old: TurnEndedPayload = serde_json::from_value(json!({"reason": "done"})).unwrap();
    assert!(old.touched.is_empty());
}
