use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
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

/// Sidecar for TUI-managed enable/disable toggles (keeps main config.toml stable).
pub fn disabled_state_path() -> PathBuf {
    default_config_dir().join("disabled.toml")
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DisabledState {
    #[serde(default)]
    pub disabled_skills: Vec<String>,
    #[serde(default)]
    pub disabled_mcp_servers: Vec<String>,
}

impl DisabledState {
    pub fn load() -> Result<Self> {
        let path = disabled_state_path();
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read disabled state: {path:?}"))?;
        toml::from_str(&content)
            .with_context(|| format!("Failed to parse disabled state: {path:?}"))
    }

    pub fn save(&self) -> Result<()> {
        ensure_config_dir()?;
        let path = disabled_state_path();
        let body = toml::to_string_pretty(self)
            .with_context(|| format!("Failed to serialize disabled state: {path:?}"))?;
        let header = "# Managed by kkagent /skills and /mcp TUI pickers.\n";
        std::fs::write(&path, format!("{header}{body}"))
            .with_context(|| format!("Failed to write disabled state: {path:?}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    pub fn apply_to_config(&self, config: &mut AppConfig) {
        for name in &self.disabled_skills {
            config.set_skill_disabled(name, true);
        }
        for name in &self.disabled_mcp_servers {
            config.set_mcp_disabled(name, true);
        }
    }

    pub fn from_config(config: &AppConfig) -> Self {
        Self {
            disabled_skills: config.disabled_skills.clone(),
            disabled_mcp_servers: config.disabled_mcp_servers.clone(),
        }
    }
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

    // Merge TUI-managed disables (sidecar wins for skill names; MCP merges).
    if let Ok(disabled) = DisabledState::load() {
        disabled.apply_to_config(&mut config);
    }
    // Seed disables from per-server `enabled = false` in config.toml.
    let seeded: Vec<String> = config
        .mcp_servers
        .iter()
        .filter(|(_, s)| s.enabled == Some(false))
        .map(|(name, _)| name.clone())
        .collect();
    for name in seeded {
        config.set_mcp_disabled(&name, true);
    }

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
