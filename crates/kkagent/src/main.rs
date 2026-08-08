use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::collections::HashMap;
use clap::{Parser, Subcommand};
use tokio::sync::{mpsc, Mutex};
use tokio::task::AbortHandle;
use anyhow::Result;

use kkagent_config::{load_config, AppConfig};
use kkagent_protocol::{Frame, PermissionMode, AgentEvent};
use kkagent_protocol::subagent::SubagentManager;
use kkagent_rpc::{RpcClient, RpcServer, transport::memory::create_memory_pair};
use kkagent_llm::{ChatMessage, ChatContent};
use kkagent_core::{AgentLoop, PermissionChain, Session, TranscriptDb};
use kkagent_tools::ToolRegistry;
use kkagent_mcp::{McpManager, register_mcp_tools};
use kkagent_client::KkagentClient;
use kkagent_tui::TuiApp;

#[derive(Parser)]
#[command(name = "kkagent", version, about = "AI coding agent for your terminal")]
struct Cli {
    /// Path to config file
    #[arg(long)]
    config: Option<PathBuf>,

    /// Auto-approve regular tool calls (YOLO mode)
    #[arg(long, short = 'y', conflicts_with = "auto")]
    yolo: bool,

    /// Fully autonomous mode
    #[arg(long, conflicts_with = "yolo")]
    auto: bool,

    /// Start in Plan mode
    #[arg(long)]
    plan: bool,

    /// Non-interactive prompt mode
    #[arg(short = 'p', long)]
    prompt: Option<String>,

    /// Resume an existing session by id (or prefix)
    #[arg(long)]
    resume: Option<String>,

    /// Connect to an existing server
    #[arg(long)]
    connect: Option<String>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Run as standalone server
    Server {
        /// Socket path to listen on
        #[arg(long)]
        listen: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let is_tui = cli.command.is_none() && cli.prompt.is_none();
    init_logging(is_tui)?;

    let config = load_config(cli.config.as_deref())?;

    let permission_mode = if cli.auto {
        PermissionMode::Auto
    } else if cli.yolo {
        PermissionMode::Yolo
    } else {
        config.effective_permission_mode().parse().unwrap_or(PermissionMode::Manual)
    };

    match cli.command {
        Some(Commands::Server { listen }) => {
            run_server(config, listen).await
        }
        None => {
            if let Some(prompt) = cli.prompt {
                run_print_mode(config, prompt, permission_mode).await
            } else {
                run_tui(config, permission_mode, cli.plan, cli.resume).await
            }
        }
    }
}

/// TUI 模式下日志写到文件，避免污染 alternate screen；
/// print / server 模式仍输出到 stderr。
fn init_logging(tui_mode: bool) -> Result<()> {
    let filter = tracing_subscriber::EnvFilter::from_default_env()
        .add_directive("kkagent=info".parse().unwrap());

    if tui_mode {
        let dir = kkagent_config::default_config_dir();
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("kkagent.log");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(file))
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .init();
    }
    Ok(())
}

async fn run_tui(
    config: AppConfig,
    permission_mode: PermissionMode,
    plan_mode: bool,
    resume: Option<String>,
) -> Result<()> {
    // Default TUI: 1:1 in-process pair via memory duplex.
    // This process owns both ends — quitting the TUI aborts the paired server
    // task so a subsequent `kkagent` never talks to a leftover in-process agent.
    // Standalone `kkagent server` (UDS) is a separate process with its own lifetime;
    // only that mode can outlive a TUI, and only when you explicitly start it.
    let (client_stream, server_stream) = create_memory_pair();

    let (event_tx, event_rx) = mpsc::channel::<Frame>(256);
    let rpc_client = RpcClient::new(client_stream, event_tx);

    let server_config = Arc::new(config.clone());
    let server_handle = tokio::spawn(async move {
        run_server_handler(server_stream, server_config).await;
    });
    // Let the server task bind the memory transport before the first RPC.
    tokio::task::yield_now().await;

    let mut tui_config = config;
    tui_config.default_permission_mode = Some(permission_mode.to_string());
    tui_config.default_plan_mode = plan_mode;

    let client = KkagentClient::new(rpc_client, event_rx);
    let app = TuiApp::new(tui_config, client);
    let result = app.run(resume).await;

    // Drop the paired server (and any in-flight agent/LLM tasks it owns).
    server_handle.abort();
    let _ = server_handle.await;

    result
}

async fn run_print_mode(config: AppConfig, prompt: String, permission_mode: PermissionMode) -> Result<()> {
    let (client_stream, server_stream) = create_memory_pair();
    let (event_tx, event_rx) = mpsc::channel::<Frame>(256);
    let rpc_client = RpcClient::new(client_stream, event_tx);

    let config_arc = Arc::new(config.clone());
    tokio::spawn(async move {
        run_server_handler(server_stream, config_arc).await;
    });

    let mut client = KkagentClient::new(rpc_client, event_rx);
    let cwd = std::env::current_dir()?.to_string_lossy().to_string();
    let session_id = client.create_session(Some(&cwd), Some(permission_mode)).await?;
    client.send_prompt(&session_id, &prompt).await?;

    while let Some(frame) = client.event_rx.recv().await {
        if let Frame::Event { data, .. } = frame {
            if let Ok(evt) = serde_json::from_value::<AgentEvent>(data) {
                match evt {
                    AgentEvent::MessageDelta { text, .. } => {
                        print!("{}", text);
                    }
                    AgentEvent::TurnEnd { .. } => {
                        println!();
                        break;
                    }
                    AgentEvent::Error { message, .. } => {
                        eprintln!("Error: {}", message);
                        break;
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

async fn run_server(config: AppConfig, listen: Option<String>) -> Result<()> {
    let socket_path = listen.unwrap_or_else(|| {
        let dir = kkagent_config::default_config_dir();
        dir.join("server.sock").to_string_lossy().to_string()
    });

    tracing::info!("Starting server on {}", socket_path);
    let listener = kkagent_rpc::transport::uds::bind_uds(std::path::Path::new(&socket_path))?;
    let config_arc = Arc::new(config);

    loop {
        let (stream, _) = listener.accept().await?;
        let cfg = config_arc.clone();
        tokio::spawn(async move {
            run_server_handler(stream, cfg).await;
        });
    }
}

struct ServerState {
    config: Arc<AppConfig>,
    sessions: Mutex<HashMap<String, Session>>,
    /// Session approval senders — must not require holding `sessions` lock
    /// (agent loop may be waiting on approval while holding the session).
    approval_txs: Mutex<HashMap<String, mpsc::Sender<kkagent_protocol::ApprovalResponse>>>,
    question_txs: Mutex<HashMap<String, mpsc::Sender<kkagent_protocol::QuestionResponse>>>,
    /// Interrupt flags remain reachable while the session is out of `sessions` during a turn.
    interrupt_flags: Mutex<HashMap<String, Arc<AtomicBool>>>,
    /// Model alias handles — reachable mid-turn when session is removed from `sessions`.
    model_aliases: Mutex<HashMap<String, Arc<std::sync::Mutex<String>>>>,
    abort_registry: Arc<Mutex<HashMap<String, AbortHandle>>>,
    transcript: Mutex<TranscriptDb>,
    subagents: Arc<SubagentManager>,
    /// Connected MCP servers; tools registered per turn from this manager.
    mcp: Arc<McpManager>,
    /// Shared background shell jobs for Bash tool.
    bash_shells: Arc<kkagent_tools::builtin::BackgroundShellManager>,
    cron: Arc<kkagent_tools::CronManager>,
    hooks: Arc<kkagent_mcp::HookManager>,
    skills: Arc<kkagent_tools::SkillCatalog>,
    web: Arc<kkagent_tools::WebServicesConfig>,
}

fn mcp_manager_from_config(config: &AppConfig) -> McpManager {
    let configs: Vec<kkagent_mcp::McpServerConfig> = config
        .mcp_servers
        .iter()
        .map(|(name, cfg)| kkagent_mcp::McpServerConfig {
            name: name.clone(),
            command: cfg.command.clone(),
            args: cfg.args.clone(),
            env: cfg.env.clone(),
        })
        .collect();
    McpManager::new(configs)
}

async fn run_server_handler<T: kkagent_rpc::transport::AsyncTransport>(
    transport: T,
    config: Arc<AppConfig>,
) {
    let transcript = match TranscriptDb::open_default() {
        Ok(db) => db,
        Err(e) => {
            tracing::error!("Failed to open transcript DB: {} — using in-memory", e);
            match TranscriptDb::open_in_memory() {
                Ok(db) => db,
                Err(e2) => {
                    tracing::error!("Failed to open in-memory transcript: {}", e2);
                    // Last resort: still try file path under temp
                    let tmp = std::env::temp_dir().join("kkagent-transcripts.db");
                    TranscriptDb::open(&tmp).unwrap_or_else(|e3| {
                        panic!("Cannot open transcript DB (file={}, mem={}, tmp={})", e, e2, e3);
                    })
                }
            }
        }
    };

    let mcp = Arc::new(mcp_manager_from_config(&config));
    if !config.mcp_servers.is_empty() {
        if let Err(e) = mcp.connect_all().await {
            tracing::warn!("MCP connect_all error: {}", e);
        } else {
            let n = mcp.list_tools().await.len();
            tracing::info!(
                "MCP ready: {} server(s), {} tool(s)",
                config.mcp_servers.len(),
                n
            );
        }
    }

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut hooks = kkagent_mcp::HookManager::new(&cwd);
    hooks.load_from_app_config(&config.hooks).await;
    let _ = hooks.discover().await;
    let skills = Arc::new(kkagent_tools::SkillCatalog::discover(&cwd).await);
    let cron = Arc::new(kkagent_tools::CronManager::new());
    let web = Arc::new(kkagent_tools::WebServicesConfig::from_app(&config));

    // Background cron poller — fires due prompts into a dedicated channel log for now.
    {
        let cron_bg = cron.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(15)).await;
                let due = cron_bg.take_due().await;
                for (id, prompt) in due {
                    tracing::info!("Cron job {} due: {}", id, prompt.chars().take(80).collect::<String>());
                }
            }
        });
    }

    let state = Arc::new(ServerState {
        config: config.clone(),
        sessions: Mutex::new(HashMap::new()),
        approval_txs: Mutex::new(HashMap::new()),
        question_txs: Mutex::new(HashMap::new()),
        interrupt_flags: Mutex::new(HashMap::new()),
        model_aliases: Mutex::new(HashMap::new()),
        abort_registry: Arc::new(Mutex::new(HashMap::new())),
        transcript: Mutex::new(transcript),
        subagents: Arc::new(SubagentManager::new(4)),
        mcp,
        bash_shells: Arc::new(kkagent_tools::builtin::BackgroundShellManager::new()),
        cron,
        hooks: Arc::new(hooks),
        skills,
        web,
    });

    let handler: kkagent_rpc::server::RequestHandler = {
        let state = state.clone();
        Arc::new(move |_id, method, params, event_tx| {
            let state = state.clone();
            Box::pin(async move {
                handle_rpc_call(state, &method, params, event_tx).await
            })
        })
    };

    let server = RpcServer::new(handler);
    server.serve(transport).await;
}

fn persist_session_messages(db: &TranscriptDb, session: &mut Session) {
    while session.persisted_message_count < session.messages.len() {
        let msg = &session.messages[session.persisted_message_count];
        let content_json = match serde_json::to_string(&msg.content) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("Failed to serialize message: {}", e);
                break;
            }
        };
        if let Err(e) = db.append_message(&session.id, &msg.role, &content_json, None) {
            tracing::warn!("Failed to persist message: {}", e);
            break;
        }
        session.persisted_message_count += 1;
    }

    // Auto-title from first user text
    if session.title.is_none() {
        if let Some(first_user) = session.messages.iter().find(|m| m.role == "user") {
            if let Some(ChatContent::Text { text }) = first_user.content.first() {
                let title: String = text.chars().take(60).collect();
                let _ = db.set_title(&session.id, &title);
                session.title = Some(title);
            }
        }
    }
}

fn messages_from_records(
    records: &[kkagent_core::transcript::MessageRecord],
) -> Vec<ChatMessage> {
    records
        .iter()
        .filter_map(|r| {
            let content: Vec<ChatContent> = serde_json::from_str(&r.content_json).ok()?;
            Some(ChatMessage {
                role: r.role.clone(),
                content,
            })
        })
        .collect()
}

async fn summarize_with_llm(config: Arc<AppConfig>, digest: &str) -> Option<String> {
    use kkagent_llm::{create_provider, LlmRequest, StreamEvent};
    let alias = config
        .secondary_model
        .clone()
        .or_else(|| config.default_model_alias().map(|s| s.to_string()))?;
    let (model_cfg, provider_cfg) = config.resolve_model(&alias)?;
    let provider = create_provider(provider_cfg, model_cfg);
    let (tx, mut rx) = mpsc::channel(64);
    let request = LlmRequest {
        model: model_cfg.model.clone(),
        messages: vec![ChatMessage {
            role: "user".into(),
            content: vec![ChatContent::Text {
                text: digest.to_string(),
            }],
        }],
        tools: Vec::new(),
        max_tokens: 1024,
        system: Some("You compress conversation history into a concise factual summary.".into()),
        thinking: None,
    };
    tokio::spawn(async move {
        let _ = provider.stream_chat(request, tx).await;
    });
    let mut out = String::new();
    while let Some(ev) = rx.recv().await {
        match ev {
            StreamEvent::TextDelta(t) => out.push_str(&t),
            StreamEvent::MessageEnd { .. } | StreamEvent::Error(_) => break,
            _ => {}
        }
    }
    if out.trim().is_empty() {
        None
    } else {
        Some(out)
    }
}

fn resolve_session_id(db: &TranscriptDb, query: &str) -> Option<String> {
    if db.get_session(query).ok().flatten().is_some() {
        return Some(query.to_string());
    }
    let sessions = db.list_sessions(50).ok()?;
    let matches: Vec<_> = sessions
        .into_iter()
        .filter(|s| s.session_id.starts_with(query))
        .collect();
    if matches.len() == 1 {
        Some(matches[0].session_id.clone())
    } else {
        None
    }
}

async fn handle_rpc_call(
    state: Arc<ServerState>,
    method: &str,
    params: Option<serde_json::Value>,
    rpc_event_tx: mpsc::Sender<Frame>,
) -> Result<serde_json::Value, (i32, String)> {
    match method {
        "sessions.create" => {
            let session_id = uuid::Uuid::new_v4().to_string();
            let workspace = params.as_ref()
                .and_then(|p| p.get("workspace"))
                .and_then(|v| v.as_str())
                .unwrap_or(".")
                .to_string();
            let perm_mode: PermissionMode = params.as_ref()
                .and_then(|p| p.get("permission_mode"))
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_else(|| state.config.effective_permission_mode().parse().unwrap_or_default());

            let model_alias = state.config.default_model_alias().unwrap_or("default").to_string();

            let mut session = Session::new(
                session_id.clone(),
                PathBuf::from(&workspace),
                perm_mode,
                model_alias.clone(),
            );
            if !kkagent_core::is_workspace_trusted(&state.config, &session.working_dir) {
                return Err((
                    -32000,
                    format!(
                        "Workspace {} is not in trusted_workspaces",
                        session.working_dir.display()
                    ),
                ));
            }
            session.inject_date_reminder();
            session.inject_workspace_instructions().await;
            session.inject_git_context();
            {
                let section = state.skills.catalog_prompt_section().await;
                if !section.is_empty() {
                    session.system_prompt.push_str(&section);
                }
            }

            {
                let db = state.transcript.lock().await;
                if let Err(e) = db.create_session(&session_id, &model_alias, &workspace) {
                    tracing::warn!("transcript create_session: {}", e);
                }
            }

            state.interrupt_flags.lock().await.insert(
                session_id.clone(),
                session.interrupted.clone(),
            );
            state.model_aliases.lock().await.insert(
                session_id.clone(),
                session.model_alias.clone(),
            );
            state.approval_txs.lock().await.insert(
                session_id.clone(),
                session.approval_tx.clone(),
            );
            state.question_txs.lock().await.insert(
                session_id.clone(),
                session.question_tx.clone(),
            );
            state.sessions.lock().await.insert(session_id.clone(), session);
            Ok(serde_json::json!({"session_id": session_id}))
        }
        "sessions.list" => {
            let limit = params.as_ref()
                .and_then(|p| p.get("limit"))
                .and_then(|v| v.as_u64())
                .unwrap_or(20) as usize;
            let db = state.transcript.lock().await;
            let sessions = db.list_sessions(limit).map_err(|e| (-32000, e.to_string()))?;
            let list: Vec<_> = sessions
                .into_iter()
                .map(|s| {
                    serde_json::json!({
                        "session_id": s.session_id,
                        "title": s.title,
                        "model": s.model,
                        "working_dir": s.working_dir,
                        "created_at": s.created_at,
                        "updated_at": s.updated_at,
                        "message_count": s.message_count,
                    })
                })
                .collect();
            Ok(serde_json::json!({"sessions": list}))
        }
        "session.resume" => {
            let query = params.as_ref()
                .and_then(|p| p.get("session_id"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| (-32602, "Missing session_id".into()))?
                .to_string();

            let (record, messages) = {
                let db = state.transcript.lock().await;
                let sid = resolve_session_id(&db, &query)
                    .ok_or_else(|| (-32602, format!("Session not found: {}", query)))?;
                let record = db
                    .get_session(&sid)
                    .map_err(|e| (-32000, e.to_string()))?
                    .ok_or_else(|| (-32602, format!("Session not found: {}", sid)))?;
                let msgs = db
                    .load_messages(&sid)
                    .map_err(|e| (-32000, e.to_string()))?;
                (record, messages_from_records(&msgs))
            };

            let session_id = record.session_id.clone();

            // If already in memory, prefer in-memory messages (may be ahead of DB)
            {
                let sessions = state.sessions.lock().await;
                if let Some(existing) = sessions.get(&session_id) {
                    return Ok(serde_json::json!({
                        "session_id": session_id,
                        "messages": existing.messages,
                        "plan_mode": existing.plan_mode,
                        "permission_mode": existing.permission_mode,
                        "model": existing.get_model_alias(),
                    }));
                }
            }

            let perm_mode = state
                .config
                .effective_permission_mode()
                .parse()
                .unwrap_or_default();
            let mut session = Session::new(
                session_id.clone(),
                PathBuf::from(&record.working_dir),
                perm_mode,
                if record.model.is_empty() {
                    state.config.default_model_alias().unwrap_or("default").to_string()
                } else {
                    record.model.clone()
                },
            );
            session.inject_workspace_instructions().await;
            session.inject_date_reminder();
            session.inject_git_context();
            {
                let section = state.skills.catalog_prompt_section().await;
                if !section.is_empty() {
                    session.system_prompt.push_str(&section);
                }
            }
            session.messages = messages.clone();
            session.persisted_message_count = messages.len();
            session.title = record.title.clone();

            state.interrupt_flags.lock().await.insert(
                session_id.clone(),
                session.interrupted.clone(),
            );
            state.model_aliases.lock().await.insert(
                session_id.clone(),
                session.model_alias.clone(),
            );
            state.approval_txs.lock().await.insert(
                session_id.clone(),
                session.approval_tx.clone(),
            );
            state.question_txs.lock().await.insert(
                session_id.clone(),
                session.question_tx.clone(),
            );
            state.sessions.lock().await.insert(session_id.clone(), session);

            Ok(serde_json::json!({
                "session_id": session_id,
                "messages": messages,
                "plan_mode": false,
                "permission_mode": perm_mode,
                "model": record.model,
            }))
        }
        "session.prompt" => {
            let session_id = params.as_ref()
                .and_then(|p| p.get("session_id"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| (-32602, "Missing session_id".into()))?
                .to_string();
            let text = params.as_ref()
                .and_then(|p| p.get("text"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| (-32602, "Missing text".into()))?
                .to_string();

            {
                let mut sessions = state.sessions.lock().await;
                if let Some(session) = sessions.get_mut(&session_id) {
                    session.clear_interrupt();
                    session.add_user_message(text);
                    session.begin_turn();
                } else {
                    return Err((-32602, format!("Session not found: {}", session_id)));
                }
            }
            // Persist without holding `sessions` + `transcript` together (avoid deadlock)
            {
                let snapshot = {
                    let sessions = state.sessions.lock().await;
                    sessions.get(&session_id).map(|s| {
                        (
                            s.messages[s.persisted_message_count..].to_vec(),
                            s.persisted_message_count,
                            s.title.clone(),
                        )
                    })
                };
                if let Some((pending, start, title)) = snapshot {
                    let mut new_title = title;
                    {
                        let db = state.transcript.lock().await;
                        for msg in &pending {
                            let content_json =
                                serde_json::to_string(&msg.content).unwrap_or_else(|_| "[]".into());
                            let _ = db.append_message(&session_id, &msg.role, &content_json, None);
                        }
                        if new_title.is_none() {
                            if let Some(ChatContent::Text { text }) =
                                pending.iter().find(|m| m.role == "user").and_then(|m| m.content.first())
                            {
                                let t: String = text.chars().take(60).collect();
                                let _ = db.set_title(&session_id, &t);
                                new_title = Some(t);
                            }
                        }
                    }
                    if let Some(session) = state.sessions.lock().await.get_mut(&session_id) {
                        session.persisted_message_count = start + pending.len();
                        if session.title.is_none() {
                            session.title = new_title;
                        }
                    }
                }
            }

            // Build agent event channel that forwards to RPC transport
            let (agent_event_tx, mut agent_event_rx) = mpsc::channel::<AgentEvent>(256);

            let rpc_tx = rpc_event_tx.clone();
            tokio::spawn(async move {
                while let Some(evt) = agent_event_rx.recv().await {
                    let data = serde_json::to_value(&evt).unwrap_or_default();
                    let frame = Frame::Event {
                        event: "agent".into(),
                        scope: None,
                        data,
                    };
                    if rpc_tx.send(frame).await.is_err() {
                        break;
                    }
                }
            });

            // Resolve model from shared alias handle (works even mid-turn)
            let model_alias = {
                let aliases = state.model_aliases.lock().await;
                aliases
                    .get(&session_id)
                    .map(|a| a.lock().unwrap_or_else(|e| e.into_inner()).clone())
                    .filter(|a| !a.is_empty())
                    .or_else(|| {
                        // Fallback if map missing
                        None
                    })
                    .or_else(|| state.config.default_model_alias().map(|s| s.to_string()))
                    .ok_or_else(|| (-32000, "No default_model in config".into()))?
            };
            if state.config.resolve_model(&model_alias).is_none() {
                return Err((-32000, format!("Model '{}' not found", model_alias)));
            }

            let mut tools = ToolRegistry::new();
            kkagent_tools::register_builtin_tools(&mut tools);
            let auto_bg = state
                .config
                .background
                .as_ref()
                .and_then(|b| b.bash_auto_background_on_timeout)
                .unwrap_or(true);
            tools.register(Arc::new(kkagent_tools::builtin::BashTool::new(
                state.bash_shells.clone(),
                kkagent_tools::builtin::BashOptions {
                    auto_background_on_timeout: auto_bg,
                },
            )));
            register_mcp_tools(&mut tools, &state.mcp).await;

            let subagents = state.subagents.clone();
            let cfg_for_sub = state.config.clone();
            let launch: kkagent_tools::builtin::task::SubagentLaunchFn = Arc::new(move |sub_cfg| {
                let mgr = subagents.clone();
                let app_cfg = cfg_for_sub.clone();
                let id = sub_cfg.agent_id.clone();
                let mgr_abort = mgr.clone();
                let id_abort = id.clone();
                let join = tokio::spawn(async move {
                    tracing::info!("Subagent {} starting: {}", id, sub_cfg.description);
                    // JoinError / cancel path: TaskStop aborts this task.
                    let run = kkagent_core::run_subagent(app_cfg, sub_cfg, PermissionMode::Auto);
                    match run.await {
                        Ok(result) => {
                            tracing::info!(
                                "Subagent {} complete ({} chars)",
                                id,
                                result.len()
                            );
                            mgr.complete(&id, result).await;
                        }
                        Err(e) => {
                            tracing::error!("Subagent {} failed: {}", id, e);
                            mgr.fail(&id, e.to_string()).await;
                        }
                    }
                });
                let abort = join.abort_handle();
                tokio::spawn(async move {
                    mgr_abort.set_abort_handle(&id_abort, abort).await;
                });
            });
            tools.register(Arc::new(kkagent_tools::builtin::TaskTool::new(
                state.subagents.clone(),
                launch.clone(),
            )));
            tools.register(Arc::new(kkagent_tools::builtin::AgentTool::new(
                state.subagents.clone(),
                launch.clone(),
            )));
            tools.register(Arc::new(kkagent_tools::builtin::AgentSwarmTool::new(
                state.subagents.clone(),
                launch,
            )));
            tools.register(Arc::new(kkagent_tools::builtin::TaskOutputTool::new(
                state.subagents.clone(),
            )));
            tools.register(Arc::new(kkagent_tools::builtin::TaskListTool::new(
                state.subagents.clone(),
            )));
            tools.register(Arc::new(kkagent_tools::builtin::TaskStopTool::new(
                state.subagents.clone(),
            )));
            // Goal tools if available
            let goal_mgr = Arc::new(kkagent_protocol::goal::GoalManager::new());
            tools.register(Arc::new(kkagent_tools::builtin::CreateGoalTool::new(goal_mgr.clone())));
            tools.register(Arc::new(kkagent_tools::builtin::GetGoalTool::new(goal_mgr.clone())));
            tools.register(Arc::new(kkagent_tools::builtin::UpdateGoalTool::new(goal_mgr)));
            tools.register(Arc::new(kkagent_tools::builtin::SkillTool::new(
                state.skills.clone(),
            )));
            tools.register(Arc::new(kkagent_tools::builtin::WebSearchTool::new(
                state.web.clone(),
            )));
            tools.register(Arc::new(kkagent_tools::builtin::FetchUrlTool::new(
                state.web.clone(),
            )));
            tools.register(Arc::new(kkagent_tools::builtin::CronCreateTool::new(
                state.cron.clone(),
            )));
            tools.register(Arc::new(kkagent_tools::builtin::CronListTool::new(
                state.cron.clone(),
            )));
            tools.register(Arc::new(kkagent_tools::builtin::CronDeleteTool::new(
                state.cron.clone(),
            )));

            let permission_rules = state.config.permission.as_ref()
                .map(|p| p.rules.clone())
                .unwrap_or_default();
            let perm_mode: PermissionMode = state.config.effective_permission_mode().parse().unwrap_or_default();
            let permission = PermissionChain::new(perm_mode, permission_rules);

            let agent_loop = Arc::new(
                AgentLoop::new(
                    state.config.clone(),
                    Arc::new(tools),
                    Arc::new(Mutex::new(permission)),
                    agent_event_tx.clone(),
                    state.abort_registry.clone(),
                )
                .with_hooks(state.hooks.clone()),
            );

            let state_clone = state.clone();
            let sid = session_id.clone();
            tokio::spawn(async move {
                // Take session out so we do NOT hold the sessions mutex while
                // waiting for tool approval (would deadlock approval.respond).
                let mut session = {
                    let mut sessions = state_clone.sessions.lock().await;
                    match sessions.remove(&sid) {
                        Some(s) => s,
                        None => {
                            tracing::error!("Session {} disappeared before turn", sid);
                            return;
                        }
                    }
                };

                if let Err(e) = agent_loop.run_turn(&mut session).await {
                    tracing::error!("Agent loop error: {}", e);
                    let _ = agent_event_tx.send(AgentEvent::Error {
                        session_id: sid.clone(),
                        message: e.to_string(),
                    }).await;
                }

                {
                    let db = state_clone.transcript.lock().await;
                    persist_session_messages(&db, &mut session);
                }

                state_clone.sessions.lock().await.insert(sid, session);
            });

            Ok(serde_json::json!({"ok": true}))
        }
        "session.interrupt" => {
            let session_id = params.as_ref()
                .and_then(|p| p.get("session_id"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| (-32602, "Missing session_id".into()))?
                .to_string();

            if let Some(flag) = state.interrupt_flags.lock().await.get(&session_id) {
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            if let Some(session) = state.sessions.lock().await.get(&session_id) {
                session.request_interrupt();
            } else {
                // Session is mid-turn (removed from map) — still cancel approvals / abort stream
                if let Some(tx) = state.approval_txs.lock().await.get(&session_id) {
                    let _ = tx.try_send(kkagent_protocol::ApprovalResponse {
                        approval_id: String::new(),
                        decision: kkagent_protocol::ApprovalDecision::Cancelled,
                        scope: None,
                        feedback: Some("interrupted".into()),
                    });
                }
            }
            if let Some(handle) = state.abort_registry.lock().await.remove(&session_id) {
                handle.abort();
            }
            Ok(serde_json::json!({"ok": true}))
        }
        "session.set_permission_mode" => {
            let session_id = params.as_ref()
                .and_then(|p| p.get("session_id"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let mode: PermissionMode = params.as_ref()
                .and_then(|p| p.get("mode"))
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            let mut sessions = state.sessions.lock().await;
            if let Some(session) = sessions.get_mut(&session_id) {
                session.permission_mode = mode;
            }
            Ok(serde_json::json!({"ok": true}))
        }
        "session.set_plan_mode" => {
            let session_id = params.as_ref()
                .and_then(|p| p.get("session_id"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let enabled = params.as_ref()
                .and_then(|p| p.get("enabled"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let mut sessions = state.sessions.lock().await;
            if let Some(session) = sessions.get_mut(&session_id) {
                session.plan_mode = enabled;
                if enabled {
                    if let Some(parent) = session.plan_file_path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                }
            }
            Ok(serde_json::json!({"ok": true}))
        }
        "session.set_model" => {
            let session_id = params.as_ref()
                .and_then(|p| p.get("session_id"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let model = params.as_ref()
                .and_then(|p| p.get("model"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| (-32602, "Missing model".into()))?
                .to_string();
            if state.config.resolve_model(&model).is_none() {
                return Err((-32602, format!("Unknown model: {}", model)));
            }
            // Always update the shared Arc (works mid-turn while session is out of the map).
            let mut updated = false;
            if let Some(arc) = state.model_aliases.lock().await.get(&session_id) {
                *arc.lock().unwrap_or_else(|e| e.into_inner()) = model.clone();
                updated = true;
            }
            if let Some(session) = state.sessions.lock().await.get(&session_id) {
                session.set_model_alias(model.clone());
                updated = true;
            }
            if !updated {
                return Err((-32602, format!("Session not found: {}", session_id)));
            }
            tracing::info!("Session {} model set to {}", session_id, model);
            Ok(serde_json::json!({"ok": true, "model": model}))
        }
        "session.set_title" => {
            let session_id = params.as_ref()
                .and_then(|p| p.get("session_id"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let title = params.as_ref()
                .and_then(|p| p.get("title"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| (-32602, "Missing title".into()))?
                .to_string();
            {
                let mut sessions = state.sessions.lock().await;
                if let Some(session) = sessions.get_mut(&session_id) {
                    session.title = Some(title.clone());
                }
            }
            let db = state.transcript.lock().await;
            let _ = db.set_title(&session_id, &title);
            Ok(serde_json::json!({"ok": true, "title": title}))
        }
        "session.undo" => {
            let session_id = params.as_ref()
                .and_then(|p| p.get("session_id"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| (-32602, "Missing session_id".into()))?
                .to_string();
            let count = params.as_ref()
                .and_then(|p| p.get("count"))
                .and_then(|v| v.as_u64())
                .unwrap_or(1) as usize;

            let mut undone = 0usize;
            let mut keep = 0usize;
            {
                let mut sessions = state.sessions.lock().await;
                let session = sessions
                    .get_mut(&session_id)
                    .ok_or_else(|| (-32602, format!("Session not found: {}", session_id)))?;
                for _ in 0..count {
                    match session.undo_last_turn() {
                        Ok(n) => {
                            keep = n;
                            undone += 1;
                        }
                        Err(_) => break,
                    }
                }
                if undone == 0 {
                    return Err((-32000, "Nothing to undo".into()));
                }
            }
            {
                let db = state.transcript.lock().await;
                let _ = db.truncate_messages(&session_id, keep);
            }
            let messages = {
                let sessions = state.sessions.lock().await;
                sessions
                    .get(&session_id)
                    .map(|s| s.messages.clone())
                    .unwrap_or_default()
            };
            Ok(serde_json::json!({
                "ok": true,
                "undone": undone,
                "message_count": keep,
                "messages": messages,
            }))
        }
        "session.compact" => {
            let session_id = params.as_ref()
                .and_then(|p| p.get("session_id"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| (-32602, "Missing session_id".into()))?
                .to_string();
            let keep_last = params.as_ref()
                .and_then(|p| p.get("keep_last"))
                .and_then(|v| v.as_u64())
                .unwrap_or(6) as usize;

            // Build a summary of older messages via secondary/default model when possible.
            let summary = {
                let sessions = state.sessions.lock().await;
                let Some(session) = sessions.get(&session_id) else {
                    drop(sessions);
                    let db = state.transcript.lock().await;
                    let deleted = db
                        .compact_session(&session_id, keep_last, "Conversation compacted.")
                        .map_err(|e| (-32000, e.to_string()))?;
                    return Ok(serde_json::json!({"ok": true, "deleted": deleted}));
                };
                if session.messages.len() <= keep_last {
                    return Ok(serde_json::json!({"ok": true, "deleted": 0}));
                }
                let old = &session.messages[..session.messages.len() - keep_last];
                let mut digest = String::from("Summarize the following conversation for future context. Keep decisions, file paths, and unfinished tasks.\n\n");
                for m in old.iter().take(40) {
                    let role = &m.role;
                    let text: String = m
                        .content
                        .iter()
                        .filter_map(|c| match c {
                            ChatContent::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join(" ");
                    if !text.is_empty() {
                        digest.push_str(&format!("[{role}] {}\n", text.chars().take(500).collect::<String>()));
                    }
                }
                digest
            };

            let summary_text = summarize_with_llm(state.config.clone(), &summary)
                .await
                .unwrap_or_else(|| "Conversation compacted.".into());

            let deleted = {
                let db = state.transcript.lock().await;
                db.compact_session(&session_id, keep_last, &summary_text)
                    .map_err(|e| (-32000, e.to_string()))?
            };

            {
                let db = state.transcript.lock().await;
                let records = db.load_messages(&session_id).unwrap_or_default();
                let msgs = messages_from_records(&records);
                drop(db);
                let mut sessions = state.sessions.lock().await;
                if let Some(session) = sessions.get_mut(&session_id) {
                    session.messages = msgs;
                    session.persisted_message_count = session.messages.len();
                    session.undo_stack.clear();
                }
            }

            Ok(serde_json::json!({"ok": true, "deleted": deleted, "summary": summary_text}))
        }
        "tasks.list" => {
            let all = state.subagents.list_all().await;
            let tasks: Vec<_> = all
                .into_iter()
                .map(|t| {
                    serde_json::json!({
                        "task_id": t.agent_id,
                        "description": t.description,
                        "status": t.status,
                        "result": t.result,
                        "error": t.error,
                        "turns_used": t.turns_used,
                    })
                })
                .collect();
            Ok(serde_json::json!({"tasks": tasks}))
        }
        "tasks.stop" => {
            let task_id = params
                .as_ref()
                .and_then(|p| p.get("task_id").or_else(|| p.get("agent_id")))
                .and_then(|v| v.as_str())
                .ok_or_else(|| (-32602, "Missing task_id".into()))?;
            match state.subagents.stop(task_id).await {
                Ok(state) => Ok(serde_json::json!({
                    "ok": true,
                    "task_id": state.agent_id,
                    "status": "cancelled",
                })),
                Err(e) => Err((-32000, e.to_string())),
            }
        }
        "approval.respond" => {
            if let Some(params) = params {
                if let Ok(response) = serde_json::from_value::<kkagent_protocol::ApprovalResponse>(params.clone()) {
                    // Prefer session_id from params if present
                    let session_id = params
                        .get("session_id")
                        .and_then(|v| v.as_str())
                        .map(String::from);

                    let txs = state.approval_txs.lock().await;
                    if let Some(sid) = session_id {
                        if let Some(tx) = txs.get(&sid) {
                            let _ = tx.try_send(response);
                            return Ok(serde_json::json!({"ok": true}));
                        }
                    }
                    // Fallback: try all
                    for tx in txs.values() {
                        let _ = tx.try_send(response.clone());
                    }
                    return Ok(serde_json::json!({"ok": true}));
                }
            }
            Err((-32602, "Invalid approval response".into()))
        }
        "question.respond" => {
            if let Some(params) = params {
                if let Ok(response) =
                    serde_json::from_value::<kkagent_protocol::QuestionResponse>(params.clone())
                {
                    let session_id = params
                        .get("session_id")
                        .and_then(|v| v.as_str())
                        .map(String::from);
                    let txs = state.question_txs.lock().await;
                    if let Some(sid) = session_id {
                        if let Some(tx) = txs.get(&sid) {
                            let _ = tx.try_send(response);
                            return Ok(serde_json::json!({"ok": true}));
                        }
                    }
                    for tx in txs.values() {
                        let _ = tx.try_send(response.clone());
                    }
                    return Ok(serde_json::json!({"ok": true}));
                }
            }
            Err((-32602, "Invalid question response".into()))
        }
        _ => {
            Err((-32601, format!("Method not found: {}", method)))
        }
    }
}
