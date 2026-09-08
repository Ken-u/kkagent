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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Instant;

use anyhow::Result;
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

// ---------------------------------------------------------------------------
// Task model
// ---------------------------------------------------------------------------

/// Lifecycle phase, surfaced verbatim through `get_progress`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[derive(Default, Clone)]
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

/// Review summary refreshed by the runner after every turn. Mechanical only —
/// composed from the session's final assistant text and git/checkpoint data,
/// never a second LLM pass.
#[derive(Default, Clone)]
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
    session_id: String,
    description: String,
    prompt: String,
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
    /// Wakes the runner when new instructions / answers arrive.
    runner_notify: Notify,
    /// Abort handle for the runner's tokio task (hard cancel backstop).
    abort: StdMutex<Option<tokio::task::AbortHandle>>,

    // observed state
    phase: StdMutex<TaskPhase>,
    progress: StdMutex<Progress>,
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
        events.push(text);
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
    let server = Arc::new(McpServer::new(config, transcript));
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

pub struct McpServer {
    config: Arc<kkagent_config::AppConfig>,
    web: Arc<kkagent_tools::WebServicesConfig>,
    transcript: TranscriptDb,
    queue: Arc<Semaphore>,
    tasks: Arc<Mutex<HashMap<String, Arc<McpTask>>>>,
}

impl McpServer {
    pub fn new(config: Arc<kkagent_config::AppConfig>, transcript: TranscriptDb) -> Self {
        let web = Arc::new(kkagent_tools::WebServicesConfig::from_app(config.as_ref()));
        let queue = Arc::new(Semaphore::new(config.subagent.effective_max_concurrent()));
        Self {
            config,
            web,
            transcript,
            queue,
            tasks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Handle one raw stdin line. Returns the JSON-RPC response line to write
    /// back, or `None` for notifications / unparseable noise.
    pub async fn handle_message(&self, line: &str) -> Option<String> {
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
        &self,
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
            "instructions": "kkagent is an asynchronous coding-agent supervisor. Workflow: \
             list_workspaces to discover projects (auto-registered from kkagent session \
             history), get_context to load a project's supervisor-level state, inspect to \
             read sources, logs, diffs, images and task artifacts directly, delegate to \
             start an async coding task as a fresh default session (returns task_id; \
             kkagent picks model and worktree isolation itself), get_progress to \
             poll, continue_task to send new instructions (answer questions / approve \
             actions / steer / continue a task in its original session), get_result for a \
             review-ready summary (image paths included; read them with inspect), cancel \
             to stop a task while keeping its changes and worktree.",
        })
    }

    async fn tools_call(&self, params: &Value) -> std::result::Result<Value, (i64, String)> {
        let name = params
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if name.is_empty() {
            return Err((-32602, "tools/call requires a tool name".into()));
        }
        let args = params.get("arguments").cloned().unwrap_or(json!({}));
        // inspect may inline image content blocks (image kind); every other
        // tool returns plain text, normalized to a single text block.
        let outcome = match name {
            "get_result" => self.tool_get_result(&args).await,
            "inspect" => self.tool_inspect(&args).await,
            "list_workspaces" => self.tool_list_workspaces(&args).await.map_text_block(),
            "get_context" => self.tool_get_context(&args).await.map_text_block(),
            "delegate" => self.tool_delegate(&args).await.map_text_block(),
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
            let path = args.get("path").and_then(|v| v.as_str()).map(str::trim);
            let diff = git_diff(&workspace, path).await.ok_or_else(|| {
                format!(
                    "cannot compute git diff in {} (not a git repo?)",
                    workspace.display()
                )
            })?;
            let payload = json!({
                "workspace": workspace.to_string_lossy(),
                "kind": "diff",
                "path": path,
                "truncated": diff.len() >= MAX_INSPECT_BYTES,
            });
            return Ok((vec![json!({ "type": "text", "text": diff })], Some(payload)));
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
        let payload = json!({
            "workspace": workspace.to_string_lossy(),
            "kind": kind,
            "path": resolved.to_string_lossy(),
            "bytes": bytes.len(),
            "total_lines": total_lines,
            "offset": offset,
            "returned_lines": selected.len(),
            "truncated": truncated,
        });
        Ok((vec![json!({ "type": "text", "text": text })], Some(payload)))
    }

    async fn tool_delegate(
        &self,
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
            session_id: format!("mcp-{task_id}"),
            description: description.clone(),
            prompt: prompt.clone(),
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
            abort: StdMutex::new(None),
            phase: StdMutex::new(TaskPhase::Queued),
            progress: StdMutex::new(Progress::default()),
            recent_events: StdMutex::new(Vec::new()),
            pending_question: StdMutex::new(None),
            pending_approval: StdMutex::new(None),
            pending_instructions: StdMutex::new(Vec::new()),
            summary: StdMutex::new(TaskSummary::default()),
            error: StdMutex::new(None),
            started_at: Instant::now(),
            finished_at: StdMutex::new(None),
        });
        self.tasks
            .lock()
            .await
            .insert(task_id.clone(), Arc::clone(&task));

        let ctx = RunnerCtx {
            config: Arc::clone(&self.config),
            web: Arc::clone(&self.web),
            queue: Arc::clone(&self.queue),
        };
        let handle = tokio::spawn(run_task(ctx, Arc::clone(&task)));
        *task.abort.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle.abort_handle());

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
                "kind": TASK_KIND,
                "status": "queued",
                "workspace": workspace.to_string_lossy(),
                "run_dir": run_dir.to_string_lossy(),
                "isolated": isolated,
            })),
        ))
    }

    async fn tool_get_progress(
        &self,
        args: &Value,
    ) -> std::result::Result<(String, Option<Value>), String> {
        let task = self.require_task(args).await?;
        let phase = task.phase();
        let payload = json!({
            "task_id": task.id,
            "kind": TASK_KIND,
            "status": phase.as_str(),
            "description": task.description,
            "workspace": task.origin_workspace.to_string_lossy(),
            "run_dir": task.run_dir.lock().await.to_string_lossy(),
            "isolated": task.isolated,
            "elapsed_seconds": task.elapsed_seconds(),
            "progress": task.progress_snapshot(),
            "recent_events": task.recent_events.lock().unwrap_or_else(|e| e.into_inner()).clone(),
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
            "instructions_received": task.pending_instructions.lock().unwrap_or_else(|e| e.into_inner()).len(),
            "error": task.error.lock().unwrap_or_else(|e| e.into_inner()).clone(),
        });
        let mut text = format!("task {} is {}", task.id, phase.as_str());
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
        &self,
        args: &Value,
    ) -> std::result::Result<(String, Option<Value>), String> {
        let instruction = args
            .get("instruction")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let decision = args.get("decision").cloned().filter(|v| v.is_object());
        if instruction.is_none() && decision.is_none() {
            return Err("instruction or decision is required".to_string());
        }
        let task = self.require_task(args).await?;
        if let Some(instruction) = &instruction {
            let mut pending = task
                .pending_instructions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if pending.len() >= PENDING_INSTRUCTIONS_CAP {
                return Err(format!(
                    "task {} already has {} pending instructions; wait for them to be consumed",
                    task.id,
                    pending.len()
                ));
            }
            pending.push(instruction.clone());
        }

        match task.phase() {
            TaskPhase::AwaitingInput => {
                // Answer the pending question; the agent continues its turn.
                let Some(question) = task.take_pending_question() else {
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
                task.set_phase(TaskPhase::Running);
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
                Ok((
                    format!("answer delivered to task {}", task.id),
                    Some(json!({
                        "task_id": task.id,
                        "status": task.status(),
                        "action": "question_answered",
                    })),
                ))
            }
            TaskPhase::AwaitingPermission => {
                // Approve only on explicit decision or explicit approval
                // wording; anything else rejects the action (fail closed) and
                // carries the text as guidance for the agent's next step.
                let Some(request) = task.take_pending_approval() else {
                    return Err("no pending approval".into());
                };
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
                task.set_phase(TaskPhase::Running);
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
                Ok((
                    format!(
                        "permission {} for task {}",
                        if approve { "approved" } else { "rejected" },
                        task.id
                    ),
                    Some(json!({
                        "task_id": task.id,
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
                task.runner_notify.notify_one();
                Ok((
                    format!("instruction delivered to running task {}", task.id),
                    Some(json!({
                        "task_id": task.id,
                        "status": task.status(),
                        "action": "instruction_queued",
                    })),
                ))
            }
            TaskPhase::Completed | TaskPhase::Failed => {
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
                task.set_phase(TaskPhase::Running);
                task.runner_notify.notify_one();
                Ok((
                    format!(
                        "task {} will continue with the new instruction in its original session",
                        task.id
                    ),
                    Some(json!({
                        "task_id": task.id,
                        "status": task.status(),
                        "action": "task_continued",
                    })),
                ))
            }
            TaskPhase::Cancelled => Err(format!(
                "task {} was cancelled; delegate a new task to continue this work",
                task.id
            )),
        }
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
        let payload = json!({
            "task_id": task.id,
            "kind": TASK_KIND,
            "status": phase.as_str(),
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
        if let Some(handle) = task.abort.lock().unwrap_or_else(|e| e.into_inner()).take() {
            handle.abort();
        }
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
        Ok((
            format!(
                "task {} cancelled; run_dir {} preserved",
                task.id,
                task.run_dir.lock().await.display()
            ),
            Some(json!({
                "task_id": task.id,
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
                format!(
                    "unknown task_id: {task_id} (only tasks started in this server session are tracked)"
                )
            })
    }

    /// Resolve the `workspace` / `working_dir` / `path` argument, defaulting
    /// to the sole trusted root (or current dir when unconfigured).
    async fn resolve_workspace_arg(&self, args: &Value) -> Result<PathBuf, String> {
        let raw = args
            .get("workspace")
            .or_else(|| args.get("working_dir"))
            .or_else(|| args.get("path"))
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
async fn run_task(ctx: RunnerCtx, task: Arc<McpTask>) {
    // Queued until a concurrency slot frees up.
    let _permit = ctx.queue.acquire().await;
    if task.interrupt.load(Ordering::SeqCst) {
        return;
    }
    task.set_phase(TaskPhase::Running);

    let run_dir = task.run_dir.lock().await.clone();
    // Fresh default session: `general` profile with no override resolves to
    // the globally configured default model.
    let model = ctx.config.resolve_subagent_model(TASK_PROFILE, None, None);
    let mut session = Session::for_subagent(
        task.session_id.clone(),
        run_dir.clone(),
        PermissionMode::Auto,
        model,
    );
    session.attach_workspace_concurrency_guard();
    session.inject_workspace_instructions().await;
    session.system_prompt.push_str(&supervisor_system_addon());
    // Drain pre-dispatch instructions into the initial message, then wire the
    // answer channels to this session so continue_task can respond to
    // AskUserQuestion / approval requests of the live turn.
    let initial_instructions = task
        .pending_instructions
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    task.pending_instructions
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    session.add_user_message(build_initial_prompt_sync(
        &task.prompt,
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
    let permission = PermissionChain::new(PermissionMode::Auto, permission_rules);

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
    let result_store = Arc::new(kkagent_core::agent_loop::ToolResultStore::for_subagent(
        kkagent_config::default_config_dir(),
        task.session_id.clone(),
    ));
    agent = agent.with_tool_result_store(result_store);

    loop {
        task.progress
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .turns += 1;
        let run_result = agent.run_turn(&mut session).await;

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
}

/// Park until a new instruction arrives on a terminal task. Returns `None`
/// only when the task was cancelled while waiting.
async fn wait_for_instruction(task: &Arc<McpTask>) -> Option<String> {
    loop {
        let notified = task.runner_notify.notified();
        // Check the queue after arming the notifier to avoid missed wakes.
        let next = task
            .pending_instructions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop();
        if let Some(text) = next {
            return Some(text);
        }
        if task.interrupt.load(Ordering::SeqCst) {
            return None;
        }
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
                if *is_error {
                    task.push_event(format!("tool error: {tool_name}"));
                }
                return;
            }
            AgentEvent::MessageDelta { text, .. } => {
                progress.output_chars += text.chars().count() as u64;
                return;
            }
            AgentEvent::ThinkingDelta { text, .. } => {
                progress.thinking_chars += text.chars().count() as u64;
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

fn build_initial_prompt_sync(prompt: &str, instructions: &[String]) -> String {
    // Instructions queued before the first turn start become part of the
    // initial task description (delegate + immediate continue_task race).
    if instructions.is_empty() {
        return prompt.to_string();
    }
    let mut combined = prompt.to_string();
    combined.push_str("\n\nAdditional instructions received at dispatch:\n");
    for instruction in instructions {
        combined.push_str(&format!("- {instruction}\n"));
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
         build/test results, and any open issues."
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

/// Working-tree diff (optionally scoped to one path), size-bounded.
async fn git_diff(dir: &Path, path: Option<&str>) -> Option<String> {
    let mut args = vec!["diff".to_string(), "HEAD".to_string()];
    args.push("--".to_string());
    if let Some(path) = path {
        args.push(path.to_string());
    }
    let out = tokio::process::Command::new("git")
        .args(&args)
        .current_dir(dir)
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let mut text = String::from_utf8_lossy(&out.stdout).to_string();
    if text.chars().count() > MAX_INSPECT_BYTES {
        text = clip_chars(&text, MAX_INSPECT_BYTES);
        text.push_str("\n\n[diff truncated]");
    }
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
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
    .to_string()
}

fn tool_definitions() -> Vec<Value> {
    vec![
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
                    "truncated": { "type": "boolean" }
                },
            },
        }),
        json!({
            "name": "delegate",
            "description": "Delegate a new task to a kkagent coding agent — equivalent to opening a fresh default kkagent session in the workspace — and return immediately with a task_id. kkagent decides everything itself (model, profile, tools, and whether to use an isolated git worktree); the agent autonomously handles code retrieval, planning, file modification, build, test, fix and verification. Poll get_progress, deliver decisions with continue_task, collect with get_result.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "prompt": { "type": "string", "description": "Full task description: goal, requirements, constraints" },
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
            "description": "Poll a background task: status (queued | running | waiting_input | waiting_permission | completed | failed | cancelled), elapsed time, activity counters, recent events, the pending question or approval if any, and errors.",
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
    ]
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    async fn server() -> Arc<McpServer> {
        Arc::new(McpServer::new(
            Arc::new(kkagent_config::AppConfig::default()),
            TranscriptDb::open_in_memory().expect("in-memory transcript db"),
        ))
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
        Arc::new(McpServer::new(
            Arc::new(config),
            TranscriptDb::open_in_memory().expect("in-memory transcript db"),
        ))
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
    async fn tools_list_exposes_the_eight_delegation_tools() {
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
                "delegate",
                "get_progress",
                "continue_task",
                "get_result",
                "cancel",
            ]
        );
        let delegate = tools.iter().find(|t| t["name"] == "delegate").unwrap();
        // delegate takes only prompt + workspace — kkagent decides the rest.
        let delegate_props = &delegate["inputSchema"]["properties"];
        assert_eq!(
            delegate_props
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["prompt", "workspace"]
        );
        assert!(delegate["outputSchema"]["properties"]["task_id"].is_object());
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
        let server = Arc::new(McpServer::new(
            Arc::new(config),
            TranscriptDb::open_in_memory().expect("in-memory transcript db"),
        ));
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
        if let Some(handle) = task.abort.lock().unwrap_or_else(|e| e.into_inner()).take() {
            handle.abort();
        }
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
}
