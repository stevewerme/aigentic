//! Who a session is, and what it may send: tokens to users, requests to
//! the roles they need, roles from the project file. Checked before
//! anything reaches a thread's mailbox, so a refused request never
//! appends.

use aigentic_api::Request;
use aigentic_runtime::aigentic_policy::{Participants, Role, needs};

use crate::config::ServerConfig;

/// The user a token names, by constant-time comparison against each
/// configured user's token (from its environment variable, or the
/// in-memory one of an embedded daemon). `None` for no match; the token
/// is never logged.
pub fn user_for_token(config: &ServerConfig, token: &str) -> Option<String> {
    let mut found = None;
    for user in &config.users {
        let expected = match (&user.token, &user.token_env) {
            (Some(t), _) => Some(t.clone()),
            (None, Some(var)) => std::env::var(var).ok(),
            (None, None) => None,
        };
        let Some(expected) = expected else {
            continue;
        };
        // Every user is compared, so timing does not say which matched.
        if constant_time_eq(expected.as_bytes(), token.as_bytes()) && found.is_none() {
            found = Some(user.name.clone());
        }
    }
    found
}

/// Equal length and equal bytes, without an early exit on the bytes.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Why a request was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Denied {
    pub reason: String,
}

/// Whether `user` may send `request` in a project whose file holds
/// `participants`. `Ok` carries nothing; `Err` says why.
pub fn allowed(
    user: &str,
    owner: Option<&str>,
    participants: &Participants,
    request: &Request,
) -> Result<(), Denied> {
    let Some(needed) = needs(request) else {
        return Ok(());
    };
    let role = participants.role(user, owner.unwrap_or(""));
    match role {
        Some(r) if r.covers(needed) => Ok(()),
        Some(r) => Err(Denied {
            reason: format!("{user} is {r} in this project; this needs {needed}"),
        }),
        None => Err(Denied {
            reason: format!("{user} has no role in this project; this needs {needed}"),
        }),
    }
}

/// The role `user` has in a project, for listings.
pub fn role_in(user: &str, owner: Option<&str>, participants: &Participants) -> Option<Role> {
    participants.role(user, owner.unwrap_or(""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::UserConfig;
    use ulid::Ulid;

    #[test]
    fn tokens_name_users_without_timing_or_echo() {
        let config = ServerConfig {
            listen: "unix".into(),
            idle_unload_secs: 1,
            users: vec![
                UserConfig {
                    name: "steve".into(),
                    token_env: None,
                    token: Some("s3".into()),
                },
                UserConfig {
                    name: "magnus".into(),
                    token_env: Some("AIGENTIC_TEST_TOKEN_UNSET".into()),
                    token: None,
                },
            ],
            projects: vec![],
        };
        assert_eq!(user_for_token(&config, "s3").as_deref(), Some("steve"));
        assert_eq!(user_for_token(&config, "s"), None);
        assert_eq!(user_for_token(&config, "s3 "), None);
        assert_eq!(
            user_for_token(&config, ""),
            None,
            "an unset variable never matches"
        );
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"ab", b"abc"));
    }

    #[test]
    fn roles_gate_requests_and_say_why() {
        let p: Participants =
            toml::from_str("magnus = \"approve\"\nreviewer = \"read\"\n").unwrap();
        let t = Ulid::from_parts(1, 1);
        let post = Request::Post {
            thread: t,
            blocks: vec![],
            interrupt: false,
        };
        assert!(allowed("magnus", Some("steve"), &p, &post).is_ok());
        let err = allowed("reviewer", Some("steve"), &p, &post).unwrap_err();
        assert_eq!(
            err.reason,
            "reviewer is read in this project; this needs write"
        );
        let err = allowed("nobody", Some("steve"), &p, &post).unwrap_err();
        assert!(
            err.reason.starts_with("nobody has no role"),
            "{}",
            err.reason
        );
        // The table names people, so the owner is not admin by default.
        assert!(allowed("steve", Some("steve"), &p, &post).is_err());
        // An empty table: the owner alone.
        let none = Participants::default();
        assert!(allowed("steve", Some("steve"), &none, &post).is_ok());
        assert!(allowed("magnus", Some("steve"), &none, &post).is_err());
        assert_eq!(role_in("steve", Some("steve"), &none), Some(Role::Admin));
        // Hello and the project list need no role.
        assert!(allowed("nobody", None, &none, &Request::ListProjects).is_ok());
    }
}
