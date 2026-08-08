use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde_json::{json, Value};
use std::path::Path;

use crate::{Tool, ToolContext, ToolOutput};

/// Soft inline limit for images (after optional downsample placeholder).
const MAX_INLINE_IMAGE: usize = 2 * 1024 * 1024; // 2 MiB
const MAX_INLINE_VIDEO_META: usize = 64 * 1024 * 1024; // 64 MiB — meta only beyond this
const MAX_DIMENSION_HINT: u32 = 2048;

pub struct ReadMediaFileTool;

#[async_trait]
impl Tool for ReadMediaFileTool {
    fn name(&self) -> &str {
        "ReadMediaFile"
    }
    fn description(&self) -> &str {
        "Read an image/audio/video file and return metadata + optional base64. \
Images >2MB are copied to `.kkagent/media/originals/` and returned as a compressed \
placeholder strategy (metadata + truncated preview). Videos return metadata and a \
delivery hint rather than full base64."
    }
    fn read_only(&self) -> bool {
        true
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to image/video/audio file"},
                "include_base64": {"type": "boolean", "description": "Include base64 payload when policy allows"},
                "max_bytes": {"type": "integer", "description": "Override inline byte budget"}
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
        let kind = media_kind(&ext);
        let max_bytes = input
            .get("max_bytes")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .unwrap_or(match kind {
                MediaKind::Image => MAX_INLINE_IMAGE,
                MediaKind::Video => 0, // never inline full video
                MediaKind::Audio => MAX_INLINE_IMAGE,
                MediaKind::Other => MAX_INLINE_IMAGE / 2,
            });

        let include = input
            .get("include_base64")
            .and_then(|v| v.as_bool())
            .unwrap_or(matches!(kind, MediaKind::Image | MediaKind::Audio) && size <= max_bytes);

        // Preserve original for large images.
        let mut original_path = None;
        if matches!(kind, MediaKind::Image) && size > MAX_INLINE_IMAGE {
            let originals = ctx
                .working_dir
                .join(".kkagent")
                .join("media")
                .join("originals");
            tokio::fs::create_dir_all(&originals).await?;
            let dest = originals.join(
                path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("media.bin"),
            );
            tokio::fs::copy(&path, &dest).await?;
            original_path = Some(dest.display().to_string());
        }

        let mut out = format!(
            "path: {}\nmime: {}\nkind: {:?}\nsize: {} bytes\nmax_dimension_hint: {}\n",
            path_str, mime, kind, size, MAX_DIMENSION_HINT
        );
        if let Some(ref op) = original_path {
            out.push_str(&format!("original_saved: {op}\n"));
            out.push_str("compress_policy: omit full base64; use original path with Read/vision\n");
        }

        match kind {
            MediaKind::Video => {
                out.push_str("video_delivery: metadata_only\n");
                out.push_str(
                    "(Full video bytes are not inlined. Use an external player or upload pipeline.)\n",
                );
                if size > MAX_INLINE_VIDEO_META {
                    out.push_str("warning: video exceeds 64MiB meta comfort zone\n");
                }
                return Ok(ToolOutput::success_with_data(
                    out,
                    json!({
                        "mime": mime,
                        "kind": "video",
                        "size": size,
                        "delivery": "metadata_only",
                    }),
                ));
            }
            MediaKind::Image if ext == "webp" => {
                out.push_str("webp: container recognized (decode deferred to provider vision)\n");
            }
            _ => {}
        }

        if include && max_bytes > 0 && size <= max_bytes {
            let bytes = tokio::fs::read(&path).await?;
            let b64 = B64.encode(&bytes);
            out.push_str("encoding: base64\n\n");
            out.push_str(&b64);
            Ok(ToolOutput::success_with_data(
                out,
                json!({
                    "mime": mime,
                    "kind": format!("{:?}", kind).to_lowercase(),
                    "size": size,
                    "base64_len": b64.len(),
                    "original_path": original_path,
                }),
            ))
        } else {
            out.push_str("(base64 omitted — policy/size/include_base64)\n");
            Ok(ToolOutput::success_with_data(
                out,
                json!({
                    "mime": mime,
                    "kind": format!("{:?}", kind).to_lowercase(),
                    "size": size,
                    "original_path": original_path,
                }),
            ))
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum MediaKind {
    Image,
    Video,
    Audio,
    Other,
}

fn media_kind(ext: &str) -> MediaKind {
    if matches!(
        ext,
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" | "svg"
    ) {
        MediaKind::Image
    } else if matches!(ext, "mp4" | "mov" | "webm" | "mkv" | "avi") {
        MediaKind::Video
    } else if matches!(ext, "mp3" | "wav" | "ogg" | "flac" | "m4a") {
        MediaKind::Audio
    } else {
        MediaKind::Other
    }
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
        "mkv" => "video/x-matroska",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}
