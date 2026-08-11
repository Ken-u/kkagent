//! Cached git status badge for the TUI footer (kimi-code chrome aligned).

use std::path::Path;
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

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
}

struct Cache {
    at: Instant,
    cwd: String,
    badge: GitBadge,
}

static CACHE: Mutex<Option<Cache>> = Mutex::new(None);

/// Refresh at most every 2s per cwd.
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
    if let Ok(guard) = CACHE.lock() {
        if let Some(c) = guard.as_ref() {
            if c.cwd == key && c.at.elapsed() < Duration::from_secs(2) {
                return c.badge.clone();
            }
        }
    }
    let badge = probe(cwd, trust);
    if let Ok(mut guard) = CACHE.lock() {
        *guard = Some(Cache {
            at: Instant::now(),
            cwd: key,
            badge: badge.clone(),
        });
    }
    badge
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
}
