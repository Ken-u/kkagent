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
    let _ = load_workspace_dotenv()?;
    let config_path = match path {
        Some(p) => p.to_path_buf(),
        None => default_config_path(),
    };

    let mut config = if !config_path.exists() {
        tracing::info!("Config file not found at {:?}, using defaults", config_path);
        AppConfig::default()
    } else {
        let content = std::fs::read_to_string(&config_path)
            .with_context(|| format!("Failed to read config: {config_path:?}"))?;
        toml::from_str(&content)
            .with_context(|| format!("Failed to parse config: {config_path:?}"))?
    };

    apply_env_overrides(&mut config);
    config
        .validate()
        .with_context(|| format!("Invalid kkagent configuration: {config_path:?}"))?;
    Ok(config)
}

/// Load only kkagent/model-provider variables from `<cwd>/.env` without
/// overriding variables already supplied by the parent process.
pub fn load_workspace_dotenv() -> Result<Option<PathBuf>> {
    let path = std::env::current_dir()?.join(".env");
    if !path.is_file() {
        return Ok(None);
    }
    // Older kkagent workspaces sometimes used `.env` as a TOML config file.
    // Keep accepting `--config .env` without trying to interpret TOML tables as
    // dotenv declarations.
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read workspace environment: {path:?}"))?;
    if content
        .lines()
        .map(str::trim)
        .any(|line| line.starts_with('['))
    {
        return Ok(Some(path));
    }
    for item in dotenvy::from_read_iter(content.as_bytes()) {
        let (name, value) = item?;
        if recognized_env_key(&name) && std::env::var_os(&name).is_none() {
            std::env::set_var(name, value);
        }
    }
    Ok(Some(path))
}

fn recognized_env_key(name: &str) -> bool {
    name.starts_with("KKAGENT_")
        || matches!(
            name,
            "ANTHROPIC_API_KEY"
                | "OPENAI_API_KEY"
                | "KIMI_API_KEY"
                | "KIMI_IMAGE_MAX_EDGE_PX"
                | "KIMI_IMAGE_READ_BYTE_BUDGET"
                | "GOOGLE_API_KEY"
                | "MOONSHOT_API_KEY"
        )
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
    if let Some(value) = env_positive_u32("KKAGENT_IMAGE_MAX_EDGE_PX")
        .or_else(|| env_positive_u32("KIMI_IMAGE_MAX_EDGE_PX"))
    {
        config.image.max_edge_px = value;
    }
    if let Some(value) = env_positive_usize("KKAGENT_IMAGE_READ_BYTE_BUDGET")
        .or_else(|| env_positive_usize("KIMI_IMAGE_READ_BYTE_BUDGET"))
    {
        config.image.read_byte_budget = value;
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
            if matches!(
                p.provider_type.as_str(),
                "openai" | "openai-responses" | "openai_responses"
            ) && p.api_key.as_ref().map(|s| s.is_empty()).unwrap_or(true)
            {
                p.api_key = Some(key.clone());
            }
        }
    }
    if let Ok(key) = std::env::var("KIMI_API_KEY") {
        for p in config.providers.values_mut() {
            if p.provider_type == "kimi" && p.api_key.as_ref().map(|s| s.is_empty()).unwrap_or(true)
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

fn env_positive_u32(name: &str) -> Option<u32> {
    std::env::var(name)
        .ok()?
        .parse()
        .ok()
        .filter(|value| *value > 0)
}

fn env_positive_usize(name: &str) -> Option<usize> {
    std::env::var(name)
        .ok()?
        .parse()
        .ok()
        .filter(|value| *value > 0)
}

pub fn ensure_config_dir() -> Result<PathBuf> {
    let dir = default_config_dir();
    if !dir.exists() {
        std::fs::create_dir_all(&dir)?;
    }
    Ok(dir)
}
