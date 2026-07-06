//! `config` command family: get/set/list/path/set-key/unset-key.
//!
//! `set`/`get` operate on the on-disk TOML file directly (never touching the
//! API key). `set-key`/`unset-key` operate on the OS keyring only -- the key
//! is never written to the config file.

use std::io::BufRead;

use crate::cli::ConfigAction;
use crate::config::{self, EffectiveConfig, FileConfig};
use crate::error::ApiError;

fn read_file(path: &std::path::Path) -> Result<FileConfig, ApiError> {
    if !path.exists() {
        return Ok(FileConfig::default());
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| ApiError::permanent(format!("failed to read config file: {e}")))?;
    toml::from_str(&text).map_err(|e| ApiError::permanent(format!("invalid config file: {e}")))
}

fn write_file(path: &std::path::Path, cfg: &FileConfig) -> Result<(), ApiError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| ApiError::permanent(format!("failed to create config dir: {e}")))?;
    }
    let text = toml::to_string_pretty(cfg)
        .map_err(|e| ApiError::permanent(format!("failed to serialize config: {e}")))?;
    std::fs::write(path, text)
        .map_err(|e| ApiError::permanent(format!("failed to write config file: {e}")))
}

pub fn run(action: &ConfigAction, cfg: &EffectiveConfig) -> Result<(), ApiError> {
    match action {
        ConfigAction::Path => {
            println!("{}", cfg.config_path.display());
            Ok(())
        }
        ConfigAction::List => {
            println!("{}", serde_json::to_string_pretty(cfg).unwrap_or_default());
            Ok(())
        }
        ConfigAction::Get { key } => {
            let file = read_file(&cfg.config_path)?;
            let value = get_field(&file, key)?;
            println!("{value}");
            Ok(())
        }
        ConfigAction::Set { key, value } => {
            let mut file = read_file(&cfg.config_path)?;
            set_field(&mut file, key, value)?;
            write_file(&cfg.config_path, &file)?;
            println!("set {key} = {value}");
            Ok(())
        }
        ConfigAction::SetKey { provider } => {
            let stdin = std::io::stdin();
            let mut line = String::new();
            stdin.lock().read_line(&mut line).map_err(|e| {
                ApiError::permanent(format!("failed to read API key from stdin: {e}"))
            })?;
            let key = line.trim();
            if key.is_empty() {
                return Err(ApiError::Usage {
                    message: "no API key provided on stdin".to_string(),
                });
            }
            let provider = provider.as_deref().unwrap_or(&cfg.active_provider);
            config::set_provider_api_key(provider, key)?;
            println!("API key stored for provider '{provider}'.");
            Ok(())
        }
        ConfigAction::UnsetKey { provider } => {
            let provider = provider.as_deref().unwrap_or(&cfg.active_provider);
            config::unset_provider_api_key(provider)?;
            println!("API key removed for provider '{provider}'.");
            Ok(())
        }
        ConfigAction::Migrate => migrate(&cfg.config_path),
        ConfigAction::CacheClear => {
            crate::cache::Cache::from_config(cfg).clear()?;
            println!("Cache cleared.");
            Ok(())
        }
    }
}

fn migrate(path: &std::path::Path) -> Result<(), ApiError> {
    let mut file = read_file(path)?;
    if !file.providers.is_empty() {
        println!("Configuration already uses provider profiles.");
        return Ok(());
    }
    let kind = if file
        .base_url
        .as_deref()
        .map(|url| url.trim_end_matches('/') == config::DEFAULT_BASE_URL)
        .unwrap_or(true)
    {
        config::ProviderKind::Nvidia
    } else {
        config::ProviderKind::OpenaiCompatible
    };
    let profile_name = if kind == config::ProviderKind::Nvidia {
        "nvidia"
    } else {
        "default"
    };
    file.providers.insert(
        profile_name.to_string(),
        config::ProviderConfig {
            kind,
            base_url: file.base_url.take(),
            model: file.model.take(),
            auth: Some(config::AuthMode::Required),
            api_key_env: Some("NVIDIA_API_KEY".to_string()),
            timeout_secs: file.timeout_secs.take(),
            rate_limit: std::mem::take(&mut file.rate_limit),
        },
    );
    file.active_provider = Some(profile_name.to_string());
    let backup = path.with_extension("toml.pre-providers.bak");
    if path.exists() {
        std::fs::copy(path, &backup)
            .map_err(|e| ApiError::permanent(format!("failed to back up config: {e}")))?;
    }
    write_file(path, &file)?;
    println!("Migrated config; backup: {}", backup.display());
    Ok(())
}

fn get_field(file: &FileConfig, key: &str) -> Result<String, ApiError> {
    Ok(match key {
        "model" => file.model.clone().unwrap_or_default(),
        "active_provider" => file.active_provider.clone().unwrap_or_default(),
        "base_url" => file.base_url.clone().unwrap_or_default(),
        "timeout_secs" => file.timeout_secs.map(|v| v.to_string()).unwrap_or_default(),
        "max_tokens" => file.max_tokens.map(|v| v.to_string()).unwrap_or_default(),
        "temperature" => file.temperature.map(|v| v.to_string()).unwrap_or_default(),
        "stream" => file.stream.map(|v| v.to_string()).unwrap_or_default(),
        "cache.enabled" => file.cache.enabled.to_string(),
        "cache.ttl_secs" => file
            .cache
            .ttl_secs
            .map(|v| v.to_string())
            .unwrap_or_default(),
        "rate_limit.rpm" => file
            .rate_limit
            .rpm
            .map(|v| v.to_string())
            .unwrap_or_default(),
        "rate_limit.min_interval" => file.rate_limit.min_interval.clone().unwrap_or_default(),
        other => {
            if let Some(rest) = other.strip_prefix("providers.") {
                let (name, field) = rest.split_once('.').ok_or_else(|| ApiError::Usage {
                    message: format!("invalid provider config key: {other}"),
                })?;
                let provider = file.providers.get(name).ok_or_else(|| ApiError::Usage {
                    message: format!("unknown provider profile: {name}"),
                })?;
                return Ok(match field {
                    "kind" => format!("{:?}", provider.kind).to_ascii_lowercase(),
                    "base_url" => provider.base_url.clone().unwrap_or_default(),
                    "model" => provider.model.clone().unwrap_or_default(),
                    "auth" => provider
                        .auth
                        .as_ref()
                        .map(|v| format!("{v:?}").to_ascii_lowercase())
                        .unwrap_or_default(),
                    "api_key_env" => provider.api_key_env.clone().unwrap_or_default(),
                    "timeout_secs" => provider
                        .timeout_secs
                        .map(|v| v.to_string())
                        .unwrap_or_default(),
                    "rate_limit.rpm" => provider
                        .rate_limit
                        .rpm
                        .map(|v| v.to_string())
                        .unwrap_or_default(),
                    "rate_limit.min_interval" => {
                        provider.rate_limit.min_interval.clone().unwrap_or_default()
                    }
                    _ => {
                        return Err(ApiError::Usage {
                            message: format!("unknown provider config key: {other}"),
                        })
                    }
                });
            }
            return Err(ApiError::Usage {
                message: format!("unknown config key: {other}"),
            });
        }
    })
}

fn set_field(file: &mut FileConfig, key: &str, value: &str) -> Result<(), ApiError> {
    let parse_err = |e: std::num::ParseIntError| ApiError::Usage {
        message: format!("invalid value: {e}"),
    };
    let parse_float_err = |e: std::num::ParseFloatError| ApiError::Usage {
        message: format!("invalid value: {e}"),
    };
    let parse_bool_err = |e: std::str::ParseBoolError| ApiError::Usage {
        message: format!("invalid value: {e}"),
    };

    match key {
        "model" => file.model = Some(value.to_string()),
        "active_provider" => file.active_provider = Some(value.to_string()),
        "base_url" => file.base_url = Some(value.to_string()),
        "timeout_secs" => file.timeout_secs = Some(value.parse().map_err(parse_err)?),
        "max_tokens" => file.max_tokens = Some(value.parse().map_err(parse_err)?),
        "temperature" => file.temperature = Some(value.parse().map_err(parse_float_err)?),
        "stream" => file.stream = Some(value.parse().map_err(parse_bool_err)?),
        "cache.enabled" => file.cache.enabled = value.parse().map_err(parse_bool_err)?,
        "cache.ttl_secs" => file.cache.ttl_secs = Some(value.parse().map_err(parse_err)?),
        "rate_limit.rpm" => file.rate_limit.rpm = Some(value.parse().map_err(parse_err)?),
        "rate_limit.min_interval" => {
            config::parse_duration(value)?;
            file.rate_limit.min_interval = Some(value.to_string());
        }
        other => {
            if let Some(rest) = other.strip_prefix("providers.") {
                let (name, field) = rest.split_once('.').ok_or_else(|| ApiError::Usage {
                    message: format!("invalid provider config key: {other}"),
                })?;
                let provider = file.providers.entry(name.to_string()).or_default();
                match field {
                    "kind" => {
                        provider.kind = match value {
                            "nvidia" => config::ProviderKind::Nvidia,
                            "lmstudio" => config::ProviderKind::Lmstudio,
                            "openai-compatible" => config::ProviderKind::OpenaiCompatible,
                            _ => {
                                return Err(ApiError::Usage {
                                    message: format!("invalid provider kind: {value}"),
                                })
                            }
                        }
                    }
                    "base_url" => provider.base_url = Some(value.to_string()),
                    "model" => provider.model = Some(value.to_string()),
                    "auth" => {
                        provider.auth = Some(match value {
                            "required" => config::AuthMode::Required,
                            "optional" => config::AuthMode::Optional,
                            "none" => config::AuthMode::None,
                            _ => {
                                return Err(ApiError::Usage {
                                    message: format!("invalid auth mode: {value}"),
                                })
                            }
                        })
                    }
                    "api_key_env" => provider.api_key_env = Some(value.to_string()),
                    "timeout_secs" => {
                        provider.timeout_secs = Some(value.parse().map_err(parse_err)?)
                    }
                    "rate_limit.rpm" => {
                        let rpm = value.parse().map_err(parse_err)?;
                        if rpm == 0 {
                            return Err(ApiError::Usage {
                                message: "rpm must be greater than zero".to_string(),
                            });
                        }
                        provider.rate_limit.rpm = Some(rpm);
                    }
                    "rate_limit.min_interval" => {
                        config::parse_duration(value)?;
                        provider.rate_limit.min_interval = Some(value.to_string());
                    }
                    _ => {
                        return Err(ApiError::Usage {
                            message: format!("unknown provider config key: {other}"),
                        })
                    }
                }
                return Ok(());
            }
            return Err(ApiError::Usage {
                message: format!("unknown config key: {other}"),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dynamic_provider_fields_can_be_created() {
        let mut file = FileConfig::default();
        set_field(&mut file, "providers.local.kind", "lmstudio").unwrap();
        set_field(&mut file, "providers.local.rate_limit.min_interval", "1s").unwrap();
        let provider = file.providers.get("local").unwrap();
        assert_eq!(provider.kind, config::ProviderKind::Lmstudio);
        assert_eq!(provider.rate_limit.min_interval.as_deref(), Some("1s"));
    }

    #[test]
    fn migrate_creates_profile_and_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "model = \"legacy/model\"\nbase_url = \"https://example.test/v1\"\n",
        )
        .unwrap();
        migrate(&path).unwrap();
        let migrated = read_file(&path).unwrap();
        assert_eq!(migrated.active_provider.as_deref(), Some("default"));
        assert!(migrated.providers.contains_key("default"));
        assert!(path.with_extension("toml.pre-providers.bak").exists());
    }
}
