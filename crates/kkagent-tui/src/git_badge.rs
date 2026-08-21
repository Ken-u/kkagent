//! Cached git status badge for the TUI footer (kimi-code chrome aligned).

use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const REFRESH_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Default)]
pub struct GitBadge {
    pub branch: Option<String>,
    pub dirty: bool,
    pub ahead: u32,
    pub behind: u32,
}

impl GitBadge {
    pub fn render(&self) -> Option<String> {
        let branch = self.branch.as_ref()?;
        let mut s = format!("git:{branch}");
        if self.dirty {
            s.push('*');
        }
        if self.ahead > 0 {
            s.push_str(&format!("↑{}", self.ahead));
        }
        if self.behind > 0 {
            s.push_str(&format!("↓{}", self.behind));
        }
        Some(s)
    }

    fn placeholder() -> Self {
        Self {
            branch: Some("…".into()),
            dirty: false,
            ahead: 0,
            behind: 0,
        }
    }
}

struct Cache {
    at: Instant,
    cwd: String,
    badge: GitBadge,
    refreshing: bool,
}

static CACHE: Mutex<Option<Cache>> = Mutex::new(None);
static BADGE_UPDATED: AtomicBool = AtomicBool::new(false);

/// True when a background probe finished since the last call (UI should redraw).
pub fn take_updated() -> bool {
    BADGE_UPDATED.swap(false, Ordering::Relaxed)
}

/// Return the cached badge and refresh stale Git metadata off the render thread.
pub fn git_badge(cwd: &Path, trust: Option<&kkagent_config::WorkspaceTrust>) -> GitBadge {
    let key = format!(
        "{}|{}|{}",
        cwd.to_string_lossy(),
        trust
            .and_then(|entry| entry.global_git_config_allowed)
            .unwrap_or(false),
        trust
            .map(|entry| entry.global_git_config_roots.join(";"))
            .unwrap_or_default()
    );
    if let Ok(mut guard) = CACHE.lock() {
        if let Some(cached) = guard.as_mut() {
            if cached.cwd == key {
                if cached.at.elapsed() < REFRESH_INTERVAL || cached.refreshing {
                    return cached.badge.clone();
                }
                cached.refreshing = true;
                let badge = cached.badge.clone();
                spawn_refresh(cwd.to_path_buf(), trust.cloned(), key);
                return badge;
            }
        }
        // Workspace switch / first probe: never block the UI thread on
        // `git status` (can take seconds on huge AOSP sub-repos).
        let badge = GitBadge::placeholder();
        *guard = Some(Cache {
            at: Instant::now(),
            cwd: key.clone(),
            badge: badge.clone(),
            refreshing: true,
        });
        spawn_refresh(cwd.to_path_buf(), trust.cloned(), key);
        return badge;
    }

    GitBadge::placeholder()
}

fn spawn_refresh(
    cwd: std::path::PathBuf,
    trust: Option<kkagent_config::WorkspaceTrust>,
    key: String,
) {
    std::thread::spawn(move || {
        let badge = probe(&cwd, trust.as_ref());
        if let Ok(mut guard) = CACHE.lock() {
            if let Some(cached) = guard.as_mut().filter(|cached| cached.cwd == key) {
                cached.at = Instant::now();
                cached.badge = badge;
                cached.refreshing = false;
                BADGE_UPDATED.store(true, Ordering::Relaxed);
            }
        }
    });
}

fn probe(cwd: &Path, trust: Option<&kkagent_config::WorkspaceTrust>) -> GitBadge {
    if !kkagent_config::git_metadata_accessible(trust) {
        return GitBadge::default();
    }
    let branch = Command::new("git")
        .args([
            "-C",
            &cwd.to_string_lossy(),
            "rev-parse",
            "--abbrev-ref",
            "HEAD",
        ])
        .envs(kkagent_config::git_environment(trust))
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty() && s != "HEAD");

    let Some(branch) = branch else {
        return GitBadge::default();
    };

    let dirty = Command::new("git")
        .args(["-C", &cwd.to_string_lossy(), "status", "--porcelain"])
        .envs(kkagent_config::git_environment(trust))
        .output()
        .ok()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);

    let mut ahead = 0u32;
    let mut behind = 0u32;
    if let Ok(out) = Command::new("git")
        .args([
            "-C",
            &cwd.to_string_lossy(),
            "rev-list",
            "--left-right",
            "--count",
            "@{upstream}...HEAD",
        ])
        .envs(kkagent_config::git_environment(trust))
        .output()
    {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout);
            let parts: Vec<_> = s.split_whitespace().collect();
            if parts.len() >= 2 {
                behind = parts[0].parse().unwrap_or(0);
                ahead = parts[1].parse().unwrap_or(0);
            }
        }
    }

    GitBadge {
        branch: Some(branch),
        dirty,
        ahead,
        behind,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_dirty() {
        let b = GitBadge {
            branch: Some("main".into()),
            dirty: true,
            ahead: 1,
            behind: 0,
        };
        assert_eq!(b.render().unwrap(), "git:main*↑1");
    }

    #[test]
    fn placeholder_renders_ellipsis() {
        assert_eq!(GitBadge::placeholder().render().unwrap(), "git:…");
    }
}
