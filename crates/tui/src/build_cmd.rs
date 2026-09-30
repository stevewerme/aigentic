//! `aigentic build <n>` (issue #58, slice 1's acceptance): start — or
//! resume — the build workflow for an issue, and follow it, with nothing
//! pasted.
//!
//! One line per lead event goes to stderr: the step and its attempt, the
//! checks, the commits, the gate and the outcome. With `--json` every
//! notice is one JSON line on stdout instead.
//!
//! A checkpoint is answered `stop` and never `go`: a `go` would start work
//! nothing has briefed. Exit 0 for a run that closed, 3 for one that
//! stopped (a human is needed: slice 2's prompts), 1 for one a runner
//! error stopped. Ctrl-C detaches; the run stays resumable.

use std::io::Write;

use aigentic_api::client::Client;
use aigentic_api::{CheckpointAnswer, Notice, Request, Response};
use aigentic_runtime::aigentic_core::EventKind;
use aigentic_runtime::aigentic_log::{
    CheckResult, CheckpointAnsweredPayload, CheckpointAskedPayload, ChecksRunPayload,
    PushedPayload, RouteTakenPayload, RunFinishedPayload, RunOutcome, StepFinishedPayload,
    StepStartedPayload,
};
use anyhow::{Context, bail};
use tokio::sync::mpsc;
use ulid::Ulid;

/// The run finished `Closed`: the issue is done.
pub const EXIT_OK: i32 = 0;
/// The run stopped, or a runner error ended it: a human is needed.
pub const EXIT_NEEDS_HUMAN: i32 = 3;
/// The request itself failed.
pub const EXIT_FAILED: i32 = 1;

#[derive(Debug, Clone)]
pub struct BuildArgs {
    /// The issue number.
    pub issue: u64,
    /// The workflow to run; `None` is the daemon's default (`build`).
    pub workflow: Option<String>,
    /// Every notice as a JSON line on stdout.
    pub json: bool,
}

/// What the run came to; `code` is the process's exit code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildOutcome {
    pub code: i32,
    pub lead: Ulid,
    pub resumed: bool,
    pub outcome: Option<RunOutcome>,
    pub reason: Option<String>,
}

/// Start or resume the run for `args.issue` and follow it to its end.
/// `out` receives the JSON lines (or nothing), `err` the progress.
pub async fn run(
    client: &Client,
    mut notices: mpsc::Receiver<Notice>,
    project: &str,
    by: &str,
    args: &BuildArgs,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> anyhow::Result<BuildOutcome> {
    let replied = client
        .request(Request::Build {
            project: project.to_owned(),
            issue: args.issue,
            workflow: args.workflow.clone(),
        })
        .await
        .context("asking the daemon to build")?;
    let (lead, resumed) = match replied {
        Response::Run { lead, resumed } => (lead, resumed),
        Response::Refused { reason } => bail!("the daemon refused to build: {reason}"),
        other => bail!("unexpected reply to build: {other:?}"),
    };
    writeln!(
        err,
        "run {lead} for issue #{} ({})",
        args.issue,
        if resumed { "resumed" } else { "started" }
    )?;

    let mut outcome = BuildOutcome {
        code: EXIT_OK,
        lead,
        resumed,
        outcome: None,
        reason: None,
    };
    // The lead belongs to a run, and the daemon serves it from its log
    // (a `Build` on a resumed run subscribes to events written from now
    // on; fix 4 of #58). Read the log first, so a resumed run shows what
    // it did, and answer a gate that is already open instead of waiting
    // for a live event that will never come.
    let mut printed = backlog(client, lead, err).await?;
    loop {
        let Some(notice) = notices.recv().await else {
            // The session ended: the run is still resumable, and the
            // person watching was told which lead it is.
            bail!("the daemon's session ended before the run did");
        };
        if args.json {
            writeln!(out, "{}", serde_json::to_string(&notice)?)?;
            out.flush()?;
        }
        match notice {
            Notice::Event { thread, event } if thread == lead => {
                // The backlog already printed this one; a subscription
                // may repeat the boundary event.
                if event.seq <= printed {
                    continue;
                }
                printed = event.seq;
                let line = render(&event);
                if let Some(line) = line {
                    writeln!(err, "{line}")?;
                    err.flush()?;
                }
                // A gate is answered `stop`: `go` starts work nothing has
                // briefed (slice 2's prompts do that).
                if event.kind == EventKind::CheckpointAsked {
                    let asked: Option<CheckpointAskedPayload> =
                        serde_json::from_value(event.payload.clone()).ok();
                    let gate = asked.map(|a| a.gate).unwrap_or_default();
                    answer_stop(client, lead, &gate, err).await?;
                }
                if event.kind == EventKind::RunFinished {
                    let finished: RunFinishedPayload =
                        serde_json::from_value(event.payload.clone())
                            .context("reading the run's outcome")?;
                    writeln!(
                        err,
                        "run {lead} finished: {}",
                        outcome_line(&finished.outcome)
                    )?;
                    outcome.outcome = Some(finished.outcome);
                    outcome.code = match finished.outcome {
                        RunOutcome::Closed => EXIT_OK,
                        // Stopped and Escalated both leave the run for a
                        // person: slice 2's prompts answer them.
                        RunOutcome::Stopped | RunOutcome::Escalated => EXIT_NEEDS_HUMAN,
                    };
                    return Ok(outcome);
                }
            }
            Notice::Note { thread, text } if thread == lead => {
                writeln!(err, "note: {text}")?;
                err.flush()?;
                if text.starts_with("run stopped") {
                    outcome.reason = Some(text);
                    outcome.code = EXIT_FAILED;
                    return Ok(outcome);
                }
            }
            _ => {}
        }
        let _ = by;
    }
}

/// Print the lead's backlog, and answer a gate it is already waiting at.
///
/// `Open` on a lead is the run's read-only path (rule 5a): no actor is
/// started, and the events come from the log. Returns the highest seq
/// printed, so the live subscription can skip what was already shown.
///
/// The lead's last event being `checkpoint_asked` means the run is
/// parked on that gate with nobody to answer it: the live loop would
/// wait for a `checkpoint_asked` that was written before this command
/// connected, and never answer it (#58 fix 4). So it is answered here,
/// as a live one is.
async fn backlog(client: &Client, lead: Ulid, err: &mut dyn Write) -> anyhow::Result<u64> {
    let opened = client
        .request(Request::Open {
            thread: lead,
            from_seq: 0,
        })
        .await
        .context("reading the lead's backlog")?;
    let events = match opened {
        Response::Opened { events, .. } => events,
        Response::Refused { reason } => {
            // Nothing appended and nothing to answer: the daemon said no,
            // and the live loop still follows the run.
            writeln!(err, "note: no backlog: {reason}")?;
            err.flush()?;
            return Ok(0);
        }
        other => bail!("unexpected reply to opening the lead: {other:?}"),
    };
    let mut printed = 0;
    for event in &events {
        if let Some(line) = render(event) {
            writeln!(err, "{line}")?;
        }
        printed = event.seq;
    }
    err.flush()?;
    if let Some(last) = events.last()
        && last.kind == EventKind::CheckpointAsked
    {
        let asked: Option<CheckpointAskedPayload> =
            serde_json::from_value(last.payload.clone()).ok();
        let gate = asked.map(|a| a.gate).unwrap_or_default();
        answer_stop(client, lead, &gate, err).await?;
    }
    Ok(printed)
}

/// Answer one gate `stop` — this command never answers `go`.
async fn answer_stop(
    client: &Client,
    lead: Ulid,
    gate: &str,
    err: &mut dyn Write,
) -> anyhow::Result<()> {
    let answered = client
        .request(Request::AnswerCheckpoint {
            lead,
            gate: gate.to_owned(),
            answer: CheckpointAnswer::Stop,
            amendment: None,
        })
        .await
        .context("answering the checkpoint")?;
    match answered {
        Response::Ok => writeln!(
            err,
            "answered gate `{gate}` stop: this command never answers go"
        )?,
        Response::Refused { reason } => writeln!(err, "could not answer gate `{gate}`: {reason}")?,
        other => writeln!(err, "could not answer gate `{gate}`: {other:?}")?,
    }
    err.flush()?;
    Ok(())
}

/// The outcome as one lowercase word.
fn outcome_line(outcome: &RunOutcome) -> String {
    match outcome {
        RunOutcome::Closed => "closed".into(),
        RunOutcome::Stopped => "stopped (a human decided)".into(),
        RunOutcome::Escalated => "escalated (a human is needed)".into(),
    }
}

/// One line for a lead event, or `None` for an event with nothing to say.
///
/// The expected strings are checked by the tests below, which build them
/// from the same payloads.
fn render(event: &aigentic_runtime::aigentic_core::Event) -> Option<String> {
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
fn step_status(status: &aigentic_runtime::aigentic_log::StepStatus) -> String {
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
fn answer_word(answer: &aigentic_runtime::aigentic_log::CheckpointAnswer) -> String {
    use aigentic_runtime::aigentic_log::CheckpointAnswer as LogAnswer;
    match answer {
        LogAnswer::Go => "go".into(),
        LogAnswer::Amend => "amend".into(),
        LogAnswer::Stop => "stop".into(),
    }
}

fn check_result(result: &CheckResult) -> String {
    match result {
        CheckResult::Pass => "passed".into(),
        CheckResult::Flag => "flagged".into(),
        CheckResult::Fail => "failed".into(),
    }
}

/// A sha, as a person reads it.
fn short(sha: &str) -> String {
    sha.chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use aigentic_runtime::aigentic_core::Event;

    use super::*;

    /// One event, as the log stores it.
    fn event(kind: EventKind, payload: serde_json::Value) -> Event {
        serde_json::from_value(serde_json::json!({
            "id": Ulid::generate(),
            "thread_id": Ulid::generate(),
            "seq": 0,
            "created_at": "2026-09-23T10:00:00Z",
            "kind": kind,
            "author": {"kind": "agent", "id": "runner"},
            "payload": payload,
            "parent_event": null,
        }))
        .expect("the fixture event reads")
    }

    /// T10 — each lead event kind renders its line: the expected strings
    /// are built from the fixture payloads, not hand-written.
    #[test]
    fn t10_each_event_kind_renders_its_line() {
        let child = Ulid::generate();
        let started = event(
            EventKind::StepStarted,
            serde_json::json!({
                "step": "implement-alone",
                "role": "implementer",
                "profile": "flash",
                "child_thread": child,
                "attempt": 2,
                "budget_usd": 3.0,
            }),
        );
        let payload: StepStartedPayload = serde_json::from_value(started.payload.clone()).unwrap();
        assert_eq!(
            render(&started).unwrap(),
            format!(
                "step {} attempt {} → child {}",
                payload.step, payload.attempt, payload.child_thread
            )
        );

        let finished = event(
            EventKind::StepFinished,
            serde_json::json!({
                "step": "implement-alone",
                "status": "done",
                "end_reason": "done",
                "cost_usd": 0.41,
            }),
        );
        let payload: StepFinishedPayload =
            serde_json::from_value(finished.payload.clone()).unwrap();
        let line = render(&finished).unwrap();
        assert_eq!(
            line,
            format!(
                "step {} finished: {} ({}) · ${:.2}",
                payload.step,
                step_status(&payload.status),
                payload.end_reason,
                payload.cost_usd
            ),
            "the status, the end reason and the cost are the payload's"
        );

        let checks = event(
            EventKind::ChecksRun,
            serde_json::json!({
                "step": "implement-alone",
                "checks": [
                    {"id": "E1", "result": "pass", "detail": "the trailer is there"},
                    {"id": "E2", "result": "flag", "detail": null},
                ],
            }),
        );
        let payload: ChecksRunPayload = serde_json::from_value(checks.payload.clone()).unwrap();
        let line = render(&checks).unwrap();
        assert_eq!(
            line.lines().next().unwrap(),
            format!(
                "checks {}: {}/{} passed",
                payload.step,
                1,
                payload.checks.len()
            )
        );
        for check in &payload.checks {
            assert!(
                line.contains(&check.id) && line.contains(&check_result(&check.result)),
                "every check's id and result is on a line: {line}"
            );
        }

        let route = event(
            EventKind::RouteTaken,
            serde_json::json!({
                "branch": "main",
                "proposed": "full",
                "taken": "ask",
                "budget_usd": 4.0,
            }),
        );
        let payload: RouteTakenPayload = serde_json::from_value(route.payload.clone()).unwrap();
        assert_eq!(
            render(&route).unwrap(),
            format!(
                "route on {} proposed {} taken {} · budget ${:.2}",
                payload.branch,
                payload.proposed,
                payload.taken,
                payload.budget_usd.unwrap()
            )
        );

        let pushed = event(
            EventKind::Pushed,
            serde_json::json!({
                "commits": [
                    {"sha": "1111111111111111111111111111111111111111", "subject": "pancake: one"},
                ],
                "ref_before": "2222222222222222222222222222222222222222",
                "ref_after": "3333333333333333333333333333333333333333",
            }),
        );
        let payload: PushedPayload = serde_json::from_value(pushed.payload.clone()).unwrap();
        let line = render(&pushed).unwrap();
        assert_eq!(
            line.lines().next().unwrap(),
            format!(
                "pushed {} → {} ({} commit)",
                short(&payload.ref_before),
                short(&payload.ref_after),
                payload.commits.len()
            )
        );
        for commit in &payload.commits {
            assert!(
                line.contains(&short(&commit.sha)) && line.contains(&commit.subject),
                "every commit's sha and subject is on a line: {line}"
            );
        }

        let asked = event(
            EventKind::CheckpointAsked,
            serde_json::json!({
                "gate": "route",
                "shown": ["size: full", "budget: 4"],
                "options": ["go", "amend", "stop"],
            }),
        );
        let payload: CheckpointAskedPayload =
            serde_json::from_value(asked.payload.clone()).unwrap();
        let line = render(&asked).unwrap();
        assert!(line.contains(&format!("checkpoint `{}`", payload.gate)));
        for shown in &payload.shown {
            assert!(line.contains(shown), "each shown line is rendered: {line}");
        }

        let answered = event(
            EventKind::CheckpointAnswered,
            serde_json::json!({
                "answer": "stop",
                "amendment": null,
            }),
        );
        let payload: CheckpointAnsweredPayload =
            serde_json::from_value(answered.payload.clone()).unwrap();
        assert_eq!(
            render(&answered).unwrap(),
            format!("checkpoint answered {}", answer_word(&payload.answer))
        );

        let done = event(
            EventKind::RunFinished,
            serde_json::json!({
                "outcome": "closed",
                "cost_usd": 1.5,
            }),
        );
        let payload: RunFinishedPayload = serde_json::from_value(done.payload.clone()).unwrap();
        assert_eq!(
            render(&done).unwrap(),
            format!("run finished: {}", outcome_line(&payload.outcome))
        );

        // An event the command has nothing to say about renders nothing.
        let other = event(EventKind::TurnEnded, serde_json::json!({"reason": "done"}));
        assert_eq!(render(&other), None);
    }

    /// T11 — `checkpoint_asked` → sends `AnswerCheckpoint { stop }`; the
    /// exit-code mapping is `Closed` → 0, `Stopped`/`Escalated` → 3, an
    /// error (the run stopped) → 1.
    #[test]
    fn t11_stop_is_the_answer_and_the_codes_map() {
        // The answer this command sends is `stop`, never `go`.
        let answer = CheckpointAnswer::Stop;
        assert_eq!(
            serde_json::to_string(&answer).unwrap(),
            "\"stop\"",
            "the word crossing the wire is `stop`"
        );
        assert_ne!(answer, CheckpointAnswer::Go);

        // The exit codes, from the outcome.
        let codes = |outcome: RunOutcome| -> i32 {
            match outcome {
                RunOutcome::Closed => EXIT_OK,
                RunOutcome::Stopped | RunOutcome::Escalated => EXIT_NEEDS_HUMAN,
            }
        };
        assert_eq!(codes(RunOutcome::Closed), 0, "closed maps to 0");
        assert_eq!(codes(RunOutcome::Stopped), 3, "stopped maps to 3");
        assert_eq!(codes(RunOutcome::Escalated), 3, "escalated maps to 3");

        // A run stopped by a runner error: the note this command sees.
        assert_eq!(EXIT_FAILED, 1, "a run stopped by an error maps to 1");
    }

    /// One event with a chosen seq, as the log stores it.
    fn event_at(kind: EventKind, seq: u64, thread: Ulid, payload: serde_json::Value) -> Event {
        serde_json::from_value(serde_json::json!({
            "id": Ulid::generate(),
            "thread_id": thread,
            "seq": seq,
            "created_at": "2026-09-23T10:00:00Z",
            "kind": kind,
            "author": {"kind": "agent", "id": "runner"},
            "payload": payload,
            "parent_event": null,
        }))
        .expect("the fixture event reads")
    }

    /// T17 — a lead already waiting at its gate: `aigentic build <n>`
    /// prints the lead's backlog, answers the open gate `stop` once, and
    /// exits 3 (issue #58 fix 4). A fake daemon over a socket records
    /// what the command asked of it.
    #[tokio::test]
    async fn t17_a_lead_waiting_at_a_gate_is_shown_and_answered() {
        use aigentic_api::{Body, Frame, ThreadState, Welcome, encode};
        use std::sync::{Arc, Mutex};
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixListener;

        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();

        let lead = Ulid::generate();
        // The lead as a resumed command finds it: the work so far, then
        // the gate it is parked on. The daemon wrote `checkpoint_asked`
        // before this command connected, so no live notice will carry it.
        let backlog = vec![
            event_at(
                EventKind::StepStarted,
                0,
                lead,
                serde_json::json!({
                    "step": "implement-alone",
                    "role": "implementer",
                    "profile": "flash",
                    "child_thread": Ulid::generate(),
                    "attempt": 1,
                    "budget_usd": 3.0,
                }),
            ),
            event_at(
                EventKind::ChecksRun,
                1,
                lead,
                serde_json::json!({
                    "step": "implement-alone",
                    "checks": [{"id": "E1", "result": "pass", "detail": "the trailer is there"}],
                }),
            ),
            event_at(
                EventKind::CheckpointAsked,
                2,
                lead,
                serde_json::json!({
                    "gate": "route",
                    "shown": ["size: full"],
                    "options": ["go", "amend", "stop"],
                }),
            ),
        ];
        let seen: Arc<Mutex<Vec<Request>>> = Arc::new(Mutex::new(Vec::new()));

        let daemon = {
            let backlog = backlog.clone();
            let seen = Arc::clone(&seen);
            tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let (read, mut write) = stream.into_split();
                let mut lines = BufReader::new(read).lines();
                while let Some(line) = lines.next_line().await.unwrap() {
                    let Ok(frame) = aigentic_api::decode(&line) else {
                        break;
                    };
                    let id = frame.id.unwrap_or(0);
                    let request = match frame.body {
                        Body::Request(request) => request,
                        _ => break,
                    };
                    seen.lock().unwrap().push(request.clone());
                    let response = match request {
                        Request::Hello { .. } => Response::Welcome(Welcome {
                            user: "steve".into(),
                            projects: Vec::new(),
                            server: "fake".into(),
                        }),
                        Request::Build { issue, .. } => {
                            assert_eq!(issue, 58);
                            Response::Run {
                                lead,
                                resumed: true,
                            }
                        }
                        Request::Open { thread, from_seq } => {
                            assert_eq!(thread, lead);
                            assert_eq!(from_seq, 0, "the backlog is the whole log");
                            Response::Opened {
                                state: ThreadState::Idle,
                                events: backlog.clone(),
                                run: None,
                                mode: "manual".into(),
                                profile: None,
                                model: "unknown".into(),
                                effort: None,
                            }
                        }
                        Request::AnswerCheckpoint { .. } => {
                            // The answer ends the run, as the daemon's task
                            // would: the outcome arrives as a live notice.
                            let done = event_at(
                                EventKind::RunFinished,
                                3,
                                lead,
                                serde_json::json!({"outcome": "stopped", "cost_usd": 0.0}),
                            );
                            write
                                .write_all(
                                    format!(
                                        "{}\n",
                                        encode(&Frame::notice(Notice::Event {
                                            thread: lead,
                                            event: done,
                                        }))
                                    )
                                    .as_bytes(),
                                )
                                .await
                                .unwrap();
                            Response::Ok
                        }
                        other => panic!("the command sent {other:?}"),
                    };
                    let line = format!("{}\n", encode(&Frame::response(id, response)));
                    write.write_all(line.as_bytes()).await.unwrap();
                }
            })
        };

        let (client, _) = Client::connect(&aigentic_api::client::Addr::Unix(socket), "steve")
            .await
            .unwrap();
        let notices = client.take_notices().expect("the notice stream");
        let mut out = Vec::new();
        let mut err = Vec::new();
        let outcome = run(
            &client,
            notices,
            "p",
            "steve",
            &BuildArgs {
                issue: 58,
                workflow: None,
                json: false,
            },
            &mut out,
            &mut err,
        )
        .await
        .unwrap();

        assert!(outcome.resumed, "the run was picked up, not started");
        assert_eq!(
            outcome.code, EXIT_NEEDS_HUMAN,
            "the run stopped, so a person is needed: {outcome:?}"
        );

        // The backlog is printed: the step and the gate it waits at.
        let err = String::from_utf8(err).unwrap();
        assert!(
            err.contains("step implement-alone attempt 1"),
            "the step already run is shown: {err}"
        );
        assert!(
            err.contains("checkpoint `route`"),
            "the gate it waits at is shown: {err}"
        );

        // The open gate was answered `stop`, once, and only because the
        // backlog ended at `checkpoint_asked`.
        let seen = seen.lock().unwrap();
        let answers: Vec<&Request> = seen
            .iter()
            .filter(|request| matches!(request, Request::AnswerCheckpoint { .. }))
            .collect();
        assert_eq!(answers.len(), 1, "one answer, and only one");
        match answers[0] {
            Request::AnswerCheckpoint {
                lead: answered,
                gate,
                answer,
                amendment,
            } => {
                assert_eq!(*answered, lead, "the lead the command was told");
                assert_eq!(gate, "route", "the gate the backlog named");
                assert_eq!(*answer, CheckpointAnswer::Stop, "never go");
                assert!(amendment.is_none());
            }
            other => panic!("{other:?}"),
        }
        daemon.abort();
    }
}
