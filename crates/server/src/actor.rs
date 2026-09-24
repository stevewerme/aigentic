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
use aigentic_runtime::aigentic_log::{PermissionRequestedPayload, UserMessagePayload};
use aigentic_runtime::{
    ASKED_HUMAN, Answered, CancelToken, Decisions, Mode, Outbox, Pending, Queued, Resumed, Runtime,
    Signal, TurnOutcome, WindowUsage, inbox,
};
use tokio::sync::{mpsc, oneshot};
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
        /// The state, the events since `from_seq`, the mode's name.
        reply: oneshot::Sender<(ThreadState, Vec<Event>, String)>,
    },
}

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
    /// Messages handed to the running turn; reported in `Running`.
    queued: Mutex<u32>,
    /// Who posted last during the turn: the next turn is "by" them.
    last_queued_by: Mutex<Option<Author>>,
    /// The permission mode's name, for `Opened`.
    mode: Mutex<String>,
    /// When the running turn started; `None` while idle.
    started: Mutex<Option<Instant>>,
    /// The last window fill the runtime reported, re-sent when the queue
    /// changes or the turn ends so a status line stays current.
    last_usage: Mutex<Option<WindowUsage>>,
}

impl Shared {
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
                // after its event, so subscribers see both in order.
                if event.kind == EventKind::UserMessage
                    && serde_json::from_value::<UserMessagePayload>(event.payload.clone())
                        .is_ok_and(|p| p.mid_turn)
                {
                    *self.queued.lock().unwrap_or_else(|e| e.into_inner()) += 1;
                    self.running(turn_by.clone());
                    self.push_usage();
                    return;
                }
                // A decision or a result ends a wait.
                let waiting = !matches!(self.state(), ThreadState::Running { .. });
                if waiting
                    && matches!(
                        event.kind,
                        EventKind::PermissionDecided | EventKind::ToolResult
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
            Signal::Waiting(pending) => self.set_state(match pending {
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
            }),
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
            last_queued_by: Mutex::new(None),
            mode: Mutex::new(runtime.mode().name().to_owned()),
        });
        let (tx, rx) = mpsc::unbounded_channel();
        let mut actor = Self {
            runtime,
            decisions,
            shared,
            reports,
            rx,
            pending_compact: None,
        };
        actor.resume(torn)?;
        Ok((actor, tx))
    }

    /// Phase 2's resume: repair events are appended (and mirrored) and,
    /// when the log ended mid-turn or on an answered question, the first
    /// thing the actor does is continue that turn.
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
            if let Some(start) = self.handle_idle(mail) {
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
    fn handle_idle(&mut self, mail: Mail) -> Option<Start> {
        match mail {
            Mail::Post {
                author,
                blocks,
                reply,
                ..
            } => {
                let _ = reply.send(Response::Ok);
                Some(Start::Post(author, blocks))
            }
            Mail::InvokeSkill {
                author,
                name,
                args,
                reply,
            } => {
                let _ = reply.send(Response::Ok);
                Some(Start::Skill(author, name, args))
            }
            Mail::Decide { call_id, reply, .. } | Mail::Answer { call_id, reply, .. } => {
                let _ = reply.send(Response::Refused {
                    reason: format!("nothing is pending for call {call_id}"),
                });
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
                let shared = self.shared.clone();
                let sys = Author::System;
                let _ = reply.send(
                    match self
                        .runtime
                        .set_project(*ctx, by, &mut |s| shared.observe(s, &sys))
                    {
                        Ok(()) => Response::Ok,
                        Err(e) => Response::Error {
                            message: e.to_string(),
                        },
                    },
                );
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

    fn subscribe(
        &self,
        from_seq: u64,
        notices: mpsc::UnboundedSender<Notice>,
        reply: oneshot::Sender<(ThreadState, Vec<Event>, String)>,
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
        let _ = reply.send((self.shared.state(), events, mode));
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
            Ok(Err(e)) => note(&self.shared, format!("memory extraction failed: {e}")),
            Err(_) => note(&self.shared, "memory extraction timed out".into()),
        }
        match tokio::time::timeout(SIDE_JOB_LIMIT, self.runtime.title_if_untitled(&mut observe))
            .await
        {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => note(&self.shared, format!("titling failed: {e}")),
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
                let _ = reply.send((shared.state(), events, mode));
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
    /// split), queued messages start the next turn, an error is left in
    /// the log and reported by the state.
    fn after_turn(
        &mut self,
        outcome: Result<TurnOutcome, aigentic_runtime::RuntimeError>,
        last_poster: Option<Author>,
    ) -> Option<Start> {
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
