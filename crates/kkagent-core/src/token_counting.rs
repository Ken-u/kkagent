//! Heuristic + measured token counting (aligned with ref `tokenCounting`).

use kkagent_llm::{ChatContent, ChatMessage, ToolDef};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TokenCountingStrategy {
    /// Live estimate floored by last measured total (default).
    #[default]
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
                // Provider image tokenization varies by model and detail mode.
                // A conservative fixed estimate keeps compaction from ignoring it.
                ChatContent::Image { .. } => 1_600,
                ChatContent::Video { .. } => 4_000,
            };
        }
        if let Some(tools) = &message.tools {
            n = n.saturating_add(Self::estimate_tools(tools));
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

    /// Strategy-aware request size for compaction decisions.
    ///
    /// [`Self::request_size`] is a pure chars/4 heuristic. That undercounts
    /// CJK-heavy conversations by ~3-4x (Chinese is ~1-1.5 chars per token),
    /// so auto-compaction keyed on it never reaches the trigger ratio even
    /// when the provider-measured usage (what the TUI footer shows) is far
    /// past it. This method folds the last measured request size into the
    /// estimate according to the configured strategy, so `measured+estimated`
    /// (the default) and `measured` strategies compact on the same numbers
    /// the user sees.
    pub fn request_size_for_compaction(
        &self,
        system: &str,
        tools: &[ToolDef],
        messages: &[ChatMessage],
    ) -> u64 {
        let estimate = self.request_size(system, tools, messages);
        match self.strategy {
            TokenCountingStrategy::Estimated => estimate,
            // No measurement yet (fresh session): fall back to the estimate
            // so the very first oversized turn can still compact.
            TokenCountingStrategy::Measured if self.latest_measured == 0 => estimate,
            TokenCountingStrategy::Measured => self.latest_measured,
            TokenCountingStrategy::MeasuredPlusEstimated => estimate.max(self.latest_measured),
        }
    }

    /// Clamp the stale measured anchor after history was discarded (e.g. a
    /// compaction rewrote the transcript). The old measurement no longer
    /// describes any request, and keeping it would make the next compaction
    /// decision re-fire immediately on the shrunk transcript.
    pub fn clamp_measured_to(&mut self, estimate: u64) {
        if self.latest_measured > estimate {
            self.latest_measured = estimate;
        }
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
            tools: None,
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

    #[test]
    fn compaction_size_uses_measured_floor_for_measured_strategies() {
        let msgs = vec![ChatMessage {
            role: "user".into(),
            content: vec![ChatContent::Text {
                // ~3k-token estimate via chars/4; far below the measurement.
                text: "a".repeat(12_000),
            }],
            tools: None,
        }];
        let mut c = TokenCounter::new(TokenCountingStrategy::MeasuredPlusEstimated);
        c.record_measured(90_000, 10);
        assert_eq!(
            c.request_size_for_compaction("", &[], &msgs),
            90_000,
            "measured+estimated must not let the chars/4 heuristic hide real usage"
        );
        // Measured-only strategy follows the measurement, with an estimate
        // fallback before the first response arrives.
        let mut measured = TokenCounter::new(TokenCountingStrategy::Measured);
        assert!(
            measured.request_size_for_compaction("", &[], &msgs) > 0,
            "fresh session falls back to the estimate"
        );
        measured.record_measured(90_000, 10);
        assert_eq!(measured.request_size_for_compaction("", &[], &msgs), 90_000);
        // Estimated strategy stays pure heuristic.
        let mut est = TokenCounter::new(TokenCountingStrategy::Estimated);
        est.record_measured(90_000, 10);
        assert!(est.request_size_for_compaction("", &[], &msgs) < 5_000);
    }

    #[test]
    fn clamp_measured_prevents_recompaction_after_history_drop() {
        let mut c = TokenCounter::new(TokenCountingStrategy::MeasuredPlusEstimated);
        c.record_measured(90_000, 10);
        c.clamp_measured_to(20_000);
        assert_eq!(c.latest_measured(), 20_000);
        // Clamping never raises the anchor.
        c.clamp_measured_to(40_000);
        assert_eq!(c.latest_measured(), 20_000);
    }
}
