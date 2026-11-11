//! Official ACP (Agent Client Protocol) v1 wire types.
//!
//! Shapes follow agentclientprotocol.com: content blocks, session updates,
//! stop reasons, agent capabilities, modes, and the agent→client
//! `session/request_permission` request payload. `session/request_input` is a
//! kkagent extension.
//!
//! Earlier kkagent releases used some non-standard field names. They are still
//! emitted next to the official ones (`toolKind` / `toolName` on `tool_call`,
//! `optionKind` on permission options, a top-level `kind` on permission
//! requests) and still accepted on input (`outcomeKind`), so existing clients
//! keep working while official clients read the standard fields.

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
        #[serde(
            rename = "mimeType",
            alias = "mime_type",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        mime_type: Option<String>,
    },
    Audio {
        data: String,
        #[serde(
            rename = "mimeType",
            alias = "mime_type",
            default,
            skip_serializing_if = "Option::is_none"
        )]
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
    #[serde(rename = "refusal")]
    Refusal,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

/// What a tool call produced, as shown to the user.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolCallContent {
    Content {
        content: ContentBlock,
    },
    Diff {
        path: String,
        #[serde(rename = "oldText", default, skip_serializing_if = "Option::is_none")]
        old_text: Option<String>,
        #[serde(rename = "newText")]
        new_text: String,
    },
    Terminal {
        #[serde(rename = "terminalId")]
        terminal_id: String,
    },
}

impl ToolCallContent {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Content {
            content: ContentBlock::text(text),
        }
    }
}

/// A file a tool call touches.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolLocation {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
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
        title: String,
        kind: ToolKind,
        status: ToolCallStatus,
        #[serde(rename = "rawInput", default, skip_serializing_if = "Value::is_null")]
        raw_input: Value,
        #[serde(rename = "rawOutput", default, skip_serializing_if = "Option::is_none")]
        raw_output: Option<Value>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        locations: Vec<ToolLocation>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        content: Vec<ToolCallContent>,
        /// Deprecated alias of `kind`, kept for older clients.
        #[serde(rename = "toolKind")]
        tool_kind: ToolKind,
        /// Deprecated; `title` carries the tool name.
        #[serde(rename = "toolName")]
        tool_name: String,
    },
    ToolCallUpdate {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<ToolCallStatus>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<ToolKind>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        content: Vec<ToolCallContent>,
        #[serde(rename = "rawInput", default, skip_serializing_if = "Option::is_none")]
        raw_input: Option<Value>,
        #[serde(rename = "rawOutput", default, skip_serializing_if = "Option::is_none")]
        raw_output: Option<Value>,
    },
    Plan {
        entries: Vec<PlanEntry>,
    },
    AvailableCommandsUpdate {
        #[serde(rename = "availableCommands")]
        available_commands: Vec<AvailableCommand>,
    },
    CurrentModeUpdate {
        #[serde(rename = "currentModeId")]
        current_mode_id: String,
    },
    SessionInfoUpdate {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(rename = "updatedAt", default, skip_serializing_if = "Option::is_none")]
        updated_at: Option<String>,
    },
    UsageUpdate {
        /// Tokens currently in the context window.
        used: u64,
        /// Size of the context window.
        size: u64,
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
    #[serde(default)]
    pub description: String,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Non-standard; kept for older clients.
    pub kind: ModeKind,
    /// Non-standard; kept for older clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModeKind {
    Primary,
    Submode,
}

/// The standard `modes` object of `session/new` / `session/load` results.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionModeState {
    #[serde(rename = "currentModeId")]
    pub current_mode_id: String,
    #[serde(rename = "availableModes")]
    pub available_modes: Vec<Mode>,
}

/// Modes kkagent offers.
pub fn default_modes() -> Vec<Mode> {
    vec![
        Mode {
            id: "agent".into(),
            name: "Agent".into(),
            description: Some("Work on the task, asking before risky actions.".into()),
            kind: ModeKind::Primary,
            enabled: Some(true),
        },
        Mode {
            id: "plan".into(),
            name: "Plan".into(),
            description: Some("Research and write a plan without changing files.".into()),
            kind: ModeKind::Submode,
            enabled: Some(true),
        },
        Mode {
            id: "yolo".into(),
            name: "YOLO".into(),
            description: Some("Approve tool calls automatically.".into()),
            kind: ModeKind::Submode,
            enabled: Some(true),
        },
    ]
}

/// `modes` for a session whose current mode is `current`.
pub fn session_mode_state(current: &str) -> SessionModeState {
    SessionModeState {
        current_mode_id: current.to_string(),
        available_modes: default_modes(),
    }
}

/// `agentCapabilities` of `initialize`: what this agent really implements.
pub fn agent_capabilities(load_session: bool) -> Value {
    serde_json::json!({
        "loadSession": load_session,
        // Prompts are reduced to their text; richer blocks are not consumed.
        "promptCapabilities": {
            "image": false,
            "audio": false,
            "embeddedContext": false,
            // Non-standard; kept for older clients.
            "tools": true,
        },
        // `mcpServers` of session requests: stdio always, remote when listed.
        "mcpCapabilities": {"http": true, "sse": true},
        "sessionCapabilities": {
            "list": {},
            "resume": {},
            "close": {},
        },
        "_meta": {
            "kkagent": {
                "sessionDelete": true,
            },
        },
    })
}

// ---------------------------------------------------------------------------
// Agent → client requests
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOptionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
}

/// One choice offered in `session/request_permission`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PermissionOption {
    #[serde(rename = "optionId")]
    pub option_id: String,
    pub name: String,
    pub kind: PermissionOptionKind,
    /// Non-standard alias of `kind`, kept for older clients.
    #[serde(rename = "optionKind")]
    pub option_kind: PermissionOptionKind,
}

impl PermissionOption {
    fn new(option_id: &str, name: &str, kind: PermissionOptionKind) -> Self {
        Self {
            option_id: option_id.into(),
            name: name.into(),
            kind,
            option_kind: kind,
        }
    }

    /// The four standard choices; ids double as the scope the host applies.
    pub fn standard_set() -> Vec<Self> {
        vec![
            Self::new("allow_once", "Allow once", PermissionOptionKind::AllowOnce),
            Self::new(
                "allow_always",
                "Always allow",
                PermissionOptionKind::AllowAlways,
            ),
            Self::new("reject_once", "Reject", PermissionOptionKind::RejectOnce),
            Self::new(
                "reject_always",
                "Always reject",
                PermissionOptionKind::RejectAlways,
            ),
        ]
    }
}

/// Non-standard description of what a permission request is about. Official
/// clients read `toolCall`; this stays for older ones.
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

/// The tool call a permission request is about.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PermissionToolCall {
    #[serde(rename = "toolCallId")]
    pub tool_call_id: String,
    pub title: String,
    pub kind: ToolKind,
    pub status: ToolCallStatus,
    #[serde(rename = "rawInput", default, skip_serializing_if = "Value::is_null")]
    pub raw_input: Value,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub locations: Vec<ToolLocation>,
}

/// `session/request_permission` request payload (agent → client).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RequestPermissionRequest {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(rename = "toolCall")]
    pub tool_call: PermissionToolCall,
    pub options: Vec<PermissionOption>,
    /// Non-standard; see [`PermissionRequestKind`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<PermissionRequestKind>,
}

/// Client answer to `session/request_permission`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionOutcome {
    Selected { option_id: String },
    Cancelled,
}

impl PermissionOutcome {
    /// Read the client's response. The standard shape is
    /// `{"outcome": {"outcome": "selected", "optionId": ".."}}` /
    /// `{"outcome": {"outcome": "cancelled"}}`; the legacy flat
    /// `{"outcomeKind": ".."}` is accepted too. Anything unreadable counts as
    /// cancelled, which never grants anything.
    pub fn from_response(response: &Value) -> Self {
        let outcome = response.get("outcome");
        let (kind, source) = match outcome {
            Some(Value::Object(object)) => (
                object.get("outcome").and_then(Value::as_str),
                outcome.unwrap(),
            ),
            Some(Value::String(kind)) => (Some(kind.as_str()), response),
            _ => (
                response.get("outcomeKind").and_then(Value::as_str),
                response,
            ),
        };
        match kind {
            Some("selected") => match source
                .get("optionId")
                .or_else(|| source.get("option_id"))
                .and_then(Value::as_str)
            {
                Some(option_id) if !option_id.is_empty() => Self::Selected {
                    option_id: option_id.to_string(),
                },
                _ => Self::Cancelled,
            },
            _ => Self::Cancelled,
        }
    }

    /// Whether the chosen option grants the call.
    pub fn grants(&self) -> bool {
        matches!(self, Self::Selected { option_id } if option_id.starts_with("allow"))
    }
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
    use serde_json::json;

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
    fn tool_call_carries_official_and_legacy_fields() {
        let update = SessionUpdate::ToolCall {
            tool_call_id: "tc_1".into(),
            title: "Edit".into(),
            kind: ToolKind::Edit,
            status: ToolCallStatus::Pending,
            raw_input: serde_json::json!({"path": "a.rs"}),
            raw_output: None,
            locations: vec![],
            content: vec![],
            tool_kind: ToolKind::Edit,
            tool_name: "Edit".into(),
        };
        let json = serde_json::to_value(&update).unwrap();
        assert_eq!(json["sessionUpdate"], "tool_call");
        assert_eq!(json["toolCallId"], "tc_1");
        assert_eq!(json["title"], "Edit");
        assert_eq!(json["kind"], "edit");
        assert_eq!(json["status"], "pending");
        assert_eq!(json["rawInput"]["path"], "a.rs");
        assert!(json.get("rawOutput").is_none());
        // Deprecated aliases still present for older clients.
        assert_eq!(json["toolKind"], "edit");
        assert_eq!(json["toolName"], "Edit");
    }

    #[test]
    fn tool_call_update_uses_content_wrappers_and_status() {
        let update = SessionUpdate::ToolCallUpdate {
            tool_call_id: "tc_1".into(),
            status: Some(ToolCallStatus::Failed),
            title: None,
            kind: None,
            content: vec![ToolCallContent::text("boom")],
            raw_input: None,
            raw_output: Some(serde_json::json!("boom")),
        };
        let json = serde_json::to_value(&update).unwrap();
        assert_eq!(json["sessionUpdate"], "tool_call_update");
        assert_eq!(json["status"], "failed");
        assert_eq!(json["content"][0]["type"], "content");
        assert_eq!(json["content"][0]["content"]["text"], "boom");
        assert_eq!(json["rawOutput"], "boom");
        assert!(json.get("title").is_none() && json.get("kind").is_none());
    }

    #[test]
    fn media_blocks_use_the_official_mime_type_key() {
        let block: ContentBlock = serde_json::from_value(
            json!({"type": "image", "data": "AAA", "mimeType": "image/png"}),
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(&block).unwrap(),
            json!({"type": "image", "data": "AAA", "mimeType": "image/png"})
        );
        // The old snake_case spelling is still read.
        let legacy: ContentBlock =
            serde_json::from_value(json!({"type": "audio", "data": "A", "mime_type": "audio/wav"}))
                .unwrap();
        assert_eq!(
            serde_json::to_value(&legacy).unwrap()["mimeType"],
            "audio/wav"
        );
    }

    #[test]
    fn new_session_update_variants_use_official_names() {
        let commands = serde_json::to_value(SessionUpdate::AvailableCommandsUpdate {
            available_commands: vec![AvailableCommand {
                name: "help".into(),
                description: "Show help".into(),
            }],
        })
        .unwrap();
        assert_eq!(commands["sessionUpdate"], "available_commands_update");
        assert_eq!(commands["availableCommands"][0]["name"], "help");

        let mode = serde_json::to_value(SessionUpdate::CurrentModeUpdate {
            current_mode_id: "plan".into(),
        })
        .unwrap();
        assert_eq!(mode["sessionUpdate"], "current_mode_update");
        assert_eq!(mode["currentModeId"], "plan");

        let info = serde_json::to_value(SessionUpdate::SessionInfoUpdate {
            title: Some("T".into()),
            updated_at: None,
        })
        .unwrap();
        assert_eq!(info["sessionUpdate"], "session_info_update");
        assert_eq!(info["title"], "T");
        assert!(info.get("updatedAt").is_none());

        let usage = serde_json::to_value(SessionUpdate::UsageUpdate {
            used: 10,
            size: 100,
        })
        .unwrap();
        assert_eq!(usage["sessionUpdate"], "usage_update");
        assert_eq!(
            (usage["used"].as_u64(), usage["size"].as_u64()),
            (Some(10), Some(100))
        );
    }

    #[test]
    fn modes_follow_the_standard_state_shape() {
        let json = serde_json::to_value(session_mode_state("plan")).unwrap();
        assert_eq!(json["currentModeId"], "plan");
        assert_eq!(json["availableModes"][0]["id"], "agent");
        assert!(json["availableModes"][0]["description"].is_string());
    }

    #[test]
    fn capabilities_declare_only_what_is_implemented() {
        let caps = agent_capabilities(true);
        assert_eq!(caps["loadSession"], true);
        assert_eq!(caps["promptCapabilities"]["image"], false);
        assert_eq!(caps["sessionCapabilities"]["resume"], serde_json::json!({}));
        assert!(caps["sessionCapabilities"].get("fork").is_none());
        assert_eq!(caps["mcpCapabilities"]["http"], true);
    }

    #[test]
    fn permission_request_uses_tool_call_and_option_ids() {
        let req = RequestPermissionRequest {
            session_id: "s1".into(),
            tool_call: PermissionToolCall {
                tool_call_id: "tc1".into(),
                title: "Bash".into(),
                kind: ToolKind::Execute,
                status: ToolCallStatus::Pending,
                raw_input: serde_json::json!({"command": "cargo test"}),
                locations: vec![],
            },
            options: PermissionOption::standard_set(),
            kind: Some(PermissionRequestKind::Command {
                command: "cargo test".into(),
            }),
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["toolCall"]["toolCallId"], "tc1");
        assert_eq!(json["toolCall"]["kind"], "execute");
        assert_eq!(json["options"][0]["optionId"], "allow_once");
        assert_eq!(json["options"][0]["name"], "Allow once");
        assert_eq!(json["options"][0]["kind"], "allow_once");
        assert_eq!(json["options"][3]["kind"], "reject_always");
        // Legacy fields ride along.
        assert_eq!(json["options"][0]["optionKind"], "allow_once");
        assert_eq!(json["kind"]["kind"], "command");
        let back: RequestPermissionRequest = serde_json::from_value(json).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn permission_outcome_reads_standard_and_legacy_shapes() {
        let selected = |id: &str| PermissionOutcome::Selected {
            option_id: id.into(),
        };
        assert_eq!(
            PermissionOutcome::from_response(
                &json!({"outcome": {"outcome": "selected", "optionId": "allow_once"}})
            ),
            selected("allow_once")
        );
        assert_eq!(
            PermissionOutcome::from_response(&json!({"outcome": {"outcome": "cancelled"}})),
            PermissionOutcome::Cancelled
        );
        assert_eq!(
            PermissionOutcome::from_response(
                &json!({"outcomeKind": "selected", "optionId": "reject_once"})
            ),
            selected("reject_once")
        );
        // Unreadable or incomplete answers never grant anything.
        for bad in [
            json!({}),
            json!(null),
            json!({"outcome": {"outcome": "selected"}}),
            json!({"outcome": {"outcome": "selected", "optionId": ""}}),
            json!({"outcome": "selected"}),
        ] {
            let outcome = PermissionOutcome::from_response(&bad);
            assert_eq!(outcome, PermissionOutcome::Cancelled, "{bad}");
            assert!(!outcome.grants());
        }
        assert!(selected("allow_always").grants());
        assert!(!selected("reject_always").grants());
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
        assert_eq!(
            serde_json::to_value(StopReason::Refusal).unwrap(),
            "refusal"
        );
        assert_eq!(
            serde_json::to_value(StopReason::MaxTurnRequests).unwrap(),
            "max_turn_requests"
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
