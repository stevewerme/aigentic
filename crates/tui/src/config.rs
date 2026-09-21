use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow};
use serde::Deserialize;

/// `config.toml`. The API key is deliberately absent: only its environment
/// variable's name is configured here, and unknown fields (such as a stray
/// `api_key`) are rejected.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// e.g. `https://api.tensorx.ai/v1` or `http://127.0.0.1:8080/v1`.
    pub base_url: String,
    pub model: String,
    /// Name of the environment variable holding the API key.
    pub api_key_env: String,
    #[serde(default = "default_max_context")]
    pub max_context_tokens: u64,
    /// Author id for your messages; defaults to `$USER`.
    #[serde(default)]
    pub user: Option<String>,
    /// Where thread logs live; defaults to `~/.local/share/aigentic/threads`.
    #[serde(default)]
    pub threads_dir: Option<PathBuf>,
}

fn default_max_context() -> u64 {
    32_768
}

const EXAMPLE: &str = r#"base_url = "https://api.tensorx.ai/v1"
model = "glm-5.3"
api_key_env = "TENSORX_API_KEY"
# max_context_tokens = 131072
# user = "steve"
# threads_dir = "/path/to/threads"
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
        toml::from_str(text).map_err(|e| {
            let at = e
                .span()
                .map(|s| format!(" at byte {}", s.start))
                .unwrap_or_default();
            anyhow!("{}{at}", e.message())
        })
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

    pub fn user_name(&self) -> String {
        self.user
            .clone()
            .or_else(|| std::env::var("USER").ok())
            .unwrap_or_else(|| "user".into())
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

    #[test]
    fn parses_the_example_and_applies_defaults() {
        let c = Config::parse(EXAMPLE).unwrap();
        assert_eq!(c.base_url, "https://api.tensorx.ai/v1");
        assert_eq!(c.model, "glm-5.3");
        assert_eq!(c.api_key_env, "TENSORX_API_KEY");
        assert_eq!(c.max_context_tokens, 32_768);
        assert_eq!(c.user, None);
        assert_eq!(c.threads_dir, None);
    }

    #[test]
    fn a_key_in_the_config_is_rejected() {
        let text = format!("{EXAMPLE}\napi_key = \"sk-oops\"\n");
        let err = Config::parse(&text).unwrap_err().to_string();
        assert!(err.contains("api_key"), "{err}");
        assert!(
            !err.contains("sk-oops"),
            "the value must never be echoed: {err}"
        );
    }

    #[test]
    fn missing_fields_are_reported() {
        assert!(Config::parse("model = \"m\"\n").is_err());
    }

    #[test]
    fn api_key_comes_only_from_the_named_variable() {
        let mut c = Config::parse(EXAMPLE).unwrap();
        c.api_key_env = "AIGENTIC_TEST_KEY_THAT_IS_UNSET".into();
        let err = c.api_key().unwrap_err().to_string();
        assert!(err.contains("AIGENTIC_TEST_KEY_THAT_IS_UNSET"));
    }
}
