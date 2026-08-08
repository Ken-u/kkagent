use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::{Tool, ToolContext, ToolOutput};

const MAX_OUTPUT: usize = 50_000;
const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 600_000;
const MAX_BG_TIMEOUT_MS: u64 = 3_600_000;

#[derive(Debug, Clone)]
pub struct BashOptions {
    /// When a foreground command times out, detach into background instead of killing.
    pub auto_background_on_timeout: bool,
}

impl Default for BashOptions {
    fn default() -> Self {
        Self {
            auto_background_on_timeout: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum ShellStatus {
    Running,
    Complete,
    Failed,
    TimedOut,
}

#[derive(Clone)]
struct ShellJob {
    description: String,
    command: String,
    status: ShellStatus,
    output: String,
    exit_code: Option<i32>,
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

    async fn insert_running(&self, id: &str, description: String, command: String) {
        self.jobs.lock().await.insert(
            id.to_string(),
            ShellJob {
                description,
                command,
                status: ShellStatus::Running,
                output: String::new(),
                exit_code: None,
            },
        );
    }

    async fn append_output(&self, id: &str, chunk: &str) {
        if let Some(job) = self.jobs.lock().await.get_mut(id) {
            if job.output.len() < MAX_OUTPUT * 2 {
                job.output.push_str(chunk);
                if job.output.len() > MAX_OUTPUT * 2 {
                    job.output.truncate(MAX_OUTPUT * 2);
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

    async fn snapshot(&self, id: &str) -> Option<ShellJob> {
        self.jobs.lock().await.get(id).cloned()
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
        "Execute a shell command. Supports cwd, description, timeout_ms, and \
run_in_background. Foreground timeouts detach to background when configured \
(poll with shell_id). Pass shell_id alone to fetch status/output."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "Shell command to execute"},
                "cwd": {"type": "string", "description": "Working directory (absolute or relative to session cwd)"},
                "description": {"type": "string", "description": "Short description of what this command does"},
                "timeout_ms": {"type": "integer", "description": "Timeout in milliseconds (default: 120000)"},
                "run_in_background": {"type": "boolean", "description": "Start in background and return a shell_id immediately"},
                "shell_id": {"type": "string", "description": "Poll a previously started background shell"}
            }
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        if let Some(shell_id) = input.get("shell_id").and_then(|v| v.as_str()) {
            let has_command = input
                .get("command")
                .and_then(|v| v.as_str())
                .map(|c| !c.is_empty())
                .unwrap_or(false);
            if !has_command {
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
        let safety_note = crate::shell_safety::safety_prefix(&risk).unwrap_or_default();

        let description = input
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let cwd = resolve_cwd(input.get("cwd").and_then(|v| v.as_str()), &ctx.working_dir);
        let run_in_background = input
            .get("run_in_background")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let timeout_ms = input
            .get("timeout_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_TIMEOUT_MS);
        let timeout_ms = if run_in_background {
            timeout_ms.min(MAX_BG_TIMEOUT_MS)
        } else {
            timeout_ms.min(MAX_TIMEOUT_MS)
        };

        if run_in_background {
            if description.is_empty() {
                return Ok(ToolOutput::error(
                    "description is required when run_in_background=true",
                ));
            }
            let mut out = self
                .spawn_background(command, description, cwd, timeout_ms)
                .await?;
            if !safety_note.is_empty() {
                out.content = format!("{safety_note}{}", out.content);
            }
            return Ok(out);
        }

        let mut out = self
            .run_foreground(command, description, cwd, timeout_ms)
            .await?;
        if !safety_note.is_empty() {
            out.content = format!("{safety_note}{}", out.content);
        }
        Ok(out)
    }
}

impl BashTool {
    async fn poll_shell(&self, shell_id: &str) -> ToolOutput {
        match self.backgrounds.snapshot(shell_id).await {
            None => ToolOutput::error(format!("Unknown shell_id: {}", shell_id)),
            Some(job) => {
                let status = match job.status {
                    ShellStatus::Running => "running",
                    ShellStatus::Complete => "complete",
                    ShellStatus::Failed => "failed",
                    ShellStatus::TimedOut => "timed_out",
                };
                let mut out = format!(
                    "shell_id: {}\nstatus: {}\ndescription: {}\ncommand: {}",
                    shell_id, status, job.description, job.command
                );
                if let Some(code) = job.exit_code {
                    out.push_str(&format!("\nexit_code: {}", code));
                }
                if !job.output.is_empty() {
                    out.push_str("\n\n");
                    out.push_str(&truncate_chars(&job.output, MAX_OUTPUT));
                } else if job.status == ShellStatus::Running {
                    out.push_str("\n\n(still running — call Bash again with this shell_id)");
                }
                ToolOutput::success(out)
            }
        }
    }

    async fn spawn_background(
        &self,
        command: String,
        description: String,
        cwd: PathBuf,
        timeout_ms: u64,
    ) -> anyhow::Result<ToolOutput> {
        let id = Uuid::new_v4().to_string();
        self.backgrounds
            .insert_running(&id, description.clone(), command.clone())
            .await;

        let mgr = self.backgrounds.clone();
        let id_clone = id.clone();
        tokio::spawn(async move {
            run_shell_job(mgr, id_clone, command, cwd, Some(timeout_ms), true).await;
        });

        Ok(ToolOutput::success(format!(
            "Background shell started: {description} (shell_id={id}). \
Poll with Bash({{\"shell_id\":\"{id}\"}})."
        )))
    }

    async fn run_foreground(
        &self,
        command: String,
        description: String,
        cwd: PathBuf,
        timeout_ms: u64,
    ) -> anyhow::Result<ToolOutput> {
        let (shell, flag) = shell_and_flag();
        let mut cmd = Command::new(shell);
        cmd.arg(flag).arg(&command);
        cmd.current_dir(&cwd);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        cmd.kill_on_drop(true);
        cmd.env("TERM", "dumb");

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return Ok(ToolOutput::error(format!("Failed to spawn: {}", e))),
        };

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let collected = Arc::new(Mutex::new(String::new()));
        let collected_pump = collected.clone();
        let pump = tokio::spawn(async move {
            collect_stdio(stdout, stderr, collected_pump).await;
        });

        let timeout = tokio::time::Duration::from_millis(timeout_ms);
        match tokio::time::timeout(timeout, child.wait()).await {
            Ok(Ok(status)) => {
                let _ = pump.await;
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
                let _ = pump.await;
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
                    self.backgrounds
                        .insert_running(&id, desc.clone(), command.clone())
                        .await;
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
                        let status = match child.wait().await {
                            Ok(s) => s,
                            Err(e) => {
                                mgr.append_output(&id_clone, &format!("\nwait error: {}", e))
                                    .await;
                                mgr.finish(&id_clone, ShellStatus::Failed, None).await;
                                let _ = pump.await;
                                return;
                            }
                        };
                        let _ = pump.await;
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
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    let _ = pump.await;
                    let output = collected.lock().await.clone();
                    let mut result = format!("Command timed out after {}ms and was killed.", timeout_ms);
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

async fn run_shell_job(
    mgr: Arc<BackgroundShellManager>,
    id: String,
    command: String,
    cwd: PathBuf,
    timeout_ms: Option<u64>,
    _is_background: bool,
) {
    let (shell, flag) = shell_and_flag();
    let mut cmd = Command::new(shell);
    cmd.arg(flag).arg(&command);
    cmd.current_dir(&cwd);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.kill_on_drop(true);
    cmd.env("TERM", "dumb");

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            mgr.append_output(&id, &format!("Failed to spawn: {}", e))
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

    let wait_fut = child.wait();
    let status = if let Some(ms) = timeout_ms {
        match tokio::time::timeout(tokio::time::Duration::from_millis(ms), wait_fut).await {
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
                mgr.append_output(&id, &format!("\nwait error: {}", e)).await;
                (ShellStatus::Failed, None)
            }
            Err(_) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                mgr.append_output(
                    &id,
                    &format!("\n(timed out after {}ms and killed)", ms),
                )
                .await;
                (ShellStatus::TimedOut, None)
            }
        }
    } else {
        match wait_fut.await {
            Ok(status) => {
                let code = status.code();
                let st = if status.success() {
                    ShellStatus::Complete
                } else {
                    ShellStatus::Failed
                };
                (st, code)
            }
            Err(e) => {
                mgr.append_output(&id, &format!("\nwait error: {}", e)).await;
                (ShellStatus::Failed, None)
            }
        }
    };
    let _ = pump.await;
    mgr.finish(&id, status.0, status.1).await;
}

async fn collect_stdio(
    stdout: Option<impl AsyncReadExt + Unpin>,
    stderr: Option<impl AsyncReadExt + Unpin>,
    collected: Arc<Mutex<String>>,
) {
    let mut buf = [0u8; 4096];
    if let Some(mut out) = stdout {
        loop {
            match out.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    let s = String::from_utf8_lossy(&buf[..n]);
                    let mut g = collected.lock().await;
                    if g.len() < MAX_OUTPUT * 2 {
                        g.push_str(&s);
                    }
                }
                Err(_) => break,
            }
        }
    }
    if let Some(mut err) = stderr {
        let mut first = true;
        loop {
            match err.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    let mut g = collected.lock().await;
                    if first {
                        g.push_str("\nSTDERR:\n");
                        first = false;
                    }
                    if g.len() < MAX_OUTPUT * 2 {
                        g.push_str(&String::from_utf8_lossy(&buf[..n]));
                    }
                }
                Err(_) => break,
            }
        }
    }
}

async fn pump_stdio_to_mgr(
    stdout: Option<impl AsyncReadExt + Unpin>,
    stderr: Option<impl AsyncReadExt + Unpin>,
    mgr: Arc<BackgroundShellManager>,
    id: String,
) {
    let mut buf = [0u8; 4096];
    if let Some(mut out) = stdout {
        loop {
            match out.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    mgr.append_output(&id, &String::from_utf8_lossy(&buf[..n]))
                        .await;
                }
                Err(_) => break,
            }
        }
    }
    if let Some(mut err) = stderr {
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
    }
}

fn shell_and_flag() -> (&'static str, &'static str) {
    if cfg!(target_os = "windows") {
        ("cmd", "/C")
    } else {
        ("bash", "-c")
    }
}

fn resolve_cwd(cwd: Option<&str>, session_cwd: &Path) -> PathBuf {
    match cwd {
        Some(c) if !c.is_empty() => {
            let p = PathBuf::from(c);
            if p.is_absolute() {
                p
            } else {
                session_cwd.join(p)
            }
        }
        _ => session_cwd.to_path_buf(),
    }
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
