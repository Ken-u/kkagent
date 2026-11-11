//! The default store: the local transcript SQLite database.

use async_trait::async_trait;

use super::{
    MessageRecord, NewMessage, SessionPatch, SessionRecord, SessionStore, StorageError,
    StorageResult, ToolResultRecord,
};
use crate::transcript::TranscriptDb;

/// [`SessionStore`] over a [`TranscriptDb`]. Behavior is exactly the
/// historical one; this is a thin typed wrapper.
#[derive(Clone)]
pub struct SqliteSessionStore {
    db: TranscriptDb,
}

impl SqliteSessionStore {
    pub fn new(db: TranscriptDb) -> Self {
        Self { db }
    }

    /// The wrapped database, for SQLite-only features (search, usage, ...).
    pub fn db(&self) -> &TranscriptDb {
        &self.db
    }

    fn require(&self, session_id: &str) -> StorageResult<SessionRecord> {
        self.db
            .get_session(session_id)
            .map_err(|error| StorageError::from_backend(&error, session_id))?
            .ok_or_else(|| StorageError::NotFound(session_id.to_string()))
    }
}

fn pairs(messages: &[NewMessage]) -> Vec<(String, String)> {
    messages
        .iter()
        .map(|message| (message.role.clone(), message.content_json.clone()))
        .collect()
}

#[async_trait]
impl SessionStore for SqliteSessionStore {
    fn backend(&self) -> &'static str {
        "sqlite"
    }

    async fn create_session(
        &self,
        session_id: &str,
        model: &str,
        working_dir: &str,
    ) -> StorageResult<()> {
        self.db
            .create_session(session_id, model, working_dir)
            .map_err(|error| StorageError::from_backend(&error, session_id))
    }

    async fn get_session(&self, session_id: &str) -> StorageResult<Option<SessionRecord>> {
        self.db
            .get_session(session_id)
            .map_err(|error| StorageError::from_backend(&error, session_id))
    }

    async fn list_sessions(&self, limit: usize) -> StorageResult<Vec<SessionRecord>> {
        self.db
            .list_sessions(limit)
            .map_err(|error| StorageError::Backend(error.to_string()))
    }

    async fn update_session(&self, session_id: &str, patch: &SessionPatch) -> StorageResult<()> {
        self.require(session_id)?;
        let map = |error: anyhow::Error| StorageError::from_backend(&error, session_id);
        if let Some(title) = &patch.title {
            self.db.set_title(session_id, title).map_err(map)?;
        }
        if let Some(model) = &patch.model {
            self.db.set_model(session_id, model).map_err(map)?;
        }
        if let Some(fallback) = &patch.fallback_model {
            self.db
                .set_fallback_model(session_id, fallback.as_deref())
                .map_err(map)?;
        }
        if let Some(archived) = patch.archived {
            self.db.set_archived(session_id, archived).map_err(map)?;
        }
        if let Some(parent) = &patch.parent_session_id {
            self.db
                .set_parent_session_id(session_id, parent)
                .map_err(map)?;
        }
        Ok(())
    }

    async fn delete_session(&self, session_id: &str) -> StorageResult<()> {
        self.db
            .purge_session(session_id)
            .map_err(|error| StorageError::from_backend(&error, session_id))
    }

    async fn append_messages(
        &self,
        session_id: &str,
        messages: &[NewMessage],
    ) -> StorageResult<()> {
        self.require(session_id)?;
        self.db
            .append_messages(session_id, &pairs(messages))
            .map_err(|error| StorageError::from_backend(&error, session_id))
    }

    async fn replace_messages(
        &self,
        session_id: &str,
        messages: &[NewMessage],
        summary: Option<&str>,
    ) -> StorageResult<()> {
        self.require(session_id)?;
        self.db
            .replace_messages(session_id, &pairs(messages), summary)
            .map_err(|error| StorageError::from_backend(&error, session_id))
    }

    async fn load_messages(&self, session_id: &str) -> StorageResult<Vec<MessageRecord>> {
        self.require(session_id)?;
        self.db
            .load_messages(session_id)
            .map_err(|error| StorageError::from_backend(&error, session_id))
    }

    async fn record_tool_result(&self, record: &ToolResultRecord) -> StorageResult<()> {
        self.db
            .record_tool_result(record)
            .map_err(|error| StorageError::from_backend(&error, &record.session_id))
    }

    async fn fork_session(
        &self,
        source: &str,
        new_id: &str,
        message_limit: Option<usize>,
    ) -> StorageResult<()> {
        let record = self.require(source)?;
        let map = |error: anyhow::Error| StorageError::from_backend(&error, new_id);
        match message_limit {
            // Clamp so a limit past the end copies everything instead of
            // failing the strict variant.
            Some(limit) => self
                .db
                .fork_session_with_message_limit(
                    source,
                    new_id,
                    None,
                    limit.min(record.message_count as usize),
                )
                .map_err(map),
            None => self
                .db
                .fork_session(source, new_id, None, None)
                .map_err(map),
        }
    }
}
