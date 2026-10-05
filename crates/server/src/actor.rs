//! One thread's actor: a task that owns the thread's `Runtime` and log
//! writer, takes mail in order, runs one turn at a time, and fans every
//! event, text delta and state change out to its subscribers. This is
//! the single writer the PRD's monothreading means. See
//! `docs/PLAN-phase5.md` sections 3 and 5.
//!
//! While a turn runs the actor keeps taking mail: a `Post` is handed to
//! the turn's inbox (in the log at once, in context from the next turn),
//! an interrupt also fires the turn's cancel token, a `Decide` or
//! `Answer` goes to the decisions table, and a `Subscribe` is answered
//! from the actor's mirror of the log. What needs the runtime itself
//! (pin, compact, set the mode, a report) waits for the turn to end.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use aigentic_api::{AskedOption, AskedQuestion, Notice, ReportKind, Response, ThreadState};
use aigentic_runtime::aigentic_core::{Author, ContentBlock, Event, EventKind};
use aigentic_runtime::aigentic_log::{
    PermissionRequestedPayload, UserMessagePayload, declined_at_startup,
};
use aigentic_runtime::{
    ASKED_HUMAN, Answered, CancelToken, Decisions, IdleProposal, Mode, Outbox, Pending, Queued,
    Resumed, Runtime, RuntimeError, Settled, Signal, SwitchAnswer, SwitchCtx, TurnOutcome,
    WindowUsage, inbox,
};
use tokio::sync::{mpsc, oneshot};

use crate::awake::KeepAwake;
use ulid::Ulid;

/// What a session sends the actor. Every request carries a reply slot
/// so the session can answer its client.
#[derive(Debug)]
pub enum Mail {
    Post {
        author: Author,
        blocks: Vec<ContentBlock>,
        interrupt: bool,
        reply: oneshot::Sender<Response>,
    },
    InvokeSkill {
        author: Author,
        name: String,
        args: String,
        reply: oneshot::Sender<Response>,
    },
    /// Cancel the running turn, nothing posted; refused while idle.
    Interrupt {
        by: Author,
        reply: oneshot::Sender<Response>,
    },
    Decide {
        by: Author,
        call_id: String,
        allow: bool,
        session: bool,
        prefix: Option<Vec<String>>,
        reason: Option<String>,
        reply: oneshot::Sender<Response>,
    },
    Answer {
        by: Author,
        call_id: String,
        text: String,
        reply: oneshot::Sender<Response>,
    },
    /// A person's answer to a `suggest_project` proposal (issue #7). A
    /// `yes` carries the target's context and the ack the session waits
    /// on; every other answer carries `SwitchCtx::none()`.
    AnswerSwitch {
        by: Author,
        call_id: String,
        answer: SwitchAnswer,
        ctx: SwitchCtx,
        reply: oneshot::Sender<Response>,
    },
    Pin {
        author: Author,
        text: String,
        reply: oneshot::Sender<Response>,
    },
    Remember {
        author: Author,
        text: String,
        reply: oneshot::Sender<Response>,
    },
    Rename {
        author: Author,
        title: String,
        reply: oneshot::Sender<Response>,
    },
    /// Swap in another project's context (built by the table); idle only.
    SwitchProject {
        ctx: Box<aigentic_runtime::ProjectContext>,
        by: Author,
        reply: oneshot::Sender<Response>,
    },
    /// Offer to switch the thread to `project` while it is idle (issue
    /// #92): the start-up proposal. Refused, writing nothing, when one
    /// is already waiting, when a turn is running, or when `project`
    /// has already been declined at start-up. Otherwise the proposal's
    /// `decision_proposed` is appended and the thread waits.
    ProposeSwitch {
        project: String,
        reason: String,
        reply: oneshot::Sender<Response>,
    },
    Compact {
        reply: oneshot::Sender<Response>,
    },
    SetMode {
        mode: Mode,
        reply: oneshot::Sender<Response>,
    },
    Report {
        kind: ReportKind,
        reply: oneshot::Sender<Response>,
    },
    /// The state now, for the table's idle sweep.
    Status {
        reply: oneshot::Sender<ThreadState>,
    },
    /// Events from `from_seq`, the state now, and every notice from
    /// here on.
    Subscribe {
        from_seq: u64,
        notices: mpsc::UnboundedSender<Notice>,
        /// The state, the events since `from_seq`, the mode's name,
        /// and the profile, model label and effort the thread runs
        /// with (issue #43).
        reply: oneshot::Sender<(ThreadState, Vec<Event>, String, Identity)>,
    },
}

/// Names the workspace a project sits in, for the switch notice (issue
/// #7). The table's `workspace_label`, as a seam so the actor is built
/// with it and tests can pass one of their own.
pub type WorkspaceLabel = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Who the thread runs as: the profile, its model and its effort
/// label, carried on `Subscribe` so a client's footer can name them
/// (issue #43).
pub type Identity = (Option<String>, String, Option<String>);

/// Renders the text reports a client prints (`/cost`, `/project`,
/// `/policy`, `/memory`, `/skills`); the daemon's step 9 wires the tui's
/// renderers in. `NoReports` is the default.
pub trait Reports: Send + Sync {
    fn render(&self, runtime: &Runtime, events: &[Event], kind: ReportKind) -> String;
}

pub struct NoReports;

impl Reports for NoReports {
    fn render(&self, _: &Runtime, _: &[Event], kind: ReportKind) -> String {
        format!("no renderer for the {kind:?} report on this daemon")
    }
}

/// How long a side job (memory extraction, a title) may hold the actor
/// after a turn before it is given up.
const SIDE_JOB_LIMIT: std::time::Duration = std::time::Duration::from_secs(60);

/// What the actor and the turn's observer share: the subscribers, the
/// mirror of the log, and the state.
struct Shared {
    thread: Ulid,
    subscribers: Mutex<Vec<mpsc::UnboundedSender<Notice>>>,
    events: Mutex<Vec<Event>>,
    state: Mutex<ThreadState>,
    /// Messages handed to the running turn and not yet read by a model
    /// call; reported in `Running`.
    queued: Mutex<u32>,
    /// Of those, the steered ones (issue #33): each is appended at a
    /// point the turn's next model call reads, so the count — and
    /// `queued` with it — drops when that call's message lands. A
    /// message posted mid-call steers nothing and is not here.
    steered: Mutex<u32>,
    /// Who posted last during the turn: the next turn is "by" them.
    last_queued_by: Mutex<Option<Author>>,
    /// The permission mode's name, for `Opened`.
    mode: Mutex<String>,
    /// The profile, model and effort the thread runs with, for
    /// `Opened` (issue #43). Replaced when a switch rebinds the
    /// provider.
    identity: Mutex<Identity>,
    /// When the running turn started; `None` while idle.
    started: Mutex<Option<Instant>>,
    /// The last window fill the runtime reported, re-sent when the queue
    /// changes or the turn ends so a status line stays current.
    last_usage: Mutex<Option<WindowUsage>>,
    /// Names the workspace a project sits in (issue #7), handed in when
    /// the actor is built: the table's `workspace_label`, so the actor
    /// never re-derives it. An `AwaitingSwitch` state carries it.
    labels: WorkspaceLabel,
    /// Set when the running turn switched the thread's project (issue
    /// #7): `after_turn` announces the new identity once, as an idle
    /// switch does.
    switched: Mutex<bool>,
    /// The keep-awake guard and whether a hold is outstanding (issue
    /// #47), under one lock. `held` keeps hold and release paired, so a
    /// turn interrupted while it was waiting cannot release twice, and
    /// the pair is swapped in when the daemon installs its guard; a
    /// test without one keeps the inert default.
    guard: Mutex<Hold>,
}

/// A guard, whether it is held right now, and whether the turn is
/// waiting on a person (issue #47). The last flag is what keeps a
/// mid-turn message that arrives during an unanswered question from
/// holding the machine awake again: a wait is not work.
struct Hold {
    guard: Arc<dyn KeepAwake>,
    held: bool,
    waiting: bool,
}

impl Shared {
    /// The turn is working: take the machine's assertion, once per
    /// working span (issue #47). `running` calls this, and `running` is
    /// called when a turn starts and whenever a wait ends, so the
    /// assertion is held exactly while work is happening.
    fn acquire(&self) {
        let mut hold = self.hold();
        // The wait is over, whatever put the thread back to work.
        hold.waiting = false;
        if hold.held {
            return;
        }
        hold.held = true;
        hold.guard.hold();
    }

    /// The turn is waiting on a person: not work, so nothing is held
    /// until whatever ends the wait calls `running` again.
    fn wait(&self) {
        let mut hold = self.hold();
        hold.waiting = true;
        if hold.held {
            hold.held = false;
            hold.guard.release();
        }
    }

    /// Whether the thread is waiting on a person right now.
    fn is_waiting(&self) -> bool {
        self.hold().waiting
    }

    /// The turn stopped working — it is waiting for a person, or it has
    /// ended: give the assertion back. Idempotent, so a turn that was
    /// already waiting when it ended gives nothing back twice.
    fn drop_guard(&self) {
        let mut hold = self.hold();
        if !hold.held {
            return;
        }
        hold.held = false;
        hold.guard.release();
    }

    fn hold(&self) -> std::sync::MutexGuard<'_, Hold> {
        self.guard.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Take the switch flag `after_turn` reads (issues #7, #92). A switch
    /// settled outside a turn must announce its identity and clear this
    /// itself, since `after_turn` never runs for it.
    fn take_switched(&self) -> bool {
        std::mem::take(&mut *self.switched.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// `Notice::Usage` from the last fill, the turn's elapsed time and
    /// the queue; nothing until the first model call reported a fill.
    fn push_usage(&self) {
        let Some(usage) = *self.last_usage.lock().unwrap_or_else(|e| e.into_inner()) else {
            return;
        };
        let started = *self.started.lock().unwrap_or_else(|e| e.into_inner());
        let queued = *self.queued.lock().unwrap_or_else(|e| e.into_inner());
        self.broadcast(Notice::Usage {
            thread: self.thread,
            tokens_in_window: usage.tokens_in_window,
            window: usage.window,
            turn_elapsed_ms: started.map(|t| t.elapsed().as_millis() as u64),
            queued,
        });
    }

    fn broadcast(&self, notice: Notice) {
        let mut subs = self.subscribers.lock().unwrap_or_else(|e| e.into_inner());
        subs.retain(|tx| tx.send(notice.clone()).is_ok());
    }

    fn set_state(&self, state: ThreadState) {
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = state.clone();
        self.broadcast(Notice::State {
            thread: self.thread,
            state,
        });
    }

    fn state(&self) -> ThreadState {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn running(&self, by: Author) {
        // Work is happening (a turn started, or a wait ended): hold the
        // machine awake until the turn stops working.
        self.acquire();
        let queued = *self.queued.lock().unwrap_or_else(|e| e.into_inner());
        self.set_state(ThreadState::Running { by, queued });
    }

    /// The turn's observer: mirror and fan out every event, stream text
    /// and tool starts, and turn a wait into a state.
    fn observe(&self, signal: Signal<'_>, turn_by: &Author) {
        match signal {
            Signal::Event(event) => {
                self.events
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(event.clone());
                self.broadcast(Notice::Event {
                    thread: self.thread,
                    event: event.clone(),
                });
                // A message that arrived mid-turn: count it and say so,
                // after its event, so subscribers see both in order. A
                // steered one (issue #33) is read at the turn's next
                // model call; one posted mid-call waits for the next
                // turn.
                if event.kind == EventKind::UserMessage
                    && let Ok(p) =
                        serde_json::from_value::<UserMessagePayload>(event.payload.clone())
                    && p.mid_turn
                {
                    *self.queued.lock().unwrap_or_else(|e| e.into_inner()) += 1;
                    if p.steer {
                        *self.steered.lock().unwrap_or_else(|e| e.into_inner()) += 1;
                    }
                    // Unless the turn is waiting on a person: a
                    // question left unanswered is not work, and the
                    // message must not put the guard back up (issue
                    // #47).
                    if !self.is_waiting() {
                        self.running(turn_by.clone());
                    }
                    self.push_usage();
                    return;
                }
                // The assistant message just appended is the model's
                // answer to a call that read every steered message so
                // far (each sits in the log before the call that reads
                // it), so those have reached the agent: they leave the
                // queue, and the state says so.
                if event.kind == EventKind::AssistantMessage {
                    let steered = *self.steered.lock().unwrap_or_else(|e| e.into_inner());
                    if steered > 0 {
                        *self.steered.lock().unwrap_or_else(|e| e.into_inner()) = 0;
                        *self.queued.lock().unwrap_or_else(|e| e.into_inner()) -= steered;
                        self.running(turn_by.clone());
                        self.push_usage();
                    }
                }
                // A switch inside the turn (issue #7) rebinds the
                // provider and the profile; `after_turn` announces it.
                if event.kind == EventKind::ProjectSwitched {
                    *self.switched.lock().unwrap_or_else(|e| e.into_inner()) = true;
                }
                // A decision or a result ends a wait.
                let waiting = !matches!(self.state(), ThreadState::Running { .. });
                if waiting
                    && matches!(
                        event.kind,
                        EventKind::PermissionDecided
                            | EventKind::ToolResult
                            | EventKind::DecisionAnswered
                    )
                {
                    self.running(turn_by.clone());
                }
            }
            Signal::Usage(usage) => {
                *self.last_usage.lock().unwrap_or_else(|e| e.into_inner()) = Some(usage);
                self.push_usage();
            }
            Signal::Note(text) => self.broadcast(Notice::Note {
                thread: self.thread,
                text,
            }),
            Signal::TextDelta(text) => self.broadcast(Notice::TextDelta {
                thread: self.thread,
                text: text.to_owned(),
            }),
            Signal::ToolCallStarted(call) => self.broadcast(Notice::ToolCallStarted {
                thread: self.thread,
                call: call.clone(),
            }),
            // A wait is not work: a person may take minutes, and the
            // machine must be free to sleep meanwhile (issue #47).
            Signal::Waiting(pending) => {
                self.wait();
                self.set_state(match pending {
                    Pending::Permission { call_id, request } => {
                        let PermissionRequestedPayload {
                            call,
                            class,
                            reason,
                        } = request.clone();
                        ThreadState::AwaitingApproval {
                            call_id: call_id.clone(),
                            call,
                            class,
                            reason,
                        }
                    }
                    Pending::Switch {
                        call_id,
                        project,
                        reason,
                    } => ThreadState::AwaitingSwitch {
                        call_id: call_id.clone(),
                        project: project.clone(),
                        workspace: (self.labels)(project),
                        reason: reason.clone(),
                    },
                    Pending::Human {
                        call_id,
                        question,
                        questions,
                    } => ThreadState::AwaitingHuman {
                        call_id: call_id.clone(),
                        question: question.clone(),
                        questions: questions
                            .iter()
                            .map(|q| AskedQuestion {
                                question: q.question.clone(),
                                header: q.header.clone(),
                                options: q
                                    .options
                                    .iter()
                                    .map(|o| AskedOption {
                                        label: o.label.clone(),
                                        description: o.description.clone(),
                                    })
                                    .collect(),
                                multi: q.multi,
                            })
                            .collect(),
                    },
                })
            }
        }
    }
}

/// How a turn starts.
enum Start {
    /// A user message, then the loop.
    Post(Author, Vec<ContentBlock>),
    /// A user-invoked skill.
    Skill(Author, String, String),
    /// The loop alone: queued messages, an answered question, a resume.
    Continue(Author),
}

pub struct ThreadActor {
    runtime: Runtime,
    decisions: Arc<Decisions>,
    shared: Arc<Shared>,
    reports: Arc<dyn Reports>,
    rx: mpsc::UnboundedReceiver<Mail>,
    /// A `Compact` taken while idle runs after the mail loop yields.
    pending_compact: Option<oneshot::Sender<Response>>,
    /// The idle start-up proposal waiting for an answer (issue #92):
    /// its call id, the `decision_proposed` event, and the project it
    /// offers. `None` when nothing is waiting.
    idle_switch: Option<IdleSwitch>,
}

/// An idle start-up switch proposal and the answer `AnswerSwitch` must
/// name to settle it (issue #92).
struct IdleSwitch {
    call_id: String,
    proposal: Ulid,
    project: String,
}

/// The sending side of an actor's mailbox. Cloned per session.
pub type Mailbox = mpsc::UnboundedSender<Mail>;

impl ThreadActor {
    /// Take a runtime built for the thread (its `Decisions` installed
    /// here) and run phase 2's resume before the first mail is read.
    /// `torn` is what `ThreadLog::open_with` repaired.
    pub fn new(
        mut runtime: Runtime,
        torn: Option<u64>,
        reports: Arc<dyn Reports>,
        labels: WorkspaceLabel,
    ) -> Result<(Self, Mailbox), aigentic_runtime::RuntimeError> {
        let decisions = Arc::new(Decisions::new());
        runtime = runtime.with_decisions(decisions.clone());
        let thread = runtime.log().thread_id();
        let shared = Arc::new(Shared {
            thread,
            subscribers: Mutex::new(Vec::new()),
            started: Mutex::new(None),
            last_usage: Mutex::new(None),
            events: Mutex::new(runtime.log().read_all()?),
            state: Mutex::new(ThreadState::Idle),
            queued: Mutex::new(0),
            steered: Mutex::new(0),
            last_queued_by: Mutex::new(None),
            mode: Mutex::new(runtime.mode().name().to_owned()),
            identity: Mutex::new(runtime.identity()),
            labels,
            switched: Mutex::new(false),
            guard: Mutex::new(Hold {
                guard: Arc::new(crate::awake::ProcessGuard::off()),
                held: false,
                waiting: false,
            }),
        });
        let (tx, rx) = mpsc::unbounded_channel();
        let mut actor = Self {
            runtime,
            decisions,
            shared,
            reports,
            rx,
            pending_compact: None,
            idle_switch: None,
        };
        actor.resume(torn)?;
        Ok((actor, tx))
    }

    /// Install the daemon's keep-awake guard (issue #47): from here on a
    /// working turn holds the machine awake, and what the guard was
    /// doing at the end of a turn lands in the thread's log. The reader
    /// is read when a turn ends, never earlier, so a guard that failed
    /// mid-run is reported by the turn it affected.
    pub fn with_keep_awake(mut self, guard: Arc<dyn KeepAwake>) -> Self {
        self.runtime.set_keep_awake(crate::awake::reader(&guard));
        *self.shared.hold() = Hold {
            guard,
            held: false,
            waiting: false,
        };
        self
    }

    /// Phase 2's resume: repair events are appended (and mirrored) and,
    /// when the log ended mid-turn or on an answered question, the first
    /// thing the actor does is continue that turn.
    /// Tell the subscribers which profile, model and effort the thread
    /// now runs with (issue #43), and remember it for the next
    /// `Opened`.
    fn announce_identity(&mut self) {
        let identity = self.runtime.identity();
        let (profile, model, effort) = identity.clone();
        *self
            .shared
            .identity
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = identity;
        self.shared.broadcast(Notice::Model {
            thread: self.shared.thread,
            profile,
            model,
            effort,
        });
    }

    fn resume(&mut self, torn: Option<u64>) -> Result<(), aigentic_runtime::RuntimeError> {
        let shared = self.shared.clone();
        let system = Author::System;
        let resumed = self
            .runtime
            .resume(torn, &mut |s| shared.observe(s, &system))?;
        let pending = matches!(resumed, Resumed::Interrupted { .. })
            || self.runtime.awaiting_continuation()?;
        if pending {
            // Read by `run` before its first mail.
            *self.shared.queued.lock().unwrap_or_else(|e| e.into_inner()) = 1;
        }
        Ok(())
    }

    /// The loop. Ends when every mailbox is dropped.
    pub async fn run(mut self) {
        // A resume that left work: continue it first.
        let queued = *self.shared.queued.lock().unwrap_or_else(|e| e.into_inner());
        if queued > 0 {
            *self.shared.queued.lock().unwrap_or_else(|e| e.into_inner()) = 0;
            self.turn(Start::Continue(Author::System)).await;
        }
        loop {
            let Some(mail) = self.rx.recv().await else {
                break;
            };
            if let Some(start) = self.handle_idle(mail).await {
                self.turn(start).await;
            }
            if let Some(reply) = self.pending_compact.take() {
                self.compact(reply).await;
            }
        }
    }

    /// `Compact` while idle: run the rules now, fanning out what they
    /// append.
    async fn compact(&mut self, reply: oneshot::Sender<Response>) {
        let shared = self.shared.clone();
        let sys = Author::System;
        let _ = reply.send(
            match self
                .runtime
                .compact_now(&mut |s| shared.observe(s, &sys))
                .await
            {
                Ok(_) => Response::Ok,
                Err(e) => Response::Error {
                    message: e.to_string(),
                },
            },
        );
    }

    /// Mail while idle. Returns a turn to start, if the mail starts one.
    ///
    /// Async since issue #92: raising, answering and withdrawing a
    /// start-up proposal goes through the async runtime and appends
    /// events.
    async fn handle_idle(&mut self, mail: Mail) -> Option<Start> {
        match mail {
            Mail::Post {
                author,
                blocks,
                reply,
                ..
            } => {
                self.withdraw_idle_switch("a message was sent instead")
                    .await;
                let _ = reply.send(Response::Ok);
                Some(Start::Post(author, blocks))
            }
            Mail::InvokeSkill {
                author,
                name,
                args,
                reply,
            } => {
                self.withdraw_idle_switch("a skill was run instead").await;
                let _ = reply.send(Response::Ok);
                Some(Start::Skill(author, name, args))
            }
            Mail::Decide { call_id, reply, .. } | Mail::Answer { call_id, reply, .. } => {
                let _ = reply.send(Response::Refused {
                    reason: format!("nothing is pending for call {call_id}"),
                });
                None
            }
            Mail::AnswerSwitch {
                by,
                call_id,
                answer,
                ctx,
                reply,
            } => {
                self.answer_idle_switch(by, call_id, answer, ctx, reply)
                    .await;
                None
            }
            Mail::ProposeSwitch {
                project,
                reason,
                reply,
            } => {
                self.propose_switch_idle(project, reason, reply).await;
                None
            }
            Mail::Pin {
                author,
                text,
                reply,
            } => {
                let shared = self.shared.clone();
                let sys = Author::System;
                let _ = reply.send(
                    match self
                        .runtime
                        .pin(author, text, &mut |s| shared.observe(s, &sys))
                    {
                        Ok(_) => Response::Ok,
                        Err(e) => Response::Error {
                            message: e.to_string(),
                        },
                    },
                );
                None
            }
            Mail::Remember {
                author,
                text,
                reply,
            } => {
                let shared = self.shared.clone();
                let sys = Author::System;
                let _ = reply.send(
                    match self
                        .runtime
                        .remember(author, &text, &mut |s| shared.observe(s, &sys))
                    {
                        Ok(()) => Response::Ok,
                        Err(e) => Response::Error {
                            message: e.to_string(),
                        },
                    },
                );
                None
            }
            Mail::Rename {
                author,
                title,
                reply,
            } => {
                let shared = self.shared.clone();
                let sys = Author::System;
                let _ = reply.send(
                    match self
                        .runtime
                        .rename(author, &title, &mut |s| shared.observe(s, &sys))
                    {
                        Ok(()) => Response::Ok,
                        Err(e) => Response::Error {
                            message: e.to_string(),
                        },
                    },
                );
                None
            }
            Mail::SwitchProject { ctx, by, reply } => {
                self.withdraw_idle_switch("the project was switched by hand")
                    .await;
                let shared = self.shared.clone();
                let sys = Author::System;
                let sent = match self
                    .runtime
                    .set_project(*ctx, by, &mut |s| shared.observe(s, &sys))
                {
                    Ok(()) => {
                        // The new project's profile may run a different
                        // model: tell the subscribers (issue #43).
                        self.announce_identity();
                        Response::Ok
                    }
                    Err(e) => Response::Error {
                        message: e.to_string(),
                    },
                };
                let _ = reply.send(sent);
                None
            }
            Mail::Interrupt { reply, .. } => {
                let _ = reply.send(Response::Refused {
                    reason: "no turn is running".into(),
                });
                None
            }
            Mail::Compact { reply } => {
                // Async; `run` does it after this mail.
                self.pending_compact = Some(reply);
                None
            }
            Mail::SetMode { mode, reply } => {
                self.runtime.set_mode(mode);
                *self.shared.mode.lock().unwrap_or_else(|e| e.into_inner()) =
                    mode.name().to_owned();
                self.shared.broadcast(Notice::Mode {
                    thread: self.shared.thread,
                    mode: mode.name().to_owned(),
                });
                let _ = reply.send(Response::Ok);
                None
            }
            Mail::Report { kind, reply } => {
                let events = self
                    .shared
                    .events
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                let text = self.reports.render(&self.runtime, &events, kind);
                let _ = reply.send(Response::Text { text });
                None
            }
            Mail::Subscribe {
                from_seq,
                notices,
                reply,
            } => {
                self.subscribe(from_seq, notices, reply);
                None
            }
            Mail::Status { reply } => {
                let _ = reply.send(self.shared.state());
                None
            }
        }
    }

    /// Raise the start-up switch proposal while idle (issue #92), as
    /// `Mail::ProposeSwitch` asks. Refused, writing nothing, when one is
    /// already waiting, when a turn is running, or when `project` has
    /// been declined at start-up before. The proposal goes through the
    /// real observer, so the thread's events and its subscribers learn
    /// of it as they would of any write.
    async fn propose_switch_idle(
        &mut self,
        project: String,
        reason: String,
        reply: oneshot::Sender<Response>,
    ) {
        if self.idle_switch.is_some() {
            let _ = reply.send(Response::Refused {
                reason: "a proposal is already waiting".into(),
            });
            return;
        }
        if !matches!(self.shared.state(), ThreadState::Idle) {
            let _ = reply.send(Response::Refused {
                reason: "a turn is running".into(),
            });
            return;
        }
        let declined = declined_at_startup(
            &self.shared.events.lock().unwrap_or_else(|e| e.into_inner()),
            &project,
        );
        if declined {
            let _ = reply.send(Response::Refused {
                reason: "declined before".into(),
            });
            return;
        }
        let shared = self.shared.clone();
        let sys = Author::System;
        let proposed = self
            .runtime
            .propose_switch_idle(&project, &reason, &mut |s| shared.observe(s, &sys))
            .await;
        match proposed {
            Ok(IdleProposal { call_id, proposal }) => {
                let waiting = IdleSwitch {
                    call_id,
                    proposal,
                    project,
                };
                // The waiting proposal drives the state it raises: its
                // own call id and project, not the mail's copy.
                let state = ThreadState::AwaitingSwitch {
                    call_id: waiting.call_id.clone(),
                    project: waiting.project.clone(),
                    workspace: (self.shared.labels)(&waiting.project),
                    reason,
                };
                self.idle_switch = Some(waiting);
                self.shared.set_state(state);
                let _ = reply.send(Response::Ok);
            }
            Err(e) => {
                let _ = reply.send(Response::Error {
                    message: e.to_string(),
                });
            }
        }
    }

    /// Settle the waiting start-up proposal (issue #92), as
    /// `Mail::AnswerSwitch` asks while idle. A `call_id` that names no
    /// waiting proposal keeps the old refusal.
    async fn answer_idle_switch(
        &mut self,
        by: Author,
        call_id: String,
        answer: SwitchAnswer,
        ctx: SwitchCtx,
        reply: oneshot::Sender<Response>,
    ) {
        let matches = self
            .idle_switch
            .as_ref()
            .is_some_and(|w| w.call_id == call_id);
        if !matches {
            let _ = reply.send(Response::Refused {
                reason: format!("nothing is pending for call {call_id}"),
            });
            return;
        }
        let proposal = self
            .idle_switch
            .as_ref()
            .map(|w| w.proposal)
            .expect("checked above");
        let shared = self.shared.clone();
        let sys = Author::System;
        let settled = self
            .runtime
            .answer_switch_idle(proposal, answer, by, ctx, &mut |s| shared.observe(s, &sys))
            .await;
        // The proposal is no longer waiting whatever the outcome: clear
        // it and return to idle, releasing the keep-awake hold the way
        // `after_turn` would (#47), since no turn runs here.
        self.idle_switch = None;
        self.shared.set_state(ThreadState::Idle);
        self.shared.take_switched();
        self.shared.drop_guard();
        match settled {
            Ok(Settled::Switched) => {
                // The project changed with no turn to announce it.
                self.announce_identity();
                let _ = reply.send(Response::Ok);
            }
            Ok(_) => {
                let _ = reply.send(Response::Ok);
            }
            Err(e) => {
                let _ = reply.send(Response::Error {
                    message: e.to_string(),
                });
            }
        }
    }

    /// Withdraw a waiting start-up proposal before a mail that would
    /// otherwise leave it unanswered (issue #92). Always clears it: a
    /// withdrawal that fails, because another path already answered,
    /// only loses the note, never the mail that withdrew it.
    async fn withdraw_idle_switch(&mut self, note: &str) {
        let Some(waiting) = self.idle_switch.take() else {
            return;
        };
        let shared = self.shared.clone();
        let sys = Author::System;
        let _ = self
            .runtime
            .answer_switch_idle(
                waiting.proposal,
                SwitchAnswer::Withdrawn(note.to_owned()),
                Author::System,
                SwitchCtx::none(),
                &mut |s| shared.observe(s, &sys),
            )
            .await;
        self.shared.set_state(ThreadState::Idle);
        self.shared.take_switched();
        self.shared.drop_guard();
    }

    fn subscribe(
        &self,
        from_seq: u64,
        notices: mpsc::UnboundedSender<Notice>,
        reply: oneshot::Sender<(ThreadState, Vec<Event>, String, Identity)>,
    ) {
        // Snapshot and register under one lock, so no event is both
        // missed and unsent.
        let mut subs = self
            .shared
            .subscribers
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let events: Vec<Event> = self
            .shared
            .events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|e| e.seq >= from_seq)
            .cloned()
            .collect();
        subs.push(notices);
        drop(subs);
        let mode = self
            .shared
            .mode
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let identity = self.runtime.identity();
        let _ = reply.send((self.shared.state(), events, mode, identity));
    }

    /// Run one turn, taking mail throughout, then any turns the mail
    /// queued behind it.
    async fn turn(&mut self, start: Start) {
        let mut next = Some(start);
        while let Some(start) = next.take() {
            let by = match &start {
                Start::Post(a, _) | Start::Skill(a, _, _) | Start::Continue(a) => a.clone(),
            };
            *self.shared.queued.lock().unwrap_or_else(|e| e.into_inner()) = 0;
            // A steered message the previous turn never read is in this
            // one's context; when it answers, there is nothing to drop.
            *self
                .shared
                .steered
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = 0;
            *self
                .shared
                .started
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
            self.shared.running(by.clone());
            let cancel = CancelToken::never();
            let (outbox, mut inbox) = inbox();
            let shared = self.shared.clone();
            let observer_by = by.clone();
            let mut observe = move |s: Signal<'_>| shared.observe(s, &observer_by);
            let outcome = {
                let fut = async {
                    match start {
                        Start::Post(author, blocks) => {
                            self.runtime
                                .run_turn_until(author, blocks, &cancel, &mut inbox, &mut observe)
                                .await
                        }
                        Start::Skill(author, name, args) => {
                            self.runtime
                                .invoke_skill_until(
                                    author,
                                    &name,
                                    &args,
                                    &cancel,
                                    &mut inbox,
                                    &mut observe,
                                )
                                .await
                        }
                        Start::Continue(_) => {
                            self.runtime
                                .continue_turn_until(&cancel, &mut inbox, &mut observe)
                                .await
                        }
                    }
                };
                tokio::pin!(fut);
                loop {
                    tokio::select! {
                        outcome = &mut fut => break outcome,
                        mail = self.rx.recv() => match mail {
                            // Every mailbox dropped: let the turn finish.
                            None => {}
                            Some(mail) => Self::handle_running(&self.shared, &self.decisions, &outbox, &cancel, mail),
                        },
                    }
                }
            };
            let last_poster = self
                .shared
                .last_queued_by
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            next = self.after_turn(outcome, last_poster);
        }
        *self
            .shared
            .started
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        self.shared.set_state(ThreadState::Idle);
        self.shared.push_usage();
        self.side_jobs().await;
        // Nothing is working any more: the machine may sleep. Released
        // after the side jobs, so a memory extraction or a title is
        // covered too (issue #47).
        self.shared.drop_guard();
    }

    /// Which note a failed side job leaves, if any (issue #22). Both
    /// side jobs are best-effort: a transient provider failure (a
    /// dropped connection, a rate limit, an unavailable backend) is
    /// deferred silently — the memory cursor does not advance and the
    /// thread stays untitled, so the next run covers the stretch — and
    /// says nothing. A refusal the model cannot recover from by waiting
    /// (a 400, an auth failure) says so once, plainly. A non-provider
    /// error keeps today's raw note.
    ///
    /// Free-standing and pure so the policy is readable and testable
    /// without a runtime.
    fn side_job_note(job: &str, e: &RuntimeError) -> Option<String> {
        match e {
            RuntimeError::Provider(p) if p.is_transient() => None,
            RuntimeError::Provider(p) => Some(format!("{job} failed: {}", p.plain_line())),
            other => Some(format!("{job} failed: {other}")),
        }
    }

    /// After the turns: memory extraction and a title for an untitled
    /// thread, on the utility model (phase 6 step 9). Their events reach
    /// subscribers like any other; a failure is a note, never an error
    /// for the person, since the turn itself is done. Mail waits in the
    /// mailbox meanwhile.
    async fn side_jobs(&mut self) {
        let shared = self.shared.clone();
        let sys = Author::System;
        let mut observe = move |s: Signal<'_>| shared.observe(s, &sys);
        let note = |shared: &Shared, text: String| {
            shared.broadcast(Notice::Note {
                thread: shared.thread,
                text,
            })
        };
        match tokio::time::timeout(SIDE_JOB_LIMIT, self.runtime.extract_memory(&mut observe)).await
        {
            Ok(Ok(_)) => {}
            Ok(Err(ref e)) => {
                if let Some(text) = Self::side_job_note("memory extraction", e) {
                    note(&self.shared, text);
                }
            }
            Err(_) => note(&self.shared, "memory extraction timed out".into()),
        }
        match tokio::time::timeout(SIDE_JOB_LIMIT, self.runtime.title_if_untitled(&mut observe))
            .await
        {
            Ok(Ok(_)) => {}
            Ok(Err(ref e)) => {
                if let Some(text) = Self::side_job_note("titling", e) {
                    note(&self.shared, text);
                }
            }
            Err(_) => note(&self.shared, "titling timed out".into()),
        }
    }

    /// Mail while a turn runs. Only what needs no runtime is handled
    /// here; the rest is refused with a reason.
    fn handle_running(
        shared: &Arc<Shared>,
        decisions: &Decisions,
        outbox: &Outbox,
        cancel: &CancelToken,
        mail: Mail,
    ) {
        match mail {
            Mail::Post {
                author,
                blocks,
                interrupt,
                reply,
            } => {
                // The turn appends it and the observer counts it.
                outbox.send(Queued {
                    author: author.clone(),
                    blocks,
                });
                *shared
                    .last_queued_by
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(author.clone());
                if interrupt {
                    cancel.cancel(author);
                }
                let _ = reply.send(Response::Ok);
            }
            Mail::Decide {
                by,
                call_id,
                allow,
                session,
                prefix,
                reason,
                reply,
            } => {
                let _ = reply.send(
                    match decisions.decide(
                        &call_id,
                        Answered::Permission {
                            allow,
                            session,
                            by,
                            prefix,
                            reason,
                        },
                    ) {
                        Ok(()) => Response::Ok,
                        Err(e) => Response::Refused {
                            reason: e.to_string(),
                        },
                    },
                );
            }
            Mail::Answer {
                by,
                call_id,
                text,
                reply,
            } => {
                let _ = reply.send(
                    match decisions.decide(&call_id, Answered::Human { text, by }) {
                        Ok(()) => Response::Ok,
                        Err(e) => Response::Refused {
                            reason: e.to_string(),
                        },
                    },
                );
            }
            Mail::AnswerSwitch {
                by,
                call_id,
                answer,
                ctx,
                reply,
            } => {
                // A refused decide hands the context back by dropping
                // it, which closes the ack: the session reads that as
                // "the turn left" and never waits for a context that
                // cannot arrive.
                let _ = reply.send(
                    match decisions.decide(&call_id, Answered::Switch { answer, by, ctx }) {
                        Ok(()) => Response::Ok,
                        Err(e) => Response::Refused {
                            reason: e.to_string(),
                        },
                    },
                );
            }
            Mail::Subscribe {
                from_seq,
                notices,
                reply,
            } => {
                let mut subs = shared.subscribers.lock().unwrap_or_else(|e| e.into_inner());
                let events: Vec<Event> = shared
                    .events
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .iter()
                    .filter(|e| e.seq >= from_seq)
                    .cloned()
                    .collect();
                subs.push(notices);
                drop(subs);
                let mode = shared
                    .mode
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                let identity = shared
                    .identity
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                let _ = reply.send((shared.state(), events, mode, identity));
            }
            Mail::Status { reply } => {
                let _ = reply.send(shared.state());
            }
            Mail::Interrupt { by, reply } => {
                cancel.cancel(by);
                let _ = reply.send(Response::Ok);
            }
            Mail::InvokeSkill { reply, .. } => {
                let _ = reply.send(Response::Refused {
                    reason: "a turn is running; post the request, or interrupt first".into(),
                });
            }
            Mail::Pin { reply, .. }
            | Mail::Remember { reply, .. }
            | Mail::Rename { reply, .. }
            | Mail::SwitchProject { reply, .. }
            | Mail::ProposeSwitch { reply, .. }
            | Mail::Compact { reply }
            | Mail::SetMode { reply, .. }
            | Mail::Report { reply, .. } => {
                let _ = reply.send(Response::Refused {
                    reason: "a turn is running; this waits for it to end".into(),
                });
            }
        }
    }

    /// After a turn: an answered question continues at once (the phase 4
    /// split), and so does a message no model call has read — one posted
    /// mid-call, or a steered one the turn ended before reading (a
    /// budget, say). An error is left in the log and reported by the
    /// state.
    fn after_turn(
        &mut self,
        outcome: Result<TurnOutcome, aigentic_runtime::RuntimeError>,
        last_poster: Option<Author>,
    ) -> Option<Start> {
        // A turn that switched the thread's project (issue #7) may run
        // a different model now: tell the subscribers, as an idle switch
        // does, before the continuation.
        if std::mem::take(
            &mut *self
                .shared
                .switched
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        ) {
            self.announce_identity();
        }
        let queued = *self.shared.queued.lock().unwrap_or_else(|e| e.into_inner());
        match outcome {
            Ok(o) if o.reason == ASKED_HUMAN => {
                Some(Start::Continue(last_poster.unwrap_or(Author::System)))
            }
            Ok(_) | Err(_) if queued > 0 => {
                Some(Start::Continue(last_poster.unwrap_or(Author::System)))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aigentic_runtime::aigentic_core::ProviderError;

    fn provider(e: ProviderError) -> RuntimeError {
        RuntimeError::Provider(e)
    }

    /// T11 (issue #22): a side job that failed on a transient provider
    /// error leaves no note at all — the work is deferred, not lost
    /// (memory cursor unadvanced, title retried), so nothing belongs on
    /// screen.
    #[test]
    fn side_job_note_defers_transient() {
        let transient = [
            ProviderError::Transport("connection closed".into()),
            ProviderError::Http {
                status: 429,
                body: "rate limited".into(),
            },
            ProviderError::Http {
                status: 503,
                body: "unavailable".into(),
            },
            ProviderError::RateLimited,
        ];
        for e in transient {
            assert_eq!(
                ThreadActor::side_job_note("memory extraction", &provider(e.clone())),
                None,
                "{e:?} is deferred silently"
            );
        }
    }

    /// T12 (issue #22): a refusal that waiting will not fix is said once,
    /// plainly; a non-provider error keeps today's raw note.
    #[test]
    fn side_job_note_states_non_transient_plainly() {
        let refused = ProviderError::Http {
            status: 400,
            body: "Budget has been exceeded! Current cost: 30.0, Max budget: 0.0".into(),
        };
        assert_eq!(
            ThreadActor::side_job_note("memory extraction", &provider(refused.clone())),
            Some(format!(
                "memory extraction failed: {}",
                refused.plain_line()
            ))
        );

        let auth = ProviderError::Http {
            status: 401,
            body: "invalid api key".into(),
        };
        assert_eq!(
            ThreadActor::side_job_note("titling", &provider(auth.clone())),
            Some(format!("titling failed: {}", auth.plain_line()))
        );

        let other = RuntimeError::UnknownSkill("nope".into());
        assert_eq!(
            ThreadActor::side_job_note("titling", &other),
            Some(format!("titling failed: {other}"))
        );
    }
}
