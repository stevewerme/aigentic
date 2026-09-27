//! Issue #47: a turn that slept says so in its `turn_ended` payload.

use std::time::Duration;

use aigentic_core::{ContentBlock, ProviderEvent};
use aigentic_log::TurnEndedPayload;

mod common;
use common::*;

fn payload(h: &Harness) -> TurnEndedPayload {
    let events = h.runtime.log().read_all().unwrap();
    let last = events.last().expect("the turn wrote a turn_ended");
    assert_eq!(last.kind, aigentic_core::EventKind::TurnEnded);
    serde_json::from_value(last.payload.clone()).unwrap()
}

/// T9 (issue #47): a scripted sleep is reported. Wall time runs five
/// minutes ahead of running time, so 240 s of the turn were sleep; the
/// wait breakdown is reported alongside it, and `wall_secs` is the whole
/// wall span.
#[tokio::test]
async fn a_scripted_sleep_lands_in_the_turn_ended_payload() {
    let mut h = harness_with_clock(
        vec![vec![ProviderEvent::TextDelta("done".into()), done("stop")]],
        None,
        Duration::from_secs(60),
        Duration::from_secs(300),
    );
    h.runtime
        .run_turn(steve(), vec![ContentBlock::Text("hi".into())], &mut |_| {})
        .await
        .unwrap();
    let p = payload(&h);
    assert_eq!(p.reason, "done");
    assert_eq!(p.wall_secs, Some(300));
    assert_eq!(p.slept_secs, Some(240));
    assert_eq!(p.slept_awaiting_secs, Some(0));
    assert_eq!(p.keep_awake, None);
}

/// T9 (issue #47), second half: with the real clock an ordinary turn
/// slept nowhere — the serialized line carries `wall_secs` and no
/// `slept_secs` key at all, so a reader cannot mistake a short turn for
/// a nap.
#[tokio::test]
async fn a_real_clock_turn_carries_no_slept_secs_key() {
    let mut h = harness(
        vec![vec![ProviderEvent::TextDelta("done".into()), done("stop")]],
        None,
    );
    h.runtime
        .run_turn(steve(), vec![ContentBlock::Text("hi".into())], &mut |_| {})
        .await
        .unwrap();
    let events = h.runtime.log().read_all().unwrap();
    let last = events.last().unwrap();
    let value = last.payload.clone();
    assert_eq!(value["reason"], "done");
    assert!(value.get("wall_secs").is_some(), "{value}");
    assert!(value.get("slept_secs").is_none(), "{value}");
    assert!(value.get("slept_awaiting_secs").is_none(), "{value}");
    assert!(value.get("keep_awake").is_none(), "{value}");
    let p = payload(&h);
    assert_eq!(p.slept_secs, None);
    assert_eq!(p.slept_awaiting_secs, None);
}
