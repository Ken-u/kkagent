//! Persist input history per workspace (not secrets).

use std::path::{Path, PathBuf};

const MAX_ENTRIES: usize = 200;

pub fn history_path(workspace: &Path) -> PathBuf {
    let key = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf())
        .to_string_lossy()
        .replace(['/', '\\', ':'], "_");
    kkagent_config::default_config_dir()
        .join("history")
        .join(format!("{key}.json"))
}

pub fn load(workspace: &Path) -> Vec<String> {
    let path = history_path(workspace);
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<String>>(&raw).unwrap_or_default()
}

pub fn save(workspace: &Path, entries: &[String]) -> anyhow::Result<()> {
    let path = history_path(workspace);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let trimmed: Vec<&String> = entries.iter().rev().take(MAX_ENTRIES).collect();
    let ordered: Vec<&String> = trimmed.into_iter().rev().collect();
    let body = serde_json::to_string_pretty(&ordered)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body)?;
    #[cfg(windows)]
    if path.exists() {
        let _ = std::fs::remove_file(&path);
    }
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

pub fn push(workspace: &Path, text: &str) -> Vec<String> {
    let t = text.trim();
    if t.is_empty() {
        return load(workspace);
    }
    let mut entries = load(workspace);
    if entries.last().map(|s| s.as_str()) != Some(t) {
        entries.push(t.to_string());
    }
    let _ = save(workspace, &entries);
    entries
}
