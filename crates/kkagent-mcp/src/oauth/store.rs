use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthTokens {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_at: Option<i64>,
    #[serde(default)]
    pub token_type: String,
    #[serde(default)]
    pub scope: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredClientInfo {
    pub client_id: String,
    #[serde(default)]
    pub client_secret: Option<String>,
    #[serde(default)]
    pub redirect_uris: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveryState {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    #[serde(default)]
    pub registration_endpoint: Option<String>,
}

/// File-backed OAuth credential store under `~/.kkagent/credentials/mcp/`.
pub struct McpOAuthStore {
    root: PathBuf,
}

impl McpOAuthStore {
    pub fn default_location() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        Self {
            root: home.join(".kkagent").join("credentials").join("mcp"),
        }
    }

    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn store_key(server_name: &str, server_url: &str) -> String {
        let canonical = canonical_resource(server_url);
        let raw = format!("{server_name}|{canonical}");
        let digest = simple_hash(&raw);
        format!("{server_name}-{digest}")
    }

    fn path(&self, key: &str, suffix: &str) -> PathBuf {
        self.root.join(format!("{key}{suffix}"))
    }

    pub async fn ensure_dir(&self) -> Result<()> {
        tokio::fs::create_dir_all(&self.root)
            .await
            .with_context(|| format!("create {}", self.root.display()))
    }

    pub async fn read_tokens(
        &self,
        server_name: &str,
        server_url: &str,
    ) -> Result<Option<OAuthTokens>> {
        self.read_json(&Self::store_key(server_name, server_url), "-tokens.json")
            .await
    }

    pub async fn write_tokens(
        &self,
        server_name: &str,
        server_url: &str,
        tokens: &OAuthTokens,
    ) -> Result<()> {
        self.write_json(
            &Self::store_key(server_name, server_url),
            "-tokens.json",
            tokens,
        )
        .await
    }

    pub async fn read_client(
        &self,
        server_name: &str,
        server_url: &str,
    ) -> Result<Option<StoredClientInfo>> {
        self.read_json(&Self::store_key(server_name, server_url), "-client.json")
            .await
    }

    pub async fn write_client(
        &self,
        server_name: &str,
        server_url: &str,
        info: &StoredClientInfo,
    ) -> Result<()> {
        self.write_json(
            &Self::store_key(server_name, server_url),
            "-client.json",
            info,
        )
        .await
    }

    pub async fn read_discovery(
        &self,
        server_name: &str,
        server_url: &str,
    ) -> Result<Option<DiscoveryState>> {
        self.read_json(&Self::store_key(server_name, server_url), "-discovery.json")
            .await
    }

    pub async fn write_discovery(
        &self,
        server_name: &str,
        server_url: &str,
        state: &DiscoveryState,
    ) -> Result<()> {
        self.write_json(
            &Self::store_key(server_name, server_url),
            "-discovery.json",
            state,
        )
        .await
    }

    async fn read_json<T: for<'de> Deserialize<'de>>(
        &self,
        key: &str,
        suffix: &str,
    ) -> Result<Option<T>> {
        let path = self.path(key, suffix);
        if !path.exists() {
            return Ok(None);
        }
        let text = tokio::fs::read_to_string(&path).await?;
        Ok(Some(serde_json::from_str(&text)?))
    }

    async fn write_json<T: Serialize>(&self, key: &str, suffix: &str, value: &T) -> Result<()> {
        self.ensure_dir().await?;
        let path = self.path(key, suffix);
        let text = serde_json::to_string_pretty(value)?;
        tokio::fs::write(path, text).await?;
        Ok(())
    }
}

pub fn canonical_resource(server_url: &str) -> String {
    match url::Url::parse(server_url) {
        Ok(mut u) => {
            u.set_fragment(None);
            let mut s = u.to_string();
            if s.ends_with('/') {
                s.pop();
            }
            s
        }
        Err(_) => server_url.trim_end_matches('/').to_string(),
    }
}

fn simple_hash(s: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}
