//! Heuristic + measured token counting (aligned with ref `tokenCounting`).

use kkagent_llm::{ChatContent, ChatMessage, ToolDef};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenCountingStrategy {
    /// Live estimate floored by last measured total (default).
    MeasuredPlusEstimated,
    /// Latest measured anchor only.
    Measured,
    /// Pure estimate, ignore anchors.
    Estimated,
}

impl TokenCountingStrategy {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "measured" => Self::Measured,
            "estimated" => Self::Estimated,
            _ => Self::MeasuredPlusEstimated,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ContextSize {
    pub size: u64,
    pub measured: u64,
    pub estimated: u64,
}

#[derive(Debug, Clone, Default)]
pub struct TokenCounter {
    pub strategy: TokenCountingStrategy,
    latest_measured: u64,
    session_input: u64,
    session_output: u64,
}

impl Default for TokenCountingStrategy {
    fn default() -> Self {
        Self::MeasuredPlusEstimated
    }
}

impl TokenCounter {
    pub fn new(strategy: TokenCountingStrategy) -> Self {
        Self {
            strategy,
            ..Default::default()
        }
    }

    pub fn latest_measured(&self) -> u64 {
        self.latest_measured
    }

    pub fn session_usage(&self) -> (u64, u64) {
        (self.session_input, self.session_output)
    }

    pub fn record_measured(&mut self, input_tokens: u64, output_tokens: u64) {
        self.latest_measured = input_tokens;
        self.session_input = self.session_input.saturating_add(input_tokens);
        self.session_output = self.session_output.saturating_add(output_tokens);
    }

    /// Character-based estimate (~4 chars/token).
    pub fn estimate_text(text: &str) -> u64 {
        let chars = text.chars().count() as u64;
        chars.div_ceil(4).max(1)
    }

    pub fn estimate_message(message: &ChatMessage) -> u64 {
        let mut n = 4; // role overhead
        for part in &message.content {
            n += match part {
                ChatContent::Text { text } => Self::estimate_text(text),
                ChatContent::Thinking { thinking } => Self::estimate_text(thinking),
                ChatContent::ToolUse { name, input, .. } => {
                    Self::estimate_text(name) + Self::estimate_text(&input.to_string()) + 8
                }
                ChatContent::ToolResult { content, .. } => Self::estimate_text(content) + 6,
            };
        }
        n
    }

    pub fn estimate_messages(messages: &[ChatMessage]) -> u64 {
        messages.iter().map(Self::estimate_message).sum()
    }

    pub fn estimate_tools(tools: &[ToolDef]) -> u64 {
        tools
            .iter()
            .map(|t| {
                Self::estimate_text(&t.name)
                    + Self::estimate_text(&t.description)
                    + Self::estimate_text(&t.input_schema.to_string())
                    + 16
            })
            .sum()
    }

    pub fn request_size(&self, system: &str, tools: &[ToolDef], messages: &[ChatMessage]) -> u64 {
        Self::estimate_text(system)
            + Self::estimate_tools(tools)
            + Self::estimate_messages(messages)
            + 32
    }

    pub fn context_size(&self, messages: &[ChatMessage]) -> ContextSize {
        let estimated = Self::estimate_messages(messages);
        let measured = self.latest_measured;
        let size = match self.strategy {
            TokenCountingStrategy::Estimated => estimated,
            TokenCountingStrategy::Measured => measured,
            TokenCountingStrategy::MeasuredPlusEstimated => estimated.max(measured),
        };
        ContextSize {
            size,
            measured,
            estimated,
        }
    }

    pub fn status_size(&self, messages: &[ChatMessage]) -> u64 {
        self.context_size(messages).size
    }

    /// Remaining budget before reserved headroom is consumed.
    pub fn remaining_budget(&self, max_context: u64, reserved: u64, request_tokens: u64) -> i64 {
        let usable = max_context.saturating_sub(reserved);
        usable as i64 - request_tokens as i64
    }

    pub fn needs_compaction(&self, max_context: u64, reserved: u64, request_tokens: u64) -> bool {
        self.remaining_budget(max_context, reserved, request_tokens) < 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_grows_with_text() {
        let short = TokenCounter::estimate_text("hi");
        let long = TokenCounter::estimate_text(&"x".repeat(400));
        assert!(long > short);
    }

    #[test]
    fn measured_plus_estimated_floors() {
        let mut c = TokenCounter::new(TokenCountingStrategy::MeasuredPlusEstimated);
        c.record_measured(10_000, 100);
        let msgs = vec![ChatMessage {
            role: "user".into(),
            content: vec![ChatContent::Text {
                text: "hello".into(),
            }],
        }];
        let size = c.context_size(&msgs);
        assert_eq!(size.measured, 10_000);
        assert!(size.size >= 10_000);
    }

    #[test]
    fn needs_compaction_when_over() {
        let c = TokenCounter::new(TokenCountingStrategy::Estimated);
        assert!(c.needs_compaction(1000, 200, 900));
        assert!(!c.needs_compaction(1000, 200, 500));
    }
}
