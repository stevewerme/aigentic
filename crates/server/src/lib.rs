//! The daemon (phase 5): one `ThreadActor` per open thread as that
//! thread's single writer, sessions over a socket (step 7), roles
//! enforced before anything reaches the log. Depends on `runtime` and
//! `api`. See `docs/PLAN-phase5.md` section 3.

pub mod actor;

pub use actor::{Mail, Mailbox, NoReports, Reports, ThreadActor};
