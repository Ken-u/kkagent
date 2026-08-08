use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde_json::{json, Value};
use std::path::Path;

use crate::{Tool, ToolContext, ToolOutput};

const MAX_INLINE: usize = 4 * 1024 * 1024; // 4 MiB

pub struct ReadMediaFileTool;

#[async_trait]
impl Tool for ReadMediaFileTool {
    fn name(&self) -> &str {
        "ReadMediaFile"
    }
    fn description(&self) -> &str {
        "Read an image or media file and return base64 plus metadata. \
Large files (>4MB) return metadata only."
    }
    fn read_only(&self) -> bool {
        true
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to image/video/audio file"},
                "include_base64": {"type": "boolean", "description": "Include base64 payload (default true for images <4MB)"}
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let path_str = input
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing path"))?;
        let path = if Path::new(path_str).is_absolute() {
            std::path::PathBuf::from(path_str)
        } else {
            ctx.working_dir.join(path_str)
        };
        if !path.exists() {
            return Ok(ToolOutput::error(format!("File not found: {}", path_str)));
        }
        let meta = tokio::fs::metadata(&path).await?;
        let size = meta.len() as usize;
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();
        let mime = mime_for_ext(&ext);
        let include = input
            .get("include_base64")
            .and_then(|v| v.as_bool())
            .unwrap_or(size <= MAX_INLINE && is_image(&ext));

        let mut out = format!(
            "path: {}\nmime: {}\nsize: {} bytes\n",
            path_str, mime, size
        );
        if include && size <= MAX_INLINE {
            let bytes = tokio::fs::read(&path).await?;
            let b64 = B64.encode(&bytes);
            out.push_str("encoding: base64\n\n");
            out.push_str(&b64);
            Ok(ToolOutput::success_with_data(
                out,
                json!({
                    "mime": mime,
                    "size": size,
                    "base64_len": b64.len(),
                }),
            ))
        } else {
            out.push_str("(base64 omitted — file too large or include_base64=false)\n");
            Ok(ToolOutput::success_with_data(
                out,
                json!({ "mime": mime, "size": size }),
            ))
        }
    }
}

fn is_image(ext: &str) -> bool {
    matches!(
        ext,
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" | "svg"
    )
}

fn mime_for_ext(ext: &str) -> &'static str {
    match ext {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}
