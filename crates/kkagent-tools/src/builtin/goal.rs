use async_trait::async_trait;
use kkagent_protocol::goal::{GoalBudget, GoalManager};
use serde_json::Value;
use std::sync::Arc;

use crate::{Tool, ToolContext, ToolOutput};

/// Unified goal tool (`Goal`) — subsumes the former CreateGoal / GetGoal /
/// UpdateGoal / SetGoalBudget quartet behind a single `action` parameter.
pub struct GoalTool {
    goal_mgr: Arc<GoalManager>,
}

impl GoalTool {
    pub fn new(goal_mgr: Arc<GoalManager>) -> Self {
        Self { goal_mgr }
    }

    async fn create(&self, input: &Value) -> ToolOutput {
        let objective = input
            .get("objective")
            .or_else(|| input.get("description"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if objective.is_empty() {
            return ToolOutput::error("objective (or description) is required");
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
                return ToolOutput::error(format!(
                    "A goal is already active ({}). Pass replace=true to replace it.",
                    existing.description
                ));
            }
            if replace {
                let goal = self
                    .goal_mgr
                    .replace_goal(objective, GoalBudget::default())
                    .await;
                if let Some(c) = criterion {
                    self.goal_mgr.set_completion_criterion(&c).await;
                }
                let goal = self.goal_mgr.get_goal().await.unwrap_or(goal);
                return ToolOutput::success(
                    serde_json::to_string_pretty(&goal).unwrap_or_default(),
                );
            }
        }

        let mut goal = self
            .goal_mgr
            .create_goal(objective, GoalBudget::default())
            .await;
        if let Some(c) = criterion {
            self.goal_mgr.set_completion_criterion(&c).await;
            if let Some(g) = self.goal_mgr.get_goal().await {
                goal = g;
            }
        }
        ToolOutput::success(serde_json::to_string_pretty(&goal).unwrap_or_default())
    }

    async fn get(&self) -> ToolOutput {
        match self.goal_mgr.snapshot_with_budget().await {
            Some((goal, budget)) => ToolOutput::success(
                serde_json::to_string_pretty(&serde_json::json!({
                    "goal": goal,
                    "budget": budget,
                }))
                .unwrap_or_default(),
            ),
            None => ToolOutput::success("No active goal."),
        }
    }

    async fn update(&self, input: &Value) -> ToolOutput {
        let status = input.get("status").and_then(|v| v.as_str()).unwrap_or("");

        match status {
            "complete" => {
                let finished = self.goal_mgr.complete_goal("completed").await;
                let body = finished
                    .map(|g| serde_json::to_string_pretty(&g).unwrap_or_default())
                    .unwrap_or_else(|| "Goal completed.".into());
                ToolOutput::success(body)
                    .with_delivery(
                        "Goal marked complete and cleared. Summarize outcomes for the user and stop autonomous goal work.",
                    )
                    .with_stop_turn()
            }
            "blocked" => {
                self.goal_mgr.block_goal("blocked").await;
                ToolOutput::success("Goal blocked.")
                    .with_delivery(
                        "Goal marked blocked. Explain the blocker and wait for user direction.",
                    )
                    .with_stop_turn()
            }
            "paused" => {
                self.goal_mgr.pause_goal().await;
                ToolOutput::success("Goal paused.").with_stop_turn()
            }
            "active" => {
                self.goal_mgr.resume_goal().await;
                ToolOutput::success("Goal resumed.")
            }
            // Legacy alias kept for old transcripts.
            "failed" => {
                self.goal_mgr.block_goal("failed").await;
                ToolOutput::success("Goal blocked (legacy status `failed` mapped to blocked).")
                    .with_stop_turn()
            }
            _ => ToolOutput::error(format!(
                "Unknown status: {status}. Use active, complete, or blocked."
            )),
        }
    }

    async fn budget(&self, input: &Value) -> ToolOutput {
        let Some(mut goal) = self.goal_mgr.get_goal().await else {
            return ToolOutput::error("No active goal.");
        };

        let mut budget = goal.budget.clone();
        let unit = input
            .get("unit")
            .or_else(|| input.get("budget_unit"))
            .and_then(|v| v.as_str());
        if let (Some(unit), Some(value)) = (unit, input.get("value").and_then(|v| v.as_f64())) {
            if !value.is_finite() || value <= 0.0 {
                return ToolOutput::error("value must be a finite positive number");
            }
            match unit {
                "turns" | "turn" => budget.turn_budget = Some(value.round() as u32),
                "tokens" | "token" => budget.token_budget = Some(value.round() as u64),
                "milliseconds" | "wall_clock" => {
                    budget.wall_clock_budget_ms = Some(value.round() as u64)
                }
                "seconds" => budget.wall_clock_budget_ms = Some((value * 1000.0).round() as u64),
                "minutes" => budget.wall_clock_budget_ms = Some((value * 60_000.0).round() as u64),
                "hours" => budget.wall_clock_budget_ms = Some((value * 3_600_000.0).round() as u64),
                other => {
                    return ToolOutput::error(format!("Unknown budget unit: {other}"));
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
        let report = goal.budget_report();
        ToolOutput::success(
            serde_json::to_string_pretty(&serde_json::json!({
                "goal": goal,
                "budget": report,
            }))
            .unwrap_or_default(),
        )
    }
}

#[async_trait]
impl Tool for GoalTool {
    fn name(&self) -> &str {
        "Goal"
    }
    fn description(&self) -> &str {
        "Create, inspect, update, or budget the current multi-turn goal. \
Actions: create (objective [+ completionCriterion] [+ replace]), get, \
update (status: active|complete|blocked|paused), budget (unit + value). \
Subsumes the former CreateGoal / GetGoal / UpdateGoal / SetGoalBudget tools."
    }
    fn disclosure(&self) -> crate::ToolDisclosure {
        crate::ToolDisclosure::Deferred
    }
    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["create", "get", "update", "budget"],
                    "description": "Which goal operation to perform"
                },
                "objective": {
                    "type": "string",
                    "description": "create: the objective to pursue. Must have a verifiable end state."
                },
                "description": {
                    "type": "string",
                    "description": "create: deprecated alias for objective"
                },
                "completionCriterion": {
                    "type": "string",
                    "description": "create: how to verify the goal is complete"
                },
                "completion_criterion": {
                    "type": "string",
                    "description": "create: alias for completionCriterion"
                },
                "replace": {
                    "type": "boolean",
                    "description": "create: replace an existing active/paused/blocked goal instead of failing"
                },
                "status": {
                    "type": "string",
                    "enum": ["active", "complete", "blocked", "paused"],
                    "description": "update: lifecycle status. Prefer active/complete/blocked."
                },
                "unit": {
                    "type": "string",
                    "enum": ["turns", "tokens", "milliseconds", "seconds", "minutes", "hours", "turn", "token", "wall_clock"],
                    "description": "budget: budget unit (kimi-aligned)"
                },
                "budget_unit": {
                    "type": "string",
                    "description": "budget: alias for unit"
                },
                "value": {
                    "type": "number",
                    "description": "budget: positive numeric budget value"
                },
                "turn_budget": {"type": "integer", "description": "budget: legacy field"},
                "token_budget": {"type": "integer", "description": "budget: legacy field"},
                "wall_clock_budget_ms": {"type": "integer", "description": "budget: legacy field"}
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let action = input
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("get");
        Ok(match action {
            "create" => self.create(&input).await,
            "get" => self.get().await,
            "update" => self.update(&input).await,
            "budget" => self.budget(&input).await,
            other => ToolOutput::error(format!(
                "Unknown action: {other}. Use create, get, update, or budget."
            )),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> ToolContext {
        ToolContext {
            working_dir: std::env::temp_dir(),
            session_id: "goal-session".into(),
            turn_id: "t".into(),
            plan_file_path: None,
            image: kkagent_config::ImageConfig::default(),
            tool_call_id: None,
            interrupted: None,
            tools_config: kkagent_config::ToolsConfig::default(),
        }
    }

    #[tokio::test]
    async fn goal_tool_covers_the_former_quartet() {
        let mgr = Arc::new(GoalManager::new());
        let tool = GoalTool::new(mgr);

        // get with no goal
        let out = tool
            .execute(serde_json::json!({"action": "get"}), &context())
            .await
            .unwrap();
        assert!(out.content.contains("No active goal."));

        // create
        let out = tool
            .execute(
                serde_json::json!({
                    "action": "create",
                    "objective": "ship the release",
                    "completionCriterion": "all tests green"
                }),
                &context(),
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("ship the release"));

        // budget
        let out = tool
            .execute(
                serde_json::json!({"action": "budget", "unit": "turns", "value": 10}),
                &context(),
            )
            .await
            .unwrap();
        assert!(!out.is_error);

        // duplicate create without replace fails
        let out = tool
            .execute(
                serde_json::json!({"action": "create", "objective": "another"}),
                &context(),
            )
            .await
            .unwrap();
        assert!(out.is_error);

        // update to complete
        let out = tool
            .execute(
                serde_json::json!({"action": "update", "status": "complete"}),
                &context(),
            )
            .await
            .unwrap();
        assert!(!out.is_error);

        // unknown action
        let out = tool
            .execute(serde_json::json!({"action": "nonsense"}), &context())
            .await
            .unwrap();
        assert!(out.is_error);
    }
}
