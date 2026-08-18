//! Durable turn checkpoints: file snapshots + undo journal on disk.
//!
//! In-memory checkpoints die with the process and are byte-capped. This store
//! keeps the same `FileChange`/`TurnCheckpoint` model but persists:
//!
//! - `checkpoints/blobs/<sha256>` — pre-change file contents (content
//!   addressed, deduped across turns and sessions in the same workspace).
//! - `checkpoints/undo.jsonl` — one line per committed turn checkpoint:
//!   `{"message_start_index":N,"changes":[{"path":...,"blob":...|null}]}`.
//!
//! `Session::record_pre_change` stages snapshots here instead of memory, so
//! oversized files are no longer skipped and the undo stack survives restarts
//! and compaction.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Journal line: a committed turn checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointEntry {
    /// Index of the user message that started this turn; `None` after a
    /// compaction rewrote the transcript (snapshot-only, no truncation).
    #[serde(default)]
    pub message_start_index: Option<usize>,
    /// Pre-change snapshots in the order they were recorded.
    pub changes: Vec<ChangeEntry>,
}

/// A single file's pre-change reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeEntry {
    pub path: PathBuf,
    /// Content hash in the blob dir; `None` = file did not exist before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
    /// Original file length, surfaced for undo previews.
    #[serde(default)]
    pub bytes: u64,
}

#[derive(Debug, Clone)]
pub struct RestorePlan {
    pub path: PathBuf,
    /// `None` = the file did not exist before (delete on restore).
    pub blob: Option<String>,
}

/// Disk-backed checkpoint set for one session.
pub struct CheckpointStore {
    root: PathBuf,
}

impl CheckpointStore {
    /// Open (and lazily create) the checkpoint dirs under `session_dir`.
    pub fn open(session_dir: &Path) -> Self {
        Self {
            root: session_dir.join("checkpoints"),
        }
    }

    fn blob_dir(&self) -> PathBuf {
        self.root.join("blobs")
    }

    fn journal_path(&self) -> PathBuf {
        self.root.join("undo.jsonl")
    }

    /// Persist `contents` as a content-addressed blob; returns its hash.
    pub fn write_blob(&self, contents: &[u8]) -> Result<String> {
        let mut hasher = Sha256::new();
        hasher.update(contents);
        let hash = format!("{:x}", hasher.finalize());
        let path = self.blob_dir().join(&hash);
        if !path.exists() {
            let dir = self.blob_dir();
            fs::create_dir_all(&dir).with_context(|| dir.display().to_string())?;
            let tmp = self.root.join(format!(".blob-{hash}.tmp"));
            {
                let mut file = fs::File::create(&tmp).with_context(|| tmp.display().to_string())?;
                file.write_all(contents)?;
                file.sync_all().ok();
            }
            fs::rename(&tmp, &path).with_context(|| path.display().to_string())?;
        }
        Ok(hash)
    }

    /// Load a blob's contents by hash.
    pub fn read_blob(&self, hash: &str) -> Result<Vec<u8>> {
        // Reject forged separators/traversal in stored hashes.
        if hash.is_empty() || !hash.bytes().all(|b| b.is_ascii_hexdigit()) || hash.len() != 64 {
            anyhow::bail!("invalid blob hash");
        }
        let path = self.blob_dir().join(hash);
        fs::read(&path).with_context(|| path.display().to_string())
    }

    /// Append a committed turn checkpoint to the journal.
    pub fn append(&self, entry: &CheckpointEntry) -> Result<()> {
        fs::create_dir_all(&self.root).with_context(|| self.root.display().to_string())?;
        let line = serde_json::to_string(entry)?;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.journal_path())
            .with_context(|| self.journal_path().display().to_string())?;
        writeln!(file, "{line}")?;
        file.sync_all().ok();
        Ok(())
    }

    /// Rewrite the journal after an undo dropped the last entry (or more).
    pub fn truncate_to(&self, remaining: &[CheckpointEntry]) -> Result<()> {
        let tmp = self.root.join(".undo.jsonl.tmp");
        {
            let mut file = fs::File::create(&tmp).with_context(|| tmp.display().to_string())?;
            for entry in remaining {
                writeln!(file, "{}", serde_json::to_string(entry)?)?;
            }
            file.sync_all().ok();
        }
        fs::rename(&tmp, self.journal_path())
            .with_context(|| self.journal_path().display().to_string())?;
        Ok(())
    }

    /// Load the committed checkpoint journal (oldest first). A torn final
    /// line (crash mid-append) is dropped.
    pub fn load(&self) -> Vec<CheckpointEntry> {
        let Ok(text) = fs::read_to_string(self.journal_path()) else {
            return Vec::new();
        };
        let mut entries = Vec::new();
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<CheckpointEntry>(line) {
                Ok(entry) => entries.push(entry),
                Err(_) => {
                    tracing::warn!("dropping malformed undo journal line");
                }
            }
        }
        entries
    }

    /// Convert a change entry into a restore plan (blob -> absolute restore
    /// is done by the caller against `working_dir`).
    pub fn restore_plan(&self, entry: &ChangeEntry) -> RestorePlan {
        RestorePlan {
            path: entry.path.clone(),
            blob: entry.blob.clone(),
        }
    }

    /// Drop blob files that no journal entry references anymore. Best-effort.
    pub fn gc(&self) {
        let entries = self.load();
        let referenced: std::collections::HashSet<String> = entries
            .iter()
            .flat_map(|e| e.changes.iter().filter_map(|c| c.blob.clone()))
            .collect();
        if let Ok(dir) = fs::read_dir(self.blob_dir()) {
            for file in dir.flatten() {
                let name = file.file_name().to_string_lossy().to_string();
                if name.starts_with('.') {
                    continue;
                }
                if !referenced.contains(&name) {
                    let _ = fs::remove_file(file.path());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kkagent-cp-test-{}-{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn write_and_read_blob_roundtrip() {
        let dir = temp_dir();
        let store = CheckpointStore::open(&dir);
        let hash = store.write_blob(b"hello").unwrap();
        assert_eq!(store.read_blob(&hash).unwrap(), b"hello");
        // Dedup: same content, same hash, single file.
        let hash2 = store.write_blob(b"hello").unwrap();
        assert_eq!(hash, hash2);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn journal_append_load_truncate() {
        let dir = temp_dir();
        let store = CheckpointStore::open(&dir);
        let blob = store.write_blob(b"data").unwrap();
        let e1 = CheckpointEntry {
            message_start_index: Some(0),
            changes: vec![ChangeEntry {
                path: PathBuf::from("a.txt"),
                blob: Some(blob.clone()),
                bytes: 4,
            }],
        };
        let e2 = CheckpointEntry {
            message_start_index: Some(3),
            changes: vec![ChangeEntry {
                path: PathBuf::from("b.txt"),
                blob: None,
                bytes: 0,
            }],
        };
        store.append(&e1).unwrap();
        store.append(&e2).unwrap();
        assert_eq!(store.load(), vec![e1.clone(), e2.clone()]);

        store.truncate_to(std::slice::from_ref(&e1)).unwrap();
        assert_eq!(store.load(), vec![e1]);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn torn_final_line_is_dropped() {
        let dir = temp_dir();
        let store = CheckpointStore::open(&dir);
        fs::create_dir_all(&store.root).unwrap();
        let e = CheckpointEntry {
            message_start_index: Some(7),
            changes: vec![],
        };
        let mut line = serde_json::to_string(&e).unwrap();
        line.truncate(line.len() - 5); // simulate torn write
        fs::write(store.journal_path(), format!("{}\n", line)).unwrap();
        assert!(store.load().is_empty());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_blob_rejects_bad_hash() {
        let dir = temp_dir();
        let store = CheckpointStore::open(&dir);
        assert!(store.read_blob("").is_err());
        assert!(store.read_blob("../etc/passwd").is_err());
        assert!(store.read_blob("abc").is_err());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn gc_removes_unreferenced_blobs() {
        let dir = temp_dir();
        let store = CheckpointStore::open(&dir);
        let kept = store.write_blob(b"keep").unwrap();
        let dropped = store.write_blob(b"drop").unwrap();
        store
            .append(&CheckpointEntry {
                message_start_index: Some(0),
                changes: vec![ChangeEntry {
                    path: PathBuf::from("f"),
                    blob: Some(kept.clone()),
                    bytes: 4,
                }],
            })
            .unwrap();
        store.gc();
        assert!(store.read_blob(&kept).is_ok());
        assert!(store.read_blob(&dropped).is_err());
        fs::remove_dir_all(&dir).ok();
    }
}
