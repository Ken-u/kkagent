//! End-to-end tests for the embedding-host surface.
//!
//! Every test starts the real `kkagent acp` binary with a throw-away
//! `KKAGENT_HOME`, a scripted OpenAI-compatible model on a local socket, and a
//! hand-driven ACP client on stdio — the same boundary a supervising process
//! uses. Nothing here touches the user's real configuration.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use kkagent_core::storage::{handle_store_request, MemoryStore, SessionStore};
use serde_json::{json, Value};

const TIMEOUT: Duration = Duration::from_secs(90);
const INSTRUCTIONS_MARK: &str = "E2E-PROJECT-INSTRUCTIONS-MARK";

// ---------------------------------------------------------------------------
// Scripted model
// ---------------------------------------------------------------------------

enum Reply {
    Text(String),
    ToolCall {
        id: String,
        name: String,
        input: Value,
    },
    /// Keep the request open until the test ends (a long model call).
    Hang,
}

impl Reply {
    fn text(text: &str) -> Self {
        Self::Text(text.to_string())
    }

    fn tool(id: &str, name: &str, input: Value) -> Self {
        Self::ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            input,
        }
    }

    fn sse(&self) -> String {
        let chunk = match self {
            Self::Text(text) => json!({"choices": [{"delta": {"content": text}}]}),
            Self::ToolCall { id, name, input } => json!({"choices": [{"delta": {"tool_calls": [{
                "index": 0,
                "id": id,
                "function": {"name": name, "arguments": input.to_string()},
            }]}}]}),
            Self::Hang => unreachable!("a hanging reply has no body"),
        };
        format!("data: {chunk}\n\ndata: [DONE]\n\n")
    }
}

type Script = dyn Fn(&Value, usize) -> Reply + Send + Sync;

struct MockLlm {
    base_url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
}

impl MockLlm {
    /// `script` picks the reply from the parsed request body and the number
    /// of requests seen so far. Deciding from the content (instead of
    /// replaying a fixed list) keeps tests independent of background model
    /// calls such as title generation.
    fn start(script: impl Fn(&Value, usize) -> Reply + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock model");
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let script: Arc<Script> = Arc::new(script);
        let counter = Arc::new(AtomicUsize::new(0));
        {
            let requests = Arc::clone(&requests);
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                for stream in listener.incoming() {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(stream) = stream else { continue };
                    let requests = Arc::clone(&requests);
                    let stop = Arc::clone(&stop);
                    let script = Arc::clone(&script);
                    let counter = Arc::clone(&counter);
                    thread::spawn(move || serve(stream, &requests, &stop, &*script, &counter));
                }
            });
        }
        Self {
            base_url,
            requests,
            stop,
        }
    }

    fn requests(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }

    fn wait_for_requests(&self, count: usize) {
        let deadline = Instant::now() + TIMEOUT;
        while self.requests().len() < count {
            assert!(
                Instant::now() < deadline,
                "model never received request #{count}"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for MockLlm {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop so its thread ends.
        let _ = TcpStream::connect(self.base_url.trim_start_matches("http://"));
    }
}

fn serve(
    mut stream: TcpStream,
    requests: &Mutex<Vec<Value>>,
    stop: &AtomicBool,
    script: &Script,
    counter: &AtomicUsize,
) {
    let mut request = Vec::new();
    let mut chunk = [0_u8; 16_384];
    let body = loop {
        let Ok(read) = stream.read(&mut chunk) else {
            return;
        };
        if read == 0 {
            return;
        }
        request.extend_from_slice(&chunk[..read]);
        let Some(header_end) = request.windows(4).position(|w| w == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&request[..header_end]).to_ascii_lowercase();
        let length = headers
            .lines()
            .find_map(|line| {
                line.strip_prefix("content-length:")?
                    .trim()
                    .parse::<usize>()
                    .ok()
            })
            .unwrap_or(0);
        if request.len() >= header_end + 4 + length {
            break request[header_end + 4..header_end + 4 + length].to_vec();
        }
    };
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let index = counter.fetch_add(1, Ordering::SeqCst);
    requests.lock().unwrap().push(body.clone());
    let reply = script(&body, index);
    if matches!(reply, Reply::Hang) {
        while !stop.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(20));
        }
        return;
    }
    let payload = reply.sse();
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
        payload.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

fn tool_names(request: &Value) -> Vec<String> {
    request["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool["function"]["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn has_tool_result(request: &Value) -> bool {
    request["messages"]
        .as_array()
        .is_some_and(|messages| messages.iter().any(|m| m["role"] == "tool"))
}

fn last_tool_result(request: &Value) -> String {
    request["messages"]
        .as_array()
        .and_then(|messages| messages.iter().rev().find(|m| m["role"] == "tool"))
        .map(|message| message["content"].to_string())
        .unwrap_or_default()
}

fn mentions(request: &Value, needle: &str) -> bool {
    request.to_string().contains(needle)
}

// ---------------------------------------------------------------------------
// ACP client
// ---------------------------------------------------------------------------

type AgentRequestHandler = Box<dyn FnMut(&str, &Value) -> Option<Result<Value, (i64, String)>>>;

struct Acp {
    child: Child,
    stdin: ChildStdin,
    incoming: Receiver<Value>,
    next_id: u64,
    /// Notifications from the agent, in arrival order.
    notifications: Vec<Value>,
    /// Requests the agent made to this client, in arrival order.
    agent_requests: Vec<(String, Value)>,
    answer: AgentRequestHandler,
    /// Methods whose requests are recorded but never answered (a dialog the
    /// user leaves open, a tool call that is still running).
    parked: Vec<String>,
    responses: HashMap<u64, Result<Value, Value>>,
    stderr: Arc<Mutex<String>>,
    home: PathBuf,
    workspace: PathBuf,
    _dir: tempfile::TempDir,
}

fn config_toml(base_url: &str, max_steps: u32, extra: &str) -> String {
    format!(
        r#"
default_model = "test/model"
default_permission_mode = "manual"

[providers.test]
type = "openai-chat"
api_key = "token"
base_url = "{base_url}"

[models."test/model"]
provider = "test"
model = "test-model"
max_context_size = 1000000
max_output_size = 1000
capabilities = ["tool_use"]

[loop_control]
max_attempts_per_step = 1
reserved_context_size = 1000
max_steps_per_turn = {max_steps}
auto_compact = false
compact_keep_last = 4
token_counting = "estimated"

{extra}
"#
    )
}

impl Acp {
    fn spawn(llm: &MockLlm, extra_config: &str, args: &[&str]) -> Self {
        Self::spawn_with(llm, 8, extra_config, args)
    }

    fn spawn_with(llm: &MockLlm, max_steps: u32, extra_config: &str, args: &[&str]) -> Self {
        let dir = tempfile::tempdir().expect("temp home");
        let home = dir.path().join("home");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(
            workspace.join("AGENTS.md"),
            format!("# Project rules\n{INSTRUCTIONS_MARK}\n"),
        )
        .unwrap();
        let config = home.join("config.toml");
        std::fs::write(&config, config_toml(&llm.base_url, max_steps, extra_config)).unwrap();

        let mut child = Command::new(env!("CARGO_BIN_EXE_kkagent"))
            .arg("--config")
            .arg(&config)
            .arg("acp")
            .args(args)
            .current_dir(&workspace)
            .env("KKAGENT_HOME", &home)
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("RUST_LOG", "warn")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start kkagent acp");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut stderr_pipe = child.stderr.take().unwrap();

        let (tx, incoming) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Ok(value) = serde_json::from_str::<Value>(&line) {
                    if tx.send(value).is_err() {
                        break;
                    }
                }
            }
        });
        let stderr = Arc::new(Mutex::new(String::new()));
        {
            let stderr = Arc::clone(&stderr);
            thread::spawn(move || {
                let mut text = String::new();
                let mut buffer = [0_u8; 4096];
                while let Ok(read) = stderr_pipe.read(&mut buffer) {
                    if read == 0 {
                        break;
                    }
                    text.push_str(&String::from_utf8_lossy(&buffer[..read]));
                    *stderr.lock().unwrap() = text.clone();
                }
            });
        }

        Self {
            child,
            stdin,
            incoming,
            next_id: 1,
            notifications: Vec::new(),
            agent_requests: Vec::new(),
            answer: Box::new(|_, _| None),
            parked: Vec::new(),
            responses: HashMap::new(),
            stderr,
            home,
            workspace: workspace.canonicalize().unwrap_or(workspace),
            _dir: dir,
        }
    }

    fn cwd(&self) -> String {
        self.workspace.to_string_lossy().to_string()
    }

    /// Install the answers to requests the agent sends to this client.
    fn on_agent_request(
        &mut self,
        handler: impl FnMut(&str, &Value) -> Option<Result<Value, (i64, String)>> + 'static,
    ) {
        self.answer = Box::new(handler);
    }

    /// Record requests of `method` without ever answering them.
    fn park(&mut self, method: &str) {
        self.parked.push(method.to_string());
    }

    fn write(&mut self, message: &Value) {
        let mut line = message.to_string();
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .and_then(|()| self.stdin.flush())
            .unwrap_or_else(|error| panic!("kkagent acp is gone ({error})\n{}", self.logs()));
    }

    fn logs(&self) -> String {
        format!("--- kkagent stderr ---\n{}", self.stderr.lock().unwrap())
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.write(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    fn send(&mut self, method: &str, params: Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.write(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        id
    }

    fn handle_incoming(&mut self, message: Value) {
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_string);
        match (method, message.get("id").cloned()) {
            (Some(method), Some(id)) => {
                let params = message.get("params").cloned().unwrap_or(Value::Null);
                self.agent_requests.push((method.clone(), params.clone()));
                if self.parked.contains(&method) {
                    return;
                }
                let reply = match (self.answer)(&method, &params) {
                    Some(Ok(result)) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                    Some(Err((code, text))) => json!({
                        "jsonrpc": "2.0", "id": id,
                        "error": {"code": code, "message": text},
                    }),
                    None => json!({
                        "jsonrpc": "2.0", "id": id,
                        "error": {"code": -32601, "message": format!("Method not found: {method}")},
                    }),
                };
                self.write(&reply);
            }
            (Some(_), None) => self.notifications.push(message),
            (None, Some(id)) => {
                let key = id.as_u64().unwrap_or(u64::MAX);
                let outcome = match message.get("error") {
                    Some(error) => Err(error.clone()),
                    None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                };
                self.responses.insert(key, outcome);
            }
            (None, None) => {}
        }
    }

    /// Pump messages until `done` holds (or the timeout hits).
    fn pump_until(&mut self, what: &str, mut done: impl FnMut(&mut Self) -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        while !done(self) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self
                .incoming
                .recv_timeout(remaining.min(Duration::from_millis(200)))
            {
                Ok(message) => self.handle_incoming(message),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    assert!(
                        Instant::now() < deadline,
                        "timed out waiting for {what}\n{}",
                        self.logs()
                    );
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    panic!(
                        "kkagent acp closed stdout while waiting for {what}\n{}",
                        self.logs()
                    )
                }
            }
        }
    }

    fn wait(&mut self, id: u64) -> Result<Value, Value> {
        self.pump_until(&format!("response {id}"), |acp| {
            acp.responses.contains_key(&id)
        });
        self.responses.remove(&id).unwrap()
    }

    fn call(&mut self, method: &str, params: Value) -> Result<Value, Value> {
        let id = self.send(method, params);
        self.wait(id)
    }

    fn ok(&mut self, method: &str, params: Value) -> Value {
        let result = self.call(method, params);
        match result {
            Ok(value) => value,
            Err(error) => panic!("{method} failed: {error}\n{}", self.logs()),
        }
    }

    fn initialize(&mut self, meta: Option<Value>) -> Value {
        let mut params = json!({"protocolVersion": 1, "clientCapabilities": {}});
        if let Some(meta) = meta {
            params["_meta"] = meta;
        }
        self.ok("initialize", params)
    }

    fn new_session(&mut self, extra: Value) -> String {
        let mut params = json!({"cwd": self.cwd(), "mcpServers": []});
        if let Value::Object(extra) = extra {
            for (key, value) in extra {
                params[key] = value;
            }
        }
        let created = self.ok("session/new", params);
        created["sessionId"]
            .as_str()
            .expect("sessionId")
            .to_string()
    }

    fn prompt(&mut self, session_id: &str, text: &str) -> Value {
        self.ok(
            "session/prompt",
            json!({"sessionId": session_id, "prompt": [{"type": "text", "text": text}]}),
        )
    }

    /// `params.update` of every `session/update` for the session.
    fn updates(&self, session_id: &str) -> Vec<Value> {
        self.notifications
            .iter()
            .filter(|n| n["method"] == "session/update" && n["params"]["sessionId"] == session_id)
            .map(|n| n["params"]["update"].clone())
            .collect()
    }

    fn updates_of(&self, session_id: &str, kind: &str) -> Vec<Value> {
        self.updates(session_id)
            .into_iter()
            .filter(|update| update["sessionUpdate"] == kind)
            .collect()
    }

    fn agent_text(&self, session_id: &str) -> String {
        self.updates_of(session_id, "agent_message_chunk")
            .iter()
            .filter_map(|update| update["content"]["text"].as_str())
            .collect()
    }

    fn requests_for(&self, method: &str) -> Vec<Value> {
        self.agent_requests
            .iter()
            .filter(|(name, _)| name == method)
            .map(|(_, params)| params.clone())
            .collect()
    }

    fn wait_for_update(&mut self, session_id: &str, kind: &str) {
        let (session_id, kind) = (session_id.to_string(), kind.to_string());
        self.pump_until(&format!("a `{kind}` update"), move |acp| {
            !acp.updates_of(&session_id, &kind).is_empty()
        });
    }

    fn wait_for_agent_request(&mut self, method: &str) {
        let method = method.to_string();
        self.pump_until(&format!("a `{method}` request"), move |acp| {
            !acp.requests_for(&method).is_empty()
        });
    }
}

impl Drop for Acp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn host_meta(host: Value) -> Value {
    json!({"kkagent/host": host})
}

fn error_code(error: &Value) -> i64 {
    error["code"].as_i64().unwrap_or_default()
}

/// What a host answers to one agent-to-host request: `None` = not a method it
/// serves, otherwise a result or a `(code, message)` error.
type HostAnswer = Option<Result<Value, (i64, String)>>;

/// A host that serves `_kkagent/store/*` from memory.
fn memory_host() -> (Arc<MemoryStore>, impl FnMut(&str, &Value) -> HostAnswer) {
    let store = Arc::new(MemoryStore::new());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let served = Arc::clone(&store);
    let handler = move |method: &str, params: &Value| {
        runtime
            .block_on(handle_store_request(&*served, &*served, method, params))
            .map(|result| result.map_err(|error| (error.code, error.message)))
    };
    (store, handler)
}

fn poll_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(50));
    }
}

// ---------------------------------------------------------------------------
// Defaults stay compatible
// ---------------------------------------------------------------------------

#[test]
fn default_configuration_speaks_plain_acp_v1() {
    let llm = MockLlm::start(|_, _| Reply::text("hello from the mock model"));
    let mut acp = Acp::spawn(&llm, "", &[]);

    let init = acp.initialize(None);
    assert_eq!(init["protocolVersion"], 1);
    assert_eq!(init["agentInfo"]["name"], "kkagent");
    assert_eq!(init["agentCapabilities"]["loadSession"], true);
    for capability in ["list", "resume", "close"] {
        assert!(
            init["agentCapabilities"]["sessionCapabilities"][capability].is_object(),
            "sessionCapabilities.{capability}"
        );
    }
    assert!(
        init.get("_meta").is_none(),
        "a client that does not speak the host extension is not answered in it"
    );

    let sid = acp.new_session(json!({}));
    let listed = acp.ok("session/list", json!({}));
    assert!(listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|session| session["sessionId"] == json!(sid)));
    // The command list follows the response of a new session.
    acp.wait_for_update(&sid, "available_commands_update");

    let result = acp.prompt(&sid, "say hello");
    assert_eq!(result["stopReason"], "end_turn");
    // Every update of the turn arrived before its response.
    assert!(acp.agent_text(&sid).contains("hello from the mock model"));

    // Default behavior: project instructions are in the prompt and the
    // built-in tools are offered; no host surface exists.
    let request = &llm.requests()[0];
    assert!(mentions(request, INSTRUCTIONS_MARK));
    let tools = tool_names(request);
    assert!(tools.iter().any(|name| name == "Read"), "{tools:?}");
    assert!(tools.iter().any(|name| name == "Bash"), "{tools:?}");
    assert!(acp.requests_for("_kkagent/tool/call").is_empty());
    assert!(!acp
        .agent_requests
        .iter()
        .any(|(method, _)| method.starts_with("_kkagent/store/")));
}

#[test]
fn project_instructions_can_be_switched_off() {
    let llm = MockLlm::start(|_, _| Reply::text("ok"));
    let mut acp = Acp::spawn(&llm, "", &["--no-project-instructions"]);
    acp.initialize(None);
    let sid = acp.new_session(json!({}));
    acp.prompt(&sid, "hi");
    assert!(!mentions(&llm.requests()[0], INSTRUCTIONS_MARK));
}

#[test]
fn a_session_can_switch_extensions_off_but_not_on() {
    // Per-session `extensions` only narrow what the process allows.
    let llm = MockLlm::start(|_, _| Reply::text("ok"));
    let mut acp = Acp::spawn(&llm, "", &[]);
    acp.initialize(None);
    let off = acp.new_session(json!({"_meta": host_meta(json!({
        "extensions": {"loadProjectInstructions": false}
    }))}));
    acp.prompt(&off, "hi");
    assert!(!mentions(&llm.requests()[0], INSTRUCTIONS_MARK));

    let default = acp.new_session(json!({"_meta": host_meta(json!({
        "extensions": {"loadProjectInstructions": true}
    }))}));
    acp.prompt(&default, "hi");
    assert!(mentions(&llm.requests()[1], INSTRUCTIONS_MARK));
}

// ---------------------------------------------------------------------------
// Host tools
// ---------------------------------------------------------------------------

#[test]
fn host_tools_round_trip_through_the_model_and_back() {
    let llm = MockLlm::start(|request, _| {
        if has_tool_result(request) {
            Reply::text("all done")
        } else {
            Reply::tool("call_1", "host_ping", json!({"message": "ping"}))
        }
    });
    let mut acp = Acp::spawn(&llm, "", &[]);
    let init = acp.initialize(Some(host_meta(json!({"tools": true}))));
    assert_eq!(init["_meta"]["kkagent/host"]["tools"], true);
    acp.on_agent_request(|method, params| {
        (method == "_kkagent/tool/call").then(|| {
            assert_eq!(params["name"], "host_ping");
            Ok(json!({"content": format!("pong:{}", params["input"]["message"].as_str().unwrap())}))
        })
    });

    let sid = acp.new_session(json!({"_meta": host_meta(json!({"hostTools": [{
        "name": "host_ping",
        "description": "Ping the host",
        "inputSchema": {"type": "object", "properties": {"message": {"type": "string"}}},
        "readOnly": true,
    }]}))}));
    let result = acp.prompt(&sid, "ping the host");
    assert_eq!(result["stopReason"], "end_turn");
    assert_eq!(acp.agent_text(&sid), "all done");

    // The model was offered the tool and received the host's answer.
    let requests = llm.requests();
    assert!(tool_names(&requests[0]).contains(&"host_ping".to_string()));
    let follow_up = requests.iter().find(|r| has_tool_result(r)).unwrap();
    assert!(last_tool_result(follow_up).contains("pong:ping"));

    // The host saw exactly one call, with the session and call ids.
    let calls = acp.requests_for("_kkagent/tool/call");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["sessionId"], json!(sid));
    assert_eq!(calls[0]["toolCallId"], "call_1");

    // Streamed as an ordinary tool call.
    let started = &acp.updates_of(&sid, "tool_call")[0];
    assert_eq!(started["title"], "host_ping");
    assert_eq!(started["toolCallId"], "call_1");
    let finished: Vec<_> = acp
        .updates_of(&sid, "tool_call_update")
        .into_iter()
        .filter(|update| update["status"] == "completed")
        .collect();
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0]["content"][0]["content"]["text"], "pong:ping");
}

#[test]
fn a_failing_host_tool_reports_an_error_result_to_the_model() {
    let llm = MockLlm::start(|request, _| {
        if has_tool_result(request) {
            Reply::text("handled")
        } else {
            Reply::tool("call_9", "host_fail", json!({}))
        }
    });
    let mut acp = Acp::spawn(&llm, "", &[]);
    acp.initialize(Some(host_meta(json!({"tools": true}))));
    acp.on_agent_request(|method, _| {
        (method == "_kkagent/tool/call").then(|| Ok(json!({"content": "denied", "isError": true})))
    });
    let sid = acp.new_session(json!({"_meta": host_meta(json!({
        "hostTools": [{"name": "host_fail"}]
    }))}));
    assert_eq!(acp.prompt(&sid, "go")["stopReason"], "end_turn");
    let failed: Vec<_> = acp
        .updates_of(&sid, "tool_call_update")
        .into_iter()
        .filter(|update| update["status"] == "failed")
        .collect();
    assert_eq!(failed.len(), 1, "{:?}", acp.updates(&sid));
}

#[test]
fn host_tools_need_the_capability_and_cannot_shadow_built_ins() {
    let llm = MockLlm::start(|_, _| Reply::text("ok"));
    let mut acp = Acp::spawn(&llm, "", &[]);
    acp.initialize(None);
    let refused = acp
        .call(
            "session/new",
            json!({"cwd": acp.cwd(), "_meta": host_meta(json!({"hostTools": [{"name": "host_ping"}]}))}),
        )
        .unwrap_err();
    assert_eq!(error_code(&refused), -32000, "{refused}");
    assert!(refused["message"].as_str().unwrap().contains("advertise"));

    // Advertise the capability: injecting a tool that reuses a built-in name
    // is still refused (no shadowing).
    let mut acp = Acp::spawn(&llm, "", &[]);
    acp.initialize(Some(host_meta(json!({"tools": true}))));
    let shadow = acp
        .call(
            "session/new",
            json!({"cwd": acp.cwd(), "_meta": host_meta(json!({"hostTools": [{"name": "Bash"}]}))}),
        )
        .unwrap_err();
    assert!(
        shadow["message"].as_str().unwrap().contains("built-in"),
        "{shadow}"
    );
}

#[test]
fn cancelling_a_turn_cancels_the_host_tool_call() {
    let llm = MockLlm::start(|request, _| {
        if has_tool_result(request) {
            Reply::text("fine")
        } else {
            Reply::tool("call_slow", "host_slow", json!({}))
        }
    });
    let mut acp = Acp::spawn(&llm, "", &[]);
    acp.initialize(Some(host_meta(json!({"tools": true}))));
    // The host starts the slow call and never finishes it.
    acp.park("_kkagent/tool/call");
    let sid = acp.new_session(json!({"_meta": host_meta(json!({
        "hostTools": [{"name": "host_slow", "timeoutSecs": 600}]
    }))}));
    let turn = acp.send(
        "session/prompt",
        json!({"sessionId": sid, "prompt": [{"type": "text", "text": "go"}]}),
    );
    acp.wait_for_agent_request("_kkagent/tool/call");
    acp.notify("session/cancel", json!({"sessionId": sid}));
    assert_eq!(acp.wait(turn).unwrap()["stopReason"], "cancelled");

    // The host is told to stop the call it was running (and that the
    // pending request is abandoned).
    acp.pump_until("_kkagent/tool/cancel", |acp| {
        acp.notifications
            .iter()
            .any(|n| n["method"] == "_kkagent/tool/cancel")
    });
    let cancel = acp
        .notifications
        .iter()
        .find(|n| n["method"] == "_kkagent/tool/cancel")
        .unwrap();
    assert_eq!(cancel["params"]["toolCallId"], "call_slow");
    assert_eq!(cancel["params"]["sessionId"], json!(sid));
    assert!(acp
        .notifications
        .iter()
        .any(|n| n["method"] == "$/cancel_request"));

    // The session is not left in a cancelled state: with the host answering
    // again, the next turn completes normally.
    acp.parked.clear();
    acp.on_agent_request(|method, _| {
        (method == "_kkagent/tool/call").then(|| Ok(json!({"content": "late"})))
    });
    assert_eq!(acp.prompt(&sid, "again")["stopReason"], "end_turn");
}

// ---------------------------------------------------------------------------
// Host storage
// ---------------------------------------------------------------------------

fn local_session_rows(home: &Path) -> i64 {
    let db = home.join("transcripts.db");
    if !db.exists() {
        return 0;
    }
    rusqlite::Connection::open(db)
        .and_then(|conn| conn.query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0)))
        .unwrap_or(0)
}

#[test]
fn host_storage_keeps_the_conversation_in_the_host() {
    let llm = MockLlm::start(|_, _| Reply::text("stored reply"));
    let mut acp = Acp::spawn(&llm, "", &["--storage", "host"]);
    let (store, handler) = memory_host();
    acp.on_agent_request(handler);

    let init = acp.initialize(Some(host_meta(
        json!({"store": {"sessions": true, "events": true}}),
    )));
    assert_eq!(init["_meta"]["kkagent/host"]["store"]["sessions"], true);
    let sid = acp.new_session(json!({}));
    assert_eq!(acp.prompt(&sid, "remember me")["stopReason"], "end_turn");

    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let stored = runtime.block_on(store.load_messages(&sid)).unwrap();
    let roles: Vec<&str> = stored.iter().map(|m| m.role.as_str()).collect();
    assert!(
        roles.contains(&"user") && roles.contains(&"assistant"),
        "{roles:?}"
    );
    assert!(stored
        .iter()
        .any(|m| m.content_json.contains("remember me")));
    assert_eq!(
        local_session_rows(&acp.home),
        0,
        "nothing about the conversation is written to the local database"
    );

    // Closing and resuming goes back to the host for the history.
    acp.ok("session/close", json!({"sessionId": sid}));
    let listed = acp.ok("session/list", json!({}));
    assert!(listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|session| session["sessionId"] == json!(sid)));
    acp.ok(
        "session/resume",
        json!({"sessionId": sid, "cwd": acp.cwd()}),
    );
    acp.prompt(&sid, "what did I say?");
    let last = llm.requests().last().cloned().unwrap();
    assert!(
        mentions(&last, "remember me"),
        "history came back from the host"
    );

    // Deleting removes it from the host too.
    acp.ok("session/delete", json!({"sessionId": sid}));
    assert!(runtime.block_on(store.get_session(&sid)).unwrap().is_none());
    let gone = acp
        .call(
            "session/resume",
            json!({"sessionId": sid, "cwd": acp.cwd()}),
        )
        .unwrap_err();
    assert_eq!(error_code(&gone), -32002, "{gone}");

    let methods: Vec<String> = acp.agent_requests.iter().map(|(m, _)| m.clone()).collect();
    for expected in [
        "_kkagent/store/session/create",
        "_kkagent/store/messages/append",
        "_kkagent/store/messages/load",
        "_kkagent/store/session/delete",
    ] {
        assert!(
            methods.iter().any(|m| m == expected),
            "{expected} in {methods:?}"
        );
    }
}

#[test]
fn host_storage_is_refused_when_the_client_does_not_offer_it() {
    let llm = MockLlm::start(|_, _| Reply::text("never"));
    let mut acp = Acp::spawn(&llm, "", &["--storage", "host"]);
    let error = acp
        .call(
            "initialize",
            json!({"protocolVersion": 1, "clientCapabilities": {}}),
        )
        .unwrap_err();
    assert_eq!(error_code(&error), -32602, "{error}");
    assert!(error["message"]
        .as_str()
        .unwrap()
        .contains("store.sessions"));
    // Nothing was stored anywhere.
    assert_eq!(local_session_rows(&acp.home), 0);
    assert!(llm.requests().is_empty());
}

#[test]
fn a_failing_host_store_fails_the_turn_instead_of_falling_back_to_sqlite() {
    let llm = MockLlm::start(|_, _| Reply::text("lost reply"));
    let mut acp = Acp::spawn(&llm, "", &["--storage", "host"]);
    let (_store, mut serve) = memory_host();
    let broken = Arc::new(AtomicBool::new(false));
    {
        let broken = Arc::clone(&broken);
        acp.on_agent_request(move |method, params| {
            if broken.load(Ordering::SeqCst) && method == "_kkagent/store/messages/append" {
                return Some(Err((-32000, "disk full".to_string())));
            }
            serve(method, params)
        });
    }
    acp.initialize(Some(host_meta(json!({"store": {"sessions": true}}))));
    let sid = acp.new_session(json!({}));
    broken.store(true, Ordering::SeqCst);
    let error = acp
        .call(
            "session/prompt",
            json!({"sessionId": sid, "prompt": [{"type": "text", "text": "x"}]}),
        )
        .unwrap_err();
    assert_eq!(error_code(&error), -32000, "{error}");
    assert_eq!(local_session_rows(&acp.home), 0, "no silent local fallback");
}

// ---------------------------------------------------------------------------
// Cancel, resume and the rest of the session lifecycle
// ---------------------------------------------------------------------------

/// Text of the newest user message (earlier turns stay in the history).
fn last_user_text(request: &Value) -> String {
    request["messages"]
        .as_array()
        .and_then(|messages| messages.iter().rev().find(|m| m["role"] == "user"))
        .map(|message| message["content"].to_string())
        .unwrap_or_default()
}

fn hang_on(marker: &'static str) -> impl Fn(&Value, usize) -> Reply + Send + Sync + 'static {
    move |request, _| {
        if last_user_text(request).contains(marker) && !has_tool_result(request) {
            Reply::Hang
        } else {
            Reply::text("fine")
        }
    }
}

#[test]
fn session_cancel_ends_the_turn_with_cancelled_and_the_session_stays_usable() {
    let llm = MockLlm::start(hang_on("SLOW-REQUEST"));
    let mut acp = Acp::spawn(&llm, "", &[]);
    acp.initialize(None);
    let sid = acp.new_session(json!({}));

    let turn = acp.send(
        "session/prompt",
        json!({"sessionId": sid, "prompt": [{"type": "text", "text": "SLOW-REQUEST please"}]}),
    );
    llm.wait_for_requests(1);
    acp.notify("session/cancel", json!({"sessionId": sid}));
    let result = acp.wait(turn).expect("a cancelled prompt still answers");
    assert_eq!(result["stopReason"], "cancelled");

    // A later turn runs normally: the cancel belonged to the earlier one.
    let next = acp.prompt(&sid, "quick one");
    assert_eq!(next["stopReason"], "end_turn");
    assert!(acp.agent_text(&sid).contains("fine"));

    // A cancel while nothing runs is harmless and does not poison the next turn.
    acp.notify("session/cancel", json!({"sessionId": sid}));
    assert_eq!(acp.prompt(&sid, "another")["stopReason"], "end_turn");
}

#[test]
fn cancel_request_on_a_prompt_cancels_it_cooperatively() {
    let llm = MockLlm::start(hang_on("SLOW-REQUEST"));
    let mut acp = Acp::spawn(&llm, "", &[]);
    acp.initialize(None);
    let sid = acp.new_session(json!({}));
    let turn = acp.send(
        "session/prompt",
        json!({"sessionId": sid, "prompt": [{"type": "text", "text": "SLOW-REQUEST"}]}),
    );
    llm.wait_for_requests(1);
    acp.notify("$/cancel_request", json!({"requestId": turn}));
    assert_eq!(acp.wait(turn).unwrap()["stopReason"], "cancelled");
}

#[test]
fn sessions_can_be_listed_closed_resumed_and_loaded() {
    let llm = MockLlm::start(|_, _| Reply::text("noted"));
    let mut acp = Acp::spawn(&llm, "", &[]);
    acp.initialize(None);
    let sid = acp.new_session(json!({}));
    acp.prompt(&sid, "first question");

    // `cwd` narrows the list.
    let here = acp.ok("session/list", json!({"cwd": acp.cwd()}));
    assert!(here["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["sessionId"] == json!(sid)));
    let elsewhere = acp.ok(
        "session/list",
        json!({"cwd": "/definitely/not/a/workspace"}),
    );
    assert!(elsewhere["sessions"].as_array().unwrap().is_empty());

    // Closing frees the session: it can no longer be prompted…
    acp.ok("session/close", json!({"sessionId": sid}));
    let closed = acp
        .call(
            "session/prompt",
            json!({"sessionId": sid, "prompt": [{"type": "text", "text": "x"}]}),
        )
        .unwrap_err();
    assert_eq!(error_code(&closed), -32002, "{closed}");

    // …but resuming brings it back, history included, without replaying it.
    let before = acp.updates(&sid).len();
    let resumed = acp.ok(
        "session/resume",
        json!({"sessionId": sid, "cwd": acp.cwd()}),
    );
    assert_eq!(resumed["modes"]["currentModeId"], "agent");
    acp.prompt(&sid, "second question");
    let last = llm.requests().last().cloned().unwrap();
    assert!(
        mentions(&last, "first question"),
        "history survived the close"
    );
    assert!(
        acp.updates(&sid)[before..]
            .iter()
            .all(|update| update["sessionUpdate"] != "user_message_chunk"),
        "resume does not replay"
    );

    // Loading replays the conversation as chunks before the response.
    acp.ok("session/close", json!({"sessionId": sid}));
    let mark = acp.notifications.len();
    acp.ok("session/load", json!({"sessionId": sid, "cwd": acp.cwd()}));
    let replayed: Vec<Value> = acp.notifications[mark..]
        .iter()
        .filter(|n| n["method"] == "session/update")
        .map(|n| n["params"]["update"].clone())
        .collect();
    assert!(replayed
        .iter()
        .any(|u| u["sessionUpdate"] == "user_message_chunk"
            && u["content"]["text"] == "first question"));
    assert!(replayed
        .iter()
        .any(|u| u["sessionUpdate"] == "agent_message_chunk"));

    // Unknown sessions answer "resource not found".
    for (method, params) in [
        (
            "session/resume",
            json!({"sessionId": "nope", "cwd": acp.cwd()}),
        ),
        (
            "session/load",
            json!({"sessionId": "nope", "cwd": acp.cwd()}),
        ),
        (
            "session/prompt",
            json!({"sessionId": "nope", "prompt": [{"type": "text", "text": "x"}]}),
        ),
        (
            "session/set_mode",
            json!({"sessionId": "nope", "modeId": "plan"}),
        ),
        ("session/close", json!({"sessionId": "nope"})),
    ] {
        let error = acp.call(method, params).unwrap_err();
        assert_eq!(error_code(&error), -32002, "{method}: {error}");
    }

    // Deleting is permanent.
    acp.ok("session/delete", json!({"sessionId": sid}));
    let gone = acp
        .call(
            "session/resume",
            json!({"sessionId": sid, "cwd": acp.cwd()}),
        )
        .unwrap_err();
    assert_eq!(error_code(&gone), -32002, "{gone}");
}

#[test]
fn session_modes_are_switched_and_announced() {
    let llm = MockLlm::start(|_, _| Reply::text("ok"));
    let mut acp = Acp::spawn(&llm, "", &[]);
    acp.initialize(None);
    let sid = acp.new_session(json!({}));
    acp.ok(
        "session/set_mode",
        json!({"sessionId": sid, "modeId": "plan"}),
    );
    acp.wait_for_update(&sid, "current_mode_update");
    let mode = &acp.updates_of(&sid, "current_mode_update")[0];
    assert_eq!(mode["currentModeId"], "plan");
    let bad = acp
        .call(
            "session/set_mode",
            json!({"sessionId": sid, "modeId": "no-such-mode"}),
        )
        .unwrap_err();
    assert_eq!(error_code(&bad), -32000, "{bad}");
}

#[test]
fn a_turn_that_uses_up_its_steps_reports_max_turn_requests() {
    let llm = MockLlm::start(|request, _| {
        if has_tool_result(request) {
            Reply::text("never reached")
        } else {
            Reply::tool("call_1", "host_ping", json!({}))
        }
    });
    let mut acp = Acp::spawn_with(&llm, 1, "", &[]);
    acp.initialize(Some(host_meta(json!({"tools": true}))));
    acp.on_agent_request(|method, _| {
        (method == "_kkagent/tool/call").then(|| Ok(json!({"content": "pong"})))
    });
    let sid = acp.new_session(json!({"_meta": host_meta(json!({
        "hostTools": [{"name": "host_ping"}]
    }))}));
    assert_eq!(acp.prompt(&sid, "go")["stopReason"], "max_turn_requests");
}

#[test]
fn slash_commands_are_announced_and_answered_without_the_model() {
    let llm = MockLlm::start(|_, _| Reply::text("model reply"));
    let mut acp = Acp::spawn(&llm, "", &[]);
    acp.initialize(None);
    let sid = acp.new_session(json!({}));
    acp.wait_for_update(&sid, "available_commands_update");
    let commands = &acp.updates_of(&sid, "available_commands_update")[0]["availableCommands"];
    assert!(commands
        .as_array()
        .unwrap()
        .iter()
        .any(|command| command["name"] == "version"));
    assert_eq!(acp.prompt(&sid, "/version")["stopReason"], "end_turn");
    assert!(
        llm.requests().is_empty(),
        "a slash command never reaches the model"
    );
    assert!(!acp.agent_text(&sid).is_empty());
}

// ---------------------------------------------------------------------------
// Permission requests
// ---------------------------------------------------------------------------

#[test]
fn permission_requests_use_the_official_shape_and_a_refusal_runs_nothing() {
    let llm = MockLlm::start(|request, _| {
        if has_tool_result(request) {
            Reply::text("understood")
        } else {
            Reply::tool(
                "call_bash",
                "Bash",
                json!({"command": "echo permission-probe"}),
            )
        }
    });
    let mut acp = Acp::spawn(&llm, "", &[]);
    acp.initialize(None);
    acp.on_agent_request(|method, params| {
        (method == "session/request_permission").then(|| {
            // Official fields only: a tool call and `optionId`s.
            assert_eq!(params["toolCall"]["title"], "Bash");
            let ids: Vec<&str> = params["options"]
                .as_array()
                .unwrap()
                .iter()
                .map(|option| option["optionId"].as_str().unwrap())
                .collect();
            assert!(ids.contains(&"allow_once") && ids.contains(&"reject_once"));
            Ok(json!({"outcome": {"outcome": "selected", "optionId": "reject_once"}}))
        })
    });
    let sid = acp.new_session(json!({}));
    assert_eq!(acp.prompt(&sid, "run it")["stopReason"], "end_turn");
    assert_eq!(acp.requests_for("session/request_permission").len(), 1);

    // The refusal reached the model as a failed tool result; nothing ran.
    let failed: Vec<_> = acp
        .updates_of(&sid, "tool_call_update")
        .into_iter()
        .filter(|update| update["status"] == "failed")
        .collect();
    assert_eq!(failed.len(), 1, "{:?}", acp.updates(&sid));
    assert!(!last_tool_result(llm.requests().last().unwrap()).contains("permission-probe\\n"));
}

#[test]
fn cancelling_while_a_permission_is_pending_releases_the_client_dialog() {
    let llm = MockLlm::start(|request, _| {
        if has_tool_result(request) {
            Reply::text("after")
        } else {
            Reply::tool("call_bash", "Bash", json!({"command": "echo never-runs"}))
        }
    });
    let mut acp = Acp::spawn(&llm, "", &[]);
    acp.initialize(None);
    // The client leaves the permission dialog open.
    acp.park("session/request_permission");
    let sid = acp.new_session(json!({}));
    let turn = acp.send(
        "session/prompt",
        json!({"sessionId": sid, "prompt": [{"type": "text", "text": "run it"}]}),
    );
    acp.wait_for_agent_request("session/request_permission");
    acp.notify("session/cancel", json!({"sessionId": sid}));
    let result = acp.wait(turn).expect("prompt answers");
    assert_eq!(result["stopReason"], "cancelled");
    // The agent told the client to drop the dialog.
    acp.pump_until("$/cancel_request", |acp| {
        acp.notifications
            .iter()
            .any(|n| n["method"] == "$/cancel_request")
    });
}

// ---------------------------------------------------------------------------
// Strict roles cannot be bypassed
// ---------------------------------------------------------------------------

const READER_ROLE: &str = r#"
[agent_roles.reader]
system_prompt = "You are a read-only reviewer ROLE-PROMPT-MARK."
tools = ["Read"]
"#;

#[test]
fn a_strict_role_limits_what_the_model_can_call() {
    let marker_dir = tempfile::tempdir().unwrap();
    let marker = marker_dir.path().join("bash-ran");
    let command = format!("touch {}", marker.display());
    let llm = MockLlm::start({
        let command = command.clone();
        move |request, _| {
            if has_tool_result(request) {
                Reply::text("finished")
            } else {
                // The model asks for a tool outside the allowlist anyway.
                Reply::tool("call_bash", "Bash", json!({"command": command}))
            }
        }
    });
    let mut acp = Acp::spawn(&llm, READER_ROLE, &["--role", "reader"]);
    acp.initialize(None);
    let sid = acp.new_session(json!({}));
    assert_eq!(acp.prompt(&sid, "do it")["stopReason"], "end_turn");

    let first = &llm.requests()[0];
    assert_eq!(tool_names(first), vec!["Read".to_string()]);
    assert!(
        mentions(first, "ROLE-PROMPT-MARK"),
        "the role prompt is in force"
    );
    assert!(!marker.exists(), "Bash must not have run");
    assert!(
        acp.requests_for("session/request_permission").is_empty(),
        "a refused tool is never even offered for approval"
    );
    let result = last_tool_result(llm.requests().last().unwrap());
    assert!(result.contains("not permitted"), "{result}");
}

#[test]
fn session_parameters_cannot_widen_a_strict_role() {
    let llm = MockLlm::start(|_, _| Reply::text("ok"));
    let mut acp = Acp::spawn(&llm, READER_ROLE, &["--role", "reader"]);
    acp.initialize(Some(host_meta(json!({"tools": true}))));

    // Asking for more than the role grants changes nothing…
    let wide = acp.new_session(json!({"_meta": host_meta(json!({
        "toolAllowlist": ["Read", "Bash", "Write"],
    }))}));
    acp.prompt(&wide, "hi");
    assert_eq!(tool_names(&llm.requests()[0]), vec!["Read".to_string()]);

    // …asking for less narrows it…
    let narrow = acp.new_session(json!({"_meta": host_meta(json!({"toolAllowlist": []}))}));
    acp.prompt(&narrow, "hi");
    assert!(tool_names(&llm.requests()[1]).is_empty());

    // …naming another role is not possible: roles come from trusted config…
    let other = acp.call(
        "session/new",
        json!({"cwd": acp.cwd(), "_meta": host_meta(json!({"role": "does-not-exist"}))}),
    );
    assert!(other.is_err(), "unknown roles are refused");

    // …and a host tool is subject to the same allowlist.
    let injected = acp.new_session(json!({"_meta": host_meta(json!({
        "hostTools": [{"name": "host_extra"}],
    }))}));
    acp.prompt(&injected, "hi");
    assert_eq!(tool_names(&llm.requests()[2]), vec!["Read".to_string()]);
}

#[test]
fn a_strict_role_closes_the_fs_and_terminal_side_doors() {
    let llm = MockLlm::start(|_, _| Reply::text("ok"));
    let mut acp = Acp::spawn(&llm, READER_ROLE, &["--role", "reader"]);
    acp.initialize(None);
    let sid = acp.new_session(json!({}));
    let target = acp.workspace.join("side-door.txt");
    for (method, params) in [
        (
            "fs/write_text_file",
            json!({"sessionId": sid, "path": target.to_string_lossy(), "content": "x"}),
        ),
        (
            "fs/read_text_file",
            json!({"sessionId": sid, "path": target.to_string_lossy()}),
        ),
        (
            "terminal/create",
            json!({"sessionId": sid, "command": "echo hi"}),
        ),
    ] {
        let error = acp.call(method, params).unwrap_err();
        assert_eq!(error_code(&error), -32601, "{method}: {error}");
    }
    assert!(!target.exists());
}

#[cfg(unix)]
#[test]
fn a_strict_role_does_not_start_mcp_servers_it_could_never_call() {
    let llm = MockLlm::start(|_, _| Reply::text("ok"));
    let markers = tempfile::tempdir().unwrap();
    let started = markers.path().join("started");
    let server = json!([{
        "name": "probe",
        "command": "/bin/sh",
        "args": ["-c", format!("touch {}; sleep 30", started.display())],
        "env": [],
    }]);

    let mut acp = Acp::spawn(&llm, READER_ROLE, &["--role", "reader"]);
    acp.initialize(None);
    acp.new_session(json!({"mcpServers": server.clone()}));
    thread::sleep(Duration::from_secs(2));
    assert!(
        !started.exists(),
        "the role can never call mcp__probe__*, so it never starts"
    );
    drop(acp);

    // Control: without the role the very same request does start it.
    let mut acp = Acp::spawn(&llm, "", &[]);
    acp.initialize(None);
    acp.new_session(json!({"mcpServers": server}));
    poll_until("the unrestricted session to start its MCP server", || {
        started.exists()
    });
}

#[test]
fn the_allowlist_ceiling_reaches_subagents() {
    // A restricted session cannot delegate its way out: the subagent tools
    // are not offered at all, so there is nothing to launch.
    let llm = MockLlm::start(|_, _| Reply::text("ok"));
    let mut acp = Acp::spawn(&llm, READER_ROLE, &["--role", "reader"]);
    acp.initialize(None);
    let sid = acp.new_session(json!({}));
    acp.prompt(&sid, "delegate");
    let tools = tool_names(&llm.requests()[0]);
    for forbidden in ["Task", "Agent", "AgentSwarm", "Skill", "SelectTools"] {
        assert!(
            !tools.iter().any(|name| name == forbidden),
            "{forbidden} in {tools:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Tool review: `yolo` runs without asking, but the host is told and can veto
// ---------------------------------------------------------------------------

/// Index of the message the agent is currently answering: the newest user
/// message that is not a harness reminder.
fn current_prompt_index(request: &Value) -> Option<usize> {
    request["messages"].as_array().and_then(|messages| {
        messages.iter().rposition(|message| {
            message["role"] == "user"
                && !message["content"]
                    .to_string()
                    .starts_with("\"<system-reminder>")
        })
    })
}

/// Text of the message the agent is currently answering.
fn current_prompt(request: &Value) -> String {
    current_prompt_index(request)
        .map(|index| request["messages"][index]["content"].to_string())
        .unwrap_or_default()
}

/// A tool result that answers the current prompt (an earlier turn's results
/// stay in the history and do not count).
fn fresh_tool_result(request: &Value) -> bool {
    let Some(index) = current_prompt_index(request) else {
        return false;
    };
    request["messages"].as_array().is_some_and(|messages| {
        messages[index + 1..]
            .iter()
            .any(|message| message["role"] == "tool")
    })
}

/// A model that writes `path` on every turn and then reports it is finished.
fn write_once(path: &'static str) -> impl Fn(&Value, usize) -> Reply + Send + Sync + 'static {
    move |request, _| {
        if fresh_tool_result(request) {
            Reply::text("finished")
        } else {
            Reply::tool(
                "call_w",
                "Write",
                json!({"path": path, "content": "written"}),
            )
        }
    }
}

fn yolo_with_review(mode: &str) -> Value {
    json!({"_meta": host_meta(json!({
        "permissionMode": "yolo",
        "toolReview": {"mode": mode},
    }))})
}

#[test]
fn a_host_can_veto_a_tool_call_that_yolo_would_wave_through() {
    let llm = MockLlm::start(write_once("veto-target.txt"));
    let mut acp = Acp::spawn(&llm, "", &[]);
    let init = acp.initialize(Some(host_meta(json!({"toolReview": true}))));
    assert_eq!(init["_meta"]["kkagent/host"]["toolReview"], true);

    let allow = Arc::new(AtomicBool::new(false));
    {
        let allow = Arc::clone(&allow);
        acp.on_agent_request(move |method, _| {
            (method == "_kkagent/tool/review").then(|| {
                if allow.load(Ordering::SeqCst) {
                    Ok(json!({"decision": "allow"}))
                } else {
                    Ok(json!({"decision": "deny", "reason": "HOST-SAYS-NO"}))
                }
            })
        });
    }
    let sid = acp.new_session(yolo_with_review("mutating"));
    let target = acp.workspace.join("veto-target.txt");

    // The host says no: nothing is written, the model is told why, and yolo
    // never asked anyone (the review is the only gate it has).
    assert_eq!(acp.prompt(&sid, "write it")["stopReason"], "end_turn");
    assert!(!target.exists(), "a vetoed call must not run");
    assert!(acp.requests_for("session/request_permission").is_empty());
    let reviews = acp.requests_for("_kkagent/tool/review");
    assert_eq!(reviews.len(), 1, "{reviews:?}");
    assert_eq!(reviews[0]["name"], "Write");
    assert_eq!(reviews[0]["sessionId"], json!(sid));
    assert_eq!(reviews[0]["toolCallId"], "call_w");
    assert_eq!(reviews[0]["readOnly"], false);
    assert_eq!(reviews[0]["input"]["content"], "written");
    assert_eq!(reviews[0]["cwd"], json!(acp.cwd()));
    let told = last_tool_result(llm.requests().last().unwrap());
    assert!(told.contains("HOST-SAYS-NO"), "{told}");
    assert!(
        acp.updates_of(&sid, "tool_call_update")
            .iter()
            .any(|update| update["status"] == "failed"),
        "{:?}",
        acp.updates(&sid)
    );

    // The host says yes: the same call now runs.
    allow.store(true, Ordering::SeqCst);
    assert_eq!(acp.prompt(&sid, "write it again")["stopReason"], "end_turn");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "written");
    assert_eq!(acp.requests_for("_kkagent/tool/review").len(), 2);
}

#[test]
fn review_modes_pick_which_calls_the_host_sees() {
    let llm = MockLlm::start(|request, _| {
        if fresh_tool_result(request) {
            Reply::text("read it")
        } else {
            Reply::tool("call_r", "Read", json!({"path": "AGENTS.md"}))
        }
    });
    let mut acp = Acp::spawn(&llm, "", &[]);
    acp.initialize(Some(host_meta(json!({"toolReview": true}))));
    acp.on_agent_request(|method, _| {
        (method == "_kkagent/tool/review").then(|| Ok(json!({"decision": "allow"})))
    });

    // `mutating` leaves a read-only tool alone…
    let mutating = acp.new_session(yolo_with_review("mutating"));
    assert_eq!(acp.prompt(&mutating, "read")["stopReason"], "end_turn");
    assert!(acp.requests_for("_kkagent/tool/review").is_empty());
    assert!(last_tool_result(llm.requests().last().unwrap()).contains(INSTRUCTIONS_MARK));

    // …`all` shows it to the host as read-only…
    let all = acp.new_session(yolo_with_review("all"));
    assert_eq!(acp.prompt(&all, "read")["stopReason"], "end_turn");
    let reviews = acp.requests_for("_kkagent/tool/review");
    assert_eq!(reviews.len(), 1, "{reviews:?}");
    assert_eq!(reviews[0]["name"], "Read");
    assert_eq!(reviews[0]["readOnly"], true);

    // …and a session that asked for no review is never bothered.
    let off = acp.new_session(json!({"_meta": host_meta(json!({"permissionMode": "yolo"}))}));
    assert_eq!(acp.prompt(&off, "read")["stopReason"], "end_turn");
    assert_eq!(acp.requests_for("_kkagent/tool/review").len(), 1);
}

#[test]
fn a_review_needs_the_capability_and_a_failing_host_means_no() {
    let llm = MockLlm::start(write_once("never-written.txt"));

    // Without `toolReview` at initialize the session is refused outright.
    let mut acp = Acp::spawn(&llm, "", &[]);
    acp.initialize(None);
    let refused = acp
        .call(
            "session/new",
            json!({"cwd": acp.cwd(), "_meta": host_meta(json!({"toolReview": {"mode": "all"}}))}),
        )
        .unwrap_err();
    assert!(
        refused["message"].as_str().unwrap().contains("toolReview"),
        "{refused}"
    );
    drop(acp);

    // A host that errors on the review denies the call (fail closed).
    let mut acp = Acp::spawn(&llm, "", &[]);
    acp.initialize(Some(host_meta(json!({"toolReview": true}))));
    acp.on_agent_request(|method, _| {
        (method == "_kkagent/tool/review").then(|| Err((-32000, "reviewer crashed".to_string())))
    });
    let sid = acp.new_session(yolo_with_review("all"));
    assert_eq!(acp.prompt(&sid, "write")["stopReason"], "end_turn");
    assert!(!acp.workspace.join("never-written.txt").exists());
    let told = last_tool_result(llm.requests().last().unwrap());
    assert!(told.contains("did not approve"), "{told}");
}

#[test]
fn the_review_follows_a_tool_call_into_a_subagent() {
    // The main model delegates; the subagent then tries to write. The host
    // lets the delegation through but vetoes the write, and the subagent
    // cannot dodge the review by being a different agent.
    let llm = MockLlm::start(|request, _| {
        let subagent = current_prompt(request).contains("SUBTASK");
        match (subagent, fresh_tool_result(request)) {
            (true, false) => Reply::tool(
                "sub_w",
                "Write",
                json!({"path": "sub-target.txt", "content": "from the subagent"}),
            ),
            (true, true) => Reply::text("subagent done"),
            (false, false) => Reply::tool(
                "call_agent",
                "Agent",
                json!({
                    "description": "write a file",
                    "prompt": "SUBTASK: write the file",
                    "profile": "general",
                }),
            ),
            (false, true) => Reply::text("main done"),
        }
    });
    let mut acp = Acp::spawn(&llm, "", &[]);
    acp.initialize(Some(host_meta(json!({"toolReview": true}))));
    let veto_writes = Arc::new(AtomicBool::new(true));
    {
        let veto_writes = Arc::clone(&veto_writes);
        acp.on_agent_request(move |method, params| {
            (method == "_kkagent/tool/review").then(|| {
                if params["name"] == "Write" && veto_writes.load(Ordering::SeqCst) {
                    Ok(json!({"decision": "deny", "reason": "SUBAGENT-WRITE-VETOED"}))
                } else {
                    Ok(json!({"decision": "allow"}))
                }
            })
        });
    }
    let sid = acp.new_session(yolo_with_review("all"));
    let target = acp.workspace.join("sub-target.txt");

    assert_eq!(acp.prompt(&sid, "delegate it")["stopReason"], "end_turn");
    let reviewed: Vec<String> = acp
        .requests_for("_kkagent/tool/review")
        .iter()
        .filter_map(|review| review["name"].as_str().map(str::to_string))
        .collect();
    assert!(reviewed.contains(&"Agent".to_string()), "{reviewed:?}");
    assert!(
        reviewed.contains(&"Write".to_string()),
        "the subagent's Write reached the host: {reviewed:?}\n{}",
        acp.logs()
    );
    assert!(!target.exists(), "the vetoed subagent write must not run");

    // Control: with the host allowing everything the subagent does write,
    // so the veto above is what stopped it.
    veto_writes.store(false, Ordering::SeqCst);
    assert_eq!(acp.prompt(&sid, "delegate it")["stopReason"], "end_turn");
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "from the subagent"
    );
}

#[test]
fn a_result_is_reported_in_both_content_and_raw_output() {
    let llm = MockLlm::start(|request, _| {
        if has_tool_result(request) {
            Reply::text("done")
        } else {
            Reply::tool("call_h", "host_echo", json!({"message": "hi"}))
        }
    });
    let mut acp = Acp::spawn(&llm, "", &[]);
    acp.initialize(Some(host_meta(json!({"tools": true}))));
    acp.on_agent_request(|method, params| {
        (method == "_kkagent/tool/call")
            .then(|| Ok(json!({"content": format!("echo:{}", params["input"]["message"])})))
    });
    let sid = acp.new_session(json!({"_meta": host_meta(json!({
        "hostTools": [{"name": "host_echo", "readOnly": true}]
    }))}));
    acp.prompt(&sid, "echo");
    let finished: Vec<_> = acp
        .updates_of(&sid, "tool_call_update")
        .into_iter()
        .filter(|update| update["status"] == "completed")
        .collect();
    assert_eq!(finished.len(), 1);
    let text = finished[0]["content"][0]["content"]["text"]
        .as_str()
        .unwrap();
    assert_eq!(text, "echo:\"hi\"");
    assert_eq!(
        finished[0]["rawOutput"], text,
        "one result, two identical places"
    );
    // A call still running carries no result at all.
    let running: Vec<_> = acp
        .updates_of(&sid, "tool_call_update")
        .into_iter()
        .filter(|update| update["status"] == "in_progress")
        .collect();
    assert!(running
        .iter()
        .all(|update| update.get("rawOutput").is_none()));
}

#[test]
fn session_load_can_skip_replaying_history_for_a_host_that_keeps_its_own() {
    let llm = MockLlm::start(|_, _| Reply::text("remembered"));
    let mut acp = Acp::spawn(&llm, "", &[]);
    acp.initialize(None);
    let sid = acp.new_session(json!({}));
    acp.prompt(&sid, "first question");
    let cwd = acp.cwd();

    let history_chunks = |acp: &Acp, from: usize| {
        acp.notifications[from..]
            .iter()
            .filter(|n| n["method"] == "session/update")
            .filter(|n| {
                matches!(
                    n["params"]["update"]["sessionUpdate"].as_str(),
                    Some("user_message_chunk" | "agent_message_chunk")
                )
            })
            .count()
    };

    // `replay: false`: the session is loaded, but no history is sent.
    acp.ok("session/close", json!({"sessionId": sid}));
    let mark = acp.notifications.len();
    acp.ok(
        "session/load",
        json!({
            "sessionId": sid, "cwd": cwd, "mcpServers": [],
            "_meta": host_meta(json!({"replay": false})),
        }),
    );
    assert_eq!(history_chunks(&acp, mark), 0, "no history is replayed");
    assert_eq!(
        acp.prompt(&sid, "and now?")["stopReason"],
        "end_turn",
        "the loaded session works"
    );

    // Default: the stored conversation is replayed.
    acp.ok("session/close", json!({"sessionId": sid}));
    let mark = acp.notifications.len();
    acp.ok(
        "session/load",
        json!({"sessionId": sid, "cwd": cwd, "mcpServers": []}),
    );
    assert!(
        history_chunks(&acp, mark) >= 2,
        "history is replayed by default"
    );
}
