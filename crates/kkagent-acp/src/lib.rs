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
    base_model_id, config_option_by_category, current_model_of, current_model_raw_of,
    parse_config_options, variant_level_of, AcpAskOption, AcpAskQuestion, AcpAskQuestionOutcome,
    AcpAskQuestionRequest, AcpClient, AcpClientOptions, AcpConfigOption, AcpConfigValue,
    AcpCreatePlanOutcome, AcpCreatePlanRequest, AcpPermissionChoice, AcpPermissionOption,
    AcpPermissionOutcome, AcpPermissionRequest, AcpPlanTodo, AcpQuestionAnswer, AcpRequestHandler,
    AcpSessionInfo, ExternalProgress, InitializeResult, PermissionPolicy, PromptOutcome,
    DEFAULT_REQUEST_TIMEOUT,
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

/// Why an agent→client request did not produce a result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeError {
    /// The client went away (or was never connected).
    Disconnected,
    TimedOut,
    /// The caller's cancel flag was raised.
    Cancelled,
    /// The client answered with a JSON-RPC error.
    Rpc {
        code: i64,
        message: String,
    },
}

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
        self.request_with(method, params, Some(Duration::from_secs(180)), None)
            .await
            .map_err(|error| match error {
                BridgeError::Disconnected => "client disconnected".to_string(),
                BridgeError::TimedOut => "client request timed out".to_string(),
                BridgeError::Cancelled => "client request cancelled".to_string(),
                BridgeError::Rpc { message, .. } => message,
            })
    }

    /// Like [`Self::request`], with structured errors, an optional timeout
    /// (`None` waits indefinitely) and cooperative cancellation: when `cancel`
    /// turns true the client is told with `$/cancel_request` and the call
    /// returns [`BridgeError::Cancelled`] without waiting for its answer.
    pub async fn request_with(
        &self,
        method: &str,
        params: Value,
        timeout: Option<Duration>,
        cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<Value, BridgeError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        self.pending.lock().await.insert(id.clone(), tx);
        let msg = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let sent = serde_json::to_string(&msg)
            .map_err(|_| BridgeError::Disconnected)
            .and_then(|text| {
                self.outgoing
                    .send(text)
                    .map_err(|_| BridgeError::Disconnected)
            });
        if let Err(error) = sent {
            self.pending.lock().await.remove(&id);
            return Err(error);
        }
        let wait = async {
            loop {
                tokio::select! {
                    answer = &mut rx => return Some(answer),
                    _ = tokio::time::sleep(Duration::from_millis(50)), if cancel.is_some() => {
                        if cancel
                            .as_ref()
                            .is_some_and(|flag| flag.load(Ordering::SeqCst))
                        {
                            return None;
                        }
                    }
                }
            }
        };
        let outcome = match timeout {
            Some(limit) => tokio::time::timeout(limit, wait).await.map_err(|_| ()),
            None => Ok(wait.await),
        };
        match outcome {
            Ok(Some(Ok(Ok(value)))) => Ok(value),
            Ok(Some(Ok(Err(error)))) => Err(BridgeError::Rpc {
                code: error.get("code").and_then(Value::as_i64).unwrap_or(-32000),
                message: error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("client returned an error")
                    .to_string(),
            }),
            // The sender was dropped: the connection is gone.
            Ok(Some(Err(_))) => Err(BridgeError::Disconnected),
            Ok(None) => {
                self.abandon(&id).await;
                Err(BridgeError::Cancelled)
            }
            Err(()) => {
                self.abandon(&id).await;
                Err(BridgeError::TimedOut)
            }
        }
    }

    /// Stop waiting for `id` and tell the client to stop working on it.
    async fn abandon(&self, id: &str) {
        self.pending.lock().await.remove(id);
        self.notify("$/cancel_request", json!({"requestId": id}))
            .await;
    }

    /// Queue an already serialized message for the client.
    fn send_raw(&self, line: String) {
        let _ = self.outgoing.send(line);
    }

    /// Fail every in-flight request (the client disconnected).
    pub async fn fail_all(&self) {
        self.pending.lock().await.clear();
    }

    /// Route an incoming client response back to the waiting request.
    pub async fn resolve(&self, id: &str, result: Result<Value, Value>) -> bool {
        match self.pending.lock().await.remove(id) {
            Some(sender) => sender.send(result).is_ok(),
            None => false,
        }
    }
}

/// A client request that is still being served (for `$/cancel_request`).
struct Inflight {
    id: Value,
    method: String,
    session_id: Option<String>,
    /// Set once the request was answered with "cancelled"; its real result is
    /// then discarded.
    cancelled: Arc<std::sync::atomic::AtomicBool>,
}

/// How a `systemPrompt` session parameter combines with the built-in prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemPromptParamMode {
    Append,
    Override,
}

/// Standard ACP `systemPrompt` session parameter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemPromptParam {
    pub mode: SystemPromptParamMode,
    pub text: String,
}

/// Session parameters of `session/new`, `session/load` and `session/resume`
/// beyond the working directory.
#[derive(Debug, Clone, Default)]
pub struct SessionSetup {
    /// Standard `systemPrompt`.
    pub system_prompt: Option<SystemPromptParam>,
    /// Standard `mcpServers` descriptors, as sent by the client.
    pub mcp_servers: Vec<Value>,
    /// Client `_meta` (carries `kkagent/host` for hosted sessions).
    pub meta: Option<Value>,
}

impl SessionSetup {
    /// Parse the shared session parameters. Errors are `-32602` material.
    pub fn from_params(params: &Value) -> Result<Self, String> {
        let system_prompt = match params.get("systemPrompt") {
            None | Some(Value::Null) => None,
            Some(Value::String(text)) => Some(SystemPromptParam {
                mode: SystemPromptParamMode::Append,
                text: text.clone(),
            }),
            Some(Value::Object(object)) => {
                let mode = match object.get("mode").and_then(Value::as_str) {
                    None | Some("append") => SystemPromptParamMode::Append,
                    Some("override") | Some("replace") => SystemPromptParamMode::Override,
                    Some(other) => {
                        return Err(format!(
                            "invalid systemPrompt.mode `{other}` (expected append or override)"
                        ))
                    }
                };
                let text = match object.get("content").or_else(|| object.get("text")) {
                    Some(Value::String(text)) => text.clone(),
                    Some(Value::Array(blocks)) => blocks
                        .iter()
                        .filter_map(|block| {
                            block
                                .get("text")
                                .and_then(Value::as_str)
                                .or_else(|| block.as_str())
                        })
                        .collect::<Vec<_>>()
                        .join("\n\n"),
                    Some(block @ Value::Object(_)) => block
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    _ => String::new(),
                };
                Some(SystemPromptParam { mode, text })
            }
            Some(_) => return Err("systemPrompt must be an object".to_string()),
        }
        .filter(|prompt| !prompt.text.trim().is_empty());
        let mcp_servers = match params.get("mcpServers") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(servers)) => servers.clone(),
            Some(_) => return Err("mcpServers must be an array".to_string()),
        };
        Ok(Self {
            system_prompt,
            mcp_servers,
            meta: params.get("_meta").filter(|meta| !meta.is_null()).cloned(),
        })
    }
}

/// Optional host bridge for wiring prompts into AgentLoop.
#[async_trait::async_trait]
pub trait AcpHost: Send + Sync {
    async fn create_session(
        &self,
        _session_id: &str,
        _cwd: &str,
        _setup: &SessionSetup,
    ) -> Result<(), String> {
        Ok(())
    }
    /// Bind a new ACP session to an existing persisted transcript.
    /// Default: unsupported.
    async fn load_session(
        &self,
        _session_id: &str,
        _cwd: &str,
        _setup: &SessionSetup,
    ) -> Result<(), String> {
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
    /// Persisted sessions for `session/list`, as
    /// `[{"sessionId", "cwd", "title"?, "updatedAt"?}]`, optionally limited to
    /// one working directory. `None` means the host has no store and the
    /// server lists the sessions of this connection instead.
    async fn list_sessions(&self, _cwd: Option<&str>) -> Result<Option<Vec<Value>>, String> {
        Ok(None)
    }

    /// `session/close`: stop any work and free the session's runtime
    /// resources. The session stays resumable.
    async fn close_session(&self, _session_id: &str) -> Result<(), String> {
        Ok(())
    }

    /// `session/delete` (kkagent extension): remove the session for good.
    async fn delete_session(&self, _session_id: &str) -> Result<(), String> {
        Err("session/delete is not supported by this agent".into())
    }

    /// Current title of a session, for `session_info_update`.
    async fn session_title(&self, _session_id: &str) -> Option<String> {
        None
    }

    /// Size of the model context window, for `usage_update`.
    async fn context_window(&self, _session_id: &str) -> Option<u64> {
        None
    }

    /// Whether the event stream from [`AcpHost::subscribe_events`] ends every
    /// prompt turn with a `turn_committed` event for the session. When it
    /// does, the server holds the `session/prompt` response back until that
    /// event has been forwarded, so no `session/update` of the turn can
    /// arrive after its response.
    fn signals_turn_commit(&self) -> bool {
        false
    }

    /// Called once with the connection to the client, before any request is
    /// served. Hosts that talk back to the client (host tools, host storage)
    /// keep the bridge.
    fn bind_client(&self, _bridge: ClientBridge) {}

    /// Called for `initialize` with the raw request params (`clientInfo`,
    /// `clientCapabilities`, `_meta`). An error rejects the handshake; the
    /// returned value, when present, becomes the response's `_meta`.
    async fn initialize(&self, _params: &Value) -> Result<Option<Value>, String> {
        Ok(None)
    }

    /// True when `session_id` (or, with `None`, the process) runs under a
    /// strict tool allowlist. The server then refuses the `fs/*` and
    /// `terminal/*` request families, which would let a client touch the
    /// filesystem or spawn processes without going through the (allowlisted)
    /// tool layer.
    async fn tools_restricted(&self, _session_id: Option<&str>) -> bool {
        false
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
    /// Raised by `session/cancel` for the turn in flight and lowered when the
    /// next prompt starts. Pending client requests of the session (permission,
    /// input) watch it so a cancelled turn never waits for an answer.
    cancel_flags: Mutex<HashMap<String, Arc<std::sync::atomic::AtomicBool>>>,
    /// `clientInfo` / `clientCapabilities` from `initialize`.
    client_info: Mutex<Value>,
    /// Last title announced through `session_info_update`.
    announced_titles: Mutex<HashMap<String, String>>,
    /// True while an event forwarder streams updates to the client.
    forwarding: std::sync::atomic::AtomicBool,
    /// Turns of each session whose events the forwarder has fully delivered
    /// (counted at `turn_committed`).
    delivered_turns: Mutex<HashMap<String, u64>>,
}

impl AcpSessionStore {
    pub fn new() -> Self {
        Self::default()
    }

    async fn cancel_flag(&self, session_id: &str) -> Arc<std::sync::atomic::AtomicBool> {
        self.cancel_flags
            .lock()
            .await
            .entry(session_id.to_string())
            .or_default()
            .clone()
    }

    async fn delivered_turns(&self, session_id: &str) -> u64 {
        self.delivered_turns
            .lock()
            .await
            .get(session_id)
            .copied()
            .unwrap_or(0)
    }

    /// Wait (bounded) until the forwarder has delivered everything the turn
    /// that started at `before` produced, so no `session/update` follows the
    /// prompt response.
    async fn wait_turn_delivered(&self, session_id: &str, before: u64) {
        if !self.forwarding.load(Ordering::SeqCst) {
            return;
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while self.delivered_turns(session_id).await <= before
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    async fn forget_session(&self, session_id: &str) {
        self.sessions.lock().await.remove(session_id);
        self.modes.lock().await.remove(session_id);
        self.running.lock().await.remove(session_id);
        self.cancel_flags.lock().await.remove(session_id);
        self.announced_titles.lock().await.remove(session_id);
        self.delivered_turns.lock().await.remove(session_id);
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

    /// `{"clientInfo": .., "clientCapabilities": ..}` as sent at `initialize`
    /// (`null` before it).
    pub async fn client_info(&self) -> Value {
        self.store.client_info.lock().await.clone()
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
        if req.method.starts_with("fs/") || req.method.starts_with("terminal/") {
            let session_id = req
                .params
                .get("sessionId")
                .or_else(|| req.params.get("session_id"))
                .and_then(Value::as_str);
            if self.host.tools_restricted(session_id).await {
                return err(id, -32601, format!("Method not found: {}", req.method));
            }
        }
        match req.method.as_str() {
            "initialize" => {
                let meta = match self.host.initialize(&req.params).await {
                    Ok(meta) => meta,
                    Err(e) => return err(id, -32602, e),
                };
                *self.store.client_info.lock().await = json!({
                    "clientInfo": req.params.get("clientInfo").cloned().unwrap_or(Value::Null),
                    "clientCapabilities": req
                        .params
                        .get("clientCapabilities")
                        .cloned()
                        .unwrap_or(Value::Null),
                });
                // Version 1 is the only one spoken: a client asking for a
                // newer one is told what this agent supports instead.
                let mut result = json!({
                    "protocolVersion": 1,
                    "agentCapabilities": agent_capabilities(self.host.supports_load_session()),
                    "agentInfo": {
                        "name": "kkagent",
                        "title": "kkagent",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                    "authMethods": auth_methods(),
                });
                if let Some(meta) = meta {
                    result["_meta"] = meta;
                }
                ok(id, result)
            }
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
                let setup = match SessionSetup::from_params(&req.params) {
                    Ok(setup) => setup,
                    Err(e) => return err(id, -32602, e),
                };
                if let Err(e) = self
                    .host
                    .create_session(&sid, &cwd.to_string_lossy(), &setup)
                    .await
                {
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
                    "modes": session_mode_state("agent"),
                });
                // initialMessage (optional): run a prompt turn in the
                // background without blocking the session/new response.
                if let Some(initial) = req.params.get("initialMessage") {
                    self.spawn_prompt_turn(&sid, initial.clone()).await;
                }
                ok(id, result)
            }
            "session/load" | "session/resume" => {
                // `load` replays the history to the client; `resume` only
                // reattaches (the client already has it).
                // A host that keeps its own copy of the conversation can ask
                // for no replay: `_meta["kkagent/host"].replay = false`.
                let replay = req.method == "session/load"
                    && req
                        .params
                        .get("_meta")
                        .and_then(|meta| meta.get(kkagent_protocol::host::HOST_META_KEY))
                        .and_then(|host| host.get("replay"))
                        .and_then(Value::as_bool)
                        .unwrap_or(true);
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
                let setup = match SessionSetup::from_params(&req.params) {
                    Ok(setup) => setup,
                    Err(e) => return err(id, -32602, e),
                };
                if let Err(e) = self
                    .host
                    .load_session(&transcript_id, &cwd.to_string_lossy(), &setup)
                    .await
                {
                    return host_failure(id, e);
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
                    "modes": session_mode_state("agent"),
                });
                if replay {
                    match self.host.session_history(&transcript_id).await {
                        Ok(history) => self.replay_history(&transcript_id, history).await,
                        Err(e) => tracing::warn!("ACP session/load replay failed: {e}"),
                    }
                }
                if let Some(initial) = req.params.get("initialMessage") {
                    self.spawn_prompt_turn(&transcript_id, initial.clone())
                        .await;
                }
                ok(id, result)
            }
            "session/list" => {
                let cwd = req
                    .params
                    .get("cwd")
                    .and_then(Value::as_str)
                    .filter(|cwd| !cwd.is_empty());
                match self.host.list_sessions(cwd).await {
                    Ok(Some(sessions)) => ok(id, json!({"sessions": sessions})),
                    Ok(None) => {
                        let sessions: Vec<Value> = self
                            .store
                            .sessions
                            .lock()
                            .await
                            .values()
                            .filter(|session| {
                                cwd.is_none_or(|cwd| {
                                    session.get("cwd").and_then(Value::as_str) == Some(cwd)
                                })
                            })
                            .map(|session| {
                                json!({
                                    "sessionId": session.get("sessionId"),
                                    "cwd": session.get("cwd"),
                                })
                            })
                            .collect();
                        ok(id, json!({"sessions": sessions}))
                    }
                    Err(e) => host_failure(id, e),
                }
            }
            "session/close" => {
                let sid = session_id_from(&req.params);
                if sid.is_empty() {
                    return err(id, -32602, "sessionId is required");
                }
                if !self.known_session(&sid).await {
                    return session_not_found(id, &sid);
                }
                self.cancel_session(&sid).await;
                if let Err(e) = self.host.close_session(&sid).await {
                    return host_failure(id, e);
                }
                self.store.forget_session(&sid).await;
                ok(id, json!({}))
            }
            "session/delete" => {
                let sid = session_id_from(&req.params);
                if sid.is_empty() {
                    return err(id, -32602, "sessionId is required");
                }
                self.cancel_session(&sid).await;
                if let Err(e) = self.host.delete_session(&sid).await {
                    return host_failure(id, e);
                }
                self.store.forget_session(&sid).await;
                ok(id, json!({}))
            }
            "session/prompt" | "prompt" => {
                let sid = session_id_from(&req.params);
                if sid.is_empty() {
                    return err(id, -32602, "sessionId is required");
                }
                if !self.known_session(&sid).await {
                    return session_not_found(id, &sid);
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
                // `/name args` runs one of the advertised commands.
                if let Some((name, args)) = parse_slash_command(&ContentBlock::join_text(&blocks)) {
                    return self.run_slash_command(id, &sid, &name, &args).await;
                }
                // Reject concurrent turns on the same session.
                {
                    let mut running = self.store.running.lock().await;
                    if running.contains_key(&sid) {
                        return err(id, -32000, format!("session is busy: {sid}"));
                    }
                    running.insert(sid.clone(), ());
                }
                // A cancel aimed at an earlier turn must not poison this one.
                let cancelled = self.store.cancel_flag(&sid).await;
                cancelled.store(false, Ordering::SeqCst);
                let delivered_before = self.store.delivered_turns(&sid).await;
                let outcome = self
                    .host
                    .prompt(&sid, &ContentBlock::join_text(&blocks))
                    .await;
                if outcome.is_ok() {
                    self.store.wait_turn_delivered(&sid, delivered_before).await;
                }
                self.store.running.lock().await.remove(&sid);
                let was_cancelled = cancelled.load(Ordering::SeqCst);
                match outcome {
                    // A turn that was cancelled ends with `cancelled`, however
                    // the host wound it down (clean stop or aborted work).
                    _ if was_cancelled => ok(id, json!({"stopReason": "cancelled"})),
                    Ok(v) => {
                        let stop = normalize_stop_reason(
                            v.get("stopReason")
                                .and_then(|s| s.as_str())
                                .unwrap_or("end_turn"),
                        );
                        self.announce_session_info(&sid).await;
                        ok(id, json!({"stopReason": stop}))
                    }
                    Err(e) => host_failure(id, e),
                }
            }
            // Official `session/cancel` is a notification (see
            // `handle_notification`); the request form is accepted too.
            "session/cancel" => {
                let sid = session_id_from(&req.params);
                self.cancel_session(&sid).await;
                ok(id, json!({}))
            }
            "session/set_mode" | "session/mode" => {
                let sid = session_id_from(&req.params);
                let mode = req
                    .params
                    .get("modeId")
                    .or_else(|| req.params.get("mode"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("agent")
                    .to_string();
                if !self.known_session(&sid).await {
                    return session_not_found(id, &sid);
                }
                if let Err(e) = self.host.set_mode(&sid, &mode).await {
                    return host_failure(id, e);
                }
                self.store
                    .modes
                    .lock()
                    .await
                    .insert(sid.clone(), mode.clone());
                if let Some(s) = self.store.sessions.lock().await.get_mut(&sid) {
                    s["mode"] = json!(mode);
                }
                self.notify_update(
                    &sid,
                    SessionUpdate::CurrentModeUpdate {
                        current_mode_id: mode.clone(),
                    },
                )
                .await;
                ok(id, json!({"ok": true, "mode": mode}))
            }
            "model/list" | "models/list" | "modelCatalog/list" => {
                ok(id, self.host.list_models().await)
            }
            "session/set_model" => {
                let sid = session_id_from(&req.params);
                let model = req
                    .params
                    .get("modelId")
                    .or_else(|| req.params.get("model"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if !self.known_session(&sid).await {
                    return session_not_found(id, &sid);
                }
                if let Err(e) = self.host.set_model(&sid, &model).await {
                    return host_failure(id, e);
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

    /// Tell the host to stop the session's turn and release anything waiting
    /// for the client on its behalf (pending permission / input requests).
    async fn cancel_session(&self, session_id: &str) {
        self.store
            .cancel_flag(session_id)
            .await
            .store(true, Ordering::SeqCst);
        if let Err(e) = self.host.cancel(session_id).await {
            tracing::debug!("ACP cancel of {session_id}: {e}");
        }
    }

    /// Send one `session/update` to the client, if one is connected.
    async fn notify_update(&self, session_id: &str, update: SessionUpdate) {
        if let Some(bridge) = self.client().await {
            bridge
                .notify(
                    "session/update",
                    json!({"sessionId": session_id, "update": update}),
                )
                .await;
        }
    }

    /// `session_info_update` when the session's title appeared or changed.
    async fn announce_session_info(&self, session_id: &str) {
        let Some(title) = self.host.session_title(session_id).await else {
            return;
        };
        {
            let mut announced = self.store.announced_titles.lock().await;
            if announced.get(session_id) == Some(&title) {
                return;
            }
            announced.insert(session_id.to_string(), title.clone());
        }
        self.notify_update(
            session_id,
            SessionUpdate::SessionInfoUpdate {
                title: Some(title),
                updated_at: Some(chrono::Utc::now().to_rfc3339()),
            },
        )
        .await;
    }

    /// Follow-ups that must reach the client only after it has the response:
    /// a freshly created / attached session announces its slash commands.
    async fn after_response(&self, method: &str, response: &AcpResponse) {
        if !matches!(method, "session/new" | "session/load" | "session/resume") {
            return;
        }
        let Some(sid) = response
            .result
            .as_ref()
            .and_then(|result| result.get("sessionId"))
            .and_then(Value::as_str)
        else {
            return;
        };
        let commands = builtin_command_list()
            .into_iter()
            .filter_map(|command| serde_json::from_value::<AvailableCommand>(command).ok())
            .collect();
        self.notify_update(
            sid,
            SessionUpdate::AvailableCommandsUpdate {
                available_commands: commands,
            },
        )
        .await;
    }

    /// Answer an advertised slash command from the prompt text itself, so
    /// clients can offer the commands from `available_commands_update`.
    async fn run_slash_command(
        &self,
        id: Value,
        session_id: &str,
        name: &str,
        args: &str,
    ) -> AcpResponse {
        let reply =
            match builtins::run_builtin(name, args, session_id, &self.store, self.host.as_ref())
                .await
            {
                Ok(value) => value
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| serde_json::to_string_pretty(&value).unwrap_or_default()),
                Err(e) => format!("/{name} failed: {e}"),
            };
        self.notify_update(session_id, SessionUpdate::agent_message_chunk(reply))
            .await;
        ok(id, json!({"stopReason": "end_turn"}))
    }

    /// Notifications (requests without an id). Only cancellation is defined.
    async fn handle_notification(
        &self,
        req: &AcpRequest,
        inflight: &Mutex<HashMap<String, Inflight>>,
        bridge: &ClientBridge,
    ) {
        match req.method.as_str() {
            "session/cancel" => {
                self.cancel_session(&session_id_from(&req.params)).await;
            }
            "$/cancel_request" => {
                let Some(request_id) = req.params.get("requestId").or_else(|| req.params.get("id"))
                else {
                    return;
                };
                let entry = {
                    let inflight = inflight.lock().await;
                    inflight.get(&id_key(request_id)).map(|entry| {
                        (
                            entry.method.clone(),
                            entry.session_id.clone(),
                            entry.cancelled.clone(),
                            entry.id.clone(),
                        )
                    })
                };
                let Some((method, session_id, cancelled, id)) = entry else {
                    return;
                };
                match (method.as_str(), session_id) {
                    // A turn is cancelled cooperatively and still answers with
                    // `stopReason: cancelled`.
                    ("session/prompt" | "prompt", Some(session_id)) => {
                        self.cancel_session(&session_id).await;
                    }
                    // Anything else is answered right away; its late result
                    // is dropped.
                    _ => {
                        if !cancelled.swap(true, Ordering::SeqCst) {
                            if let Ok(out) =
                                serde_json::to_string(&err(id, -32800, "Request cancelled"))
                            {
                                bridge.send_raw(out);
                            }
                        }
                    }
                }
            }
            _ => {}
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
        let text_of = |key: &str| event.get(key).and_then(|v| v.as_str()).unwrap_or("");
        let call_id = || {
            event
                .get("tool_call_id")
                .or_else(|| event.get("call_id"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        };
        let update = match ty {
            "message_delta" | "MessageDelta" => SessionUpdate::agent_message_chunk(text_of("text")),
            "thinking_delta" | "ThinkingDelta" => {
                SessionUpdate::agent_thought_chunk(text_of("text"))
            }
            "tool_call" | "ToolCall" => {
                let tool_name = event
                    .get("tool_name")
                    .and_then(|v| v.as_str())
                    .filter(|name| !name.is_empty())
                    .unwrap_or("tool")
                    .to_string();
                let kind = ToolKind::from_tool_name(&tool_name);
                SessionUpdate::ToolCall {
                    tool_call_id: call_id(),
                    title: tool_name.clone(),
                    kind,
                    status: ToolCallStatus::Pending,
                    raw_input: event.get("input").cloned().unwrap_or(Value::Null),
                    raw_output: None,
                    locations: vec![],
                    content: vec![],
                    tool_kind: kind,
                    tool_name,
                }
            }
            "tool_execution_status" | "ToolExecutionStatus" => {
                // Only the start of execution is news; "queued" is the state
                // the call was announced in.
                if text_of("status") != "running" {
                    return false;
                }
                SessionUpdate::ToolCallUpdate {
                    tool_call_id: call_id(),
                    status: Some(ToolCallStatus::InProgress),
                    title: None,
                    kind: None,
                    content: vec![],
                    raw_input: None,
                    raw_output: None,
                }
            }
            "tool_result" | "ToolResult" => {
                let output = event
                    .get("output")
                    .and_then(|v| v.as_str())
                    .or_else(|| event.get("text").and_then(|v| v.as_str()))
                    .unwrap_or("");
                let failed = event
                    .get("is_error")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                // A structured change becomes a real `diff` content block, so
                // a client can render the edit instead of parsing prose. The
                // text block stays first and is always present.
                let mut content = vec![ToolCallContent::text(output)];
                if let Some(changes) = event.get("changes").and_then(|v| v.as_array()) {
                    for change in changes {
                        let Some(path) = change.get("path").and_then(|v| v.as_str()) else {
                            continue;
                        };
                        let Some(new_text) = change.get("new_text").and_then(|v| v.as_str()) else {
                            continue;
                        };
                        content.push(ToolCallContent::Diff {
                            path: path.to_string(),
                            old_text: change
                                .get("old_text")
                                .and_then(|v| v.as_str())
                                .map(str::to_string),
                            new_text: new_text.to_string(),
                        });
                    }
                }
                SessionUpdate::ToolCallUpdate {
                    tool_call_id: call_id(),
                    status: Some(if failed {
                        ToolCallStatus::Failed
                    } else {
                        ToolCallStatus::Completed
                    }),
                    title: None,
                    kind: None,
                    // One shape for every finished call: the result text is in
                    // `rawOutput` and in `content[0]` alike, empty or not, so
                    // a host never has to guess which one to read.
                    content,
                    raw_input: None,
                    raw_output: Some(Value::String(output.to_string())),
                }
            }
            "plan_mode_changed" | "PlanModeChanged" => {
                let enabled = event
                    .get("enabled")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let mode = if enabled { "plan" } else { "agent" };
                if let Some(session) = self.store.sessions.lock().await.get_mut(&sid) {
                    session["mode"] = json!(mode);
                }
                self.store
                    .modes
                    .lock()
                    .await
                    .insert(sid.clone(), mode.to_string());
                SessionUpdate::CurrentModeUpdate {
                    current_mode_id: mode.to_string(),
                }
            }
            "todo_updated" | "TodoUpdated" => {
                let entries = event
                    .get("items")
                    .and_then(|v| v.as_array())
                    .map(|items| {
                        items
                            .iter()
                            .map(|item| PlanEntry {
                                id: item
                                    .get("id")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or_default()
                                    .to_string(),
                                content: item
                                    .get("content")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or_default()
                                    .to_string(),
                                priority: PlanPriority::Medium,
                                status: match item.get("status").and_then(|v| v.as_str()) {
                                    Some("completed") => PlanStatus::Completed,
                                    Some("in_progress") => PlanStatus::InProgress,
                                    _ => PlanStatus::Pending,
                                },
                                meta: None,
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                SessionUpdate::Plan { entries }
            }
            "usage_update" | "UsageUpdate" => {
                let used: u64 = event
                    .get("context")
                    .map(|context| {
                        ["system", "conversation", "tools", "media"]
                            .iter()
                            .filter_map(|key| context.get(key).and_then(|v| v.as_u64()))
                            .sum()
                    })
                    .unwrap_or(0);
                let has_usage = event.get("usage").is_some_and(|usage| !usage.is_null());
                // Host first (it owns the session model), then the event's own
                // `context_window` (the model that actually ran, which may be a
                // fallback): a call that used a different model than the session
                // default still reports its real window.
                let size = match self.host.context_window(&sid).await {
                    Some(size) if size > 0 => size,
                    _ => event
                        .get("context_window")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0),
                };
                // A host that knows no window size still needs the token
                // numbers, so `size: 0` means "unknown" rather than "drop it".
                if used == 0 && !has_usage {
                    return false;
                }
                // Standard v1 has no token accounting, so the per-call numbers
                // ride in `_meta` (a host that ignores it is unaffected).
                let mut meta = serde_json::Map::new();
                if let Some(usage) = event.get("usage") {
                    meta.insert("usage".into(), usage.clone());
                }
                if let Some(model) = event.get("model").and_then(|v| v.as_str()) {
                    meta.insert("model".into(), Value::String(model.to_string()));
                }
                if size > 0 {
                    meta.insert("contextWindow".into(), Value::from(size));
                }
                for key in ["steps", "turns", "byModel"] {
                    let source = if key == "byModel" { "by_model" } else { key };
                    if let Some(value) = event.get(source) {
                        meta.insert(key.to_string(), value.clone());
                    }
                }
                SessionUpdate::UsageUpdate {
                    used,
                    size,
                    meta: Value::Object(meta),
                }
            }
            // Everything else (status, heartbeats) has no variant in official
            // v1; the prompt response carries the outcome. The non-standard
            // updates below are prefixed `kkagent_` so a standard client
            // ignores them instead of guessing.
            "notice" | "Notice" => SessionUpdate::Notice {
                level: "info".to_string(),
                message: event
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            },
            "llm_retry" | "LlmRetry" => {
                // One update per retry, not one per countdown tick: a client
                // renders "retrying in Ns" itself from `waitSeconds`.
                if !event
                    .get("initial")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                {
                    return false;
                }
                SessionUpdate::Retry {
                    retry_number: event
                        .get("retry_number")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as u32,
                    reason: event
                        .get("reason")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    wait_seconds: event
                        .get("wait_seconds")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0),
                    remaining_seconds: event
                        .get("remaining_seconds")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0),
                    initial: true,
                }
            }
            "auto_compact_completed" | "AutoCompactCompleted" => SessionUpdate::Compaction {
                kind: "auto".to_string(),
                removed: event
                    .get("compacted_count")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                archive_session_id: event
                    .get("archive_session_id")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                error: None,
            },
            "compact_completed" | "CompactCompleted" => SessionUpdate::Compaction {
                kind: "manual".to_string(),
                removed: event.get("deleted").and_then(|v| v.as_u64()).unwrap_or(0),
                archive_session_id: event
                    .get("archive_session_id")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                error: event
                    .get("error")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
            },
            "skill_activated" | "SkillActivated" => SessionUpdate::SkillActivated {
                skill_name: event
                    .get("skill_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                trigger: event
                    .get("trigger")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            },
            _ => return false,
        };
        bridge
            .notify(
                "session/update",
                json!({"sessionId": sid, "update": update}),
            )
            .await;
        true
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
            tool_call: PermissionToolCall {
                tool_call_id: payload
                    .get("tool_call_id")
                    .or_else(|| payload.get("call_id"))
                    .and_then(|v| v.as_str())
                    .filter(|id| !id.is_empty())
                    .unwrap_or(&approval_id)
                    .to_string(),
                title: tool_name.to_string(),
                kind: ToolKind::from_tool_name(tool_name),
                status: ToolCallStatus::Pending,
                raw_input: input,
                locations: vec![],
            },
            options: PermissionOption::standard_set(),
            kind: Some(kind),
        };
        // No timeout: the user may take as long as they need. A cancelled
        // turn (`session/cancel`) or a lost client ends the wait.
        let cancel = self.store.cancel_flag(&sid).await;
        let outcome = bridge
            .request_with(
                "session/request_permission",
                serde_json::to_value(&request).unwrap_or(Value::Null),
                None,
                Some(cancel),
            )
            .await;
        let outcome = match outcome {
            Ok(response) => PermissionOutcome::from_response(&response),
            Err(_) => PermissionOutcome::Cancelled,
        };
        // Route the decision back into the host's approval channel.
        let mut payload = json!({
            "approval_id": approval_id,
            "session_id": sid,
        });
        match &outcome {
            PermissionOutcome::Selected { option_id } => {
                payload["optionId"] = json!(option_id);
                payload["approve"] = json!(outcome.grants());
            }
            // Cancelled is a refusal that is not a decision: no `approve`,
            // so nothing is granted and nothing is remembered.
            PermissionOutcome::Cancelled => {
                payload["decision"] = json!("cancelled");
            }
        }
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
        let cancel = self.store.cancel_flag(&sid).await;
        let answer = bridge
            .request_with(
                "session/request_input",
                serde_json::to_value(&request).unwrap_or(Value::Null),
                None,
                Some(cancel),
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
        server.host.bind_client(bridge.clone());

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
            server
                .store
                .forwarding
                .store(server.host.signals_turn_commit(), Ordering::SeqCst);
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
                                "turn_committed" | "TurnCommitted" => {
                                    // Everything before this event is on
                                    // the wire already (events are handled
                                    // in order).
                                    if let Some(sid) = event
                                        .get("session_id")
                                        .or_else(|| event.get("sessionId"))
                                        .and_then(|v| v.as_str())
                                    {
                                        *forward_server
                                            .store
                                            .delivered_turns
                                            .lock()
                                            .await
                                            .entry(sid.to_string())
                                            .or_default() += 1;
                                    }
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
        let inflight: Arc<Mutex<HashMap<String, Inflight>>> = Arc::new(Mutex::new(HashMap::new()));
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
                let id = value.get("id").map(id_key).unwrap_or_default();
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
            let Some(request_id) = req.id.clone() else {
                // Notifications carry no response: `session/cancel` and
                // `$/cancel_request` act, everything else is ignored.
                let notify_server = Arc::clone(&server);
                let notify_inflight = Arc::clone(&inflight);
                let notify_bridge = bridge.clone();
                tasks.spawn(async move {
                    notify_server
                        .handle_notification(&req, &notify_inflight, &notify_bridge)
                        .await;
                });
                continue;
            };
            let key = id_key(&request_id);
            let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
            inflight.lock().await.insert(
                key.clone(),
                Inflight {
                    id: request_id,
                    method: req.method.clone(),
                    session_id: Some(session_id_from(&req.params)).filter(|sid| !sid.is_empty()),
                    cancelled: Arc::clone(&cancelled),
                },
            );
            let request_server = Arc::clone(&server);
            let request_tx = tx.clone();
            let request_inflight = Arc::clone(&inflight);
            let request = req;
            tasks.spawn(async move {
                let method = request.method.clone();
                let response = request_server.handle(request).await;
                request_inflight.lock().await.remove(&key);
                // Already answered with "cancelled": the late result is stale.
                if cancelled.load(Ordering::SeqCst) {
                    return;
                }
                if let Ok(out) = serde_json::to_string(&response) {
                    let _ = request_tx.send(out);
                }
                request_server.after_response(&method, &response).await;
            });
        }
        // stdin closed: the client can no longer answer anything.
        bridge.fail_all().await;
        // Let in-flight requests finish (bounded), then give
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

/// `"/status extra args"` -> `("status", "extra args")` when `status` is one of
/// the advertised commands; anything else is an ordinary prompt.
fn parse_slash_command(text: &str) -> Option<(String, String)> {
    let trimmed = text.trim();
    let rest = trimmed.strip_prefix('/')?;
    let (name, args) = rest
        .split_once(char::is_whitespace)
        .map_or((rest, ""), |(name, args)| (name, args.trim()));
    builtin_command_list()
        .iter()
        .any(|command| command.get("name").and_then(Value::as_str) == Some(name))
        .then(|| (name.to_string(), args.to_string()))
}

fn session_id_from(params: &Value) -> String {
    params
        .get("sessionId")
        .or_else(|| params.get("session_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// Unknown sessions answer with the protocol's "resource not found" code;
/// every other host failure is a generic server error.
fn host_failure(id: Value, message: String) -> AcpResponse {
    let code = if message.starts_with("session not found") {
        -32002
    } else {
        -32000
    };
    err(id, code, message)
}

fn session_not_found(id: Value, session_id: &str) -> AcpResponse {
    err(id, -32002, format!("session not found: {session_id}"))
}

/// Request ids are numbers or strings; both must map to one stable key.
fn id_key(id: &Value) -> String {
    match id {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Stop reasons a client understands.
fn normalize_stop_reason(raw: &str) -> &'static str {
    match raw {
        "max_tokens" => "max_tokens",
        "max_turn_requests" => "max_turn_requests",
        "refusal" | "refused" => "refusal",
        "cancelled" | "canceled" => "cancelled",
        _ => "end_turn",
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

    /// A bridge wired to an in-memory "client" the test drives by hand.
    fn test_bridge() -> (ClientBridge, tokio::sync::mpsc::UnboundedReceiver<String>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (ClientBridge::new(tx), rx)
    }

    #[tokio::test]
    async fn bridge_request_keeps_the_rpc_error_code() {
        let (bridge, mut outgoing) = test_bridge();
        let waiter = {
            let bridge = bridge.clone();
            tokio::spawn(async move {
                bridge
                    .request_with("host/ping", json!({"a": 1}), None, None)
                    .await
            })
        };
        let sent: Value = serde_json::from_str(&outgoing.recv().await.unwrap()).unwrap();
        assert_eq!(sent["method"], "host/ping");
        assert_eq!(sent["params"]["a"], 1);
        let id = sent["id"].as_str().unwrap().to_string();
        assert!(
            bridge
                .resolve(
                    &id,
                    Err(json!({"code": -32002, "message": "no such thing"}))
                )
                .await
        );
        assert_eq!(
            waiter.await.unwrap(),
            Err(BridgeError::Rpc {
                code: -32002,
                message: "no such thing".into()
            })
        );
    }

    #[tokio::test]
    async fn bridge_cancel_flag_notifies_the_client_and_returns_cancelled() {
        let (bridge, mut outgoing) = test_bridge();
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let waiter = {
            let bridge = bridge.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move {
                bridge
                    .request_with("host/slow", json!({}), None, Some(cancel))
                    .await
            })
        };
        let sent: Value = serde_json::from_str(&outgoing.recv().await.unwrap()).unwrap();
        let id = sent["id"].as_str().unwrap().to_string();
        cancel.store(true, Ordering::SeqCst);
        assert_eq!(waiter.await.unwrap(), Err(BridgeError::Cancelled));
        let notice: Value = serde_json::from_str(&outgoing.recv().await.unwrap()).unwrap();
        assert_eq!(notice["method"], "$/cancel_request");
        assert_eq!(notice["params"]["requestId"], id.as_str());
        assert!(notice.get("id").is_none(), "a notification carries no id");
        // The abandoned request no longer waits for an answer.
        assert!(!bridge.resolve(&id, Ok(json!({}))).await);
    }

    #[tokio::test]
    async fn bridge_times_out_and_fails_pending_requests_on_disconnect() {
        let (bridge, mut outgoing) = test_bridge();
        let timed_out = bridge
            .request_with(
                "host/slow",
                json!({}),
                Some(Duration::from_millis(30)),
                None,
            )
            .await;
        assert_eq!(timed_out, Err(BridgeError::TimedOut));
        // Timing out also tells the client to stop.
        let _request = outgoing.recv().await.unwrap();
        let notice: Value = serde_json::from_str(&outgoing.recv().await.unwrap()).unwrap();
        assert_eq!(notice["method"], "$/cancel_request");

        let waiter = {
            let bridge = bridge.clone();
            tokio::spawn(async move {
                bridge
                    .request_with("host/wait", json!({}), None, None)
                    .await
            })
        };
        let _request = outgoing.recv().await.unwrap();
        bridge.fail_all().await;
        assert_eq!(waiter.await.unwrap(), Err(BridgeError::Disconnected));

        // With the receiving side gone, sending fails immediately.
        drop(outgoing);
        assert_eq!(
            bridge.request_with("host/x", json!({}), None, None).await,
            Err(BridgeError::Disconnected)
        );
    }

    #[tokio::test]
    async fn initialize_hook_can_reject_and_contribute_meta() {
        struct MetaHost;
        #[async_trait::async_trait]
        impl AcpHost for MetaHost {
            async fn prompt(&self, _sid: &str, _text: &str) -> Result<Value, String> {
                Ok(json!({"stopReason": "end_turn"}))
            }
            async fn cancel(&self, _sid: &str) -> Result<(), String> {
                Ok(())
            }
            async fn initialize(&self, params: &Value) -> Result<Option<Value>, String> {
                if params.get("reject").is_some() {
                    return Err("not acceptable".into());
                }
                Ok(Some(json!({"kkagent/host": {"tools": true}})))
            }
        }
        let server = AcpServer::with_host(Arc::new(MetaHost));
        let request = |params: Value| AcpRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(1)),
            method: "initialize".into(),
            params,
        };
        let accepted = server.handle(request(json!({}))).await;
        assert_eq!(
            accepted.result.unwrap()["_meta"]["kkagent/host"]["tools"],
            true
        );
        let rejected = server.handle(request(json!({"reject": true}))).await;
        let error = rejected.error.unwrap();
        assert_eq!(error["code"], -32602);
        assert!(error["message"]
            .as_str()
            .unwrap()
            .contains("not acceptable"));
    }

    #[tokio::test]
    async fn restricted_host_refuses_fs_and_terminal_families() {
        struct RestrictedHost;
        #[async_trait::async_trait]
        impl AcpHost for RestrictedHost {
            async fn prompt(&self, _sid: &str, _text: &str) -> Result<Value, String> {
                Ok(json!({"stopReason": "end_turn"}))
            }
            async fn cancel(&self, _sid: &str) -> Result<(), String> {
                Ok(())
            }
            async fn tools_restricted(&self, _session_id: Option<&str>) -> bool {
                true
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let server = AcpServer::with_host(Arc::new(RestrictedHost));
        let sid = new_session(&server, dir.path().to_str().unwrap()).await;
        for (method, params) in [
            (
                "fs/write_text_file",
                json!({"sessionId": sid, "path": "x.txt", "content": "x"}),
            ),
            (
                "fs/read_text_file",
                json!({"sessionId": sid, "path": "x.txt"}),
            ),
            (
                "terminal/create",
                json!({"sessionId": sid, "command": "echo hi"}),
            ),
        ] {
            let resp = server
                .handle(AcpRequest {
                    jsonrpc: "2.0".into(),
                    id: Some(json!(1)),
                    method: method.into(),
                    params,
                })
                .await;
            let error = resp
                .error
                .unwrap_or_else(|| panic!("{method} must be refused"));
            assert_eq!(error["code"], -32601, "{method}");
        }
        assert!(
            !dir.path().join("x.txt").exists(),
            "a refused fs/write must not touch disk"
        );
        // Non-fs methods are unaffected.
        let ping = server
            .handle(AcpRequest {
                jsonrpc: "2.0".into(),
                id: Some(json!(2)),
                method: "ping".into(),
                params: json!({}),
            })
            .await;
        assert!(ping.error.is_none());
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
        assert_eq!(result["modes"]["currentModeId"], "agent");
        let modes = result["modes"]["availableModes"].as_array().unwrap();
        assert!(modes.iter().any(|m| m["id"] == "plan"));
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
        // Official shape: the call is described by `toolCall`, choices by
        // `optionId` + `kind`.
        assert_eq!(msg["params"]["toolCall"]["title"], "Bash");
        assert_eq!(msg["params"]["toolCall"]["status"], "pending");
        assert_eq!(
            msg["params"]["toolCall"]["rawInput"]["command"],
            "cargo test"
        );
        let option_ids: Vec<&str> = msg["params"]["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o["optionId"].as_str().unwrap())
            .collect();
        assert_eq!(
            option_ids,
            vec!["allow_once", "allow_always", "reject_once", "reject_always"]
        );
        let option_kinds: Vec<&str> = msg["params"]["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o["kind"].as_str().unwrap())
            .collect();
        assert_eq!(
            option_kinds,
            vec!["allow_once", "allow_always", "reject_once", "reject_always"]
        );

        let request_id = msg["id"].as_str().unwrap().to_string();
        let answered = server_bridge
            .resolve(
                &request_id,
                Ok(json!({"outcome": {"outcome": "selected", "optionId": "allow_once"}})),
            )
            .await;
        assert!(answered);
        assert!(task.await.is_ok());

        let approvals = host.approvals.lock().await;
        assert_eq!(approvals.len(), 1);
        assert_eq!(approvals[0]["approval_id"], "ap_1");
        assert_eq!(approvals[0]["approve"], true);
        assert_eq!(approvals[0]["optionId"], "allow_once");
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
            .resolve(
                &request_id,
                Ok(json!({"outcome": {"outcome": "cancelled"}})),
            )
            .await;
        assert!(task.await.is_ok());
        let approvals = host.approvals.lock().await;
        assert!(approvals[0].get("approve").is_none());
        assert_eq!(approvals[0]["decision"], "cancelled");
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

    fn rpc(method: &str, params: Value) -> AcpRequest {
        AcpRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(7)),
            method: method.into(),
            params,
        }
    }

    /// Host with a persisted-session list and a controllable prompt turn.
    struct LifecycleHost {
        stop: Mutex<String>,
        started: tokio::sync::Notify,
        release: tokio::sync::Notify,
        block: bool,
        cancels: Mutex<Vec<String>>,
        closed: Mutex<Vec<String>>,
        deleted: Mutex<Vec<String>>,
        loaded: Mutex<Vec<String>>,
        history: Vec<Value>,
        title: Option<String>,
    }

    impl LifecycleHost {
        fn new(block: bool) -> Self {
            Self {
                stop: Mutex::new("end_turn".into()),
                started: tokio::sync::Notify::new(),
                release: tokio::sync::Notify::new(),
                block,
                cancels: Mutex::new(vec![]),
                closed: Mutex::new(vec![]),
                deleted: Mutex::new(vec![]),
                loaded: Mutex::new(vec![]),
                history: vec![],
                title: None,
            }
        }
    }

    #[async_trait::async_trait]
    impl AcpHost for LifecycleHost {
        async fn prompt(&self, _sid: &str, _text: &str) -> Result<Value, String> {
            self.started.notify_one();
            if self.block {
                self.release.notified().await;
            }
            Ok(json!({"stopReason": self.stop.lock().await.clone()}))
        }
        async fn cancel(&self, sid: &str) -> Result<(), String> {
            self.cancels.lock().await.push(sid.to_string());
            self.release.notify_one();
            Ok(())
        }
        async fn load_session(
            &self,
            sid: &str,
            _cwd: &str,
            _setup: &SessionSetup,
        ) -> Result<(), String> {
            if sid == "gone" {
                return Err("session not found: gone".into());
            }
            self.loaded.lock().await.push(sid.to_string());
            Ok(())
        }
        fn supports_load_session(&self) -> bool {
            true
        }
        async fn session_history(&self, _sid: &str) -> Result<Vec<Value>, String> {
            Ok(self.history.clone())
        }
        async fn list_sessions(&self, cwd: Option<&str>) -> Result<Option<Vec<Value>>, String> {
            let all = vec![
                json!({"sessionId": "s1", "cwd": "/a", "title": "One"}),
                json!({"sessionId": "s2", "cwd": "/b"}),
            ];
            Ok(Some(
                all.into_iter()
                    .filter(|s| cwd.is_none_or(|cwd| s["cwd"] == cwd))
                    .collect(),
            ))
        }
        async fn close_session(&self, sid: &str) -> Result<(), String> {
            self.closed.lock().await.push(sid.to_string());
            Ok(())
        }
        async fn delete_session(&self, sid: &str) -> Result<(), String> {
            self.deleted.lock().await.push(sid.to_string());
            Ok(())
        }
        async fn session_title(&self, _sid: &str) -> Option<String> {
            self.title.clone()
        }
    }

    #[tokio::test]
    async fn resume_attaches_without_replay_and_load_replays() {
        let mut host = LifecycleHost::new(false);
        host.history = vec![json!({"role": "user", "blocks": [{"type": "text", "text": "hi"}]})];
        let host = Arc::new(host);
        let server = AcpServer::with_host(Arc::clone(&host) as Arc<dyn AcpHost>);
        let (bridge, mut outgoing) = test_bridge();
        server.attach_client_bridge(bridge).await;

        let resumed = server
            .handle(rpc(
                "session/resume",
                json!({"sessionId": "s1", "cwd": "/tmp"}),
            ))
            .await;
        let result = resumed.result.unwrap();
        assert_eq!(result["sessionId"], "s1");
        assert_eq!(result["modes"]["currentModeId"], "agent");
        assert!(
            outgoing.try_recv().is_err(),
            "resume must not replay history"
        );

        let loaded = server
            .handle(rpc(
                "session/load",
                json!({"sessionId": "s1", "cwd": "/tmp"}),
            ))
            .await;
        assert!(loaded.error.is_none());
        let replay: Value = serde_json::from_str(&outgoing.recv().await.unwrap()).unwrap();
        assert_eq!(
            replay["params"]["update"]["sessionUpdate"],
            "user_message_chunk"
        );
        assert_eq!(*host.loaded.lock().await, vec!["s1", "s1"]);

        // The resumed session accepts prompts.
        let prompt = server
            .handle(rpc(
                "session/prompt",
                json!({"sessionId": "s1", "prompt": [{"type": "text", "text": "go"}]}),
            ))
            .await;
        assert_eq!(prompt.result.unwrap()["stopReason"], "end_turn");
    }

    #[tokio::test]
    async fn load_can_skip_the_replay_for_a_host_that_keeps_its_own_history() {
        let mut host = LifecycleHost::new(false);
        host.history = vec![json!({"role": "user", "blocks": [{"type": "text", "text": "hi"}]})];
        let host = Arc::new(host);
        let server = AcpServer::with_host(Arc::clone(&host) as Arc<dyn AcpHost>);
        let (bridge, mut outgoing) = test_bridge();
        server.attach_client_bridge(bridge).await;

        let loaded = server
            .handle(rpc(
                "session/load",
                json!({
                    "sessionId": "s1", "cwd": "/tmp",
                    "_meta": {"kkagent/host": {"replay": false}},
                }),
            ))
            .await;
        assert!(loaded.error.is_none(), "{:?}", loaded.error);
        assert_eq!(loaded.result.unwrap()["sessionId"], "s1");
        assert!(
            outgoing.try_recv().is_err(),
            "replay:false must not send history"
        );
        // The session is loaded all the same.
        assert_eq!(*host.loaded.lock().await, vec!["s1"]);

        // `replay: true` and the default still replay.
        for meta in [json!({"kkagent/host": {"replay": true}}), json!({})] {
            server
                .handle(rpc(
                    "session/load",
                    json!({"sessionId": "s1", "cwd": "/tmp", "_meta": meta}),
                ))
                .await;
            let replay: Value = serde_json::from_str(&outgoing.recv().await.unwrap()).unwrap();
            assert_eq!(
                replay["params"]["update"]["sessionUpdate"],
                "user_message_chunk"
            );
        }
    }

    #[tokio::test]
    async fn unknown_sessions_answer_with_resource_not_found() {
        let host = Arc::new(LifecycleHost::new(false));
        let server = AcpServer::with_host(Arc::clone(&host) as Arc<dyn AcpHost>);
        for (method, params) in [
            (
                "session/prompt",
                json!({"sessionId": "nope", "prompt": "x"}),
            ),
            (
                "session/set_mode",
                json!({"sessionId": "nope", "modeId": "plan"}),
            ),
            (
                "session/set_model",
                json!({"sessionId": "nope", "modelId": "m"}),
            ),
            ("session/close", json!({"sessionId": "nope"})),
        ] {
            let response = server.handle(rpc(method, params)).await;
            assert_eq!(
                response
                    .error
                    .unwrap_or_else(|| panic!("{method} must fail"))["code"],
                -32002,
                "{method}"
            );
        }
        // The host's own "not found" maps to the same code.
        let gone = server
            .handle(rpc(
                "session/resume",
                json!({"sessionId": "gone", "cwd": "/tmp"}),
            ))
            .await;
        assert_eq!(gone.error.unwrap()["code"], -32002);
        let missing_id = server
            .handle(rpc("session/resume", json!({"cwd": "/tmp"})))
            .await;
        assert_eq!(missing_id.error.unwrap()["code"], -32602);
    }

    #[tokio::test]
    async fn session_list_uses_the_host_store_and_filters_by_cwd() {
        let server = AcpServer::with_host(Arc::new(LifecycleHost::new(false)));
        let all = server.handle(rpc("session/list", json!({}))).await;
        assert_eq!(all.result.unwrap()["sessions"].as_array().unwrap().len(), 2);
        let one = server
            .handle(rpc("session/list", json!({"cwd": "/b"})))
            .await;
        let sessions = one.result.unwrap()["sessions"].clone();
        assert_eq!(sessions.as_array().unwrap().len(), 1);
        assert_eq!(sessions[0]["sessionId"], "s2");

        // Without a host store the connection's own sessions are listed.
        let dir = tempfile::tempdir().unwrap();
        let plain = AcpServer::new();
        let sid = new_session(&plain, dir.path().to_str().unwrap()).await;
        let listed = plain.handle(rpc("session/list", json!({}))).await;
        assert_eq!(listed.result.unwrap()["sessions"][0]["sessionId"], sid);
    }

    #[tokio::test]
    async fn close_and_delete_release_the_session() {
        let host = Arc::new(LifecycleHost::new(false));
        let server = AcpServer::with_host(Arc::clone(&host) as Arc<dyn AcpHost>);
        let dir = tempfile::tempdir().unwrap();
        let sid = new_session(&server, dir.path().to_str().unwrap()).await;

        let closed = server
            .handle(rpc("session/close", json!({"sessionId": sid})))
            .await;
        assert!(closed.error.is_none());
        assert_eq!(*host.closed.lock().await, vec![sid.clone()]);
        assert!(
            host.cancels.lock().await.contains(&sid),
            "close stops running work first"
        );
        let after = server
            .handle(rpc(
                "session/prompt",
                json!({"sessionId": sid, "prompt": "x"}),
            ))
            .await;
        assert_eq!(after.error.unwrap()["code"], -32002);

        let sid2 = new_session(&server, dir.path().to_str().unwrap()).await;
        let deleted = server
            .handle(rpc("session/delete", json!({"sessionId": sid2})))
            .await;
        assert!(deleted.error.is_none());
        assert_eq!(*host.deleted.lock().await, vec![sid2.clone()]);
        // A host without delete support says so.
        let plain = AcpServer::new();
        let refused = plain
            .handle(rpc("session/delete", json!({"sessionId": "x"})))
            .await;
        assert_eq!(refused.error.unwrap()["code"], -32000);
    }

    #[tokio::test]
    async fn cancelled_turn_returns_the_cancelled_stop_reason() {
        let host = Arc::new(LifecycleHost::new(true));
        let server = Arc::new(AcpServer::with_host(Arc::clone(&host) as Arc<dyn AcpHost>));
        let dir = tempfile::tempdir().unwrap();
        let sid = new_session(&server, dir.path().to_str().unwrap()).await;

        let turn = tokio::spawn({
            let server = Arc::clone(&server);
            let sid = sid.clone();
            async move {
                server
                    .handle(rpc(
                        "session/prompt",
                        json!({"sessionId": sid, "prompt": [{"type": "text", "text": "long"}]}),
                    ))
                    .await
            }
        });
        host.started.notified().await;
        let cancel = server
            .handle(rpc("session/cancel", json!({"sessionId": sid})))
            .await;
        assert!(cancel.error.is_none());
        assert_eq!(
            turn.await.unwrap().result.unwrap()["stopReason"],
            "cancelled"
        );
        assert_eq!(*host.cancels.lock().await, vec![sid.clone()]);

        // The next turn is not poisoned by the earlier cancel.
        *host.stop.lock().await = "end_turn".into();
        let host2 = Arc::clone(&host);
        let next = tokio::spawn({
            let server = Arc::clone(&server);
            let sid = sid.clone();
            async move {
                server
                    .handle(rpc(
                        "session/prompt",
                        json!({"sessionId": sid, "prompt": [{"type": "text", "text": "again"}]}),
                    ))
                    .await
            }
        });
        host2.started.notified().await;
        host2.release.notify_one();
        assert_eq!(
            next.await.unwrap().result.unwrap()["stopReason"],
            "end_turn"
        );
    }

    #[tokio::test]
    async fn cancel_notifications_and_cancel_request_are_handled() {
        let host = Arc::new(LifecycleHost::new(true));
        let server = Arc::new(AcpServer::with_host(Arc::clone(&host) as Arc<dyn AcpHost>));
        let (bridge, mut outgoing) = test_bridge();
        server.attach_client_bridge(bridge.clone()).await;
        let dir = tempfile::tempdir().unwrap();
        let sid = new_session(&server, dir.path().to_str().unwrap()).await;
        let inflight: Mutex<HashMap<String, Inflight>> = Mutex::new(HashMap::new());

        // `session/cancel` as a notification ends the turn with `cancelled`.
        let turn = tokio::spawn({
            let server = Arc::clone(&server);
            let sid = sid.clone();
            async move {
                server
                    .handle(rpc(
                        "session/prompt",
                        json!({"sessionId": sid, "prompt": "x"}),
                    ))
                    .await
            }
        });
        host.started.notified().await;
        let notification = AcpRequest {
            jsonrpc: "2.0".into(),
            id: None,
            method: "session/cancel".into(),
            params: json!({"sessionId": sid}),
        };
        server
            .handle_notification(&notification, &inflight, &bridge)
            .await;
        assert_eq!(
            turn.await.unwrap().result.unwrap()["stopReason"],
            "cancelled"
        );

        // `$/cancel_request` on a prompt cancels the turn cooperatively.
        let turn = tokio::spawn({
            let server = Arc::clone(&server);
            let sid = sid.clone();
            async move {
                server
                    .handle(rpc(
                        "session/prompt",
                        json!({"sessionId": sid, "prompt": "y"}),
                    ))
                    .await
            }
        });
        host.started.notified().await;
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        inflight.lock().await.insert(
            "7".into(),
            Inflight {
                id: json!(7),
                method: "session/prompt".into(),
                session_id: Some(sid.clone()),
                cancelled: Arc::clone(&cancelled),
            },
        );
        let cancel_request = AcpRequest {
            jsonrpc: "2.0".into(),
            id: None,
            method: "$/cancel_request".into(),
            params: json!({"requestId": 7}),
        };
        server
            .handle_notification(&cancel_request, &inflight, &bridge)
            .await;
        assert_eq!(
            turn.await.unwrap().result.unwrap()["stopReason"],
            "cancelled"
        );
        assert!(
            !cancelled.load(Ordering::SeqCst),
            "prompts still answer themselves"
        );
        assert!(outgoing.try_recv().is_err());

        // Any other request is answered with -32800 right away.
        inflight.lock().await.insert(
            "9".into(),
            Inflight {
                id: json!(9),
                method: "session/list".into(),
                session_id: None,
                cancelled: Arc::clone(&cancelled),
            },
        );
        let cancel_other = AcpRequest {
            params: json!({"requestId": 9}),
            ..cancel_request
        };
        server
            .handle_notification(&cancel_other, &inflight, &bridge)
            .await;
        let answer: Value = serde_json::from_str(&outgoing.recv().await.unwrap()).unwrap();
        assert_eq!(answer["id"], 9);
        assert_eq!(answer["error"]["code"], -32800);
        assert!(cancelled.load(Ordering::SeqCst));
        // Unknown ids and unrelated notifications are ignored.
        server
            .handle_notification(&cancel_other, &inflight, &bridge)
            .await;
        assert!(outgoing.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_cancelled_turn_releases_pending_permission_requests() {
        struct Host {
            approvals: Mutex<Vec<Value>>,
        }
        #[async_trait::async_trait]
        impl AcpHost for Host {
            async fn prompt(&self, _sid: &str, _text: &str) -> Result<Value, String> {
                Ok(json!({}))
            }
            async fn cancel(&self, _sid: &str) -> Result<(), String> {
                Ok(())
            }
            async fn respond_approval(&self, payload: &Value) -> Result<(), String> {
                self.approvals.lock().await.push(payload.clone());
                Ok(())
            }
        }
        let host = Arc::new(Host {
            approvals: Mutex::new(vec![]),
        });
        let server = Arc::new(AcpServer::with_host(Arc::clone(&host) as Arc<dyn AcpHost>));
        let (bridge, mut outgoing) = test_bridge();
        server.attach_client_bridge(bridge.clone()).await;
        let dir = tempfile::tempdir().unwrap();
        let sid = new_session(&server, dir.path().to_str().unwrap()).await;

        let task = tokio::spawn({
            let server = Arc::clone(&server);
            let bridge = bridge.clone();
            let event = json!({
                "type": "approval_requested",
                "session_id": sid,
                "approval_id": "ap_c",
                "tool_name": "Bash",
                "tool_input_display": {"command": "sleep 100"},
            });
            async move { server.request_permission_from_event(&bridge, &event).await }
        });
        let request: Value = serde_json::from_str(&outgoing.recv().await.unwrap()).unwrap();
        assert_eq!(request["method"], "session/request_permission");

        server.cancel_session(&sid).await;
        task.await.unwrap();
        // The client is told to drop the dialog, the host sees a refusal.
        let notice: Value = serde_json::from_str(&outgoing.recv().await.unwrap()).unwrap();
        assert_eq!(notice["method"], "$/cancel_request");
        let approvals = host.approvals.lock().await;
        assert_eq!(approvals[0]["decision"], "cancelled");
        assert!(approvals[0].get("approve").is_none());
    }

    #[tokio::test]
    async fn stop_reasons_are_limited_to_the_official_set() {
        for (raw, expected) in [
            ("max_turn_requests", "max_turn_requests"),
            ("max_tokens", "max_tokens"),
            ("refusal", "refusal"),
            ("weird", "end_turn"),
        ] {
            let host = Arc::new(LifecycleHost::new(false));
            *host.stop.lock().await = raw.into();
            let server = AcpServer::with_host(host);
            let dir = tempfile::tempdir().unwrap();
            let sid = new_session(&server, dir.path().to_str().unwrap()).await;
            let response = server
                .handle(rpc(
                    "session/prompt",
                    json!({"sessionId": sid, "prompt": "x"}),
                ))
                .await;
            assert_eq!(response.result.unwrap()["stopReason"], expected, "{raw}");
        }
    }

    #[tokio::test]
    async fn set_mode_notifies_the_client_and_accepts_the_official_param() {
        let server = AcpServer::with_host(Arc::new(LifecycleHost::new(false)));
        let (bridge, mut outgoing) = test_bridge();
        server.attach_client_bridge(bridge).await;
        let dir = tempfile::tempdir().unwrap();
        let sid = new_session(&server, dir.path().to_str().unwrap()).await;
        let response = server
            .handle(rpc(
                "session/set_mode",
                json!({"sessionId": sid, "modeId": "plan"}),
            ))
            .await;
        assert!(response.error.is_none());
        let update: Value = serde_json::from_str(&outgoing.recv().await.unwrap()).unwrap();
        assert_eq!(
            update["params"]["update"]["sessionUpdate"],
            "current_mode_update"
        );
        assert_eq!(update["params"]["update"]["currentModeId"], "plan");
    }

    #[tokio::test]
    async fn slash_commands_run_from_the_prompt_text() {
        let server = AcpServer::with_host(Arc::new(LifecycleHost::new(true)));
        let (bridge, mut outgoing) = test_bridge();
        server.attach_client_bridge(bridge).await;
        let dir = tempfile::tempdir().unwrap();
        let sid = new_session(&server, dir.path().to_str().unwrap()).await;
        // The blocking host would hang if this reached the model.
        let response = server
            .handle(rpc(
                "session/prompt",
                json!({"sessionId": sid, "prompt": [{"type": "text", "text": "/version"}]}),
            ))
            .await;
        assert_eq!(response.result.unwrap()["stopReason"], "end_turn");
        let update: Value = serde_json::from_str(&outgoing.recv().await.unwrap()).unwrap();
        assert_eq!(
            update["params"]["update"]["sessionUpdate"],
            "agent_message_chunk"
        );

        assert_eq!(
            parse_slash_command("/status now"),
            Some(("status".into(), "now".into()))
        );
        assert_eq!(parse_slash_command("/not-a-command"), None);
        assert_eq!(parse_slash_command("explain /status"), None);
    }

    #[tokio::test]
    async fn new_sessions_announce_their_commands_after_the_response() {
        let server = AcpServer::with_host(Arc::new(LifecycleHost::new(false)));
        let (bridge, mut outgoing) = test_bridge();
        server.attach_client_bridge(bridge).await;
        let dir = tempfile::tempdir().unwrap();
        let response = server
            .handle(rpc(
                "session/new",
                json!({"cwd": dir.path().to_str().unwrap()}),
            ))
            .await;
        server.after_response("session/new", &response).await;
        let update: Value = serde_json::from_str(&outgoing.recv().await.unwrap()).unwrap();
        assert_eq!(
            update["params"]["update"]["sessionUpdate"],
            "available_commands_update"
        );
        let commands = update["params"]["update"]["availableCommands"]
            .as_array()
            .unwrap();
        assert!(commands.iter().all(|c| c["description"].is_string()));
        // Other methods announce nothing.
        server.after_response("session/prompt", &response).await;
        assert!(outgoing.try_recv().is_err());
    }

    #[tokio::test]
    async fn session_info_is_announced_once_per_title() {
        let mut host = LifecycleHost::new(false);
        host.title = Some("Fix the build".into());
        let server = AcpServer::with_host(Arc::new(host));
        let (bridge, mut outgoing) = test_bridge();
        server.attach_client_bridge(bridge).await;
        let dir = tempfile::tempdir().unwrap();
        let sid = new_session(&server, dir.path().to_str().unwrap()).await;
        for _ in 0..2 {
            server
                .handle(rpc(
                    "session/prompt",
                    json!({"sessionId": sid, "prompt": "x"}),
                ))
                .await;
        }
        let update: Value = serde_json::from_str(&outgoing.recv().await.unwrap()).unwrap();
        assert_eq!(
            update["params"]["update"]["sessionUpdate"],
            "session_info_update"
        );
        assert_eq!(update["params"]["update"]["title"], "Fix the build");
        assert!(
            outgoing.try_recv().is_err(),
            "an unchanged title is not repeated"
        );
    }

    #[tokio::test]
    async fn initialize_keeps_the_client_capabilities_and_declares_agent_info() {
        let server = AcpServer::new();
        let response = server
            .handle(rpc(
                "initialize",
                json!({
                    "protocolVersion": 9,
                    "clientInfo": {"name": "zed"},
                    "clientCapabilities": {"fs": {"readTextFile": true}},
                }),
            ))
            .await;
        let result = response.result.unwrap();
        assert_eq!(
            result["protocolVersion"], 1,
            "unsupported versions get ours"
        );
        assert_eq!(result["agentInfo"]["name"], "kkagent");
        assert_eq!(
            result["agentCapabilities"]["sessionCapabilities"]["list"],
            json!({})
        );
        assert_eq!(
            result["agentCapabilities"]["sessionCapabilities"]["resume"],
            json!({})
        );
        assert_eq!(
            result["agentCapabilities"]["sessionCapabilities"]["close"],
            json!({})
        );
        let info = server.client_info().await;
        assert_eq!(info["clientInfo"]["name"], "zed");
        assert_eq!(info["clientCapabilities"]["fs"]["readTextFile"], true);
    }

    #[tokio::test]
    async fn forward_event_covers_tool_plan_mode_and_usage_updates() {
        struct WindowHost;
        #[async_trait::async_trait]
        impl AcpHost for WindowHost {
            async fn prompt(&self, _sid: &str, _text: &str) -> Result<Value, String> {
                Ok(json!({}))
            }
            async fn cancel(&self, _sid: &str) -> Result<(), String> {
                Ok(())
            }
            async fn context_window(&self, _sid: &str) -> Option<u64> {
                Some(200_000)
            }
        }
        let server = AcpServer::with_host(Arc::new(WindowHost));
        let (bridge, mut outgoing) = test_bridge();
        server.attach_client_bridge(bridge.clone()).await;
        let dir = tempfile::tempdir().unwrap();
        let sid = new_session(&server, dir.path().to_str().unwrap()).await;
        let next = |event: Value| {
            let server = &server;
            let bridge = bridge.clone();
            async move { (server.forward_event(&bridge, &event).await, event) }
        };

        let (sent, _) = next(json!({
            "type": "tool_call", "session_id": sid, "tool_call_id": "c1",
            "tool_name": "bash", "input": {"command": "ls"},
        }))
        .await;
        assert!(sent);
        let call: Value = serde_json::from_str(&outgoing.recv().await.unwrap()).unwrap();
        let update = &call["params"]["update"];
        assert_eq!(update["sessionUpdate"], "tool_call");
        assert_eq!(update["toolCallId"], "c1");
        assert_eq!(update["title"], "bash");
        assert_eq!(update["status"], "pending");
        assert_eq!(update["rawInput"]["command"], "ls");

        // "queued" is not news, "running" is.
        let (queued, _) = next(json!({
            "type": "tool_execution_status", "session_id": sid,
            "tool_call_id": "c1", "status": "queued",
        }))
        .await;
        assert!(!queued);
        next(json!({
            "type": "tool_execution_status", "session_id": sid,
            "tool_call_id": "c1", "status": "running",
        }))
        .await;
        let running: Value = serde_json::from_str(&outgoing.recv().await.unwrap()).unwrap();
        assert_eq!(
            running["params"]["update"]["sessionUpdate"],
            "tool_call_update"
        );
        assert_eq!(running["params"]["update"]["status"], "in_progress");

        next(json!({
            "type": "tool_result", "session_id": sid, "tool_call_id": "c1",
            "tool_name": "bash", "output": "boom", "is_error": true,
        }))
        .await;
        let result: Value = serde_json::from_str(&outgoing.recv().await.unwrap()).unwrap();
        assert_eq!(result["params"]["update"]["status"], "failed");
        assert_eq!(
            result["params"]["update"]["content"][0]["content"]["text"],
            "boom"
        );
        assert_eq!(result["params"]["update"]["rawOutput"], "boom");

        // An empty result has the same shape: both fields, never one or the other.
        next(json!({
            "type": "tool_result", "session_id": sid, "tool_call_id": "c2",
            "tool_name": "bash", "output": "", "is_error": false,
        }))
        .await;
        let empty: Value = serde_json::from_str(&outgoing.recv().await.unwrap()).unwrap();
        assert_eq!(empty["params"]["update"]["status"], "completed");
        assert_eq!(empty["params"]["update"]["rawOutput"], "");
        assert_eq!(
            empty["params"]["update"]["content"][0]["content"]["text"],
            ""
        );

        next(json!({"type": "plan_mode_changed", "session_id": sid, "enabled": true})).await;
        let mode: Value = serde_json::from_str(&outgoing.recv().await.unwrap()).unwrap();
        assert_eq!(
            mode["params"]["update"]["sessionUpdate"],
            "current_mode_update"
        );
        assert_eq!(mode["params"]["update"]["currentModeId"], "plan");

        next(json!({
            "type": "todo_updated", "session_id": sid,
            "items": [{"id": "1", "content": "write", "status": "in_progress"}],
        }))
        .await;
        let plan: Value = serde_json::from_str(&outgoing.recv().await.unwrap()).unwrap();
        assert_eq!(plan["params"]["update"]["sessionUpdate"], "plan");
        assert_eq!(
            plan["params"]["update"]["entries"][0]["status"],
            "in_progress"
        );

        next(json!({
            "type": "usage_update", "session_id": sid,
            "context": {"system": 10, "conversation": 20, "tools": 5, "media": 0},
        }))
        .await;
        let usage: Value = serde_json::from_str(&outgoing.recv().await.unwrap()).unwrap();
        assert_eq!(usage["params"]["update"]["sessionUpdate"], "usage_update");
        assert_eq!(usage["params"]["update"]["used"], 35);
        assert_eq!(usage["params"]["update"]["size"], 200_000);

        let (ignored, _) = next(json!({"type": "heartbeat", "session_id": sid})).await;
        assert!(!ignored);
    }

    /// Per-call token accounting, the model that ran, and the window size all
    /// reach the host in `usage_update._meta`; `used`/`size` keep the standard
    /// shape. The diff content block and the `kkagent_*` updates are additive,
    /// so a standard v1 client is unaffected.
    #[tokio::test]
    async fn usage_meta_carries_tokens_and_model_and_changes_render_as_diffs() {
        struct NoWindowHost;
        #[async_trait::async_trait]
        impl AcpHost for NoWindowHost {
            async fn prompt(&self, _sid: &str, _text: &str) -> Result<Value, String> {
                Ok(json!({}))
            }
            async fn cancel(&self, _sid: &str) -> Result<(), String> {
                Ok(())
            }
        }
        let server = AcpServer::with_host(Arc::new(NoWindowHost));
        let (bridge, mut outgoing) = test_bridge();
        server.attach_client_bridge(bridge.clone()).await;
        let dir = tempfile::tempdir().unwrap();
        let sid = new_session(&server, dir.path().to_str().unwrap()).await;

        // A host that knows no window size still gets the numbers, and the
        // event's own window (a fallback model) fills `size`.
        server
            .forward_event(
                &bridge,
                &json!({
                    "type": "usage_update", "session_id": sid,
                    "usage": {"input_tokens": 1200, "output_tokens": 30,
                              "cache_read_input_tokens": 800, "cache_creation_input_tokens": 0,
                              "input_includes_cache": true},
                    "model": "gpt-4.1", "context_window": 1047576,
                    "context": {"system": 10, "conversation": 20, "tools": 5, "media": 0},
                    "steps": 2, "turns": 1,
                }),
            )
            .await;
        let usage: Value = serde_json::from_str(&outgoing.recv().await.unwrap()).unwrap();
        let update = &usage["params"]["update"];
        assert_eq!(update["sessionUpdate"], "usage_update");
        assert_eq!(
            (update["used"].as_u64(), update["size"].as_u64()),
            (Some(35), Some(1047576))
        );
        let meta = &update["_meta"];
        assert_eq!(meta["usage"]["input_tokens"], 1200);
        assert_eq!(meta["usage"]["cache_read_input_tokens"], 800);
        assert_eq!(meta["usage"]["input_includes_cache"], true);
        assert_eq!(meta["model"], "gpt-4.1");
        assert_eq!(meta["contextWindow"], 1047576);
        assert_eq!(meta["steps"], 2);
        assert_eq!(meta["turns"], 1);

        // A token-only event (no context breakdown) is still worth sending.
        server
            .forward_event(
                &bridge,
                &json!({
                    "type": "usage_update", "session_id": sid,
                    "usage": {"input_tokens": 10, "output_tokens": 1},
                }),
            )
            .await;
        let usage: Value = serde_json::from_str(&outgoing.recv().await.unwrap()).unwrap();
        assert_eq!(usage["params"]["update"]["used"], 0);
        assert_eq!(
            usage["params"]["update"]["_meta"]["usage"]["input_tokens"],
            10
        );

        // An edit's structured change becomes a `diff` block after the text.
        server
            .forward_event(
                &bridge,
                &json!({
                    "type": "tool_result", "session_id": sid, "tool_call_id": "c9",
                    "tool_name": "Edit", "output": "Replaced 1 occurrence(s) in a.txt", "is_error": false,
                    "changes": [{"path": "a.txt", "old_text": "old", "new_text": "new"}],
                }),
            )
            .await;
        let result: Value = serde_json::from_str(&outgoing.recv().await.unwrap()).unwrap();
        let content = &result["params"]["update"]["content"];
        assert_eq!(content[0]["type"], "content");
        assert_eq!(
            content[0]["content"]["text"],
            "Replaced 1 occurrence(s) in a.txt"
        );
        assert_eq!(content[1]["type"], "diff");
        assert_eq!(content[1]["path"], "a.txt");
        assert_eq!(content[1]["oldText"], "old");
        assert_eq!(content[1]["newText"], "new");
        assert_eq!(
            result["params"]["update"]["rawOutput"], "Replaced 1 occurrence(s) in a.txt",
            "a host reading only the text still sees the same result"
        );

        // The updates that official v1 has no variant for are prefixed so a
        // standard client ignores them instead of guessing.
        let sent = server
            .forward_event(
                &bridge,
                &json!({"type": "notice", "session_id": sid, "message": "cross-session conflict"}),
            )
            .await;
        assert!(sent);
        let notice: Value = serde_json::from_str(&outgoing.recv().await.unwrap()).unwrap();
        assert_eq!(
            notice["params"]["update"]["sessionUpdate"],
            "kkagent_notice"
        );
        assert_eq!(
            notice["params"]["update"]["message"],
            "cross-session conflict"
        );

        // One update per retry, not one per countdown tick.
        let tick = server
            .forward_event(
                &bridge,
                &json!({"type": "llm_retry", "session_id": sid, "retry_number": 1,
                        "reason": "429", "wait_seconds": 8, "remaining_seconds": 5, "initial": false}),
            )
            .await;
        assert!(!tick);
        server
            .forward_event(
                &bridge,
                &json!({"type": "llm_retry", "session_id": sid, "retry_number": 1,
                        "reason": "429", "wait_seconds": 8, "remaining_seconds": 8, "initial": true}),
            )
            .await;
        let retry: Value = serde_json::from_str(&outgoing.recv().await.unwrap()).unwrap();
        assert_eq!(retry["params"]["update"]["sessionUpdate"], "kkagent_retry");
        assert_eq!(retry["params"]["update"]["retryNumber"], 1);
        assert_eq!(retry["params"]["update"]["waitSeconds"], 8);

        server
            .forward_event(
                &bridge,
                &json!({"type": "auto_compact_completed", "session_id": sid,
                        "archive_session_id": "arch", "compacted_count": 42}),
            )
            .await;
        let compact: Value = serde_json::from_str(&outgoing.recv().await.unwrap()).unwrap();
        assert_eq!(
            compact["params"]["update"]["sessionUpdate"],
            "kkagent_compaction"
        );
        assert_eq!(compact["params"]["update"]["kind"], "auto");
        assert_eq!(compact["params"]["update"]["removed"], 42);
        assert_eq!(compact["params"]["update"]["archiveSessionId"], "arch");

        server
            .forward_event(
                &bridge,
                &json!({"type": "skill_activated", "session_id": sid,
                        "activation_id": "a1", "skill_name": "pdf", "trigger": "model-tool"}),
            )
            .await;
        let skill: Value = serde_json::from_str(&outgoing.recv().await.unwrap()).unwrap();
        assert_eq!(
            skill["params"]["update"]["sessionUpdate"],
            "kkagent_skill_activated"
        );
        assert_eq!(skill["params"]["update"]["skillName"], "pdf");
        assert_eq!(skill["params"]["update"]["trigger"], "model-tool");
    }

    #[tokio::test]
    async fn the_prompt_response_waits_for_the_streamed_updates() {
        let server = Arc::new(AcpServer::with_host(Arc::new(LifecycleHost::new(false))));
        let dir = tempfile::tempdir().unwrap();
        let sid = new_session(&server, dir.path().to_str().unwrap()).await;

        // Without a forwarder there is nothing to wait for.
        server.store.wait_turn_delivered(&sid, 0).await;

        server.store.forwarding.store(true, Ordering::SeqCst);
        let waiter = tokio::spawn({
            let server = Arc::clone(&server);
            let sid = sid.clone();
            async move {
                server.store.wait_turn_delivered(&sid, 0).await;
                tokio::time::Instant::now()
            }
        });
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(
            !waiter.is_finished(),
            "must wait for the turn to be delivered"
        );
        let committed_at = tokio::time::Instant::now();
        *server
            .store
            .delivered_turns
            .lock()
            .await
            .entry(sid.clone())
            .or_default() += 1;
        let released_at = waiter.await.unwrap();
        assert!(released_at >= committed_at);
    }
}
