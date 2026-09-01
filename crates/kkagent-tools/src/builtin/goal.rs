use async_trait::async_trait;
use kkagent_protocol::goal::{GoalBudget, GoalManager};
use serde_json::Value;
use std::sync::Arc;

use crate::{Tool, ToolContext, ToolOutput};

/// Unified goal tool (`Goal`) — subsumes the former CreateGoal / GetGoal /
/// UpdateGoal / SetGoalBudget quartet behind a single `action` parameter.
///
/// `judge_enabled` switches the `complete` action: off (default) keeps the
/// legacy immediate-complete behavior; on turns the claim into a
/// `pending_verification` stub that the agent loop's judge gate resolves.
pub struct GoalTool {
    goal_mgr: Arc<GoalManager>,
    judge_enabled: bool,
}

impl GoalTool {
    pub fn new(goal_mgr: Arc<GoalManager>) -> Self {
        Self {
            goal_mgr,
            judge_enabled: false,
        }
    }

    pub fn with_judge_enabled(mut self, judge_enabled: bool) -> Self {
        self.judge_enabled = judge_enabled;
        self
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
                if self.judge_enabled {
                    // Claim only: the agent loop's judge gate runs an
                    // independent review and resolves this into either the
                    // legacy complete output (approved) or a reject feedback
                    // (turn continues).
                    return ToolOutput::success(
                        "Completion claim submitted. An independent judge agent will review \
                         the transcript evidence against the goal; do not repeat the claim — \
                         continue with any remaining work while the review runs.",
                    )
                    .with_data(serde_json::json!({"goal_status": "pending_verification"}));
                }
                let finished = self.goal_mgr.complete_goal("completed").await;
                let body = finished
                    .map(|g| serde_json::to_string_pretty(&g).unwrap_or_default())
                    .unwrap_or_else(|| "Goal completed.".into());
                ToolOutput::success(body)
                    .with_data(serde_json::json!({"goal_status": "complete"}))
                    .with_delivery(
                        "Goal marked complete and cleared. Write a short final summary of the outcome for the user now, then stop.",
                    )
                    .with_stop_turn()
            }
            "blocked" => {
                self.goal_mgr.block_goal("blocked").await;
                ToolOutput::success("Goal blocked.")
                    .with_data(serde_json::json!({"goal_status": "blocked"}))
                    .with_delivery(
                        "Goal marked blocked. Explain the blocker in one short message and stop; wait for user direction.",
                    )
                    .with_stop_turn()
            }
            "paused" => {
                self.goal_mgr.pause_goal().await;
                ToolOutput::success("Goal paused.")
                    .with_data(serde_json::json!({"goal_status": "paused"}))
                    .with_delivery(
                        "Goal paused. Confirm the pause to the user in one short message, then stop.",
                    )
                    .with_stop_turn()
            }
            "active" => {
                if self.goal_mgr.resume_goal().await {
                    ToolOutput::success("Goal resumed.")
                } else {
                    ToolOutput::error(
                        "Goal could not be resumed. It may already be active, missing, or still have an exhausted budget; increase/clear the budget or replace the goal first.",
                    )
                }
            }
            // Legacy alias kept for old transcripts.
            "failed" => {
                self.goal_mgr.block_goal("failed").await;
                ToolOutput::success("Goal blocked (legacy status `failed` mapped to blocked).")
                    .with_stop_turn()
            }
            _ => ToolOutput::error(format!(
                "Unknown status: {status}. Use active, paused, complete, or blocked."
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
        let raw_value = input.get("value");
        if unit.is_some() != raw_value.is_some() {
            return ToolOutput::error("unit and value must be provided together");
        }
        if let (Some(unit), Some(raw_value)) = (unit, raw_value) {
            match unit {
                "turns" | "turn" => {
                    let Some(value) = json_positive_u32(raw_value) else {
                        return ToolOutput::error(
                            "turn budget must be a positive number that rounds to at most 4294967295",
                        );
                    };
                    budget.turn_budget = Some(value);
                }
                "tokens" | "token" => {
                    let Some(value) = json_positive_u64(raw_value) else {
                        return ToolOutput::error(
                            "token budget must be a positive number that fits in u64",
                        );
                    };
                    budget.token_budget = Some(value);
                }
                "milliseconds" | "wall_clock" => {
                    let Some(value) = json_positive_u64(raw_value) else {
                        return ToolOutput::error(
                            "wall-clock budget must be a positive number that rounds to at least 1 millisecond and fits in u64",
                        );
                    };
                    budget.wall_clock_budget_ms = Some(value);
                }
                "seconds" => {
                    let Some(value) = json_scaled_positive_millis(raw_value, 1_000) else {
                        return ToolOutput::error(
                            "seconds budget must be positive, round to at least 1 millisecond, and fit in u64",
                        );
                    };
                    budget.wall_clock_budget_ms = Some(value);
                }
                "minutes" => {
                    let Some(value) = json_scaled_positive_millis(raw_value, 60_000) else {
                        return ToolOutput::error(
                            "minutes budget must be positive, round to at least 1 millisecond, and fit in u64",
                        );
                    };
                    budget.wall_clock_budget_ms = Some(value);
                }
                "hours" => {
                    let Some(value) = json_scaled_positive_millis(raw_value, 3_600_000) else {
                        return ToolOutput::error(
                            "hours budget must be positive, round to at least 1 millisecond, and fit in u64",
                        );
                    };
                    budget.wall_clock_budget_ms = Some(value);
                }
                other => {
                    return ToolOutput::error(format!("Unknown budget unit: {other}"));
                }
            }
        } else {
            let has_legacy_field = ["turn_budget", "token_budget", "wall_clock_budget_ms"]
                .iter()
                .any(|key| input.get(key).is_some());
            if !has_legacy_field {
                return ToolOutput::error(
                    "provide unit and value together, or at least one legacy budget field",
                );
            }
            // Legacy multi-field form
            if input
                .as_object()
                .map(|o| o.contains_key("turn_budget"))
                .unwrap_or(false)
            {
                let raw = input.get("turn_budget").expect("checked key presence");
                if raw.is_null() {
                    budget.turn_budget = None;
                } else if let Some(value) = raw
                    .as_u64()
                    .filter(|value| *value > 0)
                    .and_then(|v| u32::try_from(v).ok())
                {
                    budget.turn_budget = Some(value);
                } else {
                    return ToolOutput::error(
                        "turn_budget must be null or an integer between 1 and 4294967295",
                    );
                }
            }
            if input
                .as_object()
                .map(|o| o.contains_key("token_budget"))
                .unwrap_or(false)
            {
                let raw = input.get("token_budget").expect("checked key presence");
                if raw.is_null() {
                    budget.token_budget = None;
                } else if let Some(value) = raw.as_u64().filter(|value| *value > 0) {
                    budget.token_budget = Some(value);
                } else {
                    return ToolOutput::error("token_budget must be null or a positive integer");
                }
            }
            if input
                .as_object()
                .map(|o| o.contains_key("wall_clock_budget_ms"))
                .unwrap_or(false)
            {
                let raw = input
                    .get("wall_clock_budget_ms")
                    .expect("checked key presence");
                if raw.is_null() {
                    budget.wall_clock_budget_ms = None;
                } else if let Some(value) = raw.as_u64().filter(|value| *value > 0) {
                    budget.wall_clock_budget_ms = Some(value);
                } else {
                    return ToolOutput::error(
                        "wall_clock_budget_ms must be null or a positive integer",
                    );
                }
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

fn json_positive_u32(value: &Value) -> Option<u32> {
    if let Some(value) = value.as_u64() {
        return u32::try_from(value).ok().filter(|value| *value > 0);
    }
    rounded_positive_u32(value.as_f64()?)
}

fn json_positive_u64(value: &Value) -> Option<u64> {
    if let Some(value) = value.as_u64() {
        return (value > 0).then_some(value);
    }
    rounded_positive_u64(value.as_f64()?)
}

fn json_scaled_positive_millis(value: &Value, scale: u64) -> Option<u64> {
    if let Some(value) = value.as_u64() {
        return value.checked_mul(scale).filter(|value| *value > 0);
    }
    rounded_positive_u64(value.as_f64()? * scale as f64)
}

fn rounded_positive_u32(value: f64) -> Option<u32> {
    let rounded = value.round();
    (rounded >= 1.0 && rounded <= u32::MAX as f64).then_some(rounded as u32)
}

fn rounded_positive_u64(value: f64) -> Option<u64> {
    let rounded = value.round();
    (rounded >= 1.0 && rounded < u64::MAX as f64).then_some(rounded as u64)
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
            model_alias: None,
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

    #[tokio::test]
    async fn rejects_budget_values_that_round_or_overflow_to_zero() {
        let mgr = Arc::new(GoalManager::new());
        mgr.create_goal("budget validation", GoalBudget::default())
            .await;
        let tool = GoalTool::new(mgr.clone());

        for input in [
            serde_json::json!({"action": "budget", "unit": "turns", "value": 0.1}),
            serde_json::json!({"action": "budget", "unit": "turns", "value": 4294967296_u64}),
            serde_json::json!({"action": "budget", "turn_budget": 4294967296_u64}),
        ] {
            let out = tool.execute(input, &context()).await.unwrap();
            assert!(out.is_error, "unexpected success: {}", out.content);
        }
        assert_eq!(mgr.get_goal().await.unwrap().budget, GoalBudget::default());
    }

    #[tokio::test]
    async fn preserves_exact_integer_budget_boundaries() {
        let mgr = Arc::new(GoalManager::new());
        mgr.create_goal("budget boundaries", GoalBudget::default())
            .await;
        let tool = GoalTool::new(mgr.clone());

        let out = tool
            .execute(
                serde_json::json!({"action": "budget", "unit": "tokens", "value": u64::MAX}),
                &context(),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "unexpected error: {}", out.content);
        assert_eq!(
            mgr.get_goal().await.unwrap().budget.token_budget,
            Some(u64::MAX)
        );

        let out = tool
            .execute(
                serde_json::json!({"action": "budget", "unit": "seconds", "value": u64::MAX}),
                &context(),
            )
            .await
            .unwrap();
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn rejects_partial_or_invalid_budget_updates() {
        let mgr = Arc::new(GoalManager::new());
        mgr.create_goal("budget validation", GoalBudget::default())
            .await;
        let tool = GoalTool::new(mgr.clone());

        for input in [
            serde_json::json!({"action": "budget", "unit": "turns"}),
            serde_json::json!({"action": "budget", "value": 2}),
            serde_json::json!({"action": "budget"}),
            serde_json::json!({"action": "budget", "turn_budget": 0}),
            serde_json::json!({"action": "budget", "token_budget": "many"}),
            serde_json::json!({"action": "budget", "wall_clock_budget_ms": -1}),
        ] {
            let out = tool.execute(input, &context()).await.unwrap();
            assert!(out.is_error, "unexpected success: {}", out.content);
        }
        assert_eq!(mgr.get_goal().await.unwrap().budget, GoalBudget::default());
    }

    #[tokio::test]
    async fn resume_reports_exhausted_budget_instead_of_fake_success() {
        let mgr = Arc::new(GoalManager::new());
        mgr.create_goal(
            "budgeted",
            GoalBudget {
                turn_budget: Some(1),
                ..Default::default()
            },
        )
        .await;
        mgr.record_turn(1).await;
        mgr.block_goal("budget").await;
        let tool = GoalTool::new(mgr.clone());

        let out = tool
            .execute(
                serde_json::json!({"action": "update", "status": "active"}),
                &context(),
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert_eq!(
            mgr.get_goal().await.unwrap().status,
            kkagent_protocol::goal::GoalStatus::Blocked
        );
    }

    #[tokio::test]
    async fn judge_disabled_keeps_legacy_complete() {
        let mgr = Arc::new(GoalManager::new());
        let tool = GoalTool::new(mgr.clone());
        tool.execute(
            serde_json::json!({"action": "create", "objective": "done deal"}),
            &context(),
        )
        .await
        .unwrap();

        let out = tool
            .execute(
                serde_json::json!({"action": "update", "status": "complete"}),
                &context(),
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.stop_turn, "legacy complete stops the turn");
        assert!(out.delivery.is_some());
        assert_eq!(
            out.data
                .as_ref()
                .and_then(|d| d.get("goal_status"))
                .and_then(|v| v.as_str()),
            Some("complete")
        );
        assert!(mgr.get_goal().await.is_none(), "goal cleared immediately");
    }

    #[tokio::test]
    async fn judge_enabled_defers_completion_to_a_claim() {
        let mgr = Arc::new(GoalManager::new());
        let tool = GoalTool::new(mgr.clone()).with_judge_enabled(true);
        tool.execute(
            serde_json::json!({"action": "create", "objective": "needs review"}),
            &context(),
        )
        .await
        .unwrap();

        let out = tool
            .execute(
                serde_json::json!({"action": "update", "status": "complete"}),
                &context(),
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(!out.stop_turn, "claim must not stop the turn");
        assert!(out.delivery.is_none());
        assert_eq!(
            out.data
                .as_ref()
                .and_then(|d| d.get("goal_status"))
                .and_then(|v| v.as_str()),
            Some("pending_verification")
        );
        assert!(
            mgr.get_goal().await.is_some(),
            "goal stays active until the judge approves"
        );
    }
}
