use crate::path_policy::is_sensitive_path;
use crate::{inside_heavy_dir_list, Tool, ToolContext, ToolOutput};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub struct GlobTool;

const DEFAULT_MAX: usize = 100;
/// Cap for `head_limit = 0` (unlimited). Without this, `**/*.java` on AOSP can
/// materialize millions of paths and hundreds of MB before the walk budget
/// stops the search.
const HARD_MATCH_CAP: usize = 100_000;
/// Hard wall-clock budget for one directory walk. Giant trees (e.g. an AOSP
/// checkout with a populated `out/`) can take many minutes to walk; stop
/// early and report partial results so the agent stays responsive.
const WALK_BUDGET: Duration = Duration::from_secs(15);

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
                "path": {
                    "type": "string",
                    "description": "Directory to search. Accepts an absolute path, or a path relative to the current working directory. Defaults to the current working directory."
                },
                "include_ignored": {"type": "boolean", "description": "Include gitignored files (default false)"},
                "head_limit": {"type": "integer", "description": "Max matches to return (default 100)"}
            },
            "required": ["pattern"]
        })
    }

    fn accesses(&self, input: &Value, working_dir: &Path) -> crate::ToolAccesses {
        crate::accesses::glob_accesses(input, working_dir)
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

        let requested_root = if Path::new(root).is_absolute() {
            std::path::PathBuf::from(root)
        } else {
            ctx.working_dir.join(root)
        };

        // S1-4: workspace directory constraint
        if let Err(reason) = ctx.check_path_guard(&requested_root) {
            return Ok(ToolOutput::error(reason));
        }

        let root_dir = std::fs::canonicalize(&requested_root).unwrap_or(requested_root);
        let workspace_dir =
            std::fs::canonicalize(&ctx.working_dir).unwrap_or_else(|_| ctx.working_dir.clone());

        let glob_pattern = if pattern.starts_with("**/") {
            pattern.to_string()
        } else {
            format!("**/{}", pattern)
        };

        // Literal-prefix fast path: `a/b/**` walks `<root>/a/b` directly
        // instead of scanning the whole tree (crucial on huge checkouts).
        let walk_root =
            literal_prefix_dir(&glob_pattern, &root_dir).unwrap_or_else(|| root_dir.clone());
        let heavy_dirs = ctx.tools_config.effective_heavy_dirs();
        // Heavy dirs are pruned unless the caller explicitly descended into
        // one (pattern literal prefix or `path` argument), e.g. `out/soong/**`.
        let skip_heavy = !inside_heavy_dir_list(&walk_root, &heavy_dirs);

        let sensitive = ctx.sensitive_check_enabled();
        let interrupted = ctx.interrupted.clone();
        // The walk is synchronous and can visit millions of entries on large
        // trees; run it on the blocking pool so the async runtime never stalls.
        let outcome = tokio::task::spawn_blocking(move || {
            walk_matches(WalkConfig {
                root_dir,
                walk_root,
                workspace_dir,
                glob_pattern,
                include_ignored,
                head_limit,
                sensitive,
                skip_heavy,
                heavy_dirs,
                walk_budget: WALK_BUDGET,
                interrupted,
            })
        })
        .await
        .map_err(|e| anyhow::anyhow!("glob walk failed: {}", e))?;

        if outcome.results.is_empty() && outcome.stopped_note.is_none() {
            return Ok(ToolOutput::success("No files matched.".to_string()));
        }

        let mut output: Vec<String> = outcome
            .results
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        if outcome.truncated {
            let cap = if head_limit == 0 {
                HARD_MATCH_CAP
            } else {
                head_limit
            };
            if head_limit == 0 {
                output.push(format!(
                    "... truncated at hard cap of {cap} matches (narrow the pattern or pass a deeper `path`) ..."
                ));
            } else {
                output.push(format!(
                    "... truncated at {cap} matches (raise head_limit to see more) ..."
                ));
            }
        }
        if let Some(note) = outcome.stopped_note {
            output.push(note);
        }

        Ok(ToolOutput::success(output.join("\n")))
    }
}

struct WalkConfig {
    /// Paths are matched and displayed relative to this directory.
    root_dir: PathBuf,
    /// Directory actually walked; may be deeper than `root_dir` when the
    /// pattern starts with a literal directory prefix.
    walk_root: PathBuf,
    workspace_dir: PathBuf,
    glob_pattern: String,
    include_ignored: bool,
    head_limit: usize,
    sensitive: bool,
    /// Prune heavy build dirs during the walk. False when the walk root is
    /// already inside a heavy dir (explicit descent).
    skip_heavy: bool,
    heavy_dirs: Vec<String>,
    /// Hard wall-clock budget for the walk.
    walk_budget: Duration,
    interrupted: Option<Arc<AtomicBool>>,
}

struct WalkOutcome {
    results: Vec<PathBuf>,
    total_matches: usize,
    truncated: bool,
    stopped_note: Option<String>,
}

fn walk_matches(cfg: WalkConfig) -> WalkOutcome {
    let mut result = WalkOutcome {
        results: Vec::new(),
        total_matches: 0,
        truncated: false,
        stopped_note: None,
    };

    let matcher = match globset::GlobBuilder::new(&cfg.glob_pattern)
        .literal_separator(false)
        .build()
    {
        Ok(glob) => glob.compile_matcher(),
        Err(_) => return result, // pattern was validated earlier; nothing to do
    };

    let mut walker = ignore::WalkBuilder::new(&cfg.walk_root);
    walker.hidden(true);
    walker.git_ignore(!cfg.include_ignored);
    walker.git_global(!cfg.include_ignored);
    walker.git_exclude(!cfg.include_ignored);
    walker.ignore(!cfg.include_ignored);
    // Skip heavy build dirs (AOSP `out/`, cargo `target/`, ...) unless the
    // walk root is already inside one of them.
    if cfg.skip_heavy {
        let heavy = cfg.heavy_dirs.clone();
        walker.filter_entry(move |e| {
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                let name = e.file_name().to_string_lossy();
                return !heavy.iter().any(|h| h.as_str() == name.as_ref());
            }
            true
        });
    }

    let deadline = Instant::now() + cfg.walk_budget;
    let mut newest: BinaryHeap<Reverse<(std::time::SystemTime, PathBuf)>> = BinaryHeap::new();
    // `head_limit == 0` means "no caller limit" but still respects HARD_MATCH_CAP.
    let effective_limit = if cfg.head_limit == 0 {
        HARD_MATCH_CAP
    } else {
        cfg.head_limit
    };

    for entry in walker.build().filter_map(|e| e.ok()) {
        if let Some(flag) = cfg.interrupted.as_ref() {
            if flag.load(Ordering::Relaxed) {
                result.stopped_note = Some("... walk interrupted (partial results) ...".into());
                break;
            }
        }
        if Instant::now() >= deadline {
            result.stopped_note = Some(format!(
                "... walk stopped after {}s with partial results; pass a deeper `path` to speed up the search ...",
                cfg.walk_budget.as_secs()
            ));
            break;
        }
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        if cfg.sensitive && is_sensitive_path(entry.path()) {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(&cfg.root_dir)
            .unwrap_or(entry.path());
        if matcher.is_match(rel) {
            let display_path = entry
                .path()
                .strip_prefix(&cfg.workspace_dir)
                .map(Path::to_path_buf)
                .unwrap_or_else(|_| entry.path().to_path_buf());
            let mtime = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            result.total_matches += 1;
            newest.push(Reverse((mtime, display_path)));
            if newest.len() > effective_limit {
                newest.pop();
            }
            // Stop collecting once we know the hard/caller cap is exceeded so
            // memory stays bounded even before the walk budget fires.
            if result.total_matches > effective_limit && cfg.head_limit == 0 {
                // Keep walking briefly? No — hard-cap truncate is enough; note
                // is attached below. Breaking early saves IO on huge trees.
                result.truncated = true;
                break;
            }
        }
    }

    let mut items: Vec<(PathBuf, std::time::SystemTime)> = newest
        .into_iter()
        .map(|Reverse((mtime, path))| (path, mtime))
        .collect();
    items.sort_by_key(|item| Reverse(item.1));
    result.results = items.into_iter().map(|(p, _)| p).collect();
    result.truncated = result.truncated || result.total_matches > effective_limit;
    result
}

/// Resolve the directory Glob will actually walk (for conflict declaration).
pub fn resolve_glob_walk_root(working_dir: &Path, pattern: &str, path: Option<&str>) -> PathBuf {
    let root = path.unwrap_or(".");
    let requested_root = if Path::new(root).is_absolute() {
        PathBuf::from(root)
    } else {
        working_dir.join(root)
    };
    let root_dir = std::fs::canonicalize(&requested_root).unwrap_or(requested_root);
    let glob_pattern = if pattern.starts_with("**/") {
        pattern.to_string()
    } else {
        format!("**/{pattern}")
    };
    literal_prefix_dir(&glob_pattern, &root_dir).unwrap_or(root_dir)
}

/// If the glob pattern (already normalized to start with `**/`) begins with a
/// run of literal directory segments and that directory exists under `root`,
/// return it so the walk can start there instead of the whole tree.
fn literal_prefix_dir(glob_pattern: &str, root: &Path) -> Option<PathBuf> {
    let rest = glob_pattern.strip_prefix("**/")?;
    let mut dir = root.to_path_buf();
    let mut depth = 0usize;
    for segment in rest.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            break;
        }
        if segment
            .chars()
            .any(|c| matches!(c, '*' | '?' | '[' | ']' | '{' | '}'))
        {
            break;
        }
        dir.push(segment);
        depth += 1;
    }
    if depth == 0 {
        return None;
    }
    dir.is_dir().then_some(dir)
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
                    turn_id: "test-turn".into(),
                    plan_file_path: None,
                    image: kkagent_config::ImageConfig::default(),
                    tool_call_id: None,
                    interrupted: None,
                    tools_config: kkagent_config::ToolsConfig::default(),
                },
            )
            .await
            .unwrap();
        assert!(output.content.lines().next().unwrap().ends_with("z-new.rs"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn nested_workspace_results_keep_the_workspace_relative_prefix() {
        let dir = std::env::temp_dir().join(format!("kkagent-glob-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), "fn main() {}\n").unwrap();
        let output = GlobTool
            .execute(
                json!({"pattern": "*.rs", "path": "src"}),
                &ToolContext {
                    working_dir: dir.clone(),
                    session_id: "glob-test".into(),
                    turn_id: "test-turn".into(),
                    plan_file_path: None,
                    image: kkagent_config::ImageConfig::default(),
                    tool_call_id: None,
                    interrupted: None,
                    tools_config: kkagent_config::ToolsConfig::default(),
                },
            )
            .await
            .unwrap();
        assert_eq!(
            std::path::PathBuf::from(&output.content),
            std::path::PathBuf::from("src").join("lib.rs")
        );
        assert!(!output.content.contains(dir.to_string_lossy().as_ref()));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn literal_prefix_walk_finds_nested_matches() {
        let dir = std::env::temp_dir().join(format!("kkagent-glob-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("src/deep")).unwrap();
        std::fs::write(dir.join("src/deep/lib.rs"), "fn main() {}\n").unwrap();
        // Noise outside the literal prefix that must not be returned.
        std::fs::create_dir_all(dir.join("other")).unwrap();
        std::fs::write(dir.join("other/noise.rs"), "// noise\n").unwrap();
        let output = GlobTool
            .execute(
                json!({"pattern": "src/**/*.rs"}),
                &ToolContext {
                    working_dir: dir.clone(),
                    session_id: "glob-test".into(),
                    turn_id: "test-turn".into(),
                    plan_file_path: None,
                    image: kkagent_config::ImageConfig::default(),
                    tool_call_id: None,
                    interrupted: None,
                    tools_config: kkagent_config::ToolsConfig::default(),
                },
            )
            .await
            .unwrap();
        assert!(
            output.content.contains("src/deep/lib.rs"),
            "content: {}",
            output.content
        );
        assert!(!output.content.contains("noise.rs"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn heavy_build_dirs_are_skipped() {
        let dir = std::env::temp_dir().join(format!("kkagent-glob-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("out")).unwrap();
        std::fs::write(dir.join("out/generated.rs"), "// generated\n").unwrap();
        std::fs::write(dir.join("real.rs"), "// real\n").unwrap();
        let output = GlobTool
            .execute(
                json!({"pattern": "**/*.rs"}),
                &ToolContext {
                    working_dir: dir.clone(),
                    session_id: "glob-test".into(),
                    turn_id: "test-turn".into(),
                    plan_file_path: None,
                    image: kkagent_config::ImageConfig::default(),
                    tool_call_id: None,
                    interrupted: None,
                    tools_config: kkagent_config::ToolsConfig::default(),
                },
            )
            .await
            .unwrap();
        assert!(output.content.contains("real.rs"));
        assert!(!output.content.contains("generated.rs"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn explicit_descent_into_heavy_dir_is_not_pruned() {
        let dir = std::env::temp_dir().join(format!("kkagent-glob-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("out/soong")).unwrap();
        std::fs::write(dir.join("out/soong/generated.rs"), "// generated\n").unwrap();
        let output = GlobTool
            .execute(
                json!({"pattern": "out/soong/**/*.rs"}),
                &ToolContext {
                    working_dir: dir.clone(),
                    session_id: "glob-test".into(),
                    turn_id: "test-turn".into(),
                    plan_file_path: None,
                    image: kkagent_config::ImageConfig::default(),
                    tool_call_id: None,
                    interrupted: None,
                    tools_config: kkagent_config::ToolsConfig::default(),
                },
            )
            .await
            .unwrap();
        assert!(
            output.content.contains("out/soong/generated.rs"),
            "content: {}",
            output.content
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn literal_prefix_dir_requires_existing_directory() {
        let dir = std::env::temp_dir().join(format!("kkagent-glob-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("a/b")).unwrap();

        assert_eq!(
            literal_prefix_dir("**/a/b/*.rs", &dir),
            Some(dir.join("a/b"))
        );
        // Missing directory -> fall back to a full walk.
        assert_eq!(literal_prefix_dir("**/missing/**", &dir), None);
        // No literal leading segment -> full walk.
        assert_eq!(literal_prefix_dir("**/*.rs", &dir), None);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
