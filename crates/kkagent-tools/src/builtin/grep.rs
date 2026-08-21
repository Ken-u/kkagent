use crate::path_policy::sensitive_glob_excludes;
use crate::{inside_heavy_dir_list, Tool, ToolContext, ToolOutput};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};

const DEFAULT_TIMEOUT_MS: u64 = 60_000;
const MAX_TIMEOUT_MS: u64 = 120_000;
const MAX_STDERR_BYTES: usize = 64 * 1024;

pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "Grep"
    }
    fn description(&self) -> &str {
        "Search for a regex pattern across files. Supports output_mode \
(content/files_with_matches/count_matches), surrounding context, case-insensitive matching, \
multiline, head_limit/offset, and glob/type filters."
    }
    fn read_only(&self) -> bool {
        true
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Regex pattern to search for"},
                "path": {
                    "type": "string",
                    "description": "File or directory to search. Accepts an absolute path, or a path relative to the current working directory. Omit to search the current working directory."
                },
                "glob": {"type": "string", "description": "File glob pattern filter (e.g. '*.rs')"},
                "type": {"type": "string", "description": "ripgrep file type filter (e.g. 'rust', 'py')"},
                "case_insensitive": {"type": "boolean", "description": "Case-insensitive search"},
                "output_mode": {
                    "type": "string",
                    "enum": ["content", "files_with_matches", "count_matches"],
                    "description": "Output mode (default: files_with_matches)"
                },
                "-n": {"type": "boolean", "description": "Show line numbers in content mode (default true)"},
                "context": {"type": "integer", "description": "Lines of context around each match"},
                "context_before": {"type": "integer", "description": "Lines before each match"},
                "context_after": {"type": "integer", "description": "Lines after each match"},
                "head_limit": {"type": "integer", "description": "Max results to return (default 200)"},
                "offset": {"type": "integer", "description": "Skip first N results"},
                "multiline": {"type": "boolean", "description": "Enable multiline matching (. matches newlines)"},
                "include_ignored": {"type": "boolean", "description": "Extension: search ignored files (default false)"},
                "timeout_ms": {"type": "integer", "description": "Extension: timeout in milliseconds (default 60000, max 120000)"}
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
            .get("-i")
            .or_else(|| input.get("case_insensitive"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let output_mode_raw = input
            .get("output_mode")
            .and_then(|v| v.as_str())
            .unwrap_or("files_with_matches");
        let output_mode = match output_mode_raw {
            "count" | "count_matches" => "count_matches",
            "content" => "content",
            _ => "files_with_matches",
        };
        let show_line_numbers = input.get("-n").and_then(|v| v.as_bool()).unwrap_or(true);
        let multiline = input
            .get("multiline")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let head_limit = input
            .get("head_limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(200) as usize;
        let offset = input.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let include_ignored = input
            .get("include_ignored")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let context = input
            .get("-C")
            .or_else(|| input.get("context"))
            .and_then(|v| v.as_u64());
        let context_before = input
            .get("-B")
            .or_else(|| input.get("context_before"))
            .and_then(|v| v.as_u64());
        let context_after = input
            .get("-A")
            .or_else(|| input.get("context_after"))
            .and_then(|v| v.as_u64());
        let timeout_ms = input
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .min(MAX_TIMEOUT_MS);
        if is_interrupted(ctx.interrupted.as_ref()) {
            return Ok(ToolOutput::error("Search was interrupted"));
        }

        let search_dir = if Path::new(search_path).is_absolute() {
            std::path::PathBuf::from(search_path)
        } else {
            ctx.working_dir.join(search_path)
        };

        // S1-4: workspace directory constraint
        if let Err(reason) = ctx.check_path_guard(&search_dir) {
            return Ok(ToolOutput::error(reason));
        }

        let workspace_dir =
            std::fs::canonicalize(&ctx.working_dir).unwrap_or_else(|_| ctx.working_dir.clone());
        let resolved_search_dir =
            std::fs::canonicalize(&search_dir).unwrap_or_else(|_| search_dir.clone());
        // Huge-workspace guard computed before `resolved_search_dir` is moved
        // into `search_arg` below.
        let heavy_dirs = ctx.tools_config.effective_heavy_dirs();
        let skip_heavy =
            !include_ignored && !inside_heavy_dir_list(&resolved_search_dir, &heavy_dirs);
        let search_arg = resolved_search_dir
            .strip_prefix(&workspace_dir)
            .map(|path| {
                if path.as_os_str().is_empty() {
                    std::path::PathBuf::from(".")
                } else {
                    path.to_path_buf()
                }
            })
            .unwrap_or(resolved_search_dir);

        let mut cmd = Command::new("rg");
        // Fail fast with an actionable message when ripgrep is not installed
        // (common on minimal Linux build servers) instead of a bare os error.
        if !rg_available() {
            return Ok(ToolOutput::error(
                "ripgrep (rg) is not installed or not on PATH; the Grep tool depends on it. \
Install ripgrep (e.g. `apt install ripgrep`, `dnf install ripgrep`, \
`cargo install ripgrep`) or add it to PATH, then retry.",
            ));
        }
        cmd.arg("--color=never");
        cmd.current_dir(&ctx.working_dir);

        match output_mode {
            "files_with_matches" => {
                cmd.arg("--files-with-matches");
            }
            "count_matches" => {
                cmd.arg("--count");
            }
            _ => {
                if show_line_numbers {
                    cmd.arg("--line-number");
                } else {
                    cmd.arg("--no-line-number");
                }
                cmd.arg("--no-heading");
            }
        }

        if case_insensitive {
            cmd.arg("--ignore-case");
        }
        if multiline {
            cmd.arg("--multiline").arg("--multiline-dotall");
        }
        if include_ignored {
            cmd.arg("--no-ignore");
        }
        if let Some(glob) = glob_pattern {
            cmd.arg("--glob").arg(glob);
        }
        // S2-6: sensitive glob excludes can be disabled via config
        if ctx.sensitive_check_enabled() {
            for exclude in sensitive_glob_excludes() {
                cmd.arg("--glob").arg(exclude);
            }
        }
        // Huge-workspace guard: never descend into heavy build dirs (AOSP
        // `out/`, cargo `target/`, ...) unless the search path is already
        // inside one of them.
        if skip_heavy {
            for heavy in &heavy_dirs {
                cmd.arg("--glob").arg(format!("!{heavy}/"));
            }
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

        cmd.arg(pattern).arg(&search_arg);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        cmd.kill_on_drop(true);
        configure_process_group(&mut cmd);

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
            let mut buffer = [0_u8; 4096];
            loop {
                match stderr_pipe.read(&mut buffer).await {
                    Ok(0) | Err(_) => break,
                    Ok(count) if bytes.len() < MAX_STDERR_BYTES => {
                        let remaining = MAX_STDERR_BYTES - bytes.len();
                        bytes.extend_from_slice(&buffer[..count.min(remaining)]);
                    }
                    Ok(_) => {}
                }
            }
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
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(timeout_ms);
        const MAX_CAPTURE_BYTES: usize = 10 * 1024 * 1024;
        loop {
            let line = tokio::select! {
                result = reader.next_line() => result?,
                _ = tokio::time::sleep_until(deadline) => {
                    terminate_process_tree(&mut child).await;
                    let _ = stderr_task.await;
                    return Ok(ToolOutput::error(format!("Search timed out after {timeout_ms}ms"))
                        .with_note("Ripgrep aborted due to timeout. Narrow the path/glob or raise timeout_ms."));
                }
                _ = wait_for_interrupt(ctx.interrupted.clone()) => {
                    terminate_process_tree(&mut child).await;
                    let _ = stderr_task.await;
                    return Ok(ToolOutput::error("Search was interrupted"));
                }
            };
            let Some(line) = line else {
                break;
            };
            captured_bytes = captured_bytes.saturating_add(line.len() + 1);
            if captured_bytes > MAX_CAPTURE_BYTES {
                output_truncated = true;
                break;
            }
            lines.push(line);
            if lines.len() >= fetch {
                output_truncated = true;
                break;
            }
        }
        let status = if output_truncated {
            terminate_process_tree(&mut child).await;
            None
        } else {
            Some(child.wait().await?)
        };
        let stderr = stderr_task.await.unwrap_or_default();

        if let Some(status) = status.filter(|status| !status.success()) {
            if stderr.contains("regex parse error") {
                return Ok(ToolOutput::error(format!("Invalid regex: {}", stderr)));
            }
            if status.code() == Some(1) && lines.is_empty() {
                return Ok(ToolOutput::success("No matches found.".to_string()));
            }
            let detail = stderr.trim();
            return Ok(ToolOutput::error(if detail.is_empty() {
                format!(
                    "Search failed with exit code {}",
                    status.code().unwrap_or(-1)
                )
            } else {
                format!("Search failed: {detail}")
            }));
        }

        if output_mode == "count_matches" {
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
                return Ok(ToolOutput::success(result).with_note(
                    "Count output truncated at safety limit; refine path/glob or raise head_limit.",
                ));
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
        if result.is_empty() {
            result = "No matches found.".into();
        }
        let mut note = None;
        if offset + sliced.len() < total {
            note = Some(format!(
                "{} more results not shown (offset={}, head_limit={}).",
                total.saturating_sub(offset + sliced.len()),
                offset,
                head_limit
            ));
        } else if output_truncated && head_limit == 0 {
            note = Some("Output truncated at 10 MiB safety limit.".into());
        }
        let mut out = ToolOutput::success(result);
        if let Some(n) = note {
            out = out.with_note(n);
        }
        Ok(out)
    }
}

fn is_interrupted(flag: Option<&Arc<std::sync::atomic::AtomicBool>>) -> bool {
    flag.map(|flag| flag.load(std::sync::atomic::Ordering::SeqCst))
        .unwrap_or(false)
}

/// Cached `rg` presence probe. Probing on every search would spawn a process
/// per call; on PATH-less environments that probe is pure overhead.
fn rg_available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        std::process::Command::new("rg")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

async fn wait_for_interrupt(flag: Option<Arc<std::sync::atomic::AtomicBool>>) {
    let Some(flag) = flag else {
        std::future::pending::<()>().await;
        return;
    };
    loop {
        if flag.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    }
}

fn configure_process_group(command: &mut Command) {
    #[cfg(unix)]
    command.process_group(0);
}

async fn terminate_process_tree(child: &mut Child) {
    let pid = child.id();
    #[cfg(unix)]
    if let Some(pid) = pid {
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    #[cfg(windows)]
    if let Some(pid) = pid {
        let _ = tokio::time::timeout(
            tokio::time::Duration::from_secs(5),
            Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .status(),
        )
        .await;
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(dir: &std::path::Path) -> ToolContext {
        ToolContext {
            working_dir: dir.to_path_buf(),
            session_id: "grep-test".into(),
            turn_id: "test-turn".into(),
            plan_file_path: None,
            image: kkagent_config::ImageConfig::default(),
            tool_call_id: None,
            interrupted: None,
            tools_config: kkagent_config::ToolsConfig::default(),
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
        assert!(!output.content.contains(dir.to_string_lossy().as_ref()));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn interruption_is_reported_before_spawning_search() {
        let interrupted = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let mut ctx = context(std::path::Path::new("."));
        ctx.interrupted = Some(interrupted);
        let output = GrepTool
            .execute(json!({"pattern": "needle"}), &ctx)
            .await
            .unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("interrupted"));
    }

    #[tokio::test]
    async fn ripgrep_operational_failures_are_not_reported_as_no_matches() {
        let dir = std::env::temp_dir().join(format!("kkagent-grep-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let output = GrepTool
            .execute(
                json!({"pattern": "needle", "path": "missing-directory"}),
                &context(&dir),
            )
            .await
            .unwrap();
        assert!(output.is_error, "{}", output.content);
        assert!(output.content.contains("Search failed"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn heavy_dirs_are_excluded_by_default_and_included_when_targeted() {
        let dir = std::env::temp_dir().join(format!("kkagent-grep-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("out")).unwrap();
        std::fs::write(dir.join("out/generated.txt"), "needle generated\n").unwrap();
        std::fs::write(dir.join("real.txt"), "needle real\n").unwrap();

        let default = GrepTool
            .execute(
                json!({"pattern": "needle", "path": ".", "output_mode": "content"}),
                &context(&dir),
            )
            .await
            .unwrap();
        assert!(default.content.contains("real.txt"), "{}", default.content);
        assert!(
            !default.content.contains("generated.txt"),
            "{}",
            default.content
        );

        let targeted = GrepTool
            .execute(
                json!({"pattern": "needle", "path": "out", "output_mode": "content"}),
                &context(&dir),
            )
            .await
            .unwrap();
        assert!(
            targeted.content.contains("generated.txt"),
            "{}",
            targeted.content
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
