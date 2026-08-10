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
