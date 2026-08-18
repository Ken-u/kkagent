use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
    System,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
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
    Image {
        source: ImageSource,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageSource {
    #[serde(rename = "type")]
    pub source_type: String,
    pub media_type: String,
    pub data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
}

impl TokenUsage {
    /// Compute cache hit ratio adaptively across provider semantics:
    /// - `cache_creation > 0` (Anthropic style): `input_tokens` excludes cache,
    ///   so the total input is `input + cache_creation + cache_read`.
    /// - `cache_creation == 0` (OpenAI style): `input_tokens` already includes
    ///   cached tokens, so the total input is `input` alone.
    ///
    /// Returns `None` when there is no cache read or no measurable input.
    pub fn cache_hit_ratio(&self) -> Option<f32> {
        cache_hit_ratio(
            self.input_tokens,
            self.cache_creation_input_tokens,
            self.cache_read_input_tokens,
        )
    }
}

/// Free-function form of [`TokenUsage::cache_hit_ratio`] for aggregate totals
/// (session sums are not a `TokenUsage` instance).
pub fn cache_hit_ratio(input: u64, cache_creation: u64, cache_read: u64) -> Option<f32> {
    if cache_read == 0 {
        return None;
    }
    let total = if cache_creation > 0 {
        input
            .saturating_add(cache_creation)
            .saturating_add(cache_read)
    } else {
        input
    };
    if total == 0 {
        return None;
    }
    Some(cache_read as f32 / total as f32)
}

/// Estimated per-part token breakdown of the current request context.
/// Attached to [`crate::events::AgentEvent::UsageUpdate`] for `/usage` display.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContextBreakdownInfo {
    pub system: u64,
    pub conversation: u64,
    pub tools: u64,
    pub media: u64,
    pub reserved_output: u64,
    /// True when the numbers are heuristic estimates (chars/4), not measured.
    pub estimated: bool,
}

impl ContextBreakdownInfo {
    pub fn total_used(&self) -> u64 {
        self.system
            .saturating_add(self.conversation)
            .saturating_add(self.tools)
            .saturating_add(self.media)
    }
}
