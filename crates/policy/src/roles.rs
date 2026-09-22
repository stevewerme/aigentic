//! Who may do what in a project (phase 5). Roles are per user per
//! project and live in the project file's `[participants]`, so adding a
//! person is a reviewed change in git. `needs` says which role each wire
//! request takes; the daemon checks it before anything reaches the log.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use aigentic_api::Request;
use serde::{Deserialize, Serialize};

/// Each role includes the ones before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Open, subscribe, reports.
    Read,
    /// Post, invoke a skill, pin, answer `ask_human`.
    Write,
    /// Decide permission requests, set the mode, compact.
    Approve,
    /// Everything, plus the project file itself (in git, not over the API).
    Admin,
}

impl Role {
    pub const ALL: [Role; 4] = [Role::Read, Role::Write, Role::Approve, Role::Admin];

    /// The name on the wire and in the project file.
    pub fn name(self) -> &'static str {
        match self {
            Role::Read => "read",
            Role::Write => "write",
            Role::Approve => "approve",
            Role::Admin => "admin",
        }
    }

    /// Whether this role covers what `needs` asks for.
    pub fn covers(self, needs: Role) -> bool {
        self >= needs
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Role {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Role::ALL
            .into_iter()
            .find(|r| r.name() == s.trim())
            .ok_or_else(|| format!("unknown role {s:?}; the roles are read, write, approve, admin"))
    }
}

/// `[participants]` in `aigentic.toml`: user name to role. An empty
/// table means the daemon's owner is `admin` and nobody else has a role,
/// so a project is private until its file says otherwise. Once the table
/// names anyone, it alone decides, the owner included.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Participants(pub BTreeMap<String, Role>);

impl Participants {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The role the table gives `user`, ignoring the owner rule.
    pub fn listed(&self, user: &str) -> Option<Role> {
        self.0.get(user).copied()
    }

    /// The role `user` has, with `owner` as `admin` when the table is
    /// empty.
    pub fn role(&self, user: &str, owner: &str) -> Option<Role> {
        if self.0.is_empty() {
            return (user == owner).then_some(Role::Admin);
        }
        self.listed(user)
    }

    /// Whether `user` may do what `needs` asks.
    pub fn allows(&self, user: &str, owner: &str, needs: Role) -> bool {
        self.role(user, owner).is_some_and(|r| r.covers(needs))
    }

    /// `name (role)` per participant, in name order, for `/who`.
    pub fn describe(&self) -> Vec<String> {
        self.0
            .iter()
            .map(|(name, role)| format!("{name} ({role})"))
            .collect()
    }
}

/// The role a request needs, or `None` for the ones any authenticated
/// session may send (`Hello`, and `ListProjects`, whose answer is
/// filtered to the user's projects).
pub fn needs(request: &Request) -> Option<Role> {
    match request {
        Request::Hello { .. } | Request::ListProjects => None,
        Request::ListThreads { .. }
        | Request::Open { .. }
        | Request::Close { .. }
        | Request::Report { .. } => Some(Role::Read),
        Request::CreateThread { .. }
        | Request::Post { .. }
        | Request::InvokeSkill { .. }
        | Request::AnswerHuman { .. }
        | Request::Pin { .. } => Some(Role::Write),
        Request::Decide { .. } | Request::SetMode { .. } | Request::Compact { .. } => {
            Some(Role::Approve)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aigentic_api::ReportKind;
    use ulid::Ulid;

    #[test]
    fn roles_order_include_and_name() {
        assert!(Role::Admin.covers(Role::Read));
        assert!(Role::Approve.covers(Role::Write));
        assert!(!Role::Write.covers(Role::Approve));
        assert!(Role::Read.covers(Role::Read));
        for r in Role::ALL {
            assert_eq!(r.name().parse::<Role>().unwrap(), r);
            assert_eq!(r.to_string(), r.name());
            assert_eq!(serde_json::to_value(r).unwrap(), r.name());
        }
        let err = "owner".parse::<Role>().unwrap_err();
        assert!(err.contains("read, write, approve, admin"), "{err}");
    }

    #[test]
    fn participants_parse_from_toml_and_the_owner_rule_holds() {
        let p: Participants =
            toml::from_str("steve = \"admin\"\nmagnus = \"approve\"\nreviewer = \"read\"\n")
                .unwrap();
        assert_eq!(p.role("magnus", "steve"), Some(Role::Approve));
        assert_eq!(p.role("nobody", "steve"), None);
        assert!(p.allows("magnus", "steve", Role::Write));
        assert!(!p.allows("magnus", "steve", Role::Admin));
        assert!(!p.allows("reviewer", "steve", Role::Write));
        assert_eq!(
            p.describe(),
            vec!["magnus (approve)", "reviewer (read)", "steve (admin)"]
        );
        assert!(toml::from_str::<Participants>("x = \"owner\"\n").is_err());

        // Empty: the owner is admin, nobody else has anything.
        let empty = Participants::default();
        assert_eq!(empty.role("steve", "steve"), Some(Role::Admin));
        assert_eq!(empty.role("magnus", "steve"), None);
        assert!(empty.allows("steve", "steve", Role::Approve));
        // Named: the table alone decides, the owner included.
        let others: Participants = toml::from_str("magnus = \"write\"\n").unwrap();
        assert_eq!(others.role("steve", "steve"), None);
    }

    #[test]
    fn every_request_has_its_row() {
        let t = Ulid::from_parts(1, 1);
        let rows: Vec<(Request, Option<Role>)> = vec![
            (
                Request::Hello {
                    protocol: 1,
                    token: "t".into(),
                },
                None,
            ),
            (Request::ListProjects, None),
            (
                Request::ListThreads {
                    project: "p".into(),
                },
                Some(Role::Read),
            ),
            (
                Request::Open {
                    thread: t,
                    from_seq: 0,
                },
                Some(Role::Read),
            ),
            (Request::Close { thread: t }, Some(Role::Read)),
            (
                Request::Report {
                    thread: t,
                    report: ReportKind::Cost,
                },
                Some(Role::Read),
            ),
            (
                Request::CreateThread {
                    project: "p".into(),
                },
                Some(Role::Write),
            ),
            (
                Request::Post {
                    thread: t,
                    blocks: vec![],
                    interrupt: false,
                },
                Some(Role::Write),
            ),
            (
                Request::InvokeSkill {
                    thread: t,
                    name: "tdd".into(),
                    args: String::new(),
                },
                Some(Role::Write),
            ),
            (
                Request::AnswerHuman {
                    thread: t,
                    call_id: "c".into(),
                    text: "yes".into(),
                },
                Some(Role::Write),
            ),
            (
                Request::Pin {
                    thread: t,
                    text: "x".into(),
                },
                Some(Role::Write),
            ),
            (
                Request::Decide {
                    thread: t,
                    call_id: "c".into(),
                    allow: true,
                    session: false,
                },
                Some(Role::Approve),
            ),
            (
                Request::SetMode {
                    thread: t,
                    mode: "auto".into(),
                },
                Some(Role::Approve),
            ),
            (Request::Compact { thread: t }, Some(Role::Approve)),
        ];
        for (request, role) in rows {
            assert_eq!(needs(&request), role, "{request:?}");
        }
        // A read user may open and report, not post; an approver may
        // decide; the interrupt flag changes nothing about the role.
        let p: Participants =
            toml::from_str("r = \"read\"\nw = \"write\"\na = \"approve\"\n").unwrap();
        let may =
            |user: &str, req: &Request| needs(req).is_none_or(|role| p.allows(user, "owner", role));
        let post = Request::Post {
            thread: t,
            blocks: vec![],
            interrupt: true,
        };
        let decide = Request::Decide {
            thread: t,
            call_id: "c".into(),
            allow: false,
            session: false,
        };
        assert!(may(
            "r",
            &Request::Open {
                thread: t,
                from_seq: 0
            }
        ));
        assert!(!may("r", &post));
        assert!(may("w", &post));
        assert!(!may("w", &decide));
        assert!(may("a", &decide));
        assert!(may("nobody", &Request::ListProjects));
    }
}
