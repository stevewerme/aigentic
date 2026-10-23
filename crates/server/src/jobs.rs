//! The driver of a background job: once a person approves a `start_job`
//! proposal, it makes the job's own thread in the front thread's project,
//! seeds it with the brief, waits for it to end, and posts one
//! `job_finished` on the front thread saying what it did, what it cost
//! and whether it fell over.
//!
//! The driver writes no log itself: the child's events go through the
//! child's actor and the return through the front's, so each thread keeps
//! its single writer.

use std::sync::Arc;
use std::time::Duration;

use aigentic_api::{Notice, Response, ThreadState};
use aigentic_runtime::aigentic_core::{Author, ContentBlock, Event, EventKind};
use aigentic_runtime::aigentic_log::{
    AssistantMessagePayload, DecisionAnswer, DecisionKind, JobFinishedPayload, JobOutcome,
    TurnEndedPayload, decision_records,
};
use aigentic_runtime::{INTERRUPTED, JobSpec, Mode};
use tokio::sync::{mpsc, oneshot};
use ulid::Ulid;

use crate::actor::{Mail, Mailbox};
use crate::threads::{ThreadError, ThreadTable};

/// How often the driver asks the child whether it has ended.
pub const JOB_TICK: Duration = Duration::from_millis(250);

/// How long a job may run before the driver stops it.
pub const JOB_DEADLINE: Duration = Duration::from_secs(6 * 60 * 60);

/// The most characters of the job's last words the return carries: one
/// screen on the front thread. The rest stays in the job's own thread.
pub const JOB_SUMMARY_CHARS: usize = 2_000;

/// What a person approved, as the front thread's actor hands it over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Approved {
    /// The person who said yes: the job's thread is theirs.
    pub by: Author,
    pub task: String,
    /// The job's first message.
    pub brief: String,
    pub read_only: bool,
    /// The front thread's mode at the moment of the answer.
    pub mode: Mode,
}

/// Run an approved job to its end and post its return on `front`. A job
/// that cannot be started leaves a warning and writes nothing on the
/// front; one whose return cannot be posted leaves its own log as the
/// record.
pub async fn start(threads: Arc<ThreadTable>, front: Ulid, approved: Approved) {
    if let Err(e) = run(&threads, front, approved).await {
        eprintln!("warn  job from thread {front}: {e}");
    }
}

async fn run(
    threads: &Arc<ThreadTable>,
    front: Ulid,
    approved: Approved,
) -> Result<(), ThreadError> {
    let project = threads
        .project_of(front)
        .ok_or(ThreadError::NoThread(front))?;
    let root = threads
        .root_of_thread(front)
        .ok_or(ThreadError::NoThread(front))?;
    let child = threads.create_job(front, &project, &root, approved.by.clone())?;
    let spec = JobSpec {
        front,
        read_only: approved.read_only,
        mode: approved.mode,
    };
    let number = job_number(&threads.events_from(front, 0)?);
    let (front_box, _) = threads.open(front).await?;
    let (mailbox, _) = match threads.open_job(child, spec).await {
        Ok(opened) => opened,
        Err(e) => {
            threads.close(front);
            return Err(e);
        }
    };
    let forwarding = forward(&mailbox, front_box.clone(), child, number).await;
    let ended = drive(threads, &mailbox, approved.brief).await;
    threads.close(child);
    if let Some(task) = forwarding {
        task.abort();
    }
    let stopped = match ended {
        Ok(stopped) => stopped,
        Err(e) => {
            threads.close(front);
            return Err(e);
        }
    };
    let events = threads.events_from(child, 0)?;
    let payload = job_return(child, &events, stopped);

    let posted = send(&front_box, |reply| Mail::JobFinished { payload, reply }).await;
    threads.close(front);
    match posted {
        Some(Response::Ok) => Ok(()),
        other => Err(ThreadError::Refused(format!(
            "the return was not posted: {other:?}"
        ))),
    }
}

/// Seed the child with the brief and wait for it to go idle. The deadline
/// when it passed and the driver stopped the job, else `None`.
async fn drive(
    threads: &ThreadTable,
    mailbox: &Mailbox,
    brief: String,
) -> Result<Option<Duration>, ThreadError> {
    let posted = send(mailbox, |reply| Mail::Post {
        author: Author::System,
        blocks: vec![ContentBlock::Text(brief)],
        interrupt: false,
        reply,
    })
    .await;
    if posted != Some(Response::Ok) {
        return Err(ThreadError::Refused(format!(
            "the brief was not posted: {posted:?}"
        )));
    }
    let (tick, deadline) = threads.job_limits();
    let started = tokio::time::Instant::now();
    loop {
        tokio::time::sleep(tick).await;
        if status(mailbox).await? == ThreadState::Idle {
            return Ok(None);
        }
        if started.elapsed() >= deadline {
            let _ = send(mailbox, |reply| Mail::Interrupt {
                by: Author::System,
                reply,
            })
            .await;
            tokio::time::sleep(tick).await;
            let _ = status(mailbox).await?;
            return Ok(Some(deadline));
        }
    }
}

/// This job's place among the front thread's jobs, counted from 1 in
/// start order: every approved job proposal in the front's log, this
/// one's included, since its approval is written before it starts. Read
/// from the log, so a number is never handed out twice, across restarts
/// too.
pub fn job_number(front_events: &[Event]) -> u32 {
    decision_records(front_events)
        .records
        .iter()
        .filter(|r| r.kind == DecisionKind::Job)
        .filter(|r| {
            r.answer
                .as_ref()
                .is_some_and(|a| a.answer == DecisionAnswer::Yes)
        })
        .count() as u32
}

/// Whether a job's notice is one the front thread's watchers need: a
/// change of state, which carries a waiting permission request or
/// question, and the permission events that ask and settle one.
pub fn forwarded(notice: &Notice) -> bool {
    match notice {
        Notice::State { .. } => true,
        Notice::Event { event, .. } => matches!(
            event.kind,
            EventKind::PermissionRequested | EventKind::PermissionDecided
        ),
        _ => false,
    }
}

/// Subscribe to the job's thread and pass every forwarded notice to the
/// front thread's actor, tagged with the job, until either side goes.
/// `None` when the job's actor is already gone.
async fn forward(
    child_box: &Mailbox,
    front_box: Mailbox,
    job: Ulid,
    number: u32,
) -> Option<tokio::task::JoinHandle<()>> {
    let (notices, mut rx) = mpsc::unbounded_channel();
    let (reply, ack) = oneshot::channel();
    child_box
        .send(Mail::Subscribe {
            from_seq: u64::MAX,
            notices,
            reply,
        })
        .ok()?;
    ack.await.ok()?;
    Some(tokio::spawn(async move {
        while let Some(notice) = rx.recv().await {
            if forwarded(&notice)
                && front_box
                    .send(Mail::JobNotice {
                        job,
                        number,
                        notice,
                    })
                    .is_err()
            {
                return;
            }
        }
    }))
}

async fn status(mailbox: &Mailbox) -> Result<ThreadState, ThreadError> {
    let (reply, rx) = oneshot::channel();
    mailbox
        .send(Mail::Status { reply })
        .map_err(|_| ThreadError::Gone)?;
    rx.await.map_err(|_| ThreadError::Gone)
}

/// Send one mail and wait for its reply; `None` when the actor is gone.
async fn send(
    mailbox: &Mailbox,
    mail: impl FnOnce(oneshot::Sender<Response>) -> Mail,
) -> Option<Response> {
    let (reply, rx) = oneshot::channel();
    mailbox.send(mail(reply)).ok()?;
    rx.await.ok()
}

/// What the job's log says about how it ended. `stopped` is the deadline
/// the driver interrupted it at, if it did.
pub fn job_return(child: Ulid, events: &[Event], stopped: Option<Duration>) -> JobFinishedPayload {
    let ended: Option<TurnEndedPayload> = events
        .iter()
        .rev()
        .find(|event| event.kind == EventKind::TurnEnded)
        .and_then(|event| serde_json::from_value(event.payload.clone()).ok());
    let last_words = events
        .iter()
        .rev()
        .filter(|event| event.kind == EventKind::AssistantMessage)
        .filter_map(|event| {
            serde_json::from_value::<AssistantMessagePayload>(event.payload.clone()).ok()
        })
        .map(|payload| text_of(&payload.blocks))
        .find(|text| !text.trim().is_empty());
    let interrupted = ended.as_ref().is_some_and(|e| e.reason == INTERRUPTED);
    let failed = ended
        .as_ref()
        .filter(|e| e.error.is_some() || e.reason.starts_with("provider_error"));
    let (outcome, summary) = if stopped.is_some() || interrupted {
        let why = match stopped {
            Some(deadline) => {
                format!("The job was stopped: it was still running after {deadline:?}.")
            }
            None => "The job was stopped before it finished.".to_owned(),
        };
        let summary = match &last_words {
            Some(words) => format!("{why} Its last words:\n\n{}", one_screen(words)),
            None => why,
        };
        (JobOutcome::Stopped, summary)
    } else if let Some(ended) = failed {
        let cause = match &ended.error {
            Some(error) => error.to_string(),
            None => ended.reason.clone(),
        };
        (JobOutcome::Failed, format!("The job fell over: {cause}"))
    } else {
        let summary = match &last_words {
            Some(words) => one_screen(words),
            None => "The job finished without a word.".to_owned(),
        };
        (JobOutcome::Finished, summary)
    };
    JobFinishedPayload {
        job_thread: child,
        outcome,
        summary,
        cost_usd: cost_of(events),
    }
}

/// The sum of the prices the runtime stamped on the job's model calls; a
/// call on an unpriced endpoint adds nothing. Folded from `0.0` because
/// `Sum for f64` starts at `-0.0`, which prints as `$-0.00`.
fn cost_of(events: &[Event]) -> f64 {
    events
        .iter()
        .filter(|event| event.kind == EventKind::AssistantMessage)
        .filter_map(|event| {
            serde_json::from_value::<AssistantMessagePayload>(event.payload.clone()).ok()
        })
        .filter_map(|payload| payload.usage.and_then(|usage| usage.cost_usd))
        .fold(0.0, |total, cost| total + cost)
}

fn text_of(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `text` whole when it fits in [`JOB_SUMMARY_CHARS`], else as many whole
/// lines as fit and a line saying where the rest is. A first line longer
/// than the limit is cut at a character boundary.
fn one_screen(text: &str) -> String {
    let text = text.trim();
    if text.chars().count() <= JOB_SUMMARY_CHARS {
        return text.to_owned();
    }
    let mut kept = String::new();
    let mut count = 0;
    for line in text.lines() {
        let len = line.chars().count() + usize::from(!kept.is_empty());
        if count + len > JOB_SUMMARY_CHARS {
            break;
        }
        if !kept.is_empty() {
            kept.push('\n');
        }
        kept.push_str(line);
        count += len;
    }
    if kept.is_empty() {
        kept = text.chars().take(JOB_SUMMARY_CHARS).collect();
    }
    kept.push_str("\n\nThe rest is in the job's own thread.");
    kept
}

#[cfg(test)]
mod tests {
    use super::*;
    use aigentic_runtime::aigentic_log::{
        DecisionAnsweredPayload, DecisionProposedPayload, DecisionStage, NewEvent, ThreadLog,
        project_body,
    };

    /// A job stopped before any priced call costs nothing, and the front
    /// thread's context must say so rather than a negative zero.
    #[test]
    fn a_job_with_no_priced_call_costs_exactly_zero() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = ThreadLog::open(dir.path(), Ulid::generate()).unwrap();
        let payload = job_return(Ulid::generate(), &[], Some(JOB_DEADLINE));
        assert_eq!(payload.cost_usd.to_bits(), 0.0f64.to_bits());

        log.append(NewEvent {
            kind: EventKind::JobFinished,
            author: Author::System,
            payload: serde_json::to_value(&payload).unwrap(),
            parent_event: None,
        })
        .unwrap();
        let body = project_body(&log.read_all().unwrap()).unwrap();
        let text = body
            .iter()
            .flat_map(|message| message.blocks.iter())
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        assert!(text.contains("cost $0.00"), "{text}");
        assert!(!text.contains("$-"), "{text}");
    }

    /// The return is one screen on the front thread, and a cut never
    /// leaves half a line or reads as a cut of the log.
    #[test]
    fn a_return_that_fits_is_given_whole_and_one_that_does_not_keeps_whole_lines() {
        let short = "found three call sites\nall fixed";
        assert_eq!(one_screen(short), short);

        let line = "x".repeat(JOB_SUMMARY_CHARS / 4);
        let long = [line.as_str(); 8].join("\n");
        let cut = one_screen(&long);
        let (kept, tail) = cut.split_once("\n\n").unwrap();
        assert!(kept.lines().all(|l| l == line), "only whole lines are kept");
        assert!(kept.chars().count() <= JOB_SUMMARY_CHARS);
        assert_eq!(tail, "The rest is in the job's own thread.");

        let one_line = "y".repeat(JOB_SUMMARY_CHARS * 2);
        let cut = one_screen(&one_line);
        let (kept, _) = cut.split_once("\n\n").unwrap();
        assert_eq!(kept.chars().count(), JOB_SUMMARY_CHARS);
    }

    /// Append a `kind` proposal answered `answer`, as the front's turn
    /// writes the pair.
    fn decide(log: &mut ThreadLog, kind: DecisionKind, answer: DecisionAnswer) {
        let proposed = log
            .append(NewEvent {
                kind: EventKind::DecisionProposed,
                author: Author::System,
                payload: serde_json::to_value(DecisionProposedPayload {
                    kind,
                    proposal: "a proposal".into(),
                    target: None,
                    reason: "a reason".into(),
                    call_id: None,
                    job: None,
                    stage: DecisionStage::Ask,
                })
                .unwrap(),
                parent_event: None,
            })
            .unwrap();
        log.append(NewEvent {
            kind: EventKind::DecisionAnswered,
            author: Author::System,
            payload: serde_json::to_value(DecisionAnsweredPayload {
                answer,
                correction: None,
                note: None,
            })
            .unwrap(),
            parent_event: Some(proposed.id),
        })
        .unwrap();
    }

    /// A job's number counts only the jobs a person said yes to, so a
    /// declined proposal or another kind of decision never uses one up.
    #[test]
    fn a_jobs_number_counts_the_approved_jobs_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = ThreadLog::open(dir.path(), Ulid::generate()).unwrap();
        assert_eq!(job_number(&log.read_all().unwrap()), 0);
        decide(&mut log, DecisionKind::Job, DecisionAnswer::Yes);
        assert_eq!(job_number(&log.read_all().unwrap()), 1);
        decide(&mut log, DecisionKind::Job, DecisionAnswer::No);
        decide(&mut log, DecisionKind::Project, DecisionAnswer::Yes);
        assert_eq!(job_number(&log.read_all().unwrap()), 1);
        decide(&mut log, DecisionKind::Job, DecisionAnswer::Yes);
        assert_eq!(job_number(&log.read_all().unwrap()), 2);
    }

    /// The front thread is told what it may have to answer and nothing of
    /// the job's working: a model reply or a tool result stays in the
    /// job's own thread.
    #[test]
    fn only_state_and_permission_events_are_forwarded() {
        let dir = tempfile::tempdir().unwrap();
        let thread = Ulid::generate();
        let mut log = ThreadLog::open(dir.path(), thread).unwrap();
        let mut event = |kind| {
            log.append(NewEvent {
                kind,
                author: Author::System,
                payload: serde_json::json!({}),
                parent_event: None,
            })
            .unwrap()
        };
        let notice = |event| Notice::Event { thread, event };
        assert!(forwarded(&notice(event(EventKind::PermissionRequested))));
        assert!(forwarded(&notice(event(EventKind::PermissionDecided))));
        assert!(!forwarded(&notice(event(EventKind::AssistantMessage))));
        assert!(!forwarded(&notice(event(EventKind::ToolResult))));
        assert!(forwarded(&Notice::State {
            thread,
            state: ThreadState::Idle,
        }));
        assert!(!forwarded(&Notice::Note {
            thread,
            text: "a note".into(),
        }));
    }
}
