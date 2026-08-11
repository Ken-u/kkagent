//! Non-blocking RPC job queue for the TUI main loop.
//!
//! The UI never awaits network/RPC on the render/input path. Jobs run on
//! background tasks and deliver results through an unbounded channel. Each
//! channel has a monotonic generation so late results from superseded requests
//! are discarded.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use kkagent_client::KkagentRequester;
use serde_json::Value;
use tokio::sync::mpsc;

/// Soft threshold before showing a non-blocking busy notice.
pub const SLOW_OP_NOTICE_MS: u64 = 150;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JobChannel {
    SessionsList,
    SessionPreview,
    SessionResume,
    SessionHistory,
    SkillsList,
    McpStatus,
    McpList,
    TasksList,
    Prompt,
    Compact,
    Interrupt,
    LocalShell,
    Generic,
}

impl JobChannel {
    pub fn label(self) -> &'static str {
        match self {
            Self::SessionsList => "Loading sessions",
            Self::SessionPreview => "Loading preview",
            Self::SessionResume => "Switching session",
            Self::SessionHistory => "Loading earlier messages",
            Self::SkillsList => "Loading skills",
            Self::McpStatus => "Connecting MCP",
            Self::McpList => "Loading MCP",
            Self::TasksList => "Loading tasks",
            Self::Prompt => "Sending prompt",
            Self::Compact => "Compacting",
            Self::Interrupt => "Interrupting",
            Self::LocalShell => "Running shell",
            Self::Generic => "Working",
        }
    }
}

#[derive(Debug)]
pub enum JobPayload {
    Rpc {
        method: String,
        result: Result<Value, String>,
    },
    SessionPreview {
        session_id: String,
        result: Result<Value, String>,
    },
    SessionResume {
        query: String,
        result: Result<Value, String>,
    },
    SessionHistory {
        session_id: String,
        before: usize,
        result: Result<Value, String>,
    },
    Prompt {
        session_id: String,
        idempotency_key: String,
        result: Result<(), String>,
    },
    LocalShell {
        command: String,
        result: Result<LocalShellResult, String>,
    },
}

#[derive(Debug, Clone)]
pub struct LocalShellResult {
    pub exit_code: Option<i32>,
    pub output: String,
    pub duration_ms: u64,
    pub timed_out: bool,
}

#[derive(Debug)]
pub struct JobOutcome {
    pub channel: JobChannel,
    pub generation: u64,
    pub started: Instant,
    pub payload: JobPayload,
}

#[derive(Debug, Clone)]
pub struct PendingJob {
    pub channel: JobChannel,
    pub generation: u64,
    pub label: String,
    pub started: Instant,
    pub retryable: bool,
    pub retry_method: Option<String>,
    pub retry_params: Option<Value>,
}

#[derive(Debug, Clone)]
pub enum NoticeKind {
    Busy,
    Error,
    Info,
}

#[derive(Debug, Clone)]
pub struct UiNotice {
    pub kind: NoticeKind,
    pub text: String,
    pub channel: Option<JobChannel>,
    pub generation: Option<u64>,
    pub created: Instant,
    pub retryable: bool,
    pub retry_count: u32,
    pub retry_method: Option<String>,
    pub retry_params: Option<Value>,
}

#[derive(Debug, Clone, Default)]
pub struct McpUiStatus {
    pub configured: bool,
    pub initialized: bool,
    pub connected: usize,
    pub enabled: usize,
    pub total: usize,
    pub tool_count: usize,
    /// First prompt is blocked waiting for MCP discovery.
    pub waiting_for_prompt: bool,
}

impl McpUiStatus {
    pub fn from_json(data: &Value) -> Self {
        Self {
            configured: data
                .get("configured")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            initialized: data
                .get("initialized")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            connected: data.get("connected").and_then(|v| v.as_u64()).unwrap_or(0) as usize,
            enabled: data.get("enabled").and_then(|v| v.as_u64()).unwrap_or(0) as usize,
            total: data.get("total").and_then(|v| v.as_u64()).unwrap_or(0) as usize,
            tool_count: data.get("tool_count").and_then(|v| v.as_u64()).unwrap_or(0) as usize,
            waiting_for_prompt: false,
        }
    }

    pub fn footer_text(&self) -> Option<String> {
        if !self.configured {
            return None;
        }
        if self.initialized {
            if self.waiting_for_prompt {
                return Some(format!("MCP ready · {} tool(s)", self.tool_count));
            }
            return None;
        }
        Some(format!(
            "MCP connecting {}/{}…{}",
            self.connected,
            self.total.max(1),
            if self.waiting_for_prompt {
                "  Ctrl-C cancel"
            } else {
                ""
            }
        ))
    }
}

pub struct AsyncJobHub {
    tx: mpsc::UnboundedSender<JobOutcome>,
    rx: mpsc::UnboundedReceiver<JobOutcome>,
    gens: HashMap<JobChannel, u64>,
    pub pending: HashMap<JobChannel, PendingJob>,
    pub notices: Vec<UiNotice>,
    pub mcp: McpUiStatus,
    /// Cap automatic retries for background refresh failures.
    max_auto_retries: u32,
}

impl AsyncJobHub {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            tx,
            rx,
            gens: HashMap::new(),
            pending: HashMap::new(),
            notices: Vec::new(),
            mcp: McpUiStatus::default(),
            max_auto_retries: 3,
        }
    }

    pub fn next_generation(&mut self, channel: JobChannel) -> u64 {
        let gen = self.gens.entry(channel).or_insert(0);
        *gen = gen.wrapping_add(1);
        *gen
    }

    pub fn current_generation(&self, channel: JobChannel) -> u64 {
        self.gens.get(&channel).copied().unwrap_or(0)
    }

    pub fn is_current(&self, channel: JobChannel, generation: u64) -> bool {
        self.current_generation(channel) == generation
    }

    pub fn spawn_rpc(
        &mut self,
        requester: KkagentRequester,
        channel: JobChannel,
        method: impl Into<String>,
        params: Option<Value>,
        label: Option<String>,
        retryable: bool,
    ) -> u64 {
        let method = method.into();
        let generation = self.next_generation(channel);
        let started = Instant::now();
        let label = label.unwrap_or_else(|| channel.label().to_string());
        self.pending.insert(
            channel,
            PendingJob {
                channel,
                generation,
                label: label.clone(),
                started,
                retryable,
                retry_method: if retryable {
                    Some(method.clone())
                } else {
                    None
                },
                retry_params: if retryable { params.clone() } else { None },
            },
        );
        // Clear prior error notice for this channel.
        self.notices
            .retain(|n| n.channel != Some(channel) || !matches!(n.kind, NoticeKind::Error));

        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = requester
                .rpc_call(&method, params)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(JobOutcome {
                channel,
                generation,
                started,
                payload: JobPayload::Rpc { method, result },
            });
        });
        generation
    }

    pub fn spawn_session_preview(
        &mut self,
        requester: KkagentRequester,
        session_id: String,
    ) -> u64 {
        let generation = self.next_generation(JobChannel::SessionPreview);
        let started = Instant::now();
        self.pending.insert(
            JobChannel::SessionPreview,
            PendingJob {
                channel: JobChannel::SessionPreview,
                generation,
                label: format!("Preview {}", &session_id[..8.min(session_id.len())]),
                started,
                retryable: true,
                retry_method: Some("session.preview".into()),
                retry_params: Some(serde_json::json!({"session_id": session_id})),
            },
        );
        let tx = self.tx.clone();
        let sid = session_id.clone();
        tokio::spawn(async move {
            let params = serde_json::json!({"session_id": sid});
            let result = requester
                .rpc_call("session.preview", Some(params))
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(JobOutcome {
                channel: JobChannel::SessionPreview,
                generation,
                started,
                payload: JobPayload::SessionPreview {
                    session_id: sid,
                    result,
                },
            });
        });
        generation
    }

    pub fn spawn_session_resume(
        &mut self,
        requester: KkagentRequester,
        query: String,
        workspace: String,
    ) -> u64 {
        let generation = self.next_generation(JobChannel::SessionResume);
        let started = Instant::now();
        self.pending.insert(
            JobChannel::SessionResume,
            PendingJob {
                channel: JobChannel::SessionResume,
                generation,
                label: format!("Switching to {}", &query[..8.min(query.len())]),
                started,
                retryable: true,
                retry_method: Some("session.resume".into()),
                retry_params: Some(serde_json::json!({
                    "session_id": query,
                    "display_limit": 60,
                    "workspace": workspace,
                })),
            },
        );
        let tx = self.tx.clone();
        let q = query.clone();
        tokio::spawn(async move {
            let params = serde_json::json!({
                "session_id": q,
                "display_limit": 60,
                "workspace": workspace,
            });
            let result = requester
                .rpc_call("session.resume", Some(params))
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(JobOutcome {
                channel: JobChannel::SessionResume,
                generation,
                started,
                payload: JobPayload::SessionResume { query: q, result },
            });
        });
        generation
    }

    pub fn spawn_session_history(
        &mut self,
        requester: KkagentRequester,
        session_id: String,
        before: usize,
        limit: usize,
    ) -> u64 {
        let generation = self.next_generation(JobChannel::SessionHistory);
        let started = Instant::now();
        self.pending.insert(
            JobChannel::SessionHistory,
            PendingJob {
                channel: JobChannel::SessionHistory,
                generation,
                label: "Loading earlier messages".into(),
                started,
                retryable: true,
                retry_method: Some("session.history".into()),
                retry_params: Some(serde_json::json!({
                    "session_id": session_id,
                    "before": before,
                    "limit": limit,
                })),
            },
        );
        let tx = self.tx.clone();
        let sid = session_id.clone();
        tokio::spawn(async move {
            let params = serde_json::json!({
                "session_id": sid,
                "before": before,
                "limit": limit,
            });
            let result = requester
                .rpc_call("session.history", Some(params))
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(JobOutcome {
                channel: JobChannel::SessionHistory,
                generation,
                started,
                payload: JobPayload::SessionHistory {
                    session_id: sid,
                    before,
                    result,
                },
            });
        });
        generation
    }

    pub fn spawn_prompt(
        &mut self,
        requester: KkagentRequester,
        session_id: String,
        text: String,
        images: Vec<(String, String)>,
        idempotency_key: String,
    ) -> u64 {
        let generation = self.next_generation(JobChannel::Prompt);
        let started = Instant::now();
        self.pending.insert(
            JobChannel::Prompt,
            PendingJob {
                channel: JobChannel::Prompt,
                generation,
                label: "Sending prompt".into(),
                started,
                retryable: true,
                retry_method: Some("session.prompt".into()),
                retry_params: None,
            },
        );
        let tx = self.tx.clone();
        let sid = session_id.clone();
        let key = idempotency_key.clone();
        tokio::spawn(async move {
            let params = serde_json::json!({
                "session_id": sid,
                "text": text,
                "idempotency_key": key,
                "images": images.iter().map(|(media_type, data)| serde_json::json!({
                    "media_type": media_type,
                    "data": data,
                })).collect::<Vec<_>>(),
            });
            let result = requester
                .rpc_call("session.prompt", Some(params))
                .await
                .map(|_| ())
                .map_err(|e| e.to_string());
            let _ = tx.send(JobOutcome {
                channel: JobChannel::Prompt,
                generation,
                started,
                payload: JobPayload::Prompt {
                    session_id: sid,
                    idempotency_key: key,
                    result,
                },
            });
        });
        generation
    }

    /// Run a user `!` shell command locally without touching the agent loop.
    /// Multiple shells can complete out of order; results are never generation-cancelled.
    pub fn spawn_local_shell(&mut self, command: String, cwd: std::path::PathBuf) -> u64 {
        let generation = self.next_generation(JobChannel::LocalShell);
        let started = Instant::now();
        let label = {
            let short: String = command.chars().take(40).collect();
            format!("$ {short}")
        };
        self.pending.insert(
            JobChannel::LocalShell,
            PendingJob {
                channel: JobChannel::LocalShell,
                generation,
                label,
                started,
                retryable: false,
                retry_method: None,
                retry_params: None,
            },
        );
        self.notices
            .retain(|n| n.channel != Some(JobChannel::LocalShell) || !matches!(n.kind, NoticeKind::Error));
        let tx = self.tx.clone();
        let cmd = command.clone();
        tokio::spawn(async move {
            let result = run_local_shell_command(&cmd, &cwd).await;
            let _ = tx.send(JobOutcome {
                channel: JobChannel::LocalShell,
                generation,
                started,
                payload: JobPayload::LocalShell {
                    command: cmd,
                    result,
                },
            });
        });
        generation
    }

    pub fn try_recv(&mut self) -> Option<JobOutcome> {
        self.rx.try_recv().ok()
    }

    pub fn mark_done(&mut self, channel: JobChannel, generation: u64) {
        if self
            .pending
            .get(&channel)
            .is_some_and(|p| p.generation == generation)
        {
            self.pending.remove(&channel);
        }
        self.notices.retain(|n| {
            !(n.channel == Some(channel)
                && n.generation == Some(generation)
                && matches!(n.kind, NoticeKind::Busy))
        });
    }

    pub fn push_info(&mut self, text: impl Into<String>) {
        self.notices.push(UiNotice {
            kind: NoticeKind::Info,
            text: text.into(),
            channel: None,
            generation: None,
            created: Instant::now(),
            retryable: false,
            retry_count: 0,
            retry_method: None,
            retry_params: None,
        });
    }

    pub fn push_error(
        &mut self,
        channel: Option<JobChannel>,
        generation: Option<u64>,
        text: impl Into<String>,
        retryable: bool,
        retry_count: u32,
    ) {
        let (retry_method, retry_params) = channel
            .and_then(|ch| self.pending.get(&ch))
            .map(|p| (p.retry_method.clone(), p.retry_params.clone()))
            .unwrap_or((None, None));
        if let Some(ch) = channel {
            self.notices
                .retain(|n| n.channel != Some(ch) || !matches!(n.kind, NoticeKind::Error));
        }
        self.notices.push(UiNotice {
            kind: NoticeKind::Error,
            text: text.into(),
            channel,
            generation,
            created: Instant::now(),
            retryable,
            retry_count,
            retry_method,
            retry_params,
        });
    }

    /// Promote pending jobs older than SLOW_OP_NOTICE_MS into Busy notices.
    pub fn refresh_busy_notices(&mut self) {
        let threshold = Duration::from_millis(SLOW_OP_NOTICE_MS);
        let now = Instant::now();
        for pending in self.pending.values() {
            if now.duration_since(pending.started) < threshold {
                continue;
            }
            let already = self.notices.iter().any(|n| {
                n.channel == Some(pending.channel)
                    && n.generation == Some(pending.generation)
                    && matches!(n.kind, NoticeKind::Busy)
            });
            if already {
                continue;
            }
            self.notices.push(UiNotice {
                kind: NoticeKind::Busy,
                text: pending.label.clone(),
                channel: Some(pending.channel),
                generation: Some(pending.generation),
                created: now,
                retryable: false,
                retry_count: 0,
                retry_method: None,
                retry_params: None,
            });
        }
        // Drop stale info notices after a few seconds.
        self.notices.retain(|n| match n.kind {
            NoticeKind::Info => now.duration_since(n.created) < Duration::from_secs(4),
            NoticeKind::Busy | NoticeKind::Error => true,
        });
    }

    pub fn active_notice_text(&self) -> Option<String> {
        // Prefer error, then busy, then info.
        if let Some(n) = self
            .notices
            .iter()
            .rev()
            .find(|n| matches!(n.kind, NoticeKind::Error))
        {
            let suffix = if n.retryable { " · r retry" } else { "" };
            return Some(format!("{}{suffix}", n.text));
        }
        if let Some(n) = self
            .notices
            .iter()
            .rev()
            .find(|n| matches!(n.kind, NoticeKind::Busy))
        {
            return Some(format!("{}…", n.text));
        }
        if let Some(mcp) = self.mcp.footer_text() {
            return Some(mcp);
        }
        self.notices
            .iter()
            .rev()
            .find(|n| matches!(n.kind, NoticeKind::Info))
            .map(|n| n.text.clone())
    }

    pub fn take_retryable_error(
        &mut self,
    ) -> Option<(JobChannel, u64, String, Option<Value>, u32)> {
        let idx = self.notices.iter().rposition(|n| {
            matches!(n.kind, NoticeKind::Error) && n.retryable && n.channel.is_some()
        })?;
        let notice = self.notices.remove(idx);
        let channel = notice.channel?;
        let method = notice.retry_method.or_else(|| match channel {
            JobChannel::SessionsList => Some("sessions.list".into()),
            JobChannel::McpStatus => Some("mcp.status".into()),
            JobChannel::SkillsList => Some("skills.list".into()),
            JobChannel::TasksList => Some("tasks.list".into()),
            JobChannel::SessionPreview => Some("session.preview".into()),
            JobChannel::SessionResume => Some("session.resume".into()),
            _ => None,
        })?;
        Some((
            channel,
            notice.generation.unwrap_or(0),
            method,
            notice.retry_params,
            notice.retry_count,
        ))
    }

    pub fn can_auto_retry(&self, retry_count: u32) -> bool {
        retry_count < self.max_auto_retries
    }

    pub fn apply_mcp_status(&mut self, data: &Value) {
        let waiting = self.mcp.waiting_for_prompt;
        self.mcp = McpUiStatus::from_json(data);
        self.mcp.waiting_for_prompt = waiting && !self.mcp.initialized;
        if self.mcp.initialized {
            self.mcp.waiting_for_prompt = false;
            self.pending.remove(&JobChannel::McpStatus);
            self.notices
                .retain(|n| n.channel != Some(JobChannel::McpStatus));
        }
    }
}

const LOCAL_SHELL_TIMEOUT: Duration = Duration::from_secs(300);
const LOCAL_SHELL_OUTPUT_CAP: usize = 64 * 1024;

async fn run_local_shell_command(
    command: &str,
    cwd: &std::path::Path,
) -> Result<LocalShellResult, String> {
    let started = Instant::now();
    #[cfg(windows)]
    let child = tokio::process::Command::new("cmd")
        .args(["/C", command])
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("failed to spawn shell: {e}"))?;
    #[cfg(not(windows))]
    let child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("failed to spawn shell: {e}"))?;

    let wait = child.wait_with_output();
    let output = match tokio::time::timeout(LOCAL_SHELL_TIMEOUT, wait).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => return Err(format!("shell failed: {e}")),
        Err(_) => {
            // kill_on_drop will reap on drop of child — but wait_with_output
            // already consumed it on timeout path differently; spawn again can't.
            // Best-effort: the child handle was moved into wait_with_output future.
            return Ok(LocalShellResult {
                exit_code: None,
                output: format!(
                    "(timed out after {}s; process killed)",
                    LOCAL_SHELL_TIMEOUT.as_secs()
                ),
                duration_ms: started.elapsed().as_millis() as u64,
                timed_out: true,
            });
        }
    };

    let mut text = String::new();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stdout.is_empty() {
        text.push_str(&stdout);
    }
    if !stderr.is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&stderr);
    }
    if text.len() > LOCAL_SHELL_OUTPUT_CAP {
        let keep = LOCAL_SHELL_OUTPUT_CAP / 2;
        let head: String = text.chars().take(keep).collect();
        let tail: String = text
            .chars()
            .rev()
            .take(keep)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        text = format!("{head}\n… output truncated …\n{tail}");
    }
    if text.is_empty() {
        text = "(no output)".into();
    }

    Ok(LocalShellResult {
        exit_code: output.status.code(),
        output: text,
        duration_ms: started.elapsed().as_millis() as u64,
        timed_out: false,
    })
}

impl Default for AsyncJobHub {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_invalidates_stale() {
        let mut hub = AsyncJobHub::new();
        let g1 = hub.next_generation(JobChannel::SessionsList);
        let g2 = hub.next_generation(JobChannel::SessionsList);
        assert!(hub.is_current(JobChannel::SessionsList, g2));
        assert!(!hub.is_current(JobChannel::SessionsList, g1));
    }

    #[test]
    fn mcp_footer_while_connecting() {
        let status = McpUiStatus {
            configured: true,
            initialized: false,
            connected: 1,
            enabled: 2,
            total: 2,
            tool_count: 0,
            waiting_for_prompt: true,
        };
        let text = status.footer_text().unwrap();
        assert!(text.contains("MCP connecting"));
        assert!(text.contains("Ctrl-C"));
    }

    #[tokio::test]
    async fn local_shell_echo() {
        let cwd = std::env::temp_dir();
        let r = run_local_shell_command("echo kkagent-shell-ok", &cwd)
            .await
            .expect("shell");
        assert!(!r.timed_out);
        assert_eq!(r.exit_code, Some(0));
        assert!(r.output.contains("kkagent-shell-ok"));
    }
}
