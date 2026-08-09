//! Session MCP merged connection view.

use serde::{Deserialize, Serialize};
use std::sync::RwLock;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpConnectionView {
    pub name: String,
    pub status: String,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Default)]
pub struct SessionMcpHandle {
    connections: RwLock<Vec<McpConnectionView>>,
}

impl SessionMcpHandle {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&self, connections: Vec<McpConnectionView>) {
        *self.connections.write().unwrap_or_else(|e| e.into_inner()) = connections;
    }

    pub fn list(&self) -> Vec<McpConnectionView> {
        self.connections
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn tool_names(&self) -> Vec<String> {
        self.list().into_iter().flat_map(|c| c.tools).collect()
    }
}
