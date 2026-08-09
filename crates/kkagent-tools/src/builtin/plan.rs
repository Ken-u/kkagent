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
        "Use this tool when you are in plan mode and have finished writing your plan to the plan file \
         and are ready for user approval.\n\n\
         ## How This Tool Works\n\
         - You should have already written your plan to the plan file specified in the plan mode reminder.\n\
         - This tool does NOT take the plan content as a parameter — it reads the plan from the file you wrote.\n\
         - The user will see the plan and choose 执行 / 修改意见 / 拒绝. In auto permission mode, the tool \
         exits plan mode without asking.\n\n\
         ## Multiple Approaches\n\
         If your plan offers multiple alternative approaches, pass them via the `options` parameter so the \
         user can choose which one to execute. Do not use reserved labels (执行/拒绝/修改意见/Approve/Reject/Revise).\n\n\
         ## Before Using\n\
         - Do NOT use AskUserQuestion to ask \"Is this plan OK?\" — that is exactly what ExitPlanMode does.\n\
         - If rejected with feedback, revise the plan file and call ExitPlanMode again."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "options": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 3,
                    "description": "When the plan contains multiple alternative approaches, list them so the user can choose which to execute. 2-3 distinct approaches work best. Do not use 执行/拒绝/修改意见/Approve/Reject/Revise as labels.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "label": {
                                "type": "string",
                                "minLength": 1,
                                "maxLength": 80,
                                "description": "Short name for this option (1-8 words). Append \"(Recommended)\" if recommended."
                            },
                            "description": {
                                "type": "string",
                                "description": "Brief summary of this approach and its trade-offs."
                            }
                        },
                        "required": ["label"]
                    }
                },
                "summary": {
                    "type": "string",
                    "description": "Optional brief summary (prefer writing the full plan to the plan file)."
                }
            },
            "required": []
        })
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        // Interactive review is handled in the agent loop. This path runs for
        // auto-mode (or allow rules): exit and format the plan from disk.
        let plan_path = ctx
            .working_dir
            .join(".kkagent")
            .join("plans")
            .join(format!("{}.md", ctx.session_id));
        let plan = tokio::fs::read_to_string(&plan_path)
            .await
            .unwrap_or_default();
        if plan.trim().is_empty() {
            return Ok(ToolOutput::error(format!(
                "No plan file found. Write your plan to {} first, then call ExitPlanMode.",
                plan_path.display()
            )));
        }
        let _ = input;
        let path = plan_path.display().to_string();
        Ok(ToolOutput::success(format!(
            "Exited plan mode. Plan mode deactivated. All tools are now available.\n\
             Note: this plan was auto-approved without user review — the user has NOT explicitly approved it. \
             Follow the user's original instructions on whether to proceed with execution; if they asked you to stop, wait, or only summarize after planning, do not start executing.\n\
             Plan saved to: {path}\n\n## Plan (auto-approved, not user-reviewed):\n{plan}"
        )))
    }
}

/// Enter plan mode — agent may only write the session plan file until ExitPlanMode.
pub struct EnterPlanModeTool;

#[async_trait]
impl Tool for EnterPlanModeTool {
    fn name(&self) -> &str {
        "EnterPlanMode"
    }

    fn description(&self) -> &str {
        "Enter plan mode. Getting user sign-off on your approach via ExitPlanMode before writing code \
         prevents wasted effort. While active, you may only write/edit the session plan file. \
         Explore with read-only tools, write a complete plan, then call ExitPlanMode for approval."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "reason": {
                    "type": "string",
                    "description": "Why plan mode is needed"
                }
            }
        })
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let reason = input
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("Planning before implementation");
        Ok(ToolOutput::success(format!(
            "Entered plan mode. Reason: {}. Explore, write the plan file, then call ExitPlanMode when ready for user approval (执行 / 修改意见 / 拒绝).",
            reason
        )))
    }
}
