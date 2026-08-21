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

#[derive(Debug, Clone, PartialEq)]
enum ShellStatus {
    Running,
    Complete,
    Failed,
    TimedOut,
    Cancelled,
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
    cancel: Arc<std::sync::atomic::AtomicBool>,
}

/// Compact info about a running background shell (for /ps panel lists).
#[derive(Debug, Clone)]
pub struct BackgroundJobInfo {
    pub id: String,
    pub description: String,
    pub command: String,
    pub elapsed_secs: u64,
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
    ) -> Result<Arc<std::sync::atomic::AtomicBool>, String> {
        let mut jobs = self.jobs.lock().await;
        if jobs
            .values()
            .filter(|job| job.status == ShellStatus::Running)
            .count()
            >= MAX_RUNNING_JOBS
        {
            return Err(format!(
                "background shell limit reached ({MAX_RUNNING_JOBS} running jobs)"
            ));
        }
        if jobs.len() >= MAX_BACKGROUND_JOBS {
            jobs.retain(|_, job| job.status == ShellStatus::Running);
        }
        if jobs.len() >= MAX_BACKGROUND_JOBS {
            return Err(format!(
                "background shell history limit reached ({MAX_BACKGROUND_JOBS} jobs)"
            ));
        }
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
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
            },
        );
        Ok(cancel)
    }

    /// Cooperative cancel for every running job belonging to `session_id`.
    pub async fn cancel_session(&self, session_id: &str) {
        let jobs = self.jobs.lock().await;
        for job in jobs.values() {
            if job.session_id == session_id && job.status == ShellStatus::Running {
                job.cancel.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
    }

    async fn append_output(&self, id: &str, chunk: &str) {
        if let Some(job) = self.jobs.lock().await.get_mut(id) {
            if job.output.len() < MAX_OUTPUT * 2 {
                job.output.push_str(chunk);
                if job.output.len() > MAX_OUTPUT * 2 {
                    truncate_utf8_bytes_in_place(&mut job.output, MAX_OUTPUT * 2);
                    job.output.push_str("\n... output truncated ...");
                }
            }
        }
    }

    async fn finish(&self, id: &str, status: ShellStatus, exit_code: Option<i32>) {
        if let Some(job) = self.jobs.lock().await.get_mut(id) {
            job.status = status;
            job.exit_code = exit_code;
        }
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
        let mut jobs: Vec<(std::time::Instant, BackgroundJobInfo)> = self
            .jobs
            .lock()
            .await
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
                    },
                )
            })
            .collect();
        // Sort by Instant so sub-second starts stay stable (elapsed_secs is too coarse).
        jobs.sort_by_key(|(started_at, _)| *started_at);
        jobs.into_iter().map(|(_, info)| info).collect()
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

    pub async fn stop(&self, id: &str) -> bool {
        let jobs = self.jobs.lock().await;
        let Some(job) = jobs.get(id) else {
            return false;
        };
        if job.status != ShellStatus::Running {
            return false;
        }
        job.cancel.store(true, std::sync::atomic::Ordering::SeqCst);
        true
    }
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
                "stop": {"type": "boolean", "description": "With shell_id, stop the background process tree (prefer TaskStop)"}
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
                    return Ok(if self.backgrounds.stop(shell_id).await {
                        ToolOutput::success(format!("Stop requested for shell_id: {shell_id}"))
                    } else {
                        ToolOutput::error(format!(
                            "Unknown or no longer running shell_id: {shell_id}"
                        ))
                    });
                }
                return Ok(self.poll_shell(shell_id).await);
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
            // Also emit a delivery hint so the model can collect via Task*.
            out = out.with_delivery(
                "<system>Background bash started. Use TaskOutput/TaskStop with the shell_id when ready.</system>",
            );
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
    async fn poll_shell(&self, shell_id: &str) -> ToolOutput {
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
                if !output.is_empty() {
                    out.push_str("\n\n");
                    out.push_str(&truncate_chars(&output, MAX_OUTPUT));
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
Also available as task_id={id} via TaskOutput/TaskStop."
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
                    tokio::spawn(async move {
                        let _sandbox_guard = sandbox_guard;
                        let waited = tokio::select! {
                            result = tokio::time::timeout(
                                tokio::time::Duration::from_millis(MAX_BG_TIMEOUT_S * 1000),
                                child.wait(),
                            ) => Some(result),
                            _ = wait_for_interrupt(Some(cancel)) => None,
                        };
                        let status = match waited {
                            Some(Ok(Ok(status))) => status,
                            Some(Ok(Err(error))) => {
                                mgr.append_output(&id_clone, &format!("\nwait error: {error}"))
                                    .await;
                                mgr.finish(&id_clone, ShellStatus::Failed, None).await;
                                join_pump(pump).await;
                                return;
                            }
                            Some(Err(_)) => {
                                terminate_process_tree(&mut child).await;
                                mgr.append_output(
                                    &id_clone,
                                    "\n(background lifetime exceeded; killed)",
                                )
                                .await;
                                mgr.finish(&id_clone, ShellStatus::TimedOut, None).await;
                                join_pump(pump).await;
                                return;
                            }
                            None => {
                                terminate_process_tree(&mut child).await;
                                mgr.append_output(&id_clone, "\n(cancelled; process tree killed)")
                                    .await;
                                mgr.finish(&id_clone, ShellStatus::Cancelled, None).await;
                                join_pump(pump).await;
                                return;
                            }
                        };
                        join_pump(pump).await;
                        let late = collected.lock().await.clone();
                        if late.len() > so_far_len {
                            mgr.append_output(&id_clone, &late[so_far_len..]).await;
                        }
                        let code = status.code();
                        let st = if status.success() {
                            ShellStatus::Complete
                        } else {
                            ShellStatus::Failed
                        };
                        mgr.finish(&id_clone, st, code).await;
                    });

                    Ok(ToolOutput::success(format!(
                        "Command timed out after {timeout_ms}ms and was moved to the background.\n\
shell_id: {id}\ndescription: {desc}\n\
Poll with Bash({{\"shell_id\":\"{id}\"}})."
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

#[allow(clippy::too_many_arguments)]
async fn run_shell_job(
    mgr: Arc<BackgroundShellManager>,
    id: String,
    command: String,
    cwd: PathBuf,
    session_root: PathBuf,
    timeout_ms: Option<u64>,
    cancel: Arc<std::sync::atomic::AtomicBool>,
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

    let waited = if let Some(ms) = timeout_ms {
        tokio::select! {
            result = tokio::time::timeout(tokio::time::Duration::from_millis(ms), child.wait()) => Some(result),
            _ = wait_for_interrupt(Some(cancel)) => None,
        }
    } else {
        Some(Ok(child.wait().await))
    };
    let status = match waited {
        Some(result) => match result {
            Ok(Ok(status)) => {
                let code = status.code();
                let st = if status.success() {
                    ShellStatus::Complete
                } else {
                    ShellStatus::Failed
                };
                (st, code)
            }
            Ok(Err(e)) => {
                mgr.append_output(&id, &format!("\nwait error: {}", e))
                    .await;
                (ShellStatus::Failed, None)
            }
            Err(_) => {
                terminate_process_tree(&mut child).await;
                mgr.append_output(
                    &id,
                    &format!(
                        "\n(timed out after {}ms and killed)",
                        timeout_ms.unwrap_or_default()
                    ),
                )
                .await;
                (ShellStatus::TimedOut, None)
            }
        },
        None => {
            terminate_process_tree(&mut child).await;
            mgr.append_output(&id, "\n(cancelled; process tree killed)")
                .await;
            (ShellStatus::Cancelled, None)
        }
    };
    join_pump(pump).await;
    mgr.finish(&id, status.0, status.1).await;
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

async fn run_via_kaos(
    kaos: &kkagent_kaos::KaosHandle,
    command: &str,
    cwd: &Path,
    timeout_ms: Option<u64>,
    interrupted: Option<&Arc<std::sync::atomic::AtomicBool>>,
    description: &str,
    session_id: &str,
) -> anyhow::Result<ToolOutput> {
    tracing::info!(
        %session_id,
        kaos = kaos.kind(),
        cwd = %cwd.display(),
        "Executing Bash via Kaos"
    );
    let exec = kaos.exec(command, Some(cwd));
    let result = if let Some(ms) = timeout_ms {
        match tokio::time::timeout(std::time::Duration::from_millis(ms), exec).await {
            Ok(r) => r,
            Err(_) => {
                return Ok(ToolOutput::error(format!(
                    "Remote command timed out after {ms}ms ({description})"
                )));
            }
        }
    } else {
        exec.await
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

fn truncate_utf8_bytes_in_place(value: &mut String, max: usize) {
    if value.len() <= max {
        return;
    }
    let mut boundary = max;
    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
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
        }
    }

    #[tokio::test]
    async fn limits_concurrent_background_jobs() {
        let manager = BackgroundShellManager::new();
        for index in 0..MAX_RUNNING_JOBS {
            manager
                .insert_running(
                    &format!("job-{index}"),
                    "bash-test",
                    "test".into(),
                    "test".into(),
                )
                .await
                .expect("job within limit");
        }
        let error = manager
            .insert_running("overflow", "bash-test", "test".into(), "test".into())
            .await
            .expect_err("job over limit must fail");
        assert!(error.contains("limit reached"));
    }

    #[test]
    fn truncates_utf8_only_at_character_boundaries() {
        let mut value = "测试内容".repeat(MAX_OUTPUT);
        truncate_utf8_bytes_in_place(&mut value, MAX_OUTPUT * 2);
        assert!(value.len() <= MAX_OUTPUT * 2);
        assert!(std::str::from_utf8(value.as_bytes()).is_ok());
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
}
