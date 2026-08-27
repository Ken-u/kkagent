//! Agent Client Protocol (ACP) adapter — official v1 protocol implementation
//! with FS, terminal, auth, builtins, streaming updates and agent→client
//! permission/input requests.

mod auth;
mod builtins;
pub mod client;
mod fs;
mod terminal;
mod types;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;
use tokio::task::JoinSet;

pub use auth::{auth_methods, AuthTokenStore};
pub use builtins::builtin_command_list;
pub use client::{
    AcpClient, AcpClientOptions, ExternalProgress, InitializeResult, PermissionPolicy,
    PromptOutcome, DEFAULT_REQUEST_TIMEOUT,
};
pub use types::*;

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

/// Pending agent→client request callback.
type PendingRequest = tokio::sync::oneshot::Sender<Result<Value, Value>>;

/// Bridge for agent→client JSON-RPC requests (permission/input) and
/// `session/update` notifications, used while serving stdio.
#[derive(Clone)]
pub struct ClientBridge {
    outgoing: tokio::sync::mpsc::UnboundedSender<String>,
    pending: Arc<Mutex<HashMap<String, PendingRequest>>>,
    next_id: Arc<AtomicU64>,
}

impl ClientBridge {
    fn new(outgoing: tokio::sync::mpsc::UnboundedSender<String>) -> Self {
        Self {
            outgoing,
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Send a notification (e.g. `session/update`) to the client.
    pub async fn notify(&self, method: &str, params: Value) {
        let msg = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        if let Ok(text) = serde_json::to_string(&msg) {
            let _ = self.outgoing.send(text);
        }
    }

    /// Send a request to the client and await its response.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.pending.lock().await.insert(id.clone(), tx);
        let msg = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let sent = serde_json::to_string(&msg)
            .map_err(|e| format!("serialize request: {e}"))
            .and_then(|text| {
                self.outgoing
                    .send(text)
                    .map_err(|_| "client disconnected".to_string())
            });
        if let Err(e) = sent {
            self.pending.lock().await.remove(&id);
            return Err(e);
        }
        match tokio::time::timeout(Duration::from_secs(180), rx).await {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(error))) => Err(error
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("client returned an error")
                .to_string()),
            Ok(Err(_)) => Err("client dropped the request".into()),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err("client request timed out".into())
            }
        }
    }

    /// Route an incoming client response back to the waiting request.
    pub async fn resolve(&self, id: &str, result: Result<Value, Value>) -> bool {
        match self.pending.lock().await.remove(id) {
            Some(sender) => sender.send(result).is_ok(),
            None => false,
        }
    }
}

/// Optional host bridge for wiring prompts into AgentLoop.
#[async_trait::async_trait]
pub trait AcpHost: Send + Sync {
    async fn create_session(&self, _session_id: &str, _cwd: &str) -> Result<(), String> {
        Ok(())
    }
    /// Bind a new ACP session to an existing persisted transcript.
    /// Default: unsupported.
    async fn load_session(&self, _session_id: &str, _cwd: &str) -> Result<(), String> {
        Err("session/load is not supported by this agent".into())
    }
    /// Whether the host can honor [`AcpHost::load_session`].
    fn supports_load_session(&self) -> bool {
        false
    }
    /// Transcript history for replay after `session/load`, as
    /// `[{ "role": "user"|"assistant", "blocks": [ContentBlock] }]`.
    async fn session_history(&self, _session_id: &str) -> Result<Vec<Value>, String> {
        Ok(vec![])
    }
    async fn prompt(&self, session_id: &str, text: &str) -> Result<Value, String>;
    async fn cancel(&self, session_id: &str) -> Result<(), String>;
    async fn set_mode(&self, _session_id: &str, _mode: &str) -> Result<(), String> {
        Ok(())
    }
    async fn set_model(&self, _session_id: &str, _model: &str) -> Result<(), String> {
        Ok(())
    }
    async fn request_approval(&self, _payload: &Value) -> Result<Value, String> {
        Ok(json!({}))
    }
    async fn respond_approval(&self, _payload: &Value) -> Result<(), String> {
        Ok(())
    }
    async fn respond_question(&self, _payload: &Value) -> Result<(), String> {
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

/// Echo host used for tests and smoke runs.
pub struct EchoHost;

#[async_trait::async_trait]
impl AcpHost for EchoHost {
    async fn prompt(&self, _session_id: &str, text: &str) -> Result<Value, String> {
        Ok(json!({
            "stopReason": "end_turn",
            "content": [{"type": "text", "text": format!("echo: {text}")}],
        }))
    }
    async fn cancel(&self, _session_id: &str) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Default)]
pub struct AcpSessionStore {
    sessions: Mutex<HashMap<String, Value>>,
    terminals: Mutex<HashMap<String, terminal::TerminalSlot>>,
    pending_approvals: Mutex<HashMap<String, Value>>,
    modes: Mutex<HashMap<String, String>>,
    auth: Mutex<AuthTokenStore>,
    /// Session ids with an in-flight `session/prompt` turn.
    running: Mutex<HashMap<String, ()>>,
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

pub struct AcpServer {
    store: AcpSessionStore,
    host: Arc<dyn AcpHost>,
    kaos: kkagent_kaos::KaosHandle,
    /// Set while serving stdio; enables agent→client requests.
    client: Mutex<Option<ClientBridge>>,
}

impl Default for AcpServer {
    fn default() -> Self {
        Self::new()
    }
}

impl AcpServer {
    pub fn new() -> Self {
        Self::with_host(Arc::new(EchoHost))
    }

    pub fn with_host(host: Arc<dyn AcpHost>) -> Self {
        Self {
            store: AcpSessionStore::new(),
            host,
            kaos: kkagent_kaos::KaosHandle::Local(Arc::new(kkagent_kaos::LocalKaos::cwd())),
            client: Mutex::new(None),
        }
    }

    pub fn with_host_and_kaos(host: Arc<dyn AcpHost>, kaos: kkagent_kaos::KaosHandle) -> Self {
        Self {
            store: AcpSessionStore::new(),
            host,
            kaos,
            client: Mutex::new(None),
        }
    }

    async fn client(&self) -> Option<ClientBridge> {
        self.client.lock().await.clone()
    }

    /// Attach the bridge used for agent→client traffic. Only the first
    /// attachment wins; the bridge is never swapped mid-connection.
    pub async fn attach_client_bridge(&self, bridge: ClientBridge) {
        let mut slot = self.client.lock().await;
        if slot.is_none() {
            *slot = Some(bridge);
        }
    }

    pub async fn handle(&self, req: AcpRequest) -> AcpResponse {
        let id = req.id.clone().unwrap_or(Value::Null);
        match req.method.as_str() {
            "initialize" => ok(
                id,
                json!({
                    "protocolVersion": 1,
                    "agentCapabilities": {
                        "loadSession": self.host.supports_load_session(),
                        "promptCapabilities": {"tools": true},
                    },
                    "authMethods": auth_methods(),
                }),
            ),
            "ping" | "builtin/ping" => ok(id, json!({})),
            "getVersion" | "builtin/getVersion" | "version" => ok(
                id,
                json!({
                    "name": "kkagent-acp",
                    "version": env!("CARGO_PKG_VERSION"),
                    "protocolVersion": 1,
                }),
            ),
            "authenticate" | "auth/authenticate" => {
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
                        ok(id, json!({}))
                    }
                    "none" | "local" => {
                        self.store.auth.lock().await.clear();
                        ok(id, json!({}))
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
            "session/new" => {
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
                self.store.sessions.lock().await.insert(sid.clone(), sess);
                self.store
                    .modes
                    .lock()
                    .await
                    .insert(sid.clone(), "agent".into());
                let result = json!({
                    "sessionId": sid,
                    "modes": default_modes(),
                });
                // initialMessage (optional): run a prompt turn in the
                // background without blocking the session/new response.
                if let Some(initial) = req.params.get("initialMessage") {
                    self.spawn_prompt_turn(&sid, initial.clone()).await;
                }
                ok(id, result)
            }
            "session/load" => {
                let transcript_id = req
                    .params
                    .get("sessionId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if transcript_id.is_empty() {
                    return err(id, -32602, "sessionId is required");
                }
                let workspace = req
                    .params
                    .get("cwd")
                    .and_then(|v| v.as_str())
                    .unwrap_or(".");
                let cwd =
                    std::fs::canonicalize(workspace).unwrap_or_else(|_| PathBuf::from(workspace));
                if let Err(e) = self
                    .host
                    .load_session(&transcript_id, &cwd.to_string_lossy())
                    .await
                {
                    return err(id, -32000, e);
                }
                let sess = json!({
                    "sessionId": transcript_id,
                    "cwd": cwd.to_string_lossy(),
                    "mode": "agent",
                    "model": null,
                });
                self.store
                    .sessions
                    .lock()
                    .await
                    .insert(transcript_id.clone(), sess);
                self.store
                    .modes
                    .lock()
                    .await
                    .insert(transcript_id.clone(), "agent".into());
                let result = json!({
                    "sessionId": transcript_id,
                    "modes": default_modes(),
                });
                // Replay history as user/agent message chunks.
                match self.host.session_history(&transcript_id).await {
                    Ok(history) => self.replay_history(&transcript_id, history).await,
                    Err(e) => tracing::warn!("ACP session/load replay failed: {e}"),
                }
                if let Some(initial) = req.params.get("initialMessage") {
                    self.spawn_prompt_turn(&transcript_id, initial.clone())
                        .await;
                }
                ok(id, result)
            }
            "session/prompt" | "prompt" => {
                let sid = session_id_from(&req.params);
                if sid.is_empty() {
                    return err(id, -32602, "sessionId is required");
                }
                if self.store.sessions.lock().await.get(&sid).is_none() {
                    return err(id, -32000, format!("session not found: {sid}"));
                }
                let prompt = req
                    .params
                    .get("prompt")
                    .cloned()
                    .or_else(|| req.params.get("text").cloned())
                    .unwrap_or(Value::Null);
                let blocks = parse_prompt_blocks(&prompt);
                if blocks.is_empty() {
                    return err(id, -32602, "prompt must contain at least one content block");
                }
                // Reject concurrent turns on the same session.
                {
                    let mut running = self.store.running.lock().await;
                    if running.contains_key(&sid) {
                        return err(id, -32000, format!("session is busy: {sid}"));
                    }
                    running.insert(sid.clone(), ());
                }
                let outcome = self
                    .host
                    .prompt(&sid, &ContentBlock::join_text(&blocks))
                    .await;
                self.store.running.lock().await.remove(&sid);
                match outcome {
                    Ok(v) => {
                        let stop = v
                            .get("stopReason")
                            .and_then(|s| s.as_str())
                            .unwrap_or("end_turn");
                        ok(id, json!({"stopReason": stop}))
                    }
                    Err(e) => err(id, -32000, e),
                }
            }
            "session/cancel" => {
                let sid = session_id_from(&req.params);
                match self.host.cancel(&sid).await {
                    Ok(()) => ok(id, json!({})),
                    Err(e) => err(id, -32000, e),
                }
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
            // Legacy manual approval resolution. The official flow uses the
            // agent→client `session/request_permission` request instead.
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
            "approval/respond" => {
                let aid = req
                    .params
                    .get("approvalId")
                    .or_else(|| req.params.get("approval_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if aid.is_empty() {
                    return err(id, -32602, "approvalId is required");
                }
                if let Err(e) = self.host.respond_approval(&req.params).await {
                    return err(id, -32000, e);
                }
                self.store.pending_approvals.lock().await.remove(&aid);
                ok(id, json!({"ok": true, "approvalId": aid}))
            }
            "question/respond" => {
                if let Err(e) = self.host.respond_question(&req.params).await {
                    return err(id, -32000, e);
                }
                ok(id, json!({"ok": true}))
            }
            "mcp/list" => ok(id, self.host.list_mcp().await),
            "listSessions" | "sessions/list" | "builtin/listSessions" => {
                let sessions: Vec<Value> =
                    self.store.sessions.lock().await.values().cloned().collect();
                ok(id, json!({"sessions": sessions}))
            }
            other => err(id, -32601, format!("Method not found: {other}")),
        }
    }

    /// Fire-and-forget prompt turn used for `initialMessage`. Streaming is
    /// handled by the global event forwarder while serving stdio.
    async fn spawn_prompt_turn(&self, session_id: &str, prompt: Value) {
        let blocks = parse_prompt_blocks(&prompt);
        if blocks.is_empty() {
            return;
        }
        let text = ContentBlock::join_text(&blocks);
        let host = Arc::clone(&self.host);
        let sid = session_id.to_string();
        tokio::spawn(async move {
            if let Err(e) = host.prompt(&sid, &text).await {
                tracing::warn!("ACP initialMessage turn failed: {e}");
            }
        });
    }

    /// Replay persisted history after `session/load` as message chunks.
    async fn replay_history(&self, session_id: &str, history: Vec<Value>) {
        let Some(bridge) = self.client().await else {
            return;
        };
        for message in history {
            let role = message
                .get("role")
                .and_then(|v| v.as_str())
                .unwrap_or("user");
            let Some(blocks) = message.get("blocks").and_then(|v| v.as_array()) else {
                continue;
            };
            for block in blocks {
                let Ok(parsed) = serde_json::from_value::<ContentBlock>(block.clone()) else {
                    continue;
                };
                let update = match role {
                    "assistant" => SessionUpdate::AgentMessageChunk { content: parsed },
                    _ => SessionUpdate::UserMessageChunk { content: parsed },
                };
                bridge
                    .notify(
                        "session/update",
                        json!({"sessionId": session_id, "update": update}),
                    )
                    .await;
            }
        }
    }

    /// Forward an internal agent event to the client as official ACP
    /// notifications. Returns true when something was sent.
    async fn forward_event(&self, bridge: &ClientBridge, event: &Value) -> bool {
        let Some(ty) = event.get("type").and_then(|v| v.as_str()) else {
            return false;
        };
        let sid = event
            .get("session_id")
            .or_else(|| event.get("sessionId"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if !self.known_session(&sid).await {
            return false;
        }
        match ty {
            "message_delta" | "MessageDelta" => {
                let text = event.get("text").and_then(|v| v.as_str()).unwrap_or("");
                bridge
                    .notify(
                        "session/update",
                        json!({
                            "sessionId": sid,
                            "update": SessionUpdate::agent_message_chunk(text),
                        }),
                    )
                    .await;
                true
            }
            "thinking_delta" | "ThinkingDelta" => {
                let text = event.get("text").and_then(|v| v.as_str()).unwrap_or("");
                bridge
                    .notify(
                        "session/update",
                        json!({
                            "sessionId": sid,
                            "update": SessionUpdate::agent_thought_chunk(text),
                        }),
                    )
                    .await;
                true
            }
            "tool_call" | "ToolCall" => {
                let update = SessionUpdate::ToolCall {
                    tool_call_id: event
                        .get("tool_call_id")
                        .or_else(|| event.get("call_id"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    tool_kind: ToolKind::from_tool_name(
                        event
                            .get("tool_name")
                            .and_then(|v| v.as_str())
                            .unwrap_or(""),
                    ),
                    tool_name: event
                        .get("tool_name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("tool")
                        .to_string(),
                    raw_input: event.get("input").cloned().unwrap_or(Value::Null),
                    locations: None,
                    content: vec![],
                };
                bridge
                    .notify(
                        "session/update",
                        json!({"sessionId": sid, "update": update}),
                    )
                    .await;
                true
            }
            "tool_result" | "ToolResult" => {
                let update = SessionUpdate::ToolCallUpdate {
                    tool_call_id: event
                        .get("tool_call_id")
                        .or_else(|| event.get("call_id"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    content: vec![ContentBlock::text(
                        event
                            .get("output")
                            .and_then(|v| v.as_str())
                            .or_else(|| event.get("text").and_then(|v| v.as_str()))
                            .unwrap_or(""),
                    )],
                };
                bridge
                    .notify(
                        "session/update",
                        json!({"sessionId": sid, "update": update}),
                    )
                    .await;
                true
            }
            // Non-protocol events (status, usage, heartbeats, errors) are
            // intentionally not forwarded: official v1 has no variant for
            // them and the prompt response carries the outcome.
            _ => false,
        }
    }

    async fn known_session(&self, session_id: &str) -> bool {
        self.store.sessions.lock().await.contains_key(session_id)
    }

    /// Translate an internal approval event into the official agent→client
    /// `session/request_permission` request and route the outcome back into
    /// the host's approval channel. Called from a dedicated task so the
    /// event forwarder is never blocked waiting for the client.
    async fn request_permission_from_event(&self, bridge: &ClientBridge, event: &Value) -> bool {
        // Approval events nest the payload under `request`.
        let payload = event.get("request").unwrap_or(event);
        let approval_id = payload
            .get("approval_id")
            .or_else(|| payload.get("approvalId"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if approval_id.is_empty() {
            return false;
        }
        // Claim the approval so only one path handles it.
        {
            let mut pending = self.store.pending_approvals.lock().await;
            if pending.contains_key(&approval_id) {
                return false;
            }
            pending.insert(approval_id.clone(), payload.clone());
        }
        let sid = event
            .get("session_id")
            .or_else(|| event.get("sessionId"))
            .or_else(|| payload.get("session_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let tool_name = payload
            .get("tool_name")
            .and_then(|v| v.as_str())
            .unwrap_or("tool");
        let action = payload.get("action").and_then(|v| v.as_str()).unwrap_or("");
        let input = payload
            .get("tool_input_display")
            .cloned()
            .unwrap_or(Value::Null);
        let kind = classify_permission(tool_name, action, &input, &sid, &self.store).await;
        let request = RequestPermissionRequest {
            session_id: sid.clone(),
            options: vec![
                PermissionOption::AllowOnce { title: None },
                PermissionOption::AllowAlways { title: None },
                PermissionOption::RejectOnce { title: None },
                PermissionOption::RejectAlways { title: None },
            ],
            kind,
        };
        let outcome = bridge
            .request(
                "session/request_permission",
                serde_json::to_value(&request).unwrap_or(Value::Null),
            )
            .await;
        let approved = match outcome {
            Ok(response) => match serde_json::from_value::<PermissionOutcome>(response) {
                Ok(PermissionOutcome::Selected { option_id }) => option_id.starts_with("allow"),
                _ => false,
            },
            Err(_) => false,
        };
        // Route the decision back into the host's approval channel.
        let payload = json!({
            "approval_id": approval_id,
            "session_id": sid,
            "approve": approved,
        });
        if let Err(e) = self.host.respond_approval(&payload).await {
            tracing::warn!("ACP approval routing failed: {e}");
        }
        self.store
            .pending_approvals
            .lock()
            .await
            .remove(&approval_id);
        true
    }

    /// Translate an internal question event into the official agent→client
    /// `session/request_input` request and route the answer back.
    async fn request_input_from_event(&self, bridge: &ClientBridge, event: &Value) -> bool {
        // Question events nest the payload under `question`.
        let payload = event.get("question").unwrap_or(event);
        let question_id = payload
            .get("question_id")
            .or_else(|| payload.get("questionId"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if question_id.is_empty() {
            return false;
        }
        let sid = event
            .get("session_id")
            .or_else(|| event.get("sessionId"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let text = payload
            .get("text")
            .or_else(|| payload.get("question"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let options: Vec<SelectOption> = payload
            .get("options")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|o| {
                        Some(SelectOption {
                            label: o.get("label").and_then(|v| v.as_str())?.to_string(),
                            description: o
                                .get("description")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string()),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let allow_multiple = payload
            .get("allow_multiple")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let kind = if options.is_empty() {
            RequestInputKind::Text { password: None }
        } else {
            RequestInputKind::Select {
                options,
                multi_select: Some(allow_multiple),
            }
        };
        let request = RequestInputRequest {
            session_id: sid.clone(),
            prompt: vec![ContentBlock::text(text)],
            kind,
        };
        let answer = bridge
            .request(
                "session/request_input",
                serde_json::to_value(&request).unwrap_or(Value::Null),
            )
            .await;
        let mut payload = json!({
            "question_id": question_id,
            "session_id": sid,
        });
        match answer {
            Ok(response) => {
                if let Ok(parsed) = serde_json::from_value::<RequestInputResponse>(response) {
                    if parsed.canceled.unwrap_or(false) {
                        payload["canceled"] = json!(true);
                    } else if let Some(content) = parsed.content {
                        payload["answer"] = json!(ContentBlock::join_text(&content));
                    }
                }
            }
            Err(_) => {
                payload["canceled"] = json!(true);
            }
        }
        if let Err(e) = self.host.respond_question(&payload).await {
            tracing::warn!("ACP question routing failed: {e}");
        }
        true
    }

    pub async fn serve_stdio(self) -> anyhow::Result<()> {
        let server = Arc::new(self);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let bridge = ClientBridge::new(tx.clone());
        server.attach_client_bridge(bridge.clone()).await;

        let writer_task = tokio::spawn(async move {
            while let Some(line) = rx.recv().await {
                let mut out = tokio::io::stdout();
                let written = out.write_all(line.as_bytes()).await.is_ok()
                    && out.write_all(b"\n").await.is_ok()
                    && out.flush().await.is_ok();
                if !written {
                    break;
                }
            }
        });

        // Global event forwarder: official updates for known sessions only.
        // Approvals/questions are handled in dedicated tasks so the loop
        // never blocks on a client response.
        if let Some(mut events) = server.host.subscribe_events() {
            let forward_server = Arc::clone(&server);
            let forward_bridge = bridge.clone();
            tokio::spawn(async move {
                loop {
                    match events.recv().await {
                        Ok(event) => {
                            let ty = event.get("type").and_then(|v| v.as_str()).unwrap_or("");
                            match ty {
                                "approval_requested" | "ApprovalRequested" => {
                                    let s = Arc::clone(&forward_server);
                                    let b = forward_bridge.clone();
                                    let e = event.clone();
                                    tokio::spawn(async move {
                                        s.request_permission_from_event(&b, &e).await;
                                    });
                                }
                                "question_asked" | "QuestionAsked" => {
                                    let s = Arc::clone(&forward_server);
                                    let b = forward_bridge.clone();
                                    let e = event.clone();
                                    tokio::spawn(async move {
                                        s.request_input_from_event(&b, &e).await;
                                    });
                                }
                                _ => {
                                    forward_server.forward_event(&forward_bridge, &event).await;
                                }
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!("ACP event forwarder lagged by {n} events");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
        }

        let stdin = tokio::io::stdin();
        let mut reader = BufReader::new(stdin).lines();
        let mut tasks = JoinSet::new();
        while let Some(line) = reader.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            let value: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("ACP parse error: {e}");
                    continue;
                }
            };
            // Response to an agent→client request?
            if value.get("method").is_none() && value.get("id").is_some() {
                let id = value
                    .get("id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                let result = value
                    .get("result")
                    .cloned()
                    .map(Ok)
                    .unwrap_or_else(|| Err(value.get("error").cloned().unwrap_or(Value::Null)));
                if !bridge.resolve(&id, result).await {
                    tracing::warn!("ACP response with unknown request id: {id}");
                }
                continue;
            }
            let Ok(req) = serde_json::from_value::<AcpRequest>(value) else {
                tracing::warn!("ACP message is not a valid request");
                continue;
            };
            if req.id.is_none() {
                // Notifications are accepted but carry no response.
                continue;
            }
            let request_server = Arc::clone(&server);
            let request_tx = tx.clone();
            let request = req;
            tasks.spawn(async move {
                let response = request_server.handle(request).await;
                if let Ok(out) = serde_json::to_string(&response) {
                    let _ = request_tx.send(out);
                }
            });
        }
        // stdin closed: let in-flight requests finish (bounded), then give
        // the writer a moment to drain before shutdown. The forwarder and
        // bridge keep holding senders, so an .await on the writer could
        // block forever.
        let _ = tokio::time::timeout(Duration::from_secs(10), async {
            while tasks.join_next().await.is_some() {}
        })
        .await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        writer_task.abort();
        Ok(())
    }
}

/// Classify an internal approval into an official permission kind.
async fn classify_permission(
    tool_name: &str,
    action: &str,
    input: &Value,
    session_id: &str,
    store: &AcpSessionStore,
) -> PermissionRequestKind {
    let lowered = tool_name.to_ascii_lowercase();
    if lowered.contains("bash")
        || lowered.contains("shell")
        || lowered.contains("terminal")
        || action.contains("run")
        || action.contains("exec")
    {
        let command = input
            .get("command")
            .or_else(|| input.get("cmd"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        return PermissionRequestKind::Command { command };
    }
    if lowered.contains("fetch") || lowered.contains("http") || lowered.contains("web") {
        let url = input
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        return PermissionRequestKind::Fetch { url };
    }
    // Default: treat as an edit against the session workspace.
    let raw_path = input
        .get("path")
        .or_else(|| input.get("file_path"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let cwd = store.session_cwd(session_id).await;
    let (relative, absolute) = if raw_path.is_empty() {
        (None, cwd.map(|p| p.to_string_lossy().into_owned()))
    } else if PathBuf::from(&raw_path).is_absolute() {
        (None, Some(raw_path))
    } else {
        let abs = cwd
            .as_ref()
            .map(|c| c.join(&raw_path).to_string_lossy().into_owned());
        (Some(raw_path), abs)
    };
    PermissionRequestKind::Edit {
        file: Location { absolute, relative },
        old_string: input
            .get("old_string")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        new_string: input
            .get("new_string")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    }
}

/// Parse a `session/prompt` payload into content blocks. Accepts the official
/// block array as well as a plain string or `{ "text": ... }` for legacy
/// clients.
fn parse_prompt_blocks(prompt: &Value) -> Vec<ContentBlock> {
    match prompt {
        Value::Array(items) => items
            .iter()
            .filter_map(|item| serde_json::from_value::<ContentBlock>(item.clone()).ok())
            .collect(),
        Value::String(text) if !text.is_empty() => vec![ContentBlock::text(text)],
        Value::Object(obj) => obj
            .get("text")
            .and_then(|v| v.as_str())
            .filter(|t| !t.is_empty())
            .map(|t| vec![ContentBlock::text(t)])
            .unwrap_or_default(),
        _ => vec![],
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

#[cfg(test)]
mod tests {
    use super::*;

    async fn new_session(server: &AcpServer, cwd: &str) -> String {
        let resp = server
            .handle(AcpRequest {
                jsonrpc: "2.0".into(),
                id: Some(json!(1)),
                method: "session/new".into(),
                params: json!({"cwd": cwd}),
            })
            .await;
        resp.result
            .unwrap()
            .get("sessionId")
            .and_then(|v| v.as_str())
            .unwrap()
            .to_string()
    }

    #[tokio::test]
    async fn initialize_returns_official_capabilities() {
        let server = AcpServer::new();
        let resp = server
            .handle(AcpRequest {
                jsonrpc: "2.0".into(),
                id: Some(json!(1)),
                method: "initialize".into(),
                params: json!({}),
            })
            .await;
        let result = resp.result.unwrap();
        assert_eq!(result["protocolVersion"], 1);
        assert_eq!(
            result["agentCapabilities"]["promptCapabilities"]["tools"],
            true
        );
        assert_eq!(result["agentCapabilities"]["loadSession"], false);
        assert!(result.get("capabilities").is_none(), "legacy field removed");
    }

    #[tokio::test]
    async fn session_new_returns_modes() {
        let dir = tempfile::tempdir().unwrap();
        let server = AcpServer::new();
        let resp = server
            .handle(AcpRequest {
                jsonrpc: "2.0".into(),
                id: Some(json!(1)),
                method: "session/new".into(),
                params: json!({"cwd": dir.path().to_str().unwrap()}),
            })
            .await;
        let result = resp.result.unwrap();
        assert!(result["sessionId"].is_string());
        let modes = result["modes"].as_array().unwrap();
        assert!(modes.iter().any(|m| m["kind"] == "primary"));
    }

    #[tokio::test]
    async fn prompt_accepts_official_blocks_and_returns_stop_reason() {
        struct OnceHost;
        #[async_trait::async_trait]
        impl AcpHost for OnceHost {
            async fn prompt(&self, _sid: &str, text: &str) -> Result<Value, String> {
                Ok(json!({"stopReason": "end_turn", "saw": text}))
            }
            async fn cancel(&self, _sid: &str) -> Result<(), String> {
                Ok(())
            }
        }
        let server = AcpServer::with_host(Arc::new(OnceHost));
        let dir = tempfile::tempdir().unwrap();
        let sid = new_session(&server, dir.path().to_str().unwrap()).await;
        let resp = server
            .handle(AcpRequest {
                jsonrpc: "2.0".into(),
                id: Some(json!(2)),
                method: "session/prompt".into(),
                params: json!({
                    "sessionId": sid,
                    "prompt": [
                        {"type": "text", "text": "hello "},
                        {"type": "text", "text": "world"}
                    ]
                }),
            })
            .await;
        assert_eq!(resp.result.unwrap()["stopReason"], "end_turn");
    }

    #[tokio::test]
    async fn prompt_rejects_empty_and_missing_session() {
        let server = AcpServer::new();
        let empty = server
            .handle(AcpRequest {
                jsonrpc: "2.0".into(),
                id: Some(json!(1)),
                method: "session/prompt".into(),
                params: json!({"sessionId": "nope", "prompt": [{"type": "text", "text": "x"}]}),
            })
            .await;
        assert!(empty.error.is_some());

        let dir = tempfile::tempdir().unwrap();
        let sid = new_session(&server, dir.path().to_str().unwrap()).await;
        let no_blocks = server
            .handle(AcpRequest {
                jsonrpc: "2.0".into(),
                id: Some(json!(2)),
                method: "session/prompt".into(),
                params: json!({"sessionId": sid, "prompt": []}),
            })
            .await;
        assert!(no_blocks.error.is_some());
    }

    #[tokio::test]
    async fn session_load_unsupported_by_default() {
        let server = AcpServer::new();
        let resp = server
            .handle(AcpRequest {
                jsonrpc: "2.0".into(),
                id: Some(json!(1)),
                method: "session/load".into(),
                params: json!({"sessionId": "abc"}),
            })
            .await;
        assert!(resp.error.is_some());
    }

    #[tokio::test]
    async fn forward_event_maps_to_official_updates() {
        struct NoopHost;
        #[async_trait::async_trait]
        impl AcpHost for NoopHost {
            async fn prompt(&self, _sid: &str, _text: &str) -> Result<Value, String> {
                Ok(json!({"stopReason": "end_turn"}))
            }
            async fn cancel(&self, _sid: &str) -> Result<(), String> {
                Ok(())
            }
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let bridge = ClientBridge::new(tx);
        let server = AcpServer::with_host(Arc::new(NoopHost));
        server.attach_client_bridge(bridge).await;
        let dir = tempfile::tempdir().unwrap();
        let sid = new_session(&server, dir.path().to_str().unwrap()).await;

        let sent = server
            .forward_event(
                &server.client().await.unwrap(),
                &json!({"type": "message_delta", "session_id": sid, "text": "hi"}),
            )
            .await;
        assert!(sent);
        let line = rx.recv().await.unwrap();
        let msg: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(msg["method"], "session/update");
        assert_eq!(msg["params"]["sessionId"], sid);
        assert_eq!(
            msg["params"]["update"]["sessionUpdate"],
            "agent_message_chunk"
        );
        assert_eq!(msg["params"]["update"]["content"]["text"], "hi");

        // Unknown sessions are never forwarded.
        let dropped = server
            .forward_event(
                &server.client().await.unwrap(),
                &json!({"type": "message_delta", "session_id": "other", "text": "x"}),
            )
            .await;
        assert!(!dropped);
    }

    #[tokio::test]
    async fn permission_request_routes_outcome_to_host() {
        struct RecordingHost {
            approvals: Mutex<Vec<Value>>,
        }
        #[async_trait::async_trait]
        impl AcpHost for RecordingHost {
            async fn prompt(&self, _sid: &str, _text: &str) -> Result<Value, String> {
                Ok(json!({"stopReason": "end_turn"}))
            }
            async fn cancel(&self, _sid: &str) -> Result<(), String> {
                Ok(())
            }
            async fn respond_approval(&self, payload: &Value) -> Result<(), String> {
                self.approvals.lock().await.push(payload.clone());
                Ok(())
            }
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let bridge = ClientBridge::new(tx);
        let host = Arc::new(RecordingHost {
            approvals: Mutex::new(vec![]),
        });
        let server = AcpServer::with_host(Arc::clone(&host) as Arc<dyn AcpHost>);
        server.attach_client_bridge(bridge).await;
        let dir = tempfile::tempdir().unwrap();
        let sid = new_session(&server, dir.path().to_str().unwrap()).await;

        let event = json!({
            "type": "approval_requested",
            "session_id": sid,
            "approval_id": "ap_1",
            "tool_name": "Bash",
            "action": "run_command",
            "tool_input_display": {"command": "cargo test"},
        });
        // Run the official agent→client request; answer it from the test's
        // fake client side via the same routing serve_stdio performs.
        let server_bridge = server.client().await.unwrap();
        let (probe_tx, probe_rx) = tokio::sync::oneshot::channel::<bool>();
        let server_arc = Arc::new(server);
        let task = tokio::spawn({
            let server = Arc::clone(&server_arc);
            let bridge = server_bridge.clone();
            let event = event.clone();
            async move {
                let handled = server.request_permission_from_event(&bridge, &event).await;
                let claimed = server
                    .store
                    .pending_approvals
                    .lock()
                    .await
                    .contains_key("ap_1");
                let _ = probe_tx.send(claimed);
                handled
            }
        });
        let line = rx.recv().await.unwrap();
        let msg: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(msg["method"], "session/request_permission");
        assert_eq!(msg["params"]["sessionId"], sid);
        assert_eq!(msg["params"]["kind"]["kind"], "command");
        assert_eq!(msg["params"]["kind"]["command"], "cargo test");
        let option_ids: Vec<&str> = msg["params"]["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o["optionKind"].as_str().unwrap())
            .collect();
        assert_eq!(
            option_ids,
            vec!["allow_once", "allow_always", "reject_once", "reject_always"]
        );

        let request_id = msg["id"].as_str().unwrap().to_string();
        let answered = server_bridge
            .resolve(
                &request_id,
                Ok(json!({"outcomeKind": "selected", "optionId": "allow_once"})),
            )
            .await;
        assert!(answered);
        assert!(task.await.is_ok());

        let approvals = host.approvals.lock().await;
        assert_eq!(approvals.len(), 1);
        assert_eq!(approvals[0]["approval_id"], "ap_1");
        assert_eq!(approvals[0]["approve"], true);
        // Claimed approval is released after the decision.
        assert!(!probe_rx.await.unwrap());
    }

    #[tokio::test]
    async fn permission_request_rejects_when_client_declines() {
        struct RecordingHost {
            approvals: Mutex<Vec<Value>>,
        }
        #[async_trait::async_trait]
        impl AcpHost for RecordingHost {
            async fn prompt(&self, _sid: &str, _text: &str) -> Result<Value, String> {
                Ok(json!({"stopReason": "end_turn"}))
            }
            async fn cancel(&self, _sid: &str) -> Result<(), String> {
                Ok(())
            }
            async fn respond_approval(&self, payload: &Value) -> Result<(), String> {
                self.approvals.lock().await.push(payload.clone());
                Ok(())
            }
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let bridge = ClientBridge::new(tx);
        let host = Arc::new(RecordingHost {
            approvals: Mutex::new(vec![]),
        });
        let server = AcpServer::with_host(Arc::clone(&host) as Arc<dyn AcpHost>);
        server.attach_client_bridge(bridge).await;
        let dir = tempfile::tempdir().unwrap();
        let sid = new_session(&server, dir.path().to_str().unwrap()).await;

        let event = json!({
            "type": "approval_requested",
            "session_id": sid,
            "approval_id": "ap_2",
            "tool_name": "Edit",
            "tool_input_display": {"path": "src/a.rs", "old_string": "x", "new_string": "y"},
        });
        let server_bridge = server.client().await.unwrap();
        let task = tokio::spawn({
            let bridge = server_bridge.clone();
            let event = event.clone();
            async move { server.request_permission_from_event(&bridge, &event).await }
        });
        let line = rx.recv().await.unwrap();
        let msg: Value = serde_json::from_str(&line).unwrap();
        // Edit tool maps to the edit permission kind with a relative path.
        assert_eq!(msg["params"]["kind"]["kind"], "edit");
        assert_eq!(msg["params"]["kind"]["file"]["relative"], "src/a.rs");
        let request_id = msg["id"].as_str().unwrap().to_string();
        server_bridge
            .resolve(&request_id, Ok(json!({"outcomeKind": "cancelled"})))
            .await;
        assert!(task.await.is_ok());
        let approvals = host.approvals.lock().await;
        assert_eq!(approvals[0]["approve"], false);
    }

    #[tokio::test]
    async fn question_request_routes_answer_to_host() {
        struct RecordingHost {
            answers: Mutex<Vec<Value>>,
        }
        #[async_trait::async_trait]
        impl AcpHost for RecordingHost {
            async fn prompt(&self, _sid: &str, _text: &str) -> Result<Value, String> {
                Ok(json!({"stopReason": "end_turn"}))
            }
            async fn cancel(&self, _sid: &str) -> Result<(), String> {
                Ok(())
            }
            async fn respond_question(&self, payload: &Value) -> Result<(), String> {
                self.answers.lock().await.push(payload.clone());
                Ok(())
            }
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let bridge = ClientBridge::new(tx);
        let host = Arc::new(RecordingHost {
            answers: Mutex::new(vec![]),
        });
        let server = AcpServer::with_host(Arc::clone(&host) as Arc<dyn AcpHost>);
        server.attach_client_bridge(bridge).await;
        let dir = tempfile::tempdir().unwrap();
        let sid = new_session(&server, dir.path().to_str().unwrap()).await;

        let event = json!({
            "type": "question_asked",
            "session_id": sid,
            "question_id": "q_1",
            "text": "Which database?",
            "options": [{"label": "postgres"}, {"label": "sqlite"}],
        });
        let server_bridge = server.client().await.unwrap();
        let task = tokio::spawn({
            let bridge = server_bridge.clone();
            let event = event.clone();
            async move { server.request_input_from_event(&bridge, &event).await }
        });
        let line = rx.recv().await.unwrap();
        let msg: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(msg["method"], "session/request_input");
        assert_eq!(msg["params"]["kind"]["kind"], "select");
        assert_eq!(msg["params"]["prompt"][0]["text"], "Which database?");
        let request_id = msg["id"].as_str().unwrap().to_string();
        server_bridge
            .resolve(
                &request_id,
                Ok(json!({"content": [{"type": "text", "text": "postgres"}]})),
            )
            .await;
        assert!(task.await.is_ok());
        let answers = host.answers.lock().await;
        assert_eq!(answers[0]["question_id"], "q_1");
        assert_eq!(answers[0]["answer"], "postgres");
    }

    #[tokio::test]
    async fn classify_permission_prefers_command_kind() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let server = AcpServer::new();
        server.attach_client_bridge(ClientBridge::new(tx)).await;
        let sid = new_session(&server, dir.path().to_str().unwrap()).await;
        let kind = classify_permission(
            "Bash",
            "run_command",
            &json!({"command": "ls"}),
            &sid,
            &server.store,
        )
        .await;
        match kind {
            PermissionRequestKind::Command { command } => assert_eq!(command, "ls"),
            other => panic!("expected command kind, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fs_write_rejects_symlink_escape() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let server = AcpServer::new();
        let sid = new_session(&server, dir.path().to_str().unwrap()).await;
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
        let write = server
            .handle(AcpRequest {
                jsonrpc: "2.0".into(),
                id: Some(json!(1)),
                method: "fs/write_text_file".into(),
                params: json!({"sessionId": sid, "path": "link/x.txt", "content": "x"}),
            })
            .await;
        assert!(write.error.is_some(), "symlink escape must be rejected");
    }
}
