use crate::path_policy::{detect_crlf, is_sensitive_path, restore_line_endings};
use crate::{Tool, ToolContext, ToolOutput};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::Path;

pub struct EditTool;

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "Edit"
    }
    fn description(&self) -> &str {
        "Replace exact string occurrences in a file. old_string must be unique unless replace_all is true. \
Preserves original CRLF/LF line endings."
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
        let path_str = input
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'path'"))?;
        let old_string = input
            .get("old_string")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'old_string'"))?;
        let new_string = input
            .get("new_string")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'new_string'"))?;
        let replace_all = input
            .get("replace_all")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if old_string == new_string {
            return Ok(ToolOutput::error("old_string and new_string are identical"));
        }

        let path = if Path::new(path_str).is_absolute() {
            std::path::PathBuf::from(path_str)
        } else {
            ctx.working_dir.join(path_str)
        };

        if is_sensitive_path(&path) {
            tracing::warn!("Editing sensitive path: {}", path.display());
        }

        let raw = tokio::fs::read(&path)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to read: {}", e))?;
        let content = String::from_utf8_lossy(&raw).into_owned();
        let crlf = detect_crlf(&content);

        // Match against LF-normalized content so models can omit \r.
        let norm = content.replace("\r\n", "\n");
        let old_n = old_string.replace("\r\n", "\n");
        let new_n = new_string.replace("\r\n", "\n");

        let count = norm.matches(&old_n).count();
        if count == 0 {
            // Fuzzy hint: show nearby lines containing a short prefix
            let hint = fuzzy_hint(&norm, &old_n);
            return Ok(ToolOutput::error(format!(
                "old_string not found in {}.{}",
                path_str, hint
            )));
        }
        if count > 1 && !replace_all {
            return Ok(ToolOutput::error(format!(
                "Found {} matches in {}. Use replace_all: true to replace all.",
                count, path_str
            )));
        }

        let new_norm = if replace_all {
            norm.replace(&old_n, &new_n)
        } else {
            norm.replacen(&old_n, &new_n, 1)
        };
        let new_content = restore_line_endings(&new_norm, crlf);

        tokio::fs::write(&path, &new_content).await?;

        Ok(ToolOutput::success(format!(
            "Replaced {} occurrence(s) in {}",
            if replace_all { count } else { 1 },
            path_str
        )))
    }
}

fn fuzzy_hint(haystack: &str, needle: &str) -> String {
    let preview: String = needle.chars().take(40).collect();
    if preview.is_empty() {
        return String::new();
    }
    for (i, line) in haystack.lines().enumerate() {
        if line.contains(preview.chars().take(12).collect::<String>().as_str()) {
            return format!(
                " Closest line {}: {}",
                i + 1,
                line.chars().take(120).collect::<String>()
            );
        }
    }
    String::new()
}
