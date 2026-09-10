use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::{Tool, ToolContext, ToolOutput};

const MAX_OUTPUT: usize = 50_000;
pub const DEFAULT_TIMEOUT_S: u64 = 120; // 2 minutes foreground default
const MAX_TIMEOUT_S: u64 = 300; // 5 minutes foreground
const MIN_TIMEOUT_S: u64 = 1; // clamp zero/undersized timeouts to 1s to avoid instant kills
const DEFAULT_BG_TIMEOUT_S: u64 = 600;
const MAX_BG_TIMEOUT_S: u64 = 86_400; // 24h
const MAX_BACKGROUND_JOBS: usize = 256;
const MAX_RUNNING_JOBS: usize = 16;
/// Per-session cap, strictly tighter than the process-global cap, so one
/// session (or one delegate) cannot alone exhaust every slot on a shared
/// standalone server.
const MAX_RUNNING_JOBS_PER_SESSION: usize = 8;
/// Bound on how long `stop()` waits to observe the job actually leave
/// `Running` before reporting an unconfirmed request back to the caller.
const STOP_CONFIRM_TIMEOUT_MS: u64 = 2_000;
/// Marker prepended to the in-memory rolling output window once bytes have
/// been dropped from its front. Kept intact across trims (see
/// `append_output`) so it never accumulates partial copies.
const TRUNCATION_MARKER: &str = "... earlier output truncated ...\n";

#[derive(Debug, Clone)]
pub struct BashOptions {
    /// When a foreground command times out, detach into background instead of killing.
    pub auto_background_on_timeout: bool,
    pub sandbox: crate::sandbox::SandboxPolicy,
    /// Default foreground timeout in seconds, overridable via `bash_task_timeout_s` config.
    pub default_timeout_s: u64,
    /// Toolchain config used for global-install deny checks.
    pub toolchain: kkagent_config::ToolchainConfig,
    /// Execution environment (local or SSH).
    pub kaos: kkagent_kaos::KaosHandle,
}

impl Default for BashOptions {
    fn default() -> Self {
        Self {
            auto_background_on_timeout: true,
            sandbox: crate::sandbox::SandboxPolicy::default(),
            default_timeout_s: DEFAULT_TIMEOUT_S,
            toolchain: kkagent_config::ToolchainConfig::default(),
            kaos: kkagent_kaos::KaosHandle::Local(std::sync::Arc::new(
                kkagent_kaos::LocalKaos::cwd(),
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum ShellStatus {
    Running,
    Complete,
    Failed,
    TimedOut,
    Cancelled,
}

/// Cooperative cancellation signal with an event-driven observer: `cancelled()`
/// resolves as soon as `cancel()` is called instead of polling on an interval,
/// so cancellation works uniformly whether or not a timeout is also in play.
#[derive(Debug, Default)]
pub(crate) struct CancelToken {
    flag: std::sync::atomic::AtomicBool,
    notify: tokio::sync::Notify,
}

impl CancelToken {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub(crate) fn cancel(&self) {
        self.flag.store(true, std::sync::atomic::Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    fn is_cancelled(&self) -> bool {
        self.flag.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Resolves once `cancel()` has been called. Registers the waiter before
    /// checking the flag so a `cancel()` racing with the first poll is never
    /// missed (unlike a plain `sleep`-based busy loop).
    async fn cancelled(&self) {
        loop {
            let notified = self.notify.notified();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

#[derive(Clone)]
struct ShellJob {
    session_id: String,
    description: String,
    command: String,
    status: ShellStatus,
    output: String,
    exit_code: Option<i32>,
    started_at: std::time::Instant,
    cancel: Arc<CancelToken>,
    output_log: crate::output_log::OutputLog,
}

/// Compact info about a background shell (for /ps panel lists). Covers both
/// running jobs and — when the caller asks for recent history — jobs that
/// already reached a terminal state, so a finished task doesn't just vanish
/// from the panel (issues/bash_background_issues.md #7 point 4).
#[derive(Debug, Clone)]
pub struct BackgroundJobInfo {
    pub id: String,
    pub description: String,
    pub command: String,
    pub elapsed_secs: u64,
    /// `running` | `complete` | `failed` | `timedout` | `cancelled`
    pub status: String,
    pub running: bool,
}

/// Full snapshot of a background shell (for in-panel output views).
#[derive(Debug, Clone)]
pub struct BackgroundJobDetail {
    pub id: String,
    pub description: String,
    pub command: String,
    pub status: String,
    pub elapsed_secs: u64,
    pub exit_code: Option<i32>,
    pub running: bool,
    pub output: String,
}

/// Tracks background / detached shell processes for Bash tool polling.
pub struct BackgroundShellManager {
    jobs: Mutex<HashMap<String, ShellJob>>,
}

impl BackgroundShellManager {
    pub fn new() -> Self {
        Self {
            jobs: Mutex::new(HashMap::new()),
        }
    }

    async fn insert_running(
        &self,
        id: &str,
        session_id: &str,
        description: String,
        command: String,
    ) -> Result<Arc<CancelToken>, String> {
        let mut jobs = self.jobs.lock().await;
        let running_for_session = jobs
            .values()
            .filter(|job| job.session_id == session_id && job.status == ShellStatus::Running)
            .count();
        if running_for_session >= MAX_RUNNING_JOBS_PER_SESSION {
            return Err(format!(
                "background shell limit reached for this session \
                 ({running_for_session}/{MAX_RUNNING_JOBS_PER_SESSION} running)"
            ));
        }
        let running_total = jobs
            .values()
            .filter(|job| job.status == ShellStatus::Running)
            .count();
        if running_total >= MAX_RUNNING_JOBS {
            return Err(format!(
                "background shell limit reached globally \
                 ({running_total}/{MAX_RUNNING_JOBS} running across all sessions)"
            ));
        }
        let mut evicted_logs: Vec<crate::output_log::OutputLog> = Vec::new();
        if jobs.len() >= MAX_BACKGROUND_JOBS {
            // Evict the oldest *finished* jobs first (FIFO), not every
            // finished job in one sweep, so unrelated sessions' history
            // isn't wiped out by one session's churn. Never evict a job
            // whose completion notification hasn't been drained yet —
            // untracking it would silently destroy the notification before
            // the owning session ever sees it (issues/bash_background_issues.md #5).
            // `untrack_if_no_pending` re-checks under the hub lock, closing
            // the race where a job completes between the `has_notification`
            // filter above and the untrack below.
            let hub = crate::task_notify::global_hub();
            let mut finished: Vec<(String, std::time::Instant)> = jobs
                .iter()
                .filter(|(id, job)| job.status != ShellStatus::Running && !hub.has_notification(id))
                .map(|(id, job)| (id.clone(), job.started_at))
                .collect();
            finished.sort_by_key(|(_, started_at)| *started_at);
            let overflow = jobs.len() + 1 - MAX_BACKGROUND_JOBS;
            for (evict_id, _) in finished.into_iter().take(overflow) {
                if !hub.untrack_if_no_pending(&evict_id) {
                    // A completion notification raced in since the filter;
                    // keep the job (and its notification) for the session.
                    continue;
                }
                if let Some(job) = jobs.remove(&evict_id) {
                    evicted_logs.push(job.output_log);
                }
            }
        }
        if jobs.len() >= MAX_BACKGROUND_JOBS {
            return Err(format!(
                "background shell history limit reached ({MAX_BACKGROUND_JOBS} jobs)"
            ));
        }
        // Register with the task-notification hub so the session is reminded
        // about this task at turn end and pushed a <task-notification> when
        // it finishes — the model never needs to poll for completion.
        crate::task_notify::global_hub().track(
            session_id,
            id,
            crate::task_notify::TaskKind::Bash,
            &description,
        );
        let cancel = CancelToken::new();
        jobs.insert(
            id.to_string(),
            ShellJob {
                session_id: session_id.to_string(),
                description,
                command,
                status: ShellStatus::Running,
                output: String::new(),
                exit_code: None,
                started_at: std::time::Instant::now(),
                cancel: cancel.clone(),
                output_log: crate::output_log::OutputLog::new(session_id, id),
            },
        );
        drop(jobs);
        for log in evicted_logs {
            log.remove().await;
        }
        Ok(cancel)
    }

    /// Cooperative cancel for every running job belonging to `session_id`.
    pub async fn cancel_session(&self, session_id: &str) {
        let jobs = self.jobs.lock().await;
        for job in jobs.values() {
            if job.session_id == session_id && job.status == ShellStatus::Running {
                job.cancel.cancel();
            }
        }
    }

    async fn append_output(&self, id: &str, chunk: &str) {
        let output_log = {
            let mut jobs = self.jobs.lock().await;
            let Some(job) = jobs.get_mut(id) else {
                return;
            };
            job.output.push_str(chunk);
            if job.output.len() > MAX_OUTPUT * 2 {
                // Rebuild the rolling window: strip the previous marker whole,
                // trim the body to the cap, then re-add the marker. Trimming
                // across a live marker would leave a partial remnant that
                // stacks under the re-added one ("...lier output truncated ...").
                if job.output.starts_with(TRUNCATION_MARKER) {
                    drop_utf8_bytes_from_front_in_place(&mut job.output, TRUNCATION_MARKER.len());
                }
                if job.output.len() > MAX_OUTPUT * 2 {
                    let overflow = job.output.len() - MAX_OUTPUT * 2;
                    drop_utf8_bytes_from_front_in_place(&mut job.output, overflow);
                }
                job.output.insert_str(0, TRUNCATION_MARKER);
            }
            job.output_log.clone()
        };
        output_log.append(chunk).await;
    }

    async fn finish(&self, id: &str, status: ShellStatus, exit_code: Option<i32>) {
        let (session_id, description, output) = {
            let mut jobs = self.jobs.lock().await;
            let Some(job) = jobs.get_mut(id) else {
                return;
            };
            job.status = status;
            job.exit_code = exit_code;
            (
                job.session_id.clone(),
                job.description.clone(),
                job.output.clone(),
            )
        };
        // Push the completion into the task-notification hub (single funnel
        // for every background-shell termination path).
        crate::task_notify::global_hub().on_bash_finished(
            &session_id,
            id,
            &description,
            status,
            exit_code,
            &output,
        );
    }

    pub async fn snapshot(
        &self,
        id: &str,
    ) -> Option<(String, String, String, String, Option<i32>, bool)> {
        let job = self.jobs.lock().await.get(id)?.clone();
        Some((
            job.description,
            job.command,
            format!("{:?}", job.status).to_lowercase(),
            job.output,
            job.exit_code,
            job.status == ShellStatus::Running,
        ))
    }

    /// Handle to a job's disk-backed output log, for incremental
    /// (`since_offset`) reads (issues/bash_background_issues.md #6).
    pub async fn output_log(&self, id: &str) -> Option<crate::output_log::OutputLog> {
        self.jobs
            .lock()
            .await
            .get(id)
            .map(|job| job.output_log.clone())
    }

    /// List all known background shell jobs (for TaskList unification).
    pub async fn list_jobs(&self) -> Vec<(String, String, String, bool)> {
        self.jobs
            .lock()
            .await
            .iter()
            .map(|(id, job)| {
                (
                    id.clone(),
                    job.description.clone(),
                    format!("{:?}", job.status).to_lowercase(),
                    job.status == ShellStatus::Running,
                )
            })
            .collect()
    }

    /// Running background jobs for one session, oldest first (for /ps panel).
    pub async fn list_running_for_session(&self, session_id: &str) -> Vec<BackgroundJobInfo> {
        self.list_for_session(session_id, 0).await
    }

    /// Background jobs for one session: every running job (oldest first),
    /// followed by up to `recent_finished_limit` most-recently-started jobs
    /// that already reached a terminal state. Keeping a bounded tail of
    /// finished jobs means a completed/failed task stays visible in the /ps
    /// panel instead of disappearing the instant it stops running
    /// (issues/bash_background_issues.md #7 point 4).
    pub async fn list_for_session(
        &self,
        session_id: &str,
        recent_finished_limit: usize,
    ) -> Vec<BackgroundJobInfo> {
        let jobs = self.jobs.lock().await;
        let mut running: Vec<(std::time::Instant, BackgroundJobInfo)> = jobs
            .iter()
            .filter(|(_, job)| job.session_id == session_id && job.status == ShellStatus::Running)
            .map(|(id, job)| {
                (
                    job.started_at,
                    BackgroundJobInfo {
                        id: id.clone(),
                        description: job.description.clone(),
                        command: job.command.clone(),
                        elapsed_secs: job.started_at.elapsed().as_secs(),
                        status: "running".to_string(),
                        running: true,
                    },
                )
            })
            .collect();
        // Sort by Instant so sub-second starts stay stable (elapsed_secs is too coarse).
        running.sort_by_key(|(started_at, _)| *started_at);
        let mut result: Vec<BackgroundJobInfo> =
            running.into_iter().map(|(_, info)| info).collect();

        if recent_finished_limit > 0 {
            let mut finished: Vec<(std::time::Instant, BackgroundJobInfo)> = jobs
                .iter()
                .filter(|(_, job)| {
                    job.session_id == session_id && job.status != ShellStatus::Running
                })
                .map(|(id, job)| {
                    (
                        job.started_at,
                        BackgroundJobInfo {
                            id: id.clone(),
                            description: job.description.clone(),
                            command: job.command.clone(),
                            elapsed_secs: job.started_at.elapsed().as_secs(),
                            status: format!("{:?}", job.status).to_lowercase(),
                            running: false,
                        },
                    )
                })
                .collect();
            // Most recently started first, as a proxy for most recently finished.
            finished.sort_by_key(|(started_at, _)| std::cmp::Reverse(*started_at));
            result.extend(
                finished
                    .into_iter()
                    .take(recent_finished_limit)
                    .map(|(_, info)| info),
            );
        }
        result
    }

    /// Full snapshot of one background job (for in-panel output view).
    pub async fn snapshot_detail(&self, id: &str) -> Option<BackgroundJobDetail> {
        let job = self.jobs.lock().await.get(id)?.clone();
        Some(BackgroundJobDetail {
            id: id.to_string(),
            description: job.description,
            command: job.command,
            status: format!("{:?}", job.status).to_lowercase(),
            elapsed_secs: job.started_at.elapsed().as_secs(),
            exit_code: job.exit_code,
            running: job.status == ShellStatus::Running,
            output: job.output,
        })
    }

    /// Request cancellation of a running job and wait briefly to see whether
    /// it actually reached a terminal state, so callers (model, TUI, RPC) get
    /// an honest answer instead of an unconditional "stopped" the moment the
    /// cancel flag is set (see issues/bash_background_issues.md #1).
    pub async fn stop(&self, id: &str) -> StopOutcome {
        let cancel = {
            let jobs = self.jobs.lock().await;
            let Some(job) = jobs.get(id) else {
                return StopOutcome::NotFound;
            };
            if job.status != ShellStatus::Running {
                return StopOutcome::NotFound;
            }
            job.cancel.clone()
        };
        cancel.cancel();
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(STOP_CONFIRM_TIMEOUT_MS);
        loop {
            let still_running = {
                let jobs = self.jobs.lock().await;
                jobs.get(id).map(|job| job.status == ShellStatus::Running)
            };
            match still_running {
                None | Some(false) => return StopOutcome::Confirmed,
                Some(true) => {}
            }
            if std::time::Instant::now() >= deadline {
                return StopOutcome::Requested;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }
}

/// Outcome of [`BackgroundShellManager::stop`] — distinguishes an observed
/// termination from a cancellation request that is still in flight, so
/// callers never claim a stop that hasn't actually happened yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    NotFound,
    /// The job was observed to leave `Running` within the confirm window.
    Confirmed,
    /// Cancellation was requested but the job was still running when the
    /// confirm window elapsed (e.g. a remote/kaos job that cannot be killed,
    /// or a slow-to-die process tree).
    Requested,
}

impl Default for BackgroundShellManager {
    fn default() -> Self {
        Self::new()
    }
}

pub struct BashTool {
    backgrounds: Arc<BackgroundShellManager>,
    options: BashOptions,
}

impl BashTool {
    pub fn new(backgrounds: Arc<BackgroundShellManager>, options: BashOptions) -> Self {
        Self {
            backgrounds,
            options,
        }
    }

    /// Build the effective sandbox policy. The toolchain overlay is baked in
    /// by `SandboxPolicy::from_app_config` at startup; profile changes take
    /// effect on the next session.
    fn effective_sandbox(&self) -> crate::sandbox::SandboxPolicy {
        self.options.sandbox.clone()
    }
}

impl Default for BashTool {
    fn default() -> Self {
        Self::new(
            Arc::new(BackgroundShellManager::new()),
            BashOptions::default(),
        )
    }
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "Bash"
    }
    fn description(&self) -> &str {
        "Execute a shell command. Supports cwd, description, timeout (seconds), \
run_in_background, and disable_timeout (background only). Prefer TaskOutput/TaskStop \
for background jobs (shell_id/stop remain as aliases)."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "Shell command to execute"},
                "cwd": {
                    "type": "string",
                    "description": "The working directory in which to run the command. When omitted, the command runs in the session's working directory."
                },
                "description": {"type": "string", "description": "Short description of what this command does"},
                "timeout": {
                    "type": "integer",
                    "description": format!(
                        "Timeout in seconds. Foreground default {DEFAULT_TIMEOUT_S}s (max {MAX_TIMEOUT_S}s); \
        background default {DEFAULT_BG_TIMEOUT_S}s (max {MAX_BG_TIMEOUT_S}s). Ignored when disable_timeout=true on background."
                    )
                },
                "run_in_background": {"type": "boolean", "description": "Start in background and return a shell_id / task id immediately"},
                "disable_timeout": {"type": "boolean", "description": "If true, do not apply a timeout. Only applies when run_in_background is true."},
                "shell_id": {"type": "string", "description": "Poll a previously started background shell (prefer TaskOutput)"},
                "stop": {"type": "boolean", "description": "With shell_id, stop the background process tree (prefer TaskStop)"},
                "since_offset": {"type": "integer", "description": "With shell_id, return only output written after this byte offset (from a previous poll's next_offset) instead of a tail snapshot — for incremental catch-up on long-running jobs."}
            }
        })
    }

    fn accesses(&self, input: &Value, working_dir: &Path) -> crate::ToolAccesses {
        crate::accesses::bash_accesses(input, working_dir)
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        if let Some(shell_id) = input.get("shell_id").and_then(|v| v.as_str()) {
            let has_command = input
                .get("command")
                .and_then(|v| v.as_str())
                .map(|c| !c.is_empty())
                .unwrap_or(false);
            if !has_command {
                if input.get("stop").and_then(Value::as_bool).unwrap_or(false) {
                    return Ok(match self.backgrounds.stop(shell_id).await {
                        StopOutcome::Confirmed => {
                            ToolOutput::success(format!("Stopped shell_id: {shell_id}"))
                        }
                        StopOutcome::Requested => ToolOutput::success(format!(
                            "Stop requested for shell_id: {shell_id}, but it had not \
                             terminated within {STOP_CONFIRM_TIMEOUT_MS}ms — poll again to confirm."
                        )),
                        StopOutcome::NotFound => ToolOutput::error(format!(
                            "Unknown or no longer running shell_id: {shell_id}"
                        )),
                    });
                }
                let since_offset = input.get("since_offset").and_then(Value::as_u64);
                return Ok(self.poll_shell(shell_id, since_offset).await);
            }
        }

        let command = match input.get("command").and_then(|v| v.as_str()) {
            Some(c) if !c.is_empty() => c.to_string(),
            _ => {
                return Ok(ToolOutput::error(
                    "Missing 'command' (or pass shell_id to poll)",
                ))
            }
        };

        let risk = crate::shell_safety::analyze_shell_command(&command);
        if let crate::shell_safety::ShellRisk::Dangerous(reason) = &risk {
            return Ok(ToolOutput::error(format!(
                "Blocked dangerous shell command ({reason}). Rephrase or ask the user for confirmation with a safer variant."
            )));
        }
        if let Some(reason) =
            crate::toolchain::deny_toolchain_mutation(&command, &self.options.toolchain)
        {
            return Ok(ToolOutput::error(reason));
        }
        let safety_note = crate::shell_safety::safety_prefix(&risk).unwrap_or_default();

        let effective_sandbox = self.effective_sandbox();

        let description = input
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let cwd = match resolve_cwd(
            input.get("cwd").and_then(|v| v.as_str()),
            &ctx.working_dir,
            &effective_sandbox,
        ) {
            Ok(cwd) => cwd,
            Err(error) => return Ok(ToolOutput::error(error.to_string())),
        };
        let run_in_background = input
            .get("run_in_background")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        tracing::info!(
            session_id = %ctx.session_id,
            cwd = %cwd.display(),
            run_in_background,
            "Executing Bash command"
        );
        let disable_timeout = input
            .get("disable_timeout")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
            && run_in_background;
        let timeout_ms = if disable_timeout {
            None
        } else {
            Some(resolve_timeout_ms(
                &input,
                run_in_background,
                self.options.default_timeout_s,
            ))
        };

        if run_in_background {
            if description.is_empty() {
                return Ok(ToolOutput::error(
                    "description is required when run_in_background=true",
                ));
            }
            let mut out = self
                .spawn_background(
                    ctx.session_id.clone(),
                    command,
                    description,
                    cwd,
                    ctx.working_dir.clone(),
                    timeout_ms,
                )
                .await?;
            if !safety_note.is_empty() {
                out.content = format!("{safety_note}{}", out.content);
            }
            return Ok(out);
        }

        let mut out = self
            .run_foreground(
                ctx.session_id.clone(),
                command,
                description,
                cwd,
                ctx.working_dir.clone(),
                timeout_ms.unwrap_or(DEFAULT_TIMEOUT_S * 1000),
                ctx.interrupted.clone(),
            )
            .await?;
        if !safety_note.is_empty() {
            out.content = format!("{safety_note}{}", out.content);
        }
        Ok(out)
    }
}

fn resolve_timeout_ms(input: &Value, run_in_background: bool, default_fg_timeout_s: u64) -> u64 {
    if let Some(ms) = input.get("timeout_ms").and_then(|v| v.as_u64()) {
        let cap = if run_in_background {
            MAX_BG_TIMEOUT_S * 1000
        } else {
            MAX_TIMEOUT_S * 1000
        };
        return ms.min(cap).max(1000);
    }
    let default_s = if run_in_background {
        DEFAULT_BG_TIMEOUT_S
    } else {
        default_fg_timeout_s
    };
    let cap_s = if run_in_background {
        MAX_BG_TIMEOUT_S
    } else {
        MAX_TIMEOUT_S
    };
    let secs = input
        .get("timeout")
        .and_then(|v| v.as_u64())
        .unwrap_or(default_s)
        .clamp(MIN_TIMEOUT_S, cap_s);
    secs.saturating_mul(1000)
}

impl BashTool {
    async fn poll_shell(&self, shell_id: &str, since_offset: Option<u64>) -> ToolOutput {
        match self.backgrounds.snapshot(shell_id).await {
            None => ToolOutput::error(format!("Unknown shell_id: {}", shell_id)),
            Some((description, _command, status, output, exit_code, running)) => {
                let mut out = format!(
                    "shell_id: {}\nstatus: {}\ndescription: {}",
                    shell_id, status, description
                );
                if let Some(code) = exit_code {
                    out.push_str(&format!("\nexit_code: {}", code));
                }
                if let Some(offset) = since_offset {
                    let (delta, next_offset) = match self.backgrounds.output_log(shell_id).await {
                        Some(log) => log.read_from(offset).await,
                        None => (String::new(), offset),
                    };
                    out.push_str(&format!(
                        "\nsince_offset: {offset}\nnext_offset: {next_offset}"
                    ));
                    if !delta.is_empty() {
                        out.push_str("\n\n");
                        // Tail, not head: `next_offset` already advances past
                        // the whole delta, so anything dropped here is gone
                        // from every later incremental poll. Keeping the tail
                        // matches the snapshot branch below (`tail_chars`).
                        out.push_str(&tail_chars(&delta, MAX_OUTPUT));
                    } else if running {
                        out.push_str(
                            "\n\n(no new output since offset — still running, call again later)",
                        );
                    }
                    return ToolOutput::success(out);
                }
                if !output.is_empty() {
                    out.push_str("\n\n");
                    out.push_str(&tail_chars(&output, MAX_OUTPUT));
                } else if running {
                    out.push_str("\n\n(still running — call TaskOutput/Bash again with this id)");
                }
                ToolOutput::success(out)
            }
        }
    }

    async fn spawn_background(
        &self,
        session_id: String,
        command: String,
        description: String,
        cwd: PathBuf,
        session_root: PathBuf,
        timeout_ms: Option<u64>,
    ) -> anyhow::Result<ToolOutput> {
        let id = Uuid::new_v4().to_string();
        let cancel = match self
            .backgrounds
            .insert_running(&id, &session_id, description.clone(), command.clone())
            .await
        {
            Ok(cancel) => cancel,
            Err(error) => return Ok(ToolOutput::error(error)),
        };

        let mgr = self.backgrounds.clone();
        let id_clone = id.clone();
        let sandbox = self.effective_sandbox().clone();
        let kaos = self.options.kaos.clone();
        let desc_for_kaos = description.clone();
        tokio::spawn(async move {
            if kaos.kind() == "ssh" {
                match run_via_kaos(
                    &kaos,
                    &command,
                    &cwd,
                    timeout_ms,
                    None,
                    Some(&cancel),
                    &desc_for_kaos,
                    &session_id,
                )
                .await
                {
                    Ok(out) => {
                        mgr.append_output(&id_clone, &out.content).await;
                        let status = if out.is_error {
                            ShellStatus::Failed
                        } else {
                            ShellStatus::Complete
                        };
                        mgr.finish(&id_clone, status, None).await;
                    }
                    Err(error) => {
                        mgr.append_output(&id_clone, &format!("Kaos failed: {error}"))
                            .await;
                        mgr.finish(&id_clone, ShellStatus::Failed, None).await;
                    }
                }
                return;
            }
            run_shell_job(
                mgr,
                id_clone,
                command,
                cwd,
                session_root,
                timeout_ms,
                cancel,
                sandbox,
            )
            .await;
        });
        Ok(ToolOutput::success(format!(
            "Background shell started: {description} (shell_id={id}). \
Also available as task_id={id} via TaskOutput/TaskStop. Its completion (status and \
output) will be delivered automatically as a <task-notification> — no polling needed."
        )))
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_foreground(
        &self,
        session_id: String,
        command: String,
        description: String,
        cwd: PathBuf,
        session_root: PathBuf,
        timeout_ms: u64,
        interrupted: Option<Arc<std::sync::atomic::AtomicBool>>,
    ) -> anyhow::Result<ToolOutput> {
        let effective_sandbox = self.effective_sandbox();
        if self.options.kaos.kind() == "ssh" {
            return run_via_kaos(
                &self.options.kaos,
                &command,
                &cwd,
                Some(timeout_ms),
                interrupted.as_ref(),
                None,
                &description,
                &session_id,
            )
            .await;
        }
        let (shell, flag) = shell_and_flag();
        let mut cmd =
            match effective_sandbox.command_for_session(shell, flag, &command, &cwd, &session_root)
            {
                Ok(command) => command,
                Err(error) => {
                    return Ok(ToolOutput::error(format!("Sandbox setup failed: {error}")))
                }
            };
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        cmd.kill_on_drop(true);
        cmd.env("TERM", "dumb");
        configure_process_group(&mut cmd);

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return Ok(ToolOutput::error(format!("Failed to spawn: {}", e))),
        };
        let sandbox_guard = match effective_sandbox.contain_child(&child) {
            Ok(guard) => guard,
            Err(error) => {
                terminate_process_tree(&mut child).await;
                return Ok(ToolOutput::error(format!(
                    "Sandbox containment failed: {error}"
                )));
            }
        };

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let collected = Arc::new(Mutex::new(String::new()));
        let collected_pump = collected.clone();
        let pump = tokio::spawn(async move {
            collect_stdio(stdout, stderr, collected_pump).await;
        });

        let timeout = tokio::time::Duration::from_millis(timeout_ms);
        let wait_result = tokio::select! {
            result = tokio::time::timeout(timeout, child.wait()) => Some(result),
            _ = wait_for_interrupt(interrupted) => None,
        };
        let Some(wait_result) = wait_result else {
            terminate_process_tree(&mut child).await;
            join_pump(pump).await;
            let output = collected.lock().await.clone();
            let mut result = "Command was interrupted and its process tree was killed.".to_string();
            if !output.is_empty() {
                result.push_str("\n\n");
                result.push_str(&truncate_chars(&output, MAX_OUTPUT));
            }
            return Ok(ToolOutput::error(result));
        };
        match wait_result {
            Ok(Ok(status)) => {
                join_pump(pump).await;
                let output = collected.lock().await.clone();
                let code = status.code().unwrap_or(-1);
                let mut result = String::new();
                if !description.is_empty() {
                    result.push_str(&format!("[{}]\n", description));
                }
                if output.is_empty() {
                    result.push_str("(no output)");
                } else {
                    result.push_str(&truncate_chars(&output, MAX_OUTPUT));
                }
                if code != 0 {
                    result.push_str(&format!("\nExit code: {}", code));
                    Ok(ToolOutput::error(result))
                } else {
                    Ok(ToolOutput::success(result))
                }
            }
            Ok(Err(e)) => {
                join_pump(pump).await;
                Ok(ToolOutput::error(format!("Failed to run command: {}", e)))
            }
            Err(_) => {
                if self.options.auto_background_on_timeout {
                    // Detach: keep the child running under BackgroundShellManager.
                    let id = Uuid::new_v4().to_string();
                    let desc = if description.is_empty() {
                        format!("timeout-detached: {}", truncate_chars(&command, 80))
                    } else {
                        description.clone()
                    };
                    let so_far = collected.lock().await.clone();
                    let so_far_len = so_far.len();
                    let cancel = match self
                        .backgrounds
                        .insert_running(&id, &session_id, desc.clone(), command.clone())
                        .await
                    {
                        Ok(cancel) => cancel,
                        Err(error) => {
                            terminate_process_tree(&mut child).await;
                            join_pump(pump).await;
                            return Ok(ToolOutput::error(error));
                        }
                    };
                    if !so_far.is_empty() {
                        self.backgrounds.append_output(&id, &so_far).await;
                    }
                    self.backgrounds
                        .append_output(
                            &id,
                            &format!(
                                "\n(foreground timed out after {}ms — detached to background)\n",
                                timeout_ms
                            ),
                        )
                        .await;

                    let mgr = self.backgrounds.clone();
                    let id_clone = id.clone();
                    // `collected` keeps receiving stdio from the still-running
                    // `pump` task; a periodic flush copies the delta into the
                    // manager so `/ps` output is live instead of a single
                    // snapshot at detach time (issues/bash_background_issues.md #2).
                    let flush_offset = Arc::new(std::sync::atomic::AtomicUsize::new(so_far_len));
                    // Graceful flusher shutdown: signal between flushes and
                    // wait for the handle, so the final flush below never
                    // overlaps an in-flight periodic flush (duplicate bytes).
                    let flush_shutdown = Arc::new(tokio::sync::Notify::new());
                    tokio::spawn(async move {
                        let _sandbox_guard = sandbox_guard;
                        let flusher = spawn_output_flusher(
                            collected.clone(),
                            mgr.clone(),
                            id_clone.clone(),
                            flush_offset.clone(),
                            flush_shutdown.clone(),
                        );
                        let (status, code) = drive_background_child(
                            mgr.clone(),
                            &id_clone,
                            child,
                            pump,
                            Some(MAX_BG_TIMEOUT_S * 1000),
                            cancel,
                        )
                        .await;
                        flush_shutdown.notify_one();
                        let _ = flusher.await;
                        flush_pending_output(&collected, &mgr, &id_clone, &flush_offset).await;
                        mgr.finish(&id_clone, status, code).await;
                    });

                    Ok(ToolOutput::success(format!(
                        "Command timed out after {timeout_ms}ms and was moved to the background.\n\
shell_id: {id}\ndescription: {desc}\n\
Its completion will be delivered automatically as a <task-notification> — no polling needed."
                    )))
                } else {
                    terminate_process_tree(&mut child).await;
                    join_pump(pump).await;
                    let output = collected.lock().await.clone();
                    let mut result =
                        format!("Command timed out after {}ms and was killed.", timeout_ms);
                    if !output.is_empty() {
                        result.push_str("\n\n");
                        result.push_str(&truncate_chars(&output, MAX_OUTPUT));
                    }
                    Ok(ToolOutput::error(result))
                }
            }
        }
    }
}

/// Copy whatever `collected` has accumulated past `offset` into the manager
/// and advance `offset`, used both by the periodic flusher and for the final
/// catch-up flush once the child has exited.
async fn flush_pending_output(
    collected: &Arc<Mutex<String>>,
    mgr: &Arc<BackgroundShellManager>,
    id: &str,
    offset: &Arc<std::sync::atomic::AtomicUsize>,
) {
    let snapshot = collected.lock().await.clone();
    let prev = offset.load(std::sync::atomic::Ordering::SeqCst);
    if snapshot.len() > prev {
        mgr.append_output(id, &snapshot[prev..]).await;
        offset.store(snapshot.len(), std::sync::atomic::Ordering::SeqCst);
    }
}

/// Periodically flushes `collected` into the manager while a
/// foreground-timeout-detached child is still running (see
/// [`flush_pending_output`]). Exits between flushes when `shutdown` fires;
/// the caller awaits the handle before its own final flush, so the two can
/// never interleave inside [`flush_pending_output`] and double-append the
/// same bytes (a plain `abort()` could land exactly there).
fn spawn_output_flusher(
    collected: Arc<Mutex<String>>,
    mgr: Arc<BackgroundShellManager>,
    id: String,
    offset: Arc<std::sync::atomic::AtomicUsize>,
    shutdown: Arc<tokio::sync::Notify>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = tokio::time::sleep(tokio::time::Duration::from_millis(150)) => {}
                // `notify_one`, not `notify_waiters`: a signal sent while a
                // flush is in flight (no waiter registered) must not be lost.
                _ = shutdown.notified() => break,
            }
            flush_pending_output(&collected, &mgr, &id, &offset).await;
        }
    })
}

/// Shared tail for every background/detached child: wait for exit (bounded by
/// `timeout_ms` when set) or cooperative cancellation, kill the process tree
/// on timeout/cancel, and join the stdio pump. Does not call
/// [`BackgroundShellManager::finish`] — callers that need a final output
/// flush before the completion notification is computed (the
/// foreground-timeout-detach path) must do so between this call and calling
/// `finish` themselves.
async fn drive_background_child(
    mgr: Arc<BackgroundShellManager>,
    id: &str,
    mut child: Child,
    pump: tokio::task::JoinHandle<()>,
    timeout_ms: Option<u64>,
    cancel: Arc<CancelToken>,
) -> (ShellStatus, Option<i32>) {
    let waited = match timeout_ms {
        Some(ms) => {
            tokio::select! {
                result = tokio::time::timeout(tokio::time::Duration::from_millis(ms), child.wait()) => Some(result),
                _ = cancel.cancelled() => None,
            }
        }
        None => {
            tokio::select! {
                result = child.wait() => Some(Ok(result)),
                _ = cancel.cancelled() => None,
            }
        }
    };
    let status = match waited {
        Some(Ok(Ok(status))) => {
            let code = status.code();
            let st = if status.success() {
                ShellStatus::Complete
            } else {
                ShellStatus::Failed
            };
            (st, code)
        }
        Some(Ok(Err(e))) => {
            mgr.append_output(id, &format!("\nwait error: {}", e)).await;
            (ShellStatus::Failed, None)
        }
        Some(Err(_)) => {
            terminate_process_tree(&mut child).await;
            mgr.append_output(
                id,
                &format!(
                    "\n(timed out after {}ms and killed)",
                    timeout_ms.unwrap_or_default()
                ),
            )
            .await;
            (ShellStatus::TimedOut, None)
        }
        None => {
            terminate_process_tree(&mut child).await;
            mgr.append_output(id, "\n(cancelled; process tree killed)")
                .await;
            (ShellStatus::Cancelled, None)
        }
    };
    join_pump(pump).await;
    status
}

#[allow(clippy::too_many_arguments)]
async fn run_shell_job(
    mgr: Arc<BackgroundShellManager>,
    id: String,
    command: String,
    cwd: PathBuf,
    session_root: PathBuf,
    timeout_ms: Option<u64>,
    cancel: Arc<CancelToken>,
    sandbox: crate::sandbox::SandboxPolicy,
) {
    let (shell, flag) = shell_and_flag();
    let mut cmd = match sandbox.command_for_session(shell, flag, &command, &cwd, &session_root) {
        Ok(command) => command,
        Err(error) => {
            mgr.append_output(&id, &format!("Sandbox setup failed: {error}"))
                .await;
            mgr.finish(&id, ShellStatus::Failed, None).await;
            return;
        }
    };
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.kill_on_drop(true);
    cmd.env("TERM", "dumb");
    configure_process_group(&mut cmd);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            mgr.append_output(&id, &format!("Failed to spawn: {}", e))
                .await;
            mgr.finish(&id, ShellStatus::Failed, None).await;
            return;
        }
    };
    let _sandbox_guard = match sandbox.contain_child(&child) {
        Ok(guard) => guard,
        Err(error) => {
            terminate_process_tree(&mut child).await;
            mgr.append_output(&id, &format!("Sandbox containment failed: {error}"))
                .await;
            mgr.finish(&id, ShellStatus::Failed, None).await;
            return;
        }
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let mgr_out = mgr.clone();
    let id_out = id.clone();
    let pump = tokio::spawn(async move {
        pump_stdio_to_mgr(stdout, stderr, mgr_out, id_out).await;
    });

    let (status, code) =
        drive_background_child(mgr.clone(), &id, child, pump, timeout_ms, cancel).await;
    mgr.finish(&id, status, code).await;
}

async fn collect_stdio(
    stdout: Option<impl AsyncReadExt + Unpin>,
    stderr: Option<impl AsyncReadExt + Unpin>,
    collected: Arc<Mutex<String>>,
) {
    let stdout_collected = collected.clone();
    let stdout_task = async move {
        if let Some(mut out) = stdout {
            let mut buf = [0u8; 4096];
            loop {
                match out.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        let s = String::from_utf8_lossy(&buf[..n]);
                        let mut output = stdout_collected.lock().await;
                        if output.len() < MAX_OUTPUT * 2 {
                            output.push_str(&s);
                        }
                    }
                    Err(_) => break,
                }
            }
        }
    };
    let stderr_task = async move {
        let Some(mut err) = stderr else {
            return;
        };
        let mut buf = [0u8; 4096];
        let mut first = true;
        loop {
            match err.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    let mut output = collected.lock().await;
                    if first {
                        output.push_str("\nSTDERR:\n");
                        first = false;
                    }
                    if output.len() < MAX_OUTPUT * 2 {
                        output.push_str(&String::from_utf8_lossy(&buf[..n]));
                    }
                }
                Err(_) => break,
            }
        }
    };
    tokio::join!(stdout_task, stderr_task);
}

async fn pump_stdio_to_mgr(
    stdout: Option<impl AsyncReadExt + Unpin>,
    stderr: Option<impl AsyncReadExt + Unpin>,
    mgr: Arc<BackgroundShellManager>,
    id: String,
) {
    let stdout_mgr = mgr.clone();
    let stdout_id = id.clone();
    let stdout_task = async move {
        if let Some(mut out) = stdout {
            let mut buf = [0u8; 4096];
            loop {
                match out.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        stdout_mgr
                            .append_output(&stdout_id, &String::from_utf8_lossy(&buf[..n]))
                            .await;
                    }
                    Err(_) => break,
                }
            }
        }
    };
    let stderr_task = async move {
        let Some(mut err) = stderr else {
            return;
        };
        let mut buf = [0u8; 4096];
        let mut first = true;
        loop {
            match err.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    if first {
                        mgr.append_output(&id, "\nSTDERR:\n").await;
                        first = false;
                    }
                    mgr.append_output(&id, &String::from_utf8_lossy(&buf[..n]))
                        .await;
                }
                Err(_) => break,
            }
        }
    };
    tokio::join!(stdout_task, stderr_task);
}

async fn wait_for_interrupt(flag: Option<Arc<std::sync::atomic::AtomicBool>>) {
    let Some(flag) = flag else {
        std::future::pending::<()>().await;
        return;
    };
    loop {
        if flag.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    }
}

fn configure_process_group(command: &mut Command) {
    #[cfg(unix)]
    command.process_group(0);
}

async fn terminate_process_tree(child: &mut Child) {
    let pid = child.id();
    #[cfg(unix)]
    if let Some(pid) = pid {
        // The child is placed in its own process group before spawn.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    #[cfg(windows)]
    if let Some(pid) = pid {
        let _ = tokio::time::timeout(
            tokio::time::Duration::from_secs(5),
            Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .status(),
        )
        .await;
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
}

async fn join_pump(mut pump: tokio::task::JoinHandle<()>) {
    if tokio::time::timeout(tokio::time::Duration::from_secs(2), &mut pump)
        .await
        .is_err()
    {
        pump.abort();
        let _ = pump.await;
    }
}

fn shell_and_flag() -> (&'static str, &'static str) {
    if cfg!(target_os = "windows") {
        ("cmd", "/C")
    } else {
        ("bash", "-c")
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_via_kaos(
    kaos: &kkagent_kaos::KaosHandle,
    command: &str,
    cwd: &Path,
    timeout_ms: Option<u64>,
    interrupted: Option<&Arc<std::sync::atomic::AtomicBool>>,
    cancel: Option<&Arc<CancelToken>>,
    description: &str,
    session_id: &str,
) -> anyhow::Result<ToolOutput> {
    tracing::info!(
        %session_id,
        kaos = kaos.kind(),
        cwd = %cwd.display(),
        "Executing Bash via Kaos"
    );
    enum Outcome {
        Done(Result<kkagent_kaos::ExecResult, kkagent_kaos::KaosError>),
        TimedOut,
        Cancelled,
    }
    let exec = kaos.exec(command, Some(cwd));
    let outcome = match (timeout_ms, cancel) {
        (Some(ms), Some(cancel)) => {
            tokio::select! {
                r = tokio::time::timeout(std::time::Duration::from_millis(ms), exec) => match r {
                    Ok(r) => Outcome::Done(r),
                    Err(_) => Outcome::TimedOut,
                },
                _ = cancel.cancelled() => Outcome::Cancelled,
            }
        }
        (Some(ms), None) => {
            match tokio::time::timeout(std::time::Duration::from_millis(ms), exec).await {
                Ok(r) => Outcome::Done(r),
                Err(_) => Outcome::TimedOut,
            }
        }
        (None, Some(cancel)) => {
            tokio::select! {
                r = exec => Outcome::Done(r),
                _ = cancel.cancelled() => Outcome::Cancelled,
            }
        }
        (None, None) => Outcome::Done(exec.await),
    };
    let result = match outcome {
        Outcome::TimedOut => {
            return Ok(ToolOutput::error(format!(
                "Remote command timed out after {}ms ({description})",
                timeout_ms.unwrap_or_default()
            )));
        }
        Outcome::Cancelled => {
            // kaos/SSH exec has no remote kill: cancellation only stops us
            // from waiting on it — the remote process may still be running.
            // Honest about that instead of claiming a confirmed stop
            // (issues/bash_background_issues.md #1 point 4).
            return Ok(ToolOutput::error(
                "Cancelled: stopped waiting for the remote command. It may still be \
                 running on the remote host — kaos/SSH exec cannot be killed remotely.",
            ));
        }
        Outcome::Done(result) => result,
    };
    if interrupted.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::SeqCst)) {
        return Ok(ToolOutput::error("Interrupted"));
    }
    match result {
        Ok(out) => {
            let mut body = String::new();
            if !out.stdout.is_empty() {
                body.push_str(&out.stdout);
            }
            if !out.stderr.is_empty() {
                if !body.is_empty() {
                    body.push('\n');
                }
                body.push_str(&out.stderr);
            }
            if let Some(remote_cwd) = out.cwd {
                body.push_str(&format!("\n[kaos cwd: {}]", remote_cwd.display()));
            }
            body.push_str(&format!("\n[kaos exit: {}]", out.status));
            if out.status == 0 {
                Ok(ToolOutput::success(truncate_chars(&body, MAX_OUTPUT)))
            } else {
                Ok(ToolOutput::error(truncate_chars(&body, MAX_OUTPUT)))
            }
        }
        Err(error) => Ok(ToolOutput::error(format!("Kaos exec failed: {error}"))),
    }
}

fn resolve_cwd(
    cwd: Option<&str>,
    session_cwd: &Path,
    sandbox: &crate::sandbox::SandboxPolicy,
) -> anyhow::Result<PathBuf> {
    let requested = match cwd {
        Some(c) if !c.is_empty() => {
            let p = PathBuf::from(c);
            if p.is_absolute() {
                p
            } else {
                session_cwd.join(p)
            }
        }
        _ => session_cwd.to_path_buf(),
    };
    let (resolved, _) = sandbox.resolve_workspace_paths(&requested, session_cwd)?;
    Ok(resolved)
}

fn truncate_chars(s: &str, max: usize) -> String {
    let truncated: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        format!(
            "{}\n... truncated ({} chars total)",
            truncated,
            s.chars().count()
        )
    } else {
        truncated
    }
}

/// Keep the trailing `max` characters of `s`, prefixing a marker when
/// something was dropped. Mirrors `task_notify::excerpt`'s tail semantics so
/// polling and push notifications show the same end of a long-running job's
/// output instead of disagreeing on which end matters
/// (issues/bash_background_issues.md #6).
fn tail_chars(s: &str, max: usize) -> String {
    let total = s.chars().count();
    if total <= max {
        return s.to_string();
    }
    let start = total - max;
    let tail: String = s.chars().skip(start).collect();
    format!("... earlier output omitted ({total} chars total) ...\n{tail}")
}

fn drop_utf8_bytes_from_front_in_place(value: &mut String, drop_bytes: usize) {
    if drop_bytes == 0 {
        return;
    }
    if drop_bytes >= value.len() {
        value.clear();
        return;
    }
    let mut boundary = drop_bytes;
    while boundary < value.len() && !value.is_char_boundary(boundary) {
        boundary += 1;
    }
    value.drain(..boundary);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(interrupted: Option<Arc<std::sync::atomic::AtomicBool>>) -> ToolContext {
        ToolContext {
            working_dir: std::env::current_dir().expect("current directory"),
            session_id: "bash-test".to_string(),
            turn_id: "test-turn".into(),
            plan_file_path: None,
            image: kkagent_config::ImageConfig::default(),
            tool_call_id: None,
            interrupted,
            tools_config: kkagent_config::ToolsConfig::default(),
            model_alias: None,
        }
    }

    /// Redirect background-shell output logs to a throwaway tempdir so tests
    /// never write into the user's real config dir. Shared by the whole
    /// module (one `OnceLock` tempdir): the override is process-global, and
    /// per-test roots would stomp each other across concurrently running
    /// tests. The tempdir is not auto-removed at process exit (statics don't
    /// run `Drop`), which leaves at most one identifiable scratch dir under
    /// the OS temp dir.
    fn redirect_bg_output_logs() {
        static LOG_ROOT: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
        let dir = LOG_ROOT.get_or_init(|| {
            tempfile::Builder::new()
                .prefix("kkagent-test-bg-logs-")
                .tempdir()
                .expect("tempdir for bg output logs")
        });
        crate::output_log::set_root_for_tests(Some(std::sync::Arc::from(dir.path())));
    }

    #[tokio::test]
    async fn limits_concurrent_background_jobs() {
        let manager = BackgroundShellManager::new();
        // Spread across sessions (fewer than MAX_RUNNING_JOBS_PER_SESSION each)
        // so the per-session cap doesn't shadow the global cap under test.
        for index in 0..MAX_RUNNING_JOBS {
            let session = format!("session-{}", index % (MAX_RUNNING_JOBS_PER_SESSION - 1));
            manager
                .insert_running(
                    &format!("job-{index}"),
                    &session,
                    "test".into(),
                    "test".into(),
                )
                .await
                .expect("job within global limit");
        }
        let error = manager
            .insert_running("overflow", "session-overflow", "test".into(), "test".into())
            .await
            .expect_err("job over the global limit must fail");
        assert!(error.contains("limit reached"));
    }

    #[tokio::test]
    async fn limits_concurrent_background_jobs_per_session() {
        let manager = BackgroundShellManager::new();
        for index in 0..MAX_RUNNING_JOBS_PER_SESSION {
            manager
                .insert_running(
                    &format!("job-{index}"),
                    "bash-test",
                    "test".into(),
                    "test".into(),
                )
                .await
                .expect("job within per-session limit");
        }
        let error = manager
            .insert_running("overflow", "bash-test", "test".into(), "test".into())
            .await
            .expect_err("job over the per-session limit must fail");
        assert!(error.contains("limit reached for this session"));
    }

    #[test]
    fn drops_utf8_only_at_character_boundaries_from_the_front() {
        let repeated = "测试内容".repeat(MAX_OUTPUT);
        let mut value = repeated.clone();
        let original_len = value.len();
        drop_utf8_bytes_from_front_in_place(&mut value, MAX_OUTPUT);
        assert!(value.len() < original_len);
        assert!(std::str::from_utf8(value.as_bytes()).is_ok());
        // Dropped from the front: what remains must be a suffix of the original.
        assert!(repeated.ends_with(&value));
    }

    #[test]
    fn cwd_defaults_to_the_session_and_schema_does_not_invite_absolute_paths() {
        let root = PathBuf::from("workspace-root");
        let sandbox = crate::sandbox::SandboxPolicy::default();
        assert_eq!(resolve_cwd(None, &root, &sandbox).unwrap(), root.clone());
        assert_eq!(
            resolve_cwd(Some("crates/core"), &root, &sandbox).unwrap(),
            root.join("crates/core")
        );

        let schema = BashTool::default().parameters_schema();
        let description = schema["properties"]["cwd"]["description"].as_str().unwrap();
        assert!(description.contains("session's working directory"));
        assert!(!description.contains("absolute or relative"));
    }

    #[test]
    fn workspace_mode_rejects_cwd_outside_session_root() {
        let session = std::env::temp_dir().join(format!("kkagent-cwd-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&session).unwrap();
        let sandbox = crate::sandbox::SandboxPolicy::from_config(&kkagent_config::SandboxConfig {
            mode: "workspace".into(),
            ..Default::default()
        })
        .unwrap();
        let outside = std::env::temp_dir();
        let err = resolve_cwd(Some(outside.to_str().unwrap()), &session, &sandbox).unwrap_err();
        assert!(
            err.to_string()
                .contains("escapes the sandbox writable root"),
            "{err}"
        );
        std::fs::remove_dir_all(&session).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn foreground_interrupt_kills_the_process_tree() {
        let interrupted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let trigger = interrupted.clone();
        tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;
            trigger.store(true, std::sync::atomic::Ordering::SeqCst);
        });

        let result = tokio::time::timeout(
            tokio::time::Duration::from_secs(4),
            BashTool::default().execute(
                json!({"command": "sleep 30", "timeout_ms": 30_000}),
                &context(Some(interrupted)),
            ),
        )
        .await
        .expect("interrupt must terminate promptly")
        .expect("tool execution");

        assert!(result.is_error);
        assert!(result.content.contains("interrupted"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn drains_stdout_and_stderr_concurrently() {
        let result = tokio::time::timeout(
            tokio::time::Duration::from_secs(8),
            BashTool::default().execute(
                json!({
                    "command": "i=0; while [ $i -lt 12000 ]; do echo out; echo err >&2; i=$((i+1)); done",
                    "timeout_ms": 7_000
                }),
                &context(None),
            ),
        )
        .await
        .expect("full pipes must not deadlock")
        .expect("tool execution");

        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("STDERR"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn background_job_can_be_stopped_and_polled() {
        redirect_bg_output_logs();
        let tool = BashTool::default();
        let started = tool
            .execute(
                json!({
                    "command": "sleep 30",
                    "description": "cancellable test",
                    "run_in_background": true,
                    "timeout_ms": 30_000
                }),
                &context(None),
            )
            .await
            .expect("start background job");
        let id = started
            .content
            .split("shell_id=")
            .nth(1)
            .and_then(|tail| tail.split([')', ' ', '/', '.']).next())
            .expect("shell id in start response");
        assert!(
            !id.contains("task_id"),
            "parsed shell id should be bare uuid, got {id:?} from {}",
            started.content
        );

        let stopped = tool
            .execute(json!({"shell_id": id, "stop": true}), &context(None))
            .await
            .expect("request stop");
        assert!(!stopped.is_error, "{}", stopped.content);

        let final_output = tokio::time::timeout(tokio::time::Duration::from_secs(4), async {
            loop {
                let polled = tool
                    .execute(json!({"shell_id": id}), &context(None))
                    .await
                    .expect("poll background job");
                if polled.content.contains("status: cancelled") {
                    break polled;
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("background cancellation must finish promptly");
        assert!(final_output.content.contains("process tree killed"));
    }

    #[test]
    fn resolve_timeout_ms_clamps_zero_and_undersized_values() {
        // timeout_ms = 0 must not translate into an instant kill.
        assert_eq!(
            resolve_timeout_ms(&json!({"timeout_ms": 0}), false, 120),
            1000
        );
        // timeout (seconds) = 0 falls back to clamping at the 1s floor too.
        assert_eq!(resolve_timeout_ms(&json!({"timeout": 0}), false, 120), 1000);
        // A zero default (misconfigured bash_task_timeout_s) still floors at 1s.
        assert_eq!(resolve_timeout_ms(&json!({}), false, 0), 1000);
        // Normal values keep flowing through untouched.
        assert_eq!(resolve_timeout_ms(&json!({}), false, 120), 120_000);
        assert_eq!(
            resolve_timeout_ms(&json!({"timeout": 5}), false, 120),
            5_000
        );
        // Foreground caps at MAX_TIMEOUT_S.
        assert_eq!(
            resolve_timeout_ms(&json!({"timeout": 9_999}), false, 120),
            300_000
        );
    }

    fn context_for_session(session_id: &str) -> ToolContext {
        ToolContext {
            working_dir: std::env::current_dir().expect("current directory"),
            session_id: session_id.to_string(),
            turn_id: "test-turn".into(),
            plan_file_path: None,
            image: kkagent_config::ImageConfig::default(),
            tool_call_id: None,
            interrupted: None,
            tools_config: kkagent_config::ToolsConfig::default(),
            model_alias: None,
        }
    }

    async fn start_sleep_job(tool: &BashTool, session_id: &str, description: &str) -> String {
        let started = tool
            .execute(
                json!({
                    "command": "sleep 30",
                    "description": description,
                    "run_in_background": true,
                    "timeout_ms": 30_000
                }),
                &context_for_session(session_id),
            )
            .await
            .expect("start background job");
        started
            .content
            .split("shell_id=")
            .nth(1)
            .and_then(|tail| tail.split([')', ' ', '/', '.']).next())
            .unwrap_or_default()
            .to_string()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn list_running_for_session_filters_by_session_and_status() {
        redirect_bg_output_logs();
        let tool = BashTool::default();
        let first = start_sleep_job(&tool, "session-1", "first").await;
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        let second = start_sleep_job(&tool, "session-1", "second").await;
        let _other = start_sleep_job(&tool, "session-2", "other").await;
        assert!(!first.is_empty() && !second.is_empty());

        let jobs = tool.backgrounds.list_running_for_session("session-1").await;
        assert_eq!(jobs.len(), 2, "only this session's running jobs");
        assert_eq!(jobs[0].id, first, "oldest job first");
        assert_eq!(jobs[1].id, second);
        assert_eq!(jobs[0].description, "first");
        assert_eq!(jobs[0].command, "sleep 30");
        assert_eq!(jobs[0].elapsed_secs, 0, "just started");

        assert!(
            tool.backgrounds
                .list_running_for_session("session-unknown")
                .await
                .is_empty(),
            "unknown session has no jobs"
        );

        // Stop both jobs and wait for the poller to settle the status.
        for id in [&first, &second, &_other] {
            tool.backgrounds.stop(id).await;
        }
        tokio::time::timeout(std::time::Duration::from_secs(4), async {
            loop {
                let remaining = tool.backgrounds.list_running_for_session("session-1").await;
                if remaining.is_empty() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("stopped jobs must leave the running list");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn snapshot_detail_reports_status_output_and_elapsed() {
        redirect_bg_output_logs();
        let tool = BashTool::default();
        let id = start_sleep_job(&tool, "session-1", "watch").await;
        assert!(!id.is_empty());

        let detail = tool
            .backgrounds
            .snapshot_detail(&id)
            .await
            .expect("detail exists");
        assert_eq!(detail.id, id);
        assert!(detail.running);
        assert_eq!(detail.status, "running");
        assert_eq!(detail.command, "sleep 30");

        tool.backgrounds.stop(&id).await;
        tokio::time::timeout(std::time::Duration::from_secs(4), async {
            loop {
                let detail = tool
                    .backgrounds
                    .snapshot_detail(&id)
                    .await
                    .expect("detail exists");
                if !detail.running {
                    assert_eq!(detail.status, "cancelled");
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("stopped job must settle to cancelled");

        assert!(tool.backgrounds.snapshot_detail("missing").await.is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn disabled_timeout_background_job_can_still_be_cancelled() {
        redirect_bg_output_logs();
        let tool = BashTool::default();
        let started = tool
            .execute(
                json!({
                    "command": "sleep 30",
                    "description": "disable_timeout cancellation",
                    "run_in_background": true,
                    "disable_timeout": true
                }),
                &context(None),
            )
            .await
            .expect("start background job");
        let id = started
            .content
            .split("shell_id=")
            .nth(1)
            .and_then(|tail| tail.split([')', ' ', '/', '.']).next())
            .expect("shell id in start response")
            .to_string();

        tool.backgrounds.stop(&id).await;

        tokio::time::timeout(std::time::Duration::from_secs(4), async {
            loop {
                let detail = tool
                    .backgrounds
                    .snapshot_detail(&id)
                    .await
                    .expect("detail exists");
                if !detail.running {
                    assert_eq!(detail.status, "cancelled");
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("disable_timeout=true job must still converge to cancelled after stop");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn detached_job_reports_incremental_output_before_it_finishes() {
        redirect_bg_output_logs();
        let tool = BashTool::default();
        let started = tool
            .execute(
                json!({
                    "command": "for i in 1 2 3 4 5; do echo line-$i; sleep 0.2; done",
                    "description": "incremental output during detach",
                    "timeout_ms": 300
                }),
                &context(None),
            )
            .await
            .expect("foreground command detaches after timeout");
        let id = started
            .content
            .split("shell_id: ")
            .nth(1)
            .and_then(|tail| tail.split(['\n', ' ']).next())
            .expect("detached shell id in timeout response")
            .to_string();

        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let detail = tool
                    .backgrounds
                    .snapshot_detail(&id)
                    .await
                    .expect("detail exists");
                let lines = detail.output.matches("line-").count();
                if detail.running && lines >= 2 {
                    break;
                }
                assert!(
                    detail.running,
                    "job finished before the flusher proved incremental delivery"
                );
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("detached job must expose at least 2 lines of output before it finishes");

        tool.backgrounds.stop(&id).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn append_output_keeps_accepting_data_past_the_in_memory_cap() {
        redirect_bg_output_logs();
        let manager = BackgroundShellManager::new();
        let id = "overflow-job";
        manager
            .insert_running(id, "session-1", "overflow test".into(), "test".into())
            .await
            .expect("insert running job");

        // Push well past MAX_OUTPUT * 2 so the old implementation would have
        // frozen and silently dropped every chunk after the first ~100KB.
        let chunk = "x".repeat(1024);
        for _ in 0..(MAX_OUTPUT * 2 / 1024 + 8) {
            manager.append_output(id, &chunk).await;
        }
        manager.append_output(id, "LATEST-MARKER\n").await;

        let (_, _, _, output, _, _) = manager.snapshot(id).await.expect("job exists");
        assert!(
            output.contains("LATEST-MARKER"),
            "rolling tail buffer must keep accepting new output past the cap"
        );
        assert!(
            output.len() <= MAX_OUTPUT * 2 + TRUNCATION_MARKER.len(),
            "in-memory buffer must stay bounded, got {} bytes",
            output.len()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn in_memory_window_keeps_a_single_truncation_marker_across_refills() {
        redirect_bg_output_logs();
        let manager = BackgroundShellManager::new();
        let id = "marker-stack-job";
        manager
            .insert_running(id, "session-1", "marker test".into(), "test".into())
            .await
            .expect("insert running job");

        // Two full refill rounds: after each overflow the window must carry
        // exactly one marker (no partial remnants stacking under a fresh one).
        for _ in 0..2 {
            let chunk = "y".repeat(1024);
            for _ in 0..(MAX_OUTPUT * 2 / 1024 + 8) {
                manager.append_output(id, &chunk).await;
            }
            let (_, _, _, output, _, _) = manager.snapshot(id).await.expect("job exists");
            assert!(
                output.starts_with(TRUNCATION_MARKER),
                "window must start with the truncation marker after overflow"
            );
            assert_eq!(
                output.matches(TRUNCATION_MARKER).count(),
                1,
                "exactly one marker expected, got: {:?}",
                &output[..200.min(output.len())]
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn since_offset_polling_returns_only_new_output() {
        redirect_bg_output_logs();
        let tool = BashTool::default();
        let started = tool
            .execute(
                json!({
                    "command": "echo first; sleep 0.3; echo second; sleep 5",
                    "description": "since_offset incremental poll",
                    "run_in_background": true,
                    "timeout_ms": 30_000
                }),
                &context(None),
            )
            .await
            .expect("start background job");
        let id = started
            .content
            .split("shell_id=")
            .nth(1)
            .and_then(|tail| tail.split([')', ' ', '/', '.']).next())
            .expect("shell id in start response")
            .to_string();

        // Wait for "first" to land, then poll from offset 0 to learn the
        // current next_offset (baseline for the incremental poll below).
        let baseline = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let polled = tool
                    .execute(json!({"shell_id": id, "since_offset": 0}), &context(None))
                    .await
                    .expect("poll from offset 0");
                if polled.content.contains("first") {
                    break polled;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("first line must appear promptly");
        assert!(!baseline.content.contains("second"));
        let next_offset: u64 = baseline
            .content
            .split("next_offset: ")
            .nth(1)
            .and_then(|tail| tail.split('\n').next())
            .and_then(|n| n.trim().parse().ok())
            .expect("next_offset in poll response");

        // Poll again from that offset once "second" has been printed: the
        // response must contain only the new line, not "first" again.
        let incremental = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let polled = tool
                    .execute(
                        json!({"shell_id": id, "since_offset": next_offset}),
                        &context(None),
                    )
                    .await
                    .expect("incremental poll");
                if polled.content.contains("second") {
                    break polled;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("second line must appear promptly");
        assert!(!incremental.content.contains("first"));

        tool.backgrounds.stop(&id).await;
    }
}
