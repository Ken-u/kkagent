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
