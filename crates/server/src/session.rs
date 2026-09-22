//! One connection: `Hello` first, then requests in order, each checked
//! against the user's role before it reaches a thread, with notices from
//! every thread the session opened written back as they arrive.

use std::collections::HashMap;
use std::sync::Arc;

use aigentic_api::{
    Body, Frame, Notice, PROTOCOL_VERSION, ProjectInfo, Request, Response, Welcome, decode, encode,
};
use aigentic_runtime::Mode;
use aigentic_runtime::aigentic_core::{Author, UserId};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};
use ulid::Ulid;

use crate::actor::Mail;
use crate::auth;
use crate::config::ServerConfig;
use crate::threads::{ThreadError, ThreadTable};

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
                    writer_task.abort();
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

    // Open threads: mailbox per thread, and the task forwarding its
    // notices to the writer.
    let mut open: HashMap<Ulid, (crate::actor::Mailbox, tokio::task::JoinHandle<()>)> =
        HashMap::new();

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
    for (thread, (_, forward)) in open.drain() {
        forward.abort();
        threads.close(thread);
    }
    drop(out_tx);
    let _ = writer_task.await;
    Ok(())
}

fn project_infos(config: &ServerConfig, threads: &ThreadTable, user: &str) -> Vec<ProjectInfo> {
    threads
        .projects()
        .into_iter()
        .filter_map(|(name, root, count)| {
            let participants = threads.participants(&name).ok()?;
            let role = auth::role_in(user, config.owner(), &participants)?;
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
        Request::ListThreads { project } | Request::CreateThread { project } => {
            Some(project.clone())
        }
        Request::Open { thread, .. }
        | Request::Close { thread }
        | Request::Post { thread, .. }
        | Request::InvokeSkill { thread, .. }
        | Request::Decide { thread, .. }
        | Request::AnswerHuman { thread, .. }
        | Request::Pin { thread, .. }
        | Request::Compact { thread }
        | Request::SetMode { thread, .. }
        | Request::Report { thread, .. } => threads.project_of(*thread),
        Request::Hello { .. } | Request::ListProjects => None,
    }
}

async fn handle(
    config: &Arc<ServerConfig>,
    threads: &Arc<ThreadTable>,
    user: &str,
    author: &Author,
    open: &mut HashMap<Ulid, (crate::actor::Mailbox, tokio::task::JoinHandle<()>)>,
    out: &mpsc::UnboundedSender<String>,
    request: Request,
) -> Response {
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
    }

    match request {
        Request::Hello { .. } => Response::Refused {
            reason: "already said hello".into(),
        },
        Request::ListProjects => Response::Projects {
            projects: project_infos(config, threads, user),
        },
        Request::ListThreads { project } => match threads.list(&project) {
            Ok(list) => Response::Threads { threads: list },
            Err(e) => thread_error(e),
        },
        Request::CreateThread { project } => match threads.create(&project, author.clone()).await {
            Ok(info) => Response::Thread { thread: info },
            Err(e) => thread_error(e),
        },
        Request::Open { thread, from_seq } => {
            if let Some(mailbox) = open.get(&thread).map(|(m, _)| m.clone()) {
                // Already open here: answer from the actor again.
                return subscribe(&mailbox, thread, from_seq, out, open).await;
            }
            match threads.open(thread).await {
                Ok((mailbox, _project)) => subscribe(&mailbox, thread, from_seq, out, open).await,
                Err(e) => thread_error(e),
            }
        }
        Request::Close { thread } => {
            if let Some((_, forward)) = open.remove(&thread) {
                forward.abort();
                threads.close(thread);
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
        } => {
            let by = author.clone();
            ask_actor(threads, open, thread, |reply| Mail::Decide {
                by,
                call_id,
                allow,
                session,
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
        Request::Pin { thread, text } => {
            let author = author.clone();
            ask_actor(threads, open, thread, |reply| Mail::Pin {
                author,
                text,
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
        Request::Report { thread, report } => {
            ask_actor(threads, open, thread, |reply| Mail::Report {
                kind: report,
                reply,
            })
            .await
        }
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
    open: &mut HashMap<Ulid, (crate::actor::Mailbox, tokio::task::JoinHandle<()>)>,
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
    let Ok((state, events, mode)) = rx.await else {
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
    if let Some((_, old)) = open.insert(thread, (mailbox.clone(), forward)) {
        old.abort();
    }
    Response::Opened {
        state,
        events,
        mode,
    }
}

/// Send mail to a thread this session has open (or opens on the fly,
/// for a request without an `Open` first) and wait for the reply.
async fn ask_actor(
    threads: &Arc<ThreadTable>,
    open: &mut HashMap<Ulid, (crate::actor::Mailbox, tokio::task::JoinHandle<()>)>,
    thread: Ulid,
    mail: impl FnOnce(oneshot::Sender<Response>) -> Mail,
) -> Response {
    let mailbox = match open.get(&thread) {
        Some((m, _)) => m.clone(),
        None => match threads.mailbox(thread) {
            Some(m) => m,
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
