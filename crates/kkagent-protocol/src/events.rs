use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    MessageDelta {
        session_id: String,
        text: String,
    },
    ThinkingDelta {
        session_id: String,
        text: String,
    },
    ToolCall {
        session_id: String,
        tool_call_id: String,
        tool_name: String,
        input: serde_json::Value,
    },
    ToolResult {
        session_id: String,
        tool_call_id: String,
        tool_name: String,
        output: String,
        #[serde(default)]
        is_error: bool,
    },
    TurnStart {
        session_id: String,
    },
    TurnEnd {
        session_id: String,
    },
    StatusUpdate {
        session_id: String,
        status: SessionStatus,
    },
    UsageUpdate {
        session_id: String,
        usage: super::TokenUsage,
    },
    ApprovalRequested {
        session_id: String,
        request: super::ApprovalRequest,
    },
    QuestionAsked {
        session_id: String,
        question: QuestionPayload,
    },
    Error {
        session_id: String,
        message: String,
    },
    PlanModeChanged {
        session_id: String,
        enabled: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Idle,
    Thinking,
    ToolExecuting,
    WaitingApproval,
    WaitingQuestion,
    Compacting,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuestionPayload {
    pub question_id: String,
    pub text: String,
    pub options: Vec<QuestionOption>,
    #[serde(default)]
    pub allow_free_text: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuestionOption {
    pub id: String,
    pub label: String,
}
