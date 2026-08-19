use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use image::codecs::jpeg::JpegEncoder;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use crate::{MediaOutput, Tool, ToolContext, ToolOutput};

const MAX_INLINE_BYTES: usize = 5 * 1024 * 1024;
const MAX_SOURCE_BYTES: u64 = 100 * 1024 * 1024;

pub struct ReadMediaFileTool;

#[async_trait]
impl Tool for ReadMediaFileTool {
    fn name(&self) -> &str {
        "ReadMediaFile"
    }

    fn description(&self) -> &str {
        "Send an image to the model as multimodal content. Use region={x,y,width,height} to inspect \
fine detail in original-image coordinates, or full_resolution=true to avoid ordinary downscaling. \
Audio and video currently return metadata only. Files are never modified."
    }

    fn disclosure(&self) -> crate::ToolDisclosure {
        crate::ToolDisclosure::Deferred
    }

    fn read_only(&self) -> bool {
        true
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to an image, video, or audio file"},
                "region": {
                    "type": "object",
                    "description": "Crop in original-image pixel coordinates",
                    "properties": {
                        "x": {"type": "integer", "minimum": 0},
                        "y": {"type": "integer", "minimum": 0},
                        "width": {"type": "integer", "minimum": 1},
                        "height": {"type": "integer", "minimum": 1}
                    },
                    "required": ["x", "y", "width", "height"],
                    "additionalProperties": false
                },
                "full_resolution": {"type": "boolean", "description": "Keep original resolution; errors if the encoded image exceeds the provider-safe limit"},
                "include_base64": {"type": "boolean", "description": "Legacy switch; false returns metadata only"},
                "max_bytes": {"type": "integer", "minimum": 1, "maximum": MAX_INLINE_BYTES, "description": "Optional encoded image byte budget, capped at 5 MiB"}
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
        let region = match input.get("region") {
            Some(value) => match parse_region(value) {
                Ok(region) => Some(region),
                Err(error) => return Ok(ToolOutput::error(error)),
            },
            None => None,
        };
        let full_resolution = input
            .get("full_resolution")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !matches!(kind, MediaKind::Image) && (region.is_some() || full_resolution) {
            return Ok(ToolOutput::error(
                "region and full_resolution apply only to image files.",
            ));
        }
        let default_budget = if region.is_some() || full_resolution {
            MAX_INLINE_BYTES
        } else {
            ctx.image.read_byte_budget.min(MAX_INLINE_BYTES)
        };
        let budget = input
            .get("max_bytes")
            .and_then(Value::as_u64)
            .unwrap_or(default_budget as u64)
            .clamp(1, MAX_INLINE_BYTES as u64) as usize;
        let include = input
            .get("include_base64")
            .and_then(Value::as_bool)
            .unwrap_or(matches!(kind, MediaKind::Image | MediaKind::Audio));

        match kind {
            MediaKind::Image => {
                read_image(
                    path,
                    path_str,
                    metadata.len(),
                    ImageReadOptions {
                        include,
                        budget,
                        max_edge_px: ctx.image.max_edge_px,
                        region,
                        full_resolution,
                    },
                )
                .await
            }
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
    options: ImageReadOptions,
) -> anyhow::Result<ToolOutput> {
    if !options.include {
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
    let prepared = tokio::task::spawn_blocking(move || {
        normalize_image(
            &bytes,
            options.budget,
            options.max_edge_px,
            options.region,
            options.full_resolution,
        )
    })
    .await??;
    let encoded = B64.encode(&prepared.bytes);
    let delivery = if let Some(region) = prepared.region {
        format!(
            "crop x={} y={} width={} height={} from {}x{}",
            region.x,
            region.y,
            region.width,
            region.height,
            prepared.source_width,
            prepared.source_height
        )
    } else if options.full_resolution {
        "full_resolution".into()
    } else {
        "compressed_preview".into()
    };
    let content = format!(
        "Image attached from {display_path}. Source: {}x{} ({source_size} bytes). Delivered: {}x{} JPEG ({} bytes, {delivery}).{}",
        prepared.source_width,
        prepared.source_height,
        prepared.width,
        prepared.height,
        prepared.bytes.len(),
        if options.region.is_none() && !options.full_resolution && (prepared.width < prepared.source_width || prepared.height < prepared.source_height) {
            " Use region to inspect fine detail in original-image coordinates."
        } else {
            ""
        }
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
            "source_width": prepared.source_width,
            "source_height": prepared.source_height,
            "delivery": delivery,
        }),
    )
    .with_image("image/jpeg", encoded))
}

#[derive(Debug, Clone, Copy)]
struct ImageReadOptions {
    include: bool,
    budget: usize,
    max_edge_px: u32,
    region: Option<ImageRegion>,
    full_resolution: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ImageRegion {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

struct PreparedImage {
    bytes: Vec<u8>,
    width: u32,
    height: u32,
    source_width: u32,
    source_height: u32,
    region: Option<ImageRegion>,
}

fn normalize_image(
    bytes: &[u8],
    budget: usize,
    max_edge_px: u32,
    region: Option<ImageRegion>,
    full_resolution: bool,
) -> anyhow::Result<PreparedImage> {
    let decoded = image::load_from_memory(bytes)?;
    let (source_width, source_height) = (decoded.width(), decoded.height());
    let applied_region = region
        .map(|region| clamp_region(region, source_width, source_height))
        .transpose()?;
    let mut image = if let Some(region) = applied_region {
        decoded.crop_imm(region.x, region.y, region.width, region.height)
    } else {
        decoded
    };
    if !full_resolution
        && applied_region.is_none()
        && (image.width() > max_edge_px || image.height() > max_edge_px)
    {
        image = image.thumbnail(max_edge_px, max_edge_px);
    }

    for quality in [90, 85, 75, 65, 55, 45, 35] {
        let mut encoded = Vec::new();
        JpegEncoder::new_with_quality(&mut encoded, quality).encode_image(&image)?;
        if encoded.len() <= budget {
            return Ok(PreparedImage {
                bytes: encoded,
                width: image.width(),
                height: image.height(),
                source_width,
                source_height,
                region: applied_region,
            });
        }
        if full_resolution || applied_region.is_some() {
            continue;
        }
        let next_edge = image.width().max(image.height()).saturating_mul(3) / 4;
        if next_edge >= 256 {
            image = image.thumbnail(next_edge, next_edge);
        }
    }
    if full_resolution {
        anyhow::bail!("full_resolution image cannot fit within the {budget}-byte provider-safe limit; use region instead")
    }
    if applied_region.is_some() {
        anyhow::bail!("cropped region cannot fit within the {budget}-byte provider-safe limit; choose a smaller region")
    }
    anyhow::bail!("image preview cannot fit within the requested {budget}-byte budget")
}

/// Normalize an image returned by an external tool before it enters model history.
pub fn normalize_external_image(
    data: &str,
    config: &kkagent_config::ImageConfig,
) -> anyhow::Result<MediaOutput> {
    let decoded = B64.decode(data)?;
    if decoded.len() as u64 > MAX_SOURCE_BYTES {
        anyhow::bail!("external image exceeds the {MAX_SOURCE_BYTES}-byte source limit");
    }
    let prepared = normalize_image(
        &decoded,
        config.read_byte_budget.min(MAX_INLINE_BYTES),
        config.max_edge_px,
        None,
        false,
    )?;
    Ok(MediaOutput {
        media_type: "image/jpeg".into(),
        data: B64.encode(prepared.bytes),
    })
}

/// Normalize a user-provided image. User attachments use the provider-safe limit rather than the
/// smaller model-initiated read budget so screenshots remain legible.
pub fn normalize_user_image(
    data: &str,
    config: &kkagent_config::ImageConfig,
) -> anyhow::Result<MediaOutput> {
    let decoded = B64.decode(data)?;
    if decoded.len() as u64 > MAX_SOURCE_BYTES {
        anyhow::bail!("user image exceeds the {MAX_SOURCE_BYTES}-byte source limit");
    }
    let prepared = normalize_image(&decoded, MAX_INLINE_BYTES, config.max_edge_px, None, false)?;
    Ok(MediaOutput {
        media_type: "image/jpeg".into(),
        data: B64.encode(prepared.bytes),
    })
}

fn parse_region(value: &Value) -> Result<ImageRegion, String> {
    let read = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .ok_or_else(|| format!("region.{key} must be a non-negative integer"))
    };
    let region = ImageRegion {
        x: read("x")?,
        y: read("y")?,
        width: read("width")?,
        height: read("height")?,
    };
    if region.width == 0 || region.height == 0 {
        return Err("region width and height must be at least 1".into());
    }
    Ok(region)
}

fn clamp_region(region: ImageRegion, width: u32, height: u32) -> anyhow::Result<ImageRegion> {
    if region.x >= width || region.y >= height {
        anyhow::bail!(
            "region starts outside the {width}x{height} image (x={}, y={})",
            region.x,
            region.y
        );
    }
    Ok(ImageRegion {
        width: region.width.min(width - region.x),
        height: region.height.min(height - region.y),
        ..region
    })
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
            turn_id: "test-turn".into(),
            plan_file_path: None,
            image: kkagent_config::ImageConfig::default(),
            tool_call_id: None,
            interrupted: None,
            tools_config: kkagent_config::ToolsConfig::default(),
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
        assert!(output.data.as_ref().unwrap()["width"].as_u64().unwrap() <= 2000);
        assert_eq!(output.images.len(), 1);
        assert!(!output.content.contains(&output.images[0].data));
        assert!(!dir.join(".kkagent").exists());

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn crops_in_original_pixel_coordinates() {
        let dir = std::env::temp_dir().join(format!("kkagent-media-crop-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        image::RgbImage::new(800, 600)
            .save(dir.join("source.png"))
            .unwrap();
        let output = ReadMediaFileTool
            .execute(
                json!({"path": "source.png", "region": {"x": 700, "y": 500, "width": 200, "height": 200}}),
                &context(&dir),
            )
            .await
            .unwrap();
        assert!(!output.is_error);
        assert_eq!(output.data.as_ref().unwrap()["width"], 100);
        assert_eq!(output.data.as_ref().unwrap()["height"], 100);
        assert_eq!(output.images.len(), 1);
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

    #[test]
    fn normalizes_external_user_image_to_jpeg() {
        let mut png = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::new(2, 2))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let output =
            normalize_user_image(&B64.encode(png), &kkagent_config::ImageConfig::default())
                .unwrap();
        assert_eq!(output.media_type, "image/jpeg");
        let decoded = B64.decode(output.data).unwrap();
        assert_eq!(
            image::guess_format(&decoded).unwrap(),
            image::ImageFormat::Jpeg
        );
    }
}
