use kkagent_protocol::PermissionMode;
use kkagent_config::PermissionRule;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub enum PermissionDecision {
    Approve,
    Ask,
    Deny(String),
}

pub struct PermissionChain {
    pub mode: PermissionMode,
    pub rules: Vec<PermissionRule>,
    pub session_approved: Vec<String>,
}

const SENSITIVE_PATTERNS: &[&str] = &[
    ".env", "id_rsa", "id_ed25519", "id_ecdsa", ".pem",
    "credentials", "secret", ".key", "token",
];

const READ_ONLY_TOOLS: &[&str] = &[
    "Read", "Grep", "Glob", "ReadMediaFile", "WebSearch",
    "FetchURL", "EnterPlanMode", "ExitPlanMode", "TodoList", "GetGoal",
    "TaskList", "TaskOutput", "CronList", "SelectTools",
];

impl PermissionChain {
    pub fn new(mode: PermissionMode, rules: Vec<PermissionRule>) -> Self {
        Self {
            mode,
            rules,
            session_approved: Vec::new(),
        }
    }

    /// Evaluate permission chain (kimi-code v2 order), with plan-mode guard first.
    pub fn evaluate(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        working_dir: &Path,
        plan_mode: bool,
        plan_file: Option<&Path>,
    ) -> PermissionDecision {
        // 0. plan-mode-guard-deny (must run before auto/yolo approve)
        if plan_mode {
            if let Some(deny) = plan_mode_guard(tool_name, input, working_dir, plan_file) {
                return deny;
            }
        }

        // 1. auto-mode-ask-user-question-deny
        if self.mode == PermissionMode::Auto && tool_name == "AskUserQuestion" {
            return PermissionDecision::Deny(
                "AskUserQuestion is disabled in auto mode. Make a decision and continue.".into()
            );
        }

        // 2. user-configured-deny
        for rule in &self.rules {
            if rule.decision == "deny" && matches_pattern(&rule.pattern, tool_name, input) {
                return PermissionDecision::Deny(format!("Denied by rule: {}", rule.pattern));
            }
        }

        // 3. auto-mode-approve
        if self.mode == PermissionMode::Auto {
            return PermissionDecision::Approve;
        }

        // 4. session-approval-history
        let approval_key = format!("{}:{}", tool_name, approval_pattern(tool_name, input));
        if self.session_approved.contains(&approval_key) {
            return PermissionDecision::Approve;
        }

        // 5. user-configured-ask / user-configured-allow
        for rule in &self.rules {
            if rule.decision == "allow" && matches_pattern(&rule.pattern, tool_name, input) {
                return PermissionDecision::Approve;
            }
            if rule.decision == "ask" && matches_pattern(&rule.pattern, tool_name, input) {
                return PermissionDecision::Ask;
            }
        }

        // 6. sensitive-file-access-ask
        if has_sensitive_file_access(tool_name, input) {
            return PermissionDecision::Ask;
        }

        // 7. git-control-path-access-ask
        if accesses_git_control_path(tool_name, input) {
            return PermissionDecision::Ask;
        }

        // 8. yolo-mode-approve
        if self.mode == PermissionMode::Yolo {
            return PermissionDecision::Approve;
        }

        // 9. default-tool-approve (read-only tools)
        if READ_ONLY_TOOLS.contains(&tool_name) {
            return PermissionDecision::Approve;
        }

        // 10. git-cwd-write-approve
        // (simplified: approve writes within git working dir)

        // 11. fallback-ask
        PermissionDecision::Ask
    }

    pub fn record_session_approval(&mut self, tool_name: &str, input: &serde_json::Value) {
        let key = format!("{}:{}", tool_name, approval_pattern(tool_name, input));
        if !self.session_approved.contains(&key) {
            self.session_approved.push(key);
        }
    }

    pub fn set_mode(&mut self, mode: PermissionMode) {
        self.mode = mode;
    }
}

fn plan_mode_guard(
    tool_name: &str,
    input: &serde_json::Value,
    working_dir: &Path,
    plan_file: Option<&Path>,
) -> Option<PermissionDecision> {
    if tool_name == "Write" || tool_name == "Edit" {
        let Some(plan_path) = plan_file else {
            return Some(PermissionDecision::Deny(
                "Plan mode is active. No plan file is set; refuse all writes. Call ExitPlanMode to exit.".into(),
            ));
        };
        if writes_only_plan_file(input, working_dir, plan_path) {
            return None; // allowed — continue normal permission chain
        }
        return Some(PermissionDecision::Deny(format!(
            "Plan mode is active. You may only write to the current plan file: {}. \
             Call ExitPlanMode to exit plan mode before editing other files.",
            plan_path.display()
        )));
    }
    None
}

fn writes_only_plan_file(
    input: &serde_json::Value,
    working_dir: &Path,
    plan_file: &Path,
) -> bool {
    let Some(raw) = input.get("path").and_then(|v| v.as_str()) else {
        return false;
    };
    let candidate = resolve_path(working_dir, raw);
    let plan = resolve_path(working_dir, &plan_file.to_string_lossy());
    paths_equal(&candidate, &plan)
}

fn resolve_path(working_dir: &Path, raw: &str) -> PathBuf {
    let p = PathBuf::from(raw);
    if p.is_absolute() {
        p
    } else {
        working_dir.join(p)
    }
}

fn paths_equal(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => {
            // Files may not exist yet (new plan file) — compare lexically after normalize
            normalize_lex(a) == normalize_lex(b)
        }
    }
}

fn normalize_lex(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn matches_pattern(pattern: &str, tool_name: &str, _input: &serde_json::Value) -> bool {
    if pattern == tool_name {
        return true;
    }
    if pattern.contains('(') {
        let paren_start = pattern.find('(').unwrap();
        let prefix = &pattern[..paren_start];
        if prefix == tool_name {
            return true;
        }
    }
    if pattern == "*" {
        return true;
    }
    // Glob-style: only `*` suffix / infix used for MCP allowlists (e.g. mcp__*).
    if pattern.contains('*') {
        return glob_match(pattern, tool_name);
    }
    false
}

fn glob_match(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == text;
    }
    let mut rest = text;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if i == 0 {
            if !rest.starts_with(part) {
                return false;
            }
            rest = &rest[part.len()..];
        } else if i == parts.len() - 1 && !pattern.ends_with('*') {
            if !rest.ends_with(part) {
                return false;
            }
            return true;
        } else {
            match rest.find(part) {
                Some(idx) => rest = &rest[idx + part.len()..],
                None => return false,
            }
        }
    }
    pattern.ends_with('*') || rest.is_empty()
}

fn approval_pattern(tool_name: &str, _input: &serde_json::Value) -> String {
    tool_name.to_string()
}

fn has_sensitive_file_access(tool_name: &str, input: &serde_json::Value) -> bool {
    let file_tools = ["Read", "Write", "Edit", "Bash"];
    if !file_tools.contains(&tool_name) {
        return false;
    }

    let path_str = input.get("path")
        .or_else(|| input.get("command"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    for pattern in SENSITIVE_PATTERNS {
        if path_str.contains(pattern) {
            return true;
        }
    }
    false
}

fn accesses_git_control_path(_tool_name: &str, input: &serde_json::Value) -> bool {
    let path_str = input.get("path")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    path_str.contains(".git/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_auto_mode_approves_all() {
        let chain = PermissionChain::new(PermissionMode::Auto, vec![]);
        let decision = chain.evaluate(
            "Write",
            &serde_json::json!({"path": "foo.rs"}),
            Path::new("."),
            false,
            None,
        );
        assert_eq!(decision, PermissionDecision::Approve);
    }

    #[test]
    fn test_auto_mode_denies_ask_user() {
        let chain = PermissionChain::new(PermissionMode::Auto, vec![]);
        let decision = chain.evaluate(
            "AskUserQuestion",
            &serde_json::json!({}),
            Path::new("."),
            false,
            None,
        );
        assert!(matches!(decision, PermissionDecision::Deny(_)));
    }

    #[test]
    fn test_yolo_approves_normal_tool() {
        let chain = PermissionChain::new(PermissionMode::Yolo, vec![]);
        let decision = chain.evaluate(
            "Write",
            &serde_json::json!({"path": "foo.rs"}),
            Path::new("."),
            false,
            None,
        );
        assert_eq!(decision, PermissionDecision::Approve);
    }

    #[test]
    fn test_yolo_asks_for_sensitive_file() {
        let chain = PermissionChain::new(PermissionMode::Yolo, vec![]);
        let decision = chain.evaluate(
            "Read",
            &serde_json::json!({"path": ".env"}),
            Path::new("."),
            false,
            None,
        );
        assert_eq!(decision, PermissionDecision::Ask);
    }

    #[test]
    fn test_manual_asks_for_write() {
        let chain = PermissionChain::new(PermissionMode::Manual, vec![]);
        let decision = chain.evaluate(
            "Write",
            &serde_json::json!({"path": "foo.rs"}),
            Path::new("."),
            false,
            None,
        );
        assert_eq!(decision, PermissionDecision::Ask);
    }

    #[test]
    fn test_manual_approves_read_only() {
        let chain = PermissionChain::new(PermissionMode::Manual, vec![]);
        let decision = chain.evaluate(
            "Read",
            &serde_json::json!({"path": "foo.rs"}),
            Path::new("."),
            false,
            None,
        );
        assert_eq!(decision, PermissionDecision::Approve);
    }

    #[test]
    fn test_user_deny_rule() {
        let rules = vec![PermissionRule {
            decision: "deny".into(),
            pattern: "Bash".into(),
            scope: None,
        }];
        let chain = PermissionChain::new(PermissionMode::Yolo, rules);
        let decision = chain.evaluate(
            "Bash",
            &serde_json::json!({"command": "rm -rf /"}),
            Path::new("."),
            false,
            None,
        );
        assert!(matches!(decision, PermissionDecision::Deny(_)));
    }

    #[test]
    fn test_plan_mode_denies_non_plan_write() {
        let chain = PermissionChain::new(PermissionMode::Yolo, vec![]);
        let plan = PathBuf::from("/tmp/ws/.kkagent/plans/test.md");
        let decision = chain.evaluate(
            "Write",
            &serde_json::json!({"path": "src/main.rs"}),
            Path::new("/tmp/ws"),
            true,
            Some(&plan),
        );
        assert!(matches!(decision, PermissionDecision::Deny(_)));
    }

    #[test]
    fn test_plan_mode_allows_plan_file_write() {
        let chain = PermissionChain::new(PermissionMode::Yolo, vec![]);
        let plan = PathBuf::from("/tmp/ws/.kkagent/plans/test.md");
        let decision = chain.evaluate(
            "Write",
            &serde_json::json!({"path": ".kkagent/plans/test.md"}),
            Path::new("/tmp/ws"),
            true,
            Some(&plan),
        );
        assert_eq!(decision, PermissionDecision::Approve);
    }

    #[test]
    fn test_plan_mode_blocks_write_even_in_auto() {
        let chain = PermissionChain::new(PermissionMode::Auto, vec![]);
        let plan = PathBuf::from("/tmp/ws/.kkagent/plans/test.md");
        let decision = chain.evaluate(
            "Edit",
            &serde_json::json!({"path": "foo.rs"}),
            Path::new("/tmp/ws"),
            true,
            Some(&plan),
        );
        assert!(matches!(decision, PermissionDecision::Deny(_)));
    }

    #[test]
    fn test_mcp_wildcard_allow_rule() {
        let chain = PermissionChain::new(
            PermissionMode::Manual,
            vec![PermissionRule {
                decision: "allow".into(),
                pattern: "mcp__*".into(),
                scope: None,
            }],
        );
        let decision = chain.evaluate(
            "mcp__github__search_issues",
            &serde_json::json!({}),
            Path::new("/tmp/ws"),
            false,
            None,
        );
        assert_eq!(decision, PermissionDecision::Approve);
    }
}
