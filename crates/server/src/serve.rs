//! `Server::serve`: accept connections on a Unix socket (TCP in step 8)
//! and hand each to a session; `Server::embed`: the same daemon in the
//! process over a private socket, for the single-user client.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, UnixListener};

use crate::actor::Reports;
use crate::build::{Profiles, ProviderFactory};
use crate::config::{Config, ProjectConfig, ServerConfig, UserConfig, default_socket_path};
use crate::reports::DefaultReports;
use crate::session;
use crate::threads::{ThreadTable, project_name_at};

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("{0}")]
    Config(#[from] crate::config::ConfigError),
    #[error("listening on {addr}: {source}")]
    Bind {
        addr: String,
        source: std::io::Error,
    },
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("listen = {0:?}: use \"unix\", \"unix:/path\" or \"tcp:host:port\"")]
    Listen(String),
}

/// Where to listen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Listener {
    Unix(PathBuf),
    /// Plain TCP with tokens; TLS is a reverse proxy's or an SSH
    /// tunnel's job in this phase (plan decision 9). Port 0 asks the
    /// system for one, which `Bound::addr` then reports.
    Tcp(String, u16),
}

impl Listener {
    /// From `server.toml`'s `listen`: `"unix"` derives the path,
    /// `"unix:/path"` names it, `"tcp:host:port"` binds TCP.
    pub fn parse(listen: &str) -> Result<Self, ServerError> {
        if listen == "unix" {
            return Ok(Listener::Unix(default_socket_path()));
        }
        if let Some(path) = listen.strip_prefix("unix:") {
            if path.is_empty() {
                return Err(ServerError::Listen(listen.to_owned()));
            }
            return Ok(Listener::Unix(PathBuf::from(path)));
        }
        if let Some(rest) = listen.strip_prefix("tcp:") {
            let (host, port) = rest
                .rsplit_once(':')
                .ok_or_else(|| ServerError::Listen(listen.to_owned()))?;
            let port = port
                .parse()
                .map_err(|_| ServerError::Listen(listen.to_owned()))?;
            if host.is_empty() {
                return Err(ServerError::Listen(listen.to_owned()));
            }
            return Ok(Listener::Tcp(host.to_owned(), port));
        }
        Err(ServerError::Listen(listen.to_owned()))
    }

    pub fn addr(&self) -> String {
        match self {
            Listener::Unix(p) => format!("unix:{}", p.display()),
            Listener::Tcp(h, p) => format!("tcp:{h}:{p}"),
        }
    }
}

enum Socket {
    Unix(UnixListener, PathBuf),
    Tcp(TcpListener),
}

/// A bound listener, not yet accepting: `addr` says where, which is
/// what a caller with port 0 needs.
pub struct Bound {
    server: Arc<Server>,
    socket: Socket,
}

impl Bound {
    /// Where it listens, with the real port.
    pub fn addr(&self) -> String {
        match &self.socket {
            Socket::Unix(_, path) => format!("unix:{}", path.display()),
            Socket::Tcp(l) => l
                .local_addr()
                .map(|a| format!("tcp:{}:{}", a.ip(), a.port()))
                .unwrap_or_else(|_| "tcp:?".into()),
        }
    }

    /// The TCP port, when TCP.
    pub fn port(&self) -> Option<u16> {
        match &self.socket {
            Socket::Tcp(l) => l.local_addr().ok().map(|a| a.port()),
            Socket::Unix(..) => None,
        }
    }

    /// Accept forever, sweeping idle threads on the side.
    pub async fn serve(self) -> Result<(), ServerError> {
        let server = self.server;
        let sweeper = {
            let server = server.clone();
            tokio::spawn(async move {
                let idle = server.server.idle_unload();
                let every = idle
                    .min(Duration::from_secs(60))
                    .max(Duration::from_secs(1));
                loop {
                    tokio::time::sleep(every).await;
                    let _ = server.threads.sweep(idle).await;
                }
            })
        };
        let spawn = |r: Box<dyn AsyncRead + Unpin + Send>,
                     w: Box<dyn AsyncWrite + Unpin + Send>| {
            let config = server.server.clone();
            let threads = server.threads.clone();
            tokio::spawn(async move {
                let _ = session::serve(r, w, config, threads).await;
            });
        };
        let result = match &self.socket {
            Socket::Unix(unix, _) => loop {
                match unix.accept().await {
                    Ok((stream, _)) => {
                        let (r, w) = stream.into_split();
                        spawn(Box::new(r), Box::new(w));
                    }
                    Err(e) => break Err(ServerError::Io(e)),
                }
            },
            Socket::Tcp(tcp) => loop {
                match tcp.accept().await {
                    Ok((stream, _)) => {
                        let _ = stream.set_nodelay(true);
                        let (r, w) = stream.into_split();
                        spawn(Box::new(r), Box::new(w));
                    }
                    Err(e) => break Err(ServerError::Io(e)),
                }
            },
        };
        sweeper.abort();
        if let Socket::Unix(_, path) = &self.socket {
            let _ = std::fs::remove_file(path);
        }
        result
    }
}

/// The daemon.
pub struct Server {
    pub config: Arc<Config>,
    pub config_dir: PathBuf,
    pub server: Arc<ServerConfig>,
    pub threads: Arc<ThreadTable>,
}

impl Server {
    /// Wire the table up. `providers` is `Profiles` in production;
    /// `reports` is `NoReports` until step 9.
    pub fn new(
        config: Config,
        config_dir: PathBuf,
        server: ServerConfig,
        providers: Arc<dyn ProviderFactory>,
        reports: Arc<dyn Reports>,
    ) -> Self {
        let config = Arc::new(config);
        let server = Arc::new(server);
        let threads_base = config
            .threads_dir
            .clone()
            .unwrap_or_else(crate::config::default_threads_dir);
        let threads = Arc::new(ThreadTable::new(
            config.clone(),
            config_dir.clone(),
            server.clone(),
            providers,
            reports,
            threads_base,
        ));
        Self {
            config,
            config_dir,
            server,
            threads,
        }
    }

    /// Production wiring: providers from the config's profiles, every
    /// report rendered.
    pub fn from_configs(config: Config, config_dir: PathBuf, server: ServerConfig) -> Self {
        let providers: Arc<dyn ProviderFactory> = Arc::new(Profiles(Arc::new(config.clone())));
        let reports = Arc::new(DefaultReports {
            global_instructions: config
                .global_instructions
                .clone()
                .unwrap_or_else(|| config_dir.join("instructions.md")),
        });
        Self::new(config, config_dir, server, providers, reports)
    }

    /// Bind. A stale Unix socket file from a dead daemon is removed
    /// first; a live one refuses the bind.
    pub async fn listen(self: Arc<Self>, listener: Listener) -> Result<Bound, ServerError> {
        let socket = match &listener {
            Listener::Unix(path) => {
                if path.exists() {
                    // Live or stale? A connect tells.
                    match tokio::net::UnixStream::connect(path).await {
                        Ok(_) => {
                            return Err(ServerError::Bind {
                                addr: listener.addr(),
                                source: std::io::Error::new(
                                    std::io::ErrorKind::AddrInUse,
                                    "a daemon is already listening",
                                ),
                            });
                        }
                        Err(_) => {
                            let _ = std::fs::remove_file(path);
                        }
                    }
                }
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let unix = UnixListener::bind(path).map_err(|source| ServerError::Bind {
                    addr: listener.addr(),
                    source,
                })?;
                Socket::Unix(unix, path.clone())
            }
            Listener::Tcp(host, port) => {
                let tcp = TcpListener::bind((host.as_str(), *port))
                    .await
                    .map_err(|source| ServerError::Bind {
                        addr: listener.addr(),
                        source,
                    })?;
                Socket::Tcp(tcp)
            }
        };
        Ok(Bound {
            server: self,
            socket,
        })
    }

    /// Bind and accept forever.
    pub async fn serve(self: Arc<Self>, listener: Listener) -> Result<(), ServerError> {
        self.listen(listener).await?.serve().await
    }

    /// A daemon in this process for one user over a private socket:
    /// what `aigentic` does when nothing listens. `root` becomes the one
    /// project (named by its file, else `_none`); the token is random and
    /// lives only in memory.
    pub async fn embed(
        config: Config,
        config_dir: PathBuf,
        root: PathBuf,
        user: &str,
    ) -> Result<Embedded, ServerError> {
        let providers: Arc<dyn ProviderFactory> = Arc::new(Profiles(Arc::new(config.clone())));
        let reports = Arc::new(DefaultReports {
            global_instructions: config
                .global_instructions
                .clone()
                .unwrap_or_else(|| config_dir.join("instructions.md")),
        });
        Self::embed_with(config, config_dir, root, user, providers, reports).await
    }

    /// `embed` with the seams a test scripts.
    pub async fn embed_with(
        config: Config,
        config_dir: PathBuf,
        root: PathBuf,
        user: &str,
        providers: Arc<dyn ProviderFactory>,
        reports: Arc<dyn Reports>,
    ) -> Result<Embedded, ServerError> {
        let dir = tempfile_dir()?;
        let socket = dir.join("aigentic.sock");
        let token = random_token();
        let project = project_name_at(&root);
        let server = ServerConfig {
            listen: format!("unix:{}", socket.display()),
            idle_unload_secs: 24 * 3600,
            users: vec![UserConfig {
                name: user.to_owned(),
                token_env: None,
                token: Some(token.clone()),
            }],
            projects: vec![ProjectConfig {
                name: project.clone(),
                root,
            }],
        };
        let server = Arc::new(Self::new(config, config_dir, server, providers, reports));
        let listener = Listener::Unix(socket.clone());
        let task = tokio::spawn(server.clone().serve(listener));
        // Wait for the socket to appear.
        for _ in 0..200 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        Ok(Embedded {
            socket,
            token,
            project,
            server,
            task,
            _dir: dir,
        })
    }
}

/// A running embedded daemon. Dropping it stops the daemon and removes
/// its socket.
pub struct Embedded {
    pub socket: PathBuf,
    /// Never printed; the client sends it in `Hello`.
    pub token: String,
    pub project: String,
    pub server: Arc<Server>,
    task: tokio::task::JoinHandle<Result<(), ServerError>>,
    _dir: PathBuf,
}

impl Drop for Embedded {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.socket);
        let _ = std::fs::remove_dir(&self._dir);
    }
}

fn tempfile_dir() -> std::io::Result<PathBuf> {
    let base = std::env::temp_dir();
    let dir = base.join(format!("aigentic-{}", ulid::Ulid::generate()));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// 32 random bytes as hex, from the ULID generator's entropy twice; no
/// extra dependency, and it never leaves the process.
pub fn random_token() -> String {
    let a = ulid::Ulid::generate().random();
    let b = ulid::Ulid::generate().random();
    format!("{a:020x}{b:020x}")
}
