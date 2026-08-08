//! Agent Client Protocol (ACP) adapter — IDE bridge subset.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpRequest {
    pub jsonrpc: String,
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpResponse {
    pub jsonrpc: String,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
}

#[derive(Default)]
pub struct AcpSessionStore {
    sessions: Mutex<HashMap<String, Value>>,
}

impl AcpSessionStore {
    pub fn new() -> Self {
        Self::default()
    }
}

pub struct AcpServer {
    store: AcpSessionStore,
}

impl AcpServer {
    pub fn new() -> Self {
        Self {
            store: AcpSessionStore::new(),
        }
    }

    pub async fn handle(&self, req: AcpRequest) -> AcpResponse {
        let id = req.id.clone().unwrap_or(Value::Null);
        match req.method.as_str() {
            "initialize" => AcpResponse {
                jsonrpc: "2.0".into(),
                id,
                result: Some(json!({
                    "protocolVersion": 1,
                    "serverInfo": {"name": "kkagent-acp", "version": env!("CARGO_PKG_VERSION")},
                    "capabilities": {
                        "sessions": true,
                        "tools": true,
                        "approvals": true,
                        "fs": true,
                    }
                })),
                error: None,
            },
            "session/new" | "sessions/create" => {
                let sid = uuid::Uuid::new_v4().to_string();
                let workspace = req
                    .params
                    .get("cwd")
                    .or_else(|| req.params.get("workspace"))
                    .and_then(|v| v.as_str())
                    .unwrap_or(".");
                let sess = json!({"sessionId": sid, "cwd": workspace});
                self.store
                    .sessions
                    .lock()
                    .await
                    .insert(sid.clone(), sess.clone());
                AcpResponse {
                    jsonrpc: "2.0".into(),
                    id,
                    result: Some(sess),
                    error: None,
                }
            }
            "session/prompt" | "prompt" => {
                let text = req
                    .params
                    .get("prompt")
                    .or_else(|| req.params.get("text"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                AcpResponse {
                    jsonrpc: "2.0".into(),
                    id,
                    result: Some(json!({
                        "stopReason": "end_turn",
                        "echo": text,
                        "note": "ACP stdio adapter received prompt; wire to AgentLoop via host process"
                    })),
                    error: None,
                }
            }
            "session/cancel" => AcpResponse {
                jsonrpc: "2.0".into(),
                id,
                result: Some(json!({"ok": true})),
                error: None,
            },
            "fs/read_text_file" => {
                let path = req.params.get("path").and_then(|v| v.as_str()).unwrap_or("");
                match std::fs::read_to_string(path) {
                    Ok(content) => AcpResponse {
                        jsonrpc: "2.0".into(),
                        id,
                        result: Some(json!({"content": content})),
                        error: None,
                    },
                    Err(e) => AcpResponse {
                        jsonrpc: "2.0".into(),
                        id,
                        result: None,
                        error: Some(json!({"code": -32000, "message": e.to_string()})),
                    },
                }
            }
            "fs/write_text_file" => {
                let path = req.params.get("path").and_then(|v| v.as_str()).unwrap_or("");
                let content = req
                    .params
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                match std::fs::write(path, content) {
                    Ok(()) => AcpResponse {
                        jsonrpc: "2.0".into(),
                        id,
                        result: Some(json!({"ok": true})),
                        error: None,
                    },
                    Err(e) => AcpResponse {
                        jsonrpc: "2.0".into(),
                        id,
                        result: None,
                        error: Some(json!({"code": -32000, "message": e.to_string()})),
                    },
                }
            }
            other => AcpResponse {
                jsonrpc: "2.0".into(),
                id,
                result: None,
                error: Some(json!({"code": -32601, "message": format!("Method not found: {other}")})),
            },
        }
    }

    /// Run newline-delimited JSON-RPC over stdin/stdout.
    pub async fn serve_stdio(&self) -> anyhow::Result<()> {
        let stdin = tokio::io::stdin();
        let mut reader = BufReader::new(stdin).lines();
        let mut stdout = tokio::io::stdout();
        while let Some(line) = reader.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            let req: AcpRequest = match serde_json::from_str(&line) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("ACP parse error: {e}");
                    continue;
                }
            };
            // Notifications (no id) — acknowledge silently for unknown.
            if req.id.is_none() {
                continue;
            }
            let resp = self.handle(req).await;
            let out = serde_json::to_string(&resp)?;
            stdout.write_all(out.as_bytes()).await?;
            stdout.write_all(b"\n").await?;
            stdout.flush().await?;
        }
        Ok(())
    }
}

impl Default for AcpServer {
    fn default() -> Self {
        Self::new()
    }
}

/// Map internal agent events to ACP notifications.
pub fn map_agent_event(event: &Value) -> Option<Value> {
    let ty = event.get("type").and_then(|v| v.as_str())?;
    Some(json!({
        "jsonrpc": "2.0",
        "method": format!("session/update"),
        "params": {"kind": ty, "payload": event}
    }))
}
