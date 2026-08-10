//! Optional idle version check with disk cache and rate limit.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const CACHE_HOURS: u64 = 24;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct VersionCache {
    pub checked_at_unix: u64,
    pub latest: Option<String>,
    pub note: Option<String>,
    pub error: Option<String>,
}

fn cache_path() -> PathBuf {
    kkagent_config::default_config_dir().join("version_check.json")
}

pub fn load_cache() -> VersionCache {
    let Ok(raw) = std::fs::read_to_string(cache_path()) else {
        return VersionCache::default();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

pub fn save_cache(cache: &VersionCache) {
    let path = cache_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(body) = serde_json::to_string_pretty(cache) {
        let _ = std::fs::write(path, body);
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Returns a one-line idle hint if a newer version may be available (cache only).
pub fn idle_hint(current: &str, enabled: bool) -> Option<String> {
    if !enabled {
        return None;
    }
    let cache = load_cache();
    let latest = cache.latest.as_deref()?;
    if latest != current && version_newer(latest, current) {
        Some(format!(
            "update available: {latest} (current {current}) — see release notes; not auto-downloaded"
        ))
    } else {
        None
    }
}

pub fn cache_is_stale() -> bool {
    let cache = load_cache();
    if cache.checked_at_unix == 0 {
        return true;
    }
    now_unix().saturating_sub(cache.checked_at_unix) > CACHE_HOURS * 3600
}

/// Record a freshly fetched latest version (caller does network I/O).
pub fn record_latest(latest: &str, note: &str) {
    let cache = VersionCache {
        checked_at_unix: now_unix(),
        latest: Some(latest.to_string()),
        note: Some(note.to_string()),
        error: None,
    };
    save_cache(&cache);
}

pub fn record_check_error(err: &str) {
    let mut cache = load_cache();
    cache.checked_at_unix = now_unix();
    cache.error = Some(err.to_string());
    save_cache(&cache);
}

fn version_newer(a: &str, b: &str) -> bool {
    parse_semver(a) > parse_semver(b)
}

fn parse_semver(s: &str) -> (u64, u64, u64) {
    let mut parts = s.trim().trim_start_matches('v').split('.');
    let major = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let minor = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let patch = parts
        .next()
        .and_then(|p| p.split('-').next())
        .and_then(|p| p.parse().ok())
        .unwrap_or(0);
    (major, minor, patch)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_semver() {
        assert!(version_newer("0.2.0", "0.1.9"));
        assert!(!version_newer("0.1.0", "0.1.0"));
    }
}
