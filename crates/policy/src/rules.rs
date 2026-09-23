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
/// risk class, `path_prefix` by the call's `path` argument made relative
/// to the project root, and `command_allowed` only for a `bash` call
/// whose command matches an allow pattern. A rule with nothing set
/// matches everything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class: Option<RiskClass>,
    /// A path prefix under the project root, `/` separators, such as
    /// `.aigentic/memory/`; matches a call whose `path` argument resolves
    /// under it. A call without a `path` never matches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_prefix: Option<String>,
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
            path_prefix: None,
            command_allowed: false,
            decision,
            reason: reason.into(),
        }
    }

    pub fn tool(tool: &str, decision: Decision, reason: &str) -> Self {
        Self {
            tool: Some(tool.into()),
            class: None,
            path_prefix: None,
            command_allowed: false,
            decision,
            reason: reason.into(),
        }
    }

    /// A rule on one tool's `path` argument under a project-relative prefix.
    pub fn tool_path(tool: &str, path_prefix: &str, decision: Decision, reason: &str) -> Self {
        Self {
            tool: Some(tool.into()),
            class: None,
            path_prefix: Some(path_prefix.into()),
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
        let base = match (&self.tool, self.class) {
            (Some(tool), Some(class)) => format!("tool {tool} class {}", class_name(class)),
            (Some(tool), None) => format!("tool {tool}"),
            (None, Some(class)) => format!("class {}", class_name(class)),
            (None, None) => "any".into(),
        };
        match &self.path_prefix {
            Some(prefix) => format!("{base} path {prefix}"),
            None => base,
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

/// The project's memory folder, relative to its root; extraction writes
/// it after a turn and the model may not (phase 4 step 9).
pub const MEMORY_PREFIX: &str = ".aigentic/memory/";
pub const MEMORY_REASON: &str = "memory is written by extraction; edit it outside the thread";

/// The default table from `docs/PLAN-phase3.md` section 4, in order,
/// plus the phase 4 memory rows before the write-ask row.
pub fn default_rules() -> Vec<Rule> {
    vec![
        Rule::class(RiskClass::Safe, Decision::Allow, "harness self-management"),
        Rule::class(RiskClass::Read, Decision::Allow, "reading the workspace"),
        Rule {
            tool: Some("bash".into()),
            class: None,
            path_prefix: None,
            command_allowed: true,
            decision: Decision::Allow,
            reason: "the common case must not prompt".into(),
        },
        Rule::class(RiskClass::Exec, Decision::Ask, "anything else in a shell"),
        Rule::tool_path("write_file", MEMORY_PREFIX, Decision::Deny, MEMORY_REASON),
        Rule::tool_path("edit_file", MEMORY_PREFIX, Decision::Deny, MEMORY_REASON),
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

/// The default bash allow patterns: the commands a segment may start
/// with to be read-only. The full rule — quoting, chains, redirections
/// and each command's writing flags — is `shell::classify`'s.
pub fn default_bash_allow() -> Vec<String> {
    [
        "cargo fmt",
        "cargo build",
        "cargo test",
        "cargo clippy",
        "cargo check",
        "cargo run",
        "cargo doc",
        "cargo tree",
        "cargo metadata",
        "git status",
        "git diff",
        "git log",
        "git show",
        "git branch",
        "git ls-files",
        "git rev-parse",
        "git blame",
        // Read-only GitHub through the gh CLI; creating, commenting,
        // merging and pushing still ask.
        "gh issue list",
        "gh issue view",
        "gh pr list",
        "gh pr view",
        "gh pr checks",
        "gh pr diff",
        "gh pr status",
        "gh run list",
        "gh run view",
        "gh repo view",
        "gh label list",
        // Reads through the API; a method that writes or a field
        // without one still asks (issue #15).
        "gh api",
        "ls",
        "pwd",
        "cat",
        "head",
        "tail",
        "grep",
        "rg",
        "find",
        "wc",
        "which",
        "sort",
        "uniq",
        "diff",
        "stat",
        "du",
        "tree",
        "echo",
        // Reads a line at a time, numbered; harmless.
        "nl",
        // Column and character surgery on stdin: no file of its own.
        "cut",
        "tr",
        // A filter language, quoted as one argument; writing happens
        // only through the program, which the rules read.
        "awk",
        "jq",
        "sed",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

/// What a command line that is not all read-only asks about (issue
/// #16).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Riskiest {
    /// The riskiest segment's leading words — `git push` in
    /// `git add && git commit && git push` — so a grant covers that
    /// segment, never the whole chain. Empty when a redirection
    /// carries the risk: no command prefix covers that.
    pub words: Vec<String>,
    /// More than one segment has words or redirections: the line is a
    /// chain, and it asks once, as this segment.
    pub chain: bool,
}

/// What a `bash` command line that is not all read-only asks about
/// (issue #16): its riskiest segment — the worst kind a segment has,
/// the last of the worst — read against the default allow patterns.
/// `None` when every segment is read-only or harmless, so the line
/// runs without asking.
pub fn riskiest_segment(command: &str) -> Option<Riskiest> {
    let classified = crate::shell::classify(command, &default_bash_allow(), &[]);
    classified.riskiest.map(|words| Riskiest {
        words,
        chain: classified.compound,
    })
}

/// The words of `command` a person is asked to allow "from now on": the
/// command and its bare words (subcommands, flags) up to the first that
/// looks like a value (a path, a URL, a number, a quote, an assignment).
/// `curl -s https://x` gives `curl -s`; `cargo test --workspace` gives
/// all three. A line that is not all read-only gives its riskiest
/// segment's words (issue #16), so the grant covers that segment and
/// never the whole chain; a redirection carrying the risk gives nothing,
/// for no command prefix covers that.
pub fn prefix_of(command: &str) -> Vec<String> {
    if let Some(riskiest) = riskiest_segment(command) {
        return riskiest.words;
    }
    // The line runs without asking; a grant is moot, but its own first
    // segment's words still name it. A chain names nothing: no single
    // prefix stands for one.
    if crate::shell::is_compound(command) {
        return Vec::new();
    }
    let looks_like_value = |w: &str| {
        w.contains('/')
            || w.contains('.')
            || w.contains(':')
            || w.contains('=')
            || w.contains('"')
            || w.contains('\'')
            || w.chars().all(|c| c.is_ascii_digit())
    };
    let mut out = Vec::new();
    for (i, w) in command.split_whitespace().enumerate() {
        if i > 0 && looks_like_value(w) {
            break;
        }
        out.push(w.to_owned());
    }
    out
}

/// `allow = ["curl -s", "gh pr"]` in the rules file; missing or unreadable
/// is empty.
pub fn load_rules_file(path: &std::path::Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(value) = text.parse::<toml::Table>() else {
        return Vec::new();
    };
    value
        .get("allow")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// Append one pattern to the file's `allow` list, creating the file and
/// its directory.
pub fn append_rules_file(path: &std::path::Path, pattern: &str) -> std::io::Result<()> {
    let mut allow = load_rules_file(path);
    if !allow.contains(&pattern.to_owned()) {
        allow.push(pattern.to_owned());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut table = toml::Table::new();
    table.insert(
        "allow".into(),
        toml::Value::Array(allow.into_iter().map(toml::Value::String).collect()),
    );
    std::fs::write(
        path,
        format!(
            "# Commands allowed without asking, by leading words (`p` in the client).\n{}",
            toml::to_string(&table).expect("serialisable")
        ),
    )
}

#[cfg(test)]
mod prefix_tests {
    use super::*;

    #[test]
    fn prefixes_stop_at_the_first_value() {
        assert_eq!(prefix_of("curl -s https://example.com"), vec!["curl", "-s"]);
        assert_eq!(
            prefix_of("cargo test --workspace"),
            vec!["cargo", "test", "--workspace"]
        );
        assert_eq!(prefix_of("git log -n 5"), vec!["git", "log", "-n"]);
        assert_eq!(prefix_of("./run.sh now"), vec!["./run.sh", "now"]);
        assert!(prefix_of("ls | wc -l").is_empty());
    }

    #[test]
    fn a_chain_grants_its_riskiest_segment() {
        assert_eq!(prefix_of("curl -s https://example.com"), vec!["curl", "-s"]);
        assert_eq!(
            prefix_of("git add && git commit -m 'x' && git push"),
            vec!["git", "push"]
        );
        // The riskiest is the worst kind, the last of the worst, and a
        // redirection carries no prefix at all. The prefix stops at the
        // script, which is a value.
        assert_eq!(
            prefix_of("cargo test && sed -i 's/x/y/' f && git push"),
            vec!["sed", "-i"]
        );
        assert_eq!(
            prefix_of("cargo test && echo done > log.txt"),
            Vec::<String>::new()
        );
        assert!(prefix_of("ls | wc -l").is_empty());
        assert_eq!(prefix_of("./run.sh now"), vec!["./run.sh", "now"]);
    }

    #[test]
    fn the_rules_file_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".aigentic/rules.toml");
        assert!(load_rules_file(&path).is_empty());
        append_rules_file(&path, "curl -s").unwrap();
        append_rules_file(&path, "gh pr").unwrap();
        append_rules_file(&path, "curl -s").unwrap();
        assert_eq!(load_rules_file(&path), vec!["curl -s", "gh pr"]);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# Commands allowed"), "{text}");
    }
}
