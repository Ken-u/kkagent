//! Nudges the model to batch independent read-only lookups.
//!
//! Many models issue one `Read` / `Grep` / `Glob` per request even when the
//! next lookups do not depend on each other, paying a full LLM round trip for
//! each. After the second such request in a row the tracker produces a short
//! `<system-reminder>`; it then repeats only every few requests so it never
//! becomes noise. Any request that already batches (or does real work) resets
//! the streak.

use serde_json::Value;

const REMINDER: &str = "\n\n<system-reminder>\n\
The last several responses each made a single read-only lookup (Read / Grep / Glob / read-only Bash). \
When the next files, searches or listings do not depend on each other, request them together in one \
response: they run in parallel and save a round trip. Do not mention this reminder to the user.\n\
</system-reminder>";

/// Reminder fires at this streak, then every [`REPEAT_EVERY`] requests.
const FIRST_AT: u32 = 2;
const REPEAT_EVERY: u32 = 4;

#[derive(Debug, Default)]
pub struct BatchHintTracker {
    streak: u32,
}

impl BatchHintTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Observe the tool calls of one LLM response; returns the reminder to
    /// append to the response's last tool result, if one is due.
    pub fn observe_step(&mut self, calls: &[(String, Value)]) -> Option<&'static str> {
        let lone_lookup = matches!(calls, [(name, input)] if is_single_lookup(name, input));
        if !lone_lookup {
            self.streak = 0;
            return None;
        }
        self.streak = self.streak.saturating_add(1);
        (self.streak >= FIRST_AT && (self.streak - FIRST_AT).is_multiple_of(REPEAT_EVERY))
            .then_some(REMINDER)
    }

    pub fn reset(&mut self) {
        self.streak = 0;
    }
}

/// A lookup that could have been batched with others: `Read` of one file,
/// `Grep` / `Glob`, or a `Bash` command that only reads, searches or lists.
fn is_single_lookup(name: &str, input: &Value) -> bool {
    match name {
        "Read" => input
            .get("files")
            .and_then(Value::as_array)
            .is_none_or(|files| files.len() <= 1),
        "Grep" | "Glob" => true,
        "Bash" => {
            !input
                .get("run_in_background")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                && input
                    .get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|command| kkagent_tools::bash_route::suggest(command).is_some())
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(name: &str, input: Value) -> Vec<(String, Value)> {
        vec![(name.to_string(), input)]
    }

    #[test]
    fn fires_on_the_second_lone_lookup_then_every_fourth() {
        let mut tracker = BatchHintTracker::new();
        let read = call("Read", json!({"path": "a.rs"}));
        let fired: Vec<bool> = (0..11)
            .map(|_| tracker.observe_step(&read).is_some())
            .collect();
        // streaks 1..=11 -> fires at 2, 6, 10
        let expected: Vec<bool> = (1..=11).map(|n| matches!(n, 2 | 6 | 10)).collect();
        assert_eq!(fired, expected);
    }

    #[test]
    fn mixed_lookup_kinds_and_read_only_bash_count_together() {
        let mut tracker = BatchHintTracker::new();
        assert!(tracker
            .observe_step(&call("Grep", json!({"pattern": "x"})))
            .is_none());
        assert!(tracker
            .observe_step(&call("Bash", json!({"command": "cat Cargo.toml"})))
            .is_some());
    }

    #[test]
    fn batching_or_real_work_resets_the_streak() {
        let mut tracker = BatchHintTracker::new();
        let read = call("Read", json!({"path": "a.rs"}));
        assert!(tracker.observe_step(&read).is_none());

        // Two calls in one response: already batching.
        let batch = vec![
            ("Read".to_string(), json!({"path": "a.rs"})),
            ("Grep".to_string(), json!({"pattern": "x"})),
        ];
        assert!(tracker.observe_step(&batch).is_none());
        assert!(tracker.observe_step(&read).is_none(), "streak restarted");

        // A multi-file Read is a batch, an edit is real work.
        let files = call("Read", json!({"files": [{"path": "a"}, {"path": "b"}]}));
        assert!(tracker.observe_step(&files).is_none());
        assert!(tracker.observe_step(&read).is_none());
        assert!(tracker
            .observe_step(&call("Edit", json!({"path": "a.rs"})))
            .is_none());
        assert!(tracker.observe_step(&read).is_none());
    }

    #[test]
    fn only_pure_lookups_in_bash_count() {
        let mut tracker = BatchHintTracker::new();
        for command in ["cargo test", "git status", "cat a > b", "cat a && rm a"] {
            assert!(tracker
                .observe_step(&call("Bash", json!({"command": command})))
                .is_none());
            assert!(tracker
                .observe_step(&call("Bash", json!({"command": command})))
                .is_none());
        }
        let background = call(
            "Bash",
            json!({"command": "ls", "run_in_background": true, "description": "d"}),
        );
        assert!(tracker.observe_step(&background).is_none());
        assert!(tracker.observe_step(&background).is_none());
    }

    #[test]
    fn reset_starts_a_new_streak() {
        let mut tracker = BatchHintTracker::new();
        let read = call("Read", json!({"path": "a.rs"}));
        assert!(tracker.observe_step(&read).is_none());
        tracker.reset();
        assert!(tracker.observe_step(&read).is_none());
        assert!(tracker.observe_step(&read).is_some());
    }
}
