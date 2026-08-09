use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppConfig {
    #[serde(default)]
    pub default_model: Option<String>,
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
    #[serde(default)]
    pub telemetry: bool,
    /// Trusted workspace roots (absolute paths). Empty = trust cwd implicitly.
    #[serde(default)]
    pub trusted_workspaces: Vec<String>,
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
    #[serde(default)]
    pub models: HashMap<String, ModelConfig>,
    #[serde(default)]
    pub thinking: Option<ThinkingConfig>,
    #[serde(default)]
    pub loop_control: Option<LoopControlConfig>,
    #[serde(default)]
    pub background: Option<BackgroundConfig>,
    #[serde(default)]
    pub sandbox: SandboxConfig,
    #[serde(default)]
    pub permission: Option<PermissionConfig>,
    #[serde(default)]
    pub hooks: Vec<HookConfig>,
    #[serde(default)]
    pub services: Option<ServicesConfig>,
    #[serde(default)]
    pub mcp_servers: HashMap<String, McpServerConfig>,
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
    #[serde(default = "default_reserved_context")]
    pub reserved_context_size: u64,
    #[serde(default = "default_max_steps")]
    pub max_steps_per_turn: u32,
    /// Auto-compact when request estimate exceeds usable context.
    #[serde(default = "default_true")]
    pub auto_compact: bool,
    /// Messages to keep when auto-compacting.
    #[serde(default = "default_compact_keep")]
    pub compact_keep_last: u32,
    /// Token counting strategy: measured+estimated | measured | estimated
    #[serde(default = "default_token_strategy")]
    pub token_counting: String,
}

fn default_true() -> bool {
    true
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
fn default_reserved_context() -> u64 {
    50000
}
fn default_max_steps() -> u32 {
    64
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
    #[serde(default = "default_true")]
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
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            mode: default_sandbox_mode(),
            network: true,
            memory_mb: default_sandbox_memory_mb(),
            cpu_seconds: default_sandbox_cpu_seconds(),
            max_processes: default_sandbox_processes(),
            extra_read_paths: Vec::new(),
            extra_write_paths: Vec::new(),
        }
    }
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
    pub moonshot_search: Option<ServiceEndpoint>,
    #[serde(default)]
    pub moonshot_fetch: Option<ServiceEndpoint>,
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
    #[serde(default, rename = "type")]
    pub transport_type: Option<String>,
    /// Stdio command (required for stdio transport).
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
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
        for root in &self.trusted_workspaces {
            if !std::path::Path::new(root).is_absolute() {
                anyhow::bail!("trusted workspace must be absolute: {root}");
            }
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

    pub fn default_model_alias(&self) -> Option<&str> {
        self.default_model.as_deref()
    }

    pub fn effective_permission_mode(&self) -> &str {
        self.default_permission_mode.as_deref().unwrap_or("manual")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_config() -> AppConfig {
        let mut config = AppConfig {
            default_model: Some("test/model".into()),
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
            },
        );
        config
    }

    #[test]
    fn accepts_consistent_configuration() {
        valid_config().validate().unwrap();
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
}
