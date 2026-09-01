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
