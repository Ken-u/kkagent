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
