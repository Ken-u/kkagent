//! Persist composer drafts across process restarts (session-scoped).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DraftRecord {
    pub text: String,
    pub cursor: usize,
    pub updated_at_unix: u64,
}

fn drafts_dir() -> PathBuf {
    kkagent_config::default_config_dir().join("drafts")
}

fn draft_path(session_id: &str) -> PathBuf {
    let safe: String = session_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .take(64)
        .collect();
    drafts_dir().join(format!("{safe}.json"))
}

pub fn save_draft(session_id: &str, text: &str, cursor: usize) -> std::io::Result<()> {
    let dir = drafts_dir();
    std::fs::create_dir_all(&dir)?;
    let rec = DraftRecord {
        text: text.to_string(),
        cursor,
        updated_at_unix: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    };
    let path = draft_path(session_id);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&rec).unwrap_or_default())?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

pub fn load_draft(session_id: &str) -> Option<DraftRecord> {
    let path = draft_path(session_id);
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn clear_draft(session_id: &str) {
    let _ = std::fs::remove_file(draft_path(session_id));
}

pub fn redact_sensitive_preview(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let markers = [
        "api_key",
        "apikey",
        "secret",
        "private key",
        "begin rsa private",
        "begin openssh private",
        "password=",
        "authorization: bearer",
    ];
    if markers.iter().any(|m| lower.contains(m)) {
        Some("Possible secret detected in draft — review before send.".into())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_draft() {
        let id = format!("test-{}", uuid::Uuid::new_v4());
        save_draft(&id, "hello", 2).unwrap();
        let loaded = load_draft(&id).unwrap();
        assert_eq!(loaded.text, "hello");
        assert_eq!(loaded.cursor, 2);
        clear_draft(&id);
        assert!(load_draft(&id).is_none());
    }

    #[test]
    fn detects_secretish() {
        assert!(redact_sensitive_preview("api_key=sk-abc").is_some());
        assert!(redact_sensitive_preview("hello world").is_none());
    }
}
