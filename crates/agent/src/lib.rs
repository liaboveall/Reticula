//! Shared AI analyst runtime, three-way branch diff/merge planner, context
//! compressor, streaming OpenAI-compatible LLM client, deterministic replay
//! bundler, and SpreadsheetBench evaluation harness (`18-m4.md`).

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
        let mut map = BTreeMap::new();
        if let Some(path) = dotenv_path
            && let Ok(text) = std::fs::read_to_string(path)
        {
            for (k, v) in parse_dotenv(&text) {
                map.insert(k, v);
            }
        }
        for key in [
            "RETICULA_LLM_ENABLED",
            "RETICULA_LLM_PROVIDER",
            "RETICULA_LLM_BASE_URL",
            "RETICULA_LLM_MODEL",
            "RETICULA_LLM_API_KEY",
            "RETICULA_LLM_REASONING_EFFORT",
            "RETICULA_LLM_TIMEOUT_SECS",
        ] {
            if let Ok(val) = std::env::var(key) {
                map.insert(key.to_owned(), val);
            }
        }

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
            .map(String::as_str)
            .unwrap_or("https://api.openai.com/v1")
            .trim()
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
            .map(String::as_str)
            .unwrap_or("openai-compat")
            .trim()
            .to_owned();
        let reasoning_effort = map
            .get("RETICULA_LLM_REASONING_EFFORT")
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty());
        let timeout_secs: u64 = map
            .get("RETICULA_LLM_TIMEOUT_SECS")
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(90);

        let mut missing = Vec::new();
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

/// Parses KEY=VALUE lines from a local dotenv file, ignoring comments and quotes.
pub fn parse_dotenv(text: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let key = k.trim();
        if key.is_empty() {
            continue;
        }
        let mut val = v.trim();
        if (val.starts_with('"') && val.ends_with('"') && val.len() >= 2)
            || (val.starts_with('\'') && val.ends_with('\'') && val.len() >= 2)
        {
            val = &val[1..val.len() - 1];
        }
        pairs.push((key.to_owned(), val.to_owned()));
    }
    pairs
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
