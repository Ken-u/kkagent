//! Wire-level Todo types plus the authoritative session todo state.
//!
//! Lives in the protocol crate so both the agent core (persistence, events)
//! and the tools crate (the stateless `TodoList` tool) can share one
//! implementation without a dependency cycle.

use serde::{Deserialize, Serialize};
use std::sync::RwLock;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Done,
    Cancelled,
}

impl TodoStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            TodoStatus::Pending => "pending",
            TodoStatus::InProgress => "in_progress",
            TodoStatus::Done => "completed",
            TodoStatus::Cancelled => "cancelled",
        }
    }

    pub fn is_finished(&self) -> bool {
        matches!(self, TodoStatus::Done | TodoStatus::Cancelled)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TodoItem {
    /// Stable session-local ID, allocated once on creation. Never derived
    /// from array position and never rewritten by later mutations.
    pub id: String,
    pub title: String,
    pub status: TodoStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TodoOp {
    /// Append a new pending task (auto-promoted when nothing is active).
    Add { title: String },
    /// Rename an existing task by ID.
    Update { id: String, title: String },
    /// Mark a task done by ID (auto-promotes the next pending task).
    Complete { id: String },
    /// Cancel a task by ID (auto-promotes the next pending task).
    Cancel { id: String },
    /// Return the full current list (the only full-state LLM read path).
    List,
    /// Remove every task.
    Clear,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TodoOpResult {
    /// Transition applied; compact model-visible summary in `message`.
    Mutation {
        message: String,
        /// True when the runtime changed the current task (auto-promotion).
        promoted: bool,
    },
    /// Full list read requested by the model.
    List { rendered: String },
}

#[derive(Default)]
pub struct SessionTodoService {
    todos: RwLock<Vec<TodoItem>>,
    next_id: RwLock<u64>,
}

impl SessionTodoService {
    pub fn new() -> Self {
        Self::default()
    }

    /// Full snapshot for TUI/persistence/orchestrator observers.
    pub fn get_todos(&self) -> Vec<TodoItem> {
        self.todos.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Replace the whole list (restore / migration path only; the model-facing
    /// tool never performs full replacement).
    ///
    /// Central invariant enforcement on restore: at most one `in_progress`
    /// item. The first active item is preserved; any subsequent active item
    /// is demoted to pending. When nothing is active and unfinished items
    /// remain, the first pending item is auto-promoted.
    pub fn set_todos(&self, todos: Vec<TodoItem>) {
        let mut max: u64 = 0;
        for item in &todos {
            if let Some(rest) = item.id.strip_prefix("todo-") {
                if let Ok(n) = rest.parse::<u64>() {
                    max = max.max(n);
                }
            }
        }
        let mut normalized = todos;
        normalize_single_active(&mut normalized);
        *self.next_id.write().unwrap_or_else(|e| e.into_inner()) = max;
        *self.todos.write().unwrap_or_else(|e| e.into_inner()) = normalized;
    }

    pub fn clear(&self) {
        self.todos
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    /// Apply one incremental operation. Enforces the single-`in_progress`
    /// invariant and auto-promotes the first pending item whenever the active
    /// task goes away (complete/cancel) or a task is added with nothing active.
    pub fn apply_op(&self, op: TodoOp) -> anyhow::Result<TodoOpResult> {
        let mut todos = self.todos.write().unwrap_or_else(|e| e.into_inner());
        match op {
            TodoOp::Add { title } => {
                let title = title.trim().to_string();
                anyhow::ensure!(!title.is_empty(), "task title must not be empty");
                let id = {
                    let mut next = self.next_id.write().unwrap_or_else(|e| e.into_inner());
                    *next += 1;
                    format!("todo-{next}")
                };
                let promoted = !todos
                    .iter()
                    .any(|t| matches!(t.status, TodoStatus::InProgress));
                let mut item = TodoItem {
                    id,
                    title: title.clone(),
                    status: TodoStatus::Pending,
                };
                if promoted {
                    item.status = TodoStatus::InProgress;
                }
                todos.push(item);
                drop(todos);
                let transition = format!("Task added: {title}.");
                let message = self.transition_summary(Some(transition));
                Ok(TodoOpResult::Mutation { message, promoted })
            }
            TodoOp::Update { id, title } => {
                let title = title.trim().to_string();
                anyhow::ensure!(!title.is_empty(), "task title must not be empty");
                let item = todos
                    .iter_mut()
                    .find(|t| t.id == id)
                    .ok_or_else(|| anyhow::anyhow!("unknown task id: {id}"))?;
                item.title = title;
                drop(todos);
                Ok(TodoOpResult::Mutation {
                    message: self.transition_summary(None),
                    promoted: false,
                })
            }
            TodoOp::Complete { id } => {
                let item = todos
                    .iter_mut()
                    .find(|t| t.id == id)
                    .ok_or_else(|| anyhow::anyhow!("unknown task id: {id}"))?;
                anyhow::ensure!(
                    !matches!(item.status, TodoStatus::Cancelled),
                    "cannot complete a cancelled task: {id}"
                );
                let completed_title = item.title.clone();
                item.status = TodoStatus::Done;
                let promoted = promote_first_pending(&mut todos);
                let promotion = if promoted {
                    current_title(todos.iter())
                } else {
                    None
                };
                drop(todos);
                let message = self.transition_summary(Some(format!(
                    "Task completed: {}.{}",
                    completed_title,
                    promotion
                        .map(|t| format!(" Next task: {t}"))
                        .unwrap_or_default()
                )));
                Ok(TodoOpResult::Mutation { message, promoted })
            }
            TodoOp::Cancel { id } => {
                let item = todos
                    .iter_mut()
                    .find(|t| t.id == id)
                    .ok_or_else(|| anyhow::anyhow!("unknown task id: {id}"))?;
                anyhow::ensure!(
                    !matches!(item.status, TodoStatus::Done),
                    "cannot cancel a completed task: {id}"
                );
                let cancelled_title = item.title.clone();
                item.status = TodoStatus::Cancelled;
                let promoted = promote_first_pending(&mut todos);
                let promotion = if promoted {
                    current_title(todos.iter())
                } else {
                    None
                };
                drop(todos);
                let message = self.transition_summary(Some(format!(
                    "Task cancelled: {}.{}",
                    cancelled_title,
                    promotion
                        .map(|t| format!(" Next task: {t}"))
                        .unwrap_or_default()
                )));
                Ok(TodoOpResult::Mutation { message, promoted })
            }
            TodoOp::List => {
                let rendered = render_todo_list(&todos, "Current todo list:");
                drop(todos);
                Ok(TodoOpResult::List { rendered })
            }
            TodoOp::Clear => {
                todos.clear();
                drop(todos);
                Ok(TodoOpResult::Mutation {
                    message: "Todo list cleared.".into(),
                    promoted: false,
                })
            }
        }
    }

    /// Compact transition summary: `<transition> <current task>. <progress>`.
    /// When `transition` is `None` only the focus/progress line is produced.
    pub fn transition_summary(&self, transition: Option<String>) -> String {
        let todos = self.get_todos();
        let done = todos
            .iter()
            .filter(|t| matches!(t.status, TodoStatus::Done))
            .count();
        let total = todos.len();
        let focus = match todos
            .iter()
            .find(|t| matches!(t.status, TodoStatus::InProgress))
        {
            Some(current) => format!("Current task: {}.", current.title),
            None => "No active task.".to_string(),
        };
        let progress = format!("Progress: {done}/{total} completed.");
        match transition {
            Some(t) if !t.trim().is_empty() => format!("{t} {focus} {progress}"),
            _ => format!("{focus} {progress}"),
        }
    }

    /// Compact one-line focus + progress summary (model-visible write result).
    pub fn summary(todos: &[TodoItem]) -> String {
        let done = todos
            .iter()
            .filter(|t| matches!(t.status, TodoStatus::Done))
            .count();
        let total = todos.len();
        match todos
            .iter()
            .find(|t| matches!(t.status, TodoStatus::InProgress))
        {
            Some(current) => format!(
                "Current task: {}. Progress: {}/{} completed.",
                current.title, done, total
            ),
            None if total > 0 && done == total => {
                format!("All tasks completed. Progress: {done}/{total} completed.")
            }
            None if total > 0 => format!("No active task. Progress: {done}/{total} completed."),
            None => "Todo list is empty.".into(),
        }
    }

    /// Full text rendering (explicit `list` read path only).
    pub fn render(&self) -> String {
        render_todo_list(&self.get_todos(), "Current todo list:")
    }
}

fn current_title<'a, I: Iterator<Item = &'a TodoItem>>(mut todos: I) -> Option<String> {
    todos
        .find(|t| matches!(t.status, TodoStatus::InProgress))
        .map(|t| t.title.clone())
}

/// Enforce single-`in_progress` on restore/full-set: preserve the first
/// active item, demote subsequent active items to pending, and auto-promote
/// the first pending item when nothing is active but unfinished items remain.
fn normalize_single_active(todos: &mut [TodoItem]) {
    let mut active_seen = false;
    for item in todos.iter_mut() {
        if matches!(item.status, TodoStatus::InProgress) {
            if active_seen {
                item.status = TodoStatus::Pending;
            } else {
                active_seen = true;
            }
        }
    }
    if !active_seen {
        promote_first_pending(todos);
    }
}

/// Enforce single-`in_progress`: promote the first pending item only when
/// nothing is currently in progress.
fn promote_first_pending(todos: &mut [TodoItem]) -> bool {
    if todos
        .iter()
        .any(|t| matches!(t.status, TodoStatus::InProgress))
    {
        return false;
    }
    if let Some(item) = todos
        .iter_mut()
        .find(|t| matches!(t.status, TodoStatus::Pending))
    {
        item.status = TodoStatus::InProgress;
        return true;
    }
    false
}

pub fn render_todo_list(todos: &[TodoItem], title: &str) -> String {
    if todos.is_empty() {
        return "Todo list is empty.".into();
    }
    let mut lines = vec![title.to_string()];
    for t in todos {
        lines.push(format!("  [{}] {} ({})", t.status.as_str(), t.title, t.id));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_promotes_first_pending_and_reports_compact_summary() {
        let svc = SessionTodoService::new();
        let r = svc
            .apply_op(TodoOp::Add {
                title: "task 1".into(),
            })
            .unwrap();
        let TodoOpResult::Mutation { message, promoted } = r else {
            panic!("expected mutation");
        };
        assert!(promoted);
        assert_eq!(
            message,
            "Task added: task 1. Current task: task 1. Progress: 0/1 completed."
        );
    }

    #[test]
    fn stable_ids_survive_removals_and_status_changes() {
        let svc = SessionTodoService::new();
        svc.apply_op(TodoOp::Add { title: "a".into() }).unwrap();
        svc.apply_op(TodoOp::Add { title: "b".into() }).unwrap();
        let id_a = svc.get_todos()[0].id.clone();
        svc.apply_op(TodoOp::Complete { id: id_a.clone() }).unwrap();
        let todos = svc.get_todos();
        assert_eq!(todos[0].id, id_a, "ID must not change on status change");
        assert!(todos[1].id != id_a);
    }

    #[test]
    fn completing_current_promotes_next_pending() {
        let svc = SessionTodoService::new();
        svc.apply_op(TodoOp::Add { title: "a".into() }).unwrap();
        svc.apply_op(TodoOp::Add { title: "b".into() }).unwrap();
        let id_a = svc.get_todos()[0].id.clone();
        let r = svc.apply_op(TodoOp::Complete { id: id_a }).unwrap();
        let TodoOpResult::Mutation { message, promoted } = r else {
            panic!("expected mutation");
        };
        assert!(promoted);
        assert!(message.contains("Task completed: a."));
        assert!(message.contains("Current task: b."));
        assert!(message.contains("Progress: 1/2 completed."));
        assert_eq!(
            svc.get_todos()
                .iter()
                .filter(|t| matches!(t.status, TodoStatus::InProgress))
                .count(),
            1
        );
    }

    #[test]
    fn unknown_id_errors_cleanly() {
        let svc = SessionTodoService::new();
        assert!(svc
            .apply_op(TodoOp::Complete {
                id: "todo-99".into()
            })
            .is_err());
        assert!(svc.apply_op(TodoOp::Cancel { id: "nope".into() }).is_err());
        assert!(svc
            .apply_op(TodoOp::Update {
                id: "x".into(),
                title: "t".into()
            })
            .is_err());
    }

    #[test]
    fn list_returns_full_state_and_clear_empties() {
        let svc = SessionTodoService::new();
        svc.apply_op(TodoOp::Add { title: "a".into() }).unwrap();
        let TodoOpResult::List { rendered } = svc.apply_op(TodoOp::List).unwrap() else {
            panic!("expected list");
        };
        assert!(rendered.contains("[in_progress] a"));
        svc.apply_op(TodoOp::Clear).unwrap();
        assert!(svc.get_todos().is_empty());
    }

    #[test]
    fn restore_preserves_existing_ids_and_allocates_after_max() {
        let svc = SessionTodoService::new();
        svc.set_todos(vec![TodoItem {
            id: "todo-7".into(),
            title: "restored".into(),
            status: TodoStatus::InProgress,
        }]);
        svc.apply_op(TodoOp::Add {
            title: "fresh".into(),
        })
        .unwrap();
        let todos = svc.get_todos();
        assert_eq!(todos[0].id, "todo-7");
        assert_eq!(todos[1].id, "todo-8");
    }

    #[test]
    fn restore_demotes_extra_active_items_to_single_in_progress() {
        let svc = SessionTodoService::new();
        svc.set_todos(vec![
            TodoItem {
                id: "todo-1".into(),
                title: "first active".into(),
                status: TodoStatus::InProgress,
            },
            TodoItem {
                id: "todo-2".into(),
                title: "second active".into(),
                status: TodoStatus::InProgress,
            },
            TodoItem {
                id: "todo-3".into(),
                title: "pending".into(),
                status: TodoStatus::Pending,
            },
        ]);
        let todos = svc.get_todos();
        let active: Vec<_> = todos
            .iter()
            .filter(|t| matches!(t.status, TodoStatus::InProgress))
            .collect();
        assert_eq!(active.len(), 1, "exactly one active after restore");
        assert_eq!(active[0].title, "first active", "first active is preserved");
        assert!(matches!(todos[1].status, TodoStatus::Pending));
        assert!(matches!(todos[2].status, TodoStatus::Pending));
    }

    #[test]
    fn restore_promotes_first_pending_when_nothing_is_active() {
        let svc = SessionTodoService::new();
        svc.set_todos(vec![
            TodoItem {
                id: "todo-1".into(),
                title: "done".into(),
                status: TodoStatus::Done,
            },
            TodoItem {
                id: "todo-2".into(),
                title: "later".into(),
                status: TodoStatus::Pending,
            },
        ]);
        let todos = svc.get_todos();
        assert!(matches!(todos[1].status, TodoStatus::InProgress));
        assert_eq!(
            SessionTodoService::summary(&todos),
            "Current task: later. Progress: 1/2 completed."
        );
    }

    #[test]
    fn restore_with_all_finished_keeps_no_active() {
        let svc = SessionTodoService::new();
        svc.set_todos(vec![TodoItem {
            id: "todo-1".into(),
            title: "done".into(),
            status: TodoStatus::Done,
        }]);
        let todos = svc.get_todos();
        assert!(todos
            .iter()
            .all(|t| !matches!(t.status, TodoStatus::InProgress)));
        assert_eq!(
            SessionTodoService::summary(&todos),
            "All tasks completed. Progress: 1/1 completed."
        );
    }

    #[test]
    fn twenty_item_summary_is_compact() {
        let svc = SessionTodoService::new();
        for i in 1..=20 {
            svc.apply_op(TodoOp::Add {
                title: format!("task {i}"),
            })
            .unwrap();
        }
        let summary = SessionTodoService::summary(&svc.get_todos());
        assert!(summary.contains("Current task: task 1."));
        assert!(summary.contains("Progress: 0/20 completed."));
        assert!(summary.len() < 120, "summary must stay compact: {summary}");
    }
}
