use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    /// Follow-up user guidance accepted by an already-running turn.
    SteerInput {
        session_id: String,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        idempotency_key: Option<String>,
    },
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
    /// Lightweight turn-liveness signal. Consumers should not persist or render it.
    Heartbeat {
        session_id: String,
    },
    /// A model request will be retried. Countdown updates reuse the same
    /// `retry_number` and set `initial` to false.
    LlmRetry {
        session_id: String,
        retry_number: u32,
        reason: String,
        wait_seconds: u64,
        remaining_seconds: u64,
        initial: bool,
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
    /// Fired when the plan file was written/updated so the TUI can show full plan.
    PlanFileUpdated {
        session_id: String,
        path: String,
        content: String,
    },
    /// Live todo list for the sticky TUI panel (latest state).
    TodoUpdated {
        session_id: String,
        items: Vec<TodoItemEvent>,
    },
    /// Subagent lifecycle mirrored onto the parent session/TUI.
    SubagentSpawned {
        session_id: String,
        subagent_id: String,
        subagent_name: String,
        parent_tool_call_id: String,
        description: Option<String>,
        model: Option<String>,
        run_in_background: bool,
    },
    SubagentStarted {
        session_id: String,
        subagent_id: String,
    },
    SubagentCompleted {
        session_id: String,
        subagent_id: String,
        result_summary: String,
    },
    SubagentFailed {
        session_id: String,
        subagent_id: String,
        error: String,
    },
    /// Nested child agent event (message/tool) mirrored under a parent tool call.
    SubagentChildEvent {
        session_id: String,
        subagent_id: String,
        parent_tool_call_id: String,
        event: Box<AgentEvent>,
    },
    /// `/btw` side-question streaming (does not touch the main transcript).
    BtwDelta {
        session_id: String,
        agent_id: String,
        text: String,
    },
    BtwThinkingDelta {
        session_id: String,
        agent_id: String,
        text: String,
    },
    BtwRetry {
        session_id: String,
        agent_id: String,
        retry_number: u32,
        reason: String,
        wait_seconds: u64,
        remaining_seconds: u64,
        initial: bool,
    },
    BtwEnd {
        session_id: String,
        agent_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// MCP OAuth authorization URL for the user to open.
    McpAuthRequired {
        session_id: String,
        server_name: String,
        authorization_url: String,
    },
    /// Manual `/compact` finished (async); TUI should replace transcript display.
    CompactCompleted {
        session_id: String,
        deleted: u64,
        kept_user_message_count: u64,
        messages: Vec<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Skill loaded via `/skill:…` or the Skill tool (kimi `skill.activated`).
    SkillActivated {
        session_id: String,
        activation_id: String,
        skill_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        skill_args: Option<String>,
        /// `user-slash` | `model-tool` | `nested-skill`
        trigger: String,
    },
    /// Session-scoped settings changed by any client (model / permission / plan / cwd).
    SessionConfigChanged {
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        permission_mode: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        plan_mode: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        working_dir: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },
    /// A single long-running tool was cancelled without ending the turn.
    ToolCancelled {
        session_id: String,
        tool_call_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

impl AgentEvent {
    pub fn session_id(&self) -> &str {
        match self {
            Self::SteerInput { session_id, .. }
            | Self::MessageDelta { session_id, .. }
            | Self::ThinkingDelta { session_id, .. }
            | Self::ToolCall { session_id, .. }
            | Self::ToolResult { session_id, .. }
            | Self::TurnStart { session_id, .. }
            | Self::TurnEnd { session_id, .. }
            | Self::StatusUpdate { session_id, .. }
            | Self::Heartbeat { session_id, .. }
            | Self::LlmRetry { session_id, .. }
            | Self::UsageUpdate { session_id, .. }
            | Self::ApprovalRequested { session_id, .. }
            | Self::QuestionAsked { session_id, .. }
            | Self::Error { session_id, .. }
            | Self::PlanModeChanged { session_id, .. }
            | Self::PlanFileUpdated { session_id, .. }
            | Self::TodoUpdated { session_id, .. }
            | Self::SubagentSpawned { session_id, .. }
            | Self::SubagentStarted { session_id, .. }
            | Self::SubagentCompleted { session_id, .. }
            | Self::SubagentFailed { session_id, .. }
            | Self::SubagentChildEvent { session_id, .. }
            | Self::BtwDelta { session_id, .. }
            | Self::BtwThinkingDelta { session_id, .. }
            | Self::BtwRetry { session_id, .. }
            | Self::BtwEnd { session_id, .. }
            | Self::McpAuthRequired { session_id, .. }
            | Self::CompactCompleted { session_id, .. }
            | Self::SkillActivated { session_id, .. }
            | Self::SessionConfigChanged { session_id, .. }
            | Self::ToolCancelled { session_id, .. } => session_id,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoItemEvent {
    pub id: String,
    pub content: String,
    pub status: String,
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
    /// Interrupt requested; waiting for the turn to settle.
    Cancelling,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuestionPayload {
    pub question_id: String,
    pub text: String,
    pub options: Vec<QuestionOption>,
    #[serde(default)]
    pub allow_free_text: bool,
    /// When true, options are multi-select checkboxes.
    #[serde(default)]
    pub allow_multiple: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuestionOption {
    pub id: String,
    pub label: String,
}
