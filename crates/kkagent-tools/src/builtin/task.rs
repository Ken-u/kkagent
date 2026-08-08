use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;
use kkagent_protocol::subagent::{SubagentManager, SubagentConfig};

use crate::{Tool, ToolContext, ToolOutput};

pub struct TaskTool {
    subagent_mgr: Arc<SubagentManager>,
}

impl TaskTool {
    pub fn new(subagent_mgr: Arc<SubagentManager>) -> Self {
        Self { subagent_mgr }
    }
}

#[async_trait]
impl Tool for TaskTool {
    fn name(&self) -> &str { "Task" }
    fn description(&self) -> &str {
        "Launch a subagent to handle a complex task autonomously. Each subagent runs in its own context."
    }
    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "description": {
                    "type": "string",
                    "description": "Short description of what the subagent will do"
                },
                "prompt": {
                    "type": "string",
                    "description": "The detailed task for the subagent to perform"
                },
                "model": {
                    "type": "string",
                    "description": "Optional model override for the subagent"
                }
            },
            "required": ["description", "prompt"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let desc = input.get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("Unnamed task");
        let prompt = input.get("prompt")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let model = input.get("model")
            .and_then(|v| v.as_str())
            .map(String::from);

        let config = SubagentConfig {
            agent_id: uuid::Uuid::new_v4().to_string(),
            description: desc.to_string(),
            prompt: prompt.to_string(),
            model,
            working_dir: ctx.working_dir.to_string_lossy().to_string(),
        };

        match self.subagent_mgr.spawn(config).await {
            Ok(agent_id) => {
                Ok(ToolOutput::success(format!("Subagent launched: {} ({})", desc, agent_id)))
            }
            Err(e) => {
                Ok(ToolOutput::error(format!("Failed to launch subagent: {}", e)))
            }
        }
    }
}
