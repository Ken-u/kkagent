use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

use crate::toolchain::ToolchainConfig;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppConfig {
    #[serde(default)]
    pub default_model: Option<String>,
    /// High-quality model alias (the `quality` token's target). Falls back
    /// to `default_model` when unset. Distinct from `default_model`, which
    /// only decides what model a *new session* starts with.
    #[serde(default)]
    pub quality_model: Option<String>,
    /// Global fallback model alias used after the primary model exhausts its
    /// normal per-step retry budget.
    #[serde(default)]
    pub fallback_model: Option<String>,
    /// Mid-tier model alias (`balance` slot): used as a global fallback for
    /// subagents (after per-profile `[subagent.default_models]`) and as a
    /// mid-priority compaction summarizer when `compaction_model` is unset.
    /// Named `secondary_model` before config schema v2; startup migration
    /// renames the key.
    #[serde(default, alias = "secondary_model")]
    pub balance_model: Option<String>,
    /// Optional fast/cheap model alias targeted at subagents. The symbolic
    /// token `fast` (in tool `model` overrides and
    /// `[subagent.default_models]` values) resolves here first, then
    /// `balance_model`, then `default_model`.
    #[serde(default)]
    pub fast_model: Option<String>,
    /// Dedicated model alias for history compaction summaries. When set it
    /// takes precedence over `balance_model`, the session model and
    /// `default_model` when resolving the compaction summarizer.
    #[serde(default)]
    pub compaction_model: Option<String>,
    /// Goal-mode settings (completion judge etc.). Defaults keep the legacy
    /// behavior: goal `complete` claims are accepted without review.
    #[serde(default)]
    pub goal: GoalConfig,
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
    /// Optional local path, file URL, or HTTP(S) URL for the default KK plugin
    /// marketplace catalog. Kept for backward compatibility; additional catalogs
    /// go in `plugin_marketplaces`.
    #[serde(default)]
    pub plugin_marketplace: Option<String>,
    /// Extra plugin marketplace catalogs. Each entry is a URL/path string or
    /// `{ name = "...", source = "..." }`. Combined with `plugin_marketplace`
    /// (which stays first when set).
    #[serde(default)]
    pub plugin_marketplaces: Vec<PluginMarketplaceSpec>,
    #[serde(default)]
    pub telemetry: bool,
    /// Experimental TUI mouse-reporting mode: `capture` (default — the app
    /// owns the wheel for in-transcript scrolling) or `off` (terminal-native
    /// scrollback; for SSH clients that mishandle SGR mouse reporting and
    /// turn the wheel into arrow keys, which recall input history instead).
    /// `KKAGENT_MOUSE_MODE=off` overrides this at runtime.
    #[serde(default, alias = "mouse_capture")]
    pub mouse_mode: Option<String>,
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
    /// Plugin system behavior (`[plugins]`).
    #[serde(default)]
    pub plugins: PluginsConfig,
    /// Subagent delegation limits (`[subagent]`).
    #[serde(default)]
    pub subagent: SubagentSettings,
}

/// Subagent delegation limits and per-profile defaults (`[subagent]` in
/// config.toml).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentSettings {
    /// Maximum subagent nesting depth below the root agent. `1` means
    /// subagents cannot delegate further; clamped to `1..=4` at runtime.
    /// Default `2` (main → L1 → L2, L2 being a leaf).
    #[serde(default = "default_subagent_max_depth")]
    pub max_depth: u32,
    /// Maximum concurrently *running* subagents per manager (root session
    /// and each nested subagent session). Default `4`.
    #[serde(default = "default_subagent_max_concurrent")]
    pub max_concurrent: usize,
    /// Per-profile default model aliases (`[subagent.default_models]`).
    ///
    /// Keys are profile names such as `explore`, `coder`, and `general`.
    /// Lookup is case-insensitive; `agent` is treated as `general`. The
    /// optional key `fallback` is used when the launched profile has no
    /// dedicated entry.
    ///
    /// Values may be a real `[models]` alias, or one of the symbolic tokens
    /// `quality` (top-level `default_model`), `balance` (top-level
    /// `balance_model`, falling back to `default_model`), `fast`
    /// (top-level `fast_model`, falling back to `balance_model` then
    /// `default_model`), or `current` (parent session model).
    #[serde(default)]
    pub default_models: HashMap<String, String>,
}

fn default_subagent_max_depth() -> u32 {
    2
}

fn default_subagent_max_concurrent() -> usize {
    4
}

impl Default for SubagentSettings {
    fn default() -> Self {
        Self {
            max_depth: default_subagent_max_depth(),
            max_concurrent: default_subagent_max_concurrent(),
            default_models: HashMap::new(),
        }
    }
}

impl SubagentSettings {
    /// Effective depth cap, clamped to a sane `1..=4` range.
    pub fn effective_max_depth(&self) -> u32 {
        self.max_depth.clamp(1, 4)
    }

    /// Effective concurrency cap, clamped to at least 1.
    pub fn effective_max_concurrent(&self) -> usize {
        self.max_concurrent.max(1)
    }

    /// Resolve the configured default model alias for a subagent profile.
    ///
    /// Lookup order: exact profile key (case-insensitive; `agent` →
    /// `general`) → catch-all key `fallback`. Empty strings are ignored.
    /// Returned values may still be symbolic (`current` / `default` /
    /// `secondary`) and need expansion via
    /// [`AppConfig::expand_model_alias_token`].
    pub fn model_for_profile(&self, profile: &str) -> Option<&str> {
        let normalized = normalize_subagent_profile(profile);
        lookup_profile_model(&self.default_models, &normalized)
            .or_else(|| lookup_profile_model(&self.default_models, "fallback"))
    }
}

/// Reserved model tokens usable in `[subagent.default_models]` values and
/// tool `model` overrides. Tool schemas expose exactly these; the v1
/// spellings `default` / `secondary` are no longer accepted and are
/// rewritten automatically at startup (see [`crate::migrate`]).
pub fn is_symbolic_model_alias(token: &str) -> bool {
    matches!(
        token.trim().to_ascii_lowercase().as_str(),
        "quality" | "balance" | "fast" | "current"
    )
}

fn normalize_subagent_profile(profile: &str) -> String {
    let key = profile.trim().to_ascii_lowercase();
    if key == "agent" || key.is_empty() {
        "general".into()
    } else {
        key
    }
}

fn lookup_profile_model<'a>(models: &'a HashMap<String, String>, profile: &str) -> Option<&'a str> {
    models
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(profile))
        .map(|(_, value)| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// Plugin system behavior (`[plugins]` in config.toml).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PluginsConfig {
    /// Built-in tool names plugins may override in addition to the default
    /// low-risk allowlist (`Web`, `TaskOutput`, `Skill`, `Cron`,
    /// `ReadMediaFile`). High-risk tools such as `Bash`/`Edit`/`Write` must
    /// be opted into explicitly here. Guard tools (`AskUserQuestion`,
    /// `EnterPlanMode`, `ExitPlanMode`, `Goal`) can never be overridden.
    #[serde(default)]
    pub extra_overridable_tools: Vec<String>,
}

impl PluginsConfig {
    /// True when `name` may be overridden by a plugin under this config.
    pub fn is_overridable(&self, name: &str) -> bool {
        crate::plugin_policy::tool_overridable(name, &self.extra_overridable_tools)
    }
}

/// A plugin marketplace catalog declared in config.toml.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum PluginMarketplaceSpec {
    Source(String),
    Named {
        #[serde(default)]
        name: Option<String>,
        source: String,
    },
}

impl PluginMarketplaceSpec {
    pub fn source(&self) -> &str {
        match self {
            Self::Source(source) => source,
            Self::Named { source, .. } => source,
        }
    }

    pub fn name(&self) -> Option<&str> {
        match self {
            Self::Named { name, .. } => name
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty()),
            Self::Source(_) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginMarketplaceCatalog {
    pub source: String,
    pub name: Option<String>,
}

impl AppConfig {
    /// Configured marketplace catalogs: `plugin_marketplace` first (if set),
    /// then `plugin_marketplaces`, de-duplicated by source.
    pub fn plugin_marketplace_catalogs(&self) -> Vec<PluginMarketplaceCatalog> {
        let mut catalogs = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut push = |source: &str, name: Option<String>| {
            let source = source.trim();
            if source.is_empty() || !seen.insert(source.to_string()) {
                return;
            }
            catalogs.push(PluginMarketplaceCatalog {
                source: source.to_string(),
                name,
            });
        };
        if let Some(source) = self.plugin_marketplace.as_deref() {
            push(source, None);
        }
        for spec in &self.plugin_marketplaces {
            push(spec.source(), spec.name().map(str::to_string));
        }
        catalogs
    }
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
    /// Per-color TUI palette overrides (`[ui.theme]`). Values are `#RGB` /
    /// `#RRGGBB` hex colors; unset or invalid entries keep the built-in
    /// kimi-dark palette.
    #[serde(default)]
    pub theme: UiThemeConfig,
    /// Reduce spinner / animation updates.
    #[serde(default)]
    pub reduce_motion: bool,
    /// Check crates.io / release notes when idle (cached).
    #[serde(default = "default_true")]
    pub check_updates: bool,
    /// Experimental: recursive fuzzy `@` path completion (fd / deep walk).
    /// Default is one directory level at a time (type `/` to descend).
    #[serde(default)]
    pub experimental_smart_at_complete: bool,
    /// Optional key override map: action name → key chord (e.g. "interrupt" = "ctrl-c").
    #[serde(default)]
    pub keybindings: HashMap<String, String>,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            high_contrast: false,
            theme: UiThemeConfig::default(),
            reduce_motion: false,
            check_updates: true,
            experimental_smart_at_complete: false,
            keybindings: HashMap::new(),
        }
    }
}

/// Optional TUI color overrides (`[ui.theme]`). Field names mirror
/// `kkagent_tui::theme::Theme`; see that crate for where each color shows up.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UiThemeConfig {
    /// Links / headers / primary highlights.
    #[serde(default)]
    pub primary: Option<String>,
    /// BTW & goal-judge composer, accents, stream cursor.
    #[serde(default)]
    pub accent: Option<String>,
    /// Default message body text.
    #[serde(default)]
    pub text: Option<String>,
    /// Bold-emphasis text (inline code headers etc.).
    #[serde(default)]
    pub text_strong: Option<String>,
    /// Dimmed secondary text.
    #[serde(default)]
    pub text_dim: Option<String>,
    /// Muted hints / placeholders / timestamps.
    #[serde(default)]
    pub text_muted: Option<String>,
    /// Window + input borders (BTW, goal judge, panels) at rest.
    #[serde(default)]
    pub border: Option<String>,
    /// Focused borders (menus, pickers, search).
    #[serde(default)]
    pub border_focus: Option<String>,
    /// Success / completed states.
    #[serde(default)]
    pub success: Option<String>,
    /// Warnings, yolo badge, queued markers.
    #[serde(default)]
    pub warning: Option<String>,
    /// Errors / reject verdicts.
    #[serde(default)]
    pub error: Option<String>,
    /// Background fill for popups.
    #[serde(default)]
    pub background: Option<String>,
    /// User message marker (kimi yellow).
    #[serde(default)]
    pub role_user: Option<String>,
    /// Shell-mode input border & prefix.
    #[serde(default)]
    pub shell_mode: Option<String>,
    /// Plan-mode input border & prefix.
    #[serde(default)]
    pub plan_mode: Option<String>,
    /// Colors scoped to the goal judge window (Ctrl+J panel + its composer).
    /// Unset entries inherit the global `[ui.theme]` values.
    #[serde(default)]
    pub goal_judge: UiGoalJudgeThemeConfig,
}

/// Optional per-window overrides for the goal judge window
/// (`[ui.theme.goal_judge]`). Only the colors that window actually renders
/// are exposed; anything unset falls back to `[ui.theme]` / the default.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UiGoalJudgeThemeConfig {
    /// Judge window border and composer border.
    #[serde(default)]
    pub border: Option<String>,
    /// `judge >` prefix, `judge ›` reply marker, approve label, criterion notes.
    #[serde(default)]
    pub accent: Option<String>,
    /// `you ›` marker for user messages.
    #[serde(default)]
    pub primary: Option<String>,
    /// Errors and the reject verdict label.
    #[serde(default)]
    pub error: Option<String>,
    /// Empty-state hints and the "judge thinking…" status.
    #[serde(default)]
    pub text_muted: Option<String>,
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProviderConfig {
    #[serde(rename = "type")]
    pub provider_type: String,
    #[serde(default)]
    pub api_key: Option<String>,
    /// Name of an environment variable that holds the API key. When set and
    /// the variable is present (non-empty), it takes precedence over the
    /// inline `api_key` field — matching the behavior of web search/fetch
    /// services. Useful for keeping secrets out of `config.toml`.
    #[serde(default)]
    pub api_key_env: Option<String>,
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
    /// Total HTTP request timeout (milliseconds) for this provider's LLM
    /// requests. Unset or `0` disables it — streaming is then bounded only by
    /// the first-token gate and the per-read idle timeout (60s).
    #[serde(default)]
    pub request_timeout_ms: Option<u64>,
    /// Unknown keys captured for diagnostics. A key here almost always means
    /// either a typo or — much more commonly — that the key physically belongs
    /// to the *following* TOML table (TOML assigns keys to the nearest
    /// preceding table header). `load_config` warns about these at startup
    /// instead of silently ignoring them.
    #[serde(default, flatten)]
    pub extra_fields: BTreeMap<String, toml::Value>,
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
    /// Experimental: act as the vision proxy for non-vision models. Exactly one
    /// model may set this, and it must also declare an image input capability
    /// (e.g. `image_in`). When the active primary model declares no image input,
    /// image blocks are replaced by text descriptions produced by this model
    /// before each request, and `ReadMediaFile` stays visible to the agent.
    #[serde(default)]
    pub experimental_vision_proxy: bool,
    /// Experimental: retry a thinking-only/empty response immediately after tool results.
    #[serde(default)]
    pub experimental_visible_empty_retries: u32,
    /// Experimental: auto-retry after rolling back a malformed tool call (non-JSON arguments).
    #[serde(default)]
    pub experimental_bad_toolcall_auto_retries: u32,
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

/// Whether a `capabilities` list declares image input support. Shared by
/// capability derivation and vision-proxy validation so the two cannot drift.
pub fn declares_image_input(capabilities: &[String]) -> bool {
    capabilities.iter().any(|c| {
        matches!(
            c.to_lowercase().as_str(),
            "vision" | "image" | "image_in" | "multimodal"
        )
    })
}

/// Known reasoning effort levels. Covers OpenAI Responses/Chat Completions
/// (`none`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max` on GPT-5.6)
/// plus the Anthropic adaptive thinking vocabulary.
pub fn is_known_effort(effort: &str) -> bool {
    matches!(
        effort.trim().to_lowercase().as_str(),
        "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
    )
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

fn default_goal_judge_max_rejects() -> u32 {
    2
}

fn default_goal_judge_timeout_secs() -> u64 {
    120
}

/// Goal-mode configuration. With `judge_enabled = false` (the default) the
/// completion judge is bypassed entirely and a model `Goal update complete`
/// behaves exactly as before — accepted immediately with the summary pass.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GoalConfig {
    /// Review model-reported goal completion with an independent judge agent
    /// before accepting it. Off by default (legacy behavior).
    pub judge_enabled: bool,
    /// Model alias for the judge agent. Empty/None falls back to the
    /// compaction resolution chain (`compaction_model` > `balance_model` >
    /// session model > `default_model`).
    pub judge_model: Option<String>,
    /// Rejects tolerated before the goal is blocked instead of continued.
    pub judge_max_rejects: u32,
    /// Wall-clock timeout for a single judge agent run, in seconds.
    pub judge_timeout_secs: u64,
}

impl Default for GoalConfig {
    fn default() -> Self {
        Self {
            judge_enabled: false,
            judge_model: None,
            judge_max_rejects: default_goal_judge_max_rejects(),
            judge_timeout_secs: default_goal_judge_timeout_secs(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopControlConfig {
    #[serde(default = "default_max_attempts")]
    pub max_attempts_per_step: u32,
    /// Base delay for 429 responses without a server-provided retry hint.
    #[serde(default = "default_rate_limit_retry_base_seconds")]
    pub rate_limit_retry_base_seconds: u64,
    /// Base delay for retryable LLM failures other than 429 responses.
    #[serde(default = "default_retry_base_seconds")]
    pub retry_base_seconds: u64,
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
            retry_base_seconds: default_retry_base_seconds(),
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
fn default_retry_base_seconds() -> u64 {
    1
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
    /// When `true` (default), deferred tools are loaded progressively via
    /// `SelectTools` for the Kimi provider or models declaring the
    /// `dynamically_loaded_tools` capability. Kimi preserves message-level
    /// schemas; other adapters merge loaded schemas into top-level `tools[]`,
    /// which may refresh the provider prompt cache.
    #[serde(default = "default_true")]
    pub dynamically_loaded_tools: bool,
    /// Directory names skipped by Glob / Grep / `@` completion walks.
    /// When set (including `[]`), replaces the built-in default list.
    /// Use [`Self::extra_heavy_dirs`] to append without replacing.
    #[serde(default)]
    pub heavy_dirs: Option<Vec<String>>,
    /// Extra heavy directory names appended to the effective list.
    #[serde(default)]
    pub extra_heavy_dirs: Vec<String>,
    /// Wall-clock timeout (seconds) for the synchronous wait of `Agent` /
    /// `AgentSwarm` tool calls. `None` (default) waits indefinitely. When the
    /// timeout fires the subagents are NOT killed — they detach and keep
    /// running in the background, retrievable via `TaskOutput`.
    #[serde(default)]
    pub subagent_timeout_secs: Option<u64>,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            path_guard_mode: default_path_guard_mode(),
            sensitive_path_check: true,
            additional_dirs: Vec::new(),
            dynamically_loaded_tools: true,
            heavy_dirs: None,
            extra_heavy_dirs: Vec::new(),
            subagent_timeout_secs: None,
        }
    }
}

impl ToolsConfig {
    /// Built-in names always skipped unless the caller explicitly descends
    /// into them (e.g. Glob `out/soong/**`). Includes `.repo` for AOSP-style
    /// multi-git checkouts.
    pub const DEFAULT_HEAVY_DIRS: &'static [&'static str] =
        &["node_modules", "target", ".git", "out", ".repo"];

    /// Resolve the effective heavy-dir skip list (override + extras).
    pub fn effective_heavy_dirs(&self) -> Vec<String> {
        let mut base: Vec<String> = match &self.heavy_dirs {
            Some(dirs) => dirs.clone(),
            None => Self::DEFAULT_HEAVY_DIRS
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
        };
        for extra in &self.extra_heavy_dirs {
            if !extra.is_empty() && !base.iter().any(|d| d == extra) {
                base.push(extra.clone());
            }
        }
        base
    }

    /// Effective wall-clock timeout for synchronous `Agent` / `AgentSwarm`
    /// waits. `KKAGENT_SUBAGENT_TIMEOUT_SECS` env overrides the config value
    /// (`0` disables the timeout). Returns `None` when unbounded.
    pub fn effective_subagent_timeout_secs(&self) -> Option<u64> {
        if let Ok(raw) = std::env::var("KKAGENT_SUBAGENT_TIMEOUT_SECS") {
            let raw = raw.trim();
            if !raw.is_empty() {
                if let Ok(v) = raw.parse::<u64>() {
                    return if v == 0 { None } else { Some(v) };
                }
            }
        }
        self.subagent_timeout_secs
    }

    /// Merge project-level `.kkagent/config.toml` `[tools]` overrides into
    /// this config. Only heavy-dir related fields are applied (other project
    /// settings stay global / `--config`).
    pub fn merge_project_overrides(&mut self, workspace: &std::path::Path) {
        let path = workspace.join(".kkagent").join("config.toml");
        if !path.is_file() {
            return;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            return;
        };
        #[derive(Deserialize, Default)]
        struct ProjectFile {
            #[serde(default)]
            tools: Option<ProjectTools>,
        }
        #[derive(Deserialize, Default)]
        struct ProjectTools {
            #[serde(default)]
            heavy_dirs: Option<Vec<String>>,
            #[serde(default)]
            extra_heavy_dirs: Option<Vec<String>>,
        }
        let Ok(parsed) = toml::from_str::<ProjectFile>(&content) else {
            return;
        };
        let Some(tools) = parsed.tools else {
            return;
        };
        if tools.heavy_dirs.is_some() {
            self.heavy_dirs = tools.heavy_dirs;
        }
        if let Some(extra) = tools.extra_heavy_dirs {
            for name in extra {
                if !name.is_empty() && !self.extra_heavy_dirs.iter().any(|d| d == &name) {
                    self.extra_heavy_dirs.push(name);
                }
            }
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

/// Outbound HTTP proxy policy for a web service endpoint.
///
/// reqwest honors `http_proxy` / `https_proxy` / `all_proxy` by default, which
/// breaks endpoints that live on loopback or private networks (the remote proxy
/// cannot reach them). `auto` (default) bypasses the proxy for such endpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum WebProxyMode {
    /// Bypass the proxy when the endpoint host is loopback / link-local /
    /// private (literal IPs or `localhost`-style names); otherwise follow the
    /// system proxy environment.
    #[default]
    Auto,
    /// Never use a proxy for this endpoint, regardless of environment.
    None,
    /// Always follow system proxy environment variables.
    System,
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
    /// Outbound proxy policy when reaching `base_url` (default `auto`).
    #[serde(default)]
    pub proxy: WebProxyMode,
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
    /// Outbound proxy policy when reaching `base_url` (default `auto`).
    #[serde(default)]
    pub proxy: WebProxyMode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceEndpoint {
    pub base_url: String,
    #[serde(default)]
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
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

/// Resolve streaming first-token timeout.
///
/// Priority: model (`Some(0)` disables) → provider (`Some(0)` disables) → default 60s.
/// Values larger than the provider's opt-in `request_timeout_ms` are clamped
/// just below it so the first-token gate cannot outlive the total deadline;
/// with no total timeout configured the value is used as-is.
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
    let ms = match provider
        .request_timeout_ms
        .filter(|ms| *ms > 0 && raw >= *ms)
    {
        Some(total_ms) => {
            // Keep the first-token gate strictly below the total request
            // deadline; a 1s margin avoids racing the total timeout.
            let clamped = (total_ms - 1_000).max(1_000);
            tracing::warn!(
                configured_ms = raw,
                request_timeout_ms = total_ms,
                clamped_ms = clamped,
                "first_token_timeout_ms >= request_timeout_ms; clamping"
            );
            clamped
        }
        None => raw,
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
        // Deliberately a warning, not an error: auto + disabled is legitimate
        // inside a controlled container/VM, but outside one it removes every
        // OS-level backstop under an unattended agent.
        if self.effective_permission_mode() == "auto" {
            let sandbox_off = matches!(
                self.sandbox.mode.trim().to_ascii_lowercase().as_str(),
                "disabled" | "off" | "none"
            );
            if sandbox_off {
                tracing::warn!(
                    "permission_mode=auto with sandbox disabled: autonomous mode without \
                     OS-level containment; set sandbox.mode to auto/workspace/process unless \
                     running inside a controlled container"
                );
            }
        }
        if self
            .plugin_marketplace
            .as_deref()
            .is_some_and(|source| source.trim().is_empty())
        {
            anyhow::bail!("plugin_marketplace must not be empty");
        }
        for (index, spec) in self.plugin_marketplaces.iter().enumerate() {
            if spec.source().trim().is_empty() {
                anyhow::bail!("plugin_marketplaces[{index}] source must not be empty");
            }
            if spec.name().is_some_and(|name| name.chars().count() > 80) {
                anyhow::bail!("plugin_marketplaces[{index}] name must be at most 80 characters");
            }
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
            if model.experimental_vision_proxy && !declares_image_input(&model.capabilities) {
                anyhow::bail!(
                    "model {alias} sets experimental_vision_proxy without an image input capability (add `image_in` to its capabilities)"
                );
            }
            if model.max_context_size == Some(0) || model.max_output_size == Some(0) {
                anyhow::bail!("model {alias} token limits must be greater than zero");
            }
            if let Some(effort) = model.default_effort.as_deref() {
                let effort = effort.trim();
                if effort.is_empty() {
                    anyhow::bail!("model {alias} default_effort must not be empty");
                }
                if !is_known_effort(effort) {
                    anyhow::bail!(
                        "model {alias} default_effort '{effort}' is not a known reasoning effort (expected one of: none, minimal, low, medium, high, xhigh, max)"
                    );
                }
                if !model.support_efforts.is_empty()
                    && !model
                        .support_efforts
                        .iter()
                        .any(|e| e.eq_ignore_ascii_case(effort))
                {
                    anyhow::bail!(
                        "model {alias} default_effort '{effort}' is not listed in its support_efforts"
                    );
                }
            }
            for effort in &model.support_efforts {
                let effort = effort.trim();
                if !is_known_effort(effort) {
                    anyhow::bail!(
                        "model {alias} support_efforts contains unknown effort '{effort}' (expected one of: none, minimal, low, medium, high, xhigh, max)"
                    );
                }
            }
        }
        if !self.models.contains_key(default_model) {
            anyhow::bail!("default_model {default_model} is not present in [models]");
        }
        if let Some(quality) = self.quality_model.as_deref() {
            let quality = quality.trim();
            if !quality.is_empty() && !self.models.contains_key(quality) {
                anyhow::bail!("quality_model {quality} is not present in [models]");
            }
        }
        if let Some(secondary) = self.balance_model.as_deref() {
            let secondary = secondary.trim();
            if !secondary.is_empty() && !self.models.contains_key(secondary) {
                anyhow::bail!("balance_model {secondary} is not present in [models]");
            }
        }
        if let Some(fast) = self.fast_model.as_deref() {
            let fast = fast.trim();
            if !fast.is_empty() && !self.models.contains_key(fast) {
                anyhow::bail!("fast_model {fast} is not present in [models]");
            }
        }
        if let Some(compaction) = self.compaction_model.as_deref() {
            if !self.models.contains_key(compaction) {
                anyhow::bail!("compaction_model {compaction} is not present in [models]");
            }
        }
        if let Some(fallback) = self.fallback_model.as_deref() {
            if !self.models.contains_key(fallback) {
                anyhow::bail!("fallback_model {fallback} is not present in [models]");
            }
        }
        for alias in self.models.keys() {
            if is_symbolic_model_alias(alias) {
                anyhow::bail!(
                    "model alias '{alias}' collides with a reserved symbolic token \
                     (quality/balance/fast/current); rename it in [models]"
                );
            }
        }
        let mut seen_profiles: Vec<String> = Vec::new();
        for (profile, alias) in &self.subagent.default_models {
            let alias = alias.trim();
            if alias.is_empty() {
                anyhow::bail!("subagent.default_models.{profile} must not be empty when set");
            }
            let normalized = normalize_subagent_profile(profile);
            if let Some(existing) = seen_profiles.iter().find(|p| **p == normalized) {
                anyhow::bail!(
                    "subagent.default_models has case-insensitive duplicate keys: \
                     '{existing}' and '{profile}' normalize to '{normalized}'"
                );
            }
            seen_profiles.push(normalized);
            if is_symbolic_model_alias(alias) {
                continue;
            }
            if !self.models.contains_key(alias) {
                anyhow::bail!(
                    "subagent.default_models.{profile} = {alias} is not present in [models]"
                );
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
            if server.timeout_ms == Some(0) {
                anyhow::bail!("MCP server {name} timeout_ms must be greater than zero");
            }
            let transport = server
                .transport_type
                .as_deref()
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| {
                    if server.url.is_some() {
                        "http"
                    } else {
                        "stdio"
                    }
                });
            match transport {
                "stdio" if server.command.as_deref().unwrap_or("").trim().is_empty() => {
                    anyhow::bail!("MCP server {name} requires command for stdio transport")
                }
                "sse" | "http" | "streamable-http" => {
                    let url = server.url.as_deref().unwrap_or("").trim();
                    if url.is_empty() {
                        anyhow::bail!("MCP server {name} requires url for remote transport");
                    }
                    if !is_valid_http_url(url) {
                        anyhow::bail!(
                            "MCP server {name} url must start with http:// or https:// (got \"{url}\")"
                        );
                    }
                }
                "stdio" => {}
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

    /// The model designated as the multimodal vision proxy, if configured.
    ///
    /// Returns the alias plus owned clones of the model/provider configs so
    /// callers can build a standalone provider without borrowing `self`.
    pub fn vision_proxy(&self) -> Option<(String, ModelConfig, ProviderConfig)> {
        self.models
            .iter()
            .find(|(_, m)| m.experimental_vision_proxy)
            .and_then(|(alias, m)| {
                let provider = self.providers.get(&m.provider)?;
                Some((alias.clone(), m.clone(), provider.clone()))
            })
    }

    /// Resolve streaming first-token timeout for a model/provider pair.
    ///
    /// Priority: model (`Some(0)` disables) → provider (`Some(0)` disables) → default 60s.
    /// Values larger than the provider's opt-in `request_timeout_ms` are clamped
    /// just below it; with no total timeout configured the value is used as-is.
    pub fn resolve_first_token_timeout(
        model: &ModelConfig,
        provider: &ProviderConfig,
    ) -> Option<std::time::Duration> {
        resolve_first_token_timeout(model, provider)
    }

    /// The configured default model alias, trimmed; empty values are
    /// treated as unset. This is the terminal fallback for every symbolic
    /// model token.
    pub fn default_model_alias(&self) -> Option<&str> {
        self.default_model
            .as_deref()
            .map(str::trim)
            .filter(|alias| !alias.is_empty())
    }

    /// Expand a model token that may be a symbolic alias.
    ///
    /// - `quality` → `quality_model`, else `default_model`
    /// - `balance` → `balance_model`, else `default_model`
    /// - `current` → `current_model`, else `default_model`
    /// - `fast` → `fast_model`, else `balance_model`, else `default_model`
    /// - anything else → returned trimmed as-is
    pub fn expand_model_alias_token(&self, token: &str, current_model: Option<&str>) -> String {
        match token.trim().to_ascii_lowercase().as_str() {
            "quality" => self
                .quality_model
                .as_deref()
                .map(str::trim)
                .filter(|model| !model.is_empty())
                .map(|model| model.to_string())
                .or_else(|| self.default_model_alias().map(|model| model.to_string()))
                .unwrap_or_else(|| "default".into()),
            "balance" => self
                .balance_model
                .as_deref()
                .map(str::trim)
                .filter(|model| !model.is_empty())
                .map(|model| model.to_string())
                .or_else(|| self.default_model_alias().map(|model| model.to_string()))
                .unwrap_or_else(|| "default".into()),
            "current" => current_model
                .map(str::trim)
                .filter(|model| !model.is_empty())
                .map(|model| model.to_string())
                .or_else(|| self.default_model_alias().map(|model| model.to_string()))
                .unwrap_or_else(|| "default".into()),
            "fast" => self
                .fast_model
                .as_deref()
                .map(str::trim)
                .filter(|model| !model.is_empty())
                .map(|model| model.to_string())
                .or_else(|| {
                    self.balance_model
                        .as_deref()
                        .map(str::trim)
                        .filter(|model| !model.is_empty())
                        .map(|model| model.to_string())
                })
                .or_else(|| self.default_model_alias().map(|model| model.to_string()))
                .unwrap_or_else(|| "default".into()),
            _ => token.trim().to_string(),
        }
    }

    /// Resolve the model alias a subagent should run with.
    ///
    /// Priority: explicit tool override → `[subagent.default_models]` for the
    /// profile → global `balance_model` → `default_model`. Symbolic tokens
    /// (`quality` / `balance` / `current` / `fast`) are expanded using
    /// `current_model` (the parent session model).
    pub fn resolve_subagent_model(
        &self,
        profile: &str,
        explicit: Option<&str>,
        current_model: Option<&str>,
    ) -> String {
        let token = explicit
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .map(|model| model.to_string())
            .or_else(|| {
                self.subagent
                    .model_for_profile(profile)
                    .map(|model| model.to_string())
            })
            .or_else(|| {
                self.balance_model
                    .as_deref()
                    .map(str::trim)
                    .filter(|model| !model.is_empty())
                    .map(|model| model.to_string())
            })
            .or_else(|| self.default_model_alias().map(|model| model.to_string()))
            .unwrap_or_else(|| "default".into());
        self.expand_model_alias_token(&token, current_model)
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

/// Validate whether `url` is an absolute HTTP or HTTPS URL with a non-empty host.
pub fn is_valid_http_url(url: &str) -> bool {
    extract_http_url_host(url).is_some()
}

/// Extract the host part if `url` starts with http:// or https:// and has a non-empty host.
pub fn extract_http_url_host(url: &str) -> Option<&str> {
    let lower = url.to_ascii_lowercase();
    let rest = if lower.starts_with("http://") {
        &url["http://".len()..]
    } else if lower.starts_with("https://") {
        &url["https://".len()..]
    } else {
        return None;
    };
    if rest.is_empty() {
        return None;
    }
    let host = rest.split('/').next().unwrap_or("");
    if host.is_empty() {
        None
    } else {
        Some(host)
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
                api_key_env: None,
                base_url: Some("https://example.test".into()),
                custom_headers: HashMap::new(),
                oauth: None,
                first_token_timeout_ms: None,
                request_timeout_ms: None,
                extra_fields: BTreeMap::new(),
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
                experimental_vision_proxy: false,
                experimental_visible_empty_retries: 0,
                experimental_bad_toolcall_auto_retries: 0,
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
            experimental_vision_proxy: false,
            experimental_visible_empty_retries: 0,
            experimental_bad_toolcall_auto_retries: 0,
            first_token_timeout_ms: None,
        };
        let mut provider = ProviderConfig {
            provider_type: "openai".into(),
            api_key: None,
            api_key_env: None,
            base_url: None,
            custom_headers: HashMap::new(),
            oauth: None,
            first_token_timeout_ms: Some(30_000),
            request_timeout_ms: None,
            extra_fields: BTreeMap::new(),
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
        // No request_timeout_ms configured: the 300s first-token gate is kept.
        assert_eq!(
            resolve_first_token_timeout(&model, &provider),
            Some(std::time::Duration::from_millis(300_000))
        );
        // A total request timeout clamps the first-token gate just below it.
        provider.request_timeout_ms = Some(120_000);
        assert_eq!(
            resolve_first_token_timeout(&model, &provider),
            Some(std::time::Duration::from_millis(119_000))
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
    fn validates_compaction_model() {
        let mut config = valid_config();
        config.compaction_model = Some("missing/model".into());
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("compaction_model missing/model"));

        config.compaction_model = Some("test/model".into());
        config.validate().unwrap();
    }

    #[test]
    fn parses_and_validates_subagent_default_models() {
        let settings: SubagentSettings = toml::from_str(
            r#"
max_depth = 3
max_concurrent = 2

[default_models]
explore = "current"
Coder = "balance"
Researcher = "quality"
Reviewer = "balance"
fallback = "quality"
"#,
        )
        .unwrap();
        assert_eq!(settings.max_depth, 3);
        assert_eq!(settings.max_concurrent, 2);
        assert_eq!(settings.model_for_profile("explore"), Some("current"));
        assert_eq!(settings.model_for_profile("coder"), Some("balance"));
        assert_eq!(settings.model_for_profile("researcher"), Some("quality"));
        assert_eq!(settings.model_for_profile("reviewer"), Some("balance"));
        assert_eq!(settings.model_for_profile("agent"), Some("quality"));
        assert_eq!(settings.model_for_profile("unknown"), Some("quality"));

        let mut config = valid_config();
        config.subagent.default_models = settings.default_models.clone();
        config.validate().unwrap();

        config
            .subagent
            .default_models
            .insert("explore".into(), "missing/model".into());
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("subagent.default_models.explore = missing/model"));
    }

    #[test]
    fn resolve_subagent_model_priority() {
        let mut config = valid_config();
        config.balance_model = Some("test/secondary".into());
        config.models.insert(
            "test/secondary".into(),
            ModelConfig {
                provider: "test".into(),
                model: "secondary".into(),
                max_context_size: Some(100_000),
                max_output_size: Some(4_096),
                capabilities: vec!["tool_use".into()],
                display_name: None,
                support_efforts: Vec::new(),
                default_effort: None,
                pricing: None,
                experimental_adaptive_thinking: false,
                experimental_vision_proxy: false,
                experimental_visible_empty_retries: 0,
                experimental_bad_toolcall_auto_retries: 0,
                first_token_timeout_ms: None,
            },
        );
        config.models.insert(
            "test/explore".into(),
            ModelConfig {
                provider: "test".into(),
                model: "explore".into(),
                max_context_size: Some(100_000),
                max_output_size: Some(4_096),
                capabilities: vec!["tool_use".into()],
                display_name: None,
                support_efforts: Vec::new(),
                default_effort: None,
                pricing: None,
                experimental_adaptive_thinking: false,
                experimental_vision_proxy: false,
                experimental_visible_empty_retries: 0,
                experimental_bad_toolcall_auto_retries: 0,
                first_token_timeout_ms: None,
            },
        );
        config
            .subagent
            .default_models
            .insert("explore".into(), "test/explore".into());

        assert_eq!(
            config.resolve_subagent_model("explore", Some("test/model"), None),
            "test/model"
        );
        assert_eq!(
            config.resolve_subagent_model("explore", None, None),
            "test/explore"
        );
        assert_eq!(
            config.resolve_subagent_model("coder", Some(""), None),
            "test/secondary"
        );
        config.balance_model = None;
        assert_eq!(
            config.resolve_subagent_model("coder", None, None),
            "test/model"
        );
    }

    #[test]
    fn resolve_subagent_model_symbolic_aliases() {
        let mut config = valid_config();
        config.balance_model = Some("test/secondary".into());
        config.models.insert(
            "test/secondary".into(),
            ModelConfig {
                provider: "test".into(),
                model: "secondary".into(),
                max_context_size: Some(100_000),
                max_output_size: Some(4_096),
                capabilities: vec!["tool_use".into()],
                display_name: None,
                support_efforts: Vec::new(),
                default_effort: None,
                pricing: None,
                experimental_adaptive_thinking: false,
                experimental_vision_proxy: false,
                experimental_visible_empty_retries: 0,
                experimental_bad_toolcall_auto_retries: 0,
                first_token_timeout_ms: None,
            },
        );
        config.models.insert(
            "test/session".into(),
            ModelConfig {
                provider: "test".into(),
                model: "session".into(),
                max_context_size: Some(100_000),
                max_output_size: Some(4_096),
                capabilities: vec!["tool_use".into()],
                display_name: None,
                support_efforts: Vec::new(),
                default_effort: None,
                pricing: None,
                experimental_adaptive_thinking: false,
                experimental_vision_proxy: false,
                experimental_visible_empty_retries: 0,
                experimental_bad_toolcall_auto_retries: 0,
                first_token_timeout_ms: None,
            },
        );
        config
            .subagent
            .default_models
            .insert("explore".into(), "current".into());
        config
            .subagent
            .default_models
            .insert("coder".into(), "quality".into());
        config
            .subagent
            .default_models
            .insert("general".into(), "balance".into());
        config
            .subagent
            .default_models
            .insert("fallback".into(), "fast".into());
        config
            .subagent
            .default_models
            .insert("reviewer".into(), "quality".into());
        // quality_model is distinct from default_model when configured.
        config.quality_model = Some("test/quality".into());
        config.models.insert(
            "test/quality".into(),
            ModelConfig {
                provider: "test".into(),
                model: "quality".into(),
                max_context_size: Some(100_000),
                max_output_size: Some(4_096),
                capabilities: vec!["tool_use".into()],
                display_name: None,
                support_efforts: Vec::new(),
                default_effort: None,
                pricing: None,
                experimental_adaptive_thinking: false,
                experimental_vision_proxy: false,
                experimental_visible_empty_retries: 0,
                experimental_bad_toolcall_auto_retries: 0,
                first_token_timeout_ms: None,
            },
        );

        assert_eq!(
            config.resolve_subagent_model("explore", None, Some("test/session")),
            "test/session"
        );
        assert_eq!(
            config.resolve_subagent_model("explore", None, None),
            "test/model"
        );
        assert_eq!(
            config.resolve_subagent_model("coder", None, Some("test/session")),
            "test/quality"
        );
        assert_eq!(
            config.resolve_subagent_model("general", None, Some("test/session")),
            "test/secondary"
        );
        // `quality` pins quality_model, ignoring the parent session model.
        assert_eq!(
            config.resolve_subagent_model("reviewer", None, Some("test/session")),
            "test/quality"
        );
        assert_eq!(
            config.resolve_subagent_model("explore", Some("balance"), Some("test/session")),
            "test/secondary"
        );
        assert_eq!(
            config.resolve_subagent_model("explore", Some("CURRENT"), Some("test/session")),
            "test/session"
        );
        // `fast` falls back to balance_model when fast_model is unset.
        assert_eq!(
            config.resolve_subagent_model("unknown-profile", None, None),
            "test/secondary"
        );
        // Explicit `fast` follows the same chain: fast_model → secondary → default.
        assert_eq!(
            config.resolve_subagent_model("explore", Some("fast"), None),
            "test/secondary"
        );

        config.fast_model = Some("test/fast".into());
        assert_eq!(
            config.resolve_subagent_model("unknown-profile", None, None),
            "test/fast"
        );
        assert_eq!(
            config.resolve_subagent_model("explore", Some("FAST"), None),
            "test/fast"
        );
        // Explicit quality overrides any profile default and pins
        // quality_model (falling back to default_model when unset).
        assert_eq!(
            config.resolve_subagent_model("explore", Some("quality"), Some("test/session")),
            "test/quality"
        );
        config.quality_model = None;
        assert_eq!(
            config.resolve_subagent_model("explore", Some("quality"), Some("test/session")),
            "test/model"
        );
        // Balance pins the mid-tier balance_model.
        assert_eq!(
            config.resolve_subagent_model("explore", Some("BALANCE"), Some("test/session")),
            "test/secondary"
        );
        // Balance falls back to default_model when balance_model is unset.
        config.balance_model = None;
        assert_eq!(
            config.resolve_subagent_model("explore", Some("balance"), None),
            "test/model"
        );

        config.fast_model = None;
        assert_eq!(
            config.resolve_subagent_model("unknown-profile", None, None),
            "test/model"
        );
    }

    #[test]
    fn validates_fast_model() {
        let mut config = valid_config();
        config.fast_model = Some("missing/fast".into());
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("fast_model missing/fast is not present"));
    }

    #[test]
    fn validates_model_default_effort_against_known_and_supported_efforts() {
        let mut config = valid_config();
        // Unknown effort vocabulary is rejected.
        config.models.get_mut("test/model").unwrap().default_effort = Some("ultra".into());
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("default_effort 'ultra' is not a known reasoning effort"));

        // Effort outside the model's declared support_efforts is rejected.
        let mut config = valid_config();
        let model = config.models.get_mut("test/model").unwrap();
        model.support_efforts = vec!["low".into(), "medium".into()];
        model.default_effort = Some("xhigh".into());
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("default_effort 'xhigh' is not listed in its support_efforts"));

        // An effort listed in support_efforts passes.
        let mut config = valid_config();
        let model = config.models.get_mut("test/model").unwrap();
        model.support_efforts = vec!["none".into(), "low".into(), "medium".into()];
        model.default_effort = Some("medium".into());
        config.validate().unwrap();

        // support_efforts itself is validated against the known vocabulary.
        let mut config = valid_config();
        config.models.get_mut("test/model").unwrap().support_efforts = vec!["turbo".into()];
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("support_efforts contains unknown effort 'turbo'"));
    }

    #[test]
    fn validates_reserved_model_alias_names() {
        let mut config = valid_config();
        config.models.insert(
            "current".into(),
            config.models.get("test/model").cloned().unwrap(),
        );
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("collides with a reserved symbolic token"));

        let mut config = valid_config();
        config.models.insert(
            "Fast".into(),
            config.models.get("test/model").cloned().unwrap(),
        );
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("collides with a reserved symbolic token"));
    }

    #[test]
    fn validates_default_models_case_duplicates() {
        let mut config = valid_config();
        config
            .subagent
            .default_models
            .insert("general".into(), "current".into());
        config
            .subagent
            .default_models
            .insert("General".into(), "balance".into());
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("case-insensitive duplicate keys"));

        // `agent` normalizes to `general`, which must also collide.
        let mut config = valid_config();
        config
            .subagent
            .default_models
            .insert("general".into(), "current".into());
        config
            .subagent
            .default_models
            .insert("agent".into(), "balance".into());
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("case-insensitive duplicate keys"));
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
experimental_bad_toolcall_auto_retries = 2
"#,
        )
        .unwrap();
        assert!(model.experimental_adaptive_thinking);
        assert_eq!(model.experimental_visible_empty_retries, 1);
        assert_eq!(model.experimental_bad_toolcall_auto_retries, 2);
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
    fn parses_multiple_plugin_marketplaces() {
        let config: AppConfig = toml::from_str(
            r#"
plugin_marketplace = "https://a.example/marketplace.json"
plugin_marketplaces = [
  "https://b.example/marketplace.json",
  { name = "internal", source = "http://10.10.10.205:8091/bjc/kk-plugins" },
]
"#,
        )
        .unwrap();
        let catalogs = config.plugin_marketplace_catalogs();
        assert_eq!(catalogs.len(), 3);
        assert_eq!(catalogs[0].source, "https://a.example/marketplace.json");
        assert_eq!(catalogs[1].source, "https://b.example/marketplace.json");
        assert_eq!(catalogs[2].name.as_deref(), Some("internal"));
        assert_eq!(
            catalogs[2].source,
            "http://10.10.10.205:8091/bjc/kk-plugins"
        );
    }

    #[test]
    fn rejects_empty_plugin_marketplaces_source() {
        let mut config = valid_config();
        config.plugin_marketplaces = vec![PluginMarketplaceSpec::Source("  ".into())];
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("plugin_marketplaces[0]"));
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

    fn add_model(config: &mut AppConfig, alias: &str, capabilities: Vec<String>) {
        config.models.insert(
            alias.into(),
            ModelConfig {
                provider: "test".into(),
                model: alias.into(),
                max_context_size: Some(100_000),
                max_output_size: Some(4_096),
                capabilities,
                display_name: None,
                support_efforts: Vec::new(),
                default_effort: None,
                pricing: None,
                experimental_adaptive_thinking: false,
                experimental_vision_proxy: false,
                experimental_visible_empty_retries: 0,
                experimental_bad_toolcall_auto_retries: 0,
                first_token_timeout_ms: None,
            },
        );
    }

    #[test]
    fn vision_proxy_requires_image_capability() {
        let mut config = valid_config();
        add_model(&mut config, "test/vision", Vec::new());
        config
            .models
            .get_mut("test/vision")
            .unwrap()
            .experimental_vision_proxy = true;
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string()
                .contains("experimental_vision_proxy without an image input capability"),
            "got: {err}"
        );
    }

    #[test]
    fn vision_proxy_access_returns_resolved_configs() {
        let mut config = valid_config();
        add_model(&mut config, "test/vision", vec!["image_in".into()]);
        config
            .models
            .get_mut("test/vision")
            .unwrap()
            .experimental_vision_proxy = true;
        config.validate().unwrap();
        let (alias, model, _provider) = config.vision_proxy().expect("proxy configured");
        assert_eq!(alias, "test/vision");
        assert!(model.experimental_vision_proxy);
    }

    #[test]
    fn declares_image_input_matches_capability_aliases() {
        assert!(declares_image_input(&["image_in".into()]));
        assert!(declares_image_input(&["vision".into()]));
        assert!(declares_image_input(&["multimodal".into()]));
        assert!(!declares_image_input(&["tool_use".into()]));
        assert!(!declares_image_input(&[]));
    }

    #[test]
    fn validates_mcp_remote_url_scheme() {
        // Missing scheme
        let mut config = valid_config();
        config.mcp_servers.insert(
            "remote_no_scheme".into(),
            McpServerConfig {
                transport_type: Some("sse".into()),
                url: Some("localhost:8080/mcp".into()),
                ..Default::default()
            },
        );
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains(
            "MCP server remote_no_scheme url must start with http:// or https:// (got \"localhost:8080/mcp\")"
        ));

        // file:// scheme rejected
        let mut config = valid_config();
        config.mcp_servers.insert(
            "remote_file".into(),
            McpServerConfig {
                transport_type: Some("http".into()),
                url: Some("file:///path/to/server".into()),
                ..Default::default()
            },
        );
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains(
            "MCP server remote_file url must start with http:// or https:// (got \"file:///path/to/server\")"
        ));

        // Implicit http when transport_type is None but url is Some
        let mut config = valid_config();
        config.mcp_servers.insert(
            "remote_implicit_bad".into(),
            McpServerConfig {
                transport_type: None,
                url: Some("ftp://example.com".into()),
                ..Default::default()
            },
        );
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains(
            "MCP server remote_implicit_bad url must start with http:// or https:// (got \"ftp://example.com\")"
        ));

        // http://host/mcp accepted
        let mut config = valid_config();
        config.mcp_servers.insert(
            "remote_http".into(),
            McpServerConfig {
                transport_type: Some("streamable-http".into()),
                url: Some("http://host/mcp".into()),
                ..Default::default()
            },
        );
        assert!(config.validate().is_ok());

        // https://host accepted
        let mut config = valid_config();
        config.mcp_servers.insert(
            "remote_https".into(),
            McpServerConfig {
                transport_type: None,
                url: Some("https://host".into()),
                ..Default::default()
            },
        );
        assert!(config.validate().is_ok());
    }

    #[test]
    fn validates_mcp_timeout_ms() {
        let mut config = valid_config();
        config.mcp_servers.insert(
            "srv".into(),
            McpServerConfig {
                command: Some("echo".into()),
                timeout_ms: Some(0),
                ..Default::default()
            },
        );
        let err = config.validate().unwrap_err();
        assert!(err
            .to_string()
            .contains("MCP server srv timeout_ms must be greater than zero"));

        let mut config = valid_config();
        config.mcp_servers.insert(
            "srv".into(),
            McpServerConfig {
                command: Some("echo".into()),
                timeout_ms: Some(1),
                ..Default::default()
            },
        );
        assert!(config.validate().is_ok());
    }
}
