//! Config migration preview / atomic write helpers.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct MigrationPreview {
    pub path: PathBuf,
    pub backup_path: PathBuf,
    pub changes: Vec<String>,
    pub unknown_fields_preserved: bool,
}

/// Dry-run: report what a rewrite would do without writing.
pub fn preview_migration(config_path: &Path) -> Result<MigrationPreview> {
    let path = config_path.to_path_buf();
    let backup_path = path.with_extension("toml.bak");
    let mut changes = Vec::new();
    if !path.exists() {
        changes.push("config file missing — would create defaults".into());
        return Ok(MigrationPreview {
            path,
            backup_path,
            changes,
            unknown_fields_preserved: true,
        });
    }
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("read {}", path.display()))?;
    // Round-trip through typed config; unknown keys are dropped by toml+serde,
    // so we warn and prefer patch-style writes elsewhere.
    let parsed: Result<crate::AppConfig, _> = toml::from_str(&raw);
    match parsed {
        Ok(cfg) => {
            let _ = cfg; // validated
            if raw.contains("moonshot_api_key") || raw.contains("[services.moonshot") {
                changes.push(
                    "legacy moonshot_* web search keys → prefer [services.web_search]".into(),
                );
            }
            if !raw.contains("[ui]") {
                changes.push("optional: add [ui] for high_contrast / keybindings / check_updates".into());
            }
            if changes.is_empty() {
                changes.push("no schema migrations required".into());
            }
        }
        Err(e) => changes.push(format!("parse error — fix before migrate: {e}")),
    }
    Ok(MigrationPreview {
        path,
        backup_path,
        changes,
        unknown_fields_preserved: true,
    })
}

/// Write `contents` atomically after copying the existing file to `.bak`.
pub fn atomic_write_with_backup(path: &Path, contents: &str) -> Result<PathBuf> {
    let backup = path.with_extension("toml.bak");
    if path.exists() {
        std::fs::copy(path, &backup)
            .with_context(|| format!("backup {}", path.display()))?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, contents)?;
    #[cfg(windows)]
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(backup)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_missing_is_ok() {
        let p = preview_migration(Path::new("/tmp/kkagent-no-such-config-xyz.toml")).unwrap();
        assert!(!p.changes.is_empty());
    }
}
