use anyhow::Result;
use clap::{Parser, Subcommand};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tokio::task::AbortHandle;

use kkagent_client::KkagentClient;
use kkagent_config::{load_config, AppConfig};
use kkagent_core::SubagentMirrorContext;
use kkagent_core::{
    AgentLoop, PermissionChain, Session, SessionCloseReason, SessionCreateSource, SessionStore,
    TranscriptDb,
};
use kkagent_di::ServiceContainer;
use kkagent_llm::{ChatContent, ChatMessage};
use kkagent_mcp::{register_mcp_tools, McpManager};
use kkagent_protocol::subagent::SubagentManager;
use kkagent_protocol::{AgentEvent, Frame, PermissionMode};
use kkagent_rpc::{transport::memory::create_memory_pair, RpcClient, RpcServer};
use kkagent_telemetry::{
    CloudAppender, CloudAppenderOptions, ConsoleAppender, FileAppender, TelemetryService,
};
use kkagent_tools::ToolRegistry;
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
    /// Run as standalone RPC server (memory/socket)
    Server {
        /// Socket path to listen on
        #[arg(long)]
        listen: Option<String>,
        /// Also serve REST+WS on this address (e.g. 127.0.0.1:8787)
        #[arg(long)]
        http: Option<String>,
        /// Bearer/query token for HTTP API
        #[arg(long)]
        http_token: Option<String>,
    },
    /// Serve Agent Client Protocol over stdio (IDE bridge)
    Acp,
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
        config
            .effective_permission_mode()
            .parse()
            .unwrap_or(PermissionMode::Manual)
    };

    match cli.command {
        Some(Commands::Server {
            listen,
            http,
            http_token,
        }) => {
            let token = http_token.or_else(|| std::env::var("KKAGENT_HTTP_TOKEN").ok());
            run_server(config, listen, http, token).await
        }
        Some(Commands::Acp) => {
            let server = kkagent_acp::AcpServer::new();
            server.serve_stdio().await
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

async fn run_print_mode(
    config: AppConfig,
    prompt: String,
    permission_mode: PermissionMode,
) -> Result<()> {
    let (client_stream, server_stream) = create_memory_pair();
    let (event_tx, event_rx) = mpsc::channel::<Frame>(256);
    let rpc_client = RpcClient::new(client_stream, event_tx);

    let config_arc = Arc::new(config.clone());
    tokio::spawn(async move {
        run_server_handler(server_stream, config_arc).await;
    });

    let mut client = KkagentClient::new(rpc_client, event_rx);
    let cwd = std::env::current_dir()?.to_string_lossy().to_string();
    let session_id = client
        .create_session(Some(&cwd), Some(permission_mode))
        .await?;
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
                        return Ok(());
                    }
                    AgentEvent::Error { message, .. } => {
                        return Err(anyhow::anyhow!("Agent turn failed: {message}"));
                    }
                    _ => {}
                }
            }
        }
    }
    Err(anyhow::anyhow!(
        "Agent event stream closed before the turn completed"
    ))
}

async fn run_server(
    config: AppConfig,
    listen: Option<String>,
    http: Option<String>,
    http_token: Option<String>,
) -> Result<()> {
    let socket_path = listen.unwrap_or_else(|| {
        let dir = kkagent_config::default_config_dir();
        dir.join("server.sock").to_string_lossy().to_string()
    });

    tracing::info!("Starting server on {}", socket_path);
    let listener = kkagent_rpc::transport::uds::bind_uds(std::path::Path::new(&socket_path))?;
    let config_arc = Arc::new(config);
    let state = build_server_state(config_arc.clone()).await;

    if let Some(addr) = http {
        let backend: Arc<dyn kkagent_rpc::HttpBackend> = Arc::new(AgentHttpBackend {
            state: state.clone(),
        });
        let token = http_token;
        tokio::spawn(async move {
            if let Err(e) = kkagent_rpc::serve_http_with_backend(&addr, backend, token).await {
                tracing::error!("HTTP server error: {e}");
            }
        });
    }

    loop {
        let (stream, _) = listener.accept().await?;
        let st = state.clone();
        tokio::spawn(async move {
            run_server_handler_with_state(stream, st).await;
        });
    }
}

struct AgentHttpBackend {
    state: Arc<ServerState>,
}

#[async_trait::async_trait]
impl kkagent_rpc::HttpBackend for AgentHttpBackend {
    async fn list_sessions(&self) -> serde_json::Value {
        let sessions = self.state.sessions.lock().await;
        let list: Vec<_> = sessions
            .values()
            .map(|s| {
                serde_json::json!({
                    "session_id": s.id,
                    "title": s.title,
                    "workspace": s.working_dir.display().to_string(),
                    "messages": s.messages.len(),
                    "permission_mode": format!("{:?}", s.permission_mode),
                })
            })
            .collect();
        // Also include transcript DB sessions
        let db = self.state.transcript.lock().await;
        let archived = db.list_sessions(50).unwrap_or_default();
        let archived_json: Vec<_> = archived
            .into_iter()
            .map(|r| {
                serde_json::json!({
                    "session_id": r.session_id,
                    "title": r.title,
                    "working_dir": r.working_dir,
                    "updated_at": r.updated_at,
                })
            })
            .collect();
        serde_json::json!({"sessions": list, "transcript": archived_json})
    }

    async fn create_session(
        &self,
        workspace: Option<String>,
        title: Option<String>,
    ) -> serde_json::Value {
        let id = uuid::Uuid::new_v4().to_string();
        let cwd = workspace
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let model = self
            .state
            .config
            .default_model_alias()
            .unwrap_or("default")
            .to_string();
        let mut session = Session::new(
            id.clone(),
            cwd.clone(),
            PermissionMode::Manual,
            model.clone(),
        );
        if let Some(ref t) = title {
            let _ = session.set_title_persisted(t.clone());
        }
        {
            let db = self.state.transcript.lock().await;
            let _ = db.create_session(&id, &model, &cwd.to_string_lossy());
            if let Some(ref t) = title {
                let _ = db.set_title(&id, t);
            }
        }
        session.services.on_created().await;
        self.state
            .interrupt_flags
            .lock()
            .await
            .insert(id.clone(), session.interrupted.clone());
        self.state
            .model_aliases
            .lock()
            .await
            .insert(id.clone(), session.model_alias.clone());
        self.state
            .approval_txs
            .lock()
            .await
            .insert(id.clone(), session.approval_tx.clone());
        self.state
            .question_txs
            .lock()
            .await
            .insert(id.clone(), session.question_tx.clone());
        let session_dir = session.session_dir().display().to_string();
        self.state.sessions.lock().await.insert(id.clone(), session);
        serde_json::json!({
            "session_id": id,
            "workspace": cwd.display().to_string(),
            "title": title,
            "session_dir": session_dir,
            "created_at": chrono::Utc::now().to_rfc3339(),
        })
    }

    async fn get_session(&self, id: &str) -> Option<serde_json::Value> {
        let sessions = self.state.sessions.lock().await;
        sessions.get(id).map(|s| {
            serde_json::json!({
                "session_id": s.id,
                "title": s.title,
                "workspace": s.working_dir.display().to_string(),
                "messages": s.messages,
                "usage": {
                    "input_tokens": s.usage.snapshot().input_tokens,
                    "output_tokens": s.usage.snapshot().output_tokens,
                    "steps": s.usage.snapshot().steps,
                    "turns": s.usage.snapshot().turns,
                },
            })
        })
    }

    async fn post_message(&self, id: &str, text: &str) -> Result<serde_json::Value, String> {
        // Queue user message and run AgentLoop turn on shared ServerState.
        let mut sessions = self.state.sessions.lock().await;
        let session = sessions
            .get_mut(id)
            .ok_or_else(|| "session not found".to_string())?;
        session.add_user_message(text.to_string());
        let n = session.messages.len();
        drop(sessions);
        let state = self.state.clone();
        let sid = id.to_string();
        tokio::spawn(async move {
            if let Err(e) = run_http_turn(state, &sid).await {
                tracing::warn!("HTTP-triggered turn failed: {e}");
            }
        });
        Ok(serde_json::json!({"ok": true, "queued": true, "message_count": n}))
    }

    async fn list_tools(&self) -> serde_json::Value {
        let mut reg = ToolRegistry::new();
        kkagent_tools::register_builtin_tools(&mut reg);
        let names: Vec<_> = reg
            .tool_definitions()
            .iter()
            .map(|t| serde_json::json!({"name": t.name, "description": t.description}))
            .collect();
        serde_json::json!({"tools": names})
    }

    async fn list_tasks(&self) -> serde_json::Value {
        let all = self.state.subagents.list_all().await;
        let tasks: Vec<_> = all
            .into_iter()
            .map(|t| {
                serde_json::json!({
                    "task_id": t.agent_id,
                    "description": t.description,
                    "status": t.status,
                })
            })
            .collect();
        serde_json::json!({"tasks": tasks})
    }

    async fn list_skills(&self) -> serde_json::Value {
        let list = self.state.skills.list().await;
        let items: Vec<_> = list
            .into_iter()
            .map(|e| {
                serde_json::json!({
                    "name": e.name,
                    "description": e.description,
                })
            })
            .collect();
        serde_json::json!({"skills": items})
    }

    async fn list_models(&self) -> serde_json::Value {
        let mut models: Vec<_> = self
            .state
            .config
            .models
            .iter()
            .map(|(alias, m)| {
                serde_json::json!({
                    "alias": alias,
                    "model": m.model,
                    "provider": m.provider,
                    "max_context_size": m.max_context_size,
                })
            })
            .collect();
        for e in kkagent_llm::builtin_catalog() {
            models.push(serde_json::json!({
                "id": e.id,
                "provider": e.provider,
                "context_window": e.context_window,
                "responses_api": e.responses_api,
            }));
        }
        serde_json::json!({"models": models})
    }

    async fn get_config(&self) -> serde_json::Value {
        serde_json::json!({
            "default_model": self.state.config.default_model,
            "config_dir": kkagent_config::default_config_dir().display().to_string(),
            "mcp_servers": self.state.config.mcp_servers.len(),
        })
    }

    async fn approve(
        &self,
        id: &str,
        decision: &str,
        feedback: Option<String>,
    ) -> Result<serde_json::Value, String> {
        let dec = match decision {
            "approved" | "approve" | "allow" => kkagent_protocol::ApprovalDecision::Approved,
            "rejected" | "reject" | "deny" => kkagent_protocol::ApprovalDecision::Rejected,
            _ => kkagent_protocol::ApprovalDecision::Cancelled,
        };
        let resp = kkagent_protocol::ApprovalResponse {
            approval_id: id.to_string(),
            decision: dec,
            scope: None,
            feedback,
        };
        let txs = self.state.approval_txs.lock().await;
        for tx in txs.values() {
            let _ = tx.try_send(resp.clone());
        }
        Ok(serde_json::json!({"ok": true, "approval_id": id}))
    }

    async fn fs_read(&self, path: &str) -> Result<String, String> {
        let path = resolve_http_fs_path(&self.state.config, path, false)?;
        tokio::fs::read_to_string(path)
            .await
            .map_err(|e| e.to_string())
    }

    async fn fs_write(&self, path: &str, content: &str) -> Result<(), String> {
        let path = resolve_http_fs_path(&self.state.config, path, true)?;
        if let Some(parent) = path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        tokio::fs::write(&path, content)
            .await
            .map_err(|e| e.to_string())
    }

    async fn search(&self, query: &str) -> serde_json::Value {
        serde_json::json!({"query": query, "hits": [], "note": "use Grep tool via agent turn"})
    }

    async fn workspace_info(&self) -> serde_json::Value {
        serde_json::json!({
            "cwd": std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_else(|_| ".".into()),
            "sessions": self.state.sessions.lock().await.len(),
        })
    }
}

fn trusted_http_roots(config: &AppConfig) -> Result<Vec<PathBuf>, String> {
    let configured = if config.trusted_workspaces.is_empty() {
        vec![std::env::current_dir().map_err(|e| e.to_string())?]
    } else {
        config
            .trusted_workspaces
            .iter()
            .map(PathBuf::from)
            .collect()
    };
    configured
        .into_iter()
        .map(|root| {
            std::fs::canonicalize(&root)
                .map_err(|e| format!("invalid trusted workspace {}: {e}", root.display()))
        })
        .collect()
}

fn resolve_http_fs_path(config: &AppConfig, raw: &str, for_write: bool) -> Result<PathBuf, String> {
    if raw.trim().is_empty() {
        return Err("path must not be empty".into());
    }
    let roots = trusted_http_roots(config)?;
    let candidate = {
        let path = PathBuf::from(raw);
        if path.is_absolute() {
            path
        } else {
            std::env::current_dir()
                .map_err(|e| e.to_string())?
                .join(path)
        }
    };

    let resolved = if candidate.exists() || !for_write {
        std::fs::canonicalize(&candidate)
            .map_err(|e| format!("cannot resolve {}: {e}", candidate.display()))?
    } else {
        let mut ancestor = candidate.as_path();
        while !ancestor.exists() {
            ancestor = ancestor
                .parent()
                .ok_or_else(|| format!("cannot resolve parent of {}", candidate.display()))?;
        }
        let canonical_ancestor = std::fs::canonicalize(ancestor)
            .map_err(|e| format!("cannot resolve {}: {e}", ancestor.display()))?;
        let suffix = candidate
            .strip_prefix(ancestor)
            .map_err(|e| e.to_string())?;
        canonical_ancestor.join(suffix)
    };

    if roots.iter().any(|root| resolved.starts_with(root)) {
        Ok(resolved)
    } else {
        Err(format!(
            "path {} is outside trusted workspaces",
            candidate.display()
        ))
    }
}

#[cfg(test)]
mod http_path_tests {
    use super::*;

    fn config_with_root(root: &std::path::Path) -> AppConfig {
        AppConfig {
            trusted_workspaces: vec![root.display().to_string()],
            ..AppConfig::default()
        }
    }

    #[test]
    fn rejects_paths_outside_trusted_workspace() {
        let root = std::env::temp_dir().join(format!("kkagent-http-root-{}", uuid::Uuid::new_v4()));
        let outside =
            std::env::temp_dir().join(format!("kkagent-http-out-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "secret").unwrap();
        let err = resolve_http_fs_path(
            &config_with_root(&root),
            &outside.join("secret.txt").display().to_string(),
            false,
        )
        .unwrap_err();
        assert!(err.contains("outside trusted workspaces"));
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn allows_new_files_below_trusted_workspace() {
        let root = std::env::temp_dir().join(format!("kkagent-http-root-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let expected = root.join("new").join("file.txt");
        let actual = resolve_http_fs_path(
            &config_with_root(&root),
            &expected.display().to_string(),
            true,
        )
        .unwrap();
        assert_eq!(
            actual,
            std::fs::canonicalize(&root)
                .unwrap()
                .join("new")
                .join("file.txt")
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}

async fn run_http_turn(state: Arc<ServerState>, session_id: &str) -> anyhow::Result<()> {
    let (event_tx, mut event_rx) = mpsc::channel::<AgentEvent>(64);
    tokio::spawn(async move { while let Some(_ev) = event_rx.recv().await {} });

    let mut session = {
        let mut sessions = state.sessions.lock().await;
        sessions
            .remove(session_id)
            .ok_or_else(|| anyhow::anyhow!("session gone"))?
    };
    session.begin_turn();

    let mut tools = ToolRegistry::new();
    kkagent_tools::register_builtin_tools(&mut tools);
    tools.register(Arc::new(kkagent_tools::builtin::BashTool::new(
        state.bash_shells.clone(),
        kkagent_tools::builtin::BashOptions {
            auto_background_on_timeout: true,
        },
    )));
    tools.register(Arc::new(kkagent_tools::builtin::SkillTool::new(
        state.skills.clone(),
    )));
    tools.register(Arc::new(kkagent_tools::builtin::WebSearchTool::new(
        state.web.clone(),
    )));
    tools.register(Arc::new(kkagent_tools::builtin::FetchUrlTool::new(
        state.web.clone(),
    )));

    let permission_rules = state
        .config
        .permission
        .as_ref()
        .map(|p| p.rules.clone())
        .unwrap_or_default();
    let permission = Arc::new(Mutex::new(PermissionChain::new(
        session.permission_mode,
        permission_rules,
    )));
    let agent = AgentLoop::new(
        state.config.clone(),
        Arc::new(tools),
        permission,
        event_tx,
        state.abort_registry.clone(),
    )
    .with_hooks(state.hooks.clone())
    .with_goal_manager(state.goal_mgr.clone());

    let result = agent.run_turn(&mut session).await;
    {
        let db = state.transcript.lock().await;
        persist_session_messages(&db, &mut session);
    }
    state
        .sessions
        .lock()
        .await
        .insert(session_id.to_string(), session);
    result
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
    /// Pending cron-fire XML injections for the next turn.
    cron_fires: Arc<Mutex<Vec<String>>>,
    goal_mgr: Arc<kkagent_protocol::goal::GoalManager>,
    hooks: Arc<kkagent_mcp::HookManager>,
    skills: Arc<kkagent_tools::SkillCatalog>,
    web: Arc<kkagent_tools::WebServicesConfig>,
    plugins: Arc<kkagent_core::PluginManager>,
    telemetry: kkagent_telemetry::TelemetryServiceHandle,
}

fn mcp_manager_from_config(config: &AppConfig) -> McpManager {
    let configs: Vec<kkagent_mcp::McpServerConfig> = config
        .mcp_servers
        .iter()
        .map(|(name, cfg)| kkagent_mcp::McpServerConfig::from_app(name.clone(), cfg))
        .collect();
    McpManager::new(configs)
}

async fn build_server_state(config: Arc<AppConfig>) -> Arc<ServerState> {
    let transcript = match TranscriptDb::open_default() {
        Ok(db) => db,
        Err(e) => {
            tracing::error!("Failed to open transcript DB: {} — using in-memory", e);
            match TranscriptDb::open_in_memory() {
                Ok(db) => db,
                Err(e2) => {
                    tracing::error!("Failed to open in-memory transcript: {}", e2);
                    let tmp = std::env::temp_dir().join("kkagent-transcripts.db");
                    TranscriptDb::open(&tmp).unwrap_or_else(|e3| {
                        panic!(
                            "Cannot open transcript DB (file={}, mem={}, tmp={})",
                            e, e2, e3
                        );
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
    let mut hooks_mgr = kkagent_mcp::HookManager::new(&cwd);
    hooks_mgr.load_from_app_config(&config.hooks).await;
    let _ = hooks_mgr.discover().await;
    let hooks = Arc::new(hooks_mgr);
    let skills = Arc::new(kkagent_tools::SkillCatalog::discover(&cwd).await);
    let cron_path = kkagent_config::default_config_dir().join("cron.json");
    let cron = Arc::new(kkagent_tools::CronManager::with_persist(cron_path).await);
    let goal_mgr = Arc::new(kkagent_protocol::goal::GoalManager::new());
    let web = Arc::new(kkagent_tools::WebServicesConfig::from_app(&config));

    let cron_fires: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let cron_bg = cron.clone();
        let fires = cron_fires.clone();
        let hooks_cron = hooks.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(15)).await;
                let due = cron_bg.take_due().await;
                for (id, prompt) in due {
                    let xml = kkagent_tools::render_cron_fire_xml(
                        &id,
                        "scheduled",
                        &prompt,
                        false,
                        1,
                        false,
                    );
                    tracing::info!(
                        "Cron job {} due: {}",
                        id,
                        prompt.chars().take(80).collect::<String>()
                    );
                    fires.lock().await.push(xml);
                    let _ = hooks_cron.fire_notification(&format!("cron:{id}")).await;
                }
            }
        });
    }

    let plugins = {
        let dir = kkagent_config::default_config_dir().join("plugins");
        kkagent_core::PluginManager::discover(&dir).await
    };

    let di_root = ServiceContainer::new("kkagent-root");
    let telemetry = TelemetryService::new();
    telemetry.add_appender(Arc::new(ConsoleAppender)).await;
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    telemetry
        .add_appender(Arc::new(FileAppender::new(
            home.join(".kkagent").join("telemetry").join("events.jsonl"),
        )))
        .await;
    let mut cloud_opts = CloudAppenderOptions::default();
    cloud_opts.device_id =
        std::env::var("KKAGENT_DEVICE_ID").unwrap_or_else(|_| uuid::Uuid::new_v4().to_string());
    cloud_opts.model = config.default_model.clone();
    if std::env::var("KKAGENT_TELEMETRY_CLOUD")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
    {
        telemetry.add_appender(CloudAppender::new(cloud_opts)).await;
    }
    let _ = di_root.register_instance(telemetry.clone());
    let _ = di_root.register_instance(config.clone());
    telemetry
        .track_json(
            "app_started",
            serde_json::json!({
                "mcp_servers": config.mcp_servers.len() as u64,
            }),
        )
        .await;

    Arc::new(ServerState {
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
        cron_fires,
        goal_mgr,
        hooks,
        skills,
        web,
        plugins,
        telemetry: telemetry.clone(),
    })
}

async fn run_server_handler<T: kkagent_rpc::transport::AsyncTransport>(
    transport: T,
    config: Arc<AppConfig>,
) {
    let state = build_server_state(config).await;
    run_server_handler_with_state(transport, state).await;
}

async fn run_server_handler_with_state<T: kkagent_rpc::transport::AsyncTransport>(
    transport: T,
    state: Arc<ServerState>,
) {
    let handler: kkagent_rpc::server::RequestHandler = {
        let state = state.clone();
        Arc::new(move |_id, method, params, event_tx| {
            let state = state.clone();
            Box::pin(async move { handle_rpc_call(state, &method, params, event_tx).await })
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

fn messages_from_records(records: &[kkagent_core::transcript::MessageRecord]) -> Vec<ChatMessage> {
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
            let workspace = params
                .as_ref()
                .and_then(|p| p.get("workspace"))
                .and_then(|v| v.as_str())
                .unwrap_or(".")
                .to_string();
            let perm_mode: PermissionMode = params
                .as_ref()
                .and_then(|p| p.get("permission_mode"))
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_else(|| {
                    state
                        .config
                        .effective_permission_mode()
                        .parse()
                        .unwrap_or_default()
                });

            let model_alias = state
                .config
                .default_model_alias()
                .unwrap_or("default")
                .to_string();

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
                let plug = state.plugins.prompt_append_all().await;
                if !plug.is_empty() {
                    session.system_prompt.push_str(&plug);
                }
            }

            {
                let db = state.transcript.lock().await;
                if let Err(e) = db.create_session(&session_id, &model_alias, &workspace) {
                    tracing::warn!("transcript create_session: {}", e);
                }
            }

            state
                .interrupt_flags
                .lock()
                .await
                .insert(session_id.clone(), session.interrupted.clone());
            state
                .model_aliases
                .lock()
                .await
                .insert(session_id.clone(), session.model_alias.clone());
            state
                .approval_txs
                .lock()
                .await
                .insert(session_id.clone(), session.approval_tx.clone());
            state
                .question_txs
                .lock()
                .await
                .insert(session_id.clone(), session.question_tx.clone());
            session.services.on_created().await;
            let session_dir = session.session_dir().display().to_string();
            state
                .sessions
                .lock()
                .await
                .insert(session_id.clone(), session);
            let _ = state
                .hooks
                .fire(
                    kkagent_mcp::hooks::HookEvent::SessionStart,
                    &serde_json::json!({
                        "session_id": session_id,
                        "workspace": workspace,
                    }),
                )
                .await;
            Ok(serde_json::json!({
                "session_id": session_id,
                "session_dir": session_dir,
            }))
        }
        "sessions.list" => {
            let limit = params
                .as_ref()
                .and_then(|p| p.get("limit"))
                .and_then(|v| v.as_u64())
                .unwrap_or(20) as usize;
            let include_archived = params
                .as_ref()
                .and_then(|p| p.get("include_archived"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            // Prefer disk session store (kimi-aligned); fall back to transcript DB.
            let store = SessionStore::open_default();
            if let Ok(summaries) = store.list(include_archived, limit) {
                if !summaries.is_empty() {
                    let list: Vec<_> = summaries
                        .into_iter()
                        .map(|s| {
                            serde_json::json!({
                                "session_id": s.id,
                                "title": s.title,
                                "working_dir": s.work_dir,
                                "session_dir": s.session_dir,
                                "archived": s.archived,
                                "last_prompt": s.last_prompt,
                                "created_at": s.created_at,
                                "updated_at": s.updated_at,
                                "forked_from": s.forked_from,
                            })
                        })
                        .collect();
                    return Ok(serde_json::json!({"sessions": list}));
                }
            }
            let db = state.transcript.lock().await;
            let sessions = db
                .list_sessions(limit)
                .map_err(|e| (-32000, e.to_string()))?;
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
        "sessions.fork" => {
            let source_id = params
                .as_ref()
                .and_then(|p| p.get("session_id"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| (-32602, "Missing session_id".into()))?;
            let target_id = params
                .as_ref()
                .and_then(|p| p.get("target_id"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            let title = params
                .as_ref()
                .and_then(|p| p.get("title"))
                .and_then(|v| v.as_str());
            let turn_index = params
                .as_ref()
                .and_then(|p| p.get("turn_index"))
                .and_then(|v| v.as_u64())
                .map(|n| n as usize);
            let store = SessionStore::open_default();
            let summary = store
                .fork(source_id, &target_id, title, turn_index)
                .map_err(|e| (-32000, e.to_string()))?;
            Ok(serde_json::json!({
                "session_id": summary.id,
                "session_dir": summary.session_dir,
                "forked_from": summary.forked_from,
                "title": summary.title,
            }))
        }
        "sessions.archive" => {
            let session_id = params
                .as_ref()
                .and_then(|p| p.get("session_id"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| (-32602, "Missing session_id".into()))?;
            let archived = params
                .as_ref()
                .and_then(|p| p.get("archived"))
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            SessionStore::open_default()
                .archive(session_id, archived)
                .map_err(|e| (-32000, e.to_string()))?;
            if let Some(session) = state.sessions.lock().await.get_mut(session_id) {
                let _ = session.services.metadata.set_archived(archived);
                if archived {
                    session.services.on_close(SessionCloseReason::Archive).await;
                }
            }
            Ok(serde_json::json!({"session_id": session_id, "archived": archived}))
        }
        "sessions.rename" => {
            let session_id = params
                .as_ref()
                .and_then(|p| p.get("session_id"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| (-32602, "Missing session_id".into()))?;
            let title = params
                .as_ref()
                .and_then(|p| p.get("title"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| (-32602, "Missing title".into()))?;
            SessionStore::open_default()
                .rename(session_id, title)
                .map_err(|e| (-32000, e.to_string()))?;
            if let Some(session) = state.sessions.lock().await.get_mut(session_id) {
                let _ = session.set_title_persisted(title);
            }
            Ok(serde_json::json!({"session_id": session_id, "title": title}))
        }
        "sessions.export" => {
            let session_id = params
                .as_ref()
                .and_then(|p| p.get("session_id"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| (-32602, "Missing session_id".into()))?;
            let store = SessionStore::open_default();
            let summary = store.get(session_id).map_err(|e| (-32000, e.to_string()))?;
            let out = params
                .as_ref()
                .and_then(|p| p.get("output_path"))
                .and_then(|v| v.as_str())
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    std::env::current_dir()
                        .unwrap_or_else(|_| PathBuf::from("."))
                        .join(kkagent_core::default_export_dir_name(session_id))
                });
            let result = kkagent_core::export_session_directory(&summary, &out)
                .map_err(|e| (-32000, e.to_string()))?;
            Ok(serde_json::json!({
                "output_dir": result.output_dir.display().to_string(),
                "entries": result.entries,
                "manifest": result.manifest,
            }))
        }
        "session.resume" => {
            let query = params
                .as_ref()
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
            let mut session = Session::resume(
                session_id.clone(),
                PathBuf::from(&record.working_dir),
                perm_mode,
                if record.model.is_empty() {
                    state
                        .config
                        .default_model_alias()
                        .unwrap_or("default")
                        .to_string()
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
            if let Some(ref t) = record.title {
                let _ = session.set_title_persisted(t.clone());
            }
            session.services.create_source = SessionCreateSource::Resume;
            session.services.on_created().await;

            state
                .interrupt_flags
                .lock()
                .await
                .insert(session_id.clone(), session.interrupted.clone());
            state
                .model_aliases
                .lock()
                .await
                .insert(session_id.clone(), session.model_alias.clone());
            state
                .approval_txs
                .lock()
                .await
                .insert(session_id.clone(), session.approval_tx.clone());
            state
                .question_txs
                .lock()
                .await
                .insert(session_id.clone(), session.question_tx.clone());
            state
                .sessions
                .lock()
                .await
                .insert(session_id.clone(), session);

            Ok(serde_json::json!({
                "session_id": session_id,
                "messages": messages,
                "plan_mode": false,
                "permission_mode": perm_mode,
                "model": record.model,
            }))
        }
        "session.prompt" => {
            let session_id = params
                .as_ref()
                .and_then(|p| p.get("session_id"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| (-32602, "Missing session_id".into()))?
                .to_string();
            let text = params
                .as_ref()
                .and_then(|p| p.get("text"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| (-32602, "Missing text".into()))?
                .to_string();

            {
                let mut sessions = state.sessions.lock().await;
                if let Some(session) = sessions.get_mut(&session_id) {
                    session.clear_interrupt();
                    // Drain due cron-fire XML into the conversation.
                    let fires = {
                        let mut g = state.cron_fires.lock().await;
                        g.drain(..).collect::<Vec<_>>()
                    };
                    for xml in fires {
                        session.add_user_message(xml);
                    }
                    // Media @path refs → blob store note.
                    let media_refs = kkagent_core::resolve_media_refs(&text, &session.working_dir);
                    if !media_refs.is_empty() {
                        let store = kkagent_core::BlobStore::session_store(&session.working_dir);
                        let mut note = String::from("<system-reminder>\nAttached media paths:\n");
                        for p in media_refs {
                            if let Ok(bytes) = std::fs::read(&p) {
                                let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("bin");
                                if let Ok((id, path)) = store.put(&bytes, ext).await {
                                    note.push_str(&format!(
                                        "- {} → blob:{id} ({})\n",
                                        p.display(),
                                        path.display()
                                    ));
                                }
                            }
                        }
                        note.push_str("</system-reminder>");
                        session.add_user_message(note);
                    }
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
                            if let Some(ChatContent::Text { text }) = pending
                                .iter()
                                .find(|m| m.role == "user")
                                .and_then(|m| m.content.first())
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

            // Build agent event channel that forwards to RPC transport + wire journal + telemetry
            let (agent_event_tx, mut agent_event_rx) = mpsc::channel::<AgentEvent>(256);

            let rpc_tx = rpc_event_tx.clone();
            let home_dir = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
            let wire_dir = home_dir.join(".kkagent").join("sessions").join(&session_id);
            let wire = kkagent_wire::WireJournal::open(&wire_dir);
            let telemetry_fwd = state.telemetry.clone();
            tokio::spawn(async move {
                let _ = wire.ensure_metadata().await;
                while let Some(evt) = agent_event_rx.recv().await {
                    let data = serde_json::to_value(&evt).unwrap_or_default();
                    let evt_type = data
                        .get("type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("agent.event")
                        .to_string();
                    let record = kkagent_wire::op_to_wire_record(
                        &evt_type,
                        data.clone(),
                        chrono::Utc::now().timestamp_millis(),
                    );
                    let _ = wire.append(&record).await;
                    match &evt {
                        AgentEvent::TurnStart { .. } => {
                            telemetry_fwd
                                .track_json("turn_start", serde_json::json!({}))
                                .await;
                        }
                        AgentEvent::TurnEnd { .. } => {
                            telemetry_fwd
                                .track_json("turn_end", serde_json::json!({}))
                                .await;
                        }
                        AgentEvent::SubagentSpawned { subagent_id, .. } => {
                            telemetry_fwd
                                .track_json(
                                    "subagent_created",
                                    serde_json::json!({"subagent_id": subagent_id}),
                                )
                                .await;
                        }
                        _ => {}
                    }
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
            let mirror_tx = agent_event_tx.clone();
            let launch: kkagent_tools::builtin::task::SubagentLaunchFn = Arc::new(move |sub_cfg| {
                let mgr = subagents.clone();
                let app_cfg = cfg_for_sub.clone();
                let id = sub_cfg.agent_id.clone();
                let mgr_abort = mgr.clone();
                let id_abort = id.clone();
                let mirror = match (
                    sub_cfg.parent_session_id.clone(),
                    sub_cfg.parent_tool_call_id.clone(),
                ) {
                    (Some(parent_session_id), Some(parent_tool_call_id)) => {
                        Some(SubagentMirrorContext {
                            parent_session_id,
                            parent_tool_call_id,
                            parent_event_tx: mirror_tx.clone(),
                        })
                    }
                    _ => None,
                };
                let join = tokio::spawn(async move {
                    tracing::info!("Subagent {} starting: {}", id, sub_cfg.description);
                    let run = kkagent_core::run_subagent_mirrored(
                        app_cfg,
                        sub_cfg,
                        PermissionMode::Auto,
                        mirror,
                    );
                    match run.await {
                        Ok(result) => {
                            tracing::info!("Subagent {} complete ({} chars)", id, result.len());
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
            // Goal tools (shared across turns)
            tools.register(Arc::new(kkagent_tools::builtin::CreateGoalTool::new(
                state.goal_mgr.clone(),
            )));
            tools.register(Arc::new(kkagent_tools::builtin::GetGoalTool::new(
                state.goal_mgr.clone(),
            )));
            tools.register(Arc::new(kkagent_tools::builtin::UpdateGoalTool::new(
                state.goal_mgr.clone(),
            )));
            tools.register(Arc::new(kkagent_tools::builtin::SetGoalBudgetTool::new(
                state.goal_mgr.clone(),
            )));
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

            let permission_rules = state
                .config
                .permission
                .as_ref()
                .map(|p| p.rules.clone())
                .unwrap_or_default();
            let perm_mode: PermissionMode = state
                .config
                .effective_permission_mode()
                .parse()
                .unwrap_or_default();
            let permission = PermissionChain::new(perm_mode, permission_rules);

            let agent_loop = Arc::new(
                AgentLoop::new(
                    state.config.clone(),
                    Arc::new(tools),
                    Arc::new(Mutex::new(permission)),
                    agent_event_tx.clone(),
                    state.abort_registry.clone(),
                )
                .with_hooks(state.hooks.clone())
                .with_goal_manager(state.goal_mgr.clone()),
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
                    let _ = agent_event_tx
                        .send(AgentEvent::Error {
                            session_id: sid.clone(),
                            message: e.to_string(),
                        })
                        .await;
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
            let session_id = params
                .as_ref()
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
            let session_id = params
                .as_ref()
                .and_then(|p| p.get("session_id"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let mode: PermissionMode = params
                .as_ref()
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
            let session_id = params
                .as_ref()
                .and_then(|p| p.get("session_id"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let enabled = params
                .as_ref()
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
            let session_id = params
                .as_ref()
                .and_then(|p| p.get("session_id"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let model = params
                .as_ref()
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
            let session_id = params
                .as_ref()
                .and_then(|p| p.get("session_id"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let title = params
                .as_ref()
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
            let session_id = params
                .as_ref()
                .and_then(|p| p.get("session_id"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| (-32602, "Missing session_id".into()))?
                .to_string();
            let count = params
                .as_ref()
                .and_then(|p| p.get("count"))
                .and_then(|v| v.as_u64())
                .unwrap_or(1) as usize;

            let (undone, keep) = {
                let mut sessions = state.sessions.lock().await;
                let session = sessions
                    .get_mut(&session_id)
                    .ok_or_else(|| (-32602, format!("Session not found: {}", session_id)))?;
                let result = kkagent_core::UndoService::undo_turns(session, count);
                if result.undone_turns == 0 {
                    return Err((-32000, "Nothing to undo".into()));
                }
                (result.undone_turns, result.message_count)
            };
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
        "skills.list" => {
            let list = state.skills.list().await;
            let items: Vec<serde_json::Value> = list
                .into_iter()
                .map(|e| {
                    serde_json::json!({"name": e.name, "description": e.description, "path": e.path.display().to_string()})
                })
                .collect();
            Ok(serde_json::json!({"skills": items}))
        }
        "plugins.list" => {
            let list = state.plugins.list().await;
            Ok(serde_json::json!({"plugins": list}))
        }
        "session.compact" => {
            let session_id = params
                .as_ref()
                .and_then(|p| p.get("session_id"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| (-32602, "Missing session_id".into()))?
                .to_string();
            let keep_last = params
                .as_ref()
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
                        digest.push_str(&format!(
                            "[{role}] {}\n",
                            text.chars().take(500).collect::<String>()
                        ));
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
        "swarm.enter" => {
            let session_id = params
                .as_ref()
                .and_then(|p| p.get("session_id"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .or_else(|| {
                    // fall back: first live session
                    None
                });
            let trigger = match params
                .as_ref()
                .and_then(|p| p.get("trigger"))
                .and_then(|v| v.as_str())
                .unwrap_or("slash")
            {
                "tool" => kkagent_core::SwarmModeTrigger::Tool,
                "auto" => kkagent_core::SwarmModeTrigger::Auto,
                _ => kkagent_core::SwarmModeTrigger::Slash,
            };
            let mut sessions = state.sessions.lock().await;
            let session = if let Some(id) = session_id {
                sessions
                    .get_mut(&id)
                    .ok_or_else(|| (-32602, format!("Session not found: {id}")))?
            } else {
                sessions
                    .values_mut()
                    .next()
                    .ok_or_else(|| (-32000, "No active session".into()))?
            };
            let reminder = session.swarm.enter(trigger);
            if let Some(r) = reminder {
                session.add_user_message(r.into());
            }
            Ok(serde_json::json!({
                "ok": true,
                "active": session.swarm.is_active(),
                "roster": session.swarm.roster().iter().map(|m| {
                    serde_json::json!({"id": m.id, "role": m.role, "status": m.status})
                }).collect::<Vec<_>>(),
            }))
        }
        "swarm.exit" => {
            let session_id = params
                .as_ref()
                .and_then(|p| p.get("session_id"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let mut sessions = state.sessions.lock().await;
            let session = if let Some(id) = session_id {
                sessions
                    .get_mut(&id)
                    .ok_or_else(|| (-32602, format!("Session not found: {id}")))?
            } else {
                sessions
                    .values_mut()
                    .next()
                    .ok_or_else(|| (-32000, "No active session".into()))?
            };
            let reminder = session.swarm.exit();
            if let Some(r) = reminder {
                session.add_user_message(r.into());
            }
            Ok(serde_json::json!({"ok": true, "active": false}))
        }
        "session.usage" => {
            let session_id = params
                .as_ref()
                .and_then(|p| p.get("session_id"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| (-32602, "Missing session_id".into()))?;
            let sessions = state.sessions.lock().await;
            let session = sessions
                .get(session_id)
                .ok_or_else(|| (-32602, format!("Session not found: {session_id}")))?;
            let snap = session.usage.snapshot();
            Ok(serde_json::json!({
                "input_tokens": snap.input_tokens,
                "output_tokens": snap.output_tokens,
                "cache_read_input_tokens": snap.cache_read_input_tokens,
                "cache_creation_input_tokens": snap.cache_creation_input_tokens,
                "steps": snap.steps,
                "turns": snap.turns,
                "cache_hit_ratio": snap.cache_hit_ratio(),
            }))
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
                if let Ok(response) =
                    serde_json::from_value::<kkagent_protocol::ApprovalResponse>(params.clone())
                {
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
        _ => Err((-32601, format!("Method not found: {}", method))),
    }
}
