//! Tool resource accesses + conflict matrix (kimi `ToolAccesses`).

use std::path::{Component, Path, PathBuf};

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

fn file_operations_conflict(
    left: ToolFileAccessOperation,
    right: ToolFileAccessOperation,
) -> bool {
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

/// Default access inference for builtin tools.
pub fn infer_accesses(tool_name: &str, input: &serde_json::Value, working_dir: &Path) -> ToolAccesses {
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
        "Grep" | "Glob" => {
            let root = input
                .get("path")
                .or_else(|| input.get("directory"))
                .and_then(|v| v.as_str())
                .unwrap_or(".");
            tool_accesses::search_tree(resolve_tool_path(working_dir, root))
        }
        "Bash" | "Task" | "Agent" | "AgentSwarm" | "TaskStop" | "AskUserQuestion"
        | "EnterPlanMode" | "ExitPlanMode" | "SelectTools" | "TodoList" | "Skill"
        | "CronCreate" | "CronList" | "CronDelete" => tool_accesses::all(),
        "WebSearch" | "FetchURL" | "TaskOutput" | "TaskList" | "GetGoal" => tool_accesses::none(),
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
}
