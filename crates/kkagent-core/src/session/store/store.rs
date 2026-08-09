//! On-disk session store — create / list / fork / archive / rename / delete.

use super::index::{
    append_session_index_deletion, append_session_index_entry, read_session_index,
    SessionIndexEntry,
};
use super::workdir_key::{encode_work_dir_key, is_safe_session_id, normalize_work_dir};
use crate::session::metadata::{SessionMeta, SessionMetaPatch, SessionMetadataService};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    pub id: String,
    pub session_dir: String,
    pub work_dir: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default)]
    pub archived: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_prompt: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<String>,
}

pub struct SessionStore {
    pub home_dir: PathBuf,
    pub sessions_dir: PathBuf,
}

impl SessionStore {
    pub fn new(home_dir: impl Into<PathBuf>) -> Self {
        let home_dir = home_dir.into();
        let sessions_dir = home_dir.join("sessions");
        Self {
            home_dir,
            sessions_dir,
        }
    }

    pub fn open_default() -> Self {
        Self::new(kkagent_config::default_config_dir())
    }

    pub fn session_dir_for(&self, id: &str, work_dir: &Path) -> anyhow::Result<PathBuf> {
        if !is_safe_session_id(id) {
            anyhow::bail!("unsafe session id: {id}");
        }
        let bucket = encode_work_dir_key(work_dir);
        Ok(self.sessions_dir.join(bucket).join(id))
    }

    pub fn create(&self, id: &str, work_dir: &Path) -> anyhow::Result<SessionSummary> {
        if !is_safe_session_id(id) {
            anyhow::bail!("unsafe session id: {id}");
        }
        let work = normalize_work_dir(work_dir);
        if let Some(existing) = self.find_entry(id)? {
            if Path::new(&existing.session_dir).is_dir() {
                anyhow::bail!("session already exists: {id}");
            }
        }
        let dir = self.session_dir_for(id, &work)?;
        if dir.is_dir() {
            anyhow::bail!("session already exists: {id}");
        }
        std::fs::create_dir_all(&dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        }
        let meta = SessionMetadataService::create_new(&dir, id, &work)?;
        append_session_index_entry(
            &self.home_dir,
            &SessionIndexEntry {
                session_id: id.to_string(),
                session_dir: dir.to_string_lossy().into(),
                work_dir: work.to_string_lossy().into(),
            },
        )?;
        Ok(summary_from_meta(id, &dir, &work, meta.read()))
    }

    pub fn get(&self, id: &str) -> anyhow::Result<SessionSummary> {
        let entry = self
            .find_entry(id)?
            .ok_or_else(|| anyhow::anyhow!("session not found: {id}"))?;
        let dir = PathBuf::from(&entry.session_dir);
        let work = PathBuf::from(&entry.work_dir);
        let meta = SessionMetadataService::load_or_create(&dir, id, &work)?;
        let work = meta
            .read()
            .work_dir
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or(work);
        Ok(summary_from_meta(id, &dir, &work, meta.read()))
    }

    pub fn list(
        &self,
        include_archived: bool,
        limit: usize,
    ) -> anyhow::Result<Vec<SessionSummary>> {
        let index = read_session_index(&self.home_dir, &self.sessions_dir)?;
        let mut out = Vec::new();
        for (id, entry) in index {
            let dir = PathBuf::from(&entry.session_dir);
            if !dir.is_dir() {
                continue;
            }
            let work = PathBuf::from(&entry.work_dir);
            let Ok(meta) = SessionMetadataService::load_or_create(&dir, &id, &work) else {
                continue;
            };
            if !include_archived && meta.read().archived {
                continue;
            }
            let work = meta
                .read()
                .work_dir
                .as_ref()
                .map(PathBuf::from)
                .unwrap_or(work);
            out.push(summary_from_meta(&id, &dir, &work, meta.read()));
        }
        out.sort_by_key(|item| std::cmp::Reverse(item.updated_at));
        if out.len() > limit {
            out.truncate(limit);
        }
        Ok(out)
    }

    pub fn rename(&self, id: &str, title: &str) -> anyhow::Result<()> {
        let title = title.trim();
        if title.is_empty() {
            anyhow::bail!("session title cannot be empty");
        }
        let entry = self
            .find_entry(id)?
            .ok_or_else(|| anyhow::anyhow!("session not found: {id}"))?;
        let dir = PathBuf::from(&entry.session_dir);
        let work = PathBuf::from(&entry.work_dir);
        let mut meta = SessionMetadataService::load_or_create(&dir, id, &work)?;
        meta.set_title(title)?;
        Ok(())
    }

    pub fn archive(&self, id: &str, archived: bool) -> anyhow::Result<()> {
        let entry = self
            .find_entry(id)?
            .ok_or_else(|| anyhow::anyhow!("session not found: {id}"))?;
        let dir = PathBuf::from(&entry.session_dir);
        let work = PathBuf::from(&entry.work_dir);
        let mut meta = SessionMetadataService::load_or_create(&dir, id, &work)?;
        meta.set_archived(archived)?;
        Ok(())
    }

    pub fn delete(&self, id: &str) -> anyhow::Result<()> {
        let entry = self
            .find_entry(id)?
            .ok_or_else(|| anyhow::anyhow!("session not found: {id}"))?;
        let dir = PathBuf::from(&entry.session_dir);
        if dir.is_dir() {
            std::fs::remove_dir_all(&dir)?;
        }
        append_session_index_deletion(&self.home_dir, id)?;
        Ok(())
    }

    pub fn fork(
        &self,
        source_id: &str,
        target_id: &str,
        title: Option<&str>,
        turn_index: Option<usize>,
    ) -> anyhow::Result<SessionSummary> {
        if !is_safe_session_id(target_id) {
            anyhow::bail!("unsafe session id: {target_id}");
        }
        if self.find_entry(target_id)?.is_some() {
            anyhow::bail!("session already exists: {target_id}");
        }
        let source = self
            .find_entry(source_id)?
            .ok_or_else(|| anyhow::anyhow!("session not found: {source_id}"))?;
        let source_dir = PathBuf::from(&source.session_dir);
        let work = PathBuf::from(&source.work_dir);
        let target_dir = self.session_dir_for(target_id, &work)?;
        if target_dir.exists() {
            anyhow::bail!("session already exists: {target_id}");
        }
        copy_dir_recursive(&source_dir, &target_dir)?;
        // Drop non-forkable files
        let _ = std::fs::remove_file(target_dir.join("upcoming-goals.json"));

        let mut meta = SessionMetadataService::load_or_create(&target_dir, target_id, &work)?;
        meta.update(
            SessionMetaPatch {
                forked_from: Some(Some(source_id.to_string())),
                title: Some(title.map(|t| t.to_string()).or_else(|| {
                    Some(format!(
                        "fork of {}",
                        meta.read().title.as_deref().unwrap_or(source_id)
                    ))
                })),
                is_custom_title: Some(title.is_some()),
                archived: Some(false),
                ..Default::default()
            },
            true,
        )?;

        if let Some(idx) = turn_index {
            truncate_transcript_at_turn(&target_dir, idx)?;
        }

        append_session_index_entry(
            &self.home_dir,
            &SessionIndexEntry {
                session_id: target_id.to_string(),
                session_dir: target_dir.to_string_lossy().into(),
                work_dir: work.to_string_lossy().into(),
            },
        )?;
        Ok(summary_from_meta(
            target_id,
            &target_dir,
            &work,
            meta.read(),
        ))
    }

    fn find_entry(&self, id: &str) -> anyhow::Result<Option<SessionIndexEntry>> {
        let map = read_session_index(&self.home_dir, &self.sessions_dir)?;
        Ok(map.get(id).cloned())
    }
}

fn summary_from_meta(id: &str, dir: &Path, work: &Path, meta: &SessionMeta) -> SessionSummary {
    SessionSummary {
        id: id.to_string(),
        session_dir: dir.to_string_lossy().into(),
        work_dir: work.to_string_lossy().into(),
        title: meta.title.clone(),
        archived: meta.archived,
        last_prompt: meta.last_prompt.clone(),
        created_at: meta.created_at,
        updated_at: meta.updated_at,
        forked_from: meta.forked_from.clone(),
    }
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive(&entry.path(), &to)?;
        } else if ty.is_file() {
            std::fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}

/// Best-effort: truncate `messages.jsonl` / wire journal if present.
fn truncate_transcript_at_turn(session_dir: &Path, turn_index: usize) -> anyhow::Result<()> {
    let journal = session_dir.join("wire").join("journal.jsonl");
    if journal.is_file() {
        let text = std::fs::read_to_string(&journal)?;
        let lines: Vec<&str> = text.lines().collect();
        // Keep roughly first N turn markers; if unknown format, keep first 2*(turn+1) lines.
        let keep = lines
            .len()
            .min((turn_index.saturating_add(1)).saturating_mul(8));
        let out = lines[..keep].join("\n");
        std::fs::write(
            &journal,
            if out.is_empty() {
                out
            } else {
                format!("{out}\n")
            },
        )?;
    }
    let msgs = session_dir.join("messages.jsonl");
    if msgs.is_file() {
        let text = std::fs::read_to_string(&msgs)?;
        let lines: Vec<&str> = text.lines().collect();
        let keep = lines
            .len()
            .min(turn_index.saturating_add(1).saturating_mul(2));
        let out = lines[..keep].join("\n");
        std::fs::write(
            &msgs,
            if out.is_empty() {
                out
            } else {
                format!("{out}\n")
            },
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_list_fork_archive() {
        let home = std::env::temp_dir().join(format!("kkagent-store-{}", uuid::Uuid::new_v4()));
        let store = SessionStore::new(&home);
        let work = home.join("proj");
        std::fs::create_dir_all(&work).unwrap();
        let s = store.create("s1", &work).unwrap();
        assert_eq!(s.id, "s1");
        assert!(Path::new(&s.session_dir).join("state.json").is_file());
        store.rename("s1", "My Session").unwrap();
        let listed = store.list(false, 10).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].title.as_deref(), Some("My Session"));
        let forked = store.fork("s1", "s2", Some("fork"), None).unwrap();
        assert_eq!(forked.forked_from.as_deref(), Some("s1"));
        store.archive("s1", true).unwrap();
        assert_eq!(store.list(false, 10).unwrap().len(), 1); // only s2
        assert_eq!(store.list(true, 10).unwrap().len(), 2);
        store.delete("s2").unwrap();
        let _ = std::fs::remove_dir_all(&home);
    }
}
