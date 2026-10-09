//! Named seams. `policy_check` has its body since phase 3 and, since
//! phase 5, parks on `Decisions` when the daemon owns the thread. The
//! turn queue is the daemon's actor, not a seam here; compaction lives
//! in `compaction.rs`.

use aigentic_core::{Author, EventKind, RiskClass, ToolCall};
use aigentic_log::{
    DecisionScope, PermissionDecidedPayload, PermissionRequestedPayload, PolicyRecord,
};
use aigentic_policy::Outcome;
use std::path::{Path, PathBuf};
use ulid::Ulid;

use crate::approver::Answer;
use crate::decisions::{Answered, CancelToken, Pending};
use crate::mode::Mode;
use crate::{Runtime, RuntimeError, Signal};

/// The rule name a boundary refusal records (issue #124): a file tool
/// outside the project, refused where no person could answer.
pub const BOUNDARY_RULE: &str = "boundary";

/// What policy decided for one call, with the record the tool result
/// carries either way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Run(PolicyRecord),
    Refuse(PolicyRecord),
    /// Refused with its own result text: the turn was interrupted while
    /// the request waited.
    RefuseWith {
        record: PolicyRecord,
        text: String,
    },
}

impl Verdict {
    pub fn record(&self) -> &PolicyRecord {
        match self {
            Verdict::Run(r) | Verdict::Refuse(r) | Verdict::RefuseWith { record: r, .. } => r,
        }
    }
}

/// A standing `AllowForSession` answer. Never persisted; each use is still
/// a `permission_decided` event referencing the first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionGrant {
    pub tool: String,
    /// For `bash`, the exact command; `None` for every other tool.
    pub command: Option<String>,
    /// For a file tool allowed outside the project (issue #124), the one
    /// canonical directory the answer covers — the parent of the path
    /// the person saw. `None` on every other grant, and a `None` grant
    /// never answers a boundary ask: "for this session" on an inside
    /// path must not wave an outside one through.
    pub boundary_dir: Option<PathBuf>,
    pub first_event: Ulid,
    pub author: Author,
}

impl SessionGrant {
    /// Whether this grant answers `call`. `outside` says the ask is a
    /// boundary one and `scope` is the canonical path it found, if any:
    /// a boundary ask is answered only by a grant scoped to that path's
    /// own directory, and an ordinary ask only by a tool-wide grant.
    fn matches(&self, call: &ToolCall, outside: bool, scope: Option<&Path>) -> bool {
        if self.tool != call.name {
            return false;
        }
        if !outside {
            return self.boundary_dir.is_none() && self.command == bash_command(call);
        }
        let (Some(dir), Some(target)) = (&self.boundary_dir, scope) else {
            return false;
        };
        match target.parent() {
            Some(parent) => parent == dir,
            // A target with no parent is a filesystem root; the grant
            // for it is the root itself.
            None => target == dir,
        }
    }
}

fn bash_command(call: &ToolCall) -> Option<String> {
    (call.name == "bash")
        .then(|| {
            call.args
                .get("command")
                .and_then(|c| c.as_str())
                .map(str::to_owned)
        })
        .flatten()
}

impl Runtime {
    /// The policy seam. Rules decide first; an `Ask` consults the mode,
    /// then the session grants, then `Decisions` when set (the turn parks
    /// until someone decides or `cancel` fires) or else the approver,
    /// appending `permission_requested` and
    /// `permission_decided` so every human answer is an attributed event.
    pub async fn policy_check(
        &mut self,
        call: &ToolCall,
        class: RiskClass,
        cancel: &CancelToken,
        observe: &mut (dyn FnMut(Signal<'_>) + Send),
    ) -> Result<Verdict, RuntimeError> {
        // The file boundary is asked first, even before the step overlay
        // (issue #124): nothing but a person outside a step may let a
        // file tool reach another project, so no overlay rule, no mode
        // and no session grant may answer it. The directory is the one
        // the tools will resolve against, read now, so a `bash` `cd` and
        // the check agree. Every path in the policy below is compared
        // from that same directory, which is what closes the
        // `cd .aigentic` memory escape.
        let cwd = self.registry.workdir().map(|w| w.current());
        let outcome = {
            let decided = self.policy.decide(call, class, cwd.as_deref());
            match decided {
                Outcome::AskBoundary { reason, path } => Outcome::AskBoundary { reason, path },
                inside => match self
                    .step
                    .as_ref()
                    .and_then(|step| step.overlay.decide(call))
                {
                    Some(overlay) => overlay,
                    None => inside,
                },
            }
        };
        // `outside` says this ask is the boundary's, `boundary_scope` the
        // canonical path a session grant may cover; both are absent for
        // an ordinary ask.
        let (reason, outside, boundary_scope) = match outcome {
            Outcome::Allow { rule } => {
                return Ok(Verdict::Run(PolicyRecord::rule(rule, "allow")));
            }
            Outcome::Deny { rule, reason } if reason.is_empty() => {
                return Ok(Verdict::Refuse(PolicyRecord::rule(rule, "deny")));
            }
            Outcome::Deny { rule, reason } => {
                return Ok(Verdict::Refuse(PolicyRecord::rule_with_reason(
                    rule, "deny", reason,
                )));
            }
            Outcome::Ask { reason } => (reason, false, None),
            // A step or job thread has no person to answer, and #77 needs
            // a job in `auto` to stay inside its project: outside a
            // boundary the step is refused outright.
            Outcome::AskBoundary { reason, .. } if self.step.is_some() => {
                return Ok(Verdict::Refuse(PolicyRecord::rule_with_reason(
                    BOUNDARY_RULE,
                    "deny",
                    format!("{reason} — a step never reaches another project"),
                )));
            }
            Outcome::AskBoundary { reason, path } => (reason, true, path),
        };

        // The mode stands in for the human on what the rules would ask
        // about, never on what they deny, and never on a boundary ask: an
        // outside path is a person's to answer in every mode. The record
        // names the mode.
        let mode_allows = !outside
            && match self.mode {
                Mode::Manual => false,
                Mode::AcceptEdits => class == RiskClass::Write,
                Mode::Auto => true,
            };
        if mode_allows {
            return Ok(Verdict::Run(PolicyRecord::rule(
                self.mode.rule_name(),
                "allow",
            )));
        }

        let request = PermissionRequestedPayload {
            call: call.clone(),
            class,
            reason,
        };
        let requested = self.append(
            EventKind::PermissionRequested,
            Author::System,
            serde_json::to_value(&request).expect("serialisable"),
            None,
            observe,
        )?;

        if let Some(grant) = self
            .session_grants
            .iter()
            .find(|g| g.matches(call, outside, boundary_scope.as_deref()))
            .cloned()
        {
            let decided = self.append_decided(
                &call.id,
                true,
                DecisionScope::Session,
                grant.author,
                Some(grant.first_event),
                observe,
            )?;
            return Ok(Verdict::Run(PolicyRecord::Human {
                event: decided,
                allow: true,
            }));
        }

        let mut prefix_to_allow: Option<Vec<String>> = None;
        let mut human_reason: Option<String> = None;
        let (allow, scope, author, reason) = match self.decisions.clone() {
            Some(decisions) => {
                let pending = Pending::Permission {
                    call_id: call.id.clone(),
                    request: request.clone(),
                };
                let rx = decisions.register(pending.clone());
                observe(Signal::Waiting(&pending));
                tokio::select! {
                    biased;
                    by = cancel.cancelled() => {
                        decisions.withdraw(&call.id);
                        (false, DecisionScope::Once, by, Some(crate::runtime::INTERRUPTED.to_owned()))
                    }
                    decided = rx => match decided {
                        Ok(Answered::Permission { allow, session, by, prefix, reason }) => {
                            if allow {
                                prefix_to_allow = prefix;
                            } else {
                                human_reason = reason.filter(|r| !r.trim().is_empty());
                            }
                            (
                                allow,
                                if session { DecisionScope::Session } else { DecisionScope::Once },
                                by,
                                None,
                            )
                        }
                        // An answer to another call's wait never reaches
                        // this one (`decide` refuses the pair), so it
                        // stays a deny by nobody rather than being read
                        // as one.
                        Ok(Answered::Switch { .. }) => (false, DecisionScope::Once, Author::System, None),
                        // The table dropped the sender: treat as a deny by nobody.
                        Ok(Answered::Human { .. }) | Err(_) => (false, DecisionScope::Once, Author::System, None),
                    },
                }
            }
            None => {
                let answer = self.approver.ask(&request);
                let author = self.approver.author();
                let (allow, scope) = match answer {
                    Answer::Allow => (true, DecisionScope::Once),
                    Answer::AllowForSession => (true, DecisionScope::Session),
                    Answer::Deny => (false, DecisionScope::Once),
                };
                (allow, scope, author, None)
            }
        };
        let interrupted = reason.is_some();
        let decided = self.append_decided_with(
            &call.id,
            allow,
            scope,
            author.clone(),
            Some(requested.id),
            reason.or_else(|| human_reason.clone()),
            observe,
        )?;
        let who = author_name(&author);
        // `p`: the prefix joins the allow list now and in the rules file.
        if allow && let Some(prefix) = prefix_to_allow {
            let pattern = prefix.join(" ");
            if let Err(e) = self.policy.allow_prefix(&pattern) {
                observe(Signal::Note(format!(
                    "could not write the rules file for `{pattern}`: {e}"
                )));
            }
        }
        if scope == DecisionScope::Session && allow {
            // Outside the project the answer covers one directory — the
            // parent of the canonical path the person was shown — and
            // never the tool (issue #124).
            // `Some` only when the answer can be scoped: a boundary ask
            // whose path could not be resolved is answered once, not for
            // the session, so no tool-wide grant is ever created for it.
            let boundary_dir = match (outside, boundary_scope.as_ref()) {
                (true, Some(p)) => Some(match p.parent() {
                    Some(parent) => parent.to_path_buf(),
                    None => p.clone(),
                }),
                (true, None) => None,
                (false, _) => None,
            };
            if !outside || boundary_scope.is_some() {
                self.session_grants.push(SessionGrant {
                    tool: call.name.clone(),
                    command: bash_command(call),
                    boundary_dir,
                    first_event: decided,
                    author,
                });
            }
        }
        let record = PolicyRecord::Human {
            event: decided,
            allow,
        };
        Ok(if allow {
            Verdict::Run(record)
        } else if let Some(why) = human_reason {
            Verdict::RefuseWith {
                record,
                text: format!("denied by {who}: {why}"),
            }
        } else if interrupted {
            Verdict::RefuseWith {
                record,
                text: format!(
                    "denied by policy: the turn was interrupted by {who} before a decision"
                ),
            }
        } else {
            Verdict::Refuse(record)
        })
    }

    fn append_decided(
        &mut self,
        call_id: &str,
        allow: bool,
        scope: DecisionScope,
        author: Author,
        parent: Option<Ulid>,
        observe: &mut (dyn FnMut(Signal<'_>) + Send),
    ) -> Result<Ulid, RuntimeError> {
        self.append_decided_with(call_id, allow, scope, author, parent, None, observe)
    }

    #[allow(clippy::too_many_arguments)]
    fn append_decided_with(
        &mut self,
        call_id: &str,
        allow: bool,
        scope: DecisionScope,
        author: Author,
        parent: Option<Ulid>,
        reason: Option<String>,
        observe: &mut (dyn FnMut(Signal<'_>) + Send),
    ) -> Result<Ulid, RuntimeError> {
        let payload = serde_json::to_value(PermissionDecidedPayload {
            call_id: call_id.to_owned(),
            allow,
            scope,
            reason,
        })
        .expect("serialisable");
        Ok(self
            .append(
                EventKind::PermissionDecided,
                author,
                payload,
                parent,
                observe,
            )?
            .id)
    }
}

pub(crate) fn author_name(author: &Author) -> String {
    match author {
        Author::User(u) => u.0.clone(),
        Author::Agent(a) => a.0.clone(),
        Author::System => "system".into(),
    }
}

/// The text of a refused call's error result.
pub fn denial_text(record: &PolicyRecord) -> String {
    match record {
        PolicyRecord::Rule {
            rule,
            reason: Some(reason),
            ..
        } => format!("denied by policy: {rule} ({reason})"),
        PolicyRecord::Rule { rule, .. } => format!("denied by policy: {rule}"),
        PolicyRecord::Human { .. } => "denied by policy: the human declined this call".into(),
    }
}
