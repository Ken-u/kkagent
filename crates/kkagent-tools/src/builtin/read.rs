use crate::path_policy::{is_sensitive_path, looks_binary_ext, sniff_binary, MAX_LINE_LENGTH};
use crate::{Tool, ToolContext, ToolOutput};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::Path;
use tokio::io::AsyncReadExt;

pub struct ReadTool;

/// Window returned when the caller gives no `limit`: enough for most source
/// files without dumping the first 100KB of a large one into the context.
const DEFAULT_READ_LINES: usize = 400;
const MAX_READ_LINES: usize = 1000;
const MAX_READ_BYTES: usize = 100 * 1024;
/// A batch (`files`) read stops adding files once this much text is collected.
const MAX_BATCH_BYTES: usize = 160 * 1024;
const MAX_BATCH_FILES: usize = 8;

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "Read"
    }
    fn description(&self) -> &str {
        "Read a text file as numbered lines. Without a limit it returns the first 400 lines and \
says how to continue (offset=…; limit up to 1000). files:[{path,offset?,limit?}] reads several \
files in one call. Rejects binary/image files — use ReadMediaFile for media."
    }
    fn read_only(&self) -> bool {
        true
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to a text file. Relative paths resolve against the working directory; a path outside the working directory must be absolute."
                },
                "offset": {
                    "type": "integer",
                    "description": "Starting line (1-indexed; negative counts from end)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Number of lines to read (default 400, max 1000)"
                },
                "files": {
                    "type": "array",
                    "description": "Read several files in one call (max 8) instead of consecutive Read calls.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "path": {"type": "string"},
                            "offset": {"type": "integer"},
                            "limit": {"type": "integer"}
                        },
                        "required": ["path"]
                    }
                }
            }
        })
    }

    fn normalize_input(&self, mut input: Value) -> Value {
        let Some(obj) = input.as_object_mut() else {
            return input;
        };
        if !obj.contains_key("path") {
            if let Some(path) = obj.remove("file_path") {
                obj.insert("path".into(), path);
            }
        }
        // `paths: ["a", "b"]` and bare strings in `files` are batch reads.
        if let Some(paths) = obj.remove("paths") {
            obj.entry("files").or_insert(paths);
        }
        if let Some(Value::Array(items)) = obj.get_mut("files") {
            for item in items.iter_mut() {
                if let Value::String(path) = item {
                    *item = json!({ "path": path.clone() });
                }
            }
        }
        input
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let input = self.normalize_input(input);
        let files = input
            .get("files")
            .and_then(Value::as_array)
            .filter(|files| !files.is_empty());
        let Some(files) = files else {
            let path_str = input
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("Missing 'path' parameter"))?;
            return read_one(ctx, path_str, window_of(&input)).await;
        };
        // `path` next to `files` is simply the first file.
        let mut requests: Vec<(String, Window)> = Vec::new();
        if let Some(path) = input.get("path").and_then(Value::as_str) {
            requests.push((path.to_string(), window_of(&input)));
        }
        for file in files {
            let Some(path) = file.get("path").and_then(Value::as_str) else {
                return Ok(ToolOutput::error("Each entry in `files` needs a `path`"));
            };
            requests.push((path.to_string(), window_of(file)));
        }
        read_batch(ctx, requests).await
    }
}

/// Requested line window of one file.
#[derive(Clone, Copy)]
struct Window {
    offset: i64,
    /// `None`: the caller gave no limit, so the default window applies.
    limit: Option<usize>,
}

fn window_of(input: &Value) -> Window {
    Window {
        offset: input
            .get("offset")
            .or_else(|| input.get("line_offset"))
            .and_then(|v| v.as_i64())
            .unwrap_or(1),
        limit: input
            .get("limit")
            .or_else(|| input.get("n_lines"))
            .and_then(|v| v.as_u64())
            .map(|n| n as usize),
    }
}

/// Several files in one result: a `==> path <==` header per file, a combined
/// size budget, and per-file hashes in `data.files` for the stale-write gate.
async fn read_batch(
    ctx: &ToolContext,
    requests: Vec<(String, Window)>,
) -> anyhow::Result<ToolOutput> {
    if requests.len() > MAX_BATCH_FILES {
        return Ok(ToolOutput::error(format!(
            "At most {MAX_BATCH_FILES} files per Read call; split the list."
        )));
    }
    let mut sections = Vec::new();
    let mut notes = Vec::new();
    let mut entries = Vec::new();
    let mut total_bytes = 0usize;
    let mut failed = 0usize;
    let mut skipped = Vec::new();
    for (path, window) in requests {
        if total_bytes >= MAX_BATCH_BYTES {
            skipped.push(path);
            continue;
        }
        let output = read_one(ctx, &path, window).await?;
        total_bytes += output.content.len();
        if output.is_error {
            failed += 1;
            sections.push(format!("==> {path} <==\nerror: {}", output.content));
            continue;
        }
        if let Some(note) = &output.note {
            notes.push(format!("{path}: {note}"));
        }
        let mut entry = output.data.clone().unwrap_or_else(|| json!({}));
        entry["path"] = json!(path);
        entries.push(entry);
        sections.push(format!("==> {path} <==\n{}", output.content));
    }
    if !skipped.is_empty() {
        notes.push(format!(
            "Batch size limit reached; not read: {}. Read them in a separate call.",
            skipped.join(", ")
        ));
    }
    if failed > 0 && entries.is_empty() {
        return Ok(ToolOutput::error(sections.join("\n\n")));
    }
    let mut out = ToolOutput::success_with_data(sections.join("\n\n"), json!({ "files": entries }));
    if !notes.is_empty() {
        out = out.with_note(notes.join(" "));
    }
    Ok(out)
}

async fn read_one(ctx: &ToolContext, path_str: &str, window: Window) -> anyhow::Result<ToolOutput> {
    let path = if Path::new(path_str).is_absolute() {
        std::path::PathBuf::from(path_str)
    } else {
        ctx.working_dir.join(path_str)
    };

    if !path.exists() {
        return Ok(ToolOutput::error(format!("File not found: {}", path_str)));
    }
    // S1-4: workspace directory constraint. Exact match to the session's
    // plan file is a read-only exception so orchestrator / Plan-mode plans
    // under ~/.kkagent/sessions/... remain readable without opening the
    // whole session directory.
    let is_plan_file = ctx.plan_file_path.as_ref().is_some_and(|plan| {
        path == *plan
            || match (std::fs::canonicalize(&path), std::fs::canonicalize(plan)) {
                (Ok(a), Ok(b)) => a == b,
                _ => false,
            }
    });
    if !is_plan_file {
        if let Err(reason) = ctx.check_path_guard(&path) {
            return Ok(ToolOutput::error(reason));
        }
    }
    if looks_binary_ext(&path) {
        return Ok(ToolOutput::error(format!(
            "Refusing to Read binary/media file `{}`. Use ReadMediaFile instead.",
            path_str
        )));
    }
    // S2-6: sensitive path check can be disabled via config
    if ctx.sensitive_check_enabled() && is_sensitive_path(&path) {
        return Ok(ToolOutput::error(format!(
                "Refusing to read sensitive file `{}`. Ask the user to provide only the specific non-secret value needed.",
                path_str
            )));
    }

    let analysis = match analyze_file(&path).await {
        Ok(analysis) => analysis,
        Err(error) => {
            return Ok(ToolOutput::error(format!(
                "{}: {}. Use ReadMediaFile for media/binary.",
                path_str, error
            )));
        }
    };
    let total_lines = analysis.total_lines;

    let offset = window.offset;
    let n_lines = window
        .limit
        .unwrap_or(DEFAULT_READ_LINES)
        .min(MAX_READ_LINES);

    let start = if offset < 0 {
        (total_lines as i64 + offset).max(0) as usize
    } else {
        (offset as usize).saturating_sub(1)
    };

    // An offset beyond EOF is a valid empty page, never a slicing panic.
    let start = start.min(total_lines);
    let requested_end = start.saturating_add(n_lines).min(total_lines);
    let rendered = render_range(&path, analysis.encoding, start, requested_end).await?;
    let mut selected = Vec::new();
    let mut output_bytes = 0usize;
    let mut end = start;
    let mut line_truncated = false;
    for line in rendered {
        let suffix = if line.truncated {
            line_truncated = true;
            "…"
        } else {
            ""
        };
        let rendered = format!("{:>6}|{}{}", line.index + 1, line.text, suffix);
        let separator = usize::from(!selected.is_empty());
        if output_bytes + separator + rendered.len() > MAX_READ_BYTES {
            break;
        }
        output_bytes += separator + rendered.len();
        end = line.index + 1;
        selected.push(rendered);
    }

    let result = selected.join("\n");
    let mut note_parts = Vec::new();
    if end < total_lines {
        note_parts.push(format!(
            "{} lines read from file starting from line {}. Total lines in file: {}. \
More lines remain — call Read again with offset={}{}.",
            end.saturating_sub(start),
            start + 1,
            total_lines,
            end + 1,
            if window.limit.is_none() {
                " (limit up to 1000 for a larger window; use Grep to find a specific part)"
            } else {
                ""
            }
        ));
    } else if start > 0 || end < total_lines || n_lines < total_lines {
        note_parts.push(format!(
            "{} lines read from file starting from line {}. Total lines in file: {}.",
            end.saturating_sub(start),
            start + 1,
            total_lines
        ));
    }
    if line_truncated {
        note_parts.push(format!(
            "Some lines exceeded {MAX_LINE_LENGTH} characters and were truncated for display."
        ));
    }
    if analysis.contains_cr && !analysis.contains_crlf {
        note_parts
            .push("File contains bare CR line endings; displayed with LF normalization.".into());
    }

    let mut out = ToolOutput::success_with_data(
        result,
        json!({
            "path": path_str,
            "lineCount": total_lines,
            "bytes": analysis.bytes,
            "startLine": if end > start { start + 1 } else { start },
            "endLine": end,
            "content_hash": analysis.content_hash,
        }),
    );
    if !note_parts.is_empty() {
        out = out.with_note(note_parts.join(" "));
    }
    Ok(out)
}

#[derive(Clone, Copy)]
enum TextEncoding {
    Utf8,
    Utf16Le { bom: bool },
    Utf16Be { bom: bool },
}

struct FileAnalysis {
    encoding: TextEncoding,
    total_lines: usize,
    bytes: u64,
    content_hash: String,
    contains_cr: bool,
    contains_crlf: bool,
}

struct RenderedLine {
    index: usize,
    text: String,
    truncated: bool,
}

async fn analyze_file(path: &Path) -> Result<FileAnalysis, String> {
    use sha2::{Digest, Sha256};

    let mut sample_file = tokio::fs::File::open(path)
        .await
        .map_err(|error| format!("Failed to read file: {error}"))?;
    let mut sample = vec![0u8; 512];
    let sample_len = sample_file
        .read(&mut sample)
        .await
        .map_err(|error| format!("Failed to read file: {error}"))?;
    sample.truncate(sample_len);
    let encoding = detect_encoding(&sample)?;

    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|error| format!("Failed to read file: {error}"))?;
    let mut hash = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut bytes = 0u64;
    let mut newlines = 0usize;
    let mut contains_cr = false;
    let mut contains_crlf = false;
    let mut last_was_cr = false;
    let mut last_unit = None;
    let mut skip = match encoding {
        TextEncoding::Utf16Le { bom: true } | TextEncoding::Utf16Be { bom: true } => 2,
        _ => 0,
    };
    let mut odd_byte = None;
    loop {
        let n = file
            .read(&mut buf)
            .await
            .map_err(|error| format!("Failed to read file: {error}"))?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
        bytes = bytes.saturating_add(n as u64);
        let begin = skip.min(n);
        skip -= begin;
        let chunk = &buf[begin..n];
        match encoding {
            TextEncoding::Utf8 => {
                for &byte in chunk {
                    if byte == 0 {
                        return Err("File appears to be binary".into());
                    }
                    if byte == b'\n' {
                        newlines = newlines.saturating_add(1);
                        contains_crlf |= last_was_cr;
                    }
                    contains_cr |= byte == b'\r';
                    last_was_cr = byte == b'\r';
                    last_unit = Some(byte as u16);
                }
            }
            TextEncoding::Utf16Le { .. } | TextEncoding::Utf16Be { .. } => {
                let little = matches!(encoding, TextEncoding::Utf16Le { .. });
                for &byte in chunk {
                    if let Some(first) = odd_byte.take() {
                        let unit = if little {
                            u16::from_le_bytes([first, byte])
                        } else {
                            u16::from_be_bytes([first, byte])
                        };
                        if unit == b'\n' as u16 {
                            newlines = newlines.saturating_add(1);
                            contains_crlf |= last_was_cr;
                        }
                        contains_cr |= unit == b'\r' as u16;
                        last_was_cr = unit == b'\r' as u16;
                        last_unit = Some(unit);
                    } else {
                        odd_byte = Some(byte);
                    }
                }
            }
        }
    }
    let total_lines = if bytes == 0 || last_unit.is_none() {
        0
    } else {
        newlines + usize::from(last_unit != Some(b'\n' as u16))
    };
    let digest = hash.finalize();
    Ok(FileAnalysis {
        encoding,
        total_lines,
        bytes,
        content_hash: digest.iter().map(|byte| format!("{byte:02x}")).collect(),
        contains_cr,
        contains_crlf,
    })
}

fn detect_encoding(sample: &[u8]) -> Result<TextEncoding, String> {
    if sample.starts_with(&[0xFF, 0xFE]) {
        return Ok(TextEncoding::Utf16Le { bom: true });
    }
    if sample.starts_with(&[0xFE, 0xFF]) {
        return Ok(TextEncoding::Utf16Be { bom: true });
    }
    if sample.len() >= 4 {
        let even_nul = sample.iter().step_by(2).filter(|&&byte| byte == 0).count();
        let odd_nul = sample
            .iter()
            .skip(1)
            .step_by(2)
            .filter(|&&byte| byte == 0)
            .count();
        let pairs = sample.len() / 2;
        if even_nul * 2 > pairs {
            return Ok(TextEncoding::Utf16Be { bom: false });
        }
        if odd_nul * 2 > pairs {
            return Ok(TextEncoding::Utf16Le { bom: false });
        }
    }
    if sniff_binary(sample) {
        return Err("File appears to be binary".into());
    }
    Ok(TextEncoding::Utf8)
}

async fn render_range(
    path: &Path,
    encoding: TextEncoding,
    start: usize,
    end: usize,
) -> anyhow::Result<Vec<RenderedLine>> {
    if start >= end {
        return Ok(Vec::new());
    }
    let mut file = tokio::fs::File::open(path).await?;
    let mut buf = [0u8; 64 * 1024];
    let mut skip = match encoding {
        TextEncoding::Utf16Le { bom: true } | TextEncoding::Utf16Be { bom: true } => 2,
        _ => 0,
    };
    let mut line_index = 0usize;
    let mut raw = Vec::new();
    let mut raw_units = Vec::new();
    let mut line_overflow = false;
    let mut odd_byte = None;
    let mut output = Vec::with_capacity(end.saturating_sub(start));
    'read: loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        let begin = skip.min(n);
        skip -= begin;
        let chunk = &buf[begin..n];
        match encoding {
            TextEncoding::Utf8 => {
                for &byte in chunk {
                    if byte == b'\n' {
                        if line_index >= start {
                            finish_utf8_line(&mut output, line_index, &mut raw, line_overflow);
                        }
                        line_index += 1;
                        raw.clear();
                        line_overflow = false;
                        if line_index >= end {
                            break 'read;
                        }
                    } else if line_index >= start {
                        if raw.len() < MAX_LINE_LENGTH.saturating_mul(4).saturating_add(4) {
                            raw.push(byte);
                        } else {
                            line_overflow = true;
                        }
                    }
                }
            }
            TextEncoding::Utf16Le { .. } | TextEncoding::Utf16Be { .. } => {
                let little = matches!(encoding, TextEncoding::Utf16Le { .. });
                for &byte in chunk {
                    let Some(first) = odd_byte.take() else {
                        odd_byte = Some(byte);
                        continue;
                    };
                    let unit = if little {
                        u16::from_le_bytes([first, byte])
                    } else {
                        u16::from_be_bytes([first, byte])
                    };
                    if unit == b'\n' as u16 {
                        if line_index >= start {
                            finish_utf16_line(
                                &mut output,
                                line_index,
                                &mut raw_units,
                                line_overflow,
                            );
                        }
                        line_index += 1;
                        raw_units.clear();
                        line_overflow = false;
                        if line_index >= end {
                            break 'read;
                        }
                    } else if line_index >= start {
                        if raw_units.len() < MAX_LINE_LENGTH.saturating_mul(2).saturating_add(2) {
                            raw_units.push(unit);
                        } else {
                            line_overflow = true;
                        }
                    }
                }
            }
        }
    }
    if line_index < end && line_index >= start {
        match encoding {
            TextEncoding::Utf8 => {
                finish_utf8_line(&mut output, line_index, &mut raw, line_overflow)
            }
            TextEncoding::Utf16Le { .. } | TextEncoding::Utf16Be { .. } => {
                finish_utf16_line(&mut output, line_index, &mut raw_units, line_overflow)
            }
        }
    }
    Ok(output)
}

fn finish_utf8_line(
    output: &mut Vec<RenderedLine>,
    index: usize,
    raw: &mut Vec<u8>,
    overflow: bool,
) {
    if raw.last() == Some(&b'\r') {
        raw.pop();
    }
    let decoded = String::from_utf8_lossy(raw);
    let mut chars = decoded.chars();
    let text: String = chars.by_ref().take(MAX_LINE_LENGTH).collect();
    output.push(RenderedLine {
        index,
        text,
        truncated: overflow || chars.next().is_some(),
    });
}

fn finish_utf16_line(
    output: &mut Vec<RenderedLine>,
    index: usize,
    raw: &mut Vec<u16>,
    overflow: bool,
) {
    if raw.last() == Some(&(b'\r' as u16)) {
        raw.pop();
    }
    let decoded = String::from_utf16_lossy(raw);
    let mut chars = decoded.chars();
    let text: String = chars.by_ref().take(MAX_LINE_LENGTH).collect();
    output.push(RenderedLine {
        index,
        text,
        truncated: overflow || chars.next().is_some(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(dir: &std::path::Path) -> ToolContext {
        ToolContext {
            working_dir: dir.to_path_buf(),
            session_id: "read-test".into(),
            turn_id: "test-turn".into(),
            plan_file_path: None,
            image: kkagent_config::ImageConfig::default(),
            tool_call_id: None,
            interrupted: None,
            tools_config: kkagent_config::ToolsConfig::default(),
            model_alias: None,
        }
    }

    #[tokio::test]
    async fn offset_beyond_eof_returns_empty_page() {
        let dir = std::env::temp_dir().join(format!("kkagent-read-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("short.txt"), "one\ntwo\n").unwrap();
        let output = ReadTool
            .execute(json!({"path": "short.txt", "offset": 99}), &context(&dir))
            .await
            .unwrap();
        assert!(!output.is_error);
        assert!(output.content.is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn accepts_legacy_line_offset_alias() {
        let dir = std::env::temp_dir().join(format!("kkagent-read-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("short.txt"), "one\ntwo\nthree\n").unwrap();
        let output = ReadTool
            .execute(
                json!({"path": "short.txt", "line_offset": 2, "n_lines": 1}),
                &context(&dir),
            )
            .await
            .unwrap();
        assert!(!output.is_error);
        assert!(output.content.contains("two"));
        assert!(!output.content.contains("one"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn output_respects_utf8_byte_limit() {
        let dir = std::env::temp_dir().join(format!("kkagent-read-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let content = format!("{}\n", "中".repeat(1800)).repeat(40);
        std::fs::write(dir.join("unicode.txt"), content).unwrap();
        let output = ReadTool
            .execute(json!({"path": "unicode.txt"}), &context(&dir))
            .await
            .unwrap();
        assert!(!output.is_error);
        assert!(output.content.len() <= MAX_READ_BYTES);
        assert!(std::str::from_utf8(output.content.as_bytes()).is_ok());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn truncation_uses_note_side_channel() {
        let dir = std::env::temp_dir().join(format!("kkagent-read-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let body = (1..=50)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(dir.join("many.txt"), body).unwrap();
        let output = ReadTool
            .execute(
                json!({"path": "many.txt", "offset": 1, "limit": 10}),
                &context(&dir),
            )
            .await
            .unwrap();
        assert!(!output.is_error);
        assert!(!output.content.contains("more lines"));
        assert!(output.note.as_deref().unwrap_or("").contains("Total lines"));
        let model = output.model_content();
        assert!(model.contains("<system>"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn strict_path_guard_allows_exact_plan_file_outside_workspace() {
        let workspace =
            std::env::temp_dir().join(format!("kkagent-read-ws-{}", uuid::Uuid::new_v4()));
        let session =
            std::env::temp_dir().join(format!("kkagent-read-sess-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).unwrap();
        let plan = session
            .join("agents")
            .join("main")
            .join("plans")
            .join("plan.md");
        std::fs::create_dir_all(plan.parent().unwrap()).unwrap();
        std::fs::write(&plan, "# orchestrator plan\n").unwrap();

        let tools_config = kkagent_config::ToolsConfig {
            path_guard_mode: "strict".into(),
            ..Default::default()
        };
        let ctx = ToolContext {
            working_dir: workspace.clone(),
            session_id: "s".into(),
            turn_id: "t".into(),
            plan_file_path: Some(plan.clone()),
            image: kkagent_config::ImageConfig::default(),
            tool_call_id: None,
            interrupted: None,
            tools_config,
            model_alias: None,
        };

        let denied = ReadTool
            .execute(
                json!({"path": session.join("agents/main/plans/other.md")}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(denied.is_error); // missing file

        let sibling = session
            .join("agents")
            .join("main")
            .join("plans")
            .join("other.md");
        std::fs::write(&sibling, "secret").unwrap();
        let denied = ReadTool
            .execute(json!({"path": sibling}), &ctx)
            .await
            .unwrap();
        assert!(denied.is_error);
        assert!(
            denied.content.contains("path_guard_mode") || denied.content.contains("outside"),
            "{}",
            denied.content
        );

        let allowed = ReadTool.execute(json!({"path": plan}), &ctx).await.unwrap();
        assert!(!allowed.is_error, "{}", allowed.content);
        assert!(allowed.content.contains("orchestrator plan"));
        let _ = std::fs::remove_dir_all(&workspace);
        let _ = std::fs::remove_dir_all(&session);
    }

    #[tokio::test]
    async fn streams_large_single_line_with_bounded_output() {
        let dir = std::env::temp_dir().join(format!("kkagent-read-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("large.txt");
        std::fs::write(&path, vec![b'x'; 4 * 1024 * 1024]).unwrap();

        let output = ReadTool
            .execute(json!({"path": "large.txt"}), &context(&dir))
            .await
            .unwrap();
        assert!(!output.is_error);
        assert!(output.content.len() < 16 * 1024);
        assert!(output.content.ends_with('…'));
        assert_eq!(output.data.unwrap()["bytes"], 4 * 1024 * 1024);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn rejects_binary_nul_after_initial_sample() {
        let dir = std::env::temp_dir().join(format!("kkagent-read-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut bytes = vec![b'a'; 2048];
        bytes.push(0);
        std::fs::write(dir.join("late-nul.txt"), bytes).unwrap();

        let output = ReadTool
            .execute(json!({"path": "late-nul.txt"}), &context(&dir))
            .await
            .unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("binary"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn numbered_file(dir: &std::path::Path, name: &str, lines: usize) {
        let body = (1..=lines)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(dir.join(name), body).unwrap();
    }

    fn scratch() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("kkagent-read-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn without_a_limit_only_the_first_400_lines_are_returned() {
        let dir = scratch();
        numbered_file(&dir, "big.txt", 1200);
        numbered_file(&dir, "small.txt", 300);
        let big = ReadTool
            .execute(json!({"path": "big.txt"}), &context(&dir))
            .await
            .unwrap();
        assert_eq!(big.content.lines().count(), 400);
        assert!(big.content.ends_with("line400"));
        let note = big.note.as_deref().unwrap();
        assert!(note.contains("Total lines in file: 1200"), "{note}");
        assert!(note.contains("offset=401"), "{note}");
        assert!(note.contains("limit up to 1000"), "{note}");
        // The continuation hint works: the next window starts at line 401.
        let next = ReadTool
            .execute(json!({"path": "big.txt", "offset": 401}), &context(&dir))
            .await
            .unwrap();
        assert!(
            next.content.starts_with("   401|line401"),
            "{}",
            next.content
        );
        assert_eq!(next.content.lines().count(), 400);
        // A file shorter than the window is returned whole, with no footer.
        let small = ReadTool
            .execute(json!({"path": "small.txt"}), &context(&dir))
            .await
            .unwrap();
        assert_eq!(small.content.lines().count(), 300);
        assert!(small.note.is_none(), "{:?}", small.note);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn an_explicit_limit_is_honoured_up_to_1000_lines() {
        let dir = scratch();
        numbered_file(&dir, "big.txt", 1500);
        let wide = ReadTool
            .execute(json!({"path": "big.txt", "limit": 900}), &context(&dir))
            .await
            .unwrap();
        assert_eq!(wide.content.lines().count(), 900);
        // With an explicit limit the footer does not suggest "a larger window".
        assert!(!wide.note.as_deref().unwrap().contains("limit up to"));
        let capped = ReadTool
            .execute(json!({"path": "big.txt", "limit": 5000}), &context(&dir))
            .await
            .unwrap();
        assert_eq!(capped.content.lines().count(), 1000);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn files_reads_several_files_with_headers_and_per_file_hashes() {
        let dir = scratch();
        std::fs::write(dir.join("a.txt"), "alpha\nbeta\n").unwrap();
        std::fs::write(dir.join("b.txt"), "gamma\n").unwrap();
        numbered_file(&dir, "c.txt", 500);
        let out = ReadTool
            .execute(
                json!({"files": [
                    {"path": "a.txt"},
                    "b.txt",
                    {"path": "c.txt", "offset": 10, "limit": 2},
                    {"path": "missing.txt"}
                ]}),
                &context(&dir),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content.contains("==> a.txt <==\n     1|alpha"),
            "{}",
            out.content
        );
        assert!(out.content.contains("==> b.txt <==\n     1|gamma"));
        assert!(out
            .content
            .contains("==> c.txt <==\n    10|line10\n    11|line11"));
        assert!(out
            .content
            .contains("==> missing.txt <==\nerror: File not found"));
        // One hash per file that was read, matching the single-file result.
        let files = out.data.as_ref().unwrap()["files"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(files.len(), 3);
        let single = ReadTool
            .execute(json!({"path": "a.txt"}), &context(&dir))
            .await
            .unwrap();
        assert_eq!(files[0]["path"], "a.txt");
        assert_eq!(
            files[0]["content_hash"],
            single.data.as_ref().unwrap()["content_hash"]
        );
        // Every file failing is an error.
        let none = ReadTool
            .execute(
                json!({"files": [{"path": "nope-1"}, {"path": "nope-2"}]}),
                &context(&dir),
            )
            .await
            .unwrap();
        assert!(none.is_error);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn batch_reads_respect_the_file_count_and_size_budget() {
        let dir = scratch();
        for i in 0..9 {
            std::fs::write(dir.join(format!("f{i}.txt")), "x\n").unwrap();
        }
        let too_many = ReadTool
            .execute(
                json!({"files": (0..9).map(|i| json!({"path": format!("f{i}.txt")})).collect::<Vec<_>>()}),
                &context(&dir),
            )
            .await
            .unwrap();
        assert!(too_many.is_error);
        assert!(too_many.content.contains("At most 8"));

        // ~98KB per file: the first two fit in the 160KB budget, the third is deferred.
        let wide = format!("{}\n", "y".repeat(120)).repeat(800);
        for name in ["w1.txt", "w2.txt", "w3.txt"] {
            std::fs::write(dir.join(name), &wide).unwrap();
        }
        let out = ReadTool
            .execute(
                json!({"files": [
                    {"path": "w1.txt", "limit": 800},
                    {"path": "w2.txt", "limit": 800},
                    {"path": "w3.txt", "limit": 800}
                ]}),
                &context(&dir),
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("==> w1.txt <=="));
        assert!(out.content.contains("==> w2.txt <=="));
        assert!(!out.content.contains("==> w3.txt <=="));
        assert!(out.note.as_deref().unwrap().contains("not read: w3.txt"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn path_aliases_and_plural_forms_are_accepted() {
        let dir = scratch();
        std::fs::write(dir.join("a.txt"), "alpha\n").unwrap();
        std::fs::write(dir.join("b.txt"), "beta\n").unwrap();
        let tool = ReadTool;
        for input in [
            json!({"file_path": "a.txt"}),
            json!({"paths": ["a.txt", "b.txt"]}),
            json!({"path": "a.txt", "files": [{"path": "b.txt"}]}),
        ] {
            let input = tool.normalize_input(input);
            crate::args_validator::validate_against_schema(&tool.parameters_schema(), &input)
                .unwrap_or_else(|e| panic!("{}", e.message));
            let out = tool.execute(input, &context(&dir)).await.unwrap();
            assert!(!out.is_error, "{}", out.content);
            assert!(out.content.contains("alpha") || out.content.contains("beta"));
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn batch_reads_declare_every_file_in_their_accesses() {
        let accesses = ReadTool.accesses(
            &json!({"files": [{"path": "a.txt"}, "b.txt"]}),
            std::path::Path::new("/ws"),
        );
        assert_eq!(accesses.len(), 2);
        assert!(crate::accesses::tool_accesses::conflict(
            &accesses,
            &crate::accesses::tool_accesses::write_file("/ws/b.txt")
        ));
        assert!(!crate::accesses::tool_accesses::conflict(
            &accesses,
            &crate::accesses::tool_accesses::write_file("/ws/c.txt")
        ));
    }
}
