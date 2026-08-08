use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::Path;
use crate::{Tool, ToolContext, ToolOutput};

pub struct GlobTool;

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str { "Glob" }
    fn description(&self) -> &str {
        "Find files matching a glob pattern. Returns file paths sorted by modification time."
    }
    fn read_only(&self) -> bool { true }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Glob pattern (e.g. '**/*.rs')"},
                "path": {"type": "string", "description": "Root directory to search (defaults to cwd)"}
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let pattern = input.get("pattern").and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'pattern'"))?;
        let root = input.get("path").and_then(|v| v.as_str())
            .unwrap_or(".");

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

        let mut results: Vec<(std::path::PathBuf, std::time::SystemTime)> = Vec::new();

        for entry in walkdir::WalkDir::new(&root_dir)
            .into_iter()
            .filter_entry(|e| {
                let name = e.file_name().to_string_lossy();
                !name.starts_with('.') && name != "node_modules" && name != "target"
            })
            .filter_map(|e| e.ok())
        {
            if entry.file_type().is_file() {
                let rel = entry.path().strip_prefix(&root_dir).unwrap_or(entry.path());
                if matcher.is_match(rel) {
                    let mtime = entry.metadata()
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                    results.push((rel.to_path_buf(), mtime));
                }
            }

            if results.len() >= 500 {
                break;
            }
        }

        results.sort_by(|a, b| b.1.cmp(&a.1));

        if results.is_empty() {
            return Ok(ToolOutput::success("No files matched.".to_string()));
        }

        let output: Vec<String> = results.iter()
            .map(|(p, _)| p.display().to_string())
            .collect();

        Ok(ToolOutput::success(output.join("\n")))
    }
}
