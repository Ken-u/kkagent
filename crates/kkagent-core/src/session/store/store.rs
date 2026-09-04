//! On-disk session store — create / list / fork / archive / rename / delete.

use super::index::{
    append_session_index_deletion, append_session_index_entry, read_session_index,
    SessionIndexEntry,
};
use super::workdir_key::{encode_work_dir_key, is_safe_session_id, normalize_work_dir};
use crate::session::metadata::{SessionMeta, SessionMetaPatch, SessionMetadataService};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

// In-process writes invalidate immediately; the TTL only bounds staleness for
// another kkagent process editing the same store.
const LIST_CACHE_TTL: Duration = Duration::from_secs(10);

struct CachedSessionList {
    generation: u64,
    loaded_at: Instant,
    summaries: Vec<SessionSummary>,
}

fn list_cache() -> &'static Mutex<HashMap<PathBuf, CachedSessionList>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, CachedSessionList>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    pub id: String,
    pub session_dir: String,
    pub work_dir: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// True when the user set a title via `/title` (or equivalent).
    #[serde(default)]
    pub is_custom_title: bool,
    #[serde(default)]
    pub archived: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_prompt: Option<String>,
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
        let result = (|| {
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
        })();
        cleanup_failed_session_dir(&dir, result)
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
        let generation = super::list_generation();
        {
            let cache = list_cache()
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if let Some(cached) = cache.get(&self.home_dir).filter(|cached| {
                cached.generation == generation && cached.loaded_at.elapsed() < LIST_CACHE_TTL
            }) {
                if generation == super::list_generation() {
                    return Ok(cached
                        .summaries
                        .iter()
                        .filter(|summary| include_archived || !summary.archived)
                        .take(limit)
                        .cloned()
                        .collect());
                }
            }
        }

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
            let work = meta
                .read()
                .work_dir
                .as_ref()
                .map(PathBuf::from)
                .unwrap_or(work);
            out.push(summary_from_meta(&id, &dir, &work, meta.read()));
        }
        out.sort_by_key(|item| std::cmp::Reverse(item.updated_at));
        let result = out
            .iter()
            .filter(|summary| include_archived || !summary.archived)
            .take(limit)
            .cloned()
            .collect();
        list_cache()
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(
                self.home_dir.clone(),
                CachedSessionList {
                    generation,
                    loaded_at: Instant::now(),
                    summaries: out,
                },
            );
        Ok(result)
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

    /// Recreate the index entry and on-disk metadata for a session that is
    /// present in the transcript DB but missing from `session_index.jsonl`
    /// (e.g. tombstoned by an over-eager discard, or lost when data was
    /// copied between machines). Never overwrites an existing index entry or
    /// directory. Returns `Ok(None)` when the entry already exists.
    pub fn backfill(&self, id: &str, work_dir: &Path) -> anyhow::Result<Option<SessionSummary>> {
        if !is_safe_session_id(id) {
            anyhow::bail!("unsafe session id: {id}");
        }
        if self.find_entry(id)?.is_some() {
            return Ok(None);
        }
        let work = normalize_work_dir(work_dir);
        let dir = self.session_dir_for(id, &work)?;
        std::fs::create_dir_all(&dir)?;
        let result = (|| {
            let meta = SessionMetadataService::load_or_create(&dir, id, &work)?;
            append_session_index_entry(
                &self.home_dir,
                &SessionIndexEntry {
                    session_id: id.to_string(),
                    session_dir: dir.to_string_lossy().into(),
                    work_dir: work.to_string_lossy().into(),
                },
            )?;
            anyhow::Ok(Some(summary_from_meta(id, &dir, &work, meta.read())))
        })();
        cleanup_failed_session_dir(&dir, result)
    }

    /// Sweep legacy `sub-*` index entries left behind by old builds that
    /// persisted subagent runs as real sessions (shell with no transcript
    /// content). Each candidate is only removed when its metadata confirms it
    /// is content-less, so a user-created session whose id merely starts with
    /// `sub-` is never touched. Returns the swept session ids.
    pub fn sweep_orphan_subagent_sessions(&self) -> anyhow::Result<Vec<String>> {
        let index = read_session_index(&self.home_dir, &self.sessions_dir)?;
        let mut swept = Vec::new();
        for (id, entry) in index {
            if !id.starts_with("sub-") {
                continue;
            }
            let dir = PathBuf::from(&entry.session_dir);
            // Safety guard: only delete when the session dir holds no
            // meaningful content (no messages.jsonl rows and no recorded
            // tool-result files). Missing dir entries are just unindexed.
            let messages_path = dir.join("messages.jsonl");
            let has_messages = messages_path.is_file()
                && std::fs::read_to_string(&messages_path)
                    .map(|text| text.lines().any(|line| !line.trim().is_empty()))
                    .unwrap_or(true);
            if has_messages {
                continue;
            }
            let tool_results_dir = dir.join("tool_results");
            let has_tool_results = tool_results_dir
                .read_dir()
                .map(|entries| entries.filter_map(Result::ok).count() > 0)
                .unwrap_or(false);
            if has_tool_results {
                continue;
            }
            if dir.is_dir() {
                std::fs::remove_dir_all(&dir)?;
            }
            append_session_index_deletion(&self.home_dir, &id)?;
            swept.push(id);
        }
        Ok(swept)
    }

    pub fn fork(
        &self,
        source_id: &str,
        target_id: &str,
        title: Option<&str>,
        turn_index: Option<usize>,
    ) -> anyhow::Result<SessionSummary> {
        self.fork_inner(source_id, target_id, title, turn_index, None)
    }

    pub fn fork_with_message_limit(
        &self,
        source_id: &str,
        target_id: &str,
        title: Option<&str>,
        message_limit: usize,
    ) -> anyhow::Result<SessionSummary> {
        self.fork_inner(source_id, target_id, title, None, Some(message_limit))
    }

    fn fork_inner(
        &self,
        source_id: &str,
        target_id: &str,
        title: Option<&str>,
        turn_index: Option<usize>,
        message_limit: Option<usize>,
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
        let result = (|| {
            copy_dir_recursive(&source_dir, &target_dir)?;
            // Drop non-forkable files
            let _ = std::fs::remove_file(target_dir.join("upcoming-goals.json"));
            let _ = std::fs::remove_file(target_dir.join("goal.json"));

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
            } else if let Some(limit) = message_limit {
                truncate_transcript_at_message_limit(&target_dir, limit)?;
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
        })();
        cleanup_failed_session_dir(&target_dir, result)
    }

    fn find_entry(&self, id: &str) -> anyhow::Result<Option<SessionIndexEntry>> {
        let map = read_session_index(&self.home_dir, &self.sessions_dir)?;
        Ok(map.get(id).cloned())
    }
}

fn truncate_transcript_at_message_limit(
    session_dir: &Path,
    message_limit: usize,
) -> anyhow::Result<()> {
    let msgs = session_dir.join("messages.jsonl");
    if msgs.is_file() {
        let text = std::fs::read_to_string(&msgs)?;
        let lines: Vec<&str> = text.lines().collect();
        let keep = message_limit.min(lines.len());
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
    // This journal is an event stream rather than a one-record-per-message
    // transcript, so an exact message boundary cannot be inferred safely.
    // A fork can rebuild it from new events; keeping future events would be
    // misleading.
    let journal = session_dir.join("wire").join("journal.jsonl");
    if journal.is_file() {
        std::fs::remove_file(journal)?;
    }
    Ok(())
}

fn summary_from_meta(id: &str, dir: &Path, work: &Path, meta: &SessionMeta) -> SessionSummary {
    SessionSummary {
        id: id.to_string(),
        session_dir: dir.to_string_lossy().into(),
        work_dir: work.to_string_lossy().into(),
        title: meta.title.clone(),
        is_custom_title: meta.is_custom_title,
        archived: meta.archived,
        last_prompt: meta.last_prompt.clone(),
        first_prompt: meta.first_prompt.clone().or_else(|| {
            // Legacy sessions: auto-title was derived from the first real user text.
            if !meta.is_custom_title {
                meta.title.clone()
            } else {
                None
            }
        }),
        created_at: meta.created_at,
        updated_at: meta.updated_at,
        forked_from: meta.forked_from.clone(),
    }
}

fn cleanup_failed_session_dir<T>(dir: &Path, result: anyhow::Result<T>) -> anyhow::Result<T> {
    if result.is_err() && dir.exists() {
        if let Err(cleanup_error) = std::fs::remove_dir_all(dir) {
            tracing::warn!(
                path = %dir.display(),
                %cleanup_error,
                "failed to clean up incomplete session directory"
            );
        }
    }
    result
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
    fn backfill_restores_tombstoned_session() {
        let home = std::env::temp_dir().join(format!("kkagent-store-{}", uuid::Uuid::new_v4()));
        let store = SessionStore::new(&home);
        let work = home.join("proj");
        std::fs::create_dir_all(&work).unwrap();

        let created = store.create("bf1", &work).unwrap();
        std::fs::write(
            Path::new(&created.session_dir).join("messages.jsonl"),
            "{\"role\":\"user\"}\n",
        )
        .unwrap();
        // Simulate a discard: delete the store (removes dir + tombstones index).
        store.delete("bf1").unwrap();
        assert!(store.get("bf1").is_err());

        // Recreate the directory the way a running server would when the
        // session keeps being used after the delete.
        let dir = store.session_dir_for("bf1", &work).unwrap();
        std::fs::create_dir_all(&dir).unwrap();

        // Backfill is a no-op while the index still holds an entry… (here the
        // entry is tombstoned, so it proceeds) and refuses to overwrite an
        // existing entry.
        let summary = store.backfill("bf1", &work).unwrap().expect("backfilled");
        assert_eq!(summary.id, "bf1");
        assert_eq!(summary.work_dir, work);
        assert!(store.get("bf1").is_ok());

        // Second backfill must not clobber the existing entry.
        assert!(store.backfill("bf1", &work).unwrap().is_none());
        assert!(store.get("bf1").is_ok());

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn create_rolls_back_directory_when_index_append_fails() {
        let home = std::env::temp_dir().join(format!("kkagent-store-{}", uuid::Uuid::new_v4()));
        let store = SessionStore::new(&home);
        let work = home.join("proj");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(home.join("session_index.jsonl")).unwrap();
        let session_dir = store.session_dir_for("rollback", &work).unwrap();

        assert!(store.create("rollback", &work).is_err());
        assert!(
            !session_dir.exists(),
            "failed create left an unindexed session directory"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn fork_rolls_back_target_when_copied_metadata_is_invalid() {
        let home = std::env::temp_dir().join(format!("kkagent-store-{}", uuid::Uuid::new_v4()));
        let store = SessionStore::new(&home);
        let work = home.join("proj");
        std::fs::create_dir_all(&work).unwrap();
        let source = store.create("source", &work).unwrap();
        std::fs::write(
            Path::new(&source.session_dir).join("state.json"),
            "not json",
        )
        .unwrap();
        let target_dir = store.session_dir_for("target", &work).unwrap();

        assert!(store.fork("source", "target", None, None).is_err());
        assert!(
            !target_dir.exists(),
            "failed fork left a partial target directory"
        );
        assert!(Path::new(&source.session_dir).is_dir());
        let _ = std::fs::remove_dir_all(&home);
    }

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
        assert!(listed[0].is_custom_title);
        let forked = store.fork("s1", "s2", Some("fork"), None).unwrap();
        assert_eq!(forked.forked_from.as_deref(), Some("s1"));
        store.archive("s1", true).unwrap();
        assert_eq!(store.list(false, 10).unwrap().len(), 1); // only s2
        assert_eq!(store.list(true, 10).unwrap().len(), 2);
        store.delete("s2").unwrap();
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn fork_with_message_limit_truncates_sidecar_without_touching_source() {
        let home = std::env::temp_dir().join(format!("kkagent-store-{}", uuid::Uuid::new_v4()));
        let store = SessionStore::new(&home);
        let work = home.join("proj");
        std::fs::create_dir_all(&work).unwrap();
        let source = store.create("source", &work).unwrap();
        let source_dir = PathBuf::from(&source.session_dir);
        std::fs::write(
            source_dir.join("messages.jsonl"),
            "first\nassistant/tool\nsecond\n",
        )
        .unwrap();
        std::fs::create_dir_all(source_dir.join("wire")).unwrap();
        std::fs::write(source_dir.join("wire/journal.jsonl"), "future-event\n").unwrap();

        let fork = store
            .fork_with_message_limit("source", "edit", Some("Edit turn"), 2)
            .unwrap();
        let fork_dir = PathBuf::from(fork.session_dir);
        assert_eq!(
            std::fs::read_to_string(fork_dir.join("messages.jsonl")).unwrap(),
            "first\nassistant/tool\n"
        );
        assert!(!fork_dir.join("wire/journal.jsonl").exists());
        assert_eq!(
            std::fs::read_to_string(source_dir.join("messages.jsonl")).unwrap(),
            "first\nassistant/tool\nsecond\n"
        );
        assert!(source_dir.join("wire/journal.jsonl").is_file());

        let empty = store
            .fork_with_message_limit("source", "edit-first", None, 0)
            .unwrap();
        assert!(
            std::fs::read_to_string(Path::new(&empty.session_dir).join("messages.jsonl"))
                .unwrap()
                .is_empty()
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn sweep_orphan_subagent_sessions_removes_only_empty_sub_entries() {
        let home = std::env::temp_dir().join(format!("kkagent-store-{}", uuid::Uuid::new_v4()));
        let store = SessionStore::new(&home);
        let work = home.join("proj");
        std::fs::create_dir_all(&work).unwrap();

        // 1. Empty subagent shell (legacy junk) — should be swept.
        let empty_sub = store.create("sub-abc123", &work).unwrap();
        // 2. Sub entry WITH messages — must be preserved.
        let content_sub = store.create("sub-keepme", &work).unwrap();
        std::fs::write(
            Path::new(&content_sub.session_dir).join("messages.jsonl"),
            "{\"role\":\"user\"}\n",
        )
        .unwrap();
        // 3. Regular session — must be preserved.
        store.create("normal-session", &work).unwrap();
        // 4. Sub entry with tool results — must be preserved.
        let tool_sub = store.create("sub-tools", &work).unwrap();
        let tool_dir = Path::new(&tool_sub.session_dir).join("tool_results");
        std::fs::create_dir_all(&tool_dir).unwrap();
        std::fs::write(tool_dir.join("r1.json"), "{}").unwrap();

        let swept = store.sweep_orphan_subagent_sessions().unwrap();
        assert_eq!(swept, vec!["sub-abc123".to_string()]);
        assert!(!Path::new(&empty_sub.session_dir).exists());
        assert!(Path::new(&content_sub.session_dir).exists());
        assert!(Path::new(&tool_sub.session_dir).exists());
        assert!(store.get("normal-session").is_ok());
        assert!(store.get("sub-keepme").is_ok());
        assert!(store.get("sub-tools").is_ok());

        // Idempotent: second sweep finds nothing.
        let swept_again = store.sweep_orphan_subagent_sessions().unwrap();
        assert!(swept_again.is_empty());

        let _ = std::fs::remove_dir_all(&home);
    }
}
