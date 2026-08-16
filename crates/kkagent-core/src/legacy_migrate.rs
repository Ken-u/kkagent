//! Import legacy kimi-code / `.kimi` session directories into kkagent transcript store.

use anyhow::{Context, Result};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct MigrateReport {
    pub scanned_sessions: usize,
    pub imported_sessions: usize,
    pub imported_messages: usize,
    pub credential_candidates: usize,
    pub imported_credentials: usize,
    pub skipped: Vec<String>,
    pub errors: Vec<String>,
    pub dry_run: bool,
    pub from: String,
}

/// Scan `from` (typically `~/.kimi`) and import JSONL / JSON session transcripts.
pub fn migrate_legacy_home(from: &Path, dry_run: bool) -> Result<MigrateReport> {
    let mut report = MigrateReport {
        dry_run,
        from: from.display().to_string(),
        ..Default::default()
    };
    if !from.exists() {
        anyhow::bail!("legacy path does not exist: {}", from.display());
    }

    let candidates = discover_session_files(from);
    report.scanned_sessions = candidates.len();
    if candidates.is_empty() {
        report.skipped.push("no legacy session files found".into());
    }

    let db = if dry_run || candidates.is_empty() {
        None
    } else {
        Some(crate::transcript::TranscriptDb::open_default()?)
    };

    for path in candidates {
        match import_one(&path, db.as_ref(), dry_run) {
            Ok((session_id, message_count)) => {
                report.imported_sessions += 1;
                report.imported_messages += message_count;
                tracing::info!(
                    "imported legacy session {session_id} ({message_count} messages) from {}",
                    path.display()
                );
            }
            Err(error) => {
                report.errors.push(format!("{}: {error}", path.display()));
            }
        }
    }

    let creds = discover_credential_files(from);
    report.credential_candidates = creds.len();
    for path in creds {
        match import_credential(&path, dry_run) {
            Ok(true) => report.imported_credentials += 1,
            Ok(false) => report
                .skipped
                .push(format!("credential exists: {}", path.display())),
            Err(error) => report.errors.push(format!("{}: {error}", path.display())),
        }
    }

    Ok(report)
}

fn discover_session_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let name = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_lowercase();
                if matches!(name.as_str(), "node_modules" | ".git" | "cache") {
                    continue;
                }
                stack.push(path);
                continue;
            }
            let Some(ext) = path.extension().and_then(|s| s.to_str()) else {
                continue;
            };
            if matches!(ext, "jsonl" | "json") {
                let name = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_lowercase();
                if name.contains("session")
                    || name.contains("transcript")
                    || name.contains("history")
                    || ext == "jsonl"
                {
                    out.push(path);
                }
            }
        }
        if out.len() >= 500 {
            break;
        }
    }
    out
}

fn discover_credential_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let name = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_lowercase();
                if matches!(name.as_str(), "node_modules" | ".git" | "cache") {
                    continue;
                }
                stack.push(path);
                continue;
            }
            let name = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_lowercase();
            if name.contains("oauth")
                || name.contains("credential")
                || name.contains("token")
                || name == "auth.json"
                || name == "credentials.json"
            {
                out.push(path);
            }
        }
        if out.len() >= 50 {
            break;
        }
    }
    out
}

fn import_credential(path: &Path, dry_run: bool) -> Result<bool> {
    let dest_dir = kkagent_config::default_config_dir().join("migrated-credentials");
    let file_name = path
        .file_name()
        .map(|s| s.to_os_string())
        .unwrap_or_else(|| std::ffi::OsString::from("credential.json"));
    let dest = dest_dir.join(&file_name);
    if dest.exists() {
        return Ok(false);
    }
    if dry_run {
        return Ok(true);
    }
    fs::create_dir_all(&dest_dir)?;
    fs::copy(path, &dest).with_context(|| {
        format!(
            "cannot copy credential {} -> {}",
            path.display(),
            dest.display()
        )
    })?;
    Ok(true)
}

fn import_one(
    path: &Path,
    db: Option<&crate::transcript::TranscriptDb>,
    dry_run: bool,
) -> Result<(String, usize)> {
    let text =
        fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    let messages = parse_legacy_messages(&text)?;
    if messages.is_empty() {
        anyhow::bail!("no messages");
    }
    let session_id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| format!("legacy-{}", s.chars().take(48).collect::<String>()))
        .unwrap_or_else(|| format!("legacy-{}", uuid::Uuid::new_v4()));
    let count = messages.len();
    if dry_run {
        return Ok((session_id, count));
    }
    let db = db.ok_or_else(|| anyhow::anyhow!("database unavailable"))?;
    if db.get_session(&session_id)?.is_some() {
        // Idempotent: skip existing
        return Ok((session_id, 0));
    }
    db.create_session(&session_id, "migrated", ".")?;
    db.set_title(
        &session_id,
        &format!(
            "Migrated {}",
            path.file_name().unwrap_or_default().to_string_lossy()
        ),
    )?;
    let rows: Vec<(String, String)> = messages
        .into_iter()
        .map(|(role, content)| {
            (
                role,
                serde_json::json!([{"type":"text","text": content}]).to_string(),
            )
        })
        .collect();
    db.append_messages(&session_id, &rows)?;
    Ok((session_id, count))
}

fn parse_legacy_messages(text: &str) -> Result<Vec<(String, String)>> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    // JSON array of messages
    if trimmed.starts_with('[') {
        let value: Value = serde_json::from_str(trimmed)?;
        return Ok(extract_messages_from_value(&value));
    }
    // Single JSON object (or multi-message wrapper). Multi-line JSONL also starts
    // with `{`, so fall through to JSONL when the whole blob is not one value.
    if trimmed.starts_with('{') {
        if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
            if let Some(arr) = value
                .get("messages")
                .or_else(|| value.get("history"))
                .or_else(|| value.get("turns"))
            {
                return Ok(extract_messages_from_value(arr));
            }
            return Ok(extract_messages_from_value(&Value::Array(vec![value])));
        }
    }
    // JSONL
    let mut out = Vec::new();
    for line in trimmed.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(line)?;
        out.extend(extract_messages_from_value(&Value::Array(vec![value])));
    }
    Ok(out)
}

fn extract_messages_from_value(value: &Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let items = match value {
        Value::Array(items) => items.as_slice(),
        other => std::slice::from_ref(other),
    };
    for item in items {
        let role = item
            .get("role")
            .or_else(|| item.get("type"))
            .and_then(|v| v.as_str())
            .unwrap_or("user")
            .to_string();
        let content = if let Some(s) = item.get("content").and_then(|v| v.as_str()) {
            s.to_string()
        } else if let Some(s) = item.get("text").and_then(|v| v.as_str()) {
            s.to_string()
        } else if let Some(arr) = item.get("content").and_then(|v| v.as_array()) {
            arr.iter()
                .filter_map(|part| {
                    part.get("text")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                })
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            item.to_string()
        };
        if !content.trim().is_empty() {
            out.push((role, content));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_jsonl_fixture() {
        let dir = std::env::temp_dir().join(format!("kk-mig-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session-demo.jsonl");
        fs::write(
            &path,
            r#"{"role":"user","content":"hello legacy"}
{"role":"assistant","content":"hi from kimi"}
"#,
        )
        .unwrap();
        let report = migrate_legacy_home(&dir, true).unwrap();
        assert_eq!(report.imported_sessions, 1);
        assert_eq!(report.imported_messages, 2);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn parses_array_fixture() {
        let text = r#"[{"role":"user","content":"a"},{"role":"assistant","text":"b"}]"#;
        let messages = parse_legacy_messages(text).unwrap();
        assert_eq!(messages.len(), 2);
    }
}
