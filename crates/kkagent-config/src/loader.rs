use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use crate::AppConfig;

pub fn default_config_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".kkagent")
}

pub fn default_config_path() -> PathBuf {
    default_config_dir().join("config.toml")
}

pub fn load_config(path: Option<&Path>) -> Result<AppConfig> {
    let config_path = match path {
        Some(p) => p.to_path_buf(),
        None => default_config_path(),
    };

    let mut config = if !config_path.exists() {
        tracing::info!("Config file not found at {:?}, using defaults", config_path);
        AppConfig::default()
    } else {
        let content = std::fs::read_to_string(&config_path)
            .with_context(|| format!("Failed to read config: {:?}", config_path))?;
        toml::from_str(&content)
            .with_context(|| format!("Failed to parse config: {:?}", config_path))?
    };

    apply_env_overrides(&mut config);
    Ok(config)
}

/// Apply KKAGENT_* / common env overrides (kimi-compatible subset).
fn apply_env_overrides(config: &mut AppConfig) {
    if let Ok(v) = std::env::var("KKAGENT_DEFAULT_MODEL") {
        config.default_model = Some(v);
    }
    if let Ok(v) = std::env::var("KKAGENT_SECONDARY_MODEL") {
        config.secondary_model = Some(v);
    }
    if let Ok(v) = std::env::var("KKAGENT_PERMISSION_MODE") {
        config.default_permission_mode = Some(v);
    }
    // Inject API keys into first matching provider if empty
    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
        for p in config.providers.values_mut() {
            if p.provider_type == "anthropic"
                && p.api_key.as_ref().map(|s| s.is_empty()).unwrap_or(true)
            {
                p.api_key = Some(key.clone());
            }
        }
    }
    if let Ok(key) = std::env::var("OPENAI_API_KEY") {
        for p in config.providers.values_mut() {
            if (p.provider_type == "openai" || p.provider_type == "openai-responses")
                && p.api_key.as_ref().map(|s| s.is_empty()).unwrap_or(true)
            {
                p.api_key = Some(key.clone());
            }
        }
    }
    if let Ok(key) = std::env::var("GOOGLE_API_KEY") {
        for p in config.providers.values_mut() {
            if (p.provider_type == "google" || p.provider_type == "google-genai")
                && p.api_key.as_ref().map(|s| s.is_empty()).unwrap_or(true)
            {
                p.api_key = Some(key.clone());
            }
        }
    }
    if let Ok(url) = std::env::var("KKAGENT_MOONSHOT_SEARCH_URL") {
        let key = std::env::var("KKAGENT_MOONSHOT_SEARCH_KEY")
            .ok()
            .or_else(|| std::env::var("MOONSHOT_API_KEY").ok());
        let services = config.services.get_or_insert_with(Default::default);
        services.moonshot_search = Some(crate::ServiceEndpoint {
            base_url: url,
            api_key: key,
        });
    }
}

pub fn ensure_config_dir() -> Result<PathBuf> {
    let dir = default_config_dir();
    if !dir.exists() {
        std::fs::create_dir_all(&dir)?;
    }
    Ok(dir)
}
