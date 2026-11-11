//! In-process store: tests, ephemeral runs and a reference for host stores.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use serde_json::Value;

use super::{
    EventStore, MessageRecord, NewMessage, SessionPatch, SessionRecord, SessionStore, StorageError,
    StorageResult, StoredEvent, ToolResultRecord,
};

#[derive(Default)]
struct Inner {
    sessions: HashMap<String, SessionRecord>,
    messages: HashMap<String, Vec<MessageRecord>>,
    tool_results: Vec<ToolResultRecord>,
    events: HashMap<String, Vec<StoredEvent>>,
    next_message_id: i64,
    next_event_seq: u64,
    /// Strictly increasing logical clock. `updated_at` keeps the wall clock,
    /// but two writes within one instant must still order for `list_sessions`.
    tick: u64,
    recency: HashMap<String, u64>,
}

/// Implements both [`SessionStore`] and [`EventStore`] in memory.
#[derive(Default)]
pub struct MemoryStore {
    inner: Mutex<Inner>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Tool-result records stored so far (test inspection).
    pub fn tool_results(&self) -> Vec<ToolResultRecord> {
        self.lock().tool_results.clone()
    }
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Mark `session_id` as the most recently written session.
fn bump(inner: &mut Inner, session_id: &str) {
    inner.tick += 1;
    let tick = inner.tick;
    inner.recency.insert(session_id.to_string(), tick);
}

fn not_found(session_id: &str) -> StorageError {
    StorageError::NotFound(session_id.to_string())
}

fn touch(inner: &mut Inner, session_id: &str) {
    let stamp = now();
    if let Some(session) = inner.sessions.get_mut(session_id) {
        session.updated_at = stamp;
    }
    bump(inner, session_id);
}

fn append_locked(
    inner: &mut Inner,
    session_id: &str,
    messages: &[NewMessage],
) -> StorageResult<()> {
    if !inner.sessions.contains_key(session_id) {
        return Err(not_found(session_id));
    }
    let stamp = now();
    for message in messages {
        inner.next_message_id += 1;
        let record = MessageRecord {
            id: inner.next_message_id,
            session_id: session_id.to_string(),
            role: message.role.clone(),
            content_json: message.content_json.clone(),
            token_count: None,
            created_at: stamp.clone(),
        };
        inner
            .messages
            .entry(session_id.to_string())
            .or_default()
            .push(record);
    }
    let count = inner.messages.get(session_id).map_or(0, Vec::len) as u32;
    if let Some(session) = inner.sessions.get_mut(session_id) {
        session.message_count = count;
    }
    touch(inner, session_id);
    Ok(())
}

#[async_trait]
impl SessionStore for MemoryStore {
    fn backend(&self) -> &'static str {
        "memory"
    }

    async fn create_session(
        &self,
        session_id: &str,
        model: &str,
        working_dir: &str,
    ) -> StorageResult<()> {
        let mut inner = self.lock();
        if inner.sessions.contains_key(session_id) {
            return Err(StorageError::AlreadyExists(session_id.to_string()));
        }
        let stamp = now();
        bump(&mut inner, session_id);
        inner.sessions.insert(
            session_id.to_string(),
            SessionRecord {
                session_id: session_id.to_string(),
                title: None,
                model: model.to_string(),
                fallback_model: None,
                working_dir: working_dir.to_string(),
                created_at: stamp.clone(),
                updated_at: stamp,
                message_count: 0,
                is_archived: false,
                parent_session_id: None,
            },
        );
        inner.messages.insert(session_id.to_string(), Vec::new());
        Ok(())
    }

    async fn get_session(&self, session_id: &str) -> StorageResult<Option<SessionRecord>> {
        Ok(self.lock().sessions.get(session_id).cloned())
    }

    async fn list_sessions(&self, limit: usize) -> StorageResult<Vec<SessionRecord>> {
        let inner = self.lock();
        let mut sessions: Vec<SessionRecord> = inner
            .sessions
            .values()
            .filter(|session| !session.is_archived)
            .cloned()
            .collect();
        sessions.sort_by_key(|session| {
            std::cmp::Reverse(inner.recency.get(&session.session_id).copied().unwrap_or(0))
        });
        sessions.truncate(limit);
        Ok(sessions)
    }

    async fn update_session(&self, session_id: &str, patch: &SessionPatch) -> StorageResult<()> {
        let mut inner = self.lock();
        if !inner.sessions.contains_key(session_id) {
            return Err(not_found(session_id));
        }
        let stamp = now();
        bump(&mut inner, session_id);
        let session = inner.sessions.get_mut(session_id).expect("checked above");
        if let Some(title) = &patch.title {
            session.title = Some(title.clone());
        }
        if let Some(model) = &patch.model {
            session.model = model.clone();
        }
        if let Some(fallback) = &patch.fallback_model {
            session.fallback_model = fallback.clone();
        }
        if let Some(archived) = patch.archived {
            session.is_archived = archived;
        }
        if let Some(parent) = &patch.parent_session_id {
            session.parent_session_id = Some(parent.clone());
        }
        session.updated_at = stamp;
        Ok(())
    }

    async fn delete_session(&self, session_id: &str) -> StorageResult<()> {
        let mut inner = self.lock();
        if inner.sessions.remove(session_id).is_none() {
            return Err(not_found(session_id));
        }
        inner.recency.remove(session_id);
        inner.messages.remove(session_id);
        inner.events.remove(session_id);
        inner
            .tool_results
            .retain(|record| record.session_id != session_id);
        Ok(())
    }

    async fn append_messages(
        &self,
        session_id: &str,
        messages: &[NewMessage],
    ) -> StorageResult<()> {
        let mut inner = self.lock();
        append_locked(&mut inner, session_id, messages)
    }

    async fn replace_messages(
        &self,
        session_id: &str,
        messages: &[NewMessage],
        _summary: Option<&str>,
    ) -> StorageResult<()> {
        let mut inner = self.lock();
        if !inner.sessions.contains_key(session_id) {
            return Err(not_found(session_id));
        }
        inner.messages.insert(session_id.to_string(), Vec::new());
        append_locked(&mut inner, session_id, messages)
    }

    async fn load_messages(&self, session_id: &str) -> StorageResult<Vec<MessageRecord>> {
        let inner = self.lock();
        if !inner.sessions.contains_key(session_id) {
            return Err(not_found(session_id));
        }
        Ok(inner.messages.get(session_id).cloned().unwrap_or_default())
    }

    async fn record_tool_result(&self, record: &ToolResultRecord) -> StorageResult<()> {
        self.lock().tool_results.push(record.clone());
        Ok(())
    }

    async fn fork_session(
        &self,
        source: &str,
        new_id: &str,
        message_limit: Option<usize>,
    ) -> StorageResult<()> {
        let (model, working_dir, title, fallback, copied) = {
            let inner = self.lock();
            let record = inner
                .sessions
                .get(source)
                .ok_or_else(|| not_found(source))?;
            if inner.sessions.contains_key(new_id) {
                return Err(StorageError::AlreadyExists(new_id.to_string()));
            }
            let mut copied: Vec<NewMessage> = inner
                .messages
                .get(source)
                .map(|messages| {
                    messages
                        .iter()
                        .map(|message| NewMessage {
                            role: message.role.clone(),
                            content_json: message.content_json.clone(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            if let Some(limit) = message_limit {
                copied.truncate(limit);
            }
            (
                record.model.clone(),
                record.working_dir.clone(),
                record.title.clone(),
                record.fallback_model.clone(),
                copied,
            )
        };
        self.create_session(new_id, &model, &working_dir).await?;
        self.update_session(
            new_id,
            &SessionPatch {
                title,
                fallback_model: Some(fallback),
                ..Default::default()
            },
        )
        .await?;
        self.append_messages(new_id, &copied).await
    }
}

#[async_trait]
impl EventStore for MemoryStore {
    async fn append(&self, session_id: &str, event: &Value) -> StorageResult<u64> {
        let mut inner = self.lock();
        inner.next_event_seq += 1;
        let seq = inner.next_event_seq;
        inner
            .events
            .entry(session_id.to_string())
            .or_default()
            .push(StoredEvent {
                seq,
                event: event.clone(),
            });
        Ok(seq)
    }

    async fn since(
        &self,
        session_id: &str,
        after_seq: u64,
        limit: usize,
    ) -> StorageResult<Vec<StoredEvent>> {
        let inner = self.lock();
        Ok(inner
            .events
            .get(session_id)
            .map(|events| {
                events
                    .iter()
                    .filter(|event| event.seq > after_seq)
                    .take(limit)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn latest_seq(&self, session_id: &str) -> StorageResult<u64> {
        Ok(self
            .lock()
            .events
            .get(session_id)
            .and_then(|events| events.last())
            .map_or(0, |event| event.seq))
    }
}
