//! `Server::serve`: accept connections on a Unix socket (TCP in step 8)
//! and hand each to a session; `Server::embed`: the same daemon in the
//! process over a private socket, for the single-user client.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::UnixListener;

use crate::actor::{NoReports, Reports};
use crate::build::{Profiles, ProviderFactory};
use crate::config::{Config, ProjectConfig, ServerConfig, UserConfig, default_socket_path};
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
    #[error("listen = {0:?} is not supported by this daemon; use \"unix\" or \"unix:/path\"")]
    Listen(String),
}

/// Where to listen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Listener {
    Unix(PathBuf),
}

impl Listener {
    /// From `server.toml`'s `listen`: `"unix"` derives the path.
    pub fn parse(listen: &str) -> Result<Self, ServerError> {
        match listen {
            "unix" => Ok(Listener::Unix(default_socket_path())),
            s if s.starts_with("unix:") => Ok(Listener::Unix(PathBuf::from(&s[5..]))),
            other => Err(ServerError::Listen(other.to_owned())),
        }
    }

    pub fn addr(&self) -> String {
        match self {
            Listener::Unix(p) => format!("unix:{}", p.display()),
        }
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

    /// Production wiring: providers from the config's profiles.
    pub fn from_configs(config: Config, config_dir: PathBuf, server: ServerConfig) -> Self {
        let providers: Arc<dyn ProviderFactory> = Arc::new(Profiles(Arc::new(config.clone())));
        Self::new(config, config_dir, server, providers, Arc::new(NoReports))
    }

    /// Accept forever. A stale socket file from a dead daemon is removed
    /// first; a live one refuses the bind.
    pub async fn serve(self: Arc<Self>, listener: Listener) -> Result<(), ServerError> {
        let Listener::Unix(path) = &listener;
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
        let sweeper = {
            let server = self.clone();
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
        let result = loop {
            match unix.accept().await {
                Ok((stream, _)) => {
                    let (r, w) = stream.into_split();
                    let config = self.server.clone();
                    let threads = self.threads.clone();
                    tokio::spawn(async move {
                        let _ = session::serve(Box::new(r), Box::new(w), config, threads).await;
                    });
                }
                Err(e) => break Err(ServerError::Io(e)),
            }
        };
        sweeper.abort();
        let _ = std::fs::remove_file(path);
        result
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
        let server = Arc::new(Self::from_configs(config, config_dir, server));
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
