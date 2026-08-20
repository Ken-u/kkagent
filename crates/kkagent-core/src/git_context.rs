//! Workspace trust evaluation (shared by sandbox posture decisions).

use std::path::Path;

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
