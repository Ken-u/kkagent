//! Role allowlist enforcement inside the agent loop: calls to tools outside
//! the allowlist never execute, including the name-based interactive tools
//! that bypass `ToolRegistry::get`.

use super::*;
use kkagent_config::{LoopControlConfig, ModelConfig, ProviderConfig};
use kkagent_protocol::{PermissionMode, ToolAllowlist};
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn tool_call_sse(id: &str, name: &str, arguments: &str) -> String {
    let arguments = serde_json::to_string(arguments).unwrap();
    format!(
        "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"{id}\",\"function\":{{\"name\":\"{name}\",\"arguments\":{arguments}}}}}]}}}}]}}\n\
         data: [DONE]\n"
    )
}

fn text_sse(text: &str) -> String {
    format!(
        "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{text}\"}}}}]}}\n\
         data: [DONE]\n"
    )
}

/// Serve `bodies` one per connection and return every request body seen.
async fn spawn_mock_llm(bodies: Vec<String>) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move {
        let mut seen = Vec::new();
        for body in bodies {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut chunk = [0_u8; 16_384];
                let read = socket.read(&mut chunk).await.unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
                let Some(header_end) = request.windows(4).position(|w| w == b"\r\n\r\n") else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_len = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|value| value.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if request.len() >= header_end + 4 + content_len {
                    break;
                }
            }
            seen.push(String::from_utf8_lossy(&request).into_owned());
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        }
        seen
    });
    (base_url, handle)
}

fn test_config(base_url: String) -> AppConfig {
    let mut config = AppConfig {
        default_model: Some("test/model".into()),
        loop_control: Some(LoopControlConfig {
            max_attempts_per_step: 1,
            reserved_context_size: 1_000,
            max_steps_per_turn: 8,
            auto_compact: false,
            compact_keep_last: 4,
            token_counting: "estimated".into(),
            ..Default::default()
        }),
        ..AppConfig::default()
    };
    config.providers.insert(
        "test".into(),
        ProviderConfig {
            provider_type: "openai-chat".into(),
            api_key: Some("token".into()),
            api_key_env: None,
            base_url: Some(base_url),
            custom_headers: HashMap::new(),
            oauth: None,
            first_token_timeout_ms: None,
            request_timeout_ms: None,
            read_timeout_ms: None,
            extra_fields: Default::default(),
        },
    );
    config.models.insert(
        "test/model".into(),
        ModelConfig {
            provider: "test".into(),
            model: "test-model".into(),
            max_context_size: Some(1_000_000),
            max_output_size: Some(1_000),
            capabilities: vec!["tool_use".into()],
            display_name: None,
            support_efforts: Vec::new(),
            default_effort: None,
            pricing: None,
            experimental_adaptive_thinking: false,
            experimental_vision_proxy: false,
            experimental_visible_empty_retries: 0,
            experimental_bad_toolcall_auto_retries: 0,
            experimental_retry_partial_stream_errors: None,
            first_token_timeout_ms: None,
        },
    );
    config
}

struct ProbeTool {
    name: &'static str,
    ran: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl kkagent_tools::Tool for ProbeTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "records that it ran"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    async fn execute(
        &self,
        _input: serde_json::Value,
        _ctx: &kkagent_tools::ToolContext,
    ) -> anyhow::Result<ToolOutput> {
        self.ran.store(true, Ordering::SeqCst);
        Ok(ToolOutput::success("probe ran"))
    }
}

fn tool_result_texts(session: &Session) -> Vec<String> {
    session
        .messages
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(|content| match content {
            ChatContent::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        })
        .collect()
}

fn workspace(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "kkagent-allowlist-{label}-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disallowed_tools_are_hidden_and_calls_to_them_never_execute() {
    let (base_url, server) = spawn_mock_llm(vec![
        // The model hallucinates a call to a tool outside the allowlist, and
        // to the interactive AskUserQuestion special case.
        tool_call_sse("c1", "AskUserQuestion", "{}"),
        tool_call_sse("c2", "Secret", "{}"),
        text_sse("done"),
    ])
    .await;

    let allowed_ran = Arc::new(AtomicBool::new(false));
    let secret_ran = Arc::new(AtomicBool::new(false));
    let mut tools = ToolRegistry::new();
    tools.set_allowlist(&ToolAllowlist::new(["Allowed"]));
    tools.register(Arc::new(ProbeTool {
        name: "Allowed",
        ran: allowed_ran.clone(),
    }));
    tools.register(Arc::new(ProbeTool {
        name: "Secret",
        ran: secret_ran.clone(),
    }));
    kkagent_tools::register_builtin_tools(&mut tools);
    assert_eq!(
        tools
            .tool_definitions()
            .into_iter()
            .map(|d| d.name)
            .collect::<Vec<_>>(),
        vec!["Allowed"]
    );

    let (event_tx, mut event_rx) = mpsc::channel(512);
    let loop_ = AgentLoop::new(
        Arc::new(test_config(base_url)),
        Arc::new(tools),
        Arc::new(Mutex::new(PermissionChain::new(
            PermissionMode::Auto,
            Vec::new(),
        ))),
        event_tx,
        Arc::new(Mutex::new(HashMap::new())),
    );
    let dir = workspace("hidden");
    let mut session = Session::new(
        "allowlist-hidden".into(),
        dir.clone(),
        PermissionMode::Auto,
        "test/model".into(),
    );
    session.add_user_message("go".into());

    loop_.run_turn(&mut session).await.unwrap();
    drop(loop_);
    let requests = server.await.unwrap();

    assert!(!secret_ran.load(Ordering::SeqCst));
    assert!(!allowed_ran.load(Ordering::SeqCst));
    let results = tool_result_texts(&session);
    assert_eq!(results.len(), 2, "{results:?}");
    assert!(
        results.iter().all(|text| text.contains("not permitted")),
        "{results:?}"
    );

    // No interactive question was ever raised.
    let mut asked = false;
    while let Ok(event) = event_rx.try_recv() {
        asked |= matches!(event, AgentEvent::QuestionAsked { .. });
    }
    assert!(!asked, "AskUserQuestion must not run outside the allowlist");

    // The wire request only advertised the allowed tool.
    let first = &requests[0];
    assert!(first.contains("\"name\":\"Allowed\""), "{first}");
    for hidden in ["Secret", "AskUserQuestion", "Bash", "Write", "SelectTools"] {
        assert!(
            !first.contains(&format!("\"name\":\"{hidden}\"")),
            "{hidden} leaked into tools[]"
        );
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn allowed_tools_still_execute_under_an_allowlist() {
    let (base_url, server) = spawn_mock_llm(vec![
        tool_call_sse("c1", "Allowed", "{}"),
        text_sse("finished"),
    ])
    .await;
    let ran = Arc::new(AtomicBool::new(false));
    let mut tools = ToolRegistry::new();
    tools.set_allowlist(&ToolAllowlist::new(["Allowed"]));
    tools.register(Arc::new(ProbeTool {
        name: "Allowed",
        ran: ran.clone(),
    }));
    let (event_tx, _event_rx) = mpsc::channel(512);
    let loop_ = AgentLoop::new(
        Arc::new(test_config(base_url)),
        Arc::new(tools),
        Arc::new(Mutex::new(PermissionChain::new(
            PermissionMode::Auto,
            Vec::new(),
        ))),
        event_tx,
        Arc::new(Mutex::new(HashMap::new())),
    );
    let dir = workspace("allowed");
    let mut session = Session::new(
        "allowlist-allowed".into(),
        dir.clone(),
        PermissionMode::Auto,
        "test/model".into(),
    );
    session.add_user_message("go".into());
    loop_.run_turn(&mut session).await.unwrap();
    drop(loop_);
    server.await.unwrap();
    assert!(ran.load(Ordering::SeqCst));
    assert_eq!(tool_result_texts(&session), vec!["probe ran".to_string()]);
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn execute_tool_parallel_refuses_names_outside_the_allowlist() {
    let mut registry = ToolRegistry::new();
    registry.set_allowlist(&ToolAllowlist::new(["Allowed"]));
    let tools = Arc::new(registry);
    let output = execute_tool_parallel(ParallelToolRequest {
        tools,
        hooks: None,
        working_dir: std::env::temp_dir(),
        session_id: "s".into(),
        turn_id: "s:1".into(),
        image: kkagent_config::ImageConfig::default(),
        name: "Bash".into(),
        input: serde_json::json!({"command": "echo hi"}),
        tool_call_id: None,
        interrupted: Arc::new(AtomicBool::new(false)),
        plan_file_path: std::path::PathBuf::new(),
        tools_config: kkagent_config::ToolsConfig::default(),
        model_alias: None,
    })
    .await;
    assert!(output.is_error);
    assert!(
        output.content.contains("not permitted"),
        "{}",
        output.content
    );
}

#[tokio::test]
async fn acp_model_alias_is_refused_under_an_allowlist() {
    let mut registry = ToolRegistry::new();
    registry.set_allowlist(&ToolAllowlist::new(["Read"]));
    let (event_tx, mut event_rx) = mpsc::channel(64);
    let loop_ = AgentLoop::new(
        Arc::new(AppConfig::default()),
        Arc::new(registry),
        Arc::new(Mutex::new(PermissionChain::new(
            PermissionMode::Auto,
            Vec::new(),
        ))),
        event_tx,
        Arc::new(Mutex::new(HashMap::new())),
    );
    let dir = workspace("acp-alias");
    let mut session = Session::for_subagent(
        "allowlist-acp-alias".into(),
        dir.clone(),
        PermissionMode::Auto,
        "acp:some-plugin.agent".into(),
    );
    session.add_user_message("go".into());
    loop_.run_turn(&mut session).await.unwrap();
    drop(loop_);

    let mut message = None;
    while let Ok(event) = event_rx.try_recv() {
        if let AgentEvent::Error { message: m, .. } = event {
            message = Some(m);
        }
    }
    let message = message.expect("an error event explaining the refusal");
    assert!(message.contains("strict tool allowlist"), "{message}");
    std::fs::remove_dir_all(dir).unwrap();
}

// ---------------------------------------------------------------------------
// Host review gate
// ---------------------------------------------------------------------------

struct ScriptedGate {
    verdict: kkagent_protocol::GateVerdict,
    only_mutating: bool,
    seen: std::sync::Mutex<Vec<kkagent_protocol::ToolCallReview>>,
}

impl ScriptedGate {
    fn new(verdict: kkagent_protocol::GateVerdict, only_mutating: bool) -> Arc<Self> {
        Arc::new(Self {
            verdict,
            only_mutating,
            seen: std::sync::Mutex::new(Vec::new()),
        })
    }
}

impl kkagent_protocol::ToolGate for ScriptedGate {
    fn reviews(&self, _name: &str, read_only: bool) -> bool {
        !(self.only_mutating && read_only)
    }

    fn review<'a>(
        &'a self,
        call: &'a kkagent_protocol::ToolCallReview,
    ) -> kkagent_protocol::GateFuture<'a> {
        self.seen.lock().unwrap().push(call.clone());
        let verdict = self.verdict.clone();
        Box::pin(async move { verdict })
    }
}

/// A read-only probe, to tell reviewed calls from unreviewed ones.
struct ReadOnlyProbe {
    ran: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl kkagent_tools::Tool for ReadOnlyProbe {
    fn name(&self) -> &str {
        "Peek"
    }
    fn description(&self) -> &str {
        "read-only probe"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    fn read_only(&self) -> bool {
        true
    }
    async fn execute(
        &self,
        _input: serde_json::Value,
        _ctx: &kkagent_tools::ToolContext,
    ) -> anyhow::Result<ToolOutput> {
        self.ran.store(true, Ordering::SeqCst);
        Ok(ToolOutput::success("peek ran"))
    }
}

fn request_for(tools: Arc<ToolRegistry>, name: &str) -> ParallelToolRequest {
    ParallelToolRequest {
        tools,
        hooks: None,
        working_dir: std::env::temp_dir(),
        session_id: "gate-session".into(),
        turn_id: "gate-session:1".into(),
        image: kkagent_config::ImageConfig::default(),
        name: name.into(),
        input: serde_json::json!({}),
        tool_call_id: Some("call-7".into()),
        interrupted: Arc::new(AtomicBool::new(false)),
        plan_file_path: std::path::PathBuf::new(),
        tools_config: kkagent_config::ToolsConfig::default(),
        model_alias: None,
    }
}

#[tokio::test]
async fn a_denying_gate_stops_execution_and_tells_the_model_why() {
    let ran = Arc::new(AtomicBool::new(false));
    let gate = ScriptedGate::new(
        kkagent_protocol::GateVerdict::Deny("host says no".into()),
        false,
    );
    let mut registry = ToolRegistry::new();
    registry.set_gate(gate.clone());
    registry.register(Arc::new(ProbeTool {
        name: "Probe",
        ran: ran.clone(),
    }));
    let output = execute_tool_parallel(request_for(Arc::new(registry), "Probe")).await;
    assert!(output.is_error);
    assert_eq!(output.content, "host says no");
    assert!(!ran.load(Ordering::SeqCst));

    let seen = gate.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].name, "Probe");
    assert_eq!(seen[0].session_id, "gate-session");
    assert_eq!(seen[0].tool_call_id, "call-7");
    assert!(!seen[0].read_only);
}

#[tokio::test]
async fn an_allowing_gate_reviews_first_and_then_the_tool_runs() {
    let ran = Arc::new(AtomicBool::new(false));
    let gate = ScriptedGate::new(kkagent_protocol::GateVerdict::Allow, false);
    let mut registry = ToolRegistry::new();
    registry.set_gate(gate.clone());
    registry.register(Arc::new(ProbeTool {
        name: "Probe",
        ran: ran.clone(),
    }));
    let output = execute_tool_parallel(request_for(Arc::new(registry), "Probe")).await;
    assert!(!output.is_error, "{}", output.content);
    assert!(ran.load(Ordering::SeqCst));
    assert_eq!(gate.seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn calls_the_gate_does_not_review_run_without_asking() {
    let ran = Arc::new(AtomicBool::new(false));
    // Would deny everything it reviews, but only mutating tools are reviewed.
    let gate = ScriptedGate::new(kkagent_protocol::GateVerdict::Deny("no".into()), true);
    let mut registry = ToolRegistry::new();
    registry.set_gate(gate.clone());
    registry.register(Arc::new(ReadOnlyProbe { ran: ran.clone() }));
    let output = execute_tool_parallel(request_for(Arc::new(registry), "Peek")).await;
    assert!(!output.is_error, "{}", output.content);
    assert!(ran.load(Ordering::SeqCst));
    assert!(gate.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn the_gate_cannot_widen_the_allowlist() {
    // Allowed by the gate but outside the allowlist: still refused, and the
    // host is not even asked.
    let gate = ScriptedGate::new(kkagent_protocol::GateVerdict::Allow, false);
    let mut registry = ToolRegistry::new();
    registry.set_allowlist(&ToolAllowlist::new(["Allowed"]));
    registry.set_gate(gate.clone());
    let output = execute_tool_parallel(request_for(Arc::new(registry), "Bash")).await;
    assert!(output.is_error);
    assert!(output.content.contains("not permitted"));
    assert!(gate.seen.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_review_overrules_yolo_mode_inside_a_whole_turn() {
    let (base_url, server) = spawn_mock_llm(vec![
        tool_call_sse("c1", "Probe", "{}"),
        text_sse("understood"),
    ])
    .await;
    let ran = Arc::new(AtomicBool::new(false));
    let gate = ScriptedGate::new(
        kkagent_protocol::GateVerdict::Deny("not on a Friday".into()),
        false,
    );
    let mut tools = ToolRegistry::new();
    tools.set_gate(gate.clone());
    tools.register(Arc::new(ProbeTool {
        name: "Probe",
        ran: ran.clone(),
    }));
    let (event_tx, _event_rx) = mpsc::channel(512);
    let loop_ = AgentLoop::new(
        Arc::new(test_config(base_url)),
        Arc::new(tools),
        Arc::new(Mutex::new(PermissionChain::new(
            PermissionMode::Yolo,
            Vec::new(),
        ))),
        event_tx,
        Arc::new(Mutex::new(HashMap::new())),
    );
    let dir = workspace("yolo-review");
    let mut session = Session::new(
        "gate-yolo".into(),
        dir.clone(),
        PermissionMode::Yolo,
        "test/model".into(),
    );
    session.add_user_message("go".into());
    loop_.run_turn(&mut session).await.unwrap();
    drop(loop_);
    server.await.unwrap();

    assert!(!ran.load(Ordering::SeqCst), "yolo must not skip the review");
    assert_eq!(
        tool_result_texts(&session),
        vec!["not on a Friday".to_string()]
    );
    assert_eq!(gate.seen.lock().unwrap().len(), 1);
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acp_model_alias_is_refused_under_a_host_review() {
    let mut registry = ToolRegistry::new();
    registry.set_gate(ScriptedGate::new(
        kkagent_protocol::GateVerdict::Allow,
        false,
    ));
    let (event_tx, mut event_rx) = mpsc::channel(64);
    let loop_ = AgentLoop::new(
        Arc::new(AppConfig::default()),
        Arc::new(registry),
        Arc::new(Mutex::new(PermissionChain::new(
            PermissionMode::Yolo,
            Vec::new(),
        ))),
        event_tx,
        Arc::new(Mutex::new(HashMap::new())),
    );
    let dir = workspace("acp-alias-review");
    let mut session = Session::for_subagent(
        "gate-acp-alias".into(),
        dir.clone(),
        PermissionMode::Yolo,
        "acp:some-plugin.agent".into(),
    );
    session.add_user_message("go".into());
    loop_.run_turn(&mut session).await.unwrap();
    drop(loop_);

    let mut message = None;
    while let Ok(event) = event_rx.try_recv() {
        if let AgentEvent::Error { message: m, .. } = event {
            message = Some(m);
        }
    }
    let message = message.expect("an error event explaining the refusal");
    assert!(message.contains("host tool review"), "{message}");
    std::fs::remove_dir_all(dir).unwrap();
}

// ---------------------------------------------------------------------------
// Approval ids and waiting
// ---------------------------------------------------------------------------

fn plain_loop(namespace: Option<&str>) -> AgentLoop {
    let (event_tx, _event_rx) = mpsc::channel(8);
    let loop_ = AgentLoop::new(
        Arc::new(AppConfig::default()),
        Arc::new(ToolRegistry::new()),
        Arc::new(Mutex::new(PermissionChain::new(
            PermissionMode::Manual,
            Vec::new(),
        ))),
        event_tx,
        Arc::new(Mutex::new(HashMap::new())),
    );
    match namespace {
        Some(namespace) => loop_.with_approval_namespace(namespace),
        None => loop_,
    }
}

#[test]
fn approval_ids_repeat_for_the_same_task_and_never_collide_within_it() {
    let input = serde_json::json!({"command": "make deploy"});
    let first_run = plain_loop(Some("task-1"));
    let a1 = first_run.next_approval_id("Bash", &input);
    let a2 = first_run.next_approval_id("Bash", &input);
    assert_ne!(
        a1, a2,
        "two identical asks in one turn stay distinguishable"
    );

    // A restart re-runs the same task: the same sequence gets the same ids.
    let second_run = plain_loop(Some("task-1"));
    assert_eq!(second_run.next_approval_id("Bash", &input), a1);
    assert_eq!(second_run.next_approval_id("Bash", &input), a2);

    // Another task, another tool or other input never reuses an id.
    assert_ne!(
        plain_loop(Some("task-2")).next_approval_id("Bash", &input),
        a1
    );
    assert_ne!(
        plain_loop(Some("task-1")).next_approval_id("Write", &input),
        a1
    );
    assert_ne!(
        plain_loop(Some("task-1")).next_approval_id("Bash", &serde_json::json!({"command": "ls"})),
        a1
    );
    assert!(a1.starts_with("apr_"));
}

#[test]
fn without_a_namespace_approval_ids_are_random() {
    let loop_ = plain_loop(None);
    let input = serde_json::json!({});
    assert_ne!(
        loop_.next_approval_id("Bash", &input),
        loop_.next_approval_id("Bash", &input)
    );
}

#[tokio::test]
async fn waiting_gives_up_only_when_a_limit_is_set() {
    let never = std::future::pending::<u8>();
    assert!(wait_within(Some(0), never).await.is_err());
    assert_eq!(wait_within(Some(5), async { 7_u8 }).await.unwrap(), 7);
    // `None` is "no limit": a slow answer still arrives.
    let slow = async {
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        9_u8
    };
    assert_eq!(wait_within(None, slow).await.unwrap(), 9);
}

// ---------------------------------------------------------------------------
// Dynamic tool loading under a fixed allowlist
// ---------------------------------------------------------------------------

struct LazyProbe;

#[async_trait::async_trait]
impl kkagent_tools::Tool for LazyProbe {
    fn name(&self) -> &str {
        "Lazy"
    }
    fn description(&self) -> &str {
        "a deferred tool"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    fn disclosure(&self) -> kkagent_protocol::tools::ToolDisclosure {
        kkagent_protocol::tools::ToolDisclosure::Deferred
    }
    async fn execute(
        &self,
        _input: serde_json::Value,
        _ctx: &kkagent_tools::ToolContext,
    ) -> anyhow::Result<ToolOutput> {
        Ok(ToolOutput::success("lazy ran"))
    }
}

/// Run one turn and report whether the deferred tool's schema was sent with
/// the first request.
async fn deferred_tool_is_sent_inline(
    allowlist: Option<ToolAllowlist>,
    explicit: Option<bool>,
) -> bool {
    let (base_url, server) = spawn_mock_llm(vec![text_sse("ok")]).await;
    let mut config = test_config(base_url);
    config
        .models
        .get_mut("test/model")
        .unwrap()
        .capabilities
        .push("dynamically_loaded_tools".into());
    config.tools.dynamically_loaded_tools = explicit;
    let mut tools = ToolRegistry::new();
    if let Some(allowlist) = &allowlist {
        tools.set_allowlist(allowlist);
    }
    tools.register(Arc::new(LazyProbe));
    kkagent_tools::register_builtin_tools(&mut tools);

    let (event_tx, _event_rx) = mpsc::channel(512);
    let loop_ = AgentLoop::new(
        Arc::new(config),
        Arc::new(tools),
        Arc::new(Mutex::new(PermissionChain::new(
            PermissionMode::Auto,
            Vec::new(),
        ))),
        event_tx,
        Arc::new(Mutex::new(HashMap::new())),
    );
    let dir = workspace("dynamic");
    let mut session = Session::new(
        format!("dynamic-{}", uuid::Uuid::new_v4()),
        dir.clone(),
        PermissionMode::Auto,
        "test/model".into(),
    );
    session.add_user_message("go".into());
    loop_.run_turn(&mut session).await.unwrap();
    drop(loop_);
    let requests = server.await.unwrap();
    std::fs::remove_dir_all(dir).unwrap();
    requests[0].contains("\"name\":\"Lazy\"")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fixed_allowlist_loads_its_tools_up_front() {
    // Unrestricted: the deferred tool waits behind SelectTools.
    assert!(!deferred_tool_is_sent_inline(None, None).await);
    // A strict allowlist fixes the set, so nothing is deferred.
    assert!(deferred_tool_is_sent_inline(Some(ToolAllowlist::new(["Lazy"])), None).await);
    // An explicit setting still wins in both directions.
    assert!(
        !deferred_tool_is_sent_inline(
            Some(ToolAllowlist::new(["Lazy", "SelectTools"])),
            Some(true)
        )
        .await
    );
    assert!(deferred_tool_is_sent_inline(None, Some(false)).await);
}
