//! Thread titles (phase 6 step 9): proposed by the utility model after
//! the first finished turn, only when one is configured; `rename` wins;
//! a titled thread is never titled again.

mod common;

use aigentic_core::{Author, ContentBlock, EventKind, ProviderEvent, UserId};
use aigentic_log::{ThreadLog, ThreadRenamedPayload};
use aigentic_runtime::Prices;
use aigentic_runtime::Runtime;
use aigentic_runtime::title::title_of;
use aigentic_tools::ToolRegistry;
use common::{done, scripted};

fn steve() -> Author {
    Author::User(UserId("steve".into()))
}

fn text(t: &str) -> ProviderEvent {
    ProviderEvent::TextDelta(t.into())
}

/// The log's last `thread_renamed`, as the payload the recorder wrote.
fn last_renamed(rt: &Runtime) -> ThreadRenamedPayload {
    let events = rt.log().read_all().unwrap();
    let event = events
        .iter()
        .rev()
        .find(|e| e.kind == EventKind::ThreadRenamed)
        .expect("a thread_renamed line");
    serde_json::from_value(event.payload.clone()).unwrap()
}

fn runtime(dir: &std::path::Path, script: Vec<Vec<ProviderEvent>>) -> Runtime {
    let log = ThreadLog::open(dir, ulid::Ulid::generate()).unwrap();
    let (provider, _) = scripted(script);
    Runtime::new(
        provider,
        ToolRegistry::empty(),
        log,
        aigentic_core::AgentId("worker".into()),
    )
}

#[tokio::test]
async fn the_utility_model_titles_the_first_finished_turn_once() {
    let dir = tempfile::tempdir().unwrap();
    let (utility, seen) = scripted(vec![vec![
        text("\"Crate dependency check.\""),
        done("stop"),
    ]]);
    let mut rt = runtime(
        dir.path(),
        vec![vec![text("They share serde."), done("stop")]],
    )
    .with_utility(utility, "utility-model", None);
    // Nothing to title before a turn has finished.
    assert_eq!(rt.title_if_untitled(&mut |_| {}).await.unwrap(), None);
    rt.run_turn(
        steve(),
        vec![ContentBlock::Text(
            "Which deps do tui and api share?".into(),
        )],
        &mut |_| {},
    )
    .await
    .unwrap();
    let title = rt.title_if_untitled(&mut |_| {}).await.unwrap();
    assert_eq!(title.as_deref(), Some("Crate dependency check"));
    assert_eq!(
        rt.title().unwrap().as_deref(),
        Some("Crate dependency check")
    );
    // The prompt carried the first message and reply.
    {
        let seen = seen.lock().unwrap();
        let prompt = format!("{:?}", seen[0]);
        assert!(
            prompt.contains("Which deps do tui and api share?"),
            "{prompt}"
        );
        assert!(prompt.contains("They share serde."), "{prompt}");
    }
    // Titled: no second call (the utility script is spent; a call would
    // pend forever).
    assert_eq!(rt.title_if_untitled(&mut |_| {}).await.unwrap(), None);
    // A person's rename wins.
    rt.rename(steve(), "Deps", &mut |_| {}).unwrap();
    assert_eq!(
        title_of(&rt.log().read_all().unwrap()).as_deref(),
        Some("Deps")
    );
}

#[tokio::test]
async fn without_a_utility_model_nothing_is_titled() {
    let dir = tempfile::tempdir().unwrap();
    let mut rt = runtime(dir.path(), vec![vec![text("ok"), done("stop")]]);
    rt.run_turn(steve(), vec![ContentBlock::Text("hi".into())], &mut |_| {})
        .await
        .unwrap();
    assert_eq!(rt.title_if_untitled(&mut |_| {}).await.unwrap(), None);
    assert_eq!(rt.title().unwrap(), None);
}

/// Issue #49: the title call goes on the books. With a utility profile
/// that has a `[prices]` table, the `thread_renamed` line names the model
/// that ran it and carries a usage stamped with what that table says the
/// call cost — recomputed here from the same table and the scripted
/// usage, never a literal; with no table it claims nothing about money.
#[tokio::test]
async fn a_priced_utility_stamps_the_title_call_with_its_own_table() {
    let prices = Prices {
        input: 0.07,
        cache_read: 0.01,
        cache_write: 0.08,
        output: 0.28,
    };
    for table in [Some(prices), None] {
        let dir = tempfile::tempdir().unwrap();
        let (utility, _utility_seen) =
            scripted(vec![vec![text("\"Deps check.\""), common::usage(300, 20)]]);
        let mut rt = runtime(
            dir.path(),
            vec![vec![text("They share serde."), done("stop")]],
        )
        .with_utility(utility, "utility-model", table);
        rt.run_turn(
            steve(),
            vec![ContentBlock::Text(
                "Which deps do tui and api share?".into(),
            )],
            &mut |_| {},
        )
        .await
        .unwrap();
        assert_eq!(
            rt.title_if_untitled(&mut |_| {}).await.unwrap().as_deref(),
            Some("Deps check")
        );

        let payload = last_renamed(&rt);
        assert_eq!(payload.title, "Deps check");
        assert_eq!(payload.model.as_deref(), Some("utility-model"));
        let usage = payload.usage.expect("the call's usage");
        assert_eq!((usage.input_tokens, usage.output_tokens), (300, 20));
        // `common::usage(300, 20)`: 300 input at 0.07 and 20 output at
        // 0.28 per million when the table is there, from the table the
        // test passed in. The stamp is the code's own arithmetic.
        assert_eq!(usage.cost_usd, table.map(|p| p.cost_usd(&usage)));
        assert_eq!(usage.cost_usd.is_some(), table.is_some());
    }
}

/// A person's `/rename` makes no call: the line has the title and nothing
/// else, the shape every line written before issue #49 has.
#[tokio::test]
async fn a_persons_rename_carries_no_model_and_no_usage() {
    let dir = tempfile::tempdir().unwrap();
    let mut rt = runtime(dir.path(), vec![vec![text("ok"), done("stop")]]);
    rt.rename(steve(), "Deps", &mut |_| {}).unwrap();

    let payload = last_renamed(&rt);
    assert_eq!(payload.title, "Deps");
    assert_eq!(payload.model, None);
    assert_eq!(payload.usage, None);
    // And the bytes on the line are old-style: no nulls, no empty object.
    let events = rt.log().read_all().unwrap();
    let event = events
        .iter()
        .find(|e| e.kind == EventKind::ThreadRenamed)
        .unwrap();
    assert_eq!(event.payload, serde_json::json!({"title": "Deps"}));
}
