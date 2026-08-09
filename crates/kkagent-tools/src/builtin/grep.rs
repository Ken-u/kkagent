use crate::{Tool, ToolContext, ToolOutput};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;

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

        // Fetch enough lines to apply offset + head_limit client-side.
        let fetch = offset.saturating_add(head_limit).saturating_add(1);
        cmd.arg("--max-count").arg(fetch.to_string());

        cmd.arg(pattern).arg(&search_dir);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let output = cmd.output().await?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if !output.status.success() && stdout.is_empty() {
            if stderr.contains("regex parse error") {
                return Ok(ToolOutput::error(format!("Invalid regex: {}", stderr)));
            }
            return Ok(ToolOutput::success("No matches found.".to_string()));
        }

        let mut lines: Vec<&str> = stdout.lines().collect();

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
            let sliced: Vec<String> = file_lines
                .into_iter()
                .skip(offset)
                .take(head_limit)
                .collect();
            let mut result = format!("Total matches: {}\n{}", total, sliced.join("\n"));
            if offset + sliced.len() < total_files {
                result.push_str(&format!(
                    "\n... {} more files ...",
                    total_files.saturating_sub(offset + sliced.len())
                ));
            }
            return Ok(ToolOutput::success(result));
        }

        let total = lines.len();
        let sliced: Vec<&str> = lines.drain(..).skip(offset).take(head_limit).collect();
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
        Ok(ToolOutput::success(result))
    }
}
