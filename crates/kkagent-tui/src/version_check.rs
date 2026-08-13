//! Optional idle version check with disk cache and rate limit.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const CACHE_HOURS: u64 = 24;
const ERROR_RETRY_HOURS: u64 = 1;
const LATEST_RELEASE_API: &str = "https://api.github.com/repos/Ken-u/kkagent/releases/latest";

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct LatestRelease {
    #[serde(rename = "tag_name")]
    pub version: String,
    #[serde(rename = "html_url")]
    pub release_url: String,
    #[serde(default)]
    pub body: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct VersionCache {
    pub checked_at_unix: u64,
    #[serde(default)]
    pub source: Option<String>,
    pub latest: Option<String>,
    pub note: Option<String>,
    #[serde(default)]
    pub release_url: Option<String>,
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
    if cache.source.as_deref() != Some(LATEST_RELEASE_API) {
        return None;
    }
    let latest = cache.latest.as_deref()?;
    if latest != current && version_newer(latest, current) {
        Some(update_hint(current, latest, cache.release_url.as_deref()))
    } else {
        None
    }
}

pub fn cache_is_stale() -> bool {
    let cache = load_cache();
    if cache.checked_at_unix == 0 || cache.source.as_deref() != Some(LATEST_RELEASE_API) {
        return true;
    }
    let max_age = if cache.error.is_some() {
        ERROR_RETRY_HOURS
    } else {
        CACHE_HOURS
    };
    now_unix().saturating_sub(cache.checked_at_unix) > max_age * 3600
}

pub async fn fetch_latest() -> anyhow::Result<LatestRelease> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;
    let release = client
        .get(LATEST_RELEASE_API)
        .header(
            reqwest::header::USER_AGENT,
            concat!("kkagent/", env!("CARGO_PKG_VERSION")),
        )
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await?
        .error_for_status()?
        .json::<LatestRelease>()
        .await?;
    if release.version.trim().is_empty() || release.release_url.trim().is_empty() {
        anyhow::bail!("latest release response is missing tag_name or html_url");
    }
    Ok(release)
}

pub fn newer_release_hint(current: &str, release: &LatestRelease) -> Option<String> {
    version_newer(&release.version, current)
        .then(|| update_hint(current, &release.version, Some(&release.release_url)))
}

pub fn release_is_cached(release: &LatestRelease) -> bool {
    let cache = load_cache();
    cache.source.as_deref() == Some(LATEST_RELEASE_API)
        && cache.latest.as_deref() == Some(release.version.as_str())
}

fn update_hint(current: &str, latest: &str, release_url: Option<&str>) -> String {
    let suffix = release_url
        .filter(|url| !url.is_empty())
        .map(|url| format!(" — {url}"))
        .unwrap_or_default();
    let updater = if cfg!(windows) {
        "kkagent-update.ps1"
    } else {
        "kkagent-update"
    };
    format!("update available: {latest} (current {current}); run {updater} to upgrade{suffix}")
}

/// Record a freshly fetched latest version.
pub fn record_latest(release: &LatestRelease) {
    let cache = VersionCache {
        checked_at_unix: now_unix(),
        source: Some(LATEST_RELEASE_API.into()),
        latest: Some(release.version.clone()),
        note: (!release.body.is_empty()).then(|| release.body.clone()),
        release_url: Some(release.release_url.clone()),
        error: None,
    };
    save_cache(&cache);
}

pub fn record_check_error(err: &str) {
    let mut cache = load_cache();
    if cache.source.as_deref() != Some(LATEST_RELEASE_API) {
        cache.latest = None;
        cache.note = None;
        cache.release_url = None;
    }
    cache.checked_at_unix = now_unix();
    cache.source = Some(LATEST_RELEASE_API.into());
    cache.error = Some(err.to_string());
    save_cache(&cache);
}

fn version_newer(a: &str, b: &str) -> bool {
    let parse = |value: &str| semver::Version::parse(value.trim().trim_start_matches('v'));
    matches!((parse(a), parse(b)), (Ok(latest), Ok(current)) if latest > current)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_semver() {
        assert!(version_newer("0.2.0", "0.1.9"));
        assert!(version_newer("v1.0.1", "1.0.0"));
        assert!(version_newer("1.0.0", "1.0.0-beta.1"));
        assert!(!version_newer("0.1.0", "0.1.0"));
        assert!(!version_newer("not-a-version", "0.1.0"));
    }

    #[test]
    fn builds_an_actionable_release_hint() {
        let release = LatestRelease {
            version: "v0.3.0".into(),
            release_url: "https://github.com/example/releases/tag/v0.3.0".into(),
            body: String::new(),
        };
        let hint = newer_release_hint("0.2.0", &release).unwrap();
        assert!(hint.contains(if cfg!(windows) {
            "run kkagent-update.ps1"
        } else {
            "run kkagent-update"
        }));
        assert!(hint.contains(&release.release_url));
        assert!(newer_release_hint("0.3.0", &release).is_none());
    }
}
