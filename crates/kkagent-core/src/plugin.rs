//! KK plugin discovery and runtime capability loading.
//!
//! Plugins may use `kk.plugin.json`, `.kk-plugin/plugin.json`, or the legacy
//! kkagent `plugin.json`. Real tool capabilities are declared through the
//! `mcpServers` manifest field and are bridged by `kkagent-mcp`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, rename = "systemPrompt", alias = "prompt_append")]
    pub system_prompt: Option<String>,
    #[serde(default)]
    pub slash_commands: Vec<String>,
    #[serde(default, rename = "mcpServers")]
    pub mcp_servers: HashMap<String, kkagent_config::McpServerConfig>,
    #[serde(default)]
    pub interface: PluginInterface,
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
}

pub struct PluginManager {
    plugins_dir: PathBuf,
    kkagent_home: PathBuf,
    plugins: RwLock<HashMap<String, LoadedPlugin>>,
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
        let mut roots = Vec::new();
        let mut rd = tokio::fs::read_dir(&self.plugins_dir).await?;
        while let Some(entry) = rd.next_entry().await? {
            let path = entry.path();
            if path.is_dir() && entry.file_name() == "managed" {
                let mut managed = tokio::fs::read_dir(&path).await?;
                while let Some(managed_entry) = managed.next_entry().await? {
                    let managed_path = managed_entry.path();
                    if managed_path.is_dir() {
                        roots.push(managed_path);
                    }
                }
            } else if path.is_dir() {
                roots.push(path);
            }
        }
        roots.sort();

        let mut plugins = HashMap::new();
        for root in roots {
            match self.load_plugin(&root).await {
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

    async fn load_plugin(&self, root: &Path) -> anyhow::Result<Option<LoadedPlugin>> {
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
        for name in names {
            let config = &manifest.mcp_servers[&name];
            match normalize_mcp_server(&manifest.name, &name, config, &root, &self.kkagent_home)
                .await
            {
                Ok(config) => mcp_servers.push(config),
                Err(error) => diagnostics.push(PluginDiagnostic {
                    severity: "warning".into(),
                    message: format!("mcpServers.{name}: {error}"),
                }),
            }
        }

        Ok(Some(LoadedPlugin {
            manifest,
            root,
            manifest_path,
            diagnostics,
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
            .flat_map(|name| plugins[&name].mcp_servers.clone())
            .collect()
    }

    pub async fn prompt_append_all(&self) -> String {
        let plugins = self.plugins.read().await;
        let mut names: Vec<_> = plugins.keys().cloned().collect();
        names.sort();
        let mut out = String::new();
        for name in names {
            if let Some(prompt) = plugins[&name].manifest.system_prompt.as_deref() {
                if !prompt.trim().is_empty() {
                    out.push_str("\n\n");
                    out.push_str(prompt);
                }
            }
        }
        out
    }
}

fn select_manifest(root: &Path) -> Option<PathBuf> {
    [KK_ROOT_MANIFEST, KK_DIR_MANIFEST, LEGACY_MANIFEST]
        .into_iter()
        .map(|relative| root.join(relative))
        .find(|path| path.is_file())
}

fn validate_plugin_name(name: &str) -> anyhow::Result<()> {
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
    Ok(kkagent_mcp::McpServerConfig::from_app(
        runtime_name,
        &normalized,
    ))
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
}
