//! Media pipeline helpers — path resolve, mime sniff, size gates.

use std::path::{Path, PathBuf};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use image::codecs::jpeg::JpegEncoder;
use kkagent_llm::ChatContent;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Image,
    Audio,
    Video,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct MediaRef {
    pub path: PathBuf,
    pub kind: MediaKind,
    pub mime: String,
    pub bytes: u64,
}

#[derive(Debug, Clone)]
pub struct MediaLimits {
    pub max_image_bytes: u64,
    pub max_audio_bytes: u64,
    pub max_video_bytes: u64,
}

impl Default for MediaLimits {
    fn default() -> Self {
        Self {
            max_image_bytes: 20 * 1024 * 1024,
            max_audio_bytes: 50 * 1024 * 1024,
            max_video_bytes: 100 * 1024 * 1024,
        }
    }
}

pub fn sniff_kind(path: &Path) -> MediaKind {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg" => MediaKind::Image,
        "mp3" | "wav" | "flac" | "ogg" | "m4a" | "aac" => MediaKind::Audio,
        "mp4" | "mov" | "webm" | "mkv" | "avi" => MediaKind::Video,
        _ => MediaKind::Unknown,
    }
}

pub fn mime_for(kind: MediaKind, path: &Path) -> String {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    match (kind, ext.as_str()) {
        (MediaKind::Image, "png") => "image/png".into(),
        (MediaKind::Image, "jpg" | "jpeg") => "image/jpeg".into(),
        (MediaKind::Image, "gif") => "image/gif".into(),
        (MediaKind::Image, "webp") => "image/webp".into(),
        (MediaKind::Image, "svg") => "image/svg+xml".into(),
        (MediaKind::Image, _) => "image/*".into(),
        (MediaKind::Audio, "mp3") => "audio/mpeg".into(),
        (MediaKind::Audio, "wav") => "audio/wav".into(),
        (MediaKind::Audio, _) => "audio/*".into(),
        (MediaKind::Video, "mp4") => "video/mp4".into(),
        (MediaKind::Video, "webm") => "video/webm".into(),
        (MediaKind::Video, _) => "video/*".into(),
        _ => "application/octet-stream".into(),
    }
}

pub fn resolve_media(path: &Path, limits: &MediaLimits) -> anyhow::Result<MediaRef> {
    let meta = std::fs::metadata(path)?;
    let kind = sniff_kind(path);
    let max = match kind {
        MediaKind::Image => limits.max_image_bytes,
        MediaKind::Audio => limits.max_audio_bytes,
        MediaKind::Video => limits.max_video_bytes,
        MediaKind::Unknown => limits.max_image_bytes,
    };
    if meta.len() > max {
        anyhow::bail!(
            "media file too large: {} bytes (limit {max}) for {:?}",
            meta.len(),
            kind
        );
    }
    Ok(MediaRef {
        path: path.to_path_buf(),
        kind,
        mime: mime_for(kind, path),
        bytes: meta.len(),
    })
}

/// Load an image below `workspace`, normalize it to a bounded JPEG, and return
/// a provider-neutral multimodal content block. Canonicalizing both paths also
/// prevents absolute paths and symlinks from escaping the active workspace.
pub fn load_workspace_image(
    path: &Path,
    workspace: &Path,
    limits: &MediaLimits,
) -> anyhow::Result<ChatContent> {
    let workspace = workspace.canonicalize()?;
    let path = path.canonicalize()?;
    if !path.starts_with(&workspace) {
        anyhow::bail!(
            "media path is outside the active workspace: {}",
            path.display()
        );
    }
    let media = resolve_media(&path, limits)?;
    if media.kind != MediaKind::Image || media.mime == "image/svg+xml" {
        anyhow::bail!("unsupported vision input: {}", path.display());
    }

    let bytes = std::fs::read(&path)?;
    let decoded = image::load_from_memory(&bytes)?;
    let normalized = if decoded.width() > 2048 || decoded.height() > 2048 {
        decoded.thumbnail(2048, 2048)
    } else {
        decoded
    };
    let mut encoded = Vec::new();
    JpegEncoder::new_with_quality(&mut encoded, 85).encode_image(&normalized)?;
    if encoded.len() > 5 * 1024 * 1024 {
        encoded.clear();
        let reduced = normalized.thumbnail(1536, 1536);
        JpegEncoder::new_with_quality(&mut encoded, 70).encode_image(&reduced)?;
    }
    if encoded.len() > 5 * 1024 * 1024 {
        anyhow::bail!("normalized image still exceeds the 5 MiB provider budget");
    }
    Ok(ChatContent::Image {
        media_type: "image/jpeg".into(),
        data: BASE64.encode(encoded),
    })
}

pub fn load_workspace_video(
    path: &Path,
    workspace: &Path,
    limits: &MediaLimits,
) -> anyhow::Result<ChatContent> {
    let workspace = workspace.canonicalize()?;
    let path = path.canonicalize()?;
    if !path.starts_with(&workspace) {
        anyhow::bail!(
            "media path is outside the active workspace: {}",
            path.display()
        );
    }
    let media = resolve_media(&path, limits)?;
    if media.kind != MediaKind::Video {
        anyhow::bail!("unsupported video input: {}", path.display());
    }
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("video filename is not valid UTF-8"))?
        .to_string();
    Ok(ChatContent::Video {
        media_type: media.mime,
        path: path.to_string_lossy().into_owned(),
        filename,
    })
}

/// Extract `@path` media mentions from user text.
pub fn extract_at_paths(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for token in text.split_whitespace() {
        if let Some(rest) = token.strip_prefix('@') {
            if rest.contains('/') || rest.contains('.') {
                out.push(rest.trim_matches(|c| c == '"' || c == '\'').to_string());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    #[test]
    fn sniff() {
        assert_eq!(sniff_kind(Path::new("a.webp")), MediaKind::Image);
        assert_eq!(sniff_kind(Path::new("a.mp4")), MediaKind::Video);
    }

    #[test]
    fn at_paths() {
        let v = extract_at_paths("see @./shot.png and @/tmp/a.mp4");
        assert_eq!(v.len(), 2);
    }

    #[test]
    fn loads_and_bounds_workspace_images() {
        let root = std::env::temp_dir().join(format!("kkagent-media-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("wide.png");
        image::RgbImage::new(2400, 32).save(&path).unwrap();
        let content = load_workspace_image(&path, &root, &MediaLimits::default()).unwrap();
        let ChatContent::Image { media_type, data } = content else {
            panic!("expected image content");
        };
        assert_eq!(media_type, "image/jpeg");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .unwrap();
        let image = image::load_from_memory(&bytes).unwrap();
        assert!(image.width() <= 2048);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_images_outside_workspace() {
        let base = std::env::temp_dir().join(format!("kkagent-media-{}", uuid::Uuid::new_v4()));
        let workspace = base.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let outside = base.join("outside.png");
        image::RgbImage::new(1, 1).save(&outside).unwrap();
        let error = load_workspace_image(&outside, &workspace, &MediaLimits::default())
            .unwrap_err()
            .to_string();
        assert!(error.contains("outside the active workspace"));
        std::fs::remove_dir_all(base).unwrap();
    }
}
