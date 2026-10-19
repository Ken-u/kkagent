use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub approval_id: String,
    pub session_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_input_display: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Approved,
    Rejected,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalScope {
    /// Approve only this single tool call.
    Once,
    /// Approve matching calls for the remainder of the current turn.
    Turn,
    /// Approve matching calls for the rest of this session.
    Session,
    /// Persist an allow rule for this tool pattern (session + future turns).
    Always,
}

/// Fragment of the `approval.respond` error returned when the response targets
/// an approval the server is no longer waiting on (timed out, answered by
/// another client, interrupted, or the turn moved on).
pub const APPROVAL_NOT_PENDING: &str = "expired or is no longer pending";

/// True when an approval-response error means the approval can never be
/// resolved again, so clients must drop the stale prompt instead of retrying.
///
/// Covers `approval.respond` ([`APPROVAL_NOT_PENDING`]) and the restart-safe
/// `session.resolve_pending_plan_review` resolver.
pub fn is_stale_approval_error(message: &str) -> bool {
    [
        APPROVAL_NOT_PENDING,
        "Approval id no longer matches",
        "No persisted plan review for this session",
    ]
    .iter()
    .any(|marker| message.contains(marker))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalResponse {
    pub approval_id: String,
    pub decision: ApprovalDecision,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<ApprovalScope>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feedback: Option<String>,
    /// Plan-review / multi-choice label (e.g. "执行", approach name, "修改意见").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_label: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_approval_errors_are_recognised() {
        assert!(is_stale_approval_error(&format!(
            "Approval abc {APPROVAL_NOT_PENDING} for session s1"
        )));
        assert!(is_stale_approval_error("Approval id no longer matches"));
        assert!(is_stale_approval_error(
            "No persisted plan review for this session"
        ));
        assert!(!is_stale_approval_error(
            "Failed to deliver approval response"
        ));
        assert!(!is_stale_approval_error("delivery failed"));
    }
}
