//! Plugin override policy: which built-in tool names a plugin may replace.
//!
//! Guard tools are wired into permission/plan-mode checks by name and can
//! never be overridden. High-risk execution tools require explicit opt-in
//! via `[plugins] extra_overridable_tools`.

/// Tools that participate in permission/plan-mode guards and must never be
/// replaceable by a plugin, regardless of configuration.
pub const NEVER_OVERRIDABLE: &[&str] =
    &["AskUserQuestion", "EnterPlanMode", "ExitPlanMode", "Goal"];

/// Low-risk built-ins plugins may override by default.
pub const DEFAULT_OVERRIDABLE: &[&str] = &["Web", "TaskOutput", "Skill", "Cron", "ReadMediaFile"];

/// True when `name` may be overridden by a plugin, given the user-configured
/// extra allowlist (already-never names stay rejected even if listed).
pub fn tool_overridable(name: &str, extra: &[String]) -> bool {
    if NEVER_OVERRIDABLE.contains(&name) {
        return false;
    }
    DEFAULT_OVERRIDABLE.contains(&name) || extra.iter().any(|candidate| candidate == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extra(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn default_allowlist_is_overridable() {
        for name in DEFAULT_OVERRIDABLE {
            assert!(tool_overridable(name, &[]), "{name} should be overridable");
        }
    }

    #[test]
    fn guard_tools_are_never_overridable() {
        for name in NEVER_OVERRIDABLE {
            assert!(
                !tool_overridable(name, &[]),
                "{name} must not be overridable by default"
            );
            assert!(
                !tool_overridable(name, &extra(&[name])),
                "{name} must not be overridable even when explicitly listed"
            );
        }
    }

    #[test]
    fn high_risk_tools_require_explicit_opt_in() {
        for name in ["Bash", "Edit", "Write", "Read"] {
            assert!(!tool_overridable(name, &[]), "{name} needs opt-in");
            assert!(tool_overridable(name, &extra(&[name])));
        }
    }
}
