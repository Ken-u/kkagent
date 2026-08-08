use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tokio::io::AsyncWriteExt;

use crate::migration::{migrate_wire_records, WIRE_PROTOCOL_VERSION};
use crate::record::{create_wire_metadata_record, WireRecord, AGENT_WIRE_RECORD_KEY};

/// Append-only JSONL wire journal under a session directory.
pub struct WireJournal {
    path: PathBuf,
}

impl WireJournal {
    pub fn open(session_dir: impl AsRef<Path>) -> Self {
        Self {
            path: session_dir.as_ref().join(AGENT_WIRE_RECORD_KEY),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub async fn ensure_metadata(&self) -> Result<()> {
        if self.path.exists() {
            return Ok(());
        }
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let now = chrono::Utc::now().timestamp_millis();
        let meta = create_wire_metadata_record(now);
        let line = serde_json::to_string(&meta)?;
        let mut f = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .await?;
        f.write_all(line.as_bytes()).await?;
        f.write_all(b"\n").await?;
        Ok(())
    }

    pub async fn append(&self, record: &WireRecord) -> Result<()> {
        self.ensure_metadata().await?;
        let line = serde_json::to_string(&record.to_value())?;
        let mut f = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .await?;
        f.write_all(line.as_bytes()).await?;
        f.write_all(b"\n").await?;
        Ok(())
    }

    pub async fn read_all_migrated(&self) -> Result<(String, Vec<WireRecord>)> {
        if !self.path.exists() {
            return Ok((WIRE_PROTOCOL_VERSION.into(), Vec::new()));
        }
        let text = tokio::fs::read_to_string(&self.path)
            .await
            .with_context(|| format!("read wire journal {}", self.path.display()))?;
        let mut records = Vec::new();
        let mut version = WIRE_PROTOCOL_VERSION.to_string();
        for (idx, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let value: serde_json::Value = serde_json::from_str(line)
                .with_context(|| format!("parse wire line {}", idx + 1))?;
            let Some(record) = WireRecord::from_value(value) else {
                continue;
            };
            if idx == 0 && record.record_type == "metadata" {
                if let Some(v) = record
                    .fields
                    .get("protocol_version")
                    .and_then(|v| v.as_str())
                {
                    version = v.to_string();
                }
                continue;
            }
            records.push(record);
        }
        let migrated = migrate_wire_records(records, Some(&version))?;
        Ok((WIRE_PROTOCOL_VERSION.into(), migrated))
    }
}
