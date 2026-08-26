use crate::path_policy::{is_sensitive_path, looks_binary_ext, sniff_binary, MAX_LINE_LENGTH};
use crate::{Tool, ToolContext, ToolOutput};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::Path;
use tokio::io::AsyncReadExt;

pub struct ReadTool;

const MAX_READ_LINES: usize = 1000;
const MAX_READ_BYTES: usize = 100 * 1024;

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "Read"
    }
    fn description(&self) -> &str {
        "Read the contents of a text file. Returns numbered lines (up to 1000 lines or 100KB). \
Rejects binary/image files — use ReadMediaFile for media."
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
                    "description": "Number of lines to read"
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let path_str = input
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'path' parameter"))?;

        let path = if Path::new(path_str).is_absolute() {
            std::path::PathBuf::from(path_str)
        } else {
            ctx.working_dir.join(path_str)
        };

        if !path.exists() {
            return Ok(ToolOutput::error(format!("File not found: {}", path_str)));
        }
        // S1-4: workspace directory constraint
        if let Err(reason) = ctx.check_path_guard(&path) {
            return Ok(ToolOutput::error(reason));
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

        let offset = input
            .get("offset")
            .or_else(|| input.get("line_offset"))
            .and_then(|v| v.as_i64())
            .unwrap_or(1);
        let n_lines = input
            .get("limit")
            .or_else(|| input.get("n_lines"))
            .and_then(|v| v.as_u64())
            .unwrap_or(MAX_READ_LINES as u64) as usize;
        let n_lines = n_lines.min(MAX_READ_LINES);

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
More lines remain — call Read again with offset={}.",
                end.saturating_sub(start),
                start + 1,
                total_lines,
                end + 1
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
            note_parts.push(
                "File contains bare CR line endings; displayed with LF normalization.".into(),
            );
        }

        let mut out = ToolOutput::success_with_data(
            result,
            json!({
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
}
