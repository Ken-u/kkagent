use async_trait::async_trait;
use kkagent_protocol::goal::{GoalBudget, GoalManager};
use serde_json::Value;
use std::sync::Arc;

use crate::{Tool, ToolContext, ToolOutput};

pub struct CreateGoalTool {
    goal_mgr: Arc<GoalManager>,
}

impl CreateGoalTool {
    pub fn new(goal_mgr: Arc<GoalManager>) -> Self {
        Self { goal_mgr }
    }
}

#[async_trait]
impl Tool for CreateGoalTool {
    fn name(&self) -> &str {
        "CreateGoal"
    }
    fn description(&self) -> &str {
        "Create a new multi-turn goal that will drive autonomous execution across many turns."
    }
    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "description": {
                    "type": "string",
                    "description": "A clear description of the goal to accomplish"
                },
                "turn_budget": {
                    "type": "integer",
                    "description": "Max turns allowed (default: unlimited)"
                },
                "token_budget": {
                    "type": "integer",
                    "description": "Max tokens allowed (default: unlimited)"
                },
                "wall_clock_budget_ms": {
                    "type": "integer",
                    "description": "Max wall-clock milliseconds (default: unlimited)"
                }
            },
            "required": ["description"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let desc = input
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("Unnamed goal");

        let budget = GoalBudget {
            turn_budget: input
                .get("turn_budget")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32),
            token_budget: input.get("token_budget").and_then(|v| v.as_u64()),
            wall_clock_budget_ms: input.get("wall_clock_budget_ms").and_then(|v| v.as_u64()),
        };

        let goal = self.goal_mgr.create_goal(desc, budget).await;
        Ok(ToolOutput::success(
            serde_json::to_string_pretty(&goal).unwrap_or_default(),
        ))
    }
}

pub struct GetGoalTool {
    goal_mgr: Arc<GoalManager>,
}

impl GetGoalTool {
    pub fn new(goal_mgr: Arc<GoalManager>) -> Self {
        Self { goal_mgr }
    }
}

#[async_trait]
impl Tool for GetGoalTool {
    fn name(&self) -> &str {
        "GetGoal"
    }
    fn description(&self) -> &str {
        "Get the current goal status, budget usage, and progress."
    }
    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {}
        })
    }
    fn read_only(&self) -> bool {
        true
    }

    async fn execute(&self, _input: Value, _ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        match self.goal_mgr.get_goal().await {
            Some(goal) => Ok(ToolOutput::success(
                serde_json::to_string_pretty(&goal).unwrap_or_default(),
            )),
            None => Ok(ToolOutput::success("No active goal.")),
        }
    }
}

pub struct UpdateGoalTool {
    goal_mgr: Arc<GoalManager>,
}

impl UpdateGoalTool {
    pub fn new(goal_mgr: Arc<GoalManager>) -> Self {
        Self { goal_mgr }
    }
}

#[async_trait]
impl Tool for UpdateGoalTool {
    fn name(&self) -> &str {
        "UpdateGoal"
    }
    fn description(&self) -> &str {
        "Update the goal status: complete, fail, pause, or resume."
    }
    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "status": {
                    "type": "string",
                    "enum": ["complete", "failed", "paused", "active"],
                    "description": "New goal status"
                },
                "reason": {
                    "type": "string",
                    "description": "Reason for the status change"
                }
            },
            "required": ["status"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let status = input.get("status").and_then(|v| v.as_str()).unwrap_or("");
        let reason = input
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("No reason given");

        match status {
            "complete" => {
                self.goal_mgr.complete_goal(reason).await;
                Ok(ToolOutput::success("Goal completed."))
            }
            "failed" => {
                self.goal_mgr.fail_goal(reason).await;
                Ok(ToolOutput::success("Goal failed."))
            }
            "paused" => {
                self.goal_mgr.pause_goal().await;
                Ok(ToolOutput::success("Goal paused."))
            }
            "active" => {
                self.goal_mgr.resume_goal().await;
                Ok(ToolOutput::success("Goal resumed."))
            }
            _ => Ok(ToolOutput::error(format!("Unknown status: {}", status))),
        }
    }
}

pub struct SetGoalBudgetTool {
    goal_mgr: Arc<GoalManager>,
}

impl SetGoalBudgetTool {
    pub fn new(goal_mgr: Arc<GoalManager>) -> Self {
        Self { goal_mgr }
    }
}

#[async_trait]
impl Tool for SetGoalBudgetTool {
    fn name(&self) -> &str {
        "SetGoalBudget"
    }
    fn description(&self) -> &str {
        "Update the active goal's turn/token/wall-clock budget without changing its status."
    }
    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "turn_budget": {
                    "type": "integer",
                    "description": "Max turns (omit to leave unchanged; null to clear)"
                },
                "token_budget": {
                    "type": "integer",
                    "description": "Max tokens (omit to leave unchanged; null to clear)"
                },
                "wall_clock_budget_ms": {
                    "type": "integer",
                    "description": "Max wall-clock ms (omit to leave unchanged; null to clear)"
                }
            }
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let Some(mut goal) = self.goal_mgr.get_goal().await else {
            return Ok(ToolOutput::error("No active goal."));
        };

        let mut budget = goal.budget.clone();
        if input
            .as_object()
            .map(|o| o.contains_key("turn_budget"))
            .unwrap_or(false)
        {
            budget.turn_budget = input
                .get("turn_budget")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);
        }
        if input
            .as_object()
            .map(|o| o.contains_key("token_budget"))
            .unwrap_or(false)
        {
            budget.token_budget = input.get("token_budget").and_then(|v| v.as_u64());
        }
        if input
            .as_object()
            .map(|o| o.contains_key("wall_clock_budget_ms"))
            .unwrap_or(false)
        {
            budget.wall_clock_budget_ms =
                input.get("wall_clock_budget_ms").and_then(|v| v.as_u64());
        }

        self.goal_mgr.update_budget(budget).await;
        goal = self.goal_mgr.get_goal().await.unwrap_or(goal);
        Ok(ToolOutput::success(
            serde_json::to_string_pretty(&goal).unwrap_or_default(),
        ))
    }
}
