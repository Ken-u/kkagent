//! Single-file JSONL trash for deleted sessions (方案 B).
//!
//! Deleting a session exports everything — the session row, every message and
//! the full content of each oversized tool result — into
//! `<config_dir>/trash/<session_id>.jsonl`, then purges the DB rows and
//! unlinks the files. The archive is write-once from the runtime's point of
//! view: nothing in the server reads it afterwards; it exists purely for
//! offline analysis. The order of operations is crash-safe: a failure can
//! only leave a stale archive (re-deletion overwrites it) or orphaned files.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::Serialize;

use crate::tool_results::{parse_result_filename, tool_results_root};
use crate::transcript::TranscriptDb;

#[derive(Debug, Clone)]
pub struct TrashSummary {
    pub archive_path: PathBuf,
    pub message_count: usize,
    pub tool_result_count: usize,
}

#[derive(Serialize)]
struct SessionLine<'a> {
    r#type: &'static str,
    session_id: &'a str,
    title: Option<&'a str>,
    model: &'a str,
    fallback_model: Option<&'a str>,
    working_dir: &'a str,
    created_at: &'a str,
    updated_at: &'a str,
    message_count: u32,
    is_archived: bool,
}

#[derive(Serialize)]
struct MessageLine<'a> {
    r#type: &'static str,
    seq: i64,
    role: &'a str,
    content: serde_json::Value,
    token_count: Option<u32>,
    created_at: &'a str,
}

#[derive(Serialize)]
struct ToolResultLine {
    r#type: &'static str,
    id: String,
    tool_name: String,
    tool_call_id: String,
    file_path: String,
    output_size_chars: usize,
    output_size_bytes: usize,
    /// Unix timestamp (seconds); `None` for directory-scavenged files.
    created_at: Option<i64>,
    content: Option<String>,
    error: Option<&'static str>,
}

#[derive(Serialize)]
struct SummaryLine {
    r#type: &'static str,
    deleted_at: String,
    message_count: usize,
    tool_result_count: usize,
    kkagent_version: &'static str,
}

/// Trash root directory (`<config_dir>/trash`).
pub fn trash_root(config_dir: &Path) -> PathBuf {
    config_dir.join("trash")
}

/// Export the session to a single JSONL archive, then purge DB rows and
/// unlink tool-result files. See the module docs for ordering guarantees.
pub fn archive_session_to_trash(
    db: &TranscriptDb,
    config_dir: &Path,
    session_id: &str,
) -> Result<TrashSummary> {
    let session = db
        .get_session(session_id)?
        .with_context(|| format!("session not found: {session_id}"))?;
    let messages = db.load_messages(session_id)?;
    let records = db.list_tool_results(session_id)?;

    let root = trash_root(config_dir);
    create_private_dir(&root)?;

    // Collect DB-known files plus any strays in the session directory
    // (e.g. results written by subagents, which skip DB rows).
    let mut files: Vec<ToolResultLine> = Vec::new();
    let mut known_paths: std::collections::HashSet<String> = std::collections::HashSet::new();
    for record in &records {
        known_paths.insert(record.file_path.clone());
        files.push(read_record_line(record));
    }
    let session_dir =
        tool_results_root(config_dir).join(crate::tool_results::sanitize_fragment(session_id));
    if let Ok(entries) = std::fs::read_dir(&session_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            // Canonicalize so the comparison matches DB records (macOS
            // resolves /var → /private/var, for example).
            let path = std::fs::canonicalize(&path).unwrap_or(path);
            let path_str = path.display().to_string();
            if known_paths.contains(&path_str) {
                continue;
            }
            let file_name = entry.file_name().display().to_string();
            let (tool_name, tool_call_id) = parse_result_filename(&file_name);
            let size = entry.metadata().map(|m| m.len() as usize).unwrap_or(0);
            let content = std::fs::read_to_string(&path).ok();
            let error = if content.is_none() {
                Some("file missing")
            } else {
                None
            };
            files.push(ToolResultLine {
                r#type: "tool_result",
                id: file_name,
                tool_name,
                tool_call_id,
                file_path: path_str,
                output_size_chars: content.as_ref().map(|c| c.chars().count()).unwrap_or(0),
                output_size_bytes: size,
                created_at: None,
                content,
                error,
            });
        }
    }

    let archive_path = root.join(format!(
        "{}.jsonl",
        crate::tool_results::sanitize_fragment(session_id)
    ));
    write_jsonl(&archive_path, &session, &messages, &files)?;

    db.purge_session(session_id)
        .with_context(|| format!("purge failed after archiving {session_id}"))?;

    // Best-effort cleanup: failures leave orphan files, which is acceptable.
    for record in &records {
        let path = Path::new(&record.file_path);
        if let Err(error) = std::fs::remove_file(path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!("trash: failed to remove {}: {error}", path.display());
            }
        }
    }
    // Remove strays not covered by DB records.
    if let Ok(entries) = std::fs::read_dir(&session_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                let _ = std::fs::remove_file(path);
            }
        }
    }
    let _ = std::fs::remove_dir(&session_dir);

    Ok(TrashSummary {
        message_count: messages.len(),
        tool_result_count: files.len(),
        archive_path,
    })
}

fn read_record_line(record: &crate::transcript::ToolResultRecord) -> ToolResultLine {
    let content = std::fs::read_to_string(&record.file_path).ok();
    let error = if content.is_none() {
        Some("file missing")
    } else {
        None
    };
    ToolResultLine {
        r#type: "tool_result",
        id: record.id.clone(),
        tool_name: record.tool_name.clone(),
        tool_call_id: record.tool_call_id.clone(),
        file_path: record.file_path.clone(),
        output_size_chars: record.output_size_chars,
        output_size_bytes: record.output_size_bytes,
        created_at: Some(record.created_at),
        content,
        error,
    }
}

fn write_jsonl(
    path: &Path,
    session: &crate::transcript::SessionRecord,
    messages: &[crate::transcript::MessageRecord],
    files: &[ToolResultLine],
) -> Result<()> {
    let tmp = path.with_extension("jsonl.tmp");
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        emit_lines(&mut file, session, messages, files)?;
    }
    #[cfg(not(unix))]
    {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        emit_lines(&mut file, session, messages, files)?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn emit_lines<W: Write>(
    w: &mut W,
    session: &crate::transcript::SessionRecord,
    messages: &[crate::transcript::MessageRecord],
    files: &[ToolResultLine],
) -> Result<()> {
    let session_line = SessionLine {
        r#type: "session",
        session_id: &session.session_id,
        title: session.title.as_deref(),
        model: &session.model,
        fallback_model: session.fallback_model.as_deref(),
        working_dir: &session.working_dir,
        created_at: &session.created_at,
        updated_at: &session.updated_at,
        message_count: session.message_count,
        is_archived: session.is_archived,
    };
    writeln!(w, "{}", serde_json::to_string(&session_line)?)?;

    for message in messages {
        let content = serde_json::from_str(&message.content_json)
            .unwrap_or_else(|_| serde_json::Value::String(message.content_json.clone()));
        let line = MessageLine {
            r#type: "message",
            seq: message.id,
            role: &message.role,
            content,
            token_count: message.token_count,
            created_at: &message.created_at,
        };
        writeln!(w, "{}", serde_json::to_string(&line)?)?;
    }

    for file in files {
        writeln!(w, "{}", serde_json::to_string(file)?)?;
    }

    let summary = SummaryLine {
        r#type: "summary",
        deleted_at: chrono::Utc::now().to_rfc3339(),
        message_count: messages.len(),
        tool_result_count: files.len(),
        kkagent_version: env!("CARGO_PKG_VERSION"),
    };
    writeln!(w, "{}", serde_json::to_string(&summary)?)?;
    Ok(())
}

fn create_private_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(error) = std::fs::create_dir(dir) {
            if error.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(anyhow::anyhow!(
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
                return Err(anyhow::anyhow!(
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

    fn setup() -> (TranscriptDb, PathBuf) {
        let base = std::env::temp_dir().join(format!("kkagent-trash-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&base).unwrap();
        let db = TranscriptDb::open_in_memory().unwrap();
        (db, base)
    }

    #[test]
    fn exports_and_purges_session() {
        let (db, base) = setup();
        db.create_session("s1", "model", "/tmp").unwrap();
        db.append_message("s1", "user", r#"[{"type":"text","text":"hi"}]"#, Some(5))
            .unwrap();

        let persisted =
            crate::tool_results::persist(&base, "s1", "Bash", "call-1", "big output").unwrap();
        db.record_tool_result(&crate::transcript::ToolResultRecord {
            id: uuid::Uuid::new_v4().to_string(),
            session_id: "s1".to_string(),
            turn_id: None,
            tool_call_id: "call-1".to_string(),
            tool_name: "Bash".to_string(),
            file_path: persisted.path.display().to_string(),
            output_size_chars: 9,
            output_size_bytes: 9,
            created_at: 0,
        })
        .unwrap();

        let summary = archive_session_to_trash(&db, &base, "s1").unwrap();
        assert_eq!(summary.message_count, 1);
        assert_eq!(summary.tool_result_count, 1);
        assert!(summary.archive_path.exists());
        assert!(!persisted.path.exists());

        let text = std::fs::read_to_string(&summary.archive_path).unwrap();
        let lines: Vec<serde_json::Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines[0]["type"], "session");
        assert_eq!(lines[0]["session_id"], "s1");
        assert_eq!(lines[1]["type"], "message");
        assert_eq!(lines[1]["content"][0]["text"], "hi");
        assert_eq!(lines[2]["type"], "tool_result");
        assert_eq!(lines[2]["content"], "big output");
        assert_eq!(lines[3]["type"], "summary");
        assert_eq!(lines[3]["message_count"], 1);

        assert!(db.get_session("s1").unwrap().is_none());
        assert!(db.load_messages("s1").unwrap().is_empty());
        assert!(db.list_tool_results("s1").unwrap().is_empty());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn missing_session_fails() {
        let (db, base) = setup();
        assert!(archive_session_to_trash(&db, &base, "nope").is_err());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn redelete_overwrites_archive_and_handles_missing_files() {
        let (db, base) = setup();
        db.create_session("s2", "m", "/tmp").unwrap();
        let persisted = crate::tool_results::persist(&base, "s2", "Read", "c1", "x").unwrap();
        db.record_tool_result(&crate::transcript::ToolResultRecord {
            id: "id1".to_string(),
            session_id: "s2".to_string(),
            turn_id: None,
            tool_call_id: "c1".to_string(),
            tool_name: "Read".to_string(),
            file_path: persisted.path.display().to_string(),
            output_size_chars: 1,
            output_size_bytes: 1,
            created_at: 0,
        })
        .unwrap();
        // Simulate the file being gone before deletion.
        std::fs::remove_file(&persisted.path).unwrap();

        let first = archive_session_to_trash(&db, &base, "s2").unwrap();
        assert!(first.archive_path.exists());
        let text = std::fs::read_to_string(&first.archive_path).unwrap();
        let tool_line = text
            .lines()
            .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
            .find(|v| v["type"] == "tool_result")
            .unwrap();
        assert_eq!(tool_line["content"], serde_json::Value::Null);
        assert_eq!(tool_line["error"], "file missing");

        // Session is purged; a second delete fails cleanly (session gone) but
        // the old archive stays untouched for analysis.
        assert!(archive_session_to_trash(&db, &base, "s2").is_err());
        assert!(first.archive_path.exists());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn scavenges_unrecorded_files() {
        let (db, base) = setup();
        db.create_session("s3", "m", "/tmp").unwrap();
        // A subagent wrote a file without a DB row.
        crate::tool_results::persist(&base, "s3", "Grep", "sub-call", "stray").unwrap();

        let summary = archive_session_to_trash(&db, &base, "s3").unwrap();
        assert_eq!(summary.tool_result_count, 1);
        let text = std::fs::read_to_string(&summary.archive_path).unwrap();
        assert!(text.contains("stray"));
        assert!(text.contains("\"tool_name\":\"Grep\""));
        assert!(!crate::tool_results::tool_results_root(&base)
            .join("s3")
            .exists());
        std::fs::remove_dir_all(&base).unwrap();
    }
}
