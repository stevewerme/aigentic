//! The daemon's configuration (phase 5): `config.toml` moved here from
//! the terminal binary, since the daemon builds every thread's provider,
//! layers and skills; and `server.toml`, the daemon's own file: where it
//! listens, its users and their token variables, its projects.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use aigentic_runtime::aigentic_core::{Budget, Provider};
use aigentic_runtime::aigentic_providers::{
    Anthropic, AnthropicConfig, OpenAiCompat, OpenAiCompatConfig, Thinking,
};
use aigentic_runtime::project::{BudgetConfig, CompactionConfig};
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
#[serde(deny_unknown_fields)]
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
    /// `anthropic` only: emit cache breakpoints (default true).
    #[serde(default)]
    pub cache: Option<bool>,
    /// When and how to compact; every field optional.
    #[serde(default)]
    pub compaction: Option<CompactionConfig>,
    /// Per-turn budget; every field optional.
    #[serde(default)]
    pub budget: Option<BudgetConfig>,
}

/// The file on disk. Either the phase 0 flat form (top-level `base_url`,
/// `model`, `api_key_env`) or named `[profiles.<name>]` tables; not both.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct GlobalSection {
    /// Defaults to `instructions.md` beside the config file.
    #[serde(default)]
    pub instructions: Option<PathBuf>,
}

/// `[tools] denied` and `[skills] denied`: what no project may offer.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeniedSection {
    #[serde(default)]
    pub denied: Vec<String>,
}

/// `[display]`: how much of a tool result the terminal shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
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
}

pub const EXAMPLE: &str = r#"default_profile = "tensorx"

[profiles.tensorx]
provider = "openai_compat"
base_url = "https://api.tensorx.ai/v1"
model = "z-ai/glm-5.3"
api_key_env = "TENSORX_API_KEY"

[profiles.anthropic]
provider = "anthropic"
model = "claude-opus-5"
api_key_env = "ANTHROPIC_API_KEY"
# effort = "high"
# thinking = "adaptive"
"#;

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            ConfigError::msg(format!(
                "reading config {}: {e}\n\nCreate it with, for example:\n\n{EXAMPLE}",
                path.display()
            ))
        })?;
        Self::parse(&text).map_err(|e| ConfigError::msg(format!("parsing {}: {e}", path.display())))
    }

    /// Parse, reporting only the message and position on failure. The
    /// parser's default error quotes the offending line, which could echo a
    /// secret someone pasted into the file.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
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
                    cache: None,
                    compaction: None,
                    budget: None,
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
        Ok(Self {
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
        })
    }

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
            }
            ProviderKind::Anthropic => {
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
        assert!(Config::parse("[profiles.a]\nbase_url = \"u\"\nmodel = \"m\"\napi_key_env = \"K\"\n[profiles.a.compaction]\ntrigger_fraction = 2.0\n").is_err());
        assert!(Config::parse("[profiles.a]\nbase_url = \"u\"\nmodel = \"m\"\napi_key_env = \"K\"\n[profiles.a.compaction]\nnope = 1\n").is_err());
        assert!(Config::parse("[profiles.a]\nbase_url = \"u\"\nmodel = \"m\"\napi_key_env = \"K\"\n[profiles.a.compaction]\ncontext_ceiling_tokens = 1024\n").is_err());
        assert!(Config::parse("[profiles.a]\nbase_url = \"u\"\nmodel = \"m\"\napi_key_env = \"K\"\n[profiles.a.budget]\nnope = 1\n").is_err());
        let c = Config::parse("[profiles.a]\nbase_url = \"u\"\nmodel = \"m\"\napi_key_env = \"K\"\n[global]\ninstructions = \"/x/i.md\"\n[tools]\ndenied = [\"mcp.*\"]\n[skills]\ndenied = [\"wizard\"]\n").unwrap();
        assert_eq!(
            c.global_instructions.as_deref(),
            Some(std::path::Path::new("/x/i.md"))
        );
        assert_eq!(c.denied_tools, vec!["mcp.*"]);
        assert_eq!(c.denied_skills, vec!["wizard"]);
        assert!(Config::parse("[profiles.a]\nbase_url = \"u\"\nmodel = \"m\"\napi_key_env = \"K\"\n[tools]\nallow = []\n").is_err());
        let c = Config::parse("[profiles.a]\nbase_url = \"u\"\nmodel = \"m\"\napi_key_env = \"K\"\n[profiles.a.budget]\nmax_tokens = 5\n").unwrap();
        let b = c.profiles["a"].budget();
        assert_eq!(
            (b.max_iterations, b.max_tokens),
            (DEFAULT_BUDGET.max_iterations, 5)
        );
        assert_eq!(b.max_wall_time, DEFAULT_BUDGET.max_wall_time);
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
        assert!(Config::parse(
            "[profiles.a]\nbase_url = \"u\"\nmodel = \"m\"\napi_key_env = \"K\"\n[display]\nnope = 1\n"
        )
        .is_err());
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
            "[profiles.a]\nprovider = \"gemini\"\nmodel = \"m\"\napi_key_env = \"K\"\n".into(),
            "model = \"m\"\n".into(),
        ];
        for text in cases {
            let err = Config::parse(&text).unwrap_err().to_string();
            assert!(!err.contains("sk-oops"), "value echoed: {err}");
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
