use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: Vec<ChatContent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatContent {
    Text {
        text: String,
    },
    Image {
        media_type: String,
        data: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
    Thinking {
        thinking: String,
    },
}

#[derive(Debug, Clone)]
pub struct LlmRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolDef>,
    pub max_tokens: u32,
    pub system: Option<String>,
    pub thinking: Option<ThinkingParams>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone, Copy)]
pub struct ThinkingParams {
    pub budget_tokens: u32,
}

#[derive(Debug, Clone)]
pub enum StreamEvent {
    TextDelta(String),
    ThinkingDelta(String),
    ToolUseStart { id: String, name: String },
    ToolUseInputDelta { id: String, delta: String },
    ToolUseEnd { id: String },
    MessageEnd { usage: TokenUsage },
    Error(String),
}

#[derive(Debug, Clone, Default)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
}
