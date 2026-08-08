//! Per-turn and cross-turn tool-call deduplication (ref `tool-dedup`).

use serde_json::Value;
use std::collections::{HashMap, HashSet};

const REMINDER_1: &str = "\n\n<system-reminder>\n\
The same tool call has been repeated several times in a row. \
Before making your next call, write one sentence stating what new information you expect it to produce. \
Then act on that sentence.\n</system-reminder>";

const REMINDER_2: &str = "\n\n<system-reminder>\n\
The same tool call has been issued many times in a row. Choose one: \
(1) cheapest falsification check, (2) ask the user for missing input, \
(3) conclude with best evidence so far.\n</system-reminder>";

const REMINDER_3: &str = "\n\n<system-reminder>\n\
Write your final response now without further tool calls. \
Cover the blocker, approaches tried, and what you need from the user.\n</system-reminder>";

#[derive(Debug, Default)]
pub struct ToolDedupeTracker {
    /// Cross-turn streak of identical (name + canonical args).
    last_key: Option<String>,
    streak: u32,
}

#[derive(Debug, Clone)]
pub struct DedupeOutcome {
    /// Tool call indices to skip (same-step duplicates).
    pub skip_indices: HashSet<usize>,
    /// Reminder text to append to the last kept duplicate's result (if any).
    pub reminder: Option<String>,
    /// Force stop after executing kept tools (no more LLM recursion).
    pub force_stop: bool,
}

impl ToolDedupeTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn make_key(tool_name: &str, args: &Value) -> String {
        format!("{tool_name} {}", canonical_args(args))
    }

    /// Deduplicate within one assistant step and update cross-turn streak.
    pub fn observe_step(&mut self, calls: &[(String, Value)]) -> DedupeOutcome {
        let mut seen: HashMap<String, usize> = HashMap::new();
        let mut skip = HashSet::new();
        for (i, (name, args)) in calls.iter().enumerate() {
            let key = Self::make_key(name, args);
            if let Some(&first) = seen.get(&key) {
                let _ = first;
                skip.insert(i);
            } else {
                seen.insert(key, i);
            }
        }

        // Cross-turn streak based on first kept call (or sole call).
        let primary = calls.iter().enumerate().find(|(i, _)| !skip.contains(i));
        let mut reminder = None;
        let mut force_stop = false;
        if let Some((_, (name, args))) = primary {
            let key = Self::make_key(name, args);
            if self.last_key.as_deref() == Some(key.as_str()) {
                self.streak = self.streak.saturating_add(1);
            } else {
                self.last_key = Some(key);
                self.streak = 1;
            }
            if self.streak >= 12 {
                reminder = Some(REMINDER_3.to_string());
                force_stop = true;
            } else if self.streak >= 8 {
                reminder = Some(REMINDER_3.to_string());
            } else if self.streak >= 5 {
                reminder = Some(REMINDER_2.to_string());
            } else if self.streak >= 3 {
                reminder = Some(REMINDER_1.to_string());
            }
        }

        DedupeOutcome {
            skip_indices: skip,
            reminder,
            force_stop,
        }
    }

    pub fn reset(&mut self) {
        self.last_key = None;
        self.streak = 0;
    }
}

/// Stable JSON canonicalization for dedupe keys.
pub fn canonical_args(v: &Value) -> String {
    match v {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys
                .into_iter()
                .map(|k| format!("{}:{}", k, canonical_args(&map[k])))
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        Value::Array(arr) => {
            let parts: Vec<String> = arr.iter().map(canonical_args).collect();
            format!("[{}]", parts.join(","))
        }
        Value::String(s) => format!("\"{s}\""),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn same_step_dedupe() {
        let mut t = ToolDedupeTracker::new();
        let calls = vec![
            ("Read".into(), json!({"path": "a.rs"})),
            ("Read".into(), json!({"path": "a.rs"})),
            ("Read".into(), json!({"path": "b.rs"})),
        ];
        let out = t.observe_step(&calls);
        assert!(out.skip_indices.contains(&1));
        assert!(!out.skip_indices.contains(&0));
        assert!(!out.skip_indices.contains(&2));
    }

    #[test]
    fn streak_reminder() {
        let mut t = ToolDedupeTracker::new();
        let call = vec![("Grep".into(), json!({"pattern": "foo"}))];
        for _ in 0..3 {
            let _ = t.observe_step(&call);
        }
        let out = t.observe_step(&call);
        assert!(out.reminder.is_some());
        assert!(t.streak >= 3);
    }
}
