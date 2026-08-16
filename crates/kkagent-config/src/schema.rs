use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::toolchain::ToolchainConfig;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppConfig {
    #[serde(default)]
    pub default_model: Option<String>,
    /// Global fallback model alias used after the primary model exhausts its
    /// normal per-step retry budget.
    #[serde(default)]
    pub fallback_model: Option<String>,
    /// Optional secondary model alias for subagents / summarization.
    #[serde(default)]
    pub secondary_model: Option<String>,
    #[serde(default)]
    pub default_permission_mode: Option<String>,
    #[serde(default)]
    pub default_plan_mode: bool,
    #[serde(default)]
    pub merge_all_available_skills: bool,
    #[serde(default)]
    pub extra_skill_dirs: Vec<String>,
    /// Skill names permanently disabled via TUI / config (not offered to the model).
    #[serde(default)]
    pub disabled_skills: Vec<String>,
    /// MCP server names permanently disabled via TUI / config.
    /// Also honored when `[mcp_servers.X] enabled = false`.
    #[serde(default)]
    pub disabled_mcp_servers: Vec<String>,
    /// Optional local path, file URL, or HTTP(S) URL for the KK plugin
    /// marketplace catalog.
    #[serde(default)]
    pub plugin_marketplace: Option<String>,
    #[serde(default)]
    pub telemetry: bool,
    /// Trusted workspace roots (absolute paths). Empty = trust cwd implicitly.
    #[serde(default)]
    pub trusted_workspaces: Vec<String>,
    /// Runtime workspace grants loaded from the config-adjacent trust sidecar.
    #[serde(skip)]
    pub workspace_trust: crate::WorkspaceTrustStore,
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
    #[serde(default)]
    pub models: HashMap<String, ModelConfig>,
    #[serde(default)]
    pub thinking: Option<ThinkingConfig>,
    #[serde(default)]
    pub loop_control: Option<LoopControlConfig>,
    /// Image normalization limits shared by every multimodal input path.
    #[serde(default)]
    pub image: ImageConfig,
    #[serde(default)]
    pub background: Option<BackgroundConfig>,
    #[serde(default)]
    pub sandbox: SandboxConfig,
    /// Declarative language toolchain sandbox profiles (`[toolchain]`).
    #[serde(default)]
    pub toolchain: ToolchainConfig,
    #[serde(default)]
    pub permission: Option<PermissionConfig>,
    #[serde(default)]
    pub hooks: Vec<HookConfig>,
    #[serde(default)]
    pub services: Option<ServicesConfig>,
    #[serde(default)]
    pub mcp_servers: HashMap<String, McpServerConfig>,
    /// TUI / accessibility / update preferences.
    #[serde(default)]
    pub ui: UiConfig,
    /// Standalone server lifecycle (idle exit, default detach mode).
    #[serde(default)]
    pub server: ServerConfig,
    /// Application-layer path-policy and sensitive-file settings.
    #[serde(default)]
    pub tools: ToolsConfig,
    /// Optional SSH remote execution target (`[remote]`).
    #[serde(default)]
    pub remote: RemoteConfig,
}

/// Optional SSH remote environment (`[remote]` in config.toml).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RemoteConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub host: String,
    #[serde(default = "default_ssh_port")]
    pub port: u16,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub identity_file: Option<String>,
    #[serde(default)]
    pub remote_cwd: Option<String>,
}

fn default_ssh_port() -> u16 {
    22
}

/// Standalone RPC server preferences (`[server]` in config.toml).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    /// Seconds with no clients and no active turns before the standalone server
    /// exits. `0` disables automatic exit (requires `kkagent server stop`).
    #[serde(default = "default_idle_timeout_secs")]
    pub idle_timeout_secs: u64,
    /// When true, `kk` auto-spawns/connects a standalone server (Ctrl+B works).
    /// When false, use the legacy in-process server (Ctrl+B unavailable).
    #[serde(default = "default_true")]
    pub standalone: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            idle_timeout_secs: default_idle_timeout_secs(),
            standalone: true,
        }
    }
}

fn default_idle_timeout_secs() -> u64 {
    1800
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiConfig {
    /// High-contrast theme preference (text/symbols over color alone).
    #[serde(default)]
    pub high_contrast: bool,
    /// Reduce spinner / animation updates.
    #[serde(default)]
    pub reduce_motion: bool,
    /// Check crates.io / release notes when idle (cached).
    #[serde(default = "default_true")]
    pub check_updates: bool,
    /// Optional key override map: action name → key chord (e.g. "interrupt" = "ctrl-c").
    #[serde(default)]
    pub keybindings: HashMap<String, String>,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            high_contrast: false,
            reduce_motion: false,
            check_updates: true,
            keybindings: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageConfig {
    #[serde(default = "default_image_max_edge_px")]
    pub max_edge_px: u32,
    #[serde(default = "default_image_read_byte_budget")]
    pub read_byte_budget: usize,
}

impl Default for ImageConfig {
    fn default() -> Self {
        Self {
            max_edge_px: default_image_max_edge_px(),
            read_byte_budget: default_image_read_byte_budget(),
        }
    }
}

fn default_image_max_edge_px() -> u32 {
    2000
}

fn default_image_read_byte_budget() -> usize {
    256 * 1024
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    #[serde(rename = "type")]
    pub provider_type: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub custom_headers: HashMap<String, String>,
    #[serde(default)]
    pub oauth: Option<ProviderOAuthConfig>,
    /// Provider-level default for streaming first-token timeout (milliseconds).
    /// Model-level config wins; `0` disables. See [`resolve_first_token_timeout`].
    #[serde(default)]
    pub first_token_timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderOAuthConfig {
    #[serde(default = "default_oauth_storage")]
    pub storage: String,
    #[serde(default = "default_kimi_oauth_key")]
    pub key: String,
    #[serde(default)]
    pub oauth_host: Option<String>,
}

fn default_oauth_storage() -> String {
    "file".into()
}

fn default_kimi_oauth_key() -> String {
    "kimi-code".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub max_context_size: Option<u64>,
    #[serde(default)]
    pub max_output_size: Option<u64>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub support_efforts: Vec<String>,
    #[serde(default)]
    pub default_effort: Option<String>,
    /// Optional USD pricing per 1M tokens for `/usage` estimates.
    #[serde(default)]
    pub pricing: Option<ModelPricing>,
    /// Experimental: use Anthropic adaptive thinking and forward configured effort.
    #[serde(default)]
    pub experimental_adaptive_thinking: bool,
    /// Experimental: retry a thinking-only/empty response immediately after tool results.
    #[serde(default)]
    pub experimental_visible_empty_retries: u32,
    /// Wait this many milliseconds for the first meaningful stream chunk.
    /// `0` disables; unset inherits provider / default (60s). See [`resolve_first_token_timeout`].
    #[serde(default)]
    pub first_token_timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelPricing {
    #[serde(default)]
    pub input_per_mtok: Option<f64>,
    #[serde(default)]
    pub output_per_mtok: Option<f64>,
    #[serde(default)]
    pub cache_creation_per_mtok: Option<f64>,
    #[serde(default)]
    pub cache_read_per_mtok: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThinkingConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub keep: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopControlConfig {
    #[serde(default = "default_max_attempts")]
    pub max_attempts_per_step: u32,
    /// Base delay for 429 responses without a server-provided retry hint.
    #[serde(default = "default_rate_limit_retry_base_seconds")]
    pub rate_limit_retry_base_seconds: u64,
    #[serde(default = "default_reserved_context")]
    pub reserved_context_size: u64,
    #[serde(default = "default_max_steps")]
    pub max_steps_per_turn: u32,
    /// Auto-compact when request estimate exceeds usable context.
    #[serde(default = "default_true")]
    pub auto_compact: bool,
    /// Messages to keep when auto-compacting (legacy KeepTail strategy).
    #[serde(default = "default_compact_keep")]
    pub compact_keep_last: u32,
    /// Fraction of max context that triggers auto-compaction (kimi default 0.85).
    #[serde(default)]
    pub compact_trigger_ratio: Option<f64>,
    /// Fraction of max context that blocks the turn on compaction (kimi default 0.85).
    #[serde(default)]
    pub compact_block_ratio: Option<f64>,
    /// Max overflow→compact→overflow loops per turn before failing.
    #[serde(default)]
    pub compact_max_overflow_attempts: Option<u32>,
    /// Token counting strategy: measured+estimated | measured | estimated
    #[serde(default = "default_token_strategy")]
    pub token_counting: String,
}

impl Default for LoopControlConfig {
    fn default() -> Self {
        Self {
            max_attempts_per_step: default_max_attempts(),
            rate_limit_retry_base_seconds: default_rate_limit_retry_base_seconds(),
            reserved_context_size: default_reserved_context(),
            max_steps_per_turn: default_max_steps(),
            auto_compact: true,
            compact_keep_last: default_compact_keep(),
            compact_trigger_ratio: None,
            compact_block_ratio: None,
            compact_max_overflow_attempts: None,
            token_counting: default_token_strategy(),
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_false() -> bool {
    false
}
fn default_compact_keep() -> u32 {
    8
}
fn default_token_strategy() -> String {
    "measured+estimated".into()
}

fn default_max_attempts() -> u32 {
    10
}
fn default_rate_limit_retry_base_seconds() -> u64 {
    5
}
fn default_reserved_context() -> u64 {
    50000
}
fn default_max_steps() -> u32 {
    0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackgroundConfig {
    #[serde(default = "default_max_tasks")]
    pub max_running_tasks: u32,
    #[serde(default)]
    pub keep_alive_on_exit: bool,
    #[serde(default)]
    pub bash_auto_background_on_timeout: Option<bool>,
    #[serde(default)]
    pub bash_task_timeout_s: Option<u64>,
    /// Maximum time a turn may wait for an approval before rejecting it.
    #[serde(default)]
    pub approval_timeout_s: Option<u64>,
}

fn default_max_tasks() -> u32 {
    4
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxConfig {
    /// auto | disabled | process | workspace. Auto is the cross-platform default.
    #[serde(default = "default_sandbox_mode")]
    pub mode: String,
    /// Permit network access from tool processes.
    /// Defaults to `false` (least privilege). Set `true` explicitly when builds need network.
    #[serde(default = "default_false")]
    pub network: bool,
    #[serde(default = "default_sandbox_memory_mb")]
    pub memory_mb: u64,
    #[serde(default = "default_sandbox_cpu_seconds")]
    pub cpu_seconds: u64,
    #[serde(default = "default_sandbox_processes")]
    pub max_processes: u32,
    #[serde(default)]
    pub extra_read_paths: Vec<String>,
    #[serde(default)]
    pub extra_write_paths: Vec<String>,
    /// Escape hatch: allow `extra_*_paths` to include HOME / credential dirs.
    /// Default `false` — opening those paths defeats workspace isolation.
    #[serde(default)]
    pub allow_sensitive_extra_paths: bool,
    /// Extra read-only bind roots for the Linux workspace sandbox (bwrap),
    /// e.g. `/nix/store` on NixOS or a custom toolchain prefix. Paths that do
    /// not exist are skipped. The default system roots are always bound.
    #[serde(default)]
    pub system_read_paths: Vec<String>,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            mode: default_sandbox_mode(),
            network: false,
            memory_mb: default_sandbox_memory_mb(),
            cpu_seconds: default_sandbox_cpu_seconds(),
            max_processes: default_sandbox_processes(),
            extra_read_paths: Vec::new(),
            extra_write_paths: Vec::new(),
            allow_sensitive_extra_paths: false,
            system_read_paths: Vec::new(),
        }
    }
}

impl SandboxConfig {
    /// An explicitly disabled sandbox treats workspace access as unrestricted,
    /// so startup should not gate it on a trust review.
    pub fn is_disabled(&self) -> bool {
        Self::mode_is_disabled(&self.mode)
    }

    pub fn mode_is_disabled(mode: &str) -> bool {
        matches!(
            mode.trim().to_ascii_lowercase().as_str(),
            "disabled" | "off" | "none"
        )
    }

    /// Reject `extra_read_paths` / `extra_write_paths` that open HOME or
    /// credential directories unless `allow_sensitive_extra_paths` is set.
    pub fn validate_extra_paths(&self) -> anyhow::Result<()> {
        if self.allow_sensitive_extra_paths {
            return Ok(());
        }
        let home = dirs::home_dir();
        for (kind, path) in self
            .extra_read_paths
            .iter()
            .map(|p| ("extra_read_paths", p.as_str()))
            .chain(
                self.extra_write_paths
                    .iter()
                    .map(|p| ("extra_write_paths", p.as_str())),
            )
        {
            let expanded = expand_user_path(path);
            if let Some(home) = home.as_ref() {
                if paths_equal_or_same(&expanded, home) {
                    anyhow::bail!(
                        "sandbox.{kind} must not include the user HOME ({}); \
set sandbox.allow_sensitive_extra_paths = true to override (unsafe)",
                        home.display()
                    );
                }
            }
            if is_sensitive_extra_path(&expanded) {
                anyhow::bail!(
                    "sandbox.{kind} includes sensitive path `{}`; \
set sandbox.allow_sensitive_extra_paths = true to override (unsafe)",
                    expanded.display()
                );
            }
        }
        Ok(())
    }
}

/// Expand a leading `~` / `~/` to the user's home directory, leaving other
/// paths untouched. Public so sibling crates (e.g. the sandbox runtime) apply
/// the exact same expansion the config validator uses — otherwise a path like
/// `~/sdk` passes validation expanded but is bound literally at runtime.
pub fn expand_user_path(raw: &str) -> std::path::PathBuf {
    let path = std::path::PathBuf::from(raw);
    if let Some(stripped) = raw.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(stripped);
        }
    }
    if raw == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    }
    path
}

fn paths_equal_or_same(a: &std::path::Path, b: &std::path::Path) -> bool {
    let a = std::fs::canonicalize(a).unwrap_or_else(|_| a.to_path_buf());
    let b = std::fs::canonicalize(b).unwrap_or_else(|_| b.to_path_buf());
    a == b
}

fn is_sensitive_extra_path(path: &std::path::Path) -> bool {
    let components: Vec<String> = path
        .components()
        .filter_map(|c| c.as_os_str().to_str().map(|s| s.to_ascii_lowercase()))
        .collect();
    for name in &components {
        if matches!(
            name.as_str(),
            ".ssh" | ".gnupg" | ".aws" | ".gcp" | ".docker" | ".kube"
        ) {
            return true;
        }
    }
    for window in components.windows(2) {
        if window[0] == ".config" && window[1] == "gcloud" {
            return true;
        }
    }
    false
}

fn default_sandbox_mode() -> String {
    "auto".into()
}

fn default_sandbox_memory_mb() -> u64 {
    4096
}

fn default_sandbox_cpu_seconds() -> u64 {
    600
}

fn default_sandbox_processes() -> u32 {
    128
}

/// Application-layer path-policy and sensitive-file settings (`[tools]` in config.toml).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolsConfig {
    /// `warn` (default) — allow access outside workspace but log a warning.
    /// `strict` — deny any path outside the workspace and `additional_dirs`.
    #[serde(default = "default_path_guard_mode")]
    pub path_guard_mode: String,
    /// Default `true`. Set to `false` to skip sensitive-file detection entirely
    /// (escape hatch — use with caution).
    #[serde(default = "default_true")]
    pub sensitive_path_check: bool,
    /// Additional directories allowed for file access in strict mode.
    #[serde(default)]
    pub additional_dirs: Vec<std::path::PathBuf>,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            path_guard_mode: default_path_guard_mode(),
            sensitive_path_check: true,
            additional_dirs: Vec::new(),
        }
    }
}

fn default_path_guard_mode() -> String {
    "warn".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PermissionConfig {
    #[serde(default)]
    pub rules: Vec<PermissionRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionRule {
    pub decision: String,
    pub pattern: String,
    #[serde(default)]
    pub scope: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookConfig {
    pub event: String,
    #[serde(default)]
    pub matcher: Option<String>,
    pub command: String,
    #[serde(default = "default_hook_timeout")]
    pub timeout: u64,
}

fn default_hook_timeout() -> u64 {
    5
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ServicesConfig {
    #[serde(default)]
    pub web_search: Option<WebSearchConfig>,
    #[serde(default)]
    pub web_fetch: Option<WebFetchConfig>,
    /// Deprecated — prefer `web_search`. Still read for one-time migration compat.
    #[serde(default)]
    pub moonshot_search: Option<ServiceEndpoint>,
    /// Deprecated — prefer `web_fetch`.
    #[serde(default)]
    pub moonshot_fetch: Option<ServiceEndpoint>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebSearchConfig {
    /// `searxng` | `brave` | `custom`
    #[serde(default)]
    pub provider: Option<String>,
    /// Full search endpoint URL (not auto-suffixed with `/v1/search`).
    pub base_url: String,
    #[serde(default)]
    pub api_key: Option<String>,
    /// Prefer reading the key from this environment variable.
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub default_limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebFetchConfig {
    /// Optional full proxy fetch endpoint. When omitted, FetchURL uses direct GET.
    pub base_url: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceEndpoint {
    pub base_url: String,
    #[serde(default)]
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// Transport: `stdio` (default), `sse`, `http`, or `streamable-http`.
    #[serde(default, rename = "type", alias = "transport")]
    pub transport_type: Option<String>,
    /// Stdio command (required for stdio transport).
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Working directory for stdio servers. Plugin manifests restrict this to
    /// paths inside the plugin root before it reaches the runtime.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Remote URL for sse / http / streamable-http transports.
    #[serde(default)]
    pub url: Option<String>,
    /// Extra HTTP headers for remote transports.
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// OAuth configuration for remote MCP servers.
    #[serde(default)]
    pub oauth: Option<McpOAuthConfig>,
    /// Request timeout in milliseconds.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// When `false`, the server is not connected and its tools are hidden.
    /// Defaults to enabled when omitted.
    #[serde(default)]
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct McpOAuthConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub client_secret: Option<String>,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub redirect_uri: Option<String>,
    /// Optional client label shown during DCR / authorize.
    #[serde(default)]
    pub client_label: Option<String>,
}

const DEFAULT_FIRST_TOKEN_TIMEOUT_MS: u64 = 60_000;
const HTTP_TOTAL_TIMEOUT_MS: u64 = 300_000;
const FIRST_TOKEN_TIMEOUT_CLAMP_MS: u64 = 290_000;

/// Resolve streaming first-token timeout.
///
/// Priority: model (`Some(0)` disables) → provider (`Some(0)` disables) → default 60s.
/// Values ≥ 300s are clamped to 290s.
pub fn resolve_first_token_timeout(
    model: &ModelConfig,
    provider: &ProviderConfig,
) -> Option<std::time::Duration> {
    let raw = match model.first_token_timeout_ms {
        Some(0) => return None,
        Some(ms) => ms,
        None => match provider.first_token_timeout_ms {
            Some(0) => return None,
            Some(ms) => ms,
            None => DEFAULT_FIRST_TOKEN_TIMEOUT_MS,
        },
    };
    let ms = if raw >= HTTP_TOTAL_TIMEOUT_MS {
        tracing::warn!(
            configured_ms = raw,
            clamped_ms = FIRST_TOKEN_TIMEOUT_CLAMP_MS,
            "first_token_timeout_ms >= 300s; clamping to 290s"
        );
        FIRST_TOKEN_TIMEOUT_CLAMP_MS
    } else {
        raw
    };
    Some(std::time::Duration::from_millis(ms))
}

impl AppConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        let default_model = self
            .default_model
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("default_model must be configured"))?;
        if !matches!(self.effective_permission_mode(), "manual" | "yolo" | "auto") {
            anyhow::bail!(
                "default_permission_mode must be one of manual, yolo, auto (got {})",
                self.effective_permission_mode()
            );
        }
        if self
            .plugin_marketplace
            .as_deref()
            .is_some_and(|source| source.trim().is_empty())
        {
            anyhow::bail!("plugin_marketplace must not be empty");
        }
        for (name, provider) in &self.providers {
            if !matches!(
                provider.provider_type.as_str(),
                "anthropic"
                    | "kimi"
                    | "openai"
                    | "openai-responses"
                    | "openai_responses"
                    | "responses"
                    | "openai-legacy"
                    | "openai-chat"
                    | "google"
                    | "google-genai"
                    | "gemini"
            ) {
                anyhow::bail!(
                    "provider {name} has unsupported type {}",
                    provider.provider_type
                );
            }
            if let Some(base_url) = provider.base_url.as_deref() {
                if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
                    anyhow::bail!("provider {name} base_url must use http or https");
                }
            }
            if let Some(oauth) = &provider.oauth {
                if provider.provider_type != "kimi" {
                    anyhow::bail!("provider {name} uses oauth but is not a Kimi provider");
                }
                if oauth.storage != "file" || oauth.key.trim().is_empty() {
                    anyhow::bail!("provider {name} has an invalid oauth configuration");
                }
            }
        }
        for (alias, model) in &self.models {
            if model.model.trim().is_empty() {
                anyhow::bail!("model {alias} has an empty upstream model id");
            }
            if !self.providers.contains_key(&model.provider) {
                anyhow::bail!(
                    "model {alias} references missing provider {}",
                    model.provider
                );
            }
            if model.max_context_size == Some(0) || model.max_output_size == Some(0) {
                anyhow::bail!("model {alias} token limits must be greater than zero");
            }
        }
        if !self.models.contains_key(default_model) {
            anyhow::bail!("default_model {default_model} is not present in [models]");
        }
        if let Some(secondary) = self.secondary_model.as_deref() {
            if !self.models.contains_key(secondary) {
                anyhow::bail!("secondary_model {secondary} is not present in [models]");
            }
        }
        if let Some(fallback) = self.fallback_model.as_deref() {
            if !self.models.contains_key(fallback) {
                anyhow::bail!("fallback_model {fallback} is not present in [models]");
            }
        }
        for root in &self.trusted_workspaces {
            if !std::path::Path::new(root).is_absolute() {
                anyhow::bail!("trusted workspace must be absolute: {root}");
            }
        }
        for entry in &self.workspace_trust.workspaces {
            entry.validate()?;
        }
        self.sandbox.validate_extra_paths()?;
        if self.image.max_edge_px == 0 || self.image.max_edge_px > 16_384 {
            anyhow::bail!("image.max_edge_px must be between 1 and 16384");
        }
        if self.image.read_byte_budget == 0 || self.image.read_byte_budget > 20 * 1024 * 1024 {
            anyhow::bail!("image.read_byte_budget must be between 1 and 20971520 bytes");
        }
        for (name, server) in &self.mcp_servers {
            match server.transport_type.as_deref().unwrap_or("stdio") {
                "stdio" if server.command.as_deref().unwrap_or("").trim().is_empty() => {
                    anyhow::bail!("MCP server {name} requires command for stdio transport")
                }
                "sse" | "http" | "streamable-http"
                    if server.url.as_deref().unwrap_or("").trim().is_empty() =>
                {
                    anyhow::bail!("MCP server {name} requires url for remote transport")
                }
                "stdio" | "sse" | "http" | "streamable-http" => {}
                other => anyhow::bail!("MCP server {name} has unsupported transport {other}"),
            }
        }
        Ok(())
    }

    pub fn resolve_model(&self, alias: &str) -> Option<(&ModelConfig, &ProviderConfig)> {
        let model = self.models.get(alias)?;
        let provider = self.providers.get(&model.provider)?;
        Some((model, provider))
    }

    /// Resolve streaming first-token timeout for a model/provider pair.
    ///
    /// Priority: model (`Some(0)` disables) → provider (`Some(0)` disables) → default 60s.
    /// Values ≥ 300s are clamped to 290s (below the HTTP total timeout).
    pub fn resolve_first_token_timeout(
        model: &ModelConfig,
        provider: &ProviderConfig,
    ) -> Option<std::time::Duration> {
        resolve_first_token_timeout(model, provider)
    }

    pub fn default_model_alias(&self) -> Option<&str> {
        self.default_model.as_deref()
    }

    pub fn effective_permission_mode(&self) -> &str {
        self.default_permission_mode.as_deref().unwrap_or("manual")
    }

    pub fn is_skill_disabled(&self, name: &str) -> bool {
        self.disabled_skills.iter().any(|s| s == name)
    }

    pub fn is_mcp_disabled(&self, name: &str) -> bool {
        self.disabled_mcp_servers.iter().any(|s| s == name)
    }

    pub fn set_skill_disabled(&mut self, name: &str, disabled: bool) {
        if disabled {
            if !self.disabled_skills.iter().any(|s| s == name) {
                self.disabled_skills.push(name.to_string());
                self.disabled_skills.sort();
            }
        } else {
            self.disabled_skills.retain(|s| s != name);
        }
    }

    pub fn set_mcp_disabled(&mut self, name: &str, disabled: bool) {
        if disabled {
            if !self.disabled_mcp_servers.iter().any(|s| s == name) {
                self.disabled_mcp_servers.push(name.to_string());
                self.disabled_mcp_servers.sort();
            }
        } else {
            self.disabled_mcp_servers.retain(|s| s != name);
        }
        if let Some(server) = self.mcp_servers.get_mut(name) {
            server.enabled = Some(!disabled);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_config() -> AppConfig {
        let mut config = AppConfig {
            default_model: Some("test/model".into()),
            fallback_model: None,
            ..AppConfig::default()
        };
        config.providers.insert(
            "test".into(),
            ProviderConfig {
                provider_type: "openai".into(),
                api_key: None,
                base_url: Some("https://example.test".into()),
                custom_headers: HashMap::new(),
                oauth: None,
                first_token_timeout_ms: None,
            },
        );
        config.models.insert(
            "test/model".into(),
            ModelConfig {
                provider: "test".into(),
                model: "upstream-model".into(),
                max_context_size: Some(100_000),
                max_output_size: Some(4_096),
                capabilities: vec!["tool_use".into()],
                display_name: None,
                support_efforts: Vec::new(),
                default_effort: None,
                pricing: None,
                experimental_adaptive_thinking: false,
                experimental_visible_empty_retries: 0,
                first_token_timeout_ms: None,
            },
        );
        config
    }

    #[test]
    fn first_token_timeout_priority_and_disable() {
        let mut model = ModelConfig {
            provider: "test".into(),
            model: "m".into(),
            max_context_size: None,
            max_output_size: None,
            capabilities: Vec::new(),
            display_name: None,
            support_efforts: Vec::new(),
            default_effort: None,
            pricing: None,
            experimental_adaptive_thinking: false,
            experimental_visible_empty_retries: 0,
            first_token_timeout_ms: None,
        };
        let mut provider = ProviderConfig {
            provider_type: "openai".into(),
            api_key: None,
            base_url: None,
            custom_headers: HashMap::new(),
            oauth: None,
            first_token_timeout_ms: Some(30_000),
        };
        assert_eq!(
            resolve_first_token_timeout(&model, &provider),
            Some(std::time::Duration::from_millis(30_000))
        );
        model.first_token_timeout_ms = Some(45_000);
        assert_eq!(
            resolve_first_token_timeout(&model, &provider),
            Some(std::time::Duration::from_millis(45_000))
        );
        model.first_token_timeout_ms = Some(0);
        assert_eq!(resolve_first_token_timeout(&model, &provider), None);
        model.first_token_timeout_ms = None;
        provider.first_token_timeout_ms = Some(0);
        assert_eq!(resolve_first_token_timeout(&model, &provider), None);
        provider.first_token_timeout_ms = None;
        assert_eq!(
            resolve_first_token_timeout(&model, &provider),
            Some(std::time::Duration::from_millis(60_000))
        );
        model.first_token_timeout_ms = Some(300_000);
        assert_eq!(
            resolve_first_token_timeout(&model, &provider),
            Some(std::time::Duration::from_millis(290_000))
        );
    }

    #[test]
    fn accepts_consistent_configuration() {
        valid_config().validate().unwrap();
    }

    #[test]
    fn validates_global_fallback_model() {
        let mut config = valid_config();
        config.fallback_model = Some("missing/model".into());
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("fallback_model missing/model"));

        config.fallback_model = Some("test/model".into());
        config.validate().unwrap();
    }

    #[test]
    fn loop_step_limit_defaults_to_unlimited() {
        let loop_control: LoopControlConfig = toml::from_str("").unwrap();
        assert_eq!(loop_control.max_steps_per_turn, 0);
        assert_eq!(loop_control.rate_limit_retry_base_seconds, 5);
        assert_eq!(LoopControlConfig::default().max_steps_per_turn, 0);
        assert_eq!(
            LoopControlConfig::default().rate_limit_retry_base_seconds,
            5
        );
    }

    #[test]
    fn parses_experimental_model_recovery_options() {
        let model: ModelConfig = toml::from_str(
            r#"
provider = "local"
model = "claude-opus-4-8"
experimental_adaptive_thinking = true
experimental_visible_empty_retries = 1
"#,
        )
        .unwrap();
        assert!(model.experimental_adaptive_thinking);
        assert_eq!(model.experimental_visible_empty_retries, 1);
    }

    #[test]
    fn recognizes_all_disabled_sandbox_aliases() {
        for mode in ["disabled", "OFF", " none "] {
            let sandbox = SandboxConfig {
                mode: mode.into(),
                ..SandboxConfig::default()
            };
            assert!(sandbox.is_disabled(), "mode {mode:?}");
        }
        for mode in ["auto", "process", "workspace", "unknown"] {
            assert!(!SandboxConfig::mode_is_disabled(mode), "mode {mode:?}");
        }
    }

    #[test]
    fn rejects_unknown_provider_type() {
        let mut config = valid_config();
        config.providers.get_mut("test").unwrap().provider_type = "typo".into();
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("unsupported type"));
    }

    #[test]
    fn rejects_missing_default_model() {
        let mut config = valid_config();
        config.default_model = Some("missing".into());
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("not present"));
    }

    #[test]
    fn validates_image_limits() {
        let mut config = valid_config();
        config.image.max_edge_px = 0;
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("max_edge_px"));
        config.image.max_edge_px = 2000;
        config.image.read_byte_budget = 0;
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("read_byte_budget"));
    }

    #[test]
    fn rejects_empty_plugin_marketplace() {
        let mut config = valid_config();
        config.plugin_marketplace = Some("   ".into());
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("plugin_marketplace"));
    }

    #[test]
    fn skill_and_mcp_disable_helpers() {
        let mut config = valid_config();
        config.mcp_servers.insert(
            "demo".into(),
            McpServerConfig {
                transport_type: Some("stdio".into()),
                command: Some("echo".into()),
                args: Vec::new(),
                env: HashMap::new(),
                cwd: None,
                url: None,
                headers: HashMap::new(),
                oauth: None,
                timeout_ms: None,
                enabled: None,
            },
        );
        assert!(!config.is_skill_disabled("x"));
        config.set_skill_disabled("x", true);
        assert!(config.is_skill_disabled("x"));
        config.set_skill_disabled("x", false);
        assert!(!config.is_skill_disabled("x"));

        config.set_mcp_disabled("demo", true);
        assert!(config.is_mcp_disabled("demo"));
        assert_eq!(config.mcp_servers["demo"].enabled, Some(false));
        config.set_mcp_disabled("demo", false);
        assert!(!config.is_mcp_disabled("demo"));
        assert_eq!(config.mcp_servers["demo"].enabled, Some(true));
    }
}
