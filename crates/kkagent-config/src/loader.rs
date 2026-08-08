use std::path::{Path, PathBuf};
use anyhow::{Context, Result};

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

    if !config_path.exists() {
        tracing::info!("Config file not found at {:?}, using defaults", config_path);
        return Ok(AppConfig::default());
    }

    let content = std::fs::read_to_string(&config_path)
        .with_context(|| format!("Failed to read config: {:?}", config_path))?;

    let config: AppConfig = toml::from_str(&content)
        .with_context(|| format!("Failed to parse config: {:?}", config_path))?;

    Ok(config)
}

pub fn ensure_config_dir() -> Result<PathBuf> {
    let dir = default_config_dir();
    if !dir.exists() {
        std::fs::create_dir_all(&dir)?;
    }
    Ok(dir)
}
