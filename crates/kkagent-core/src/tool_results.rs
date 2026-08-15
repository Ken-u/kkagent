//! Persistent storage for oversized tool results.
//!
//! Files live under `<config_dir>/tool-results/<session_id>/` with a
//! `<tool_name>-<tool_call_id>_<uuid>.txt` name so they can be traced back to
//! the call that produced them. Main-loop writes also record a row in
//! `transcripts.db` (`tool_results` table); subagent runs share the parent
//! session directory without DB rows (see `crate::trash` for archival).

use std::io::Write;
use std::path::{Path, PathBuf};

/// Characters kept inline before the result is spilled to disk.
pub const TOOL_RESULT_MAX_CHARS: usize = 50_000;
/// Preview length embedded in the message shown to the model.
pub const TOOL_RESULT_PREVIEW_CHARS: usize = 2_000;

/// Where a persisted tool result ended up.
#[derive(Debug, Clone)]
pub struct PersistedToolResult {
    pub path: PathBuf,
    pub output_size_chars: usize,
    pub output_size_bytes: usize,
}

/// Root directory holding every session's tool results
/// (`<config_dir>/tool-results`).
pub fn tool_results_root(config_dir: &Path) -> PathBuf {
    config_dir.join("tool-results")
}

/// Sanitize a path fragment so it is safe on Windows/macOS/Linux.
///
/// Keeps alphanumerics, `-`, `_`, `.`; collapses anything else to `_`.
/// Windows reserved device names (CON, NUL, COM1, ...) get a `_` prefix.
pub fn sanitize_fragment(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    let trimmed = out.trim_matches(|c| c == '.' || c == '_').to_string();
    let candidate = if trimmed.is_empty() {
        "unknown".to_string()
    } else {
        trimmed
    };
    if is_windows_reserved(&candidate) {
        format!("_{candidate}")
    } else {
        candidate
    }
}

fn is_windows_reserved(name: &str) -> bool {
    const RESERVED: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    let upper = name.to_ascii_uppercase();
    let stem = upper.split('.').next().unwrap_or("");
    RESERVED.contains(&stem)
}

/// Best-effort parse of `<tool_name>-<tool_call_id>_<uuid>.txt`.
pub fn parse_result_filename(file_name: &str) -> (String, String) {
    let stem = file_name.strip_suffix(".txt").unwrap_or(file_name);
    if let Some((name, rest)) = stem.split_once('-') {
        if let Some(id) = rest.rsplit_once('_').map(|(id, _)| id) {
            return (name.to_string(), id.to_string());
        }
    }
    ("unknown".to_string(), "unknown".to_string())
}

/// Reject a path component that is a symlink (defense in depth).
fn reject_symlink(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(format!(
            "refusing symlinked path for tool results: {}",
            path.display()
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("cannot inspect path {}: {error}", path.display())),
    }
}

/// Persist `content` for `session_id` under `config_dir`.
///
/// Returns the absolute file path plus size metadata. Fails softly with a
/// human-readable message so callers can degrade to a truncation notice.
pub fn persist(
    config_dir: &Path,
    session_id: &str,
    tool_name: &str,
    tool_call_id: &str,
    content: &str,
) -> Result<PersistedToolResult, String> {
    let root = tool_results_root(config_dir);
    reject_symlink(&root)?;
    create_private_dir(&root)?;

    let session_dir_name = sanitize_fragment(session_id);
    let session_dir = root.join(&session_dir_name);
    reject_symlink(&session_dir)?;
    create_private_dir(&session_dir)?;

    // The canonical check guards against a session dir being swapped for a
    // symlink after creation (TOCTOU hardening over the workspace-era logic).
    let canonical_root = std::fs::canonicalize(&root)
        .map_err(|error| format!("cannot resolve tool-results root ({error})"))?;
    let canonical_session = std::fs::canonicalize(&session_dir)
        .map_err(|error| format!("cannot resolve session directory ({error})"))?;
    if !canonical_session.starts_with(&canonical_root) {
        return Err("tool-results session directory escapes the store".into());
    }

    let file_name = format!(
        "{}-{}_{}.txt",
        sanitize_fragment(tool_name),
        sanitize_fragment(tool_call_id),
        uuid::Uuid::new_v4()
    );
    let path = canonical_session.join(file_name);

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .map_err(|error| format!("cannot create result file ({error})"))?;
    file.write_all(content.as_bytes())
        .map_err(|error| format!("cannot write result file ({error})"))?;

    Ok(PersistedToolResult {
        path,
        output_size_chars: content.chars().count(),
        output_size_bytes: content.len(),
    })
}

/// Create `dir` with 0o700 on Unix, ignoring an already-existing entry.
fn create_private_dir(dir: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(error) = std::fs::create_dir(dir) {
            if error.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(format!(
                    "cannot create directory {} ({error})",
                    dir.display()
                ));
            }
        }
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    #[cfg(not(unix))]
    {
        if let Err(error) = std::fs::create_dir_all(dir) {
            if error.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(format!(
                    "cannot create directory {} ({error})",
                    dir.display()
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_unsafe_fragments() {
        assert_eq!(sanitize_fragment("Bash"), "Bash");
        assert_eq!(sanitize_fragment("tool/call\\id"), "tool_call_id");
        assert_eq!(sanitize_fragment("..."), "unknown");
        assert_eq!(sanitize_fragment(""), "unknown");
        assert_eq!(sanitize_fragment("CON"), "_CON");
    }

    #[test]
    fn parses_generated_file_name() {
        let name = "Bash-call123_abc.txt";
        let (tool, id) = parse_result_filename(name);
        assert_eq!(tool, "Bash");
        assert_eq!(id, "call123");
        let (tool, id) = parse_result_filename("weird.txt");
        assert_eq!(tool, "unknown");
        assert_eq!(id, "unknown");
    }

    #[test]
    fn persists_under_session_directory() {
        let base = std::env::temp_dir().join(format!("kkagent-tr-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&base).unwrap();
        let persisted = persist(&base, "sess 1", "Read", "call/1", "hello").unwrap();
        let root = tool_results_root(&base);
        assert!(persisted
            .path
            .starts_with(std::fs::canonicalize(&root).unwrap()));
        assert_eq!(std::fs::read_to_string(&persisted.path).unwrap(), "hello");
        assert_eq!(persisted.output_size_chars, 5);
        assert_eq!(persisted.output_size_bytes, 5);
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlinked_session_directory() {
        use std::os::unix::fs::symlink;
        let base = std::env::temp_dir().join(format!("kkagent-tr-link-{}", uuid::Uuid::new_v4()));
        let outside = base.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let root = tool_results_root(&base);
        std::fs::create_dir_all(&root).unwrap();
        symlink(&outside, root.join("sess")).unwrap();
        let error = persist(&base, "sess", "Read", "call", "x").unwrap_err();
        assert!(error.contains("escapes") || error.contains("symlinked"));
        assert!(std::fs::read_dir(&outside).unwrap().next().is_none());
        std::fs::remove_dir_all(&base).unwrap();
    }
}
