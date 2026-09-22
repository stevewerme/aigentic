//! The daemon (phase 5): one `ThreadActor` per open thread as that
//! thread's single writer, sessions over a socket (step 7), roles
//! enforced before anything reaches the log. Depends on `runtime` and
//! `api`. See `docs/PLAN-phase5.md` section 3.

pub mod actor;
pub mod auth;
pub mod build;
pub mod config;
pub mod reports;
pub mod serve;
pub mod session;
pub mod skills;
pub mod threads;

pub use actor::{Mail, Mailbox, NoReports, Reports, ThreadActor};
pub use build::{Profiles, ProviderFactory};
pub use config::{ConfigError, ServerConfig};
pub use reports::DefaultReports;
pub use serve::{Bound, Embedded, Listener, Server, ServerError};
pub use threads::ThreadTable;
