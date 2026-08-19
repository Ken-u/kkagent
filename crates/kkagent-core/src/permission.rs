use kkagent_config::PermissionRule;
use kkagent_protocol::PermissionMode;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, PartialEq)]
pub enum PermissionDecision {
    Approve,
    Ask,
    Deny(String),
}

pub struct PermissionChain {
    /// Live mode handle — shared with the session so `/permission` takes effect mid-turn.
    pub mode: Arc<Mutex<PermissionMode>>,
    pub rules: Vec<PermissionRule>,
    /// Whether `record_always_approval` writes through to the sidecar file.
    /// Disabled in unit tests and headless one-shots to avoid mutating the
    /// developer's real `~/.kkagent/permissions.toml`.
    pub persist: bool,
    pub session_approved: Vec<String>,
    /// Approvals that expire at the end of the current turn.
    pub turn_approved: Vec<String>,
}

const SENSITIVE_PATTERNS: &[&str] = &[
    ".env",
    "id_rsa",
    "id_ed25519",
    "id_ecdsa",
    ".pem",
    "credentials",
    "secret",
    ".key",
    ".aws/credentials",
    ".aws/config",
    ".gcp/credentials",
    ".kube/config",
    ".docker/config.json",
    ".config/gcloud/credentials",
    ".netrc",
    ".npmrc",
];

/// Default-approve set aligned with kimi `default-tool-approve`
/// (Agent included; Skill is write-ish and requires ask). TaskOutput stop
/// and Cron/Goal mutations are gated by their own ask rules.
const READ_ONLY_TOOLS: &[&str] = &[
    "Read",
    "Grep",
    "Glob",
    "ReadMediaFile",
    "Web",
    "EnterPlanMode",
    "ExitPlanMode",
    "TodoList",
    "TaskOutput",
    "SelectTools",
    "Agent",
];

impl PermissionChain {
    pub fn new(mode: PermissionMode, rules: Vec<PermissionRule>) -> Self {
        Self::with_shared_mode(Arc::new(Mutex::new(mode)), rules)
    }

    pub fn with_shared_mode(mode: Arc<Mutex<PermissionMode>>, rules: Vec<PermissionRule>) -> Self {
        // `cargo test` must stay hermetic: neither load from nor write to the
        // developer's real ~/.kkagent/permissions.toml.
        let in_tests = cfg!(test);
        let mut chain = Self {
            mode,
            rules,
            persist: !in_tests,
            session_approved: Vec::new(),
            turn_approved: Vec::new(),
        };
        if !in_tests {
            chain.load_persisted_approvals();
        }
        chain
    }

    /// Tests / one-shot runs: never touch the sidecar file.
    pub fn without_persistence(mut self) -> Self {
        self.persist = false;
        self
    }

    pub fn current_mode(&self) -> PermissionMode {
        *self.mode.lock().unwrap_or_else(|e| e.into_inner())
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
        self.evaluate_sourced(tool_name, input, working_dir, plan_mode, plan_file)
            .0
    }

    /// Same as [`Self::evaluate`] but reports which chain step decided, for
    /// the audit trail.
    pub fn evaluate_sourced(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        working_dir: &Path,
        plan_mode: bool,
        plan_file: Option<&Path>,
    ) -> (PermissionDecision, &'static str) {
        let mode = self.current_mode();

        // WritePlan is a host-scoped plan primitive: it never accepts a path
        // and is available without approval only while plan mode is active.
        if tool_name == "WritePlan" {
            return if plan_mode && plan_file.is_some() {
                (PermissionDecision::Approve, "write-plan-plan-mode")
            } else {
                (
                    PermissionDecision::Deny(
                        "WritePlan is only available while plan mode is active.".into(),
                    ),
                    "write-plan-plan-mode",
                )
            };
        }

        // 0. plan-mode-guard-deny (must run before auto/yolo approve)
        if plan_mode {
            if let Some(deny) = plan_mode_guard(tool_name, input, working_dir, plan_file) {
                return (deny, "plan-mode-guard");
            }
        }

        // 1. auto-mode-ask-user-question-deny
        if mode == PermissionMode::Auto && tool_name == "AskUserQuestion" {
            return (
                PermissionDecision::Deny(
                    "AskUserQuestion is disabled in auto mode. Make a decision and continue."
                        .into(),
                ),
                "auto-ask-user-deny",
            );
        }

        // AskUserQuestion is the user interaction itself — never gate behind another approval.
        if tool_name == "AskUserQuestion" {
            return (PermissionDecision::Approve, "ask-user-question");
        }

        // 2. user-configured-deny
        for rule in &self.rules {
            if rule.decision == "deny" && matches_pattern(&rule.pattern, tool_name, input) {
                return (
                    PermissionDecision::Deny(format!("Denied by rule: {}", rule.pattern)),
                    "user-deny-rule",
                );
            }
        }

        // 3. auto-mode-approve
        if mode == PermissionMode::Auto {
            return (PermissionDecision::Approve, "auto-mode-approve");
        }

        // 3b. exit-plan-mode-review-ask (kimi): always ask in manual/yolo
        if tool_name == "ExitPlanMode" {
            return (PermissionDecision::Ask, "exit-plan-mode-review");
        }

        // 4. session-approval-history (session + turn scoped)
        let approval_key = format!("{}:{}", tool_name, approval_pattern(tool_name, input));
        if self.session_approved.contains(&approval_key)
            || self.turn_approved.contains(&approval_key)
        {
            return (PermissionDecision::Approve, "session-approval-history");
        }

        // 5. user-configured-ask / user-configured-allow
        for rule in &self.rules {
            if rule.decision == "allow" && matches_pattern(&rule.pattern, tool_name, input) {
                return (PermissionDecision::Approve, "user-allow-rule");
            }
            if rule.decision == "ask" && matches_pattern(&rule.pattern, tool_name, input) {
                return (PermissionDecision::Ask, "user-ask-rule");
            }
        }

        // 6. sensitive-file-access-ask
        if has_sensitive_file_access(tool_name, input) {
            return (PermissionDecision::Ask, "sensitive-file-access");
        }

        // 7. git-control-path-access-ask
        if accesses_git_control_path(tool_name, input) {
            return (PermissionDecision::Ask, "git-control-path");
        }

        // 8. yolo-mode-approve
        if mode == PermissionMode::Yolo {
            return (PermissionDecision::Approve, "yolo-mode-approve");
        }

        // 9. default-tool-approve (read-only tools)
        if READ_ONLY_TOOLS.contains(&tool_name) {
            return (PermissionDecision::Approve, "read-only-tool");
        }

        // 10. git-cwd-write-approve
        // (simplified: approve writes within git working dir)

        // 11. fallback-ask
        (PermissionDecision::Ask, "fallback-ask")
    }

    pub fn record_session_approval(&mut self, tool_name: &str, input: &serde_json::Value) {
        let key = format!("{}:{}", tool_name, approval_pattern(tool_name, input));
        if !self.session_approved.contains(&key) {
            self.session_approved.push(key);
        }
    }

    pub fn record_turn_approval(&mut self, tool_name: &str, input: &serde_json::Value) {
        let key = format!("{}:{}", tool_name, approval_pattern(tool_name, input));
        if !self.turn_approved.contains(&key) {
            self.turn_approved.push(key);
        }
    }

    pub fn clear_turn_approvals(&mut self) {
        self.turn_approved.clear();
    }

    /// Persist an allow rule for matching tool patterns (Always scope).
    ///
    /// The rule is applied to the in-memory chain immediately and — unless the
    /// chain was constructed with `without_persistence` — written to the
    /// `permissions.toml` sidecar so it survives restarts. Failures to persist
    /// are logged but do not undo the in-memory approval: the user already
    /// approved this action for the current session.
    pub fn record_always_approval(&mut self, tool_name: &str, input: &serde_json::Value) {
        // Patterns match the tool name (see matches_pattern); the historical
        // `"{tool}:{tool}"` form never matched anything after a restart.
        let pattern = approval_pattern(tool_name, input);
        let rule = PermissionRule {
            decision: "allow".into(),
            pattern: pattern.clone(),
            scope: Some("always".into()),
        };
        if !self
            .rules
            .iter()
            .any(|r| r.decision == "allow" && r.pattern == pattern)
        {
            self.rules.push(rule.clone());
        }
        if self.persist {
            let mut persisted = kkagent_config::PersistedApprovals::load().unwrap_or_default();
            persisted.upsert(rule);
            if let Err(error) = persisted.save() {
                tracing::warn!("could not persist always-approval: {error:#}");
            }
        }
        self.record_session_approval(tool_name, input);
    }

    /// Merge durable "always allow" rules from the permissions sidecar.
    pub fn load_persisted_approvals(&mut self) {
        match kkagent_config::PersistedApprovals::load() {
            Ok(persisted) => {
                for rule in persisted.rules {
                    if rule.decision == "allow"
                        && !self
                            .rules
                            .iter()
                            .any(|r| r.decision == rule.decision && r.pattern == rule.pattern)
                    {
                        self.rules.push(rule);
                    }
                }
            }
            Err(error) => tracing::warn!("could not load persisted approvals: {error:#}"),
        }
    }

    pub fn set_mode(&self, mode: PermissionMode) {
        *self.mode.lock().unwrap_or_else(|e| e.into_inner()) = mode;
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
                "Plan mode is active. Normal file writes are disabled; use WritePlan for the plan document or ExitPlanMode to exit.".into(),
            ));
        };
        if writes_only_plan_file(input, working_dir, plan_path) {
            return None; // allowed — continue normal permission chain
        }
        return Some(PermissionDecision::Deny(format!(
            "Plan mode is active. Write/Edit cannot modify `{}`. Use WritePlan for the \
             host-managed plan document, or call ExitPlanMode before editing project files.",
            input
                .get("path")
                .and_then(|value| value.as_str())
                .unwrap_or("this path")
        )));
    }
    None
}

fn writes_only_plan_file(input: &serde_json::Value, working_dir: &Path, plan_file: &Path) -> bool {
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
    // ReadMediaFile reads file contents just like Read — same sensitive-path
    // gate applies.
    let file_tools = ["Read", "ReadMediaFile", "Write", "Edit", "Bash"];
    if !file_tools.contains(&tool_name) {
        return false;
    }

    let path_str = input
        .get("path")
        .or_else(|| input.get("command"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    // Prefer path_policy for file tools (handles .env.example exemption).
    if let Some(path) = input.get("path").and_then(|v| v.as_str()) {
        if !kkagent_tools::path_policy::is_sensitive_path(std::path::Path::new(path)) {
            // Still scan bash commands for credential tokens below when tool is Bash.
            if tool_name != "Bash" {
                return false;
            }
        } else {
            return true;
        }
    }

    for pattern in SENSITIVE_PATTERNS {
        if path_str.contains(pattern) {
            // Allow common non-secret templates in command strings too.
            if path_str.contains(".env.example")
                || path_str.contains(".env.sample")
                || path_str.contains(".env.template")
            {
                continue;
            }
            // S0-2: public key files are not secrets
            if path_str.contains(".pub") {
                continue;
            }
            return true;
        }
    }
    false
}

fn accesses_git_control_path(_tool_name: &str, input: &serde_json::Value) -> bool {
    let path_str = input.get("path").and_then(|v| v.as_str()).unwrap_or("");
    path_str.contains(".git/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_auto_mode_approves_all() {
        let chain = PermissionChain::new(PermissionMode::Auto, vec![]);
        assert_eq!(
            chain.evaluate(
                "Write",
                &serde_json::json!({"path": "foo.rs"}),
                Path::new("."),
                false,
                None,
            ),
            PermissionDecision::Approve
        );
        // Mid-turn switch to manual must take effect on the next evaluate.
        chain.set_mode(PermissionMode::Manual);
        assert_eq!(
            chain.evaluate(
                "Write",
                &serde_json::json!({"path": "foo.rs"}),
                Path::new("."),
                false,
                None,
            ),
            PermissionDecision::Ask
        );
        assert_eq!(
            chain.evaluate(
                "Edit",
                &serde_json::json!({"path": "foo.rs"}),
                Path::new("."),
                false,
                None,
            ),
            PermissionDecision::Ask
        );
    }

    #[test]
    fn test_shared_mode_handle_updates_live() {
        let mode = Arc::new(Mutex::new(PermissionMode::Auto));
        let chain = PermissionChain::with_shared_mode(mode.clone(), vec![]);
        assert_eq!(
            chain.evaluate(
                "Write",
                &serde_json::json!({"path": "a.rs"}),
                Path::new("."),
                false,
                None,
            ),
            PermissionDecision::Approve
        );
        *mode.lock().unwrap() = PermissionMode::Manual;
        assert_eq!(
            chain.evaluate(
                "Write",
                &serde_json::json!({"path": "a.rs"}),
                Path::new("."),
                false,
                None,
            ),
            PermissionDecision::Ask
        );
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
    fn write_plan_is_scoped_to_plan_mode_and_never_prompts() {
        for mode in [
            PermissionMode::Auto,
            PermissionMode::Yolo,
            PermissionMode::Manual,
        ] {
            let chain = PermissionChain::new(mode, vec![]);
            assert_eq!(
                chain.evaluate(
                    "WritePlan",
                    &serde_json::json!({"content": "# Plan"}),
                    Path::new("/tmp/ws"),
                    true,
                    Some(Path::new("/tmp/session/plans/plan.md")),
                ),
                PermissionDecision::Approve
            );
            assert!(matches!(
                chain.evaluate(
                    "WritePlan",
                    &serde_json::json!({"content": "# Plan"}),
                    Path::new("/tmp/ws"),
                    false,
                    Some(Path::new("/tmp/session/plans/plan.md")),
                ),
                PermissionDecision::Deny(_)
            ));
        }
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
    fn exit_plan_mode_asks_in_yolo_and_manual() {
        for mode in [PermissionMode::Manual, PermissionMode::Yolo] {
            let chain = PermissionChain::new(mode, vec![]);
            let decision = chain.evaluate(
                "ExitPlanMode",
                &serde_json::json!({}),
                Path::new("/tmp/ws"),
                true,
                Some(Path::new("/tmp/ws/.kkagent/plans/x.md")),
            );
            assert_eq!(decision, PermissionDecision::Ask);
        }
        let auto = PermissionChain::new(PermissionMode::Auto, vec![]);
        let decision = auto.evaluate(
            "ExitPlanMode",
            &serde_json::json!({}),
            Path::new("/tmp/ws"),
            true,
            Some(Path::new("/tmp/ws/.kkagent/plans/x.md")),
        );
        assert_eq!(decision, PermissionDecision::Approve);
    }

    /// Cross-semantic decision matrix: mode × risk shape. Locks the kimi-v2
    /// chain ordering (user-deny > auto-approve > sensitive-ask > yolo-approve
    /// > read-only-approve > fallback-ask) against accidental reordering.
    #[test]
    fn permission_mode_risk_matrix() {
        fn verdict(mode: PermissionMode, tool: &str, input: serde_json::Value) -> &'static str {
            let chain = PermissionChain::new(mode, vec![]);
            match chain.evaluate(tool, &input, Path::new("/tmp/ws"), false, None) {
                PermissionDecision::Approve => "approve",
                PermissionDecision::Ask => "ask",
                PermissionDecision::Deny(_) => "deny",
            }
        }
        let normal = serde_json::json!({"path": "src/main.rs"});
        let sensitive = serde_json::json!({"path": "/Users/x/.ssh/id_rsa"});
        let git_control = serde_json::json!({"path": "/tmp/ws/.git/config"});

        // Write: normal file
        assert_eq!(
            verdict(PermissionMode::Manual, "Write", normal.clone()),
            "ask"
        );
        assert_eq!(
            verdict(PermissionMode::Yolo, "Write", normal.clone()),
            "approve"
        );
        assert_eq!(
            verdict(PermissionMode::Auto, "Write", normal.clone()),
            "approve"
        );

        // Write: sensitive file — yolo still asks (deliberate kimi ordering)
        assert_eq!(
            verdict(PermissionMode::Manual, "Write", sensitive.clone()),
            "ask"
        );
        assert_eq!(
            verdict(PermissionMode::Yolo, "Write", sensitive.clone()),
            "ask"
        );
        assert_eq!(
            verdict(PermissionMode::Auto, "Write", sensitive.clone()),
            "approve"
        );

        // Write: git control path — yolo still asks
        assert_eq!(
            verdict(PermissionMode::Manual, "Write", git_control.clone()),
            "ask"
        );
        assert_eq!(
            verdict(PermissionMode::Yolo, "Write", git_control.clone()),
            "ask"
        );
        assert_eq!(
            verdict(PermissionMode::Auto, "Write", git_control.clone()),
            "approve"
        );

        // Read: sensitive file hits the sensitive gate before read-only approve
        assert_eq!(
            verdict(PermissionMode::Manual, "Read", sensitive.clone()),
            "ask"
        );
        assert_eq!(
            verdict(PermissionMode::Yolo, "Read", sensitive.clone()),
            "ask"
        );
        assert_eq!(
            verdict(PermissionMode::Auto, "Read", sensitive.clone()),
            "approve"
        );

        // ReadMediaFile shares the Read gate
        assert_eq!(
            verdict(PermissionMode::Manual, "ReadMediaFile", sensitive.clone()),
            "ask"
        );

        // Read: normal file is always approved (read-only list)
        assert_eq!(
            verdict(PermissionMode::Manual, "Read", normal.clone()),
            "approve"
        );
        assert_eq!(
            verdict(PermissionMode::Yolo, "Read", normal.clone()),
            "approve"
        );

        // AskUserQuestion: denied in auto, allowed elsewhere
        let ask_q = serde_json::json!({"question": "q"});
        assert_eq!(
            verdict(PermissionMode::Auto, "AskUserQuestion", ask_q.clone()),
            "deny"
        );
        assert_eq!(
            verdict(PermissionMode::Manual, "AskUserQuestion", ask_q.clone()),
            "approve"
        );
        assert_eq!(
            verdict(PermissionMode::Yolo, "AskUserQuestion", ask_q.clone()),
            "approve"
        );
    }

    /// User deny rules outrank auto approve — the one hard backstop inside
    /// autonomous mode (kimi-v2 order: user-deny before auto-approve).
    #[test]
    fn user_rules_outrank_auto_approve() {
        let deny_rule = PermissionRule {
            decision: "deny".into(),
            pattern: "Bash".into(),
            scope: None,
        };
        let chain = PermissionChain::new(PermissionMode::Auto, vec![deny_rule]);
        match chain.evaluate(
            "Bash",
            &serde_json::json!({"command": "git push --force"}),
            Path::new("/tmp/ws"),
            false,
            None,
        ) {
            PermissionDecision::Deny(_) => {}
            other => panic!("auto must not override explicit user deny: {other:?}"),
        }
    }

    /// Regression: always-approval rules must match after a restart. The old
    /// `"{tool}:{tool}"` pattern never matched, silently reverting "always
    /// allow" to "ask" on every restart.
    #[test]
    fn always_approval_rule_matches_after_reload() {
        let mut chain = PermissionChain::new(PermissionMode::Manual, vec![]);
        chain.persist = false;
        chain.record_always_approval("Write", &serde_json::json!({"path": "a.rs"}));

        // Simulate a restart: rules round-trip through a fresh chain.
        let reloaded_rules = chain.rules.clone();
        let fresh = PermissionChain::new(PermissionMode::Manual, reloaded_rules);
        let decision = fresh.evaluate(
            "Write",
            &serde_json::json!({"path": "b.rs"}),
            Path::new("/tmp/ws"),
            false,
            None,
        );
        assert_eq!(decision, PermissionDecision::Approve);
    }
}
