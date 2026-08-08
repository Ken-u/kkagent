use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use anyhow::Result;
use rmcp::{ServiceExt, transport::TokioChildProcess};
use tokio::process::Command;
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct McpServerConfig {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct McpToolInfo {
    pub server_name: String,
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

struct McpConnection {
    client: rmcp::service::RunningService<rmcp::RoleClient, ()>,
}

pub struct McpManager {
    configs: Vec<McpServerConfig>,
    connections: Arc<Mutex<HashMap<String, McpConnection>>>,
    tools_cache: Arc<Mutex<Vec<McpToolInfo>>>,
}

impl McpManager {
    pub fn new(configs: Vec<McpServerConfig>) -> Self {
        Self {
            configs,
            connections: Arc::new(Mutex::new(HashMap::new())),
            tools_cache: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub async fn connect_all(&self) -> Result<()> {
        for config in &self.configs {
            match self.connect_server(config).await {
                Ok(_) => tracing::info!("Connected to MCP server: {}", config.name),
                Err(e) => tracing::warn!("Failed to connect to MCP server {}: {}", config.name, e),
            }
        }
        self.refresh_tools().await?;
        Ok(())
    }

    async fn connect_server(&self, config: &McpServerConfig) -> Result<()> {
        let mut cmd = Command::new(&config.command);
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

        self.connections.lock().await.insert(
            config.name.clone(),
            McpConnection { client },
        );
        Ok(())
    }

    async fn refresh_tools(&self) -> Result<()> {
        let mut all_tools = Vec::new();
        let connections = self.connections.lock().await;

        for (server_name, conn) in connections.iter() {
            match conn.client.list_tools(None).await {
                Ok(tools_result) => {
                    for tool in &tools_result.tools {
                        all_tools.push(McpToolInfo {
                            server_name: server_name.clone(),
                            name: tool.name.to_string(),
                            description: tool.description.as_deref().unwrap_or("").to_string(),
                            input_schema: serde_json::to_value(&tool.input_schema).unwrap_or_default(),
                        });
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to list tools from {}: {}", server_name, e);
                }
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
    ) -> Result<String> {
        let connections = self.connections.lock().await;
        let conn = connections.get(server_name)
            .ok_or_else(|| anyhow::anyhow!("MCP server not found: {}", server_name))?;

        let mut params = rmcp::model::CallToolRequestParams::new(tool_name.to_string());
        if let Value::Object(map) = arguments {
            params = params.with_arguments(map);
        }

        let result = conn.client.call_tool(params).await?;

        let mut output = String::new();
        for content in &result.content {
            match content {
                rmcp::model::ContentBlock::Text(text) => {
                    output.push_str(&text.text);
                }
                _ => {
                    output.push_str("[non-text content]");
                }
            }
        }

        Ok(output)
    }
}
