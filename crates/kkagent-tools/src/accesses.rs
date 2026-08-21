//! Tool resource accesses + conflict matrix (kimi `ToolAccesses`).

use std::path::{Component, Path, PathBuf};

use crate::bash_ast::{collect_commands, extract_dependencies, parse as parse_bash};
use crate::builtin::glob::resolve_glob_walk_root;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolFileAccessOperation {
    Read,
    Write,
    ReadWrite,
    Search,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolResourceAccess {
    File {
        operation: ToolFileAccessOperation,
        path: String,
        recursive: bool,
    },
    All,
}

pub type ToolAccesses = Vec<ToolResourceAccess>;

pub mod tool_accesses {
    use super::*;

    pub fn none() -> ToolAccesses {
        Vec::new()
    }

    pub fn all() -> ToolAccesses {
        vec![ToolResourceAccess::All]
    }

    pub fn file(
        operation: ToolFileAccessOperation,
        path: impl Into<String>,
        recursive: bool,
    ) -> ToolAccesses {
        vec![ToolResourceAccess::File {
            operation,
            path: path.into(),
            recursive,
        }]
    }

    pub fn read_file(path: impl Into<String>) -> ToolAccesses {
        file(ToolFileAccessOperation::Read, path, false)
    }

    pub fn read_tree(path: impl Into<String>) -> ToolAccesses {
        file(ToolFileAccessOperation::Read, path, true)
    }

    pub fn write_file(path: impl Into<String>) -> ToolAccesses {
        file(ToolFileAccessOperation::Write, path, false)
    }

    pub fn write_tree(path: impl Into<String>) -> ToolAccesses {
        file(ToolFileAccessOperation::Write, path, true)
    }

    pub fn read_write_file(path: impl Into<String>) -> ToolAccesses {
        file(ToolFileAccessOperation::ReadWrite, path, false)
    }

    pub fn search_tree(path: impl Into<String>) -> ToolAccesses {
        file(ToolFileAccessOperation::Search, path, true)
    }

    pub fn conflict(left: &ToolAccesses, right: &ToolAccesses) -> bool {
        left.iter()
            .any(|l| right.iter().any(|r| resource_accesses_conflict(l, r)))
    }
}

fn resource_accesses_conflict(left: &ToolResourceAccess, right: &ToolResourceAccess) -> bool {
    match (left, right) {
        (ToolResourceAccess::All, _) | (_, ToolResourceAccess::All) => true,
        (
            ToolResourceAccess::File {
                operation: lo,
                path: lp,
                recursive: lr,
            },
            ToolResourceAccess::File {
                operation: ro,
                path: rp,
                recursive: rr,
            },
        ) => {
            if !file_operations_conflict(*lo, *ro) {
                return false;
            }
            file_accesses_overlap(lp, *lr, rp, *rr)
        }
    }
}

fn file_operations_conflict(left: ToolFileAccessOperation, right: ToolFileAccessOperation) -> bool {
    file_operation_writes(left) || file_operation_writes(right)
}

fn file_operation_writes(op: ToolFileAccessOperation) -> bool {
    matches!(
        op,
        ToolFileAccessOperation::Write | ToolFileAccessOperation::ReadWrite
    )
}

fn file_accesses_overlap(left: &str, left_rec: bool, right: &str, right_rec: bool) -> bool {
    let left_path = normalize_path(left);
    let right_path = normalize_path(right);
    if left_path == right_path {
        return true;
    }
    let left_prefix = if left_path.ends_with('/') {
        left_path.clone()
    } else {
        format!("{left_path}/")
    };
    let right_prefix = if right_path.ends_with('/') {
        right_path.clone()
    } else {
        format!("{right_path}/")
    };
    if left_rec && right_path.starts_with(&left_prefix) {
        return true;
    }
    if right_rec && left_path.starts_with(&right_prefix) {
        return true;
    }
    false
}

fn normalize_path(path: &str) -> String {
    let p = PathBuf::from(path);
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    let s = out.to_string_lossy().replace('\\', "/");
    if s.is_empty() {
        ".".into()
    } else {
        s
    }
}

/// Resolve a tool input path against working_dir.
pub fn resolve_tool_path(working_dir: &Path, path: &str) -> String {
    let p = Path::new(path);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        working_dir.join(p)
    };
    normalize_path(&abs.to_string_lossy())
}

/// Glob conflict scope follows the literal-prefix walk root, not the whole
/// workspace, so a subtree Glob can run alongside an unrelated Bash.
pub fn glob_accesses(input: &serde_json::Value, working_dir: &Path) -> ToolAccesses {
    let pattern = input
        .get("pattern")
        .and_then(|v| v.as_str())
        .unwrap_or("**/*");
    let path = input.get("path").and_then(|v| v.as_str());
    let walk = resolve_glob_walk_root(working_dir, pattern, path);
    tool_accesses::search_tree(normalize_path(&walk.to_string_lossy()))
}

/// Ambient pathless commands that do not touch the filesystem for conflict
/// purposes (lets `Bash pwd` run parallel with a subtree Glob).
const PATHLESS_AMBIENT: &[&str] = &[
    "pwd", "echo", "printf", "true", "false", ":", "clear", "sleep", "date", "whoami", "hostname",
    "uname", "id", "nproc", "arch", "basename", "dirname", "yes",
];

/// Bash accesses derived from AST path deps. Unknown mutating commands without
/// clear paths fall back to `all` (conservative).
pub fn bash_accesses(input: &serde_json::Value, working_dir: &Path) -> ToolAccesses {
    let Some(command) = input.get("command").and_then(|v| v.as_str()) else {
        return tool_accesses::all();
    };
    if command.trim().is_empty() {
        // Polling / stop background shells — no filesystem conflict.
        return tool_accesses::none();
    }

    let cwd = input
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(|c| resolve_tool_path(working_dir, c));

    let ast = parse_bash(command);
    let deps = extract_dependencies(&ast);
    let mut out = ToolAccesses::new();

    if let Some(cwd) = cwd {
        // Running in an explicit cwd still only conflicts if other tools
        // write there; treat as a read of that tree.
        out.push(ToolResourceAccess::File {
            operation: ToolFileAccessOperation::Read,
            path: cwd,
            recursive: true,
        });
    }

    for path in &deps.writes {
        out.push(ToolResourceAccess::File {
            operation: ToolFileAccessOperation::Write,
            path: resolve_tool_path(working_dir, path),
            recursive: false,
        });
    }
    for path in &deps.reads {
        out.push(ToolResourceAccess::File {
            operation: ToolFileAccessOperation::Read,
            path: resolve_tool_path(working_dir, path),
            recursive: false,
        });
    }

    if !out.is_empty() {
        return out;
    }

    let cmds = collect_commands(&ast);
    if !cmds.is_empty()
        && cmds
            .iter()
            .all(|c| PATHLESS_AMBIENT.contains(&c.to_ascii_lowercase().as_str()))
    {
        return tool_accesses::none();
    }

    tool_accesses::all()
}

/// Default access inference for builtin tools.
pub fn infer_accesses(
    tool_name: &str,
    input: &serde_json::Value,
    working_dir: &Path,
) -> ToolAccesses {
    match tool_name {
        "Read" | "ReadMediaFile" => {
            if let Some(p) = input.get("path").and_then(|v| v.as_str()) {
                tool_accesses::read_file(resolve_tool_path(working_dir, p))
            } else {
                tool_accesses::all()
            }
        }
        "Write" => {
            if let Some(p) = input.get("path").and_then(|v| v.as_str()) {
                tool_accesses::write_file(resolve_tool_path(working_dir, p))
            } else {
                tool_accesses::all()
            }
        }
        "Edit" => {
            if let Some(p) = input.get("path").and_then(|v| v.as_str()) {
                tool_accesses::read_write_file(resolve_tool_path(working_dir, p))
            } else {
                tool_accesses::all()
            }
        }
        "Grep" => {
            let root = input
                .get("path")
                .or_else(|| input.get("directory"))
                .and_then(|v| v.as_str())
                .unwrap_or(".");
            tool_accesses::search_tree(resolve_tool_path(working_dir, root))
        }
        "Glob" => glob_accesses(input, working_dir),
        "Bash" => bash_accesses(input, working_dir),
        "Agent" | "AskUserQuestion" | "EnterPlanMode" | "WritePlan" | "ExitPlanMode"
        | "SelectTools" | "TodoList" | "Skill" | "Cron" => tool_accesses::all(),
        "Web" | "TaskOutput" => tool_accesses::none(),
        n if n.starts_with("mcp__") => tool_accesses::all(),
        _ => {
            if tool_name.ends_with("Read") {
                tool_accesses::none()
            } else {
                tool_accesses::all()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_do_not_conflict() {
        let a = tool_accesses::read_file("/tmp/a");
        let b = tool_accesses::read_file("/tmp/b");
        assert!(!tool_accesses::conflict(&a, &b));
    }

    #[test]
    fn write_conflicts_with_read_same_path() {
        let a = tool_accesses::write_file("/tmp/a");
        let b = tool_accesses::read_file("/tmp/a");
        assert!(tool_accesses::conflict(&a, &b));
    }

    #[test]
    fn tree_write_conflicts_child_read() {
        let a = tool_accesses::write_tree("/tmp/proj");
        let b = tool_accesses::read_file("/tmp/proj/src/main.rs");
        assert!(tool_accesses::conflict(&a, &b));
    }

    #[test]
    fn glob_literal_prefix_narrows_conflict_scope() {
        let dir = std::env::temp_dir().join(format!("kkagent-acc-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("src/deep")).unwrap();
        let accesses = glob_accesses(&json!({"pattern": "src/**/*.rs"}), &dir);
        let walk = resolve_glob_walk_root(&dir, "src/**/*.rs", None);
        assert_eq!(
            accesses,
            tool_accesses::search_tree(normalize_path(&walk.to_string_lossy()))
        );
        assert!(walk.ends_with("src") || walk.ends_with("src/deep") || walk == dir.join("src"));
        let bash = bash_accesses(&json!({"command": "pwd"}), &dir);
        assert!(!tool_accesses::conflict(&accesses, &bash));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn bash_pwd_is_ambient_and_make_is_all() {
        let cwd = Path::new("/tmp");
        assert_eq!(
            bash_accesses(&json!({"command": "pwd"}), cwd),
            tool_accesses::none()
        );
        assert_eq!(
            bash_accesses(&json!({"command": "make -j8"}), cwd),
            tool_accesses::all()
        );
        let with_path = bash_accesses(&json!({"command": "cat src/main.rs"}), cwd);
        assert!(!tool_accesses::conflict(
            &with_path,
            &tool_accesses::search_tree("/tmp/other")
        ));
    }
}
