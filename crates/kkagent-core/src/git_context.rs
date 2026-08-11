//! Collect lightweight git context for system prompt injection.

use std::path::Path;
use std::process::Command;

pub fn collect_git_context(working_dir: &Path) -> Option<String> {
    collect_git_context_with_trust(working_dir, None)
}

pub fn collect_git_context_with_trust(
    working_dir: &Path,
    trust: Option<&kkagent_config::WorkspaceTrust>,
) -> Option<String> {
    if !kkagent_config::git_metadata_accessible(trust) {
        return None;
    }
    if !working_dir.join(".git").exists() {
        // Might be a worktree; still try git commands.
    }
    let branch = git(working_dir, &["rev-parse", "--abbrev-ref", "HEAD"], trust)?;
    let status = git(working_dir, &["status", "--short"], trust).unwrap_or_default();
    let log = git(
        working_dir,
        &["log", "-5", "--oneline", "--decorate"],
        trust,
    )
    .unwrap_or_default();
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

fn git(
    cwd: &Path,
    args: &[&str],
    trust: Option<&kkagent_config::WorkspaceTrust>,
) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .envs(kkagent_config::git_environment(trust))
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub fn is_workspace_trusted(config: &kkagent_config::AppConfig, working_dir: &Path) -> bool {
    if config.sandbox.is_disabled() {
        return true;
    }
    if config.workspace_trust.matching(working_dir).is_some() {
        return true;
    }
    if config.trusted_workspaces.is_empty() {
        let Ok(server_workspace) = std::env::current_dir().and_then(std::fs::canonicalize) else {
            return false;
        };
        let working_dir =
            std::fs::canonicalize(working_dir).unwrap_or_else(|_| working_dir.to_path_buf());
        return working_dir.starts_with(server_workspace);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_static_trust_is_scoped_to_the_server_workspace() {
        let config = kkagent_config::AppConfig::default();
        let workspace = std::env::current_dir().unwrap().canonicalize().unwrap();
        assert!(is_workspace_trusted(&config, &workspace));
        assert!(is_workspace_trusted(&config, &workspace.join("nested")));
        assert!(!is_workspace_trusted(
            &config,
            workspace.parent().expect("workspace has a parent")
        ));
    }

    #[test]
    fn disabled_sandbox_does_not_require_workspace_trust() {
        let mut config = kkagent_config::AppConfig::default();
        config.sandbox.mode = "disabled".into();
        assert!(is_workspace_trusted(
            &config,
            Path::new("/an/unreviewed/workspace")
        ));
    }
}
