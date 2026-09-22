//! Thread titles (phase 6 step 9): proposed by the utility model after
//! the first finished turn, only when one is configured; `rename` wins;
//! a titled thread is never titled again.

mod common;

use aigentic_core::{Author, ContentBlock, ProviderEvent, UserId};
use aigentic_log::ThreadLog;
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
    .with_utility(utility);
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
