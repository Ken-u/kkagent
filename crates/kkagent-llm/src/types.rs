use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatMessage {
    pub role: String,
    pub content: Vec<ChatContent>,
    /// Message-level tool definitions (progressive disclosure).
    /// Provider serialization merges these into the wire `tools` array.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDef>>,
}

impl ChatMessage {
    pub fn new(role: impl Into<String>, content: Vec<ChatContent>) -> Self {
        Self {
            role: role.into(),
            content,
            tools: None,
        }
    }

    pub fn text(role: impl Into<String>, text: impl Into<String>) -> Self {
        Self::new(role, vec![ChatContent::Text { text: text.into() }])
    }

    /// Schema-only system message used after `SelectTools` loads deferred tools.
    pub fn schema(tools: Vec<ToolDef>) -> Self {
        Self {
            role: "system".into(),
            content: Vec::new(),
            tools: Some(tools),
        }
    }

    pub fn is_dynamic_tool_schema(&self) -> bool {
        self.tools.as_ref().is_some_and(|tools| !tools.is_empty())
    }

    /// True when the message exists only to carry `tools` (no visible content).
    pub fn is_schema_only(&self) -> bool {
        self.is_dynamic_tool_schema() && self.content.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatContent {
    Text {
        text: String,
    },
    Image {
        media_type: String,
        data: String,
    },
    Video {
        media_type: String,
        path: String,
        filename: String,
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
    pub max_tokens: Option<u32>,
    pub system: Option<String>,
    pub thinking: Option<ThinkingParams>,
    /// Stable routing key for providers with native prompt-cache routing.
    /// Omitted for compatible endpoints unless support is known.
    pub prompt_cache_key: Option<String>,
    /// When set, abort the stream if no meaningful content arrives in time.
    pub first_token_timeout: Option<std::time::Duration>,
}

/// Merge message-level tool definitions onto the top-level `tools[]` snapshot.
///
/// Anthropic / OpenAI / Google wire formats have no `messages[].tools`, so
/// providers call this at serialize time. Top-level `LlmRequest.tools` stays
/// the immutable core set; loaded deferred schemas live on history messages.
///
/// The result is sorted by name: providers key their prompt cache on the
/// serialized `tools[]` prefix, so any order churn (registration timing,
/// load order of deferred tools) would invalidate the cache every turn.
pub fn merge_message_level_tools(request: &LlmRequest) -> Vec<ToolDef> {
    let mut tools = request.tools.clone();
    let mut seen: std::collections::HashSet<String> =
        tools.iter().map(|tool| tool.name.clone()).collect();
    for message in &request.messages {
        let Some(message_tools) = &message.tools else {
            continue;
        };
        for tool in message_tools {
            if seen.insert(tool.name.clone()) {
                tools.push(tool.clone());
            }
        }
    }
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    tools
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct ThinkingParams {
    pub budget_tokens: u32,
    pub adaptive: bool,
    pub effort: Option<String>,
}

#[derive(Debug, Clone)]
pub enum StreamEvent {
    TextDelta(String),
    ThinkingDelta(String),
    ToolUseStart {
        id: String,
        name: String,
    },
    ToolUseInputDelta {
        id: String,
        delta: String,
    },
    ToolUseEnd {
        id: String,
    },
    MessageEnd {
        usage: TokenUsage,
        stop_reason: Option<String>,
    },
    RateLimited {
        message: String,
        retry_after: Option<std::time::Duration>,
    },
    Error(String),
}

#[derive(Debug, Clone, Default)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    /// Provider semantics of `input_tokens`: `Some(false)` = excludes cache
    /// buckets (Anthropic), `Some(true)` = includes them (OpenAI / Gemini),
    /// `None` = unknown yet.
    pub input_includes_cache: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(name: &str) -> ToolDef {
        ToolDef {
            name: name.into(),
            description: format!("{name} desc"),
            input_schema: json!({"type": "object"}),
        }
    }

    #[test]
    fn merge_appends_message_level_tools_without_duplicating_core() {
        let request = LlmRequest {
            model: "m".into(),
            messages: vec![ChatMessage::schema(vec![tool("mcp__a"), tool("Read")])],
            tools: vec![tool("Read"), tool("SelectTools")],
            max_tokens: None,
            system: None,
            thinking: None,
            prompt_cache_key: None,
            first_token_timeout: None,
        };
        let merged = merge_message_level_tools(&request);
        let names: Vec<_> = merged.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["Read", "SelectTools", "mcp__a"]);
    }

    #[test]
    fn merge_sorts_tools_by_name_for_stable_prompt_cache() {
        let request = LlmRequest {
            model: "m".into(),
            messages: vec![ChatMessage::schema(vec![tool("a_z_first")])],
            tools: vec![tool("Zeta"), tool("alpha")],
            max_tokens: None,
            system: None,
            thinking: None,
            prompt_cache_key: None,
            first_token_timeout: None,
        };
        let merged = merge_message_level_tools(&request);
        let names: Vec<_> = merged.iter().map(|t| t.name.as_str()).collect();
        // Byte-wise name order, regardless of core-set or load ordering.
        assert_eq!(names, vec!["Zeta", "a_z_first", "alpha"]);
    }

    #[test]
    fn schema_only_message_round_trips_tools() {
        let msg = ChatMessage::schema(vec![tool("mcp__x")]);
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["role"], "system");
        assert!(json.get("content").unwrap().as_array().unwrap().is_empty());
        let back: ChatMessage = serde_json::from_value(json).unwrap();
        assert!(back.is_schema_only());
        assert_eq!(back.tools.as_ref().unwrap()[0].name, "mcp__x");
    }

    #[test]
    fn omitted_tools_field_deserializes_as_none() {
        let msg: ChatMessage = serde_json::from_value(json!({
            "role": "user",
            "content": [{"type": "text", "text": "hi"}]
        }))
        .unwrap();
        assert!(msg.tools.is_none());
    }
}
