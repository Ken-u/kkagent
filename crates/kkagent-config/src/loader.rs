use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::AppConfig;

/// Process-global redirect of the kkagent home directory. Test-only escape
/// hatch: once set it cannot be reset, so a stray call in production code
/// would be obvious (and `#[doc(hidden)]` keeps it out of the public surface).
static CONFIG_DIR_OVERRIDE: OnceLock<PathBuf> = OnceLock::new();

/// Redirect the default config/session home for the current process.
/// Used by test harnesses to keep `cargo test` from touching the real
/// `~/.kkagent` store. First call wins; subsequent calls are ignored.
#[doc(hidden)]
pub fn set_default_config_dir_override(dir: PathBuf) {
    let _ = CONFIG_DIR_OVERRIDE.set(dir);
}

/// Resolve the kkagent home directory.
///
/// Precedence: in-process override (tests) → `KKAGENT_HOME` env var →
/// `~/.kkagent`. `KKAGENT_HOME` makes the whole home (config sidecars,
/// sessions store, skills, transcript DB) relocatable — useful for portable
/// installs and CI.
pub fn default_config_dir() -> PathBuf {
    if let Some(dir) = CONFIG_DIR_OVERRIDE.get() {
        return dir.clone();
    }
    if let Some(home) = std::env::var_os("KKAGENT_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
    {
        return home;
    }
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

/// Sidecar for durable "always allow" approval decisions. Rewriting the main
/// config.toml from the agent would destroy user comments/formatting, so — like
/// `disabled.toml` — persisted approvals live in a managed file and are merged
/// into the permission chain at session start.
pub fn permission_sidecar_path() -> PathBuf {
    default_config_dir().join("permissions.toml")
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PersistedApprovals {
    /// Durable "always allow" rules written by `record_always_approval`.
    #[serde(default)]
    pub rules: Vec<crate::PermissionRule>,
}

impl PersistedApprovals {
    pub fn load() -> Result<Self> {
        let path = permission_sidecar_path();
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read persisted approvals: {path:?}"))?;
        toml::from_str(&content)
            .with_context(|| format!("Failed to parse persisted approvals: {path:?}"))
    }

    pub fn save(&self) -> Result<()> {
        ensure_config_dir()?;
        let path = permission_sidecar_path();
        let body = toml::to_string_pretty(self)
            .with_context(|| format!("Failed to serialize persisted approvals: {path:?}"))?;
        let header =
            "# Managed by kkagent \"always allow\" approval choices.\n# Safe to delete; approvals then revert to asking.\n";
        std::fs::write(&path, format!("{header}{body}"))
            .with_context(|| format!("Failed to write persisted approvals: {path:?}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    #[cfg(test)]
    fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&content)?)
    }

    #[cfg(test)]
    fn save_to(&self, path: &Path) -> Result<()> {
        let body = toml::to_string_pretty(self)?;
        std::fs::write(path, body)?;
        Ok(())
    }

    /// Append an allow rule, deduplicating identical patterns.
    pub fn upsert(&mut self, rule: crate::PermissionRule) {
        if !self
            .rules
            .iter()
            .any(|r| r.decision == rule.decision && r.pattern == rule.pattern)
        {
            self.rules.push(rule);
        }
    }
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

    config.workspace_trust = crate::WorkspaceTrustStore::load(&config_path)?;

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
    // Project-level `.kkagent/config.toml` can override heavy-dir skips for
    // the current working directory (AOSP / monorepo tuning).
    if let Ok(cwd) = std::env::current_dir() {
        config.tools.merge_project_overrides(&cwd);
    }
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
    if let Ok(v) = std::env::var("KKAGENT_COMPACTION_MODEL") {
        config.compaction_model = Some(v);
    }
    if let Ok(v) = std::env::var("KKAGENT_PERMISSION_MODE") {
        config.default_permission_mode = Some(v);
    }
    if let Ok(v) = std::env::var("KKAGENT_PLUGIN_MARKETPLACE_URL") {
        config.plugin_marketplace = Some(v);
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
    // Resolve `api_key_env` first — when set and the environment variable is
    // present (non-empty), it takes precedence over the inline `api_key`,
    // matching the behavior of web search/fetch services.
    for p in config.providers.values_mut() {
        if let Some(name) = p.api_key_env.as_deref() {
            if let Ok(v) = std::env::var(name) {
                let v = v.trim();
                if !v.is_empty() {
                    p.api_key = Some(v.to_string());
                }
            }
        }
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
    if let Ok(url) = std::env::var("KKAGENT_WEB_SEARCH_URL") {
        let key = std::env::var("KKAGENT_WEB_SEARCH_KEY")
            .ok()
            .or_else(|| std::env::var("KKAGENT_MOONSHOT_SEARCH_KEY").ok())
            .or_else(|| std::env::var("MOONSHOT_API_KEY").ok());
        let provider = std::env::var("KKAGENT_WEB_SEARCH_PROVIDER").ok();
        let services = config.services.get_or_insert_with(Default::default);
        services.web_search = Some(crate::WebSearchConfig {
            provider,
            base_url: url,
            api_key: key,
            api_key_env: None,
            timeout_ms: None,
            default_limit: None,
            proxy: Default::default(),
        });
    } else if let Ok(url) = std::env::var("KKAGENT_MOONSHOT_SEARCH_URL") {
        // Legacy env — still maps to deprecated moonshot_search for one-time compat.
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

/// Default local IPC endpoint for the standalone server.
pub fn default_server_socket_path() -> PathBuf {
    default_config_dir().join("server.sock")
}

fn active_session_path() -> PathBuf {
    default_config_dir().join("active-session")
}

fn write_active_session_file(path: &Path, session_id: &str) -> Result<()> {
    let trimmed = session_id.trim();
    if trimmed.is_empty() {
        anyhow::bail!("active session id must not be empty");
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, trimmed).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn read_active_session_file(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn remove_active_session_file(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "failed to clear active-session");
        }
    }
}

/// Persist the session id that `kk` should auto-resume after a TUI detach.
pub fn save_active_session(session_id: &str) -> Result<()> {
    ensure_config_dir()?;
    write_active_session_file(&active_session_path(), session_id)
}

/// Load the last detached session id, if any.
pub fn load_active_session() -> Option<String> {
    read_active_session_file(&active_session_path())
}

/// Clear a stale or intentionally discarded active-session marker.
pub fn clear_active_session() {
    remove_active_session_file(&active_session_path())
}

#[cfg(test)]
mod active_session_tests {
    use super::*;

    #[test]
    fn save_load_clear_active_session_roundtrip() {
        let dir = std::env::temp_dir().join(format!(
            "kkagent-active-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("active-session");

        assert!(read_active_session_file(&path).is_none());
        write_active_session_file(&path, "  sess-123  ").unwrap();
        assert_eq!(read_active_session_file(&path).as_deref(), Some("sess-123"));
        remove_active_session_file(&path);
        assert!(read_active_session_file(&path).is_none());
        remove_active_session_file(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_empty_active_session_id() {
        let path = std::env::temp_dir().join(format!(
            "kkagent-active-empty-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        assert!(write_active_session_file(&path, "   ").is_err());
    }
}

#[cfg(test)]
mod persisted_approval_tests {
    use super::*;

    #[test]
    fn upsert_dedupes_and_roundtrips() {
        let dir =
            std::env::temp_dir().join(format!("kkagent-perm-{:?}", std::time::SystemTime::now()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("permissions.toml");

        let mut a = PersistedApprovals::default();
        let rule = crate::PermissionRule {
            decision: "allow".into(),
            pattern: "Bash:git push".into(),
            scope: Some("always".into()),
        };
        a.upsert(rule.clone());
        a.upsert(rule.clone()); // duplicate ignored
        assert_eq!(a.rules.len(), 1);
        a.save_to(&path).unwrap();

        let b = PersistedApprovals::load_from(&path).unwrap();
        assert_eq!(b.rules.len(), 1);
        assert_eq!(b.rules[0].pattern, "Bash:git push");
        assert_eq!(b.rules[0].decision, "allow");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

#[cfg(test)]
mod api_key_env_tests {
    use super::*;
    use crate::schema::{AppConfig, ProviderConfig};
    use std::collections::HashMap;

    fn make_config(api_key: Option<&str>, api_key_env: Option<&str>) -> AppConfig {
        let mut providers = HashMap::new();
        providers.insert(
            "test".to_string(),
            ProviderConfig {
                provider_type: "openai".to_string(),
                api_key: api_key.map(str::to_string),
                api_key_env: api_key_env.map(str::to_string),
                base_url: None,
                custom_headers: HashMap::new(),
                oauth: None,
                first_token_timeout_ms: None,
            },
        );
        AppConfig {
            providers,
            ..Default::default()
        }
    }

    /// `api_key_env` wins over inline `api_key` when the variable is set.
    #[test]
    fn env_var_takes_precedence_over_inline() {
        let name = format!(
            "KKAGENT_TEST_KEY_ENV_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        std::env::set_var(&name, "secret-from-env");

        let mut config = make_config(Some("inline-secret"), Some(&name));
        apply_env_overrides(&mut config);

        assert_eq!(
            config.providers["test"].api_key.as_deref(),
            Some("secret-from-env")
        );

        std::env::remove_var(&name);
    }

    /// Falls back to inline `api_key` when the env var is unset.
    #[test]
    fn falls_back_to_inline_when_env_absent() {
        let mut config = make_config(Some("inline-secret"), Some("KKAGENT_TEST_KEY_NEVER_SET_42"));
        apply_env_overrides(&mut config);

        assert_eq!(
            config.providers["test"].api_key.as_deref(),
            Some("inline-secret")
        );
    }

    /// Empty env var value is ignored, falling back to inline.
    #[test]
    fn empty_env_value_falls_back_to_inline() {
        let name = format!(
            "KKAGENT_TEST_KEY_EMPTY_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        std::env::set_var(&name, "   ");

        let mut config = make_config(Some("inline-secret"), Some(&name));
        apply_env_overrides(&mut config);

        assert_eq!(
            config.providers["test"].api_key.as_deref(),
            Some("inline-secret")
        );

        std::env::remove_var(&name);
    }
}
