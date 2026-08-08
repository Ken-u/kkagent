use async_trait::async_trait;
use serde_json::{json, Value};
use crate::{Tool, ToolContext, ToolOutput};

pub struct BashTool;

const MAX_OUTPUT: usize = 50_000;
const DEFAULT_TIMEOUT_MS: u64 = 120_000;

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str { "Bash" }
    fn description(&self) -> &str {
        "Execute a shell command. Returns stdout, stderr, and exit code."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "Shell command to execute"},
                "timeout_ms": {"type": "integer", "description": "Timeout in milliseconds (default: 120000)"}
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let command = input.get("command").and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'command'"))?;
        let timeout_ms = input.get("timeout_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_TIMEOUT_MS);

        let shell = if cfg!(target_os = "windows") { "cmd" } else { "bash" };
        let flag = if cfg!(target_os = "windows") { "/C" } else { "-c" };

        let mut cmd = tokio::process::Command::new(shell);
        cmd.arg(flag).arg(command);
        cmd.current_dir(&ctx.working_dir);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        cmd.env("TERM", "dumb");

        let child = cmd.spawn()?;
        let timeout = tokio::time::Duration::from_millis(timeout_ms);

        match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Ok(Ok(output)) => {
                let mut result = String::new();
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);

                if !stdout.is_empty() {
                    let truncated: String = stdout.chars().take(MAX_OUTPUT).collect();
                    result.push_str(&truncated);
                    if stdout.len() > MAX_OUTPUT {
                        result.push_str(&format!("\n... stdout truncated ({} chars total)", stdout.len()));
                    }
                }
                if !stderr.is_empty() {
                    if !result.is_empty() { result.push('\n'); }
                    result.push_str("STDERR:\n");
                    let truncated: String = stderr.chars().take(MAX_OUTPUT / 2).collect();
                    result.push_str(&truncated);
                }

                let code = output.status.code().unwrap_or(-1);
                if code != 0 {
                    result.push_str(&format!("\nExit code: {}", code));
                }

                if code != 0 {
                    Ok(ToolOutput::error(result))
                } else {
                    Ok(ToolOutput::success(result))
                }
            }
            Ok(Err(e)) => Ok(ToolOutput::error(format!("Failed to run command: {}", e))),
            Err(_) => Ok(ToolOutput::error(format!(
                "Command timed out after {}ms",
                timeout_ms
            ))),
        }
    }
}
