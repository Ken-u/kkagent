//! Strict tool allowlists.
//!
//! A [`ToolAllowlist`] is a set of tool-name patterns. Each pattern is either
//! an exact tool name (`"Read"`) or a prefix glob that ends in `*`
//! (`"mcp__github__*"`, `"*"` for everything). Matching is case-sensitive.
//!
//! The allowlist is a *ceiling*: [`ToolAllowlist::intersect`] can only shrink
//! the set of allowed names, never grow it. `Option<ToolAllowlist>` is used
//! throughout the runtime where `None` means "unrestricted" (the default,
//! compatible behavior) and `Some(empty)` means "deny every tool".

use serde::{Deserialize, Serialize};

/// A set of exact / prefix-glob tool-name patterns.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ToolAllowlist {
    patterns: Vec<String>,
}

impl ToolAllowlist {
    /// Build an allowlist from patterns. Blank patterns are dropped and
    /// duplicates removed (first occurrence wins, order preserved).
    pub fn new<I, S>(patterns: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut out: Vec<String> = Vec::new();
        for pattern in patterns {
            let pattern = pattern.into();
            let pattern = pattern.trim();
            if pattern.is_empty() || out.iter().any(|existing| existing == pattern) {
                continue;
            }
            out.push(pattern.to_string());
        }
        Self { patterns: out }
    }

    /// An allowlist that permits nothing.
    pub fn deny_all() -> Self {
        Self::default()
    }

    /// The raw patterns.
    pub fn patterns(&self) -> &[String] {
        &self.patterns
    }

    /// True when no pattern is present (every tool is denied).
    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// Whether `name` matches at least one pattern.
    pub fn allows(&self, name: &str) -> bool {
        self.patterns
            .iter()
            .any(|pattern| pattern_matches(pattern, name))
    }

    /// Whether some name that starts with `prefix` could be allowed. Used to
    /// avoid side effects (for example starting an MCP server) for tools the
    /// allowlist can never permit; it never decides what a tool may do.
    pub fn may_allow_prefix(&self, prefix: &str) -> bool {
        self.patterns
            .iter()
            .any(|pattern| match split_glob(pattern) {
                (exact, false) => exact.starts_with(prefix),
                (glob, true) => glob.starts_with(prefix) || prefix.starts_with(glob),
            })
    }

    /// The exact intersection of two allowlists: a name is allowed by the
    /// result iff it is allowed by both inputs.
    pub fn intersect(&self, other: &Self) -> Self {
        let mut out: Vec<String> = Vec::new();
        for a in &self.patterns {
            for b in &other.patterns {
                if let Some(pattern) = intersect_patterns(a, b) {
                    if !out.contains(&pattern) {
                        out.push(pattern);
                    }
                }
            }
        }
        Self { patterns: out }
    }

    /// Combine two optional ceilings: `None` is "unrestricted", so the result
    /// is the other side; two restrictions are intersected.
    pub fn narrow(a: Option<&Self>, b: Option<&Self>) -> Option<Self> {
        match (a, b) {
            (None, None) => None,
            (Some(only), None) | (None, Some(only)) => Some(only.clone()),
            (Some(a), Some(b)) => Some(a.intersect(b)),
        }
    }
}

fn split_glob(pattern: &str) -> (&str, bool) {
    match pattern.strip_suffix('*') {
        Some(prefix) => (prefix, true),
        None => (pattern, false),
    }
}

fn pattern_matches(pattern: &str, name: &str) -> bool {
    match split_glob(pattern) {
        (prefix, true) => name.starts_with(prefix),
        (exact, false) => name == exact,
    }
}

fn intersect_patterns(a: &str, b: &str) -> Option<String> {
    match (split_glob(a), split_glob(b)) {
        ((exact_a, false), (exact_b, false)) => (exact_a == exact_b).then(|| exact_a.to_string()),
        ((exact, false), (prefix, true)) | ((prefix, true), (exact, false)) => {
            exact.starts_with(prefix).then(|| exact.to_string())
        }
        ((prefix_a, true), (prefix_b, true)) => {
            if prefix_a.starts_with(prefix_b) {
                Some(a.to_string())
            } else if prefix_b.starts_with(prefix_a) {
                Some(b.to_string())
            } else {
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_probe_tracks_what_could_ever_match() {
        let allow = ToolAllowlist::new(["Read", "mcp__gh__*", "mcp__only__issues"]);
        assert!(allow.may_allow_prefix("mcp__gh__"));
        assert!(allow.may_allow_prefix("mcp__only__"));
        assert!(!allow.may_allow_prefix("mcp__other__"));
        assert!(!allow.may_allow_prefix("Bash"));
        assert!(ToolAllowlist::new(["*"]).may_allow_prefix("mcp__anything__"));
        assert!(!ToolAllowlist::deny_all().may_allow_prefix("mcp__gh__"));
    }

    #[test]
    fn exact_and_prefix_patterns_match() {
        let allow = ToolAllowlist::new(["Read", "mcp__gh__*"]);
        assert!(allow.allows("Read"));
        assert!(!allow.allows("Reader"));
        assert!(allow.allows("mcp__gh__issues"));
        assert!(!allow.allows("mcp__other__issues"));
        assert!(!allow.allows("Bash"));
    }

    #[test]
    fn empty_allowlist_denies_everything_and_star_allows_everything() {
        assert!(!ToolAllowlist::deny_all().allows("Read"));
        assert!(ToolAllowlist::new(["*"]).allows("anything"));
    }

    #[test]
    fn blank_and_duplicate_patterns_are_dropped() {
        let allow = ToolAllowlist::new(["Read", " ", "Read", ""]);
        assert_eq!(allow.patterns(), ["Read".to_string()]);
    }

    #[test]
    fn intersection_is_exact_for_every_pattern_shape() {
        let a = ToolAllowlist::new(["Read", "Grep", "mcp__*", "host__task_*"]);
        let b = ToolAllowlist::new(["Read", "Bash", "mcp__gh__*", "host__*"]);
        let both = a.intersect(&b);
        for name in [
            "Read",
            "Grep",
            "Bash",
            "mcp__gh__issues",
            "mcp__x__y",
            "host__task_create",
            "host__other",
            "Write",
        ] {
            assert_eq!(
                both.allows(name),
                a.allows(name) && b.allows(name),
                "mismatch for {name}"
            );
        }
    }

    #[test]
    fn intersection_never_widens() {
        let narrow = ToolAllowlist::new(["Read"]);
        let wide = ToolAllowlist::new(["*"]);
        let both = narrow.intersect(&wide);
        assert!(both.allows("Read"));
        assert!(!both.allows("Bash"));
        assert!(narrow.intersect(&ToolAllowlist::deny_all()).is_empty());
    }

    #[test]
    fn narrow_treats_none_as_unrestricted() {
        let read = ToolAllowlist::new(["Read"]);
        assert_eq!(ToolAllowlist::narrow(None, None), None);
        assert_eq!(ToolAllowlist::narrow(Some(&read), None), Some(read.clone()));
        assert_eq!(ToolAllowlist::narrow(None, Some(&read)), Some(read.clone()));
        let write = ToolAllowlist::new(["Write"]);
        assert_eq!(
            ToolAllowlist::narrow(Some(&read), Some(&write)),
            Some(ToolAllowlist::deny_all())
        );
    }

    #[test]
    fn serializes_as_plain_string_array() {
        let allow = ToolAllowlist::new(["Read", "mcp__*"]);
        let json = serde_json::to_string(&allow).unwrap();
        assert_eq!(json, r#"["Read","mcp__*"]"#);
        let back: ToolAllowlist = serde_json::from_str(&json).unwrap();
        assert_eq!(back, allow);
    }
}
