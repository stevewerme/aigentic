//! The in-memory cache the log serves reads from: it equals what was
//! appended, and the file is read only at open — twice when a torn tail
//! is repaired.

use aigentic_core::{Author, ContentBlock, EventKind, UserId};
use aigentic_log::{NewEvent, Repair, ThreadLog, UserMessagePayload};
use ulid::Ulid;

fn user(text: &str) -> NewEvent {
    NewEvent {
        kind: EventKind::UserMessage,
        author: Author::User(UserId("steve".into())),
        payload: serde_json::to_value(UserMessagePayload::new(vec![ContentBlock::Text(
            text.into(),
        )]))
        .unwrap(),
        parent_event: None,
    }
}

#[test]
fn cache_matches_file_across_appends() {
    let dir = tempfile::tempdir().unwrap();
    let mut log = ThreadLog::open(dir.path(), Ulid::generate()).unwrap();
    assert_eq!(log.file_reads(), 0, "a fresh open has no file to read");

    let mut appended = Vec::new();
    for i in 0..5 {
        appended.push(log.append(user(&format!("note {i}"))).unwrap());
        assert_eq!(log.events(), appended.as_slice());
        assert_eq!(log.read_all().unwrap(), appended);
        assert_eq!(log.len(), appended.len() as u64);
    }
}

#[test]
fn repaired_torn_tail_seeds_cache() {
    let dir = tempfile::tempdir().unwrap();
    let thread = Ulid::generate();
    let mut log = ThreadLog::open(dir.path(), thread).unwrap();
    let mut fixture = Vec::new();
    for i in 0..4 {
        fixture.push(log.append(user(&format!("note {i}"))).unwrap());
    }
    let n = fixture.len() as u64;

    // A crash mid-append: a partial line past the last newline.
    let text = std::fs::read_to_string(log.path()).unwrap();
    let torn = "{\"kind\":\"user_message\"";
    std::fs::write(log.path(), format!("{text}{torn}")).unwrap();
    drop(log);

    let (log, cut) = ThreadLog::open_with(dir.path(), thread, Repair::TruncateTornTail).unwrap();
    assert_eq!(cut, Some(torn.len() as u64));
    assert_eq!(log.events(), fixture.as_slice());
    assert_eq!(log.len(), n);
    for (i, e) in log.events().iter().enumerate() {
        assert_eq!(e.seq, i as u64, "read_file's gapless rule");
    }
    assert_eq!(
        log.file_reads(),
        2,
        "the failed pass and the re-read after the cut"
    );
}
