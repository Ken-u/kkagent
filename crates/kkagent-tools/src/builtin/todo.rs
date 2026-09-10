use crate::{Tool, ToolContext, ToolOutput};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::HashSet, sync::Mutex};

pub struct TodoListTool {
    todos: Mutex<Vec<TodoItem>>,
}

#[derive(Debug, Clone)]
struct TodoItem {
    id: String,
    title: String,
    status: String,
}

#[derive(Deserialize)]
struct TodoWrite {
    id: Option<String>,
    #[serde(alias = "content")]
    title: String,
    #[serde(default = "pending_status")]
    status: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TodoUpdate {
    id: String,
    title: Option<String>,
    status: Option<String>,
}

fn pending_status() -> String {
    "pending".into()
}

fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

impl TodoListTool {
    pub fn new() -> Self {
        Self {
            todos: Mutex::new(Vec::new()),
        }
    }

    pub fn with_items(items: Vec<kkagent_protocol::TodoItemEvent>) -> Self {
        let mut ids = HashSet::new();
        let todos = items
            .into_iter()
            .filter_map(|item| {
                let title = item.content.trim().to_string();
                let id = if item.id.trim().is_empty() || !ids.insert(item.id.clone()) {
                    let id = new_id();
                    ids.insert(id.clone());
                    id
                } else {
                    item.id
                };
                (!title.is_empty()).then(|| TodoItem {
                    id,
                    title,
                    status: Self::normalize_status(&item.status),
                })
            })
            .collect();
        Self {
            todos: Mutex::new(todos),
        }
    }

    fn normalize_status(raw: &str) -> String {
        Self::parse_status(raw).unwrap_or("pending").into()
    }

    fn parse_status(raw: &str) -> Result<&'static str, String> {
        match raw {
            "pending" => Ok("pending"),
            "completed" | "done" => Ok("done"),
            "in_progress" | "in-progress" => Ok("in_progress"),
            "cancelled" | "canceled" => Ok("cancelled"),
            _ => Err(format!("Unknown todo status: {raw}")),
        }
    }

    fn display_status(status: &str) -> &'static str {
        match status {
            "done" => "completed",
            "in_progress" => "in_progress",
            "cancelled" => "cancelled",
            _ => "pending",
        }
    }

    fn items_event_json(todos: &[TodoItem]) -> Value {
        Value::Array(
            todos
                .iter()
                .map(|item| {
                    json!({
                        "id": item.id,
                        "content": item.title,
                        "status": Self::display_status(&item.status),
                    })
                })
                .collect(),
        )
    }

    fn render_list(todos: &[TodoItem]) -> String {
        if todos.is_empty() {
            return "Todo list is empty.".into();
        }
        let lines: Vec<String> = todos
            .iter()
            .map(|t| {
                let marker = match t.status.as_str() {
                    "done" => "[done]",
                    "in_progress" => "[in_progress]",
                    "cancelled" => "[cancelled]",
                    _ => "[pending]",
                };
                format!("  {} {} (id: {})", marker, t.title, t.id)
            })
            .collect();
        format!("Current todo list:\n{}", lines.join("\n"))
    }

    fn render_update(todos: &[TodoItem]) -> String {
        if todos.is_empty() {
            return "Todo list cleared.".into();
        }
        let completed = todos.iter().filter(|t| t.status == "done").count();
        let pending = todos.iter().filter(|t| t.status == "pending").count();
        let cancelled = todos.iter().filter(|t| t.status == "cancelled").count();
        let mut active = todos.iter().filter(|t| t.status == "in_progress");
        let current = active.next();
        let active_count = usize::from(current.is_some()) + active.count();
        let focus = match (active_count, current) {
            (1, Some(item)) => format!(
                "Current task: {} (id: {})\nFocus on this task and its necessary dependencies. \
Take the next concrete action once you have enough information; defer details of pending tasks. \
After completing and verifying this task, or identifying a blocker, update progress and choose the next unblocked task.",
                item.title, item.id
            ),
            (0, _) if pending > 0 => {
                let candidate = todos.iter().find(|t| t.status == "pending").unwrap();
                format!("No task is in_progress. Next pending candidate: {} (id: {}). If unblocked and relevant to the user's current request, mark it in_progress using updates; otherwise read the list and choose another. Defer details of the other pending tasks.", candidate.title, candidate.id)
            }
            (0, _) => "No unfinished todo items remain. Check the result against the user's request before concluding.".into(),
            _ => "Multiple tasks are in_progress. Choose one current task and return the others to pending; only expand the current task and its necessary dependencies.".into(),
        };
        format!(
            "Todo list updated: {} total, {completed} done, {active_count} in_progress, {pending} pending, {cancelled} cancelled.\n{focus}\nCall TodoList with no arguments to read the full list when needed.",
            todos.len()
        )
    }

    fn checked_title(title: &str) -> Result<String, String> {
        let title = title.trim();
        if title.is_empty() {
            return Err("Todo title must not be empty".into());
        }
        Ok(title.into())
    }

    fn replace(todos: &[TodoItem], value: &Value) -> Result<Vec<TodoItem>, String> {
        let items: Vec<TodoWrite> =
            serde_json::from_value(value.clone()).map_err(|e| format!("Invalid todos: {e}"))?;
        let explicit_ids: HashSet<_> = items.iter().filter_map(|t| t.id.clone()).collect();
        let mut ids = HashSet::new();
        items.into_iter().map(|item| {
            let title = Self::checked_title(&item.title)?;
            let status = Self::parse_status(&item.status)?.into();
            let id = if let Some(id) = item.id {
                if !todos.iter().any(|t| t.id == id) {
                    return Err(format!("Unknown todo id: {id}. Omit id for a new item, or read TodoList for current IDs."));
                }
                id
            } else {
                todos.iter().find(|t| t.title == title && !ids.contains(&t.id) && !explicit_ids.contains(&t.id))
                    .map(|t| t.id.clone()).unwrap_or_else(new_id)
            };
            if !ids.insert(id.clone()) {
                return Err(format!("Duplicate todo id: {id}"));
            }
            Ok(TodoItem { id, title, status })
        }).collect()
    }

    fn patch(todos: &[TodoItem], input: &Value) -> Result<Vec<TodoItem>, String> {
        // Stage all changes so a bad ID/status cannot partially update the list.
        let mut next = todos.to_vec();
        if let Some(value) = input.get("updates") {
            let updates: Vec<TodoUpdate> = serde_json::from_value(value.clone())
                .map_err(|e| format!("Invalid updates: {e}"))?;
            let mut ids = HashSet::new();
            for update in updates {
                if !ids.insert(update.id.clone()) {
                    return Err(format!("Duplicate update for todo id: {}", update.id));
                }
                if update.title.is_none() && update.status.is_none() {
                    return Err("Each update needs a title or status".into());
                }
                let item = next.iter_mut().find(|t| t.id == update.id).ok_or_else(|| {
                    format!(
                        "Unknown todo id: {}. Read TodoList for current IDs.",
                        update.id
                    )
                })?;
                if let Some(title) = update.title {
                    item.title = Self::checked_title(&title)?;
                }
                if let Some(status) = update.status {
                    item.status = Self::parse_status(&status)?.into();
                }
            }
        }
        if let Some(value) = input.get("add") {
            let items: Vec<TodoWrite> =
                serde_json::from_value(value.clone()).map_err(|e| format!("Invalid add: {e}"))?;
            for item in items {
                if item.id.is_some() {
                    return Err(
                        "New items in add must omit id; IDs are generated by the tool".into(),
                    );
                }
                next.push(TodoItem {
                    id: new_id(),
                    title: Self::checked_title(&item.title)?,
                    status: Self::parse_status(&item.status)?.into(),
                });
            }
        }
        Ok(next)
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
        "Manage a structured TODO list for tracking progress on multi-step tasks. \
Prefer updates to change only specific tasks by stable id, and add to append new tasks. \
Call with no arguments to read the full list and IDs. Use todos only to create or replace the entire list \
(include unchanged items; preserve their IDs when renaming/reordering); todos: [] clears. \
Do not combine todos with updates/add. Updates and add may be combined atomically. \
Keep task titles brief and at most one task in_progress. Plan overall order and dependencies, \
then work on the current task and its necessary dependencies; defer implementation details of pending tasks. \
Writes return progress and the current focus/candidate with its ID; reads return the full list."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "description": "Create or deliberately replace the complete list, including unchanged items. Prefer updates/add for routine changes. Empty array clears.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": {"type": "string", "description": "Existing stable ID. Omit for new tasks."},
                            "title": {"type": "string", "description": "Short actionable title"},
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "done", "cancelled"],
                                "description": "Current status. Keep at most one task in_progress; mark done after completing the task and its relevant verification."
                            }
                        },
                        "required": ["title", "status"]
                    }
                },
                "updates": {
                    "type": "array",
                    "description": "Preferred for progress updates. Only specified fields change. Use cancelled to drop a task; may complete the current task and start the next in one call.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": {"type": "string", "description": "Stable ID from TodoList; never a list position."},
                            "title": {"type": "string"},
                            "status": {"type": "string", "enum": ["pending", "in_progress", "done", "cancelled"]}
                        },
                        "required": ["id"],
                        "additionalProperties": false
                    }
                },
                "add": {
                    "type": "array",
                    "description": "Append new tasks without rewriting existing tasks. IDs are generated; status defaults to pending.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "title": {"type": "string"},
                            "status": {"type": "string", "enum": ["pending", "in_progress", "done", "cancelled"]}
                        },
                        "required": ["title"]
                    }
                }
            }
        })
    }
    fn read_only(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        if !input.is_object() {
            return Ok(ToolOutput::error("TodoList arguments must be an object"));
        }
        let replace = input.get("todos");
        let patch = input.get("updates").is_some() || input.get("add").is_some();
        if (replace.is_some() && patch)
            || (input.get("action").is_some() && (replace.is_some() || patch))
        {
            return Ok(ToolOutput::error(
                "Use either todos, updates/add, or legacy action; do not mix these forms",
            ));
        }
        // Backward compatible: accept legacy `items`/`action` shape.
        if input.get("action").is_some() {
            return self.execute_legacy(input).await;
        }
        let mut todos = self.todos.lock().unwrap();
        let next = if let Some(value) = replace {
            Self::replace(&todos, value)
        } else if patch {
            Self::patch(&todos, &input)
        } else {
            return Ok(ToolOutput::success_with_data(
                Self::render_list(&todos),
                json!({ "items": Self::items_event_json(&todos) }),
            ));
        };
        match next {
            Ok(next) => *todos = next,
            Err(error) => return Ok(ToolOutput::error(error)),
        }
        Ok(ToolOutput::success_with_data(
            Self::render_update(&todos),
            json!({ "items": Self::items_event_json(&todos) }),
        ))
    }
}

impl TodoListTool {
    async fn execute_legacy(&self, input: Value) -> anyhow::Result<ToolOutput> {
        let action = input
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("list");
        match action {
            "list" => {
                let todos = self.todos.lock().unwrap();
                Ok(ToolOutput::success_with_data(
                    Self::render_list(&todos),
                    json!({ "items": Self::items_event_json(&todos) }),
                ))
            }
            "set" => {
                let merge = input.get("merge").and_then(|v| v.as_bool()).unwrap_or(true);
                let new_items = input
                    .get("items")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                let mut todos = self.todos.lock().unwrap();
                let previous = if merge {
                    Vec::new()
                } else {
                    std::mem::take(&mut *todos)
                };
                for item_val in &new_items {
                    let title = item_val
                        .get("content")
                        .or_else(|| item_val.get("title"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let status = Self::normalize_status(
                        item_val
                            .get("status")
                            .and_then(|v| v.as_str())
                            .unwrap_or("pending"),
                    );
                    if title.is_empty() {
                        continue;
                    }
                    if let Some(existing) = todos.iter_mut().find(|t| t.title == title) {
                        existing.status = status;
                    } else {
                        todos.push(TodoItem {
                            id: previous
                                .iter()
                                .find(|t| t.title == title)
                                .map(|t| t.id.clone())
                                .unwrap_or_else(new_id),
                            title,
                            status,
                        });
                    }
                }
                Ok(ToolOutput::success_with_data(
                    Self::render_update(&todos),
                    json!({ "items": Self::items_event_json(&todos) }),
                ))
            }
            _ => Ok(ToolOutput::error(format!("Unknown action: {}", action))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn run(tool: &TodoListTool, input: Value) -> ToolOutput {
        let ctx = ToolContext {
            working_dir: std::env::temp_dir(),
            session_id: "todo-test".into(),
            turn_id: "todo-test:1".into(),
            plan_file_path: None,
            image: Default::default(),
            tool_call_id: None,
            interrupted: None,
            tools_config: Default::default(),
            model_alias: None,
        };
        tool.execute(input, &ctx).await.unwrap()
    }

    fn items(output: &ToolOutput) -> Vec<kkagent_protocol::TodoItemEvent> {
        serde_json::from_value(output.data.as_ref().unwrap()["items"].clone()).unwrap()
    }

    #[tokio::test]
    async fn incremental_updates_preserve_other_tasks_and_full_snapshot() {
        let tool = TodoListTool::new();
        let created = run(
            &tool,
            json!({"todos": [
                {"title": "Current work", "status": "in_progress"},
                {"title": "Future work", "status": "pending"},
                {"title": "Later work", "status": "pending"}
            ]}),
        )
        .await;
        let original = items(&created);
        assert!(created.content.contains("Current work"));
        assert!(created.content.contains(&original[0].id));
        assert!(!created.content.contains("Future work"));
        assert!(!created.content.contains("Later work"));

        let updated = run(
            &tool,
            json!({
                "updates": [
                    {"id": original[0].id, "status": "done"},
                    {"id": original[1].id, "status": "in_progress", "title": "Renamed work"}
                ],
                "add": [{"title": "New work"}]
            }),
        )
        .await;
        assert!(!updated.is_error);
        let after = items(&updated);
        assert_eq!(after.len(), 4);
        assert_eq!(after[0].status, "completed");
        assert_eq!(after[1].content, "Renamed work");
        assert_eq!(after[1].status, "in_progress");
        assert_eq!(after[2].content, "Later work");
        assert_eq!(after[2].status, "pending");
        for (before, after) in original.iter().zip(&after) {
            assert_eq!(before.id, after.id);
        }
        assert_eq!(after[3].status, "pending");
        assert!(after.iter().take(3).all(|t| t.id != after[3].id));
        assert!(updated.content.contains("Renamed work"));
        assert!(!updated.content.contains("Later work"));
        assert!(!updated.content.contains("New work"));

        let seeded = TodoListTool::with_items(after);
        let read = run(&seeded, json!({})).await;
        assert!(read.content.contains("Later work"));
        assert!(read.content.contains("New work"));
        let result = run(
            &seeded,
            json!({"updates": [{"id": original[2].id, "status": "cancelled"}]}),
        )
        .await;
        assert_eq!(items(&result)[2].status, "cancelled");
    }

    #[tokio::test]
    async fn invalid_batches_leave_the_entire_list_unchanged() {
        let tool = TodoListTool::new();
        let created = run(&tool, json!({"add": [{"title": "Keep"}]})).await;
        let id = &items(&created)[0].id;
        for invalid in [
            json!({"updates": [{"id": id, "status": "done"}, {"id": "missing", "status": "done"}]}),
            json!({"updates": [{"id": id, "status": "done"}], "add": [{"title": ""}]}),
            json!({"updates": [{"id": id, "status": "bogus"}]}),
            json!({"updates": [{"id": id, "title": " "}]}),
            json!({"updates": [{"id": id}]}),
            json!({"updates": [{"id": id, "status": "done"}, {"id": id, "status": "pending"}]}),
            json!({"updates": null}),
            json!({"add": [{"title": "New", "id": "invented"}]}),
            json!({"todos": [], "updates": []}),
            json!({"action": "set", "add": []}),
            json!({"todos": [{"title": "Bad", "status": "bogus"}]}),
            json!({"todos": [{"id": "missing", "title": "Bad", "status": "pending"}]}),
            json!({"todos": [{"id": id, "title": "A"}, {"id": id, "title": "B"}]}),
        ] {
            let output = run(&tool, invalid.clone()).await;
            assert!(output.is_error, "accepted invalid input: {invalid}");
            assert_eq!(run(&tool, json!({})).await.data, created.data);
        }
    }

    #[tokio::test]
    async fn full_replacement_preserves_ids_across_reordering_and_renaming() {
        let tool = TodoListTool::new();
        let original = items(
            &run(
                &tool,
                json!({"todos": [
                    {"title": "First", "status": "pending"},
                    {"title": "Second", "status": "pending"}
                ]}),
            )
            .await,
        );
        let replaced = run(
            &tool,
            json!({"todos": [
                {"id": original[1].id, "title": "Renamed second", "status": "in_progress"},
                {"title": "First", "status": "pending"}
            ]}),
        )
        .await;
        assert_eq!(items(&replaced)[0].id, original[1].id);
        assert_eq!(items(&replaced)[1].id, original[0].id);
        let cleared = run(&tool, json!({"todos": []})).await;
        assert_eq!(cleared.content, "Todo list cleared.");
        assert!(items(&cleared).is_empty());
        assert!(
            run(
                &tool,
                json!({"updates": [{"id": original[0].id, "status": "done"}]})
            )
            .await
            .is_error
        );
    }

    #[tokio::test]
    async fn summaries_guide_selection_without_automatically_changing_status() {
        let tool = TodoListTool::new();
        let pending = run(
            &tool,
            json!({"add": [{"title": "Next"}, {"title": "Later"}]}),
        )
        .await;
        let initial = items(&pending);
        assert!(pending.content.contains("Next pending candidate: Next"));
        assert!(!pending.content.contains("Later"));
        assert!(initial.iter().all(|t| t.status == "pending"));
        let multiple = run(&tool, json!({"updates": initial.iter().map(|t| json!({"id": t.id, "status": "in_progress"})).collect::<Vec<_>>()})).await;
        assert!(multiple.content.contains("Multiple tasks are in_progress"));
        assert!(items(&multiple).iter().all(|t| t.status == "in_progress"));
        let finished = run(
            &tool,
            json!({"updates": [
                {"id": initial[0].id, "status": "done"},
                {"id": initial[1].id, "status": "cancelled"}
            ]}),
        )
        .await;
        assert!(finished
            .content
            .contains("1 done, 0 in_progress, 0 pending, 1 cancelled"));
        assert!(finished.content.contains("No unfinished todo items remain"));
    }

    #[tokio::test]
    async fn legacy_calls_still_merge_and_return_full_data_with_short_content() {
        let tool = TodoListTool::new();
        let first = run(
            &tool,
            json!({"action": "set", "items": [
                {"content": "Current", "status": "in_progress"},
                {"content": "Later", "status": "pending"}
            ]}),
        )
        .await;
        let second = run(
            &tool,
            json!({"action": "set", "items": [{"content": "Current", "status": "done"}]}),
        )
        .await;
        assert_eq!(items(&first)[0].id, items(&second)[0].id);
        assert_eq!(items(&second).len(), 2);
        assert_eq!(items(&second)[0].status, "completed");
        assert!(!first.content.contains("Later"));
        assert!(run(&tool, json!({"action": "list"}))
            .await
            .content
            .contains("Later"));
        let replaced = run(
            &tool,
            json!({"action": "set", "merge": false, "items": [
                {"content": "Later", "status": "in_progress"},
                {"content": "Current", "status": "done"}
            ]}),
        )
        .await;
        assert_eq!(items(&replaced)[0].id, items(&first)[1].id);
        assert_eq!(items(&replaced)[1].id, items(&first)[0].id);
    }

    #[tokio::test]
    async fn same_title_tasks_keep_distinct_ids_during_replacement() {
        let tool = TodoListTool::new();
        let initial = items(
            &run(
                &tool,
                json!({"add": [{"title": "Same"}, {"title": "Same"}]}),
            )
            .await,
        );
        assert_ne!(initial[0].id, initial[1].id);
        let replaced = run(
            &tool,
            json!({"todos": [
                {"title": "Same", "status": "pending"},
                {"id": initial[0].id, "title": "Renamed", "status": "in_progress"}
            ]}),
        )
        .await;
        assert!(!replaced.is_error);
        assert_eq!(items(&replaced)[0].id, initial[1].id);
        assert_eq!(items(&replaced)[1].id, initial[0].id);
    }

    #[test]
    fn seeded_items_preserve_progress_for_the_next_turn() {
        let tool = TodoListTool::with_items(vec![
            kkagent_protocol::TodoItemEvent {
                id: "todo-a".into(),
                content: "Finished".into(),
                status: "completed".into(),
            },
            kkagent_protocol::TodoItemEvent {
                id: "todo-b".into(),
                content: "Running".into(),
                status: "in_progress".into(),
            },
        ]);
        let todos = tool.todos.lock().unwrap();
        assert_eq!(todos.len(), 2);
        assert_eq!(todos[0].status, "done");
        assert_eq!(todos[0].id, "todo-a");
        assert_eq!(todos[1].id, "todo-b");
        assert_eq!(todos[1].status, "in_progress");
        assert!(TodoListTool::render_list(&todos).contains("[in_progress] Running"));
    }
}
