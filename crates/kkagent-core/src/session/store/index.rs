//! Append-only `session_index.jsonl` (kimi-code session-index aligned).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionIndexEntry {
    pub session_id: String,
    pub session_dir: String,
    pub work_dir: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionIndexDeletion {
    session_id: String,
    deleted: bool,
}

static APPEND_LOCK: Mutex<()> = Mutex::new(());

pub fn session_index_path(home_dir: &Path) -> PathBuf {
    home_dir.join("session_index.jsonl")
}

pub fn append_session_index_entry(home_dir: &Path, entry: &SessionIndexEntry) -> anyhow::Result<()> {
    append_line(home_dir, &serde_json::to_string(entry)?)
}

pub fn append_session_index_deletion(home_dir: &Path, session_id: &str) -> anyhow::Result<()> {
    let rec = SessionIndexDeletion {
        session_id: session_id.to_string(),
        deleted: true,
    };
    append_line(home_dir, &serde_json::to_string(&rec)?)
}

fn append_line(home_dir: &Path, line: &str) -> anyhow::Result<()> {
    let _guard = APPEND_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let path = session_index_path(home_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    writeln!(f, "{line}")?;
    Ok(())
}

pub fn read_session_index(
    home_dir: &Path,
    sessions_dir: &Path,
) -> anyhow::Result<HashMap<String, SessionIndexEntry>> {
    let path = session_index_path(home_dir);
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Ok(HashMap::new());
    };
    let sessions_canon = sessions_dir
        .canonicalize()
        .unwrap_or_else(|_| sessions_dir.to_path_buf());
    let mut result = HashMap::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("deleted").and_then(|d| d.as_bool()) == Some(true) {
            if let Some(id) = v.get("sessionId").and_then(|s| s.as_str()) {
                result.remove(id);
            }
            continue;
        }
        let Ok(entry) = serde_json::from_value::<SessionIndexEntry>(v) else {
            continue;
        };
        let session_dir = PathBuf::from(&entry.session_dir);
        if !session_dir.is_absolute() {
            continue;
        }
        let canon = session_dir
            .canonicalize()
            .unwrap_or_else(|_| session_dir.clone());
        if !canon.starts_with(&sessions_canon) && !session_dir.starts_with(sessions_dir) {
            continue;
        }
        if session_dir.file_name().and_then(|n| n.to_str()) != Some(entry.session_id.as_str()) {
            continue;
        }
        result.insert(entry.session_id.clone(), entry);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_and_read() {
        let home = std::env::temp_dir().join(format!("kkagent-idx-{}", uuid::Uuid::new_v4()));
        let sessions = home.join("sessions");
        let bucket = sessions.join("wd_demo");
        let sid = "sess1";
        let sdir = bucket.join(sid);
        std::fs::create_dir_all(&sdir).unwrap();
        append_session_index_entry(
            &home,
            &SessionIndexEntry {
                session_id: sid.into(),
                session_dir: sdir.to_string_lossy().into(),
                work_dir: "/proj".into(),
            },
        )
        .unwrap();
        let map = read_session_index(&home, &sessions).unwrap();
        assert!(map.contains_key(sid));
        append_session_index_deletion(&home, sid).unwrap();
        let map = read_session_index(&home, &sessions).unwrap();
        assert!(!map.contains_key(sid));
        let _ = std::fs::remove_dir_all(&home);
    }
}
