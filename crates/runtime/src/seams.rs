//! Named seams. `policy_check` has its body since phase 3; the turn queue
//! still passes through until phase 5; compaction lives in `compaction.rs`.

use aigentic_core::{Author, ContentBlock, EventKind, RiskClass, ToolCall};
use aigentic_log::{
    DecisionScope, PermissionDecidedPayload, PermissionRequestedPayload, PolicyRecord,
};
use aigentic_policy::Outcome;
use ulid::Ulid;

use crate::approver::Answer;
use crate::mode::Mode;
use crate::{Runtime, RuntimeError, Signal};

/// What policy decided for one call, with the record the tool result
/// carries either way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Run(PolicyRecord),
    Refuse(PolicyRecord),
}

impl Verdict {
    pub fn record(&self) -> &PolicyRecord {
        match self {
            Verdict::Run(r) | Verdict::Refuse(r) => r,
        }
    }
}

/// A standing `AllowForSession` answer. Never persisted; each use is still
/// a `permission_decided` event referencing the first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionGrant {
    pub(crate) tool: String,
    /// For `bash`, the exact command; `None` for every other tool.
    pub(crate) command: Option<String>,
    pub(crate) first_event: Ulid,
    pub(crate) author: Author,
}

impl SessionGrant {
    fn matches(&self, call: &ToolCall) -> bool {
        self.tool == call.name && self.command == bash_command(call)
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
    /// then the session grants, then the approver, appending `permission_requested` and
    /// `permission_decided` so every human answer is an attributed event.
    pub fn policy_check(
        &mut self,
        call: &ToolCall,
        class: RiskClass,
        observe: &mut dyn FnMut(Signal<'_>),
    ) -> Result<Verdict, RuntimeError> {
        let reason = match self.policy.decide(call, class) {
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
            Outcome::Ask { reason } => reason,
        };

        // The mode stands in for the human on what the rules would ask
        // about, never on what they deny. The record names the mode.
        let mode_allows = match self.mode {
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
            .find(|g| g.matches(call))
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

        let answer = self.approver.ask(&request);
        let author = self.approver.author();
        let (allow, scope) = match answer {
            Answer::Allow => (true, DecisionScope::Once),
            Answer::AllowForSession => (true, DecisionScope::Session),
            Answer::Deny => (false, DecisionScope::Once),
        };
        let decided = self.append_decided(
            &call.id,
            allow,
            scope,
            author.clone(),
            Some(requested.id),
            observe,
        )?;
        if answer == Answer::AllowForSession {
            self.session_grants.push(SessionGrant {
                tool: call.name.clone(),
                command: bash_command(call),
                first_event: decided,
                author,
            });
        }
        let record = PolicyRecord::Human {
            event: decided,
            allow,
        };
        Ok(if allow {
            Verdict::Run(record)
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
        observe: &mut dyn FnMut(Signal<'_>),
    ) -> Result<Ulid, RuntimeError> {
        let payload = serde_json::to_value(PermissionDecidedPayload {
            call_id: call_id.to_owned(),
            allow,
            scope,
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

    /// Turn queue seam: a message that arrived mid-turn and should either
    /// be queued for the next turn or interrupt this one. Nothing queues
    /// before phase 5, so there is never one.
    pub fn turn_queue_next(&mut self) -> Option<(Author, Vec<ContentBlock>)> {
        None
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
