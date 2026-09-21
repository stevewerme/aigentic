//! Rules and the default rule set. First match wins; `aigentic.toml` may
//! prepend rules and replace the bash allow patterns.

use aigentic_core::RiskClass;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allow,
    Ask,
    Deny,
}

/// One rule. A rule matches a call when every set field matches: `tool`
/// by exact name or a trailing `*` prefix (`mcp.*`), `class` by the tool's
/// risk class, and `command_allowed` only for a `bash` call whose command
/// matches an allow pattern. A rule with nothing set matches everything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class: Option<RiskClass>,
    /// The bash allow-pattern row of the default table; not for config.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub command_allowed: bool,
    pub decision: Decision,
    #[serde(default)]
    pub reason: String,
}

impl Rule {
    pub fn class(class: RiskClass, decision: Decision, reason: &str) -> Self {
        Self {
            tool: None,
            class: Some(class),
            command_allowed: false,
            decision,
            reason: reason.into(),
        }
    }

    pub fn tool(tool: &str, decision: Decision, reason: &str) -> Self {
        Self {
            tool: Some(tool.into()),
            class: None,
            command_allowed: false,
            decision,
            reason: reason.into(),
        }
    }

    /// The name a `PolicyRecord` carries: `class read`, `tool mcp.*`,
    /// `bash allow-pattern` (the pattern is appended by the caller), or
    /// `any`.
    pub fn name(&self) -> String {
        if self.command_allowed {
            return "bash allow-pattern".into();
        }
        match (&self.tool, self.class) {
            (Some(tool), Some(class)) => format!("tool {tool} class {}", class_name(class)),
            (Some(tool), None) => format!("tool {tool}"),
            (None, Some(class)) => format!("class {}", class_name(class)),
            (None, None) => "any".into(),
        }
    }

    pub(crate) fn tool_matches(&self, name: &str) -> bool {
        match &self.tool {
            None => true,
            Some(pattern) => match pattern.strip_suffix('*') {
                Some(prefix) => name.starts_with(prefix),
                None => pattern == name,
            },
        }
    }
}

pub(crate) fn class_name(class: RiskClass) -> &'static str {
    match class {
        RiskClass::Read => "read",
        RiskClass::Write => "write",
        RiskClass::Exec => "exec",
        RiskClass::Network => "network",
        RiskClass::Safe => "safe",
    }
}

/// The default table from `docs/PLAN-phase3.md` section 4, in order.
pub fn default_rules() -> Vec<Rule> {
    vec![
        Rule::class(RiskClass::Safe, Decision::Allow, "harness self-management"),
        Rule::class(RiskClass::Read, Decision::Allow, "reading the workspace"),
        Rule {
            tool: Some("bash".into()),
            class: None,
            command_allowed: true,
            decision: Decision::Allow,
            reason: "the common case must not prompt".into(),
        },
        Rule::class(RiskClass::Exec, Decision::Ask, "anything else in a shell"),
        Rule::class(RiskClass::Write, Decision::Ask, "changes the workspace"),
        Rule::class(
            RiskClass::Network,
            Decision::Ask,
            "MCP tools and, later, the web",
        ),
        Rule::tool(
            "mcp.*",
            Decision::Ask,
            "until a human downgrades a server's class",
        ),
    ]
}

/// The default bash allow patterns: a command whose leading words equal
/// the pattern's words, with no shell operators.
pub fn default_bash_allow() -> Vec<String> {
    [
        "cargo fmt",
        "cargo build",
        "cargo test",
        "cargo clippy",
        "cargo check",
        "cargo run",
        "git status",
        "git diff",
        "git log",
        "git show",
        "ls",
        "pwd",
        "cat",
        "head",
        "tail",
        "grep",
        "rg",
        "find",
        "wc",
        "echo",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}
