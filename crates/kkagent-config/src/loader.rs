use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::AppConfig;

/// Process-global redirect of the kkagent home directory. Test-only escape
/// hatch: once set it cannot be reset, so a stray call in production code
/// would be obvious (and `#[doc(hidden)]` keeps it out of the public surface).
static CONFIG_DIR_OVERRIDE: OnceLock<PathBuf> = OnceLock::new();

/// Values injected from the last workspace `.env` load. Tracking the previous
/// injected value lets `/reload` refresh changed entries and remove deleted
/// ones without overwriting a value that another part of the process
/// deliberately changed in the meantime.
static WORKSPACE_DOTENV_STATE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

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

/// Verify the kkagent home path is usable: it must either not exist yet (it
/// will be created on demand) or be a directory. A regular file named
/// `.kkagent` (or a `KKAGENT_HOME` pointing at a file) makes every later
/// write fail with obscure `File exists` / `Not a directory` OS errors, so we
/// detect it up front and tell the user how to fix it.
pub fn validate_config_dir() -> Result<()> {
    validate_config_dir_at(&default_config_dir())
}

fn validate_config_dir_at(dir: &Path) -> Result<()> {
    let meta = match std::fs::symlink_metadata(dir) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(anyhow::Error::new(error)
                .context(format!("cannot inspect kkagent home {}", dir.display())));
        }
    };
    // Follow symlinks: a symlink to a real directory is fine; a dangling one
    // or one pointing at a file is reported like any other non-directory.
    let is_dir = if meta.file_type().is_symlink() {
        std::fs::metadata(dir)
            .map(|followed| followed.is_dir())
            .unwrap_or(false)
    } else {
        meta.is_dir()
    };
    if is_dir {
        return Ok(());
    }
    let kind = if meta.file_type().is_symlink() {
        "a symlink that does not point to a directory"
    } else {
        "a regular file"
    };
    let mut hints = vec![format!(
        "  - move the file out of the way, e.g.:  mv {} {}.bak",
        dir.display(),
        dir.display()
    )];
    if std::env::var_os("KKAGENT_HOME").is_some() {
        hints.insert(
            0,
            "  - fix or unset KKAGENT_HOME so it points to a directory".to_string(),
        );
    }
    hints
        .push("  - or relocate the kkagent home:  export KKAGENT_HOME=~/.kkagent-home".to_string());
    anyhow::bail!(
        "kkagent home {} is {}, not a directory.\n\
         kkagent stores its config, sessions, and logs under this directory and cannot run \
         while the path is occupied by a file.\n\
         Fix options:\n{}",
        dir.display(),
        kind,
        hints.join("\n")
    );
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

    if config_path.is_dir() {
        anyhow::bail!(
            "config path {} is a directory — pass the config file itself (e.g. ~/.kkagent/config.toml), not the directory",
            config_path.display()
        );
    }

    let mut config = if !config_path.exists() {
        tracing::info!("Config file not found at {:?}, using defaults", config_path);
        AppConfig::default()
    } else {
        // Bring the stored config up to the current schema before parsing.
        // Migration is lossless (toml_edit) and backs up the original; a
        // failure here must never block startup — the regular parse below
        // will surface real problems.
        if let Err(error) = crate::migrate::migrate_config_file(&config_path) {
            tracing::warn!("config migration skipped: {error:#}");
        }
        if let Err(error) =
            crate::migrate::migrate_plugin_manifests(&default_config_dir().join("plugins"))
        {
            tracing::warn!("plugin manifest migration skipped: {error:#}");
        }
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
/// overriding variables supplied by the parent process. Values previously
/// injected by this function are refreshed on subsequent loads, so `/reload`
/// observes edits and removals instead of retaining stale workspace secrets.
pub fn load_workspace_dotenv() -> Result<Option<PathBuf>> {
    let path = std::env::current_dir()?.join(".env");
    load_workspace_dotenv_at(&path)
}

fn load_workspace_dotenv_at(path: &Path) -> Result<Option<PathBuf>> {
    let mut parsed = HashMap::new();
    if path.is_file() {
        // Older kkagent workspaces sometimes used `.env` as a TOML config file.
        // Keep accepting `--config .env` without trying to interpret TOML tables as
        // dotenv declarations.
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read workspace environment: {path:?}"))?;
        if content
            .lines()
            .map(str::trim)
            .any(|line| line.starts_with('['))
        {
            refresh_workspace_dotenv(HashMap::new());
            return Ok(Some(path.to_path_buf()));
        }
        for item in dotenvy::from_read_iter(content.as_bytes()) {
            let (name, value) = item?;
            if recognized_env_key(&name) {
                parsed.insert(name, value);
            }
        }
    }

    refresh_workspace_dotenv(parsed);
    Ok(path.is_file().then(|| path.to_path_buf()))
}

fn refresh_workspace_dotenv(parsed: HashMap<String, String>) {
    let state = WORKSPACE_DOTENV_STATE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut state = state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    for (name, previous) in std::mem::take(&mut *state) {
        if std::env::var_os(&name).as_deref() == Some(previous.as_ref()) {
            std::env::remove_var(&name);
        }
    }

    for (name, value) in parsed {
        if std::env::var_os(&name).is_none() {
            std::env::set_var(&name, &value);
            state.insert(name, value);
        }
    }
}

fn recognized_env_key(name: &str) -> bool {
    #[cfg(test)]
    if name.starts_with("KKAGENT_TEST_DOTENV_") {
        return true;
    }

    matches!(
        name,
        "KKAGENT_DEFAULT_MODEL"
            | "KKAGENT_QUALITY_MODEL"
            | "KKAGENT_BALANCE_MODEL"
            | "KKAGENT_COMPACTION_MODEL"
            | "KKAGENT_IMAGE_MAX_EDGE_PX"
            | "KKAGENT_IMAGE_READ_BYTE_BUDGET"
            | "KKAGENT_WEB_SEARCH_URL"
            | "KKAGENT_WEB_SEARCH_KEY"
            | "KKAGENT_WEB_SEARCH_PROVIDER"
            | "KKAGENT_MOONSHOT_SEARCH_URL"
            | "KKAGENT_MOONSHOT_SEARCH_KEY"
            | "ANTHROPIC_API_KEY"
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
    if let Ok(v) = std::env::var("KKAGENT_QUALITY_MODEL") {
        config.quality_model = Some(v);
    }
    if let Ok(v) = std::env::var("KKAGENT_BALANCE_MODEL") {
        config.balance_model = Some(v);
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
    for (name, p) in config.providers.iter_mut() {
        for key in p.extra_fields.keys() {
            tracing::warn!(
                "providers.{name}: unknown configuration key {key:?} — likely a typo, or the \
                 key belongs to the next TOML table (keys are assigned to the nearest \
                 preceding table header)"
            );
        }
        if let Some(env_name) = p.api_key_env.as_deref() {
            match std::env::var(env_name) {
                Ok(v) => {
                    let v = v.trim();
                    if v.is_empty() {
                        tracing::warn!(
                            "providers.{name}: api_key_env={env_name:?} is set but empty; \
                             falling back to the inline api_key"
                        );
                    } else {
                        p.api_key = Some(v.to_string());
                    }
                }
                Err(std::env::VarError::NotPresent) => {
                    tracing::warn!(
                        "providers.{name}: api_key_env={env_name:?} is not set in the \
                         environment; falling back to the inline api_key"
                    );
                }
                Err(std::env::VarError::NotUnicode(_)) => {
                    tracing::warn!(
                        "providers.{name}: api_key_env={env_name:?} holds a non-UTF-8 value; \
                         falling back to the inline api_key"
                    );
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
    // Fail with an actionable message when e.g. `~/.kkagent` is a regular
    // file, instead of a cryptic `File exists` from `create_dir_all`.
    validate_config_dir()?;
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
mod workspace_dotenv_tests {
    use super::*;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn test_guard() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "kkagent-dotenv-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ))
    }

    fn unique_env(tag: &str) -> String {
        format!(
            "KKAGENT_TEST_DOTENV_{tag}_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        )
    }

    #[test]
    fn reload_refreshes_and_removes_injected_values() {
        let _guard = test_guard();
        let root = temp_path("refresh");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join(".env");
        let name = unique_env("REFRESH");

        std::fs::write(&path, format!("{name}=first\n")).unwrap();
        load_workspace_dotenv_at(&path).unwrap();
        assert_eq!(std::env::var(&name).as_deref(), Ok("first"));

        std::fs::write(&path, format!("{name}=second\n")).unwrap();
        load_workspace_dotenv_at(&path).unwrap();
        assert_eq!(std::env::var(&name).as_deref(), Ok("second"));

        std::fs::remove_file(&path).unwrap();
        assert!(load_workspace_dotenv_at(&path).unwrap().is_none());
        assert!(std::env::var_os(&name).is_none());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn workspace_dotenv_rejects_process_level_security_overrides() {
        let _guard = test_guard();
        let root = temp_path("security");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join(".env");
        let forbidden = [
            "KKAGENT_HOME",
            "KKAGENT_HTTP_TOKEN",
            "KKAGENT_PERMISSION_MODE",
            "KKAGENT_ALLOW_IN_MEMORY_TRANSCRIPTS",
            "KKAGENT_PLUGIN_MARKETPLACE_URL",
        ];
        for name in forbidden {
            std::env::remove_var(name);
        }
        std::fs::write(
            &path,
            forbidden
                .iter()
                .map(|name| format!("{name}=workspace-controlled"))
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        load_workspace_dotenv_at(&path).unwrap();
        for name in forbidden {
            assert!(
                std::env::var_os(name).is_none(),
                "workspace .env unexpectedly set {name}"
            );
        }

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn parent_environment_keeps_precedence() {
        let _guard = test_guard();
        let root = temp_path("parent");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join(".env");
        let name = unique_env("PARENT");
        std::env::set_var(&name, "from-parent");

        std::fs::write(&path, format!("{name}=from-dotenv\n")).unwrap();
        load_workspace_dotenv_at(&path).unwrap();
        assert_eq!(std::env::var(&name).as_deref(), Ok("from-parent"));

        std::env::remove_var(&name);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn reload_does_not_undo_external_runtime_change() {
        let _guard = test_guard();
        let root = temp_path("runtime");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join(".env");
        let name = unique_env("RUNTIME");

        std::fs::write(&path, format!("{name}=from-dotenv\n")).unwrap();
        load_workspace_dotenv_at(&path).unwrap();
        std::env::set_var(&name, "changed-elsewhere");

        std::fs::remove_file(&path).unwrap();
        load_workspace_dotenv_at(&path).unwrap();
        assert_eq!(std::env::var(&name).as_deref(), Ok("changed-elsewhere"));

        std::env::remove_var(&name);
        let _ = std::fs::remove_dir_all(root);
    }
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
mod validate_config_dir_tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kkagent-validate-{}-{}",
            tag,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_path_is_ok() {
        let root = temp_root("missing");
        let target = root.join(".kkagent");
        assert!(validate_config_dir_at(&target).is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn directory_is_ok() {
        let root = temp_root("dir");
        let target = root.join(".kkagent");
        std::fs::create_dir_all(&target).unwrap();
        assert!(validate_config_dir_at(&target).is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The customer-reported case: `~/.kkagent` exists but is a regular file.
    #[test]
    fn regular_file_is_rejected_with_actionable_error() {
        let root = temp_root("file");
        let target = root.join(".kkagent");
        std::fs::write(&target, "stray file").unwrap();

        let err = validate_config_dir_at(&target).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("not a directory"), "message: {msg}");
        assert!(
            msg.contains(target.display().to_string().as_str()),
            "message: {msg}"
        );
        // Must tell the user how to fix it, not just that it failed.
        assert!(msg.contains("mv"), "message: {msg}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_to_directory_is_ok() {
        let root = temp_root("symlink-dir");
        let real = root.join("real-home");
        std::fs::create_dir_all(&real).unwrap();
        let link = root.join(".kkagent");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(validate_config_dir_at(&link).is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_to_file_is_rejected() {
        let root = temp_root("symlink-file");
        let file = root.join("stray.txt");
        std::fs::write(&file, "x").unwrap();
        let link = root.join(".kkagent");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert!(validate_config_dir_at(&link).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn dangling_symlink_is_rejected() {
        let root = temp_root("symlink-dangling");
        let link = root.join(".kkagent");
        std::os::unix::fs::symlink(root.join("does-not-exist"), &link).unwrap();
        assert!(validate_config_dir_at(&link).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn load_config_rejects_directory_path() {
        let root = temp_root("load-dir");
        let err = load_config(Some(&root)).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("is a directory"),
            "unexpected error message: {msg}"
        );
        let _ = std::fs::remove_dir_all(&root);
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
                extra_fields: Default::default(),
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

    /// Unknown keys inside a `[providers.*]` table are captured into
    /// `extra_fields` instead of being silently dropped, so typos and keys
    /// misplaced under the wrong TOML table header become visible.
    #[test]
    fn unknown_provider_key_is_captured() {
        let raw = r#"
[providers.oai]
type = "openai"
api_key = "k"
api_key_envx = "OAI_API_KEY"
"#;
        let config: AppConfig = toml::from_str(raw).unwrap();
        let provider = &config.providers["oai"];
        assert!(provider.api_key_env.is_none());
        assert_eq!(
            provider.extra_fields.get("api_key_envx"),
            Some(&toml::Value::String("OAI_API_KEY".into()))
        );
    }

    /// A key physically placed after the *next* table header belongs to that
    /// table — TOML semantics — and the provider must not see it.
    #[test]
    fn key_after_next_table_header_is_not_provider_field() {
        let raw = r#"
[providers.oai]
type = "openai"
api_key = "k"

[models."oai/mini"]
provider = "oai"
model = "mini"
api_key_env = "OAI_API_KEY"
"#;
        let config: AppConfig = toml::from_str(raw).unwrap();
        assert!(config.providers["oai"].api_key_env.is_none());
        assert!(config.providers["oai"].extra_fields.is_empty());
    }
}
