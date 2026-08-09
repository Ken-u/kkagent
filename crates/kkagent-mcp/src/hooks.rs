use anyhow::Result;
use std::path::{Path, PathBuf};
use tokio::process::Command;

#[derive(Debug, Clone)]
pub struct HookConfig {
    pub event: HookEvent,
    pub command: String,
    pub args: Vec<String>,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, PartialEq)]
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
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
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
}

pub struct HookManager {
    hooks: Vec<HookConfig>,
    working_dir: PathBuf,
}

impl HookManager {
    pub fn new(working_dir: &Path) -> Self {
        Self {
            hooks: Vec::new(),
            working_dir: working_dir.to_path_buf(),
        }
    }

    pub async fn load_from_app_config(&mut self, hooks: &[kkagent_config::HookConfig]) {
        for h in hooks {
            if let Some(event) = HookEvent::from_str(&h.event) {
                self.hooks.push(HookConfig {
                    event,
                    command: h.command.clone(),
                    args: Vec::new(),
                    timeout_ms: h.timeout.saturating_mul(1000).max(1000),
                });
            }
        }
    }

    /// Load hooks from ~/.kkagent/hooks.json or .kkagent/hooks.json
    pub async fn discover(&mut self) -> Result<()> {
        self.hooks.clear();

        let locations = [
            kkagent_config::default_config_dir().join("hooks.json"),
            self.working_dir.join(".kkagent").join("hooks.json"),
        ];

        for path in &locations {
            if path.exists() {
                match self.load_hooks_file(path).await {
                    Ok(hooks) => {
                        tracing::info!("Loaded {} hooks from {}", hooks.len(), path.display());
                        self.hooks.extend(hooks);
                    }
                    Err(e) => {
                        tracing::warn!("Failed to load hooks from {}: {}", path.display(), e);
                    }
                }
            }
        }
        Ok(())
    }

    async fn load_hooks_file(&self, path: &Path) -> Result<Vec<HookConfig>> {
        let content = tokio::fs::read_to_string(path).await?;
        let raw: Vec<serde_json::Value> = serde_json::from_str(&content)?;
        let mut hooks = Vec::new();

        for entry in raw {
            let event_str = entry.get("event").and_then(|v| v.as_str()).unwrap_or("");
            let command = entry
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let args: Vec<String> = entry
                .get("args")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let timeout_ms = entry
                .get("timeout_ms")
                .and_then(|v| v.as_u64())
                .unwrap_or(30000);

            if let Some(event) = HookEvent::from_str(event_str) {
                hooks.push(HookConfig {
                    event,
                    command,
                    args,
                    timeout_ms,
                });
            }
        }
        Ok(hooks)
    }

    pub async fn fire(&self, event: HookEvent, context: &serde_json::Value) -> Result<()> {
        let _ = self.fire_with_control(event, context).await?;
        Ok(())
    }

    /// Fire hooks; stdout JSON may `{ "block": true, "reason": "..." }` or `{ "rewrite": {...} }`.
    pub async fn fire_with_control(
        &self,
        event: HookEvent,
        context: &serde_json::Value,
    ) -> Result<HookOutcome> {
        let mut outcome = HookOutcome::default();
        for hook in &self.hooks {
            if hook.event != event {
                continue;
            }
            let context_str = serde_json::to_string(context)?;
            let mut cmd = Command::new(&hook.command);
            for arg in &hook.args {
                cmd.arg(arg);
            }
            cmd.env("KKAGENT_HOOK_EVENT", format!("{:?}", hook.event));
            cmd.env("KKAGENT_HOOK_CONTEXT", &context_str);
            cmd.current_dir(&self.working_dir);
            cmd.stdout(std::process::Stdio::piped());
            cmd.stderr(std::process::Stdio::piped());

            let timeout = std::time::Duration::from_millis(hook.timeout_ms);
            match tokio::time::timeout(timeout, cmd.output()).await {
                Ok(Ok(output)) => {
                    if !output.status.success() {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        tracing::warn!("Hook {:?} failed: {}", hook.event, stderr);
                        // Non-zero exit on PreToolCall can block.
                        if event == HookEvent::PreToolCall {
                            outcome.block = true;
                            outcome.reason = Some(format!("hook failed: {stderr}"));
                        }
                    } else {
                        let stdout = String::from_utf8_lossy(&output.stdout);
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(stdout.trim()) {
                            if v.get("block").and_then(|b| b.as_bool()) == Some(true) {
                                outcome.block = true;
                                outcome.reason =
                                    v.get("reason").and_then(|r| r.as_str()).map(String::from);
                            }
                            if let Some(rw) = v.get("rewrite").cloned() {
                                outcome.rewrite = Some(rw);
                            }
                        }
                    }
                }
                Ok(Err(e)) => {
                    tracing::warn!("Hook {:?} error: {}", hook.event, e);
                }
                Err(_) => {
                    tracing::warn!(
                        "Hook {:?} timed out after {}ms",
                        hook.event,
                        hook.timeout_ms
                    );
                }
            }
        }
        Ok(outcome)
    }

    pub async fn fire_notification(&self, message: &str) -> Result<()> {
        self.fire(
            HookEvent::Notification,
            &serde_json::json!({"message": message}),
        )
        .await
    }

    pub fn list(&self) -> &[HookConfig] {
        &self.hooks
    }
}

#[derive(Debug, Clone, Default)]
pub struct HookOutcome {
    pub block: bool,
    pub reason: Option<String>,
    pub rewrite: Option<serde_json::Value>,
}
