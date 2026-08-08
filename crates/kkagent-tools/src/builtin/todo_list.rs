use async_trait::async_trait;
use serde_json::{json, Value};

use crate::registry::{Tool, ToolContext, ToolOutput};

pub struct TodoListTool;

#[async_trait]
impl Tool for TodoListTool {
    fn name(&self) -> &str { "TodoList" }
    fn description(&self) -> &str {
        "Manage a task list. Pass todos array to update, omit to query, empty array to clear."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "title": {"type": "string"},
                            "status": {"type": "string", "enum": ["pending", "in_progress", "done"]}
                        },
                        "required": ["title", "status"]
                    }
                }
            }
        })
    }
    fn read_only(&self) -> bool { true }

    async fn execute(&self, _input: Value, _ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        // TodoList state is managed by the core session, this is a passthrough
        Ok(ToolOutput::success("Todo list updated."))
    }
}
