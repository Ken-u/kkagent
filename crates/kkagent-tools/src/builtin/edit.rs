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
                "path": {
                    "type": "string",
                    "description": "Path to the text file to edit. Relative paths resolve against the working directory; a path outside the working directory must be absolute."
                },
                "old_string": {"type": "string", "description": "Exact text to replace"},
                "new_string": {"type": "string", "description": "Replacement text"},
                "replace_all": {"type": "boolean", "description": "Replace all occurrences (default: false)"},
                "expected_content_hash": {
                    "type": "string",
                    "description": "Optional SHA-256 hex of current file contents from a prior Read; fails safely if the file changed externally"
                }
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
        let expected_hash = input
            .get("expected_content_hash")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());

        if old_string == new_string {
            return Ok(ToolOutput::error("old_string and new_string are identical"));
        }

        let path = if Path::new(path_str).is_absolute() {
            std::path::PathBuf::from(path_str)
        } else {
            ctx.working_dir.join(path_str)
        };

        if is_sensitive_path(&path) {
            return Ok(ToolOutput::error(format!(
                "Refusing to edit sensitive file `{}`.",
                path_str
            )));
        }

        let raw = tokio::fs::read(&path)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to read: {}", e))?;
        if let Some(expected) = expected_hash {
            let actual = content_sha256_hex(&raw);
            if !actual.eq_ignore_ascii_case(expected) {
                return Ok(ToolOutput::error(format!(
                    "File changed externally: {path_str} (hash mismatch — re-read before editing)"
                )));
            }
        }
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

fn content_sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn refuses_sensitive_files_without_changing_them() {
        let dir = std::env::temp_dir().join(format!("kkagent-edit-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("client.key");
        std::fs::write(&path, "old secret").unwrap();
        let context = ToolContext {
            working_dir: dir.clone(),
            session_id: "edit-test".into(),
            image: kkagent_config::ImageConfig::default(),
            tool_call_id: None,
            interrupted: None,
        };
        let output = EditTool
            .execute(
                json!({"path": "client.key", "old_string": "old", "new_string": "new"}),
                &context,
            )
            .await
            .unwrap();
        assert!(output.is_error);
        assert_eq!(std::fs::read_to_string(path).unwrap(), "old secret");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
