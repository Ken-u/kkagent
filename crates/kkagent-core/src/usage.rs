//! Session usage tracker (tokens / cache / steps).

use std::collections::BTreeMap;

use kkagent_protocol::{ModelUsageEntry, TokenUsage};

pub use kkagent_protocol::cache_hit_ratio_ex;

/// Well-known call-site labels for [`UsageService::record_labeled`].
pub mod usage_location {
    /// Primary conversation turns (any model, incl. mid-turn fallback switches).
    pub const MAIN: &str = "main";
    /// Auto/overflow compaction summary calls.
    pub const COMPACTION: &str = "compaction";
    /// Goal completion judge runs.
    pub const JUDGE: &str = "judge";
    /// Delegated subagent runs (incl. their nested turns).
    pub const SUBAGENT: &str = "subagent";
}

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
    /// Cumulative usage keyed by `(model alias, call site)`.
    by_model: BTreeMap<(String, String), ModelUsageEntry>,
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
        self.record_labeled(usage, "", "");
    }

    /// Record a call's usage and attribute it to `model` at `location`.
    /// Empty `model`/`location` are displayed as `"?"` in the breakdown and
    /// skipped in event payloads only when the whole entry would be empty.
    pub fn record_labeled(&mut self, usage: &TokenUsage, model: &str, location: &str) {
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
        if model.is_empty() && location.is_empty() {
            return;
        }
        let entry = self
            .by_model
            .entry((model.to_string(), location.to_string()))
            .or_default();
        entry.model = model.to_string();
        entry.location = location.to_string();
        entry.calls = entry.calls.saturating_add(1);
        entry.input_tokens = entry.input_tokens.saturating_add(usage.input_tokens);
        entry.output_tokens = entry.output_tokens.saturating_add(usage.output_tokens);
        entry.cache_creation_input_tokens = entry
            .cache_creation_input_tokens
            .saturating_add(usage.cache_creation_input_tokens);
        entry.cache_read_input_tokens = entry
            .cache_read_input_tokens
            .saturating_add(usage.cache_read_input_tokens);
        if usage.input_includes_cache.is_some() {
            entry.input_includes_cache = usage.input_includes_cache;
        }
        // Durable cross-session history (no-op when the global sink is not
        // installed or running under `cargo test`). The usage service does
        // not know its owning session id; the TUI/RPC layer tags events by
        // session, while the history tables only aggregate by day.
        crate::usage_store::try_record(crate::usage_store::UsageEvent {
            ts: crate::audit::now_rfc3339(),
            day: crate::usage_store::local_day_string(),
            session_id: String::new(),
            model: model.to_string(),
            location: location.to_string(),
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_creation_input_tokens: usage.cache_creation_input_tokens,
            cache_read_input_tokens: usage.cache_read_input_tokens,
        });
    }

    /// Sorted snapshot of the per-model/per-location breakdown.
    pub fn by_model_entries(&self) -> Vec<ModelUsageEntry> {
        self.by_model.values().cloned().collect()
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
    fn labeled_records_group_by_model_and_location() {
        let mut u = UsageService::new();
        u.record_labeled(
            &TokenUsage {
                input_tokens: 100,
                output_tokens: 10,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                input_includes_cache: Some(true),
            },
            "oai/glm-5.3",
            usage_location::MAIN,
        );
        u.record_labeled(
            &TokenUsage {
                input_tokens: 50,
                output_tokens: 5,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                input_includes_cache: Some(true),
            },
            "oai/glm-5.3",
            usage_location::MAIN,
        );
        u.record_labeled(
            &TokenUsage {
                input_tokens: 2000,
                output_tokens: 300,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                input_includes_cache: Some(true),
            },
            "glm-5.3-flash",
            usage_location::COMPACTION,
        );
        u.record_labeled(
            &TokenUsage {
                input_tokens: 700,
                output_tokens: 900,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                input_includes_cache: Some(true),
            },
            "glm-5.3-flash",
            usage_location::SUBAGENT,
        );
        let entries = u.by_model_entries();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].model, "glm-5.3-flash");
        assert_eq!(entries[0].location, usage_location::COMPACTION);
        assert_eq!(entries[0].calls, 1);
        assert_eq!(entries[0].total_tokens(), 2300);
        assert_eq!(entries[1].location, usage_location::SUBAGENT);
        assert_eq!(entries[2].model, "oai/glm-5.3");
        assert_eq!(entries[2].calls, 2);
        assert_eq!(entries[2].total_input_tokens(), 150);
        // Plain `record` (no labels) never lands in the breakdown.
        u.record(&TokenUsage {
            input_tokens: 1,
            output_tokens: 1,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            input_includes_cache: None,
        });
        assert_eq!(u.by_model_entries().len(), 3);
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
