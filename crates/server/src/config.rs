//! The daemon's configuration (phase 5): `config.toml` moved here from
//! the terminal binary, since the daemon builds every thread's provider,
//! layers and skills; and `server.toml`, the daemon's own file: where it
//! listens, its users and their token variables, its projects.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use aigentic_runtime::aigentic_core::{Budget, Provider};
use aigentic_runtime::aigentic_providers::{
    Anthropic, AnthropicConfig, OpenAiCompat, OpenAiCompatConfig, REASONING_EFFORT_PARAM,
    ReasoningEffort, Thinking,
};
use aigentic_runtime::config_keys::Table;
use aigentic_runtime::project::{
    BUDGET_SPEC, BudgetConfig, COMPACTION_SPEC, CompactionConfig, MCP_SERVER_KEYS,
};
use aigentic_runtime::{CompactionSettings, DEFAULT_BUDGET, DEFAULT_COMPACTION};
use serde::Deserialize;

/// A config that could not be read or does not make sense. The message
/// never echoes a value from the file, only positions and field names.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ConfigError(pub String);

impl ConfigError {
    pub fn msg(text: impl Into<String>) -> Self {
        Self(text.into())
    }
}

/// Which adapter a profile uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    #[default]
    OpenaiCompat,
    Anthropic,
}

/// One backend. The API key is deliberately absent: only its environment
/// variable's name is configured here.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Profile {
    #[serde(default)]
    pub provider: ProviderKind,
    /// Required for `openai_compat`; optional override for `anthropic`.
    #[serde(default)]
    pub base_url: Option<String>,
    pub model: String,
    /// Name of the environment variable holding the API key.
    pub api_key_env: String,
    #[serde(default)]
    pub max_context_tokens: Option<u64>,
    /// `anthropic` only: `adaptive` (default) or `off`.
    #[serde(default)]
    pub thinking: Option<String>,
    /// `anthropic` only: `low` | `medium` | `high` | `xhigh` | `max`.
    #[serde(default)]
    pub effort: Option<String>,
    /// `anthropic` only: `max_tokens` when the runtime sets no cap.
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
    /// `openai_compat` only: the reasoning effort to send. An integer
    /// (DeepSeek takes 1-100) or a label (`low` | `medium` | `high`);
    /// accepted values differ per endpoint, so the doctor's probe is the
    /// check. Absent means the field is not sent (issue #44).
    #[serde(default)]
    pub reasoning_effort: Option<ReasoningEffort>,
    /// `openai_compat` only: the param name the endpoint expects for the
    /// effort, a dotted path when it nests it. Defaults to
    /// `reasoning_effort` (issue #44).
    #[serde(default)]
    pub reasoning_effort_param: Option<String>,
    /// `anthropic` only: emit cache breakpoints (default true).
    #[serde(default)]
    pub cache: Option<bool>,
    /// When and how to compact; every field optional.
    #[serde(default)]
    pub compaction: Option<CompactionConfig>,
    /// Per-turn budget; every field optional.
    #[serde(default)]
    pub budget: Option<BudgetConfig>,
    /// USD per 1M tokens, for `/cost` and `aigentic stats` (issue #31).
    /// Absent means the endpoint is unpriced and calls carry no cost.
    #[serde(default)]
    pub prices: Option<PricesConfig>,
}

/// `[profiles.<name>.prices]`: USD per 1M tokens. `input` and `output`
/// are required; the cache rates default to them, so a config that knows
/// one number per direction still prices correctly.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PricesConfig {
    pub input: f64,
    pub output: f64,
    #[serde(default)]
    pub cache_read: Option<f64>,
    #[serde(default)]
    pub cache_write: Option<f64>,
}

impl PricesConfig {
    pub fn prices(&self) -> aigentic_runtime::Prices {
        aigentic_runtime::Prices {
            input: self.input,
            cache_read: self.cache_read.unwrap_or(self.input),
            cache_write: self.cache_write.unwrap_or(self.input),
            output: self.output,
        }
    }
}

/// The file on disk. Either the phase 0 flat form (top-level `base_url`,
/// `model`, `api_key_env`) or named `[profiles.<name>]` tables; not both.
#[derive(Debug, Default, Deserialize)]
struct ConfigFile {
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    api_key_env: Option<String>,
    #[serde(default)]
    max_context_tokens: Option<u64>,
    #[serde(default)]
    default_profile: Option<String>,
    #[serde(default)]
    utility_profile: Option<String>,
    #[serde(default)]
    profiles: BTreeMap<String, Profile>,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    threads_dir: Option<PathBuf>,
    #[serde(default)]
    bundled_dir: Option<PathBuf>,
    #[serde(default)]
    global: GlobalSection,
    #[serde(default)]
    tools: DeniedSection,
    #[serde(default)]
    skills: DeniedSection,
    #[serde(default)]
    display: DisplaySection,
}

/// `[global]`: the owner's instructions file.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct GlobalSection {
    /// Defaults to `instructions.md` beside the config file.
    #[serde(default)]
    pub instructions: Option<PathBuf>,
}

/// `[tools] denied` and `[skills] denied`: what no project may offer.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct DeniedSection {
    #[serde(default)]
    pub denied: Vec<String>,
}

/// `[display]`: how much of a tool result the terminal shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct DisplaySection {
    /// Lines of tool output shown before truncating.
    #[serde(default = "default_result_lines")]
    pub result_lines: usize,
    /// Bytes of tool output shown before truncating.
    #[serde(default = "default_result_bytes")]
    pub result_bytes: usize,
}

impl Default for DisplaySection {
    fn default() -> Self {
        Self {
            result_lines: default_result_lines(),
            result_bytes: default_result_bytes(),
        }
    }
}

fn default_result_lines() -> usize {
    3
}

fn default_result_bytes() -> usize {
    600
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub profiles: BTreeMap<String, Profile>,
    pub default_profile: String,
    /// The profile for side jobs (titles, memory extraction), phase 6
    /// step 9; unset, each thread's own profile does them.
    pub utility_profile: Option<String>,
    /// Author id for your messages; defaults to `$USER`.
    pub user: Option<String>,
    /// Where thread logs live; defaults to `~/.local/share/aigentic/threads`.
    pub threads_dir: Option<PathBuf>,
    /// The directory holding the bundled `skills/` and `skills.lock.toml`;
    /// defaults to the repository the binary was built from.
    pub bundled_dir: Option<PathBuf>,
    /// `[global] instructions`, when set.
    pub global_instructions: Option<PathBuf>,
    pub denied_tools: Vec<String>,
    pub denied_skills: Vec<String>,
    /// `[display]`: the tool-result caps; defaults when absent.
    pub display: DisplaySection,
    /// Dotted paths of keys `config_spec` does not list, as the last
    /// parse found them (issue #37). Empty for a `Config` built by hand.
    pub unknown: Vec<String>,
}

/// The keys of `config.toml`, in one place so a struct that gains a
/// field has one line to add. The walk in `config_keys` reads them.
pub const TOP_KEYS: &[&str] = &[
    "base_url",
    "model",
    "api_key_env",
    "max_context_tokens",
    "default_profile",
    "utility_profile",
    "profiles",
    "user",
    "threads_dir",
    "bundled_dir",
    "global",
    "tools",
    "skills",
    "display",
];
pub const PROFILE_KEYS: &[&str] = &[
    "provider",
    "base_url",
    "model",
    "api_key_env",
    "max_context_tokens",
    "thinking",
    "effort",
    "max_output_tokens",
    "reasoning_effort",
    "reasoning_effort_param",
    "cache",
    "compaction",
    "budget",
    "prices",
];
pub const PRICES_KEYS: &[&str] = &["input", "output", "cache_read", "cache_write"];
pub const GLOBAL_KEYS: &[&str] = &["instructions"];
pub const DENIED_KEYS: &[&str] = &["denied"];
pub const DISPLAY_KEYS: &[&str] = &["result_lines", "result_bytes"];

/// The spec `config.toml` is checked against (issue #37). `[profiles]` is
/// a map of user-chosen names, each holding a table of `PROFILE_KEYS`.
pub fn config_spec() -> Table {
    static PROFILE: Table = Table::with(
        PROFILE_KEYS,
        &[
            ("budget", BUDGET_SPEC),
            ("compaction", COMPACTION_SPEC),
            ("prices", Table::new(PRICES_KEYS)),
        ],
    );
    static PROFILES: Table = Table::named(&PROFILE);
    static GLOBAL: Table = Table::new(GLOBAL_KEYS);
    static DENIED: Table = Table::new(DENIED_KEYS);
    static DISPLAY: Table = Table::new(DISPLAY_KEYS);
    static MCP_SERVERS: Table = Table::new(MCP_SERVER_KEYS);
    static TOP: Table = Table::with(
        TOP_KEYS,
        &[
            ("profiles", PROFILES),
            ("global", GLOBAL),
            ("tools", DENIED),
            ("skills", DENIED),
            ("display", DISPLAY),
            ("mcp_servers", MCP_SERVERS),
        ],
    );
    TOP
}

pub const EXAMPLE: &str = r#"default_profile = "tensorx"

[profiles.tensorx]
provider = "openai_compat"
base_url = "https://api.tensorx.ai/v1"
model = "z-ai/glm-5.3"
api_key_env = "TENSORX_API_KEY"
# An integer 1-100 or "low"/"medium"/"high"; absent sends nothing.
# reasoning_effort = 50
# reasoning_effort_param = "reasoning_effort"

[profiles.anthropic]
provider = "anthropic"
model = "claude-opus-5"
api_key_env = "ANTHROPIC_API_KEY"
# effort = "high"
# thinking = "adaptive"
"#;

impl Config {
    /// The config and the table of its keys, so a caller can name the
    /// ones it ignored (issue #37).
    pub fn load_with(path: &Path) -> Result<(Self, Vec<String>), ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            ConfigError::msg(format!(
                "reading config {}: {e}\n\nCreate it with, for example:\n\n{EXAMPLE}",
                path.display()
            ))
        })?;
        Self::parse_with(&text)
            .map_err(|e| ConfigError::msg(format!("parsing {}: {e}", path.display())))
    }

    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        Self::load_with(path).map(|(config, _)| config)
    }

    /// Parse, reporting only the message and position on failure. The
    /// parser's default error quotes the offending line, which could echo a
    /// secret someone pasted into the file.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        Self::parse_with(text).map(|(config, _)| config)
    }

    /// Parse and collect the dotted path of every key `config_spec` does
    /// not list. Unknown keys are ignored, not refused (issue #37): the
    /// caller reports them where a human will see them.
    pub fn parse_with(text: &str) -> Result<(Self, Vec<String>), ConfigError> {
        let mut config = parse_config(text)?;
        let unknown = match toml::from_str::<toml::Value>(text) {
            Ok(value) => config_spec().unknown(&value),
            Err(_) => Vec::new(),
        };
        config.unknown = unknown.clone();
        Ok((config, unknown))
    }

    /// The listed top-level key a dotted path was probably meant to be.
    pub fn suggest_key(dotted: &str) -> Option<String> {
        config_spec().suggest(dotted)
    }
}

fn parse_config(text: &str) -> Result<Config, ConfigError> {
    {
        // A key in the file is refused by name, not ignored: `api_key`
        // is not in `config_spec`, so a file that sets it would otherwise
        // travel on as a silent no-op while the adapter builds with no
        // key at all.
        if let Ok(value) = toml::from_str::<toml::Value>(text)
            && let Some(found) = config_spec()
                .unknown(&value)
                .into_iter()
                .find(|path| path.ends_with("api_key"))
        {
            return Err(ConfigError::msg(format!(
                "{found}: the key itself is never read from a file, only from the \
                 environment variable named by api_key_env"
            )));
        }
        let file: ConfigFile = toml::from_str(text).map_err(|e| {
            let at = e
                .span()
                .map(|s| format!(" at byte {}", s.start))
                .unwrap_or_default();
            ConfigError::msg(format!("{}{at}", e.message()))
        })?;
        let flat = file.base_url.is_some() || file.model.is_some() || file.api_key_env.is_some();
        let (profiles, default_profile) = match (flat, file.profiles.is_empty()) {
            (true, false) => {
                return Err(ConfigError::msg(
                    "use either top-level base_url/model/api_key_env or [profiles.*], not both",
                ));
            }
            (true, true) => {
                let profile = Profile {
                    provider: ProviderKind::OpenaiCompat,
                    base_url: Some(
                        file.base_url
                            .ok_or_else(|| ConfigError::msg("missing base_url"))?,
                    ),
                    model: file
                        .model
                        .ok_or_else(|| ConfigError::msg("missing model"))?,
                    api_key_env: file
                        .api_key_env
                        .ok_or_else(|| ConfigError::msg("missing api_key_env"))?,
                    max_context_tokens: file.max_context_tokens,
                    thinking: None,
                    effort: None,
                    max_output_tokens: None,
                    reasoning_effort: None,
                    reasoning_effort_param: None,
                    cache: None,
                    compaction: None,
                    budget: None,
                    prices: None,
                };
                (
                    BTreeMap::from([("default".to_owned(), profile)]),
                    "default".to_owned(),
                )
            }
            (false, true) => return Err(ConfigError::msg("no profiles configured")),
            (false, false) => {
                let default = match file.default_profile {
                    Some(name) => name,
                    None if file.profiles.len() == 1 => {
                        file.profiles.keys().next().unwrap().clone()
                    }
                    None => {
                        return Err(ConfigError::msg(
                            "default_profile is required when more than one profile is defined",
                        ));
                    }
                };
                if !file.profiles.contains_key(&default) {
                    return Err(ConfigError::msg(format!(
                        "default_profile {default:?} is not a defined profile"
                    )));
                }
                (file.profiles, default)
            }
        };
        for (name, p) in &profiles {
            p.validate()
                .map_err(|e| ConfigError::msg(format!("profile {name:?}: {e}")))?;
        }
        if let Some(u) = &file.utility_profile
            && !profiles.contains_key(u)
        {
            return Err(ConfigError::msg(format!(
                "utility_profile {u:?} is not a defined profile"
            )));
        }
        Ok(Config {
            profiles,
            default_profile,
            utility_profile: file.utility_profile,
            user: file.user,
            threads_dir: file.threads_dir,
            bundled_dir: file.bundled_dir,
            global_instructions: file.global.instructions,
            denied_tools: file.tools.denied,
            denied_skills: file.skills.denied,
            display: file.display,
            unknown: Vec::new(),
        })
    }
}

impl Config {
    /// The named profile, or the default.
    pub fn select(&self, name: Option<&str>) -> Result<(&str, &Profile), ConfigError> {
        let name = name.unwrap_or(&self.default_profile);
        let (name, profile) = self.profiles.get_key_value(name).ok_or_else(|| {
            ConfigError::msg(format!(
                "no profile {name:?}; defined: {}",
                self.profiles.keys().cloned().collect::<Vec<_>>().join(", ")
            ))
        })?;
        Ok((name.as_str(), profile))
    }

    pub fn user_name(&self) -> String {
        self.user
            .clone()
            .or_else(|| std::env::var("USER").ok())
            .unwrap_or_else(|| "user".into())
    }
}

impl Profile {
    fn validate(&self) -> Result<(), ConfigError> {
        if let Some(b) = &self.budget
            && let Some(r) = b.cache_read_price_ratio
            && !(0.0..=1.0).contains(&r)
        {
            return Err(ConfigError::msg(format!(
                "budget.cache_read_price_ratio must be between 0.0 and 1.0, got {r}"
            )));
        }
        if let Some(c) = &self.compaction {
            if let Some(f) = c.trigger_fraction
                && !(0.05..=0.95).contains(&f)
            {
                return Err(ConfigError::msg(format!(
                    "compaction.trigger_fraction must be between 0.05 and 0.95, got {f}"
                )));
            }
            if let Some(n) = c.context_ceiling_tokens
                && n < 8_192
            {
                return Err(ConfigError::msg(
                    "compaction.context_ceiling_tokens must be at least 8192",
                ));
            }
        }
        match self.provider {
            ProviderKind::OpenaiCompat => {
                if self.base_url.is_none() {
                    return Err(ConfigError::msg(
                        "base_url is required for provider = \"openai_compat\"",
                    ));
                }
                for (field, set) in [
                    ("thinking", self.thinking.is_some()),
                    ("effort", self.effort.is_some()),
                    ("max_output_tokens", self.max_output_tokens.is_some()),
                    ("cache", self.cache.is_some()),
                ] {
                    if set {
                        return Err(ConfigError::msg(format!(
                            "{field} only applies to provider = \"anthropic\""
                        )));
                    }
                }
                // The param names where the effort goes, so it means
                // nothing without one (issue #44).
                match (&self.reasoning_effort, &self.reasoning_effort_param) {
                    (None, Some(_)) => {
                        return Err(ConfigError::msg(
                            "reasoning_effort_param needs reasoning_effort: the param names \
                             where an effort is sent",
                        ));
                    }
                    (Some(_), Some(param)) => {
                        let segments: Vec<&str> = param.split('.').collect();
                        if segments.is_empty() || segments.iter().any(|s| s.is_empty()) {
                            return Err(ConfigError::msg(
                                "reasoning_effort_param must be a param name or a dotted path \
                                 with no empty segment",
                            ));
                        }
                    }
                    _ => {}
                }
            }
            ProviderKind::Anthropic => {
                for (field, set) in [
                    ("reasoning_effort", self.reasoning_effort.is_some()),
                    (
                        "reasoning_effort_param",
                        self.reasoning_effort_param.is_some(),
                    ),
                ] {
                    if set {
                        return Err(ConfigError::msg(format!(
                            "{field} only applies to provider = \"openai_compat\""
                        )));
                    }
                }
                if let Some(t) = &self.thinking
                    && t != "adaptive"
                    && t != "off"
                {
                    return Err(ConfigError::msg(format!(
                        "thinking must be \"adaptive\" or \"off\", got {t:?}"
                    )));
                }
            }
        }
        Ok(())
    }

    /// The key, read only from the named environment variable.
    pub fn api_key(&self) -> Result<String, ConfigError> {
        std::env::var(&self.api_key_env).map_err(|_| {
            ConfigError::msg(format!(
                "environment variable {} is not set (named by api_key_env in the config; \
                 put it in .env or export it)",
                self.api_key_env
            ))
        })
    }

    pub fn compaction_settings(&self) -> CompactionSettings {
        self.compaction
            .as_ref()
            .map_or(DEFAULT_COMPACTION, CompactionConfig::settings)
    }

    pub fn budget(&self) -> Budget {
        self.budget
            .as_ref()
            .map_or(DEFAULT_BUDGET, BudgetConfig::budget)
    }

    /// The effort label the profile runs at: `effort` for anthropic,
    /// the openai_compat `reasoning_effort` as its string otherwise
    /// (issue #44). `None` when the profile sets neither.
    pub fn effort_label(&self) -> Option<String> {
        match self.provider {
            ProviderKind::OpenaiCompat => {
                self.reasoning_effort.as_ref().map(ReasoningEffort::label)
            }
            ProviderKind::Anthropic => self.effort.clone(),
        }
    }

    /// The param the effort goes out under, when the profile sets one
    /// (issue #44). The doctor names it when an endpoint rejects it.
    pub fn effort_param(&self) -> Option<&str> {
        self.reasoning_effort.as_ref().map(|_| {
            self.reasoning_effort_param
                .as_deref()
                .unwrap_or(REASONING_EFFORT_PARAM)
        })
    }

    /// Where requests go, for the banner. Never includes the key.
    pub fn endpoint(&self) -> String {
        match self.provider {
            ProviderKind::OpenaiCompat => self.base_url.clone().unwrap_or_default(),
            ProviderKind::Anthropic => self
                .base_url
                .clone()
                .unwrap_or_else(|| "api.anthropic.com".into()),
        }
    }

    /// Build the adapter. `api_key` is passed in so this stays testable
    /// without touching the environment.
    pub fn build_provider(&self, api_key: String) -> Box<dyn Provider> {
        match self.provider {
            ProviderKind::OpenaiCompat => {
                let mut c =
                    OpenAiCompatConfig::new(self.base_url.clone().unwrap_or_default(), &self.model)
                        .with_api_key(api_key);
                if let Some(n) = self.max_context_tokens {
                    c = c.with_max_context_tokens(n);
                }
                if let Some(effort) = &self.reasoning_effort {
                    c = c.with_reasoning_effort(
                        self.reasoning_effort_param
                            .as_deref()
                            .unwrap_or(REASONING_EFFORT_PARAM),
                        effort.clone(),
                    );
                }
                Box::new(OpenAiCompat::new(c))
            }
            ProviderKind::Anthropic => {
                let mut c = AnthropicConfig::new(api_key, &self.model);
                if let Some(u) = &self.base_url {
                    c = c.with_base_url(u);
                }
                if let Some(n) = self.max_context_tokens {
                    c = c.with_max_context_tokens(n);
                }
                if let Some(n) = self.max_output_tokens {
                    c = c.with_max_output_tokens(n);
                }
                if self.thinking.as_deref() == Some("off") {
                    c = c.with_thinking(Thinking::Off);
                }
                if let Some(e) = &self.effort {
                    c = c.with_effort(e);
                }
                if let Some(cache) = self.cache {
                    c = c.with_cache(cache);
                }
                Box::new(Anthropic::new(c))
            }
        }
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// `$XDG_CONFIG_HOME/aigentic/config.toml`, else `~/.config/aigentic/config.toml`.
pub fn default_config_path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"))
        .join("aigentic")
        .join("config.toml")
}

/// `$XDG_DATA_HOME/aigentic/threads`, else `~/.local/share/aigentic/threads`.
pub fn default_threads_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local").join("share"))
        .join("aigentic")
        .join("threads")
}

#[cfg(test)]
mod tests {
    use super::*;

    const FLAT: &str = r#"base_url = "https://api.tensorx.ai/v1"
model = "z-ai/glm-5.3"
api_key_env = "TENSORX_API_KEY"
"#;

    #[test]
    fn flat_phase0_file_becomes_a_default_profile() {
        let c = Config::parse(FLAT).unwrap();
        assert_eq!(c.default_profile, "default");
        let (name, p) = c.select(None).unwrap();
        assert_eq!(name, "default");
        assert_eq!(p.provider, ProviderKind::OpenaiCompat);
        assert_eq!(p.base_url.as_deref(), Some("https://api.tensorx.ai/v1"));
        assert_eq!(p.model, "z-ai/glm-5.3");
        assert_eq!(p.api_key_env, "TENSORX_API_KEY");
        assert!(c.select(Some("anthropic")).is_err());
    }

    /// Issue #31: a profile's price table parses field for field, and
    /// absent cache rates default to the input rate so one number per
    /// direction still prices.
    #[test]
    fn a_price_table_parses_and_defaults_the_cache_rates() {
        // The named-profile form: `[profiles]` and the phase 0 flat form
        // are exclusive, so a price table needs profiles.
        let text = r#"
default_profile = "tensorx"

[profiles.tensorx]
provider = "openai_compat"
base_url = "https://api.tensorx.ai/v1"
model = "z-ai/glm-5.3"
api_key_env = "TENSORX_API_KEY"

[profiles.tensorx.prices]
input = 0.25
output = 1.5
cache_read = 0.05
"#;
        let c = Config::parse(text).unwrap();
        let (_, p) = c.select(Some("tensorx")).unwrap();
        let prices = p.prices.as_ref().expect("a price table").prices();
        assert_eq!(prices.input, 0.25);
        assert_eq!(prices.output, 1.5);
        assert_eq!(prices.cache_read, 0.05);
        // Not given, so the input rate prices it.
        assert_eq!(prices.cache_write, 0.25);
    }

    /// Every key `config.toml`'s structs have, in one file. Amendment
    /// 2.2: this is the checklist — a struct that gains a field without
    /// a line here and in `config_spec()` fails the test below.
    const FULL_CONFIG: &str = r#"
default_profile = "tensorx"
utility_profile = "tensorx"
user = "steve"
threads_dir = "/tmp/t"
bundled_dir = "/tmp/b"

[profiles.tensorx]
provider = "openai_compat"
base_url = "https://api.tensorx.ai/v1"
model = "z-ai/glm-5.3"
api_key_env = "TENSORX_API_KEY"
max_context_tokens = 200000
reasoning_effort = 50
reasoning_effort_param = "deepseek_effort"

[profiles.tensorx.budget]
max_iterations = 5
max_tokens = 1000
max_wall_time_secs = 60
cache_read_price_ratio = 0.25

[profiles.tensorx.compaction]
trigger_fraction = 0.8
keep_turns = 2
max_result_bytes = 100
summary_max_output_tokens = 10
keep_last_calls = 3
context_ceiling_tokens = 90000
evict_above_tokens = 64000

[profiles.tensorx.prices]
input = 0.25
output = 1.5
cache_read = 0.05
cache_write = 0.3

# The anthropic-only keys, in their own profile: they are refused under
# `openai_compat` (see `Profile::validate`).
[profiles.sonnet]
provider = "anthropic"
model = "claude-opus-5"
api_key_env = "ANTHROPIC_API_KEY"
thinking = "adaptive"
effort = "high"
max_output_tokens = 4096
cache = true

[global]
instructions = "/tmp/i.md"

[tools]
denied = ["mcp.*"]

[skills]
denied = ["wizard"]

[display]
result_lines = 4
result_bytes = 800

[[mcp_servers]]
name = "docs"
transport = { stdio = { command = "npx" } }
"#;

    /// `FULL_CONFIG` is clean, and a typo among its keys is reported with
    /// the dotted path and a suggestion from the same table.
    #[test]
    fn a_full_config_is_clean_and_a_typo_is_named() {
        let (_, unknown) = Config::parse_with(FULL_CONFIG).unwrap();
        assert_eq!(unknown, Vec::<String>::new(), "{FULL_CONFIG}");

        // A positive suggestion: `efort` is one edit from `effort`, the
        // key of the table the path names, and the expected text is the
        // suggester's own output rather than a literal (issue #39).
        let expected = Config::suggest_key("profiles.a.efort").expect("a suggestion");
        assert_eq!(expected, "profiles.a.effort");
        let (_, unknown) = Config::parse_with(
            "[profiles.a]\nprovider = \"anthropic\"\nmodel = \"m\"\napi_key_env = \"K\"\nefort = \"high\"\n",
        )
        .unwrap();
        assert_eq!(unknown, vec!["profiles.a.efort"]);

        let (_, unknown) = Config::parse_with(
            "[profiles.a]\nbase_url = \"u\"\nmodel = \"m\"\napi_key_env = \"K\"\n[profiles.a.compaction]\ntrigger_fraction = 0.8\ndefault_profile = \"x\"\n",
        )
        .unwrap();
        assert_eq!(unknown, vec!["profiles.a.compaction.default_profile"]);
        assert_eq!(
            Config::suggest_key("profiles.a.compaction.default_profile"),
            None,
            "no key of that table is close enough"
        );
    }

    /// A profile without a table is `None`, not a zero price: nothing is
    /// claimed about a call whose cost is unknown (issue #31).
    #[test]
    fn a_profile_without_prices_has_none() {
        let c = Config::parse(EXAMPLE).unwrap();
        let (_, p) = c.select(Some("anthropic")).unwrap();
        assert!(p.prices.is_none());
    }

    /// Issue #44: `reasoning_effort` takes an integer or a label, and the
    /// param defaults to none (the adapter's own default applies). Both
    /// are asserted equal to what the fixture sets.
    #[test]
    fn an_openai_profile_parses_reasoning_effort_and_its_param() {
        let c = Config::parse(
            "default_profile = \"a\"\n[profiles.a]\nbase_url = \"u\"\nmodel = \"m\"\napi_key_env = \"K\"\nreasoning_effort = 50\n",
        )
        .unwrap();
        let (_, a) = c.select(Some("a")).unwrap();
        assert_eq!(a.reasoning_effort, Some(ReasoningEffort::Int(50)));
        assert_eq!(a.reasoning_effort_param, None);
        assert_eq!(a.effort_label(), Some(ReasoningEffort::Int(50).label()));

        let c = Config::parse(
            "default_profile = \"b\"\n[profiles.b]\nbase_url = \"u\"\nmodel = \"m\"\napi_key_env = \"K\"\nreasoning_effort = \"high\"\nreasoning_effort_param = \"deepseek_effort\"\n",
        )
        .unwrap();
        let (_, b) = c.select(Some("b")).unwrap();
        assert_eq!(
            b.reasoning_effort,
            Some(ReasoningEffort::Label("high".into()))
        );
        assert_eq!(b.reasoning_effort_param.as_deref(), Some("deepseek_effort"));
        assert_eq!(b.effort_label(), Some("high".to_owned()));
    }

    /// The pin `profiles_parse_select_and_build` held before issue #37:
    /// `select` by name and by default, the derived anthropic endpoint
    /// and the capabilities each provider reports.
    #[test]
    fn profiles_parse_select_and_build() {
        let c = Config::parse(EXAMPLE).unwrap();
        assert_eq!(c.default_profile, "tensorx");
        assert_eq!(c.select(None).unwrap().0, "tensorx");
        let (_, a) = c.select(Some("anthropic")).unwrap();
        assert_eq!(a.provider, ProviderKind::Anthropic);
        assert_eq!(a.base_url, None);
        assert_eq!(a.endpoint(), "api.anthropic.com");
        let provider = a.build_provider("k".into());
        assert!(provider.capabilities().supports_caching);
        assert_eq!(provider.capabilities().max_context_tokens, 1_000_000);
        let (_, t) = c.select(Some("tensorx")).unwrap();
        assert!(!t.build_provider("k".into()).capabilities().supports_caching);
    }

    /// A single profile needs no `default_profile`, and `[compaction]`
    /// keeps the defaults for the fields it leaves out. Issue #37 left
    /// the range checks below with no other home: an unknown key is now
    /// ignored, but an out-of-range value still fails.
    #[test]
    fn single_profile_needs_no_default() {
        let c = Config::parse(
            r#"[profiles.only]
provider = "anthropic"
model = "claude-opus-5"
api_key_env = "ANTHROPIC_API_KEY"
thinking = "off"
effort = "low"
cache = false

[profiles.only.compaction]
trigger_fraction = 0.5
keep_turns = 3
keep_last_calls = 20
context_ceiling_tokens = 96_000
"#,
        )
        .unwrap();
        assert_eq!(c.default_profile, "only");
        let (_, p) = c.select(None).unwrap();
        assert!(!p.build_provider("k".into()).capabilities().supports_caching);
        let s = p.compaction_settings();
        assert_eq!((s.trigger_fraction, s.keep_turns), (0.5, 3));
        assert_eq!((s.keep_last_calls, s.context_ceiling_tokens), (20, 96_000));
        assert_eq!(s.max_result_bytes, DEFAULT_COMPACTION.max_result_bytes);
        let bare = Config::parse(FLAT)
            .unwrap()
            .select(None)
            .unwrap()
            .1
            .compaction_settings();
        assert_eq!(
            (bare.keep_last_calls, bare.context_ceiling_tokens),
            (
                DEFAULT_COMPACTION.keep_last_calls,
                DEFAULT_COMPACTION.context_ceiling_tokens
            ),
            "an absent [compaction] keeps the defaults"
        );
        // `Profile::validate`'s range checks: out of range still fails,
        // even though an unknown key beside it would be ignored.
        assert!(Config::parse("[profiles.a]\nbase_url = \"u\"\nmodel = \"m\"\napi_key_env = \"K\"\n[profiles.a.compaction]\ntrigger_fraction = 2.0\n").is_err());
        assert!(Config::parse("[profiles.a]\nbase_url = \"u\"\nmodel = \"m\"\napi_key_env = \"K\"\n[profiles.a.compaction]\ncontext_ceiling_tokens = 1024\n").is_err());
        assert!(Config::parse("[profiles.a]\nbase_url = \"u\"\nmodel = \"m\"\napi_key_env = \"K\"\n[profiles.a.budget]\ncache_read_price_ratio = 1.5\n").is_err());
        let c = Config::parse("[profiles.a]\nbase_url = \"u\"\nmodel = \"m\"\napi_key_env = \"K\"\n[global]\ninstructions = \"/x/i.md\"\n[tools]\ndenied = [\"mcp.*\"]\n[skills]\ndenied = [\"wizard\"]\n").unwrap();
        assert_eq!(
            c.global_instructions.as_deref(),
            Some(std::path::Path::new("/x/i.md"))
        );
        assert_eq!(c.denied_tools, vec!["mcp.*"]);
        assert_eq!(c.denied_skills, vec!["wizard"]);
        // An unknown key in a section is ignored, not refused (issue
        // #37); the `[profiles.a]` prefix keeps the flat form out of it.
        let c = Config::parse("[profiles.a]\nbase_url = \"u\"\nmodel = \"m\"\napi_key_env = \"K\"\n[profiles.a.budget]\nmax_tokens = 5\ncache_read_price_ratio = 0.19\n").unwrap();
        let b = c.profiles["a"].budget();
        assert_eq!(
            (b.max_iterations, b.max_tokens),
            (DEFAULT_BUDGET.max_iterations, 5)
        );
        assert_eq!(b.max_wall_time, DEFAULT_BUDGET.max_wall_time);
        assert_eq!(b.cache_read_price_ratio, 0.19);
        assert_eq!(DEFAULT_BUDGET.cache_read_price_ratio, 0.25);
    }

    #[test]
    fn display_caps_default_override_and_reject_the_unknown() {
        let c = Config::parse(FLAT).unwrap();
        assert_eq!(
            (c.display.result_lines, c.display.result_bytes),
            (3, 600),
            "an absent [display] keeps the defaults"
        );
        let c = Config::parse(
            "[profiles.a]\nbase_url = \"u\"\nmodel = \"m\"\napi_key_env = \"K\"\n\
             [display]\nresult_lines = 10\nresult_bytes = 2000\n",
        )
        .unwrap();
        assert_eq!((c.display.result_lines, c.display.result_bytes), (10, 2000));
        // `FULL_CONFIG` sets `[display]` too, but it is the checklist for
        // the walk's silence; these asserts are what says the values
        // themselves parse.
        let (c, unknown) = Config::parse_with(FULL_CONFIG).unwrap();
        assert_eq!(unknown, Vec::<String>::new(), "{FULL_CONFIG}");
        assert_eq!((c.display.result_lines, c.display.result_bytes), (4, 800));
    }

    #[test]
    fn bad_files_are_rejected_without_echoing_values() {
        let cases = [
            format!("{EXAMPLE}\napi_key = \"sk-oops\"\n"),
            format!("{FLAT}\n[profiles.x]\nmodel = \"m\"\napi_key_env = \"K\"\nbase_url = \"u\"\n"),
            "default_profile = \"nope\"\n[profiles.a]\nmodel = \"m\"\napi_key_env = \"K\"\nbase_url = \"u\"\n".into(),
            "[profiles.a]\nmodel = \"m\"\napi_key_env = \"K\"\n".into(), // openai without base_url
            "[profiles.a]\nprovider = \"anthropic\"\nmodel = \"m\"\napi_key_env = \"K\"\nthinking = \"lots\"\n".into(),
            "[profiles.a]\nbase_url = \"u\"\nmodel = \"m\"\napi_key_env = \"K\"\neffort = \"high\"\n".into(),
            // Issue #44, the anthropic mirror: the openai-only keys are
            // refused there, by name.
            "[profiles.a]\nprovider = \"anthropic\"\nmodel = \"m\"\napi_key_env = \"K\"\nreasoning_effort = 50\n".into(),
            "[profiles.a]\nprovider = \"anthropic\"\nmodel = \"m\"\napi_key_env = \"K\"\nreasoning_effort_param = \"reasoning_effort\"\n".into(),
            // A param without an effort, and an empty segment in one.
            "[profiles.a]\nbase_url = \"u\"\nmodel = \"m\"\napi_key_env = \"K\"\nreasoning_effort_param = \"reasoning_effort\"\n".into(),
            "[profiles.a]\nbase_url = \"u\"\nmodel = \"m\"\napi_key_env = \"K\"\nreasoning_effort = 50\nreasoning_effort_param = \"thinking..effort\"\n".into(),
            "[profiles.a]\nprovider = \"gemini\"\nmodel = \"m\"\napi_key_env = \"K\"\n".into(),
            "model = \"m\"\n".into(),
        ];
        for text in cases {
            let err = Config::parse(&text).unwrap_err().to_string();
            assert!(!err.contains("sk-oops"), "value echoed: {err}");
        }
    }

    /// Amendment 2.1: a literal `api_key` is a hard error wherever it
    /// appears — root or inside a profile — and the message names the
    /// dotted path, never the value. This test existed before issue #37
    /// to keep a pasted secret out of a file; it keeps that purpose.
    #[test]
    fn a_literal_api_key_is_refused_by_path_and_never_echoed() {
        for (text, expected) in [
            (format!("{EXAMPLE}\napi_key = \"sk-oops\"\n"), "profiles.anthropic.api_key"),
            (
                "[profiles.a]\nbase_url = \"u\"\nmodel = \"m\"\napi_key_env = \"K\"\napi_key = \"sk-oops\"\n".to_owned(),
                "profiles.a.api_key",
            ),
        ] {
            let err = Config::parse(&text).unwrap_err().to_string();
            assert!(err.contains(expected), "{expected} not in {err}");
            assert!(err.contains("api_key_env"), "{err}");
            assert!(!err.contains("sk-oops"), "value echoed: {err}");
            assert!(
                Config::parse_with(&text).is_err(),
                "the collect path refuses it too"
            );
        }
    }

    #[test]
    fn api_key_comes_only_from_the_named_variable() {
        let c = Config::parse(FLAT).unwrap();
        let mut p = c.select(None).unwrap().1.clone();
        p.api_key_env = "AIGENTIC_TEST_KEY_THAT_IS_UNSET".into();
        let err = p.api_key().unwrap_err().to_string();
        assert!(err.contains("AIGENTIC_TEST_KEY_THAT_IS_UNSET"));
    }
}

/// `~/.config/aigentic/server.toml`: the daemon's own file.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// `"unix"` (the derived socket path), `"unix:/path"`, or
    /// `"tcp:host:port"` (step 8).
    #[serde(default = "default_listen")]
    pub listen: String,
    /// An idle thread with no open sessions is unloaded after this.
    #[serde(default = "default_idle_unload_secs")]
    pub idle_unload_secs: u64,
    /// The first user is the daemon's owner.
    #[serde(default)]
    pub users: Vec<UserConfig>,
    #[serde(default)]
    pub projects: Vec<ProjectConfig>,
}

fn default_listen() -> String {
    "unix".into()
}

fn default_idle_unload_secs() -> u64 {
    600
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserConfig {
    pub name: String,
    /// The environment variable, in the daemon's environment, holding
    /// this user's token. Never the token itself.
    #[serde(default)]
    pub token_env: Option<String>,
    /// An in-memory token for the embedded single-user daemon; never
    /// read from a file.
    #[serde(skip)]
    pub token: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectConfig {
    /// Must equal `[project] name` in the checkout's `aigentic.toml`
    /// when one exists; a root without one is a bare working directory.
    pub name: String,
    pub root: PathBuf,
}

impl ServerConfig {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| ConfigError::msg(format!("reading {}: {e}", path.display())))?;
        Self::parse(&text).map_err(|e| ConfigError::msg(format!("parsing {}: {e}", path.display())))
    }

    /// Parse; the error carries only the message and position, never a
    /// value from the file.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(text).map_err(|e| {
            let at = e
                .span()
                .map(|s| format!(" at byte {}", s.start))
                .unwrap_or_default();
            ConfigError::msg(format!("{}{at}", e.message()))
        })?;
        let mut names = std::collections::HashSet::new();
        for u in &config.users {
            if u.name.trim().is_empty() {
                return Err(ConfigError::msg("a user needs a name"));
            }
            if !names.insert(u.name.as_str()) {
                return Err(ConfigError::msg(format!(
                    "user {:?} is listed twice",
                    u.name
                )));
            }
            if u.token_env.is_none() && u.token.is_none() {
                return Err(ConfigError::msg(format!(
                    "user {:?} needs token_env, the variable holding its token",
                    u.name
                )));
            }
        }
        let mut projects = std::collections::HashSet::new();
        for p in &config.projects {
            if !projects.insert(p.name.as_str()) {
                return Err(ConfigError::msg(format!(
                    "project {:?} is listed twice",
                    p.name
                )));
            }
        }
        Ok(config)
    }

    /// The first user: `admin` of every project whose file names nobody.
    pub fn owner(&self) -> Option<&str> {
        self.users.first().map(|u| u.name.as_str())
    }

    pub fn project(&self, name: &str) -> Option<&ProjectConfig> {
        self.projects.iter().find(|p| p.name == name)
    }

    pub fn idle_unload(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.idle_unload_secs)
    }
}

/// `$XDG_CONFIG_HOME/aigentic/server.toml`, else `~/.config/aigentic/server.toml`.
pub fn default_server_config_path() -> PathBuf {
    default_config_path().with_file_name("server.toml")
}

/// `$XDG_RUNTIME_DIR/aigentic.sock`, else `~/.local/share/aigentic/aigentic.sock`.
pub fn default_socket_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("XDG_DATA_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home().join(".local").join("share"))
                .join("aigentic")
        })
        .join("aigentic.sock")
}

#[cfg(test)]
mod server_tests {
    use super::*;

    #[test]
    fn server_config_parses_and_checks_users_and_projects() {
        let c = ServerConfig::parse(
            "listen = \"unix\"\n[[users]]\nname = \"steve\"\ntoken_env = \"T_STEVE\"\n[[users]]\nname = \"magnus\"\ntoken_env = \"T_MAGNUS\"\n[[projects]]\nname = \"p\"\nroot = \"/srv/p\"\n",
        )
        .unwrap();
        assert_eq!(c.owner(), Some("steve"));
        assert_eq!(c.idle_unload_secs, 600);
        assert_eq!(c.project("p").unwrap().root, PathBuf::from("/srv/p"));
        assert!(c.project("q").is_none());
        for bad in [
            "[[users]]\nname = \"a\"\n",
            "[[users]]\nname = \"a\"\ntoken_env = \"T\"\n[[users]]\nname = \"a\"\ntoken_env = \"U\"\n",
            "[[projects]]\nname = \"p\"\nroot = \"/a\"\n[[projects]]\nname = \"p\"\nroot = \"/b\"\n",
            "[[users]]\nname = \"a\"\ntoken = \"never-in-a-file\"\n",
            "nope = 1\n",
        ] {
            let err = ServerConfig::parse(bad).unwrap_err().to_string();
            assert!(!err.contains("never-in-a-file"), "{err}");
        }
        assert_eq!(ServerConfig::parse("").unwrap().users.len(), 0);
    }
}
