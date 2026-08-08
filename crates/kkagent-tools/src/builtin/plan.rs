use async_trait::async_trait;
use serde_json::{json, Value};

use crate::{Tool, ToolContext, ToolOutput};

/// Signal that the agent should leave plan mode after writing a complete plan.
pub struct ExitPlanModeTool;

#[async_trait]
impl Tool for ExitPlanModeTool {
    fn name(&self) -> &str {
        "ExitPlanMode"
    }

    fn description(&self) -> &str {
        "Exit plan mode after you have written a complete plan to the plan file. \
         Call this only when the plan is ready for the user to review and approve. \
         Do not call this to start implementation — the user will exit plan mode \
         (or approve) before edits to non-plan files are allowed."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "summary": {
                    "type": "string",
                    "description": "Brief summary of the plan for the user"
                }
            },
            "required": []
        })
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let summary = input
            .get("summary")
            .and_then(|v| v.as_str())
            .unwrap_or("Plan is ready for review.");
        Ok(ToolOutput::success(format!(
            "Exited plan mode. {}",
            summary
        )))
    }
}
