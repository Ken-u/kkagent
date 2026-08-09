use crate::path_policy::{decode_text, is_sensitive_path, looks_binary_ext, MAX_LINE_LENGTH};
use crate::{Tool, ToolContext, ToolOutput};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::Path;

pub struct ReadTool;

const MAX_READ_LINES: usize = 1000;
const MAX_READ_BYTES: usize = 100 * 1024;

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "Read"
    }
    fn description(&self) -> &str {
        "Read the contents of a text file. Returns numbered lines (up to 1000 lines or 100KB). \
Rejects binary/image files — use ReadMediaFile for media."
    }
    fn read_only(&self) -> bool {
        true
    }
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
        let path_str = input
            .get("path")
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
        if looks_binary_ext(&path) {
            return Ok(ToolOutput::error(format!(
                "Refusing to Read binary/media file `{}`. Use ReadMediaFile instead.",
                path_str
            )));
        }
        if is_sensitive_path(&path) {
            // Still allow, but annotate — permission layer may have asked already.
            tracing::warn!("Reading sensitive path: {}", path.display());
        }

        let bytes = tokio::fs::read(&path)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to read file: {}", e))?;

        let content = match decode_text(&bytes) {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolOutput::error(format!(
                    "{}: {}. Use ReadMediaFile for media/binary.",
                    path_str, e
                )));
            }
        };

        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();

        let offset = input
            .get("line_offset")
            .and_then(|v| v.as_i64())
            .unwrap_or(1);
        let n_lines = input
            .get("n_lines")
            .and_then(|v| v.as_u64())
            .unwrap_or(MAX_READ_LINES as u64) as usize;
        let n_lines = n_lines.min(MAX_READ_LINES);

        let start = if offset < 0 {
            (total_lines as i64 + offset).max(0) as usize
        } else {
            (offset as usize).saturating_sub(1)
        };

        // An offset beyond EOF is a valid empty page, never a slicing panic.
        let start = start.min(total_lines);
        let requested_end = start.saturating_add(n_lines).min(total_lines);
        let mut selected = Vec::new();
        let mut output_bytes = 0usize;
        let mut end = start;
        for (i, line) in lines[start..requested_end].iter().enumerate() {
            let truncated: String = line.chars().take(MAX_LINE_LENGTH).collect();
            let suffix = if line.chars().count() > MAX_LINE_LENGTH {
                "…"
            } else {
                ""
            };
            let rendered = format!("{:>6}|{}{}", start + i + 1, truncated, suffix);
            let separator = usize::from(!selected.is_empty());
            if output_bytes + separator + rendered.len() > MAX_READ_BYTES {
                break;
            }
            output_bytes += separator + rendered.len();
            selected.push(rendered);
            end = start + i + 1;
        }

        let mut result = selected.join("\n");
        if end < total_lines {
            let notice = format!("... {} more lines not shown ...", total_lines - end);
            if !result.is_empty() {
                result.push('\n');
            }
            // Keep the documented 100 KiB hard ceiling even with the notice.
            let available = MAX_READ_BYTES.saturating_sub(result.len());
            result.push_str(truncate_utf8_bytes(&notice, available));
        }

        Ok(ToolOutput::success_with_data(
            result,
            json!({
                "lineCount": total_lines,
                "bytes": bytes.len(),
                "startLine": start + 1,
                "endLine": end,
            }),
        ))
    }
}

fn truncate_utf8_bytes(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(dir: &std::path::Path) -> ToolContext {
        ToolContext {
            working_dir: dir.to_path_buf(),
            session_id: "read-test".into(),
            tool_call_id: None,
        }
    }

    #[tokio::test]
    async fn offset_beyond_eof_returns_empty_page() {
        let dir = std::env::temp_dir().join(format!("kkagent-read-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("short.txt"), "one\ntwo\n").unwrap();
        let output = ReadTool
            .execute(
                json!({"path": "short.txt", "line_offset": 99}),
                &context(&dir),
            )
            .await
            .unwrap();
        assert!(!output.is_error);
        assert!(output.content.is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn output_respects_utf8_byte_limit() {
        let dir = std::env::temp_dir().join(format!("kkagent-read-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let content = format!("{}\n", "中".repeat(1800)).repeat(40);
        std::fs::write(dir.join("unicode.txt"), content).unwrap();
        let output = ReadTool
            .execute(json!({"path": "unicode.txt"}), &context(&dir))
            .await
            .unwrap();
        assert!(!output.is_error);
        assert!(output.content.len() <= MAX_READ_BYTES);
        assert!(std::str::from_utf8(output.content.as_bytes()).is_ok());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
