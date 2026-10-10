//! A watch: one step of a running issue, opened read-only
//! inside the front thread. The step's own events draw in the dev view
//! through its own engine, the run's figures and its gate come with it,
//! and its moves are read from the lead's log.

use aigentic_api::client::Client;
use aigentic_api::{Notice, Request, Response, RunThread, ThreadKind};
use aigentic_runtime::aigentic_core::{Event, EventKind};
use aigentic_runtime::aigentic_log::{StepStartedPayload, run_state};
use ulid::Ulid;

use super::engine::{ClientRepl, Identity, Printer};

/// What `/watch` was asked for: a step by name, or the run's own step,
/// followed as the run moves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Target {
    Step(String),
    Pin,
}

/// A watch that cannot be opened, with the line to print.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WatchRefused(pub(crate) String);

/// What a folded lead event means for a watch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Step {
    Stay,
    /// The watch moved to another child: `target` is the watch's own
    /// target, unchanged — a named target never changes step.
    Moved {
        issue: u64,
        target: Target,
        child: Ulid,
        step: String,
    },
    /// The watched run finished, and this watch followed the run itself.
    Ended,
}

/// The issue `/watch` names, from its first argument.
pub(crate) fn parse(arg: &str) -> Result<u64, String> {
    arg.split_whitespace()
        .next()
        .unwrap_or("")
        .parse::<u64>()
        .map_err(|_| "[watch: expected an issue number: /watch <issue> [step]]".to_owned())
}

/// The target the rest of `/watch`'s line names: a step name, `pin`, or
/// nothing at all — which is the run's own step.
pub(crate) fn target(arg: &str) -> Result<Target, String> {
    let mut words = arg.split_whitespace().skip(1);
    match (words.next(), words.next()) {
        (None, _) | (Some("pin"), None) => Ok(Target::Pin),
        (Some(step), None) => Ok(Target::Step(step.to_owned())),
        _ => Err("[watch: one step name, and only one: /watch <issue> [step]]".to_owned()),
    }
}

/// The step a watch opens, and its live child, read from the lead's log
/// without writing anything.
///
/// `Target::Step` takes the latest attempt of that step, whether or not
/// the run is still on it; `Target::Pin` takes the run's own open step. A
/// log that is no run, a run that has ended, and a step the log never
/// started all refuse.
pub(crate) fn watchable(
    issue: u64,
    lead_events: &[Event],
    target: &Target,
) -> Result<(String, Ulid), WatchRefused> {
    let named = match target {
        Target::Step(name) => Some(name.clone()),
        Target::Pin => None,
    };
    let no_child = |step: &str| refused_child(issue, step);
    if lead_events.iter().any(|e| e.kind == EventKind::RunFinished) {
        return Err(no_child(named.as_deref().unwrap_or("pin")));
    }
    let Ok(state) = run_state(lead_events) else {
        return Err(refused_no_run(issue));
    };
    let record = match target {
        Target::Step(name) => state.steps.iter().rev().find(|r| &r.step == name),
        Target::Pin => state
            .steps
            .iter()
            .rev()
            .find(|r| r.status.is_none())
            .or_else(|| state.steps.last()),
    };
    match record {
        Some(record) => Ok((record.step.clone(), record.child_thread)),
        None => Err(match named {
            Some(name) => no_child(&name),
            None => refused_no_run(issue),
        }),
    }
}

/// The refusal a step with no live child draws: the run may have moved on
/// since the watch was asked for.
pub(crate) fn refused_child(issue: u64, step: &str) -> WatchRefused {
    WatchRefused(format!(
        "[watch: #{issue}'s step {step} has no live child (the run may have moved: a plain \
         /watch follows the current step, pin the name to keep it)]"
    ))
}

/// The refusal a watch draws when the project holds no run of the issue.
pub(crate) fn refused_no_run(issue: u64) -> WatchRefused {
    WatchRefused(format!("[watch: no run of issue #{issue} in this project]"))
}

/// A watch the daemon refused, or one the wire could not answer: the
/// daemon's own words, so a refusal is never mistaken for a local guess.
pub(crate) fn refused_by_daemon(why: impl std::fmt::Display) -> WatchRefused {
    WatchRefused(format!("[error: {why}]"))
}

/// The run's lead thread and its log, from the issue it builds: the run
/// is named by the listing, then read without writing.
async fn read_lead(client: &Client, issue: u64) -> Result<(Ulid, Vec<Event>), WatchRefused> {
    let threads = match client.request(Request::ListThreads { project: None }).await {
        Ok(Response::Threads { threads }) => threads,
        Ok(Response::Refused { reason }) => return Err(refused_by_daemon(reason)),
        Ok(other) => return Err(refused_by_daemon(format!("unexpected reply: {other:?}"))),
        Err(e) => return Err(refused_by_daemon(e)),
    };
    let lead = threads
        .into_iter()
        .find(|t| t.kind == ThreadKind::Run(RunThread::Lead { issue }))
        .map(|t| t.id);
    let Some(lead) = lead else {
        return Err(refused_no_run(issue));
    };
    match client
        .request(Request::Open {
            thread: lead,
            from_seq: 0,
        })
        .await
    {
        Ok(Response::Opened { events, .. }) => Ok((lead, events)),
        Ok(Response::Refused { reason }) => Err(refused_by_daemon(reason)),
        Ok(other) => Err(refused_by_daemon(format!("unexpected reply: {other:?}"))),
        Err(e) => Err(refused_by_daemon(e)),
    }
}

/// The `step_started` a child was opened from, and its step and attempt.
pub(crate) fn step_started(event: &Event) -> Option<StepStartedPayload> {
    if event.kind != EventKind::StepStarted {
        return None;
    }
    serde_json::from_value(event.payload.clone()).ok()
}

/// What the lead events folded *after* `seen` mean for a watch.
///
/// `target` is the watch's own: a named target follows a new attempt or
/// child of its own step and ignores every other step, so a pin never
/// changes step. `Target::Pin` follows the run's own step wherever it
/// goes. The newest `step_started` decides; a `run_finished` ends a watch
/// that follows the run and leaves a named one alone.
pub(crate) fn advance(target: &Target, issue: u64, seen: &Event, since: &[Event]) -> Step {
    let seen_child = step_started(seen).map(|p| p.child_thread);
    for event in since.iter().rev() {
        match event.kind {
            EventKind::RunFinished => {
                return match target {
                    Target::Step(_) => Step::Stay,
                    Target::Pin => Step::Ended,
                };
            }
            EventKind::StepStarted => {
                let Some(p) = step_started(event) else {
                    continue;
                };
                if matches!(target, Target::Step(name) if name != &p.step) {
                    continue;
                }
                if Some(p.child_thread) == seen_child {
                    return Step::Stay;
                }
                return Step::Moved {
                    issue,
                    target: target.clone(),
                    child: p.child_thread,
                    step: p.step,
                };
            }
            _ => {}
        }
    }
    Step::Stay
}

/// The figures a watch takes from a pushed status: the watched run's own,
/// never another issue's, so a status for a run the person is following
/// elsewhere does not become the watch's. `None` before one arrives.
pub(crate) fn figures_after(
    issue: u64,
    current: Option<&str>,
    status: &aigentic_api::RunStatus,
) -> Option<String> {
    if status.issue != issue {
        return current.map(str::to_owned);
    }
    Some(crate::app::status::run_figures(status))
}

/// A step of a running issue, open inside the front thread: the child's
/// own engine, the run's figures as the daemon pushes them, and the lead
/// events folded for the watch's moves.
pub(crate) struct Watch {
    issue: u64,
    target: Target,
    lead: Ulid,
    child_thread: Ulid,
    /// Every lead event the watch has been given, in order, and the event
    /// the watch is on: the moves are read between the two, so a repeated
    /// fold cannot move the watch twice.
    lead_events: Vec<Event>,
    seen_at: usize,
    step: String,
    /// The step's own engine: its events draw in the dev view, its menu
    /// takes keys, and `/answer` reaches the gate through it.
    child: Box<ClientRepl>,
    /// The last child `seq` drawn; `None` before the backlog is drawn, so
    /// a child whose log starts at `seq` 0 still draws.
    printed: Option<u64>,
    figures: Option<String>,
    /// The child a move left to open: opening one is a request, so it
    /// happens after the notices a batch carried, not inside the fold.
    pending: Option<Ulid>,
}

impl Watch {
    /// Open `/watch`'s target: read the run's log, find the step's child,
    /// and open the child read-only with its backlog.
    pub(crate) async fn enter(
        client: &Client,
        user: &str,
        role: Option<String>,
        project: &str,
        issue: u64,
        target: Target,
        out: &mut dyn Printer,
    ) -> Result<Watch, WatchRefused> {
        let (lead, lead_events) = read_lead(client, issue).await?;
        let (step, child_thread) = watchable(issue, &lead_events, &target)?;
        let opened = match client
            .request(Request::Open {
                thread: child_thread,
                from_seq: 0,
            })
            .await
        {
            Ok(Response::Opened {
                events,
                state,
                mode,
                profile,
                model,
                effort,
                ..
            }) => (events, state, mode, profile, model, effort),
            // A step the log still lists but whose child is gone draws the
            // same line as a step with no child at all.
            Ok(_) | Err(_) => return Err(refused_child(issue, &step)),
        };
        let (backlog, state, mode, profile, model, effort) = opened;
        let identity = Identity {
            profile,
            model,
            effort,
        };
        let child = ClientRepl::new(
            client.clone(),
            child_thread,
            user,
            role,
            state,
            mode,
            identity,
            project,
        );
        let seen_at = lead_events
            .iter()
            .rposition(|event| step_started(event).is_some_and(|p| p.child_thread == child_thread))
            .ok_or_else(|| refused_child(issue, &step))?;
        let mut watch = Watch {
            issue,
            target,
            lead,
            child_thread,
            lead_events,
            seen_at,
            step,
            child: Box::new(child),
            printed: None,
            figures: None,
            pending: None,
        };
        watch.draw_backlog(&backlog, out);
        Ok(watch)
    }

    /// The watch's issue.
    pub(crate) fn issue(&self) -> u64 {
        self.issue
    }

    /// The watched run's lead.
    pub(crate) fn lead(&self) -> Ulid {
        self.lead
    }

    /// The watched step's own thread.
    pub(crate) fn child_thread(&self) -> Ulid {
        self.child_thread
    }

    /// `#140 spec`: the watch's issue and the step it is on.
    fn label(&self) -> String {
        let step = if self.step.is_empty() {
            "pin"
        } else {
            self.step.as_str()
        };
        format!("#{} {}", self.issue, step)
    }

    /// What the status line shows in place of the front thread's parts.
    pub(crate) fn inside(&self) -> String {
        format!("inside {}", self.label())
    }

    /// What a typed line inside the watch is refused with.
    pub(crate) fn refusal(&self) -> String {
        format!(
            "[watching {} read-only: /back to talk to the front thread]",
            self.label()
        )
    }

    /// Where the front thread resumes.
    pub(crate) fn divider(&self) -> String {
        "── back at the front thread ──".to_owned()
    }

    /// Where the watch announces the step it moved to.
    pub(crate) fn moved_divider(&self) -> String {
        format!("── {} ──", self.label())
    }

    /// The watched run's figures, as the daemon last pushed them.
    pub(crate) fn figures(&self) -> Option<&str> {
        self.figures.as_deref()
    }

    /// The step's engine, for the pane and the answering keys.
    pub(crate) fn child(&mut self) -> &mut ClientRepl {
        &mut self.child
    }

    /// The step's engine, read only.
    pub(crate) fn child_ref(&self) -> &ClientRepl {
        &self.child
    }

    /// Draw the child's backlog: every event, in record order, through the
    /// child's own engine — the dev view's cells, exactly as a live event
    /// of that thread would draw.
    pub(crate) fn draw_backlog(&mut self, events: &[Event], out: &mut dyn Printer) {
        for event in events {
            self.printed = Some(event.seq);
            self.child.render(
                Notice::Event {
                    thread: self.child_thread,
                    event: event.clone(),
                },
                out,
            );
        }
    }

    /// One live notice for the watched child: the `seq <= printed` rule
    /// keeps a notice the backlog already drew from drawing twice.
    pub(crate) fn render_child(&mut self, event: Event, out: &mut dyn Printer) {
        if self.printed.is_some_and(|printed| event.seq <= printed) {
            return;
        }
        self.printed = Some(event.seq);
        self.child.render(
            Notice::Event {
                thread: self.child_thread,
                event,
            },
            out,
        );
    }

    /// Fold a lead event into the watch, draw whatever gate it carries,
    /// and say what the watch does next.
    pub(crate) fn absorb(&mut self, event: Event, out: &mut dyn Printer) -> Step {
        // A gate on the lead is the gate the run waits at: it is shown
        // inside, and answered from here.
        let lead = self.lead;
        self.child.gate_event(&event, lead, out);
        self.lead_events.push(event);
        let lead_events = &self.lead_events;
        let step = advance(
            &self.target,
            self.issue,
            &lead_events[self.seen_at],
            &lead_events[self.seen_at + 1..],
        );
        match &step {
            Step::Stay | Step::Ended => {}
            Step::Moved { child, step, .. } => {
                self.step = step.clone();
                self.pending = Some(*child);
            }
        }
        step
    }

    /// The child a move left to open, once: the shell opens it with
    /// `settle` after the notices it drained.
    pub(crate) fn take_pending(&mut self) -> Option<Ulid> {
        self.pending.take()
    }

    /// Take the new child of a move: open it read-only, draw its backlog,
    /// and read the moved-to `step_started` off the folded log.
    pub(crate) async fn moved(
        &mut self,
        child: Ulid,
        out: &mut dyn Printer,
    ) -> Result<(), WatchRefused> {
        let opened = match self
            .child
            .client()
            .request(Request::Open {
                thread: child,
                from_seq: 0,
            })
            .await
        {
            Ok(Response::Opened {
                events,
                state,
                mode,
                profile,
                model,
                effort,
                ..
            }) => (events, state, mode, profile, model, effort),
            Ok(_) | Err(_) => return Err(refused_child(self.issue, &self.step)),
        };
        let (backlog, state, mode, profile, model, effort) = opened;
        let identity = Identity {
            profile,
            model,
            effort,
        };
        let child_at = self
            .lead_events
            .iter()
            .rposition(|event| step_started(event).is_some_and(|p| p.child_thread == child))
            .ok_or_else(|| refused_child(self.issue, &self.step))?;
        let project = self.child.project().unwrap_or_default().to_owned();
        let user = self.child.user().to_owned();
        let role = self.child.role().cloned();
        self.child_thread = child;
        self.seen_at = child_at;
        self.printed = None;
        *self.child = ClientRepl::new(
            self.child.client().clone(),
            child,
            &user,
            role,
            state,
            mode,
            identity,
            &project,
        );
        out.line(&self.moved_divider());
        self.draw_backlog(&backlog, out);
        Ok(())
    }

    /// Take a `RunStatus` for the watched run as the figures shown inside.
    pub(crate) fn status(&mut self, status: &aigentic_api::RunStatus) {
        self.figures = figures_after(self.issue, self.figures.as_deref(), status);
    }
}
