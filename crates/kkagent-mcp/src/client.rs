use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{transport::TokioChildProcess, ServiceExt};
use serde_json::Value;
use tokio::process::Command;
use tokio::sync::{watch, Mutex};

use crate::oauth::{interactive_oauth_login, McpOAuthStore, OAuthTokens};
use crate::sse_client::SseMcpClient;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpTransportKind {
    Stdio,
    Sse,
    Http,
}

#[derive(Debug, Clone)]
pub struct McpServerConfig {
    pub name: String,
    pub transport: McpTransportKind,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub url: Option<String>,
    pub headers: HashMap<String, String>,
    pub oauth: Option<kkagent_config::McpOAuthConfig>,
    pub timeout_ms: Option<u64>,
}

impl McpServerConfig {
    pub fn from_app(name: String, cfg: &kkagent_config::McpServerConfig) -> Self {
        let transport = match cfg
            .transport_type
            .as_deref()
            .unwrap_or_else(|| if cfg.url.is_some() { "http" } else { "stdio" })
            .to_ascii_lowercase()
            .as_str()
        {
            "sse" => McpTransportKind::Sse,
            "http" | "streamable-http" | "streamable_http" => McpTransportKind::Http,
            _ => McpTransportKind::Stdio,
        };
        Self {
            name,
            transport,
            command: cfg.command.clone(),
            args: cfg.args.clone(),
            env: cfg.env.clone(),
            url: cfg.url.clone(),
            headers: cfg.headers.clone(),
            oauth: cfg.oauth.clone(),
            timeout_ms: cfg.timeout_ms,
        }
    }
}

#[derive(Debug, Clone)]
pub struct McpToolInfo {
    pub server_name: String,
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, Clone)]
pub struct McpServerStatus {
    pub name: String,
    pub enabled: bool,
    pub connected: bool,
    pub transport: String,
}

#[derive(Debug, Clone, Default)]
pub struct McpCallOutput {
    pub text: String,
    pub images: Vec<kkagent_tools::MediaOutput>,
}

enum McpConnection {
    Stdio(rmcp::service::RunningService<rmcp::RoleClient, ()>),
    Http(rmcp::service::RunningService<rmcp::RoleClient, ()>),
    Sse(SseMcpClient),
}

pub struct McpManager {
    configs: Vec<McpServerConfig>,
    connections: Arc<Mutex<HashMap<String, McpConnection>>>,
    tools_cache: Arc<Mutex<Vec<McpToolInfo>>>,
    oauth_store: Arc<McpOAuthStore>,
    /// Pending auth URLs / status for UI.
    auth_status: Arc<Mutex<HashMap<String, String>>>,
    /// Runtime-disabled server names (persisted via config / disabled.toml).
    disabled: Arc<Mutex<HashSet<String>>>,
    /// Initial discovery runs off the UI startup path. Agent turns wait for it
    /// so the first prompt still receives the complete MCP tool set.
    initialized: watch::Sender<bool>,
}

impl McpManager {
    pub fn new(configs: Vec<McpServerConfig>) -> Self {
        let (initialized, _) = watch::channel(configs.is_empty());
        Self {
            configs,
            connections: Arc::new(Mutex::new(HashMap::new())),
            tools_cache: Arc::new(Mutex::new(Vec::new())),
            oauth_store: Arc::new(McpOAuthStore::default_location()),
            auth_status: Arc::new(Mutex::new(HashMap::new())),
            disabled: Arc::new(Mutex::new(HashSet::new())),
            initialized,
        }
    }

    pub async fn wait_until_initialized(&self) {
        let mut initialized = self.initialized.subscribe();
        while !*initialized.borrow_and_update() {
            if initialized.changed().await.is_err() {
                break;
            }
        }
    }

    fn mark_initialized(&self) {
        self.initialized.send_replace(true);
    }

    pub async fn set_disabled_names(&self, names: impl IntoIterator<Item = String>) {
        let mut g = self.disabled.lock().await;
        *g = names.into_iter().collect();
    }

    pub async fn is_enabled(&self, name: &str) -> bool {
        !self.disabled.lock().await.contains(name)
    }

    pub fn configured_names(&self) -> Vec<String> {
        self.configs.iter().map(|c| c.name.clone()).collect()
    }

    pub fn server_configs(&self) -> &[McpServerConfig] {
        &self.configs
    }

    pub async fn list_server_status(&self) -> Vec<McpServerStatus> {
        let disabled = self.disabled.lock().await.clone();
        let connections = self.connections.lock().await;
        self.configs
            .iter()
            .map(|c| McpServerStatus {
                name: c.name.clone(),
                enabled: !disabled.contains(&c.name),
                connected: connections.contains_key(&c.name),
                transport: format!("{:?}", c.transport).to_ascii_lowercase(),
            })
            .collect()
    }

    pub async fn set_enabled(&self, name: &str, enabled: bool) -> Result<()> {
        if !self.configs.iter().any(|c| c.name == name) {
            anyhow::bail!("unknown MCP server {name}");
        }
        if enabled {
            self.disabled.lock().await.remove(name);
            if !self.connections.lock().await.contains_key(name) {
                self.reconnect(name).await?;
            } else {
                self.refresh_tools().await?;
            }
        } else {
            self.disabled.lock().await.insert(name.to_string());
            self.disconnect(name).await;
            self.refresh_tools().await?;
        }
        Ok(())
    }

    pub async fn disconnect(&self, name: &str) {
        self.connections.lock().await.remove(name);
    }

    pub fn oauth_store(&self) -> Arc<McpOAuthStore> {
        Arc::clone(&self.oauth_store)
    }

    pub async fn connect_all(&self) -> Result<()> {
        let disabled = self.disabled.lock().await.clone();
        let to_connect: Vec<McpServerConfig> = self
            .configs
            .iter()
            .filter(|config| {
                if disabled.contains(&config.name) {
                    tracing::info!("Skipping disabled MCP server {}", config.name);
                    false
                } else {
                    true
                }
            })
            .cloned()
            .collect();

        // Concurrent connects — each server is independent until refresh_tools.
        let futures = to_connect.iter().map(|config| {
            let config = config.clone();
            async move {
                match self.connect_server(&config).await {
                    Ok(()) => {
                        tracing::info!(
                            "Connected to MCP server {} ({:?})",
                            config.name,
                            config.transport
                        );
                    }
                    Err(e) => {
                        tracing::warn!("Failed to connect to MCP server {}: {}", config.name, e);
                    }
                }
            }
        });
        futures::future::join_all(futures).await;
        let result = self.refresh_tools().await;
        self.mark_initialized();
        result
    }

    pub async fn reconnect(&self, server_name: &str) -> Result<()> {
        if self.disabled.lock().await.contains(server_name) {
            anyhow::bail!("MCP server {server_name} is disabled");
        }
        let config = self
            .configs
            .iter()
            .find(|c| c.name == server_name)
            .cloned()
            .ok_or_else(|| anyhow!("unknown MCP server {server_name}"))?;
        self.connections.lock().await.remove(server_name);
        self.connect_server(&config).await?;
        self.refresh_tools().await?;
        Ok(())
    }

    async fn resolve_bearer(&self, config: &McpServerConfig) -> Result<Option<String>> {
        let Some(url) = &config.url else {
            return Ok(None);
        };
        let oauth_enabled = config
            .oauth
            .as_ref()
            .and_then(|o| o.enabled)
            .unwrap_or(config.oauth.is_some());
        if !oauth_enabled && config.oauth.is_none() {
            // Still try stored tokens if present
            if let Some(tokens) = self.oauth_store.read_tokens(&config.name, url).await? {
                return Ok(Some(tokens.access_token));
            }
            return Ok(None);
        }
        if let Some(tokens) = self.oauth_store.read_tokens(&config.name, url).await? {
            if !token_expired(&tokens) {
                return Ok(Some(tokens.access_token));
            }
        }
        // Interactive login
        self.auth_status
            .lock()
            .await
            .insert(config.name.clone(), "authorizing".into());
        let tokens =
            interactive_oauth_login(&config.name, url, &self.oauth_store, config.oauth.as_ref())
                .await?;
        self.auth_status
            .lock()
            .await
            .insert(config.name.clone(), "authorized".into());
        Ok(Some(tokens.access_token))
    }

    async fn connect_server(&self, config: &McpServerConfig) -> Result<()> {
        match config.transport {
            McpTransportKind::Stdio => self.connect_stdio(config).await,
            McpTransportKind::Http => self.connect_http(config).await,
            McpTransportKind::Sse => self.connect_sse(config).await,
        }
    }

    async fn connect_stdio(&self, config: &McpServerConfig) -> Result<()> {
        let command = config
            .command
            .as_deref()
            .ok_or_else(|| anyhow!("stdio MCP server {} missing command", config.name))?;
        let mut cmd = Command::new(command);
        for arg in &config.args {
            cmd.arg(arg);
        }
        for (key, val) in &config.env {
            cmd.env(key, val);
        }
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());

        let transport = TokioChildProcess::new(cmd)?;
        let client = ().serve(transport).await?;
        self.connections
            .lock()
            .await
            .insert(config.name.clone(), McpConnection::Stdio(client));
        Ok(())
    }

    async fn connect_http(&self, config: &McpServerConfig) -> Result<()> {
        let url = config
            .url
            .as_deref()
            .ok_or_else(|| anyhow!("http MCP server {} missing url", config.name))?;
        let bearer = self.resolve_bearer(config).await?;

        // Use rmcp's bundled reqwest via from_config to avoid dual-reqwest type mismatch.
        let mut http_cfg = StreamableHttpClientTransportConfig::with_uri(url.to_string());
        if let Some(token) = &bearer {
            http_cfg = http_cfg.auth_header(format!("Bearer {token}"));
        }
        if !config.headers.is_empty() {
            let mut map = HashMap::new();
            for (k, v) in &config.headers {
                if let (Ok(name), Ok(val)) = (
                    http::HeaderName::from_bytes(k.as_bytes()),
                    http::HeaderValue::from_str(v),
                ) {
                    map.insert(name, val);
                }
            }
            http_cfg = http_cfg.custom_headers(map);
        }

        let transport = StreamableHttpClientTransport::from_config(http_cfg);

        let client = ()
            .serve(transport)
            .await
            .with_context(|| format!("streamable HTTP connect to {} ({url})", config.name))?;
        self.connections
            .lock()
            .await
            .insert(config.name.clone(), McpConnection::Http(client));
        Ok(())
    }

    async fn connect_sse(&self, config: &McpServerConfig) -> Result<()> {
        let url = config
            .url
            .as_deref()
            .ok_or_else(|| anyhow!("sse MCP server {} missing url", config.name))?;
        let bearer = self.resolve_bearer(config).await?;
        let client = SseMcpClient::connect(url, config.headers.clone(), bearer).await?;
        client.initialize().await?;
        self.connections
            .lock()
            .await
            .insert(config.name.clone(), McpConnection::Sse(client));
        Ok(())
    }

    /// Trigger interactive OAuth for a server (exposed as mcp auth tool).
    pub async fn authorize_server(&self, server_name: &str) -> Result<String> {
        let config = self
            .configs
            .iter()
            .find(|c| c.name == server_name)
            .cloned()
            .ok_or_else(|| anyhow!("unknown MCP server {server_name}"))?;
        let url = config
            .url
            .clone()
            .ok_or_else(|| anyhow!("server {server_name} has no url"))?;
        let tokens =
            interactive_oauth_login(&config.name, &url, &self.oauth_store, config.oauth.as_ref())
                .await?;
        let _ = tokens;
        self.reconnect(server_name).await?;
        Ok(format!(
            "OAuth completed for {server_name}; tools refreshed"
        ))
    }

    pub async fn refresh_tools(&self) -> Result<()> {
        let mut all_tools = Vec::new();
        let connections = self.connections.lock().await;

        for (server_name, conn) in connections.iter() {
            match conn {
                McpConnection::Stdio(client) | McpConnection::Http(client) => {
                    match client.list_tools(None).await {
                        Ok(tools_result) => {
                            for tool in &tools_result.tools {
                                all_tools.push(McpToolInfo {
                                    server_name: server_name.clone(),
                                    name: tool.name.to_string(),
                                    description: tool
                                        .description
                                        .as_deref()
                                        .unwrap_or("")
                                        .to_string(),
                                    input_schema: serde_json::to_value(&tool.input_schema)
                                        .unwrap_or_default(),
                                });
                            }
                        }
                        Err(e) => {
                            tracing::warn!("Failed to list tools from {}: {}", server_name, e);
                        }
                    }
                }
                McpConnection::Sse(client) => match client.list_tools().await {
                    Ok(mut tools) => {
                        for t in &mut tools {
                            t.server_name = server_name.clone();
                        }
                        all_tools.extend(tools);
                    }
                    Err(e) => {
                        tracing::warn!("Failed to list SSE tools from {}: {}", server_name, e);
                    }
                },
            }
        }

        // Synthetic authenticate tools for remote servers needing OAuth
        for config in &self.configs {
            if config.url.is_some()
                && (config.oauth.is_some()
                    || matches!(
                        config.transport,
                        McpTransportKind::Http | McpTransportKind::Sse
                    ))
            {
                all_tools.push(McpToolInfo {
                    server_name: config.name.clone(),
                    name: "authenticate".into(),
                    description: format!(
                        "Run interactive OAuth for MCP server `{}` and refresh tools",
                        config.name
                    ),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {},
                    }),
                });
            }
        }

        *self.tools_cache.lock().await = all_tools;
        Ok(())
    }

    pub async fn list_tools(&self) -> Vec<McpToolInfo> {
        self.tools_cache.lock().await.clone()
    }

    pub async fn call_tool(
        &self,
        server_name: &str,
        tool_name: &str,
        arguments: Value,
    ) -> Result<McpCallOutput> {
        if tool_name == "authenticate" {
            return Ok(McpCallOutput {
                text: self.authorize_server(server_name).await?,
                images: Vec::new(),
            });
        }

        let connections = self.connections.lock().await;
        let conn = connections
            .get(server_name)
            .ok_or_else(|| anyhow::anyhow!("MCP server not found: {}", server_name))?;

        match conn {
            McpConnection::Stdio(client) | McpConnection::Http(client) => {
                let mut params = rmcp::model::CallToolRequestParams::new(tool_name.to_string());
                if let Value::Object(map) = arguments {
                    params = params.with_arguments(map);
                }
                let result = client.call_tool(params).await?;
                let mut output = McpCallOutput::default();
                for content in &result.content {
                    match content {
                        rmcp::model::ContentBlock::Text(text) => {
                            output.text.push_str(&text.text);
                        }
                        rmcp::model::ContentBlock::Image(image) => {
                            output.images.push(kkagent_tools::MediaOutput {
                                media_type: image.mime_type.clone(),
                                data: image.data.clone(),
                            })
                        }
                        rmcp::model::ContentBlock::Resource(resource) => match &resource.resource {
                            rmcp::model::ResourceContents::TextResourceContents {
                                text, ..
                            } => {
                                output.text.push_str(text);
                            }
                            rmcp::model::ResourceContents::BlobResourceContents {
                                mime_type: Some(mime),
                                blob,
                                ..
                            } if mime.starts_with("image/") => {
                                output.images.push(kkagent_tools::MediaOutput {
                                    media_type: mime.clone(),
                                    data: blob.clone(),
                                })
                            }
                            _ => output.text.push_str("[non-text resource]"),
                        },
                        _ => {
                            output.text.push_str("[non-text content]");
                        }
                    }
                }
                Ok(output)
            }
            McpConnection::Sse(client) => client.call_tool(tool_name, arguments).await,
        }
    }
}

fn token_expired(tokens: &OAuthTokens) -> bool {
    match tokens.expires_at {
        Some(ts) => chrono::Utc::now().timestamp() >= ts - 30,
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn empty_manager_is_ready_immediately() {
        let manager = McpManager::new(Vec::new());
        tokio::time::timeout(Duration::from_millis(100), manager.wait_until_initialized())
            .await
            .expect("an empty manager should already be initialized");
    }

    #[tokio::test]
    async fn configured_manager_becomes_ready_after_initial_discovery() {
        let manager = McpManager::new(vec![McpServerConfig {
            name: "disabled-test".into(),
            transport: McpTransportKind::Stdio,
            command: None,
            args: Vec::new(),
            env: HashMap::new(),
            url: None,
            headers: HashMap::new(),
            oauth: None,
            timeout_ms: None,
        }]);
        manager
            .set_disabled_names(["disabled-test".to_string()])
            .await;

        assert!(
            tokio::time::timeout(Duration::from_millis(10), manager.wait_until_initialized())
                .await
                .is_err(),
            "configured managers should wait for initial discovery"
        );

        manager.connect_all().await.unwrap();
        tokio::time::timeout(Duration::from_millis(100), manager.wait_until_initialized())
            .await
            .expect("connect_all should release readiness waiters");
    }
}
