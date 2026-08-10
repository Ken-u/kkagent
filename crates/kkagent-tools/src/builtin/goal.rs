use async_trait::async_trait;
use kkagent_protocol::goal::{GoalBudget, GoalManager, GoalStatus};
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
        "Create a new multi-turn goal that will drive autonomous execution across many turns. \
Use SetGoalBudget afterwards to attach hard limits."
    }
    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "objective": {
                    "type": "string",
                    "description": "The objective to pursue. Must have a verifiable end state."
                },
                "description": {
                    "type": "string",
                    "description": "Deprecated alias for objective"
                },
                "completionCriterion": {
                    "type": "string",
                    "description": "How to verify the goal is complete"
                },
                "completion_criterion": {
                    "type": "string",
                    "description": "Alias for completionCriterion"
                },
                "replace": {
                    "type": "boolean",
                    "description": "Replace an existing active/paused/blocked goal instead of failing"
                }
            },
            "required": []
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let objective = input
            .get("objective")
            .or_else(|| input.get("description"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if objective.is_empty() {
            return Ok(ToolOutput::error("objective (or description) is required"));
        }
        let criterion = input
            .get("completionCriterion")
            .or_else(|| input.get("completion_criterion"))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let replace = input
            .get("replace")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if let Some(existing) = self.goal_mgr.get_goal().await {
            if !existing.is_terminal() && !replace {
                return Ok(ToolOutput::error(format!(
                    "A goal is already active ({}). Pass replace=true to replace it.",
                    existing.description
                )));
            }
        }

        // Budgets are set via SetGoalBudget (kimi-aligned).
        let mut goal = self
            .goal_mgr
            .create_goal(objective, GoalBudget::default())
            .await;
        if let Some(c) = criterion {
            self.goal_mgr.set_completion_criterion(&c).await;
            if let Some(g) = self.goal_mgr.get_goal().await {
                goal = g;
            } else {
                let _ = c;
            }
        }
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
        "Update the goal status: active, complete, or blocked (kimi-aligned)."
    }
    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "status": {
                    "type": "string",
                    "enum": ["active", "complete", "blocked", "paused"],
                    "description": "Lifecycle status. Prefer active/complete/blocked."
                }
            },
            "required": ["status"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let status = input.get("status").and_then(|v| v.as_str()).unwrap_or("");

        match status {
            "complete" => {
                self.goal_mgr.complete_goal("completed").await;
                Ok(ToolOutput::success("Goal completed.").with_delivery(
                    "Goal marked complete. Summarize outcomes for the user and stop autonomous goal work.",
                ))
            }
            "blocked" => {
                self.goal_mgr.block_goal("blocked").await;
                Ok(ToolOutput::success("Goal blocked.").with_delivery(
                    "Goal marked blocked. Explain the blocker and wait for user direction.",
                ))
            }
            "paused" => {
                self.goal_mgr.pause_goal().await;
                Ok(ToolOutput::success("Goal paused."))
            }
            "active" => {
                self.goal_mgr.resume_goal().await;
                Ok(ToolOutput::success("Goal resumed."))
            }
            // Legacy alias kept for old transcripts.
            "failed" => {
                self.goal_mgr.block_goal("failed").await;
                Ok(ToolOutput::success(
                    "Goal blocked (legacy status `failed` mapped to blocked).",
                ))
            }
            _ => Ok(ToolOutput::error(format!(
                "Unknown status: {status}. Use active, complete, or blocked."
            ))),
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
        "Set one hard budget limit for the active goal (unit + value). \
Legacy multi-field token_budget/turn_budget/wall_clock_budget_ms still accepted."
    }
    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "unit": {
                    "type": "string",
                    "enum": ["turns", "tokens", "milliseconds", "seconds", "minutes", "hours", "turn", "token", "wall_clock"],
                    "description": "Budget unit (kimi-aligned)"
                },
                "budget_unit": {
                    "type": "string",
                    "description": "Alias for unit"
                },
                "value": {
                    "type": "number",
                    "description": "Positive numeric budget value"
                },
                "turn_budget": {"type": "integer"},
                "token_budget": {"type": "integer"},
                "wall_clock_budget_ms": {"type": "integer"}
            }
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let Some(mut goal) = self.goal_mgr.get_goal().await else {
            return Ok(ToolOutput::error("No active goal."));
        };

        let mut budget = goal.budget.clone();
        let unit = input
            .get("unit")
            .or_else(|| input.get("budget_unit"))
            .and_then(|v| v.as_str());
        if let (Some(unit), Some(value)) = (unit, input.get("value").and_then(|v| v.as_f64())) {
            if value <= 0.0 {
                return Ok(ToolOutput::error("value must be positive"));
            }
            match unit {
                "turns" | "turn" => budget.turn_budget = Some(value.round() as u32),
                "tokens" | "token" => budget.token_budget = Some(value.round() as u64),
                "milliseconds" | "wall_clock" => {
                    budget.wall_clock_budget_ms = Some(value.round() as u64)
                }
                "seconds" => budget.wall_clock_budget_ms = Some((value * 1000.0).round() as u64),
                "minutes" => {
                    budget.wall_clock_budget_ms = Some((value * 60_000.0).round() as u64)
                }
                "hours" => {
                    budget.wall_clock_budget_ms = Some((value * 3_600_000.0).round() as u64)
                }
                other => {
                    return Ok(ToolOutput::error(format!("Unknown budget unit: {other}")));
                }
            }
        } else {
            // Legacy multi-field form
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
        }

        self.goal_mgr.update_budget(budget).await;
        goal = self.goal_mgr.get_goal().await.unwrap_or(goal);
        let _ = GoalStatus::Active;
        Ok(ToolOutput::success(
            serde_json::to_string_pretty(&goal).unwrap_or_default(),
        ))
    }
}
