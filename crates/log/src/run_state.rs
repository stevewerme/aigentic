//! The build runner's state, replayed from a lead thread's log.
//!
//! Layer 2 keeps no side table: `run_state` folds the runner's events in
//! log order into the run's steps and budget, and
//! [`RunState::next_move`] answers "where was the run when the log ends"
//! from the *last* decisive event — a warning is not a decision, so it
//! reports the move of the event before it (issue #53, PLAN-layer2 §4,
//! §9).

use aigentic_core::{Event, EventKind};
use serde::de::DeserializeOwned;
use ulid::Ulid;

use crate::payload::{
    BudgetScope, BudgetWarnedPayload, CheckOutcome, CheckResult, CheckpointAnswer,
    CheckpointAnsweredPayload, CheckpointAskedPayload, ChecksRunPayload, CommitRef, PushedPayload,
    ReleaseImpact, RouteTakenPayload, RunFinishedPayload, RunOutcome, RunStartedPayload,
    StepFinishedPayload, StepStartedPayload, StepStatus,
};
use crate::store::LogError;

/// How a step's checks did as a whole: what `NextMove::ChecksDone`
/// reports. The runner decides a send-back and its limit from this; the
/// state does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChecksOutcome {
    Pass,
    Flagged,
    Fail,
}

impl ChecksOutcome {
    /// `fail` if any check failed, else `flagged` if any flagged, else
    /// `pass`.
    fn of(checks: &[CheckOutcome]) -> Self {
        if checks.iter().any(|c| c.result == CheckResult::Fail) {
            Self::Fail
        } else if checks.iter().any(|c| c.result == CheckResult::Flag) {
            Self::Flagged
        } else {
            Self::Pass
        }
    }
}

/// One entry into one step: a `step_started` and everything the log
/// records about that attempt. A send-back or a `continue` appends a new
/// `step_started` and so a new record; a fresh-thread continuation has a
/// new `child_thread` too.
#[derive(Debug, Clone, PartialEq)]
pub struct StepRecord {
    pub step: String,
    /// 1-based, straight from `step_started`.
    pub attempt: u32,
    pub role: String,
    pub profile: String,
    pub child_thread: Ulid,
    /// The budget `step_started` was given.
    pub budget_usd: f64,
    pub status: Option<StepStatus>,
    pub end_reason: Option<String>,
    pub cost_usd: f64,
    pub reported_event: Option<Ulid>,
    pub checks: Option<Vec<CheckOutcome>>,
    /// The commits this step pushed, if it pushed. Per step, because a
    /// fix round pushes again and a run-wide flag could not say which.
    pub pushed: Option<Vec<CommitRef>>,
    /// Step-scope warnings, in log order, for the step that was open
    /// when each was appended. Both numbers are kept: §8 warns twice at
    /// each scope, so a restart needs the level.
    pub warned: Vec<BudgetWarnedPayload>,
}

impl StepRecord {
    fn open(&self) -> bool {
        self.status.is_none()
    }
}

/// What the log says the run should do next. Exactly one variant applies
/// to any log, decided by the last lead-thread event.
#[derive(Debug, Clone, PartialEq)]
pub enum NextMove {
    /// No `run_started` in sight: not a run's log.
    NotARun,
    /// `run_started` and nothing since: enter the workflow's first step.
    StartFirstStep,
    /// The last `step_started` has no `step_finished`: await that child.
    ReAwait {
        step: String,
        child_thread: Ulid,
        attempt: u32,
    },
    /// A child ended and its checks have not run.
    AfterStep {
        step: String,
        attempt: u32,
        status: StepStatus,
    },
    /// The checks ran: the runner decides send-back or push from here.
    ChecksDone {
        step: String,
        attempt: u32,
        outcome: ChecksOutcome,
    },
    /// Pushed (and installed): resume after the push — install, close the
    /// issue, `run_finished` — never re-run the step or re-push.
    Pushed {
        step: String,
        commits: Vec<CommitRef>,
    },
    /// A route was taken: follow the logged decision. Preconditions read
    /// git state a crash can change, so replay does not re-derive it.
    FollowRoute { branch: String, taken: String },
    /// A gate is open and unanswered: wait for the human.
    AwaitingCheckpoint { gate: String },
    /// A gate was answered: act on the answer, do not re-ask.
    Answered {
        gate: String,
        answer: CheckpointAnswer,
        amendment: Option<String>,
    },
    /// The run ended.
    Done { outcome: RunOutcome },
}

/// The run's replayed state: one lead thread, one run.
#[derive(Debug, Clone, PartialEq)]
pub struct RunState {
    pub issue: u64,
    pub workflow: String,
    /// `workflow.toml`'s version the run began with.
    pub version: u32,
    pub content_hash: String,
    /// The latest budget the log sets: `run_started`'s provisional one,
    /// or a later `route_taken`'s, which is the brief's.
    pub budget_usd: f64,
    pub steps: Vec<StepRecord>,
    /// The sum of the steps' `step_finished` costs.
    pub cost_usd: f64,
    /// Issue-scope warnings, in log order.
    pub warned: Vec<BudgetWarnedPayload>,
    /// Every route taken, in log order; the last one is where replay is.
    pub routes: Vec<RouteTakenPayload>,
    /// The gate of the open `checkpoint_asked`, if any.
    pub pending_checkpoint: Option<String>,
    pub outcome: Option<RunOutcome>,
    pub release_impact: Option<ReleaseImpact>,
    /// The last event that decides a move. `BudgetWarned` never becomes
    /// this: a warning is not a decision.
    last: Option<EventKind>,
    /// The last answered checkpoint: the gate that was open when the
    /// answer came, and the answer itself.
    answered: Option<(String, CheckpointAnsweredPayload)>,
}

impl RunState {
    /// Where the run is, from the last decisive event of the log. The
    /// arms reach into state the same event wrote, so each is total.
    pub fn next_move(&self) -> NextMove {
        let Some(kind) = self.last else {
            return NextMove::NotARun;
        };
        match kind {
            EventKind::RunStarted => NextMove::StartFirstStep,
            EventKind::StepStarted => {
                let open = self.open_step();
                NextMove::ReAwait {
                    step: open.step.clone(),
                    child_thread: open.child_thread,
                    attempt: open.attempt,
                }
            }
            EventKind::StepFinished => NextMove::AfterStep {
                step: self.last_finished().step.clone(),
                attempt: self.last_finished().attempt,
                status: self
                    .last_finished()
                    .status
                    .expect("a step_finished writes the status it reports"),
            },
            EventKind::ChecksRun => NextMove::ChecksDone {
                step: self.last_checked().step.clone(),
                attempt: self.last_checked().attempt,
                outcome: ChecksOutcome::of(
                    self.last_checked()
                        .checks
                        .as_deref()
                        .expect("a checks_run writes the checks it reports"),
                ),
            },
            EventKind::Pushed => {
                let pushed = self
                    .steps
                    .iter()
                    .rev()
                    .find(|s| s.pushed.is_some())
                    .expect("a pushed is written to the step that pushed");
                NextMove::Pushed {
                    step: pushed.step.clone(),
                    commits: pushed.pushed.clone().unwrap_or_default(),
                }
            }
            EventKind::RouteTaken => {
                let route = self
                    .routes
                    .last()
                    .expect("a route_taken is recorded when it is folded");
                NextMove::FollowRoute {
                    branch: route.branch.clone(),
                    taken: route.taken.clone(),
                }
            }
            EventKind::CheckpointAsked => NextMove::AwaitingCheckpoint {
                gate: self
                    .pending_checkpoint
                    .clone()
                    .expect("a checkpoint_asked opens the gate it names"),
            },
            EventKind::CheckpointAnswered => {
                let (gate, answer) = self
                    .answered
                    .as_ref()
                    .expect("a checkpoint_answered records the gate it answered");
                NextMove::Answered {
                    gate: gate.clone(),
                    answer: answer.answer,
                    amendment: answer.amendment.clone(),
                }
            }
            EventKind::RunFinished => NextMove::Done {
                outcome: self
                    .outcome
                    .expect("a run_finished records the outcome it names"),
            },
            // `last` never odd-kinded, and a warning never gets in.
            _ => NextMove::NotARun,
        }
    }

    fn open_step(&self) -> &StepRecord {
        self.steps
            .iter()
            .rev()
            .find(|s| s.open())
            .expect("a step_started opens the step it names")
    }

    fn last_finished(&self) -> &StepRecord {
        self.steps
            .iter()
            .rev()
            .find(|s| s.status.is_some())
            .expect("a step_finished closes the step it names")
    }

    fn last_checked(&self) -> &StepRecord {
        self.steps
            .iter()
            .rev()
            .find(|s| s.checks.is_some())
            .expect("a checks_run writes to the open step")
    }
}

/// The runner's own event kinds: the ones `run_state` reads. The match
/// on `EventKind` is exhaustive on purpose — a new kind cannot slip past
/// this classification and be silently ignored on a lead thread.
fn runner_kind(kind: EventKind) -> Option<RunnerKind> {
    match kind {
        EventKind::RunStarted => Some(RunnerKind::RunStarted),
        EventKind::StepStarted => Some(RunnerKind::StepStarted),
        EventKind::StepFinished => Some(RunnerKind::StepFinished),
        EventKind::ChecksRun => Some(RunnerKind::ChecksRun),
        EventKind::RouteTaken => Some(RunnerKind::RouteTaken),
        EventKind::CheckpointAsked => Some(RunnerKind::CheckpointAsked),
        EventKind::CheckpointAnswered => Some(RunnerKind::CheckpointAnswered),
        EventKind::BudgetWarned => Some(RunnerKind::BudgetWarned),
        EventKind::Pushed => Some(RunnerKind::Pushed),
        EventKind::RunFinished => Some(RunnerKind::RunFinished),
        EventKind::UserMessage
        | EventKind::AssistantMessage
        | EventKind::ToolResult
        | EventKind::TurnEnded
        | EventKind::Compacted
        | EventKind::Pinned
        | EventKind::Interrupted
        | EventKind::SkillLoaded
        | EventKind::PermissionRequested
        | EventKind::PermissionDecided
        | EventKind::MemoryExtracted
        | EventKind::MemoryRemembered
        | EventKind::ThreadStarted
        | EventKind::ThreadRenamed
        | EventKind::ProjectSwitched
        | EventKind::ContextEvicted
        | EventKind::ContextSaturated
        | EventKind::ProviderRetried
        // A child thread's report, never on the lead thread.
        | EventKind::StepReported
        // Decisions (issue #74) are not runner facts either: a lead
        // thread's run state reads none of them.
        | EventKind::DecisionProposed
        | EventKind::DecisionAnswered => None,
    }
}

/// The lead thread's kinds, spelled out so the fold can match without a
/// catch-all arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunnerKind {
    RunStarted,
    StepStarted,
    StepFinished,
    ChecksRun,
    RouteTaken,
    CheckpointAsked,
    CheckpointAnswered,
    BudgetWarned,
    Pushed,
    RunFinished,
}

/// Fold a lead thread's log into the run's state. Runner events out of
/// order are an error, never a guess: the whole point of writing the
/// log ahead of the action is that replay lands where the run was.
pub fn run_state(events: &[Event]) -> Result<RunState, LogError> {
    let mut st = RunState {
        issue: 0,
        workflow: String::new(),
        version: 0,
        content_hash: String::new(),
        budget_usd: 0.0,
        steps: Vec::new(),
        cost_usd: 0.0,
        warned: Vec::new(),
        routes: Vec::new(),
        pending_checkpoint: None,
        outcome: None,
        release_impact: None,
        last: None,
        answered: None,
    };
    let mut opened: Option<usize> = None;
    for event in events {
        let Some(kind) = runner_kind(event.kind) else {
            continue;
        };
        if kind != RunnerKind::RunStarted && st.last.is_none() {
            return Err(ordering(event, "a runner event before run_started"));
        }
        match kind {
            RunnerKind::RunStarted => {
                if st.last.is_some() {
                    return Err(ordering(event, "the run already began"));
                }
                let p: RunStartedPayload = payload(event)?;
                st.issue = p.issue;
                st.workflow = p.workflow;
                st.version = p.version;
                st.content_hash = p.content_hash;
                st.budget_usd = p.budget_usd;
            }
            RunnerKind::StepStarted => {
                if let Some(i) = opened {
                    return Err(ordering(
                        event,
                        &format!("{} is still open", st.steps[i].step),
                    ));
                }
                let p: StepStartedPayload = payload(event)?;
                st.steps.push(StepRecord {
                    step: p.step,
                    attempt: p.attempt,
                    role: p.role,
                    profile: p.profile,
                    child_thread: p.child_thread,
                    budget_usd: p.budget_usd,
                    status: None,
                    end_reason: None,
                    cost_usd: 0.0,
                    reported_event: None,
                    checks: None,
                    pushed: None,
                    warned: Vec::new(),
                });
                opened = Some(st.steps.len() - 1);
            }
            RunnerKind::StepFinished => {
                let p: StepFinishedPayload = payload(event)?;
                let i = open_index(&st, opened, event, &p.step)?;
                let record = &mut st.steps[i];
                record.status = Some(p.status);
                record.end_reason = Some(p.end_reason);
                record.cost_usd = p.cost_usd;
                record.reported_event = p.reported_event;
                st.cost_usd += p.cost_usd;
                opened = None;
            }
            RunnerKind::ChecksRun => {
                let p: ChecksRunPayload = payload(event)?;
                let i = checked_index(&st, event, &p.step)?;
                st.steps[i].checks = Some(p.checks);
            }
            RunnerKind::RouteTaken => {
                let p: RouteTakenPayload = payload(event)?;
                if let Some(budget) = p.budget_usd {
                    st.budget_usd = budget;
                }
                st.routes.push(p);
            }
            RunnerKind::CheckpointAsked => {
                let p: CheckpointAskedPayload = payload(event)?;
                st.pending_checkpoint = Some(p.gate);
            }
            RunnerKind::CheckpointAnswered => {
                let p: CheckpointAnsweredPayload = payload(event)?;
                let Some(gate) = st.pending_checkpoint.take() else {
                    return Err(ordering(event, "no checkpoint is open"));
                };
                st.answered = Some((gate, p));
            }
            RunnerKind::BudgetWarned => {
                let p: BudgetWarnedPayload = payload(event)?;
                match p.scope {
                    BudgetScope::Issue => st.warned.push(p),
                    BudgetScope::Step => {
                        // The step open when it was appended; a warning
                        // between steps still belongs to the step it is
                        // about, which is the latest one.
                        let at = match opened {
                            Some(i) => Some(i),
                            None => st.steps.len().checked_sub(1),
                        };
                        let Some(record) = at.and_then(|i| st.steps.get_mut(i)) else {
                            return Err(ordering(event, "no step to warn about"));
                        };
                        record.warned.push(p);
                    }
                }
                // A warning is not a decision: `last` keeps the move of
                // the event before it.
                continue;
            }
            RunnerKind::Pushed => {
                let p: PushedPayload = payload(event)?;
                let Some(record) = st.steps.last_mut() else {
                    return Err(ordering(event, "no step to push for"));
                };
                record.pushed = Some(p.commits);
            }
            RunnerKind::RunFinished => {
                let p: RunFinishedPayload = payload(event)?;
                st.outcome = Some(p.outcome);
                st.release_impact = p.release_impact;
            }
        }
        st.last = Some(event.kind);
    }
    Ok(st)
}

/// The index of the step a `checks_run` applies to: the latest attempt
/// of the step it names. The checks run after the child ended, so that
/// step is closed by now; naming a step the run never entered is the
/// ordering error.
fn checked_index(st: &RunState, event: &Event, step: &str) -> Result<usize, LogError> {
    st.steps
        .iter()
        .rposition(|s| s.step == step)
        .ok_or_else(|| ordering(event, &format!("no step named {step} to check")))
}

/// The index of the step an event applies to, or the ordering error the
/// log deserves.
fn open_index(
    st: &RunState,
    opened: Option<usize>,
    event: &Event,
    step: &str,
) -> Result<usize, LogError> {
    let Some(i) = opened else {
        return Err(ordering(event, &format!("no step is open for {step}")));
    };
    if st.steps[i].step != step {
        return Err(ordering(
            event,
            &format!("{step} does not match the open {}", st.steps[i].step),
        ));
    }
    Ok(i)
}

fn ordering(event: &Event, detail: &str) -> LogError {
    LogError::RunOrdering {
        seq: event.seq,
        kind: event.kind,
        detail: detail.to_owned(),
    }
}

fn payload<T: DeserializeOwned>(event: &Event) -> Result<T, LogError> {
    serde_json::from_value(event.payload.clone()).map_err(|source| LogError::Payload {
        seq: event.seq,
        kind: event.kind,
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aigentic_core::{AgentId, Author};
    use serde::Serialize;
    use time::macros::datetime;

    fn runner() -> Author {
        Author::Agent(AgentId("runner".into()))
    }

    fn ev(seq: u64, kind: EventKind, payload: serde_json::Value) -> Event {
        Event {
            id: Ulid::from_parts(1_700_000_000_000 + seq, u128::from(seq)),
            thread_id: Ulid::from_parts(1_700_000_000_000, 1),
            seq,
            kind,
            author: runner(),
            payload,
            parent_event: None,
            created_at: datetime!(2026-09-24 12:00:00 UTC),
        }
    }

    fn at(kind: EventKind, payload: impl Serialize) -> Event {
        ev(0, kind, serde_json::to_value(payload).unwrap())
    }

    fn numbered(mut events: Vec<Event>) -> Vec<Event> {
        for (seq, e) in events.iter_mut().enumerate() {
            e.seq = seq as u64;
        }
        events
    }

    /// The first child thread, and the one a send-back re-enters.
    fn child() -> Ulid {
        Ulid::from_parts(1_700_000_000_000, 5)
    }

    /// A fresh-thread continuation's child.
    fn other_child() -> Ulid {
        Ulid::from_parts(1_700_000_000_000, 6)
    }

    fn run_started() -> Event {
        at(
            EventKind::RunStarted,
            RunStartedPayload {
                issue: 53,
                workflow: "build".into(),
                version: 1,
                content_hash: "abc".into(),
                budget_usd: 10.0,
            },
        )
    }

    fn step_started(attempt: u32, child_thread: Ulid) -> Event {
        step_started_named("implement", attempt, child_thread)
    }

    fn step_started_named(step: &str, attempt: u32, child_thread: Ulid) -> Event {
        at(
            EventKind::StepStarted,
            StepStartedPayload {
                step: step.into(),
                role: "implementer".into(),
                profile: "flash".into(),
                child_thread,
                attempt,
                budget_usd: 3.0,
                head_at_start: None,
                remote_at_start: None,
            },
        )
    }

    fn step_finished(status: StepStatus, cost_usd: f64) -> Event {
        step_finished_named("implement", status, cost_usd)
    }

    fn step_finished_named(step: &str, status: StepStatus, cost_usd: f64) -> Event {
        at(
            EventKind::StepFinished,
            StepFinishedPayload {
                step: step.into(),
                status,
                end_reason: "done".into(),
                cost_usd,
                reported_event: None,
            },
        )
    }

    fn check(id: &str, result: CheckResult) -> CheckOutcome {
        CheckOutcome {
            id: id.into(),
            result,
            detail: None,
        }
    }

    fn checks_run(checks: Vec<CheckOutcome>) -> Event {
        checks_run_named("implement", checks)
    }

    fn checks_run_named(step: &str, checks: Vec<CheckOutcome>) -> Event {
        at(
            EventKind::ChecksRun,
            ChecksRunPayload {
                step: step.into(),
                checks,
            },
        )
    }

    fn pushed(shas: &[&str]) -> Event {
        at(
            EventKind::Pushed,
            PushedPayload {
                commits: shas
                    .iter()
                    .map(|sha| CommitRef {
                        sha: (*sha).into(),
                        subject: "log: add the run state".into(),
                    })
                    .collect(),
                ref_before: "abc000".into(),
                ref_after: "abc123".into(),
                installed: Some("abc123".into()),
            },
        )
    }

    fn route_taken(taken: &str) -> Event {
        at(
            EventKind::RouteTaken,
            RouteTakenPayload {
                branch: "fix".into(),
                proposed: "trivial".into(),
                taken: taken.into(),
                preconditions: vec![],
                fallback_reason: None,
                budget_usd: None,
            },
        )
    }

    fn checkpoint_asked(gate: &str) -> Event {
        at(
            EventKind::CheckpointAsked,
            CheckpointAskedPayload {
                gate: gate.into(),
                shown: vec!["## Plan".into()],
                options: vec!["go".into(), "stop".into()],
            },
        )
    }

    fn checkpoint_answered(answer: CheckpointAnswer, amendment: Option<&str>) -> Event {
        at(
            EventKind::CheckpointAnswered,
            CheckpointAnsweredPayload {
                answer,
                amendment: amendment.map(str::to_owned),
                marks: vec![],
            },
        )
    }

    fn budget_warned(scope: BudgetScope, spent_usd: f64, limit_usd: f64) -> Event {
        at(
            EventKind::BudgetWarned,
            BudgetWarnedPayload {
                scope,
                spent_usd,
                limit_usd,
            },
        )
    }

    fn run_finished(outcome: RunOutcome) -> Event {
        at(
            EventKind::RunFinished,
            RunFinishedPayload {
                outcome,
                cost_usd: 0.4,
                release_impact: Some(ReleaseImpact::NoImpact),
            },
        )
    }

    /// T5: no runner event at all is not a run.
    #[test]
    fn an_empty_log_is_not_a_run() {
        assert_eq!(run_state(&[]).unwrap().next_move(), NextMove::NotARun);
        // And neither is one full of the thread's own business.
        let events = numbered(vec![
            at(
                EventKind::UserMessage,
                serde_json::json!({"text": "hello", "attachments": []}),
            ),
            at(
                EventKind::TurnEnded,
                serde_json::json!({"reason": "done", "steps": 1}),
            ),
        ]);
        assert_eq!(run_state(&events).unwrap().next_move(), NextMove::NotARun);
    }

    /// T6: `run_started` and nothing since enters the first step.
    #[test]
    fn a_run_started_alone_starts_the_first_step() {
        let st = run_state(&numbered(vec![run_started()])).unwrap();
        assert_eq!(st.issue, 53);
        assert_eq!(st.workflow, "build");
        assert_eq!(st.version, 1);
        assert_eq!(st.content_hash, "abc");
        assert_eq!(st.budget_usd, 10.0);
        assert_eq!(st.next_move(), NextMove::StartFirstStep);
    }

    /// T7: an unclosed `step_started` re-awaits that child.
    #[test]
    fn an_open_step_re_awaits_its_child() {
        let st = run_state(&numbered(vec![run_started(), step_started(1, child())])).unwrap();
        assert_eq!(
            st.next_move(),
            NextMove::ReAwait {
                step: "implement".into(),
                child_thread: child(),
                attempt: 1,
            }
        );
    }

    /// T8: a child ended and the checks have not run.
    #[test]
    fn a_finished_step_asks_for_what_comes_after_it() {
        let st = run_state(&numbered(vec![
            run_started(),
            step_started(1, child()),
            step_finished(StepStatus::Done, 0.4),
        ]))
        .unwrap();
        assert_eq!(
            st.next_move(),
            NextMove::AfterStep {
                step: "implement".into(),
                attempt: 1,
                status: StepStatus::Done,
            }
        );
    }

    /// T9: the checks ran; the runner decides send-back or push from the
    /// outcome, and a `fail` beats a `flag` beats a `pass`.
    #[test]
    fn checks_report_their_own_outcome() {
        let with = |checks: Vec<CheckOutcome>| {
            numbered(vec![
                run_started(),
                step_started(1, child()),
                step_finished(StepStatus::Done, 0.4),
                checks_run(checks),
            ])
        };
        let outcome =
            |checks: Vec<CheckOutcome>| match run_state(&with(checks)).unwrap().next_move() {
                NextMove::ChecksDone { outcome, .. } => outcome,
                other => panic!("expected ChecksDone, got {other:?}"),
            };

        assert_eq!(outcome(vec![]), ChecksOutcome::Pass);
        assert_eq!(
            outcome(vec![
                check("E1", CheckResult::Pass),
                check("E2", CheckResult::Flag)
            ]),
            ChecksOutcome::Flagged
        );
        assert_eq!(
            outcome(vec![
                check("E1", CheckResult::Flag),
                check("E2", CheckResult::Fail)
            ]),
            ChecksOutcome::Fail
        );
    }

    /// T10: pushed-but-not-finished resumes after the push.
    #[test]
    fn a_push_resumes_after_the_push() {
        let st = run_state(&numbered(vec![
            run_started(),
            step_started(1, child()),
            step_finished(StepStatus::Done, 0.4),
            checks_run(vec![check("E1", CheckResult::Pass)]),
            pushed(&["abc123"]),
        ]))
        .unwrap();
        assert_eq!(
            st.next_move(),
            NextMove::Pushed {
                step: "implement".into(),
                commits: vec![CommitRef {
                    sha: "abc123".into(),
                    subject: "log: add the run state".into(),
                }],
            }
        );
    }

    /// T11: replay follows the route the log took, not a fresh derivation.
    #[test]
    fn a_route_is_followed_from_the_log() {
        let st = run_state(&numbered(vec![run_started(), route_taken("trivial")])).unwrap();
        assert_eq!(
            st.next_move(),
            NextMove::FollowRoute {
                branch: "fix".into(),
                taken: "trivial".into(),
            }
        );
    }

    /// T12: an unanswered gate keeps waiting.
    #[test]
    fn an_unanswered_checkpoint_keeps_waiting() {
        let st = run_state(&numbered(vec![
            run_started(),
            checkpoint_asked("plan_gate"),
        ]))
        .unwrap();
        assert_eq!(st.pending_checkpoint.as_deref(), Some("plan_gate"));
        assert_eq!(
            st.next_move(),
            NextMove::AwaitingCheckpoint {
                gate: "plan_gate".into(),
            }
        );
    }

    /// T13: an answered gate replays its answer instead of re-asking.
    #[test]
    fn an_answered_checkpoint_replays_its_answer() {
        let st = run_state(&numbered(vec![
            run_started(),
            checkpoint_asked("plan_gate"),
            checkpoint_answered(CheckpointAnswer::Amend, Some("add T9")),
        ]))
        .unwrap();
        assert_eq!(st.pending_checkpoint, None);
        assert_eq!(
            st.next_move(),
            NextMove::Answered {
                gate: "plan_gate".into(),
                answer: CheckpointAnswer::Amend,
                amendment: Some("add T9".into()),
            }
        );
    }

    /// T14: a warning is not a decision — it reports the move of the
    /// event before it.
    #[test]
    fn a_warning_does_not_change_the_move() {
        let base = numbered(vec![
            run_started(),
            step_started(1, child()),
            step_finished(StepStatus::Done, 0.4),
        ]);
        let before = run_state(&base).unwrap().next_move();

        let with_step_warning = numbered(vec![
            run_started(),
            step_started(1, child()),
            budget_warned(BudgetScope::Step, 2.4, 3.0),
            step_finished(StepStatus::Done, 0.4),
            budget_warned(BudgetScope::Step, 3.0, 3.0),
        ]);
        let st = run_state(&with_step_warning).unwrap();
        assert_eq!(st.next_move(), before);
        assert_eq!(
            st.next_move(),
            NextMove::AfterStep {
                step: "implement".into(),
                attempt: 1,
                status: StepStatus::Done,
            }
        );
        // And the step's warnings folded onto the step that was open.
        assert_eq!(st.steps[0].warned.len(), 2);
        assert_eq!(st.warned, vec![]);
    }

    /// T15: a finished run is done.
    #[test]
    fn a_finished_run_is_done() {
        let st = run_state(&numbered(vec![
            run_started(),
            run_finished(RunOutcome::Closed),
        ]))
        .unwrap();
        assert_eq!(st.outcome, Some(RunOutcome::Closed));
        assert_eq!(st.release_impact, Some(ReleaseImpact::NoImpact));
        assert_eq!(
            st.next_move(),
            NextMove::Done {
                outcome: RunOutcome::Closed,
            }
        );
    }

    /// T16: a send-back is a second `step_started`, attempt 2, on the
    /// same child thread — and it is the write-ahead event for re-entry.
    #[test]
    fn a_send_back_is_a_second_step_started_on_the_same_child() {
        let st = run_state(&numbered(vec![
            run_started(),
            step_started(1, child()),
            step_finished(StepStatus::Done, 0.4),
            checks_run(vec![check("E1", CheckResult::Fail)]),
            step_started(2, child()),
        ]))
        .unwrap();
        assert_eq!(st.steps.len(), 2);
        assert_eq!(st.steps[0].attempt, 1);
        assert_eq!(st.steps[1].attempt, 2);
        assert_eq!(st.steps[0].child_thread, st.steps[1].child_thread);
        assert_eq!(
            st.next_move(),
            NextMove::ReAwait {
                step: "implement".into(),
                child_thread: child(),
                attempt: 2,
            }
        );
    }

    /// Extra (amendment 2 item 2, not one of its named tests): a
    /// fresh-thread continuation is a new `step_started` with a new child
    /// thread, so a replay awaits the new child, not the old one.
    #[test]
    fn a_fresh_thread_continuation_gets_a_new_child() {
        let st = run_state(&numbered(vec![
            run_started(),
            step_started(1, child()),
            step_finished(StepStatus::Partial, 0.2),
            step_started(2, other_child()),
        ]))
        .unwrap();
        assert_eq!(st.steps[0].child_thread, child());
        assert_eq!(st.steps[1].child_thread, other_child());
    }

    /// T17: two pushing steps, each record holding its own commits.
    #[test]
    fn each_step_keeps_the_commits_it_pushed() {
        let st = run_state(&numbered(vec![
            run_started(),
            step_started(1, child()),
            step_finished(StepStatus::Done, 0.4),
            checks_run(vec![check("E1", CheckResult::Pass)]),
            pushed(&["aaa111"]),
            step_started_named("review", 1, other_child()),
            step_finished_named("review", StepStatus::Done, 0.3),
            checks_run_named("review", vec![check("E1", CheckResult::Pass)]),
            pushed(&["bbb222", "ccc333"]),
        ]))
        .unwrap();
        assert_eq!(st.steps.len(), 2);
        assert_eq!(
            st.steps[0].pushed.as_ref().unwrap()[0].sha,
            "aaa111",
            "the first step keeps its own commits"
        );
        assert_eq!(
            st.steps[1]
                .pushed
                .as_ref()
                .unwrap()
                .iter()
                .map(|c| c.sha.as_str())
                .collect::<Vec<_>>(),
            vec!["bbb222", "ccc333"]
        );
        assert_eq!(
            st.next_move(),
            NextMove::Pushed {
                step: "review".into(),
                commits: st.steps[1].pushed.clone().unwrap(),
            }
        );
    }

    /// T18: at the issue scope, the 80% warning does not suppress the
    /// 100% one — both levels stay in `warned`.
    #[test]
    fn the_eighty_percent_warning_does_not_suppress_the_hundred() {
        let st = run_state(&numbered(vec![
            run_started(),
            budget_warned(BudgetScope::Issue, 8.0, 10.0),
            budget_warned(BudgetScope::Issue, 10.0, 10.0),
        ]))
        .unwrap();
        let levels: Vec<(f64, f64)> = st
            .warned
            .iter()
            .map(|w| (w.spent_usd, w.limit_usd))
            .collect();
        assert_eq!(levels, vec![(8.0, 10.0), (10.0, 10.0)]);
        assert_eq!(st.next_move(), NextMove::StartFirstStep);
    }

    /// T19: one table, one row per lead-thread kind — every kind as the
    /// last event yields the move of amendment 2's table, so a future
    /// kind cannot fall through to a default.
    #[test]
    fn every_lead_thread_kind_has_a_move() {
        let table: Vec<(EventKind, Vec<Event>, NextMove)> = vec![
            (
                EventKind::RunStarted,
                vec![run_started()],
                NextMove::StartFirstStep,
            ),
            (
                EventKind::StepStarted,
                vec![run_started(), step_started(1, child())],
                NextMove::ReAwait {
                    step: "implement".into(),
                    child_thread: child(),
                    attempt: 1,
                },
            ),
            (
                EventKind::StepFinished,
                vec![
                    run_started(),
                    step_started(1, child()),
                    step_finished(StepStatus::Done, 0.4),
                ],
                NextMove::AfterStep {
                    step: "implement".into(),
                    attempt: 1,
                    status: StepStatus::Done,
                },
            ),
            (
                EventKind::ChecksRun,
                vec![
                    run_started(),
                    step_started(1, child()),
                    step_finished(StepStatus::Done, 0.4),
                    checks_run(vec![check("E1", CheckResult::Pass)]),
                ],
                NextMove::ChecksDone {
                    step: "implement".into(),
                    attempt: 1,
                    outcome: ChecksOutcome::Pass,
                },
            ),
            (
                EventKind::RouteTaken,
                vec![run_started(), route_taken("trivial")],
                NextMove::FollowRoute {
                    branch: "fix".into(),
                    taken: "trivial".into(),
                },
            ),
            (
                EventKind::CheckpointAsked,
                vec![run_started(), checkpoint_asked("plan_gate")],
                NextMove::AwaitingCheckpoint {
                    gate: "plan_gate".into(),
                },
            ),
            (
                EventKind::CheckpointAnswered,
                vec![
                    run_started(),
                    checkpoint_asked("plan_gate"),
                    checkpoint_answered(CheckpointAnswer::Go, None),
                ],
                NextMove::Answered {
                    gate: "plan_gate".into(),
                    answer: CheckpointAnswer::Go,
                    amendment: None,
                },
            ),
            (
                EventKind::BudgetWarned,
                vec![
                    run_started(),
                    step_started(1, child()),
                    budget_warned(BudgetScope::Step, 2.4, 3.0),
                ],
                NextMove::ReAwait {
                    step: "implement".into(),
                    child_thread: child(),
                    attempt: 1,
                },
            ),
            (
                EventKind::Pushed,
                vec![
                    run_started(),
                    step_started(1, child()),
                    step_finished(StepStatus::Done, 0.4),
                    checks_run(vec![check("E1", CheckResult::Pass)]),
                    pushed(&["abc123"]),
                ],
                NextMove::Pushed {
                    step: "implement".into(),
                    commits: vec![CommitRef {
                        sha: "abc123".into(),
                        subject: "log: add the run state".into(),
                    }],
                },
            ),
            (
                EventKind::RunFinished,
                vec![run_started(), run_finished(RunOutcome::Closed)],
                NextMove::Done {
                    outcome: RunOutcome::Closed,
                },
            ),
        ];

        for (kind, events, want) in &table {
            let log = numbered(events.clone());
            let got = run_state(&log)
                .unwrap_or_else(|e| panic!("{kind:?} is a legal last event: {e}"))
                .next_move();
            assert_eq!(&got, want, "{kind:?} as the last event");
        }
        // The table covers every lead-thread kind, and no non-runner one.
        let kinds: Vec<EventKind> = table.iter().map(|(k, _, _)| *k).collect();
        for kind in [
            EventKind::RunStarted,
            EventKind::StepStarted,
            EventKind::StepFinished,
            EventKind::ChecksRun,
            EventKind::RouteTaken,
            EventKind::CheckpointAsked,
            EventKind::CheckpointAnswered,
            EventKind::BudgetWarned,
            EventKind::Pushed,
            EventKind::RunFinished,
        ] {
            assert_eq!(
                runner_kind(kind).is_some(),
                kinds.contains(&kind),
                "{kind:?} is a lead-thread kind"
            );
        }
    }

    /// T4 (issue #74): the decision events are not runner facts —
    /// `runner_kind` reads none of them, and a run with proposals and
    /// answers woven into its log folds to the same `RunState` as the same
    /// run without them.
    #[test]
    fn the_decision_kinds_are_not_runner_facts() {
        for kind in [EventKind::DecisionProposed, EventKind::DecisionAnswered] {
            assert_eq!(
                runner_kind(kind),
                None,
                "{kind:?} is not a lead-thread kind"
            );
        }
        let proposal = ev(
            0,
            EventKind::DecisionProposed,
            serde_json::json!({"kind": "project", "proposal": "p", "reason": "r"}),
        );
        let mut answer = ev(
            0,
            EventKind::DecisionAnswered,
            serde_json::json!({"answer": "yes"}),
        );
        answer.parent_event = Some(proposal.id);

        let plain = numbered(vec![run_started(), step_started(1, child())]);
        let with = numbered(vec![
            run_started(),
            proposal,
            step_started(1, child()),
            answer,
        ]);
        assert_eq!(run_state(&plain).unwrap(), run_state(&with).unwrap());
    }

    /// T20: a `step_finished` before any `step_started` is an ordering
    /// error, not a guess.
    #[test]
    fn a_step_finished_before_any_step_started_is_an_ordering_error() {
        let log = numbered(vec![run_started(), step_finished(StepStatus::Done, 0.4)]);
        match run_state(&log) {
            Err(LogError::RunOrdering { seq, kind, detail }) => {
                assert_eq!(seq, 1);
                assert_eq!(kind, EventKind::StepFinished);
                assert!(detail.contains("no step is open for implement"), "{detail}");
            }
            other => panic!("expected a run-ordering error, got {other:?}"),
        }
    }

    /// The other ordering rules the plan names, each an error rather than
    /// a guess.
    #[test]
    fn the_run_ordering_rules_hold() {
        let checks = || checks_run(vec![check("E1", CheckResult::Pass)]);
        let cases: Vec<(&str, Vec<Event>)> = vec![
            (
                "a runner event before run_started",
                vec![step_started(1, child())],
            ),
            (
                "a second step_started while one is open",
                vec![
                    run_started(),
                    step_started(1, child()),
                    step_started(2, child()),
                ],
            ),
            (
                "checks_run for a step never started",
                vec![run_started(), checks()],
            ),
            (
                "checks_run for a step that is not the open one",
                vec![
                    run_started(),
                    step_started(1, child()),
                    at(
                        EventKind::ChecksRun,
                        ChecksRunPayload {
                            step: "review".into(),
                            checks: vec![],
                        },
                    ),
                ],
            ),
            (
                "a checkpoint answered with nothing open",
                vec![
                    run_started(),
                    checkpoint_answered(CheckpointAnswer::Go, None),
                ],
            ),
            (
                "a push with no step behind it",
                vec![run_started(), pushed(&["abc123"])],
            ),
            (
                "a step warning with no step to warn about",
                vec![run_started(), budget_warned(BudgetScope::Step, 1.0, 3.0)],
            ),
            ("a second run_started", vec![run_started(), run_started()]),
        ];
        for (what, log) in cases {
            let log = numbered(log);
            match run_state(&log) {
                Err(LogError::RunOrdering { .. }) => {}
                other => panic!("{what} should be an ordering error, got {other:?}"),
            }
        }
    }

    /// T21: the run's cost is the sum of its steps' `step_finished`
    /// costs.
    #[test]
    fn cost_is_the_sum_of_the_step_finished_costs() {
        let st = run_state(&numbered(vec![
            run_started(),
            step_started(1, child()),
            step_finished(StepStatus::Done, 0.4),
            checks_run(vec![check("E1", CheckResult::Fail)]),
            step_started(2, child()),
            step_finished(StepStatus::Done, 0.35),
            checks_run(vec![check("E1", CheckResult::Pass)]),
            pushed(&["abc123"]),
            run_finished(RunOutcome::Closed),
        ]))
        .unwrap();
        let summed: f64 = st.steps.iter().map(|s| s.cost_usd).sum();
        assert_eq!(st.cost_usd, summed);
        assert!((st.cost_usd - 0.75).abs() < 1e-9, "{}", st.cost_usd);
        assert_eq!(st.steps[0].cost_usd, 0.4);
        assert_eq!(st.steps[1].cost_usd, 0.35);
    }

    /// The budget is the latest the log sets: `run_started`'s
    /// provisional one, overridden by the brief's route.
    #[test]
    fn the_latest_budget_wins() {
        let mut route = route_taken("full");
        route.payload["budget_usd"] = serde_json::json!(7.5);
        let st = run_state(&numbered(vec![run_started(), route])).unwrap();
        assert_eq!(st.budget_usd, 7.5);
    }
}
