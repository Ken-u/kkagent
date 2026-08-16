use serde_json::{json, Value};
use std::path::{Component, Path, PathBuf};

use crate::AcpSessionStore;

fn session_id(params: &Value) -> String {
    params
        .get("sessionId")
        .or_else(|| params.get("session_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// Resolve a client path against the session cwd and reject escapes.
pub async fn resolve_path(store: &AcpSessionStore, params: &Value) -> Result<PathBuf, String> {
    let sid = session_id(params);
    let cwd = store
        .session_cwd(&sid)
        .await
        .ok_or_else(|| format!("session not found: {sid}"))?;
    let raw = params
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or(".")
        .trim();
    if raw.is_empty() {
        return Ok(cwd);
    }
    let candidate = if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        cwd.join(raw)
    };
    // Normalize without requiring the path to exist; reject escapes.
    let mut normalized = PathBuf::new();
    for comp in candidate.components() {
        match comp {
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(format!(
                        "path escapes session workspace: {}",
                        candidate.display()
                    ));
                }
            }
            Component::CurDir => {}
            other => normalized.push(other.as_os_str()),
        }
    }
    let cwd_canon = std::fs::canonicalize(&cwd).unwrap_or(cwd.clone());
    // Defense in depth: refuse paths that traverse a symlink component, so a
    // pre-existing `cwd/link -> /etc` cannot be used to write outside the
    // workspace (approximates O_NOFOLLOW for the write tools below).
    if has_symlink_component(&cwd, &normalized) {
        return Err(format!(
            "path traverses a symlink and may escape the session workspace: {}",
            candidate.display()
        ));
    }
    // Compare against cwd: if normalized is absolute and outside cwd_canon, reject.
    if normalized.is_absolute() {
        if let Ok(canon) = std::fs::canonicalize(&normalized) {
            if !canon.starts_with(&cwd_canon) {
                return Err(format!(
                    "path escapes session workspace: {}",
                    candidate.display()
                ));
            }
            return Ok(canon);
        }
        let parent = normalized.parent().unwrap_or(Path::new("."));
        if parent.exists() {
            let parent_canon = std::fs::canonicalize(parent).map_err(|e| e.to_string())?;
            if !parent_canon.starts_with(&cwd_canon) {
                return Err(format!(
                    "path escapes session workspace: {}",
                    candidate.display()
                ));
            }
        } else if !normalized.starts_with(&cwd_canon) && !normalized.starts_with(&cwd) {
            return Err(format!(
                "path escapes session workspace: {}",
                candidate.display()
            ));
        }
        return Ok(normalized);
    }
    Ok(normalized)
}

/// Returns true when any component of `path` below `cwd` is an existing symlink.
fn has_symlink_component(cwd: &Path, path: &Path) -> bool {
    let Ok(rel) = path.strip_prefix(cwd) else {
        return false;
    };
    let mut cur = cwd.to_path_buf();
    for comp in rel.components() {
        cur.push(comp.as_os_str());
        if std::fs::symlink_metadata(&cur)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            return true;
        }
    }
    false
}

pub async fn read_text(store: &AcpSessionStore, params: &Value) -> Result<Value, String> {
    let path = resolve_path(store, params).await?;
    let content = tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| e.to_string())?;
    Ok(json!({
        "path": path.to_string_lossy(),
        "content": content,
    }))
}

pub async fn write_text(store: &AcpSessionStore, params: &Value) -> Result<Value, String> {
    let path = resolve_path(store, params).await?;
    let content = params.get("content").and_then(|v| v.as_str()).unwrap_or("");
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| e.to_string())?;
    }
    tokio::fs::write(&path, content)
        .await
        .map_err(|e| e.to_string())?;
    Ok(json!({"ok": true, "path": path.to_string_lossy()}))
}

pub async fn edit_text(store: &AcpSessionStore, params: &Value) -> Result<Value, String> {
    let path = resolve_path(store, params).await?;
    let old = params
        .get("old_string")
        .or_else(|| params.get("oldString"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let new = params
        .get("new_string")
        .or_else(|| params.get("newString"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if old.is_empty() {
        return Err("old_string is required".into());
    }
    let content = tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| e.to_string())?;
    let count = content.matches(old).count();
    if count == 0 {
        return Err("old_string not found".into());
    }
    if count > 1
        && !params
            .get("replace_all")
            .or_else(|| params.get("replaceAll"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    {
        return Err(format!(
            "old_string matched {count} times; pass replace_all=true to replace all"
        ));
    }
    let next = if count == 1 {
        content.replacen(old, new, 1)
    } else {
        content.replace(old, new)
    };
    tokio::fs::write(&path, next)
        .await
        .map_err(|e| e.to_string())?;
    Ok(json!({"ok": true, "path": path.to_string_lossy(), "replacements": count}))
}

pub async fn list_dir(store: &AcpSessionStore, params: &Value) -> Result<Value, String> {
    let path = resolve_path(store, params).await?;
    let mut entries = Vec::new();
    let mut rd = tokio::fs::read_dir(&path)
        .await
        .map_err(|e| e.to_string())?;
    while let Some(entry) = rd.next_entry().await.map_err(|e| e.to_string())? {
        let meta = entry.metadata().await.map_err(|e| e.to_string())?;
        entries.push(json!({
            "name": entry.file_name().to_string_lossy(),
            "path": entry.path().to_string_lossy(),
            "isDir": meta.is_dir(),
            "size": meta.len(),
        }));
    }
    Ok(json!({"path": path.to_string_lossy(), "entries": entries}))
}

pub async fn stat_path(store: &AcpSessionStore, params: &Value) -> Result<Value, String> {
    let path = resolve_path(store, params).await?;
    let meta = tokio::fs::metadata(&path)
        .await
        .map_err(|e| e.to_string())?;
    Ok(json!({
        "path": path.to_string_lossy(),
        "isDir": meta.is_dir(),
        "isFile": meta.is_file(),
        "size": meta.len(),
    }))
}

pub async fn glob_paths(store: &AcpSessionStore, params: &Value) -> Result<Value, String> {
    let sid = session_id(params);
    let cwd = store
        .session_cwd(&sid)
        .await
        .ok_or_else(|| format!("session not found: {sid}"))?;
    let pattern = params
        .get("pattern")
        .or_else(|| params.get("glob"))
        .and_then(|v| v.as_str())
        .unwrap_or("*");
    let base = params
        .get("path")
        .and_then(|v| v.as_str())
        .map(|p| cwd.join(p))
        .unwrap_or(cwd);
    let walker = walkdir::WalkDir::new(&base).max_depth(8);
    let mut matches = Vec::new();
    for entry in walker.into_iter().filter_map(|e| e.ok()) {
        let name = entry.file_name().to_string_lossy();
        if glob_match(pattern, &name) {
            matches.push(json!(entry.path().to_string_lossy()));
        }
        if matches.len() >= 200 {
            break;
        }
    }
    Ok(json!({"matches": matches, "pattern": pattern}))
}

pub async fn grep_paths(store: &AcpSessionStore, params: &Value) -> Result<Value, String> {
    let sid = session_id(params);
    let cwd = store
        .session_cwd(&sid)
        .await
        .ok_or_else(|| format!("session not found: {sid}"))?;
    let pattern = params
        .get("pattern")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if pattern.is_empty() {
        return Err("pattern is required".into());
    }
    let base = params
        .get("path")
        .and_then(|v| v.as_str())
        .map(|p| cwd.join(p))
        .unwrap_or(cwd);
    let mut matches = Vec::new();
    let walker = walkdir::WalkDir::new(&base).max_depth(6);
    for entry in walker.into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        for (idx, line) in text.lines().enumerate() {
            if line.contains(&pattern) {
                matches.push(json!({
                    "path": entry.path().to_string_lossy(),
                    "line": idx + 1,
                    "text": line,
                }));
                if matches.len() >= 100 {
                    return Ok(json!({"matches": matches, "pattern": pattern}));
                }
            }
        }
    }
    Ok(json!({"matches": matches, "pattern": pattern}))
}

fn glob_match(pattern: &str, name: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(suffix) = pattern.strip_prefix('*') {
        return name.ends_with(suffix);
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return name.starts_with(prefix);
    }
    pattern == name
}
