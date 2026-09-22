//! The daemon's wire protocol: one JSON object per line in both
//! directions. A request carries an `id` and its response carries it
//! back; a notice has none and is pushed to every subscriber of a
//! thread. Depends on `core` only, so a client in any crate or language
//! needs nothing else; the `client` feature adds a `tokio` client.
//! See `docs/PLAN-phase5.md` sections 2 and 3.

#[cfg(feature = "client")]
pub mod client;

use std::path::PathBuf;

use aigentic_core::{Author, ContentBlock, Event, RiskClass, ToolCall};
use serde::{Deserialize, Serialize};
use ulid::Ulid;

/// Sent in `Hello`; a mismatch is refused with both numbers.
pub const PROTOCOL_VERSION: u32 = 1;

/// One line on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Frame {
    /// Present on a request and on its response; absent on a notice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    #[serde(flatten)]
    pub body: Body,
}

impl Frame {
    pub fn request(id: u64, request: Request) -> Self {
        Self {
            id: Some(id),
            body: Body::Request(request),
        }
    }
    pub fn response(id: u64, response: Response) -> Self {
        Self {
            id: Some(id),
            body: Body::Response(response),
        }
    }
    pub fn notice(notice: Notice) -> Self {
        Self {
            id: None,
            body: Body::Notice(notice),
        }
    }
}

/// `{"request": {...}}`, `{"response": {...}}` or `{"notice": {...}}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Body {
    Request(Request),
    Response(Response),
    Notice(Notice),
}

/// What a client asks. The role each needs is decided in the daemon
/// (plan section 6); a request without it is `Refused` and nothing is
/// appended.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Request {
    /// First on every connection. The token names the user; it is never
    /// echoed.
    Hello {
        protocol: u32,
        token: String,
    },
    ListProjects,
    ListThreads {
        project: String,
    },
    CreateThread {
        project: String,
    },
    /// The events since `from_seq`, then a live subscription.
    Open {
        thread: Ulid,
        from_seq: u64,
    },
    Close {
        thread: Ulid,
    },
    /// A message; `interrupt` cancels a running turn first.
    Post {
        thread: Ulid,
        blocks: Vec<ContentBlock>,
        #[serde(default)]
        interrupt: bool,
    },
    InvokeSkill {
        thread: Ulid,
        name: String,
        args: String,
    },
    /// Answer a pending permission request; `session` makes it a
    /// standing grant for identical calls.
    Decide {
        thread: Ulid,
        call_id: String,
        allow: bool,
        #[serde(default)]
        session: bool,
    },
    AnswerHuman {
        thread: Ulid,
        call_id: String,
        text: String,
    },
    Pin {
        thread: Ulid,
        text: String,
    },
    Compact {
        thread: Ulid,
    },
    SetMode {
        thread: Ulid,
        mode: String,
    },
    Report {
        thread: Ulid,
        report: ReportKind,
    },
}

/// The text reports the REPL prints, rendered by the daemon so every
/// client shows the same thing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportKind {
    Cost,
    Project,
    Policy,
    Memory,
    Skills,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    Welcome(Welcome),
    Projects {
        projects: Vec<ProjectInfo>,
    },
    Threads {
        threads: Vec<ThreadInfo>,
    },
    Thread {
        thread: ThreadInfo,
    },
    /// The reply to `Open`: the state now and the events since `from_seq`.
    Opened {
        state: ThreadState,
        events: Vec<Event>,
    },
    Ok,
    /// A rendered report, for the client to print as is.
    Text {
        text: String,
    },
    /// A role or a rule said no; nothing was appended.
    Refused {
        reason: String,
    },
    /// The daemon could not do it; the log may or may not have changed
    /// and the message says.
    Error {
        message: String,
    },
}

/// The reply to `Hello`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Welcome {
    pub user: String,
    /// The projects this user has a role in.
    pub projects: Vec<ProjectInfo>,
    /// The daemon's version string, for the banner.
    pub server: String,
}

/// Pushed to every subscriber of a thread, in order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Notice {
    Event {
        thread: Ulid,
        event: Event,
    },
    /// Streamed assistant text; not an event. The assistant message
    /// arrives whole as an `Event` when the call ends.
    TextDelta {
        thread: Ulid,
        text: String,
    },
    ToolCallStarted {
        thread: Ulid,
        call: ToolCall,
    },
    State {
        thread: Ulid,
        state: ThreadState,
    },
}

/// Where a thread is, for the state line and the listings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ThreadState {
    Idle,
    Running {
        by: Author,
        queued: u32,
    },
    /// A tool call waits for someone with `approve`.
    AwaitingApproval {
        call_id: String,
        call: ToolCall,
        class: RiskClass,
        reason: String,
    },
    /// `ask_human` waits for someone with `write`.
    AwaitingHuman {
        call_id: String,
        question: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectInfo {
    pub name: String,
    pub root: PathBuf,
    /// The asking user's role: `read`, `write`, `approve` or `admin`;
    /// `None` when they have none. A string on the wire so this crate
    /// stays free of the policy crate.
    pub role: Option<String>,
    pub threads: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThreadInfo {
    pub id: Ulid,
    pub project: Option<String>,
    /// `YYYY-MM-DD` of the first event.
    pub date: String,
    pub events: u64,
    pub first_line: String,
    pub state: ThreadState,
}

/// A line that could not be read.
#[derive(Debug, thiserror::Error)]
#[error("cannot read frame (this side speaks protocol {PROTOCOL_VERSION}): {message}")]
pub struct DecodeError {
    pub message: String,
}

/// One frame as one line, newline included. `serde_json` escapes every
/// newline inside a string, so the line is the frame.
pub fn encode(frame: &Frame) -> String {
    let mut line = serde_json::to_string(frame).expect("wire types serialise");
    debug_assert!(!line.contains('\n'));
    line.push('\n');
    line
}

/// One line back to a frame. A frame from a newer protocol with a kind
/// this side does not know fails here, naming the version this side
/// speaks.
pub fn decode(line: &str) -> Result<Frame, DecodeError> {
    serde_json::from_str(line.trim_end_matches(['\n', '\r'])).map_err(|e| DecodeError {
        message: e.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aigentic_core::{AgentId, EventKind, UserId};
    use serde_json::json;

    fn steve() -> Author {
        Author::User(UserId("steve".into()))
    }

    fn call() -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            args: json!({"command": "rm -rf x"}),
        }
    }

    fn event() -> Event {
        Event {
            id: Ulid::from_parts(1_700_000_000_000, 7),
            thread_id: Ulid::from_parts(1_700_000_000_000, 1),
            seq: 3,
            kind: EventKind::TurnEnded,
            author: Author::Agent(AgentId("assistant".into())),
            payload: json!({"reason": "done"}),
            parent_event: None,
            created_at: time::macros::datetime!(2026-09-22 12:00:00 UTC),
        }
    }

    fn thread() -> Ulid {
        Ulid::from_parts(1_700_000_000_000, 1)
    }

    fn every_request() -> Vec<Request> {
        vec![
            Request::Hello {
                protocol: PROTOCOL_VERSION,
                token: "t".into(),
            },
            Request::ListProjects,
            Request::ListThreads {
                project: "p".into(),
            },
            Request::CreateThread {
                project: "p".into(),
            },
            Request::Open {
                thread: thread(),
                from_seq: 4,
            },
            Request::Close { thread: thread() },
            Request::Post {
                thread: thread(),
                blocks: vec![ContentBlock::Text("hi".into())],
                interrupt: true,
            },
            Request::InvokeSkill {
                thread: thread(),
                name: "tdd".into(),
                args: "".into(),
            },
            Request::Decide {
                thread: thread(),
                call_id: "c1".into(),
                allow: true,
                session: true,
            },
            Request::AnswerHuman {
                thread: thread(),
                call_id: "c2".into(),
                text: "yes".into(),
            },
            Request::Pin {
                thread: thread(),
                text: "Use Swedish.".into(),
            },
            Request::Compact { thread: thread() },
            Request::SetMode {
                thread: thread(),
                mode: "auto".into(),
            },
            Request::Report {
                thread: thread(),
                report: ReportKind::Policy,
            },
        ]
    }

    fn every_response() -> Vec<Response> {
        let info = ProjectInfo {
            name: "p".into(),
            root: PathBuf::from("/srv/p"),
            role: Some("approve".into()),
            threads: 2,
        };
        let thread_info = ThreadInfo {
            id: thread(),
            project: Some("p".into()),
            date: "2026-09-22".into(),
            events: 9,
            first_line: "What's next?".into(),
            state: ThreadState::Idle,
        };
        vec![
            Response::Welcome(Welcome {
                user: "steve".into(),
                projects: vec![info.clone()],
                server: "aigentic 0.1.0".into(),
            }),
            Response::Projects {
                projects: vec![info],
            },
            Response::Threads {
                threads: vec![thread_info.clone()],
            },
            Response::Thread {
                thread: thread_info,
            },
            Response::Opened {
                state: ThreadState::Running {
                    by: steve(),
                    queued: 1,
                },
                events: vec![event()],
            },
            Response::Ok,
            Response::Text {
                text: "policy rules, first match wins".into(),
            },
            Response::Refused {
                reason: "read may not post".into(),
            },
            Response::Error {
                message: "log unwritable".into(),
            },
        ]
    }

    fn every_notice() -> Vec<Notice> {
        vec![
            Notice::Event {
                thread: thread(),
                event: event(),
            },
            Notice::TextDelta {
                thread: thread(),
                text: "hel\nlo".into(),
            },
            Notice::ToolCallStarted {
                thread: thread(),
                call: call(),
            },
            Notice::State {
                thread: thread(),
                state: ThreadState::AwaitingApproval {
                    call_id: "c1".into(),
                    call: call(),
                    class: RiskClass::Exec,
                    reason: "class exec: ask".into(),
                },
            },
            Notice::State {
                thread: thread(),
                state: ThreadState::AwaitingHuman {
                    call_id: "c2".into(),
                    question: "Ship it?".into(),
                },
            },
        ]
    }

    #[test]
    fn every_frame_round_trips_on_one_line() {
        let mut frames: Vec<Frame> = every_request()
            .into_iter()
            .enumerate()
            .map(|(i, r)| Frame::request(i as u64, r))
            .collect();
        frames.extend(
            every_response()
                .into_iter()
                .enumerate()
                .map(|(i, r)| Frame::response(i as u64, r)),
        );
        frames.extend(every_notice().into_iter().map(Frame::notice));
        for frame in frames {
            let line = encode(&frame);
            assert!(line.ends_with('\n'));
            assert_eq!(line.matches('\n').count(), 1, "{line}");
            assert_eq!(decode(&line).unwrap(), frame, "{line}");
        }
    }

    #[test]
    fn the_wire_shape_is_tagged_and_ids_are_optional() {
        let line = encode(&Frame::request(
            1,
            Request::Open {
                thread: thread(),
                from_seq: 0,
            },
        ));
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["id"], 1);
        assert_eq!(v["request"]["kind"], "open");
        assert_eq!(v["request"]["from_seq"], 0);
        let line = encode(&Frame::notice(Notice::State {
            thread: thread(),
            state: ThreadState::Idle,
        }));
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert!(v.get("id").is_none());
        assert_eq!(v["notice"]["kind"], "state");
        assert_eq!(v["notice"]["state"]["state"], "idle");
        // Defaults: a post without the flag is not an interrupt.
        let f = decode(r#"{"id":2,"request":{"kind":"post","thread":"01ARZ3NDEKTSV4RRFFQ69G5FAV","blocks":[{"type":"text","text":"x"}]}}"#).unwrap();
        assert!(matches!(
            f.body,
            Body::Request(Request::Post {
                interrupt: false,
                ..
            })
        ));
    }

    #[test]
    fn an_unknown_kind_fails_naming_the_protocol() {
        let err = decode(r#"{"id":1,"request":{"kind":"teleport","thread":"x"}}"#).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("protocol 1"), "{text}");
        assert!(text.contains("teleport"), "{text}");
        assert!(decode("not json").is_err());
        assert!(decode("").is_err());
    }
}
