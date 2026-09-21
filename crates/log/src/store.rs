use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use aigentic_core::{Author, Event, EventKind};
use time::OffsetDateTime;
use ulid::{Generator, Ulid};

/// Errors from reading or writing a thread log.
#[derive(Debug, thiserror::Error)]
pub enum LogError {
    #[error("io error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// A line other than the last failed to parse. The file is corrupt.
    #[error("{path}:{line}: malformed event: {source}")]
    Malformed {
        path: PathBuf,
        line: usize,
        #[source]
        source: serde_json::Error,
    },
    /// The last line is incomplete (no trailing newline or unparsable),
    /// which is what a crash mid-append leaves behind. `good_events` events
    /// before it are intact; the caller decides whether to truncate at
    /// `offset` and resume.
    #[error(
        "{path}:{line}: truncated last line after {good_events} good events (byte offset {offset})"
    )]
    TruncatedTail {
        path: PathBuf,
        line: usize,
        good_events: usize,
        offset: u64,
    },
    #[error("{path}:{line}: seq gap, expected {expected} found {found}")]
    SeqGap {
        path: PathBuf,
        line: usize,
        expected: u64,
        found: u64,
    },
    #[error("{path}:{line}: event belongs to thread {found}, log is thread {expected}")]
    ThreadMismatch {
        path: PathBuf,
        line: usize,
        expected: Ulid,
        found: Ulid,
    },
    #[error("event seq {seq} ({kind:?}) has a malformed payload: {source}")]
    Payload {
        seq: u64,
        kind: EventKind,
        #[source]
        source: serde_json::Error,
    },
    #[error("could not generate a monotonic ulid")]
    UlidOverflow,
}

/// What a caller supplies to [`ThreadLog::append`]; the store assigns
/// `id`, `thread_id`, `seq` and `created_at`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewEvent {
    pub kind: EventKind,
    pub author: Author,
    pub payload: serde_json::Value,
    pub parent_event: Option<Ulid>,
}

/// The single writer for one thread's JSONL file.
///
/// One turn at a time, one append at a time: hold one `ThreadLog` per thread
/// and route every write through it.
#[derive(Debug)]
pub struct ThreadLog {
    thread_id: Ulid,
    path: PathBuf,
    next_seq: u64,
    ids: Generator,
}

impl ThreadLog {
    /// Open (or create) the log for `thread_id` under `dir`, at
    /// `<dir>/<thread_id>.jsonl`. An existing file is validated in full so
    /// the next `seq` is known; a damaged file is an error, never repaired
    /// silently.
    pub fn open(dir: impl AsRef<Path>, thread_id: Ulid) -> Result<Self, LogError> {
        let path = dir.as_ref().join(format!("{thread_id}.jsonl"));
        let next_seq = if path.exists() {
            read_file(&path, thread_id)?.len() as u64
        } else {
            0
        };
        Ok(Self {
            thread_id,
            path,
            next_seq,
            ids: Generator::new(),
        })
    }

    pub fn thread_id(&self) -> Ulid {
        self.thread_id
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Number of events in the log, which is also the next `seq`.
    pub fn len(&self) -> u64 {
        self.next_seq
    }

    pub fn is_empty(&self) -> bool {
        self.next_seq == 0
    }

    /// Append one event and return it as stored. The line is flushed and
    /// synced before this returns.
    pub fn append(&mut self, new: NewEvent) -> Result<Event, LogError> {
        let event = Event {
            id: self.ids.generate().map_err(|_| LogError::UlidOverflow)?,
            thread_id: self.thread_id,
            seq: self.next_seq,
            kind: new.kind,
            author: new.author,
            payload: new.payload,
            parent_event: new.parent_event,
            created_at: OffsetDateTime::now_utc(),
        };
        let mut line = serde_json::to_vec(&event).expect("Event is always serialisable");
        line.push(b'\n');

        let io = |source| LogError::Io {
            path: self.path.clone(),
            source,
        };
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(io)?;
        file.write_all(&line).map_err(io)?;
        file.sync_data().map_err(io)?;

        self.next_seq += 1;
        Ok(event)
    }

    /// Replay the whole file, oldest first, validating thread id and gapless
    /// `seq` on the way.
    pub fn read_all(&self) -> Result<Vec<Event>, LogError> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        read_file(&self.path, self.thread_id)
    }
}

fn read_file(path: &Path, thread_id: Ulid) -> Result<Vec<Event>, LogError> {
    let io = |source| LogError::Io {
        path: path.to_path_buf(),
        source,
    };
    let mut reader = BufReader::new(File::open(path).map_err(io)?);
    let mut events = Vec::new();
    let mut buf = Vec::new();
    let mut offset: u64 = 0;
    let mut line_no = 0usize;

    loop {
        buf.clear();
        let read = reader.read_until(b'\n', &mut buf).map_err(io)?;
        if read == 0 {
            break;
        }
        line_no += 1;
        let complete = buf.last() == Some(&b'\n');
        let parsed = serde_json::from_slice::<Event>(&buf);

        let event = match (parsed, complete) {
            (Ok(event), true) => event,
            (Err(source), true) => {
                return Err(LogError::Malformed {
                    path: path.to_path_buf(),
                    line: line_no,
                    source,
                });
            }
            // No trailing newline: the append never finished, whether or not
            // the bytes happen to parse.
            (_, false) => {
                return Err(LogError::TruncatedTail {
                    path: path.to_path_buf(),
                    line: line_no,
                    good_events: events.len(),
                    offset,
                });
            }
        };

        if event.thread_id != thread_id {
            return Err(LogError::ThreadMismatch {
                path: path.to_path_buf(),
                line: line_no,
                expected: thread_id,
                found: event.thread_id,
            });
        }
        let expected = events.len() as u64;
        if event.seq != expected {
            return Err(LogError::SeqGap {
                path: path.to_path_buf(),
                line: line_no,
                expected,
                found: event.seq,
            });
        }

        offset += read as u64;
        events.push(event);
    }
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::payload::{AssistantMessagePayload, TurnEndedPayload, Usage, UserMessagePayload};
    use crate::project;
    use aigentic_core::{AgentId, ContentBlock, Role, ToolCall, ToolResult, UserId};
    use serde_json::json;

    fn user(text: &str) -> NewEvent {
        NewEvent {
            kind: EventKind::UserMessage,
            author: Author::User(UserId("steve".into())),
            payload: serde_json::to_value(UserMessagePayload {
                blocks: vec![ContentBlock::Text(text.into())],
            })
            .unwrap(),
            parent_event: None,
        }
    }

    fn assistant(blocks: Vec<ContentBlock>) -> NewEvent {
        NewEvent {
            kind: EventKind::AssistantMessage,
            author: Author::Agent(AgentId("worker".into())),
            payload: serde_json::to_value(AssistantMessagePayload {
                blocks,
                usage: Some(Usage::reported(aigentic_core::Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    ..Default::default()
                })),
            })
            .unwrap(),
            parent_event: None,
        }
    }

    fn turn_ended() -> NewEvent {
        NewEvent {
            kind: EventKind::TurnEnded,
            author: Author::Agent(AgentId("worker".into())),
            payload: serde_json::to_value(TurnEndedPayload {
                reason: "done".into(),
            })
            .unwrap(),
            parent_event: None,
        }
    }

    #[test]
    fn append_then_read_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let thread = Ulid::generate();
        let mut log = ThreadLog::open(dir.path(), thread).unwrap();
        assert!(log.is_empty());

        let a = log.append(user("hi")).unwrap();
        let b = log
            .append(assistant(vec![ContentBlock::Text("hello".into())]))
            .unwrap();
        let c = log.append(turn_ended()).unwrap();
        assert_eq!((a.seq, b.seq, c.seq), (0, 1, 2));
        assert!(
            a.id < b.id && b.id < c.id,
            "ids must be sortable in append order"
        );
        assert_eq!(log.len(), 3);

        let read = log.read_all().unwrap();
        assert_eq!(read, vec![a, b, c]);

        let text = std::fs::read_to_string(log.path()).unwrap();
        assert_eq!(text.lines().count(), 3);
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn reopen_resumes_seq_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let thread = Ulid::generate();
        let mut log = ThreadLog::open(dir.path(), thread).unwrap();
        log.append(user("one")).unwrap();
        log.append(user("two")).unwrap();
        drop(log);

        let mut log = ThreadLog::open(dir.path(), thread).unwrap();
        assert_eq!(log.len(), 2);
        let e = log.append(user("three")).unwrap();
        assert_eq!(e.seq, 2);
        assert_eq!(log.read_all().unwrap().len(), 3);
    }

    #[test]
    fn seq_gaps_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let thread = Ulid::generate();
        let mut log = ThreadLog::open(dir.path(), thread).unwrap();
        log.append(user("one")).unwrap();
        log.append(user("two")).unwrap();
        let third = log.append(user("three")).unwrap();

        // Rewrite the file with the middle line removed.
        let text = std::fs::read_to_string(log.path()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        std::fs::write(log.path(), format!("{}\n{}\n", lines[0], lines[2])).unwrap();

        let err = log.read_all().unwrap_err();
        match err {
            LogError::SeqGap {
                line,
                expected,
                found,
                ..
            } => {
                assert_eq!((line, expected, found), (2, 1, third.seq));
            }
            other => panic!("expected SeqGap, got {other:?}"),
        }
        assert!(matches!(
            ThreadLog::open(dir.path(), thread).unwrap_err(),
            LogError::SeqGap { .. }
        ));
    }

    #[test]
    fn truncated_last_line_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let thread = Ulid::generate();
        let mut log = ThreadLog::open(dir.path(), thread).unwrap();
        log.append(user("one")).unwrap();
        log.append(user("two")).unwrap();

        // Simulate a crash mid-append: chop the last line in half.
        let text = std::fs::read_to_string(log.path()).unwrap();
        let first_len = text.find('\n').unwrap() + 1;
        let cut = first_len + (text.len() - first_len) / 2;
        std::fs::write(log.path(), &text[..cut]).unwrap();

        match log.read_all().unwrap_err() {
            LogError::TruncatedTail {
                line,
                good_events,
                offset,
                ..
            } => {
                assert_eq!((line, good_events), (2, 1));
                assert_eq!(offset as usize, first_len);
            }
            other => panic!("expected TruncatedTail, got {other:?}"),
        }
    }

    #[test]
    fn complete_last_line_without_newline_is_still_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let thread = Ulid::generate();
        let mut log = ThreadLog::open(dir.path(), thread).unwrap();
        log.append(user("one")).unwrap();
        let text = std::fs::read_to_string(log.path()).unwrap();
        std::fs::write(log.path(), text.trim_end()).unwrap();
        assert!(matches!(
            log.read_all().unwrap_err(),
            LogError::TruncatedTail { good_events: 0, .. }
        ));
    }

    #[test]
    fn malformed_middle_line_is_corrupt_not_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let thread = Ulid::generate();
        let mut log = ThreadLog::open(dir.path(), thread).unwrap();
        log.append(user("one")).unwrap();
        log.append(user("two")).unwrap();
        let text = std::fs::read_to_string(log.path()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        std::fs::write(log.path(), format!("{{not json}}\n{}\n", lines[1])).unwrap();
        assert!(matches!(
            log.read_all().unwrap_err(),
            LogError::Malformed { line: 1, .. }
        ));
    }

    #[test]
    fn foreign_thread_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let a = Ulid::generate();
        let mut log = ThreadLog::open(dir.path(), a).unwrap();
        log.append(user("one")).unwrap();
        let b = Ulid::generate();
        std::fs::rename(log.path(), dir.path().join(format!("{b}.jsonl"))).unwrap();
        assert!(matches!(
            ThreadLog::open(dir.path(), b).unwrap_err(),
            LogError::ThreadMismatch { .. }
        ));
    }

    #[test]
    fn resume_is_read_all_then_project() {
        let dir = tempfile::tempdir().unwrap();
        let thread = Ulid::generate();
        let mut log = ThreadLog::open(dir.path(), thread).unwrap();
        log.append(user("read Cargo.toml")).unwrap();
        let call = ToolCall {
            id: "call_1".into(),
            name: "read_file".into(),
            args: json!({"path": "Cargo.toml"}),
        };
        let asst = log
            .append(assistant(vec![
                ContentBlock::Text("Reading.".into()),
                ContentBlock::ToolCall(call.clone()),
            ]))
            .unwrap();
        let result = ToolResult {
            id: "call_1".into(),
            content: "[workspace]".into(),
            is_error: false,
        };
        log.append(NewEvent {
            kind: EventKind::ToolResult,
            author: Author::System,
            payload: serde_json::to_value(&result).unwrap(),
            parent_event: Some(asst.id),
        })
        .unwrap();
        log.append(turn_ended()).unwrap();

        let messages = project(&log.read_all().unwrap()).unwrap();
        assert_eq!(messages.len(), 3, "turn_ended emits no message");
        assert_eq!(messages[0].role, Role::User);
        assert_eq!(messages[0].author, Author::User(UserId("steve".into())));
        assert_eq!(messages[1].role, Role::Assistant);
        assert_eq!(messages[1].blocks[1], ContentBlock::ToolCall(call));
        assert_eq!(messages[2].role, Role::Tool);
        assert_eq!(messages[2].blocks, vec![ContentBlock::ToolResult(result)]);
    }

    #[test]
    fn malformed_payload_is_reported_by_seq() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = ThreadLog::open(dir.path(), Ulid::generate()).unwrap();
        log.append(NewEvent {
            kind: EventKind::UserMessage,
            author: Author::System,
            payload: json!({"nope": 1}),
            parent_event: None,
        })
        .unwrap();
        assert!(matches!(
            project(&log.read_all().unwrap()).unwrap_err(),
            LogError::Payload { seq: 0, .. }
        ));
    }
}
