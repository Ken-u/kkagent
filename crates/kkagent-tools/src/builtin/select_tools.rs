use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::Mutex;

use crate::{Tool, ToolContext, ToolOutput};

/// Progressive tool disclosure — restrict which tools the model may call next.
pub struct SelectToolsTool {
    enabled: Mutex<Option<HashSet<String>>>,
}

impl SelectToolsTool {
    pub fn new() -> Self {
        Self {
            enabled: Mutex::new(None),
        }
    }

    pub fn current_filter(&self) -> Option<HashSet<String>> {
        self.enabled.lock().unwrap().clone()
    }
}

impl Default for SelectToolsTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for SelectToolsTool {
    fn name(&self) -> &str {
        "SelectTools"
    }

    fn description(&self) -> &str {
        "Progressively disclose tools. Pass `tools` (array of tool names) to enable only those \
(plus always-available control tools). Pass empty array or omit to restore full tool set."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "tools": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Tool names to enable. Empty/omit restores all tools."
                }
            }
        })
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let mut guard = self.enabled.lock().unwrap();
        match input.get("tools").and_then(|v| v.as_array()) {
            None => {
                *guard = None;
                Ok(ToolOutput::success_with_data(
                    "Tool filter cleared — all tools available.",
                    json!({ "tools": Value::Null }),
                ))
            }
            Some(arr) if arr.is_empty() => {
                *guard = None;
                Ok(ToolOutput::success_with_data(
                    "Tool filter cleared — all tools available.",
                    json!({ "tools": Value::Null }),
                ))
            }
            Some(arr) => {
                let set: HashSet<String> = arr
                    .iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect();
                // Always keep SelectTools itself available.
                let mut with_self = set.clone();
                with_self.insert("SelectTools".into());
                let list: Vec<String> = with_self.iter().cloned().collect();
                *guard = Some(with_self);
                Ok(ToolOutput::success_with_data(
                    format!("Enabled tools: {}", list.join(", ")),
                    json!({ "tools": list }),
                ))
            }
        }
    }
}
