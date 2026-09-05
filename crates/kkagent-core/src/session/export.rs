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

    // 4. Diagnostic log: two extracts into one file.
    //    a) lines mentioning the session id (explicit correlation), and
    //    b) lines inside the session's activity window — transport errors
    //       like `LLM stream error` may not carry the session id, but their
    //       timestamp falls between the first and last transcript message.
    //    (b) is appended only for lines not already captured by (a).
    let log_src = audit_src
        .parent()
        .map(|p| p.join("kkagent.log"))
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").unwrap_or_default();
            PathBuf::from(home).join(".kkagent").join("kkagent.log")
        });
    let mut log_line_count = None;
    if log_src.is_file() {
        let window = session_activity_window(&summary.id);
        match filter_log_lines(
            &log_src,
            &summary.id,
            window.as_ref(),
            &output_dir.join("kkagent.log"),
        ) {
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

/// Resolve a session summary by id. Tolerates a pruned session index by
/// scanning the sessions directory (flat `sessions/<id>` or bucketed
/// `sessions/<bucket>/<id>` layouts), mirroring `kk export-session`.
pub fn find_session_summary(session_id: &str) -> anyhow::Result<SessionSummary> {
    anyhow::ensure!(
        !session_id.trim().is_empty(),
        "session id must not be empty"
    );
    let store = crate::session::store::SessionStore::open_default();
    if let Ok(summary) = store.get(session_id) {
        return Ok(summary);
    }
    let sessions_dir = store.sessions_dir;
    let found = sessions_dir
        .join(session_id)
        .is_dir()
        .then(|| sessions_dir.join(session_id))
        .or_else(|| {
            std::fs::read_dir(&sessions_dir)
                .ok()?
                .flatten()
                .find_map(|bucket| {
                    let candidate = bucket.path().join(session_id);
                    candidate.is_dir().then_some(candidate)
                })
        });
    let Some(session_dir) = found else {
        anyhow::bail!(
            "session not found: {session_id} (nothing in session store under {})",
            sessions_dir.display()
        );
    };
    Ok(SessionSummary {
        id: session_id.to_string(),
        session_dir: session_dir.to_string_lossy().into_owned(),
        work_dir: std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
        title: None,
        is_custom_title: false,
        archived: false,
        last_prompt: None,
        first_prompt: None,
        created_at: 0,
        updated_at: 0,
        forked_from: None,
    })
}

/// Default debug-bundle zip name under the system temp dir:
/// `kkagent-debug-<session8>-<YYYYmmdd-HHMMSS>.zip`.
pub fn default_debug_zip_path(session_id: &str) -> PathBuf {
    let short = &session_id[..session_id.len().min(8)];
    let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    std::env::temp_dir().join(format!("kkagent-debug-{short}-{ts}.zip"))
}

/// Export a session debug bundle (see [`export_session_debug_bundle`]) and
/// package it as a single zip at `output_zip`. The bundle is staged in a
/// temporary directory next to the final zip (same filesystem, reliable
/// cleanup) and removed after archiving.
pub fn export_session_debug_bundle_zip(
    summary: &SessionSummary,
    output_zip: impl AsRef<Path>,
) -> anyhow::Result<(PathBuf, DebugExportManifest)> {
    export_session_debug_bundle_zip_with_secrets(summary, output_zip, &collect_config_secrets())
}

/// Testable core of [`export_session_debug_bundle_zip`]: `secrets` is the
/// explicit allow-list of literal values to scrub before archiving.
fn export_session_debug_bundle_zip_with_secrets(
    summary: &SessionSummary,
    output_zip: impl AsRef<Path>,
    secrets: &[String],
) -> anyhow::Result<(PathBuf, DebugExportManifest)> {
    let output_zip = output_zip.as_ref().to_path_buf();
    let staging = staging_dir_for(&output_zip);
    let mut result = export_session_debug_bundle(summary, &staging);
    let sanitized_files = sanitize_bundle_secrets(&staging, secrets);
    if let Ok(result) = result.as_mut() {
        result.manifest.notes.push(format!(
            "Sanitized {sanitized_files} file(s): known API keys and URL key= parameters redacted"
        ));
        result.manifest.notes.push(
            "Redaction is best-effort: review bundle contents for other sensitive text \
             (e.g. strings you typed into the session) before sharing"
                .into(),
        );
    }
    let zip_result = result
        .map(|r| (r, staging.clone()))
        .and_then(|(r, staging)| zip_directory(&staging, &output_zip).map(|_| r));
    let _ = std::fs::remove_dir_all(&staging);
    let manifest = zip_result?.manifest;
    Ok((output_zip, manifest))
}

/// Resolve a session by id and export its debug bundle as a zip, defaulting
/// the output path to [`default_debug_zip_path`].
pub fn export_session_debug_bundle_zip_by_id(
    session_id: &str,
    output_zip: Option<&Path>,
) -> anyhow::Result<(PathBuf, DebugExportManifest)> {
    let summary = find_session_summary(session_id)?;
    let path = output_zip
        .map(Path::to_path_buf)
        .unwrap_or_else(|| default_debug_zip_path(session_id));
    export_session_debug_bundle_zip(&summary, &path)
}

fn staging_dir_for(output_zip: &Path) -> PathBuf {
    let parent = output_zip.parent().unwrap_or(Path::new("."));
    let name = output_zip
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "bundle.zip".into());
    parent.join(format!(".{name}.staging-{}", uuid::Uuid::new_v4()))
}

const SECRET_PLACEHOLDER: &str = "<redacted>";

/// Collect secret strings that must never leave the machine in a debug
/// bundle: provider API keys (inline `api_key` and the env vars named by
/// `api_key_env`) and custom header values (auth tokens). Only strings worth
/// scrubbing are kept (short values would mangle ordinary text).
pub fn collect_config_secrets() -> Vec<String> {
    let Ok(config) = kkagent_config::load_config(None) else {
        return Vec::new();
    };
    let mut secrets = Vec::new();
    for provider in config.providers.values() {
        if let Some(key) = provider.api_key.as_deref() {
            secrets.push(key.to_string());
        }
        if let Some(env_name) = provider.api_key_env.as_deref() {
            if let Ok(value) = std::env::var(env_name) {
                secrets.push(value);
            }
        }
        for value in provider.custom_headers.values() {
            secrets.push(value.clone());
        }
    }
    // Custom web-search/fetch service keys follow the same env convention.
    for env_name in [
        "KKAGENT_SEARCH_API_KEY",
        "KKAGENT_FETCH_API_KEY",
        "KIMI_API_KEY",
        "ANTHROPIC_API_KEY",
        "OPENAI_API_KEY",
        "GOOGLE_API_KEY",
        "GEMINI_API_KEY",
    ] {
        if let Ok(value) = std::env::var(env_name) {
            secrets.push(value);
        }
    }
    secrets.into_iter().filter(|s| s.len() >= 8).collect()
}

/// Redact query parameters that commonly carry credentials in URLs (the
/// Google GenAI streaming URL embeds `?key=<api_key>` and reqwest error
/// messages include the full URL, which then lands in kkagent.log).
fn redact_url_secrets(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find("key=") {
        let before = &rest[..pos];
        // Only treat it as a query parameter when preceded by `?` or `&`
        // (possibly with whitespace between).
        let trimmed = before.trim_end();
        let is_param = trimmed.ends_with('?') || trimmed.ends_with('&');
        out.push_str(before);
        if !is_param {
            out.push_str("key=");
            rest = &rest[pos + 4..];
            continue;
        }
        out.push_str("key=");
        out.push_str(SECRET_PLACEHOLDER);
        rest = &rest[pos + 4..];
        let end = rest
            .find(['&', ' ', '"', '\'', '\n', '\r', '\\', ')'])
            .unwrap_or(rest.len());
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// Redact every occurrence of `secret` in `text` with a placeholder.
fn redact_literal(text: &str, secret: &str) -> String {
    text.replace(secret, SECRET_PLACEHOLDER)
}

/// Whether the buffer looks like UTF-8 text (secrets never hide in binary
/// payloads in practice, and blind replacement would corrupt binaries).
fn looks_like_text(bytes: &[u8]) -> bool {
    std::str::from_utf8(bytes).is_ok()
}

/// Best-effort sanitize of every text file under `dir`: known secret
/// literals and `?key=`/`&key=` URL parameters are replaced with
/// `<redacted>`. Returns the number of files modified. Never fails on
/// unreadable files — the export must succeed even when sanitizing does not.
pub fn sanitize_bundle_secrets(dir: &Path, secrets: &[String]) -> usize {
    let mut modified = 0usize;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(ty) = entry.file_type() else { continue };
            if ty.is_dir() {
                stack.push(entry.path());
                continue;
            }
            if !ty.is_file() {
                continue;
            }
            let path = entry.path();
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            if !looks_like_text(&bytes) {
                continue;
            }
            let Ok(mut text) = String::from_utf8(bytes) else {
                continue;
            };
            let original = text.clone();
            for secret in secrets {
                if !secret.is_empty() {
                    text = redact_literal(&text, secret);
                }
            }
            text = redact_url_secrets(&text);
            if text != original && std::fs::write(&path, &text).is_ok() {
                modified += 1;
            }
        }
    }
    modified
}

/// Recursively archive `src` into a zip at `dst`. Directory entries are
/// recorded so empty directories survive; symlinks are skipped.
fn zip_directory(src: &Path, dst: &Path) -> anyhow::Result<()> {
    let file = std::fs::File::create(dst)
        .map_err(|e| anyhow::anyhow!("failed to create zip {}: {e}", dst.display()))?;
    let mut writer = zip::ZipWriter::new(std::io::BufWriter::new(file));
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    add_tree_to_zip(src, "", &mut writer, options)?;
    writer
        .finish()
        .map_err(|e| anyhow::anyhow!("failed to finalize zip {}: {e}", dst.display()))?;
    Ok(())
}

fn add_tree_to_zip(
    src: &Path,
    prefix: &str,
    writer: &mut zip::ZipWriter<std::io::BufWriter<std::fs::File>>,
    options: zip::write::SimpleFileOptions,
) -> anyhow::Result<()> {
    use std::io::Write;

    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let entry_path = format!("{prefix}{name}");
        let ty = entry.file_type()?;
        if ty.is_dir() {
            writer.add_directory(&entry_path, options)?;
            add_tree_to_zip(&entry.path(), &format!("{entry_path}/"), writer, options)?;
        } else if ty.is_file() {
            writer.start_file(&entry_path, options)?;
            let data = std::fs::read(entry.path())?;
            writer.write_all(&data)?;
        }
        // Symlinks and other special files are skipped.
    }
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

/// A half-open time window `[start, end]` parsed from RFC3339 timestamps.
#[derive(Debug, Clone, Copy)]
struct ActivityWindow {
    start: chrono::DateTime<chrono::Utc>,
    end: chrono::DateTime<chrono::Utc>,
}

/// Compute the session's activity window from its transcript messages:
/// first message timestamp (minus a lead-in margin) to last message
/// timestamp (plus a tail margin). Returns `None` when the transcript has no
/// parseable timestamps — the log extract then falls back to id matching only.
fn session_activity_window(session_id: &str) -> Option<ActivityWindow> {
    const LEAD_IN: chrono::Duration = chrono::Duration::minutes(10);
    const TAIL: chrono::Duration = chrono::Duration::minutes(10);

    let db = crate::transcript::TranscriptDb::open_default().ok()?;
    let messages = db.load_messages(session_id).ok()?;
    let mut start: Option<chrono::DateTime<chrono::Utc>> = None;
    let mut end: Option<chrono::DateTime<chrono::Utc>> = None;
    for message in &messages {
        let Ok(ts) = chrono::DateTime::parse_from_rfc3339(&message.created_at) else {
            continue;
        };
        let ts = ts.with_timezone(&chrono::Utc);
        start = Some(start.map_or(ts, |s: chrono::DateTime<chrono::Utc>| s.min(ts)));
        end = Some(end.map_or(ts, |e: chrono::DateTime<chrono::Utc>| e.max(ts)));
    }
    Some(ActivityWindow {
        start: start? - LEAD_IN,
        end: end? + TAIL,
    })
}

/// Extract a diagnostic-log bundle for one session:
/// - every line mentioning `session_id` (verbatim, any age), then
/// - lines whose tracing timestamp falls inside `window` (time-correlated
///   context such as `LLM stream error` lines that lack a session id).
///
/// Duplicates are suppressed: a line captured by the id filter is not
/// repeated by the window filter.
fn filter_log_lines(
    src: &Path,
    session_id: &str,
    window: Option<&ActivityWindow>,
    dst: &Path,
) -> anyhow::Result<usize> {
    use std::io::{BufRead, BufWriter, Write};

    anyhow::ensure!(!session_id.is_empty(), "session id must not be empty");
    let input = std::fs::File::open(src)?;
    let reader = std::io::BufReader::new(input);
    let output = std::fs::File::create(dst)?;
    let mut writer = BufWriter::new(output);
    let mut kept = 0usize;

    // First pass: id-matching lines, in order.
    let mut window_pending: Vec<String> = Vec::new();
    for line in reader.lines() {
        let Ok(line) = line else { continue };
        if line.contains(session_id) {
            writeln!(writer, "{line}")?;
            kept += 1;
        } else if window.is_some() {
            window_pending.push(line);
        }
    }

    // Second pass: window-correlated lines (session id absent from the line).
    if let Some(window) = window {
        for line in window_pending {
            let Some(ts) = tracing_line_timestamp(&line) else {
                continue;
            };
            if ts >= window.start && ts <= window.end {
                writeln!(writer, "{line}")?;
                kept += 1;
            }
        }
    }
    writer.flush()?;
    Ok(kept)
}

/// Parse the RFC3339 UTC timestamp prefix of a tracing log line
/// (`2026-09-05T08:46:12.613044Z  INFO ...`). Returns `None` for lines
/// without a recognizable timestamp.
fn tracing_line_timestamp(line: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    let end = line.find(|c: char| c.is_whitespace()).unwrap_or(line.len());
    chrono::DateTime::parse_from_rfc3339(&line[..end])
        .ok()
        .map(|ts| ts.with_timezone(&chrono::Utc))
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
        let kept = filter_log_lines(&src, "sid", None, &dst).unwrap();
        assert_eq!(kept, 0);
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn log_extract_includes_time_window_lines_without_session_id() {
        let dir =
            std::env::temp_dir().join(format!("kkagent-export-test-win-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("kkagent.log");
        std::fs::write(
            &src,
            concat!(
                "2026-09-05T08:20:00.000000Z  INFO kkagent: startup unrelated-session line\n",
                "2026-09-05T08:26:30.000000Z  INFO kkagent: Auto-resuming session session_id=sid-1\n",
                "2026-09-05T08:27:00.000000Z ERROR sid: LLM stream error: decode/timeout no session id\n",
                "2026-09-05T08:28:00.000000Z  INFO kkagent: window line without id or level marker\n",
                "2027-01-01T00:00:00.000000Z ERROR sid: LLM stream error: far outside the window\n",
                "not-a-timestamp-line sid\n",
            ),
        )
        .unwrap();
        let dst = dir.join("extract.log");
        let window = ActivityWindow {
            start: chrono::Utc::now() - chrono::Duration::hours(1),
            end: chrono::Utc::now() + chrono::Duration::hours(1),
        };
        let kept = filter_log_lines(&src, "sid-1", Some(&window), &dst).unwrap();
        let body = std::fs::read_to_string(&dst).unwrap();

        // The resume line carries the session id.
        assert!(body.contains("Auto-resuming session session_id=sid-1"));
        // The id-less transport error inside the window is captured by time.
        assert!(
            body.contains("LLM stream error: decode/timeout no session id"),
            "window extraction must keep id-less error lines: {body}"
        );
        // The same error far outside the window must not leak in.
        assert!(!body.contains("far outside the window"));
        // Timestamp-less lines are never window-matched; other sessions' ids
        // ("sid") must not be captured by this session's filter ("sid-1").
        assert!(!body.contains("not-a-timestamp-line sid"));
        // The 08:28 id-less line is inside the window and does carry a
        // timestamp, so it is expected to be captured by time correlation.
        // The 08:20 startup line is likewise inside the window (now ± 1h).
        assert!(body.contains("window line without id"));
        assert_eq!(kept, 4);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tracing_timestamp_parsing() {
        assert!(tracing_line_timestamp("2026-09-05T08:46:12.613044Z  INFO x: y").is_some());
        assert!(tracing_line_timestamp("not a timestamp").is_none());
        assert!(tracing_line_timestamp("").is_none());
    }

    #[test]
    fn debug_bundle_zip_round_trips_all_files() {
        let root =
            std::env::temp_dir().join(format!("kkagent-export-zip-{}", uuid::Uuid::new_v4()));
        let session_dir = root.join("session-source");
        std::fs::create_dir_all(session_dir.join("plans")).unwrap();
        std::fs::write(session_dir.join("state.json"), "{\"ok\":true}").unwrap();
        std::fs::write(session_dir.join("plans/plan.md"), "# plan\n").unwrap();
        std::fs::write(session_dir.join("plans/.empty-dir-marker"), "").unwrap();
        let zip_path = root.join("bundle.zip");

        // manifest.json is always written into the bundle (not listed in
        // manifest.files, which only covers the artifacts it describes).
        let (path, manifest) =
            export_session_debug_bundle_zip(&summary_for_export(&session_dir), &zip_path).unwrap();
        assert_eq!(path, zip_path);
        assert!(zip_path.is_file());
        // The staging directory must not linger next to the zip.
        let siblings: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".bundle.zip.staging-"))
            .collect();
        assert!(
            siblings.is_empty(),
            "staging dirs left behind: {siblings:?}"
        );

        // Round-trip: every manifest entry must be present inside the zip.
        let file = std::fs::File::open(&zip_path).unwrap();
        let mut archive = zip::ZipArchive::new(std::io::BufReader::new(file)).unwrap();
        for entry in archive.file_names() {
            assert!(!entry.starts_with('/'), "absolute zip entry: {entry}");
            assert!(!entry.contains(".."), "traversal zip entry: {entry}");
        }
        let names: Vec<String> = archive.file_names().map(str::to_string).collect();
        assert!(names.contains(&"manifest.json".to_string()));
        for file in &manifest.files {
            assert!(names.contains(file), "zip missing manifest entry: {file}");
        }
        assert!(names.contains(&"session/state.json".to_string()));
        assert!(names.contains(&"session/plans/plan.md".to_string()));

        // Spot-check content survives compression.
        let mut entry = archive.by_name("session/state.json").unwrap();
        let mut body = String::new();
        std::io::Read::read_to_string(&mut entry, &mut body).unwrap();
        assert_eq!(body, "{\"ok\":true}");

        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn default_debug_zip_path_lives_in_temp_dir_with_short_id() {
        let path = default_debug_zip_path("12345678-abcd");
        assert!(path.starts_with(std::env::temp_dir()));
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("kkagent-debug-12345678-"), "{name}");
        assert!(name.ends_with(".zip"), "{name}");
        // Short ids must not panic on slicing.
        let short = default_debug_zip_path("ab");
        assert!(short
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("kkagent-debug-ab-"));
    }

    #[test]
    fn find_session_summary_errors_on_unknown_id() {
        let err = find_session_summary("").unwrap_err();
        assert!(err.to_string().contains("must not be empty"));
        let err = find_session_summary("kkagent-nonexistent-session").unwrap_err();
        assert!(err.to_string().contains("session not found"));
    }

    #[test]
    fn sanitize_redacts_known_secrets_and_url_key_params() {
        let dir = std::env::temp_dir().join(format!("kkagent-sanitize-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("nested")).unwrap();
        let log = dir.join("kkagent.log");
        std::fs::write(
            &log,
            "LLM transport error: url=https://api.example.com/v1beta/models/gemini:streamGenerateContent?alt=sse&key=AIzaSyD-1234567890abcdef [kind=timeout]\n\
             auth header was Bearer sk-ant-api03-real-secret-value-123\n",
        )
        .unwrap();
        let other = dir.join("nested/notes.txt");
        std::fs::write(&other, "no secrets here, just key= in plain text\n").unwrap();

        let secrets = vec!["sk-ant-api03-real-secret-value-123".to_string()];
        let modified = sanitize_bundle_secrets(&dir, &secrets);
        assert_eq!(modified, 1, "only the log should change");

        let log_body = std::fs::read_to_string(&log).unwrap();
        assert!(
            log_body.contains("&key=<redacted>"),
            "url key param must be redacted: {log_body}"
        );
        assert!(
            log_body.contains("Bearer <redacted>"),
            "known secret must be redacted: {log_body}"
        );
        assert!(!log_body.contains("AIzaSyD-1234567890abcdef"));
        assert!(!log_body.contains("sk-ant-api03-real-secret-value-123"));

        // Plain-text `key=` (not a query param) must survive untouched.
        let other_body = std::fs::read_to_string(&other).unwrap();
        assert_eq!(
            other_body, "no secrets here, just key= in plain text\n",
            "non-param key= occurrences must not be redacted"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sanitize_skips_binary_files() {
        let dir =
            std::env::temp_dir().join(format!("kkagent-sanitize-bin-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("image.bin");
        let payload: Vec<u8> = vec![0xFF, 0xFE, b'k', b'e', b'y', b'=', 0x00, 0x01];
        std::fs::write(&bin, &payload).unwrap();

        let modified = sanitize_bundle_secrets(&dir, &["whatever-secret".to_string()]);
        assert_eq!(modified, 0);
        assert_eq!(
            std::fs::read(&bin).unwrap(),
            payload,
            "binary must be untouched"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn redact_url_secrets_handles_multiple_params_and_line_end() {
        let text = "a?key=one&key=two b?x=1&key=three\nc url?friendlykey=keep";
        let out = redact_url_secrets(text);
        assert_eq!(
            out,
            "a?key=<redacted>&key=<redacted> b?x=1&key=<redacted>\nc url?friendlykey=keep"
        );
    }

    #[test]
    fn debug_bundle_zip_sanitizes_secrets_before_archiving() {
        let root =
            std::env::temp_dir().join(format!("kkagent-export-sec-{}", uuid::Uuid::new_v4()));
        let session_dir = root.join("session-source");
        std::fs::create_dir_all(&session_dir).unwrap();
        // The fake secret below must never appear in the zipped log; simulate
        // a leaked log inside the session directory.
        std::fs::write(
            session_dir.join("leaked.log"),
            "transport error: Bearer sk-test-secret-abcdef1234 [kind=timeout]\n",
        )
        .unwrap();
        let zip_path = root.join("bundle.zip");

        // Drive the sanitize-then-zip path with an explicit secret list.
        let (_, manifest) = export_session_debug_bundle_zip_with_secrets(
            &summary_for_export(&session_dir),
            &zip_path,
            &["sk-test-secret-abcdef1234".to_string()],
        )
        .unwrap();
        assert!(zip_path.is_file());
        assert!(
            manifest
                .notes
                .iter()
                .any(|n| n.starts_with("Sanitized") || n.contains("redacted")),
            "manifest should mention sanitization: {:?}",
            manifest.notes
        );

        let file = std::fs::File::open(&zip_path).unwrap();
        let mut archive = zip::ZipArchive::new(std::io::BufReader::new(file)).unwrap();
        let mut entry = archive
            .by_name("session/leaked.log")
            .expect("leaked.log must be inside the zip");
        let mut body = String::new();
        std::io::Read::read_to_string(&mut entry, &mut body).unwrap();
        assert!(
            !body.contains("sk-test-secret-abcdef1234"),
            "secret must not survive into the zip when configured"
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
