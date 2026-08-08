use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::Path;
use crate::{Tool, ToolContext, ToolOutput};

pub struct ReadTool;

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str { "Read" }
    fn description(&self) -> &str {
        "Read the contents of a file. Returns up to 1000 lines or 100KB."
    }
    fn read_only(&self) -> bool { true }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path to read"},
                "line_offset": {"type": "integer", "description": "Starting line (1-indexed, negative counts from end)"},
                "n_lines": {"type": "integer", "description": "Number of lines to read"}
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let path_str = input.get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'path' parameter"))?;

        let path = if Path::new(path_str).is_absolute() {
            std::path::PathBuf::from(path_str)
        } else {
            ctx.working_dir.join(path_str)
        };

        if !path.exists() {
            return Ok(ToolOutput::error(format!("File not found: {}", path_str)));
        }

        let content = tokio::fs::read_to_string(&path).await
            .map_err(|e| anyhow::anyhow!("Failed to read file: {}", e))?;

        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();

        let offset = input.get("line_offset")
            .and_then(|v| v.as_i64())
            .unwrap_or(1);
        let n_lines = input.get("n_lines")
            .and_then(|v| v.as_u64())
            .unwrap_or(1000) as usize;

        let start = if offset < 0 {
            (total_lines as i64 + offset).max(0) as usize
        } else {
            (offset as usize).saturating_sub(1)
        };

        let end = (start + n_lines).min(total_lines);
        let selected: Vec<String> = lines[start..end]
            .iter()
            .enumerate()
            .map(|(i, line)| format!("{:>6}|{}", start + i + 1, line))
            .collect();

        let mut result = selected.join("\n");
        if end < total_lines {
            result.push_str(&format!("\n... {} more lines not shown ...", total_lines - end));
        }

        Ok(ToolOutput::success(result))
    }
}
