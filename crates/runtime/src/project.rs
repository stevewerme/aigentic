//! The project: `aigentic.toml` and the `.aigentic/` folders beside it.
//! A project is plain files in the repository, edited by humans and read
//! into the stable prefix. See `docs/PLAN-phase4.md` section 3.

use std::path::{Path, PathBuf};

use aigentic_core::Budget;
use aigentic_policy::{Policy, Rule};
use aigentic_tools::McpServerConfig;
use serde::Deserialize;

use crate::config_keys::Table;
use crate::{CompactionSettings, DEFAULT_BUDGET, DEFAULT_COMPACTION};

pub const FILE_NAME: &str = "aigentic.toml";
/// The project's folders, under the root beside `aigentic.toml`.
pub const DOT_DIR: &str = ".aigentic";
pub const INSTRUCTIONS_FILE: &str = "instructions.md";
pub const KNOWLEDGE_DIR: &str = "knowledge";
pub const MEMORY_DIR: &str = "memory";
/// The memory block's heading; says who writes the files so the model
/// does not (phase 4 step 9).
pub const MEMORY_HEADING: &str = "# Project memory\n\nWritten by the harness after each turn from what participants \
stated. Edit these files outside the thread; a tool call that writes them is refused.\n";

/// The keys of each table in `aigentic.toml`, in one place so a struct
/// that gains a field has one line to add here. The walk in
/// `config_keys` reads them; the spec tree below is built from them.
pub const PROJECT_SECTION_KEYS: &[&str] = &["name", "description"];
pub const MODEL_SECTION_KEYS: &[&str] = &["profile"];
// BUDGET_KEYS and COMPACTION_KEYS live beside the shared structs below:
// `[budget]` and `[compaction]` appear in `aigentic.toml` and in a
// `config.toml` profile alike.
pub const BUDGET_KEYS: &[&str] = &[
    "max_iterations",
    "max_tokens",
    "max_wall_time_secs",
    "cache_read_price_ratio",
];
pub const COMPACTION_KEYS: &[&str] = &[
    "trigger_fraction",
    "keep_turns",
    "max_result_bytes",
    "summary_max_output_tokens",
    "keep_last_calls",
    "context_ceiling_tokens",
    "evict_above_tokens",
];
pub const TOOLS_SECTION_KEYS: &[&str] = &["allow", "bash_timeout_secs"];
pub const KNOWLEDGE_SECTION_KEYS: &[&str] = &["threshold_fraction", "max_hits"];
pub const MEMORY_SECTION_KEYS: &[&str] = &["enabled", "every_n_turns"];
pub const POCOCK_SECTION_KEYS: &[&str] = &[
    "issue_tracker",
    "triage_labels",
    "docs_dir",
    "prs_as_requests",
];
pub const SKILLS_SECTION_KEYS: &[&str] = &["enabled"];
pub const POLICY_SECTION_KEYS: &[&str] = &["rules", "bash_allow"];
/// One `[[mcp_servers]]` entry, shared by `aigentic.toml` and
/// `config.toml`; `crates/tools/src/mcp.rs` parses it in both.
pub const MCP_SERVER_KEYS: &[&str] = &[
    "name",
    "transport",
    "class",
    "enabled",
    "command",
    "args",
    "env",
];

/// `[budget]`, in `aigentic.toml` and in a config profile.
pub static BUDGET_SPEC: Table = Table::new(BUDGET_KEYS);
/// `[compaction]`, in both files.
pub static COMPACTION_SPEC: Table = Table::new(COMPACTION_KEYS);
/// A `[[mcp_servers]]` entry, shared by both files; its keys are the
/// struct's, which `crates/tools/src/mcp.rs` parses.
pub static MCP_SERVER_SPEC: Table = Table::new(MCP_SERVER_KEYS);

/// `[participants]`: a map of user names to roles. A role is a string,
/// not a table, so nothing under a name is ever judged or reported.
pub static PARTICIPANTS_SPEC: Table = Table::open();

/// The spec `aigentic.toml` is checked against.
pub fn project_spec() -> Table {
    static TABLES: &[(&str, Table)] = &[
        ("project", Table::new(PROJECT_SECTION_KEYS)),
        ("model", Table::new(MODEL_SECTION_KEYS)),
        ("budget", BUDGET_SPEC),
        ("compaction", COMPACTION_SPEC),
        ("tools", Table::new(TOOLS_SECTION_KEYS)),
        ("knowledge", Table::new(KNOWLEDGE_SECTION_KEYS)),
        ("memory", Table::new(MEMORY_SECTION_KEYS)),
        ("pocock", Table::new(POCOCK_SECTION_KEYS)),
        ("skills", Table::new(SKILLS_SECTION_KEYS)),
        ("policy", Table::new(POLICY_SECTION_KEYS)),
        ("mcp_servers", MCP_SERVER_SPEC),
        ("participants", PARTICIPANTS_SPEC),
    ];
    Table::with(
        &[
            "project",
            "model",
            "budget",
            "compaction",
            "tools",
            "knowledge",
            "memory",
            "pocock",
            "skills",
            "policy",
            "mcp_servers",
            "participants",
        ],
        TABLES,
    )
}

#[derive(Debug, thiserror::Error)]
pub enum ProjectError {
    #[error("{path}: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("{path}: [project] name is required once a phase 4 section is present")]
    NameRequired { path: PathBuf },
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// `aigentic.toml`. Every section is optional so a phase 3 file still
/// parses. Unknown fields are collected, not refused (issue #37):
/// [`ProjectFile::parse_with`] returns them beside the file, and a
/// wrapper `parse` drops them. Keys here are listed in `PROJECT_SPEC`.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct ProjectFile {
    #[serde(default)]
    pub project: Option<ProjectSection>,
    #[serde(default)]
    pub model: Option<ModelSection>,
    /// Overrides the profile's budget field by field.
    #[serde(default)]
    pub budget: Option<BudgetConfig>,
    /// Overrides the profile's compaction settings field by field.
    #[serde(default)]
    pub compaction: Option<CompactionConfig>,
    #[serde(default)]
    pub tools: ToolsSection,
    #[serde(default)]
    pub knowledge: KnowledgeSection,
    #[serde(default)]
    pub memory: MemorySection,
    #[serde(default)]
    pub pocock: Option<PocockSection>,
    #[serde(default)]
    pub skills: SkillsSection,
    #[serde(default)]
    pub policy: PolicySection,
    #[serde(default)]
    pub mcp_servers: Vec<McpServerConfig>,
    /// `[participants]`: user name to role (phase 5). Empty means the
    /// daemon's owner alone, as `admin`. An open map: `PROJECT_OPEN`
    /// keeps every name out of the unknown-key report.
    #[serde(default)]
    pub participants: aigentic_policy::Participants,
}

/// Keys in `PROJECT_SECTION_KEYS`.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct ProjectSection {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// Keys in `MODEL_SECTION_KEYS`.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct ModelSection {
    /// A profile in `config.toml`; `--profile` on the command line wins.
    pub profile: String,
}

/// `[tools]`: what this project's model may see and how the bash tool's
/// calls are limited. An empty `allow` means every registered tool; the
/// global layer narrows it further. Keys in `TOOLS_SECTION_KEYS`.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct ToolsSection {
    #[serde(default)]
    pub allow: Vec<String>,
    /// The bash tool's default wall-clock limit in seconds when a call
    /// passes no `timeout_secs`; 1 to 900, anything else is refused at
    /// parse time. Missing falls back to 120.
    #[serde(default)]
    pub bash_timeout_secs: Option<u64>,
}

impl ToolsSection {
    /// The bash tool's default wall-clock limit.
    pub fn bash_timeout(&self) -> std::time::Duration {
        self.bash_timeout_secs.map_or(
            aigentic_tools::DEFAULT_TIMEOUT,
            std::time::Duration::from_secs,
        )
    }
}

/// Keys in `KNOWLEDGE_SECTION_KEYS`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct KnowledgeSection {
    /// Of the model's window; over it the folder is indexed, not inlined.
    #[serde(default = "default_threshold")]
    pub threshold_fraction: f32,
    #[serde(default = "default_max_hits")]
    pub max_hits: usize,
}

fn default_threshold() -> f32 {
    0.4
}
fn default_max_hits() -> usize {
    5
}

impl Default for KnowledgeSection {
    fn default() -> Self {
        Self {
            threshold_fraction: default_threshold(),
            max_hits: default_max_hits(),
        }
    }
}

/// Keys in `MEMORY_SECTION_KEYS`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MemorySection {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_one")]
    pub every_n_turns: u32,
}

fn default_true() -> bool {
    true
}
fn default_one() -> u32 {
    1
}

impl Default for MemorySection {
    fn default() -> Self {
        Self {
            enabled: true,
            every_n_turns: 1,
        }
    }
}

/// What upstream's `setup-matt-pocock-skills` asks; `aigentic project
/// setup` renders the files its skills read from these.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct PocockSection {
    /// `github`, `gitlab` or `local`.
    pub issue_tracker: String,
    /// Overrides for the five canonical triage roles, role to this
    /// tracker's label (`needs-triage = "bug:triage"`); absent roles keep
    /// their canonical name.
    #[serde(default)]
    pub triage_labels: std::collections::BTreeMap<String, String>,
    /// Where `agents/` goes; default `docs`.
    #[serde(default)]
    pub docs_dir: Option<String>,
    /// Upstream's "PRs as a request surface" flag, off by default.
    #[serde(default)]
    pub prs_as_requests: bool,
}

/// Keys in `SKILLS_SECTION_KEYS`.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct SkillsSection {
    #[serde(default)]
    pub enabled: Vec<String>,
}

/// Keys in `POLICY_SECTION_KEYS`.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct PolicySection {
    /// Prepended to the defaults.
    #[serde(default)]
    pub rules: Vec<Rule>,
    /// Replaces the default bash allow patterns when set.
    #[serde(default)]
    pub bash_allow: Option<Vec<String>>,
}

/// A per-turn budget, every field optional. Used by `[profiles.<name>.budget]`
/// in the config and `[budget]` in the project file. Keys in
/// `BUDGET_KEYS`, which both files share.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct BudgetConfig {
    #[serde(default)]
    pub max_iterations: Option<u32>,
    #[serde(default)]
    pub max_tokens: Option<u64>,
    #[serde(default)]
    pub max_wall_time_secs: Option<u64>,
    /// What a cache read counts against `max_tokens`, as a fraction of
    /// an uncached input token; a profile sets its own to what its
    /// model actually charges. Missing takes the default quarter.
    #[serde(default)]
    pub cache_read_price_ratio: Option<f64>,
}

impl BudgetConfig {
    /// Missing fields take `base`.
    pub fn over(&self, base: Budget) -> Budget {
        Budget {
            max_iterations: self.max_iterations.unwrap_or(base.max_iterations),
            max_tokens: self.max_tokens.unwrap_or(base.max_tokens),
            max_wall_time: self
                .max_wall_time_secs
                .map_or(base.max_wall_time, std::time::Duration::from_secs),
            cache_read_price_ratio: self
                .cache_read_price_ratio
                .unwrap_or(base.cache_read_price_ratio),
        }
    }

    pub fn budget(&self) -> Budget {
        self.over(DEFAULT_BUDGET)
    }
}

/// Compaction settings, every field optional; same two homes as
/// `BudgetConfig`. Keys in `COMPACTION_KEYS`.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct CompactionConfig {
    #[serde(default)]
    pub trigger_fraction: Option<f32>,
    #[serde(default)]
    pub keep_turns: Option<usize>,
    #[serde(default)]
    pub max_result_bytes: Option<usize>,
    #[serde(default)]
    pub summary_max_output_tokens: Option<u64>,
    /// Calls of the running turn kept in full before in-turn eviction
    /// stubs the rest (issue #30).
    #[serde(default)]
    pub keep_last_calls: Option<usize>,
    /// What one call's context may cost, whatever the window (issue #30).
    #[serde(default)]
    pub context_ceiling_tokens: Option<u64>,
    /// The sweep runs only over this many tokens (issue #32).
    #[serde(default)]
    pub evict_above_tokens: Option<u64>,
}

impl CompactionConfig {
    pub fn over(&self, base: CompactionSettings) -> CompactionSettings {
        CompactionSettings {
            trigger_fraction: self.trigger_fraction.unwrap_or(base.trigger_fraction),
            keep_turns: self.keep_turns.unwrap_or(base.keep_turns),
            max_result_bytes: self.max_result_bytes.unwrap_or(base.max_result_bytes),
            summary_max_output_tokens: self
                .summary_max_output_tokens
                .unwrap_or(base.summary_max_output_tokens),
            keep_last_calls: self.keep_last_calls.unwrap_or(base.keep_last_calls),
            context_ceiling_tokens: self
                .context_ceiling_tokens
                .unwrap_or(base.context_ceiling_tokens),
            evict_above_tokens: self.evict_above_tokens.unwrap_or(base.evict_above_tokens),
        }
    }

    pub fn settings(&self) -> CompactionSettings {
        self.over(DEFAULT_COMPACTION)
    }
}

impl ProjectFile {
    /// The file and the dotted path of every key it set that
    /// `PROJECT_SPEC` does not list (issue #37: ignored, not refused).
    pub fn parse_with(text: &str) -> Result<(Self, Vec<String>), String> {
        let file: ProjectFile = toml::from_str(text).map_err(|e| e.to_string())?;
        if let Some(secs) = file.tools.bash_timeout_secs
            && !(1..=aigentic_tools::MAX_TIMEOUT_SECS).contains(&secs)
        {
            return Err(format!(
                "[tools] bash_timeout_secs must be 1 to {}, got {secs}",
                aigentic_tools::MAX_TIMEOUT_SECS
            ));
        }
        let unknown = match toml::from_str::<toml::Value>(text) {
            Ok(value) => project_spec().unknown(&value),
            // The typed parse above already failed on a syntax error, so
            // an unparsable value here is unreachable; say nothing about
            // keys rather than fail a file that parsed fine.
            Err(_) => Vec::new(),
        };
        Ok((file, unknown))
    }

    /// A file on its own, for callers with no line to report on.
    pub fn parse(text: &str) -> Result<Self, String> {
        Self::parse_with(text).map(|(file, _)| file)
    }

    /// The file and the dotted path of every key it set that
    /// `project_spec` does not list.
    pub fn load_with(path: &Path) -> Result<(Self, Vec<String>), ProjectError> {
        let text = std::fs::read_to_string(path).map_err(|source| ProjectError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let (file, unknown) = Self::parse_with(&text).map_err(|message| ProjectError::Parse {
            path: path.to_path_buf(),
            message,
        })?;
        Ok((file, unknown))
    }

    pub fn load(path: &Path) -> Result<Self, ProjectError> {
        Self::load_with(path).map(|(file, _)| file)
    }

    pub fn policy(&self) -> Policy {
        Policy::configured(self.policy.rules.clone(), self.policy.bash_allow.clone())
    }

    /// Whether any phase 4 section is present, which makes `[project]
    /// name` required.
    pub fn has_phase4_sections(&self) -> bool {
        self.model.is_some()
            || self.budget.is_some()
            || self.compaction.is_some()
            || !self.tools.allow.is_empty()
            || self.tools.bash_timeout_secs.is_some()
            || self.knowledge != KnowledgeSection::default()
            || self.memory != MemorySection::default()
            || self.pocock.is_some()
            || !self.participants.is_empty()
    }
}

/// The opened project: the file, the folders, their contents.
#[derive(Debug, Clone, PartialEq)]
pub struct Project {
    /// `[project] name`, or the root directory's name for a phase 3 file.
    pub name: String,
    /// Where `aigentic.toml` is.
    pub root: PathBuf,
    pub file: ProjectFile,
    /// `.aigentic/instructions.md`, else `AGENTS.md`.
    pub instructions: Option<String>,
    /// `.aigentic/memory/*.md` as `(file name, contents)`, sorted by name.
    pub memory: Vec<(String, String)>,
    /// Dotted paths of keys in `aigentic.toml` that `PROJECT_SPEC` does
    /// not list. Ignored, not refused (issue #37); reported at start.
    pub unknown: Vec<String>,
}

impl Project {
    /// The listed key a dotted path in `aigentic.toml` was probably meant
    /// to be.
    pub fn suggest_key(dotted: &str) -> Option<String> {
        project_spec().suggest(dotted)
    }

    /// The nearest `aigentic.toml` at or above `cwd`, or `None`.
    pub fn open(cwd: &Path) -> Result<Option<Self>, ProjectError> {
        let Some(root) = find_root(cwd) else {
            return Ok(None);
        };
        Self::open_root(&root).map(Some)
    }

    /// Open the project whose `aigentic.toml` is in `root`.
    pub fn open_root(root: &Path) -> Result<Self, ProjectError> {
        let path = root.join(FILE_NAME);
        let (file, unknown) = ProjectFile::load_with(&path)?;
        let name = match &file.project {
            Some(p) if !p.name.trim().is_empty() => p.name.trim().to_owned(),
            Some(_) => return Err(ProjectError::NameRequired { path }),
            None if file.has_phase4_sections() => {
                return Err(ProjectError::NameRequired { path });
            }
            None => root
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "project".into()),
        };
        let mut project = Self {
            name,
            root: root.to_path_buf(),
            file,
            instructions: None,
            memory: Vec::new(),
            unknown,
        };
        project.instructions = load_instructions(root)?;
        project.reload_memory()?;
        Ok(project)
    }

    pub fn dot_dir(&self) -> PathBuf {
        self.root.join(DOT_DIR)
    }

    pub fn knowledge_dir(&self) -> PathBuf {
        self.dot_dir().join(KNOWLEDGE_DIR)
    }

    pub fn memory_dir(&self) -> PathBuf {
        self.dot_dir().join(MEMORY_DIR)
    }

    /// Re-read the memory files: after an extraction, before the next turn.
    pub fn reload_memory(&mut self) -> Result<(), ProjectError> {
        self.memory = read_md_files(&self.memory_dir())?;
        Ok(())
    }

    /// The memory block for the prefix: each file under its name.
    pub fn memory_prefix(&self) -> Option<String> {
        if self.memory.iter().all(|(_, text)| text.trim().is_empty()) {
            return None;
        }
        let mut out = String::from(MEMORY_HEADING);
        for (name, text) in &self.memory {
            if text.trim().is_empty() {
                continue;
            }
            out.push_str(&format!("\n## {name}\n\n{}\n", text.trim_end()));
        }
        Some(out.trim_end().to_owned())
    }
}

/// Walk up from `cwd` to the nearest directory holding `aigentic.toml`.
pub fn find_root(cwd: &Path) -> Option<PathBuf> {
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        if d.join(FILE_NAME).is_file() {
            return Some(d.to_path_buf());
        }
        dir = d.parent();
    }
    None
}

/// `.aigentic/instructions.md` at `root`, else `AGENTS.md`, the
/// vendor-neutral convention. No tool-specific file is read.
pub fn load_instructions(root: &Path) -> Result<Option<String>, ProjectError> {
    let candidates = [
        root.join(DOT_DIR).join(INSTRUCTIONS_FILE),
        root.join("AGENTS.md"),
    ];
    for path in candidates {
        match std::fs::read_to_string(&path) {
            Ok(text) => return Ok(Some(text)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => return Err(ProjectError::Io { path, source }),
        }
    }
    Ok(None)
}

/// Every `*.md` directly in `dir`, `(file name, contents)`, sorted by name.
/// A missing directory is empty.
pub fn read_md_files(dir: &Path) -> Result<Vec<(String, String)>, ProjectError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(ProjectError::Io {
                path: dir.to_path_buf(),
                source,
            });
        }
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| ProjectError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") || !path.is_file() {
            continue;
        }
        let text = std::fs::read_to_string(&path).map_err(|source| ProjectError::Io {
            path: path.clone(),
            source,
        })?;
        files.push((entry.file_name().to_string_lossy().into_owned(), text));
    }
    files.sort();
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aigentic_core::RiskClass;
    use aigentic_policy::Decision;
    use aigentic_tools::McpTransport;

    const PHASE3: &str = r#"
[skills]
enabled = ["implement", "tdd"]

[policy]
rules = [{ class = "write", decision = "allow" }]
bash_allow = ["cargo", "git status"]

[[mcp_servers]]
name = "docs"
transport = { stdio = { command = "npx", args = ["-y", "@example/docs-mcp"] } }
class = "read"
"#;

    const PHASE4: &str = r#"
[project]
name = "vendela"
description = "The second real project of the phase 4 acceptance"

[model]
profile = "tensorx"

[budget]
max_tokens = 3000000

[compaction]
keep_turns = 4
context_ceiling_tokens = 96_000

[tools]
allow = ["read_file", "bash"]
bash_timeout_secs = 600

[knowledge]
threshold_fraction = 0.3

[memory]
every_n_turns = 2

[pocock]
issue_tracker = "github"
triage_labels = { needs-triage = "bug:triage" }
docs_dir = "docs"

[skills]
enabled = ["implement"]
"#;

    #[test]
    fn the_phase3_file_still_parses_and_has_no_phase4_sections() {
        let p = ProjectFile::parse(PHASE3).unwrap();
        assert_eq!(p.skills.enabled, vec!["implement", "tdd"]);
        assert_eq!(p.policy.rules[0].class, Some(RiskClass::Write));
        assert_eq!(p.policy.rules[0].decision, Decision::Allow);
        assert!(matches!(
            p.mcp_servers[0].transport,
            McpTransport::Stdio { .. }
        ));
        assert_eq!(p.mcp_servers[0].class, RiskClass::Read);
        assert!(!p.has_phase4_sections());
        // No [tools] bash_timeout_secs: the bash default stays 120.
        assert_eq!(p.tools.bash_timeout(), std::time::Duration::from_secs(120));
        assert_eq!(p.knowledge, KnowledgeSection::default());
        assert_eq!(p.memory.every_n_turns, 1);
        assert!(p.memory.enabled);
        let policy = p.policy();
        assert_eq!(policy.bash_allow, vec!["cargo", "git status"]);
    }

    #[test]
    fn the_phase4_file_parses_every_section() {
        let p = ProjectFile::parse(PHASE4).unwrap();
        assert_eq!(p.project.as_ref().unwrap().name, "vendela");
        assert_eq!(p.model.as_ref().unwrap().profile, "tensorx");
        assert_eq!(p.budget.as_ref().unwrap().budget().max_tokens, 3_000_000);
        assert_eq!(
            p.budget.as_ref().unwrap().budget().max_iterations,
            DEFAULT_BUDGET.max_iterations
        );
        assert_eq!(p.compaction.as_ref().unwrap().settings().keep_turns, 4);
        assert_eq!(
            p.compaction
                .as_ref()
                .unwrap()
                .settings()
                .context_ceiling_tokens,
            96_000
        );
        assert_eq!(p.tools.allow, vec!["read_file", "bash"]);
        assert_eq!(p.tools.bash_timeout(), std::time::Duration::from_secs(600));
        assert_eq!(p.knowledge.threshold_fraction, 0.3);
        assert_eq!(p.knowledge.max_hits, 5);
        assert_eq!(p.memory.every_n_turns, 2);
        let pocock = p.pocock.as_ref().unwrap();
        assert_eq!(pocock.issue_tracker, "github");
        assert_eq!(
            pocock.triage_labels.get("needs-triage").map(String::as_str),
            Some("bug:triage")
        );
        assert!(!pocock.prs_as_requests);
        assert!(p.has_phase4_sections());
    }

    #[test]
    fn unknown_fields_are_collected_not_refused() {
        // The shape this replaces ("unknown fields are rejected in every
        // section") is issue #37's whole point: a newer file must load.
        for text in [
            "[nope]\n",
            "[project]\nname = \"x\"\nbogus = 1\n",
            "[tools]\ndeny = []\n",
            "[memory]\nevery = 1\n",
            "",
        ] {
            assert!(ProjectFile::parse(text).is_ok(), "{text}");
        }
        let (_, unknown) = ProjectFile::parse_with("[nope]\n").unwrap();
        assert_eq!(unknown, vec!["nope"]);
        let (_, unknown) = ProjectFile::parse_with(
            "[project]\nname = \"x\"\nbogus = 1\n[tools]\nbash_timout_secs = 5\n",
        )
        .unwrap();
        assert_eq!(unknown, vec!["project.bogus", "tools.bash_timout_secs"]);
    }

    #[test]
    fn a_typo_gets_a_suggestion_from_the_same_table() {
        let spec = project_spec();
        assert_eq!(
            spec.suggest("tools.bash_timout_secs").as_deref(),
            Some("tools.bash_timeout_secs")
        );
        assert_eq!(spec.suggest("porject").as_deref(), Some("project"));
        assert_eq!(spec.suggest("nope"), None);
        // A key the spec cannot place at all has nothing to suggest.
        assert_eq!(spec.suggest("nope.bash_timout_secs"), None);
    }

    #[test]
    fn settings_and_services_are_never_reported() {
        // `[participants]` is a map of user names: no name in it may warn,
        // whatever it holds (issue #37's scope guard).
        let (_, unknown) = ProjectFile::parse_with(
            "[project]\nname = \"p\"\n[participants]\nsteve = \"admin\"\nmagnus = \"approve\"\n",
        )
        .unwrap();
        assert_eq!(unknown, Vec::<String>::new());
        // A `[[mcp_servers]]` entry answers to `mcp.rs`'s struct, so its
        // own key list is the one that judges it.
        let (_, unknown) = ProjectFile::parse_with(
            "[[mcp_servers]]\nname = \"docs\"\ntransport = { stdio = { command = \"npx\" } }\nclass = \"read\"\n",
        )
        .unwrap();
        assert_eq!(unknown, Vec::<String>::new());
    }

    /// Every key in `aigentic.toml`'s own spec, so a fixture can turn the
    /// walk's silence into evidence: a clean fixture means one of the two
    /// lists is missing a key, and this test says which.
    fn spec_paths() -> Vec<String> {
        let mut out = Vec::new();
        project_spec().paths("", &mut out);
        out.sort();
        out
    }

    #[test]
    fn the_spec_lists_every_key_a_file_can_set() {
        // The dotted paths, from the spec itself rather than hand-written
        // (a hand-written list would only repeat the spec's own mistake).
        let paths = spec_paths();
        for expected in [
            "budget.cache_read_price_ratio",
            "budget.max_iterations",
            "budget.max_tokens",
            "budget.max_wall_time_secs",
            "compaction.context_ceiling_tokens",
            "compaction.evict_above_tokens",
            "compaction.keep_last_calls",
            "compaction.keep_turns",
            "compaction.max_result_bytes",
            "compaction.summary_max_output_tokens",
            "compaction.trigger_fraction",
            "knowledge.max_hits",
            "knowledge.threshold_fraction",
            "mcp_servers.args",
            "mcp_servers.class",
            "mcp_servers.command",
            "mcp_servers.enabled",
            "mcp_servers.env",
            "mcp_servers.name",
            "mcp_servers.transport",
            "memory.enabled",
            "memory.every_n_turns",
            "model.profile",
            "participants",
            "pocock.docs_dir",
            "pocock.issue_tracker",
            "pocock.prs_as_requests",
            "pocock.triage_labels",
            "policy.bash_allow",
            "policy.rules",
            "project.description",
            "project.name",
            "skills.enabled",
            "tools.allow",
            "tools.bash_timeout_secs",
        ] {
            assert!(paths.contains(&expected.to_owned()), "missing {expected}");
        }
    }

    /// Every key `aigentic.toml`'s structs have, in one file. This is the
    /// checklist: a struct that gains a field without a line here and in
    /// `PROJECT_SPEC` fails `the_full_fixture_is_clean_under_the_spec`.
    const FULL_PROJECT: &str = "\
[project]
name = \"vendela\"
description = \"d\"
[model]
profile = \"tensorx\"
[budget]
max_iterations = 5
max_tokens = 1000
max_wall_time_secs = 60
cache_read_price_ratio = 0.25
[compaction]
trigger_fraction = 0.8
keep_turns = 2
max_result_bytes = 100
summary_max_output_tokens = 10
keep_last_calls = 3
context_ceiling_tokens = 90000
evict_above_tokens = 64000
[tools]
allow = [\"bash\"]
bash_timeout_secs = 30
[knowledge]
threshold_fraction = 0.5
max_hits = 2
[memory]
enabled = false
every_n_turns = 3
[pocock]
issue_tracker = \"local\"
triage_labels = { a = \"b\" }
docs_dir = \"docs\"
prs_as_requests = true
[skills]
enabled = [\"tdd\"]
[policy]
rules = [{ class = \"write\", decision = \"allow\" }]
bash_allow = [\"cargo\"]
[[mcp_servers]]
name = \"docs\"
transport = { stdio = { command = \"npx\" } }
class = \"read\"
[participants]
steve = \"admin\"
";

    #[test]
    fn the_full_fixture_is_clean_under_the_spec() {
        let (file, unknown) = ProjectFile::parse_with(FULL_PROJECT).unwrap();
        assert_eq!(unknown, Vec::<String>::new(), "{FULL_PROJECT}");
        // The fixture really is the whole struct, not a subset: a key the
        // struct has but the file leaves out would go unnoticed above.
        assert!(file.project.is_some() && file.model.is_some());
        assert!(file.budget.is_some() && file.compaction.is_some());
        assert!(file.pocock.is_some());
        assert!(file.has_phase4_sections());
    }

    #[test]
    fn bash_timeout_secs_is_validated() {
        assert!(ProjectFile::parse("[tools]\nbash_timeout_secs = 0\n").is_err());
        assert!(ProjectFile::parse("[tools]\nbash_timeout_secs = 901\n").is_err());
        let p = ProjectFile::parse("[tools]\nbash_timeout_secs = 900\n").unwrap();
        assert_eq!(p.tools.bash_timeout(), std::time::Duration::from_secs(900));
        assert!(p.has_phase4_sections());
        assert_eq!(
            ProjectFile::default().tools.bash_timeout(),
            std::time::Duration::from_secs(120)
        );
    }

    #[test]
    fn open_finds_the_file_from_a_subdirectory_and_none_outside() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        std::fs::create_dir_all(root.join("src/deep")).unwrap();
        std::fs::write(root.join(FILE_NAME), PHASE3).unwrap();
        let p = Project::open(&root.join("src/deep")).unwrap().unwrap();
        assert_eq!(p.root, root);
        assert_eq!(p.name, "repo", "phase 3 file: the directory names it");
        assert_eq!(p.instructions, None);
        assert!(p.memory.is_empty());
        assert_eq!(p.memory_prefix(), None);
        assert!(Project::open(dir.path()).unwrap().is_none());
    }

    #[test]
    fn a_phase4_section_requires_a_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE_NAME), "[tools]\nallow = [\"bash\"]\n").unwrap();
        let err = Project::open(dir.path()).unwrap_err();
        assert!(matches!(err, ProjectError::NameRequired { .. }), "{err}");
        std::fs::write(dir.path().join(FILE_NAME), "[project]\nname = \"  \"\n").unwrap();
        assert!(matches!(
            Project::open(dir.path()).unwrap_err(),
            ProjectError::NameRequired { .. }
        ));
        std::fs::write(dir.path().join(FILE_NAME), PHASE4).unwrap();
        assert_eq!(Project::open(dir.path()).unwrap().unwrap().name, "vendela");
    }

    #[test]
    fn instructions_fall_back_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(FILE_NAME), "").unwrap();
        assert_eq!(Project::open_root(root).unwrap().instructions, None);
        std::fs::write(root.join("CLAUDE.md"), "vendor file").unwrap();
        assert_eq!(
            Project::open_root(root).unwrap().instructions,
            None,
            "no tool-specific file is read"
        );
        std::fs::write(root.join("AGENTS.md"), "agents").unwrap();
        assert_eq!(
            Project::open_root(root).unwrap().instructions.as_deref(),
            Some("agents")
        );
        std::fs::create_dir_all(root.join(DOT_DIR)).unwrap();
        std::fs::write(root.join(DOT_DIR).join(INSTRUCTIONS_FILE), "ours").unwrap();
        assert_eq!(
            Project::open_root(root).unwrap().instructions.as_deref(),
            Some("ours")
        );
    }

    #[test]
    fn memory_files_are_read_sorted_and_reloaded() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(FILE_NAME), "").unwrap();
        let mem = root.join(DOT_DIR).join(MEMORY_DIR);
        std::fs::create_dir_all(&mem).unwrap();
        std::fs::write(mem.join("facts.md"), "- The repo is aigentic.\n").unwrap();
        std::fs::write(mem.join("decisions.md"), "- Use Swedish.\n").unwrap();
        std::fs::write(mem.join("notes.txt"), "ignored").unwrap();
        std::fs::write(mem.join("empty.md"), "\n").unwrap();
        let mut p = Project::open_root(root).unwrap();
        assert_eq!(
            p.memory.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
            vec!["decisions.md", "empty.md", "facts.md"]
        );
        assert_eq!(
            p.memory_prefix().unwrap(),
            format!(
                "{MEMORY_HEADING}\n## decisions.md\n\n- Use Swedish.\n\n## facts.md\n\n- The repo is aigentic."
            )
        );
        std::fs::write(mem.join("decisions.md"), "- Use Swedish.\n- Ship Friday.\n").unwrap();
        p.reload_memory().unwrap();
        assert!(p.memory_prefix().unwrap().contains("Ship Friday"));
    }

    #[test]
    fn participants_parse_and_require_a_name() {
        let f = ProjectFile::parse(
            "[project]\nname = \"p\"\n[participants]\nsteve = \"admin\"\nmagnus = \"approve\"\n",
        )
        .unwrap();
        assert_eq!(
            f.participants.role("magnus", "steve"),
            Some(aigentic_policy::Role::Approve)
        );
        assert!(f.has_phase4_sections());
        assert!(ProjectFile::parse("[participants]\nx = \"owner\"\n").is_err());
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(FILE_NAME),
            "[participants]\nsteve = \"admin\"\n",
        )
        .unwrap();
        assert!(
            Project::open_root(dir.path()).is_err(),
            "a phase 5 section needs [project] name"
        );
        assert!(
            ProjectFile::parse("[project]\nname = \"p\"\n")
                .unwrap()
                .participants
                .is_empty()
        );
    }
}
