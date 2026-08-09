use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use image::codecs::jpeg::JpegEncoder;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use crate::{Tool, ToolContext, ToolOutput};

const MAX_INLINE_BYTES: usize = 2 * 1024 * 1024;
const MAX_SOURCE_BYTES: u64 = 100 * 1024 * 1024;
const MAX_IMAGE_DIMENSION: u32 = 2048;

pub struct ReadMediaFileTool;

#[async_trait]
impl Tool for ReadMediaFileTool {
    fn name(&self) -> &str {
        "ReadMediaFile"
    }

    fn description(&self) -> &str {
        "Read media metadata. Raster images are decoded and normalized to a bounded JPEG preview; \
audio can be returned as bounded base64; video is metadata-only. This tool never modifies files."
    }

    fn read_only(&self) -> bool {
        true
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to an image, video, or audio file"},
                "include_base64": {"type": "boolean", "description": "Include a bounded base64 image/audio payload (default: true for images and small audio)"},
                "max_bytes": {"type": "integer", "minimum": 1, "maximum": MAX_INLINE_BYTES, "description": "Maximum decoded payload bytes, capped at 2 MiB"}
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let path_str = input
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("Missing path"))?;
        let path = resolve_path(path_str, &ctx.working_dir);
        let metadata = match tokio::fs::metadata(&path).await {
            Ok(metadata) if metadata.is_file() => metadata,
            Ok(_) => return Ok(ToolOutput::error(format!("Not a file: {path_str}"))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ToolOutput::error(format!("File not found: {path_str}")));
            }
            Err(error) => return Err(error.into()),
        };
        if metadata.len() > MAX_SOURCE_BYTES {
            return Ok(ToolOutput::error(format!(
                "Media file is too large: {} bytes (limit {MAX_SOURCE_BYTES})",
                metadata.len()
            )));
        }

        let ext = path
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let kind = media_kind(&ext);
        let mime = mime_for_ext(&ext);
        let budget = input
            .get("max_bytes")
            .and_then(Value::as_u64)
            .unwrap_or(MAX_INLINE_BYTES as u64)
            .clamp(1, MAX_INLINE_BYTES as u64) as usize;
        let include = input
            .get("include_base64")
            .and_then(Value::as_bool)
            .unwrap_or(matches!(kind, MediaKind::Image | MediaKind::Audio));

        match kind {
            MediaKind::Image => read_image(path, path_str, metadata.len(), include, budget).await,
            MediaKind::Audio => {
                read_bounded_bytes(
                    path,
                    path_str,
                    mime,
                    "audio",
                    metadata.len(),
                    include,
                    budget,
                )
                .await
            }
            MediaKind::Video => Ok(metadata_only(path_str, mime, "video", metadata.len())),
            MediaKind::Other => Ok(ToolOutput::error(format!(
                "Unsupported media type: {path_str}"
            ))),
        }
    }
}

async fn read_image(
    path: PathBuf,
    display_path: &str,
    source_size: u64,
    include: bool,
    budget: usize,
) -> anyhow::Result<ToolOutput> {
    if !include {
        return Ok(metadata_only(
            display_path,
            mime_for_path(&path),
            "image",
            source_size,
        ));
    }
    if mime_for_path(&path) == "image/svg+xml" {
        return Ok(ToolOutput::error(
            "SVG previews are not supported; rasterize the image first.",
        ));
    }

    let bytes = tokio::fs::read(&path).await?;
    let prepared = tokio::task::spawn_blocking(move || normalize_image(&bytes, budget)).await??;
    let encoded = B64.encode(&prepared.bytes);
    let content = format!(
        "path: {display_path}\nmime: image/jpeg\nkind: image\nsource_size: {source_size} bytes\npreview_size: {} bytes\ndimensions: {}x{}\nencoding: base64\n\n{encoded}",
        prepared.bytes.len(), prepared.width, prepared.height
    );
    Ok(ToolOutput::success_with_data(
        content,
        json!({
            "mime": "image/jpeg",
            "kind": "image",
            "source_size": source_size,
            "preview_size": prepared.bytes.len(),
            "width": prepared.width,
            "height": prepared.height,
            "base64_len": encoded.len(),
        }),
    ))
}

struct PreparedImage {
    bytes: Vec<u8>,
    width: u32,
    height: u32,
}

fn normalize_image(bytes: &[u8], budget: usize) -> anyhow::Result<PreparedImage> {
    let decoded = image::load_from_memory(bytes)?;
    let mut image =
        if decoded.width() > MAX_IMAGE_DIMENSION || decoded.height() > MAX_IMAGE_DIMENSION {
            decoded.thumbnail(MAX_IMAGE_DIMENSION, MAX_IMAGE_DIMENSION)
        } else {
            decoded
        };

    for (dimension, quality) in [(2048, 85), (1536, 75), (1024, 65), (768, 55), (512, 45)] {
        if image.width() > dimension || image.height() > dimension {
            image = image.thumbnail(dimension, dimension);
        }
        let mut encoded = Vec::new();
        JpegEncoder::new_with_quality(&mut encoded, quality).encode_image(&image)?;
        if encoded.len() <= budget {
            return Ok(PreparedImage {
                bytes: encoded,
                width: image.width(),
                height: image.height(),
            });
        }
    }
    anyhow::bail!("image preview cannot fit within the requested {budget}-byte budget")
}

async fn read_bounded_bytes(
    path: PathBuf,
    display_path: &str,
    mime: &'static str,
    kind: &'static str,
    size: u64,
    include: bool,
    budget: usize,
) -> anyhow::Result<ToolOutput> {
    if !include || size > budget as u64 {
        return Ok(metadata_only(display_path, mime, kind, size));
    }
    let bytes = tokio::fs::read(path).await?;
    let encoded = B64.encode(bytes);
    Ok(ToolOutput::success_with_data(
        format!(
            "path: {display_path}\nmime: {mime}\nkind: {kind}\nsize: {size} bytes\nencoding: base64\n\n{encoded}"
        ),
        json!({"mime": mime, "kind": kind, "size": size, "base64_len": encoded.len()}),
    ))
}

fn metadata_only(path: &str, mime: &'static str, kind: &'static str, size: u64) -> ToolOutput {
    ToolOutput::success_with_data(
        format!("path: {path}\nmime: {mime}\nkind: {kind}\nsize: {size} bytes\nbase64: omitted"),
        json!({"mime": mime, "kind": kind, "size": size, "delivery": "metadata_only"}),
    )
}

fn resolve_path(path: &str, working_dir: &Path) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        working_dir.join(path)
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
    if matches!(ext, "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg") {
        MediaKind::Image
    } else if matches!(ext, "mp4" | "mov" | "webm" | "mkv" | "avi") {
        MediaKind::Video
    } else if matches!(ext, "mp3" | "wav" | "ogg" | "flac" | "m4a") {
        MediaKind::Audio
    } else {
        MediaKind::Other
    }
}

fn mime_for_path(path: &Path) -> &'static str {
    mime_for_ext(path.extension().and_then(|ext| ext.to_str()).unwrap_or(""))
}

fn mime_for_ext(ext: &str) -> &'static str {
    match ext.to_ascii_lowercase().as_str() {
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
        "avi" => "video/x-msvideo",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "flac" => "audio/flac",
        "m4a" => "audio/mp4",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(dir: &Path) -> ToolContext {
        ToolContext {
            working_dir: dir.to_path_buf(),
            session_id: "media-test".into(),
            tool_call_id: None,
        }
    }

    #[tokio::test]
    async fn normalizes_large_image_without_writing_files() {
        let dir = std::env::temp_dir().join(format!("kkagent-media-tool-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        image::RgbImage::new(3000, 32)
            .save(dir.join("wide.png"))
            .unwrap();

        let output = ReadMediaFileTool
            .execute(json!({"path": "wide.png"}), &context(&dir))
            .await
            .unwrap();
        assert!(!output.is_error);
        assert_eq!(output.data.as_ref().unwrap()["mime"], "image/jpeg");
        assert!(output.data.as_ref().unwrap()["width"].as_u64().unwrap() <= 2048);
        assert!(!dir.join(".kkagent").exists());

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn never_inlines_video() {
        let dir = std::env::temp_dir().join(format!("kkagent-media-tool-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("clip.mp4"), b"fake video").unwrap();
        let output = ReadMediaFileTool
            .execute(
                json!({"path": "clip.mp4", "include_base64": true}),
                &context(&dir),
            )
            .await
            .unwrap();
        assert!(!output.is_error);
        assert!(!output.content.contains(&B64.encode(b"fake video")));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
