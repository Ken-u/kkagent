//! Media pipeline helpers — path resolve, mime sniff, size gates.

use std::path::{Path, PathBuf};

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
}
