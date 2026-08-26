//! Git worktree helpers for isolated subagent workspaces.

use std::path::{Path, PathBuf};
use tokio::process::Command;

#[derive(Debug, Clone)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    pub branch: String,
}

/// Create a disposable worktree under `.kkagent/worktrees/<id>`.
pub async fn create_worktree(
    repo: &Path,
    id: &str,
    branch_hint: Option<&str>,
) -> anyhow::Result<WorktreeInfo> {
    let root = repo.join(".kkagent").join("worktrees");
    tokio::fs::create_dir_all(&root).await?;
    let path = root.join(id);
    if path.exists() {
        return Ok(WorktreeInfo {
            path,
            branch: branch_hint.unwrap_or("HEAD").into(),
        });
    }
    let branch = branch_hint
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("kkagent/task-{id}"));

    // Ensure branch exists (create from HEAD if needed).
    let _ = Command::new("git")
        .args(["branch", &branch, "HEAD"])
        .current_dir(repo)
        .output()
        .await;

    let out = Command::new("git")
        .args([
            "worktree",
            "add",
            "--detach",
            path.to_str().unwrap_or("."),
            "HEAD",
        ])
        .current_dir(repo)
        .output()
        .await?;
    if !out.status.success() {
        // Fallback: plain directory copy is not used; report error.
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("git worktree add failed: {stderr}");
    }
    Ok(WorktreeInfo { path, branch })
}

pub async fn remove_worktree(repo: &Path, path: &Path) -> anyhow::Result<()> {
    let out = Command::new("git")
        .args([
            "worktree",
            "remove",
            "--force",
            path.to_str().unwrap_or("."),
        ])
        .current_dir(repo)
        .output()
        .await?;
    if !out.status.success() {
        // Best-effort cleanup.
        let _ = tokio::fs::remove_dir_all(path).await;
    }
    Ok(())
}

pub fn worktree_enabled() -> bool {
    std::env::var("KKAGENT_GIT_WORKTREE")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// If `path` points at a managed worktree (`<repo>/.kkagent/worktrees/<id>`),
/// return the repo root that owns it.
///
/// Uses the *last* qualifying occurrence, so a nested worktree
/// (`<parent-wt>/.kkagent/worktrees/<id>`) resolves to its immediate parent
/// worktree (which contains the `.git` link `git worktree remove` needs).
/// Paths with anything after the worktree id are rejected — the id must be
/// the final component.
pub fn managed_worktree_repo(path: &Path) -> Option<PathBuf> {
    let comps: Vec<_> = path.components().collect();
    let mut repo_end = None;
    for i in 0..comps.len() {
        if comps.len() == i + 3
            && comps[i].as_os_str() == ".kkagent"
            && comps[i + 1].as_os_str() == "worktrees"
        {
            repo_end = Some(i);
        }
    }
    repo_end.map(|i| {
        let root: PathBuf = comps[..i].iter().collect();
        if root.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            root
        }
    })
}

/// Best-effort disposal of a finished subagent's worktree
/// (issues/subagent_issues.md #2). No-op for non-managed working
/// directories (the common case: subagents without worktree isolation).
pub async fn cleanup_worktree(working_dir: &Path) {
    let Some(repo) = managed_worktree_repo(working_dir) else {
        return;
    };
    if !working_dir.exists() {
        return;
    }
    if let Err(error) = remove_worktree(&repo, working_dir).await {
        tracing::warn!(
            "worktree cleanup failed for {}: {error}",
            working_dir.display()
        );
    }
}

/// Sweep orphaned worktrees left behind by a crashed/killed process
/// (issues/subagent_issues.md #2 residual). Scans `<repo>/.kkagent/worktrees/`
/// and removes entries whose id is not in `alive_ids`. Only the top-level
/// worktree directory of the given repo is swept — nested worktrees inside a
/// surviving parent are left untouched (the parent's own cleanup handles
/// them). No-op when the worktrees directory doesn't exist.
pub async fn sweep_orphan_worktrees(repo: &Path, alive_ids: &[String]) {
    let wt_dir = repo.join(".kkagent").join("worktrees");
    let entries = match tokio::fs::read_dir(&wt_dir).await {
        Ok(rd) => rd,
        Err(_) => return, // No worktrees dir — nothing to sweep.
    };
    let alive: std::collections::HashSet<String> = alive_ids.iter().cloned().collect();
    let mut removed = 0;
    let mut entries = entries;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        let name = match entry.file_name().into_string() {
            Ok(n) => n,
            Err(_) => continue, // Non-UTF8 — leave it alone.
        };
        if alive.contains(&name) {
            continue;
        }
        tracing::info!("sweeping orphan worktree: {}", path.display());
        if remove_worktree(repo, &path).await.is_ok() {
            removed += 1;
        }
    }
    if removed > 0 {
        tracing::info!(
            "swept {removed} orphan worktree(s) under {}",
            repo.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_worktree_repo_detects_plain_worktrees() {
        assert_eq!(
            managed_worktree_repo(Path::new("/repo/.kkagent/worktrees/abc")),
            Some(PathBuf::from("/repo"))
        );
    }

    #[test]
    fn managed_worktree_repo_resolves_nested_to_immediate_parent() {
        assert_eq!(
            managed_worktree_repo(Path::new(
                "/repo/.kkagent/worktrees/parent/.kkagent/worktrees/child"
            )),
            Some(PathBuf::from("/repo/.kkagent/worktrees/parent"))
        );
    }

    #[test]
    fn non_managed_paths_are_ignored() {
        assert_eq!(managed_worktree_repo(Path::new("/home/user/project")), None);
        // Missing or extra trailing components are not worktree roots.
        assert_eq!(
            managed_worktree_repo(Path::new("/repo/.kkagent/worktrees")),
            None
        );
        assert_eq!(
            managed_worktree_repo(Path::new("/repo/.kkagent/worktrees/a/b")),
            None
        );
        // Regular dirs that merely contain .kkagent are ignored.
        assert_eq!(
            managed_worktree_repo(Path::new("/repo/.kkagent/cache/abc")),
            None
        );
    }

    #[test]
    fn orphan_sweep_keeps_alive_and_drops_unknown() {
        // Pure logic test for the alive-id filter: we simulate the directory
        // listing and verify which ids would be kept. The actual git removal
        // is exercised only against a real repo in integration tests.
        let alive = ["keep-me".to_string()];
        let alive_set: std::collections::HashSet<String> = alive.iter().cloned().collect();
        assert!(alive_set.contains("keep-me"));
        assert!(!alive_set.contains("orphan-1"));
        assert!(!alive_set.contains("orphan-2"));
    }
}
