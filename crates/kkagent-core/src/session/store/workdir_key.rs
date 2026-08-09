//! Workdir bucket keys — `wd_<slug>_<sha256[:12]>` (kimi-code aligned).

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

const WORKDIR_KEY_PREFIX: &str = "wd_";
const HASH_LENGTH: usize = 12;

pub fn normalize_work_dir(work_dir: &Path) -> PathBuf {
    let s = work_dir.to_string_lossy();
    if is_windows_absolute(&s) {
        let resolved = if cfg!(windows) {
            std::fs::canonicalize(work_dir).unwrap_or_else(|_| work_dir.to_path_buf())
        } else {
            // Shape-based: keep as-is but slash-normalize.
            PathBuf::from(s.replace('\\', "/"))
        };
        return PathBuf::from(resolved.to_string_lossy().replace('\\', "/"));
    }
    if work_dir.is_absolute() {
        work_dir.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(work_dir)
    }
}

pub fn encode_work_dir_key(work_dir: &Path) -> String {
    let normalized = normalize_work_dir(work_dir);
    let normalized_s = normalized.to_string_lossy().replace('\\', "/");
    let base = normalized
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("workdir");
    let slug = slugify(base);
    let mut hasher = Sha256::new();
    hasher.update(normalized_s.as_bytes());
    let hash = hex::encode(hasher.finalize());
    format!("{WORKDIR_KEY_PREFIX}{slug}_{}", &hash[..HASH_LENGTH])
}

/// Identity key for "same workspace?" comparisons.
pub fn workspace_root_key(root: &str) -> String {
    let slashed = root.replace('\\', "/");
    let shaped = is_windows_shaped(&slashed);
    let normalized = slashed.trim_end_matches('/').to_string();
    if shaped {
        normalized.to_lowercase()
    } else {
        normalized
    }
}

fn slugify(name: &str) -> String {
    let mut out = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "workdir".into()
    } else {
        trimmed.chars().take(48).collect()
    }
}

fn is_windows_absolute(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
    {
        return true;
    }
    value.starts_with("\\\\") || value.starts_with("//")
}

fn is_windows_shaped(value: &str) -> bool {
    is_windows_absolute(value) || value.starts_with('\\') || value.starts_with("//")
}

pub fn is_safe_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_stable() {
        let a = encode_work_dir_key(Path::new("/tmp/kkagent-demo"));
        let b = encode_work_dir_key(Path::new("/tmp/kkagent-demo"));
        assert_eq!(a, b);
        assert!(a.starts_with("wd_"));
    }

    #[test]
    fn safe_id() {
        assert!(is_safe_session_id("abc-123"));
        assert!(!is_safe_session_id("../x"));
    }
}
