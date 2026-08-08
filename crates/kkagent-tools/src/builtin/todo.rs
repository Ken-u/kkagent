use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Mutex;
use crate::{Tool, ToolContext, ToolOutput};

pub struct TodoListTool {
    todos: Mutex<Vec<TodoItem>>,
}

#[derive(Debug, Clone)]
struct TodoItem {
    title: String,
    status: String,
}

impl TodoListTool {
    pub fn new() -> Self {
        Self {
            todos: Mutex::new(Vec::new()),
        }
    }

    fn normalize_status(raw: &str) -> String {
        match raw {
            "completed" | "done" => "done".into(),
            "in_progress" | "in-progress" => "in_progress".into(),
            "cancelled" | "canceled" => "cancelled".into(),
            _ => "pending".into(),
        }
    }

    fn display_status(status: &str) -> &'static str {
        match status {
            "done" => "completed",
            other if other == "in_progress" => "in_progress",
            "cancelled" => "cancelled",
            _ => "pending",
        }
    }

    fn items_event_json(todos: &[TodoItem]) -> Value {
        Value::Array(
            todos
                .iter()
                .enumerate()
                .map(|(i, item)| {
                    json!({
                        "id": format!("{}", i + 1),
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
                format!("  {} {}", marker, t.title)
            })
            .collect();
        format!("Current todo list:\n{}", lines.join("\n"))
    }
}

#[async_trait]
impl Tool for TodoListTool {
    fn name(&self) -> &str {
        "TodoList"
    }
    fn description(&self) -> &str {
        "Manage a structured TODO list for tracking progress on multi-step tasks. \
Pass todos to replace the list, omit todos to read, or pass an empty array to clear."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "description": "Updated todo list. Omit to read. Empty array clears.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "title": {"type": "string", "description": "Short actionable title"},
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "done"],
                                "description": "Current status"
                            }
                        },
                        "required": ["title", "status"]
                    }
                }
            }
        })
    }
    fn read_only(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        // Backward compatible: accept legacy `items`/`action` shape.
        if input.get("todos").is_none() && input.get("action").is_some() {
            return self.execute_legacy(input).await;
        }

        match input.get("todos") {
            None => {
                let todos = self.todos.lock().unwrap();
                let rendered = Self::render_list(&todos);
                Ok(ToolOutput::success_with_data(
                    rendered,
                    json!({ "items": Self::items_event_json(&todos) }),
                ))
            }
            Some(Value::Array(arr)) => {
                let mut next = Vec::new();
                for item in arr {
                    let title = item
                        .get("title")
                        .or_else(|| item.get("content"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    if title.is_empty() {
                        continue;
                    }
                    let status = Self::normalize_status(
                        item.get("status")
                            .and_then(|v| v.as_str())
                            .unwrap_or("pending"),
                    );
                    next.push(TodoItem { title, status });
                }
                let mut todos = self.todos.lock().unwrap();
                *todos = next;
                let msg = if todos.is_empty() {
                    "Todo list cleared.".to_string()
                } else {
                    format!("Todo list updated.\n{}", Self::render_list(&todos))
                };
                Ok(ToolOutput::success_with_data(
                    msg,
                    json!({ "items": Self::items_event_json(&todos) }),
                ))
            }
            Some(_) => Ok(ToolOutput::error("'todos' must be an array")),
        }
    }
}

impl TodoListTool {
    async fn execute_legacy(&self, input: Value) -> anyhow::Result<ToolOutput> {
        let action = input.get("action").and_then(|v| v.as_str()).unwrap_or("list");
        match action {
            "list" => {
                let todos = self.todos.lock().unwrap();
                Ok(ToolOutput::success_with_data(
                    Self::render_list(&todos),
                    json!({ "items": Self::items_event_json(&todos) }),
                ))
            }
            "set" => {
                let merge = input
                    .get("merge")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                let new_items = input
                    .get("items")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                let mut todos = self.todos.lock().unwrap();
                if !merge {
                    todos.clear();
                }
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
                        todos.push(TodoItem { title, status });
                    }
                }
                let msg = if todos.is_empty() {
                    "Todo list cleared.".to_string()
                } else {
                    format!("Todo list updated.\n{}", Self::render_list(&todos))
                };
                Ok(ToolOutput::success_with_data(
                    msg,
                    json!({ "items": Self::items_event_json(&todos) }),
                ))
            }
            _ => Ok(ToolOutput::error(format!("Unknown action: {}", action))),
        }
    }
}
