//! Agent Client Protocol (ACP) adapter — IDE bridge with terminals, catalog, events.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
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
    terminals: Mutex<HashMap<String, TerminalSlot>>,
    pending_approvals: Mutex<HashMap<String, Value>>,
    modes: Mutex<HashMap<String, String>>,
}

struct TerminalSlot {
    info: Value,
    child: Option<Child>,
}

impl AcpSessionStore {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Optional host bridge for wiring prompts into AgentLoop.
#[async_trait::async_trait]
pub trait AcpHost: Send + Sync {
    async fn create_session(&self, _session_id: &str, _cwd: &str) -> Result<(), String> {
        Ok(())
    }
    async fn prompt(&self, session_id: &str, text: &str) -> Result<Value, String>;
    async fn cancel(&self, session_id: &str) -> Result<(), String>;
    async fn set_mode(&self, _session_id: &str, _mode: &str) -> Result<(), String> {
        Ok(())
    }
    async fn set_model(&self, _session_id: &str, _model: &str) -> Result<(), String> {
        Ok(())
    }
    async fn respond_approval(&self, _params: &Value) -> Result<(), String> {
        Ok(())
    }
    async fn respond_question(&self, _params: &Value) -> Result<(), String> {
        Ok(())
    }
    async fn list_models(&self) -> Value {
        json!({"models": default_model_catalog()})
    }
    async fn list_mcp(&self) -> Value {
        json!({"servers": []})
    }
    fn subscribe_events(&self) -> Option<tokio::sync::broadcast::Receiver<Value>> {
        None
    }
}

pub struct EchoHost;

#[async_trait::async_trait]
impl AcpHost for EchoHost {
    async fn prompt(&self, session_id: &str, text: &str) -> Result<Value, String> {
        Ok(json!({
            "stopReason": "end_turn",
            "sessionId": session_id,
            "echo": text,
        }))
    }
    async fn cancel(&self, _session_id: &str) -> Result<(), String> {
        Ok(())
    }
}

pub struct AcpServer {
    store: AcpSessionStore,
    host: Arc<dyn AcpHost>,
}

impl AcpServer {
    pub fn new() -> Self {
        Self::with_host(Arc::new(EchoHost))
    }

    pub fn with_host(host: Arc<dyn AcpHost>) -> Self {
        Self {
            store: AcpSessionStore::new(),
            host,
        }
    }

    pub async fn handle(&self, req: AcpRequest) -> AcpResponse {
        let id = req.id.clone().unwrap_or(Value::Null);
        match req.method.as_str() {
            "initialize" => ok(
                id,
                json!({
                    "protocolVersion": 1,
                    "serverInfo": {"name": "kkagent-acp", "version": env!("CARGO_PKG_VERSION")},
                    "capabilities": {
                        "sessions": true,
                        "tools": true,
                        "approvals": true,
                        "fs": true,
                        "terminal": true,
                        "modelCatalog": true,
                        "modes": true,
                        "mcp": true,
                        "slashCommands": true,
                    }
                }),
            ),
            "session/new" | "sessions/create" => {
                let sid = uuid::Uuid::new_v4().to_string();
                let workspace = req
                    .params
                    .get("cwd")
                    .or_else(|| req.params.get("workspace"))
                    .and_then(|v| v.as_str())
                    .unwrap_or(".");
                if let Err(e) = self.host.create_session(&sid, workspace).await {
                    return err(id, -32000, e);
                }
                let sess = json!({
                    "sessionId": sid,
                    "cwd": workspace,
                    "mode": "agent",
                    "model": null,
                });
                self.store
                    .sessions
                    .lock()
                    .await
                    .insert(sid.clone(), sess.clone());
                self.store.modes.lock().await.insert(sid, "agent".into());
                ok(id, sess)
            }
            "session/prompt" | "prompt" => {
                let sid = req
                    .params
                    .get("sessionId")
                    .or_else(|| req.params.get("session_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let text = req
                    .params
                    .get("prompt")
                    .or_else(|| req.params.get("text"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                match self.host.prompt(sid, text).await {
                    Ok(v) => ok(id, v),
                    Err(e) => err(id, -32000, e),
                }
            }
            "session/cancel" => {
                let sid = req
                    .params
                    .get("sessionId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let _ = self.host.cancel(sid).await;
                ok(id, json!({"ok": true}))
            }
            "session/set_mode" | "session/mode" => {
                let sid = req
                    .params
                    .get("sessionId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let mode = req
                    .params
                    .get("mode")
                    .and_then(|v| v.as_str())
                    .unwrap_or("agent")
                    .to_string();
                if let Err(e) = self.host.set_mode(&sid, &mode).await {
                    return err(id, -32000, e);
                }
                self.store
                    .modes
                    .lock()
                    .await
                    .insert(sid.clone(), mode.clone());
                if let Some(s) = self.store.sessions.lock().await.get_mut(&sid) {
                    s["mode"] = json!(mode);
                }
                ok(id, json!({"ok": true, "mode": mode}))
            }
            "model/list" | "models/list" | "modelCatalog/list" => {
                ok(id, self.host.list_models().await)
            }
            "session/set_model" => {
                let sid = req
                    .params
                    .get("sessionId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let model = req
                    .params
                    .get("model")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if let Err(e) = self.host.set_model(&sid, &model).await {
                    return err(id, -32000, e);
                }
                if let Some(s) = self.store.sessions.lock().await.get_mut(&sid) {
                    s["model"] = json!(model);
                }
                ok(id, json!({"ok": true, "model": model}))
            }
            "commands/list" | "slash/list" => ok(
                id,
                json!({"commands": [
                    {"name": "help", "description": "Show help"},
                    {"name": "model", "description": "Switch model"},
                    {"name": "compact", "description": "Compact context"},
                    {"name": "plan", "description": "Toggle plan mode"},
                    {"name": "yolo", "description": "Toggle yolo"},
                    {"name": "swarm", "description": "Swarm mode"},
                ]}),
            ),
            "fs/read_text_file" => {
                let path = req
                    .params
                    .get("path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                match std::fs::read_to_string(path) {
                    Ok(content) => ok(id, json!({"content": content})),
                    Err(e) => err(id, -32000, e.to_string()),
                }
            }
            "fs/write_text_file" => {
                let path = req
                    .params
                    .get("path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let content = req
                    .params
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                match std::fs::write(path, content) {
                    Ok(()) => ok(id, json!({"ok": true})),
                    Err(e) => err(id, -32000, e.to_string()),
                }
            }
            "terminal/create" | "terminal/new" => {
                let cmd = req
                    .params
                    .get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or("echo kkagent-acp-terminal");
                let cwd = req
                    .params
                    .get("cwd")
                    .and_then(|v| v.as_str())
                    .map(std::path::PathBuf::from);
                let tid = uuid::Uuid::new_v4().to_string();
                let mut command = if cfg!(windows) {
                    let mut c = Command::new("cmd");
                    c.args(["/C", cmd]);
                    c
                } else {
                    let mut c = Command::new("sh");
                    c.args(["-lc", cmd]);
                    c
                };
                if let Some(dir) = cwd {
                    command.current_dir(dir);
                }
                let child = command
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .ok();
                let info = json!({
                    "terminalId": tid,
                    "command": cmd,
                    "status": if child.is_some() { "running" } else { "failed" },
                });
                self.store.terminals.lock().await.insert(
                    tid.clone(),
                    TerminalSlot {
                        info: info.clone(),
                        child,
                    },
                );
                ok(id, info)
            }
            "terminal/output" | "terminal/read" => {
                let tid = req
                    .params
                    .get("terminalId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let mut map = self.store.terminals.lock().await;
                if let Some(slot) = map.get_mut(tid) {
                    let mut stdout = String::new();
                    let mut stderr = String::new();
                    let finished = if let Some(child) = slot.child.as_mut() {
                        matches!(child.try_wait(), Ok(Some(_)))
                    } else {
                        false
                    };
                    if finished {
                        if let Some(child) = slot.child.take() {
                            if let Ok(out) = child.wait_with_output().await {
                                stdout = String::from_utf8_lossy(&out.stdout).into_owned();
                                stderr = String::from_utf8_lossy(&out.stderr).into_owned();
                                slot.info["status"] = json!("exited");
                            }
                        }
                    }
                    ok(
                        id,
                        json!({
                            "terminalId": tid,
                            "stdout": stdout,
                            "stderr": stderr,
                            "info": slot.info,
                        }),
                    )
                } else {
                    err(id, -32000, "terminal not found")
                }
            }
            "terminal/kill" | "terminal/close" => {
                let tid = req
                    .params
                    .get("terminalId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let mut map = self.store.terminals.lock().await;
                if let Some(mut slot) = map.remove(tid) {
                    if let Some(mut child) = slot.child.take() {
                        let _ = child.kill().await;
                    }
                    ok(id, json!({"ok": true}))
                } else {
                    err(id, -32000, "terminal not found")
                }
            }
            "approval/respond" | "session/approve" => {
                let aid = req
                    .params
                    .get("approvalId")
                    .or_else(|| req.params.get("approval_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if let Err(e) = self.host.respond_approval(&req.params).await {
                    return err(id, -32000, e);
                }
                self.store
                    .pending_approvals
                    .lock()
                    .await
                    .insert(aid.clone(), req.params.clone());
                ok(id, json!({"ok": true, "approvalId": aid}))
            }
            "question/respond" | "session/question_response" => {
                if let Err(e) = self.host.respond_question(&req.params).await {
                    return err(id, -32000, e);
                }
                ok(id, json!({"ok": true}))
            }
            "mcp/list" => ok(id, self.host.list_mcp().await),
            other => err(id, -32601, format!("Method not found: {other}")),
        }
    }

    pub async fn serve_stdio(self) -> anyhow::Result<()> {
        let server = Arc::new(self);
        let stdin = tokio::io::stdin();
        let mut reader = BufReader::new(stdin).lines();
        let stdout = Arc::new(Mutex::new(tokio::io::stdout()));
        if let Some(mut events) = server.host.subscribe_events() {
            let event_stdout = stdout.clone();
            tokio::spawn(async move {
                while let Ok(event) = events.recv().await {
                    let notification = map_agent_event(&event).unwrap_or_else(|| {
                        json!({
                            "jsonrpc": "2.0",
                            "method": "session/update",
                            "params": event,
                        })
                    });
                    let mut writer = event_stdout.lock().await;
                    if writer
                        .write_all(format!("{notification}\n").as_bytes())
                        .await
                        .is_err()
                    {
                        break;
                    }
                    let _ = writer.flush().await;
                }
            });
        }
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
            if req.id.is_none() {
                // notification — could map agent events later
                continue;
            }
            let request_server = server.clone();
            let request_stdout = stdout.clone();
            tokio::spawn(async move {
                let response = request_server.handle(req).await;
                if let Ok(out) = serde_json::to_string(&response) {
                    let mut writer = request_stdout.lock().await;
                    let _ = writer.write_all(out.as_bytes()).await;
                    let _ = writer.write_all(b"\n").await;
                    let _ = writer.flush().await;
                }
            });
        }
        Ok(())
    }
}

impl Default for AcpServer {
    fn default() -> Self {
        Self::new()
    }
}

fn ok(id: Value, result: Value) -> AcpResponse {
    AcpResponse {
        jsonrpc: "2.0".into(),
        id,
        result: Some(result),
        error: None,
    }
}

fn err(id: Value, code: i64, message: impl Into<String>) -> AcpResponse {
    AcpResponse {
        jsonrpc: "2.0".into(),
        id,
        result: None,
        error: Some(json!({"code": code, "message": message.into()})),
    }
}

fn default_model_catalog() -> Vec<Value> {
    [
        ("gpt-4.1", "openai", true),
        ("o4-mini", "openai", true),
        ("claude-sonnet-4-20250514", "anthropic", false),
        ("gemini-2.5-pro", "google", false),
    ]
    .into_iter()
    .map(|(id, provider, responses)| {
        json!({
            "id": id,
            "provider": provider,
            "responsesApi": responses,
        })
    })
    .collect()
}

/// Map internal agent events to ACP notifications.
pub fn map_agent_event(event: &Value) -> Option<Value> {
    let ty = event.get("type").and_then(|v| v.as_str())?;
    let method = match ty {
        "message_delta" | "MessageDelta" => "session/update",
        "tool_call" | "ToolCall" => "session/update",
        "tool_result" | "ToolResult" => "session/update",
        "approval_requested" | "ApprovalRequested" => "session/request_permission",
        "question_asked" | "QuestionAsked" => "session/request_input",
        "usage_update" | "UsageUpdate" => "session/update",
        "error" | "Error" => "session/update",
        "turn_end" | "TurnEnd" => "session/update",
        _ => "session/update",
    };
    Some(json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": {
            "kind": ty,
            "payload": event,
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct RecordingHost {
        created: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl AcpHost for RecordingHost {
        async fn create_session(&self, session_id: &str, _cwd: &str) -> Result<(), String> {
            self.created.lock().await.push(session_id.to_string());
            Ok(())
        }

        async fn prompt(&self, session_id: &str, text: &str) -> Result<Value, String> {
            Ok(json!({"sessionId": session_id, "output": text.to_uppercase()}))
        }

        async fn cancel(&self, _session_id: &str) -> Result<(), String> {
            Ok(())
        }
    }

    #[test]
    fn maps_approval() {
        let ev = json!({"type": "ApprovalRequested", "request": {}});
        let n = map_agent_event(&ev).unwrap();
        assert_eq!(n["method"], "session/request_permission");
    }

    #[tokio::test]
    async fn delegates_session_lifecycle_to_host() {
        let host = Arc::new(RecordingHost {
            created: Mutex::new(Vec::new()),
        });
        let server = AcpServer::with_host(host.clone());
        let created = server
            .handle(AcpRequest {
                jsonrpc: "2.0".into(),
                id: Some(json!(1)),
                method: "session/new".into(),
                params: json!({"cwd": "."}),
            })
            .await;
        let session_id = created.result.unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(host.created.lock().await.as_slice(), &[session_id.clone()]);

        let prompted = server
            .handle(AcpRequest {
                jsonrpc: "2.0".into(),
                id: Some(json!(2)),
                method: "session/prompt".into(),
                params: json!({"sessionId": session_id, "prompt": "hello"}),
            })
            .await;
        assert_eq!(prompted.result.unwrap()["output"], "HELLO");
    }
}
