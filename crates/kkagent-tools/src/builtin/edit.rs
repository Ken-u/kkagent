use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::Path;
use crate::{Tool, ToolContext, ToolOutput};

pub struct EditTool;

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str { "Edit" }
    fn description(&self) -> &str {
        "Replace exact string occurrences in a file. old_string must be unique unless replace_all is true."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path to edit"},
                "old_string": {"type": "string", "description": "Exact text to replace"},
                "new_string": {"type": "string", "description": "Replacement text"},
                "replace_all": {"type": "boolean", "description": "Replace all occurrences (default: false)"}
            },
            "required": ["path", "old_string", "new_string"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let path_str = input.get("path").and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'path'"))?;
        let old_string = input.get("old_string").and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'old_string'"))?;
        let new_string = input.get("new_string").and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'new_string'"))?;
        let replace_all = input.get("replace_all").and_then(|v| v.as_bool()).unwrap_or(false);

        if old_string == new_string {
            return Ok(ToolOutput::error("old_string and new_string are identical"));
        }

        let path = if Path::new(path_str).is_absolute() {
            std::path::PathBuf::from(path_str)
        } else {
            ctx.working_dir.join(path_str)
        };

        let content = tokio::fs::read_to_string(&path).await
            .map_err(|e| anyhow::anyhow!("Failed to read: {}", e))?;

        let count = content.matches(old_string).count();
        if count == 0 {
            return Ok(ToolOutput::error(format!("old_string not found in {}", path_str)));
        }
        if count > 1 && !replace_all {
            return Ok(ToolOutput::error(format!(
                "Found {} matches in {}. Use replace_all: true to replace all.",
                count, path_str
            )));
        }

        let new_content = if replace_all {
            content.replace(old_string, new_string)
        } else {
            content.replacen(old_string, new_string, 1)
        };

        tokio::fs::write(&path, &new_content).await?;

        Ok(ToolOutput::success(format!(
            "Replaced {} occurrence(s) in {}",
            if replace_all { count } else { 1 },
            path_str
        )))
    }
}
