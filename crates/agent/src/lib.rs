//! Shared AI analyst runtime, three-way branch diff/merge planner, context
//! compressor, streaming OpenAI-compatible LLM client, deterministic replay
//! bundler, and SpreadsheetBench evaluation harness.

pub mod context;
pub mod diff;
pub mod eval;
pub mod llm;
pub mod replay;
pub mod runner;
pub mod tools;

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use thiserror::Error;

/// Default maximum reasoning/tool turns per agent run.
pub const DEFAULT_MAX_STEPS: u32 = 25;
/// Default maximum cumulative tokens (prompt + completion) per agent run.
pub const DEFAULT_MAX_TOKENS_PER_RUN: u64 = 120_000;
/// Default online wall-clock timeout per agent run.
pub const DEFAULT_RUN_TIMEOUT_SECS: u64 = 180;

/// Validated LLM provider configuration (never prints or logs `api_key`).
#[derive(Clone)]
pub struct LlmConfig {
    pub enabled: bool,
    pub provider: String,
    pub base_url: String,
    pub model: String,
    api_key: String,
    pub reasoning_effort: Option<String>,
    pub request_timeout: Duration,
}

impl std::fmt::Debug for LlmConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmConfig")
            .field("enabled", &self.enabled)
            .field("provider", &self.provider)
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field(
                "api_key",
                &if self.api_key.is_empty() {
                    "<empty>"
                } else {
                    "<redacted>"
                },
            )
            .field("reasoning_effort", &self.reasoning_effort)
            .field("request_timeout", &self.request_timeout)
            .finish()
    }
}

/// Environment variable keys consumed by [`LlmConfig::from_env_and_dotenv`].
pub const LLM_ENV_KEYS: [&str; 7] = [
    "RETICULA_LLM_ENABLED",
    "RETICULA_LLM_PROVIDER",
    "RETICULA_LLM_BASE_URL",
    "RETICULA_LLM_MODEL",
    "RETICULA_LLM_API_KEY",
    "RETICULA_LLM_REASONING_EFFORT",
    "RETICULA_LLM_TIMEOUT_SECS",
];

/// Loads `keys` from an optional dotenv file and overlays non-blank values from
/// `env_lookup`. Blank process env values (such as Compose `${VAR:-}` defaults)
/// do not clobber non-empty entries from `.env.local`.
pub fn load_env_map<F>(
    dotenv_path: Option<&Path>,
    keys: &[&str],
    mut env_lookup: F,
) -> BTreeMap<String, String>
where
    F: FnMut(&str) -> Option<String>,
{
    let mut map = BTreeMap::new();
    if let Some(path) = dotenv_path
        && let Ok(text) = std::fs::read_to_string(path)
    {
        for (k, v) in parse_dotenv(&text) {
            map.insert(k, v);
        }
    }
    for &key in keys {
        if let Some(val) = env_lookup(key)
            && !val.trim().is_empty()
        {
            map.insert(key.to_owned(), val);
        }
    }
    map
}

impl LlmConfig {
    /// Constructs a validated configuration for an OpenAI-compatible endpoint.
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        Self {
            enabled: true,
            provider: "openai-compat".to_owned(),
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            model: model.into(),
            api_key: api_key.into(),
            reasoning_effort: None,
            request_timeout: Duration::from_secs(90),
        }
    }

    /// Returns the bearer secret strictly for HTTP Authorization headers.
    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    /// Returns whether live LLM calls are enabled and credentials are present.
    pub fn is_live_ready(&self) -> bool {
        self.enabled
            && !self.base_url.trim().is_empty()
            && !self.model.trim().is_empty()
            && !self.api_key.trim().is_empty()
    }

    /// Loads LLM configuration from process environment overlaid over an optional
    /// local `.env.local` / `.env` file. Follows fail-closed validation: never
    /// echoes secret values in errors.
    pub fn from_env_and_dotenv(dotenv_path: Option<&Path>) -> Result<Option<Self>, AgentError> {
        let map = load_env_map(dotenv_path, &LLM_ENV_KEYS, |k| std::env::var(k).ok());
        Self::from_map(&map)
    }

    /// Parses and validates LLM configuration from an explicit key-value map.
    /// Blank optional values fall back cleanly to defaults so Compose `${VAR:-}`
    /// pass-through entries work without overriding defaults.
    pub fn from_map(map: &BTreeMap<String, String>) -> Result<Option<Self>, AgentError> {
        let enabled_raw = map
            .get("RETICULA_LLM_ENABLED")
            .map(String::as_str)
            .unwrap_or("")
            .trim();
        let api_key = map
            .get("RETICULA_LLM_API_KEY")
            .cloned()
            .unwrap_or_default()
            .trim()
            .to_owned();
        let enabled = match enabled_raw.to_ascii_lowercase().as_str() {
            "true" | "1" => true,
            "false" | "0" => false,
            "" => !api_key.is_empty(),
            _ => {
                return Err(AgentError::Config(
                    "RETICULA_LLM_ENABLED must be one of: true | false | 1 | 0".to_owned(),
                ));
            }
        };
        if !enabled {
            return Ok(None);
        }

        let base_url = map
            .get("RETICULA_LLM_BASE_URL")
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
            .unwrap_or("https://api.openai.com/v1")
            .trim_end_matches('/')
            .to_owned();
        let model = map
            .get("RETICULA_LLM_MODEL")
            .cloned()
            .unwrap_or_default()
            .trim()
            .to_owned();
        let provider = map
            .get("RETICULA_LLM_PROVIDER")
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
            .unwrap_or("openai-compat")
            .to_owned();
        let reasoning_effort = map
            .get("RETICULA_LLM_REASONING_EFFORT")
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty());

        let mut missing = Vec::new();
        let timeout_secs: u64 = match map
            .get("RETICULA_LLM_TIMEOUT_SECS")
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
        {
            None => 90,
            Some(raw) => match raw.parse::<u64>() {
                Ok(v) if v > 0 => v,
                _ => {
                    missing.push("RETICULA_LLM_TIMEOUT_SECS (must be a positive integer)");
                    90
                }
            },
        };
        if !(base_url.starts_with("https://") || base_url.starts_with("http://")) {
            missing.push("RETICULA_LLM_BASE_URL (must be http:// or https://)");
        }
        if model.is_empty() {
            missing.push("RETICULA_LLM_MODEL (must be non-empty when RETICULA_LLM_ENABLED=true)");
        }
        if api_key.is_empty() {
            missing.push("RETICULA_LLM_API_KEY (must be non-empty when RETICULA_LLM_ENABLED=true)");
        }
        if !missing.is_empty() {
            return Err(AgentError::Config(format!(
                "invalid LLM configuration: {}",
                missing.join("; ")
            )));
        }

        Ok(Some(Self {
            enabled: true,
            provider,
            base_url,
            model,
            api_key,
            reasoning_effort,
            request_timeout: Duration::from_secs(timeout_secs.clamp(5, 600)),
        }))
    }
}

/// Parses KEY=VALUE lines from a local dotenv file, handling UTF-8 BOM, optional
/// `export ` prefixes, quoted values, and inline comments.
pub fn parse_dotenv(text: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    for (idx, raw_line) in text.lines().enumerate() {
        let mut line = raw_line;
        if idx == 0 {
            line = line.strip_prefix('\u{feff}').unwrap_or(line);
        }
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line
            .strip_prefix("export")
            .filter(|rest| rest.starts_with(|c: char| c.is_ascii_whitespace()))
            .map(str::trim_start)
            .unwrap_or(line);
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let key = k.trim();
        if key.is_empty() {
            continue;
        }
        let trimmed = v.trim();
        let parsed = if trimmed.len() >= 2
            && ((trimmed.starts_with('"') && trimmed.ends_with('"'))
                || (trimmed.starts_with('\'') && trimmed.ends_with('\'')))
        {
            &trimmed[1..trimmed.len() - 1]
        } else if trimmed.starts_with('"') || trimmed.starts_with('\'') {
            let quote = trimmed.as_bytes()[0] as char;
            if let Some(end) = trimmed[1..].find(quote) {
                let after = trimmed[end + 2..].trim_start();
                if after.is_empty() || after.starts_with('#') {
                    &trimmed[1..1 + end]
                } else {
                    trimmed
                }
            } else {
                trimmed
            }
        } else {
            strip_unquoted_inline_comment(v)
        };
        pairs.push((key.to_owned(), parsed.to_owned()));
    }
    pairs
}

fn strip_unquoted_inline_comment(raw_after_eq: &str) -> &str {
    let bytes = raw_after_eq.as_bytes();
    for i in 1..bytes.len() {
        if bytes[i] == b'#' && bytes[i - 1].is_ascii_whitespace() {
            return raw_after_eq[..i].trim();
        }
    }
    raw_after_eq.trim()
}

/// Shared execution guardrails for an agent run.
#[derive(Clone, Copy, Debug)]
pub struct AgentRunConfig {
    pub max_steps: u32,
    pub max_tokens: u64,
    pub timeout: Duration,
    pub max_identical_tool_repeats: u32,
}

impl Default for AgentRunConfig {
    fn default() -> Self {
        Self {
            max_steps: DEFAULT_MAX_STEPS,
            max_tokens: DEFAULT_MAX_TOKENS_PER_RUN,
            timeout: Duration::from_secs(DEFAULT_RUN_TIMEOUT_SECS),
            max_identical_tool_repeats: 4,
        }
    }
}

/// Top-level error returned by `reticula-agent` operations.
#[derive(Debug, Error)]
pub enum AgentError {
    #[error("configuration error: {0}")]
    Config(String),
    #[error("LLM provider error: {0}")]
    Llm(String),
    #[error("workbook operation failed: {0}")]
    Workbook(String),
    #[error("step store error: {0}")]
    Store(String),
    #[error("agent run cancelled")]
    Cancelled,
    #[error("agent run timed out after {0}s")]
    TimedOut(u64),
    #[error("agent run exceeded budget: {0}")]
    BudgetExceeded(String),
    #[error("merge planning error: {0}")]
    Merge(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_env_file(tag: &str, content: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("reticula-env-test-{}-{tag}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(".env.local");
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn llm_config_falls_back_to_default_base_url_and_provider_when_blank_in_dotenv() {
        let env_path = temp_env_file(
            "blank-defaults",
            "RETICULA_LLM_ENABLED=true\nRETICULA_LLM_BASE_URL=\nRETICULA_LLM_PROVIDER=\nRETICULA_LLM_REASONING_EFFORT=\nRETICULA_LLM_TIMEOUT_SECS=\nRETICULA_LLM_MODEL=test-model\nRETICULA_LLM_API_KEY=sk-test-secret-never-echo\n",
        );
        let map = load_env_map(Some(&env_path), &LLM_ENV_KEYS, |_| None);
        let cfg = LlmConfig::from_map(&map).unwrap().expect("enabled config");
        assert_eq!(cfg.base_url, "https://api.openai.com/v1");
        assert_eq!(cfg.provider, "openai-compat");
        assert_eq!(cfg.model, "test-model");
        assert_eq!(cfg.reasoning_effort, None);
        assert_eq!(cfg.request_timeout, Duration::from_secs(90));
        assert!(!format!("{cfg:?}").contains("sk-test-secret-never-echo"));
        let _ = std::fs::remove_dir_all(env_path.parent().unwrap());
    }

    #[test]
    fn parse_dotenv_handles_utf8_bom_export_prefix_quotes_and_inline_comments() {
        let content = "\u{feff}export RETICULA_LLM_ENABLED=true # enable\n\
            RETICULA_LLM_MODEL=\"gpt-test #v2\" # quoted hash kept\n\
            RETICULA_LLM_API_KEY=sk-abc#123\n\
            RETICULA_LLM_TIMEOUT_SECS=120 # two minutes\n\
            RETICULA_LLM_REASONING_EFFORT= # blank after comment\n";
        let map: BTreeMap<String, String> = parse_dotenv(content).into_iter().collect();
        let cfg = LlmConfig::from_map(&map).unwrap().expect("enabled config");
        assert_eq!(cfg.model, "gpt-test #v2");
        assert_eq!(cfg.api_key(), "sk-abc#123");
        assert_eq!(cfg.request_timeout, Duration::from_secs(120));
        assert_eq!(cfg.reasoning_effort, None);
    }

    #[test]
    fn blank_process_env_does_not_clobber_dotenv_while_non_blank_overrides() {
        let env_path = temp_env_file(
            "overlay",
            "RETICULA_LLM_ENABLED=true\nRETICULA_LLM_MODEL=dotenv-model\nRETICULA_LLM_API_KEY=sk-dotenv\nRETICULA_LLM_TIMEOUT_SECS=110\n",
        );
        let proc_env = BTreeMap::from([
            ("RETICULA_LLM_MODEL", ""),
            ("RETICULA_LLM_API_KEY", "   "),
            ("RETICULA_LLM_TIMEOUT_SECS", "150"),
        ]);
        let map = load_env_map(Some(&env_path), &LLM_ENV_KEYS, |k| {
            proc_env.get(k).map(|&v| v.to_owned())
        });
        let cfg = LlmConfig::from_map(&map).unwrap().expect("enabled config");
        assert_eq!(cfg.model, "dotenv-model");
        assert_eq!(cfg.api_key(), "sk-dotenv");
        assert_eq!(cfg.request_timeout, Duration::from_secs(150));

        // Explicit non-blank RETICULA_LLM_ENABLED=false disables dotenv live config.
        let disable_env = BTreeMap::from([("RETICULA_LLM_ENABLED", "false")]);
        let disabled_map = load_env_map(Some(&env_path), &LLM_ENV_KEYS, |k| {
            disable_env.get(k).map(|&v| v.to_owned())
        });
        assert!(LlmConfig::from_map(&disabled_map).unwrap().is_none());
        let _ = std::fs::remove_dir_all(env_path.parent().unwrap());
    }

    #[test]
    fn llm_config_rejects_invalid_timeout_without_echoing_value() {
        let map = BTreeMap::from([
            ("RETICULA_LLM_ENABLED".to_owned(), "true".to_owned()),
            ("RETICULA_LLM_MODEL".to_owned(), "test/model".to_owned()),
            ("RETICULA_LLM_API_KEY".to_owned(), "sk-secret".to_owned()),
            (
                "RETICULA_LLM_TIMEOUT_SECS".to_owned(),
                "not-a-number".to_owned(),
            ),
        ]);
        let err = LlmConfig::from_map(&map).unwrap_err().to_string();
        assert!(err.contains("RETICULA_LLM_TIMEOUT_SECS"));
        assert!(!err.contains("not-a-number"));
        assert!(!err.contains("sk-secret"));
    }

    #[test]
    fn llm_config_parses_custom_provider_reasoning_and_timeout() {
        let map = BTreeMap::from([
            ("RETICULA_LLM_ENABLED".to_owned(), "true".to_owned()),
            ("RETICULA_LLM_PROVIDER".to_owned(), "openrouter".to_owned()),
            (
                "RETICULA_LLM_BASE_URL".to_owned(),
                "https://openrouter.ai/api/v1/".to_owned(),
            ),
            ("RETICULA_LLM_MODEL".to_owned(), "test/model".to_owned()),
            ("RETICULA_LLM_API_KEY".to_owned(), "sk-custom".to_owned()),
            (
                "RETICULA_LLM_REASONING_EFFORT".to_owned(),
                "medium".to_owned(),
            ),
            ("RETICULA_LLM_TIMEOUT_SECS".to_owned(), "150".to_owned()),
        ]);
        let cfg = LlmConfig::from_map(&map).unwrap().expect("enabled config");
        assert_eq!(cfg.provider, "openrouter");
        assert_eq!(cfg.base_url, "https://openrouter.ai/api/v1");
        assert_eq!(cfg.reasoning_effort.as_deref(), Some("medium"));
        assert_eq!(cfg.request_timeout, Duration::from_secs(150));
    }
}
