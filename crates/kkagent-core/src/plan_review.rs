//! ExitPlanMode plan-review outcomes (aligned with kimi-code `exitPlanModeReview`).

use kkagent_protocol::{ApprovalDecision, ApprovalResponse};
use kkagent_tools::ToolOutput;
use serde_json::json;

/// Labels used by the TUI plan-review panel (and matched case-insensitively).
pub const LABEL_EXECUTE: &str = "执行";
pub const LABEL_REVISE: &str = "修改意见";
pub const LABEL_REJECT: &str = "拒绝";

#[derive(Debug, Clone)]
pub struct PlanReviewOption {
    pub label: String,
    pub description: String,
}

#[derive(Debug, Clone)]
pub struct PlanReviewDisplay {
    pub plan: String,
    pub path: String,
    pub options: Vec<PlanReviewOption>,
}

impl PlanReviewDisplay {
    pub fn from_tool_input(input: &serde_json::Value, plan: String, path: String) -> Self {
        let options = input
            .get("options")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|o| {
                        let label = o.get("label")?.as_str()?.trim();
                        if label.is_empty() || is_reserved_label(label) {
                            return None;
                        }
                        Some(PlanReviewOption {
                            label: label.to_string(),
                            description: o
                                .get("description")
                                .and_then(|d| d.as_str())
                                .unwrap_or("")
                                .to_string(),
                        })
                    })
                    .take(3)
                    .collect()
            })
            .unwrap_or_default();
        Self {
            plan,
            path,
            options,
        }
    }

    pub fn to_display_json(&self) -> serde_json::Value {
        let mut obj = json!({
            "kind": "plan_review",
            "plan": self.plan,
            "path": self.path,
        });
        if self.options.len() >= 2 {
            obj["options"] = json!(self
                .options
                .iter()
                .map(|o| json!({"label": o.label, "description": o.description}))
                .collect::<Vec<_>>());
        }
        obj
    }
}

pub fn is_reserved_label(label: &str) -> bool {
    matches!(
        normalize_label(label).as_str(),
        "approve"
            | "reject"
            | "reject and exit"
            | "revise"
            | "执行"
            | "拒绝"
            | "修改意见"
    )
}

fn normalize_label(label: &str) -> String {
    label.trim().to_lowercase()
}

fn label_is(selected: Option<&str>, candidates: &[&str]) -> bool {
    let Some(s) = selected.map(normalize_label) else {
        return false;
    };
    candidates.iter().any(|c| normalize_label(c) == s)
}

/// Resolve an ExitPlanMode approval into a tool result (+ whether plan mode exits).
pub fn resolve_exit_plan_approval(
    response: &ApprovalResponse,
    display: &PlanReviewDisplay,
) -> (ToolOutput, bool /* exit_plan_mode */) {
    match response.decision {
        ApprovalDecision::Approved => {
            let selected = response.selected_label.as_deref();
            let approach = display.options.iter().find(|o| {
                selected.is_some_and(|s| normalize_label(&o.label) == normalize_label(s))
            });
            let option_prefix = match approach {
                Some(opt) => format!(
                    "Selected approach: {}\nExecute ONLY the selected approach. Do not execute any unselected alternatives.\n\n",
                    opt.label
                ),
                None => String::new(),
            };
            let saved_to = if display.path.is_empty() {
                String::new()
            } else {
                format!("Plan saved to: {}\n\n", display.path)
            };
            let content = format!(
                "Exited plan mode. {option_prefix}Plan mode deactivated. All tools are now available.\n{saved_to}## Approved Plan:\n{}",
                display.plan
            );
            (ToolOutput::success(content), true)
        }
        ApprovalDecision::Cancelled => (
            ToolOutput::success("Plan approval dismissed. Plan mode remains active."),
            false,
        ),
        ApprovalDecision::Rejected => {
            let label = response.selected_label.as_deref();
            let feedback = response.feedback.as_deref().unwrap_or("").trim();

            if label_is(label, &["Reject and Exit", "拒绝并退出"]) {
                return (
                    ToolOutput::error_stop("Plan rejected by user. Plan mode deactivated."),
                    true,
                );
            }

            if label_is(label, &[LABEL_REVISE, "Revise"]) || !feedback.is_empty() {
                let content = if feedback.is_empty() {
                    "User requested revisions. Plan mode remains active.".to_string()
                } else {
                    format!("User rejected the plan. Feedback:\n\n{feedback}")
                };
                return (ToolOutput::success(content), false);
            }

            // Plain reject — stay in plan mode, stop the turn (kimi `Reject`).
            (
                ToolOutput::error_stop("Plan rejected by user. Plan mode remains active."),
                false,
            )
        }
    }
}

pub fn format_auto_approved_plan(plan: &str, path: &str) -> String {
    let saved_to = if path.is_empty() {
        String::new()
    } else {
        format!("Plan saved to: {path}\n\n")
    };
    format!(
        "Exited plan mode. Plan mode deactivated. All tools are now available.\n\
         Note: this plan was auto-approved without user review — the user has NOT explicitly approved it. \
         Follow the user's original instructions on whether to proceed with execution; if they asked you to stop, wait, or only summarize after planning, do not start executing.\n\
         {saved_to}## Plan (auto-approved, not user-reviewed):\n{plan}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use kkagent_protocol::ApprovalDecision;

    fn display() -> PlanReviewDisplay {
        PlanReviewDisplay {
            plan: "# Plan\n\ndo stuff".into(),
            path: "/tmp/p.md".into(),
            options: vec![],
        }
    }

    #[test]
    fn approve_exits_plan_mode() {
        let resp = ApprovalResponse {
            approval_id: "a".into(),
            decision: ApprovalDecision::Approved,
            scope: None,
            feedback: None,
            selected_label: Some(LABEL_EXECUTE.into()),
        };
        let (out, exit) = resolve_exit_plan_approval(&resp, &display());
        assert!(exit);
        assert!(!out.is_error);
        assert!(out.content.contains("## Approved Plan:"));
    }

    #[test]
    fn revise_keeps_plan_mode_with_feedback() {
        let resp = ApprovalResponse {
            approval_id: "a".into(),
            decision: ApprovalDecision::Rejected,
            scope: None,
            feedback: Some("改成用 REST".into()),
            selected_label: Some(LABEL_REVISE.into()),
        };
        let (out, exit) = resolve_exit_plan_approval(&resp, &display());
        assert!(!exit);
        assert!(!out.is_error);
        assert!(out.content.contains("改成用 REST"));
    }

    #[test]
    fn reject_stops_turn_keeps_plan_mode() {
        let resp = ApprovalResponse {
            approval_id: "a".into(),
            decision: ApprovalDecision::Rejected,
            scope: None,
            feedback: None,
            selected_label: Some(LABEL_REJECT.into()),
        };
        let (out, exit) = resolve_exit_plan_approval(&resp, &display());
        assert!(!exit);
        assert!(out.is_error);
        assert!(out.stop_turn);
    }
}
