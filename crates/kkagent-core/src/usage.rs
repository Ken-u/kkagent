//! Session usage tracker (tokens / cache / steps).

use kkagent_protocol::TokenUsage;

pub use kkagent_protocol::cache_hit_ratio_ex;

#[derive(Debug, Clone, Default)]
pub struct UsageSnapshot {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    /// Provider semantics of `input_tokens` (`Some(false)` = Anthropic,
    /// `Some(true)` = OpenAI/Gemini, `None` = unknown/empty snapshot).
    pub input_includes_cache: Option<bool>,
    pub steps: u64,
    pub turns: u64,
}

impl UsageSnapshot {
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }

    pub fn cache_hit_ratio(&self, input_includes_cache: Option<bool>) -> Option<f32> {
        cache_hit_ratio_ex(
            self.input_tokens,
            self.cache_creation_input_tokens,
            self.cache_read_input_tokens,
            input_includes_cache,
        )
    }
}

#[derive(Debug, Default)]
pub struct UsageService {
    pub session: UsageSnapshot,
    pub last_step: UsageSnapshot,
    /// Last estimated context breakdown attached by the agent loop.
    pub last_context: Option<kkagent_protocol::ContextBreakdownInfo>,
    /// Cumulative provider-normalized tokens (effective input + output).
    total_consumed: u64,
    /// `total_consumed` baseline captured at the current turn start; goal
    /// budgeting records the per-turn delta of this counter.
    turn_start_consumed: u64,
    /// Provider semantics of the recorded `input_tokens` (latest step wins).
    /// `None` until the first step with an explicit flag arrives.
    input_includes_cache: Option<bool>,
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
            input_includes_cache: usage.input_includes_cache,
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
        if usage.input_includes_cache.is_some() {
            self.input_includes_cache = usage.input_includes_cache;
        }
        self.total_consumed = self.total_consumed.saturating_add(
            usage
                .total_input_tokens()
                .saturating_add(usage.output_tokens),
        );
    }

    /// Provider semantics of the recorded input totals; `None` = unknown
    /// (legacy data), consumers fall back to the heuristic.
    pub fn input_includes_cache(&self) -> Option<bool> {
        self.input_includes_cache
    }

    pub fn begin_turn(&mut self) {
        self.session.turns = self.session.turns.saturating_add(1);
        self.turn_start_consumed = self.total_consumed;
    }

    /// Provider-normalized tokens consumed by the current turn so far
    /// (effective input + output across all steps of the turn).
    pub fn turn_tokens(&self) -> u64 {
        self.total_consumed.saturating_sub(self.turn_start_consumed)
    }

    pub fn set_context(&mut self, ctx: kkagent_protocol::ContextBreakdownInfo) {
        self.last_context = Some(ctx);
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
            input_includes_cache: Some(true),
        });
        assert_eq!(u.session.total_tokens(), 15);
        assert!((u.session.cache_hit_ratio(Some(true)).unwrap() - 0.4).abs() < 0.01);
    }

    #[test]
    fn cache_hit_ratio_openai_style() {
        // OpenAI: input_tokens already includes cached tokens.
        // 400 cached out of 1000 total input → 40%.
        let r = cache_hit_ratio_ex(1000, 0, 400, Some(true)).unwrap();
        assert!((r - 0.4).abs() < 0.01);
    }

    #[test]
    fn cache_hit_ratio_anthropic_style() {
        // Anthropic: input excludes cache; total = 100 + 400 + 500 = 1000,
        // cache_read = 500 → 50%.
        let r = cache_hit_ratio_ex(100, 400, 500, Some(false)).unwrap();
        assert!((r - 0.5).abs() < 0.01);

        // Steady-state hit: mostly cache reads.
        // total = 200 + 100 + 2700 = 3000, read = 2700 → 90%.
        let r = cache_hit_ratio_ex(200, 100, 2_700, Some(false)).unwrap();
        assert!((r - 0.9).abs() < 0.01);
    }

    #[test]
    fn cache_hit_ratio_semantics_mismatch_caps_at_one() {
        // Anthropic data + OpenAI math would give 500/100 = 5.0; the explicit
        // flag keeps the ratio in [0, 1].
        let r = cache_hit_ratio_ex(100, 0, 500, Some(false)).unwrap();
        assert!((r - 500.0 / 600.0).abs() < 0.01);
    }

    #[test]
    fn cache_hit_ratio_none_when_no_read() {
        assert!(cache_hit_ratio_ex(1000, 500, 0, None).is_none());
        assert!(cache_hit_ratio_ex(0, 0, 0, None).is_none());
        // All-cache edge: total = 0 + 0 + 10 → ratio 1.0, not None.
        let r = cache_hit_ratio_ex(0, 0, 10, Some(false)).unwrap();
        assert!((r - 1.0).abs() < 1e-6);
    }

    #[test]
    fn turn_tokens_across_multi_step_turn() {
        let mut u = UsageService::new();
        u.begin_turn();
        // Anthropic semantics: input excludes cache buckets.
        u.record(&TokenUsage {
            input_tokens: 100,
            output_tokens: 50,
            cache_creation_input_tokens: 400,
            cache_read_input_tokens: 0,
            input_includes_cache: Some(false),
        });
        u.record(&TokenUsage {
            input_tokens: 100,
            output_tokens: 30,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 500,
            input_includes_cache: Some(false),
        });
        // Effective input: (100+400) + (100+500) = 1100, output: 80.
        assert_eq!(u.turn_tokens(), 1180);

        // Next turn starts a fresh baseline.
        u.begin_turn();
        assert_eq!(u.turn_tokens(), 0);
        u.record(&TokenUsage {
            input_tokens: 200,
            output_tokens: 10,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            input_includes_cache: None,
        });
        assert_eq!(u.turn_tokens(), 210);
    }

    #[test]
    fn turn_tokens_openai_style_usage() {
        let mut u = UsageService::new();
        u.begin_turn();
        // OpenAI semantics: input_tokens already includes cached tokens;
        // effective input stays 1000 (no double counting).
        u.record(&TokenUsage {
            input_tokens: 1000,
            output_tokens: 200,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 400,
            input_includes_cache: Some(true),
        });
        assert_eq!(u.turn_tokens(), 1200);
    }
}
