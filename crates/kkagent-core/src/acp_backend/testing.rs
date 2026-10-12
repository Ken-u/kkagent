//! In-process stub ACP agent for tests of the hub and the turn runner.
//!
//! The stub speaks the agent side of ACP over `tokio::io::duplex` pipes, so a
//! real [`AcpClient`] (reader loop, reverse-request handling, timeouts) is
//! exercised without spawning a process. Each `session/prompt` pops the next
//! scripted [`StubStep`] list; unscripted prompts just answer `ok`.

use super::*;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicI64, AtomicUsize};
use tokio::io::{duplex, AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

pub(crate) const STUB_PLUGIN: &str = "stub-plugin";
pub(crate) const STUB_AGENT_ID: &str = "stub-plugin.stub";
pub(crate) const STUB_ALIAS: &str = "acp:stub-plugin.stub";

#[derive(Clone, Debug)]
pub(crate) enum StubStep {
    /// A `session/update` notification with this `update` body.
    Update(Value),
    /// A reverse request. The agent waits for the client's answer, which is
    /// recorded in [`StubAgent::replies`].
    Request { method: &'static str, params: Value },
    /// Idle for this many milliseconds (cut short by `session/cancel`).
    Sleep(u64),
    /// Block until `session/cancel` arrives, then finish as cancelled.
    WaitCancel,
    /// Stall for this many milliseconds *ignoring* `session/cancel` (an
    /// unresponsive agent).
    Hang(u64),
    /// The agent process dies: its stdout closes without a response.
    Crash,
    /// Answer the prompt with a JSON-RPC error.
    Fail(&'static str),
}

pub(crate) fn text_chunk(text: &str) -> StubStep {
    StubStep::Update(json!({
        "sessionUpdate": "agent_message_chunk",
        "content": { "type": "text", "text": text }
    }))
}

pub(crate) fn thought_chunk(text: &str) -> StubStep {
    StubStep::Update(json!({
        "sessionUpdate": "agent_thought_chunk",
        "content": { "type": "text", "text": text }
    }))
}

pub(crate) fn tool_started(id: &str, kind: &str, title: &str, raw_input: Value) -> StubStep {
    StubStep::Update(json!({
        "sessionUpdate": "tool_call",
        "toolCallId": id, "title": title, "kind": kind,
        "status": "in_progress", "rawInput": raw_input
    }))
}

pub(crate) fn tool_finished(id: &str, status: &str, raw_output: Value) -> StubStep {
    StubStep::Update(json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": id, "status": status, "rawOutput": raw_output
    }))
}

/// The thinking-level option a stub offers (like Cursor's `reasoning_effort`).
struct StubThought {
    levels: Vec<String>,
    current: String,
}

impl StubThought {
    /// `configOptions` of the stub: mode, model and the thinking level.
    fn config_options(&self) -> Value {
        json!([
            {
                "id": "mode", "name": "Mode", "category": "mode", "type": "select",
                "currentValue": "agent",
                "options": [ { "value": "agent", "name": "Agent" }, { "value": "plan", "name": "Plan" } ]
            },
            {
                "id": "model", "name": "Model", "category": "model", "type": "select",
                "currentValue": "stub-model",
                "options": [ { "value": "stub-model", "name": "Stub Model" } ]
            },
            {
                "id": "reasoning_effort", "name": "Reasoning effort",
                "category": "thought_level", "type": "select",
                "currentValue": self.current,
                "options": self.levels.iter()
                    .map(|level| json!({ "value": level, "name": level }))
                    .collect::<Vec<_>>()
            }
        ])
    }
}

#[derive(Default)]
pub(crate) struct StubAgent {
    /// Clients the connector has created (process spawns).
    pub spawns: AtomicUsize,
    /// `session/new` requests answered.
    pub sessions: AtomicUsize,
    /// `(acp session id, prompt text)` of every `session/prompt`.
    pub prompts: StdMutex<Vec<(String, String)>>,
    /// `(acp session id, mode id)` of every `session/set_mode`.
    pub modes: StdMutex<Vec<(String, String)>>,
    /// `(acp session id, config id, value)` of every
    /// `session/set_config_option`.
    pub config_calls: StdMutex<Vec<(String, String, String)>>,
    /// Frames the client sent back for the stub's reverse requests.
    pub replies: StdMutex<Vec<Value>>,
    /// `session/cancel` notifications received.
    pub cancels: AtomicUsize,
    scripts: StdMutex<VecDeque<Vec<StubStep>>>,
    /// Offered thinking levels; `None`: the agent has no config options.
    thought: StdMutex<Option<StubThought>>,
    /// Answer `session/set_config_option` with an error.
    reject_config: AtomicBool,
}

type Out = Arc<Mutex<DuplexStream>>;
type Pending = Arc<StdMutex<HashMap<i64, oneshot::Sender<Value>>>>;

impl StubAgent {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Queue the behavior of the next `session/prompt`.
    pub(crate) fn script(&self, steps: Vec<StubStep>) {
        lock(&self.scripts).push_back(steps);
    }

    pub(crate) fn prompt_texts(&self) -> Vec<String> {
        lock(&self.prompts).iter().map(|(_, t)| t.clone()).collect()
    }

    pub(crate) fn prompt_sessions(&self) -> Vec<String> {
        lock(&self.prompts).iter().map(|(s, _)| s.clone()).collect()
    }

    /// Make the stub offer a thinking-level option (`levels`, currently
    /// `current`) the way Cursor does: it persists across ACP sessions.
    pub(crate) fn offer_thought_levels(&self, levels: &[&str], current: &str) {
        *lock(&self.thought) = Some(StubThought {
            levels: levels.iter().map(|level| level.to_string()).collect(),
            current: current.to_string(),
        });
    }

    /// Make every `session/set_config_option` fail.
    pub(crate) fn reject_config_changes(&self) {
        self.reject_config.store(true, Ordering::SeqCst);
    }

    /// The thinking level the stub currently has (see
    /// [`StubAgent::offer_thought_levels`]).
    pub(crate) fn thought_level(&self) -> Option<String> {
        lock(&self.thought).as_ref().map(|t| t.current.clone())
    }

    pub(crate) fn config_call_values(&self) -> Vec<(String, String)> {
        lock(&self.config_calls)
            .iter()
            .map(|(_, id, value)| (id.clone(), value.clone()))
            .collect()
    }

    pub(crate) fn reply_values(&self) -> Vec<Value> {
        lock(&self.replies).clone()
    }

    /// A connector that starts a fresh in-process stub per client.
    pub(crate) fn connector(self: &Arc<Self>) -> AcpConnector {
        let agent = self.clone();
        Arc::new(move |options| {
            let agent = agent.clone();
            Box::pin(async move {
                agent.spawns.fetch_add(1, Ordering::SeqCst);
                let (client_read, agent_out) = duplex(1 << 20);
                let (agent_in, client_write) = duplex(1 << 20);
                tokio::spawn(serve(agent, agent_in, agent_out));
                AcpClient::connect_streams(client_read, client_write, options).await
            })
        })
    }
}

async fn send(out: &Out, frame: Value) {
    let mut line = frame.to_string();
    line.push('\n');
    let mut out = out.lock().await;
    let _ = out.write_all(line.as_bytes()).await;
    let _ = out.flush().await;
}

async fn serve(agent: Arc<StubAgent>, input: DuplexStream, output: DuplexStream) {
    let out: Out = Arc::new(Mutex::new(output));
    let pending: Pending = Arc::new(StdMutex::new(HashMap::new()));
    let cancel = Arc::new(tokio::sync::Notify::new());
    let next_id = Arc::new(AtomicI64::new(1000));
    let mut lines = BufReader::new(input).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(frame) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(method) = frame.get("method").and_then(Value::as_str) else {
            // A response to one of our reverse requests.
            if let Some(id) = frame.get("id").and_then(Value::as_i64) {
                if let Some(tx) = lock(&pending).remove(&id) {
                    let _ = tx.send(frame);
                }
            }
            continue;
        };
        let id = frame.get("id").cloned().unwrap_or(Value::Null);
        let reply = |result: Value| json!({ "jsonrpc": "2.0", "id": id, "result": result });
        match method {
            "initialize" => {
                send(
                    &out,
                    reply(json!({
                        "protocolVersion": 1,
                        "agentCapabilities": { "loadSession": true },
                        "authMethods": [ { "id": "cursor_login", "name": "Cursor Login" } ]
                    })),
                )
                .await;
            }
            "authenticate" => send(&out, reply(json!({}))).await,
            "session/new" => {
                let n = agent.sessions.fetch_add(1, Ordering::SeqCst) + 1;
                let mut result = json!({
                    "sessionId": format!("stub-session-{n}"),
                    "modes": {
                        "currentModeId": "agent",
                        "availableModes": [ { "id": "agent" }, { "id": "plan" }, { "id": "ask" } ]
                    }
                });
                if let Some(thought) = lock(&agent.thought).as_ref() {
                    result["models"] = json!({ "currentModelId": "stub-model" });
                    result["configOptions"] = thought.config_options();
                }
                send(&out, reply(result)).await;
            }
            "session/set_config_option" => {
                let params = &frame["params"];
                let text = |key: &str| params[key].as_str().unwrap_or_default().to_string();
                let (session, config_id, value) =
                    (text("sessionId"), text("configId"), text("value"));
                lock(&agent.config_calls).push((session, config_id.clone(), value.clone()));
                let refused = agent.reject_config.load(Ordering::SeqCst);
                let answer = {
                    let mut thought = lock(&agent.thought);
                    match thought.as_mut() {
                        Some(t)
                            if !refused
                                && config_id == "reasoning_effort"
                                && t.levels.contains(&value) =>
                        {
                            t.current = value;
                            reply(json!({ "configOptions": t.config_options() }))
                        }
                        _ => json!({ "jsonrpc": "2.0", "id": id, "error": {
                            "code": -32602, "message": "Invalid params",
                            "data": { "message": format!("cannot set {config_id}") } } }),
                    }
                };
                send(&out, answer).await;
            }
            "session/set_mode" => {
                lock(&agent.modes).push((
                    frame["params"]["sessionId"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                    frame["params"]["modeId"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                ));
                send(&out, reply(json!({}))).await;
            }
            "session/cancel" => {
                agent.cancels.fetch_add(1, Ordering::SeqCst);
                cancel.notify_one();
            }
            "session/prompt" => {
                let session_id = frame["params"]["sessionId"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                let text = frame["params"]["prompt"][0]["text"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                lock(&agent.prompts).push((session_id.clone(), text));
                let steps = lock(&agent.scripts)
                    .pop_front()
                    .unwrap_or_else(|| vec![text_chunk("ok")]);
                tokio::spawn(run_prompt(
                    agent.clone(),
                    out.clone(),
                    pending.clone(),
                    cancel.clone(),
                    next_id.clone(),
                    session_id,
                    id.clone(),
                    steps,
                ));
            }
            other => {
                send(
                    &out,
                    json!({ "jsonrpc": "2.0", "id": id, "error": {
                        "code": -32601, "message": format!("stub: {other} unsupported") } }),
                )
                .await;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_prompt(
    agent: Arc<StubAgent>,
    out: Out,
    pending: Pending,
    cancel: Arc<tokio::sync::Notify>,
    next_id: Arc<AtomicI64>,
    session_id: String,
    prompt_id: Value,
    steps: Vec<StubStep>,
) {
    let mut stop_reason = "end_turn";
    for step in steps {
        match step {
            StubStep::Update(update) => {
                send(
                    &out,
                    json!({ "jsonrpc": "2.0", "method": "session/update",
                        "params": { "sessionId": session_id, "update": update } }),
                )
                .await;
            }
            StubStep::Request { method, params } => {
                let id = next_id.fetch_add(1, Ordering::SeqCst);
                let (tx, rx) = oneshot::channel();
                lock(&pending).insert(id, tx);
                send(
                    &out,
                    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }),
                )
                .await;
                tokio::select! {
                    answer = rx => {
                        if let Ok(frame) = answer {
                            lock(&agent.replies).push(frame);
                        }
                    }
                    _ = cancel.notified() => {
                        stop_reason = "cancelled";
                        break;
                    }
                }
            }
            StubStep::Sleep(ms) => {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(ms)) => {}
                    _ = cancel.notified() => {
                        stop_reason = "cancelled";
                        break;
                    }
                }
            }
            StubStep::Hang(ms) => {
                tokio::time::sleep(Duration::from_millis(ms)).await;
            }
            StubStep::WaitCancel => {
                let _ = tokio::time::timeout(Duration::from_secs(10), cancel.notified()).await;
                stop_reason = "cancelled";
                break;
            }
            StubStep::Crash => {
                let _ = out.lock().await.shutdown().await;
                return;
            }
            StubStep::Fail(message) => {
                send(
                    &out,
                    json!({ "jsonrpc": "2.0", "id": prompt_id,
                        "error": { "code": -32000, "message": message } }),
                )
                .await;
                return;
            }
        }
    }
    send(
        &out,
        json!({ "jsonrpc": "2.0", "id": prompt_id, "result": { "stopReason": stop_reason } }),
    )
    .await;
}

/// A plugin directory containing the ACP agent `stub-plugin.stub`.
pub(crate) struct StubPlugin {
    pub manager: Arc<PluginManager>,
    dir: PathBuf,
}

impl StubPlugin {
    pub(crate) async fn new() -> Self {
        let dir =
            std::env::temp_dir().join(format!("kkagent-acp-backend-{}", uuid::Uuid::new_v4()));
        let root = dir.join(STUB_PLUGIN);
        tokio::fs::create_dir_all(&root).await.unwrap();
        tokio::fs::write(
            root.join("kk.plugin.json"),
            json!({
                "name": STUB_PLUGIN,
                "subagents": [{
                    "name": "stub",
                    "transport": "acp",
                    "description": "Stub ACP agent",
                    "timeoutSecs": 5,
                    "transportConfig": { "command": ["stub-agent", "acp"] }
                }, {
                    "name": "inner",
                    "transport": "internal",
                    "description": "Not an ACP agent"
                }]
            })
            .to_string(),
        )
        .await
        .unwrap();
        let manager = PluginManager::discover(&dir).await;
        Self { manager, dir }
    }
}

impl Drop for StubPlugin {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
