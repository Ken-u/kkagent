use async_trait::async_trait;
use serde_json::{json, Value};

use crate::{Tool, ToolContext, ToolOutput};

/// Progressive tool disclosure — load deferred tool definitions by name.
///
/// Some tools (e.g. MCP tools) are `Deferred` by default: their full JSON
/// schema is omitted from LLM requests to conserve context. This tool lets
/// the model load those definitions on demand. The loaded names are tracked
/// in `Session.loaded_deferred_tools` by the agent loop.
pub struct SelectToolsTool;

impl SelectToolsTool {
    pub fn new() -> Self {
        Self
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
        "Load one or more deferred tools by name so you can call them. \
         All available tool names are listed in the <tools_added>/<tools_removed> announcements \
         in the system context — fold them in order to get the current list. \
         Pass the exact name(s) you need; their full definitions become available immediately, \
         so you can call them directly in your next tool call."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "tools": {
                    "type": "array",
                    "items": {"type": "string"},
                    "minItems": 1,
                    "description": "Deferred tool names to load. Call with exact names listed in the <tools_added>/<tools_removed> announcements."
                }
            },
            "required": ["tools"]
        })
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let tools: Vec<String> = input
            .get("tools")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        if tools.is_empty() {
            return Ok(ToolOutput::success_with_data(
                "No tools requested.",
                json!({ "tools": [] }),
            ));
        }

        Ok(ToolOutput::success_with_data(
            format!("Loaded tools: {}", tools.join(", ")),
            json!({ "tools": tools }),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> ToolContext {
        ToolContext {
            working_dir: std::env::temp_dir(),
            session_id: "select-tools-test".into(),
            turn_id: "test-turn".into(),
            plan_file_path: None,
            image: kkagent_config::ImageConfig::default(),
            tool_call_id: None,
            interrupted: None,
            tools_config: kkagent_config::ToolsConfig::default(),
            model_alias: None,
        }
    }

    #[tokio::test]
    async fn returns_requested_tool_names() {
        let tool = SelectToolsTool::new();
        let input = json!({
            "tools": ["mcp__server__tool_a", "mcp__server__tool_b"]
        });
        let output = tool.execute(input, &context()).await.unwrap();
        assert!(!output.is_error);
        let tools: Vec<String> = output
            .data
            .as_ref()
            .unwrap()
            .get("tools")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(tools, vec!["mcp__server__tool_a", "mcp__server__tool_b"]);
    }

    #[tokio::test]
    async fn empty_tools_returns_empty_list() {
        let tool = SelectToolsTool::new();
        let input = json!({"tools": []});
        let output = tool.execute(input, &context()).await.unwrap();
        assert!(!output.is_error);
        assert_eq!(
            output
                .data
                .as_ref()
                .unwrap()
                .get("tools")
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            0
        );
    }

    #[test]
    fn description_mentions_deferred_loading() {
        let tool = SelectToolsTool::new();
        assert!(tool.description().contains("deferred"));
    }
}
