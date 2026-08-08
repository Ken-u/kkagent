use async_trait::async_trait;
use serde_json::{json, Value};

use crate::{Tool, ToolContext, ToolOutput};

/// Schema-only registration; AgentLoop intercepts execution to wait for the user.
pub struct AskUserQuestionTool;

#[async_trait]
impl Tool for AskUserQuestionTool {
    fn name(&self) -> &str {
        "AskUserQuestion"
    }

    fn description(&self) -> &str {
        "Ask the user a question with optional multiple-choice options. \
Use when you need a decision, preference, or clarification before continuing. \
Unavailable in auto mode."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "The question to ask the user"
                },
                "options": {
                    "type": "array",
                    "description": "Optional choices. If omitted, free-text answer is expected.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": {"type": "string"},
                            "label": {"type": "string"}
                        },
                        "required": ["id", "label"]
                    }
                },
                "allow_multiple": {
                    "type": "boolean",
                    "description": "Allow selecting multiple options (default false)"
                },
                "allow_free_text": {
                    "type": "boolean",
                    "description": "Allow a free-text answer in addition to options (default true if no options)"
                },
                "background": {
                    "type": "boolean",
                    "description": "If true, park the question as a background task the user can answer later (default false)"
                }
            },
            "required": ["question"]
        })
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn execute(&self, _input: Value, _ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        // Reached only if AgentLoop failed to intercept — should not happen.
        Ok(ToolOutput::error(
            "AskUserQuestion must be handled by the agent loop",
        ))
    }
}
