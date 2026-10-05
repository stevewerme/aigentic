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

/// Sent in `Hello`; a mismatch is refused with both numbers. Version 2
/// adds `Build`/`AnswerCheckpoint` and `Response::Run` (issue #58);
/// version 3 adds `AnswerSwitch` and `ThreadState::AwaitingSwitch`
/// (issue #7), so a version-2 client is refused rather than left never
/// seeing the switch it is asked to answer; version 4 adds
/// `Front`/`NewFront` and `Response::Front` (issue #84).
pub const PROTOCOL_VERSION: u32 = 4;

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
    /// The user's front thread (issue #84): the newest thread they made
    /// with `front` set, or a new one in `project` when they have none
    /// or the old one can't be opened. The handler judges the roles:
    /// `read` in the front thread's project to resume, `write` in the
    /// requested project to create.
    Front {
        project: String,
    },
    /// A new front thread in `project` (issue #84), replacing whatever
    /// was front; the old one stays listed.
    NewFront {
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
    /// Cancel the running turn without posting anything (Ctrl-C, Esc);
    /// refused when no turn runs. `Post` with `interrupt` cancels and
    /// posts in one.
    Interrupt {
        thread: Ulid,
    },
    /// Answer a pending permission request; `session` makes it a
    /// standing grant for identical calls.
    Decide {
        thread: Ulid,
        call_id: String,
        allow: bool,
        #[serde(default)]
        session: bool,
        /// With `allow`: also allow commands starting with these words
        /// from now on, written to the project's rules file (step 7).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prefix: Option<Vec<String>>,
        /// With a deny: why, told to the model in the result.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    AnswerHuman {
        thread: Ulid,
        call_id: String,
        text: String,
    },
    /// Answer a `suggest_project` proposal (issue #7), read from
    /// `ThreadState::AwaitingSwitch`; needs `write` in the thread's
    /// project, and for a `yes` in the target too.
    AnswerSwitch {
        thread: Ulid,
        call_id: String,
        answer: SwitchReply,
    },
    Pin {
        thread: Ulid,
        text: String,
    },
    /// `/remember <text>` (issue #14): file a memory line directly,
    /// no model call.
    Remember {
        thread: Ulid,
        text: String,
    },
    /// Move the thread to another project (phase 6 step 10): needs
    /// `write` in both the thread's project and the target; refused while
    /// a turn runs.
    SwitchProject {
        thread: Ulid,
        project: String,
    },
    /// Set the thread's title (phase 6 step 9); refused while a turn runs.
    Rename {
        thread: Ulid,
        title: String,
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
    /// Start a run for `issue` in `project`, or resume the unfinished one
    /// (issue #58). Refused unless the user has `approve`: a build pushes
    /// to the main branch. `workflow` defaults to the bundled `build`.
    Build {
        project: String,
        issue: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workflow: Option<String>,
    },
    /// Answer the open checkpoint of a run's lead (issue #58). In slice 1
    /// only `stop` is accepted; `go` and `amend` are refused. The lead is
    /// the run's own thread, not a project thread.
    AnswerCheckpoint {
        lead: Ulid,
        gate: String,
        answer: CheckpointAnswer,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        amendment: Option<String>,
    },
}

/// A human's answer at a run's checkpoint (issue #58). Mirrors the log's
/// `CheckpointAnswer` rather than take an edge to `log` (AGENTS.md); the
/// serde shape is identical, so the same word crosses the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointAnswer {
    Go,
    Amend,
    Stop,
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
    /// The project's participants and their roles.
    Who,
    /// The project's working-tree diff, untracked files included, run
    /// by the daemon in the project root (phase 6 step 6).
    Diff,
}

/// The model label a daemon sends when it knows none.
fn unknown_model() -> String {
    "unknown".to_owned()
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
    /// The reply to `Front` (issue #84): the front thread, and how it
    /// was arrived at. `Replaced` names why the old one could not be
    /// opened, in words a banner can print; the old thread stays
    /// listed.
    Front {
        thread: ThreadInfo,
        outcome: FrontOutcome,
    },
    /// The reply to `Build` (issue #58): the run's lead thread.
    /// `resumed` says an unfinished run was picked up rather than a new
    /// one started. The session subscribes this client to the lead.
    Run {
        lead: Ulid,
        resumed: bool,
    },
    /// The reply to `Open`: the state now, the events since `from_seq`,
    /// and the thread's permission mode, with the identity the footer
    /// names (issue #43): the profile, its model label and its
    /// reasoning effort, absent when the daemon does not know them.
    Opened {
        state: ThreadState,
        events: Vec<Event>,
        /// Set when the thread belongs to a run (issue #58): a lead, or
        /// one of its steps' children. Such a thread is served read-only,
        /// because the run alone writes its log.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run: Option<RunThread>,
        #[serde(default)]
        mode: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile: Option<String>,
        /// The model the thread runs on. Always known: a daemon with no
        /// profile sends `unknown`.
        #[serde(default = "unknown_model")]
        model: String,
        /// The profile's reasoning effort, when it names one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
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

/// How a `Front` request was answered (issue #84).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FrontOutcome {
    /// The user's existing front thread, opened where it was.
    Resumed,
    /// The user had none: this is the first one.
    First,
    /// The old front thread could not be opened; this is a new one, and
    /// `reason` says why the old one could not be, in words a banner
    /// can print.
    Replaced { reason: String },
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
    /// The permission mode changed (`SetMode`).
    Mode {
        thread: Ulid,
        mode: String,
    },
    /// The thread's identity changed (`/project`; issue #43). Sent
    /// after the switch, as `Mode` is after `SetMode`; `None` means
    /// the daemon does not know that part.
    Model {
        thread: Ulid,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile: Option<String>,
        /// The model the thread runs on. Always known: a daemon with
        /// no profile sends `unknown`.
        #[serde(default = "unknown_model")]
        model: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
    },
    /// The window fill after a model call, a queue change or a turn's
    /// end (phase 6): what a status line shows. `turn_elapsed_ms` is
    /// `None` when no turn runs.
    Usage {
        thread: Ulid,
        tokens_in_window: u64,
        window: u64,
        turn_elapsed_ms: Option<u64>,
        queued: u32,
    },
    /// A remark for the person that is not an event (a rules file that
    /// could not be written).
    Note {
        thread: Ulid,
        text: String,
    },
}

/// A thread a run owns (issue #58). The daemon serves it read-only:
/// the run is its only writer, whichever client asks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunThread {
    /// A run's lead thread: the issue it builds.
    Lead {
        /// The issue the run was asked for.
        issue: u64,
    },
    /// A step's child thread, under its run's lead.
    Child {
        /// The run's lead.
        lead: Ulid,
        /// The workflow step the child works.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        step: Option<String>,
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
    /// `ask_human` waits for someone with `write`. `question` is the
    /// questions as one plain text, the fallback for a client that does
    /// not know `questions` (an old one, or an old daemon's frame).
    AwaitingHuman {
        call_id: String,
        question: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        questions: Vec<AskedQuestion>,
    },
    /// A `suggest_project` proposal waits for someone with `write`
    /// (issue #7). `workspace` is the daemon's label for the target.
    AwaitingSwitch {
        call_id: String,
        project: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<String>,
        reason: String,
    },
}

/// A person's answer to a switch proposal (issue #7). Mirrors the
/// runtime's `SwitchAnswer` rather than take an edge to it (AGENTS.md).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "answer", rename_all = "snake_case")]
pub enum SwitchReply {
    Yes,
    No,
    /// The person named where the work belongs instead.
    Corrected {
        to: String,
    },
    /// Nobody answered it: `exec`'s decline, or a client giving up.
    Withdrawn {
        note: String,
    },
}

/// One `ask_human` question on the wire; mirrors the runtime's
/// `HumanQuestion` rather than take an edge to it (AGENTS.md).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskedQuestion {
    pub question: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<AskedOption>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub multi: bool,
}

/// One option of an `ask_human` question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskedOption {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

fn is_false(b: &bool) -> bool {
    !*b
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
    /// The last `thread_renamed` title; absent before phase 6.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
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

    /// A thread row, for the replies that carry one.
    fn info() -> ThreadInfo {
        ThreadInfo {
            id: thread(),
            project: Some("p".into()),
            date: "2026-09-22".into(),
            events: 9,
            first_line: "What's next?".into(),
            state: ThreadState::Idle,
            title: None,
        }
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
            Request::Front {
                project: "p".into(),
            },
            Request::NewFront {
                project: "q".into(),
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
                prefix: None,
                reason: None,
            },
            Request::AnswerHuman {
                thread: thread(),
                call_id: "c2".into(),
                text: "yes".into(),
            },
            Request::AnswerSwitch {
                thread: thread(),
                call_id: "c3".into(),
                answer: SwitchReply::Yes,
            },
            Request::AnswerSwitch {
                thread: thread(),
                call_id: "c3".into(),
                answer: SwitchReply::No,
            },
            Request::AnswerSwitch {
                thread: thread(),
                call_id: "c3".into(),
                answer: SwitchReply::Corrected {
                    to: "customer X".into(),
                },
            },
            Request::AnswerSwitch {
                thread: thread(),
                call_id: "c3".into(),
                answer: SwitchReply::Withdrawn {
                    note: "exec declines proposals".into(),
                },
            },
            Request::Pin {
                thread: thread(),
                text: "Use Swedish.".into(),
            },
            Request::Remember {
                thread: thread(),
                text: "We deploy from main only.".into(),
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
            Request::Build {
                project: "p".into(),
                issue: 58,
                workflow: Some("build".into()),
            },
            Request::AnswerCheckpoint {
                lead: thread(),
                gate: "brief".into(),
                answer: CheckpointAnswer::Stop,
                amendment: None,
            },
        ]
    }

    fn every_response() -> Vec<Response> {
        let project_info = ProjectInfo {
            name: "p".into(),
            root: PathBuf::from("/srv/p"),
            role: Some("approve".into()),
            threads: 2,
        };
        let thread_info = info();
        vec![
            Response::Welcome(Welcome {
                user: "steve".into(),
                projects: vec![project_info.clone()],
                server: "aigentic 0.1.0".into(),
            }),
            Response::Projects {
                projects: vec![project_info],
            },
            Response::Threads {
                threads: vec![thread_info.clone()],
            },
            Response::Thread {
                thread: thread_info.clone(),
            },
            Response::Front {
                thread: thread_info.clone(),
                outcome: FrontOutcome::Resumed,
            },
            Response::Front {
                thread: thread_info.clone(),
                outcome: FrontOutcome::First,
            },
            Response::Front {
                thread: thread_info,
                outcome: FrontOutcome::Replaced {
                    reason: "you no longer have a role in p".into(),
                },
            },
            Response::Run {
                lead: thread(),
                resumed: true,
            },
            Response::Opened {
                state: ThreadState::Running {
                    by: steve(),
                    queued: 1,
                },
                events: vec![event()],
                run: Some(RunThread::Lead { issue: 58 }),
                mode: "manual".into(),
                profile: Some("flash".into()),
                model: "deepseek-v4.1-flash".into(),
                effort: Some("50".into()),
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
                    question: "Ship it? When?".into(),
                    questions: vec![AskedQuestion {
                        question: "Ship it?".into(),
                        header: Some("ship".into()),
                        options: vec![AskedOption {
                            label: "Yes".into(),
                            description: Some("merge and tag".into()),
                        }],
                        multi: false,
                    }],
                },
            },
            Notice::State {
                thread: thread(),
                state: ThreadState::AwaitingSwitch {
                    call_id: "c3".into(),
                    project: "aigentic-web".into(),
                    workspace: Some("~/aigentic-web".into()),
                    reason: "the message is about the site".into(),
                },
            },
            Notice::Mode {
                thread: thread(),
                mode: "auto".into(),
            },
            Notice::Model {
                thread: thread(),
                profile: Some("flash".into()),
                model: "deepseek-v4.1-flash".into(),
                effort: Some("50".into()),
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

    /// T2 (issue #84): the front thread's requests and reply are on the
    /// wire at protocol 4, and the outcome is tagged by kind.
    #[test]
    fn the_front_requests_and_their_reply_are_at_protocol_four() {
        assert_eq!(PROTOCOL_VERSION, 4);
        let line = encode(&Frame::request(
            1,
            Request::Front {
                project: "p".into(),
            },
        ));
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["request"]["kind"], "front");
        assert_eq!(v["request"]["project"], "p");

        let line = encode(&Frame::request(
            2,
            Request::NewFront {
                project: "p".into(),
            },
        ));
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["request"]["kind"], "new_front");

        // The three outcomes, each through the codec, tagged by kind.
        for (outcome, kind) in [
            (FrontOutcome::Resumed, "resumed"),
            (FrontOutcome::First, "first"),
            (
                FrontOutcome::Replaced {
                    reason: "gone".into(),
                },
                "replaced",
            ),
        ] {
            let frame = Frame::response(
                3,
                Response::Front {
                    thread: info(),
                    outcome: outcome.clone(),
                },
            );
            let line = encode(&frame);
            assert_eq!(decode(&line).unwrap(), frame, "{line}");
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(v["response"]["kind"], "front");
            assert_eq!(v["response"]["outcome"]["kind"], kind);
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

    /// An `Opened` frame from a daemon that predates the identity
    /// (issue #43) decodes with the three fields absent, and this
    /// client then shows the footer it always did.
    #[test]
    fn an_opened_frame_without_the_identity_decodes_to_none() {
        let line = encode(&Frame::response(
            1,
            Response::Opened {
                state: ThreadState::Idle,
                events: vec![],
                run: None,
                mode: "auto".into(),
                profile: Some("flash".into()),
                model: "deepseek-v4.1-flash".into(),
                effort: Some("50".into()),
            },
        ));
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["response"]["profile"], "flash");
        assert_eq!(v["response"]["model"], "deepseek-v4.1-flash");
        assert_eq!(v["response"]["effort"], "50");
        let old =
            r#"{"response":{"kind":"opened","state":{"state":"idle"},"events":[],"mode":"auto"}}"#;
        let f = decode(old).unwrap();
        match f.body {
            Body::Response(Response::Opened {
                profile,
                model,
                effort,
                run,
                ..
            }) => {
                assert!(profile.is_none());
                assert_eq!(model, "unknown", "a daemon that sends no model");
                assert!(effort.is_none());
                assert!(run.is_none(), "a daemon that sends no run");
            }
            other => panic!("wrong body: {other:?}"),
        }
        // Nothing to name means nothing on the wire.
        let line = encode(&Frame::response(
            1,
            Response::Opened {
                state: ThreadState::Idle,
                events: vec![],
                run: None,
                mode: "auto".into(),
                profile: None,
                model: "unknown".into(),
                effort: None,
            },
        ));
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert!(v["response"].get("profile").is_none());
        assert!(v["response"].get("effort").is_none());
        // The model is always on the wire, even when it is the stand-in.
        assert_eq!(v["response"]["model"], "unknown");
    }

    /// A waiting human's questions ride the state; a daemon that does
    /// not know them yet sends no field, and an old frame parses with
    /// the plain question as the fallback.
    #[test]
    fn a_waiting_humans_questions_default_to_none() {
        let line = encode(&Frame::notice(Notice::State {
            thread: thread(),
            state: ThreadState::AwaitingHuman {
                call_id: "c2".into(),
                question: "Ship it?".into(),
                questions: vec![AskedQuestion {
                    question: "Ship it?".into(),
                    header: Some("ship".into()),
                    options: vec![AskedOption {
                        label: "Yes".into(),
                        description: Some("merge and tag".into()),
                    }],
                    multi: false,
                }],
            },
        }));
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["notice"]["state"]["state"], "awaiting_human");
        assert_eq!(v["notice"]["state"]["questions"][0]["question"], "Ship it?");
        assert_eq!(v["notice"]["state"]["questions"][0]["header"], "ship");
        assert_eq!(
            v["notice"]["state"]["questions"][0]["options"][0]["label"],
            "Yes"
        );
        assert!(v["notice"]["state"]["questions"][0].get("multi").is_none());
        let old = r#"{"notice":{"kind":"state","thread":"01ARZ3NDEKTSV4RRFFQ69G5FAV","state":{"state":"awaiting_human","call_id":"c2","question":"Ship it?"}}}"#;
        let f = decode(old).unwrap();
        match f.body {
            Body::Notice(Notice::State {
                state: ThreadState::AwaitingHuman { questions, .. },
                ..
            }) => assert!(questions.is_empty()),
            other => panic!("wrong body: {other:?}"),
        }
    }

    #[test]
    fn an_unknown_kind_fails_naming_the_protocol() {
        let err = decode(r#"{"id":1,"request":{"kind":"teleport","thread":"x"}}"#).unwrap_err();
        let text = err.to_string();
        // The message names the version this side speaks, so the literal
        // follows the constant rather than the bump.
        assert!(
            text.contains(&format!("protocol {PROTOCOL_VERSION}")),
            "{text}"
        );
        assert!(text.contains("teleport"), "{text}");
        assert!(decode("not json").is_err());
        assert!(decode("").is_err());
    }
}
