use crate::{Tool, ToolContext, ToolOutput};
use async_trait::async_trait;
use serde_json::{json, Value};
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

        let mut results: Vec<(std::path::PathBuf, std::time::SystemTime)> = Vec::new();
        let mut truncated = false;

        for entry in walker.build().filter_map(|e| e.ok()) {
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let rel = entry.path().strip_prefix(&root_dir).unwrap_or(entry.path());
            if matcher.is_match(rel) {
                let mtime = entry
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                results.push((rel.to_path_buf(), mtime));
                if results.len() > head_limit {
                    truncated = true;
                    break;
                }
            }
        }

        results.sort_by(|a, b| b.1.cmp(&a.1));
        if results.len() > head_limit {
            results.truncate(head_limit);
            truncated = true;
        }

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
