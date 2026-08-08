//! Minimal plugin surface: register extra tools / prompt snippets / slash commands.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub prompt_append: Option<String>,
    #[serde(default)]
    pub slash_commands: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct LoadedPlugin {
    pub manifest: PluginManifest,
    pub root: PathBuf,
}

#[derive(Default)]
pub struct PluginManager {
    plugins: RwLock<HashMap<String, LoadedPlugin>>,
}

impl PluginManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn discover(dir: &Path) -> Arc<Self> {
        let mgr = Arc::new(Self::new());
        let _ = mgr.scan(dir).await;
        mgr
    }

    pub async fn scan(&self, dir: &Path) -> anyhow::Result<usize> {
        if !dir.exists() {
            return Ok(0);
        }
        let mut n = 0;
        let mut rd = tokio::fs::read_dir(dir).await?;
        while let Some(entry) = rd.next_entry().await? {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let manifest_path = path.join("plugin.json");
            if !manifest_path.exists() {
                continue;
            }
            let text = tokio::fs::read_to_string(&manifest_path).await?;
            let manifest: PluginManifest = serde_json::from_str(&text)?;
            let name = manifest.name.clone();
            self.plugins.write().await.insert(
                name,
                LoadedPlugin {
                    manifest,
                    root: path,
                },
            );
            n += 1;
        }
        Ok(n)
    }

    pub async fn list(&self) -> Vec<PluginManifest> {
        self.plugins
            .read()
            .await
            .values()
            .map(|p| p.manifest.clone())
            .collect()
    }

    pub async fn prompt_append_all(&self) -> String {
        let mut out = String::new();
        for p in self.plugins.read().await.values() {
            if let Some(ref s) = p.manifest.prompt_append {
                out.push_str("\n\n");
                out.push_str(s);
            }
        }
        out
    }
}
