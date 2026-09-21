//! `aigentic.toml` in the working directory: the proto-project file
//! (phase 3 decision 2). Optional; without it no skills are enabled, the
//! default policy applies and no MCP servers connect.

use std::path::{Path, PathBuf};

use aigentic_runtime::aigentic_policy::{Policy, Rule};
use aigentic_runtime::aigentic_tools::McpServerConfig;
use anyhow::Context;
use serde::Deserialize;

pub const FILE_NAME: &str = "aigentic.toml";

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectFile {
    #[serde(default)]
    pub skills: SkillsSection,
    #[serde(default)]
    pub policy: PolicySection,
    #[serde(default)]
    pub mcp_servers: Vec<McpServerConfig>,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillsSection {
    #[serde(default)]
    pub enabled: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicySection {
    /// Prepended to the defaults.
    #[serde(default)]
    pub rules: Vec<Rule>,
    /// Replaces the default bash allow patterns when set.
    #[serde(default)]
    pub bash_allow: Option<Vec<String>>,
}

impl ProjectFile {
    /// `dir/aigentic.toml`, or the defaults when it does not exist.
    pub fn load(dir: &Path) -> anyhow::Result<(Self, Option<PathBuf>)> {
        let path = dir.join(FILE_NAME);
        match std::fs::read_to_string(&path) {
            Ok(text) => Ok((
                Self::parse(&text).with_context(|| format!("reading {}", path.display()))?,
                Some(path),
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((Self::default(), None)),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn parse(text: &str) -> anyhow::Result<Self> {
        Ok(toml::from_str(text)?)
    }

    pub fn policy(&self) -> Policy {
        Policy::configured(self.policy.rules.clone(), self.policy.bash_allow.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aigentic_runtime::aigentic_core::RiskClass;
    use aigentic_runtime::aigentic_policy::Decision;
    use aigentic_runtime::aigentic_tools::McpTransport;

    #[test]
    fn the_plan_example_parses() {
        let text = r#"
[skills]
enabled = ["implement", "tdd", "code-review", "diagnosing-bugs", "grilling"]

[policy]
rules = [{ class = "write", decision = "allow" }]
bash_allow = ["cargo", "git status", "git diff", "pnpm test"]

[[mcp_servers]]
name = "docs"
transport = { stdio = { command = "npx", args = ["-y", "@example/docs-mcp"] } }
class = "read"
"#;
        let p = ProjectFile::parse(text).unwrap();
        assert_eq!(p.skills.enabled.len(), 5);
        assert_eq!(p.policy.rules[0].class, Some(RiskClass::Write));
        assert_eq!(p.policy.rules[0].decision, Decision::Allow);
        assert_eq!(p.mcp_servers[0].name, "docs");
        assert!(matches!(
            p.mcp_servers[0].transport,
            McpTransport::Stdio { .. }
        ));
        assert_eq!(p.mcp_servers[0].class, RiskClass::Read);
        let policy = p.policy();
        assert_eq!(policy.rules[0].class, Some(RiskClass::Write));
        assert_eq!(
            policy.bash_allow,
            vec!["cargo", "git status", "git diff", "pnpm test"]
        );
    }

    #[test]
    fn empty_and_unknown_fields() {
        let p = ProjectFile::parse("").unwrap();
        assert_eq!(p, ProjectFile::default());
        assert_eq!(p.policy(), Policy::defaults());
        assert!(ProjectFile::parse("[skills]\nenable = []\n").is_err());
        assert!(ProjectFile::parse("[nope]\n").is_err());
    }

    #[test]
    fn missing_file_is_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let (p, path) = ProjectFile::load(dir.path()).unwrap();
        assert_eq!(p, ProjectFile::default());
        assert!(path.is_none());
    }
}
