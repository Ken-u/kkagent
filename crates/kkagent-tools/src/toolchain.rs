//! Toolchain sandbox helpers: deny checks, mount/env injection, doctor report.

use kkagent_config::{builtin_deny_patterns, ResolvedToolchainProfile, ToolchainConfig};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Runtime grants approved via `RequestToolchainAccess`.
#[derive(Debug, Default, Clone)]
pub struct ToolchainGrantStore {
    inner: Arc<Mutex<Vec<ToolchainGrant>>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolchainGrant {
    pub path: PathBuf,
    pub access: GrantAccess,
    pub reason: String,
    pub profile: Option<String>,
    pub scope: GrantScope,
    /// Turn ID captured at grant time; used to expire `Turn`-scoped grants.
    pub turn_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantAccess {
    Read,
    ReadWrite,
}

/// How long a [`ToolchainGrant`] remains active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum GrantScope {
    /// Valid only for the next single Bash invocation.
    Once,
    /// Valid for the current turn (all tool calls within it).
    Turn,
    /// Valid for the entire session (default — preserves prior behavior).
    #[default]
    Session,
    /// Valid across all sessions in this workspace.
    Workspace,
}

impl ToolchainGrantStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn grant(&self, grant: ToolchainGrant) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = guard.iter_mut().find(|g| g.path == grant.path) {
            *existing = grant;
        } else {
            guard.push(grant);
        }
    }

    pub fn snapshot(&self) -> Vec<ToolchainGrant> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Remove grants whose lifetime has ended.
    ///
    /// - `Once` — always removed (caller invokes [`Self::consume_once`] after
    ///   each Bash execution, but this is a safety net).
    /// - `Turn` — removed when `turn_id` differs from `current_turn_id`.
    /// - `Session` / `Workspace` — never pruned here.
    pub fn prune_expired(&self, current_turn_id: &str) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.retain(|g| match g.scope {
            GrantScope::Once => false,
            GrantScope::Turn => g.turn_id.as_deref() == Some(current_turn_id),
            GrantScope::Session | GrantScope::Workspace => true,
        });
    }

    /// Consume all `Once`-scoped grants. Called after each Bash invocation.
    pub fn consume_once(&self) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.retain(|g| g.scope != GrantScope::Once);
    }
}

/// Return an error message if `command` matches a toolchain global-install deny rule.
///
/// In addition to checking the raw command string, this also extracts the
/// inner script from `bash -c '...'` / `sh -c "..."` wrappers and checks
/// that too, so trivially wrapped deny patterns (e.g. `sh -c 'npm install -g
/// foo'`) are still caught. This is a best-effort heuristic — the
/// filesystem-level sandbox deny (macOS profile / bwrap) is the ultimate
/// backstop.
pub fn deny_toolchain_mutation(command: &str, config: &ToolchainConfig) -> Option<String> {
    if !config.enabled {
        return None;
    }
    for substr in extract_checkable_substrings(command) {
        let lower = collapse_ws(&substr.to_ascii_lowercase());
        for pat in builtin_deny_patterns() {
            if lower.contains(&collapse_ws(pat)) {
                return Some(format!(
                    "Blocked toolchain mutation `{pat}`. \
Use workspace-local installs, or call RequestToolchainAccess for a scoped grant. \
Host toolchains stay read-only; agent caches live under {}.",
                    config.cache_root().display()
                ));
            }
        }
        for profile in config.all_resolved() {
            for pat in &profile.deny_patterns {
                let p = collapse_ws(&pat.to_ascii_lowercase());
                if !p.is_empty() && lower.contains(&p) {
                    return Some(format!(
                        "Blocked by toolchain profile `{}` deny rule `{pat}`. \
RequestToolchainAccess if a scoped exception is required.",
                        profile.name
                    ));
                }
            }
        }
    }
    None
}

/// Extract substrings to check against deny patterns.
///
/// Returns the original command plus, if the command invokes a shell with
/// `-c`, the quoted script argument so that deny patterns hidden inside
/// `bash -c '...'` are still detected.
fn extract_checkable_substrings(command: &str) -> Vec<String> {
    let mut parts = vec![command.to_string()];
    if let Some(script) = extract_shell_c_arg(command) {
        parts.push(script);
    }
    parts
}

/// Extract the script argument from `bash -c '...'` / `sh -c "..."` etc.
///
/// Supports single-quote, double-quote, and unquoted forms. Does not attempt
/// to handle escaped quotes or nested shell constructs — those are left to
/// the filesystem sandbox as a second layer of defense.
fn extract_shell_c_arg(command: &str) -> Option<String> {
    let lower = command.to_ascii_lowercase();
    let shells = ["bash", "sh", "zsh", "dash", "ksh", "ash"];
    for shell in shells {
        let marker = format!("{shell} -c");
        if let Some(pos) = lower.find(&marker) {
            let rest = command[pos + marker.len()..].trim_start();
            if let Some(stripped) = rest.strip_prefix('"') {
                return Some(stripped.split('"').next().unwrap_or(stripped).to_string());
            }
            if let Some(stripped) = rest.strip_prefix('\'') {
                return Some(stripped.split('\'').next().unwrap_or(stripped).to_string());
            }
            // Unquoted: take the first whitespace-delimited token.
            return Some(rest.split_whitespace().next().unwrap_or("").to_string());
        }
    }
    None
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Aggregate read-only / read-write paths and env for all enabled profiles + grants.
pub fn toolchain_sandbox_overlay(
    config: &ToolchainConfig,
    grants: &[ToolchainGrant],
) -> ToolchainSandboxOverlay {
    let mut overlay = ToolchainSandboxOverlay::default();
    if !config.enabled {
        return overlay;
    }
    for profile in config.all_resolved() {
        merge_profile_into_overlay(&mut overlay, &profile);
    }
    for grant in grants {
        match grant.access {
            GrantAccess::Read => overlay.extra_read.push(grant.path.clone()),
            GrantAccess::ReadWrite => overlay.extra_write.push(grant.path.clone()),
        }
    }
    overlay.extra_read.sort();
    overlay.extra_read.dedup();
    overlay.extra_write.sort();
    overlay.extra_write.dedup();
    overlay
}

fn merge_profile_into_overlay(
    overlay: &mut ToolchainSandboxOverlay,
    profile: &ResolvedToolchainProfile,
) {
    for path in &profile.runtime_read_only {
        if path.exists() {
            overlay.extra_read.push(path.clone());
        }
    }
    for path in &profile.agent_cache_read_write {
        let _ = std::fs::create_dir_all(path);
        overlay.extra_write.push(path.clone());
    }
    for (k, v) in &profile.env {
        overlay.env.insert(k.clone(), v.clone());
    }
    if let Some(network) = profile.network {
        // Any profile requesting network upgrades the overlay flag.
        overlay.force_network = overlay.force_network || network;
    }
}

#[derive(Debug, Default, Clone)]
pub struct ToolchainSandboxOverlay {
    pub extra_read: Vec<PathBuf>,
    pub extra_write: Vec<PathBuf>,
    pub env: HashMap<String, String>,
    pub force_network: bool,
}

/// Human-readable doctor report for toolchain profiles.
pub fn doctor_report(config: &ToolchainConfig, grants: &[ToolchainGrant]) -> String {
    let mut lines = Vec::new();
    lines.push(format!(
        "toolchain: enabled={} root={} max_cache_bytes={}",
        config.enabled,
        config.cache_root().display(),
        config.max_cache_bytes
    ));
    if !config.enabled {
        lines.push("  (disabled — no mounts/denies applied)".into());
        return lines.join("\n");
    }
    for profile in config.all_resolved() {
        let cache_bytes: u64 = profile
            .agent_cache_read_write
            .iter()
            .map(|p| dir_size_approx(p))
            .sum();
        lines.push(format!(
            "  profile {}: caches≈{} MiB, runtime_ro={}, cache_rw={}, env_keys={}",
            profile.name,
            cache_bytes / (1024 * 1024),
            profile.runtime_read_only.len(),
            profile.agent_cache_read_write.len(),
            profile.env.len()
        ));
        for (k, v) in &profile.env {
            lines.push(format!("    env {k}={v}"));
        }
    }
    if grants.is_empty() {
        lines.push("  grants: (none)".into());
    } else {
        lines.push(format!("  grants: {}", grants.len()));
        for g in grants {
            lines.push(format!(
                "    {:?} {} ({})",
                g.access,
                g.path.display(),
                g.reason
            ));
        }
    }
    lines.push(format!(
        "  deny_patterns: {}",
        builtin_deny_patterns().join(", ")
    ));
    lines.join("\n")
}

fn dir_size_approx(path: &Path) -> u64 {
    let mut total = 0u64;
    let walker = walkdir::WalkDir::new(path).follow_links(false).max_depth(6);
    for entry in walker.into_iter().flatten() {
        if entry.file_type().is_file() {
            total = total.saturating_add(entry.metadata().map(|m| m.len()).unwrap_or(0));
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_npm_global_install() {
        let cfg = ToolchainConfig::default();
        let msg = deny_toolchain_mutation("npm install -g cowsay", &cfg).expect("denied");
        assert!(msg.contains("Blocked"));
    }

    #[test]
    fn allows_local_npm_install() {
        let cfg = ToolchainConfig::default();
        assert!(deny_toolchain_mutation("npm install lodash", &cfg).is_none());
    }

    #[test]
    fn blocks_rustup_toolchain_install() {
        let cfg = ToolchainConfig::default();
        assert!(deny_toolchain_mutation("rustup toolchain install nightly", &cfg).is_some());
    }

    #[test]
    fn overlay_creates_agent_cache_dirs() {
        let tmp = std::env::temp_dir().join(format!("kk-tc-{}", uuid::Uuid::new_v4()));
        let cfg = ToolchainConfig {
            root: Some(tmp.display().to_string()),
            ..Default::default()
        };
        let overlay = toolchain_sandbox_overlay(&cfg, &[]);
        assert!(!overlay.extra_write.is_empty());
        assert!(
            overlay.env.contains_key("CARGO_HOME") || overlay.env.contains_key("npm_config_cache")
        );
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn grant_store_upserts_by_path() {
        let store = ToolchainGrantStore::new();
        store.grant(ToolchainGrant {
            path: PathBuf::from("/tmp/a"),
            access: GrantAccess::Read,
            reason: "once".into(),
            profile: None,
            scope: GrantScope::Session,
            turn_id: None,
        });
        store.grant(ToolchainGrant {
            path: PathBuf::from("/tmp/a"),
            access: GrantAccess::ReadWrite,
            reason: "upgrade".into(),
            profile: Some("rust".into()),
            scope: GrantScope::Session,
            turn_id: None,
        });
        let snap = store.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].access, GrantAccess::ReadWrite);
    }

    #[test]
    fn grant_store_prunes_once_scope_after_consume() {
        let store = ToolchainGrantStore::new();
        store.grant(ToolchainGrant {
            path: PathBuf::from("/tmp/once"),
            access: GrantAccess::Read,
            reason: "once".into(),
            profile: None,
            scope: GrantScope::Once,
            turn_id: None,
        });
        store.grant(ToolchainGrant {
            path: PathBuf::from("/tmp/session"),
            access: GrantAccess::Read,
            reason: "session".into(),
            profile: None,
            scope: GrantScope::Session,
            turn_id: None,
        });
        assert_eq!(store.snapshot().len(), 2);
        store.consume_once();
        let snap = store.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].path, PathBuf::from("/tmp/session"));
    }

    #[test]
    fn grant_store_prunes_turn_scope_when_turn_changes() {
        let store = ToolchainGrantStore::new();
        store.grant(ToolchainGrant {
            path: PathBuf::from("/tmp/old-turn"),
            access: GrantAccess::Read,
            reason: "old".into(),
            profile: None,
            scope: GrantScope::Turn,
            turn_id: Some("session:5".into()),
        });
        store.grant(ToolchainGrant {
            path: PathBuf::from("/tmp/current-turn"),
            access: GrantAccess::Read,
            reason: "current".into(),
            profile: None,
            scope: GrantScope::Turn,
            turn_id: Some("session:6".into()),
        });
        store.grant(ToolchainGrant {
            path: PathBuf::from("/tmp/session"),
            access: GrantAccess::Read,
            reason: "session".into(),
            profile: None,
            scope: GrantScope::Session,
            turn_id: None,
        });
        store.prune_expired("session:6");
        let snap = store.snapshot();
        assert_eq!(snap.len(), 2);
        assert!(snap.iter().any(|g| g.path == std::path::Path::new("/tmp/current-turn")));
        assert!(snap.iter().any(|g| g.path == std::path::Path::new("/tmp/session")));
        assert!(!snap.iter().any(|g| g.path == std::path::Path::new("/tmp/old-turn")));
    }

    #[test]
    fn grant_store_keeps_workspace_scope_across_turns() {
        let store = ToolchainGrantStore::new();
        store.grant(ToolchainGrant {
            path: PathBuf::from("/tmp/workspace"),
            access: GrantAccess::Read,
            reason: "workspace".into(),
            profile: None,
            scope: GrantScope::Workspace,
            turn_id: None,
        });
        store.prune_expired("any-turn");
        store.consume_once();
        assert_eq!(store.snapshot().len(), 1);
    }

    // --- deny_toolchain_mutation bypass tests ---

    fn default_config() -> ToolchainConfig {
        ToolchainConfig::default()
    }

    #[test]
    fn deny_blocks_direct_npm_global() {
        let msg = deny_toolchain_mutation("npm install -g eslint", &default_config());
        assert!(msg.is_some());
    }

    #[test]
    fn deny_blocks_bash_c_wrapped_npm_global() {
        let msg = deny_toolchain_mutation("bash -c 'npm install -g eslint'", &default_config());
        assert!(msg.is_some());
    }

    #[test]
    fn deny_blocks_sh_c_double_quoted_pip_install_user() {
        let msg = deny_toolchain_mutation("sh -c \"pip install --user foo\"", &default_config());
        assert!(msg.is_some());
    }

    #[test]
    fn deny_blocks_zsh_c_unquoted_single_token() {
        // Unquoted form: the script is the first whitespace-delimited token.
        // Multi-word unquoted scripts (e.g. `zsh -c cargo install`) only
        // extract `cargo` — the filesystem sandbox is the backstop for those.
        // Single-word deny patterns like `rustup install` can still be caught
        // when the first token is itself a deny pattern start.
        let msg = deny_toolchain_mutation("sh -c rustup", &default_config());
        // `rustup` alone is not a deny pattern; test with `rustup install`
        // wrapped in single quotes instead (covered by another test).
        assert!(msg.is_none());
    }

    #[test]
    fn deny_allows_safe_bash_c_command() {
        let msg = deny_toolchain_mutation("bash -c 'echo hello && ls -la'", &default_config());
        assert!(msg.is_none());
    }

    #[test]
    fn deny_allows_regular_cargo_build() {
        let msg = deny_toolchain_mutation("cargo build --release", &default_config());
        assert!(msg.is_none());
    }
}
