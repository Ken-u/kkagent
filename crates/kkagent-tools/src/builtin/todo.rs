//! Stateless Todo attention-scheduler tool.
//!
//! All authoritative state lives in the session-scoped
//! `SessionTodoService` (shared via [`TodoListHandle`]). Writes are
//! incremental transitions (add/update/complete/cancel/clear) whose
//! model-visible result is a compact summary — the current task plus
//! progress counts. The full structured state flows to TUI/persistence via
//! `AgentEvent::TodoUpdated`, and the model reads the full list only via an
//! explicit `list` op.

use crate::{Tool, ToolContext, ToolOutput};
use async_trait::async_trait;
use kkagent_protocol::todo::{SessionTodoService, TodoOp, TodoOpResult};
use serde_json::{json, Value};
use std::sync::Arc;

/// Shared session-scoped todo state. The host (agent loop / turn runner)
/// clones this handle into the per-turn `ToolRegistry`; the tool itself keeps
/// no mirrored list.
pub type TodoListHandle = Arc<SessionTodoService>;

/// Fallback shared state used by registries built without a live session
/// (e.g. tool inventory listing): isolated, per-registry.
pub fn detached_handle() -> TodoListHandle {
    Arc::new(SessionTodoService::new())
}

pub struct TodoListTool {
    todos: TodoListHandle,
}

impl TodoListTool {
    pub fn new() -> Self {
        Self {
            todos: detached_handle(),
        }
    }

    /// Bind to the session-scoped service (authoritative state).
    pub fn with_service(service: TodoListHandle) -> Self {
        Self { todos: service }
    }

    fn parse_op(value: &Value) -> anyhow::Result<Vec<TodoOp>> {
        let ops = match value {
            Value::Array(ops) => ops.clone(),
            v => match v.get("ops").and_then(|v| v.as_array()) {
                Some(ops) => ops.clone(),
                None => anyhow::bail!("'ops' must be an array of operations"),
            },
        };
        let mut parsed = Vec::with_capacity(ops.len());
        for op in ops {
            let kind = op
                .get("op")
                .or_else(|| op.get("action"))
                .and_then(|v| v.as_str())
                .unwrap_or("list");
            let id = || {
                op.get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string()
            };
            let title = || {
                op.get("title")
                    .or_else(|| op.get("content"))
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string()
            };
            let parsed_op = match kind {
                "add" => TodoOp::Add { title: title() },
                "update" | "rename" => TodoOp::Update {
                    id: id(),
                    title: title(),
                },
                "complete" | "done" => TodoOp::Complete { id: id() },
                "cancel" | "cancelled" => TodoOp::Cancel { id: id() },
                "list" => TodoOp::List,
                "clear" => TodoOp::Clear,
                other => anyhow::bail!("unknown todo op: {other}"),
            };
            parsed.push(parsed_op);
        }
        Ok(parsed)
    }
}

impl Default for TodoListTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for TodoListTool {
    fn name(&self) -> &str {
        "TodoList"
    }
    fn description(&self) -> &str {
        "Track execution checkpoints for multi-step work as an attention scheduler. \
The runtime keeps the full list; you normally see only the current task and progress. \
Change it only on material transitions: starting work (add), finishing a task (complete), \
abandoning a task (cancel), replanning after a scope change (add/update, list to inspect). \
Completing the current task automatically promotes the next pending one — do not issue \
separate start commands. Never re-read or re-write the list just because turns passed; \
stay focused on the current task."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "ops": {
                    "type": "array",
                    "description": "Incremental operations to apply in order.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "op": {
                                "type": "string",
                                "enum": ["add", "update", "complete", "cancel", "list", "clear"],
                                "description": "Operation kind"
                            },
                            "id": {"type": "string", "description": "Existing task id (update/complete/cancel)"},
                            "title": {"type": "string", "description": "Task title (add/update)"}
                        },
                        "required": ["op"]
                    }
                }
            },
            "required": ["ops"]
        })
    }
    fn read_only(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let ops = Self::parse_op(&input)?;
        let op_count = ops.len();
        let mut last_transition = String::new();
        let mut wrote = false;
        for op in ops {
            match self.todos.apply_op(op)? {
                TodoOpResult::Mutation { message, .. } => {
                    wrote = true;
                    last_transition = message;
                }
                TodoOpResult::List { rendered } => {
                    return Ok(ToolOutput::success(rendered));
                }
            }
        }
        // Batch mutations report one compact transition summary — never a
        // per-op replay, and never the full list.
        let message = if wrote {
            if op_count == 1 {
                // Single transition: keep the transition prefix ("Task
                // completed: A. Current task: B. Progress: 4/8 completed.").
                last_transition
            } else {
                self.todos.transition_summary(None)
            }
        } else {
            "Todo state updated.".into()
        };
        Ok(ToolOutput::success(message))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(tool: &TodoListTool, ops: Value) -> anyhow::Result<ToolOutput> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(tool.execute(ops, &test_ctx()))
    }

    fn ok(tool: &TodoListTool, ops: Value) -> ToolOutput {
        run(tool, ops).unwrap()
    }

    fn test_ctx() -> ToolContext {
        ToolContext {
            working_dir: std::env::temp_dir(),
            session_id: "test".into(),
            turn_id: "test:1".into(),
            plan_file_path: None,
            image: Default::default(),
            tool_call_id: None,
            interrupted: None,
            tools_config: Default::default(),
            model_alias: None,
        }
    }

    fn add(tool: &TodoListTool, title: &str) {
        ok(tool, json!({ "ops": [{ "op": "add", "title": title }] }));
    }

    #[test]
    fn twenty_item_write_stays_compact_and_never_echoes_the_list() {
        let tool = TodoListTool::new();
        let mut ops = Vec::new();
        for i in 1..=20 {
            ops.push(json!({ "op": "add", "title": format!("task {i}") }));
        }
        let out = ok(&tool, json!(ops));
        assert!(!out.is_error);
        assert!(out.content.contains("Current task: task 1."));
        assert!(out.content.contains("Progress: 0/20 completed."));
        assert!(!out.content.contains("[pending]"), "must not echo the list");
        // The batch summary is per-op; the final line names only the current
        // task. "task 20" appears in transition lines ("Task added: task 20")
        // but the full list render (e.g. "[pending] task 20") must not.
        assert!(
            !out.content.contains("[pending] task 20"),
            "must not rescan items"
        );
        assert!(out.content.len() < 400, "compact summary: {}", out.content);
    }

    #[test]
    fn complete_promotes_next_and_reports_transition() {
        let tool = TodoListTool::new();
        add(&tool, "task 1");
        add(&tool, "task 2");
        let svc = tool.todos.clone();
        let id1 = svc.get_todos()[0].id.clone();
        let out = ok(&tool, json!([{ "op": "complete", "id": id1 }]));
        assert!(out.content.contains("Task completed: task 1."));
        assert!(out.content.contains("Current task: task 2."));
        assert!(out.content.contains("Progress: 1/2 completed."));
        assert!(!out.content.contains("[pending]"));
    }

    #[test]
    fn explicit_list_returns_full_state() {
        let tool = TodoListTool::new();
        add(&tool, "a");
        add(&tool, "b");
        let out = ok(&tool, json!([{ "op": "list" }]));
        assert!(out.content.contains("Current todo list:"));
        assert!(out.content.contains("[in_progress] a"));
        assert!(out.content.contains("[pending] b"));
        assert!(out.content.contains("todo-1"));
    }

    #[test]
    fn shared_service_is_the_authoritative_state() {
        let handle = detached_handle();
        let tool = TodoListTool::with_service(handle.clone());
        add(&tool, "shared");
        assert_eq!(handle.get_todos().len(), 1);
        // A second tool bound to the same service observes the mutation.
        let reader = TodoListTool::with_service(handle);
        let out = ok(&reader, json!([{ "op": "list" }]));
        assert!(out.content.contains("shared"));
    }

    #[test]
    fn unknown_id_and_unknown_op_error_cleanly() {
        let tool = TodoListTool::new();
        let out = run(&tool, json!([{ "op": "complete", "id": "todo-99" }])).unwrap_err();
        let _ = out;
        let out = run(&tool, json!([{ "op": "explode" }])).unwrap_err();
        let _ = out;
    }

    #[test]
    fn clear_empties_and_reports() {
        let tool = TodoListTool::new();
        add(&tool, "a");
        let out = ok(&tool, json!([{ "op": "clear" }]));
        assert!(out.content.contains("Todo list cleared."));
        assert!(tool.todos.get_todos().is_empty());
    }
}
