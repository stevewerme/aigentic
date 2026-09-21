use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use aigentic_runtime::aigentic_core::Provider;
use aigentic_runtime::aigentic_providers::{
    Anthropic, AnthropicConfig, OpenAiCompat, OpenAiCompatConfig, Thinking,
};
use aigentic_runtime::{CompactionSettings, DEFAULT_COMPACTION};
use anyhow::{Context, anyhow, bail};
use serde::Deserialize;

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
}

/// `[profiles.<name>.compaction]`. Missing fields take the runtime defaults.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionConfig {
    #[serde(default)]
    pub trigger_fraction: Option<f32>,
    #[serde(default)]
    pub keep_turns: Option<usize>,
    #[serde(default)]
    pub max_result_bytes: Option<usize>,
    #[serde(default)]
    pub summary_max_output_tokens: Option<u64>,
}

impl CompactionConfig {
    pub fn settings(&self) -> CompactionSettings {
        CompactionSettings {
            trigger_fraction: self
                .trigger_fraction
                .unwrap_or(DEFAULT_COMPACTION.trigger_fraction),
            keep_turns: self.keep_turns.unwrap_or(DEFAULT_COMPACTION.keep_turns),
            max_result_bytes: self
                .max_result_bytes
                .unwrap_or(DEFAULT_COMPACTION.max_result_bytes),
            summary_max_output_tokens: self
                .summary_max_output_tokens
                .unwrap_or(DEFAULT_COMPACTION.summary_max_output_tokens),
        }
    }
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
    profiles: BTreeMap<String, Profile>,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    threads_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub profiles: BTreeMap<String, Profile>,
    pub default_profile: String,
    /// Author id for your messages; defaults to `$USER`.
    pub user: Option<String>,
    /// Where thread logs live; defaults to `~/.local/share/aigentic/threads`.
    pub threads_dir: Option<PathBuf>,
}

const EXAMPLE: &str = r#"default_profile = "tensorx"

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
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| {
            format!(
                "reading config {}\n\nCreate it with, for example:\n\n{EXAMPLE}",
                path.display()
            )
        })?;
        Self::parse(&text).with_context(|| format!("parsing {}", path.display()))
    }

    /// Parse, reporting only the message and position on failure. The
    /// parser's default error quotes the offending line, which could echo a
    /// secret someone pasted into the file.
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let file: ConfigFile = toml::from_str(text).map_err(|e| {
            let at = e
                .span()
                .map(|s| format!(" at byte {}", s.start))
                .unwrap_or_default();
            anyhow!("{}{at}", e.message())
        })?;

        let flat = file.base_url.is_some() || file.model.is_some() || file.api_key_env.is_some();
        let (profiles, default_profile) = match (flat, file.profiles.is_empty()) {
            (true, false) => {
                bail!("use either top-level base_url/model/api_key_env or [profiles.*], not both")
            }
            (true, true) => {
                let profile = Profile {
                    provider: ProviderKind::OpenaiCompat,
                    base_url: Some(file.base_url.ok_or_else(|| anyhow!("missing base_url"))?),
                    model: file.model.ok_or_else(|| anyhow!("missing model"))?,
                    api_key_env: file
                        .api_key_env
                        .ok_or_else(|| anyhow!("missing api_key_env"))?,
                    max_context_tokens: file.max_context_tokens,
                    thinking: None,
                    effort: None,
                    max_output_tokens: None,
                    cache: None,
                    compaction: None,
                };
                (
                    BTreeMap::from([("default".to_owned(), profile)]),
                    "default".to_owned(),
                )
            }
            (false, true) => bail!("no profiles configured"),
            (false, false) => {
                let default = match file.default_profile {
                    Some(name) => name,
                    None if file.profiles.len() == 1 => {
                        file.profiles.keys().next().unwrap().clone()
                    }
                    None => {
                        bail!("default_profile is required when more than one profile is defined")
                    }
                };
                if !file.profiles.contains_key(&default) {
                    bail!("default_profile {default:?} is not a defined profile");
                }
                (file.profiles, default)
            }
        };
        for (name, p) in &profiles {
            p.validate().with_context(|| format!("profile {name:?}"))?;
        }
        Ok(Self {
            profiles,
            default_profile,
            user: file.user,
            threads_dir: file.threads_dir,
        })
    }

    /// The named profile, or the default.
    pub fn select(&self, name: Option<&str>) -> anyhow::Result<(&str, &Profile)> {
        let name = name.unwrap_or(&self.default_profile);
        let (name, profile) = self.profiles.get_key_value(name).ok_or_else(|| {
            anyhow!(
                "no profile {name:?}; defined: {}",
                self.profiles.keys().cloned().collect::<Vec<_>>().join(", ")
            )
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
    fn validate(&self) -> anyhow::Result<()> {
        if let Some(c) = &self.compaction
            && let Some(f) = c.trigger_fraction
            && !(0.05..=0.95).contains(&f)
        {
            bail!("compaction.trigger_fraction must be between 0.05 and 0.95, got {f}");
        }
        match self.provider {
            ProviderKind::OpenaiCompat => {
                if self.base_url.is_none() {
                    bail!("base_url is required for provider = \"openai_compat\"");
                }
                for (field, set) in [
                    ("thinking", self.thinking.is_some()),
                    ("effort", self.effort.is_some()),
                    ("max_output_tokens", self.max_output_tokens.is_some()),
                    ("cache", self.cache.is_some()),
                ] {
                    if set {
                        bail!("{field} only applies to provider = \"anthropic\"");
                    }
                }
            }
            ProviderKind::Anthropic => {
                if let Some(t) = &self.thinking
                    && t != "adaptive"
                    && t != "off"
                {
                    bail!("thinking must be \"adaptive\" or \"off\", got {t:?}");
                }
            }
        }
        Ok(())
    }

    /// The key, read only from the named environment variable.
    pub fn api_key(&self) -> anyhow::Result<String> {
        std::env::var(&self.api_key_env).map_err(|_| {
            anyhow!(
                "environment variable {} is not set (named by api_key_env in the config; \
                 put it in .env or export it)",
                self.api_key_env
            )
        })
    }

    pub fn compaction_settings(&self) -> CompactionSettings {
        self.compaction
            .as_ref()
            .map_or(DEFAULT_COMPACTION, CompactionConfig::settings)
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
"#,
        )
        .unwrap();
        assert_eq!(c.default_profile, "only");
        let (_, p) = c.select(None).unwrap();
        assert!(!p.build_provider("k".into()).capabilities().supports_caching);
        let s = p.compaction_settings();
        assert_eq!((s.trigger_fraction, s.keep_turns), (0.5, 3));
        assert_eq!(s.max_result_bytes, DEFAULT_COMPACTION.max_result_bytes);
        assert_eq!(
            Config::parse(FLAT)
                .unwrap()
                .select(None)
                .unwrap()
                .1
                .compaction_settings(),
            DEFAULT_COMPACTION
        );
        assert!(Config::parse("[profiles.a]\nbase_url = \"u\"\nmodel = \"m\"\napi_key_env = \"K\"\n[profiles.a.compaction]\ntrigger_fraction = 2.0\n").is_err());
        assert!(Config::parse("[profiles.a]\nbase_url = \"u\"\nmodel = \"m\"\napi_key_env = \"K\"\n[profiles.a.compaction]\nnope = 1\n").is_err());
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
