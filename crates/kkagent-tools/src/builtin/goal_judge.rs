use async_trait::async_trait;
use serde_json::Value;
use std::sync::{Arc, Mutex};

use crate::{Tool, ToolContext, ToolOutput};

/// Verdict recorded by the judge agent via [`GoalJudgeTool`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JudgeVerdict {
    /// "approve" | "reject"
    pub verdict: String,
    /// Concrete missing evidence items (reject only).
    pub gaps: Vec<String>,
    /// One-or-two sentence rationale.
    pub summary: String,
}

/// Tool the completion-judge agent uses to mark its verdict.
///
/// Both actions write the shared slot and stop the judge turn. The judge
/// runner treats a turn that ends without a toolcall as "no verdict"
/// (fail-open upstream).
pub struct GoalJudgeTool {
    verdict_slot: Arc<Mutex<Option<JudgeVerdict>>>,
}

impl GoalJudgeTool {
    pub fn new(verdict_slot: Arc<Mutex<Option<JudgeVerdict>>>) -> Self {
        Self { verdict_slot }
    }
}

#[async_trait]
impl Tool for GoalJudgeTool {
    fn name(&self) -> &str {
        "GoalJudge"
    }

    fn description(&self) -> &str {
        "Record your completion-review verdict for the goal under audit. \
Call exactly once, after inspecting the evidence. \
Use `approve` only when the transcript evidence shows every part of the \
objective (and any stated validation) is done. \
Use `reject` with concrete `gaps` listing what is missing or unverified."
    }

    fn disclosure(&self) -> crate::ToolDisclosure {
        crate::ToolDisclosure::Inline
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["approve", "reject"],
                    "description": "approve = the evidence supports completion; reject = it does not"
                },
                "gaps": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "reject: concrete missing/unverified items (required for reject)"
                },
                "summary": {
                    "type": "string",
                    "description": "One or two sentences citing the decisive evidence"
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let action = input
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if action != "approve" && action != "reject" {
            return Ok(ToolOutput::error("action must be `approve` or `reject`"));
        }
        let mut gaps: Vec<String> = input
            .get("gaps")
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        if action == "reject" && gaps.is_empty() {
            return Ok(ToolOutput::error(
                "reject requires at least one concrete gap (what is missing or unverified)",
            ));
        }
        if action == "approve" {
            gaps.clear();
        }
        let summary = input
            .get("summary")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();

        *self.verdict_slot.lock().unwrap() = Some(JudgeVerdict {
            verdict: action.clone(),
            gaps,
            summary,
        });

        let body = if action == "approve" {
            "Verdict recorded: approve. The goal will be marked complete."
        } else {
            "Verdict recorded: reject. The goal owner will receive the gap list."
        };
        Ok(ToolOutput::success(body).with_stop_turn())
    }
}

/// Criterion update recorded by the judge agent via [`GoalCriterionTool`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CriterionUpdate {
    /// Full replacement text of the acceptance criterion.
    pub criterion: String,
    /// Short change summary for the user.
    pub note: String,
}

/// Tool the completion-judge agent uses to persist an acceptance criterion
/// agreed during a discussion turn. Mirrors [`GoalJudgeTool`]: writes a shared
/// slot and stops the judge turn.
pub struct GoalCriterionTool {
    slot: Arc<Mutex<Option<CriterionUpdate>>>,
}

impl GoalCriterionTool {
    pub fn new(slot: Arc<Mutex<Option<CriterionUpdate>>>) -> Self {
        Self { slot }
    }
}

#[async_trait]
impl Tool for GoalCriterionTool {
    fn name(&self) -> &str {
        "GoalCriterion"
    }

    fn description(&self) -> &str {
        "Record the acceptance criterion you and the user agreed on for the goal under \
discussion. Call exactly once per turn, when the discussion settles. \
`criterion` is the full replacement text (not a diff); `note` is a one-sentence \
change summary shown to the user."
    }

    fn disclosure(&self) -> crate::ToolDisclosure {
        crate::ToolDisclosure::Inline
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "criterion": {
                    "type": "string",
                    "description": "Full replacement text of the acceptance criterion"
                },
                "note": {
                    "type": "string",
                    "description": "One-sentence summary of what changed for the user"
                }
            },
            "required": ["criterion"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let criterion = input
            .get("criterion")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|c| !c.is_empty());
        let Some(criterion) = criterion else {
            return Ok(ToolOutput::error(
                "criterion is required (full replacement text)",
            ));
        };
        let note = input
            .get("note")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .unwrap_or("criterion updated")
            .to_string();

        *self.slot.lock().unwrap() = Some(CriterionUpdate {
            criterion: criterion.to_string(),
            note: note.clone(),
        });

        Ok(ToolOutput::success(format!("Acceptance criterion recorded: {note}")).with_stop_turn())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> ToolContext {
        ToolContext {
            working_dir: std::env::temp_dir(),
            session_id: "judge-session".into(),
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
    async fn records_criterion_updates_and_rejects_empty() {
        let slot: Arc<Mutex<Option<CriterionUpdate>>> = Arc::new(Mutex::new(None));
        let tool = GoalCriterionTool::new(slot.clone());

        let out = tool
            .execute(serde_json::json!({"criterion": "   "}), &context())
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(slot.lock().unwrap().is_none());

        let out = tool
            .execute(
                serde_json::json!({
                    "criterion": "cargo test green AND clippy clean",
                    "note": "added clippy requirement"
                }),
                &context(),
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.stop_turn);
        let update = slot.lock().unwrap().take().unwrap();
        assert_eq!(update.criterion, "cargo test green AND clippy clean");
        assert_eq!(update.note, "added clippy requirement");

        // Missing note falls back to a default.
        let out = tool
            .execute(serde_json::json!({"criterion": "docs build"}), &context())
            .await
            .unwrap();
        assert!(!out.is_error);
        let update = slot.lock().unwrap().take().unwrap();
        assert_eq!(update.criterion, "docs build");
        assert_eq!(update.note, "criterion updated");
    }
}

/// Verdict-review tests (GoalJudge tool), separate from the criterion tests.
#[cfg(test)]
mod verdict_tests {
    use super::*;

    fn context() -> ToolContext {
        ToolContext {
            working_dir: std::env::temp_dir(),
            session_id: "judge-session".into(),
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
    async fn records_approve_and_reject_verdicts() {
        let slot: Arc<Mutex<Option<JudgeVerdict>>> = Arc::new(Mutex::new(None));
        let tool = GoalJudgeTool::new(slot.clone());

        // reject without gaps is refused
        let out = tool
            .execute(serde_json::json!({"action": "reject"}), &context())
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(slot.lock().unwrap().is_none());

        // reject with gaps
        let out = tool
            .execute(
                serde_json::json!({
                    "action": "reject",
                    "gaps": ["test suite never ran", "file X missing"],
                    "summary": "evidence contradicts the claim"
                }),
                &context(),
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.stop_turn);
        let verdict = slot.lock().unwrap().take().unwrap();
        assert_eq!(verdict.verdict, "reject");
        assert_eq!(verdict.gaps.len(), 2);
        assert!(verdict.summary.contains("contradicts"));

        // approve clears gaps even if provided
        let out = tool
            .execute(
                serde_json::json!({"action": "approve", "gaps": ["ignored"], "summary": "all done"}),
                &context(),
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.stop_turn);
        let verdict = slot.lock().unwrap().take().unwrap();
        assert_eq!(verdict.verdict, "approve");
        assert!(verdict.gaps.is_empty());
    }

    #[tokio::test]
    async fn rejects_unknown_action() {
        let slot: Arc<Mutex<Option<JudgeVerdict>>> = Arc::new(Mutex::new(None));
        let tool = GoalJudgeTool::new(slot);
        let out = tool
            .execute(serde_json::json!({"action": "maybe"}), &context())
            .await
            .unwrap();
        assert!(out.is_error);
    }
}
