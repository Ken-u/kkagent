use crate::path_policy::is_sensitive_path;
use crate::{Tool, ToolContext, ToolOutput};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::Path;

pub struct WriteTool;

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "Write"
    }
    fn description(&self) -> &str {
        "Create or overwrite a file with the given content. Missing parent directories are created automatically."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path to write"},
                "content": {"type": "string", "description": "Content to write"},
                "mode": {"type": "string", "enum": ["overwrite", "append"], "description": "Write mode (default: overwrite)"}
            },
            "required": ["path", "content"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let path_str = input
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'path'"))?;
        let content = input
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'content'"))?;
        let mode = input
            .get("mode")
            .and_then(|v| v.as_str())
            .unwrap_or("overwrite");

        let path = if Path::new(path_str).is_absolute() {
            std::path::PathBuf::from(path_str)
        } else {
            ctx.working_dir.join(path_str)
        };

        if is_sensitive_path(&path) {
            return Ok(ToolOutput::error(format!(
                "Refusing to write sensitive file `{}`.",
                path_str
            )));
        }

        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let bytes = content.as_bytes();
        match mode {
            "append" => {
                use tokio::io::AsyncWriteExt;
                let mut file = tokio::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .await?;
                file.write_all(bytes).await?;
            }
            _ => {
                tokio::fs::write(&path, bytes).await?;
            }
        }

        let line_count = content.lines().count();
        Ok(ToolOutput::success_with_data(
            format!(
                "Wrote {} lines ({} bytes) to {}",
                line_count,
                bytes.len(),
                path_str
            ),
            json!({
                "bytesWritten": bytes.len(),
                "lineCount": line_count,
                "path": path_str,
            }),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn refuses_sensitive_files_without_creating_them() {
        let dir = std::env::temp_dir().join(format!("kkagent-write-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let context = ToolContext {
            working_dir: dir.clone(),
            session_id: "write-test".into(),
            image: kkagent_config::ImageConfig::default(),
            tool_call_id: None,
            interrupted: None,
        };
        let output = WriteTool
            .execute(
                json!({"path": ".env", "content": "API_KEY=secret"}),
                &context,
            )
            .await
            .unwrap();
        assert!(output.is_error);
        assert!(!dir.join(".env").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
