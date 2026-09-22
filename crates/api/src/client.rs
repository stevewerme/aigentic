//! A `tokio` client over the line protocol: connect, `Hello`, then
//! requests matched to responses by id while notices flow to a channel.
//! Behind the `client` feature so the types stay runtime-free.

use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

use crate::{Body, Frame, Notice, PROTOCOL_VERSION, Request, Response, Welcome, decode, encode};

/// Where a daemon listens. `unix:/path` or `tcp:host:port`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Addr {
    Unix(PathBuf),
    Tcp(String, u16),
}

impl FromStr for Addr {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some(path) = s.strip_prefix("unix:") {
            if path.is_empty() {
                return Err("unix: needs a socket path".into());
            }
            return Ok(Addr::Unix(PathBuf::from(path)));
        }
        if let Some(rest) = s.strip_prefix("tcp:") {
            let (host, port) = rest
                .rsplit_once(':')
                .ok_or_else(|| format!("tcp: needs host:port, got {rest:?}"))?;
            let port = port
                .parse()
                .map_err(|_| format!("tcp: bad port in {rest:?}"))?;
            if host.is_empty() {
                return Err(format!("tcp: needs a host in {rest:?}"));
            }
            return Ok(Addr::Tcp(host.to_owned(), port));
        }
        Err(format!("expected unix:/path or tcp:host:port, got {s:?}"))
    }
}

impl std::fmt::Display for Addr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Addr::Unix(p) => write!(f, "unix:{}", p.display()),
            Addr::Tcp(h, p) => write!(f, "tcp:{h}:{p}"),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("connecting to {addr}: {source}")]
    Connect {
        addr: String,
        source: std::io::Error,
    },
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Decode(#[from] crate::DecodeError),
    #[error("the daemon closed the connection")]
    Closed,
    #[error("hello refused: {0}")]
    Refused(String),
    #[error("unexpected reply to hello: {0:?}")]
    BadWelcome(Box<Response>),
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Response>>>>;
type Writer = Arc<tokio::sync::Mutex<Box<dyn AsyncWrite + Unpin + Send>>>;

/// One connection. Cheap to clone; every clone shares the connection.
#[derive(Clone)]
pub struct Client {
    writer: Writer,
    pending: Pending,
    next_id: Arc<AtomicU64>,
    notices: Arc<Mutex<Option<mpsc::Receiver<Notice>>>>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("next_id", &self.next_id.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl Client {
    /// Connect and say hello. The token is sent once and never kept.
    pub async fn connect(addr: &Addr, token: &str) -> Result<(Self, Welcome), ClientError> {
        let (reader, writer): (
            Box<dyn AsyncRead + Unpin + Send>,
            Box<dyn AsyncWrite + Unpin + Send>,
        ) = match addr {
            Addr::Unix(path) => {
                let stream = tokio::net::UnixStream::connect(path)
                    .await
                    .map_err(|source| ClientError::Connect {
                        addr: addr.to_string(),
                        source,
                    })?;
                let (r, w) = stream.into_split();
                (Box::new(r), Box::new(w))
            }
            Addr::Tcp(host, port) => {
                let stream = tokio::net::TcpStream::connect((host.as_str(), *port))
                    .await
                    .map_err(|source| ClientError::Connect {
                        addr: addr.to_string(),
                        source,
                    })?;
                let (r, w) = stream.into_split();
                (Box::new(r), Box::new(w))
            }
        };
        let client = Self::over(reader, writer);
        let welcome = client
            .request(Request::Hello {
                protocol: PROTOCOL_VERSION,
                token: token.to_owned(),
            })
            .await?;
        match welcome {
            Response::Welcome(w) => Ok((client, w)),
            Response::Refused { reason } | Response::Error { message: reason } => {
                Err(ClientError::Refused(reason))
            }
            other => Err(ClientError::BadWelcome(Box::new(other))),
        }
    }

    /// A client over any pair of halves; the reader task starts here.
    pub fn over(
        reader: Box<dyn AsyncRead + Unpin + Send>,
        writer: Box<dyn AsyncWrite + Unpin + Send>,
    ) -> Self {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (notice_tx, notice_rx) = mpsc::channel(256);
        let reader_pending = pending.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(reader).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(frame) = decode(&line) else {
                    continue; // a frame this side cannot read is dropped, not fatal
                };
                match (frame.id, frame.body) {
                    (Some(id), Body::Response(response)) => {
                        let waiter = reader_pending
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .remove(&id);
                        if let Some(tx) = waiter {
                            let _ = tx.send(response);
                        }
                    }
                    (_, Body::Notice(notice)) => {
                        if notice_tx.send(notice).await.is_err() {
                            break;
                        }
                    }
                    _ => {} // a request from the daemon, or a response without an id
                }
            }
            // Closed: every waiter learns it by its sender dropping.
            reader_pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
        });
        Self {
            writer: Arc::new(tokio::sync::Mutex::new(writer)),
            pending,
            next_id: Arc::new(AtomicU64::new(1)),
            notices: Arc::new(Mutex::new(Some(notice_rx))),
        }
    }

    /// Send a request and wait for its response.
    pub async fn request(&self, request: Request) -> Result<Response, ClientError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, tx);
        let line = encode(&Frame::request(id, request));
        {
            let mut w = self.writer.lock().await;
            w.write_all(line.as_bytes()).await?;
            w.flush().await?;
        }
        rx.await.map_err(|_| ClientError::Closed)
    }

    /// The notice stream. It can be taken once; a second call gets
    /// `None`.
    pub fn take_notices(&self) -> Option<mpsc::Receiver<Notice>> {
        self.notices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ProjectInfo, ThreadState};
    use tokio::io::AsyncWriteExt;

    #[test]
    fn addresses_parse_both_ways() {
        assert_eq!(
            "unix:/tmp/a.sock".parse::<Addr>().unwrap(),
            Addr::Unix(PathBuf::from("/tmp/a.sock"))
        );
        assert_eq!(
            "tcp:vm.example:7420".parse::<Addr>().unwrap(),
            Addr::Tcp("vm.example".into(), 7420)
        );
        assert_eq!(Addr::Tcp("h".into(), 1).to_string(), "tcp:h:1");
        for bad in ["unix:", "tcp:host", "tcp::7420", "tcp:h:x", "http://x"] {
            assert!(bad.parse::<Addr>().is_err(), "{bad}");
        }
    }

    /// A daemon that answers two requests in reverse order and pushes a
    /// notice between them, over a Unix socket in a temp dir.
    #[tokio::test]
    async fn responses_match_by_id_out_of_order_and_notices_flow() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let thread = ulid::Ulid::from_parts(1_700_000_000_000, 1);
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (r, mut w) = stream.into_split();
            let mut lines = BufReader::new(r).lines();
            // Hello.
            let hello = decode(&lines.next_line().await.unwrap().unwrap()).unwrap();
            let Body::Request(Request::Hello { protocol, token }) = hello.body else {
                panic!("expected hello");
            };
            assert_eq!((protocol, token.as_str()), (PROTOCOL_VERSION, "secret"));
            let welcome = Response::Welcome(Welcome {
                user: "steve".into(),
                projects: vec![ProjectInfo {
                    name: "p".into(),
                    root: PathBuf::from("/p"),
                    role: Some("admin".into()),
                    threads: 0,
                }],
                server: "test".into(),
            });
            w.write_all(encode(&Frame::response(hello.id.unwrap(), welcome)).as_bytes())
                .await
                .unwrap();
            // Two requests, answered in reverse with a notice between.
            let a = decode(&lines.next_line().await.unwrap().unwrap()).unwrap();
            let b = decode(&lines.next_line().await.unwrap().unwrap()).unwrap();
            w.write_all(
                encode(&Frame::notice(Notice::State {
                    thread,
                    state: ThreadState::Idle,
                }))
                .as_bytes(),
            )
            .await
            .unwrap();
            w.write_all(
                encode(&Frame::response(
                    b.id.unwrap(),
                    Response::Text { text: "b".into() },
                ))
                .as_bytes(),
            )
            .await
            .unwrap();
            w.write_all(encode(&Frame::response(a.id.unwrap(), Response::Ok)).as_bytes())
                .await
                .unwrap();
            // A line this client cannot read is dropped, not fatal.
            w.write_all(b"{\"notice\":{\"kind\":\"from_the_future\"}}\n")
                .await
                .unwrap();
            // Then close.
        });

        let (client, welcome) = Client::connect(&Addr::Unix(path.clone()), "secret")
            .await
            .unwrap();
        assert_eq!(welcome.user, "steve");
        assert_eq!(welcome.projects[0].role.as_deref(), Some("admin"));
        let mut notices = client.take_notices().unwrap();
        assert!(client.take_notices().is_none());
        let c2 = client.clone();
        let (a, b) = tokio::join!(
            client.request(Request::ListProjects),
            c2.request(Request::Report {
                thread,
                report: crate::ReportKind::Cost
            })
        );
        assert_eq!(a.unwrap(), Response::Ok);
        assert_eq!(b.unwrap(), Response::Text { text: "b".into() });
        assert_eq!(
            notices.recv().await.unwrap(),
            Notice::State {
                thread,
                state: ThreadState::Idle
            }
        );
        server.await.unwrap();
        // After the daemon closes, a request fails as Closed rather than
        // hanging.
        let err = client.request(Request::ListProjects).await;
        assert!(
            matches!(err, Err(ClientError::Closed) | Err(ClientError::Io(_))),
            "{err:?}"
        );
        assert!(notices.recv().await.is_none());
    }

    #[tokio::test]
    async fn a_refused_hello_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (r, mut w) = stream.into_split();
            let mut lines = BufReader::new(r).lines();
            let hello = decode(&lines.next_line().await.unwrap().unwrap()).unwrap();
            w.write_all(
                encode(&Frame::response(
                    hello.id.unwrap(),
                    Response::Refused {
                        reason: "unknown token".into(),
                    },
                ))
                .as_bytes(),
            )
            .await
            .unwrap();
        });
        let err = Client::connect(&Addr::Unix(path), "nope")
            .await
            .unwrap_err();
        assert!(
            matches!(err, ClientError::Refused(ref r) if r == "unknown token"),
            "{err}"
        );
        let err = Client::connect(&Addr::Unix(PathBuf::from("/nonexistent/x.sock")), "t")
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .starts_with("connecting to unix:/nonexistent/x.sock"),
            "{err}"
        );
    }
}
