use anyhow::Result;
use std::path::{Path, PathBuf};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

const MAX_HOOK_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_HOOK_TIMEOUT_MS: u64 = 300_000;

#[derive(Debug, Clone)]
pub struct HookConfig {
    pub event: HookEvent,
    pub matcher: Option<String>,
    pub command: String,
    pub args: Vec<String>,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    PreToolCall,
    PostToolCall,
    SessionStart,
    SessionEnd,
    TurnStart,
    TurnEnd,
    Notification,
}

impl HookEvent {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pre_tool_call" => Some(Self::PreToolCall),
            "post_tool_call" => Some(Self::PostToolCall),
            "session_start" => Some(Self::SessionStart),
            "session_end" => Some(Self::SessionEnd),
            "turn_start" => Some(Self::TurnStart),
            "turn_end" => Some(Self::TurnEnd),
            "notification" => Some(Self::Notification),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::PreToolCall => "pre_tool_call",
            Self::PostToolCall => "post_tool_call",
            Self::SessionStart => "session_start",
            Self::SessionEnd => "session_end",
            Self::TurnStart => "turn_start",
            Self::TurnEnd => "turn_end",
            Self::Notification => "notification",
        }
    }
}

pub struct HookManager {
    configured_hooks: Vec<HookConfig>,
    global_hooks: Vec<HookConfig>,
    default_working_dir: PathBuf,
}

impl HookManager {
    pub fn new(working_dir: &Path) -> Self {
        Self {
            configured_hooks: Vec::new(),
            global_hooks: Vec::new(),
            default_working_dir: working_dir.to_path_buf(),
        }
    }

    pub async fn load_from_app_config(&mut self, hooks: &[kkagent_config::HookConfig]) {
        self.configured_hooks = hooks
            .iter()
            .filter_map(|hook| {
                HookEvent::parse(&hook.event).map(|event| HookConfig {
                    event,
                    matcher: hook.matcher.clone(),
                    command: hook.command.clone(),
                    args: Vec::new(),
                    timeout_ms: normalized_timeout(hook.timeout.saturating_mul(1000)),
                })
            })
            .filter(|hook| !hook.command.trim().is_empty())
            .collect();
    }

    /// Load user hooks. Workspace hooks are resolved at fire time so one server
    /// can safely serve multiple workspaces and observe file changes.
    pub async fn discover(&mut self) -> Result<()> {
        let path = kkagent_config::default_config_dir().join("hooks.json");
        self.global_hooks = load_hooks_file(&path).await.unwrap_or_else(|error| {
            if path.exists() {
                tracing::warn!("Failed to load hooks from {}: {error}", path.display());
            }
            Vec::new()
        });
        Ok(())
    }

    async fn hooks_for(&self, context: &serde_json::Value) -> (PathBuf, Vec<HookConfig>) {
        let working_dir = context
            .get("workspace")
            .and_then(serde_json::Value::as_str)
            .map(PathBuf::from)
            .unwrap_or_else(|| self.default_working_dir.clone());
        let mut hooks = self.configured_hooks.clone();
        hooks.extend(self.global_hooks.clone());
        let project_path = working_dir.join(".kkagent").join("hooks.json");
        match load_hooks_file(&project_path).await {
            Ok(project_hooks) => hooks.extend(project_hooks),
            Err(error) if project_path.exists() => {
                tracing::warn!(
                    "Failed to load hooks from {}: {error}",
                    project_path.display()
                );
            }
            Err(_) => {}
        }
        (working_dir, hooks)
    }

    pub async fn fire(&self, event: HookEvent, context: &serde_json::Value) -> Result<()> {
        let _ = self.fire_with_control(event, context).await?;
        Ok(())
    }

    /// Fire hooks; stdout JSON may `{ "block": true, "reason": "..." }` or
    /// `{ "rewrite": {...} }`. stdout/stderr are drained and bounded.
    pub async fn fire_with_control(
        &self,
        event: HookEvent,
        context: &serde_json::Value,
    ) -> Result<HookOutcome> {
        let (working_dir, hooks) = self.hooks_for(context).await;
        let tool = context
            .get("tool")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let mut outcome = HookOutcome::default();
        for hook in hooks {
            if hook.event != event || !matcher_applies(hook.matcher.as_deref(), tool) {
                continue;
            }
            let context_string = serde_json::to_string(context)?;
            let mut command = Command::new(&hook.command);
            command
                .args(&hook.args)
                .env("KKAGENT_HOOK_EVENT", event.as_str())
                .env("KKAGENT_HOOK_CONTEXT", &context_string)
                .current_dir(&working_dir)
                .kill_on_drop(true)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            let mut child = match command.spawn() {
                Ok(child) => child,
                Err(error) => {
                    tracing::warn!("Hook {:?} error: {error}", hook.event);
                    if event == HookEvent::PreToolCall {
                        outcome.block = true;
                        outcome.reason = Some(format!("hook failed to start: {error}"));
                    }
                    continue;
                }
            };
            let stdout_task = child
                .stdout
                .take()
                .map(|mut stdout| tokio::spawn(async move { read_bounded(&mut stdout).await }));
            let stderr_task = child
                .stderr
                .take()
                .map(|mut stderr| tokio::spawn(async move { read_bounded(&mut stderr).await }));
            let status = match tokio::time::timeout(
                std::time::Duration::from_millis(hook.timeout_ms),
                child.wait(),
            )
            .await
            {
                Ok(Ok(status)) => Some(status),
                Ok(Err(error)) => {
                    tracing::warn!("Hook {:?} wait error: {error}", hook.event);
                    None
                }
                Err(_) => {
                    let _ = child.kill().await;
                    tracing::warn!(
                        "Hook {:?} timed out after {}ms",
                        hook.event,
                        hook.timeout_ms
                    );
                    if event == HookEvent::PreToolCall {
                        outcome.block = true;
                        outcome.reason = Some("pre-tool hook timed out".into());
                    }
                    None
                }
            };
            let stdout = join_output(stdout_task).await;
            let stderr = join_output(stderr_task).await;
            if let Some(status) = status {
                if !status.success() {
                    tracing::warn!("Hook {:?} failed: {}", hook.event, stderr.trim());
                    if event == HookEvent::PreToolCall {
                        outcome.block = true;
                        outcome.reason = Some(format!("hook failed: {}", stderr.trim()));
                    }
                } else if let Ok(value) = serde_json::from_str::<serde_json::Value>(stdout.trim()) {
                    if value.get("block").and_then(serde_json::Value::as_bool) == Some(true) {
                        outcome.block = true;
                        outcome.reason = value
                            .get("reason")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string);
                    }
                    if let Some(rewrite) = value.get("rewrite").cloned() {
                        outcome.rewrite = Some(rewrite);
                    }
                }
            }
        }
        Ok(outcome)
    }

    pub async fn fire_notification(&self, message: &str) -> Result<()> {
        self.fire(
            HookEvent::Notification,
            &serde_json::json!({
                "message": message,
                "workspace": self.default_working_dir,
            }),
        )
        .await
    }

    pub fn list(&self) -> Vec<HookConfig> {
        self.configured_hooks
            .iter()
            .chain(&self.global_hooks)
            .cloned()
            .collect()
    }
}

async fn load_hooks_file(path: &Path) -> Result<Vec<HookConfig>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = tokio::fs::read_to_string(path).await?;
    let raw: Vec<serde_json::Value> = serde_json::from_str(&content)?;
    Ok(raw
        .into_iter()
        .filter_map(|entry| {
            let event = HookEvent::parse(entry.get("event")?.as_str()?)?;
            let command = entry.get("command")?.as_str()?.trim().to_string();
            if command.is_empty() {
                return None;
            }
            Some(HookConfig {
                event,
                matcher: entry
                    .get("matcher")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
                command,
                args: entry
                    .get("args")
                    .and_then(serde_json::Value::as_array)
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(serde_json::Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default(),
                timeout_ms: normalized_timeout(
                    entry
                        .get("timeout_ms")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(30_000),
                ),
            })
        })
        .collect())
}

fn normalized_timeout(timeout_ms: u64) -> u64 {
    timeout_ms.clamp(1_000, MAX_HOOK_TIMEOUT_MS)
}

fn matcher_applies(matcher: Option<&str>, tool: &str) -> bool {
    let Some(matcher) = matcher.map(str::trim).filter(|value| !value.is_empty()) else {
        return true;
    };
    if tool.is_empty() {
        return false;
    }
    wildcard_match(matcher, tool)
}

fn wildcard_match(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let parts = pattern.split('*').collect::<Vec<_>>();
    if parts.len() == 1 {
        return pattern == value;
    }
    let mut remainder = value;
    for (index, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        let Some(position) = remainder.find(part) else {
            return false;
        };
        if index == 0 && !pattern.starts_with('*') && position != 0 {
            return false;
        }
        remainder = &remainder[position + part.len()..];
    }
    pattern.ends_with('*') || remainder.is_empty()
}

async fn read_bounded(reader: &mut (impl tokio::io::AsyncRead + Unpin)) -> Vec<u8> {
    let mut output = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(count) if output.len() < MAX_HOOK_OUTPUT_BYTES => {
                let remaining = MAX_HOOK_OUTPUT_BYTES - output.len();
                output.extend_from_slice(&buffer[..count.min(remaining)]);
            }
            Ok(_) => {}
        }
    }
    output
}

async fn join_output(task: Option<tokio::task::JoinHandle<Vec<u8>>>) -> String {
    let bytes = match task {
        Some(task) => task.await.unwrap_or_default(),
        None => Vec::new(),
    };
    String::from_utf8_lossy(&bytes).into_owned()
}

#[derive(Debug, Clone, Default)]
pub struct HookOutcome {
    pub block: bool,
    pub reason: Option<String>,
    pub rewrite: Option<serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matcher_supports_exact_and_wildcards() {
        assert!(matcher_applies(Some("Bash"), "Bash"));
        assert!(matcher_applies(Some("mcp_*_write"), "mcp_repo_write"));
        assert!(!matcher_applies(Some("Read"), "Write"));
        assert!(!matcher_applies(Some("Bash"), ""));
    }

    #[tokio::test]
    async fn configured_and_workspace_hooks_are_merged() {
        let workspace =
            std::env::temp_dir().join(format!("kkagent-hooks-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(workspace.join(".kkagent")).unwrap();
        std::fs::write(
            workspace.join(".kkagent/hooks.json"),
            r#"[{"event":"pre_tool_call","matcher":"Write","command":"project"}]"#,
        )
        .unwrap();
        let mut manager = HookManager::new(&workspace);
        manager
            .load_from_app_config(&[kkagent_config::HookConfig {
                event: "pre_tool_call".into(),
                matcher: Some("Read".into()),
                command: "configured".into(),
                timeout: 5,
            }])
            .await;
        manager.discover().await.unwrap();
        let (_, hooks) = manager
            .hooks_for(&serde_json::json!({"workspace": workspace}))
            .await;
        assert!(hooks.iter().any(|hook| hook.command == "configured"));
        assert!(hooks.iter().any(|hook| hook.command == "project"));
        std::fs::remove_dir_all(workspace).unwrap();
    }
}
