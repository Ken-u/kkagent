//! Session export — directory bundle + manifest (zip-less portable export).

use crate::session::store::SessionSummary;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportManifest {
    pub version: String,
    pub session_id: String,
    pub work_dir: String,
    pub exported_at: String,
    pub title: Option<String>,
    pub files: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_log: Option<String>,
    #[serde(default)]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ExportResult {
    pub output_dir: PathBuf,
    pub manifest: ExportManifest,
    pub entries: usize,
}

pub fn export_session_directory(
    summary: &SessionSummary,
    output_dir: impl AsRef<Path>,
) -> anyhow::Result<ExportResult> {
    let output_dir = output_dir.as_ref().to_path_buf();
    let session_dir = PathBuf::from(&summary.session_dir);
    if !session_dir.is_dir() {
        anyhow::bail!("session directory missing: {}", session_dir.display());
    }
    ensure_export_destination_outside_source(&session_dir, &output_dir)?;
    std::fs::create_dir_all(&output_dir)?;
    let mut files = Vec::new();
    copy_tree(&session_dir, &output_dir.join("session"), &mut files)?;
    let manifest_files: Vec<String> = files
        .iter()
        .map(|file| relative_name(Path::new(file), &output_dir))
        .collect();
    let manifest = ExportManifest {
        version: "1".into(),
        session_id: summary.id.clone(),
        work_dir: summary.work_dir.clone(),
        exported_at: chrono::Utc::now().to_rfc3339(),
        title: summary.title.clone(),
        files: manifest_files.clone(),
        session_log: manifest_files
            .iter()
            .find(|f| f.ends_with("kkagent.log") || f.ends_with("kimi-code.log"))
            .cloned(),
        notes: vec!["Exported by kkagent (directory bundle)".into()],
    };
    std::fs::write(
        output_dir.join("manifest.json"),
        serde_json::to_string_pretty(&manifest)?,
    )?;
    Ok(ExportResult {
        entries: files.len() + 1,
        output_dir,
        manifest,
    })
}

fn ensure_export_destination_outside_source(src: &Path, dst: &Path) -> anyhow::Result<()> {
    let src = std::fs::canonicalize(src)?;
    let dst = resolve_path_through_existing_ancestor(dst)?;
    if dst.starts_with(&src) {
        anyhow::bail!(
            "export destination {} must not be inside session directory {}",
            dst.display(),
            src.display()
        );
    }
    Ok(())
}

fn resolve_path_through_existing_ancestor(path: &Path) -> anyhow::Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut ancestor = absolute.as_path();
    let mut suffix = Vec::new();
    while !ancestor.exists() {
        let name = ancestor.file_name().ok_or_else(|| {
            anyhow::anyhow!("cannot resolve export destination {}", path.display())
        })?;
        suffix.push(name.to_os_string());
        ancestor = ancestor.parent().ok_or_else(|| {
            anyhow::anyhow!("cannot resolve export destination {}", path.display())
        })?;
    }
    let mut resolved = std::fs::canonicalize(ancestor)?;
    for part in suffix.into_iter().rev() {
        resolved.push(part);
    }
    Ok(resolved)
}

fn copy_tree(src: &Path, dst: &Path, files: &mut Vec<String>) -> anyhow::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let name = entry.file_name();
        let to = dst.join(&name);
        if ty.is_dir() {
            copy_tree(&entry.path(), &to, files)?;
        } else if ty.is_file() {
            std::fs::copy(entry.path(), &to)?;
            files.push(to.to_string_lossy().into());
        }
    }
    Ok(())
}

pub fn default_export_dir_name(session_id: &str) -> String {
    let short = &session_id[..session_id.len().min(8)];
    let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    format!("kkagent-debug-{short}-{ts}")
}

/// Debug bundle manifest — richer than [`ExportManifest`]: it points at the
/// per-session extracts (transcript, audit trail, filtered log) used to
/// analyze a single misbehaving session without exporting the whole home.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DebugExportManifest {
    pub version: String,
    pub session_id: String,
    pub work_dir: String,
    pub exported_at: String,
    pub title: Option<String>,
    /// Absolute paths of the artifacts inside the bundle (relative to root).
    pub files: Vec<String>,
    /// Artifacts that were skipped because the source was missing/unreadable.
    pub missing: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit_event_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_line_count: Option<usize>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct DebugExportResult {
    pub output_dir: PathBuf,
    pub manifest: DebugExportManifest,
    pub entries: usize,
}

/// Export everything needed to analyze one session:
/// - `session/` copy of the on-disk session directory (plans, drafts, …)
/// - `transcript.json` full message history from the transcript DB
/// - `audit.jsonl` lines of the global audit trail matching this session
/// - `kkagent.log` log lines mentioning the session id
/// - `tool-results.json` recorded tool results, when present
///
/// Never fails on a missing optional artifact; the manifest records what was
/// skipped so analysis knows the bundle is partial.
pub fn export_session_debug_bundle(
    summary: &SessionSummary,
    output_dir: impl AsRef<Path>,
) -> anyhow::Result<DebugExportResult> {
    let output_dir = output_dir.as_ref().to_path_buf();
    let session_dir = PathBuf::from(&summary.session_dir);
    if session_dir.is_dir() {
        ensure_export_destination_outside_source(&session_dir, &output_dir)?;
    }
    std::fs::create_dir_all(&output_dir)?;

    let mut files: Vec<String> = Vec::new();
    let mut missing: Vec<String> = Vec::new();

    // 1. On-disk session directory (best effort — older sessions may be pruned).
    if session_dir.is_dir() {
        copy_tree(&session_dir, &output_dir.join("session"), &mut files)?;
    } else {
        missing.push(format!("session dir not found: {}", session_dir.display()));
    }

    // 2. Transcript: full message history from the shared transcript DB.
    let mut message_count = None;
    match crate::transcript::TranscriptDb::open_default() {
        Ok(db) => match db.load_messages(&summary.id) {
            Ok(messages) => {
                message_count = Some(messages.len());
                let path = output_dir.join("transcript.json");
                write_json(&path, &serde_json::to_value(&messages)?)?;
                files.push(relative_name(&path, &output_dir));
            }
            Err(e) => missing.push(format!("transcript load failed: {e}")),
        },
        Err(e) => missing.push(format!("transcript db open failed: {e}")),
    }

    // 3. Audit trail: keep only lines whose session_id matches.
    let audit_src = crate::audit::audit_path();
    let mut audit_event_count = None;
    if audit_src.is_file() {
        match filter_audit_lines_by_session(
            &audit_src,
            &summary.id,
            &output_dir.join("audit.jsonl"),
        ) {
            Ok(count) => {
                audit_event_count = Some(count);
                files.push("audit.jsonl".into());
            }
            Err(e) => missing.push(format!("audit filter failed: {e}")),
        }
    } else {
        missing.push(format!("audit log not found: {}", audit_src.display()));
    }

    // 4. Diagnostic log: lines mentioning the session id.
    let log_src = audit_src
        .parent()
        .map(|p| p.join("kkagent.log"))
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").unwrap_or_default();
            PathBuf::from(home).join(".kkagent").join("kkagent.log")
        });
    let mut log_line_count = None;
    if log_src.is_file() {
        match filter_lines_by_session(&log_src, &summary.id, &output_dir.join("kkagent.log")) {
            Ok(count) => {
                log_line_count = Some(count);
                files.push("kkagent.log".into());
            }
            Err(e) => missing.push(format!("log filter failed: {e}")),
        }
    } else {
        missing.push(format!("log file not found: {}", log_src.display()));
    }

    // 5. Tool results recorded for this session, when the API is available.
    let tool_results = crate::transcript::TranscriptDb::open_default()
        .and_then(|db| db.list_tool_results(&summary.id));
    match tool_results {
        Ok(records) if !records.is_empty() => {
            let path = output_dir.join("tool-results.json");
            write_json(&path, &serde_json::to_value(&records)?)?;
            files.push(relative_name(&path, &output_dir));
        }
        Ok(_) => {}
        Err(e) => missing.push(format!("tool results load failed: {e}")),
    }

    let manifest = DebugExportManifest {
        version: "1".into(),
        session_id: summary.id.clone(),
        work_dir: summary.work_dir.clone(),
        exported_at: chrono::Utc::now().to_rfc3339(),
        title: summary.title.clone(),
        files: files
            .iter()
            .map(|f| {
                let p = Path::new(f);
                relative_name(p, &output_dir)
            })
            .collect(),
        missing,
        message_count,
        audit_event_count,
        log_line_count,
        notes: vec![
            "Single-session debug export (audit + log filtered by session id)".into(),
            "audit.jsonl / kkagent.log lines are verbatim extracts of the global files".into(),
        ],
    };
    let path = output_dir.join("manifest.json");
    write_json(&path, &serde_json::to_value(&manifest)?)?;
    Ok(DebugExportResult {
        entries: manifest.files.len(),
        output_dir,
        manifest,
    })
}

fn write_json(path: &Path, value: &serde_json::Value) -> anyhow::Result<()> {
    std::fs::write(path, serde_json::to_string_pretty(value)?)?;
    Ok(())
}

fn relative_name(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string_lossy().into_owned())
}

/// Copy JSONL audit entries from `src` to `dst` only when the top-level
/// `session_id` field exactly matches. Malformed/truncated lines and global
/// events without a session id are skipped.
fn filter_audit_lines_by_session(
    src: &Path,
    session_id: &str,
    dst: &Path,
) -> anyhow::Result<usize> {
    anyhow::ensure!(!session_id.is_empty(), "session id must not be empty");
    filter_lines(src, dst, |line| {
        serde_json::from_str::<serde_json::Value>(line)
            .ok()
            .and_then(|value| {
                value
                    .get("session_id")
                    .and_then(|value| value.as_str())
                    .map(|value| value == session_id)
            })
            .unwrap_or(false)
    })
}

/// Copy diagnostic log lines that mention `session_id`.
fn filter_lines_by_session(src: &Path, session_id: &str, dst: &Path) -> anyhow::Result<usize> {
    anyhow::ensure!(!session_id.is_empty(), "session id must not be empty");
    filter_lines(src, dst, |line| line.contains(session_id))
}

fn filter_lines(
    src: &Path,
    dst: &Path,
    mut include: impl FnMut(&str) -> bool,
) -> anyhow::Result<usize> {
    use std::io::{BufRead, BufWriter, Write};
    let input = std::fs::File::open(src)?;
    let reader = std::io::BufReader::new(input);
    let output = std::fs::File::create(dst)?;
    let mut writer = BufWriter::new(output);
    let mut kept = 0usize;
    for line in reader.lines() {
        let line = match line {
            Ok(line) => line,
            Err(_) => continue,
        };
        if include(&line) {
            writeln!(writer, "{line}")?;
            kept += 1;
        }
    }
    writer.flush()?;
    Ok(kept)
}

#[cfg(test)]
mod debug_export_tests {
    use super::*;

    fn summary_for_export(session_dir: &Path) -> SessionSummary {
        SessionSummary {
            id: "session-1".into(),
            session_dir: session_dir.to_string_lossy().into_owned(),
            work_dir: session_dir.to_string_lossy().into_owned(),
            title: None,
            is_custom_title: false,
            archived: false,
            last_prompt: None,
            first_prompt: None,
            created_at: 0,
            updated_at: 0,
            forked_from: None,
        }
    }

    #[test]
    fn export_manifest_uses_portable_relative_paths() {
        let root =
            std::env::temp_dir().join(format!("kkagent-export-path-{}", uuid::Uuid::new_v4()));
        let session_dir = root.join("session-source");
        std::fs::create_dir_all(session_dir.join("logs")).unwrap();
        std::fs::write(session_dir.join("state.json"), "{}").unwrap();
        std::fs::write(session_dir.join("logs/kkagent.log"), "line\n").unwrap();
        let output = root.join("bundle");

        let result = export_session_directory(&summary_for_export(&session_dir), &output).unwrap();
        assert!(result.manifest.files.contains(&"session/state.json".into()));
        assert!(result
            .manifest
            .files
            .contains(&"session/logs/kkagent.log".into()));
        assert_eq!(
            result.manifest.session_log.as_deref(),
            Some("session/logs/kkagent.log")
        );
        for file in &result.manifest.files {
            assert!(
                !Path::new(file).is_absolute(),
                "absolute manifest path: {file}"
            );
            assert!(output.join(file).is_file(), "missing exported file: {file}");
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn export_rejects_destination_inside_session_directory() {
        let root =
            std::env::temp_dir().join(format!("kkagent-export-loop-{}", uuid::Uuid::new_v4()));
        let session_dir = root.join("session-source");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(session_dir.join("state.json"), "{}").unwrap();
        let output = session_dir.join("nested/export");

        let err = export_session_directory(&summary_for_export(&session_dir), &output).unwrap_err();
        assert!(err.to_string().contains("must not be inside"));
        assert!(!output.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn export_rejects_symlinked_parent_into_session_directory() {
        let root =
            std::env::temp_dir().join(format!("kkagent-export-link-{}", uuid::Uuid::new_v4()));
        let session_dir = root.join("session-source");
        std::fs::create_dir_all(&session_dir).unwrap();
        let link = root.join("linked-parent");
        std::os::unix::fs::symlink(&session_dir, &link).unwrap();
        let output = link.join("export");

        let err = export_session_directory(&summary_for_export(&session_dir), &output).unwrap_err();
        assert!(err.to_string().contains("must not be inside"));
        assert!(!session_dir.join("export").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn audit_filter_requires_exact_top_level_session_id() {
        let dir = std::env::temp_dir().join(format!("kkagent-export-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sid = "aaaa-bbbb-cccc";
        let src = dir.join("audit.jsonl");
        std::fs::write(
            &src,
            format!(
                "{{\"kind\":\"permission_verdict\",\"session_id\":\"{sid}\",\"tool\":\"Read\"}}\n\
                 {{\"kind\":\"permission_verdict\",\"session_id\":\"other-session\",\"detail\":\"mentions {sid}\"}}\n\
                 {{\"kind\":\"permission_verdict\",\"session_id\":\"{sid}-child\",\"tool\":\"Bash\"}}\n\
                 {{\"kind\":\"sandbox_fallback\",\"reason\":\"mentions {sid}\"}}\n\
                 not-json-{sid}\n\
                 {{\"kind\":\"approval_response\",\"session_id\":\"{sid}\"}}\n"
            ),
        )
        .unwrap();
        let dst = dir.join("filtered.jsonl");
        let kept = filter_audit_lines_by_session(&src, sid, &dst).unwrap();
        assert_eq!(kept, 2);
        let content = std::fs::read_to_string(&dst).unwrap();
        assert!(content.contains("\"tool\":\"Read\""));
        assert!(content.contains("approval_response"));
        assert!(!content.contains("other-session"));
        assert!(!content.contains("-child"));
        assert!(!content.contains("sandbox_fallback"));
        assert!(!content.contains("not-json"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn filter_lines_tolerates_missing_lines_and_empty_source() {
        let dir =
            std::env::temp_dir().join(format!("kkagent-export-test-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("empty.log");
        std::fs::write(&src, "").unwrap();
        let dst = dir.join("out.log");
        let kept = filter_lines_by_session(&src, "sid", &dst).unwrap();
        assert_eq!(kept, 0);
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "");
        std::fs::remove_dir_all(&dir).ok();
    }
}
