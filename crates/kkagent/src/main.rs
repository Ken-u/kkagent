use anyhow::Result;
use clap::{Parser, Subcommand};
use std::collections::HashMap;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tokio::task::AbortHandle;

use kkagent_client::KkagentClient;
use kkagent_config::{load_config, AppConfig};
use kkagent_core::SubagentMirrorContext;
use kkagent_core::{
    AgentLoop, BtwTurn, PermissionChain, Session, SessionBtwService, SessionCloseReason,
    SessionCreateSource, SessionStore, TranscriptDb,
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

mod onboarding;
use onboarding::{run_config, run_doctor, run_init};

struct LocalEndpointGuard(PathBuf);

impl Drop for LocalEndpointGuard {
    fn drop(&mut self) {
        if let Err(error) = kkagent_rpc::transport::uds::remove_endpoint(&self.0) {
            tracing::warn!(path = %self.0.display(), %error, "failed to remove local endpoint");
        }
    }
}

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
        /// Enable the authenticated arbitrary-command terminal API
        #[arg(long)]
        allow_terminal_api: bool,
        /// Enable direct authenticated writes through POST /api/v1/fs
        #[arg(long)]
        allow_fs_write_api: bool,
        /// Per-token HTTP request limit per minute (0 disables limiting)
        #[arg(long, default_value_t = 600)]
        http_rate_limit: u32,
        /// Append structured HTTP audit records to this file
        #[arg(long)]
        http_audit_log: Option<PathBuf>,
    },
    /// Serve Agent Client Protocol over stdio (IDE bridge)
    Acp,
    /// Manage Kimi Code managed-account authentication
    Auth {
        #[command(subcommand)]
        command: AuthCommands,
    },
    /// Create a minimal working configuration (interactive by default)
    Init {
        /// safe | default | full-auto
        #[arg(long, default_value = "default")]
        preset: String,
        /// openai | anthropic | kimi | google | custom
        #[arg(long)]
        provider: Option<String>,
        /// Provider model id
        #[arg(long)]
        model: Option<String>,
        /// Override provider base URL
        #[arg(long)]
        base_url: Option<String>,
        /// Replace an existing config file
        #[arg(long)]
        force: bool,
        /// Never prompt; missing required values are errors
        #[arg(long)]
        non_interactive: bool,
    },
    /// Inspect and edit configuration
    Config {
        #[command(subcommand)]
        command: ConfigCommands,
    },
    /// Check configuration, provider, sandbox, storage, and common tools
    Doctor {
        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
        /// Also probe the configured provider endpoint
        #[arg(long)]
        live: bool,
    },
}

#[derive(Subcommand)]
enum ConfigCommands {
    /// Print the effective config with secrets redacted
    Show,
    /// Read a dotted config key
    Get { key: String },
    /// Set a dotted config key to a TOML value
    Set { key: String, value: String },
    /// Apply safe, default, or full-auto runtime defaults
    Preset { name: String },
}

#[derive(Subcommand)]
enum AuthCommands {
    /// Sign in with the Kimi device-code flow and provision managed models
    Login {
        #[arg(long)]
        oauth_host: Option<String>,
        #[arg(long)]
        base_url: Option<String>,
    },
    /// Remove the locally stored Kimi credential
    Logout,
    /// Show whether a Kimi credential is available
    Status,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let is_tui = cli.command.is_none() && cli.prompt.is_none();
    init_logging(is_tui)?;

    if let Some(Commands::Auth { command }) = &cli.command {
        return run_auth(command, cli.config.as_deref()).await;
    }
    match &cli.command {
        Some(Commands::Init {
            preset,
            provider,
            model,
            base_url,
            force,
            non_interactive,
        }) => {
            return run_init(
                cli.config.as_deref(),
                preset,
                provider.as_deref(),
                model.as_deref(),
                base_url.as_deref(),
                *force,
                *non_interactive,
            )
        }
        Some(Commands::Config { command }) => {
            return run_config(command, cli.config.as_deref());
        }
        Some(Commands::Doctor { json, live }) => {
            return run_doctor(cli.config.as_deref(), *json, *live).await;
        }
        _ => {}
    }

    if cli.connect.is_some() && cli.command.is_some() {
        anyhow::bail!("--connect is only valid for TUI or --prompt mode");
    }

    let config_path = cli
        .config
        .clone()
        .unwrap_or_else(kkagent_config::default_config_path);
    if !config_path.exists() {
        if cli.config.is_none() && io::stdin().is_terminal() && io::stdout().is_terminal() {
            println!("No kkagent config found. Starting first-run setup.\n");
            run_init(None, "default", None, None, None, false, false)?;
        } else {
            anyhow::bail!(
                "configuration not found at {}; run `kkagent init` first",
                config_path.display()
            );
        }
    }
    let mut config = load_config(Some(&config_path))?;
    if cli.connect.is_none() {
        hydrate_provider_oauth(&mut config).await?;
    }

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
            allow_terminal_api,
            allow_fs_write_api,
            http_rate_limit,
            http_audit_log,
        }) => {
            let mut token = http_token.or_else(|| std::env::var("KKAGENT_HTTP_TOKEN").ok());
            if http.is_some() && token.as_deref().is_none_or(str::is_empty) {
                let generated = format!("{}{}", uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
                tracing::warn!(
                    "No HTTP token was supplied. Generated one for this server process: {generated}"
                );
                token = Some(generated);
            }
            let mut scoped_tokens = HashMap::new();
            if let Ok(token) = std::env::var("KKAGENT_HTTP_READ_TOKEN") {
                if !token.trim().is_empty() {
                    scoped_tokens.insert(token, vec!["read".into()]);
                }
            }
            if let Ok(token) = std::env::var("KKAGENT_HTTP_WRITE_TOKEN") {
                if !token.trim().is_empty() {
                    scoped_tokens.insert(token, vec!["read".into(), "write".into()]);
                }
            }
            if let Ok(token) = std::env::var("KKAGENT_HTTP_TERMINAL_TOKEN") {
                if !token.trim().is_empty() {
                    scoped_tokens.insert(token, vec!["read".into(), "terminal".into()]);
                }
            }
            let security = kkagent_rpc::HttpSecurityOptions {
                scoped_tokens,
                allow_terminal_api,
                allow_fs_write_api,
                requests_per_minute: http_rate_limit,
                audit_log: Some(http_audit_log.unwrap_or_else(|| {
                    kkagent_config::default_config_dir().join("http-audit.jsonl")
                })),
            };
            run_server(config, listen, http, token, security).await
        }
        Some(Commands::Acp) => {
            let state = build_server_state(Arc::new(config)).await?;
            let server = kkagent_acp::AcpServer::with_host(Arc::new(AgentAcpHost { state }));
            server.serve_stdio().await
        }
        Some(Commands::Auth { .. }) => unreachable!("auth handled before config startup"),
        Some(Commands::Init { .. } | Commands::Config { .. } | Commands::Doctor { .. }) => {
            unreachable!("setup commands handled before runtime startup")
        }
        None => {
            if let Some(prompt) = cli.prompt {
                run_print_mode(config, prompt, permission_mode, cli.connect).await
            } else {
                run_tui(config, permission_mode, cli.plan, cli.resume, cli.connect).await
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

async fn run_auth(command: &AuthCommands, config_path: Option<&std::path::Path>) -> Result<()> {
    let storage = kkagent_oauth::FileTokenStorage::for_key("kimi-code")?;
    match command {
        AuthCommands::Login {
            oauth_host,
            base_url,
        } => {
            let flow = kkagent_oauth::kimi_oauth_config(oauth_host.as_deref());
            let device = kkagent_oauth::request_device_authorization(&flow).await?;
            let verification = device
                .verification_uri_complete
                .as_deref()
                .unwrap_or(&device.verification_uri);
            println!("Open this URL to sign in:\n{verification}");
            println!("Device code: {}", device.user_code);
            let token = kkagent_oauth::poll_device_token(&flow, &device).await?;
            storage.save(&token)?;
            let base_url = base_url
                .as_deref()
                .unwrap_or(kkagent_oauth::DEFAULT_KIMI_CODE_BASE_URL);
            provision_managed_kimi_config(
                config_path.unwrap_or(&kkagent_config::default_config_path()),
                base_url,
                oauth_host.as_deref(),
                &token.access_token,
            )
            .await?;
            println!("Kimi login succeeded and managed models were written to config.");
            Ok(())
        }
        AuthCommands::Logout => {
            storage.clear()?;
            println!("Kimi credential removed.");
            Ok(())
        }
        AuthCommands::Status => {
            let status = match storage.load_result()? {
                Some(token) if token.expires_at.is_some_and(|at| at <= chrono::Utc::now()) => {
                    "expired"
                }
                Some(_) => "authenticated",
                None => "not authenticated",
            };
            println!("Kimi OAuth: {status}");
            Ok(())
        }
    }
}

async fn hydrate_provider_oauth(config: &mut AppConfig) -> Result<()> {
    for (name, provider) in &mut config.providers {
        let Some(oauth) = provider.oauth.as_ref() else {
            continue;
        };
        if provider
            .api_key
            .as_deref()
            .is_some_and(|key| !key.is_empty())
        {
            continue;
        }
        let storage = kkagent_oauth::FileTokenStorage::for_key(&oauth.key)?;
        let token = kkagent_oauth::load_fresh_kimi_token(&storage, oauth.oauth_host.as_deref())
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "provider {name} requires Kimi OAuth login; run `kkagent auth login`"
                )
            })?;
        provider.api_key = Some(token.access_token);
        for (header, value) in kkagent_oauth::kimi_identity_headers() {
            provider.custom_headers.entry(header).or_insert(value);
        }
    }
    Ok(())
}

async fn provision_managed_kimi_config(
    config_path: &std::path::Path,
    base_url: &str,
    oauth_host: Option<&str>,
    access_token: &str,
) -> Result<()> {
    let base_url = base_url.trim_end_matches('/');
    let mut request = reqwest::Client::new()
        .get(format!("{base_url}/models"))
        .bearer_auth(access_token)
        .header("accept", "application/json");
    for (name, value) in kkagent_oauth::kimi_identity_headers() {
        request = request.header(name, value);
    }
    let response = request.send().await?;
    let status = response.status();
    let body = response.text().await?;
    if !status.is_success() {
        anyhow::bail!("Kimi model catalog failed with HTTP {status}: {body}");
    }
    let payload: serde_json::Value = serde_json::from_str(&body)?;
    let models = payload
        .get("data")
        .and_then(|data| data.as_array())
        .ok_or_else(|| anyhow::anyhow!("Kimi model catalog response has no data array"))?;
    let mut config: AppConfig = if config_path.exists() {
        toml::from_str(&std::fs::read_to_string(config_path)?)?
    } else {
        AppConfig::default()
    };
    let provider_name = "managed:kimi-code".to_string();
    config.providers.insert(
        provider_name.clone(),
        kkagent_config::ProviderConfig {
            provider_type: "kimi".into(),
            api_key: None,
            base_url: Some(base_url.into()),
            custom_headers: HashMap::new(),
            oauth: Some(kkagent_config::ProviderOAuthConfig {
                storage: "file".into(),
                key: "kimi-code".into(),
                oauth_host: oauth_host.map(str::to_string),
            }),
        },
    );
    let mut first_alias = None;
    for model in models {
        let Some(id) = model.get("id").and_then(|id| id.as_str()) else {
            continue;
        };
        let context_length = model
            .get("context_length")
            .and_then(|value| value.as_u64())
            .filter(|value| *value > 0)
            .ok_or_else(|| anyhow::anyhow!("Kimi model {id} has no positive context_length"))?;
        let mut capabilities = Vec::new();
        if model
            .get("supports_tool_use")
            .and_then(|value| value.as_bool())
            .unwrap_or(true)
        {
            capabilities.push("tool_use".into());
        }
        if model
            .get("supports_reasoning")
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
        {
            capabilities.push("thinking".into());
        }
        if model
            .get("supports_image_in")
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
        {
            capabilities.push("image_in".into());
        }
        if model
            .get("supports_video_in")
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
        {
            capabilities.push("video_in".into());
        }
        let alias = format!("kimi-code/{id}");
        first_alias.get_or_insert_with(|| alias.clone());
        config.models.insert(
            alias,
            kkagent_config::ModelConfig {
                provider: provider_name.clone(),
                model: id.into(),
                max_context_size: Some(context_length),
                max_output_size: None,
                capabilities,
                display_name: model
                    .get("display_name")
                    .and_then(|value| value.as_str())
                    .map(str::to_string),
                support_efforts: model
                    .get("think_efforts")
                    .and_then(|value| value.get("valid_efforts"))
                    .and_then(|value| value.as_array())
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(|value| value.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default(),
                default_effort: model
                    .get("think_efforts")
                    .and_then(|value| value.get("default_effort"))
                    .and_then(|value| value.as_str())
                    .map(str::to_string),
            },
        );
    }
    let first_alias = first_alias.ok_or_else(|| anyhow::anyhow!("Kimi returned no models"))?;
    if config.default_model.is_none()
        || config
            .default_model
            .as_deref()
            .is_some_and(|model| model.starts_with("kimi-code/"))
    {
        config.default_model = Some(first_alias);
    }
    config.validate()?;
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_private_config(config_path, toml::to_string_pretty(&config)?.as_bytes())?;
    Ok(())
}

fn write_private_config(path: &std::path::Path, contents: &[u8]) -> Result<()> {
    let tmp = path.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp)?;
    use std::io::Write;
    file.write_all(contents)?;
    file.sync_all()?;
    drop(file);
    #[cfg(windows)]
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    if let Err(error) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error.into());
    }
    Ok(())
}

async fn run_tui(
    config: AppConfig,
    permission_mode: PermissionMode,
    plan_mode: bool,
    resume: Option<String>,
    connect: Option<String>,
) -> Result<()> {
    // Default TUI: 1:1 in-process pair via memory duplex.
    // This process owns both ends — quitting the TUI aborts the paired server
    // task so a subsequent `kkagent` never talks to a leftover in-process agent.
    // Standalone `kkagent server` (UDS) is a separate process with its own lifetime;
    // only that mode can outlive a TUI, and only when you explicitly start it.
    let (event_tx, event_rx) = mpsc::channel::<Frame>(256);
    let (rpc_client, server_handle) = if let Some(endpoint) = connect {
        let stream =
            kkagent_rpc::transport::uds::connect_uds(std::path::Path::new(&endpoint)).await?;
        (RpcClient::new(stream, event_tx), None)
    } else {
        let (client_stream, server_stream) = create_memory_pair();
        let server_config = Arc::new(config.clone());
        let handle =
            tokio::spawn(async move { run_server_handler(server_stream, server_config).await });
        tokio::task::yield_now().await;
        (RpcClient::new(client_stream, event_tx), Some(handle))
    };

    let mut tui_config = config;
    tui_config.default_permission_mode = Some(permission_mode.to_string());
    tui_config.default_plan_mode = plan_mode;

    let client = KkagentClient::new(rpc_client, event_rx);
    let app = TuiApp::new(tui_config, client);
    let result = app.run(resume).await;

    // Drop the paired server (and any in-flight agent/LLM tasks it owns).
    if let Some(server_handle) = server_handle {
        server_handle.abort();
        let _ = server_handle.await;
    }

    result
}

async fn run_print_mode(
    config: AppConfig,
    prompt: String,
    permission_mode: PermissionMode,
    connect: Option<String>,
) -> Result<()> {
    let (event_tx, event_rx) = mpsc::channel::<Frame>(256);
    let (rpc_client, server_handle) = if let Some(endpoint) = connect {
        let stream =
            kkagent_rpc::transport::uds::connect_uds(std::path::Path::new(&endpoint)).await?;
        (RpcClient::new(stream, event_tx), None)
    } else {
        let (client_stream, server_stream) = create_memory_pair();
        let config_arc = Arc::new(config.clone());
        let handle =
            tokio::spawn(async move { run_server_handler(server_stream, config_arc).await });
        (RpcClient::new(client_stream, event_tx), Some(handle))
    };

    let mut client = KkagentClient::new(rpc_client, event_rx);
    let result = run_print_client(&mut client, prompt, permission_mode).await;
    if let Some(server_handle) = server_handle {
        server_handle.abort();
        let _ = server_handle.await;
    }
    result
}

async fn run_print_client(
    client: &mut KkagentClient,
    prompt: String,
    permission_mode: PermissionMode,
) -> Result<()> {
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
    http_security: kkagent_rpc::HttpSecurityOptions,
) -> Result<()> {
    let socket_path = listen.unwrap_or_else(|| {
        let dir = kkagent_config::default_config_dir();
        dir.join("server.sock").to_string_lossy().to_string()
    });

    tracing::info!("Starting server on {}", socket_path);
    let endpoint_path = std::path::PathBuf::from(&socket_path);
    let listener = kkagent_rpc::transport::uds::bind_uds(&endpoint_path)?;
    let _endpoint_guard = LocalEndpointGuard(endpoint_path.clone());
    let config_arc = Arc::new(config);
    let state = build_server_state(config_arc.clone()).await?;

    let mut recovery_backend = None;
    let mut http_ready = None;
    let http_handle = if let Some(addr) = http {
        let concrete_backend = Arc::new(AgentHttpBackend {
            state: state.clone(),
        });
        recovery_backend = Some(concrete_backend.clone());
        let backend: Arc<dyn kkagent_rpc::HttpBackend> = concrete_backend;
        let token = http_token;
        let durable_http = state.durable_http.clone();
        let http_listener = kkagent_rpc::bind_http(&addr, token.as_deref()).await?;
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        http_ready = Some(ready_rx);
        Some(tokio::spawn(async move {
            if let Err(e) = kkagent_rpc::serve_http_listener_with_backend_security_and_persistence(
                http_listener,
                backend,
                token,
                http_security,
                durable_http,
                Some(ready_tx),
            )
            .await
            {
                tracing::error!("HTTP server error: {e}");
            }
        }))
    } else {
        None
    };
    if let Some(ready) = http_ready {
        ready
            .await
            .map_err(|_| anyhow::anyhow!("HTTP server stopped before initialization"))?;
    }
    if let Some(backend) = recovery_backend {
        recover_durable_turns(backend).await;
    }

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let st = state.clone();
                tokio::spawn(async move {
                    run_server_handler_with_state(stream, st).await;
                });
            }
            signal = tokio::signal::ctrl_c() => {
                signal?;
                tracing::info!("Shutdown signal received");
                break;
            }
        }
    }
    if let Some(handle) = http_handle {
        handle.abort();
        let _ = handle.await;
    }
    state.shutdown().await;
    Ok(())
}

struct AgentHttpBackend {
    state: Arc<ServerState>,
}

struct AgentAcpHost {
    state: Arc<ServerState>,
}

async fn initialize_session_context(state: &ServerState, session: &mut Session) {
    session.image_config = state.config.image.clone();
    session.inject_date_reminder();
    session.inject_workspace_instructions().await;
    session.inject_git_context();
    let skill_section = state
        .skills
        .catalog_prompt_section_for(&session.working_dir)
        .await;
    if !skill_section.is_empty() {
        session.system_prompt.push_str(&skill_section);
    }
    let plugin_section = state.plugins.prompt_append_all().await;
    if !plugin_section.is_empty() {
        session.system_prompt.push_str(&plugin_section);
    }
}

async fn ensure_http_session_loaded(
    state: &Arc<ServerState>,
    session_id: &str,
) -> Result<(), String> {
    if state.sessions.lock().await.contains_key(session_id) {
        return Ok(());
    }
    let (record, messages) = {
        let database = state.transcript.lock().await;
        let record = database
            .get_session(session_id)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "session not found".to_string())?;
        let records = database
            .load_messages(session_id)
            .map_err(|error| error.to_string())?;
        (record, messages_from_records(&records))
    };
    let permission_mode = state
        .config
        .effective_permission_mode()
        .parse()
        .unwrap_or_default();
    let model = if record.model.is_empty() {
        state
            .config
            .default_model_alias()
            .unwrap_or("default")
            .to_string()
    } else {
        record.model
    };
    let mut session = Session::resume(
        session_id.to_string(),
        PathBuf::from(record.working_dir),
        permission_mode,
        model,
    );
    initialize_session_context(state, &mut session).await;
    session.messages = messages;
    session.persisted_message_count = session.messages.len();
    if let Some(title) = record.title {
        let _ = session.set_title_persisted(title);
    }
    session.services.create_source = SessionCreateSource::Resume;
    session.services.on_created().await;
    state
        .interrupt_flags
        .lock()
        .await
        .insert(session_id.to_string(), session.interrupted.clone());
    state
        .model_aliases
        .lock()
        .await
        .insert(session_id.to_string(), session.model_alias.clone());
    state
        .approval_txs
        .lock()
        .await
        .insert(session_id.to_string(), session.approval_tx.clone());
    state
        .question_txs
        .lock()
        .await
        .insert(session_id.to_string(), session.question_tx.clone());
    state
        .sessions
        .lock()
        .await
        .insert(session_id.to_string(), session);
    Ok(())
}

async fn recover_durable_turns(backend: Arc<AgentHttpBackend>) {
    let turns = match backend.state.durable_http.recoverable_turns() {
        Ok(turns) => turns,
        Err(error) => {
            tracing::error!("Failed to load durable turn queue: {error}");
            return;
        }
    };
    for turn in turns {
        tracing::warn!(task_id = %turn.task_id, session_id = %turn.session_id, attempts = turn.attempts, "Recovering durable turn");
        let (text, images) = turn.message_input();
        if let Err(error) = kkagent_rpc::HttpBackend::post_message(
            backend.as_ref(),
            &turn.session_id,
            &text,
            &images,
            Some(&turn.task_id),
        )
        .await
        {
            let _ = backend
                .state
                .durable_http
                .finish_turn(&turn.task_id, "failed", Some(&error));
        }
    }
}

async fn recover_subagents(state: Arc<ServerState>) {
    let configs = match state.subagents.recoverable_configs().await {
        Ok(configs) => configs,
        Err(error) => {
            tracing::error!("Failed to load durable subagents: {error}");
            return;
        }
    };
    for config in configs {
        let agent_id = config.agent_id.clone();
        if let Err(error) = state.subagents.resume(&agent_id).await {
            tracing::error!("Failed to claim recovered subagent {agent_id}: {error}");
            continue;
        }
        let manager = state.subagents.clone();
        let app_config = state.config.clone();
        let abort_manager = manager.clone();
        let abort_agent_id = agent_id.clone();
        let join = tokio::spawn(async move {
            match kkagent_core::run_subagent_mirrored(
                app_config,
                config,
                PermissionMode::Auto,
                None,
            )
            .await
            {
                Ok(result) => manager.complete(&agent_id, result).await,
                Err(error) => manager.fail(&agent_id, error.to_string()).await,
            }
        });
        let abort = join.abort_handle();
        abort_manager.set_abort_handle(&abort_agent_id, abort).await;
    }
}

async fn fire_session_hook(
    state: &ServerState,
    event: kkagent_mcp::HookEvent,
    session_id: &str,
    workspace: &Path,
) {
    let _ = state
        .hooks
        .fire(
            event,
            &serde_json::json!({
                "session_id": session_id,
                "workspace": workspace,
            }),
        )
        .await;
}

#[async_trait::async_trait]
impl kkagent_acp::AcpHost for AgentAcpHost {
    async fn create_session(&self, session_id: &str, cwd: &str) -> Result<(), String> {
        if session_id.is_empty() {
            return Err("session id must not be empty".into());
        }
        let working_dir = std::fs::canonicalize(cwd)
            .map_err(|e| format!("invalid ACP working directory {cwd}: {e}"))?;
        let model = self
            .state
            .config
            .default_model_alias()
            .ok_or_else(|| "default_model is not configured".to_string())?
            .to_string();
        let permission_mode = self
            .state
            .config
            .effective_permission_mode()
            .parse()
            .map_err(|_| "invalid default permission mode".to_string())?;
        let mut session = Session::new(
            session_id.to_string(),
            working_dir.clone(),
            permission_mode,
            model.clone(),
        );
        initialize_session_context(&self.state, &mut session).await;
        {
            let db = self.state.transcript.lock().await;
            db.create_session(session_id, &model, &working_dir.to_string_lossy())
                .map_err(|e| e.to_string())?;
        }
        session.services.on_created().await;
        fire_session_hook(
            &self.state,
            kkagent_mcp::HookEvent::SessionStart,
            &session.id,
            &session.working_dir,
        )
        .await;
        self.state
            .interrupt_flags
            .lock()
            .await
            .insert(session_id.to_string(), session.interrupted.clone());
        self.state
            .model_aliases
            .lock()
            .await
            .insert(session_id.to_string(), session.model_alias.clone());
        self.state
            .approval_txs
            .lock()
            .await
            .insert(session_id.to_string(), session.approval_tx.clone());
        self.state
            .question_txs
            .lock()
            .await
            .insert(session_id.to_string(), session.question_tx.clone());
        self.state
            .sessions
            .lock()
            .await
            .insert(session_id.to_string(), session);
        Ok(())
    }

    async fn prompt(&self, session_id: &str, text: &str) -> Result<serde_json::Value, String> {
        if text.trim().is_empty() {
            return Err("prompt must not be empty".into());
        }
        let _turn_permit = self.state.turn_locks.try_acquire(session_id).await?;
        {
            let mut sessions = self.state.sessions.lock().await;
            let session = sessions
                .get_mut(session_id)
                .ok_or_else(|| "session not found".to_string())?;
            session.add_user_message(text.to_string());
        }
        run_http_turn(self.state.clone(), session_id, None)
            .await
            .map_err(|e| e.to_string())?;
        let sessions = self.state.sessions.lock().await;
        let session = sessions
            .get(session_id)
            .ok_or_else(|| "session disappeared after turn".to_string())?;
        let output = session
            .messages
            .iter()
            .rev()
            .find(|message| message.role == "assistant")
            .map(|message| {
                message
                    .content
                    .iter()
                    .filter_map(|part| match part {
                        ChatContent::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        Ok(serde_json::json!({
            "stopReason": "end_turn",
            "sessionId": session_id,
            "content": [{"type": "text", "text": output}],
        }))
    }

    async fn cancel(&self, session_id: &str) -> Result<(), String> {
        let flag = self
            .state
            .interrupt_flags
            .lock()
            .await
            .get(session_id)
            .cloned()
            .ok_or_else(|| "session not found".to_string())?;
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(handle) = self.state.abort_registry.lock().await.remove(session_id) {
            handle.abort();
        }
        Ok(())
    }

    async fn set_mode(&self, session_id: &str, mode: &str) -> Result<(), String> {
        let mut sessions = self.state.sessions.lock().await;
        let session = sessions
            .get_mut(session_id)
            .ok_or_else(|| "session not found".to_string())?;
        match mode {
            "agent" => session.plan_mode = false,
            "plan" => session.plan_mode = true,
            "manual" => session.permission_mode = PermissionMode::Manual,
            "yolo" => session.permission_mode = PermissionMode::Yolo,
            "auto" => session.permission_mode = PermissionMode::Auto,
            _ => return Err(format!("unsupported ACP mode: {mode}")),
        }
        Ok(())
    }

    async fn set_model(&self, session_id: &str, model: &str) -> Result<(), String> {
        if self.state.config.resolve_model(model).is_none() {
            return Err(format!("unknown model alias: {model}"));
        }
        let aliases = self.state.model_aliases.lock().await;
        let alias = aliases
            .get(session_id)
            .ok_or_else(|| "session not found".to_string())?;
        *alias.lock().unwrap_or_else(|e| e.into_inner()) = model.to_string();
        Ok(())
    }

    async fn respond_approval(&self, params: &serde_json::Value) -> Result<(), String> {
        let id = params
            .get("approvalId")
            .or_else(|| params.get("approval_id"))
            .and_then(|value| value.as_str())
            .ok_or_else(|| "missing approvalId".to_string())?;
        let decision = match params
            .get("decision")
            .and_then(|value| value.as_str())
            .unwrap_or("cancelled")
        {
            "approved" | "approve" | "allow" => kkagent_protocol::ApprovalDecision::Approved,
            "rejected" | "reject" | "deny" => kkagent_protocol::ApprovalDecision::Rejected,
            _ => kkagent_protocol::ApprovalDecision::Cancelled,
        };
        let response = kkagent_protocol::ApprovalResponse {
            approval_id: id.to_string(),
            decision,
            scope: None,
            feedback: params
                .get("feedback")
                .and_then(|value| value.as_str())
                .map(str::to_string),
            selected_label: params
                .get("selected_label")
                .or_else(|| params.get("selectedLabel"))
                .and_then(|value| value.as_str())
                .map(str::to_string),
        };
        let senders = self.state.approval_txs.lock().await;
        for sender in senders.values() {
            let _ = sender.try_send(response.clone());
        }
        Ok(())
    }

    async fn respond_question(&self, params: &serde_json::Value) -> Result<(), String> {
        let id = params
            .get("questionId")
            .or_else(|| params.get("question_id"))
            .and_then(|value| value.as_str())
            .ok_or_else(|| "missing questionId".to_string())?;
        let response = kkagent_protocol::QuestionResponse {
            question_id: id.to_string(),
            selected_option_ids: params
                .get("selectedOptionIds")
                .or_else(|| params.get("selected_option_ids"))
                .and_then(|value| value.as_array())
                .map(|values| {
                    values
                        .iter()
                        .filter_map(|value| value.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            free_text: params
                .get("freeText")
                .or_else(|| params.get("free_text"))
                .and_then(|value| value.as_str())
                .map(str::to_string),
            cancelled: params
                .get("cancelled")
                .and_then(|value| value.as_bool())
                .unwrap_or(false),
        };
        let senders = self.state.question_txs.lock().await;
        for sender in senders.values() {
            let _ = sender.try_send(response.clone());
        }
        Ok(())
    }

    async fn list_models(&self) -> serde_json::Value {
        let models = self
            .state
            .config
            .models
            .iter()
            .map(|(alias, model)| {
                serde_json::json!({
                    "id": alias,
                    "model": model.model,
                    "provider": model.provider,
                    "capabilities": model.capabilities,
                })
            })
            .collect::<Vec<_>>();
        serde_json::json!({"models": models})
    }

    async fn list_mcp(&self) -> serde_json::Value {
        let tool_count = self.state.mcp.list_tools().await.len();
        serde_json::json!({
            "servers": self.state.config.mcp_servers.keys().collect::<Vec<_>>(),
            "toolCount": tool_count,
        })
    }

    fn subscribe_events(&self) -> Option<tokio::sync::broadcast::Receiver<serde_json::Value>> {
        Some(self.state.events.subscribe())
    }
}

#[async_trait::async_trait]
impl kkagent_rpc::HttpBackend for AgentHttpBackend {
    fn event_sender(&self) -> Option<tokio::sync::broadcast::Sender<serde_json::Value>> {
        Some(self.state.events.clone())
    }

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
    ) -> Result<serde_json::Value, String> {
        let id = uuid::Uuid::new_v4().to_string();
        let requested = workspace
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let cwd = std::fs::canonicalize(&requested)
            .map_err(|error| format!("invalid workspace {}: {error}", requested.display()))?;
        if !kkagent_core::is_workspace_trusted(&self.state.config, &cwd) {
            return Err(format!(
                "workspace {} is outside trusted_workspaces",
                cwd.display()
            ));
        }
        let model = self
            .state
            .config
            .default_model_alias()
            .ok_or_else(|| "default_model is not configured".to_string())?
            .to_string();
        let mut session = Session::new(
            id.clone(),
            cwd.clone(),
            PermissionMode::Manual,
            model.clone(),
        );
        initialize_session_context(&self.state, &mut session).await;
        if let Some(ref t) = title {
            session
                .set_title_persisted(t.clone())
                .map_err(|error| error.to_string())?;
        }
        {
            let db = self.state.transcript.lock().await;
            db.create_session(&id, &model, &cwd.to_string_lossy())
                .map_err(|error| error.to_string())?;
            if let Some(ref t) = title {
                db.set_title(&id, t).map_err(|error| error.to_string())?;
            }
        }
        session.services.on_created().await;
        fire_session_hook(
            &self.state,
            kkagent_mcp::HookEvent::SessionStart,
            &session.id,
            &session.working_dir,
        )
        .await;
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
        Ok(serde_json::json!({
            "session_id": id,
            "workspace": cwd.display().to_string(),
            "title": title,
            "session_dir": session_dir,
            "created_at": chrono::Utc::now().to_rfc3339(),
        }))
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

    async fn delete_session(&self, id: &str) -> Result<(), String> {
        let _turn_permit = self.state.turn_locks.try_acquire(id).await?;
        let session = self.state.sessions.lock().await.remove(id);
        if session.is_none() {
            let exists = self
                .state
                .transcript
                .lock()
                .await
                .get_session(id)
                .map_err(|e| e.to_string())?
                .is_some();
            if !exists {
                return Err("session not found".into());
            }
        }
        self.state.interrupt_flags.lock().await.remove(id);
        self.state.model_aliases.lock().await.remove(id);
        self.state.approval_txs.lock().await.remove(id);
        self.state.question_txs.lock().await.remove(id);
        self.state.turn_locks.remove(id).await;
        self.state
            .transcript
            .lock()
            .await
            .archive_session(id)
            .map_err(|e| e.to_string())?;
        if let Some(session) = session.as_ref() {
            fire_session_hook(
                &self.state,
                kkagent_mcp::HookEvent::SessionEnd,
                &session.id,
                &session.working_dir,
            )
            .await;
        }
        Ok(())
    }

    async fn post_message(
        &self,
        id: &str,
        text: &str,
        images: &[kkagent_rpc::HttpImageInput],
        task_id: Option<&str>,
    ) -> Result<serde_json::Value, String> {
        // Queue user message and run AgentLoop turn on shared ServerState.
        if text.trim().is_empty() && images.is_empty() {
            return Err("message text must not be empty".into());
        }
        ensure_http_session_loaded(&self.state, id).await?;
        let turn_permit = self.state.turn_locks.try_acquire(id).await?;
        if let Some(task_id) = task_id {
            self.state
                .durable_http
                .claim_turn(task_id)
                .map_err(|error| error.to_string())?;
        }
        let mut sessions = self.state.sessions.lock().await;
        let session = sessions
            .get_mut(id)
            .ok_or_else(|| "session not found".to_string())?;
        session
            .add_user_message_with_images(
                text.to_string(),
                images
                    .iter()
                    .map(|image| (image.media_type.clone(), image.data.clone()))
                    .collect(),
            )
            .map_err(|error| format!("invalid image input: {error}"))?;
        let n = session.messages.len();
        drop(sessions);
        let state = self.state.clone();
        let sid = id.to_string();
        let durable_task_id = task_id.map(str::to_string);
        tokio::spawn(async move {
            let _turn_permit = turn_permit;
            let result = run_http_turn(state.clone(), &sid, durable_task_id.clone()).await;
            if let Some(task_id) = durable_task_id {
                match &result {
                    Ok(()) => {
                        if let Err(error) =
                            state.durable_http.finish_turn(&task_id, "completed", None)
                        {
                            tracing::error!("Failed to complete durable task {task_id}: {error}");
                        }
                    }
                    Err(error) => {
                        let _ = state.durable_http.finish_turn(
                            &task_id,
                            "failed",
                            Some(&error.to_string()),
                        );
                    }
                }
            }
            if let Err(e) = result {
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
        let turns = self
            .state
            .durable_http
            .list_turns(200)
            .unwrap_or_default()
            .into_iter()
            .map(|turn| {
                serde_json::json!({
                    "task_id": turn.task_id,
                    "session_id": turn.session_id,
                    "description": "agent turn",
                    "status": turn.state,
                    "attempts": turn.attempts,
                    "updated_at": turn.updated_at,
                    "error": turn.error,
                    "kind": "turn",
                })
            })
            .collect::<Vec<_>>();
        serde_json::json!({"tasks": tasks, "turns": turns})
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
            "sandbox": self.state.sandbox_policy.mode_name(),
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
            selected_label: None,
        };
        let txs = self.state.approval_txs.lock().await;
        for tx in txs.values() {
            let _ = tx.try_send(resp.clone());
        }
        Ok(serde_json::json!({"ok": true, "approval_id": id}))
    }

    async fn cancel_turn(&self, task_id: &str) -> Result<serde_json::Value, String> {
        let turn = self
            .state
            .durable_http
            .get_turn(task_id)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "task not found".to_string())?;
        let cancelled = self
            .state
            .durable_http
            .cancel_turn(task_id)
            .map_err(|error| error.to_string())?;
        if let Some(flag) = self
            .state
            .interrupt_flags
            .lock()
            .await
            .get(&turn.session_id)
            .cloned()
        {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        if let Some(handle) = self
            .state
            .abort_registry
            .lock()
            .await
            .remove(&turn.session_id)
        {
            handle.abort();
        }
        Ok(serde_json::json!({
            "ok": true,
            "task_id": cancelled.task_id,
            "session_id": cancelled.session_id,
            "state": cancelled.state,
        }))
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

    async fn list_files(&self, path: &str) -> Result<serde_json::Value, String> {
        let path = resolve_http_fs_path(&self.state.config, path, false)?;
        let mut directory = tokio::fs::read_dir(&path)
            .await
            .map_err(|error| error.to_string())?;
        let mut entries = Vec::new();
        while entries.len() < 200 {
            let Some(entry) = directory
                .next_entry()
                .await
                .map_err(|error| error.to_string())?
            else {
                break;
            };
            let file_type = entry.file_type().await.map_err(|error| error.to_string())?;
            entries.push(serde_json::json!({
                "name": entry.file_name().to_string_lossy(),
                "path": entry.path().display().to_string(),
                "is_dir": file_type.is_dir(),
            }));
        }
        Ok(serde_json::json!({"entries": entries}))
    }

    async fn search(&self, query: &str) -> serde_json::Value {
        let needle = query.to_lowercase();
        let sessions = self.state.sessions.lock().await;
        let mut hits = Vec::new();
        for session in sessions.values() {
            for (index, message) in session.messages.iter().enumerate() {
                for part in &message.content {
                    let text = match part {
                        ChatContent::Text { text } | ChatContent::Thinking { thinking: text } => {
                            text
                        }
                        ChatContent::ToolResult { content, .. } => content,
                        ChatContent::ToolUse { .. }
                        | ChatContent::Image { .. }
                        | ChatContent::Video { .. } => continue,
                    };
                    if text.to_lowercase().contains(&needle) {
                        hits.push(serde_json::json!({
                            "session_id": session.id,
                            "message_index": index,
                            "role": message.role,
                            "preview": text.chars().take(240).collect::<String>(),
                        }));
                        if hits.len() == 100 {
                            break;
                        }
                    }
                }
            }
        }
        serde_json::json!({"query": query, "hits": hits})
    }

    async fn workspace_info(&self) -> serde_json::Value {
        serde_json::json!({
            "cwd": std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_else(|_| ".".into()),
            "sessions": self.state.sessions.lock().await.len(),
        })
    }

    async fn list_questions(&self) -> serde_json::Value {
        let questions = self.state.pending_questions.lock().await;
        serde_json::json!({"questions": questions.values().cloned().collect::<Vec<_>>()})
    }

    async fn answer_question(
        &self,
        id: &str,
        response: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let pending = self
            .state
            .pending_questions
            .lock()
            .await
            .remove(id)
            .ok_or_else(|| "question not found".to_string())?;
        let session_id = pending
            .get("session_id")
            .and_then(|value| value.as_str())
            .ok_or_else(|| "pending question has no session".to_string())?;
        let question_response = kkagent_protocol::QuestionResponse {
            question_id: id.to_string(),
            selected_option_ids: response
                .get("selected_option_ids")
                .or_else(|| response.get("selectedOptionIds"))
                .and_then(|value| value.as_array())
                .map(|values| {
                    values
                        .iter()
                        .filter_map(|value| value.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            free_text: response
                .get("free_text")
                .or_else(|| response.get("freeText"))
                .and_then(|value| value.as_str())
                .map(str::to_string),
            cancelled: response
                .get("cancelled")
                .and_then(|value| value.as_bool())
                .unwrap_or(false),
        };
        let senders = self.state.question_txs.lock().await;
        let sender = senders
            .get(session_id)
            .ok_or_else(|| "session question channel not found".to_string())?;
        sender
            .try_send(question_response)
            .map_err(|e| e.to_string())?;
        Ok(serde_json::json!({"ok": true, "question_id": id}))
    }

    async fn health(&self) -> serde_json::Value {
        let persistence_responding = self.state.transcript.lock().await.list_sessions(1).is_ok();
        let task_persistence_responding = self.state.durable_http.list_turns(1).is_ok();
        serde_json::json!({
            "status": "ok",
            "uptime_seconds": self.state.started_at.elapsed().as_secs(),
            "persistence": {
                "durable": self.state.persistence_durable,
                "responding": persistence_responding,
                "tasks_responding": task_persistence_responding,
                "error": self.state.persistence_error,
            },
            "sessions": self.state.sessions.lock().await.len(),
            "mcp_servers": self.state.config.mcp_servers.len(),
            "sandbox": {
                "mode": self.state.sandbox_policy.mode_name(),
                "network": self.state.sandbox_policy.network,
            },
        })
    }

    async fn readiness(&self) -> Result<serde_json::Value, String> {
        if !self.state.persistence_durable {
            return Err(self
                .state
                .persistence_error
                .clone()
                .unwrap_or_else(|| "transcript persistence is not durable".into()));
        }
        self.state
            .transcript
            .lock()
            .await
            .list_sessions(1)
            .map_err(|error| format!("transcript persistence unavailable: {error}"))?;
        self.state
            .durable_http
            .list_turns(1)
            .map_err(|error| format!("task persistence unavailable: {error}"))?;
        Ok(serde_json::json!({
            "status": "ready",
            "persistence": "durable",
        }))
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

async fn build_turn_tool_registry(
    state: &Arc<ServerState>,
    event_tx: mpsc::Sender<AgentEvent>,
) -> ToolRegistry {
    let mut tools = ToolRegistry::new();
    kkagent_tools::register_builtin_tools(&mut tools);
    let auto_background_on_timeout = state
        .config
        .background
        .as_ref()
        .and_then(|background| background.bash_auto_background_on_timeout)
        .unwrap_or(true);
    tools.register(Arc::new(kkagent_tools::builtin::BashTool::new(
        state.bash_shells.clone(),
        kkagent_tools::builtin::BashOptions {
            auto_background_on_timeout,
            sandbox: state.sandbox_policy.clone(),
        },
    )));
    register_mcp_tools(&mut tools, &state.mcp).await;

    let subagents = state.subagents.clone();
    let config = state.config.clone();
    let launch: kkagent_tools::builtin::task::SubagentLaunchFn = Arc::new(move |sub_config| {
        let manager = subagents.clone();
        let app_config = config.clone();
        let agent_id = sub_config.agent_id.clone();
        let abort_manager = manager.clone();
        let abort_agent_id = agent_id.clone();
        let mirror = match (
            sub_config.parent_session_id.clone(),
            sub_config.parent_tool_call_id.clone(),
        ) {
            (Some(parent_session_id), Some(parent_tool_call_id)) => Some(SubagentMirrorContext {
                parent_session_id,
                parent_tool_call_id,
                parent_event_tx: event_tx.clone(),
            }),
            _ => None,
        };
        let join = tokio::spawn(async move {
            tracing::info!("Subagent {} starting: {}", agent_id, sub_config.description);
            match kkagent_core::run_subagent_mirrored(
                app_config,
                sub_config,
                PermissionMode::Auto,
                mirror,
            )
            .await
            {
                Ok(result) => {
                    tracing::info!("Subagent {} complete ({} chars)", agent_id, result.len());
                    manager.complete(&agent_id, result).await;
                }
                Err(error) => {
                    tracing::error!("Subagent {} failed: {}", agent_id, error);
                    manager.fail(&agent_id, error.to_string()).await;
                }
            }
        });
        let abort = join.abort_handle();
        tokio::spawn(async move {
            abort_manager.set_abort_handle(&abort_agent_id, abort).await;
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
    tools
}

async fn run_http_turn(
    state: Arc<ServerState>,
    session_id: &str,
    durable_task_id: Option<String>,
) -> anyhow::Result<()> {
    let (event_tx, mut event_rx) = mpsc::channel::<AgentEvent>(64);
    let live_events = state.events.clone();
    let event_state = state.clone();
    tokio::spawn(async move {
        while let Some(event) = event_rx.recv().await {
            if matches!(event, AgentEvent::ApprovalRequested { .. }) {
                if let Some(task_id) = durable_task_id.as_deref() {
                    let _ = event_state
                        .durable_http
                        .finish_turn(task_id, "waiting_approval", None);
                }
            }
            if let AgentEvent::QuestionAsked {
                session_id,
                question,
            } = &event
            {
                event_state.pending_questions.lock().await.insert(
                    question.question_id.clone(),
                    serde_json::json!({
                        "session_id": session_id,
                        "question": question,
                    }),
                );
            }
            if let Ok(value) = serde_json::to_value(event) {
                let _ = live_events.send(value);
            }
        }
    });

    let mut session = {
        let mut sessions = state.sessions.lock().await;
        sessions
            .remove(session_id)
            .ok_or_else(|| anyhow::anyhow!("session gone"))?
    };
    session.begin_turn();

    let tools = build_turn_tool_registry(&state, event_tx.clone()).await;

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
    let persist_result = {
        let db = state.transcript.lock().await;
        persist_session_messages(&db, &mut session)
    };
    state
        .sessions
        .lock()
        .await
        .insert(session_id.to_string(), session);
    result?;
    persist_result
}

struct ServerState {
    config: Arc<AppConfig>,
    sandbox_policy: kkagent_tools::sandbox::SandboxPolicy,
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
    durable_http: kkagent_rpc::DurableHttpStore,
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
    events: tokio::sync::broadcast::Sender<serde_json::Value>,
    pending_questions: Mutex<HashMap<String, serde_json::Value>>,
    background_tasks: Vec<AbortHandle>,
    turn_locks: SessionTurnLocks,
    persistence_durable: bool,
    persistence_error: Option<String>,
    started_at: std::time::Instant,
}

#[derive(Default)]
struct SessionTurnLocks {
    entries: Mutex<HashMap<String, Arc<tokio::sync::Semaphore>>>,
}

impl SessionTurnLocks {
    async fn try_acquire(
        &self,
        session_id: &str,
    ) -> Result<tokio::sync::OwnedSemaphorePermit, String> {
        let semaphore = self
            .entries
            .lock()
            .await
            .entry(session_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(1)))
            .clone();
        semaphore
            .try_acquire_owned()
            .map_err(|_| format!("session {session_id} is busy with another turn"))
    }

    async fn remove(&self, session_id: &str) {
        self.entries.lock().await.remove(session_id);
    }
}

impl ServerState {
    async fn shutdown(&self) {
        for (_, handle) in self.abort_registry.lock().await.drain() {
            handle.abort();
        }
        for handle in &self.background_tasks {
            handle.abort();
        }
    }
}

fn mcp_manager_from_config(config: &AppConfig) -> McpManager {
    let configs: Vec<kkagent_mcp::McpServerConfig> = config
        .mcp_servers
        .iter()
        .map(|(name, cfg)| kkagent_mcp::McpServerConfig::from_app(name.clone(), cfg))
        .collect();
    McpManager::new(configs)
}

fn open_transcript_with_policy(
    path: &Path,
    allow_in_memory: bool,
) -> Result<(TranscriptDb, bool, Option<String>)> {
    match TranscriptDb::open(path) {
        Ok(database) => Ok((database, true, None)),
        Err(error) if allow_in_memory => {
            tracing::error!(
                "Failed to open transcript DB: {error}; explicitly entering in-memory degraded mode"
            );
            Ok((
                TranscriptDb::open_in_memory().map_err(|memory_error| {
                    anyhow::anyhow!(
                        "cannot open transcript DB (durable={error}, memory={memory_error})"
                    )
                })?,
                false,
                Some(error.to_string()),
            ))
        }
        Err(error) => Err(anyhow::anyhow!(
            "cannot open durable transcript DB: {error}; set KKAGENT_ALLOW_IN_MEMORY_TRANSCRIPTS=1 only for explicit degraded operation"
        )),
    }
}

async fn build_server_state(config: Arc<AppConfig>) -> Result<Arc<ServerState>> {
    let (events, _) = tokio::sync::broadcast::channel(1024);
    let allow_in_memory = std::env::var("KKAGENT_ALLOW_IN_MEMORY_TRANSCRIPTS")
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let transcript_path = kkagent_config::default_config_dir().join("transcripts.db");
    let (transcript, persistence_durable, persistence_error) =
        open_transcript_with_policy(&transcript_path, allow_in_memory)?;
    let durable_http = if persistence_durable {
        kkagent_rpc::DurableHttpStore::open(&transcript_path)?
    } else {
        kkagent_rpc::DurableHttpStore::open_in_memory()?
    };
    let sandbox_policy = kkagent_tools::sandbox::SandboxPolicy::from_config(&config.sandbox)?;

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
    let skills = Arc::new(
        kkagent_tools::SkillCatalog::configured(
            &cwd,
            &config.extra_skill_dirs,
            config.merge_all_available_skills,
        )
        .await,
    );
    let cron_path = kkagent_config::default_config_dir().join("cron.json");
    let cron = Arc::new(kkagent_tools::CronManager::with_persist(cron_path).await);
    let goal_mgr = Arc::new(kkagent_protocol::goal::GoalManager::new());
    let web = Arc::new(kkagent_tools::WebServicesConfig::from_app(&config));

    let cron_fires: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let mut background_tasks = Vec::new();
    {
        let cron_bg = cron.clone();
        let fires = cron_fires.clone();
        let hooks_cron = hooks.clone();
        let task = tokio::spawn(async move {
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
        background_tasks.push(task.abort_handle());
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
    let cloud_opts = CloudAppenderOptions {
        device_id: std::env::var("KKAGENT_DEVICE_ID")
            .unwrap_or_else(|_| uuid::Uuid::new_v4().to_string()),
        model: config.default_model.clone(),
        ..CloudAppenderOptions::default()
    };
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

    let subagents = if persistence_durable {
        Arc::new(SubagentManager::new_persistent(4, &transcript_path)?)
    } else {
        Arc::new(SubagentManager::new(4))
    };
    let state = Arc::new(ServerState {
        config: config.clone(),
        sandbox_policy,
        sessions: Mutex::new(HashMap::new()),
        approval_txs: Mutex::new(HashMap::new()),
        question_txs: Mutex::new(HashMap::new()),
        interrupt_flags: Mutex::new(HashMap::new()),
        model_aliases: Mutex::new(HashMap::new()),
        abort_registry: Arc::new(Mutex::new(HashMap::new())),
        transcript: Mutex::new(transcript),
        durable_http,
        subagents,
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
        events,
        pending_questions: Mutex::new(HashMap::new()),
        background_tasks,
        turn_locks: SessionTurnLocks::default(),
        persistence_durable,
        persistence_error,
        started_at: std::time::Instant::now(),
    });
    recover_subagents(state.clone()).await;
    Ok(state)
}

async fn run_server_handler<T: kkagent_rpc::transport::AsyncTransport>(
    transport: T,
    config: Arc<AppConfig>,
) -> Result<()> {
    let state = build_server_state(config).await?;
    run_server_handler_with_state(transport, state).await;
    Ok(())
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

fn persist_session_messages(db: &TranscriptDb, session: &mut Session) -> anyhow::Result<()> {
    if session.transcript_rewrite_required {
        let replacement = serialize_transcript_messages(&session.messages)?;
        db.replace_messages(&session.id, &replacement, None)?;
        session.persisted_message_count = session.messages.len();
        session.transcript_rewrite_required = false;
    }

    if session.persisted_message_count < session.messages.len() {
        let pending = &session.messages[session.persisted_message_count..];
        let serialized = serialize_transcript_messages(pending)?;
        db.append_messages(&session.id, &serialized)?;
        session.persisted_message_count = session.messages.len();
    }

    // Auto-title from first user text
    if session.title.is_none() {
        if let Some(first_user) = session.messages.iter().find(|m| m.role == "user") {
            if let Some(ChatContent::Text { text }) = first_user.content.first() {
                let title: String = text.chars().take(60).collect();
                db.set_title(&session.id, &title)?;
                session.title = Some(title);
            }
        }
    }
    Ok(())
}

fn serialize_transcript_messages(
    messages: &[ChatMessage],
) -> anyhow::Result<Vec<(String, String)>> {
    messages
        .iter()
        .map(|message| {
            Ok((
                message.role.clone(),
                serde_json::to_string(&message.content)?,
            ))
        })
        .collect()
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
    let provider = create_provider(provider_cfg, model_cfg).ok()?;
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
    let handle = tokio::spawn(async move {
        if let Err(error) = provider.stream_chat(request, tx.clone()).await {
            let _ = tx.send(StreamEvent::Error(error.to_string())).await;
        }
    });
    let mut out = String::new();
    let mut complete = false;
    let collected = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        while let Some(ev) = rx.recv().await {
            match ev {
                StreamEvent::TextDelta(t) => out.push_str(&t),
                StreamEvent::MessageEnd { .. } => {
                    complete = true;
                    break;
                }
                StreamEvent::Error(error) => {
                    tracing::warn!("compaction summary stream failed: {error}");
                    break;
                }
                _ => {}
            }
        }
    })
    .await;
    if collected.is_err() {
        tracing::warn!("compaction summary timed out");
    }
    handle.abort();
    if !complete || out.trim().is_empty() {
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
            let requested_workspace = params
                .as_ref()
                .and_then(|p| p.get("workspace"))
                .and_then(|v| v.as_str())
                .unwrap_or(".")
                .to_string();
            let workspace = std::fs::canonicalize(&requested_workspace).map_err(|error| {
                (
                    -32602,
                    format!("Invalid workspace {requested_workspace}: {error}"),
                )
            })?;
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
                workspace.clone(),
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
            initialize_session_context(&state, &mut session).await;

            {
                let db = state.transcript.lock().await;
                db.create_session(&session_id, &model_alias, &workspace.to_string_lossy())
                    .map_err(|error| (-32000, error.to_string()))?;
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
            fire_session_hook(
                &state,
                kkagent_mcp::HookEvent::SessionStart,
                &session_id,
                &workspace,
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
                    let db = state.transcript.lock().await;
                    let list: Vec<_> = summaries
                        .into_iter()
                        .map(|s| {
                            let message_count = db
                                .get_session(&s.id)
                                .ok()
                                .flatten()
                                .map(|r| r.message_count)
                                .unwrap_or(0);
                            let empty = message_count == 0
                                && s.last_prompt
                                    .as_ref()
                                    .map(|p| p.trim().is_empty())
                                    .unwrap_or(true);
                            serde_json::json!({
                                "session_id": s.id,
                                "title": s.title,
                                "is_custom_title": s.is_custom_title,
                                "working_dir": s.work_dir,
                                "session_dir": s.session_dir,
                                "archived": s.archived,
                                "last_prompt": s.last_prompt,
                                "created_at": s.created_at,
                                "updated_at": s.updated_at,
                                "forked_from": s.forked_from,
                                "message_count": message_count,
                                "empty": empty,
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
                    let empty = s.message_count == 0;
                    serde_json::json!({
                        "session_id": s.session_id,
                        "title": s.title,
                        "model": s.model,
                        "working_dir": s.working_dir,
                        "created_at": s.created_at,
                        "updated_at": s.updated_at,
                        "message_count": s.message_count,
                        "empty": empty,
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
            let transcript_result = {
                let db = state.transcript.lock().await;
                db.fork_session(source_id, &target_id, summary.title.as_deref(), turn_index)
            };
            if let Err(error) = transcript_result {
                let _ = store.delete(&target_id);
                return Err((-32000, error.to_string()));
            }
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
            {
                let db = state.transcript.lock().await;
                if let Err(error) = db.set_archived(session_id, archived) {
                    let _ = SessionStore::open_default().archive(session_id, !archived);
                    return Err((-32000, error.to_string()));
                }
            }
            if let Some(session) = state.sessions.lock().await.get_mut(session_id) {
                let _ = session.services.metadata.set_archived(archived);
                if archived {
                    session.services.on_close(SessionCloseReason::Archive).await;
                }
            }
            Ok(serde_json::json!({"session_id": session_id, "archived": archived}))
        }
        "sessions.delete" => {
            let session_id = params
                .as_ref()
                .and_then(|p| p.get("session_id"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| (-32602, "Missing session_id".into()))?
                .to_string();
            let _turn_permit = state
                .turn_locks
                .try_acquire(&session_id)
                .await
                .map_err(|message| (-32001, message))?;
            SessionStore::open_default()
                .delete(&session_id)
                .map_err(|e| (-32000, e.to_string()))?;
            {
                let db = state.transcript.lock().await;
                let _ = db.archive_session(&session_id);
            }
            let removed = state.sessions.lock().await.remove(&session_id);
            state.interrupt_flags.lock().await.remove(&session_id);
            state.model_aliases.lock().await.remove(&session_id);
            state.approval_txs.lock().await.remove(&session_id);
            state.question_txs.lock().await.remove(&session_id);
            if let Some(session) = removed {
                session
                    .services
                    .on_close(SessionCloseReason::Exit)
                    .await;
            }
            drop(_turn_permit);
            state.turn_locks.remove(&session_id).await;
            Ok(serde_json::json!({"session_id": session_id, "deleted": true}))
        }
        "session.preview" => {
            let session_id = params
                .as_ref()
                .and_then(|p| p.get("session_id"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| (-32602, "Missing session_id".into()))?
                .to_string();
            let limit = params
                .as_ref()
                .and_then(|p| p.get("limit"))
                .and_then(|v| v.as_u64())
                .unwrap_or(12) as usize;

            let (title, model, messages) = {
                let sessions = state.sessions.lock().await;
                if let Some(session) = sessions.get(&session_id) {
                    let msgs: Vec<_> = session
                        .messages
                        .iter()
                        .rev()
                        .filter(|m| m.role == "user" || m.role == "assistant")
                        .take(limit)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .map(|m| {
                            let text = m
                                .content
                                .iter()
                                .filter_map(|c| match c {
                                    ChatContent::Text { text } => Some(text.as_str()),
                                    _ => None,
                                })
                                .collect::<Vec<_>>()
                                .join("\n");
                            serde_json::json!({
                                "role": m.role,
                                "text": text.chars().take(240).collect::<String>(),
                            })
                        })
                        .collect();
                    (
                        session.title.clone(),
                        Some(session.get_model_alias()),
                        msgs,
                    )
                } else {
                    drop(sessions);
                    let db = state.transcript.lock().await;
                    let sid = resolve_session_id(&db, &session_id)
                        .unwrap_or_else(|| session_id.clone());
                    let record = db
                        .get_session(&sid)
                        .map_err(|e| (-32000, e.to_string()))?;
                    let records = db
                        .load_messages(&sid)
                        .map_err(|e| (-32000, e.to_string()))?;
                    let chat = messages_from_records(&records);
                    let preview: Vec<_> = chat
                        .iter()
                        .rev()
                        .filter(|m| m.role == "user" || m.role == "assistant")
                        .take(limit)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .map(|m| {
                            let text = m
                                .content
                                .iter()
                                .filter_map(|c| match c {
                                    ChatContent::Text { text } => Some(text.as_str()),
                                    _ => None,
                                })
                                .collect::<Vec<_>>()
                                .join("\n");
                            serde_json::json!({
                                "role": m.role,
                                "text": text.chars().take(240).collect::<String>(),
                            })
                        })
                        .collect();
                    (
                        record.as_ref().and_then(|r| r.title.clone()),
                        record.as_ref().map(|r| r.model.clone()),
                        preview,
                    )
                }
            };

            Ok(serde_json::json!({
                "session_id": session_id,
                "title": title,
                "model": model,
                "messages": messages,
            }))
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
            let store = SessionStore::open_default();
            let old_title = store
                .get(session_id)
                .map_err(|error| (-32000, error.to_string()))?
                .title
                .unwrap_or_else(|| session_id.to_string());
            store
                .rename(session_id, title)
                .map_err(|e| (-32000, e.to_string()))?;
            let transcript_result = {
                let db = state.transcript.lock().await;
                db.set_title(session_id, title)
            };
            if let Err(error) = transcript_result {
                let _ = store.rename(session_id, &old_title);
                return Err((-32000, error.to_string()));
            }
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
            initialize_session_context(&state, &mut session).await;
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
            let images = params
                .as_ref()
                .and_then(|p| p.get("images"))
                .and_then(|value| value.as_array())
                .map(|images| {
                    images
                        .iter()
                        .map(|image| {
                            let media_type = image
                                .get("media_type")
                                .or_else(|| image.get("mime_type"))
                                .and_then(|value| value.as_str())
                                .ok_or_else(|| (-32602, "Image is missing media_type".into()))?;
                            let data = image
                                .get("data")
                                .and_then(|value| value.as_str())
                                .ok_or_else(|| (-32602, "Image is missing base64 data".into()))?;
                            Ok((media_type.to_string(), data.to_string()))
                        })
                        .collect::<Result<Vec<_>, _>>()
                })
                .transpose()?
                .unwrap_or_default();
            if text.trim().is_empty() && images.is_empty() {
                return Err((-32602, "Prompt text must not be empty".into()));
            }
            let turn_permit = state
                .turn_locks
                .try_acquire(&session_id)
                .await
                .map_err(|message| (-32001, message))?;

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
                    session
                        .add_user_message_with_images(text, images)
                        .map_err(|error| (-32602, format!("Invalid image input: {error}")))?;
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
                        let rewrite = s.transcript_rewrite_required;
                        let start = if rewrite {
                            0
                        } else {
                            s.persisted_message_count.min(s.messages.len())
                        };
                        (
                            s.messages[start..].to_vec(),
                            start,
                            s.title.clone(),
                            rewrite,
                        )
                    })
                };
                if let Some((pending, start, title, rewrite)) = snapshot {
                    let mut new_title = title;
                    let persisted = {
                        let db = state.transcript.lock().await;
                        let result = if rewrite {
                            serialize_transcript_messages(&pending).and_then(|messages| {
                                db.replace_messages(&session_id, &messages, None)
                            })
                        } else {
                            serialize_transcript_messages(&pending)
                                .and_then(|messages| db.append_messages(&session_id, &messages))
                        };
                        if result.is_ok() && new_title.is_none() {
                            if let Some(ChatContent::Text { text }) = pending
                                .iter()
                                .find(|message| message.role == "user")
                                .and_then(|message| message.content.first())
                            {
                                let title: String = text.chars().take(60).collect();
                                if db.set_title(&session_id, &title).is_ok() {
                                    new_title = Some(title);
                                }
                            }
                        }
                        if let Err(error) = &result {
                            tracing::warn!("Failed to persist session prompt: {error}");
                        }
                        result.is_ok()
                    };
                    if persisted {
                        if let Some(session) = state.sessions.lock().await.get_mut(&session_id) {
                            session.persisted_message_count = start + pending.len();
                            session.transcript_rewrite_required = false;
                            if session.title.is_none() {
                                session.title = new_title;
                            }
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
                    .or_else(|| state.config.default_model_alias().map(|s| s.to_string()))
                    .ok_or_else(|| (-32000, "No default_model in config".into()))?
            };
            if state.config.resolve_model(&model_alias).is_none() {
                return Err((-32000, format!("Model '{}' not found", model_alias)));
            }

            let tools = build_turn_tool_registry(&state, agent_event_tx.clone()).await;

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
                let _turn_permit = turn_permit;
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
                    if let Err(error) = persist_session_messages(&db, &mut session) {
                        tracing::error!("Failed to persist completed turn: {error}");
                        let _ = agent_event_tx.try_send(AgentEvent::Error {
                            session_id: sid.clone(),
                            message: format!("turn persistence failed: {error}"),
                        });
                    }
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

            // Always flip the cooperative cancel flag first (works while session is out of the map).
            if let Some(flag) = state.interrupt_flags.lock().await.get(&session_id) {
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            // Cancel any in-flight approval / question waiters even mid-turn.
            if let Some(tx) = state.approval_txs.lock().await.get(&session_id) {
                let _ = tx.try_send(kkagent_protocol::ApprovalResponse {
                    approval_id: String::new(),
                    decision: kkagent_protocol::ApprovalDecision::Cancelled,
                    scope: None,
                    feedback: Some("interrupted".into()),
                    selected_label: None,
                });
            }
            if let Some(tx) = state.question_txs.lock().await.get(&session_id) {
                let _ = tx.try_send(kkagent_protocol::QuestionResponse {
                    question_id: String::new(),
                    selected_option_ids: Vec::new(),
                    free_text: None,
                    cancelled: true,
                });
            }
            if let Some(session) = state.sessions.lock().await.get(&session_id) {
                session.request_interrupt();
            }
            // Abort LLM stream task if still registered (no-op once tools are running).
            if let Some(handle) = state.abort_registry.lock().await.remove(&session_id) {
                handle.abort();
            }
            // Kill any background Bash jobs owned by this session.
            state.bash_shells.cancel_session(&session_id).await;
            Ok(serde_json::json!({"ok": true}))
        }
        "session.btw" => {
            let session_id = params
                .as_ref()
                .and_then(|p| p.get("session_id"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| (-32602, "Missing session_id".into()))?
                .to_string();
            let question = params
                .as_ref()
                .and_then(|p| p.get("text").or_else(|| p.get("question")))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if question.is_empty() {
                return Err((-32602, "BTW question must not be empty".into()));
            }

            let (history, model_alias, prior_turns, cancel, agent_id) = {
                let sessions = state.sessions.lock().await;
                let session = sessions
                    .get(&session_id)
                    .ok_or_else(|| (-32602, format!("Session not found: {session_id}")))?;
                if !session.services.btw.try_begin() {
                    return Err((
                        -32001,
                        "Wait for /btw to finish before sending another question.".into(),
                    ));
                }
                let agent_id = session.services.btw.start(&session.services.agents);
                let history = session.messages.clone();
                let model_alias = session.get_model_alias();
                let prior_turns = session.services.btw.turns();
                let cancel = session.services.btw.cancel_flag();
                cancel.store(false, std::sync::atomic::Ordering::SeqCst);
                (history, model_alias, prior_turns, cancel, agent_id)
            };

            let rpc_tx = rpc_event_tx.clone();
            let config = state.config.clone();
            let state_clone = state.clone();
            let sid = session_id.clone();
            let q = question.clone();
            tokio::spawn(async move {
                let (stream_tx, mut stream_rx) =
                    mpsc::channel::<kkagent_llm::types::StreamEvent>(256);
                let stream_task = {
                    let config = config.clone();
                    let history = history;
                    let prior = prior_turns;
                    let question = q.clone();
                    let cancel = cancel.clone();
                    tokio::spawn(async move {
                        SessionBtwService::stream_side_question(
                            &config,
                            &model_alias,
                            &history,
                            &prior,
                            &question,
                            stream_tx,
                            cancel,
                        )
                        .await
                    })
                };

                let mut answer = String::new();
                let mut stream_error: Option<String> = None;
                while let Some(evt) = stream_rx.recv().await {
                    match evt {
                        kkagent_llm::types::StreamEvent::TextDelta(text) => {
                            answer.push_str(&text);
                            let frame = Frame::Event {
                                event: "agent".into(),
                                scope: None,
                                data: serde_json::to_value(AgentEvent::BtwDelta {
                                    session_id: sid.clone(),
                                    text,
                                })
                                .unwrap_or_default(),
                            };
                            if rpc_tx.send(frame).await.is_err() {
                                break;
                            }
                        }
                        kkagent_llm::types::StreamEvent::ThinkingDelta(text) => {
                            let frame = Frame::Event {
                                event: "agent".into(),
                                scope: None,
                                data: serde_json::to_value(AgentEvent::BtwThinkingDelta {
                                    session_id: sid.clone(),
                                    text,
                                })
                                .unwrap_or_default(),
                            };
                            if rpc_tx.send(frame).await.is_err() {
                                break;
                            }
                        }
                        kkagent_llm::types::StreamEvent::Error(message) => {
                            stream_error = Some(message);
                        }
                        kkagent_llm::types::StreamEvent::MessageEnd { .. } => {}
                        _ => {}
                    }
                }

                if let Err(e) = stream_task.await.unwrap_or(Ok(())) {
                    if stream_error.is_none() {
                        stream_error = Some(e.to_string());
                    }
                }

                if stream_error.is_none() && !answer.trim().is_empty() {
                    if let Some(session) = state_clone.sessions.lock().await.get(&sid) {
                        session.services.btw.push_turn(BtwTurn {
                            question: q,
                            answer,
                        });
                    }
                }

                if let Some(session) = state_clone.sessions.lock().await.get(&sid) {
                    session.services.btw.end();
                }

                let frame = Frame::Event {
                    event: "agent".into(),
                    scope: None,
                    data: serde_json::to_value(AgentEvent::BtwEnd {
                        session_id: sid,
                        error: stream_error,
                    })
                    .unwrap_or_default(),
                };
                let _ = rpc_tx.send(frame).await;
            });

            Ok(serde_json::json!({
                "ok": true,
                "agent_id": agent_id,
                "question": question,
            }))
        }
        "session.btw_cancel" => {
            let session_id = params
                .as_ref()
                .and_then(|p| p.get("session_id"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| (-32602, "Missing session_id".into()))?;
            let sessions = state.sessions.lock().await;
            if let Some(session) = sessions.get(session_id) {
                session.services.btw.request_cancel();
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
                    session
                        .set_title_persisted(title.clone())
                        .map_err(|e| (-32000, e.to_string()))?;
                }
            }
            let _ = SessionStore::open_default().rename(&session_id, &title);
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
                let cut = kkagent_core::compact_cut_index(&session.messages, keep_last);
                let old = if cut == 0 {
                    &session.messages[..0]
                } else {
                    &session.messages[..cut]
                };
                let mut digest = String::from(
                    "Summarize the following conversation for future context. \
                     Keep decisions, file paths, tool outcomes, and unfinished tasks.\n\n",
                );
                digest.push_str(&kkagent_core::build_compaction_digest(old));
                digest
            };

            let summary_text = summarize_with_llm(state.config.clone(), &summary)
                .await
                .unwrap_or_else(|| {
                    // No LLM available: keep the tool-aware local digest so tool
                    // outcomes are not silently discarded (kimi-compatible).
                    summary.chars().take(4_000).collect()
                });

            let deleted = {
                let sessions = state.sessions.lock().await;
                let keep = if let Some(session) = sessions.get(&session_id) {
                    let cut = kkagent_core::compact_cut_index(&session.messages, keep_last);
                    session.messages.len().saturating_sub(cut).max(1)
                } else {
                    keep_last
                };
                drop(sessions);
                let db = state.transcript.lock().await;
                db.compact_session(&session_id, keep, &summary_text)
                    .map_err(|e| (-32000, e.to_string()))?
            };

            {
                let db = state.transcript.lock().await;
                let records = db.load_messages(&session_id).unwrap_or_default();
                let mut msgs = messages_from_records(&records);
                drop(db);
                kkagent_core::repair_tool_exchanges(&mut msgs, true);
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
                .map(|s| s.to_string());
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
    fn transcript_policy_fails_closed_unless_degraded_mode_is_explicit() {
        let directory =
            std::env::temp_dir().join(format!("kkagent-db-policy-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let error = match open_transcript_with_policy(&directory, false) {
            Ok(_) => panic!("directory path unexpectedly opened as a transcript database"),
            Err(error) => error,
        };
        assert!(error
            .to_string()
            .contains("cannot open durable transcript DB"));
        let (_, durable, degraded_error) = open_transcript_with_policy(&directory, true).unwrap();
        assert!(!durable);
        assert!(degraded_error.is_some());
        std::fs::remove_dir_all(directory).unwrap();
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

    #[tokio::test]
    async fn serializes_turns_per_session_but_not_across_sessions() {
        let locks = SessionTurnLocks::default();
        let first = locks.try_acquire("session-a").await.unwrap();
        assert!(locks.try_acquire("session-a").await.is_err());
        let other = locks.try_acquire("session-b").await.unwrap();
        drop(first);
        assert!(locks.try_acquire("session-a").await.is_ok());
        drop(other);
    }

    #[test]
    fn compacted_in_memory_history_atomically_replaces_transcript() {
        let workspace =
            std::env::temp_dir().join(format!("kkagent-persist-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).unwrap();
        let db = TranscriptDb::open_in_memory().unwrap();
        db.create_session("persist-test", "model", workspace.to_str().unwrap())
            .unwrap();
        let mut session = Session::new(
            "persist-test".into(),
            workspace.clone(),
            PermissionMode::Auto,
            "model".into(),
        );
        for index in 0..6 {
            session.add_user_message(format!("message {index}"));
        }
        persist_session_messages(&db, &mut session).unwrap();
        assert_eq!(db.load_messages("persist-test").unwrap().len(), 6);

        kkagent_core::compact_messages(&mut session.messages, 2, "durable digest");
        session.transcript_rewrite_required = true;
        persist_session_messages(&db, &mut session).unwrap();

        let records = db.load_messages("persist-test").unwrap();
        assert_eq!(records.len(), 3);
        assert!(records[0].content_json.contains("durable digest"));
        assert!(records[1].content_json.contains("message 4"));
        assert!(!session.transcript_rewrite_required);
        assert_eq!(session.persisted_message_count, 3);
        std::fs::remove_dir_all(workspace).unwrap();
    }
}
