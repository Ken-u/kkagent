use crate::path_policy::sensitive_glob_excludes;
use crate::{Tool, ToolContext, ToolOutput};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "Grep"
    }
    fn description(&self) -> &str {
        "Search for a regex pattern across files. Supports output_mode \
(content/files_with_matches/count), context lines (-A/-B/-C), head_limit/offset, \
glob/type filters, and case_insensitive."
    }
    fn read_only(&self) -> bool {
        true
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Regex pattern to search for"},
                "path": {"type": "string", "description": "Directory or file to search in (defaults to cwd)"},
                "glob": {"type": "string", "description": "File glob pattern filter (e.g. '*.rs')"},
                "type": {"type": "string", "description": "ripgrep file type filter (e.g. 'rust', 'py')"},
                "case_insensitive": {"type": "boolean", "description": "Case-insensitive search"},
                "output_mode": {
                    "type": "string",
                    "enum": ["content", "files_with_matches", "count"],
                    "description": "Output mode (default: content)"
                },
                "context": {"type": "integer", "description": "Lines of context around each match (-C)"},
                "context_before": {"type": "integer", "description": "Lines before each match (-B)"},
                "context_after": {"type": "integer", "description": "Lines after each match (-A)"},
                "head_limit": {"type": "integer", "description": "Max results to return (default 200)"},
                "offset": {"type": "integer", "description": "Skip first N results"},
                "include_ignored": {"type": "boolean", "description": "Search ignored files (default false)"}
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let pattern = input
            .get("pattern")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'pattern'"))?;
        let search_path = input.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let glob_pattern = input.get("glob").and_then(|v| v.as_str());
        let file_type = input.get("type").and_then(|v| v.as_str());
        let case_insensitive = input
            .get("case_insensitive")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let output_mode = input
            .get("output_mode")
            .and_then(|v| v.as_str())
            .unwrap_or("content");
        let head_limit = input
            .get("head_limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(200) as usize;
        let offset = input.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let include_ignored = input
            .get("include_ignored")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let context = input.get("context").and_then(|v| v.as_u64());
        let context_before = input.get("context_before").and_then(|v| v.as_u64());
        let context_after = input.get("context_after").and_then(|v| v.as_u64());

        let search_dir = if Path::new(search_path).is_absolute() {
            std::path::PathBuf::from(search_path)
        } else {
            ctx.working_dir.join(search_path)
        };

        let mut cmd = tokio::process::Command::new("rg");
        cmd.arg("--color=never");

        match output_mode {
            "files_with_matches" => {
                cmd.arg("--files-with-matches");
            }
            "count" => {
                cmd.arg("--count");
            }
            _ => {
                cmd.arg("--line-number").arg("--no-heading");
            }
        }

        if case_insensitive {
            cmd.arg("--ignore-case");
        }
        if include_ignored {
            cmd.arg("--no-ignore");
        }
        if let Some(glob) = glob_pattern {
            cmd.arg("--glob").arg(glob);
        }
        for exclude in sensitive_glob_excludes() {
            cmd.arg("--glob").arg(exclude);
        }
        if let Some(t) = file_type {
            cmd.arg("--type").arg(t);
        }
        if output_mode == "content" {
            if let Some(c) = context {
                cmd.arg("-C").arg(c.to_string());
            } else {
                if let Some(b) = context_before {
                    cmd.arg("-B").arg(b.to_string());
                }
                if let Some(a) = context_after {
                    cmd.arg("-A").arg(a.to_string());
                }
            }
        }

        cmd.arg(pattern).arg(&search_dir);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let mut child = cmd.spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("failed to capture ripgrep stdout"))?;
        let mut stderr_pipe = child
            .stderr
            .take()
            .ok_or_else(|| anyhow::anyhow!("failed to capture ripgrep stderr"))?;
        let stderr_task = tokio::spawn(async move {
            let mut bytes = Vec::new();
            let _ = stderr_pipe.read_to_end(&mut bytes).await;
            String::from_utf8_lossy(&bytes).into_owned()
        });
        let fetch = if head_limit == 0 {
            usize::MAX
        } else {
            offset.saturating_add(head_limit).saturating_add(1)
        };
        let mut reader = BufReader::new(stdout).lines();
        let mut lines = Vec::new();
        let mut captured_bytes = 0usize;
        let mut output_truncated = false;
        const MAX_CAPTURE_BYTES: usize = 10 * 1024 * 1024;
        while let Some(line) = reader.next_line().await? {
            captured_bytes = captured_bytes.saturating_add(line.len() + 1);
            if captured_bytes > MAX_CAPTURE_BYTES {
                output_truncated = true;
                break;
            }
            lines.push(line);
            if output_mode != "count" && lines.len() >= fetch {
                output_truncated = true;
                break;
            }
        }
        if output_truncated {
            let _ = child.kill().await;
        }
        let status = child.wait().await?;
        let stderr = stderr_task.await.unwrap_or_default();

        if !status.success() && lines.is_empty() {
            if stderr.contains("regex parse error") {
                return Ok(ToolOutput::error(format!("Invalid regex: {}", stderr)));
            }
            return Ok(ToolOutput::success("No matches found.".to_string()));
        }

        if output_mode == "count" {
            // Aggregate: path:count → total / per-file
            let mut by_file: HashMap<String, u64> = HashMap::new();
            let mut total = 0u64;
            for line in &lines {
                if let Some((path, count_s)) = line.rsplit_once(':') {
                    if let Ok(n) = count_s.parse::<u64>() {
                        total += n;
                        *by_file.entry(path.to_string()).or_insert(0) += n;
                    }
                }
            }
            let mut file_lines: Vec<String> = by_file
                .into_iter()
                .map(|(p, n)| format!("{}:{}", p, n))
                .collect();
            file_lines.sort();
            let total_files = file_lines.len();
            let take = if head_limit == 0 {
                usize::MAX
            } else {
                head_limit
            };
            let sliced: Vec<String> = file_lines.into_iter().skip(offset).take(take).collect();
            let mut result = format!("Total matches: {}\n{}", total, sliced.join("\n"));
            if offset + sliced.len() < total_files {
                result.push_str(&format!(
                    "\n... {} more files ...",
                    total_files.saturating_sub(offset + sliced.len())
                ));
            }
            if output_truncated {
                result.push_str("\n... count output truncated at 10 MiB safety limit ...");
            }
            return Ok(ToolOutput::success(result));
        }

        let total = lines.len();
        let take = if head_limit == 0 {
            usize::MAX
        } else {
            head_limit
        };
        let sliced: Vec<&str> = lines
            .iter()
            .map(String::as_str)
            .skip(offset)
            .take(take)
            .collect();
        let mut result = sliced.join("\n");
        if offset + sliced.len() < total {
            result.push_str(&format!(
                "\n... {} more results (offset={}, head_limit={}) ...",
                total.saturating_sub(offset + sliced.len()),
                offset,
                head_limit
            ));
        }
        if result.is_empty() {
            result = "No matches found.".into();
        }
        if output_truncated && head_limit == 0 {
            result.push_str("\n... output truncated at 10 MiB safety limit ...");
        }
        Ok(ToolOutput::success(result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(dir: &std::path::Path) -> ToolContext {
        ToolContext {
            working_dir: dir.to_path_buf(),
            session_id: "grep-test".into(),
            tool_call_id: None,
            interrupted: None,
        }
    }

    #[tokio::test]
    async fn applies_global_limit_and_filters_sensitive_files() {
        let dir = std::env::temp_dir().join(format!("kkagent-grep-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("visible.txt"),
            "needle one\nneedle two\nneedle three\n",
        )
        .unwrap();
        std::fs::write(dir.join("credentials"), "needle secret\n").unwrap();
        let output = GrepTool
            .execute(
                json!({
                    "pattern": "needle",
                    "output_mode": "content",
                    "head_limit": 2,
                    "include_ignored": true
                }),
                &context(&dir),
            )
            .await
            .unwrap();
        assert_eq!(
            output
                .content
                .lines()
                .filter(|line| line.contains("visible.txt"))
                .count(),
            2
        );
        assert!(!output.content.contains("credentials"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
