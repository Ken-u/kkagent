//! Official ACP (Agent Client Protocol) v1 wire types.
//!
//! Shapes follow agentclientprotocol.com: content blocks, session updates,
//! stop reasons, agent capabilities, modes, and the agent→client
//! `session/request_permission` / `session/request_input` request payloads.

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Content blocks
// ---------------------------------------------------------------------------

/// A block of message content exchanged in prompts and session updates.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Image {
        data: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
    },
    Audio {
        data: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
    },
    ResourceLink {
        uri: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
}

impl ContentBlock {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    /// Concatenated plain text of all text blocks.
    pub fn join_text(blocks: &[ContentBlock]) -> String {
        blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                other => {
                    let _ = other;
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("")
    }
}

// ---------------------------------------------------------------------------
// Stop reasons
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum StopReason {
    #[serde(rename = "refused")]
    Refused,
    #[serde(rename = "max_tokens")]
    MaxTokens,
    #[serde(rename = "max_turn_requests")]
    MaxTurnRequests,
    #[serde(rename = "cancelled")]
    Cancelled,
    #[default]
    #[serde(rename = "end_turn")]
    EndTurn,
}

// ---------------------------------------------------------------------------
// Session updates (notifications `session/update`)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    Read,
    Edit,
    Delete,
    Move,
    Search,
    Execute,
    Think,
    Fetch,
    Other,
}

impl ToolKind {
    /// Best-effort classification of an internal tool name onto ACP kinds.
    pub fn from_tool_name(name: &str) -> Self {
        let lowered = name.to_ascii_lowercase();
        if lowered.contains("edit") || lowered.contains("write") || lowered.contains("apply") {
            Self::Edit
        } else if lowered.contains("read") || lowered.contains("cat") {
            Self::Read
        } else if lowered.contains("grep")
            || lowered.contains("glob")
            || lowered.contains("search")
            || lowered.contains("find")
        {
            Self::Search
        } else if lowered.contains("bash")
            || lowered.contains("shell")
            || lowered.contains("terminal")
            || lowered.contains("exec")
        {
            Self::Execute
        } else if lowered.contains("fetch") || lowered.contains("http") || lowered.contains("curl")
        {
            Self::Fetch
        } else if lowered.contains("think") || lowered.contains("plan") || lowered.contains("todo")
        {
            Self::Think
        } else if lowered.contains("delete") || lowered.contains("remove") || lowered.contains("rm")
        {
            Self::Delete
        } else if lowered.contains("move") || lowered.contains("rename") || lowered.contains("mv") {
            Self::Move
        } else {
            Self::Other
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "sessionUpdate", rename_all = "snake_case")]
pub enum SessionUpdate {
    AgentMessageChunk {
        content: ContentBlock,
    },
    UserMessageChunk {
        content: ContentBlock,
    },
    AgentThoughtChunk {
        content: ContentBlock,
    },
    ToolCall {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(rename = "toolKind")]
        tool_kind: ToolKind,
        #[serde(rename = "toolName")]
        tool_name: String,
        #[serde(rename = "rawInput")]
        raw_input: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        locations: Option<Value>,
        content: Vec<ContentBlock>,
    },
    ToolCallUpdate {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        content: Vec<ContentBlock>,
    },
    Plan {
        entries: Vec<PlanEntry>,
    },
    AvailableCommandsUpdate {
        commands: Vec<AvailableCommand>,
    },
    DiffEvent {
        event: DiffEventType,
        #[serde(rename = "textDiff")]
        text_diff: TextDiff,
    },
}

impl SessionUpdate {
    pub fn agent_message_chunk(text: impl Into<String>) -> Self {
        Self::AgentMessageChunk {
            content: ContentBlock::text(text),
        }
    }

    pub fn user_message_chunk(text: impl Into<String>) -> Self {
        Self::UserMessageChunk {
            content: ContentBlock::text(text),
        }
    }

    pub fn agent_thought_chunk(text: impl Into<String>) -> Self {
        Self::AgentThoughtChunk {
            content: ContentBlock::text(text),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffEventType {
    Add,
    Delete,
    Update,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TextDiff {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<Location>,
    pub contents: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Location {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub absolute: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relative: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlanEntry {
    pub id: String,
    pub content: String,
    pub priority: PlanPriority,
    pub status: PlanStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanPriority {
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AvailableCommand {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

// ---------------------------------------------------------------------------
// Capabilities / modes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentCapabilities {
    #[serde(
        rename = "loadSession",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub load_session: Option<bool>,
    #[serde(
        rename = "promptCapabilities",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub prompt_capabilities: Option<PromptCapabilities>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PromptCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Mode {
    pub id: String,
    pub name: String,
    pub kind: ModeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModeKind {
    Primary,
    Submode,
}

/// Modes exposed over `session/new` / `session/load` results.
pub fn default_modes() -> Vec<Mode> {
    vec![
        Mode {
            id: "agent".into(),
            name: "Agent".into(),
            kind: ModeKind::Primary,
            enabled: Some(true),
        },
        Mode {
            id: "plan".into(),
            name: "Plan".into(),
            kind: ModeKind::Submode,
            enabled: Some(true),
        },
        Mode {
            id: "yolo".into(),
            name: "YOLO".into(),
            kind: ModeKind::Submode,
            enabled: Some(true),
        },
    ]
}

// ---------------------------------------------------------------------------
// Agent → client requests
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "optionKind", rename_all = "snake_case")]
pub enum PermissionOption {
    AllowOnce {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    AllowAlways {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    RejectOnce {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    RejectAlways {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
}

impl PermissionOption {
    pub fn option_id(&self) -> &'static str {
        match self {
            Self::AllowOnce { .. } => "allow_once",
            Self::AllowAlways { .. } => "allow_always",
            Self::RejectOnce { .. } => "reject_once",
            Self::RejectAlways { .. } => "reject_always",
        }
    }
}

/// `session/request_permission` request payload (agent → client).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PermissionRequestKind {
    Edit {
        file: Location,
        #[serde(rename = "oldString", default)]
        old_string: String,
        #[serde(rename = "newString", default)]
        new_string: String,
    },
    Command {
        command: String,
    },
    Fetch {
        url: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RequestPermissionRequest {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    pub options: Vec<PermissionOption>,
    pub kind: PermissionRequestKind,
}

/// Client response for `session/request_permission`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "outcomeKind", rename_all = "snake_case")]
pub enum PermissionOutcome {
    Selected {
        #[serde(rename = "optionId")]
        option_id: String,
    },
    Cancelled,
}

/// `session/request_input` request payload (agent → client).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RequestInputKind {
    Text {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        password: Option<bool>,
    },
    Select {
        options: Vec<SelectOption>,
        #[serde(
            rename = "multiSelect",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        multi_select: Option<bool>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SelectOption {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RequestInputRequest {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    pub prompt: Vec<ContentBlock>,
    pub kind: RequestInputKind,
}

/// Client response for `session/request_input`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RequestInputResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<ContentBlock>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canceled: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_update_serializes_with_official_tags() {
        let update = SessionUpdate::agent_message_chunk("hello");
        let json = serde_json::to_value(&update).unwrap();
        assert_eq!(
            json["sessionUpdate"], "agent_message_chunk",
            "official tag field must be sessionUpdate"
        );
        assert_eq!(json["content"]["type"], "text");
        assert_eq!(json["content"]["text"], "hello");
    }

    #[test]
    fn tool_call_uses_camel_case_fields() {
        let update = SessionUpdate::ToolCall {
            tool_call_id: "tc_1".into(),
            tool_kind: ToolKind::Edit,
            tool_name: "Edit".into(),
            raw_input: serde_json::json!({"path": "a.rs"}),
            locations: None,
            content: vec![],
        };
        let json = serde_json::to_value(&update).unwrap();
        assert_eq!(json["sessionUpdate"], "tool_call");
        assert_eq!(json["toolCallId"], "tc_1");
        assert_eq!(json["toolKind"], "edit");
        assert_eq!(json["toolName"], "Edit");
        assert_eq!(json["rawInput"]["path"], "a.rs");
    }

    #[test]
    fn permission_request_round_trips() {
        let req = RequestPermissionRequest {
            session_id: "s1".into(),
            options: vec![
                PermissionOption::AllowOnce { title: None },
                PermissionOption::RejectOnce { title: None },
            ],
            kind: PermissionRequestKind::Command {
                command: "cargo test".into(),
            },
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["kind"]["kind"], "command");
        assert_eq!(json["kind"]["command"], "cargo test");
        assert_eq!(json["options"][0]["optionKind"], "allow_once");
        let back: RequestPermissionRequest = serde_json::from_value(json).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn request_input_round_trips() {
        let req = RequestInputRequest {
            session_id: "s1".into(),
            prompt: vec![ContentBlock::text("Proceed?")],
            kind: RequestInputKind::Select {
                options: vec![SelectOption {
                    label: "Yes".into(),
                    description: None,
                }],
                multi_select: Some(false),
            },
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["kind"]["kind"], "select");
        let back: RequestInputRequest = serde_json::from_value(json).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn stop_reason_uses_official_values() {
        assert_eq!(
            serde_json::to_value(StopReason::EndTurn).unwrap(),
            "end_turn"
        );
        assert_eq!(
            serde_json::to_value(StopReason::Cancelled).unwrap(),
            "cancelled"
        );
        assert_eq!(
            serde_json::to_value(StopReason::MaxTokens).unwrap(),
            "max_tokens"
        );
    }

    #[test]
    fn tool_kind_classification() {
        assert_eq!(ToolKind::from_tool_name("Edit"), ToolKind::Edit);
        assert_eq!(ToolKind::from_tool_name("Bash"), ToolKind::Execute);
        assert_eq!(ToolKind::from_tool_name("Grep"), ToolKind::Search);
        assert_eq!(ToolKind::from_tool_name("WebFetch"), ToolKind::Fetch);
    }
}
