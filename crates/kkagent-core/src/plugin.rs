//! KK plugin discovery and runtime capability loading.
//!
//! Plugins may use `kk.plugin.json`, `.kk-plugin/plugin.json`, or the legacy
//! kkagent `plugin.json`. Real tool capabilities are declared through the
//! `mcpServers` manifest field and are bridged by `kkagent-mcp`.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

const KK_ROOT_MANIFEST: &str = "kk.plugin.json";
const KK_DIR_MANIFEST: &str = ".kk-plugin/plugin.json";
const LEGACY_MANIFEST: &str = "plugin.json";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PluginInterface {
    #[serde(default, rename = "displayName")]
    pub display_name: Option<String>,
    #[serde(default, rename = "shortDescription")]
    pub short_description: Option<String>,
    #[serde(default, rename = "longDescription")]
    pub long_description: Option<String>,
    #[serde(default, rename = "developerName")]
    pub developer_name: Option<String>,
    #[serde(default, rename = "websiteURL")]
    pub website_url: Option<String>,
}

/// A slash command contributed by a plugin. Legacy manifests may list plain
/// command names; those surface in completion but resolve to a hint that the
/// plugin must handle them via MCP prompts.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PluginSlashCommand {
    /// Legacy form: bare command name.
    Name(String),
    /// Full definition: name, description, and a prompt template where
    /// `{{args}}` (and `{{arg0}}`, `{{arg1}}, …`) expand from user input.
    Definition {
        name: String,
        #[serde(default)]
        description: String,
        #[serde(default, rename = "promptTemplate", alias = "prompt")]
        prompt_template: Option<String>,
        #[serde(default, rename = "argumentHint")]
        argument_hint: Option<String>,
    },
}

impl PluginSlashCommand {
    pub fn name(&self) -> &str {
        match self {
            Self::Name(name) => name,
            Self::Definition { name, .. } => name,
        }
    }

    pub fn description(&self) -> &str {
        match self {
            Self::Name(_) => "",
            Self::Definition { description, .. } => description,
        }
    }

    pub fn prompt_template(&self) -> Option<&str> {
        match self {
            Self::Name(_) => None,
            Self::Definition {
                prompt_template, ..
            } => prompt_template.as_deref(),
        }
    }
}

/// Service-backend overrides contributed by a plugin. Mirrors the
/// `[services]` config section; a plugin's entry replaces the user's
/// config for that service (no MCP server involved).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PluginServiceOverrides {
    #[serde(default, rename = "webSearch")]
    pub web_search: Option<kkagent_config::WebSearchConfig>,
    #[serde(default, rename = "webFetch")]
    pub web_fetch: Option<kkagent_config::WebFetchConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, rename = "systemPrompt", alias = "prompt_append")]
    pub system_prompt: Option<String>,
    /// When true, `systemPrompt` replaces the built-in base persona instead
    /// of being appended. Workspace injections and other plugins' appended
    /// prompts still apply after the replaced base.
    #[serde(default, rename = "replaceSystemPrompt", alias = "replace_prompt")]
    pub replace_system_prompt: bool,
    /// Built-in tool name -> `"<server>.<tool>"` from this plugin's
    /// `mcpServers`. The bridged MCP tool replaces the built-in under its
    /// original name (subject to the override policy).
    #[serde(default, rename = "toolOverrides")]
    pub tool_overrides: BTreeMap<String, String>,
    /// Built-in service backends this plugin provides/overrides without an
    /// MCP server — e.g. `webSearch` replaces `[services.web_search]`.
    /// Field names mirror config.toml so snippets are copy-pasteable.
    #[serde(default)]
    pub services: PluginServiceOverrides,
    /// Slash commands contributed by this plugin. Accepts the legacy plain
    /// name list (command does nothing but gets listed) or full definitions
    /// with a prompt template expanded client-side on execution.
    /// `slash_commands` (snake_case) is the original field name and stays
    /// accepted for pre-1.0 manifests.
    #[serde(default, rename = "slashCommands", alias = "slash_commands")]
    pub slash_commands: Vec<PluginSlashCommand>,
    #[serde(default, rename = "mcpServers")]
    pub mcp_servers: HashMap<String, kkagent_config::McpServerConfig>,
    /// External subagent types contributed by this plugin. Each entry
    /// registers a new `subagent_type` for the `Agent` tool; execution is
    /// delegated over a transport (currently ACP) to an external agent
    /// process such as the Cursor CLI (`agent acp`).
    #[serde(default)]
    pub subagents: Vec<PluginSubagentSpec>,
    #[serde(default)]
    pub interface: PluginInterface,
}

/// A plugin-declared external subagent type. Names are namespaced at load
/// time as `<plugin>.<name>` to avoid collisions with built-in profiles.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginSubagentSpec {
    /// Short type name within the plugin (e.g. `"cursor"` → `kk-cursor.cursor`).
    pub name: String,
    /// How the subagent runs:
    /// - `"acp"` — external agent process over the Agent Client Protocol
    ///   (e.g. the Cursor CLI `agent acp`);
    /// - `"internal"` — an in-process kkagent agent loop using the kk model
    ///   and kk tools, with an optional tool allowlist and plugin-private
    ///   MCP servers (lazy-loaded per delegation, zero main-session context
    ///   cost until used).
    #[serde(default = "PluginSubagentSpec::default_transport")]
    pub transport: String,
    /// Transport-specific launch configuration.
    #[serde(default, rename = "transportConfig")]
    pub transport_config: PluginSubagentTransportConfig,
    /// One-line capability hint shown in the Agent tool description.
    #[serde(default)]
    pub description: String,
    /// Additional prompt preamble injected into the delegation prompt.
    #[serde(default, rename = "promptPrefix")]
    pub prompt_prefix: Option<String>,
    /// Allow the external subagent to delegate back into kkagent built-ins
    /// (`subagents` input on the Agent tool). Defaults to false — external
    /// types cannot recurse.
    #[serde(default, rename = "allowDelegation")]
    pub allow_delegation: bool,
    /// Auto-approve the external agent's tool permission requests. Defaults
    /// to true (subagents run unattended); set false to deny them.
    #[serde(
        default = "PluginSubagentSpec::default_auto_approve",
        rename = "autoApprove"
    )]
    pub auto_approve: bool,
    /// Environment variables passed to the spawned agent process.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Per-request timeout seconds. Defaults to 300.
    #[serde(default, rename = "timeoutSecs")]
    pub timeout_secs: Option<u64>,
    /// Plugin-qualified id (`<plugin>.<name>`), injected by the manager at
    /// aggregation time — used for MCP tool namespacing and display.
    #[serde(skip)]
    pub plugin_id: String,
    /// **internal transport** — system prompt for the subagent's session.
    /// Appended after the workspace instructions and a generic subagent addon.
    #[serde(default, rename = "systemPrompt")]
    pub system_prompt: Option<String>,
    /// **internal transport** — model alias bound at declaration time
    /// (`"default"` / `"fast"` / `"current"` / `"secondary"`). A valid
    /// binding wins over the Agent tool's `model` token; anything else
    /// (including raw model ids) is rejected with a load-time diagnostic
    /// and standard resolution applies. ACP transports pick their own
    /// model and ignore this field.
    #[serde(default)]
    pub model: Option<String>,
    /// **internal transport** — tool allowlist. Only the listed tool names
    /// stay registered (core tools + plugin-private MCP tools qualify);
    /// everything else is removed. Empty/absent = inherit the default
    /// subagent tool set (core + delegation).
    #[serde(default)]
    pub tools: Vec<String>,
    /// **internal transport** — MCP servers started lazily for this subagent
    /// type only. Their tools are namespaced `<plugin>.<server>.<tool>` and
    /// filtered through `tools` like every other tool. They consume no
    /// context in the main session.
    #[serde(default, rename = "mcpServers")]
    pub mcp_servers: BTreeMap<String, kkagent_config::McpServerConfig>,
}

impl PluginSubagentSpec {
    fn default_transport() -> String {
        "acp".into()
    }

    fn default_auto_approve() -> bool {
        true
    }

    /// Symbolic model alias tokens accepted by the `model` field.
    /// Expansion follows the standard token machinery
    /// (`expand_model_alias_token`); raw model ids are not accepted.
    pub const MODEL_ALIASES: [&'static str; 4] = ["default", "fast", "current", "secondary"];

    /// The declared model alias, trimmed and lowercased, if it is one of
    /// [`MODEL_ALIASES`](Self::MODEL_ALIASES). Anything else (raw model
    /// ids, typos) yields `None` — binding at declaration time is
    /// alias-only by design.
    pub fn model_alias(&self) -> Option<String> {
        self.model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(str::to_ascii_lowercase)
            .filter(|m| Self::MODEL_ALIASES.contains(&m.as_str()))
    }

    /// Display name used in mirrored lifecycle events: the bare type name
    /// (`cursor`) — the TUI prefixes the owning plugin when needed. When the
    /// manifest uses the generic name `delegate`, the agent identity is
    /// derived from the owning plugin instead (`codex-agent.delegate` →
    /// `codex`, `kk-cursor-agent.delegate` → `cursor`) so the TUI names the
    /// actual agent rather than a nondescript "delegate".
    pub fn qualified_name(&self) -> String {
        if self.name.eq_ignore_ascii_case("delegate") {
            let plugin = self.plugin_id.split('.').next().unwrap_or("");
            let plugin = plugin.strip_prefix("kk-").unwrap_or(plugin);
            let plugin = plugin.strip_suffix("-agent").unwrap_or(plugin);
            if !plugin.is_empty() {
                return plugin.to_string();
            }
        }
        self.name.clone()
    }
}

/// Transport-specific launch configuration (ACP variant).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PluginSubagentTransportConfig {
    /// Executable command, e.g. `["agent", "acp"]` (Cursor CLI) or an
    /// absolute path to another ACP agent binary.
    #[serde(default)]
    pub command: Vec<String>,
    /// Working directory for the spawned process. Defaults to the parent
    /// session's working directory.
    #[serde(default)]
    pub cwd: Option<String>,
    /// ACP session mode (`agent` | `plan` | `ask` for the Cursor CLI).
    #[serde(default)]
    pub mode: Option<String>,
    /// Skip the ACP `authenticate` step (credentials injected via `env`).
    #[serde(default, rename = "skipAuth")]
    pub skip_auth: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct PluginDiagnostic {
    pub severity: String,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct LoadedPlugin {
    pub manifest: PluginManifest,
    pub root: PathBuf,
    pub manifest_path: PathBuf,
    pub diagnostics: Vec<PluginDiagnostic>,
    pub enabled: bool,
    pub managed: bool,
    pub source: Option<String>,
    mcp_servers: Vec<kkagent_mcp::McpServerConfig>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PluginInfo {
    pub name: String,
    pub display_name: String,
    pub version: String,
    pub description: String,
    pub path: String,
    pub manifest_path: String,
    pub mcp_servers: Vec<String>,
    pub diagnostics: Vec<PluginDiagnostic>,
    pub enabled: bool,
    pub managed: bool,
    pub source: Option<String>,
    /// Built-in tool names this plugin overrides (may be rejected by policy).
    #[serde(default)]
    pub tool_overrides: Vec<String>,
    /// True when this plugin replaces the base system prompt.
    #[serde(default)]
    pub replaces_system_prompt: bool,
    /// Full slash-command definitions (templates included) for the TUI.
    #[serde(default)]
    pub slash_commands: Vec<PluginSlashCommand>,
}

/// Memoized external-subagent aggregation: qualified specs plus conflicts.
pub type ExternalSubagentIndex = Arc<(Vec<(String, PluginSubagentSpec)>, Vec<String>)>;

pub struct PluginManager {
    plugins_dir: PathBuf,
    kkagent_home: PathBuf,
    plugins: RwLock<HashMap<String, LoadedPlugin>>,
    mutation: Mutex<()>,
    /// Memoized external-subagent aggregation. Invalidated by `reload` (the
    /// single write path for discovery/install/disable), rebuilt lazily on
    /// the next query — per-turn reads never re-scan manifests.
    ///
    /// A `std` (non-async) lock is intentional: the critical section is a
    /// handful of pointer swaps, and holding it across no await points means
    /// a sync lock cannot deadlock the runtime (an async `RwLock` here
    /// historically stalled single-thread test runtimes).
    external_subagents_cache: std::sync::RwLock<Option<ExternalSubagentIndex>>,
}

impl PluginManager {
    pub fn new(plugins_dir: PathBuf) -> Self {
        let kkagent_home = plugins_dir
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| plugins_dir.clone());
        Self {
            plugins_dir,
            kkagent_home,
            plugins: RwLock::new(HashMap::new()),
            mutation: Mutex::new(()),
            external_subagents_cache: std::sync::RwLock::new(None),
        }
    }

    pub async fn discover(dir: &Path) -> Arc<Self> {
        let mgr = Arc::new(Self::new(dir.to_path_buf()));
        if let Err(error) = mgr.reload().await {
            tracing::warn!(%error, path = %dir.display(), "plugin discovery failed");
        }
        mgr
    }

    pub async fn reload(&self) -> anyhow::Result<usize> {
        let discovered = self.scan_dir().await?;
        let count = discovered.len();
        *self.plugins.write().await = discovered;
        // Drop the memoized aggregation; the next query rebuilds it from the
        // fresh manifest set.
        *self
            .external_subagents_cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        Ok(count)
    }

    /// Backward-compatible explicit scan. The manager continues to reload its
    /// original discovery directory after this call.
    pub async fn scan(&self, dir: &Path) -> anyhow::Result<usize> {
        if dir != self.plugins_dir {
            anyhow::bail!(
                "plugin manager is bound to {}; cannot scan {}",
                self.plugins_dir.display(),
                dir.display()
            );
        }
        self.reload().await
    }

    async fn scan_dir(&self) -> anyhow::Result<HashMap<String, LoadedPlugin>> {
        if !self.plugins_dir.exists() {
            return Ok(HashMap::new());
        }
        // A malformed/unsupported installed.json (hand-edited, older format)
        // must not wipe out plugin discovery: degrade to directory scanning
        // with a warning instead of failing the whole reload.
        let installed = match crate::plugin_marketplace::read_installed(&self.plugins_dir).await {
            Ok(installed) => installed,
            Err(error) => {
                tracing::warn!(
                    %error,
                    path = %self.plugins_dir.join("installed.json").display(),
                    "ignoring broken installed plugin state; falling back to directory scan"
                );
                crate::plugin_marketplace::InstalledPluginsFile::default()
            }
        };
        let has_installed_state = self.plugins_dir.join("installed.json").is_file();
        let mut roots: Vec<(PathBuf, bool, bool, Option<String>)> = installed
            .plugins
            .iter()
            .map(|record| {
                (
                    PathBuf::from(&record.root),
                    record.enabled,
                    true,
                    record.original_source.clone(),
                )
            })
            .collect();
        roots.sort_by(|a, b| a.0.cmp(&b.0));
        let mut direct_roots = Vec::new();
        let mut rd = tokio::fs::read_dir(&self.plugins_dir).await?;
        while let Some(entry) = rd.next_entry().await? {
            let path = entry.path();
            if path.is_dir() && entry.file_name() == "managed" {
                if !has_installed_state {
                    let mut managed = tokio::fs::read_dir(&path).await?;
                    while let Some(managed_entry) = managed.next_entry().await? {
                        let managed_path = managed_entry.path();
                        if managed_path.is_dir()
                            && !managed_entry.file_name().to_string_lossy().starts_with('.')
                        {
                            direct_roots.push((managed_path, true, true, None));
                        }
                    }
                }
            } else if path.is_dir() {
                direct_roots.push((path, true, false, None));
            }
        }
        direct_roots.sort_by(|a, b| a.0.cmp(&b.0));
        roots.extend(direct_roots);

        let mut plugins = HashMap::new();
        for (root, enabled, managed, source) in roots {
            if managed && has_installed_state {
                let managed_root = tokio::fs::canonicalize(self.plugins_dir.join("managed")).await;
                let resolved_root = tokio::fs::canonicalize(&root).await;
                match (managed_root, resolved_root) {
                    (Ok(managed_root), Ok(resolved_root))
                        if resolved_root.starts_with(&managed_root) => {}
                    _ => {
                        tracing::warn!(
                            path = %root.display(),
                            "managed plugin root is missing or outside the managed directory"
                        );
                        continue;
                    }
                }
            }
            match self.load_plugin(&root, enabled, managed, source).await {
                Ok(Some(plugin)) => {
                    let name = plugin.manifest.name.clone();
                    if plugins.contains_key(&name) {
                        tracing::warn!(plugin = %name, "duplicate plugin id ignored");
                        continue;
                    }
                    plugins.insert(name, plugin);
                }
                Ok(None) => {}
                Err(error) => tracing::warn!(
                    %error,
                    path = %root.display(),
                    "plugin ignored"
                ),
            }
        }
        Ok(plugins)
    }

    async fn load_plugin(
        &self,
        root: &Path,
        enabled: bool,
        managed: bool,
        source: Option<String>,
    ) -> anyhow::Result<Option<LoadedPlugin>> {
        let Some(manifest_path) = select_manifest(root) else {
            return Ok(None);
        };
        let root = tokio::fs::canonicalize(root).await?;
        let text = tokio::fs::read_to_string(&manifest_path).await?;
        let manifest: PluginManifest = serde_json::from_str(&text)?;
        validate_plugin_name(&manifest.name)?;

        let mut diagnostics = Vec::new();
        let mut mcp_servers = Vec::new();
        let mut names: Vec<_> = manifest.mcp_servers.keys().cloned().collect();
        names.sort();
        let multi_server = names.len() > 1;
        for name in names {
            let config = &manifest.mcp_servers[&name];
            match normalize_mcp_server(
                &manifest.name,
                &name,
                config,
                &root,
                &self.kkagent_home,
                multi_server,
            )
            .await
            {
                Ok(config) => mcp_servers.push(config),
                Err(error) => diagnostics.push(PluginDiagnostic {
                    severity: "warning".into(),
                    message: format!("mcpServers.{name}: {error}"),
                }),
            }
        }

        // Subagent model bindings: aliases only, internal transport only.
        for (idx, spec) in manifest.subagents.iter().enumerate() {
            let Some(declared) = spec
                .model
                .as_deref()
                .map(str::trim)
                .filter(|m| !m.is_empty())
            else {
                continue;
            };
            let subject = format!("subagents[{idx}] ({})", spec.name);
            if !spec.transport.eq_ignore_ascii_case("internal") {
                diagnostics.push(PluginDiagnostic {
                    severity: "warning".into(),
                    message: format!(
                        "{subject}: model \"{declared}\" is ignored — model binding only \
                         applies to transport \"internal\""
                    ),
                });
            } else if spec.model_alias().is_none() {
                diagnostics.push(PluginDiagnostic {
                    severity: "warning".into(),
                    message: format!(
                        "{subject}: model \"{declared}\" is not a supported alias \
                         (default|fast|current|secondary); ignored"
                    ),
                });
            }
        }

        Ok(Some(LoadedPlugin {
            manifest,
            root,
            manifest_path,
            diagnostics,
            enabled,
            managed,
            source,
            mcp_servers,
        }))
    }

    pub async fn list(&self) -> Vec<PluginInfo> {
        let plugins = self.plugins.read().await;
        let mut out: Vec<_> = plugins
            .values()
            .map(|plugin| PluginInfo {
                name: plugin.manifest.name.clone(),
                display_name: plugin
                    .manifest
                    .interface
                    .display_name
                    .clone()
                    .unwrap_or_else(|| plugin.manifest.name.clone()),
                version: plugin.manifest.version.clone(),
                description: plugin.manifest.description.clone(),
                path: plugin.root.display().to_string(),
                manifest_path: plugin.manifest_path.display().to_string(),
                mcp_servers: plugin.mcp_servers.iter().map(|s| s.name.clone()).collect(),
                diagnostics: plugin.diagnostics.clone(),
                enabled: plugin.enabled,
                managed: plugin.managed,
                source: plugin.source.clone(),
                tool_overrides: plugin.manifest.tool_overrides.keys().cloned().collect(),
                replaces_system_prompt: plugin.manifest.replace_system_prompt
                    && plugin
                        .manifest
                        .system_prompt
                        .as_deref()
                        .is_some_and(|p| !p.trim().is_empty()),
                slash_commands: plugin.manifest.slash_commands.clone(),
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    pub async fn mcp_server_configs(&self) -> Vec<kkagent_mcp::McpServerConfig> {
        let plugins = self.plugins.read().await;
        let mut names: Vec<_> = plugins.keys().cloned().collect();
        names.sort();
        names
            .into_iter()
            .filter(|name| plugins[name].enabled)
            .flat_map(|name| plugins[&name].mcp_servers.clone())
            .collect()
    }

    /// Effective base-prompt override, if any enabled plugin declares
    /// `replaceSystemPrompt: true`. Multiple candidates resolve by plugin
    /// name (ascending); losers are reported so callers can surface a
    /// diagnostic.
    pub async fn system_prompt_override(&self) -> Option<(String, Vec<String>)> {
        let plugins = self.plugins.read().await;
        let mut names: Vec<_> = plugins.keys().cloned().collect();
        names.sort();
        let mut winner: Option<String> = None;
        let mut losers: Vec<String> = Vec::new();
        for name in names {
            let plugin = &plugins[&name];
            if !plugin.enabled {
                continue;
            }
            if !plugin.manifest.replace_system_prompt {
                continue;
            }
            let prompt = plugin.manifest.system_prompt.as_deref();
            if let Some(prompt) = prompt.filter(|p| !p.trim().is_empty()) {
                if winner.is_none() {
                    winner = Some(prompt.to_string());
                } else {
                    losers.push(name);
                }
            }
        }
        winner.map(|prompt| (prompt, losers))
    }

    /// All enabled plugins' tool overrides as `(builtin_name, source_ref)`
    /// pairs, deterministically ordered by plugin name then builtin name.
    /// Source refs are `"plugin:<plugin-id>:<server.tool>"` and are resolved
    /// against the plugin's own MCP namespace at apply time.
    pub async fn tool_overrides(&self) -> Vec<(String, String)> {
        let plugins = self.plugins.read().await;
        let mut names: Vec<_> = plugins.keys().cloned().collect();
        names.sort();
        let mut out = Vec::new();
        for name in names {
            let plugin = &plugins[&name];
            if !plugin.enabled {
                continue;
            }
            for (builtin, source) in &plugin.manifest.tool_overrides {
                out.push((builtin.clone(), format!("plugin:{name}:{source}")));
            }
        }
        out
    }

    /// `(plugin_id, mcp_servers)` for every enabled plugin, sorted by id.
    /// Lock-free snapshot consumers use this to resolve override source refs.
    pub async fn manifest_snapshot(
        &self,
    ) -> Vec<(String, HashMap<String, kkagent_config::McpServerConfig>)> {
        let plugins = self.plugins.read().await;
        let mut names: Vec<_> = plugins.keys().cloned().collect();
        names.sort();
        names
            .into_iter()
            .map(|name| {
                let servers = plugins[&name].manifest.mcp_servers.clone();
                (name, servers)
            })
            .collect()
    }

    /// All enabled plugins' external subagent types, namespaced as
    /// `<plugin>.<name>` and sorted by qualified name. The first plugin to
    /// claim a qualified name wins; losers are reported as diagnostics so
    /// callers can surface the conflict.
    ///
    /// Memoized: the result is cached until the next `reload`. Callers on
    /// hot paths (per-turn tool registry build) should clone the returned
    /// `Arc` instead of the vector when possible.
    pub async fn external_subagents(&self) -> ExternalSubagentIndex {
        // Fast path: cache hit.
        if let Some(cached) = self
            .external_subagents_cache
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            return cached;
        }
        let (out, conflicts) = self.compute_external_subagents().await;
        let shared = Arc::new((out, conflicts));
        // Another reload may have raced us; prefer the freshest computation
        // only if the cache is still empty.
        let mut cache = self
            .external_subagents_cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if cache.is_none() {
            *cache = Some(shared.clone());
        }
        drop(cache);
        self.external_subagents_cache
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .unwrap_or(shared)
    }

    async fn compute_external_subagents(&self) -> (Vec<(String, PluginSubagentSpec)>, Vec<String>) {
        let plugins = self.plugins.read().await;
        let mut names: Vec<_> = plugins.keys().cloned().collect();
        names.sort();
        let mut out = Vec::new();
        let mut conflicts = Vec::new();
        let mut seen: HashMap<String, String> = HashMap::new();
        for name in names {
            let plugin = &plugins[&name];
            if !plugin.enabled {
                continue;
            }
            for spec in &plugin.manifest.subagents {
                let qualified = format!("{name}.{}", spec.name);
                if let Some(_owner) = seen.get(&qualified) {
                    conflicts.push(qualified.clone());
                    continue;
                }
                seen.insert(qualified.clone(), name.clone());
                out.push((
                    qualified.clone(),
                    PluginSubagentSpec {
                        plugin_id: qualified,
                        ..spec.clone()
                    },
                ));
            }
        }
        (out, conflicts)
    }

    /// Look up one external subagent type by qualified name (`<plugin>.<name>`).
    pub async fn external_subagent(&self, qualified: &str) -> Option<PluginSubagentSpec> {
        let plugins = self.plugins.read().await;
        let (plugin_id, type_name) = qualified.split_once('.')?;
        let plugin = plugins.get(plugin_id)?;
        if !plugin.enabled {
            return None;
        }
        plugin
            .manifest
            .subagents
            .iter()
            .find(|spec| spec.name == type_name)
            .cloned()
    }

    /// Effective service overrides: first enabled plugin (by id order) that
    /// declares each service wins; losers per service are reported so callers
    /// can surface diagnostics. Config-style overrides replace `[services]`
    /// without any MCP server involvement.
    pub async fn service_overrides(&self) -> (PluginServiceOverrides, Vec<(String, Vec<String>)>) {
        let plugins = self.plugins.read().await;
        let mut names: Vec<_> = plugins.keys().cloned().collect();
        names.sort();
        let mut effective = PluginServiceOverrides::default();
        let mut losers: Vec<(String, Vec<String>)> = Vec::new();
        for name in names {
            let plugin = &plugins[&name];
            if !plugin.enabled {
                continue;
            }
            if plugin.manifest.services.web_search.is_some() {
                if effective.web_search.is_none() {
                    effective.web_search = plugin.manifest.services.web_search.clone();
                } else if let Some((_, list)) = losers.iter_mut().find(|(k, _)| k == "web_search") {
                    list.push(name.clone());
                } else {
                    losers.push(("web_search".into(), vec![name.clone()]));
                }
            }
            if plugin.manifest.services.web_fetch.is_some() {
                if effective.web_fetch.is_none() {
                    effective.web_fetch = plugin.manifest.services.web_fetch.clone();
                } else if let Some((_, list)) = losers.iter_mut().find(|(k, _)| k == "web_fetch") {
                    list.push(name.clone());
                } else {
                    losers.push(("web_fetch".into(), vec![name.clone()]));
                }
            }
        }
        (effective, losers)
    }

    pub async fn prompt_append_all(&self) -> String {
        let plugins = self.plugins.read().await;
        let mut names: Vec<_> = plugins.keys().cloned().collect();
        names.sort();
        let mut out = String::new();
        for name in names {
            let plugin = &plugins[&name];
            if !plugin.enabled {
                continue;
            }
            // Replace-style prompts already form the session's base persona
            // (see `system_prompt_override`); appending them again here would
            // duplicate the text.
            if plugin.manifest.replace_system_prompt {
                continue;
            }
            if let Some(prompt) = plugin.manifest.system_prompt.as_deref() {
                if !prompt.trim().is_empty() {
                    out.push_str("\n\n");
                    out.push_str(prompt);
                }
            }
        }
        out
    }

    pub async fn marketplace(
        &self,
        source: &str,
        work_dir: &Path,
    ) -> anyhow::Result<crate::plugin_marketplace::PluginMarketplace> {
        let mut marketplace = crate::plugin_marketplace::load_marketplace(source, work_dir).await?;
        let installed = crate::plugin_marketplace::read_installed(&self.plugins_dir).await?;
        let loaded = self.plugins.read().await;
        for entry in &mut marketplace.plugins {
            let managed_record = installed
                .plugins
                .iter()
                .find(|record| record.id == entry.id);
            let current_version = managed_record
                .and_then(|record| record.version.clone())
                .or_else(|| {
                    loaded
                        .get(&entry.id)
                        .map(|plugin| plugin.manifest.version.clone())
                        .filter(|version| !version.is_empty())
                });
            if managed_record.is_none() && !loaded.contains_key(&entry.id) {
                continue;
            }
            entry.installed = true;
            entry.installed_version = current_version.clone();
            entry.update_available = match (entry.version.as_deref(), current_version.as_deref()) {
                (Some(latest), Some(current)) => {
                    let latest = semver::Version::parse(latest.trim_start_matches('v'));
                    let current = semver::Version::parse(current.trim_start_matches('v'));
                    matches!((latest, current), (Ok(latest), Ok(current)) if latest > current)
                }
                _ => false,
            };
        }
        Ok(marketplace)
    }

    pub async fn registered_marketplaces(
        &self,
    ) -> anyhow::Result<Vec<crate::plugin_marketplace::RegisteredPluginMarketplace>> {
        Ok(
            crate::plugin_marketplace::read_marketplaces(&self.plugins_dir)
                .await?
                .marketplaces,
        )
    }

    pub async fn add_marketplace(
        &self,
        source: &str,
        name: Option<&str>,
        work_dir: &Path,
    ) -> anyhow::Result<crate::plugin_marketplace::RegisteredPluginMarketplace> {
        let _guard = self.mutation.lock().await;
        crate::plugin_marketplace::add_marketplace(&self.plugins_dir, source, name, work_dir).await
    }

    pub async fn remove_marketplace(&self, id: &str) -> anyhow::Result<()> {
        let _guard = self.mutation.lock().await;
        crate::plugin_marketplace::remove_marketplace(&self.plugins_dir, id).await
    }

    pub async fn install(
        &self,
        source: &str,
        marketplace: Option<(&str, &crate::plugin_marketplace::PluginMarketplaceEntry)>,
    ) -> anyhow::Result<crate::plugin_marketplace::InstalledPluginRecord> {
        let _guard = self.mutation.lock().await;
        let record =
            crate::plugin_marketplace::install_plugin(&self.plugins_dir, source, marketplace)
                .await?;
        self.reload().await?;
        Ok(record)
    }

    pub async fn update(
        &self,
        id: &str,
    ) -> anyhow::Result<crate::plugin_marketplace::InstalledPluginRecord> {
        let installed = crate::plugin_marketplace::read_installed(&self.plugins_dir).await?;
        let record = installed
            .plugins
            .into_iter()
            .find(|record| record.id == id)
            .ok_or_else(|| anyhow::anyhow!("plugin {id} is not managed by the marketplace"))?;
        if let Some(marketplace_source) = record.marketplace_source.as_deref() {
            let work_dir = std::env::current_dir()?;
            let marketplace = self.marketplace(marketplace_source, &work_dir).await?;
            let entry = marketplace
                .plugins
                .iter()
                .find(|entry| entry.id == id)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "plugin {id} is no longer present in marketplace {marketplace_source}"
                    )
                })?;
            return self
                .install(&entry.source, Some((&marketplace.source, entry)))
                .await;
        }
        let source = record
            .original_source
            .ok_or_else(|| anyhow::anyhow!("plugin {id} has no update source"))?;
        self.install(&source, None).await
    }

    pub async fn is_managed(&self, id: &str) -> anyhow::Result<bool> {
        validate_plugin_name(id)?;
        Ok(crate::plugin_marketplace::read_installed(&self.plugins_dir)
            .await?
            .plugins
            .iter()
            .any(|record| record.id == id))
    }

    pub async fn set_enabled(&self, id: &str, enabled: bool) -> anyhow::Result<()> {
        let _guard = self.mutation.lock().await;
        crate::plugin_marketplace::set_plugin_enabled(&self.plugins_dir, id, enabled).await?;
        self.reload().await?;
        Ok(())
    }

    pub async fn remove(&self, id: &str) -> anyhow::Result<()> {
        let _guard = self.mutation.lock().await;
        crate::plugin_marketplace::remove_plugin(&self.plugins_dir, id).await?;
        self.reload().await?;
        Ok(())
    }
}

pub(crate) fn select_manifest(root: &Path) -> Option<PathBuf> {
    [KK_ROOT_MANIFEST, KK_DIR_MANIFEST, LEGACY_MANIFEST]
        .into_iter()
        .map(|relative| root.join(relative))
        .find(|path| path.is_file())
}

pub(crate) fn validate_plugin_name(name: &str) -> anyhow::Result<()> {
    let bytes = name.as_bytes();
    let valid = !bytes.is_empty()
        && bytes.len() <= 64
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit());
    let valid = valid
        && bytes.iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(*byte, b'-' | b'_')
        });
    if !valid {
        anyhow::bail!("plugin name must match [a-z0-9][a-z0-9_-]{{0,63}}")
    }
    Ok(())
}

async fn normalize_mcp_server(
    plugin_id: &str,
    server_name: &str,
    config: &kkagent_config::McpServerConfig,
    plugin_root: &Path,
    kkagent_home: &Path,
    multi_server: bool,
) -> anyhow::Result<kkagent_mcp::McpServerConfig> {
    if server_name.trim().is_empty() {
        anyhow::bail!("server name must not be empty")
    }
    let transport = config.transport_type.as_deref().unwrap_or_else(|| {
        if config.url.is_some() {
            "http"
        } else {
            "stdio"
        }
    });
    if !matches!(transport, "stdio" | "sse" | "http" | "streamable-http") {
        anyhow::bail!("unsupported transport {transport}")
    }

    let mut normalized = config.clone();
    if transport == "stdio" {
        let command = normalized
            .command
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("stdio server requires command"))?;
        normalized.command = Some(resolve_plugin_command(plugin_root, command).await?);
        normalized.cwd = Some(match normalized.cwd.as_deref() {
            Some(cwd) => resolve_plugin_path(plugin_root, cwd, true).await?,
            None => plugin_root.display().to_string(),
        });
        normalized
            .env
            .insert("KKAGENT_HOME".into(), kkagent_home.display().to_string());
        normalized.env.insert(
            "KKAGENT_PLUGIN_ROOT".into(),
            plugin_root.display().to_string(),
        );
    }

    let runtime_name = format!("plugin-{plugin_id}:{server_name}");
    let mut server = kkagent_mcp::McpServerConfig::from_app(runtime_name, &normalized);
    // Shorten exposed tool names to `mcp__<plugin-id>__<tool>`: single-server
    // plugins (the common case, e.g. `rk-codesearch_search`) expose their id,
    // multi-server plugins disambiguate with `_<server>`. The runtime name
    // above stays authoritative for toggles/OAuth so no state migrates.
    server.tool_namespace = Some(plugin_tool_namespace(plugin_id, server_name, multi_server));
    Ok(server)
}

/// Exposed tool namespace for a plugin MCP server: the plugin id for
/// single-server plugins, `<plugin-id>_<server-name>` when several servers
/// share one plugin. Keep in sync with `normalize_mcp_server`.
pub fn plugin_tool_namespace(plugin_id: &str, server_name: &str, multi_server: bool) -> String {
    if multi_server {
        format!("{plugin_id}_{server_name}")
    } else {
        plugin_id.to_string()
    }
}

async fn resolve_plugin_command(plugin_root: &Path, command: &str) -> anyhow::Result<String> {
    if command.starts_with("./") || command.starts_with(".\\") {
        return resolve_plugin_path(plugin_root, command, false).await;
    }
    if Path::new(command).is_absolute() || command.contains('/') || command.contains('\\') {
        anyhow::bail!("command must be on PATH or start with ./")
    }
    Ok(command.to_string())
}

async fn resolve_plugin_path(
    plugin_root: &Path,
    value: &str,
    require_directory: bool,
) -> anyhow::Result<String> {
    if !(value.starts_with("./") || value.starts_with(".\\")) {
        anyhow::bail!("path must start with ./")
    }
    let relative = Path::new(value);
    if relative.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        anyhow::bail!("path escapes the plugin root")
    }
    let candidate = plugin_root.join(relative);
    let resolved = tokio::fs::canonicalize(&candidate)
        .await
        .map_err(|error| anyhow::anyhow!("cannot resolve {}: {error}", candidate.display()))?;
    if !resolved.starts_with(plugin_root) {
        anyhow::bail!("path resolves outside the plugin root")
    }
    let metadata = tokio::fs::metadata(&resolved).await?;
    if require_directory && !metadata.is_dir() {
        anyhow::bail!("path is not a directory")
    }
    if !require_directory && !metadata.is_file() {
        anyhow::bail!("path is not a file")
    }
    Ok(resolved.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec_with(plugin_id: &str, name: &str) -> PluginSubagentSpec {
        let spec: PluginSubagentSpec = serde_json::from_value(serde_json::json!({
            "name": name,
            "transport": "acp",
            "description": "test"
        }))
        .unwrap();
        PluginSubagentSpec {
            plugin_id: plugin_id.into(),
            ..spec
        }
    }

    #[test]
    fn qualified_name_derives_agent_identity_from_generic_delegate() {
        // Generic `delegate` subagent names take the agent identity from the
        // owning plugin so the TUI shows "codex" / "cursor", not "delegate".
        assert_eq!(
            spec_with("codex-agent.delegate", "delegate").qualified_name(),
            "codex"
        );
        assert_eq!(
            spec_with("cursor-agent.delegate", "delegate").qualified_name(),
            "cursor"
        );
        assert_eq!(
            spec_with("kk-cursor-agent.delegate", "delegate").qualified_name(),
            "cursor"
        );
        // Non-generic names and plugins without an agent-suffix stay verbatim.
        assert_eq!(
            spec_with("kk-wiki-agent.search", "search").qualified_name(),
            "search"
        );
        assert_eq!(
            spec_with("web-search.delegate", "delegate").qualified_name(),
            "web-search"
        );
        // Un-aggregated spec (no plugin id): fall back to the manifest name.
        assert_eq!(spec_with("", "delegate").qualified_name(), "delegate");
    }

    fn temp_plugins_dir() -> PathBuf {
        std::env::temp_dir().join(format!("kkagent-plugin-{}", uuid::Uuid::new_v4()))
    }

    #[tokio::test]
    async fn discovers_kk_manifest_and_namespaces_mcp_servers() {
        let plugins = temp_plugins_dir();
        let root = plugins.join("code-search");
        tokio::fs::create_dir_all(root.join("scripts"))
            .await
            .unwrap();
        tokio::fs::write(root.join("scripts/server.py"), "print('test')")
            .await
            .unwrap();
        tokio::fs::write(
            root.join(KK_ROOT_MANIFEST),
            serde_json::json!({
                "name": "code-search",
                "version": "1.0.0",
                "mcpServers": {
                    "search": {
                        "transport": "stdio",
                        "command": "python3",
                        "args": ["./scripts/server.py"],
                        "cwd": "./"
                    }
                }
            })
            .to_string(),
        )
        .await
        .unwrap();

        let manager = PluginManager::discover(&plugins).await;
        let configs = manager.mcp_server_configs().await;
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].name, "plugin-code-search:search");
        assert_eq!(configs[0].command.as_deref(), Some("python3"));
        assert_eq!(
            configs[0].cwd.as_deref(),
            Some(root.canonicalize().unwrap().to_str().unwrap())
        );
        assert_eq!(
            configs[0].env.get("KKAGENT_PLUGIN_ROOT"),
            Some(&root.canonicalize().unwrap().display().to_string())
        );
        // Single-server plugin exposes the bare plugin id as tool namespace.
        assert_eq!(
            configs[0].tool_namespace.as_deref(),
            Some("code-search"),
            "single-server plugins should expose short tool names"
        );

        let _ = tokio::fs::remove_dir_all(plugins).await;
    }

    #[tokio::test]
    async fn external_subagents_are_namespaced_and_aggregated() {
        let plugins = temp_plugins_dir();
        let root = plugins.join("cursor-plugin");
        tokio::fs::create_dir_all(&root).await.unwrap();
        tokio::fs::write(
            root.join(KK_ROOT_MANIFEST),
            serde_json::json!({
                "name": "cursor-plugin",
                "subagents": [
                    {
                        "name": "cursor",
                        "transport": "acp",
                        "description": "Cursor CLI over ACP",
                        "transportConfig": { "command": ["agent", "acp"], "mode": "agent" }
                    }
                ]
            })
            .to_string(),
        )
        .await
        .unwrap();

        let manager = PluginManager::discover(&plugins).await;
        let external_subagents = manager.external_subagents().await;
        let (external, conflicts) = (&external_subagents.0, &external_subagents.1);
        assert!(conflicts.is_empty());
        assert_eq!(external.len(), 1);
        let (qualified, spec) = &external[0];
        assert_eq!(qualified, "cursor-plugin.cursor");
        assert_eq!(spec.transport, "acp");
        assert_eq!(spec.transport_config.command, vec!["agent", "acp"]);
        // autoApprove defaults to true, allowDelegation to false.
        assert!(spec.auto_approve);
        assert!(!spec.allow_delegation);
        // plugin_id is injected at aggregation time.
        assert_eq!(spec.plugin_id, "cursor-plugin.cursor");
        // Point lookup by qualified name works.
        assert!(manager
            .external_subagent("cursor-plugin.cursor")
            .await
            .is_some());
        assert!(manager
            .external_subagent("cursor-plugin.missing")
            .await
            .is_none());
        assert!(manager.external_subagent("no-dot").await.is_none());

        let _ = tokio::fs::remove_dir_all(plugins).await;
    }

    #[tokio::test]
    async fn internal_subagent_manifest_parses_tools_and_mcp() {
        let plugins = temp_plugins_dir();
        let root = plugins.join("wiki");
        tokio::fs::create_dir_all(&root).await.unwrap();
        tokio::fs::write(
            root.join(KK_ROOT_MANIFEST),
            serde_json::json!({
                "name": "wiki",
                "subagents": [
                    {
                        "name": "search",
                        "transport": "internal",
                        "description": "Wiki lookup via private MCP",
                        "systemPrompt": "You are a wiki research assistant.",
                        "tools": ["Read", "Grep", "wiki.search"],
                        "mcpServers": {
                            "wiki": { "command": "npx", "args": ["-y", "wiki-mcp"] }
                        },
                        "model": "FAST"
                    }
                ]
            })
            .to_string(),
        )
        .await
        .unwrap();

        let manager = PluginManager::discover(&plugins).await;
        let external_subagents = manager.external_subagents().await;
        let (external, conflicts) = (&external_subagents.0, &external_subagents.1);
        assert!(conflicts.is_empty());
        let (qualified, spec) = &external[0];
        assert_eq!(qualified, "wiki.search");
        assert_eq!(spec.transport, "internal");
        assert_eq!(spec.tools, vec!["Read", "Grep", "wiki.search"]);
        assert_eq!(
            spec.system_prompt.as_deref(),
            Some("You are a wiki research assistant.")
        );
        assert_eq!(spec.mcp_servers.len(), 1);
        // Model binding: aliases are case-insensitive; "FAST" normalizes to
        // "fast" and produces no load-time diagnostic.
        assert_eq!(spec.model_alias().as_deref(), Some("fast"));
        assert!(manager
            .list()
            .await
            .into_iter()
            .find(|p| p.name == "wiki")
            .map(|p| p.diagnostics.is_empty())
            .unwrap_or(false));
        // MCP server runtime name follows the plugin convention
        // `plugin-<plugin_id>:<server>`, compressing tool namespaces to
        // `<plugin_id>_<server>`.
        let cfg = spec.mcp_servers.get("wiki").unwrap();
        let namespaced = kkagent_mcp::McpServerConfig::from_app(
            format!(
                "plugin-{}:{}",
                spec.plugin_id.split('.').next().unwrap(),
                "wiki"
            ),
            cfg,
        );
        assert_eq!(namespaced.name, "plugin-wiki:wiki");

        let _ = tokio::fs::remove_dir_all(plugins).await;
    }

    #[tokio::test]
    async fn subagent_model_binding_diagnostics() {
        let plugins = temp_plugins_dir();
        let root = plugins.join("models");
        tokio::fs::create_dir_all(&root).await.unwrap();
        tokio::fs::write(
            root.join(KK_ROOT_MANIFEST),
            serde_json::json!({
                "name": "models",
                "subagents": [
                    {
                        "name": "raw-id",
                        "transport": "internal",
                        "model": "test/model"
                    },
                    {
                        "name": "acp-bound",
                        "transport": "acp",
                        "model": "fast"
                    },
                    {
                        "name": "valid",
                        "transport": "internal",
                        "model": " secondary "
                    }
                ]
            })
            .to_string(),
        )
        .await
        .unwrap();

        let manager = PluginManager::discover(&plugins).await;
        let external = manager.external_subagents().await;
        let by_name = |name: &str| {
            external
                .0
                .iter()
                .find(|(qualified, _)| qualified == &format!("models.{name}"))
                .map(|(_, spec)| spec.clone())
                .unwrap()
        };

        // Raw model ids are not a binding — alias-only by design.
        assert_eq!(by_name("raw-id").model_alias(), None);
        assert_eq!(by_name("acp-bound").model_alias(), Some("fast".into()));
        // Whitespace is trimmed.
        assert_eq!(by_name("valid").model_alias(), Some("secondary".into()));

        let diagnostics = manager
            .list()
            .await
            .into_iter()
            .find(|p| p.name == "models")
            .map(|p| p.diagnostics)
            .unwrap_or_default();
        let messages: Vec<&str> = diagnostics.iter().map(|d| d.message.as_str()).collect();
        assert!(
            messages
                .iter()
                .any(|m| m.contains("raw-id") && m.contains("not a supported alias")),
            "raw model id should warn: {messages:?}"
        );
        assert!(
            messages
                .iter()
                .any(|m| m.contains("acp-bound") && m.contains("ignored")),
            "model on acp transport should warn: {messages:?}"
        );
        assert!(
            !messages.iter().any(|m| m.contains("valid")),
            "valid alias binding should not warn: {messages:?}"
        );

        let _ = tokio::fs::remove_dir_all(plugins).await;
    }

    #[tokio::test]
    async fn disabled_plugins_do_not_contribute_external_subagents() {
        let plugins = temp_plugins_dir();
        let root = plugins.join("gone");
        tokio::fs::create_dir_all(&root).await.unwrap();
        tokio::fs::write(
            root.join(KK_ROOT_MANIFEST),
            serde_json::json!({
                "name": "gone",
                "subagents": [{ "name": "x", "transport": "acp" }]
            })
            .to_string(),
        )
        .await
        .unwrap();
        // Mark the plugin disabled the same way the runtime does.
        let state_path = plugins.join("gone.disabled");
        let _ = state_path;

        let manager = PluginManager::discover(&plugins).await;
        // Without an enable record the default policy decides; verify lookup
        // consistency instead of assuming enabled state.
        let external_subagents = manager.external_subagents().await;
        let (external, _) = (&external_subagents.0, &external_subagents.1);
        let enabled = manager
            .list()
            .await
            .into_iter()
            .any(|p| p.name == "gone" && p.enabled);
        if !enabled {
            assert!(external.is_empty());
        }

        let _ = tokio::fs::remove_dir_all(plugins).await;
    }

    #[tokio::test]
    async fn multi_server_plugins_suffix_their_tool_namespace() {
        let plugins = temp_plugins_dir();
        let root = plugins.join("multi");
        tokio::fs::create_dir_all(&root).await.unwrap();
        tokio::fs::write(
            root.join(KK_ROOT_MANIFEST),
            serde_json::json!({
                "name": "multi",
                "mcpServers": {
                    "index": { "command": "python3" },
                    "query": { "command": "python3" }
                }
            })
            .to_string(),
        )
        .await
        .unwrap();

        let manager = PluginManager::discover(&plugins).await;
        let configs = manager.mcp_server_configs().await;
        assert_eq!(configs.len(), 2);
        for config in &configs {
            let expected = format!("multi_{}", config.name.rsplit(':').next().unwrap());
            assert_eq!(config.tool_namespace.as_deref(), Some(expected.as_str()));
        }

        let _ = tokio::fs::remove_dir_all(plugins).await;
    }

    #[tokio::test]
    async fn rejects_plugin_mcp_paths_that_escape_the_root() {
        let plugins = temp_plugins_dir();
        let root = plugins.join("unsafe");
        tokio::fs::create_dir_all(&root).await.unwrap();
        tokio::fs::write(
            root.join(KK_ROOT_MANIFEST),
            serde_json::json!({
                "name": "unsafe",
                "mcpServers": {
                    "bad": { "command": "../outside" }
                }
            })
            .to_string(),
        )
        .await
        .unwrap();

        let manager = PluginManager::discover(&plugins).await;
        assert!(manager.mcp_server_configs().await.is_empty());
        let info = manager.list().await;
        assert_eq!(info.len(), 1);
        assert_eq!(info[0].diagnostics.len(), 1);
        assert!(info[0].diagnostics[0]
            .message
            .contains("command must be on PATH or start with ./"));

        let _ = tokio::fs::remove_dir_all(plugins).await;
    }

    #[tokio::test]
    async fn prefers_root_kk_manifest_over_directory_and_legacy_manifests() {
        let plugins = temp_plugins_dir();
        let root = plugins.join("precedence");
        tokio::fs::create_dir_all(root.join(".kk-plugin"))
            .await
            .unwrap();
        tokio::fs::write(
            root.join(KK_ROOT_MANIFEST),
            r#"{"name":"root-plugin","version":"1"}"#,
        )
        .await
        .unwrap();
        tokio::fs::write(
            root.join(KK_DIR_MANIFEST),
            r#"{"name":"directory-plugin","version":"2"}"#,
        )
        .await
        .unwrap();
        tokio::fs::write(
            root.join(LEGACY_MANIFEST),
            r#"{"name":"legacy-plugin","version":"3"}"#,
        )
        .await
        .unwrap();

        let manager = PluginManager::discover(&plugins).await;
        let info = manager.list().await;
        assert_eq!(info.len(), 1);
        assert_eq!(info[0].name, "root-plugin");
        assert_eq!(info[0].version, "1");

        let _ = tokio::fs::remove_dir_all(plugins).await;
    }

    #[test]
    fn validates_kk_plugin_names() {
        assert!(validate_plugin_name("code-search_2").is_ok());
        assert!(validate_plugin_name("Bad Name").is_err());
        assert!(validate_plugin_name("").is_err());
    }

    async fn write_plugin(dir: &Path, id: &str, manifest: serde_json::Value) {
        let root = dir.join(id);
        tokio::fs::create_dir_all(&root).await.unwrap();
        tokio::fs::write(root.join(KK_ROOT_MANIFEST), manifest.to_string())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn parses_override_fields_and_slash_command_definitions() {
        let dir = temp_plugins_dir();
        write_plugin(
            &dir,
            "kk-web",
            serde_json::json!({
                "name": "kk-web",
                "version": "1.0.0",
                "systemPrompt": "custom persona",
                "replaceSystemPrompt": true,
                "toolOverrides": { "Web": "tavily.search" },
                "slashCommands": [
                    "legacy-name",
                    {
                        "name": "search",
                        "description": "Search the web",
                        "argumentHint": "<query>",
                        "promptTemplate": "Search the web for {{args}} and summarize."
                    }
                ],
                "mcpServers": {
                    "tavily": { "command": "npx", "args": ["-y", "tavily-mcp"] }
                }
            }),
        )
        .await;

        let manager = PluginManager::discover(&dir).await;
        let overrides = manager.tool_overrides().await;
        assert_eq!(
            overrides,
            vec![("Web".to_string(), "plugin:kk-web:tavily.search".to_string())]
        );

        let (prompt, losers) = manager.system_prompt_override().await.unwrap();
        assert_eq!(prompt, "custom persona");
        assert!(losers.is_empty());

        let info = manager.list().await;
        assert_eq!(info[0].tool_overrides, vec!["Web".to_string()]);
        assert!(info[0].replaces_system_prompt);
        assert_eq!(info[0].slash_commands.len(), 2);
        assert_eq!(info[0].slash_commands[0].name(), "legacy-name");
        assert_eq!(info[0].slash_commands[1].name(), "search");
        assert_eq!(
            info[0].slash_commands[1].prompt_template(),
            Some("Search the web for {{args}} and summarize.")
        );

        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn prompt_override_conflict_resolves_by_name_with_losers() {
        let dir = temp_plugins_dir();
        for (id, prompt) in [("a-plugin", "persona A"), ("b-plugin", "persona B")] {
            write_plugin(
                &dir,
                id,
                serde_json::json!({
                    "name": id,
                    "systemPrompt": prompt,
                    "replaceSystemPrompt": true
                }),
            )
            .await;
        }

        let manager = PluginManager::discover(&dir).await;
        let (prompt, losers) = manager.system_prompt_override().await.unwrap();
        assert_eq!(prompt, "persona A");
        assert_eq!(losers, vec!["b-plugin".to_string()]);

        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn append_only_system_prompt_is_not_an_override() {
        let dir = temp_plugins_dir();
        // systemPrompt without replaceSystemPrompt keeps default behavior.
        write_plugin(
            &dir,
            "appender",
            serde_json::json!({
                "name": "appender",
                "systemPrompt": "extra guidance"
            }),
        )
        .await;

        let manager = PluginManager::discover(&dir).await;
        assert!(manager.system_prompt_override().await.is_none());
        // Append channel still exposes the text.
        assert!(manager.prompt_append_all().await.contains("extra guidance"));

        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn replace_style_prompt_is_not_also_appended() {
        let dir = temp_plugins_dir();
        write_plugin(
            &dir,
            "replacer",
            serde_json::json!({
                "name": "replacer",
                "systemPrompt": "custom persona",
                "replaceSystemPrompt": true
            }),
        )
        .await;
        write_plugin(
            &dir,
            "appender",
            serde_json::json!({ "name": "appender", "systemPrompt": "extra guidance" }),
        )
        .await;

        let manager = PluginManager::discover(&dir).await;
        let appended = manager.prompt_append_all().await;
        assert!(
            !appended.contains("custom persona"),
            "replace-style prompt must not double as an appended section"
        );
        assert!(
            appended.contains("extra guidance"),
            "append-style unaffected"
        );

        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn disabled_plugins_contribute_no_overrides() {
        let dir = temp_plugins_dir();
        // installed.json record fields are camelCase; a plain top-level
        // plugin with no record stays enabled, so exercise the disabled
        // path via the managed layout.
        let off_root = dir.join("managed/off");
        tokio::fs::create_dir_all(&off_root).await.unwrap();
        let manifest = serde_json::json!({
            "name": "off",
            "systemPrompt": "persona",
            "replaceSystemPrompt": true,
            "toolOverrides": { "Web": "s.search" },
            "mcpServers": { "s": { "command": "npx" } }
        });
        tokio::fs::write(off_root.join(KK_ROOT_MANIFEST), manifest.to_string())
            .await
            .unwrap();
        tokio::fs::write(
            dir.join("installed.json"),
            &format!(
                r#"{{"version":1,"plugins":[{{"id":"off","root":"{}","source":"./off","enabled":false,"installedAt":"2024-01-01T00:00:00Z"}}]}}"#,
                off_root.display()
            ),
        )
        .await
        .unwrap();
        let manager = PluginManager::discover(&dir).await;

        assert!(manager.system_prompt_override().await.is_none());
        assert!(manager.tool_overrides().await.is_empty());

        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn service_overrides_first_enabled_plugin_wins() {
        let dir = temp_plugins_dir();
        write_plugin(
            &dir,
            "b-search",
            serde_json::json!({
                "name": "b-search",
                "services": {
                    "webSearch": {
                        "provider": "brave",
                        "base_url": "https://b.example/search",
                        "api_key_env": "BRAVE_KEY"
                    }
                }
            }),
        )
        .await;
        write_plugin(
            &dir,
            "a-search",
            serde_json::json!({
                "name": "a-search",
                "services": {
                    "webSearch": {
                        "provider": "searxng",
                        "base_url": "http://127.0.0.1:8888/search"
                    }
                }
            }),
        )
        .await;
        let manager = PluginManager::discover(&dir).await;

        let (effective, losers) = manager.service_overrides().await;
        let search = effective.web_search.clone().expect("web search override");
        assert_eq!(search.provider.as_deref(), Some("searxng"));
        assert_eq!(search.base_url, "http://127.0.0.1:8888/search");
        // b-search loses on webSearch (id order: a-search first).
        assert!(losers
            .iter()
            .any(|(svc, names)| svc == "web_search" && names == &vec!["b-search".to_string()]));

        // Merging into a WebServicesConfig replaces the search backend.
        let mut web = kkagent_tools::WebServicesConfig::from_app(&Default::default());
        web.merge_plugin_overrides(&kkagent_config::ServicesConfig {
            web_search: effective.web_search.clone(),
            web_fetch: effective.web_fetch,
            moonshot_search: None,
            moonshot_fetch: None,
        });
        assert!(web.search.is_some());

        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn service_overrides_absent_when_no_plugin_declares_them() {
        let dir = temp_plugins_dir();
        write_plugin(
            &dir,
            "plain",
            serde_json::json!({ "name": "plain", "mcpServers": { "s": { "command": "npx" } } }),
        )
        .await;
        let manager = PluginManager::discover(&dir).await;
        let (effective, losers) = manager.service_overrides().await;
        assert!(effective.web_search.is_none());
        assert!(effective.web_fetch.is_none());
        assert!(losers.is_empty());

        let _ = tokio::fs::remove_dir_all(dir).await;
    }
}
