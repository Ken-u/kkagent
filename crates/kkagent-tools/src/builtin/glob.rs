use crate::path_policy::is_sensitive_path;
use crate::{Tool, ToolContext, ToolOutput};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::path::Path;

pub struct GlobTool;

const DEFAULT_MAX: usize = 100;

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "Glob"
    }
    fn description(&self) -> &str {
        "Find files matching a glob pattern. Respects .gitignore unless include_ignored is true. \
Returns paths sorted by modification time (newest first)."
    }
    fn read_only(&self) -> bool {
        true
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Glob pattern (e.g. '**/*.rs')"},
                "path": {"type": "string", "description": "Root directory to search (defaults to cwd)"},
                "include_ignored": {"type": "boolean", "description": "Include gitignored files (default false)"},
                "head_limit": {"type": "integer", "description": "Max matches to return (default 100)"}
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let pattern = input
            .get("pattern")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'pattern'"))?;
        let root = input.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let include_ignored = input
            .get("include_ignored")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let head_limit = input
            .get("head_limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_MAX as u64) as usize;

        let root_dir = if Path::new(root).is_absolute() {
            std::path::PathBuf::from(root)
        } else {
            ctx.working_dir.join(root)
        };

        let glob_pattern = if pattern.starts_with("**/") {
            pattern.to_string()
        } else {
            format!("**/{}", pattern)
        };

        let matcher = globset::GlobBuilder::new(&glob_pattern)
            .literal_separator(false)
            .build()
            .map_err(|e| anyhow::anyhow!("Invalid glob: {}", e))?
            .compile_matcher();

        let mut walker = ignore::WalkBuilder::new(&root_dir);
        walker.hidden(true);
        walker.git_ignore(!include_ignored);
        walker.git_global(!include_ignored);
        walker.git_exclude(!include_ignored);
        walker.ignore(!include_ignored);
        // Always skip heavy build dirs unless explicitly included via pattern under include_ignored.
        walker.filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            if name == "node_modules" || name == "target" || name == ".git" {
                return false;
            }
            true
        });

        let mut newest: BinaryHeap<Reverse<(std::time::SystemTime, std::path::PathBuf)>> =
            BinaryHeap::new();
        let mut unlimited = Vec::new();
        let mut total_matches = 0usize;

        for entry in walker.build().filter_map(|e| e.ok()) {
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            if is_sensitive_path(entry.path()) {
                continue;
            }
            let rel = entry.path().strip_prefix(&root_dir).unwrap_or(entry.path());
            if matcher.is_match(rel) {
                let mtime = entry
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                total_matches += 1;
                if head_limit == 0 {
                    unlimited.push((rel.to_path_buf(), mtime));
                } else {
                    newest.push(Reverse((mtime, rel.to_path_buf())));
                    if newest.len() > head_limit {
                        newest.pop();
                    }
                }
            }
        }

        let mut results: Vec<(std::path::PathBuf, std::time::SystemTime)> = if head_limit == 0 {
            unlimited
        } else {
            newest
                .into_iter()
                .map(|Reverse((mtime, path))| (path, mtime))
                .collect()
        };
        results.sort_by_key(|item| Reverse(item.1));
        let truncated = head_limit > 0 && total_matches > head_limit;

        if results.is_empty() {
            return Ok(ToolOutput::success("No files matched.".to_string()));
        }

        let mut output: Vec<String> = results
            .iter()
            .map(|(p, _)| p.display().to_string())
            .collect();
        if truncated {
            output.push(format!(
                "... truncated at {} matches (raise head_limit to see more) ...",
                head_limit
            ));
        }

        Ok(ToolOutput::success(output.join("\n")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn limit_is_applied_after_global_mtime_sort() {
        let dir = std::env::temp_dir().join(format!("kkagent-glob-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["a-old.rs", "b-mid.rs", "z-new.rs"] {
            std::fs::write(dir.join(name), name).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
        let output = GlobTool
            .execute(
                json!({"pattern": "*.rs", "head_limit": 1}),
                &ToolContext {
                    working_dir: dir.clone(),
                    session_id: "glob-test".into(),
                    tool_call_id: None,
                    interrupted: None,
                },
            )
            .await
            .unwrap();
        assert!(output.content.lines().next().unwrap().ends_with("z-new.rs"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
