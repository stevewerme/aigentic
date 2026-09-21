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

/// What [`ThreadLog::open_with`] may do to a damaged file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Repair {
    /// Any damage is an error; today's `open`.
    Refuse,
    /// A torn last line (a crash mid-append) is cut at the byte offset the
    /// error reports. Nothing acknowledged is ever lost: the torn bytes
    /// were never a complete event. Other damage is still an error.
    TruncateTornTail,
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
        Self::open_with(dir, thread_id, Repair::Refuse).map(|(log, _)| log)
    }

    /// `open`, optionally repairing a torn tail. Returns the log and how
    /// many bytes were cut, if any.
    pub fn open_with(
        dir: impl AsRef<Path>,
        thread_id: Ulid,
        repair: Repair,
    ) -> Result<(Self, Option<u64>), LogError> {
        let path = dir.as_ref().join(format!("{thread_id}.jsonl"));
        let mut cut = None;
        let next_seq = if path.exists() {
            match read_file(&path, thread_id) {
                Ok(events) => events.len() as u64,
                Err(LogError::TruncatedTail {
                    offset,
                    good_events,
                    ..
                }) if repair == Repair::TruncateTornTail => {
                    let io = |source| LogError::Io {
                        path: path.clone(),
                        source,
                    };
                    let len = std::fs::metadata(&path).map_err(io)?.len();
                    OpenOptions::new()
                        .write(true)
                        .open(&path)
                        .map_err(io)?
                        .set_len(offset)
                        .map_err(io)?;
                    cut = Some(len - offset);
                    good_events as u64
                }
                Err(e) => return Err(e),
            }
        } else {
            0
        };
        Ok((
            Self {
                thread_id,
                path,
                next_seq,
                ids: Generator::new(),
            },
            cut,
        ))
    }

    /// The events of a turn that never ended: everything after the last
    /// `turn_ended`, when that tail holds a message or a tool result.
    /// Pins, compactions and interrupted notes between turns do not count.
    pub fn open_turn(events: &[Event]) -> Option<&[Event]> {
        let start = events
            .iter()
            .rposition(|e| e.kind == EventKind::TurnEnded)
            .map_or(0, |i| i + 1);
        let tail = &events[start..];
        tail.iter()
            .any(|e| {
                matches!(
                    e.kind,
                    EventKind::UserMessage | EventKind::AssistantMessage | EventKind::ToolResult
                )
            })
            .then_some(tail)
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
    fn torn_tail_can_be_cut_on_open_and_the_log_continues() {
        let dir = tempfile::tempdir().unwrap();
        let thread = Ulid::generate();
        let mut log = ThreadLog::open(dir.path(), thread).unwrap();
        log.append(user("one")).unwrap();
        log.append(user("two")).unwrap();
        let text = std::fs::read_to_string(log.path()).unwrap();
        let first_len = text.find('\n').unwrap() + 1;
        let cut = first_len + (text.len() - first_len) / 2;
        std::fs::write(log.path(), &text[..cut]).unwrap();
        drop(log);

        assert!(matches!(
            ThreadLog::open(dir.path(), thread).unwrap_err(),
            LogError::TruncatedTail { .. }
        ));
        let (mut log, cut_bytes) =
            ThreadLog::open_with(dir.path(), thread, Repair::TruncateTornTail).unwrap();
        assert_eq!(cut_bytes, Some((cut - first_len) as u64));
        assert_eq!(log.len(), 1);
        let e = log.append(user("three")).unwrap();
        assert_eq!(e.seq, 1);
        let events = log.read_all().unwrap();
        assert_eq!(events.len(), 2);

        // A clean file reports no cut.
        let (_, none) = ThreadLog::open_with(dir.path(), thread, Repair::TruncateTornTail).unwrap();
        assert_eq!(none, None);
    }

    #[test]
    fn open_turn_detects_a_turn_that_never_ended() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = ThreadLog::open(dir.path(), Ulid::generate()).unwrap();
        log.append(user("one")).unwrap();
        log.append(turn_ended()).unwrap();
        assert!(ThreadLog::open_turn(&log.read_all().unwrap()).is_none());

        log.append(NewEvent {
            kind: EventKind::Pinned,
            author: Author::System,
            payload: json!({"text": "fact"}),
            parent_event: None,
        })
        .unwrap();
        assert!(
            ThreadLog::open_turn(&log.read_all().unwrap()).is_none(),
            "a pin is not a turn"
        );

        log.append(user("two")).unwrap();
        log.append(assistant(vec![ContentBlock::Text("hm".into())]))
            .unwrap();
        let events = log.read_all().unwrap();
        let open = ThreadLog::open_turn(&events).unwrap();
        assert_eq!(
            open.len(),
            3,
            "pin, user, assistant since the last turn_ended"
        );
        assert_eq!(open[1].kind, EventKind::UserMessage);
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

        let messages = project(&log.read_all().unwrap()).unwrap().body;
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
