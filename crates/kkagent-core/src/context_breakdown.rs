//! Rough context token breakdown for `/context` transparency.

use kkagent_llm::{ChatContent, ChatMessage, ToolDef};

#[derive(Debug, Clone, Default)]
pub struct ContextBreakdown {
    pub system: u64,
    pub conversation: u64,
    pub tools: u64,
    pub media: u64,
    pub reserved_output: u64,
    pub estimated: bool,
}

impl ContextBreakdown {
    pub fn total_used(&self) -> u64 {
        self.system
            .saturating_add(self.conversation)
            .saturating_add(self.tools)
            .saturating_add(self.media)
    }

    pub fn remaining(&self, max_context: u64) -> i64 {
        max_context as i64 - self.total_used() as i64 - self.reserved_output as i64
    }

    /// Character-based estimate (~4 chars/token). Marks unknown as estimated.
    ///
    /// `tools` covers tool *definitions* (schema text) while tool call/result
    /// blocks inside `messages` are folded into the same bucket for display.
    pub fn estimate(
        system_prompt: &str,
        tools: &[ToolDef],
        messages: &[ChatMessage],
        reserved_output: u64,
    ) -> Self {
        let mut out = Self {
            system: estimate_tokens(system_prompt),
            tools: crate::token_counting::TokenCounter::estimate_tools(tools),
            reserved_output,
            estimated: true,
            ..Default::default()
        };
        for m in messages {
            if let Some(message_tools) = &m.tools {
                out.tools =
                    out.tools
                        .saturating_add(crate::token_counting::TokenCounter::estimate_tools(
                            message_tools,
                        ));
            }
            for part in &m.content {
                match part {
                    ChatContent::Text { text } => {
                        out.conversation = out.conversation.saturating_add(estimate_tokens(text));
                    }
                    ChatContent::ToolUse { .. } | ChatContent::ToolResult { .. } => {
                        let s = serde_json::to_string(part).unwrap_or_default();
                        out.tools = out.tools.saturating_add(estimate_tokens(&s));
                    }
                    ChatContent::Image { .. } | ChatContent::Video { .. } => {
                        // Fixed budget estimate for media when exact tokens unknown.
                        out.media = out.media.saturating_add(1_200);
                    }
                    ChatContent::Thinking { thinking } => {
                        out.conversation =
                            out.conversation.saturating_add(estimate_tokens(thinking));
                    }
                }
            }
        }
        out
    }
}

impl From<ContextBreakdown> for kkagent_protocol::ContextBreakdownInfo {
    fn from(b: ContextBreakdown) -> Self {
        Self {
            system: b.system,
            conversation: b.conversation,
            tools: b.tools,
            media: b.media,
            reserved_output: b.reserved_output,
            estimated: b.estimated,
        }
    }
}

fn estimate_tokens(text: &str) -> u64 {
    (text.chars().count() as u64).div_ceil(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimates_system_and_text() {
        let b = ContextBreakdown::estimate("hello world", &[], &[], 1024);
        assert!(b.system > 0);
        assert_eq!(b.reserved_output, 1024);
        assert!(b.estimated);
    }

    #[test]
    fn counts_tool_definitions() {
        let tools = vec![ToolDef {
            name: "read_file".into(),
            description: "Read a file from disk and return its contents.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Absolute file path" }
                },
                "required": ["path"]
            }),
        }];
        let b = ContextBreakdown::estimate("sys", &tools, &[], 0);
        assert!(b.tools > 0, "tool defs should contribute tokens");
        assert!(b.total_used() >= b.tools);
    }
}
