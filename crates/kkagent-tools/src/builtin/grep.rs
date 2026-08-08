use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::Path;
use crate::{Tool, ToolContext, ToolOutput};

pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str { "Grep" }
    fn description(&self) -> &str {
        "Search for a regex pattern across files. Returns matching lines with file paths and line numbers."
    }
    fn read_only(&self) -> bool { true }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Regex pattern to search for"},
                "path": {"type": "string", "description": "Directory or file to search in (defaults to cwd)"},
                "glob": {"type": "string", "description": "File glob pattern filter (e.g. '*.rs')"},
                "case_insensitive": {"type": "boolean", "description": "Case-insensitive search"}
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let pattern = input.get("pattern").and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'pattern'"))?;
        let search_path = input.get("path").and_then(|v| v.as_str())
            .unwrap_or(".");
        let glob_pattern = input.get("glob").and_then(|v| v.as_str());
        let case_insensitive = input.get("case_insensitive").and_then(|v| v.as_bool()).unwrap_or(false);

        let search_dir = if Path::new(search_path).is_absolute() {
            std::path::PathBuf::from(search_path)
        } else {
            ctx.working_dir.join(search_path)
        };

        // Build rg command
        let mut cmd = tokio::process::Command::new("rg");
        cmd.arg("--line-number")
           .arg("--no-heading")
           .arg("--color=never")
           .arg("--max-count=200");

        if case_insensitive {
            cmd.arg("--ignore-case");
        }

        if let Some(glob) = glob_pattern {
            cmd.arg("--glob").arg(glob);
        }

        cmd.arg(pattern).arg(&search_dir);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let output = cmd.output().await?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if !output.status.success() && stdout.is_empty() {
            if stderr.contains("regex parse error") {
                return Ok(ToolOutput::error(format!("Invalid regex: {}", stderr)));
            }
            return Ok(ToolOutput::success("No matches found.".to_string()));
        }

        let lines: Vec<&str> = stdout.lines().collect();
        let total = lines.len();
        let display: String = lines.iter().take(200).cloned().collect::<Vec<_>>().join("\n");

        let mut result = display;
        if total > 200 {
            result.push_str(&format!("\n... {} more matches ...", total - 200));
        }

        Ok(ToolOutput::success(result))
    }
}
