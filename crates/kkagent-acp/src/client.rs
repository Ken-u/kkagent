//! ACP **client** — drives an external agent (e.g. Cursor CLI `agent acp`)
//! over stdio newline-delimited JSON-RPC 2.0.
//!
//! This is the inverse of the ACP server in `lib.rs` (where kkagent *is* the
//! agent behind an editor). Here kkagent acts as the ACP *client* and spawns
//! the external agent as a child process.
//!
//! Handshake: `initialize` → (`authenticate`, skipped when the child inherits
//! credentials from the environment) → `session/new` → `session/prompt`.
//! Streaming progress arrives as `session/update` notifications; the prompt
//! request resolves with a `stopReason`. Reverse requests
//! (`session/request_permission`) are answered from a per-connection
//! [`PermissionPolicy`] so external subagents never deadlock on approval.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
// Disambiguate against tokio's AsyncWriteExt::write / RwLock::write.
use tokio::process::{Child, Command};
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;

/// Default timeout for a single ACP request (handshake, permission prompts).
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

/// Behavior for `session/request_permission` reverse requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionPolicy {
    /// Auto-approve every tool invocation (default for external subagents).
    AutoApprove,
    /// Deny every tool invocation; the external agent must degrade gracefully.
    Deny,
}

impl PermissionPolicy {
    /// Build the response payload. ACP permission options carry string
    /// `optionId`s; agents order them most-permissive first, so auto-approve
    /// selects the first option.
    fn response(self, params: &Value) -> Value {
        match self {
            PermissionPolicy::AutoApprove => {
                let first_option = params
                    .get("options")
                    .and_then(Value::as_array)
                    .and_then(|opts| opts.first())
                    .and_then(|opt| opt.get("optionId"))
                    .cloned()
                    .unwrap_or_else(|| json!(0));
                json!({ "outcome": { "outcome": "selected", "optionId": first_option } })
            }
            PermissionPolicy::Deny => {
                json!({ "outcome": { "outcome": "cancelled" } })
            }
        }
    }
}

/// Streamed progress from a prompt turn, translated from `session/update`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExternalProgress {
    /// Incremental agent text output.
    Text { delta: String },
    /// A tool call started (or progressed) inside the external agent.
    ToolCall {
        tool_call_id: String,
        title: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_kind: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
    },
    /// A plan / todo list update (Cursor's `plan` update variant).
    Plan {
        #[serde(default)]
        entries: Vec<Value>,
    },
    /// Unrecognized or vendor-specific update, kept for observability.
    Unknown { update: Value },
}

/// Final result of a completed prompt turn.
#[derive(Debug, Clone)]
pub struct PromptOutcome {
    /// `stopReason` from the `session/prompt` response.
    pub stop_reason: String,
    /// Final agent text (accumulated from streamed chunks).
    pub text: String,
}

/// Options for spawning an external ACP agent.
#[derive(Debug, Clone)]
pub struct AcpClientOptions {
    /// Executable and arguments, e.g. `["agent", "acp"]` (Cursor CLI) or
    /// `["cursor-agent", "acp"]`.
    pub command: Vec<String>,
    /// Working directory for the child process.
    pub cwd: PathBuf,
    /// Extra environment variables for the child (e.g. `CURSOR_API_KEY`).
    pub env: HashMap<String, String>,
    /// Per-request timeout. Defaults to [`DEFAULT_REQUEST_TIMEOUT`].
    pub request_timeout: Option<Duration>,
    /// Reverse-request permission policy. Defaults to [`PermissionPolicy::AutoApprove`].
    pub permission: Option<PermissionPolicy>,
}

impl AcpClientOptions {
    /// Build options for the Cursor CLI ACP mode.
    pub fn cursor_cli(cwd: PathBuf) -> Self {
        Self {
            command: vec!["agent".into(), "acp".into()],
            cwd,
            env: HashMap::new(),
            request_timeout: None,
            permission: None,
        }
    }
}

/// `initialize` result payload (subset we rely on).
#[derive(Debug, Clone, Deserialize)]
pub struct InitializeResult {
    #[serde(default)]
    pub protocol_version: Option<Value>,
    #[serde(default, rename = "agentCapabilities")]
    pub agent_capabilities: Option<Value>,
    #[serde(default, rename = "authMethods")]
    pub auth_methods: Vec<Value>,
}

impl InitializeResult {
    /// True when the agent advertises at least one auth method. Callers that
    /// injected credentials via the environment may still skip `authenticate`
    /// (the CLI accepts pre-seeded `CURSOR_API_KEY`/`CURSOR_AUTH_TOKEN`).
    pub fn requires_auth(&self) -> bool {
        !self.auth_methods.is_empty()
    }
}

/// Progress sink for the in-flight prompt: (session id, sender).
type ProgressSink = Option<(String, mpsc::Sender<(String, ExternalProgress)>)>;

/// Shared connection state mutated by the reader loop.
struct Conn {
    stdin: Mutex<Option<tokio::process::ChildStdin>>,
    pending: Mutex<HashMap<i64, tokio::sync::oneshot::Sender<Result<Value, Value>>>>,
    /// Progress sink for the in-flight prompt: (session id, sender).
    progress_sink: AsyncMutex<ProgressSink>,
    /// Accumulated agent text for the active prompt turn.
    text_acc: AsyncMutex<String>,
    permission: PermissionPolicy,
    next_id: AtomicI64,
}

impl Conn {
    async fn write_frame(&self, frame: &Value) -> anyhow::Result<()> {
        let mut line = serde_json::to_string(frame)?;
        line.push('\n');
        let mut guard = self.stdin.lock().await;
        let stdin = guard
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("external agent stdin closed"))?;
        stdin.write_all(line.as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }
}

/// A running connection to one external ACP agent process.
pub struct AcpClient {
    conn: Arc<Conn>,
    request_timeout: Duration,
    child: Mutex<Option<Child>>,
    /// Session id created during [`AcpClient::start_session`].
    session_id: AsyncMutex<Option<String>>,
    reader_task: Mutex<Option<JoinHandle<()>>>,
}

impl AcpClient {
    /// Spawn the external agent process and perform the `initialize`
    /// handshake. Does **not** authenticate or create a session yet.
    pub async fn spawn(opts: AcpClientOptions) -> anyhow::Result<Arc<Self>> {
        let (command, args) = match opts.command.split_first() {
            Some((cmd, args)) => (cmd.clone(), args.to_vec()),
            None => anyhow::bail!("external subagent command is empty"),
        };
        let mut cmd = Command::new(&command);
        cmd.args(&args)
            .current_dir(&opts.cwd)
            .envs(&opts.env)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        {
            cmd.process_group(0);
        }

        let mut child = cmd.spawn().map_err(|e| {
            anyhow::anyhow!(
                "failed to spawn external subagent `{command}`: {e}; \
                 is it on PATH and authenticated (`{command} login`)?"
            )
        })?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("external agent stdout unavailable"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("external agent stdin unavailable"))?;
        let stderr = child.stderr.take();

        if let Some(stderr) = stderr {
            tokio::spawn(async move {
                let mut stderr = stderr;
                let mut buf = Vec::new();
                if stderr.read_to_end(&mut buf).await.is_ok() && !buf.is_empty() {
                    let text = String::from_utf8_lossy(&buf);
                    tracing::debug!(target: "acp_client", "external agent stderr: {text}");
                }
            });
        }

        let conn = Arc::new(Conn {
            stdin: Mutex::new(Some(stdin)),
            pending: Mutex::new(HashMap::new()),
            progress_sink: Mutex::new(None),
            text_acc: Mutex::new(String::new()),
            permission: opts.permission.unwrap_or(PermissionPolicy::AutoApprove),
            next_id: AtomicI64::new(1),
        });
        let client = Arc::new(Self {
            conn: conn.clone(),
            request_timeout: opts.request_timeout.unwrap_or(DEFAULT_REQUEST_TIMEOUT),
            child: Mutex::new(Some(child)),
            session_id: Mutex::new(None),
            reader_task: Mutex::new(None),
        });

        // Reader loop: parse newline-delimited JSON-RPC frames and route them.
        let task = tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) if !line.trim().is_empty() => {
                        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                            tracing::warn!(target: "acp_client", "non-JSON frame from external agent: {line}");
                            continue;
                        };
                        route_message(&conn, &msg).await;
                    }
                    _ => break,
                }
            }
        });
        *client.reader_task.lock().await = Some(task);

        // Handshake. Clients MUST declare the fs/terminal capabilities they
        // implement; we implement none — external agents fall back to their
        // own tooling.
        client
            .request(
                "initialize",
                json!({
                    "protocolVersion": 1,
                    "clientCapabilities": {
                        "fs": { "readTextFile": false, "writeTextFile": false },
                        "terminal": false,
                    }
                }),
            )
            .await?;
        Ok(client)
    }

    /// Perform `authenticate` using the agent's first advertised method.
    /// Only needed when the child process has no ambient credentials; callers
    /// may skip it when `env` injected `CURSOR_API_KEY`/`CURSOR_AUTH_TOKEN`.
    pub async fn authenticate(&self) -> anyhow::Result<()> {
        self.request("authenticate", json!({})).await?;
        Ok(())
    }

    /// Create a session bound to a working directory (and optional mode).
    pub async fn start_session(&self, cwd: &Path, mode: Option<&str>) -> anyhow::Result<String> {
        let mut params = json!({ "cwd": cwd.display().to_string(), "mcpServers": [] });
        if let Some(mode) = mode {
            params["mode"] = json!(mode);
        }
        let result = self.request("session/new", params).await?;
        let session_id = result
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("session/new response missing sessionId"))?
            .to_string();
        *self.session_id.lock().await = Some(session_id.clone());
        Ok(session_id)
    }

    /// Run one prompt turn to completion, forwarding streamed progress to
    /// `progress_tx`. Resolves when the agent replies with a `stopReason`.
    pub async fn prompt(
        &self,
        session_id: &str,
        text: &str,
        progress_tx: mpsc::Sender<(String, ExternalProgress)>,
    ) -> anyhow::Result<PromptOutcome> {
        *self.conn.text_acc.lock().await = String::new();
        let result = self
            .request_with_progress(
                "session/prompt",
                json!({
                    "sessionId": session_id,
                    "prompt": [ { "type": "text", "text": text } ]
                }),
                Some(session_id.to_string()),
                Some(progress_tx),
            )
            .await
            .map_err(|error| anyhow::anyhow!("session/prompt failed: {error}"))?;
        let stop_reason = result
            .get("stopReason")
            .and_then(Value::as_str)
            .unwrap_or("end_turn")
            .to_string();
        let text = std::mem::take(&mut *self.conn.text_acc.lock().await);
        Ok(PromptOutcome { stop_reason, text })
    }

    /// Cancel the current prompt turn on a session (fire-and-forget).
    pub async fn cancel(&self, session_id: &str) {
        let _ = self
            .conn
            .write_frame(&json!({
                "jsonrpc": "2.0",
                "method": "session/cancel",
                "params": { "sessionId": session_id }
            }))
            .await;
    }

    /// Terminate the external agent process and abort the reader loop.
    pub async fn shutdown(&self) {
        if let Some(mut child) = self.child.lock().await.take() {
            let _ = child.kill().await;
        }
        if let Some(task) = self.reader_task.lock().await.take() {
            task.abort();
        }
    }

    /// Send a request and await its response with the configured timeout.
    async fn request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        self.request_with_progress(method, params, None, None)
            .await
            .map_err(|e| anyhow::anyhow!("{method} failed: {e}"))
    }

    async fn request_with_progress(
        &self,
        method: &str,
        params: Value,
        progress_session: Option<String>,
        progress_tx: Option<mpsc::Sender<(String, ExternalProgress)>>,
    ) -> Result<Value, String> {
        let id = self.conn.next_id.fetch_add(1, Ordering::Relaxed);
        let frame = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.conn.pending.lock().await.insert(id, tx);
        if let Err(e) = self.conn.write_frame(&frame).await {
            self.conn.pending.lock().await.remove(&id);
            return Err(e.to_string());
        }

        // Install the progress sink for this request's duration; the reader
        // loop consults it when translating session/update notifications.
        // The sink is a client-level slot — one prompt at a time per
        // connection, matching ACP's one-turn-per-session model.
        {
            let mut sink = self.conn.progress_sink.lock().await;
            *sink = progress_session.zip(progress_tx);
        }
        let result = tokio::time::timeout(self.request_timeout, rx).await;
        self.conn.progress_sink.lock().await.take();

        match result {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(error))) => Err(error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown JSON-RPC error")
                .to_string()),
            Ok(Err(_)) => Err("connection dropped while awaiting the response".into()),
            Err(_) => Err(format!(
                "timed out after {}s",
                self.request_timeout.as_secs()
            )),
        }
    }
}

/// Route one parsed JSON-RPC message: responses to pending requests,
/// reverse requests to the permission policy, notifications to the
/// progress sink.
async fn route_message(conn: &Arc<Conn>, msg: &Value) {
    // A response carries "id" and no "method".
    let id = msg.get("id").and_then(Value::as_i64);
    let method = msg.get("method").and_then(Value::as_str);

    match (id, method) {
        (Some(id), None) => {
            if let Some(tx) = conn.pending.lock().await.remove(&id) {
                let result = match (msg.get("result"), msg.get("error")) {
                    (Some(result), _) => Ok(result.clone()),
                    (_, Some(error)) => Err(error.clone()),
                    _ => Err(json!({ "message": "malformed response" })),
                };
                let _ = tx.send(result);
            }
        }
        (Some(id), Some("session/request_permission")) => {
            let response = conn
                .permission
                .response(msg.get("params").unwrap_or(&Value::Null));
            tracing::debug!(target: "acp_client", "answered external agent permission request");
            let frame = json!({ "jsonrpc": "2.0", "id": id, "result": response });
            if let Err(e) = conn.write_frame(&frame).await {
                tracing::warn!(target: "acp_client", "failed to answer permission request: {e}");
            }
        }
        (Some(_), Some(other)) => {
            // Unknown reverse request: answer with a JSON-RPC error so the
            // agent does not hang awaiting a response.
            let _ = conn
                .write_frame(&json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": format!("method not supported: {other}") }
                }))
                .await;
        }
        (None, Some("session/update")) => {
            let params = msg.get("params").cloned().unwrap_or(Value::Null);
            if let Some(progress) = translate_update(&params) {
                let session = params
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if let ExternalProgress::Text { delta } = &progress {
                    conn.text_acc.lock().await.push_str(delta);
                }
                let sink = conn.progress_sink.lock().await.clone();
                if let Some((sink_session, tx)) = sink {
                    if sink_session.is_empty() || sink_session == session {
                        let _ = tx.send((session, progress)).await;
                    }
                }
            }
        }
        (None, Some(other)) => {
            tracing::trace!(target: "acp_client", "ignoring notification: {other}");
        }
        (None, None) => {}
    }
}

/// Translate a `session/update` notification into [`ExternalProgress`].
fn translate_update(params: &Value) -> Option<ExternalProgress> {
    let update = params.get("update")?;
    match update.get("sessionUpdate").and_then(Value::as_str) {
        Some("agent_message_chunk") => {
            let delta = update
                .pointer("/content/text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            Some(ExternalProgress::Text { delta })
        }
        Some("tool_call") | Some("tool_call_update") => {
            let title = update
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            Some(ExternalProgress::ToolCall {
                tool_call_id: update
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                title,
                tool_kind: update
                    .get("kind")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                status: update
                    .get("status")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            })
        }
        Some("plan") => Some(ExternalProgress::Plan {
            entries: update
                .get("entries")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
        }),
        _ => Some(ExternalProgress::Unknown {
            update: update.clone(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_auto_approve_selects_first_option() {
        let policy = PermissionPolicy::AutoApprove;
        let params = serde_json::json!({
            "sessionId": "s",
            "options": [
                { "optionId": "allow-once", "name": "Allow", "kind": "allow_once" },
                { "optionId": "reject-once", "name": "Reject", "kind": "reject_once" }
            ]
        });
        let response = policy.response(&params);
        assert_eq!(response["outcome"]["optionId"], "allow-once");
        assert_eq!(response["outcome"]["outcome"], "selected");
    }

    #[test]
    fn permission_deny_cancels() {
        let policy = PermissionPolicy::Deny;
        let response = policy.response(&serde_json::json!({ "options": [] }));
        assert_eq!(response["outcome"]["outcome"], "cancelled");
    }

    #[test]
    fn translate_text_chunk() {
        let params = serde_json::json!({
            "sessionId": "s1",
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": "hello " }
            }
        });
        let progress = translate_update(&params).expect("translated");
        match progress {
            ExternalProgress::Text { delta } => assert_eq!(delta, "hello "),
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn translate_tool_call() {
        let params = serde_json::json!({
            "sessionId": "s1",
            "update": {
                "sessionUpdate": "tool_call",
                "toolCallId": "tc1",
                "title": "Read main.rs",
                "kind": "read",
                "status": "pending"
            }
        });
        match translate_update(&params).expect("translated") {
            ExternalProgress::ToolCall {
                tool_call_id,
                title,
                tool_kind,
                status,
            } => {
                assert_eq!(tool_call_id, "tc1");
                assert_eq!(title, "Read main.rs");
                assert_eq!(tool_kind.as_deref(), Some("read"));
                assert_eq!(status.as_deref(), Some("pending"));
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn translate_unknown_update_is_preserved() {
        let params = serde_json::json!({
            "sessionId": "s1",
            "update": { "sessionUpdate": "something_new", "payload": {"x": 1} }
        });
        match translate_update(&params).expect("translated") {
            ExternalProgress::Unknown { update } => {
                assert_eq!(update["payload"]["x"], 1);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn initialize_result_auth_detection() {
        let empty: InitializeResult =
            serde_json::from_value(serde_json::json!({ "authMethods": [] })).unwrap();
        assert!(!empty.requires_auth());
        let with_auth: InitializeResult = serde_json::from_value(serde_json::json!({
            "authMethods": [ { "id": "cursor_login", "name": "Cursor Login" } ]
        }))
        .unwrap();
        assert!(with_auth.requires_auth());
        assert_eq!(with_auth.auth_methods[0]["id"], "cursor_login");
    }
}
