//! Collect lightweight git context for system prompt injection.

use std::path::Path;
use std::process::Command;

pub fn collect_git_context(working_dir: &Path) -> Option<String> {
    if !working_dir.join(".git").exists() {
        // Might be a worktree; still try git commands.
    }
    let branch = git(working_dir, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    let status = git(working_dir, &["status", "--short"]).unwrap_or_default();
    let log = git(working_dir, &["log", "-5", "--oneline", "--decorate"]).unwrap_or_default();
    let mut out = String::from("\n\n# Git context\n\n");
    out.push_str(&format!("- branch: `{}`\n", branch.trim()));
    if status.trim().is_empty() {
        out.push_str("- status: clean\n");
    } else {
        out.push_str("- status:\n```\n");
        out.push_str(status.trim());
        out.push_str("\n```\n");
    }
    if !log.trim().is_empty() {
        out.push_str("- recent commits:\n```\n");
        out.push_str(log.trim());
        out.push_str("\n```\n");
    }
    Some(out)
}

fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub fn is_workspace_trusted(config: &kkagent_config::AppConfig, working_dir: &Path) -> bool {
    if config.trusted_workspaces.is_empty() {
        return true;
    }
    let cwd = working_dir
        .canonicalize()
        .unwrap_or_else(|_| working_dir.to_path_buf());
    config.trusted_workspaces.iter().any(|t| {
        let p = std::path::PathBuf::from(t);
        let p = p.canonicalize().unwrap_or(p);
        cwd.starts_with(&p)
    })
}
