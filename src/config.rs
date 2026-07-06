//! Configuration loading with precedence: flags > env > file > default.
//!
//! The API key itself never passes through the file layer: it comes from
//! `NVIDIA_API_KEY` first, then the OS keyring (service `insane-cli`, user
//! `nvidia_api_key`). It is never written to the config file and never
//! logged.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::cli::Cli;
use crate::error::ApiError;

pub const DEFAULT_BASE_URL: &str = "https://integrate.api.nvidia.com/v1";
pub const DEFAULT_MODEL: &str = "meta/llama-3.3-70b-instruct";
pub const DEFAULT_TIMEOUT_SECS: u64 = 60;
pub const DEFAULT_MAX_TOKENS: u32 = 4096;
pub const DEFAULT_TEMPERATURE: f32 = 0.7;
/// Default `agent.temperature` when neither the config file nor
/// `INSANE_AGENT_TEMPERATURE` set one, and the global `temperature` itself
/// was never explicitly configured either (SPEC-UX A2). A lower temperature
/// than the general-purpose default makes tool-calling more deterministic.
pub const DEFAULT_AGENT_TEMPERATURE: f32 = 0.2;
/// Default for `agent.lenient_tool_calls` (SPEC-UX A4).
pub const DEFAULT_LENIENT_TOOL_CALLS: bool = true;
pub const DEFAULT_RPM: u32 = 40;
/// Hard ceiling for `rate_limit.rpm` when talking to the public NIM
/// endpoint, per SPEC §5.
pub const NIM_RPM_CEILING: u32 = 40;
pub const DEFAULT_MAX_CONTEXT_BYTES: usize = 192 * 1024;
/// Default cap on tool-calling rounds per user turn (SPEC-AGENT §4).
pub const DEFAULT_AGENT_MAX_ROUNDS: u32 = 20;
/// Default `ui` mode (SPEC-UX Part B): the fullscreen TUI when the terminal
/// supports it, falling back to line mode automatically for non-TTY
/// stdin/stdout regardless of this setting.
pub const DEFAULT_UI: &str = "tui";

const KEYRING_SERVICE: &str = "insane-cli";
const KEYRING_USER: &str = "nvidia_api_key";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CacheConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub ttl_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RateLimitConfig {
    #[serde(default)]
    pub rpm: Option<u32>,
    /// Human duration such as `250ms`, `1s`, or `2m`.
    #[serde(default)]
    pub min_interval: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    #[default]
    Nvidia,
    Lmstudio,
    OpenaiCompatible,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum AuthMode {
    #[default]
    Required,
    Optional,
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProviderConfig {
    #[serde(default)]
    pub kind: ProviderKind,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub auth: Option<AuthMode>,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub rate_limit: RateLimitConfig,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResolvedProvider {
    pub name: String,
    pub kind: ProviderKind,
    pub base_url: String,
    pub model: String,
    pub auth: AuthMode,
    pub api_key_env: String,
    pub timeout_secs: u64,
    pub rate_limit_rpm: Option<u32>,
    pub min_interval_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentConfig {
    /// Cap on tool-calling rounds per user turn before the agent loop gives
    /// up with a clear error (SPEC-AGENT §4). Defaults to 20.
    #[serde(default)]
    pub max_rounds: Option<u32>,
    /// Generation temperature used specifically for agent (tool-calling)
    /// turns (SPEC-UX A2). Falls back to the global `temperature` if that
    /// was explicitly configured, else to `DEFAULT_AGENT_TEMPERATURE`.
    #[serde(default)]
    pub temperature: Option<f32>,
    /// Whether to detect a tool call emitted as plain text (some models
    /// don't reliably use structured `tool_calls`) and recover it into a
    /// normal tool execution (SPEC-UX A4). Defaults to `true`.
    #[serde(default)]
    pub lenient_tool_calls: Option<bool>,
    /// Extra text appended to the end of the agent's system prompt
    /// (SPEC-UX A1).
    #[serde(default)]
    pub system_prompt_extra: Option<String>,
}

/// Config file schema (`{config_dir}/insane-cli/config.toml`). Never
/// contains the API key.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FileConfig {
    #[serde(default)]
    pub active_provider: Option<String>,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub stream: Option<bool>,
    #[serde(default)]
    pub cache: CacheConfig,
    #[serde(default)]
    pub rate_limit: RateLimitConfig,
    #[serde(default)]
    pub ignore: Vec<String>,
    #[serde(default)]
    pub agent: AgentConfig,
    /// `"tui"` (default) or `"plain"` (SPEC-UX Part B). Overridden by
    /// `--plain` and by a non-TTY stdin/stdout regardless of this value.
    #[serde(default)]
    pub ui: Option<String>,
}

/// Fully resolved, effective configuration used by the rest of the program.
/// Deliberately excludes the API key so it can be printed/serialized freely
/// (e.g. by `status`, `config list`).
#[derive(Debug, Clone, Serialize)]
pub struct EffectiveConfig {
    pub active_provider: String,
    pub providers: BTreeMap<String, ProviderConfig>,
    pub provider_kind: ProviderKind,
    pub provider_auth: AuthMode,
    pub provider_api_key_env: String,
    pub model: String,
    pub base_url: String,
    pub timeout_secs: u64,
    pub max_tokens: u32,
    pub temperature: f32,
    pub stream: bool,
    pub cache: CacheConfig,
    pub rate_limit_rpm: Option<u32>,
    pub rate_limit_min_interval_ms: u64,
    pub ignore: Vec<String>,
    pub max_context_bytes: usize,
    pub agent_max_rounds: u32,
    pub agent_temperature: f32,
    pub lenient_tool_calls: bool,
    pub system_prompt_extra: String,
    /// `"tui"` or `"plain"` (SPEC-UX Part B); `chat` also honors `--plain`
    /// and falls back to plain automatically for a non-TTY stdin/stdout.
    pub ui: String,
    #[serde(skip)]
    pub config_path: PathBuf,
}

pub fn config_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "insane-cli").map(|d| d.config_dir().to_path_buf())
}

pub fn default_config_path() -> PathBuf {
    config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("config.toml")
}

/// Not wired up yet -- phase 2's `cache.rs` will read/write under this
/// directory. Kept here now since it's a `directories`-derived path that
/// belongs with the rest of the OS-path resolution logic.
#[allow(dead_code)]
pub fn cache_dir() -> PathBuf {
    directories::ProjectDirs::from("", "", "insane-cli")
        .map(|d| d.cache_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".cache/insane-cli"))
}

fn read_file_config(path: &Path) -> Result<FileConfig, ApiError> {
    if !path.exists() {
        return Ok(FileConfig::default());
    }
    let text = std::fs::read_to_string(path).map_err(|e| {
        ApiError::permanent(format!(
            "failed to read config file {}: {e}",
            path.display()
        ))
    })?;
    toml::from_str(&text)
        .map_err(|e| ApiError::permanent(format!("invalid config file {}: {e}", path.display())))
}

/// Resolves the effective configuration by layering flags over env over the
/// TOML file over built-in defaults.
pub fn load(cli: &Cli) -> Result<EffectiveConfig, ApiError> {
    let config_path = cli.config.clone().unwrap_or_else(default_config_path);
    let mut file = read_file_config(&config_path)?;

    // Legacy flat settings become an implicit profile in memory. `config
    // migrate` can persist the new shape without making existing installs
    // unusable on upgrade.
    if file.providers.is_empty() {
        let kind = if file
            .base_url
            .as_deref()
            .map(|url| url.trim_end_matches('/') == DEFAULT_BASE_URL.trim_end_matches('/'))
            .unwrap_or(true)
        {
            ProviderKind::Nvidia
        } else {
            ProviderKind::OpenaiCompatible
        };
        file.providers.insert(
            "nvidia".to_string(),
            ProviderConfig {
                kind,
                base_url: file.base_url.clone(),
                model: file.model.clone(),
                auth: Some(AuthMode::Required),
                api_key_env: Some("NVIDIA_API_KEY".to_string()),
                timeout_secs: file.timeout_secs,
                rate_limit: file.rate_limit.clone(),
            },
        );
        file.active_provider
            .get_or_insert_with(|| "nvidia".to_string());
    }

    let active_provider = cli
        .provider
        .clone()
        .or_else(|| std::env::var("INSANE_PROVIDER").ok())
        .or_else(|| file.active_provider.clone())
        .or_else(|| {
            if file.providers.contains_key("nvidia") {
                Some("nvidia".to_string())
            } else {
                file.providers.keys().next().cloned()
            }
        })
        .unwrap_or_else(|| "nvidia".to_string());
    let raw_profile = file
        .providers
        .get(&active_provider)
        .ok_or_else(|| ApiError::Usage {
            message: format!(
                "unknown provider profile '{active_provider}' (configured: {})",
                file.providers
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        })?;
    let mut resolved = resolve_provider(&active_provider, raw_profile)?;

    if let Ok(value) = std::env::var("INSANE_BASE_URL") {
        resolved.base_url = value;
    }
    if let Some(value) = cli
        .model
        .clone()
        .or_else(|| std::env::var("INSANE_MODEL").ok())
    {
        resolved.model = value;
    }
    if let Some(value) = cli.timeout.or_else(|| {
        std::env::var("INSANE_TIMEOUT")
            .ok()
            .and_then(|v| v.parse().ok())
    }) {
        resolved.timeout_secs = value;
    }
    if let Some(value) = std::env::var("INSANE_RPM")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
    {
        resolved.rate_limit_rpm = Some(value);
    }
    if let Ok(value) = std::env::var("INSANE_MIN_INTERVAL") {
        resolved.min_interval_ms = parse_duration(&value)?.as_millis() as u64;
    }

    if resolved.base_url.trim_end_matches('/') == DEFAULT_BASE_URL.trim_end_matches('/') {
        resolved.rate_limit_rpm = Some(
            resolved
                .rate_limit_rpm
                .unwrap_or(DEFAULT_RPM)
                .min(NIM_RPM_CEILING),
        );
    }

    let max_tokens = std::env::var("INSANE_MAX_TOKENS")
        .ok()
        .and_then(|v| v.parse().ok())
        .or(file.max_tokens)
        .unwrap_or(DEFAULT_MAX_TOKENS);

    let temperature = std::env::var("INSANE_TEMPERATURE")
        .ok()
        .and_then(|v| v.parse().ok())
        .or(file.temperature)
        .unwrap_or(DEFAULT_TEMPERATURE);

    let stream = if cli.no_stream {
        false
    } else if cli.stream {
        true
    } else {
        std::env::var("INSANE_STREAM")
            .ok()
            .and_then(|v| v.parse().ok())
            .or(file.stream)
            .unwrap_or(true)
    };

    let cache_enabled = if cli.no_cache {
        false
    } else {
        file.cache.enabled
    };

    let agent_max_rounds = std::env::var("INSANE_AGENT_MAX_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .or(file.agent.max_rounds)
        .unwrap_or(DEFAULT_AGENT_MAX_ROUNDS);

    // agent.temperature: explicit env/file value wins; otherwise fall back
    // to the global `temperature` *only if it was itself explicitly
    // configured* (env or file), else use the agent-specific default. This
    // keeps "temperature = 0.9" in the file applying to plain chat/ask while
    // still giving tool-calling turns a more deterministic default when the
    // user never touched either setting (SPEC-UX A2).
    let global_temperature_explicit = std::env::var("INSANE_TEMPERATURE")
        .ok()
        .and_then(|v| v.parse().ok())
        .or(file.temperature);
    let agent_temperature = std::env::var("INSANE_AGENT_TEMPERATURE")
        .ok()
        .and_then(|v| v.parse().ok())
        .or(file.agent.temperature)
        .or(global_temperature_explicit)
        .unwrap_or(DEFAULT_AGENT_TEMPERATURE);

    let lenient_tool_calls = std::env::var("INSANE_LENIENT_TOOL_CALLS")
        .ok()
        .and_then(|v| v.parse().ok())
        .or(file.agent.lenient_tool_calls)
        .unwrap_or(DEFAULT_LENIENT_TOOL_CALLS);

    let system_prompt_extra = file.agent.system_prompt_extra.clone().unwrap_or_default();

    let ui = std::env::var("INSANE_UI")
        .ok()
        .or(file.ui.clone())
        .unwrap_or_else(|| DEFAULT_UI.to_string());
    let ui = if ui == "plain" || ui == "tui" {
        ui
    } else {
        DEFAULT_UI.to_string()
    };

    Ok(EffectiveConfig {
        active_provider,
        providers: file.providers,
        provider_kind: resolved.kind,
        provider_auth: resolved.auth,
        provider_api_key_env: resolved.api_key_env,
        model: resolved.model,
        base_url: resolved.base_url,
        timeout_secs: resolved.timeout_secs,
        max_tokens,
        temperature,
        stream,
        cache: CacheConfig {
            enabled: cache_enabled,
            ttl_secs: file.cache.ttl_secs,
        },
        rate_limit_rpm: resolved.rate_limit_rpm,
        rate_limit_min_interval_ms: resolved.min_interval_ms,
        ignore: file.ignore,
        max_context_bytes: DEFAULT_MAX_CONTEXT_BYTES,
        agent_max_rounds,
        agent_temperature,
        lenient_tool_calls,
        system_prompt_extra,
        ui,
        config_path,
    })
}

pub fn parse_duration(value: &str) -> Result<Duration, ApiError> {
    let value = value.trim().to_ascii_lowercase();
    let (number, multiplier) = if let Some(v) = value.strip_suffix("ms") {
        (v, 1u64)
    } else if let Some(v) = value.strip_suffix('s') {
        (v, 1_000)
    } else if let Some(v) = value.strip_suffix('m') {
        (v, 60_000)
    } else {
        return Err(ApiError::Usage {
            message: format!("invalid duration '{value}'; use ms, s, or m (for example 1s)"),
        });
    };
    let amount: u64 = number.trim().parse().map_err(|_| ApiError::Usage {
        message: format!("invalid duration '{value}'"),
    })?;
    Ok(Duration::from_millis(amount.saturating_mul(multiplier)))
}

pub fn resolve_provider(
    name: &str,
    profile: &ProviderConfig,
) -> Result<ResolvedProvider, ApiError> {
    let (default_url, default_auth, default_env, default_rpm) = match profile.kind {
        ProviderKind::Nvidia => (
            DEFAULT_BASE_URL,
            AuthMode::Required,
            "NVIDIA_API_KEY",
            Some(DEFAULT_RPM),
        ),
        ProviderKind::Lmstudio => (
            "http://127.0.0.1:1234/v1",
            AuthMode::Optional,
            "LMSTUDIO_API_KEY",
            None,
        ),
        ProviderKind::OpenaiCompatible => ("", AuthMode::Required, "OPENAI_API_KEY", None),
    };
    let base_url = profile
        .base_url
        .clone()
        .unwrap_or_else(|| default_url.to_string());
    if base_url.trim().is_empty() {
        return Err(ApiError::Usage {
            message: format!("provider '{name}' requires base_url"),
        });
    }
    if profile.rate_limit.rpm == Some(0) {
        return Err(ApiError::Usage {
            message: format!("provider '{name}': rpm must be greater than zero or omitted"),
        });
    }
    let min_interval = match profile.rate_limit.min_interval.as_deref() {
        Some(value) => parse_duration(value)?,
        None => Duration::ZERO,
    };
    Ok(ResolvedProvider {
        name: name.to_string(),
        kind: profile.kind.clone(),
        base_url,
        model: profile
            .model
            .clone()
            .unwrap_or_else(|| DEFAULT_MODEL.to_string()),
        auth: profile.auth.clone().unwrap_or(default_auth),
        api_key_env: profile
            .api_key_env
            .clone()
            .unwrap_or_else(|| default_env.to_string()),
        timeout_secs: profile.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS),
        rate_limit_rpm: profile.rate_limit.rpm.or(default_rpm),
        min_interval_ms: min_interval.as_millis() as u64,
    })
}

impl EffectiveConfig {
    pub fn activated(&self, name: &str) -> Result<Self, ApiError> {
        let profile = self.providers.get(name).ok_or_else(|| ApiError::Usage {
            message: format!("unknown provider profile '{name}'"),
        })?;
        let resolved = resolve_provider(name, profile)?;
        let mut next = self.clone();
        next.active_provider = name.to_string();
        next.provider_kind = resolved.kind;
        next.provider_auth = resolved.auth;
        next.provider_api_key_env = resolved.api_key_env;
        next.model = resolved.model;
        next.base_url = resolved.base_url;
        next.timeout_secs = resolved.timeout_secs;
        next.rate_limit_rpm = resolved.rate_limit_rpm;
        next.rate_limit_min_interval_ms = resolved.min_interval_ms;
        Ok(next)
    }
}

/// Resolves the API key: `NVIDIA_API_KEY` env var first, then the OS
/// keyring. Never logs or persists the value beyond returning it.
pub fn resolve_api_key() -> Result<String, ApiError> {
    if let Ok(key) = std::env::var("NVIDIA_API_KEY") {
        if !key.trim().is_empty() {
            return Ok(key);
        }
    }
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER).map_err(|e| ApiError::Auth {
        message: format!("keyring unavailable: {e}"),
    })?;
    entry.get_password().map_err(|_| ApiError::Auth {
        message: "no API key found; set NVIDIA_API_KEY or run `insane config set-key`".to_string(),
    })
}

pub fn resolve_provider_api_key(cfg: &EffectiveConfig) -> Result<Option<String>, ApiError> {
    if cfg.provider_auth == AuthMode::None {
        return Ok(None);
    }
    if let Ok(key) = std::env::var(&cfg.provider_api_key_env) {
        if !key.trim().is_empty() {
            return Ok(Some(key));
        }
    }
    let user = format!("provider:{}", cfg.active_provider);
    let entry = keyring::Entry::new(KEYRING_SERVICE, &user).map_err(|e| ApiError::Auth {
        message: format!("keyring unavailable: {e}"),
    })?;
    match entry.get_password() {
        Ok(key) => Ok(Some(key)),
        Err(_) => {
            if cfg.provider_kind == ProviderKind::Nvidia {
                if let Ok(legacy) = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER) {
                    if let Ok(key) = legacy.get_password() {
                        return Ok(Some(key));
                    }
                }
            }
            if cfg.provider_auth == AuthMode::Optional {
                Ok(None)
            } else {
                Err(ApiError::Auth {
                    message: format!(
                        "no API key for provider '{}'; set {} or run `insane config set-key --provider {}`",
                        cfg.active_provider, cfg.provider_api_key_env, cfg.active_provider
                    ),
                })
            }
        }
    }
}

pub fn set_provider_api_key(provider: &str, key: &str) -> Result<(), ApiError> {
    let user = format!("provider:{provider}");
    let entry = keyring::Entry::new(KEYRING_SERVICE, &user)
        .map_err(|e| ApiError::permanent(format!("keyring unavailable: {e}")))?;
    entry
        .set_password(key)
        .map_err(|e| ApiError::permanent(format!("failed to store API key: {e}")))
}

pub fn unset_provider_api_key(provider: &str) -> Result<(), ApiError> {
    let user = format!("provider:{provider}");
    let entry = keyring::Entry::new(KEYRING_SERVICE, &user)
        .map_err(|e| ApiError::permanent(format!("keyring unavailable: {e}")))?;
    match entry.delete_password() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(ApiError::permanent(format!(
            "failed to remove API key: {e}"
        ))),
    }
}

pub fn set_api_key(key: &str) -> Result<(), ApiError> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)
        .map_err(|e| ApiError::permanent(format!("keyring unavailable: {e}")))?;
    entry
        .set_password(key)
        .map_err(|e| ApiError::permanent(format!("failed to store API key: {e}")))
}

pub fn unset_api_key() -> Result<(), ApiError> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)
        .map_err(|e| ApiError::permanent(format!("keyring unavailable: {e}")))?;
    match entry.delete_password() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(ApiError::permanent(format!(
            "failed to remove API key: {e}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn file_config_round_trips_rpm() {
        let file = FileConfig {
            rate_limit: RateLimitConfig {
                rpm: Some(999),
                min_interval: None,
            },
            ..Default::default()
        };
        let text = toml::to_string(&file).unwrap();
        let parsed: FileConfig = toml::from_str(&text).unwrap();
        assert_eq!(parsed.rate_limit.rpm, Some(999));
    }

    #[test]
    fn rpm_ceiling_enforced_for_default_endpoint() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "rate_limit.rpm = 999\n").unwrap();

        let cli = Cli::parse_from([
            "insane",
            "--config",
            config_path.to_str().unwrap(),
            "status",
        ]);
        let effective = load(&cli).unwrap();
        assert_eq!(effective.rate_limit_rpm, Some(NIM_RPM_CEILING));
    }

    #[test]
    fn agent_max_rounds_defaults_to_20() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "").unwrap();

        let cli = Cli::parse_from([
            "insane",
            "--config",
            config_path.to_str().unwrap(),
            "status",
        ]);
        let effective = load(&cli).unwrap();
        assert_eq!(effective.agent_max_rounds, DEFAULT_AGENT_MAX_ROUNDS);
    }

    #[test]
    fn agent_max_rounds_configurable_via_file() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "[agent]\nmax_rounds = 5\n").unwrap();

        let cli = Cli::parse_from([
            "insane",
            "--config",
            config_path.to_str().unwrap(),
            "status",
        ]);
        let effective = load(&cli).unwrap();
        assert_eq!(effective.agent_max_rounds, 5);
    }

    #[test]
    fn max_tokens_defaults_to_4096() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "").unwrap();
        let cli = Cli::parse_from([
            "insane",
            "--config",
            config_path.to_str().unwrap(),
            "status",
        ]);
        let effective = load(&cli).unwrap();
        assert_eq!(effective.max_tokens, 4096);
    }

    #[test]
    fn agent_temperature_defaults_to_0_2_when_nothing_configured() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "").unwrap();
        let cli = Cli::parse_from([
            "insane",
            "--config",
            config_path.to_str().unwrap(),
            "status",
        ]);
        let effective = load(&cli).unwrap();
        assert_eq!(effective.agent_temperature, DEFAULT_AGENT_TEMPERATURE);
    }

    #[test]
    fn agent_temperature_falls_back_to_explicit_global_temperature() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "temperature = 0.9\n").unwrap();
        let cli = Cli::parse_from([
            "insane",
            "--config",
            config_path.to_str().unwrap(),
            "status",
        ]);
        let effective = load(&cli).unwrap();
        assert_eq!(effective.agent_temperature, 0.9);
        assert_eq!(effective.temperature, 0.9);
    }

    #[test]
    fn agent_temperature_configurable_independently_of_global() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            "temperature = 0.9\n[agent]\ntemperature = 0.1\n",
        )
        .unwrap();
        let cli = Cli::parse_from([
            "insane",
            "--config",
            config_path.to_str().unwrap(),
            "status",
        ]);
        let effective = load(&cli).unwrap();
        assert_eq!(effective.agent_temperature, 0.1);
        assert_eq!(effective.temperature, 0.9);
    }

    #[test]
    fn lenient_tool_calls_defaults_to_true() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "").unwrap();
        let cli = Cli::parse_from([
            "insane",
            "--config",
            config_path.to_str().unwrap(),
            "status",
        ]);
        let effective = load(&cli).unwrap();
        assert!(effective.lenient_tool_calls);
    }

    #[test]
    fn lenient_tool_calls_configurable_via_file() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "[agent]\nlenient_tool_calls = false\n").unwrap();
        let cli = Cli::parse_from([
            "insane",
            "--config",
            config_path.to_str().unwrap(),
            "status",
        ]);
        let effective = load(&cli).unwrap();
        assert!(!effective.lenient_tool_calls);
    }

    #[test]
    fn system_prompt_extra_configurable_via_file() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            "[agent]\nsystem_prompt_extra = \"always run cargo fmt\"\n",
        )
        .unwrap();
        let cli = Cli::parse_from([
            "insane",
            "--config",
            config_path.to_str().unwrap(),
            "status",
        ]);
        let effective = load(&cli).unwrap();
        assert_eq!(effective.system_prompt_extra, "always run cargo fmt");
    }

    #[test]
    fn rpm_not_clamped_for_custom_endpoint() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            "base_url = \"https://example.com/v1\"\nrate_limit.rpm = 999\n",
        )
        .unwrap();

        let cli = Cli::parse_from([
            "insane",
            "--config",
            config_path.to_str().unwrap(),
            "status",
        ]);
        let effective = load(&cli).unwrap();
        assert_eq!(effective.rate_limit_rpm, Some(999));
    }

    #[test]
    fn parses_human_request_intervals() {
        assert_eq!(parse_duration("250ms").unwrap(), Duration::from_millis(250));
        assert_eq!(parse_duration("1s").unwrap(), Duration::from_secs(1));
        assert_eq!(parse_duration("2m").unwrap(), Duration::from_secs(120));
        assert!(parse_duration("1").is_err());
    }

    #[test]
    fn lmstudio_profile_defaults_to_local_optional_auth() {
        let profile = ProviderConfig {
            kind: ProviderKind::Lmstudio,
            model: Some("local".into()),
            ..Default::default()
        };
        let resolved = resolve_provider("local", &profile).unwrap();
        assert_eq!(resolved.base_url, "http://127.0.0.1:1234/v1");
        assert_eq!(resolved.auth, AuthMode::Optional);
        assert_eq!(resolved.rate_limit_rpm, None);
    }

    #[test]
    fn selects_configured_provider_profile() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            r#"
active_provider = "local"
[providers.local]
kind = "lmstudio"
model = "local/model"
[providers.local.rate_limit]
min_interval = "1s"
"#,
        )
        .unwrap();
        let cli = Cli::parse_from([
            "insane",
            "--config",
            config_path.to_str().unwrap(),
            "status",
        ]);
        let effective = load(&cli).unwrap();
        assert_eq!(effective.active_provider, "local");
        assert_eq!(effective.model, "local/model");
        assert_eq!(effective.rate_limit_min_interval_ms, 1000);
        assert_eq!(effective.rate_limit_rpm, None);
    }
}
