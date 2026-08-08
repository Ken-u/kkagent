use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppConfig {
    #[serde(default)]
    pub default_model: Option<String>,
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
}

fn default_max_attempts() -> u32 { 10 }
fn default_reserved_context() -> u64 { 50000 }

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
}

fn default_max_tasks() -> u32 { 4 }

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

fn default_hook_timeout() -> u64 { 5 }

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
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

impl AppConfig {
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
