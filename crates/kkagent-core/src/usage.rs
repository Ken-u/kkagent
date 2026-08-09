//! Session usage tracker (tokens / cache / steps).

use kkagent_protocol::TokenUsage;

#[derive(Debug, Clone, Default)]
pub struct UsageSnapshot {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub steps: u64,
    pub turns: u64,
}

impl UsageSnapshot {
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }

    pub fn cache_hit_ratio(&self) -> Option<f32> {
        if self.input_tokens == 0 {
            return None;
        }
        Some(self.cache_read_input_tokens as f32 / self.input_tokens as f32)
    }
}

#[derive(Debug, Default)]
pub struct UsageService {
    pub session: UsageSnapshot,
    pub last_step: UsageSnapshot,
}

impl UsageService {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&mut self, usage: &TokenUsage) {
        self.last_step = UsageSnapshot {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_creation_input_tokens: usage.cache_creation_input_tokens,
            cache_read_input_tokens: usage.cache_read_input_tokens,
            steps: 1,
            turns: 0,
        };
        self.session.input_tokens = self.session.input_tokens.saturating_add(usage.input_tokens);
        self.session.output_tokens = self
            .session
            .output_tokens
            .saturating_add(usage.output_tokens);
        self.session.cache_creation_input_tokens = self
            .session
            .cache_creation_input_tokens
            .saturating_add(usage.cache_creation_input_tokens);
        self.session.cache_read_input_tokens = self
            .session
            .cache_read_input_tokens
            .saturating_add(usage.cache_read_input_tokens);
        self.session.steps = self.session.steps.saturating_add(1);
    }

    pub fn begin_turn(&mut self) {
        self.session.turns = self.session.turns.saturating_add(1);
    }

    pub fn snapshot(&self) -> UsageSnapshot {
        self.session.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accumulates() {
        let mut u = UsageService::new();
        u.record(&TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 4,
        });
        assert_eq!(u.session.total_tokens(), 15);
        assert!((u.session.cache_hit_ratio().unwrap() - 0.4).abs() < 0.01);
    }
}
