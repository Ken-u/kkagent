//! Toolchain sandbox helpers: deny checks, mount/env injection, doctor report.

use kkagent_config::{builtin_deny_patterns, ResolvedToolchainProfile, ToolchainConfig};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Runtime grants approved via `RequestToolchainAccess` (session-scoped).
#[derive(Debug, Default, Clone)]
pub struct ToolchainGrantStore {
    inner: Arc<Mutex<Vec<ToolchainGrant>>>,
}

#[derive(Debug, Clone)]
pub struct ToolchainGrant {
    pub path: PathBuf,
    pub access: GrantAccess,
    pub reason: String,
    pub profile: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantAccess {
    Read,
    ReadWrite,
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
}

/// Return an error message if `command` matches a toolchain global-install deny rule.
pub fn deny_toolchain_mutation(command: &str, config: &ToolchainConfig) -> Option<String> {
    if !config.enabled {
        return None;
    }
    let lower = collapse_ws(&command.to_ascii_lowercase());
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
        });
        store.grant(ToolchainGrant {
            path: PathBuf::from("/tmp/a"),
            access: GrantAccess::ReadWrite,
            reason: "upgrade".into(),
            profile: Some("rust".into()),
        });
        let snap = store.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].access, GrantAccess::ReadWrite);
    }
}
