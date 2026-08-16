//! Agent Client Protocol (ACP) adapter — IDE bridge with FS, terminal, auth, builtins.

mod auth;
mod builtins;
mod fs;
mod terminal;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

pub use auth::{auth_methods, AuthTokenStore};
pub use builtins::builtin_command_list;

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
    terminals: Mutex<HashMap<String, terminal::TerminalSlot>>,
    pending_approvals: Mutex<HashMap<String, Value>>,
    modes: Mutex<HashMap<String, String>>,
    auth: Mutex<AuthTokenStore>,
}

impl AcpSessionStore {
    pub fn new() -> Self {
        Self::default()
    }

    async fn session_cwd(&self, session_id: &str) -> Option<PathBuf> {
        let sessions = self.sessions.lock().await;
        sessions
            .get(session_id)
            .and_then(|s| s.get("cwd"))
            .and_then(|v| v.as_str())
            .map(PathBuf::from)
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
    async fn request_approval(&self, _params: &Value) -> Result<Value, String> {
        Err("host does not support approval requests".into())
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
    kaos: kkagent_kaos::KaosHandle,
}

impl AcpServer {
    pub fn new() -> Self {
        Self::with_host(Arc::new(EchoHost))
    }

    pub fn with_host(host: Arc<dyn AcpHost>) -> Self {
        Self {
            store: AcpSessionStore::new(),
            host,
            kaos: kkagent_kaos::KaosHandle::Local(std::sync::Arc::new(
                kkagent_kaos::LocalKaos::cwd(),
            )),
        }
    }

    pub fn with_host_and_kaos(host: Arc<dyn AcpHost>, kaos: kkagent_kaos::KaosHandle) -> Self {
        Self {
            store: AcpSessionStore::new(),
            host,
            kaos,
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
                    "authMethods": auth_methods(),
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
                        "auth": true,
                    }
                }),
            ),
            "ping" | "builtin/ping" => ok(id, json!({"pong": true, "ok": true})),
            "getVersion" | "builtin/getVersion" | "version" => ok(
                id,
                json!({
                    "name": "kkagent-acp",
                    "version": env!("CARGO_PKG_VERSION"),
                    "protocolVersion": 1,
                }),
            ),
            "listSessions" | "sessions/list" | "builtin/listSessions" => {
                let sessions: Vec<Value> =
                    self.store.sessions.lock().await.values().cloned().collect();
                ok(id, json!({"sessions": sessions}))
            }
            "auth/authenticate" | "authenticate" => {
                let method = req
                    .params
                    .get("methodId")
                    .or_else(|| req.params.get("method"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("token");
                match method {
                    "token" => {
                        let token = req
                            .params
                            .get("token")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .trim();
                        if token.is_empty() {
                            return err(id, -32602, "token is required");
                        }
                        self.store.auth.lock().await.set_token(token);
                        ok(id, json!({"ok": true, "method": "token"}))
                    }
                    "none" | "local" => {
                        self.store.auth.lock().await.clear();
                        ok(id, json!({"ok": true, "method": "local"}))
                    }
                    other => err(id, -32602, format!("unsupported auth method: {other}")),
                }
            }
            "auth/status" => {
                let auth = self.store.auth.lock().await;
                ok(
                    id,
                    json!({
                        "authenticated": auth.is_authenticated(),
                        "method": auth.method(),
                    }),
                )
            }
            "session/new" | "sessions/create" => {
                let sid = uuid::Uuid::new_v4().to_string();
                let workspace = req
                    .params
                    .get("cwd")
                    .or_else(|| req.params.get("workspace"))
                    .and_then(|v| v.as_str())
                    .unwrap_or(".");
                let cwd =
                    std::fs::canonicalize(workspace).unwrap_or_else(|_| PathBuf::from(workspace));
                if let Err(e) = self.host.create_session(&sid, &cwd.to_string_lossy()).await {
                    return err(id, -32000, e);
                }
                let sess = json!({
                    "sessionId": sid,
                    "cwd": cwd.to_string_lossy(),
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
            "session/status" | "builtin/status" => {
                let sid = session_id_from(&req.params);
                match self.store.sessions.lock().await.get(&sid) {
                    Some(s) => ok(id, s.clone()),
                    None if sid.is_empty() => err(id, -32602, "sessionId required"),
                    None => err(id, -32000, format!("session not found: {sid}")),
                }
            }
            "session/prompt" | "prompt" => {
                let sid = session_id_from(&req.params);
                let text = req
                    .params
                    .get("prompt")
                    .or_else(|| req.params.get("text"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                match self.host.prompt(&sid, text).await {
                    Ok(v) => ok(id, v),
                    Err(e) => err(id, -32000, e),
                }
            }
            "session/cancel" => {
                let sid = session_id_from(&req.params);
                let _ = self.host.cancel(&sid).await;
                ok(id, json!({"ok": true}))
            }
            "session/set_mode" | "session/mode" => {
                let sid = session_id_from(&req.params);
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
                let sid = session_id_from(&req.params);
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
            "commands/list" | "slash/list" | "builtin/help" => {
                ok(id, json!({"commands": builtin_command_list()}))
            }
            "commands/run" | "slash/run" | "builtin/run" => {
                let name = req
                    .params
                    .get("name")
                    .or_else(|| req.params.get("command"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let args = req
                    .params
                    .get("args")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let sid = session_id_from(&req.params);
                match builtins::run_builtin(name, args, &sid, &self.store, self.host.as_ref()).await
                {
                    Ok(v) => ok(id, v),
                    Err(e) => err(id, -32000, e),
                }
            }
            "fs/read_text_file" | "fs/read" => {
                match fs::read_text(&self.store, &req.params).await {
                    Ok(v) => ok(id, v),
                    Err(e) => err(id, -32000, e),
                }
            }
            "fs/write_text_file" | "fs/write" => {
                match fs::write_text(&self.store, &req.params).await {
                    Ok(v) => ok(id, v),
                    Err(e) => err(id, -32000, e),
                }
            }
            "fs/edit_text_file" | "fs/edit" => {
                match fs::edit_text(&self.store, &req.params).await {
                    Ok(v) => ok(id, v),
                    Err(e) => err(id, -32000, e),
                }
            }
            "fs/list_dir" | "fs/readdir" => match fs::list_dir(&self.store, &req.params).await {
                Ok(v) => ok(id, v),
                Err(e) => err(id, -32000, e),
            },
            "fs/stat" => match fs::stat_path(&self.store, &req.params).await {
                Ok(v) => ok(id, v),
                Err(e) => err(id, -32000, e),
            },
            "fs/glob" => match fs::glob_paths(&self.store, &req.params).await {
                Ok(v) => ok(id, v),
                Err(e) => err(id, -32000, e),
            },
            "fs/grep" => match fs::grep_paths(&self.store, &req.params).await {
                Ok(v) => ok(id, v),
                Err(e) => err(id, -32000, e),
            },
            "terminal/create" | "terminal/new" => {
                match terminal::create(&self.store, &self.kaos, &req.params).await {
                    Ok(v) => ok(id, v),
                    Err(e) => err(id, -32000, e),
                }
            }
            "terminal/output" | "terminal/read" => {
                match terminal::output(&self.store, &req.params).await {
                    Ok(v) => ok(id, v),
                    Err(e) => err(id, -32000, e),
                }
            }
            "terminal/wait_for_exit" | "terminal/wait" => {
                match terminal::wait_for_exit(&self.store, &req.params).await {
                    Ok(v) => ok(id, v),
                    Err(e) => err(id, -32000, e),
                }
            }
            "terminal/kill" | "terminal/close" | "terminal/release" => {
                match terminal::kill(&self.store, &req.params).await {
                    Ok(v) => ok(id, v),
                    Err(e) => err(id, -32000, e),
                }
            }
            "approval/request" | "session/request_permission" => {
                let aid = uuid::Uuid::new_v4().to_string();
                let mut payload = req.params.clone();
                if let Some(obj) = payload.as_object_mut() {
                    obj.insert("approvalId".into(), json!(aid));
                }
                self.store
                    .pending_approvals
                    .lock()
                    .await
                    .insert(aid.clone(), payload.clone());
                match self.host.request_approval(&payload).await {
                    Ok(v) => ok(id, v),
                    Err(_) => ok(
                        id,
                        json!({
                            "approvalId": aid,
                            "status": "pending",
                            "request": payload,
                        }),
                    ),
                }
            }
            "approval/list" => {
                let pending: Vec<Value> = self
                    .store
                    .pending_approvals
                    .lock()
                    .await
                    .values()
                    .cloned()
                    .collect();
                ok(id, json!({"approvals": pending}))
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
                self.store.pending_approvals.lock().await.remove(&aid);
                self.store
                    .pending_approvals
                    .lock()
                    .await
                    .insert(format!("{aid}:resolved"), req.params.clone());
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

fn session_id_from(params: &Value) -> String {
    params
        .get("sessionId")
        .or_else(|| params.get("session_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
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
        "goal_updated" | "GoalUpdated" => "session/update",
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
    use std::io::Write;

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

    async fn new_session(server: &AcpServer, cwd: &str) -> String {
        let created = server
            .handle(AcpRequest {
                jsonrpc: "2.0".into(),
                id: Some(json!(1)),
                method: "session/new".into(),
                params: json!({"cwd": cwd}),
            })
            .await;
        created.result.unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_string()
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
        let session_id = new_session(&server, ".").await;
        assert_eq!(
            host.created.lock().await.as_slice(),
            std::slice::from_ref(&session_id)
        );

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

    #[tokio::test]
    async fn fs_read_write_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let server = AcpServer::new();
        let sid = new_session(&server, dir.path().to_str().unwrap()).await;
        let write = server
            .handle(AcpRequest {
                jsonrpc: "2.0".into(),
                id: Some(json!(1)),
                method: "fs/write_text_file".into(),
                params: json!({
                    "sessionId": sid,
                    "path": "hello.txt",
                    "content": "hi acp",
                }),
            })
            .await;
        assert!(write.error.is_none(), "{write:?}");
        let read = server
            .handle(AcpRequest {
                jsonrpc: "2.0".into(),
                id: Some(json!(2)),
                method: "fs/read_text_file".into(),
                params: json!({
                    "sessionId": sid,
                    "path": "hello.txt",
                }),
            })
            .await;
        assert_eq!(read.result.unwrap()["content"], "hi acp");
    }

    #[tokio::test]
    async fn fs_rejects_path_escape() {
        let dir = tempfile::tempdir().unwrap();
        let server = AcpServer::new();
        let sid = new_session(&server, dir.path().to_str().unwrap()).await;
        let read = server
            .handle(AcpRequest {
                jsonrpc: "2.0".into(),
                id: Some(json!(1)),
                method: "fs/read_text_file".into(),
                params: json!({
                    "sessionId": sid,
                    "path": "../outside.txt",
                }),
            })
            .await;
        assert!(read.error.is_some());
    }

    #[tokio::test]
    async fn terminal_exec_via_kaos() {
        let dir = tempfile::tempdir().unwrap();
        let server = AcpServer::new();
        let sid = new_session(&server, dir.path().to_str().unwrap()).await;
        let created = server
            .handle(AcpRequest {
                jsonrpc: "2.0".into(),
                id: Some(json!(1)),
                method: "terminal/create".into(),
                params: json!({
                    "sessionId": sid,
                    "command": "echo acp-term-ok",
                }),
            })
            .await;
        let tid = created.result.as_ref().unwrap()["terminalId"]
            .as_str()
            .unwrap()
            .to_string();
        let waited = server
            .handle(AcpRequest {
                jsonrpc: "2.0".into(),
                id: Some(json!(2)),
                method: "terminal/wait_for_exit".into(),
                params: json!({"terminalId": tid}),
            })
            .await;
        let result = waited.result.unwrap();
        assert_eq!(result["exitCode"], 0);
        assert!(result["stdout"].as_str().unwrap().contains("acp-term-ok"));
    }

    #[tokio::test]
    async fn builtins_ping_version_list_sessions_help_status() {
        let server = AcpServer::new();
        let sid = new_session(&server, ".").await;

        let ping = server
            .handle(AcpRequest {
                jsonrpc: "2.0".into(),
                id: Some(json!(1)),
                method: "ping".into(),
                params: json!({}),
            })
            .await;
        assert_eq!(ping.result.unwrap()["pong"], true);

        let ver = server
            .handle(AcpRequest {
                jsonrpc: "2.0".into(),
                id: Some(json!(2)),
                method: "getVersion".into(),
                params: json!({}),
            })
            .await;
        assert!(ver.result.unwrap()["version"].as_str().is_some());

        let listed = server
            .handle(AcpRequest {
                jsonrpc: "2.0".into(),
                id: Some(json!(3)),
                method: "listSessions".into(),
                params: json!({}),
            })
            .await;
        assert_eq!(
            listed.result.unwrap()["sessions"].as_array().unwrap().len(),
            1
        );

        let help = server
            .handle(AcpRequest {
                jsonrpc: "2.0".into(),
                id: Some(json!(4)),
                method: "commands/list".into(),
                params: json!({}),
            })
            .await;
        assert!(help.result.unwrap()["commands"].as_array().unwrap().len() >= 5);

        let status = server
            .handle(AcpRequest {
                jsonrpc: "2.0".into(),
                id: Some(json!(5)),
                method: "session/status".into(),
                params: json!({"sessionId": sid}),
            })
            .await;
        assert_eq!(status.result.unwrap()["sessionId"], sid);
    }

    #[tokio::test]
    async fn approval_request_and_respond() {
        let server = AcpServer::new();
        let req = server
            .handle(AcpRequest {
                jsonrpc: "2.0".into(),
                id: Some(json!(1)),
                method: "approval/request".into(),
                params: json!({"tool": "Bash", "command": "rm -rf /"}),
            })
            .await;
        let aid = req.result.unwrap()["approvalId"]
            .as_str()
            .unwrap()
            .to_string();
        let listed = server
            .handle(AcpRequest {
                jsonrpc: "2.0".into(),
                id: Some(json!(2)),
                method: "approval/list".into(),
                params: json!({}),
            })
            .await;
        assert!(!listed.result.unwrap()["approvals"]
            .as_array()
            .unwrap()
            .is_empty());
        let respond = server
            .handle(AcpRequest {
                jsonrpc: "2.0".into(),
                id: Some(json!(3)),
                method: "approval/respond".into(),
                params: json!({"approvalId": aid, "decision": "deny"}),
            })
            .await;
        assert!(respond.result.unwrap()["ok"].as_bool().unwrap());
    }

    #[tokio::test]
    async fn auth_token_flow() {
        let server = AcpServer::new();
        let before = server
            .handle(AcpRequest {
                jsonrpc: "2.0".into(),
                id: Some(json!(1)),
                method: "auth/status".into(),
                params: json!({}),
            })
            .await;
        assert_eq!(before.result.unwrap()["authenticated"], false);
        let auth = server
            .handle(AcpRequest {
                jsonrpc: "2.0".into(),
                id: Some(json!(2)),
                method: "auth/authenticate".into(),
                params: json!({"methodId": "token", "token": "secret"}),
            })
            .await;
        assert!(auth.result.unwrap()["ok"].as_bool().unwrap());
        let after = server
            .handle(AcpRequest {
                jsonrpc: "2.0".into(),
                id: Some(json!(3)),
                method: "auth/status".into(),
                params: json!({}),
            })
            .await;
        assert_eq!(after.result.unwrap()["authenticated"], true);
    }

    #[tokio::test]
    async fn fs_edit_and_grep() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.md");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(f, "alpha\nbeta\ngamma").unwrap();
        }
        let server = AcpServer::new();
        let sid = new_session(&server, dir.path().to_str().unwrap()).await;
        let edited = server
            .handle(AcpRequest {
                jsonrpc: "2.0".into(),
                id: Some(json!(1)),
                method: "fs/edit".into(),
                params: json!({
                    "sessionId": sid,
                    "path": "note.md",
                    "old_string": "beta",
                    "new_string": "BETA",
                }),
            })
            .await;
        assert!(edited.error.is_none(), "{edited:?}");
        let grepped = server
            .handle(AcpRequest {
                jsonrpc: "2.0".into(),
                id: Some(json!(2)),
                method: "fs/grep".into(),
                params: json!({
                    "sessionId": sid,
                    "pattern": "BETA",
                    "path": ".",
                }),
            })
            .await;
        let matches = grepped.result.unwrap()["matches"]
            .as_array()
            .unwrap()
            .clone();
        assert!(!matches.is_empty());
    }
}
