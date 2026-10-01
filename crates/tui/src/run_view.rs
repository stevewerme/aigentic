//! The run view (issue #68): one line per lead event, shared by
//! `aigentic build <n>` (issue #58) and the REPL's `/build <n>`.
//!
//! [`render`] is the only source of a run cell's text: whatever the two
//! callers show, they show from here, so a lead reads the same in a
//! shell and inside the REPL.
//!
//! [`fetch_backlog`] reads a lead's log from the start and does nothing
//! else: it prints nothing and answers nothing. The callers want
//! different things from a backlog a gate is parked at — `build`
//! answers it `stop` at once, the REPL shows the prompt and waits for a
//! person — so the decision stays with them.

use aigentic_api::client::Client;
use aigentic_api::{Request, Response};
use aigentic_runtime::aigentic_core::{Event, EventKind};
use aigentic_runtime::aigentic_log::{
    CheckResult, CheckpointAnsweredPayload, CheckpointAskedPayload, ChecksRunPayload,
    PushedPayload, RouteTakenPayload, RunFinishedPayload, RunOutcome, StepFinishedPayload,
    StepStartedPayload,
};
use anyhow::{Context, bail};
use ulid::Ulid;

/// The daemon refused to open a lead's log, with its reason.
///
/// A refusal is not a transport failure: `aigentic build` prints the
/// reason as a note and goes on following the live stream, and the REPL
/// says it and keeps following too. Carried as its own type so a caller
/// can tell the two apart without reading an error's text.
#[derive(Debug)]
pub(crate) struct Refused(pub String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for Refused {}

/// Read the lead's log from its first event and hand back the events,
/// printing nothing and answering nothing.
///
/// `Open` on a lead is the run's read-only path: no actor is started,
/// and the events come from the log.
pub(crate) async fn fetch_backlog(client: &Client, lead: Ulid) -> anyhow::Result<Vec<Event>> {
    let opened = client
        .request(Request::Open {
            thread: lead,
            from_seq: 0,
        })
        .await
        .context("reading the lead's backlog")?;
    match opened {
        Response::Opened { events, .. } => Ok(events),
        Response::Refused { reason } => Err(anyhow::Error::new(Refused(reason))),
        other => bail!("unexpected reply to opening the lead: {other:?}"),
    }
}

/// The outcome as one lowercase word.
pub(crate) fn outcome_line(outcome: &RunOutcome) -> String {
    match outcome {
        RunOutcome::Closed => "closed".into(),
        RunOutcome::Stopped => "stopped (a human decided)".into(),
        RunOutcome::Escalated => "escalated (a human is needed)".into(),
    }
}

/// One line for a lead event, or `None` for an event with nothing to say.
///
/// The expected strings are checked by `build_cmd`'s tests, which build
/// them from the same payloads.
pub(crate) fn render(event: &Event) -> Option<String> {
    match event.kind {
        EventKind::StepStarted => {
            let payload: StepStartedPayload = serde_json::from_value(event.payload.clone()).ok()?;
            Some(format!(
                "step {} attempt {} → child {}",
                payload.step, payload.attempt, payload.child_thread
            ))
        }
        EventKind::StepFinished => {
            let payload: StepFinishedPayload =
                serde_json::from_value(event.payload.clone()).ok()?;
            Some(format!(
                "step {} finished: {} ({}) · ${:.2}",
                payload.step,
                step_status(&payload.status),
                payload.end_reason,
                payload.cost_usd
            ))
        }
        EventKind::ChecksRun => {
            let payload: ChecksRunPayload = serde_json::from_value(event.payload.clone()).ok()?;
            let passed = payload
                .checks
                .iter()
                .filter(|check| check.result == CheckResult::Pass)
                .count();
            let mut lines = vec![format!(
                "checks {}: {}/{} passed",
                payload.step,
                passed,
                payload.checks.len()
            )];
            for check in &payload.checks {
                lines.push(format!(
                    "  check {}: {} — {}",
                    check.id,
                    check_result(&check.result),
                    check.detail.as_deref().unwrap_or_default()
                ));
            }
            Some(lines.join("\n"))
        }
        EventKind::RouteTaken => {
            let payload: RouteTakenPayload = serde_json::from_value(event.payload.clone()).ok()?;
            let budget = payload
                .budget_usd
                .map(|b| format!(" · budget ${b:.2}"))
                .unwrap_or_default();
            Some(format!(
                "route on {} proposed {} taken {}{budget}",
                payload.branch, payload.proposed, payload.taken
            ))
        }
        EventKind::Pushed => {
            let payload: PushedPayload = serde_json::from_value(event.payload.clone()).ok()?;
            let mut lines = vec![format!(
                "pushed {} → {} ({} commit{})",
                short(&payload.ref_before),
                short(&payload.ref_after),
                payload.commits.len(),
                if payload.commits.len() == 1 { "" } else { "s" }
            )];
            for commit in &payload.commits {
                lines.push(format!("  {} {}", short(&commit.sha), commit.subject));
            }
            Some(lines.join("\n"))
        }
        EventKind::CheckpointAsked => {
            let payload: CheckpointAskedPayload =
                serde_json::from_value(event.payload.clone()).ok()?;
            Some(format!(
                "checkpoint `{}`: {}{}",
                payload.gate,
                payload.shown.join("; "),
                if payload.options.is_empty() {
                    String::new()
                } else {
                    format!(" (answer: {})", payload.options.join(", "))
                }
            ))
        }
        EventKind::CheckpointAnswered => {
            let payload: CheckpointAnsweredPayload =
                serde_json::from_value(event.payload.clone()).ok()?;
            Some(format!(
                "checkpoint answered {}{}",
                answer_word(&payload.answer),
                match &payload.amendment {
                    Some(a) => format!(": {a}"),
                    None => String::new(),
                }
            ))
        }
        EventKind::RunFinished => {
            let payload: RunFinishedPayload = serde_json::from_value(event.payload.clone()).ok()?;
            Some(format!("run finished: {}", outcome_line(&payload.outcome)))
        }
        _ => None,
    }
}

/// The step's status, as the line spells it.
pub(crate) fn step_status(status: &aigentic_runtime::aigentic_log::StepStatus) -> String {
    use aigentic_runtime::aigentic_log::StepStatus;
    match status {
        StepStatus::Done => "done".into(),
        StepStatus::Partial => "partial".into(),
        StepStatus::Failed => "failed".into(),
    }
}

/// The answer, as the line spells it. The log's answer and the wire's are
/// two types with the same words (the wire mirrors the log), so this takes
/// the log's and prints the word they share.
pub(crate) fn answer_word(answer: &aigentic_runtime::aigentic_log::CheckpointAnswer) -> String {
    use aigentic_runtime::aigentic_log::CheckpointAnswer as LogAnswer;
    match answer {
        LogAnswer::Go => "go".into(),
        LogAnswer::Amend => "amend".into(),
        LogAnswer::Stop => "stop".into(),
    }
}

/// The check's result, as the line spells it.
pub(crate) fn check_result(result: &CheckResult) -> String {
    match result {
        CheckResult::Pass => "passed".into(),
        CheckResult::Flag => "flagged".into(),
        CheckResult::Fail => "failed".into(),
    }
}

/// A sha, as a person reads it.
pub(crate) fn short(sha: &str) -> String {
    sha.chars().take(8).collect()
}

/// A scripted daemon over a socket, for the REPL's `/build` tests
/// (issue #68): it answers the requests a followed run makes, records
/// them, and pushes the notices a test asks it to.
///
/// After `build_cmd`'s T17: a `UnixListener`, one connection, requests
/// matched to responses. Its own addition is the notice side — a test
/// pushes a lead's live events through `push`, and they arrive on the
/// client's notice stream as `Notice::Event`.
#[cfg(test)]
pub(crate) mod fake_daemon {
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use aigentic_api::client::Addr;
    use aigentic_api::{
        Body, Frame, Notice, ProjectInfo, Request, Response, ThreadState, Welcome, encode,
    };
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;
    use tokio::sync::mpsc;
    use ulid::Ulid;

    use super::Event;

    /// What the daemon answers, and what it holds.
    pub(crate) struct Script {
        /// The lead a `Build` is answered with.
        pub(crate) lead: Ulid,
        /// What `Open` on the lead hands back.
        pub(crate) backlog: Vec<Event>,
        /// Whether `Build` says `resumed`.
        pub(crate) resumed: bool,
        /// The user's role in the project, as `Hello`'s welcome says it.
        pub(crate) role: &'static str,
        /// Refuse `Build` with this reason instead of running.
        pub(crate) refuse_build: Option<String>,
        /// Refuse `Open` on the lead with this reason.
        pub(crate) refuse_open: Option<String>,
    }

    /// A running scripted daemon. Drop it (or let the test end) to stop
    /// its task.
    pub(crate) struct FakeDaemon {
        pub(crate) socket: PathBuf,
        seen: Arc<Mutex<Vec<Request>>>,
        notices: mpsc::UnboundedSender<Notice>,
        handle: tokio::task::JoinHandle<()>,
    }

    impl FakeDaemon {
        /// Listen under `dir`, in a task, and answer per `script`.
        pub(crate) async fn start(dir: &std::path::Path, script: Script) -> Self {
            let socket = dir.join("daemon.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            let lead = script.lead;
            let seen: Arc<Mutex<Vec<Request>>> = Arc::new(Mutex::new(Vec::new()));
            let (notices, mut pushed) = mpsc::unbounded_channel::<Notice>();
            let handle = {
                let seen = Arc::clone(&seen);
                tokio::spawn(async move {
                    let (stream, _) = listener.accept().await.unwrap();
                    let (read, mut write) = stream.into_split();
                    let mut lines = BufReader::new(read).lines();
                    loop {
                        let line = tokio::select! {
                            line = lines.next_line() => line.unwrap(),
                            Some(notice) = pushed.recv() => {
                                let frame = Frame::notice(notice);
                                write
                                    .write_all(format!("{}\n", encode(&frame)).as_bytes())
                                    .await
                                    .unwrap();
                                continue;
                            }
                        };
                        let Some(line) = line else { break };
                        let Ok(frame) = aigentic_api::decode(&line) else {
                            break;
                        };
                        let id = frame.id.unwrap_or(0);
                        let request = match frame.body {
                            Body::Request(request) => request,
                            _ => break,
                        };
                        seen.lock().unwrap().push(request.clone());
                        let response = match &request {
                            Request::Hello { .. } => Response::Welcome(Welcome {
                                user: "steve".into(),
                                projects: vec![ProjectInfo {
                                    name: "proj".into(),
                                    root: PathBuf::from("/tmp/proj"),
                                    role: Some(script.role.into()),
                                    threads: 0,
                                }],
                                server: "fake".into(),
                            }),
                            Request::Build { .. } => match &script.refuse_build {
                                Some(reason) => Response::Refused {
                                    reason: reason.clone(),
                                },
                                None => Response::Run {
                                    lead,
                                    resumed: script.resumed,
                                },
                            },
                            Request::Open { thread, from_seq } => {
                                if *thread == lead && from_seq == &0 {
                                    match &script.refuse_open {
                                        Some(reason) => Response::Refused {
                                            reason: reason.clone(),
                                        },
                                        None => Response::Opened {
                                            state: ThreadState::Idle,
                                            events: script.backlog.clone(),
                                            run: None,
                                            mode: "manual".into(),
                                            profile: None,
                                            model: "unknown".into(),
                                            effort: None,
                                        },
                                    }
                                } else {
                                    // The chat thread: open, and empty.
                                    Response::Opened {
                                        state: ThreadState::Idle,
                                        events: Vec::new(),
                                        run: None,
                                        mode: "manual".into(),
                                        profile: None,
                                        model: "unknown".into(),
                                        effort: None,
                                    }
                                }
                            }
                            // Every answer the REPL sends is accepted.
                            Request::AnswerCheckpoint { .. } => Response::Ok,
                            other => panic!("the tests sent an unscripted request: {other:?}"),
                        };
                        write
                            .write_all(
                                format!("{}\n", encode(&Frame::response(id, response))).as_bytes(),
                            )
                            .await
                            .unwrap();
                    }
                })
            };
            Self {
                socket,
                seen,
                notices,
                handle,
            }
        }

        /// Where the daemon listens.
        pub(crate) fn addr(&self) -> Addr {
            Addr::Unix(self.socket.clone())
        }

        /// Every request the daemon has answered, in order.
        pub(crate) fn requests(&self) -> Vec<Request> {
            self.seen.lock().unwrap().clone()
        }

        /// Push a notice to the client, as a live event would arrive.
        pub(crate) fn push(&self, notice: Notice) {
            self.notices.send(notice).unwrap();
        }

        /// Stop the daemon's task.
        pub(crate) fn stop(self) {
            self.handle.abort();
        }
    }
}
