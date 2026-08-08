use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Mutex;
use crate::{Tool, ToolContext, ToolOutput};

pub struct TodoListTool {
    items: Mutex<Vec<TodoItem>>,
}

#[derive(Debug, Clone)]
struct TodoItem {
    id: String,
    content: String,
    status: String,
}

impl TodoListTool {
    pub fn new() -> Self {
        Self {
            items: Mutex::new(Vec::new()),
        }
    }

    fn items_json(items: &[TodoItem]) -> Value {
        Value::Array(
            items
                .iter()
                .map(|item| {
                    json!({
                        "id": item.id,
                        "content": item.content,
                        "status": item.status,
                    })
                })
                .collect(),
        )
    }

    fn render_list(items: &[TodoItem]) -> String {
        items
            .iter()
            .map(|item| {
                let icon = match item.status.as_str() {
                    "completed" | "done" => "✓",
                    "in_progress" => "▸",
                    "cancelled" => "✗",
                    _ => "○",
                };
                format!("{} [{}] {}", icon, item.id, item.content)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[async_trait]
impl Tool for TodoListTool {
    fn name(&self) -> &str {
        "TodoList"
    }
    fn description(&self) -> &str {
        "Manage a structured TODO list for tracking progress on multi-step tasks."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["list", "set"], "description": "Action to perform"},
                "items": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": {"type": "string"},
                            "content": {"type": "string"},
                            "status": {"type": "string", "enum": ["pending", "in_progress", "completed", "cancelled"]}
                        },
                        "required": ["id", "content", "status"]
                    },
                    "description": "TODO items to set/update"
                },
                "merge": {"type": "boolean", "description": "Merge with existing items (default: true)"}
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let action = input.get("action").and_then(|v| v.as_str()).unwrap_or("list");

        match action {
            "list" => {
                let items = self.items.lock().unwrap();
                if items.is_empty() {
                    return Ok(ToolOutput::success_with_data(
                        "No TODO items.",
                        json!({ "items": [] }),
                    ));
                }
                let rendered = Self::render_list(&items);
                Ok(ToolOutput::success_with_data(
                    rendered,
                    json!({ "items": Self::items_json(&items) }),
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

                let mut items = self.items.lock().unwrap();

                if !merge {
                    items.clear();
                }

                for item_val in &new_items {
                    let id = item_val
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let content = item_val
                        .get("content")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let status = item_val
                        .get("status")
                        .and_then(|v| v.as_str())
                        .unwrap_or("pending")
                        .to_string();

                    if let Some(existing) = items.iter_mut().find(|i| i.id == id) {
                        existing.content = content;
                        existing.status = status;
                    } else {
                        items.push(TodoItem {
                            id,
                            content,
                            status,
                        });
                    }
                }

                let msg = if items.is_empty() {
                    "Todo list cleared.".to_string()
                } else {
                    format!(
                        "Todo list updated.\n{}",
                        Self::render_list(&items)
                    )
                };
                Ok(ToolOutput::success_with_data(
                    msg,
                    json!({ "items": Self::items_json(&items) }),
                ))
            }
            _ => Ok(ToolOutput::error(format!("Unknown action: {}", action))),
        }
    }
}
