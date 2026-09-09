//! MCP (Model Context Protocol) server over stdio.
//!
//! `kkagent mcp serve` exposes kkagent to an external orchestrator (another
//! agent, an IDE, ChatGPT) as an asynchronous supervisor/delegation API:
//!
//! - `list_workspaces` — workspaces auto-registered from historical kkagent
//!   sessions (transcript DB) plus configured trusted roots.
//! - `get_context` — supervisor-level project context: git state, recent
//!   sessions, in-flight tasks, items needing attention.
//! - `inspect` — direct read-only fetch of a resource in a workspace: source
//!   code, text, logs, git diffs, images, and task artifacts; no agent runs.
//! - `write_plan` — store an orchestrator-authored execution plan (markdown)
//!   and get a `plan_id`; `delegate` accepts it and injects the full plan
//!   ahead of the task prompt as the scope source of truth.
//! - `delegate` — start an async coding task (equivalent to a fresh default
//!   kkagent session in the workspace); returns `task_id` immediately. kkagent
//!   decides model, tools, and worktree isolation itself.
//! - `get_progress` — status poll: queued / running / waiting_input /
//!   waiting_permission / completed / failed / cancelled.
//! - `continue_task` — send a new instruction or a structured decision into
//!   an EXISTING task, waking / continuing its agent loop: answer a pending
//!   question (explicit option ids / free text / dismissal), approve or
//!   reject a permission request, steer a running turn, or continue a
//!   finished task with new work; everything lands in the SAME task/session
//!   (no new agent session).
//! - `get_result` — mechanical review summary: the agent's own final output
//!   plus mechanically collected changed files, diff --stat, and produced
//!   image paths (read them via `inspect`) — never the raw transcript, and
//!   never a second LLM pass to compose the summary.
//! - `cancel` — stop a task but keep its code changes and worktree.
//!
//! The transport is newline-delimited JSON-RPC 2.0 on stdin/stdout — exactly
//! what the MCP stdio transport requires. Nothing besides protocol frames may
//! ever be written to stdout; logging goes to stderr.
//!
//! `kkagent mcp serve --http [ADDR]` serves the same protocol over the MCP
//! Streamable HTTP transport instead (default `127.0.0.1:8788`): MCP clients
//! POST JSON-RPC messages to `http://ADDR/mcp`; notifications are answered
//! with `202 Accepted`. Bearer-token auth is required (`--http-token`,
//! `KKAGENT_MCP_HTTP_TOKEN`, or the persisted local kkagent HTTP token) and
//! `GET /healthz` reports liveness without auth for connection checks.
//!
//! `--tunnel <TUNNEL_ID>` additionally runs the OpenAI Secure MCP Tunnel
//! client (https://github.com/openai/tunnel-client) as a supervised child
//! process, so ChatGPT / Codex / the Responses API can reach this private
//! endpoint through an OpenAI-hosted tunnel. The runtime API key is read
//! from `CONTROL_PLANE_API_KEY` (never argv); the MCP bearer token is passed
//! to tunnel-client via `MCP_EXTRA_HEADERS` with an `env:` value reference.
//! Stopping kkagent (Ctrl-C / SIGTERM) also stops the child.

mod collaboration;
use collaboration::{CollaborationStore, TaskRecord};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Instant, SystemTime};

use anyhow::{anyhow, Context as _, Result};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use kkagent_core::permission::PermissionChain;
use kkagent_core::session::runtime::TurnCheckpoint;
use kkagent_core::transcript::TranscriptDb;
use kkagent_core::{AgentLoop, Session, SessionSteerMailbox, SteerInput};
use kkagent_protocol::approval::ApprovalRequest;
use kkagent_protocol::approval::{ApprovalDecision, ApprovalResponse};
use kkagent_protocol::events::QuestionPayload;
use kkagent_protocol::question::QuestionResponse;
use kkagent_protocol::subagent::{stamp_child_depth, SubagentConfig};
use kkagent_protocol::{AgentEvent, PermissionMode, SessionStatus};
use serde_json::{json, Value};
use tokio::sync::{mpsc, Mutex, Notify, Semaphore};

const SUPPORTED_PROTOCOL_VERSIONS: [&str; 3] = ["2024-11-05", "2025-03-26", "2025-06-18"];
const LATEST_PROTOCOL_VERSION: &str = "2025-06-18";
const SERVER_NAME: &str = "kkagent";
/// Cap on pending follow-up instructions per task.
const PENDING_INSTRUCTIONS_CAP: usize = 32;
/// Byte cap for inspect reads and git diffs.
const MAX_INSPECT_BYTES: usize = 1_000_000;
/// Default line page size for text `inspect`.
const DEFAULT_INSPECT_LINE_LIMIT: u64 = 400;
/// Default page size for `list_workspaces`.
const DEFAULT_WORKSPACE_LIMIT: u64 = 5;
/// Cap on per-task recent-event log kept for `get_progress`.
const RECENT_EVENT_CAP: usize = 8;
/// Every MCP task is a delegate-style task; `kind` stays in payloads for
/// forward compatibility.
const TASK_KIND: &str = "delegate";
/// MCP tasks always run as a fresh default session: `general` profile with no
/// model override resolves to the globally configured default model. kkagent
/// decides everything itself — the caller only supplies goal and workspace.
const TASK_PROFILE: &str = "general";
/// Char cap for orchestrator-authored plans stored with `write_plan`.
const MAX_PLAN_CHARS: usize = 100_000;

// ---------------------------------------------------------------------------
// Task model
// ---------------------------------------------------------------------------

/// Lifecycle phase, surfaced verbatim through `get_progress`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum TaskPhase {
    Queued,
    Running,
    AwaitingInput,
    AwaitingPermission,
    Completed,
    Failed,
    Cancelled,
}

impl TaskPhase {
    fn as_str(self) -> &'static str {
        match self {
            TaskPhase::Queued => "queued",
            TaskPhase::Running => "running",
            TaskPhase::AwaitingInput => "waiting_input",
            TaskPhase::AwaitingPermission => "waiting_permission",
            TaskPhase::Completed => "completed",
            TaskPhase::Failed => "failed",
            TaskPhase::Cancelled => "cancelled",
        }
    }

    fn terminal(self) -> bool {
        matches!(
            self,
            TaskPhase::Completed | TaskPhase::Failed | TaskPhase::Cancelled
        )
    }
}

#[derive(Default, Clone, Serialize, Deserialize)]
struct Progress {
    tool_calls: u64,
    tool_results: u64,
    output_chars: u64,
    thinking_chars: u64,
    turns: u64,
}

impl Progress {
    fn snapshot(&self) -> Value {
        json!({
            "turns": self.turns,
            "tool_calls": self.tool_calls,
            "tool_results": self.tool_results,
            "output_chars": self.output_chars,
            "thinking_chars": self.thinking_chars,
        })
    }
}

/// Liveness slot for a task's runner, mutated only under its mutex so
/// spawn decisions are atomic with runner exit decisions.
///
/// The invariant that makes `continue_task` reliable: a spawn decision keys
/// on the synchronous `exited` flag, never on `AbortHandle::is_finished()`
/// (which is only asynchronously true and races every exit path). A runner
/// that stops serving the task must pass through `runner_should_restart`
/// or the [`RunnerGuard`] drop, both of which set `exited` under the lock
/// before the tokio task can be observed as finished. Combined with the
/// runner's final pre-exit check running under the same lock, exactly one
/// of "the parked runner consumes the instruction" and "`ensure_runner`
/// spawns a fresh runner for it" holds for every accepted instruction.
struct RunnerSlot {
    /// Abort handle for the runner's tokio task (hard cancel backstop).
    abort: Option<tokio::task::AbortHandle>,
    /// The runner is parked in `wait_for_instruction` and reachable via
    /// `runner_notify`.
    waiting: bool,
    /// No runner is serving (or will ever serve) the task. `true` by default
    /// so the first `ensure_runner` call spawns.
    exited: bool,
}

impl RunnerSlot {
    /// Fresh slot: no runner yet, so the next `ensure_runner` spawns one.
    fn initial() -> Self {
        Self {
            abort: None,
            waiting: false,
            exited: true,
        }
    }
}

/// Wall-clock timestamps of the latest model / tool activity, surfaced by
/// `get_progress` so an orchestrator can tell "thinking", "tool blocked" and
/// "runner gone" apart.
#[derive(Default)]
struct ActivityStamp {
    last_progress: Option<SystemTime>,
    last_model: Option<SystemTime>,
    last_tool: Option<SystemTime>,
}

impl ActivityStamp {
    fn note_progress(&mut self) {
        self.last_progress = Some(SystemTime::now());
    }

    fn note_model(&mut self) {
        self.note_progress();
        self.last_model = Some(SystemTime::now());
    }

    fn note_tool(&mut self) {
        self.note_progress();
        self.last_tool = Some(SystemTime::now());
    }

    /// `(progress, model, tool)` ages in whole seconds; `None` = never.
    fn ages(&self) -> (Option<u64>, Option<u64>, Option<u64>) {
        fn age(stamp: Option<SystemTime>) -> Option<u64> {
            stamp.and_then(|t| t.elapsed().ok()).map(|d| d.as_secs())
        }
        (
            age(self.last_progress),
            age(self.last_model),
            age(self.last_tool),
        )
    }
}

/// RAII marker for a live runner: dropping it (normal exit, panic unwind or
/// early return) publishes `exited` under the slot lock, so a racing
/// `spawn_runner` always observes the death synchronously.
struct RunnerGuard {
    task: Arc<McpTask>,
}

impl Drop for RunnerGuard {
    fn drop(&mut self) {
        let mut slot = self.task.runner.lock().unwrap_or_else(|e| e.into_inner());
        slot.exited = true;
        slot.waiting = false;
    }
}

/// Review summary refreshed by the runner after every turn. Mechanical only —
/// composed from the session's final assistant text and git/checkpoint data,
/// never a second LLM pass.
#[derive(Default, Clone, Serialize, Deserialize)]
struct TaskSummary {
    final_message: String,
    files_changed: Vec<String>,
    diff_stat: Option<String>,
    warnings: Vec<String>,
}

/// One image produced by a task, reported by `get_result` as a path only.
/// Callers read the content with the `inspect` tool — it is never inlined.
#[derive(Debug, Clone)]
struct TaskImage {
    path: String,
    media_type: String,
}

impl TaskImage {
    fn to_value(&self) -> Value {
        json!({
            "path": self.path,
            "media_type": self.media_type,
        })
    }
}

/// Product-size guard: skip artifacts larger than this when scanning.
const MAX_ARTIFACT_BYTES: u64 = 5 * 1024 * 1024;
/// Hard cap on image paths reported by one `get_result` call.
const MAX_IMAGES_PER_RESULT: usize = 8;

impl TaskSummary {
    fn to_value(&self) -> Value {
        json!({
            "final_message": self.final_message,
            "files_changed": self.files_changed,
            "diff_stat": self.diff_stat,
            "warnings": self.warnings,
        })
    }
}

/// Shared state of one delegated task. The runner task and the MCP
/// request handlers both hold `Arc<McpTask>`.
struct McpTask {
    id: String,
    resume: bool,
    plan_ref: StdMutex<Option<Value>>,
    review: StdMutex<String>,
    reviewed_snapshot: StdMutex<Option<String>>,
    event_sequence: std::sync::atomic::AtomicU64,
    session_id: String,
    description: String,
    prompt: String,
    /// Orchestrator-authored plan (write_plan) injected ahead of the prompt.
    plan_title: Option<String>,
    plan: Option<String>,
    origin_workspace: PathBuf,
    /// Directory the agent actually runs in (worktree path when isolated).
    run_dir: Mutex<PathBuf>,
    worktree: Mutex<Option<kkagent_tools::git_worktree::WorktreeInfo>>,
    isolated: bool,
    /// Git HEAD sha captured before the task started, for diff stats.
    base_commit: StdMutex<Option<String>>,

    // runtime handles
    interrupt: Arc<AtomicBool>,
    mailbox: SessionSteerMailbox,
    /// Live answer channels, pointed at the runner's Session before its first
    /// turn. `None` until then (tasks queued have nothing to answer).
    question_tx: StdMutex<Option<mpsc::Sender<QuestionResponse>>>,
    approval_tx: StdMutex<Option<mpsc::Sender<ApprovalResponse>>>,
    /// Best-effort wake for a runner parked in `wait_for_instruction`
    /// (Notify keeps an unconsumed permit, so the wake itself is never
    /// lost). Correctness never depends on it: instruction consumption is
    /// guaranteed by the [`RunnerSlot`] protocol instead.
    runner_notify: Notify,
    /// Liveness bookkeeping for the runner task. Guards spawn decisions
    /// against the lost-wakeup race between completion/runner-exit and
    /// `continue_task`.
    runner: StdMutex<RunnerSlot>,

    // observed state
    phase: StdMutex<TaskPhase>,
    progress: StdMutex<Progress>,
    /// Timestamps of the last model / tool activity, for get_progress.
    activity: StdMutex<ActivityStamp>,
    recent_events: StdMutex<Vec<String>>,
    pending_question: StdMutex<Option<QuestionPayload>>,
    pending_approval: StdMutex<Option<ApprovalRequest>>,
    /// Follow-up instructions buffered while no turn is active; the runner
    /// drains them into the session as new turns of the same task.
    pending_instructions: StdMutex<Vec<String>>,
    summary: StdMutex<TaskSummary>,
    error: StdMutex<Option<String>>,
    started_at: Instant,
    finished_at: StdMutex<Option<Instant>>,
}

impl McpTask {
    fn phase(&self) -> TaskPhase {
        *self.phase.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn set_phase(&self, phase: TaskPhase) {
        *self.phase.lock().unwrap_or_else(|e| e.into_inner()) = phase;
        if phase.terminal() {
            *self.finished_at.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
        }
    }

    fn status(&self) -> &'static str {
        self.phase().as_str()
    }

    fn set_error(&self, error: String) {
        *self.error.lock().unwrap_or_else(|e| e.into_inner()) = Some(error);
    }

    fn push_event(&self, text: String) {
        let mut events = self.recent_events.lock().unwrap_or_else(|e| e.into_inner());
        let sequence = self.event_sequence.fetch_add(1, Ordering::SeqCst) + 1;
        events.push(format!("{sequence}: {text}"));
        let len = events.len();
        if len > RECENT_EVENT_CAP {
            events.drain(..len - RECENT_EVENT_CAP);
        }
    }

    fn elapsed_seconds(&self) -> u64 {
        match *self.finished_at.lock().unwrap_or_else(|e| e.into_inner()) {
            Some(finished) => (finished - self.started_at).as_secs(),
            None => self.started_at.elapsed().as_secs(),
        }
    }

    /// Hard-cancel backstop for the runner's tokio task, if one is live.
    /// Marks the slot exited synchronously under the lock: an abort is only
    /// processed at the runner's next await point, so a continue_task racing
    /// the cancellation must not conclude a runner is still alive.
    fn abort_runner(&self) {
        let mut slot = self.runner.lock().unwrap_or_else(|e| e.into_inner());
        slot.exited = true;
        slot.waiting = false;
        if let Some(handle) = slot.abort.take() {
            handle.abort();
        }
    }

    /// Runner liveness for get_progress: `waiting` when parked for
    /// instructions, `running` when a tokio task exists, `exited` otherwise.
    fn runner_state(&self) -> &'static str {
        let slot = self.runner.lock().unwrap_or_else(|e| e.into_inner());
        if slot.exited {
            "exited"
        } else if slot.waiting {
            "waiting_for_instruction"
        } else {
            "running"
        }
    }

    /// Mark the runner as parked in `wait_for_instruction` (reachable via
    /// `runner_notify`).
    fn mark_runner_waiting(&self) {
        self.runner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .waiting = true;
    }

    /// Clear the parked flag before the runner does non-idle work.
    fn mark_runner_active(&self) {
        self.runner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .waiting = false;
    }

    /// Final pre-exit check for the runner: returns `true` when instructions
    /// arrived in the closing window between the last queue check and now.
    /// Runs while `RunnerGuard` still holds, so a racing `spawn_runner` sees
    /// `exited == false` and never double-spawns.
    fn runner_should_restart(&self) -> bool {
        let should_restart = !self
            .pending_instructions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
            && !self.interrupt.load(Ordering::SeqCst);
        if should_restart {
            self.mark_runner_active();
        }
        should_restart
    }

    fn note_model_activity(&self) {
        self.activity
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .note_model();
    }

    fn note_tool_activity(&self) {
        self.activity
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .note_tool();
    }

    fn progress_snapshot(&self) -> Value {
        self.progress
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .snapshot()
    }

    fn summary_snapshot(&self) -> TaskSummary {
        self.summary
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn take_pending_question(&self) -> Option<QuestionPayload> {
        self.pending_question
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    fn store_pending_question(&self, question: QuestionPayload) {
        *self
            .pending_question
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(question);
    }

    fn take_pending_approval(&self) -> Option<ApprovalRequest> {
        self.pending_approval
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    fn store_pending_approval(&self, request: ApprovalRequest) {
        *self
            .pending_approval
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(request);
    }

    fn send_question_answer(&self, response: QuestionResponse) -> Result<(), String> {
        let sender = self
            .question_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(|| "task runner is not active yet".to_string())?;
        sender
            .try_send(response)
            .map_err(|_| "task runner is gone".to_string())
    }

    fn send_approval(&self, response: ApprovalResponse) -> Result<(), String> {
        let sender = self
            .approval_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(|| "task runner is not active yet".to_string())?;
        sender
            .try_send(response)
            .map_err(|_| "task runner is gone".to_string())
    }
}

/// Everything the runner needs from the server, cloned per task.
struct RunnerCtx {
    config: Arc<kkagent_config::AppConfig>,
    web: Arc<kkagent_tools::WebServicesConfig>,
    queue: Arc<Semaphore>,
    transcript: TranscriptDb,
    store: CollaborationStore,
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// Build the MCP runtime and serve over stdio until stdin closes.
pub async fn run_mcp_serve(config: Arc<kkagent_config::AppConfig>) -> Result<()> {
    let transcript = TranscriptDb::open_default().map_err(|e| {
        // Diagnostics/logs go to stderr; stdout must stay protocol-clean.
        eprintln!("kkagent mcp: transcript registry unavailable: {e}");
        e
    })?;
    let server = Arc::new(McpServer::new(config, transcript)?);
    serve_stdio(server).await
}

/// Serve the MCP protocol over stdio.
pub async fn serve_stdio(server: Arc<McpServer>) -> Result<()> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let writer = tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let mut out = tokio::io::stdout();
        while let Some(line) = rx.recv().await {
            let written = out.write_all(line.as_bytes()).await.is_ok()
                && out.write_all(b"\n").await.is_ok()
                && out.flush().await.is_ok();
            if !written {
                break;
            }
        }
    });

    use tokio::io::AsyncBufReadExt;
    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        let server = Arc::clone(&server);
        let tx = tx.clone();
        // Requests are handled concurrently so `cancel` / `continue_task`
        // can run while a task is in flight; responses are serialized by the
        // writer task.
        tokio::spawn(async move {
            if let Some(response) = server.handle_message(&line).await {
                let _ = tx.send(response);
            }
        });
    }
    drop(tx);
    let _ = writer.await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Streamable HTTP transport
// ---------------------------------------------------------------------------

/// Build the MCP runtime and serve over the MCP Streamable HTTP transport
/// until the process is stopped. `addr` is `host:port`, e.g.
/// `127.0.0.1:8788`. When `tunnel` is set, the OpenAI Secure MCP Tunnel
/// client is run as a supervised child and stopped together with the server.
pub async fn run_mcp_serve_http(
    config: Arc<kkagent_config::AppConfig>,
    addr: &str,
    token: Option<String>,
    tunnel: Option<TunnelOptions>,
) -> Result<()> {
    let transcript = TranscriptDb::open_default().context("opening transcript registry")?;
    let server = Arc::new(McpServer::new(config, transcript)?);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding MCP HTTP listener on {addr}"))?;
    serve_http(server, listener, token, tunnel).await
}

/// Options for running the OpenAI Secure MCP Tunnel client alongside the
/// HTTP endpoint (`kkagent mcp serve --http --tunnel <TUNNEL_ID>`).
#[derive(Debug, Clone)]
pub struct TunnelOptions {
    /// OpenAI tunnel id (`tunnel_` + 32 hex chars) from Platform tunnel
    /// settings.
    pub tunnel_id: String,
    /// Explicit path to the `tunnel-client` binary; searched on PATH when
    /// `None`.
    pub client_bin: Option<PathBuf>,
    /// OpenAI runtime API key. When `None`, read from the
    /// `CONTROL_PLANE_API_KEY` environment variable.
    pub api_key: Option<String>,
}

/// Serve the MCP protocol over the Streamable HTTP transport.
///
/// The MCP spec (2025-03-26+) defines Streamable HTTP: clients POST JSON-RPC
/// messages to a single endpoint and receive either `application/json` (the
/// response) or an SSE stream; notifications are answered with `202 Accepted`.
/// This server only uses the plain-JSON mode — every tool call here is
/// synchronous from the protocol's point of view, so a stream adds no value.
///
/// Takes an already-bound listener so tests (and callers embedding the
/// endpoint) can bind port 0 and discover the address without a race.
///
/// When `tunnel` is `Some`, the OpenAI Secure MCP Tunnel client is spawned
/// before serving starts and killed when the server stops (Ctrl-C / SIGTERM).
pub async fn serve_http(
    server: Arc<McpServer>,
    listener: tokio::net::TcpListener,
    token: Option<String>,
    tunnel: Option<TunnelOptions>,
) -> Result<()> {
    use axum::routing::{any, get};

    // Auth: `--http-token`, else `KKAGENT_MCP_HTTP_TOKEN`, else the persisted
    // local kkagent HTTP token (already 0600-protected on disk). HTTP is
    // reachable by anything on the network that can route to `addr`, so an
    // unauthenticated run is refused — kkagent mcp can run arbitrary delegated
    // coding tasks.
    let token = match token.clone().or_else(|| {
        std::env::var("KKAGENT_MCP_HTTP_TOKEN")
            .ok()
            .filter(|t| !t.is_empty())
    }) {
        Some(token) => token,
        None => load_or_generate_http_token(),
    };

    let app = axum::Router::new()
        .route("/mcp", any(handle_mcp_post))
        .route("/healthz", get(async || "ok\n"))
        .layer(axum::extract::Extension(Arc::clone(&server)))
        .layer(axum::extract::Extension(HttpToken(Arc::new(token.clone()))))
        .with_state(());

    let mcp_url = mcp_url_for(listener.local_addr().ok());
    eprintln!("kkagent mcp: HTTP endpoint {mcp_url} (Streamable HTTP; bearer auth)");

    // Serve FIRST: tunnel-client starts probing this endpoint (2s budget)
    // as soon as it spawns, and a bound-but-not-accepting listener would
    // let that probe time out. Failures in tunnel setup abort the serve
    // task via the JoinHandle drop below.
    let serve_task = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .context("running MCP HTTP server")
    });

    // Optional OpenAI tunnel: resolve + spawn before parking on the serve
    // task so a bad setup (missing binary, missing API key, client that
    // dies immediately) fails fast instead of serving an endpoint nothing
    // can reach through the tunnel.
    let mut startup_failure_rx = None;
    let tunnel_child = match &tunnel {
        Some(options) => {
            let child = match spawn_tunnel_child(options, &mcp_url, &token).await {
                Ok(child) => Arc::new(child),
                Err(error) => {
                    serve_task.abort();
                    return Err(error);
                }
            };
            // A client that dies right after spawn must fail the serve (a
            // tunnel nobody can reach is worse than no tunnel). The monitor
            // owns that decision — polling survives slow fork+exec that a
            // fixed startup probe window would race — and reports it back.
            let (startup_tx, startup_rx) = tokio::sync::oneshot::channel();
            spawn_tunnel_monitor(&child, Some(startup_tx));
            startup_failure_rx = Some(startup_rx);
            Some(child)
        }
        None => None,
    };

    // Resolves only when the tunnel-client exited within its startup window;
    // stays pending forever otherwise (benign outcomes just drop the
    // sender), so the select below keeps waiting on serve/shutdown.
    let startup_failure = async move {
        if let Some(rx) = startup_failure_rx.as_mut() {
            if let Ok(Err(error)) = rx.await {
                return error;
            }
        }
        std::future::pending().await
    };
    // Pinned so the select can poll the JoinHandle by reference without
    // consuming it (the startup-failure branch still needs `abort()`).
    tokio::pin!(serve_task);
    let outcome = tokio::select! {
        result = &mut serve_task => result.context("MCP HTTP serve task"),
        _ = shutdown_signal() => {
            eprintln!("kkagent mcp: shutting down on signal");
            if let Some(child) = &tunnel_child {
                child.kill().await;
            }
            Ok(Ok(()))
        }
        error = startup_failure => {
            eprintln!("kkagent mcp: failing serve because the tunnel-client died during startup");
            serve_task.abort();
            Ok(Err(error))
        }
    };
    outcome.and_then(|inner| inner)
}
/// Local URL for the bound listener (e.g. `http://127.0.0.1:8788/mcp`),
/// preferring 127.0.0.1 over an unspecified bind address so tunnel-client
/// always dials loopback.
fn mcp_url_for(addr: Option<std::net::SocketAddr>) -> String {
    let Some(addr) = addr else {
        return "http://127.0.0.1/mcp".to_string();
    };
    let host = match addr.ip() {
        std::net::IpAddr::V4(v4) => v4.to_string(),
        std::net::IpAddr::V6(v6) => format!("[{v6}]"),
    };
    let host = if addr.ip().is_unspecified() {
        "127.0.0.1".to_string()
    } else {
        host
    };
    format!("http://{host}:{}/mcp", addr.port())
}

// -- OpenAI Secure MCP Tunnel supervision ------------------------------------

/// Shared handle for the supervised `tunnel-client` child process.
struct TunnelChild {
    child: Mutex<Option<tokio::process::Child>>,
}

impl TunnelChild {
    /// Kill the child (no-op when it already exited or was reaped).
    async fn kill(&self) {
        if let Some(mut child) = self.child.lock().await.take() {
            let _ = child.kill().await;
        }
    }
}

/// Spawn `tunnel-client run` wired to this HTTP endpoint.
///
/// The runtime API key is read from `CONTROL_PLANE_API_KEY` (never argv);
/// the kkagent bearer token is forwarded on every MCP request through
/// `MCP_EXTRA_HEADERS` with an `env:` value reference that tunnel-client
/// resolves itself, so the token stays out of its argv and config files.
async fn spawn_tunnel_child(
    options: &TunnelOptions,
    mcp_url: &str,
    token: &str,
) -> Result<TunnelChild> {
    let bin = resolve_tunnel_client(options.client_bin.as_deref())?;
    let api_key = options
        .api_key
        .clone()
        .filter(|k| !k.is_empty())
        .or_else(|| {
            std::env::var("CONTROL_PLANE_API_KEY")
                .ok()
                .filter(|k| !k.is_empty())
        })
        .context(
            "--tunnel requires CONTROL_PLANE_API_KEY (OpenAI runtime API key with Tunnels \
             Read+Use; create one at https://platform.openai.com/settings/organization/api-keys)",
        )?;

    let mut command = tokio::process::Command::new(&bin);
    command
        .args([
            "run",
            "--control-plane.tunnel-id",
            &options.tunnel_id,
            "--mcp.server-url",
            mcp_url,
        ])
        .env("CONTROL_PLANE_API_KEY", api_key)
        // `env:` references in tunnel-client header values must be the WHOLE
        // value (any other prefix makes it a literal), so the "Bearer " part
        // lives inside the variable.
        .env("KKAGENT_MCP_HTTP_AUTH", format!("Bearer {token}"))
        // Runtime MCP traffic AND discovery/probe requests each have their
        // own static-header set in tunnel-client; both must carry the bearer
        // token or the OpenAI-side discover probe gets a 401 from kkagent.
        .env(
            "MCP_EXTRA_HEADERS",
            "Authorization: env:KKAGENT_MCP_HTTP_AUTH",
        )
        .env(
            "MCP_DISCOVERY_EXTRA_HEADERS",
            "Authorization: env:KKAGENT_MCP_HTTP_AUTH",
        )
        // Poll only the main MCP channel: without an allowlist the client
        // receives harpoon-channel commands too and logs "unsupported
        // channel" errors for every one of them.
        .env("CONTROL_PLANE_POLL_CHANNELS", "main")
        .stdin(std::process::Stdio::null())
        // tunnel-client reports health/ready progress on stdout+stderr;
        // surface both so operators can follow startup in the terminal.
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        // If the serve task is cancelled/dropped without the explicit signal
        // path, dropping the Child must still terminate the client.
        .kill_on_drop(true);
    let child = command
        .spawn()
        .with_context(|| format!("spawning tunnel-client ({})", bin.display()))?;
    eprintln!(
        "kkagent mcp: tunnel-client running (pid {}), forwarding {mcp_url} through tunnel {}",
        child.id().unwrap_or_default(),
        options.tunnel_id
    );
    Ok(TunnelChild {
        child: Mutex::new(Some(child)),
    })
}

/// Startup window: a client exiting within it fails `serve_http` (the tunnel
/// is unusable and likely misconfigured). Generous enough to absorb slow
/// fork+exec under load — the flake mode of the fixed probe this replaces —
/// and overridable via `KKAGENT_TUNNEL_STARTUP_WINDOW_SECS` for tests on
/// pathologically slow file systems.
fn tunnel_startup_window() -> std::time::Duration {
    let secs = std::env::var("KKAGENT_TUNNEL_STARTUP_WINDOW_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(10);
    std::time::Duration::from_secs(secs)
}

/// Reap the tunnel client in the background; the HTTP server keeps serving
/// locally even when the tunnel goes down. Exits during the startup window
/// are reported through `startup_failure` so the server fails instead of
/// serving an unreachable tunnel; later exits are degraded-service warnings.
///
/// Holds only a weak handle: when `serve_http` is dropped/aborted its strong
/// reference goes away, the `Child` is dropped with it and `kill_on_drop`
/// terminates the client even without a graceful shutdown.
fn spawn_tunnel_monitor(
    child: &Arc<TunnelChild>,
    mut startup_failure: Option<tokio::sync::oneshot::Sender<anyhow::Result<()>>>,
) {
    let child = Arc::downgrade(child);
    let started = std::time::Instant::now();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            // Upgrade failed ⇒ the server is gone; kill_on_drop fired.
            let Some(child) = child.upgrade() else {
                break;
            };
            let mut guard = child.child.lock().await;
            match guard.as_mut() {
                // Killed by shutdown — stop monitoring.
                None => break,
                Some(running) => match running.try_wait() {
                    Ok(None) => {
                        if started.elapsed() >= tunnel_startup_window() {
                            // Survived startup: drop the sender so the
                            // server stops waiting on it; later exits are
                            // just warnings.
                            startup_failure = None;
                        }
                    }
                    Ok(Some(status)) => {
                        if started.elapsed() < tunnel_startup_window() {
                            if let Some(sender) = startup_failure.take() {
                                let _ = sender.send(Err(anyhow!(
                                    "tunnel-client exited during startup ({status}); check \
                                     CONTROL_PLANE_API_KEY, the tunnel id, and outbound access \
                                     to api.openai.com:443 (diagnose with `tunnel-client doctor \
                                     --explain`)"
                                )));
                            }
                        } else {
                            eprintln!(
                                "kkagent mcp: WARNING tunnel-client exited ({status}); the \
                                 tunnel is down but the local MCP HTTP server keeps serving"
                            );
                        }
                        guard.take();
                        break;
                    }
                    Err(error) => {
                        eprintln!("kkagent mcp: WARNING tunnel-client wait failed: {error}");
                        break;
                    }
                },
            }
        }
    });
}

/// Platform-appropriate installation hint for `tunnel-client`. Homebrew is
/// the supported channel on macOS (release ZIPs are not notarized and can be
/// blocked by Gatekeeper); everywhere else, official release archives come
/// from GitHub Releases or the Platform tunnel settings download link.
fn tunnel_client_install_hint() -> &'static str {
    match std::env::consts::OS {
        "macos" => "install it with `brew install openai/tools/tunnel-client`",
        "windows" => {
            "download the windows release from \
             https://github.com/openai/tunnel-client/releases/latest"
        }
        _ => {
            "download a release archive from \
             https://github.com/openai/tunnel-client/releases/latest"
        }
    }
}

/// Locate the `tunnel-client` binary: explicit path first, then a PATH
/// search (both `tunnel-client` and `tunnel-client.exe`).
fn resolve_tunnel_client(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(explicit) = explicit {
        if explicit.is_file() {
            return Ok(explicit.to_path_buf());
        }
        return Err(anyhow!("tunnel-client not found at {}", explicit.display()));
    }
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            for name in ["tunnel-client", "tunnel-client.exe"] {
                let candidate = dir.join(name);
                if candidate.is_file() {
                    return Ok(candidate);
                }
            }
        }
    }
    Err(anyhow!(
        "tunnel-client not found on PATH; {}, or pass \
         --tunnel-client /path/to/tunnel-client",
        tunnel_client_install_hint()
    ))
}

/// Resolve when the process is asked to stop (Ctrl-C or SIGTERM).
async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut terminate = signal(SignalKind::terminate()).expect("installing SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = ctrl_c.await;
    }
}

/// Shared bearer token for the HTTP transport.
#[derive(Clone)]
struct HttpToken(Arc<String>);

async fn handle_mcp_post(
    axum::extract::Extension(server): axum::extract::Extension<Arc<McpServer>>,
    axum::extract::Extension(token): axum::extract::Extension<HttpToken>,
    method: axum::http::Method,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> axum::response::Response {
    use axum::http::{header, StatusCode};
    use axum::response::IntoResponse;

    let unauthorized = || {
        (
            StatusCode::UNAUTHORIZED,
            [
                (header::WWW_AUTHENTICATE, "Bearer realm=\"kkagent-mcp\""),
                (header::CONTENT_TYPE, "application/json"),
            ],
            // JSON body: tunnel probes (e.g. the OpenAI tunnel-client
            // discover request) try to parse every response as JSON; a text
            // body surfaces upstream as "malformed_json" transport errors.
            error_response_value(
                Value::Null,
                -32000,
                "unauthorized: missing or invalid bearer token",
            )
            .to_string(),
        )
    };
    let authorized = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|given| constant_time_eq(given.as_bytes(), token.0.as_bytes()));
    if !authorized {
        return unauthorized().into_response();
    }

    // JSON-only Streamable HTTP: no SSE listening stream (GET) and no session
    // lifecycle (DELETE — this server is stateless, it never issues
    // Mcp-Session-Id). Spec-compliant 405 lets Streamable HTTP clients such
    // as the OpenAI tunnel-client http-streamable transport degrade cleanly.
    if method != axum::http::Method::POST {
        return (
            StatusCode::METHOD_NOT_ALLOWED,
            [(header::ALLOW, "POST")],
            "kkagent mcp HTTP transport supports POST (application/json) only\n",
        )
            .into_response();
    }

    // SSE responses are never produced; refuse clients that demand a stream.
    if headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|accept| {
            !accept.contains("application/json") && accept.contains("text/event-stream")
        })
    {
        return (
            StatusCode::NOT_ACCEPTABLE,
            "kkagent mcp HTTP transport returns application/json only\n",
        )
            .into_response();
    }

    let text = match std::str::from_utf8(&body) {
        Ok(text) => text,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                error_response_value(Value::Null, -32700, "Parse error: body is not UTF-8")
                    .to_string(),
            )
                .into_response()
        }
    };
    match server.handle_message(text).await {
        // Notification (no id): no response body per JSON-RPC / MCP spec.
        None => StatusCode::ACCEPTED.into_response(),
        Some(response) => ([(header::CONTENT_TYPE, "application/json")], response).into_response(),
    }
}

/// Length-checked byte equality that does not early-exit on mismatch.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// Resolve the bearer token for the HTTP transport: explicit `--http-token`,
/// then `KKAGENT_MCP_HTTP_TOKEN`, else reuse the persisted local kkagent HTTP
/// token (shared with `kkagent server --http`; 0600 on disk).
fn load_or_generate_http_token() -> String {
    let path = kkagent_config::default_config_dir().join("http_token");
    if let Ok(content) = std::fs::read_to_string(&path) {
        let trimmed = content.trim();
        if !trimmed.is_empty() {
            // Older releases created the token file world-readable; tighten it
            // so a token minted before the 0600-at-creation fix is protected.
            tighten_secret_permissions(&path);
            return trimmed.to_string();
        }
    }
    // Generate a new token and persist it for future restarts.
    let token = uuid::Uuid::new_v4().to_string();
    if let Err(error) = persist_token(&path, &token) {
        eprintln!(
            "kkagent mcp: failed to persist HTTP token to {}: {error}; token will change on restart",
            path.display()
        );
    }
    token
}

/// Write the HTTP token to disk with restrictive permissions (0600 on Unix).
///
/// The mode is applied when the file's inode is first created, not only
/// after the write: a two-step `create → chmod` lets any local process
/// observe (and open) a briefly world-readable token.
fn persist_token(path: &Path, token: &str) -> Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = create_secret_file(path)?;
    file.write_all(token.as_bytes())?;
    file.flush()?;
    Ok(())
}

/// Create (or truncate) a secret file with mode 0600 applied at inode
/// creation time on Unix, so the file is never observable with wider
/// permissions. Windows relies on the default user-profile ACL.
fn create_secret_file(path: &Path) -> Result<std::fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
    }
    #[cfg(not(unix))]
    {
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
    }
}

/// Best-effort restriction of an already-persisted secret file (e.g. written
/// by an older kkagent release without 0600 at creation).
fn tighten_secret_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// One orchestrator-authored execution plan stored with `write_plan` and
/// referenced from `delegate` via `plan_id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredPlan {
    version: u64,
    revisions: std::collections::BTreeMap<u64, String>,
    title: String,
    content: String,
    /// Workspace the plan was authored for (informational).
    workspace: Option<PathBuf>,
}

pub struct McpServer {
    store: CollaborationStore,
    mutation: Mutex<()>,
    config: Arc<kkagent_config::AppConfig>,
    web: Arc<kkagent_tools::WebServicesConfig>,
    transcript: TranscriptDb,
    queue: Arc<Semaphore>,
    tasks: Arc<Mutex<HashMap<String, Arc<McpTask>>>>,
    plans: Arc<Mutex<HashMap<String, StoredPlan>>>,
}

impl McpServer {
    pub fn new(config: Arc<kkagent_config::AppConfig>, transcript: TranscriptDb) -> Result<Self> {
        let web = Arc::new(kkagent_tools::WebServicesConfig::from_app(config.as_ref()));
        let queue = Arc::new(Semaphore::new(config.subagent.effective_max_concurrent()));
        let store = CollaborationStore::new(&transcript);
        let plans = store
            .list("plan")
            .map_err(anyhow::Error::msg)?
            .into_iter()
            .map(|(id, v)| Ok((id, serde_json::from_value(v)?)))
            .collect::<Result<HashMap<String, StoredPlan>>>()?;
        let tasks = store
            .list("task")
            .map_err(anyhow::Error::msg)?
            .into_iter()
            .map(|(id, v)| {
                let record: TaskRecord = serde_json::from_value(v)?;
                let mut task = record.into_task();
                task.isolated = task.worktree.get_mut().is_some();
                Ok((id, Arc::new(task)))
            })
            .collect::<Result<HashMap<String, Arc<McpTask>>>>()?;
        Ok(Self {
            store,
            mutation: Mutex::new(()),
            config,
            web,
            transcript,
            queue,
            tasks: Arc::new(Mutex::new(tasks)),
            plans: Arc::new(Mutex::new(plans)),
        })
    }

    /// Handle one raw stdin line. Returns the JSON-RPC response line to write
    /// back, or `None` for notifications / unparseable noise.
    pub async fn handle_message(self: &Arc<Self>, line: &str) -> Option<String> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return None;
        }
        let Ok(message) = serde_json::from_str::<Value>(trimmed) else {
            return Some(error_response(Value::Null, -32700, "Parse error"));
        };
        // Notifications (no id) get no response, per JSON-RPC and MCP.
        let id = message.get("id").cloned()?;
        let Some(method) = message.get("method").and_then(|v| v.as_str()) else {
            return Some(error_response(
                id,
                -32600,
                "Invalid Request: missing method",
            ));
        };
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        match self.handle_request(method, &params).await {
            Ok(result) => Some(json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string()),
            Err((code, message)) => Some(error_response(id, code, &message)),
        }
    }

    async fn handle_request(
        self: &Arc<Self>,
        method: &str,
        params: &Value,
    ) -> std::result::Result<Value, (i64, String)> {
        match method {
            "initialize" => Ok(self.initialize(params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tool_definitions() })),
            "tools/call" => self.tools_call(params).await,
            // Advertised without capabilities, but answered for friendly
            // interop with clients that probe them.
            "prompts/list" => Ok(json!({ "prompts": [] })),
            "resources/list" => Ok(json!({ "resources": [] })),
            "resources/templates/list" => Ok(json!({ "resourceTemplates": [] })),
            "logging/setLevel" => Ok(json!({})),
            other => Err((-32601, format!("Method not found: {other}"))),
        }
    }

    fn initialize(&self, params: &Value) -> Value {
        let requested = params
            .get("protocolVersion")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let version = if SUPPORTED_PROTOCOL_VERSIONS.contains(&requested) {
            requested
        } else {
            LATEST_PROTOCOL_VERSION
        };
        json!({
            "protocolVersion": version,
            "capabilities": { "tools": {} },
            "serverInfo": {
                "name": SERVER_NAME,
                "version": env!("CARGO_PKG_VERSION"),
            },
            "instructions": "kkagent is an asynchronous coding-agent supervisor designed for \
             orchestrator-driven work: the orchestrator decides and plans, kkagent executes. \
             Workflow: list_workspaces to discover projects (auto-registered from kkagent \
             session history), get_context to load a project's supervisor-level state, inspect \
             to read sources, logs, diffs, images and task artifacts directly, write_plan to \
             store an execution plan and get a plan_id (revise by re-storing with the same \
             plan_id), delegate to start an async coding task as a fresh default session \
             (optionally with plan_id; returns task_id; kkagent picks model and worktree \
             isolation itself), get_progress to poll, continue_task to send new instructions \
             (answer questions / approve actions / steer / continue a task in its original \
             session), get_result for a review-ready summary (image paths included; read them \
             with inspect), cancel to stop a task while keeping its changes and worktree.",
        })
    }

    async fn tools_call(
        self: &Arc<Self>,
        params: &Value,
    ) -> std::result::Result<Value, (i64, String)> {
        let name = params
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if name.is_empty() {
            return Err((-32602, "tools/call requires a tool name".into()));
        }
        let args = params.get("arguments").cloned().unwrap_or(json!({}));
        let mutating = matches!(name, "delegate" | "continue_task" | "write_plan" | "cancel");
        let _guard = if mutating {
            Some(self.mutation.lock().await)
        } else {
            None
        };
        let request_id = args.get("request_id").and_then(Value::as_str);
        let request_key = request_id.map(|id| format!("{name}:{id}"));
        if let Some(key) = &request_key {
            let saved = self.store.list("request").map_err(|e| (-32603, e))?;
            if let Some((_, v)) = saved.into_iter().find(|(id, _)| id == key) {
                if v["arguments"] != args {
                    return Err((
                        -32602,
                        "request_id already used with different arguments".into(),
                    ));
                }
                return Ok(v["result"].clone());
            }
        }
        // inspect may inline image content blocks (image kind); every other
        // tool returns plain text, normalized to a single text block.
        let outcome = match name {
            "get_result" => self.tool_get_result(&args).await,
            "inspect" => self.tool_inspect(&args).await,
            "Glob" | "Grep" => self.tool_search(name, &args).await.map_text_block(),
            "get_plan" => self.tool_get_plan(&args).await.map_text_block(),
            "get_session_context" => self.tool_session_context(&args).await.map_text_block(),
            "list_workspaces" => self.tool_list_workspaces(&args).await.map_text_block(),
            "get_context" => self.tool_get_context(&args).await.map_text_block(),
            "delegate" => self.tool_delegate(&args).await.map_text_block(),
            "write_plan" => self.tool_write_plan(&args).await.map_text_block(),
            "get_progress" => self.tool_get_progress(&args).await.map_text_block(),
            "continue_task" => self.tool_continue_task(&args).await.map_text_block(),
            "cancel" => self.tool_cancel(&args).await.map_text_block(),
            other => return Err((-32602, format!("Unknown tool: {other}"))),
        };
        match outcome {
            Ok((content, structured)) => {
                let mut result = json!({
                    "content": content,
                    "isError": false,
                });
                if let Some(structured) = structured {
                    result["structuredContent"] = structured;
                }
                if let Some(key) = request_key {
                    self.store
                        .put("request", &key, &json!({"arguments":args,"result":result}))
                        .map_err(|e| (-32603, e))?;
                }
                Ok(result)
            }
            // Tool execution failures are reported in-result with isError, so
            // the client shows them as tool output rather than a transport
            // error (only unknown tools / malformed params are protocol errors).
            Err(message) => Ok(json!({
                "content": [{ "type": "text", "text": message }],
                "isError": true,
            })),
        }
    }

    // -- tool implementations ----------------------------------------------

    async fn tool_list_workspaces(
        &self,
        args: &Value,
    ) -> std::result::Result<(String, Option<Value>), String> {
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_WORKSPACE_LIMIT)
            .clamp(1, 100) as usize;
        // Auto-registration: every directory that ever hosted a kkagent
        // session is a workspace. Merge configured trusted roots.
        let records = self
            .transcript
            .list_sessions(500)
            .map_err(|e| format!("transcript registry unavailable: {e}"))?;
        let mut entries: HashMap<String, (u64, Option<String>, bool)> = HashMap::new();
        for record in records.iter().filter(|r| !r.is_archived) {
            let dir = std::fs::canonicalize(&record.working_dir)
                .unwrap_or_else(|_| PathBuf::from(&record.working_dir));
            let key = dir.to_string_lossy().to_string();
            let entry = entries.entry(key).or_insert((0, None, false));
            entry.0 += 1;
            // updated_at is ISO-8601; lexicographic max == most recent.
            if entry
                .1
                .as_deref()
                .is_none_or(|last| record.updated_at.as_str() > last)
            {
                entry.1 = Some(record.updated_at.clone());
            }
        }
        for root in trusted_roots(&self.config) {
            let key = root.to_string_lossy().to_string();
            entries.entry(key).or_insert((0, None, true));
        }
        let tasks = self.tasks.lock().await;
        let mut workspaces: Vec<Value> = entries
            .into_iter()
            .map(|(path, (sessions, last_active, _trusted))| {
                let dir = PathBuf::from(&path);
                let (running, waiting, total) = tasks
                    .values()
                    .filter(|t| t.origin_workspace == dir)
                    .fold((0u64, 0u64, 0u64), |acc, t| {
                        let phase = t.phase();
                        (
                            acc.0
                                + u64::from(
                                    phase == TaskPhase::Running || phase == TaskPhase::Queued,
                                ),
                            acc.1
                                + u64::from(
                                    phase == TaskPhase::AwaitingInput
                                        || phase == TaskPhase::AwaitingPermission,
                                ),
                            acc.2 + 1,
                        )
                    });
                json!({
                    "path": path,
                    "exists": dir.exists(),
                    "is_git_repo": dir.join(".git").exists(),
                    "has_agents_md": dir.join("AGENTS.md").exists(),
                    "session_count": sessions,
                    "last_active": last_active,
                    "tasks": { "running": running, "waiting": waiting, "total": total },
                })
            })
            .collect();
        // Most recently active first; never-active trusted roots last, then
        // trim to the requested page size.
        workspaces.sort_by(|a, b| {
            let active = |w: &Value| w["last_active"].as_str().unwrap_or("").to_string();
            active(b).cmp(&active(a)).then_with(|| {
                b["session_count"]
                    .as_u64()
                    .unwrap_or(0)
                    .cmp(&a["session_count"].as_u64().unwrap_or(0))
            })
        });
        let total_count = workspaces.len();
        workspaces.truncate(limit);
        let text = workspaces
            .iter()
            .map(|w| {
                format!(
                    "- {}{}{} sessions={} tasks={}{}",
                    w["path"].as_str().unwrap_or("?"),
                    if w["is_git_repo"].as_bool().unwrap_or(false) {
                        " (git)"
                    } else {
                        ""
                    },
                    if w["has_agents_md"].as_bool().unwrap_or(false) {
                        " AGENTS.md"
                    } else {
                        ""
                    },
                    w["session_count"],
                    w["tasks"]["total"],
                    if w["tasks"]["running"].as_u64().unwrap_or(0) > 0 {
                        format!(" running={}", w["tasks"]["running"])
                    } else {
                        String::new()
                    },
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        Ok((
            if text.is_empty() {
                "no workspaces registered yet".to_string()
            } else {
                format!(
                    "{text}\n\n(showing {} of {total_count} workspaces; pass `limit` for more)",
                    workspaces.len()
                )
            },
            Some(json!({
                "workspaces": workspaces,
                "total_count": total_count,
                "limit": limit,
            })),
        ))
    }

    async fn tool_get_context(
        &self,
        args: &Value,
    ) -> std::result::Result<(String, Option<Value>), String> {
        let workspace = self.resolve_workspace_arg(args).await?;
        let git = git_snapshot(&workspace).await;
        let records = self
            .transcript
            .list_sessions(500)
            .map_err(|e| format!("transcript registry unavailable: {e}"))?;
        let mut sessions: Vec<Value> = records
            .iter()
            .filter(|r| !r.is_archived && same_dir(Path::new(&r.working_dir), &workspace))
            .take(8)
            .map(|r| {
                json!({
                    "session_id": r.session_id,
                    "title": r.title,
                    "updated_at": r.updated_at,
                    "message_count": r.message_count,
                })
            })
            .collect();

        let tasks = self.tasks.lock().await;
        let mut task_list: Vec<Value> = Vec::new();
        let mut attention: Vec<Value> = Vec::new();
        for task in tasks.values().filter(|t| t.origin_workspace == workspace) {
            let entry = json!({
                "task_id": task.id,
            "session_id": task.session_id,
                "kind": TASK_KIND,
                "status": task.status(),
                "description": task.description,
            });
            let phase = task.phase();
            if matches!(
                phase,
                TaskPhase::AwaitingInput | TaskPhase::AwaitingPermission | TaskPhase::Failed
            ) {
                attention.push(json!({
                        "task_id": task.id,
                "session_id": task.session_id,
                        "reason": match phase {
                            TaskPhase::AwaitingInput => "waiting for input (question pending)",
                            TaskPhase::AwaitingPermission => "waiting for permission approval",
                            _ => "failed; inspect with get_result",
                        },
                    }));
            }
            task_list.push(entry);
        }
        // Most recent sessions first (list_sessions is ordered by recency).
        sessions.truncate(5);

        let payload = json!({
            "workspace": workspace.to_string_lossy(),
            "trusted": is_trusted(&self.config, &workspace),
            "git": git,
            "recent_sessions": sessions,
            "tasks": task_list,
            "attention": attention,
        });
        let mut text = format!(
            "workspace {}{}{}",
            workspace.to_string_lossy(),
            if is_trusted(&self.config, &workspace) {
                ""
            } else {
                " (untrusted)"
            },
            match &git {
                Some(g) => format!(
                    "; git branch={} head={} dirty_files={}",
                    g["branch"].as_str().unwrap_or("?"),
                    g["head_subject"].as_str().unwrap_or(""),
                    g["dirty_files"],
                ),
                None => "; not a git repo".to_string(),
            },
        );
        if !attention.is_empty() {
            text.push_str(&format!("; {} item(s) need attention", attention.len()));
        }
        Ok((text, Some(payload)))
    }

    /// Direct read-only inspect: fetch a specified resource from a workspace —
    /// source code, text, logs, git diffs, images, and task artifacts. No
    /// agent involvement — the file (or diff) is returned immediately. Images
    /// come back as MCP image content blocks; text content supports line
    /// windowing.
    async fn tool_inspect(
        &self,
        args: &Value,
    ) -> std::result::Result<(Vec<Value>, Option<Value>), String> {
        let workspace = self.resolve_workspace_arg(args).await?;
        let kind = args
            .get("kind")
            .and_then(|v| v.as_str())
            .unwrap_or("auto")
            .trim()
            .to_ascii_lowercase();

        // Git diff: no path required (whole-worktree diff, optionally scoped).
        if kind == "diff" {
            return collaboration::inspect_diff(&workspace, args).await;
        }

        let raw = args
            .get("path")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "path is required (kind=diff may omit it)".to_string())?;
        let candidate = {
            let p = PathBuf::from(raw);
            if p.is_absolute() {
                p
            } else {
                workspace.join(p)
            }
        };
        if !candidate.exists() {
            return Err(format!("path does not exist: {}", candidate.display()));
        }
        // Confinement: the resolved file must stay inside the workspace.
        let resolved = std::fs::canonicalize(&candidate)
            .map_err(|e| format!("cannot resolve {}: {e}", candidate.display()))?;
        let confined = std::fs::canonicalize(&workspace).unwrap_or_else(|_| workspace.clone());
        if !resolved.starts_with(&confined) {
            return Err(format!(
                "path {} escapes workspace {}",
                resolved.display(),
                confined.display()
            ));
        }
        if resolved.is_dir() {
            return Err(format!(
                "{} is a directory; inspect reads a single file (use get_context for directory listings)",
                resolved.display()
            ));
        }
        let metadata = std::fs::metadata(&resolved)
            .map_err(|e| format!("cannot stat {}: {e}", resolved.display()))?;
        if metadata.len() > MAX_ARTIFACT_BYTES {
            return Err(format!(
                "{} is {} bytes; inspect reads files up to {MAX_ARTIFACT_BYTES} bytes",
                resolved.display(),
                metadata.len()
            ));
        }

        let detected = detect_inspect_kind(&resolved);
        let kind = if kind == "auto" { detected } else { kind };

        if kind == "image" {
            let Some(media_type) = image_media_type(&resolved) else {
                return Err(format!(
                    "{} is not a readable image type",
                    resolved.display()
                ));
            };
            let data = std::fs::read(&resolved)
                .map_err(|e| format!("cannot read {}: {e}", resolved.display()))?;
            let payload = json!({
                "workspace": workspace.to_string_lossy(),
                "kind": "image",
                "path": resolved.to_string_lossy(),
                "media_type": media_type,
                "bytes": data.len(),
            });
            // Image kind: text summary block followed by the inline image.
            return Ok((
                vec![
                    json!({ "type": "text", "text": format!("image {} ({media_type}, {} bytes)", resolved.display(), data.len()) }),
                    json!({ "type": "image", "data": BASE64.encode(&data), "mimeType": media_type }),
                ],
                Some(payload),
            ));
        }

        // Text: source code, logs, configs, any readable text resource. Support a
        // line window (offset/limit) so huge logs stay bounded.
        let bytes = std::fs::read(&resolved)
            .map_err(|e| format!("cannot read {}: {e}", resolved.display()))?;
        let full = String::from_utf8_lossy(&bytes);
        let total_lines = full.lines().count();
        let offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_INSPECT_LINE_LIMIT)
            .clamp(1, 5_000) as usize;
        let selected: Vec<&str> = full.lines().skip(offset).take(limit).collect();
        let truncated = offset + selected.len() < total_lines;
        let mut text = selected.join("\n");
        if text.is_empty() {
            text = "(empty file)".to_string();
        } else if truncated {
            text.push_str(&format!(
                "\n\n[lines {}..{} of {total_lines}; pass offset/limit to page through]",
                offset + 1,
                offset + selected.len()
            ));
        }
        // Put the file body in structuredContent too: OpenAI hosts (ChatGPT /
        // Codex) drop content[] whenever structuredContent is present, so a
        // metadata-only structured payload makes inspect look empty there.
        let payload = json!({
            "workspace": workspace.to_string_lossy(),
            "kind": kind,
            "path": resolved.to_string_lossy(),
            "bytes": bytes.len(),
            "total_lines": total_lines,
            "offset": offset,
            "returned_lines": selected.len(),
            "truncated": truncated,
            "text": text.clone(),
        });
        Ok((vec![json!({ "type": "text", "text": text })], Some(payload)))
    }

    /// Store an orchestrator-authored execution plan (ChatGPT web decides and
    /// plans; kkagent executes). Revisions use the same `plan_id`. Plans only
    /// take effect when a task is delegated with the matching `plan_id` —
    /// later revisions never alter already-running tasks.
    async fn tool_write_plan(
        &self,
        args: &Value,
    ) -> std::result::Result<(String, Option<Value>), String> {
        let content = args
            .get("plan")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                "plan is required (markdown: goal, steps, acceptance criteria)".to_string()
            })?;
        let chars = content.chars().count();
        if chars > MAX_PLAN_CHARS {
            return Err(format!(
                "plan is {chars} chars; write_plan accepts up to {MAX_PLAN_CHARS}"
            ));
        }
        let title = args
            .get("title")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| clip_chars(content.lines().next().unwrap_or("plan"), 80));
        // Optional informational workspace binding; validated when provided.
        let workspace = if args.get("workspace").is_some() || args.get("working_dir").is_some() {
            Some(self.resolve_workspace_arg(args).await?)
        } else {
            None
        };

        let plan_id = match args
            .get("plan_id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(plan_id) => {
                let exists = self.plans.lock().await.contains_key(plan_id);
                if !exists {
                    return Err(format!(
                        "unknown plan_id: {plan_id}; omit plan_id to create a new plan"
                    ));
                }
                plan_id.to_string()
            }
            None => uuid::Uuid::new_v4().to_string(),
        };
        let mut plans = self.plans.lock().await;
        let mut revisions = plans
            .get(&plan_id)
            .map(|p| p.revisions.clone())
            .unwrap_or_default();
        let version = plans.get(&plan_id).map_or(1, |p| p.version + 1);
        revisions.insert(version, content.to_string());
        let stored = StoredPlan {
            title: title.clone(),
            content: content.to_string(),
            workspace,
            version,
            revisions,
        };
        self.store.put(
            "plan",
            &plan_id,
            &serde_json::to_value(&stored).map_err(|e| e.to_string())?,
        )?;
        plans.insert(plan_id.clone(), stored);

        Ok((
            format!(
                "plan {plan_id} stored: {title} ({chars} chars). Delegate work against it \
                 with delegate or continue_task and plan_id/plan_version; later revisions \
                 never modify running tasks implicitly."
            ),
            Some(json!({
                "plan_id": plan_id,
                "title": title,
                "chars": chars,
                "plan_version": version,
            })),
        ))
    }

    async fn tool_delegate(
        self: &Arc<Self>,
        args: &Value,
    ) -> std::result::Result<(String, Option<Value>), String> {
        let prompt = args
            .get("prompt")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "prompt is required".to_string())?
            .to_string();
        let workspace = self.resolve_workspace_arg(args).await?;
        if !is_trusted(&self.config, &workspace) {
            return Err(format!(
                "workspace {} is not a trusted workspace; add it to `trusted_workspaces` in kkagent config",
                workspace.display()
            ));
        }
        let description = clip_chars(&prompt, 80);

        // Optional orchestrator-authored plan (write_plan): looked up before
        // any state is created, so unknown ids fail without side effects.
        let plan = match args
            .get("plan_id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(plan_id) => {
                let stored = self
                    .plans
                    .lock()
                    .await
                    .get(plan_id)
                    .ok_or_else(|| {
                        format!("unknown plan_id: {plan_id} (create it with write_plan first)")
                    })?
                    .clone();
                // The plan's workspace binding is informational; surface a
                // mismatch so a plan authored for another project is visible.
                if let Some(plan_ws) = &stored.workspace {
                    if plan_ws != &workspace {
                        eprintln!(
                            "kkagent mcp: WARNING plan {plan_id} was authored for {} but is \
                             being executed in {}",
                            plan_ws.display(),
                            workspace.display()
                        );
                    }
                }
                let version = args["plan_version"].as_u64().unwrap_or(stored.version);
                let content = stored
                    .revisions
                    .get(&version)
                    .ok_or("unknown plan_version")?
                    .clone();
                Some((plan_id.to_string(), stored.title, content))
            }
            None => None,
        };
        let (plan_id, plan_title, plan) = match plan {
            Some((id, title, content)) => (Some(id), Some(title), Some(content)),
            None => (None, None, None),
        };

        // kkagent decides worktree isolation itself: git repos with the
        // worktree feature enabled run in an isolated worktree so the main
        // checkout and other tasks stay untouched; everything else runs in
        // place.
        let is_git = workspace.join(".git").exists();
        let isolated = is_git && kkagent_tools::git_worktree::worktree_enabled();

        let task_id = uuid::Uuid::new_v4().to_string();
        let (run_dir, worktree) = if isolated && is_git {
            match kkagent_tools::git_worktree::create_worktree(&workspace, &task_id, None).await {
                Ok(info) => (info.path.clone(), Some(info)),
                Err(error) => {
                    return Err(format!(
                        "failed to create worktree for {}: {error}",
                        workspace.display()
                    ));
                }
            }
        } else {
            (workspace.clone(), None)
        };
        let base_commit = git_head(&run_dir).await;

        let task = Arc::new(McpTask {
            id: task_id.clone(),
            resume: false,
            plan_ref: StdMutex::new(if plan_id.is_some() {
                self.tool_get_plan(args).await?.1
            } else {
                None
            }),
            review: StdMutex::new("pending".into()),
            reviewed_snapshot: StdMutex::new(None),
            event_sequence: std::sync::atomic::AtomicU64::new(0),
            session_id: format!("mcp-{task_id}"),
            description: description.clone(),
            prompt: prompt.clone(),
            plan_title: plan_title.clone(),
            plan: plan.clone(),
            origin_workspace: workspace.clone(),
            run_dir: Mutex::new(run_dir.clone()),
            worktree: Mutex::new(worktree),
            isolated,
            base_commit: StdMutex::new(base_commit),
            interrupt: Arc::new(AtomicBool::new(false)),
            mailbox: SessionSteerMailbox::default(),
            question_tx: StdMutex::new(None),
            approval_tx: StdMutex::new(None),
            runner_notify: Notify::new(),
            runner: StdMutex::new(RunnerSlot::initial()),
            phase: StdMutex::new(TaskPhase::Queued),
            progress: StdMutex::new(Progress::default()),
            activity: StdMutex::new(ActivityStamp::default()),
            recent_events: StdMutex::new(Vec::new()),
            pending_question: StdMutex::new(None),
            pending_approval: StdMutex::new(None),
            pending_instructions: StdMutex::new(Vec::new()),
            summary: StdMutex::new(TaskSummary::default()),
            error: StdMutex::new(None),
            started_at: Instant::now(),
            finished_at: StdMutex::new(None),
        });
        self.persist_task(&task).await?;
        self.tasks
            .lock()
            .await
            .insert(task_id.clone(), Arc::clone(&task));

        // RunnerSlot::initial() reports `exited`, so the initial runner goes
        // through the same slot protocol as every respawn.
        self.ensure_runner(&task);

        Ok((
            format!(
                "task {task_id} queued: {description} (workspace {}{})",
                workspace.display(),
                if isolated {
                    format!(", isolated worktree at {}", run_dir.display())
                } else {
                    String::new()
                }
            ),
            Some(json!({
                "task_id": task_id,
                "session_id": task.session_id,
                "kind": TASK_KIND,
                "status": "queued",
                "workspace": workspace.to_string_lossy(),
                "run_dir": run_dir.to_string_lossy(),
                "isolated": isolated,
                "plan_id": plan_id,
            })),
        ))
    }

    async fn tool_get_progress(
        &self,
        args: &Value,
    ) -> std::result::Result<(String, Option<Value>), String> {
        let task = self.require_task(args).await?;
        let phase = task.phase();
        let (events, event_cursor, events_lost) = {
            let events = task.recent_events.lock().unwrap_or_else(|e| e.into_inner());
            let cursor = task.event_sequence.load(Ordering::SeqCst);
            let after = args["after_event"].as_u64().unwrap_or(0);
            (
                events
                    .iter()
                    .filter(|e| {
                        e.split(':')
                            .next()
                            .and_then(|s| s.parse::<u64>().ok())
                            .unwrap_or(0)
                            > after
                    })
                    .cloned()
                    .collect::<Vec<_>>(),
                cursor,
                after < cursor.saturating_sub(events.len() as u64),
            )
        };
        let (runner_state, pending_instruction_count, activity_ages) = {
            let state = task.runner_state();
            let pending = task
                .pending_instructions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .len();
            let ages = task
                .activity
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .ages();
            (state, pending, ages)
        };
        let (last_progress_age, last_model_age, last_tool_age) = activity_ages;
        let payload = json!({
            "task_id": task.id,
            "session_id": task.session_id,
            "kind": TASK_KIND,
            "status": phase.as_str(),
            "description": task.description,
            "plan_title": task.plan_title,
            "workspace": task.origin_workspace.to_string_lossy(),
            "run_dir": task.run_dir.lock().await.to_string_lossy(),
            "isolated": task.isolated,
            "elapsed_seconds": task.elapsed_seconds(),
            "progress": task.progress_snapshot(),
            "recent_events": events,
            "event_cursor":event_cursor,"events_lost":events_lost,
            "review_status":task.review.lock().unwrap_or_else(|e|e.into_inner()).clone(),
            "plan":task.plan_ref.lock().unwrap_or_else(|e|e.into_inner()).clone(),
            "pending_question": task.pending_question.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|q| json!({
                "question_id": q.question_id,
                "text": q.text,
                "options": q.options,
                "allow_free_text": q.allow_free_text,
                "allow_multiple": q.allow_multiple,
            })),
            "pending_approval": task.pending_approval.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|a| json!({
                "approval_id": a.approval_id,
                "tool_name": a.tool_name,
                "action": a.action,
                "tool_input_display": a.tool_input_display,
            })),
            "instructions_received": pending_instruction_count,
            "pending_instruction_count": pending_instruction_count,
            // `running` (a turn can run) | `waiting_for_instruction` (runner
            // parked, instructions would start a turn immediately) | `exited`
            // (no runner; with pending instructions > 0 this would be a bug
            // because an accepted instruction must have a runner).
            "runner_state": runner_state,
            // Seconds since the last observable activity; `null` = never.
            // A growing `last_model_activity_age_seconds` with
            // `runner_state == "running"` means the model is thinking or the
            // request is stuck; `last_tool_activity_age_seconds` growing
            // alone points at a long-running/blocked tool.
            "last_progress_at": last_progress_age.map(|s| json!(s)).unwrap_or(Value::Null),
            "last_progress_age_seconds": last_progress_age,
            "last_model_activity_age_seconds": last_model_age,
            "last_tool_activity_age_seconds": last_tool_age,
            "error": task.error.lock().unwrap_or_else(|e| e.into_inner()).clone(),
        });
        let mut text = format!(
            "task {} is {} (runner: {})",
            task.id,
            phase.as_str(),
            runner_state
        );
        if phase == TaskPhase::AwaitingInput {
            if let Some(q) = task
                .pending_question
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
            {
                text.push_str(&format!("; question: {}", q.text));
            }
        }
        if phase == TaskPhase::AwaitingPermission {
            if let Some(a) = task
                .pending_approval
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
            {
                text.push_str(&format!(
                    "; approval needed for {} ({})",
                    a.tool_name, a.action
                ));
            }
        }
        if let Some(error) = task.error.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            text.push_str(&format!("; error: {error}"));
        }
        Ok((text, Some(payload)))
    }

    async fn tool_continue_task(
        self: &Arc<Self>,
        args: &Value,
    ) -> std::result::Result<(String, Option<Value>), String> {
        if args.get("plan_version").is_some() && args.get("plan_id").is_none() {
            return Err("plan_version requires plan_id".into());
        }
        if args
            .get("instruction")
            .and_then(Value::as_str)
            .is_none_or(|s| s.trim().is_empty())
            && !args["decision"].is_object()
            && args.get("plan_id").is_none()
            && !args["review"].is_object()
        {
            return Err("instruction or decision is required".into());
        }
        let plan = self.plan_instruction(args).await?;
        let task = if args.get("session_id").is_some() {
            self.resume_task(args).await?
        } else {
            self.require_task(args).await?
        };
        if let Some(review) = args.get("review") {
            if !task.phase().terminal() {
                return Err("review requires a finished task".into());
            }
            if args.get("instruction").is_some()
                || args.get("decision").is_some()
                || args.get("plan_id").is_some()
            {
                return Err("review cannot be combined with instruction, decision or plan".into());
            }
            let expected = review["head"].as_str().ok_or("review.head is required")?;
            let dir = task.run_dir.lock().await.clone();
            if git_head(&dir).await.as_deref() != Some(expected) {
                return Err("review head changed; inspect the latest code first".into());
            }
            let snapshot = review["snapshot"]
                .as_str()
                .ok_or("review.snapshot is required (from get_result)")?;
            if collaboration::review_snapshot(&dir).await? != snapshot {
                return Err("review snapshot changed; inspect the latest code first".into());
            }
            let accepted = review["accepted"]
                .as_bool()
                .ok_or("review.accepted is required")?;
            *task.review.lock().unwrap_or_else(|e| e.into_inner()) = if accepted {
                "accepted"
            } else {
                "changes_requested"
            }
            .into();
            *task
                .reviewed_snapshot
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(snapshot.into());
            self.persist_task(&task).await?;
            return Ok((
                "review recorded".into(),
                Some(
                    json!({"task_id":task.id,"session_id":task.session_id,"review":task.review.lock().unwrap_or_else(|e|e.into_inner()).clone()}),
                ),
            ));
        }
        let mut instruction = args
            .get("instruction")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let decision = args.get("decision").cloned().filter(Value::is_object);
        if let Some((text, _)) = &plan {
            instruction = Some(format!("{text}\n{}", instruction.unwrap_or_default()));
        }
        if instruction.is_none() && decision.is_none() {
            return Err("instruction or decision is required".into());
        }
        if let Some((_, metadata)) = &plan {
            if decision.is_some()
                || matches!(
                    task.phase(),
                    TaskPhase::AwaitingInput | TaskPhase::AwaitingPermission
                )
            {
                return Err("apply a plan with a follow-up instruction after resolving the pending decision".into());
            }
            *task.plan_ref.lock().unwrap_or_else(|e| e.into_inner()) = Some(metadata.clone());
        }
        if task
            .pending_instructions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
            >= PENDING_INSTRUCTIONS_CAP
        {
            return Err("too many pending instructions".into());
        }
        let outcome: Result<(String, Option<Value>), String> = match task.phase() {
            TaskPhase::AwaitingInput => {
                // Answer the pending question; the agent continues its turn.
                let Some(question) = task
                    .pending_question
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()
                else {
                    return Err("no pending question".into());
                };
                let response = match &decision {
                    // Structured decision: explicit option ids, free text or
                    // dismissal — no string matching involved.
                    Some(decision) => {
                        if let Some(expected) = decision.get("question_id").and_then(|v| v.as_str())
                        {
                            if expected != question.question_id {
                                return Err(format!(
                                    "decision targets question `{expected}` but the pending \
                                     question is `{}`",
                                    question.question_id
                                ));
                            }
                        }
                        let cancelled = decision
                            .get("cancelled")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        let mut selected = decision
                            .get("selected_option_ids")
                            .and_then(|v| v.as_array())
                            .map(|ids| {
                                ids.iter()
                                    .filter_map(|v| v.as_str().map(str::to_string))
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default();
                        for id in &selected {
                            if !question.options.iter().any(|option| &option.id == id) {
                                return Err(format!(
                                    "unknown option id `{id}` for question `{}`; valid ids: {}",
                                    question.question_id,
                                    question
                                        .options
                                        .iter()
                                        .map(|o| o.id.as_str())
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                ));
                            }
                        }
                        let free_text = decision
                            .get("free_text")
                            .and_then(|v| v.as_str())
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(str::to_string)
                            .or_else(|| instruction.clone());
                        if !cancelled && selected.is_empty() && free_text.is_none() {
                            return Err(
                                "decision must include selected_option_ids, free_text, or \
                                 cancelled=true"
                                    .into(),
                            );
                        }
                        if cancelled {
                            selected.clear();
                        }
                        QuestionResponse {
                            question_id: question.question_id.clone(),
                            selected_option_ids: selected,
                            free_text: if cancelled { None } else { free_text },
                            cancelled,
                        }
                    }
                    // Plain-text fallback: the instruction answers the
                    // question verbatim or as free text.
                    None => {
                        let Some(instruction) = instruction.clone() else {
                            return Err(
                                "instruction is required to answer the pending question".into()
                            );
                        };
                        QuestionResponse {
                            question_id: question.question_id.clone(),
                            selected_option_ids: select_option_ids(&question, &instruction),
                            free_text: Some(instruction),
                            cancelled: false,
                        }
                    }
                };
                task.push_event(format!(
                    "input: {}",
                    clip_chars(
                        response
                            .free_text
                            .as_deref()
                            .unwrap_or("<options selected>"),
                        120
                    )
                ));
                task.send_question_answer(response)?;
                task.take_pending_question();
                task.set_phase(TaskPhase::Running);
                Ok((
                    format!("answer delivered to task {}", task.id),
                    Some(json!({
                                "task_id": task.id,
                    "session_id": task.session_id,
                                "status": task.status(),
                                "action": "question_answered",
                            })),
                ))
            }
            TaskPhase::AwaitingPermission => {
                // Approve only on explicit decision or explicit approval
                // wording; anything else rejects the action (fail closed) and
                // carries the text as guidance for the agent's next step.
                let Some(request) = task
                    .pending_approval
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()
                else {
                    return Err("no pending approval".into());
                };
                if let Some(expected) = decision.as_ref().and_then(|d| d["approval_id"].as_str()) {
                    if expected != request.approval_id {
                        return Err("approval_id does not match pending approval".into());
                    }
                }
                let (approve, feedback) = match &decision {
                    Some(decision) => {
                        let Some(approve) = decision.get("approve").and_then(|v| v.as_bool())
                        else {
                            return Err(
                                "decision.approve is required to resolve a permission request"
                                    .into(),
                            );
                        };
                        let feedback = if approve {
                            None
                        } else {
                            decision
                                .get("feedback")
                                .and_then(|v| v.as_str())
                                .map(str::to_string)
                                .or(instruction.clone())
                        };
                        (approve, feedback)
                    }
                    None => {
                        let Some(instruction) = instruction.clone() else {
                            return Err(
                                "instruction is required to resolve the pending approval".into()
                            );
                        };
                        let approve = matches!(
                            instruction.trim().to_ascii_lowercase().as_str(),
                            "approve" | "approved" | "allow" | "allow_once" | "yes" | "y" | "ok"
                        );
                        let feedback = if approve { None } else { Some(instruction) };
                        (approve, feedback)
                    }
                };
                task.push_event(format!(
                    "permission {} for {}",
                    if approve { "approved" } else { "rejected" },
                    request.tool_name
                ));
                task.send_approval(ApprovalResponse {
                    approval_id: request.approval_id,
                    decision: if approve {
                        ApprovalDecision::Approved
                    } else {
                        ApprovalDecision::Rejected
                    },
                    scope: None,
                    feedback,
                    selected_label: None,
                })?;
                task.take_pending_approval();
                task.set_phase(TaskPhase::Running);
                Ok((
                    format!(
                        "permission {} for task {}",
                        if approve { "approved" } else { "rejected" },
                        task.id
                    ),
                    Some(json!({
                                "task_id": task.id,
                    "session_id": task.session_id,
                                "status": task.status(),
                                "action": if approve { "approved" } else { "rejected" },
                            })),
                ))
            }
            TaskPhase::Queued | TaskPhase::Running => {
                if decision.is_some() {
                    return Err(
                        "task has no pending question or approval; use instruction to steer it"
                            .into(),
                    );
                }
                let Some(instruction) = instruction.clone() else {
                    return Err("instruction is required".into());
                };
                // Steer the active turn when the mailbox is open; when it is
                // closed (turn ended / not started) park the instruction for
                // the runner to apply as the next turn of the SAME task.
                if task
                    .mailbox
                    .try_push(SteerInput {
                        text: instruction.clone(),
                        images: Vec::new(),
                    })
                    .is_err()
                {
                    task.pending_instructions
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(instruction);
                }
                // Backstop with the same contract as the terminal branch: if
                // the phase was stale (runner already gone between the last
                // turn ending and this call), a fresh runner is spawned to
                // consume the parked instruction.
                self.ensure_runner(&task);
                task.runner_notify.notify_one();
                Ok((
                    format!("instruction delivered to running task {}", task.id),
                    Some(json!({
                                "task_id": task.id,
                    "session_id": task.session_id,
                                "status": task.status(),
                                "action": "instruction_queued",
                            })),
                ))
            }
            TaskPhase::Completed | TaskPhase::Failed | TaskPhase::Cancelled => {
                if decision.is_some() {
                    return Err(
                        "task has no pending question or approval; use instruction to continue it"
                            .into(),
                    );
                }
                if instruction.is_none() {
                    return Err("instruction is required to continue a finished task".into());
                }
                // Continue the finished task in its original session: wake
                // the runner, which appends the instruction and runs a new
                // turn (no new agent session is created).
                if !is_trusted(&self.config, &task.origin_workspace) {
                    return Err("not a trusted workspace".into());
                }
                task.interrupt.store(false, Ordering::SeqCst);
                task.pending_instructions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(instruction.unwrap());
                *task.review.lock().unwrap_or_else(|e| e.into_inner()) = "pending".into();
                *task.error.lock().unwrap_or_else(|e| e.into_inner()) = None;
                *task.finished_at.lock().unwrap_or_else(|e| e.into_inner()) = None;
                task.set_phase(TaskPhase::Queued);
                // Reliability contract: an accepted continue guarantees the
                // instruction is consumed. `ensure_runner` decides under the
                // slot lock — the same lock the dying runner holds for its
                // final pre-exit check — so either the parked runner is alive
                // and will pick the instruction up, or a fresh runner is
                // spawned here to consume it. No lost wakeup is possible.
                self.ensure_runner(&task);
                task.runner_notify.notify_one();
                self.persist_task(&task).await?;
                Ok((
                    format!(
                        "task {} will continue with the new instruction in its original session",
                        task.id
                    ),
                    Some(json!({
                                "task_id": task.id,
                    "session_id": task.session_id,
                                "status": task.status(),
                                "action": "task_continued",
                            })),
                ))
            }
        };
        let (text, mut payload) = outcome?;
        if let Some(value) = &mut payload {
            value["run_dir"] = json!(task.run_dir.lock().await.clone());
            value["workspace"] = json!(task.origin_workspace);
            value["plan"] = task
                .plan_ref
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
                .unwrap_or(Value::Null);
        }
        self.persist_task(&task).await?;
        Ok((text, payload))
    }
    async fn tool_get_result(
        &self,
        args: &Value,
    ) -> std::result::Result<(Vec<Value>, Option<Value>), String> {
        let task = self.require_task(args).await?;
        let phase = task.phase();
        let summary = task.summary_snapshot();
        let worktree = task.worktree.lock().await.clone();
        let error = task.error.lock().unwrap_or_else(|e| e.into_inner()).clone();
        // Image paths are re-scanned at call time: the runner may have been
        // aborted (cancel) before publishing a summary, or finished between
        // the last summary refresh and this call. Content is NOT inlined
        // here; callers read images with the `inspect` tool.
        let run_dir = task.run_dir.lock().await.clone();
        let images = collect_task_image_paths(&run_dir).await;
        let head = git_head(&run_dir).await;
        let snapshot = collaboration::review_snapshot(&run_dir).await.ok();
        let status = tokio::process::Command::new("git")
            .args(["status", "--porcelain=v1", "--untracked-files=all"])
            .current_dir(&run_dir)
            .output()
            .await
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
        let payload = json!({
            "task_id": task.id,
            "session_id": task.session_id,
            "kind": TASK_KIND,
            "status": phase.as_str(),
            "reviewed_snapshot":task.reviewed_snapshot.lock().unwrap_or_else(|e|e.into_inner()).clone(),
            "snapshot":snapshot,"head":head,"base":task.base_commit.lock().unwrap_or_else(|e|e.into_inner()).clone(),
            "working_tree_status":status,"review_status":task.review.lock().unwrap_or_else(|e|e.into_inner()).clone(),
            "plan":task.plan_ref.lock().unwrap_or_else(|e|e.into_inner()).clone(),
            "description": task.description,
            "workspace": task.origin_workspace.to_string_lossy(),
            "run_dir": task.run_dir.lock().await.to_string_lossy(),
            "worktree": worktree.as_ref().map(|w| json!({
                "path": w.path.to_string_lossy(),
                "branch": w.branch,
                "kept": true,
            })),
            "result": summary.to_value(),
            "images": images.iter().map(TaskImage::to_value).collect::<Vec<_>>(),
            "error": error,
        });
        let mut text = match phase {
            TaskPhase::Completed => {
                let mut text = summary.final_message.clone();
                if !summary.files_changed.is_empty() {
                    text.push_str(&format!(
                        "\n\nFiles changed ({}): {}",
                        summary.files_changed.len(),
                        summary.files_changed.join(", ")
                    ));
                }
                if let Some(stat) = &summary.diff_stat {
                    text.push_str(&format!("\n\ndiff --stat:\n{stat}"));
                }
                text
            }
            TaskPhase::Failed => format!(
                "task failed: {}",
                error.unwrap_or_else(|| "unknown error".into())
            ),
            TaskPhase::Cancelled => {
                "task was cancelled; changes and worktree are preserved".to_string()
            }
            _ => "task is not finished yet; poll get_progress".to_string(),
        };
        if !images.is_empty() {
            text.push_str(&format!(
                "\n\nImages produced ({}), read with inspect (kind=image):",
                images.len()
            ));
            for image in &images {
                text.push_str(&format!("\n- {}", image.path));
            }
        }
        Ok((vec![json!({ "type": "text", "text": text })], Some(payload)))
    }

    async fn tool_cancel(
        &self,
        args: &Value,
    ) -> std::result::Result<(String, Option<Value>), String> {
        let task = self.require_task(args).await?;
        if task.phase().terminal() {
            return Err(format!("task {} is already finished", task.id));
        }
        // Cooperative stop (interrupt flag) + hard abort backstop. Code
        // changes and the worktree are intentionally kept for later review.
        task.interrupt.store(true, Ordering::SeqCst);
        task.abort_runner();
        // Answer any pending question/approval so nothing blocks on them.
        if let Some(question) = task.take_pending_question() {
            let _ = task.send_question_answer(QuestionResponse {
                question_id: question.question_id,
                selected_option_ids: Vec::new(),
                free_text: None,
                cancelled: true,
            });
        }
        task.set_phase(TaskPhase::Cancelled);
        task.set_error("cancelled by client".into());
        task.push_event("cancelled".into());
        self.persist_task(&task).await?;
        Ok((
            format!(
                "task {} cancelled; run_dir {} preserved",
                task.id,
                task.run_dir.lock().await.display()
            ),
            Some(json!({
                "task_id": task.id,
            "session_id": task.session_id,
                "status": "cancelled",
                "run_dir": task.run_dir.lock().await.to_string_lossy(),
                "worktree_kept": task.isolated,
            })),
        ))
    }

    // -- helpers ------------------------------------------------------------

    async fn require_task(&self, args: &Value) -> std::result::Result<Arc<McpTask>, String> {
        let task_id = args
            .get("task_id")
            .or_else(|| args.get("agent_id"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "task_id is required".to_string())?;
        self.tasks
            .lock()
            .await
            .get(task_id)
            .cloned()
            .ok_or_else(|| {
                format!("unknown task_id: {task_id}; use session_id to resume a historical session")
            })
    }

    /// Resolve the `workspace` / `working_dir` / `path` argument, defaulting
    /// to the sole trusted root (or current dir when unconfigured).
    async fn resolve_workspace_arg(&self, args: &Value) -> Result<PathBuf, String> {
        let raw = args
            .get("workspace")
            .or_else(|| args.get("working_dir"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        match raw {
            Some(raw) => {
                let candidate = PathBuf::from(raw);
                let path = if candidate.is_absolute() {
                    candidate
                } else {
                    std::env::current_dir()
                        .map_err(|e| format!("failed to resolve working directory: {e}"))?
                        .join(candidate)
                };
                if !path.exists() {
                    return Err(format!("workspace does not exist: {}", path.display()));
                }
                std::fs::canonicalize(&path)
                    .map_err(|e| format!("cannot resolve {}: {e}", path.display()))
            }
            None => trusted_roots(&self.config)
                .into_iter()
                .next()
                .ok_or_else(|| "no workspace available; pass `workspace` explicitly".to_string()),
        }
    }
}

// ---------------------------------------------------------------------------
// Task runner
// ---------------------------------------------------------------------------

/// Execute one MCP task: acquire the queue slot, run agent turns, apply
/// instructions (steer / continuation / answers), and publish summaries.
/// Exits when the task is cancelled or a terminal state is reached without
/// further queued work.
///
/// Every exit path (early return, normal break, panic unwind) releases the
/// [`RunnerGuard`], which publishes `RunnerSlot::exited` synchronously so the
/// next `continue_task` can spawn a replacement runner. The pre-exit
/// instruction check runs while the guard is still held: if instructions
/// arrived in the closing window the loop continues, so an accepted
/// instruction is always consumed by *some* runner.
async fn run_task(ctx: RunnerCtx, task: Arc<McpTask>) {
    let _guard = RunnerGuard {
        task: Arc::clone(&task),
    };
    // Each active turn acquires a slot; idle sessions retain their context.
    if task.interrupt.load(Ordering::SeqCst) {
        return;
    }
    task.mark_runner_active();
    let run_dir = task.run_dir.lock().await.clone();
    // Fresh default session: `general` profile with no override resolves to
    // the globally configured default model.
    let model = ctx.config.resolve_subagent_model(TASK_PROFILE, None, None);
    let saved = match ctx.transcript.get_session(&task.session_id) {
        Ok(r) => r,
        Err(e) => {
            task.set_error(e.to_string());
            task.set_phase(TaskPhase::Failed);
            return;
        }
    };

    let mut session = if let Some(record) = saved {
        let mut session = Session::resume(
            task.session_id.clone(),
            run_dir.clone(),
            ctx.config
                .effective_permission_mode()
                .parse()
                .unwrap_or_default(),
            if record.model.is_empty() {
                model.clone()
            } else {
                record.model
            },
        );
        session.set_fallback_model(
            kkagent_core::session::runtime::SessionFallbackModel::from_persisted(
                record.fallback_model.as_deref(),
            ),
        );
        match ctx.transcript.load_messages(&task.session_id) {
            Ok(records) => {
                session.messages = super::messages_from_records(&records);
                session.persisted_message_count = session.messages.len();
            }
            Err(e) => {
                task.set_error(e.to_string());
                task.set_phase(TaskPhase::Failed);
                return;
            }
        }
        session
    } else {
        if task.resume {
            task.set_error("session transcript missing".into());
            task.set_phase(TaskPhase::Failed);
            return;
        }
        if let Err(e) =
            ctx.transcript
                .create_session(&task.session_id, &model, &run_dir.to_string_lossy())
        {
            task.set_error(e.to_string());
            task.set_phase(TaskPhase::Failed);
            return;
        }
        Session::new(
            task.session_id.clone(),
            run_dir.clone(),
            PermissionMode::Auto,
            model,
        )
    };
    let restoring = !session.messages.is_empty();
    session.inherit_interrupted(task.interrupt.clone());
    session.steer_mailbox = task.mailbox.clone();
    session.attach_workspace_concurrency_guard();
    session.inject_workspace_instructions().await;
    session.system_prompt.push_str(&supervisor_system_addon());
    // Drain pre-dispatch instructions into the initial message, then wire the
    // answer channels to this session so continue_task can respond to
    // AskUserQuestion / approval requests of the live turn.
    let initial_instructions = std::mem::take(
        &mut *task
            .pending_instructions
            .lock()
            .unwrap_or_else(|e| e.into_inner()),
    );
    session.add_user_message(build_initial_prompt_sync(
        if restoring {
            "Continue the existing session following the new instructions below."
        } else {
            &task.prompt
        },
        if restoring {
            None
        } else {
            task.plan_title.as_deref()
        },
        if restoring {
            None
        } else {
            task.plan.as_deref()
        },
        &initial_instructions,
    ));
    *task.question_tx.lock().unwrap_or_else(|e| e.into_inner()) = Some(session.question_tx.clone());
    *task.approval_tx.lock().unwrap_or_else(|e| e.into_inner()) = Some(session.approval_tx.clone());

    let permission_rules = ctx
        .config
        .permission
        .as_ref()
        .map(|p| p.rules.clone())
        .unwrap_or_default();
    // Auto keeps background delegation unattended: everything is
    // auto-approved except AskUserQuestion; config deny-rules still apply.
    // Actions requiring interactive approval emit ApprovalRequested, which
    // the collector surfaces as `waiting_permission` for continue_task.
    let permission = PermissionChain::new(
        session
            .permission_mode
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone(),
        permission_rules,
    );

    let (event_tx, mut event_rx) = mpsc::channel::<AgentEvent>(512);
    let collector_task = Arc::clone(&task);
    let collector = tokio::spawn(async move {
        while let Some(event) = event_rx.recv().await {
            handle_task_event(&collector_task, event);
        }
    });

    let mut tools = kkagent_tools::ToolRegistry::new();
    kkagent_tools::register_core_tools(&mut tools);
    if let Some(web_tool) = kkagent_tools::builtin::WebTool::try_new(Arc::clone(&ctx.web)) {
        tools.register(Arc::new(web_tool));
    }
    // Nested delegation inside a task: fresh manager so MCP-task agents can
    // spawn explore/coder helpers while staying under the depth budget.
    let nested_manager = Arc::new(kkagent_protocol::subagent::SubagentManager::new(
        ctx.config.subagent.effective_max_concurrent(),
    ));
    let nested_launch: kkagent_tools::builtin::task::SubagentLaunchFn = {
        let manager = Arc::clone(&nested_manager);
        let config = Arc::clone(&ctx.config);
        let web = Arc::clone(&ctx.web);
        let max_depth = ctx.config.subagent.effective_max_depth();
        Arc::new(
            move |mut sub_config: SubagentConfig, interrupt: Arc<AtomicBool>| {
                let manager = Arc::clone(&manager);
                let config = Arc::clone(&config);
                let web = Arc::clone(&web);
                let agent_id = sub_config.agent_id.clone();
                let parent_depth = sub_config.depth;
                if let Err(reason) = stamp_child_depth(&mut sub_config, parent_depth, max_depth) {
                    tracing::warn!("Rejected nested subagent {agent_id}: {reason}");
                    let mgr = Arc::clone(&manager);
                    tokio::spawn(async move { mgr.fail(&agent_id, reason).await });
                    return;
                }
                let run_agent_id = agent_id.clone();
                let run_manager = Arc::clone(&manager);
                let join = tokio::spawn(async move {
                    match kkagent_core::run_subagent_mirrored(
                        config,
                        web,
                        sub_config,
                        PermissionMode::Auto,
                        None,
                        Some(interrupt),
                    )
                    .await
                    {
                        Ok(result) => run_manager.complete(&run_agent_id, result).await,
                        Err(error) => run_manager.fail(&run_agent_id, error.to_string()).await,
                    }
                });
                let abort_manager = Arc::clone(&manager);
                tokio::spawn(async move {
                    abort_manager
                        .set_abort_handle(&agent_id, join.abort_handle())
                        .await;
                });
            },
        )
    };
    let allowed_subagents = kkagent_protocol::subagent::allowed_subagents_for(TASK_PROFILE);
    kkagent_tools::register_subagent_tools(
        &mut tools,
        nested_manager,
        nested_launch,
        allowed_subagents,
        ctx.config.tools.clone(),
        Vec::new(),
    );
    kkagent_tools::retain_profile_tools(&mut tools, TASK_PROFILE);

    // `general` profile turn budget.
    let max_rounds = 24;
    let abort_registry = Arc::new(Mutex::new(
        HashMap::<String, tokio::task::AbortHandle>::new(),
    ));
    let mut agent = AgentLoop::with_max_rounds(
        Arc::clone(&ctx.config),
        Arc::new(tools),
        Arc::new(Mutex::new(permission)),
        event_tx,
        abort_registry,
        max_rounds,
    );
    let result_store = Arc::new(kkagent_core::agent_loop::ToolResultStore::new(
        kkagent_config::default_config_dir(),
        Some(ctx.transcript.clone()),
    ));
    agent = agent
        .with_tool_result_store(result_store)
        .with_transcript_db(ctx.transcript.clone());

    loop {
        task.set_phase(TaskPhase::Queued);
        let Ok(permit) = ctx.queue.acquire().await else {
            break;
        };
        if task.interrupt.load(Ordering::SeqCst) {
            break;
        }
        task.set_phase(TaskPhase::Running);
        *task.review.lock().unwrap_or_else(|e| e.into_inner()) = "pending".into();
        task.progress
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .turns += 1;
        let run_result = agent.run_turn(&mut session).await;
        drop(permit);
        if let Err(e) =
            kkagent_core::transcript::persist_session_delta(&ctx.transcript, &mut session)
        {
            task.set_error(format!("cannot persist session: {e}"));
        }

        // Publish a review summary after every turn.
        let summary = build_task_summary(&task, &session, &run_dir).await;
        *task.summary.lock().unwrap_or_else(|e| e.into_inner()) = summary;
        let final_text = extract_final_assistant_text(&session);

        if task.interrupt.load(Ordering::SeqCst) {
            task.set_phase(TaskPhase::Cancelled);
            task.set_error("cancelled".into());
            break;
        }
        match &run_result {
            Ok(()) => {
                task.set_phase(TaskPhase::Completed);
                *task.review.lock().unwrap_or_else(|e| e.into_inner()) = "awaiting_review".into();
                let summary =
                    build_summary_with_message(&task, &session, &run_dir, final_text).await;
                *task.summary.lock().unwrap_or_else(|e| e.into_inner()) = summary;
            }
            Err(error) => {
                task.set_phase(TaskPhase::Failed);
                task.set_error(error.to_string());
                let mut summary = task.summary_snapshot();
                summary.warnings.push(format!("turn failed: {error}"));
                *task.summary.lock().unwrap_or_else(|e| e.into_inner()) = summary;
            }
        }

        if let Err(e) = collaboration::persist_task(&ctx.store, &task).await {
            task.set_error(format!("cannot persist task: {e}"));
        }
        // Steers that arrived during the closing window become continuations.
        for steer in task.mailbox.close_and_drain() {
            task.pending_instructions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(steer.text);
        }

        // Wait for a follow-up instruction (continuation) or park forever on
        // terminal tasks. Cancel aborts this task handle outright.
        let instruction = wait_for_instruction(&task).await;
        match instruction {
            Some(text) => {
                session.add_user_message(text);
                task.set_phase(TaskPhase::Running);
            }
            None => break,
        }
    }

    drop(agent);
    let _ = collector.await;
    // Final pre-exit check while the RunnerGuard is still held: instructions
    // accepted in the closing window (the terminal publish above and this
    // point) must be consumed. If any arrived, restart the turn loop as the
    // same runner instead of exiting — from a `continue_task` caller's point
    // of view nothing was lost. Cancelled/interrupted tasks do not restart.
    if task.runner_should_restart() {
        return Box::pin(run_task(ctx, task)).await;
    }
}

/// Park until a new instruction arrives on a terminal task. Returns `None`
/// only when the task was cancelled while waiting.
async fn wait_for_instruction(task: &Arc<McpTask>) -> Option<String> {
    loop {
        let notified = task.runner_notify.notified();
        // Check the queue after arming the notifier to avoid missed wakes.
        let next = {
            let mut pending = task
                .pending_instructions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if pending.is_empty() {
                None
            } else {
                Some(pending.remove(0))
            }
        };
        if let Some(text) = next {
            task.mark_runner_active();
            return Some(text);
        }
        if task.interrupt.load(Ordering::SeqCst) {
            return None;
        }
        // Publish the parked state for get_progress; cleared on wake above.
        task.mark_runner_waiting();
        notified.await;
    }
}

fn handle_task_event(task: &Arc<McpTask>, event: AgentEvent) {
    {
        let mut progress = task.progress.lock().unwrap_or_else(|e| e.into_inner());
        match &event {
            AgentEvent::ToolCall { tool_name, .. } => {
                progress.tool_calls += 1;
                drop(progress);
                task.note_tool_activity();
                task.push_event(format!("tool: {tool_name}"));
                return;
            }
            AgentEvent::ToolResult {
                tool_name,
                is_error,
                ..
            } => {
                progress.tool_results += 1;
                drop(progress);
                task.note_tool_activity();
                if *is_error {
                    task.push_event(format!("tool error: {tool_name}"));
                }
                return;
            }
            AgentEvent::MessageDelta { text, .. } => {
                progress.output_chars += text.chars().count() as u64;
                drop(progress);
                task.note_model_activity();
                return;
            }
            AgentEvent::ThinkingDelta { text, .. } => {
                progress.thinking_chars += text.chars().count() as u64;
                drop(progress);
                task.note_model_activity();
                return;
            }
            _ => {}
        }
    }
    match &event {
        AgentEvent::QuestionAsked { question, .. } => {
            task.store_pending_question(question.clone());
            task.set_phase(TaskPhase::AwaitingInput);
            task.push_event(format!("question: {}", clip_chars(&question.text, 120)));
        }
        AgentEvent::ApprovalRequested { request, .. } => {
            task.store_pending_approval(request.clone());
            task.set_phase(TaskPhase::AwaitingPermission);
            task.push_event(format!(
                "approval requested: {} ({})",
                request.tool_name, request.action
            ));
        }
        AgentEvent::StatusUpdate { status, .. } => match status {
            SessionStatus::WaitingQuestion => task.set_phase(TaskPhase::AwaitingInput),
            SessionStatus::WaitingApproval => task.set_phase(TaskPhase::AwaitingPermission),
            SessionStatus::Cancelling => task.push_event("cancelling".into()),
            _ => {}
        },
        AgentEvent::Error { message, .. } => {
            task.push_event(format!("error: {}", clip_chars(message, 120)));
        }
        _ => {}
    }
}

/// Compose the initial user message for a task's first turn: the goal, the
/// orchestrator-authored plan (when delegated with a plan_id) ahead of it as
/// the scope source of truth, then any pre-dispatch instructions.
fn build_initial_prompt_sync(
    prompt: &str,
    plan_title: Option<&str>,
    plan: Option<&str>,
    instructions: &[String],
) -> String {
    let mut combined = String::new();
    if let Some(plan) = plan {
        combined.push_str("# Execution plan (authored by the orchestrator)\n\n");
        if let Some(title) = plan_title {
            combined.push_str(&format!("Plan: {title}\n\n"));
        }
        combined.push_str(plan.trim_end());
        combined.push_str(
            "\n\nThe plan above is the scope source of truth. Follow its \
                           steps and decisions; update your final report to state which \
                           plan steps are done and which are not. If you must deviate, \
                           finish the plan-compatible part first, then state the deviation \
                           explicitly in the report.\n\n",
        );
        combined.push_str("# Task\n\n");
    }
    combined.push_str(prompt);
    // Instructions queued before the first turn start become part of the
    // initial task description (delegate + immediate continue_task race).
    if !instructions.is_empty() {
        combined.push_str("\n\nAdditional instructions received at dispatch:\n");
        for instruction in instructions {
            combined.push_str(&format!("- {instruction}\n"));
        }
    }
    combined
}

/// Compose the review summary — mechanical only: the agent's final assistant
/// text plus checkpoint/git-derived data. Never a second LLM pass.
async fn build_summary_with_message(
    task: &Arc<McpTask>,
    session: &Session,
    run_dir: &Path,
    final_message: String,
) -> TaskSummary {
    let mut files: Vec<String> = Vec::new();
    collect_changed_files(&session.undo_stack.iter().collect::<Vec<_>>(), &mut files);
    for path in &session.current_turn_changes {
        push_unique(&mut files, path.path.to_string_lossy().to_string());
    }
    files.sort();
    let base = task
        .base_commit
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let diff_stat = match base {
        Some(base) => git_diff_stat(run_dir, &base).await,
        None => None,
    };
    let mut warnings = Vec::new();
    if task.isolated && diff_stat.is_none() {
        warnings.push("diff stat unavailable for this worktree".into());
    }
    TaskSummary {
        final_message,
        files_changed: files,
        diff_stat,
        warnings,
    }
}

/// Collect the paths of image files a task produced under its run directory
/// (screenshots, generated images, test output). Bounded depth to keep large
/// repos cheap; capped at [`MAX_IMAGES_PER_RESULT`]. Only paths are returned —
/// `get_result` never inlines image content; callers use `inspect`.
async fn collect_task_image_paths(run_dir: &Path) -> Vec<TaskImage> {
    let mut artifacts: Vec<(PathBuf, u64)> = Vec::new();
    collect_image_artifacts(run_dir, 0, &mut artifacts).await;
    artifacts.sort();
    artifacts
        .into_iter()
        .take(MAX_IMAGES_PER_RESULT)
        .filter_map(|(path, _)| {
            let media_type = image_media_type(&path)?.to_string();
            Some(TaskImage {
                path: path.to_string_lossy().to_string(),
                media_type,
            })
        })
        .collect()
}

/// Recursively collect image files under `dir` (bounded depth 3, skipping
/// heavyweight/irrelevant directories). Returns `(path, size)` pairs.
fn collect_image_artifacts<'a>(
    dir: &'a Path,
    depth: usize,
    out: &'a mut Vec<(PathBuf, u64)>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
    Box::pin(async move { collect_image_artifacts_inner(dir, depth, out).await })
}

async fn collect_image_artifacts_inner(dir: &Path, depth: usize, out: &mut Vec<(PathBuf, u64)>) {
    const SKIP_DIRS: [&str; 10] = [
        "target",
        "node_modules",
        ".git",
        "dist",
        "build",
        ".next",
        "__pycache__",
        "venv",
        ".venv",
        "vendor",
    ];
    if depth > 3 || out.len() >= MAX_IMAGES_PER_RESULT * 2 {
        return;
    }
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let Ok(file_type) = entry.file_type().await else {
            continue;
        };
        let path = entry.path();
        if file_type.is_dir() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !SKIP_DIRS.contains(&name.as_str()) {
                collect_image_artifacts(&path, depth + 1, out).await;
            }
        } else if image_media_type(&path).is_some() {
            if let Ok(meta) = entry.metadata().await {
                if meta.len() > 0 && meta.len() <= MAX_ARTIFACT_BYTES {
                    out.push((path, meta.len()));
                }
            }
        }
    }
}

fn image_media_type(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => return None,
    })
}

async fn build_task_summary(task: &Arc<McpTask>, session: &Session, run_dir: &Path) -> TaskSummary {
    let final_message = extract_final_assistant_text(session);
    build_summary_with_message(task, session, run_dir, final_message).await
}

fn collect_changed_files(checkpoints: &[&TurnCheckpoint], files: &mut Vec<String>) {
    for checkpoint in checkpoints {
        for change in &checkpoint.file_changes {
            push_unique(files, change.path.to_string_lossy().to_string());
        }
    }
}

fn push_unique(files: &mut Vec<String>, path: String) {
    if !files.contains(&path) {
        files.push(path);
    }
}

/// Final assistant text of the session (most recent non-empty).
fn extract_final_assistant_text(session: &Session) -> String {
    for message in session.messages.iter().rev() {
        if message.role != "assistant" {
            continue;
        }
        let mut text = String::new();
        for block in &message.content {
            if let kkagent_llm::ChatContent::Text { text: t } = block {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(t);
            }
        }
        if !text.trim().is_empty() {
            return text;
        }
    }
    String::new()
}

/// System-prompt add-on for every MCP-delegated task: autonomous execution
/// and a review-ready closing report.
fn supervisor_system_addon() -> String {
    let kind_line = "\nThis task was delegated over MCP by a remote orchestrator. Work \
        autonomously: analyze, implement, build, test, fix and verify. Do not ask \
        clarifying questions unless a decision is truly blocking; if you must ask, \
        use AskUserQuestion so the orchestrator can answer it.";
    let profile_line = "Complete the assigned task thoroughly, then finish with a concise \
        report.";
    format!(
        "\n\n# MCP delegated task\n{kind_line}\n{profile_line}\n\
         End with a review-ready report: what was done, which files changed and why, \
         build/test commands, exit codes and log paths, and any open issues. \
         Completion hands the work to the orchestrator for review. Follow the assigned \
         validation scope; do not commit, push or publish unless explicitly instructed."
    )
}

async fn git_head(dir: &Path) -> Option<String> {
    let out = tokio::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(dir)
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

async fn git_diff_stat(dir: &Path, base: &str) -> Option<String> {
    let out = tokio::process::Command::new("git")
        .args(["diff", "--stat", base])
        .current_dir(dir)
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Some(text)
}

/// Guess the inspect `kind` from a file's extension/name.
fn detect_inspect_kind(path: &Path) -> String {
    if image_media_type(path).is_some() {
        return "image".to_string();
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if name.ends_with(".log") || name.contains("log") && name.ends_with(".txt") {
        return "log".to_string();
    }
    if matches!(name.as_str(), "diff" | "patch") || name.ends_with(".patch") {
        return "diff".to_string();
    }
    "text".to_string()
}

/// Normalize a plain-text tool outcome into single-block content.
trait MapTextBlock {
    fn map_text_block(self) -> std::result::Result<(Vec<Value>, Option<Value>), String>;
}

impl MapTextBlock for std::result::Result<(String, Option<Value>), String> {
    fn map_text_block(self) -> std::result::Result<(Vec<Value>, Option<Value>), String> {
        self.map(|(text, structured)| (vec![json!({ "type": "text", "text": text })], structured))
    }
}

fn clip_chars(text: &str, limit: usize) -> String {
    let mut clipped: String = text.chars().take(limit).collect();
    if text.chars().count() > limit {
        clipped.push('…');
    }
    clipped
}

fn select_option_ids(question: &QuestionPayload, instruction: &str) -> Vec<String> {
    // If the instruction names one of the option labels/ids verbatim, select
    // it; the free text always accompanies the answer.
    let normalized = instruction.trim().to_ascii_lowercase();
    question
        .options
        .iter()
        .filter(|option| {
            normalized == option.label.to_ascii_lowercase()
                || normalized == option.id.to_ascii_lowercase()
        })
        .map(|option| option.id.clone())
        .collect()
}

/// Canonical trusted workspace roots: `trusted_workspaces` config, or the
/// current directory when unconfigured (matches the HTTP server behavior).
fn trusted_roots(config: &kkagent_config::AppConfig) -> Vec<PathBuf> {
    let configured = if config.trusted_workspaces.is_empty() {
        vec![std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))]
    } else {
        config
            .trusted_workspaces
            .iter()
            .map(PathBuf::from)
            .collect()
    };
    configured
        .into_iter()
        .filter_map(|root| std::fs::canonicalize(&root).ok())
        .collect()
}

fn is_trusted(config: &kkagent_config::AppConfig, path: &Path) -> bool {
    trusted_roots(config)
        .iter()
        .any(|root| path.starts_with(root))
}

/// Loose path equality: canonicalize when possible, fall back to string.
fn same_dir(a: &Path, b: &Path) -> bool {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canon(a) == canon(b)
}

/// Git state for `get_context`: branch, HEAD subject, dirty file count.
async fn git_snapshot(dir: &Path) -> Option<Value> {
    if !dir.join(".git").exists() {
        return None;
    }
    let run = async |args: &[&str]| -> Option<String> {
        let out = tokio::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .await
            .ok()?;
        if !out.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let branch = run(&["rev-parse", "--abbrev-ref", "HEAD"])
        .await
        .unwrap_or_default();
    let head_subject = run(&["log", "-1", "--pretty=%s"]).await.unwrap_or_default();
    let dirty_files = run(&["status", "--porcelain"])
        .await
        .map(|status| status.lines().count() as u64)
        .unwrap_or(0);
    Some(json!({
        "branch": branch,
        "head_subject": head_subject,
        "dirty_files": dirty_files,
    }))
}

fn error_response(id: Value, code: i64, message: &str) -> String {
    error_response_value(id, code, message).to_string()
}

fn error_response_value(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
}

fn tool_definitions() -> Vec<Value> {
    let mut definitions = vec![
        json!({
            "name": "list_workspaces",
            "description": "List workspaces available to kkagent. Workspaces auto-register: any directory that ever hosted a kkagent session appears here, plus configured trusted roots. Each entry reports session count, last activity and in-flight task counts. Returns the most recently active workspaces first (default 5; use limit for more).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": { "type": "integer", "description": "Max workspaces to return (default 5, max 100)" }
                },
                "additionalProperties": false,
            },
        }),
        json!({
            "name": "get_context",
            "description": "Supervisor-level context for one workspace: git state (branch, head, dirty files), recent kkagent sessions, current background tasks with statuses, and items needing attention (pending questions, permission requests, failures). Main entry point when entering or resuming a project.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "workspace": { "type": "string", "description": "Workspace directory (absolute or relative); defaults to the first trusted workspace" }
                },
                "additionalProperties": false,
            },
        }),
        json!({
            "name": "inspect",
            "description": "Direct read-only fetch of a specified resource in a workspace — source code, text, logs, git diffs, images, and task artifacts. No agent runs; the content is returned immediately. Text content supports line windowing (offset/limit); images are returned inline as image content blocks. For whole-worktree diffs use kind=diff (path optional). This is also how you read image files listed in get_result.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "workspace": { "type": "string", "description": "Workspace directory the path is relative to (defaults to the first trusted workspace)" },
                    "path": { "type": "string", "description": "File to read, relative to the workspace (or absolute). Optional only when kind=diff." },
                    "kind": { "type": "string", "enum": ["auto", "text", "log", "image", "diff"], "description": "Content type; auto-detects from the file when omitted. diff reads the working-tree git diff." },
                    "offset": { "type": "integer", "description": "First line to return for text/log content (0-based, default 0)" },
                    "limit": { "type": "integer", "description": "Max lines to return for text/log content (default 400, max 5000)" }
                },
                "additionalProperties": false,
            },
            "outputSchema": {
                "type": "object",
                "properties": {
                    "kind": { "type": "string" },
                    "path": { "type": "string" },
                    "truncated": { "type": "boolean" },
                    "text": { "type": "string", "description": "File / diff body (also in content[].text; required for hosts that prefer structuredContent)" }
                },
            },
        }),
        json!({
            "name": "write_plan",
            "description": "Store an orchestrator-authored execution plan (markdown: goal, steps, acceptance criteria) and get a plan_id. This is how a remote orchestrator decides and plans while kkagent executes. The plan takes effect when passed to delegate via plan_id; it is injected ahead of the task prompt as the scope source of truth. Revise by re-storing with the same plan_id BEFORE delegating — later revisions never modify already-running tasks.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "plan": { "type": "string", "description": "Full execution plan in markdown: goal, ordered steps, risks, acceptance criteria (max 100k chars)" },
                    "title": { "type": "string", "description": "Short plan name (defaults to the first line of plan)" },
                    "workspace": { "type": "string", "description": "Optional informational workspace the plan targets" },
                    "plan_id": { "type": "string", "description": "Existing plan id to REVISE an earlier plan; omit to create a new plan" }
                },
                "required": ["plan"],
                "additionalProperties": false,
            },
            "outputSchema": {
                "type": "object",
                "properties": {
                    "plan_id": { "type": "string", "description": "Pass this to delegate(plan_id)" },
                    "title": { "type": "string" },
                    "chars": { "type": "integer" }
                },
                "required": ["plan_id"],
            },
        }),
        json!({
            "name": "delegate",
            "description": "Delegate a new task to a kkagent coding agent — equivalent to opening a fresh default kkagent session in the workspace — and return immediately with a task_id. kkagent decides everything itself (model, profile, tools, and whether to use an isolated git worktree); the agent autonomously handles code retrieval, planning, file modification, build, test, fix and verification. Pass plan_id (from write_plan) to execute an orchestrator-authored plan as the scope source of truth. Poll get_progress, deliver decisions with continue_task, collect with get_result.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "prompt": { "type": "string", "description": "Full task description: goal, requirements, constraints" },
                    "plan_id": { "type": "string", "description": "Plan id from write_plan; its content is injected ahead of the prompt as the scope source of truth" },
                    "workspace": { "type": "string", "description": "Target workspace directory (must be trusted; defaults to the first trusted workspace)" }
                },
                "required": ["prompt"],
                "additionalProperties": false,
            },
            "outputSchema": {
                "type": "object",
                "properties": {
                    "task_id": { "type": "string" },
                    "status": { "type": "string" },
                    "run_dir": { "type": "string", "description": "Directory the agent works in (worktree path when isolated)" }
                },
                "required": ["task_id"],
            },
        }),
        json!({
            "name": "get_progress",
            "description": "Poll a background task: status (queued | running | waiting_input | waiting_permission | completed | failed | cancelled), runner_state (running | waiting_for_instruction | exited), elapsed time, activity counters, last model/tool activity ages, recent events, pending_instruction_count, the pending question or approval if any, and errors.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task_id": { "type": "string", "description": "Id returned by delegate" }
                },
                "required": ["task_id"],
                "additionalProperties": false,
            },
        }),
        json!({
            "name": "continue_task",
            "description": "Send a new instruction or a structured decision into an EXISTING task (no new agent session), waking / continuing its agent loop. When get_progress reports waiting_input, answer the pending question with decision {question_id?, selected_option_ids, free_text, cancelled} — option ids come straight from pending_question.options (supports multi-select); when it reports waiting_permission, resolve it with decision {approve: true|false, feedback?}. A plain instruction instead answers verbatim (option label/id match), steers a running turn, or continues a completed/failed task in its original session. Returns immediately; poll get_progress for the effect.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task_id": { "type": "string", "description": "Target task id" },
                    "instruction": { "type": "string", "description": "Answer, decision, additional constraint, or follow-up instruction (required unless decision is given)" },
                    "decision": {
                        "type": "object",
                        "description": "Structured decision for a pending question (waiting_input) or permission request (waiting_permission); takes precedence over instruction text matching",
                        "properties": {
                            "question_id": { "type": "string", "description": "Validated against the pending question when provided" },
                            "selected_option_ids": { "type": "array", "items": { "type": "string" }, "description": "Option ids from pending_question.options (multi-select supported)" },
                            "free_text": { "type": "string", "description": "Free-text answer accompanying the question" },
                            "cancelled": { "type": "boolean", "description": "Dismiss the pending question" },
                            "approve": { "type": "boolean", "description": "true approves, false rejects the pending permission request" },
                            "feedback": { "type": "string", "description": "Guidance carried with a rejection (or approval)" }
                        },
                        "additionalProperties": false
                    }
                },
                "required": ["task_id"],
                "additionalProperties": false,
            },
        }),
        json!({
            "name": "get_result",
            "description": "Mechanical review result of a task: the agent's own final report plus mechanically collected files changed, diff --stat, warnings, worktree state, and the PATHS of images the task produced (never the image content — read them with the inspect tool, kind=image). No extra LLM pass is used to compose this summary. Does NOT include the raw transcript.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task_id": { "type": "string", "description": "Id returned by delegate" }
                },
                "required": ["task_id"],
                "additionalProperties": false,
            },
            "outputSchema": {
                "type": "object",
                "properties": {
                    "task_id": { "type": "string" },
                    "status": { "type": "string" },
                    "images": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": { "type": "string", "description": "Absolute path; read via the inspect tool" },
                                "media_type": { "type": "string" }
                            },
                            "required": ["path", "media_type"],
                        }
                    }
                },
                "required": ["task_id"],
            },
        }),
        json!({
            "name": "cancel",
            "description": "Cancel a running background task asynchronously: stops the agent loop, nested agents, and running build/test processes. Code changes and the worktree are preserved by default for later review or continuation.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task_id": { "type": "string", "description": "Id of the task (returned by delegate) to cancel" }
                },
                "required": ["task_id"],
                "additionalProperties": false,
            },
        }),
    ];
    collaboration::extend_definitions(&mut definitions);
    definitions
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes the tunnel-integration tests: they spawn real child
    /// processes and (in one case) mutate process env, which is not safe to
    /// race with sibling tests in the same process.
    #[cfg(unix)]
    static TUNNEL_PROC_TESTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    async fn server() -> Arc<McpServer> {
        Arc::new(
            McpServer::new(
                Arc::new(kkagent_config::AppConfig::default()),
                TranscriptDb::open_in_memory().expect("in-memory transcript db"),
            )
            .unwrap(),
        )
    }

    /// Server with the given directories marked as trusted workspaces.
    async fn trusted_server(paths: &[&Path]) -> Arc<McpServer> {
        let config = kkagent_config::AppConfig {
            trusted_workspaces: paths
                .iter()
                .map(|p| {
                    // Canonicalize so macOS /var ↔ /private/var symlinks match
                    // the canonicalized transcript working dirs.
                    std::fs::canonicalize(p)
                        .unwrap_or_else(|_| p.to_path_buf())
                        .to_string_lossy()
                        .to_string()
                })
                .collect(),
            ..kkagent_config::AppConfig::default()
        };
        Arc::new(
            McpServer::new(
                Arc::new(config),
                TranscriptDb::open_in_memory().expect("in-memory transcript db"),
            )
            .unwrap(),
        )
    }

    #[tokio::test]
    async fn initialize_echoes_supported_version() {
        let server = server().await;
        let response = server
            .handle_message(
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#,
            )
            .await
            .expect("initialize expects a response");
        let parsed: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(parsed["id"], 1);
        assert_eq!(parsed["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(parsed["result"]["serverInfo"]["name"], "kkagent");
        assert!(parsed["result"]["capabilities"]["tools"].is_object());
    }

    #[tokio::test]
    async fn initialize_falls_back_to_latest_version() {
        let server = server().await;
        let response = server
            .handle_message(
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"1999-01-01"}}"#,
            )
            .await
            .unwrap();
        let parsed: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(parsed["result"]["protocolVersion"], LATEST_PROTOCOL_VERSION);
    }

    #[tokio::test]
    async fn tools_list_exposes_the_delegation_tools() {
        let server = server().await;
        let response = server
            .handle_message(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#)
            .await
            .unwrap();
        let parsed: Value = serde_json::from_str(&response).unwrap();
        let tools = parsed["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            vec![
                "list_workspaces",
                "get_context",
                "inspect",
                "write_plan",
                "delegate",
                "get_progress",
                "continue_task",
                "get_result",
                "cancel",
                "Glob",
                "Grep",
                "get_plan",
                "get_session_context",
            ]
        );
        let delegate = tools.iter().find(|t| t["name"] == "delegate").unwrap();
        // delegate takes only prompt + plan_id + workspace — kkagent decides
        // the rest.
        let delegate_props = &delegate["inputSchema"]["properties"];
        assert_eq!(
            delegate_props
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec![
                "plan_id",
                "plan_version",
                "prompt",
                "request_id",
                "workspace"
            ]
        );
        assert!(delegate["outputSchema"]["properties"]["task_id"].is_object());
        let write_plan = tools.iter().find(|t| t["name"] == "write_plan").unwrap();
        assert!(write_plan["inputSchema"]["properties"]["plan"].is_object());
        assert!(write_plan["outputSchema"]["properties"]["plan_id"].is_object());
    }

    async fn call_write_plan(server: &Arc<McpServer>, args: Value) -> Value {
        let request = json!({
            "jsonrpc": "2.0",
            "id": 99,
            "method": "tools/call",
            "params": { "name": "write_plan", "arguments": args },
        });
        let response = server
            .handle_message(&request.to_string())
            .await
            .expect("write_plan expects a response");
        serde_json::from_str(&response).expect("jsonrpc response")
    }

    #[tokio::test]
    async fn continue_task_with_session_id_wraps_previous_session() {
        let server = server().await;
        // Seed a previous session with a couple of messages.
        let full_id = "cccccccc-1111-2222-3333-444444444444";
        server
            .transcript
            .create_session(full_id, "test-model", "/tmp")
            .unwrap();
        server
            .transcript
            .append_message(full_id, "user", r#"[{"type":"text","text":"hello"}]"#, None)
            .unwrap();

        // Mutual exclusion: session_id + task_id is rejected.
        let request = json!({
            "jsonrpc": "2.0", "id": 70, "method": "tools/call",
            "params": { "name": "continue_task", "arguments": {
                "session_id": full_id, "task_id": "t1", "instruction": "go"
            } },
        });
        let parsed: Value =
            serde_json::from_str(&server.handle_message(&request.to_string()).await.unwrap())
                .unwrap();
        assert_eq!(parsed["result"]["isError"], true);
        assert!(parsed["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("either session_id"));

        // decision with session_id is rejected.
        let request = json!({
            "jsonrpc": "2.0", "id": 71, "method": "tools/call",
            "params": { "name": "continue_task", "arguments": {
                "session_id": full_id, "decision": { "approve": true }
            } },
        });
        let parsed: Value =
            serde_json::from_str(&server.handle_message(&request.to_string()).await.unwrap())
                .unwrap();
        assert_eq!(parsed["result"]["isError"], true);
        assert!(parsed["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("only valid with task_id"));

        // Unknown session id is a tool error, not a panic.
        let request = json!({
            "jsonrpc": "2.0", "id": 71, "method": "tools/call",
            "params": { "name": "continue_task", "arguments": {
                "session_id": "deadbeef-0000", "instruction": "go"
            } },
        });
        let parsed: Value =
            serde_json::from_str(&server.handle_message(&request.to_string()).await.unwrap())
                .unwrap();
        assert_eq!(parsed["result"]["isError"], true);
        assert!(parsed["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("unknown session_id"));

        // Happy-path prefix resolution: the unique PREFIX resolves and
        // reaches the trusted-workspace gate (/tmp is not trusted in the
        // test server), which is the next check before any runner spawns.
        let request = json!({
            "jsonrpc": "2.0", "id": 72, "method": "tools/call",
            "params": { "name": "continue_task", "arguments": {
                "session_id": "cccccccc-1111", "instruction": "carry on"
            } },
        });
        let parsed: Value =
            serde_json::from_str(&server.handle_message(&request.to_string()).await.unwrap())
                .unwrap();
        assert_eq!(parsed["result"]["isError"], true);
        assert!(
            parsed["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("not a trusted workspace"),
            "unexpected: {}",
            parsed["result"]["content"][0]["text"]
        );
    }

    #[tokio::test]
    async fn write_plan_stores_and_revises_by_id() {
        let server = server().await;
        // Create.
        let parsed = call_write_plan(
            &server,
            json!({
                "title": "Add MCP auth",
                "plan": "# Goal\nAdd bearer auth.\n\n## Steps\n1. Add token check\n2. Test it",
            }),
        )
        .await;
        let plan_id = parsed["result"]["structuredContent"]["plan_id"]
            .as_str()
            .expect("plan_id")
            .to_string();
        assert!(!plan_id.is_empty());
        assert_eq!(
            parsed["result"]["structuredContent"]["title"],
            "Add MCP auth"
        );

        // Revise with the same id (accepted, content replaced).
        let parsed = call_write_plan(
            &server,
            json!({
                "plan_id": plan_id,
                "title": "Add MCP auth v2",
                "plan": "# Goal\nAdd bearer auth with constant-time compare.",
            }),
        )
        .await;
        assert_eq!(
            parsed["result"]["structuredContent"]["plan_id"].as_str(),
            Some(plan_id.as_str())
        );

        // Unknown revision id is rejected without creating anything.
        let parsed =
            call_write_plan(&server, json!({ "plan_id": "does-not-exist", "plan": "x" })).await;
        assert_eq!(parsed["result"]["isError"], true);
        assert!(parsed["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("unknown plan_id"));

        // Missing plan content is a tool error, not a protocol error.
        let parsed = call_write_plan(&server, json!({ "title": "no content" })).await;
        assert_eq!(parsed["result"]["isError"], true);
        assert!(parsed["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("plan is required"));

        // Oversized plan is rejected with the cap named.
        let parsed =
            call_write_plan(&server, json!({ "plan": "x".repeat(MAX_PLAN_CHARS + 1) })).await;
        assert_eq!(parsed["result"]["isError"], true);
        assert!(parsed["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("write_plan accepts up to"));
    }

    #[test]
    fn initial_prompt_puts_plan_ahead_of_task() {
        let prompt = build_initial_prompt_sync(
            "implement it",
            Some("Add MCP auth"),
            Some("1. Add token check\n2. Test it"),
            &["extra note".to_string()],
        );
        let plan_pos = prompt.find("# Execution plan").expect("plan header");
        let task_pos = prompt.find("# Task").expect("task header");
        let goal_pos = prompt.find("implement it").expect("goal text");
        assert!(plan_pos == 0, "plan must come first");
        assert!(plan_pos < task_pos && task_pos < goal_pos);
        assert!(prompt.contains("Add token check"));
        assert!(prompt.contains("scope source of truth"));
        assert!(prompt.contains("- extra note"));
        // No plan → unchanged single-section layout.
        let plain = build_initial_prompt_sync("just do it", None, None, &[]);
        assert_eq!(plain, "just do it");
    }

    #[tokio::test]
    async fn delegate_validates_prompt_and_trust() {
        let server = server().await;
        let response = server
            .handle_message(
                r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"delegate","arguments":{"prompt":"   "}}}"#,
            )
            .await
            .unwrap();
        let parsed: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(parsed["result"]["isError"], true);
        assert!(parsed["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("prompt is required"));
    }

    #[tokio::test]
    async fn inspect_reads_source_file_with_line_windowing() {
        let dir = tempfile::tempdir().unwrap();
        let server = trusted_server(&[dir.path()]).await;
        let content = (1..=100)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(dir.path().join("main.rs"), &content).unwrap();
        let request = json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": { "name": "inspect", "arguments": { "workspace": dir.path().to_string_lossy(), "path": "main.rs", "offset": 10, "limit": 5 } },
        });
        let response = server.handle_message(&request.to_string()).await.unwrap();
        let parsed: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(parsed["result"]["isError"], false);
        let text = parsed["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("line 11"), "{text}");
        assert!(!text.contains("line 10 "), "{text}");
        assert!(text.contains("lines 11..15 of 100"), "{text}");
        let structured = &parsed["result"]["structuredContent"];
        assert_eq!(structured["kind"], "text");
        assert_eq!(structured["total_lines"], 100);
        assert_eq!(structured["truncated"], true);
        // Hosts that prefer structuredContent (ChatGPT/Codex) must still see
        // the file body — not just path/offset metadata.
        assert_eq!(structured["text"].as_str().unwrap(), text);
    }

    #[tokio::test]
    async fn inspect_returns_image_blocks_for_image_files() {
        let dir = tempfile::tempdir().unwrap();
        let server = trusted_server(&[dir.path()]).await;
        let png: &[u8] = &[
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00,
            0x00, 0x90, 0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08,
            0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, 0x18, 0xDD, 0x8D,
            0xB0, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];
        std::fs::write(dir.path().join("shot.png"), png).unwrap();
        let request = json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "tools/call",
            "params": { "name": "inspect", "arguments": { "workspace": dir.path().to_string_lossy(), "path": "shot.png" } },
        });
        let response = server.handle_message(&request.to_string()).await.unwrap();
        let parsed: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(parsed["result"]["isError"], false);
        let content = parsed["result"]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["mimeType"], "image/png");
        assert_eq!(parsed["result"]["structuredContent"]["kind"], "image");
    }

    #[tokio::test]
    async fn inspect_confinement_rejects_escaping_paths() {
        let dir = tempfile::tempdir().unwrap();
        let server = trusted_server(&[dir.path()]).await;
        let secret = tempfile::tempdir().unwrap();
        let secret_path = secret.path().join("secret.txt");
        std::fs::write(&secret_path, b"secret").unwrap();
        let request = json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": { "name": "inspect", "arguments": { "workspace": dir.path().to_string_lossy(), "path": secret_path.to_string_lossy() } },
        });
        let response = server.handle_message(&request.to_string()).await.unwrap();
        let parsed: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(parsed["result"]["isError"], true);
        assert!(parsed["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("escapes workspace"));
    }

    #[tokio::test]
    async fn inspect_rejects_directories_and_missing_paths() {
        let dir = tempfile::tempdir().unwrap();
        let server = trusted_server(&[dir.path()]).await;
        // Directory.
        let request = json!({
            "jsonrpc": "2.0",
            "id": 8,
            "method": "tools/call",
            "params": { "name": "inspect", "arguments": { "workspace": dir.path().to_string_lossy(), "path": "." } },
        });
        let response = server.handle_message(&request.to_string()).await.unwrap();
        let parsed: Value = serde_json::from_str(&response).unwrap();
        assert!(parsed["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("is a directory"));
        // Missing file.
        let request = json!({
            "jsonrpc": "2.0",
            "id": 9,
            "method": "tools/call",
            "params": { "name": "inspect", "arguments": { "workspace": dir.path().to_string_lossy(), "path": "nope.txt" } },
        });
        let response = server.handle_message(&request.to_string()).await.unwrap();
        let parsed: Value = serde_json::from_str(&response).unwrap();
        assert!(parsed["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("path does not exist"));
    }

    #[tokio::test]
    async fn delegate_rejects_untrusted_workspace() {
        let server = server().await;
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().to_string_lossy().to_string();
        let request = json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": { "name": "delegate", "arguments": { "prompt": "do work", "workspace": workspace } },
        });
        let response = server.handle_message(&request.to_string()).await.unwrap();
        let parsed: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(parsed["result"]["isError"], true);
        assert!(parsed["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("not a trusted workspace"));
    }

    #[tokio::test]
    async fn task_tools_reject_unknown_task_ids() {
        let server = server().await;
        for tool in ["get_progress", "get_result", "cancel"] {
            let request = json!({
                "jsonrpc": "2.0",
                "id": 8,
                "method": "tools/call",
                "params": { "name": tool, "arguments": { "task_id": "nope-123" } },
            });
            let response = server.handle_message(&request.to_string()).await.unwrap();
            let parsed: Value = serde_json::from_str(&response).unwrap();
            assert_eq!(parsed["result"]["isError"], true, "{tool}");
            assert!(
                parsed["result"]["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .contains("unknown task_id"),
                "{tool}"
            );
        }
        // continue_task additionally requires instruction or decision.
        let request = json!({
            "jsonrpc": "2.0",
            "id": 81,
            "method": "tools/call",
            "params": { "name": "continue_task", "arguments": { "task_id": "nope-123", "instruction": "go on" } },
        });
        let response = server.handle_message(&request.to_string()).await.unwrap();
        let parsed: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(parsed["result"]["isError"], true);
        assert!(parsed["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("unknown task_id"));
    }

    #[tokio::test]
    async fn list_workspaces_pages_by_limit_and_reports_total() {
        // Trust a dedicated cwd-free set: seed 3 session workspaces and make
        // the server trust exactly them, so the registry has exactly 3.
        let dirs: Vec<tempfile::TempDir> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
        let server = trusted_server(&dirs.iter().map(|d| d.path()).collect::<Vec<_>>()).await;
        {
            let db = &server.transcript;
            for (i, dir) in dirs.iter().enumerate() {
                db.create_session(
                    &format!("seed-{i}"),
                    "local/model",
                    &dir.path().to_string_lossy(),
                )
                .unwrap();
            }
        }
        // limit=2: only two entries, total_count == 3.
        let request = json!({
            "jsonrpc": "2.0",
            "id": 34,
            "method": "tools/call",
            "params": { "name": "list_workspaces", "arguments": { "limit": 2 } },
        });
        let response = server.handle_message(&request.to_string()).await.unwrap();
        let parsed: Value = serde_json::from_str(&response).unwrap();
        let structured = &parsed["result"]["structuredContent"];
        let workspaces = structured["workspaces"].as_array().unwrap();
        assert_eq!(workspaces.len(), 2);
        assert_eq!(structured["limit"], 2);
        assert_eq!(structured["total_count"], 3);
        // Every returned entry is one of the seeded dirs (canonicalized to
        // match macOS /var ↔ /private/var symlink resolution).
        for workspace in workspaces {
            let path = workspace["path"].as_str().unwrap();
            assert!(
                dirs.iter().any(|d| {
                    let canonical =
                        std::fs::canonicalize(d.path()).unwrap_or_else(|_| d.path().to_path_buf());
                    path.starts_with(&*canonical.to_string_lossy())
                }),
                "unexpected workspace {path}"
            );
        }
    }

    #[tokio::test]
    async fn get_result_reports_image_paths_not_content() {
        // 1x1 red PNG, valid base64 of a real (tiny) image.
        let png: &[u8] = &[
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00,
            0x00, 0x90, 0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08,
            0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, 0x18, 0xDD, 0x8D,
            0xB0, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("screenshot.png"), png).unwrap();
        std::fs::write(dir.path().join("notes.txt"), b"not an image").unwrap();

        let server = trusted_server(&[dir.path()]).await;
        let request = json!({
            "jsonrpc": "2.0",
            "id": 30,
            "method": "tools/call",
            "params": { "name": "delegate", "arguments": { "prompt": "do work", "workspace": dir.path().to_string_lossy() } },
        });
        let response = server.handle_message(&request.to_string()).await.unwrap();
        let parsed: Value = serde_json::from_str(&response).unwrap();
        let task_id = parsed["result"]["structuredContent"]["task_id"]
            .as_str()
            .unwrap()
            .to_string();

        // The task is queued/running; cancel it to reach a terminal state so
        // get_result serves the collected summary without model traffic.
        let request = json!({
            "jsonrpc": "2.0",
            "id": 31,
            "method": "tools/call",
            "params": { "name": "cancel", "arguments": { "task_id": task_id } },
        });
        let response = server.handle_message(&request.to_string()).await.unwrap();
        let parsed: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(parsed["result"]["isError"], false);

        // get_result: exactly one text content block — image content is never
        // inlined. The image shows up as a path in both the structured payload
        // and the text (to be read via the inspect tool).
        let request = json!({
            "jsonrpc": "2.0",
            "id": 32,
            "method": "tools/call",
            "params": { "name": "get_result", "arguments": { "task_id": task_id } },
        });
        let response = server.handle_message(&request.to_string()).await.unwrap();
        let parsed: Value = serde_json::from_str(&response).unwrap();
        let content = parsed["result"]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");

        let structured = &parsed["result"]["structuredContent"];
        let images = structured["images"].as_array().unwrap();
        assert_eq!(images.len(), 1);
        assert!(
            images[0]["path"]
                .as_str()
                .unwrap()
                .ends_with("screenshot.png"),
            "unexpected image path: {images:?}"
        );
        assert_eq!(images[0]["media_type"], "image/png");
        // No content bytes are exposed — only the path.
        assert!(images[0].get("bytes").is_none());
        let text = content[0]["text"].as_str().unwrap();
        assert!(text.contains("screenshot.png"), "{text}");
        assert!(text.contains("inspect"), "{text}");
    }

    #[tokio::test]
    async fn image_helpers_skip_non_images_and_oversize() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"text").unwrap();
        std::fs::write(dir.path().join("b.webp"), b"RIFFwebp").unwrap();
        let mut out = Vec::new();
        collect_image_artifacts(dir.path(), 0, &mut out).await;
        assert_eq!(out.len(), 1);
        assert!(out[0].0.ends_with("b.webp"));
        assert_eq!(image_media_type(Path::new("x.PNG")), Some("image/png"));
        assert_eq!(image_media_type(Path::new("x.svg")), None);
    }

    #[tokio::test]
    async fn task_tools_require_task_id() {
        let server = server().await;
        let response = server
            .handle_message(
                r#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"get_progress","arguments":{}}}"#,
            )
            .await
            .unwrap();
        let parsed: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(parsed["result"]["isError"], true);
        assert!(parsed["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("task_id is required"));
    }

    #[tokio::test]
    async fn continue_task_rejects_unknown_tasks() {
        let server = server().await;
        // Craft a task record directly through delegate with a trusted
        // workspace (temporarily widen the trust to the tempdir is not
        // possible; instead verify the error path for an unknown task).
        let response = server
            .handle_message(
                r#"{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"continue_task","arguments":{"task_id":"ghost","instruction":"go"}}}"#,
            )
            .await
            .unwrap();
        let parsed: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(parsed["result"]["isError"], true);
        assert!(parsed["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("unknown task_id"));
    }

    /// Start a delegate task in a trusted temp workspace and return the task
    /// together with a server trusting that workspace. The tempdir stays
    /// alive via the returned guard; the task is `Queued` and the runner
    /// never reaches model traffic because the tests drive the task state
    /// directly.
    async fn delegated_task() -> (Arc<McpServer>, Arc<McpTask>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let config = kkagent_config::AppConfig {
            trusted_workspaces: vec![std::fs::canonicalize(dir.path())
                .unwrap_or_else(|_| dir.path().to_path_buf())
                .to_string_lossy()
                .to_string()],
            ..kkagent_config::AppConfig::default()
        };
        let server = Arc::new(
            McpServer::new(
                Arc::new(config),
                TranscriptDb::open_in_memory().expect("in-memory transcript db"),
            )
            .unwrap(),
        );
        let request = json!({
            "jsonrpc": "2.0",
            "id": 900,
            "method": "tools/call",
            "params": { "name": "delegate", "arguments": { "prompt": "do work", "workspace": dir.path().to_string_lossy() } },
        });
        let response = server.handle_message(&request.to_string()).await.unwrap();
        let parsed: Value = serde_json::from_str(&response).unwrap();
        let task_id = parsed["result"]["structuredContent"]["task_id"]
            .as_str()
            .unwrap()
            .to_string();
        let task = server
            .tasks
            .lock()
            .await
            .get(&task_id)
            .cloned()
            .expect("task registered");
        // Kill the runner immediately: it would fail fast under the default
        // (model-less) config and race the test's manual state setup.
        task.abort_runner();
        (server, task, dir)
    }

    #[tokio::test]
    async fn continue_task_decision_answers_question_with_option_ids() {
        let (server, task, _dir) = delegated_task().await;

        // Simulate the agent asking AskUserQuestion mid-turn.
        let (answer_tx, mut answer_rx) = mpsc::channel::<QuestionResponse>(4);
        *task.question_tx.lock().unwrap_or_else(|e| e.into_inner()) = Some(answer_tx);
        task.store_pending_question(kkagent_protocol::events::QuestionPayload {
            question_id: "q-1".into(),
            text: "Which plan?".into(),
            options: vec![
                kkagent_protocol::events::QuestionOption {
                    id: "opt-a".into(),
                    label: "Plan A".into(),
                },
                kkagent_protocol::events::QuestionOption {
                    id: "opt-b".into(),
                    label: "Plan B".into(),
                },
            ],
            allow_free_text: true,
            allow_multiple: false,
        });
        task.set_phase(TaskPhase::AwaitingInput);

        let request = json!({
            "jsonrpc": "2.0",
            "id": 901,
            "method": "tools/call",
            "params": { "name": "continue_task", "arguments": {
                "task_id": task.id,
                "decision": { "question_id": "q-1", "selected_option_ids": ["opt-b"], "free_text": "with tests please" }
            } },
        });
        let response = server.handle_message(&request.to_string()).await.unwrap();
        let parsed: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(parsed["result"]["isError"], false);
        assert_eq!(
            parsed["result"]["structuredContent"]["action"],
            "question_answered"
        );
        assert_eq!(task.phase(), TaskPhase::Running);

        let answer = answer_rx.recv().await.expect("answer delivered");
        assert_eq!(answer.question_id, "q-1");
        assert_eq!(answer.selected_option_ids, vec!["opt-b".to_string()]);
        assert_eq!(answer.free_text.as_deref(), Some("with tests please"));
        assert!(!answer.cancelled);
    }

    #[tokio::test]
    async fn continue_task_decision_rejects_unknown_option_ids() {
        let (server, task, _dir) = delegated_task().await;

        let (answer_tx, _answer_rx) = mpsc::channel::<QuestionResponse>(4);
        *task.question_tx.lock().unwrap_or_else(|e| e.into_inner()) = Some(answer_tx);
        task.store_pending_question(kkagent_protocol::events::QuestionPayload {
            question_id: "q-1".into(),
            text: "Which plan?".into(),
            options: vec![kkagent_protocol::events::QuestionOption {
                id: "opt-a".into(),
                label: "Plan A".into(),
            }],
            allow_free_text: true,
            allow_multiple: false,
        });
        task.set_phase(TaskPhase::AwaitingInput);

        let request = json!({
            "jsonrpc": "2.0",
            "id": 902,
            "method": "tools/call",
            "params": { "name": "continue_task", "arguments": {
                "task_id": task.id,
                "decision": { "selected_option_ids": ["nope"] }
            } },
        });
        let response = server.handle_message(&request.to_string()).await.unwrap();
        let parsed: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(parsed["result"]["isError"], true);
        assert!(parsed["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("unknown option id `nope`"));
        assert!(task.pending_question.lock().unwrap().is_some());
        assert!(task.pending_instructions.lock().unwrap().is_empty());
        let retry=server.tool_continue_task(&json!({"task_id":task.id,"decision":{"question_id":"q-1","selected_option_ids":["opt-a"]}})).await;
        assert!(retry.is_ok(), "{retry:?}");
    }

    #[tokio::test]
    async fn continue_task_decision_approves_permission_explicitly() {
        let (server, task, _dir) = delegated_task().await;

        let (approval_tx, mut approval_rx) = mpsc::channel::<ApprovalResponse>(4);
        *task.approval_tx.lock().unwrap_or_else(|e| e.into_inner()) = Some(approval_tx);
        task.store_pending_approval(kkagent_protocol::approval::ApprovalRequest {
            approval_id: "ap-1".into(),
            session_id: task.session_id.clone(),
            tool_call_id: "call-1".into(),
            tool_name: "Bash".into(),
            action: "run cargo test".into(),
            tool_input_display: None,
            created_at: chrono::Utc::now(),
        });
        task.set_phase(TaskPhase::AwaitingPermission);

        // decision {approve: true} — no keyword guessing involved.
        let request = json!({
            "jsonrpc": "2.0",
            "id": 903,
            "method": "tools/call",
            "params": { "name": "continue_task", "arguments": {
                "task_id": task.id,
                "decision": { "approve": true }
            } },
        });
        let response = server.handle_message(&request.to_string()).await.unwrap();
        let parsed: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(parsed["result"]["isError"], false);
        assert_eq!(parsed["result"]["structuredContent"]["action"], "approved");

        let approval = approval_rx.recv().await.expect("approval delivered");
        assert_eq!(approval.approval_id, "ap-1");
        assert_eq!(approval.decision, ApprovalDecision::Approved);
        assert!(approval.feedback.is_none());
    }

    #[tokio::test]
    async fn continue_task_decision_rejected_without_pending_decision_target() {
        let (server, task, _dir) = delegated_task().await;

        // Task is Queued: there is nothing to decide on; decision must be
        // rejected so the orchestrator falls back to instruction steering.
        let request = json!({
            "jsonrpc": "2.0",
            "id": 904,
            "method": "tools/call",
            "params": { "name": "continue_task", "arguments": {
                "task_id": task.id,
                "decision": { "approve": true }
            } },
        });
        let response = server.handle_message(&request.to_string()).await.unwrap();
        let parsed: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(parsed["result"]["isError"], true);
        assert!(parsed["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("no pending question or approval"));
    }

    #[tokio::test]
    async fn cancel_rejects_finished_tasks() {
        let server = server().await;
        // delegate is rejected (untrusted) so craft the flow: expect the
        // untrusted error and skip; instead verify cancel of unknown task.
        let request = json!({
            "jsonrpc": "2.0",
            "id": 11,
            "method": "tools/call",
            "params": { "name": "cancel", "arguments": { "task_id": "ghost" } },
        });
        let response = server.handle_message(&request.to_string()).await.unwrap();
        let parsed: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(parsed["result"]["isError"], true);
    }

    #[tokio::test]
    async fn list_workspaces_reports_registered_and_task_counts() {
        let server = server().await;
        let dir = tempfile::tempdir().unwrap();
        // Seed a session record pointing at the tempdir (auto-registration).
        {
            let db = &server.transcript;
            db.create_session("seed-session", "local/model", &dir.path().to_string_lossy())
                .unwrap();
        }
        let response = server
            .handle_message(r#"{"jsonrpc":"2.0","id":12,"method":"tools/call","params":{"name":"list_workspaces","arguments":{}}}"#)
            .await
            .unwrap();
        let parsed: Value = serde_json::from_str(&response).unwrap();
        let content = &parsed["result"]["structuredContent"];
        let workspaces = content["workspaces"].as_array().unwrap();
        assert!(
            workspaces
                .iter()
                .any(|w| w["session_count"].as_u64() == Some(1)),
            "tempdir workspace should be auto-registered: {content}"
        );
    }

    #[tokio::test]
    async fn get_context_reports_untrusted_missing_markers() {
        let server = server().await;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        let request = json!({
            "jsonrpc": "2.0",
            "id": 13,
            "method": "tools/call",
            "params": { "name": "get_context", "arguments": { "workspace": dir.path().to_string_lossy() } },
        });
        let response = server.handle_message(&request.to_string()).await.unwrap();
        let parsed: Value = serde_json::from_str(&response).unwrap();
        let content = &parsed["result"]["structuredContent"];
        assert_eq!(content["trusted"], false);
        assert_eq!(content["recent_sessions"], json!([]));
        assert_eq!(content["tasks"], json!([]));
        assert_eq!(content["attention"], json!([]));
    }

    #[tokio::test]
    async fn get_context_rejects_missing_paths() {
        let server = server().await;
        let request = json!({
            "jsonrpc": "2.0",
            "id": 14,
            "method": "tools/call",
            "params": { "name": "get_context", "arguments": { "workspace": "/definitely/not/here-xyz" } },
        });
        let response = server.handle_message(&request.to_string()).await.unwrap();
        let parsed: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(parsed["result"]["isError"], true);
        assert!(parsed["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("workspace does not exist"));
    }

    #[tokio::test]
    async fn unknown_tool_is_protocol_error() {
        let server = server().await;
        let response = server
            .handle_message(
                r#"{"jsonrpc":"2.0","id":15,"method":"tools/call","params":{"name":"nope","arguments":{}}}"#,
            )
            .await
            .unwrap();
        let parsed: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(parsed["error"]["code"], -32602);
    }

    #[tokio::test]
    async fn unknown_method_is_method_not_found() {
        let server = server().await;
        let response = server
            .handle_message(r#"{"jsonrpc":"2.0","id":16,"method":"wat/xyz"}"#)
            .await
            .unwrap();
        let parsed: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(parsed["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn notifications_and_garbage() {
        let server = server().await;
        assert!(server
            .handle_message(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
            .await
            .is_none());
        assert!(server.handle_message("   ").await.is_none());
        let parsed: Value =
            serde_json::from_str(&server.handle_message("not json").await.unwrap()).unwrap();
        assert_eq!(parsed["error"]["code"], -32700);
    }

    #[tokio::test]
    async fn ping_and_probes_roundtrip() {
        let server = server().await;
        for method in [
            "ping",
            "prompts/list",
            "resources/list",
            "resources/templates/list",
        ] {
            let response = server
                .handle_message(&format!(
                    r#"{{"jsonrpc":"2.0","id":17,"method":"{method}"}}"#
                ))
                .await
                .unwrap();
            let parsed: Value = serde_json::from_str(&response).unwrap();
            assert!(
                parsed["result"].is_object(),
                "{method} should return a result"
            );
        }
    }

    #[test]
    fn task_phase_strings_match_spec() {
        assert_eq!(TaskPhase::Queued.as_str(), "queued");
        assert_eq!(TaskPhase::Running.as_str(), "running");
        assert_eq!(TaskPhase::AwaitingInput.as_str(), "waiting_input");
        assert_eq!(TaskPhase::AwaitingPermission.as_str(), "waiting_permission");
        assert_eq!(TaskPhase::Completed.as_str(), "completed");
        assert_eq!(TaskPhase::Failed.as_str(), "failed");
        assert_eq!(TaskPhase::Cancelled.as_str(), "cancelled");
    }

    #[test]
    fn clip_chars_truncates() {
        assert_eq!(clip_chars("hello", 10), "hello");
        assert_eq!(clip_chars("hello", 4), "hell…");
    }

    #[test]
    fn constant_time_eq_matches_and_rejects() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secreT"));
        assert!(!constant_time_eq(b"secret", b"secret "));
        assert!(!constant_time_eq(b"", b"x"));
        assert!(constant_time_eq(b"", b""));
    }

    #[tokio::test]
    async fn http_transport_end_to_end() {
        let server = server().await;
        // Bind port 0 so parallel test runs never collide; the bound listener
        // is handed to serve_http (no double bind race).
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr").to_string();
        let handle = tokio::spawn(serve_http(
            Arc::clone(&server),
            listener,
            Some("tok1".into()),
            None,
        ));

        let url = format!("http://{addr}");
        // Bounded client: a server-side regression must fail fast, never
        // hang the whole test suite.
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .expect("reqwest client");
        let json_call = |method: &str, id: u64| {
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": { "name": "test", "version": "0" },
                },
            })
        };

        // Missing token → 401 with WWW-Authenticate and a JSON body (tunnel
        // probes parse responses as JSON; a text body would surface as a
        // malformed_json transport error upstream).
        let res = client
            .post(format!("{url}/mcp"))
            .header("accept", "application/json")
            .body(json_call("initialize", 1).to_string())
            .send()
            .await
            .expect("request");
        assert_eq!(res.status(), reqwest::StatusCode::UNAUTHORIZED);
        assert_eq!(
            res.headers()
                .get("www-authenticate")
                .and_then(|v| v.to_str().ok()),
            Some("Bearer realm=\"kkagent-mcp\"")
        );
        let body: Value = res.json().await.expect("401 body is JSON");
        assert_eq!(body["error"]["code"], -32000);

        // Wrong token → 401.
        let res = client
            .post(format!("{url}/mcp"))
            .header("accept", "application/json")
            .header("authorization", "Bearer nope")
            .body(json_call("initialize", 2).to_string())
            .send()
            .await
            .expect("request");
        assert_eq!(res.status(), reqwest::StatusCode::UNAUTHORIZED);

        // Correct token → JSON-RPC result with MCP headers.
        let res = client
            .post(format!("{url}/mcp"))
            .header("accept", "application/json")
            .header("authorization", "Bearer tok1")
            .header("mcp-protocol-version", "2025-06-18")
            .json(&json_call("initialize", 3))
            .send()
            .await
            .expect("request");
        assert_eq!(res.status(), reqwest::StatusCode::OK);
        assert_eq!(
            res.headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("application/json")
        );
        let parsed: Value = res.json().await.expect("json body");
        assert_eq!(parsed["id"], 3);
        assert_eq!(parsed["result"]["protocolVersion"], "2025-06-18");

        // Notification (no id) → 202 Accepted, empty body.
        let res = client
            .post(format!("{url}/mcp"))
            .header("accept", "application/json")
            .header("authorization", "Bearer tok1")
            .body(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
            .send()
            .await
            .expect("request");
        assert_eq!(res.status(), reqwest::StatusCode::ACCEPTED);
        assert!(res.text().await.expect("body").is_empty());

        // healthz needs no auth.
        let res = client
            .get(format!("{url}/healthz"))
            .send()
            .await
            .expect("request");
        assert_eq!(res.status(), reqwest::StatusCode::OK);
        assert_eq!(res.text().await.expect("body").trim(), "ok");

        // SSE-only accept → 406.
        let res = client
            .post(format!("{url}/mcp"))
            .header("accept", "text/event-stream")
            .header("authorization", "Bearer tok1")
            .body(json_call("ping", 4).to_string())
            .send()
            .await
            .expect("request");
        assert_eq!(res.status(), reqwest::StatusCode::NOT_ACCEPTABLE);

        // Streamable HTTP: GET (SSE listener) and DELETE (session teardown)
        // get a spec-compliant 405 — this server is stateless and JSON-only.
        for method in ["GET", "DELETE"] {
            let res = client
                .request(
                    reqwest::Method::from_bytes(method.as_bytes()).unwrap(),
                    format!("{url}/mcp"),
                )
                .header("authorization", "Bearer tok1")
                .send()
                .await
                .expect("request");
            assert_eq!(
                res.status(),
                reqwest::StatusCode::METHOD_NOT_ALLOWED,
                "{method}"
            );
            assert_eq!(
                res.headers().get("allow").and_then(|v| v.to_str().ok()),
                Some("POST"),
                "{method}"
            );
        }

        handle.abort();
    }

    #[tokio::test]
    async fn http_transport_binds_and_shuts_down() {
        let server = server().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let handle = tokio::spawn(serve_http(
            Arc::clone(&server),
            listener,
            Some("t".into()),
            None,
        ));
        // Give the accept loop a moment, then drop it — serve_http must end
        // cleanly when its listener is closed.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        handle.abort();
        let _ = handle.await;
    }

    #[test]
    fn mcp_url_for_prefers_loopback() {
        let addr: std::net::SocketAddr = "0.0.0.0:8788".parse().unwrap();
        assert_eq!(mcp_url_for(Some(addr)), "http://127.0.0.1:8788/mcp");
        let addr: std::net::SocketAddr = "192.168.1.5:9000".parse().unwrap();
        assert_eq!(mcp_url_for(Some(addr)), "http://192.168.1.5:9000/mcp");
        let addr: std::net::SocketAddr = "[::]:8788".parse().unwrap();
        assert_eq!(mcp_url_for(Some(addr)), "http://127.0.0.1:8788/mcp");
        assert_eq!(mcp_url_for(None), "http://127.0.0.1/mcp");
    }

    /// The install hint must match the running platform: Homebrew is
    /// macOS-only; other platforms point at release archives.
    #[test]
    fn tunnel_client_install_hint_matches_platform() {
        match std::env::consts::OS {
            "macos" => {
                assert!(tunnel_client_install_hint().contains("brew install"));
            }
            "windows" => {
                let hint = tunnel_client_install_hint();
                assert!(hint.contains("windows release"));
                assert!(hint.contains("releases/latest"));
            }
            _ => {
                let hint = tunnel_client_install_hint();
                assert!(hint.contains("release archive"));
                assert!(hint.contains("releases/latest"));
                assert!(!hint.contains("brew"));
            }
        }
    }

    #[cfg(unix)]
    fn write_executable(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).expect("write fake tunnel-client");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake tunnel-client");
        path
    }

    #[cfg(unix)]
    #[test]
    fn resolve_tunnel_client_explicit_and_path() {
        let _guard = TUNNEL_PROC_TESTS.blocking_lock();
        let temp = tempfile::tempdir().expect("tempdir");
        let fake = write_executable(temp.path(), "tunnel-client", "#!/bin/sh\nexit 0\n");
        // Explicit path resolves.
        assert_eq!(
            resolve_tunnel_client(Some(&fake)).expect("explicit path"),
            fake
        );
        // Explicit but missing path fails.
        assert!(resolve_tunnel_client(Some(&temp.path().join("nope"))).is_err());
        // PATH search finds it.
        let path_var = std::env::join_paths([
            temp.path().to_path_buf(),
            std::path::PathBuf::from("/usr/bin"),
            std::path::PathBuf::from("/bin"),
        ])
        .expect("join paths");
        let previous = std::env::var_os("PATH");
        // Safety: single-threaded test mutation of process env.
        std::env::set_var("PATH", &path_var);
        let resolved = resolve_tunnel_client(None).expect("PATH search");
        match previous {
            Some(previous) => std::env::set_var("PATH", previous),
            None => std::env::remove_var("PATH"),
        }
        assert_eq!(resolved, fake);
    }

    /// End-to-end: a fake `tunnel-client` shell script records its argv and
    /// environment and stays alive; serve_http must spawn it with the right
    /// wiring and kill it when the server stops.
    #[cfg(unix)]
    #[tokio::test]
    async fn tunnel_child_spawned_and_stopped_with_server() {
        let _guard = TUNNEL_PROC_TESTS.lock().await;
        use std::sync::atomic::AtomicUsize;

        // Unique files so concurrent test runs never collide.
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
        let temp = tempfile::tempdir().expect("tempdir");
        let args_file = temp.path().join(format!("args-{unique}.txt"));
        let env_file = temp.path().join(format!("env-{unique}.txt"));
        let pid_file = temp.path().join(format!("pid-{unique}.txt"));
        let fake_bin = write_executable(
            temp.path(),
            "tunnel-client",
            &format!(
                "#!/bin/sh\n\
                 echo \"$@\" > \"{args}\"\n\
                 env > \"{env}\"\n\
                 echo $$ > \"{pid}\"\n\
                 exec sleep 300\n",
                args = args_file.display(),
                env = env_file.display(),
                pid = pid_file.display(),
            ),
        );

        let server = server().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let handle = tokio::spawn(serve_http(
            Arc::clone(&server),
            listener,
            Some("t".into()),
            Some(TunnelOptions {
                tunnel_id: "tunnel_0123456789abcdef0123456789abcdef".into(),
                client_bin: Some(fake_bin),
                api_key: Some("sk-test-key".into()),
            }),
        ));
        // Startup probe window: wait for the fake client to actually start
        // (files appear after its fork+exec completes) instead of a fixed
        // sleep — exec latency is environment-dependent.
        async fn wait_for_file(path: &std::path::Path) -> String {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
            loop {
                if let Ok(content) = std::fs::read_to_string(path) {
                    return content;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "fake tunnel-client never wrote {}",
                    path.display()
                );
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
        let args = wait_for_file(&args_file).await;
        assert!(
            !handle.is_finished(),
            "serve_http should still be running with a healthy tunnel child"
        );

        // The endpoint must already be serving while the tunnel child is
        // alive — tunnel-client probes it with a 2s budget right after
        // spawning.
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .expect("reqwest client");
        let res = client
            .post(format!("http://{addr}/mcp"))
            .header("accept", "application/json")
            .header("authorization", "Bearer t")
            .body(
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "ping",
                })
                .to_string(),
            )
            .send()
            .await
            .expect("request during tunnel startup");
        assert_eq!(res.status(), reqwest::StatusCode::OK);

        // argv wiring.
        assert!(
            args.contains("--control-plane.tunnel-id tunnel_0123456789abcdef0123456789abcdef"),
            "argv: {args}"
        );
        assert!(
            args.contains(&format!(
                "--mcp.server-url http://127.0.0.1:{}/mcp",
                addr.port()
            )),
            "argv: {args}"
        );
        // env wiring: key + token via env-ref header.
        let env = wait_for_file(&env_file).await;
        assert!(
            env.contains("CONTROL_PLANE_API_KEY=sk-test-key"),
            "env: {env}"
        );
        assert!(
            env.lines()
                .any(|line| line == "KKAGENT_MCP_HTTP_AUTH=Bearer t"),
            "auth env: {env}"
        );
        assert!(
            env.lines()
                .any(|line| line == "MCP_EXTRA_HEADERS=Authorization: env:KKAGENT_MCP_HTTP_AUTH"),
            "headers env: {env}"
        );
        assert!(
            env.lines().any(|line| line
                == "MCP_DISCOVERY_EXTRA_HEADERS=Authorization: env:KKAGENT_MCP_HTTP_AUTH"),
            "discovery headers env: {env}"
        );
        // Only the main MCP channel is polled; harpoon commands are ignored.
        assert!(
            env.lines()
                .any(|line| line == "CONTROL_PLANE_POLL_CHANNELS=main"),
            "poll channels env: {env}"
        );
        // The fake client is actually running.
        let pid: u32 = std::fs::read_to_string(&pid_file)
            .expect("pid file")
            .trim()
            .parse()
            .expect("pid");

        // Abort the server: the tunnel child must die with it (kill_on_drop).
        handle.abort();
        let _ = handle.await;
        for _ in 0..50 {
            let gone = !std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .expect("kill probe")
                .success();
            if gone {
                return; // child is gone ✓
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        panic!("tunnel child (pid {pid}) survived serve_http shutdown");
    }

    /// Startup failures fail fast with actionable errors (empty explicit key
    /// is rejected without touching process env, so no parallel-test races).
    #[cfg(unix)]
    #[tokio::test]
    async fn tunnel_child_fails_fast_without_api_key() {
        let _guard = TUNNEL_PROC_TESTS.lock().await;
        let temp = tempfile::tempdir().expect("tempdir");
        let fake_bin = write_executable(temp.path(), "tunnel-client", "#!/bin/sh\nexit 0\n");
        let server = server().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let error = serve_http(
            Arc::clone(&server),
            listener,
            Some("t".into()),
            Some(TunnelOptions {
                tunnel_id: "tunnel_0123456789abcdef0123456789abcdef".into(),
                client_bin: Some(fake_bin),
                api_key: Some(String::new()),
            }),
        )
        .await
        .expect_err("missing API key must fail");
        assert!(
            error.to_string().contains("CONTROL_PLANE_API_KEY"),
            "error: {error}"
        );
    }

    /// A tunnel-client that dies during startup aborts serve_http instead of
    /// serving a tunnel nobody can reach. Bounded: a probe regression must
    /// fail the test, never hang the whole suite.
    #[cfg(unix)]
    #[tokio::test]
    async fn tunnel_child_dying_at_startup_fails_serve() {
        let _guard = TUNNEL_PROC_TESTS.lock().await;
        // Slow fork+exec on network file systems can keep the client "not
        // yet started" past the default startup window; widen it so the test
        // exercises the fail-fast path itself rather than environment
        // latency. The guard removes the override even on panic.
        struct RemoveOnDrop(&'static str);
        impl Drop for RemoveOnDrop {
            fn drop(&mut self) {
                std::env::remove_var(self.0);
            }
        }
        std::env::set_var("KKAGENT_TUNNEL_STARTUP_WINDOW_SECS", "120");
        let _window_override = RemoveOnDrop("KKAGENT_TUNNEL_STARTUP_WINDOW_SECS");
        let served = tokio::time::timeout(
            std::time::Duration::from_secs(150),
            tunnel_dying_serve_once(),
        )
        .await
        .unwrap_or_else(|_| {
            panic!("serve_http did not fail within 150s for a dying tunnel-client")
        });
        let error = served.expect_err("dead client must fail serve_http");
        assert!(
            error.to_string().contains("exited during startup"),
            "error: {error}"
        );
    }

    #[cfg(unix)]
    async fn tunnel_dying_serve_once() -> Result<()> {
        let temp = tempfile::tempdir().expect("tempdir");
        let fake_bin = write_executable(
            temp.path(),
            "tunnel-client",
            "#!/bin/sh\necho boom >&2\nexit 7\n",
        );
        let server = server().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        serve_http(
            Arc::clone(&server),
            listener,
            Some("t".into()),
            Some(TunnelOptions {
                tunnel_id: "tunnel_0123456789abcdef0123456789abcdef".into(),
                client_bin: Some(fake_bin),
                api_key: Some("sk-test-key".into()),
            }),
        )
        .await
    }
    #[tokio::test]
    async fn continuation_queue_is_fifo_and_rejects_decisions_without_mutation() {
        let (server, task, _dir) = delegated_task().await;
        task.set_phase(TaskPhase::Queued);
        let invalid = server.tool_continue_task(&json!({"task_id":task.id,"instruction":"must not queue","decision":{"approve":true}})).await;
        assert!(invalid.is_err());
        assert!(task.pending_instructions.lock().unwrap().is_empty());
        for instruction in ["first", "second"] {
            server
                .tool_continue_task(&json!({"task_id":task.id,"instruction":instruction}))
                .await
                .unwrap();
        }
        assert_eq!(task.pending_instructions.lock().unwrap().len(), 2);
        assert_eq!(wait_for_instruction(&task).await.as_deref(), Some("first"));
        assert_eq!(wait_for_instruction(&task).await.as_deref(), Some("second"));
        assert!(task.pending_instructions.lock().unwrap().is_empty());
        task.mailbox.start_turn();
        server
            .tool_continue_task(&json!({"task_id":task.id,"instruction":"live"}))
            .await
            .unwrap();
        assert!(task.pending_instructions.lock().unwrap().is_empty());
        assert_eq!(task.mailbox.drain().len(), 1);
    }
}
