//! One connection: `Hello` first, then requests in order, each checked
//! against the user's role before it reaches a thread, with notices from
//! every thread the session opened written back as they arrive.

use std::collections::HashMap;
use std::sync::Arc;

use aigentic_api::{
    Body, CheckpointAnswer, Frame, FrontOutcome, Notice, PROTOCOL_VERSION, ProjectInfo, Request,
    Response, StartAsk, SwitchReply, ThreadInfo, ThreadState, Welcome, decode, encode,
};
use aigentic_runtime::aigentic_core::{AgentId, Author, UserId};
use aigentic_runtime::aigentic_policy::Role;
use aigentic_runtime::{Mode, SwitchAnswer, SwitchCtx};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};
use ulid::Ulid;

use crate::actor::Mail;
use crate::auth;
use crate::config::ServerConfig;
use crate::threads::{RunThread, ThreadError, ThreadTable};

/// The longest line a session may send; longer closes it. A post can
/// carry a large block, so this is generous, but not unbounded.
pub const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;

/// One line without its newline, or `None` at end of stream. A line
/// over the cap is an error, which closes the session. Works on the
/// reader's own buffer, so nothing is read past the newline.
async fn next_line<R: AsyncBufReadExt + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
) -> std::io::Result<Option<String>> {
    buf.clear();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(if buf.is_empty() {
                None
            } else {
                Some(String::from_utf8_lossy(buf).into_owned())
            });
        }
        match available.iter().position(|&b| b == b'\n') {
            Some(pos) => {
                buf.extend_from_slice(&available[..pos]);
                reader.consume(pos + 1);
                if buf.len() > MAX_LINE_BYTES {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "line over the size cap",
                    ));
                }
                return Ok(Some(String::from_utf8_lossy(buf).into_owned()));
            }
            None => {
                let n = available.len();
                buf.extend_from_slice(available);
                reader.consume(n);
                if buf.len() > MAX_LINE_BYTES {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "line over the size cap",
                    ));
                }
            }
        }
    }
}

/// The daemon's name and version, for `Welcome`.
pub fn server_name() -> String {
    format!("aigentic {}", env!("CARGO_PKG_VERSION"))
}

/// Serve one connection until it closes. Errors are the connection's own
/// (a broken pipe); a bad request is answered, never fatal.
pub async fn serve(
    reader: Box<dyn AsyncRead + Unpin + Send>,
    writer: Box<dyn AsyncWrite + Unpin + Send>,
    config: Arc<ServerConfig>,
    threads: Arc<ThreadTable>,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(reader);
    let mut buf = Vec::new();
    // One writer task: responses and notices interleave on the same
    // line stream in the order they are sent here.
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
    let writer_task = tokio::spawn(async move {
        let mut writer = writer;
        while let Some(line) = out_rx.recv().await {
            if writer.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            let _ = writer.flush().await;
        }
    });
    let send = |frame: Frame| {
        let _ = out_tx.send(encode(&frame));
    };

    // Hello.
    let user = loop {
        let Some(line) = next_line(&mut reader, &mut buf).await? else {
            writer_task.abort();
            return Ok(());
        };
        let Ok(frame) = decode(&line) else {
            continue;
        };
        let Some(id) = frame.id else { continue };
        let Body::Request(request) = frame.body else {
            continue;
        };
        match request {
            Request::Hello { protocol, token } => {
                if protocol != PROTOCOL_VERSION {
                    send(Frame::response(
                        id,
                        Response::Refused {
                            reason: format!(
                                "protocol {protocol} is not this daemon's {PROTOCOL_VERSION}"
                            ),
                        },
                    ));
                    // Give the writer a moment to flush the refusal (issue
                    // #7: the numbers are what a stale client needs to
                    // read), then close.
                    drop(out_tx);
                    let _ = writer_task.await;
                    return Ok(());
                }
                match auth::user_for_token(&config, &token) {
                    Some(user) => {
                        let projects = project_infos(&config, &threads, &user);
                        send(Frame::response(
                            id,
                            Response::Welcome(Welcome {
                                user: user.clone(),
                                projects,
                                server: server_name(),
                            }),
                        ));
                        break user;
                    }
                    None => {
                        send(Frame::response(
                            id,
                            Response::Refused {
                                reason: "unknown token".into(),
                            },
                        ));
                        // Give the writer a moment, then close.
                        drop(out_tx);
                        let _ = writer_task.await;
                        return Ok(());
                    }
                }
            }
            _ => send(Frame::response(
                id,
                Response::Refused {
                    reason: "hello first".into(),
                },
            )),
        }
    };
    let author = Author::User(UserId(user.clone()));

    // No start-up scan here (issue #58 fix 5): `serve()` runs the one
    // scan, behind `resume_runs`, before it accepts a connection. A
    // per-connection scan would repeat it for every client and race the
    // first scan's tasks.

    // Open threads: the mailbox of the thread's actor when it has one
    // (a run-owned thread has none, issue #58), and the task forwarding
    // its notices to the writer.
    let mut open: HashMap<Ulid, OpenThread> = HashMap::new();

    while let Some(line) = next_line(&mut reader, &mut buf).await? {
        let Ok(frame) = decode(&line) else {
            continue;
        };
        let Some(id) = frame.id else { continue };
        let Body::Request(request) = frame.body else {
            continue;
        };
        let response = handle(
            &config, &threads, &user, &author, &mut open, &out_tx, request,
        )
        .await;
        send(Frame::response(id, response));
    }
    for (thread, entry) in open.drain() {
        entry.forward.abort();
        if entry.mailbox.is_some() {
            threads.close(thread);
        }
    }
    drop(out_tx);
    let _ = writer_task.await;
    Ok(())
}

/// The role `user` holds in a project of this daemon: `read` counts, so
/// a reader is listed. This is the one rule — the client's project list
/// ([`project_infos`]) and #81's projects listing both ask it, never a
/// copy of it.
pub(crate) fn role_in_project(
    config: &ServerConfig,
    threads: &ThreadTable,
    project: &str,
    user: &str,
) -> Option<Role> {
    let participants = threads.participants(project).ok()?;
    auth::role_in(user, config.owner(), &participants)
}

fn project_infos(config: &ServerConfig, threads: &ThreadTable, user: &str) -> Vec<ProjectInfo> {
    threads
        .projects()
        .into_iter()
        .filter_map(|(name, root, count)| {
            let role = role_in_project(config, threads, &name, user)?;
            Some(ProjectInfo {
                name,
                root,
                role: Some(role.name().to_owned()),
                threads: count,
            })
        })
        .collect()
}

/// The project a request is about, for the role check.
fn project_for(threads: &ThreadTable, request: &Request) -> Option<String> {
    match request {
        Request::ListThreads {
            project: Some(project),
        }
        | Request::CreateThread { project }
        | Request::Build { project, .. } => Some(project.clone()),
        // Listed across projects (issue #86): there is no one project
        // to check, so the handler judges each row's project. The arm
        // keeps this match exhaustive.
        Request::ListThreads { project: None } => None,
        // A checkpoint is answered in the project its run works in.
        Request::AnswerCheckpoint { lead, .. } => threads.project_of(*lead),
        Request::Open { thread, .. }
        | Request::Close { thread }
        | Request::Post { thread, .. }
        | Request::InvokeSkill { thread, .. }
        | Request::Interrupt { thread }
        | Request::Decide { thread, .. }
        | Request::AnswerHuman { thread, .. }
        | Request::AnswerSwitch { thread, .. }
        | Request::Pin { thread, .. }
        | Request::Remember { thread, .. }
        | Request::Rename { thread, .. }
        | Request::SwitchProject { thread, .. }
        | Request::Compact { thread }
        | Request::SetMode { thread, .. }
        | Request::Report { thread, .. } => threads.project_of(*thread),
        // The front thread (issue #84): an existing one says its
        // project; a new one is made in the project asked for.
        Request::Front { project, .. } | Request::NewFront { project } => Some(project.clone()),
        Request::Hello { .. } | Request::ListProjects | Request::StartRows => None,
    }
}

/// The reply to `Front` (issue #84): resume the user's front thread, or
/// make one in `project`. The handler judges the roles, because a resume
/// needs `read` where the front thread lives and a create needs `write`
/// where the person stands, and the session's pre-check cannot know both.
///
/// A front thread that cannot be opened is not a refusal: the person gets
/// a new one in `project`, and `Replaced` says why in words a banner can
/// print. The old one stays listed.
async fn front_thread(
    config: &Arc<ServerConfig>,
    threads: &Arc<ThreadTable>,
    open: &mut HashMap<Ulid, OpenThread>,
    user: &str,
    author: &Author,
    project: &str,
    asked: Option<&StartAsk>,
) -> FrontReply {
    // One process at a time, per user (issue #88): two terminals launched
    // together must not both find no front thread and both make one. Held
    // across the whole check-and-create, and released by the guard's drop
    // however this returns; a `kill -9` closes the fd.
    let _lock = match crate::runs::FrontLock::take(threads.threads_base(), user).await {
        Ok(lock) => lock,
        Err(reason) => return FrontReply::Refused(Box::new(Response::Refused { reason })),
    };
    let replaced = match threads.latest_front(user) {
        Some(front) => match resumable(config, threads, user, front) {
            Ok(info) => {
                return match asked {
                    Some(ask) => {
                        honour_startup_ask(config, threads, open, user, author, info, ask).await
                    }
                    None => FrontReply::Open {
                        thread: Box::new(info),
                        outcome: FrontOutcome::Resumed,
                        answered: false,
                    },
                };
            }
            Err(reason) => Some(reason),
        },
        None => None,
    };
    match create_front(config, threads, user, author, project).await {
        Ok(info) => {
            // Issue #121: the client's answer is logged in the thread it
            // created — `create_front` already lands it in `chosen`.
            if let Some(ask) = asked {
                record_startup_ask(threads, open, info.id, ask, author).await;
            }
            FrontReply::Open {
                thread: Box::new(info),
                outcome: match replaced {
                    None => FrontOutcome::First,
                    Some(reason) => FrontOutcome::Replaced { reason },
                },
                answered: asked.is_some(),
            }
        }
        Err(refusal) => FrontReply::Refused(refusal),
    }
}

/// What `Front` gets back: a thread to open and how it was reached, or
/// the refusal that stood in the way instead.
enum FrontReply {
    Open {
        thread: Box<ThreadInfo>,
        outcome: FrontOutcome,
        /// Issue #121: the request carried the start-up ask's answer, so
        /// the person has already chosen and #92's proposal is not
        /// raised for this thread.
        answered: bool,
    },
    Refused(Box<Response>),
}

/// Whether a resumed front thread's start-up answer can be honoured
/// (issue #121), and the thread to open either way.
///
/// The server is the judge of a busy thread: the client's listing cannot
/// say (design 5), so a thread that is running or waiting on a person
/// ignores the answer — no move and no pair — and resumes where it
/// lives. The person has still answered, so #92's proposal is not raised
/// on top of it.
async fn honour_startup_ask(
    config: &Arc<ServerConfig>,
    threads: &Arc<ThreadTable>,
    open: &mut HashMap<Ulid, OpenThread>,
    user: &str,
    author: &Author,
    info: ThreadInfo,
    ask: &StartAsk,
) -> FrontReply {
    let thread = info.id;
    if !matches!(live_state(threads, open, thread).await, ThreadState::Idle) {
        return FrontReply::Open {
            thread: Box::new(info),
            outcome: FrontOutcome::Resumed,
            answered: true,
        };
    }
    if threads.project_of(thread).as_deref() != Some(ask.chosen.as_str()) {
        // The person needs `write` where they are going, the role a
        // `SwitchProject` needs — the same check `Front`'s pre-check
        // makes for one.
        let participants = match threads.participants(&ask.chosen) {
            Ok(p) => p,
            Err(e) => return FrontReply::Refused(Box::new(thread_error(e))),
        };
        let switch = Request::SwitchProject {
            thread,
            project: ask.chosen.clone(),
        };
        if let Err(denied) = auth::allowed(user, config.owner(), &participants, &switch) {
            return FrontReply::Refused(Box::new(Response::Refused {
                reason: format!("in {}: {}", ask.chosen, denied.reason),
            }));
        }
        // The move without a proposal (issue #121): the target's context
        // is built here and the actor swaps it in, as `AnswerSwitch`
        // does for a `yes`. Not `threads.switch`, which needs a live
        // mailbox a just-started daemon does not have.
        let ctx = match threads.build_target(thread, &ask.chosen).await {
            Ok(ctx) => ctx,
            Err(e) => return FrontReply::Refused(Box::new(thread_error(e))),
        };
        let moved = ask_actor(threads, open, thread, |reply| Mail::SwitchProject {
            ctx: Box::new(ctx),
            by: author.clone(),
            reply,
        })
        .await;
        match moved {
            Response::Ok => threads.note_project(thread, &ask.chosen),
            Response::Refused { reason } => {
                return FrontReply::Refused(Box::new(Response::Refused { reason }));
            }
            other => {
                return FrontReply::Refused(Box::new(Response::Error {
                    message: format!("the switch to {} said {other:?}", ask.chosen),
                }));
            }
        }
    }
    // The row after the move, so the client's banner and its role follow
    // the switch (`front::thread_project`).
    let row = threads.info(thread).unwrap_or(info);
    record_startup_ask(threads, open, thread, ask, author).await;
    FrontReply::Open {
        thread: Box::new(row),
        outcome: FrontOutcome::Resumed,
        answered: true,
    }
}

/// Write the pair the start-up ask leaves in the log (issue #121): the
/// `project` proposal and its answer, back to back. The reply is
/// ignored, as the `Front` answer is the same either way.
async fn record_startup_ask(
    threads: &Arc<ThreadTable>,
    open: &mut HashMap<Ulid, OpenThread>,
    thread: Ulid,
    ask: &StartAsk,
    author: &Author,
) {
    let _ = ask_actor(threads, open, thread, |reply| Mail::RecordStartupAsk {
        offered: ask.offered.clone(),
        chosen: ask.chosen.clone(),
        reason: ask.reason.clone(),
        by: author.clone(),
        reply,
    })
    .await;
}

/// A thread's live state, without opening anything: the actor's own
/// answer when one runs, and `Idle` when none does — nothing is writing
/// a thread without an actor.
async fn live_state(
    threads: &Arc<ThreadTable>,
    open: &mut HashMap<Ulid, OpenThread>,
    thread: Ulid,
) -> ThreadState {
    let mailbox = match open.get(&thread).and_then(|entry| entry.mailbox.clone()) {
        Some(m) => m,
        None => match threads.mailbox(thread) {
            Some(m) => m,
            None => return ThreadState::Idle,
        },
    };
    let (reply, rx) = oneshot::channel();
    if mailbox.send(Mail::Status { reply }).is_err() {
        return ThreadState::Idle;
    }
    rx.await.unwrap_or(ThreadState::Idle)
}

/// Whether the user's front thread can be opened, and its listing row if
/// so; `Err` says why not, for `Replaced`.
fn resumable(
    config: &Arc<ServerConfig>,
    threads: &Arc<ThreadTable>,
    user: &str,
    front: Ulid,
) -> Result<ThreadInfo, String> {
    if threads.run_thread(front) != RunThread::No {
        return Err(format!("your front thread {front} belongs to a run"));
    }
    let Some(project) = threads.project_of(front) else {
        return Err(format!(
            "your front thread {front} has no project this daemon can open"
        ));
    };
    let participants = match threads.participants(&project) {
        Ok(p) => p,
        Err(ThreadError::NoProject(_)) => {
            return Err(format!(
                "your front thread is in {project}, which this daemon does not know"
            ));
        }
        Err(e) => {
            return Err(format!("your front thread {front} cannot be opened: {e}"));
        }
    };
    // The name must be the same project, not another folder's project of
    // that name (issue #88): a front thread in bare `/x/app` must not
    // open in a daemon whose `/y/app` is the project. Compared only when
    // the thread's recorded root is still there — a project whose folder
    // moved is resumed by name, as #84 did, and a thread with no recorded
    // root is never refused for it.
    let thread_root = threads.root_of_thread(front);
    let daemon_root = threads.project_root(&project);
    if let (Some(thread_root), Some(daemon_root)) = (&thread_root, &daemon_root)
        && thread_root.is_dir()
        && !crate::threads::same_root(thread_root, daemon_root)
    {
        return Err(format!(
            "your front thread is in {project} at {}, not this daemon's {project} at {}",
            thread_root.display(),
            daemon_root.display()
        ));
    }
    // A resume is an `Open` of the front thread, so that is the request
    // `auth` judges: `read` where the front thread lives.
    let open = Request::Open {
        thread: front,
        from_seq: 0,
    };
    if auth::allowed(user, config.owner(), &participants, &open).is_err() {
        return Err(format!("you no longer have a role in {project}"));
    }
    threads
        .info(front)
        .ok_or_else(|| format!("your front thread {front} has no project this daemon can open"))
}

/// A new front thread in `project`, or the refusal that stops it: the
/// project's participants decide, and a denial reads like `AnswerSwitch`'s
/// — `in {project}: ` and why.
async fn create_front(
    config: &Arc<ServerConfig>,
    threads: &Arc<ThreadTable>,
    user: &str,
    author: &Author,
    project: &str,
) -> Result<ThreadInfo, Box<Response>> {
    let participants = match threads.participants(project) {
        Ok(p) => p,
        Err(e) => return Err(Box::new(thread_error(e))),
    };
    let create = Request::CreateThread {
        project: project.to_owned(),
    };
    if let Err(denied) = auth::allowed(user, config.owner(), &participants, &create) {
        return Err(Box::new(Response::Refused {
            reason: format!("in {project}: {}", denied.reason),
        }));
    }
    threads
        .create(project, author.clone(), true)
        .await
        .map_err(|e| Box::new(thread_error(e)))
}

/// Offer the resumed front thread a switch to the folder's project
/// (issue #92), in #82's block. Raised only when the daemon knows
/// `here`, the user could switch there, and the user could answer it —
/// `write` in the thread's own project, without which a proposal would
/// be raised that nobody can answer or clear. The reply is ignored:
/// the `Front` answer is the same either way. The actor refuses, too,
/// when a proposal is already waiting or the thread is not idle.
async fn raise_startup_proposal(
    config: &Arc<ServerConfig>,
    threads: &Arc<ThreadTable>,
    open: &mut HashMap<Ulid, OpenThread>,
    user: &str,
    thread: Ulid,
    here: Option<&str>,
) {
    let Some(here) = here else {
        return;
    };
    // The thread's own project, and `here`, must be two different ones.
    let Some(project) = threads.project_of(thread) else {
        return;
    };
    if here == project {
        return;
    }
    // The daemon must know `here`, and the user must be able to switch
    // there — the role a `SwitchProject` needs.
    let Ok(participants_here) = threads.participants(here) else {
        return;
    };
    let switch = Request::SwitchProject {
        thread,
        project: here.to_owned(),
    };
    if auth::allowed(user, config.owner(), &participants_here, &switch).is_err() {
        return;
    }
    // And the user must be able to answer it: `write` in the thread's
    // own project, the role `AnswerSwitch` needs.
    let Ok(participants_thread) = threads.participants(&project) else {
        return;
    };
    let answer = Request::AnswerSwitch {
        thread,
        call_id: String::new(),
        answer: SwitchReply::No,
    };
    if auth::allowed(user, config.owner(), &participants_thread, &answer).is_err() {
        return;
    }
    let _ = ask_actor(threads, open, thread, |reply| Mail::ProposeSwitch {
        project: here.to_owned(),
        reason: format!("aigentic was started in {here}'s folder"),
        reply,
    })
    .await;
}

async fn handle(
    config: &Arc<ServerConfig>,
    threads: &Arc<ThreadTable>,
    user: &str,
    author: &Author,
    open: &mut HashMap<Ulid, OpenThread>,
    out: &mpsc::UnboundedSender<String>,
    request: Request,
) -> Response {
    // A run-owned thread belongs to its run (issue #58, rule 5): its
    // step child or its lead is written by the run's task alone, so
    // nothing here may post into it, and no actor is started for it.
    if let Some(thread) = mutates_run(&request)
        && threads.run_thread(thread) != RunThread::No
    {
        return Response::Refused {
            reason: "this thread belongs to a run".into(),
        };
    }
    // Roles first: a refused request never reaches a mailbox.
    if aigentic_runtime::aigentic_policy::needs(&request).is_some() {
        let Some(project) = project_for(threads, &request) else {
            return Response::Refused {
                reason: "no such thread or project on this daemon".into(),
            };
        };
        let participants = match threads.participants(&project) {
            Ok(p) => p,
            Err(e) => return thread_error(e),
        };
        if let Err(denied) = auth::allowed(user, config.owner(), &participants, &request) {
            return Response::Refused {
                reason: denied.reason,
            };
        }
        // A switch needs the same role in the project it goes to.
        if let Request::SwitchProject { project: to, .. } = &request {
            let target = match threads.participants(to) {
                Ok(p) => p,
                Err(e) => return thread_error(e),
            };
            if let Err(denied) = auth::allowed(user, config.owner(), &target, &request) {
                return Response::Refused {
                    reason: format!("in {to}: {}", denied.reason),
                };
            }
        }
    }

    match request {
        Request::Hello { .. } => Response::Refused {
            reason: "already said hello".into(),
        },
        // The front thread (issue #84): `Front` resumes one or makes
        // the replacement, `NewFront` always makes one.
        Request::Front {
            project,
            here,
            asked,
        } => {
            // Issue #121: the client's answer names the project, so the
            // front thread is reached in `chosen` and #92's proposal is
            // not raised on top of an answered ask.
            let target = match &asked {
                Some(ask) => ask.chosen.as_str(),
                None => project.as_str(),
            };
            match front_thread(config, threads, open, user, author, target, asked.as_ref()).await {
                FrontReply::Open {
                    thread,
                    outcome,
                    answered,
                } => {
                    // Issue #92: a resumed front thread started in
                    // another folder's project is offered a switch in
                    // #82's block. The `Front` reply is the same either
                    // way; the proposal only changes the state the
                    // client's `Open` then finds.
                    if matches!(outcome, FrontOutcome::Resumed) && !answered {
                        raise_startup_proposal(
                            config,
                            threads,
                            open,
                            user,
                            thread.id,
                            here.as_deref(),
                        )
                        .await;
                    }
                    Response::Front {
                        thread: *thread,
                        outcome,
                    }
                }
                FrontReply::Refused(refusal) => *refusal,
            }
        }
        Request::NewFront { project } => match threads.create(&project, author.clone(), true).await
        {
            Ok(info) => Response::Thread { thread: info },
            Err(e) => thread_error(e),
        },
        Request::ListProjects => Response::Projects {
            projects: project_infos(config, threads, user),
        },
        // The start-up ask's light listing (issue #121): the index and
        // one first line per unindexed log, never a whole log. Off the
        // worker anyway, like the heavy listing.
        Request::StartRows => {
            let table = Arc::clone(threads);
            let user = user.to_owned();
            let rows = tokio::task::spawn_blocking(move || table.start_rows(&user)).await;
            match rows {
                Ok(rows) => Response::StartRows { rows },
                Err(e) => Response::Refused {
                    reason: format!("the listing failed: {e}"),
                },
            }
        }
        Request::ListThreads { project } => {
            // A listing reads and parses a log per row — a whole tree
            // of them with no project (issue #86) — so it runs off the
            // session's async worker. The role rule is asked inside,
            // against the same table: one rule, never a copy.
            let table = Arc::clone(threads);
            let config = Arc::clone(config);
            let user = user.to_owned();
            let listed = tokio::task::spawn_blocking(move || {
                table.list(project.as_deref(), &user, |q| {
                    role_in_project(&config, &table, q, &user).is_some()
                })
            })
            .await;
            match listed {
                Ok(Ok(list)) => Response::Threads { threads: list },
                Ok(Err(e)) => thread_error(e),
                Err(e) => Response::Refused {
                    reason: format!("the listing failed: {e}"),
                },
            }
        }
        Request::CreateThread { project } => {
            match threads.create(&project, author.clone(), false).await {
                Ok(info) => Response::Thread { thread: info },
                Err(e) => thread_error(e),
            }
        }
        Request::Open { thread, from_seq } => {
            let run = threads.run_thread(thread);
            if run != RunThread::No {
                return open_run(threads, run, thread, from_seq, out, open);
            }
            if let Some(mailbox) = open.get(&thread).and_then(|entry| entry.mailbox.clone()) {
                // Already open here: answer from the actor again.
                return subscribe(&mailbox, thread, from_seq, out, open).await;
            }
            match threads.open(thread).await {
                Ok((mailbox, _project)) => subscribe(&mailbox, thread, from_seq, out, open).await,
                Err(e) => thread_error(e),
            }
        }
        Request::Close { thread } => {
            if let Some(entry) = open.remove(&thread) {
                entry.forward.abort();
                if entry.mailbox.is_some() {
                    threads.close(thread);
                }
            }
            Response::Ok
        }
        Request::Post {
            thread,
            blocks,
            interrupt,
        } => {
            let author = author.clone();
            ask_actor(threads, open, thread, |reply| Mail::Post {
                author,
                blocks,
                interrupt,
                reply,
            })
            .await
        }
        Request::Interrupt { thread } => {
            let by = author.clone();
            ask_actor(threads, open, thread, |reply| Mail::Interrupt { by, reply }).await
        }
        Request::InvokeSkill { thread, name, args } => {
            let author = author.clone();
            ask_actor(threads, open, thread, |reply| Mail::InvokeSkill {
                author,
                name,
                args,
                reply,
            })
            .await
        }
        Request::Decide {
            thread,
            call_id,
            allow,
            session,
            prefix,
            reason,
        } => {
            let by = author.clone();
            ask_actor(threads, open, thread, |reply| Mail::Decide {
                by,
                call_id,
                allow,
                session,
                prefix,
                reason,
                reply,
            })
            .await
        }
        Request::AnswerHuman {
            thread,
            call_id,
            text,
        } => {
            let by = author.clone();
            ask_actor(threads, open, thread, |reply| Mail::Answer {
                by,
                call_id,
                text,
                reply,
            })
            .await
        }
        Request::AnswerSwitch {
            thread,
            call_id,
            answer,
        } => {
            // A `yes` needs the target's context built here, before the
            // answer reaches the actor, so the proposal's project is
            // read from the actor's state first (issue #7). The role in
            // the target is the one a `SwitchProject` needs, and the
            // refusal text is the same.
            let mut ack = None;
            let mut target = None;
            let (answer, ctx) = match answer {
                SwitchReply::Yes => {
                    let Some(project) = threads.waiting_switch(thread, &call_id).await else {
                        return Response::Refused {
                            reason: format!("nothing is pending for call {call_id}"),
                        };
                    };
                    let participants = match threads.participants(&project) {
                        Ok(p) => p,
                        Err(e) => return thread_error(e),
                    };
                    let asked = Request::AnswerSwitch {
                        thread,
                        call_id: call_id.clone(),
                        answer: SwitchReply::Yes,
                    };
                    if let Err(denied) = auth::allowed(user, config.owner(), &participants, &asked)
                    {
                        return Response::Refused {
                            reason: format!("in {project}: {}", denied.reason),
                        };
                    }
                    match threads.build_target(thread, &project).await {
                        Ok(ctx) => {
                            let (ctx, rx) = SwitchCtx::oneshot(ctx);
                            ack = Some(rx);
                            target = Some(project);
                            (SwitchAnswer::Yes, ctx)
                        }
                        Err(e) => return thread_error(e),
                    }
                }
                SwitchReply::No => (SwitchAnswer::No, SwitchCtx::none()),
                SwitchReply::Corrected { to } => (SwitchAnswer::Corrected(to), SwitchCtx::none()),
                SwitchReply::Withdrawn { note } => {
                    (SwitchAnswer::Withdrawn(note), SwitchCtx::none())
                }
            };
            let by = author.clone();
            let reply = ask_actor(threads, open, thread, |reply| Mail::AnswerSwitch {
                by,
                call_id,
                answer,
                ctx,
                reply,
            })
            .await;
            match (ack, target) {
                // The switch happened only when the parked turn says so:
                // an `Ok` ack is the only evidence a `yes` was applied.
                (Some(ack), Some(project)) if matches!(reply, Response::Ok) => match ack.await {
                    Ok(Ok(())) => {
                        threads.note_project(thread, &project);
                        Response::Ok
                    }
                    // The turn left, or the runtime refused the switch
                    // itself; either way the log is the truth and the
                    // entry is left alone.
                    Ok(Err(e)) => Response::Refused {
                        reason: format!("in {project}: {e}"),
                    },
                    Err(_) => Response::Refused {
                        reason: "the turn left before the switch happened".into(),
                    },
                },
                _ => reply,
            }
        }
        Request::Pin { thread, text } => {
            let author = author.clone();
            ask_actor(threads, open, thread, |reply| Mail::Pin {
                author,
                text,
                reply,
            })
            .await
        }
        Request::Remember { thread, text } => {
            let author = author.clone();
            ask_actor(threads, open, thread, |reply| Mail::Remember {
                author,
                text,
                reply,
            })
            .await
        }
        Request::SwitchProject { thread, project } => {
            if !open.contains_key(&thread) {
                return Response::Refused {
                    reason: "open the thread first".into(),
                };
            }
            match threads.switch(thread, &project, author.clone()).await {
                Ok(()) => Response::Ok,
                Err(e) => thread_error(e),
            }
        }
        Request::Rename { thread, title } => {
            let author = author.clone();
            ask_actor(threads, open, thread, |reply| Mail::Rename {
                author,
                title,
                reply,
            })
            .await
        }
        Request::Compact { thread } => {
            ask_actor(threads, open, thread, |reply| Mail::Compact { reply }).await
        }
        Request::SetMode { thread, mode } => match mode.parse::<Mode>() {
            Ok(mode) => {
                ask_actor(threads, open, thread, |reply| Mail::SetMode { mode, reply }).await
            }
            Err(e) => Response::Refused { reason: e },
        },
        Request::Build {
            project,
            issue,
            workflow,
        } => match threads
            .build_run(&project, issue, workflow, author.clone())
            .await
        {
            Ok((lead, resumed)) => {
                // Watch the run's lead from here on: its events arrive as
                // notices, and the log holds what happened before.
                watch_run(threads, lead, out, open);
                Response::Run { lead, resumed }
            }
            Err(e) => run_error(e),
        },
        Request::AnswerCheckpoint {
            lead,
            gate,
            answer,
            amendment,
        } => {
            // Slice 1 answers `stop` only: acting on `go` or `amend` is
            // the next slice's work, and nothing is written for them.
            if !matches!(answer, CheckpointAnswer::Stop) {
                return Response::Refused {
                    reason: "answering go or amend comes in slice 2".into(),
                };
            }
            match threads
                .answer_checkpoint(lead, gate, answer, amendment, author.clone())
                .await
            {
                Ok(()) => Response::Ok,
                Err(e) => run_error(e),
            }
        }
        Request::Report { thread, report } => {
            // A report only reads, so a run-owned thread answers it from
            // its own log (rule 5c). `mutates_run` does not list it, and
            // it must not fall through to a mailbox that does not exist
            // (#58 fix 3): the run's task is the thread's only writer,
            // and rendering writes nothing.
            if threads.run_thread(thread) != RunThread::No {
                return match threads.report_of(thread, report).await {
                    Ok(text) => Response::Text { text },
                    Err(e) => thread_error(e),
                };
            }
            ask_actor(threads, open, thread, |reply| Mail::Report {
                kind: report,
                reply,
            })
            .await
        }
    }
}

/// One thread a session has opened. A run-owned thread (issue #58) has
/// no actor, so `mailbox` is `None` and only the notice forward is real.
struct OpenThread {
    mailbox: Option<crate::actor::Mailbox>,
    forward: tokio::task::JoinHandle<()>,
}

/// The thread a request would write into, when the request is one of
/// rule 5c's: those are refused on a run-owned thread.
fn mutates_run(request: &Request) -> Option<Ulid> {
    match request {
        Request::Post { thread, .. }
        | Request::InvokeSkill { thread, .. }
        | Request::Interrupt { thread }
        | Request::Decide { thread, .. }
        | Request::AnswerHuman { thread, .. }
        | Request::SetMode { thread, .. }
        | Request::Compact { thread }
        | Request::Pin { thread, .. }
        | Request::Remember { thread, .. }
        | Request::Rename { thread, .. }
        | Request::SwitchProject { thread, .. } => Some(*thread),
        _ => None,
    }
}

/// A run's own failures: the run's task refused the answer for a reason
/// the caller can act on. Those are refusals, not errors, so a client
/// can tell "no such lead" from "the daemon broke".
fn run_error(e: ThreadError) -> Response {
    match &e {
        ThreadError::Refused(_)
        | ThreadError::NoThread(_)
        | ThreadError::NoProject(_)
        | ThreadError::NotARun(_) => Response::Refused {
            reason: e.to_string(),
        },
        _ => Response::Error {
            message: e.to_string(),
        },
    }
}

/// `Open` on a run-owned thread: the log from `from_seq`, and the run's
/// live events from here on. No actor is started — the run's task is the
/// thread's only writer — and nothing is resumed.
fn open_run(
    threads: &Arc<ThreadTable>,
    run: RunThread,
    thread: Ulid,
    from_seq: u64,
    out: &mpsc::UnboundedSender<String>,
    open: &mut HashMap<Ulid, OpenThread>,
) -> Response {
    let events = match threads.events_from(thread, from_seq) {
        Ok(events) => events,
        Err(e) => return thread_error(e),
    };
    // A run-owned thread's state says the run drives it: no actor is
    // started for it, and only the run's task is running it. A lead whose
    // task has ended reads `idle`, which is what it is — the log holds
    // everything either way, and `run_finished` says how it ended.
    let state = if threads.runs().held(thread) {
        ThreadState::Running {
            by: Author::Agent(AgentId(aigentic_runtime::workflow::RUNNER.into())),
            queued: 0,
        }
    } else {
        ThreadState::Idle
    };
    if !open.contains_key(&thread) {
        watch_run(threads, thread, out, open);
    }
    Response::Opened {
        state,
        events,
        run: run.wire(),
        // A run's child works in `auto` with no approver; there is no
        // mode to read from the log, and this is what it runs as.
        mode: Mode::Auto.name().to_owned(),
        profile: None,
        model: "runner".into(),
        effort: None,
    }
}

/// Watch a run's lead: every notice the run broadcasts goes to this
/// session from now on. Nothing is written and no actor is started.
fn watch_run(
    threads: &Arc<ThreadTable>,
    lead: Ulid,
    out: &mpsc::UnboundedSender<String>,
    open: &mut HashMap<Ulid, OpenThread>,
) {
    let (notices, mut notice_rx) = mpsc::unbounded_channel::<Notice>();
    threads.runs().watch(lead, notices);
    let out = out.clone();
    let forward = tokio::spawn(async move {
        while let Some(notice) = notice_rx.recv().await {
            if out.send(encode(&Frame::notice(notice))).is_err() {
                break;
            }
        }
    });
    if let Some(old) = open.insert(
        lead,
        OpenThread {
            mailbox: None,
            forward,
        },
    ) {
        old.forward.abort();
    }
}

fn thread_error(e: ThreadError) -> Response {
    match e {
        ThreadError::NoProject(_) | ThreadError::NoThread(_) => Response::Refused {
            reason: e.to_string(),
        },
        other => Response::Error {
            message: other.to_string(),
        },
    }
}

/// Subscribe this session to `thread`: the actor answers with the state
/// and the events since `from_seq`, and a task forwards its notices.
async fn subscribe(
    mailbox: &crate::actor::Mailbox,
    thread: Ulid,
    from_seq: u64,
    out: &mpsc::UnboundedSender<String>,
    open: &mut HashMap<Ulid, OpenThread>,
) -> Response {
    let (notices, mut notice_rx) = mpsc::unbounded_channel::<Notice>();
    let (reply, rx) = oneshot::channel();
    if mailbox
        .send(Mail::Subscribe {
            from_seq,
            notices,
            reply,
        })
        .is_err()
    {
        return Response::Error {
            message: "the thread's actor is gone".into(),
        };
    }
    let Ok((state, events, mode, (profile, model, effort))) = rx.await else {
        return Response::Error {
            message: "the thread's actor is gone".into(),
        };
    };
    let out = out.clone();
    let forward = tokio::spawn(async move {
        while let Some(n) = notice_rx.recv().await {
            if out.send(encode(&Frame::notice(n))).is_err() {
                break;
            }
        }
    });
    if let Some(old) = open.insert(
        thread,
        OpenThread {
            mailbox: Some(mailbox.clone()),
            forward,
        },
    ) {
        old.forward.abort();
    }
    Response::Opened {
        state,
        events,
        run: None,
        mode,
        profile,
        model,
        effort,
    }
}

/// Send mail to a thread this session has open (or opens on the fly,
/// for a request without an `Open` first) and wait for the reply.
async fn ask_actor(
    threads: &Arc<ThreadTable>,
    open: &mut HashMap<Ulid, OpenThread>,
    thread: Ulid,
    mail: impl FnOnce(oneshot::Sender<Response>) -> Mail,
) -> Response {
    let mailbox = match open.get(&thread).and_then(|entry| entry.mailbox.clone()) {
        Some(m) => m,
        None => match threads.mailbox(thread) {
            Some(m) => m,
            // A run-owned thread never gets an actor: starting one would
            // be a second writer over the run's log (issue #58, rule 5).
            None if threads.run_thread(thread) != RunThread::No => {
                return Response::Refused {
                    reason: "this thread belongs to a run".into(),
                };
            }
            None => match threads.open(thread).await {
                Ok((m, _)) => {
                    // Opened for this request only; not a subscription.
                    threads.close(thread);
                    m
                }
                Err(e) => return thread_error(e),
            },
        },
    };
    let (reply, rx) = oneshot::channel();
    if mailbox.send(mail(reply)).is_err() {
        return Response::Error {
            message: "the thread's actor is gone".into(),
        };
    }
    rx.await.unwrap_or(Response::Error {
        message: "the thread's actor dropped the request".into(),
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use aigentic_runtime::aigentic_core::UserId;

    use super::*;
    use crate::awake::detect;
    use crate::config::{Config, ProjectConfig, UserConfig};

    /// A project `name` at `dir/name`, with `participants` written into
    /// its file.
    fn project(dir: &std::path::Path, name: &str, participants: &str) -> PathBuf {
        let root = dir.join(name);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("aigentic.toml"),
            format!("[project]\nname = \"{name}\"\n{participants}"),
        )
        .unwrap();
        root
    }

    /// A factory the test never calls: it only needs to exist before the
    /// first build.
    struct Stub;

    impl crate::build::ProviderFactory for Stub {
        fn build(
            &self,
            _: &str,
        ) -> Result<
            (Box<dyn aigentic_runtime::aigentic_core::Provider>, String),
            crate::build::BuildError,
        > {
            Err(crate::build::BuildError::Config("no build".into()))
        }
    }

    fn table(dir: &std::path::Path, config: &Arc<ServerConfig>) -> ThreadTable {
        ThreadTable::new(
            Arc::new(
                Config::parse(
                    "default_profile = \"a\"\n[profiles.a]\nbase_url = \"u\"\nmodel = \"m\"\napi_key_env = \"K\"\n",
                )
                .unwrap(),
            ),
            dir.join("cfg"),
            config.clone(),
            Arc::new(Stub),
            Arc::new(crate::actor::NoReports),
            dir.join("threads"),
            detect(false),
        )
    }

    /// T3 (issue #81): the projects a creator can reach are the shared
    /// role helper's projects — a `read`-only role counts — and the
    /// listing shows exactly those. `mia` reads `alpha` and has no role
    /// in the other two, so the block names `alpha` alone.
    #[test]
    fn a_read_only_role_is_a_project_in_reach() {
        let dir = tempfile::tempdir().unwrap();
        let projects: Vec<ProjectConfig> = vec![
            // mia reads alpha and nothing else.
            ProjectConfig {
                name: "alpha".into(),
                root: project(dir.path(), "alpha", "[participants]\nmia = \"read\"\n"),
            },
            ProjectConfig {
                name: "beta".into(),
                root: project(dir.path(), "beta", "[participants]\nsteve = \"admin\"\n"),
            },
            ProjectConfig {
                name: "gamma".into(),
                root: project(dir.path(), "gamma", ""),
            },
        ];
        let config = Arc::new(ServerConfig {
            listen: "unix".into(),
            idle_unload_secs: 3600,
            users: vec![
                UserConfig {
                    name: "steve".into(),
                    token_env: None,
                    token: Some("t".into()),
                },
                UserConfig {
                    name: "mia".into(),
                    token_env: None,
                    token: Some("t2".into()),
                },
            ],
            projects: projects.clone(),
            resume_runs: false,
        });
        let table = table(dir.path(), &config);

        // A `read`-only role counts.
        assert_eq!(
            role_in_project(&config, &table, "alpha", "mia"),
            Some(Role::Read)
        );
        assert_eq!(role_in_project(&config, &table, "beta", "mia"), None);

        // The client's list and the rule agree: one set, no copy.
        let infos: Vec<String> = project_infos(&config, &table, "mia")
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(infos, vec!["alpha"]);

        // The listing is over exactly that set: no other project's name
        // appears anywhere in it.
        let listing = table
            .projects_in_reach(Some(&UserId("mia".into())), Some("alpha"))
            .unwrap();
        assert!(
            listing.starts_with("Projects in reach. This thread is in alpha "),
            "{listing}"
        );
        for name in ["alpha", "beta", "gamma"] {
            let allowed = role_in_project(&config, &table, name, "mia").is_some();
            assert_eq!(listing.contains(name), allowed, "{listing}");
        }
    }
}
