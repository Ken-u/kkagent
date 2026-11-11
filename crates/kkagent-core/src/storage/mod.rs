//! Replaceable session, message and event storage.
//!
//! The runtime talks to [`SessionStore`] and [`EventStore`] instead of a
//! concrete database. Three implementations ship:
//!
//! - [`SqliteSessionStore`]: the historical default (`transcripts.db`).
//! - [`MemoryStore`]: in-process, used by tests and ephemeral runs.
//! - [`HostSessionStore`] / [`HostEventStore`]: an embedding host owns the
//!   data and serves it through `_kkagent/store/*` requests.
//!
//! Only the session main path goes through these traits (create, load,
//! persist, list, rename, fork, delete). SQLite-only features such as
//! full-text search, usage tables and subagent bookkeeping stay on the local
//! operational database.

pub mod conformance;
mod host;
mod memory;
mod sqlite;

#[cfg(test)]
mod contract_tests;

pub use host::{handle_store_request, HostEventStore, HostSessionStore, StoreRpcError};
pub use memory::MemoryStore;
pub use sqlite::SqliteSessionStore;

pub use crate::transcript::{MessageRecord, SessionRecord, ToolResultRecord};
pub use kkagent_protocol::host::StoreNewMessage as NewMessage;
pub use kkagent_protocol::host::StoreSessionPatch as SessionPatch;

use async_trait::async_trait;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StorageError {
    #[error("session not found: {0}")]
    NotFound(String),
    #[error("session already exists: {0}")]
    AlreadyExists(String),
    /// The backend cannot be reached (for example the host is not connected).
    /// Callers must not fall back to another store.
    #[error("storage unavailable: {0}")]
    Unavailable(String),
    #[error("storage error: {0}")]
    Backend(String),
}

pub type StorageResult<T> = Result<T, StorageError>;

impl StorageError {
    /// Classify a backend failure by its message. The SQLite layer reports
    /// plain `anyhow` errors, so this keeps the two well-known cases typed.
    pub(crate) fn from_backend(error: &anyhow::Error, session_id: &str) -> Self {
        let message = error.to_string();
        if message.contains("session not found") {
            Self::NotFound(session_id.to_string())
        } else if message.contains("session already exists")
            || message.contains("UNIQUE constraint failed")
        {
            Self::AlreadyExists(session_id.to_string())
        } else {
            Self::Backend(message)
        }
    }
}

/// Sessions and their messages.
///
/// Contract shared by every backend:
/// - `create_session` on an existing id fails with `AlreadyExists`.
/// - `get_session` of an unknown id is `Ok(None)`; every other call that
///   names an unknown session fails with `NotFound`.
/// - Messages keep insertion order; `load_messages` returns them oldest first
///   and `message_count` always equals the number of stored messages.
/// - `list_sessions` hides archived sessions and orders by `updated_at`,
///   newest first.
#[async_trait]
pub trait SessionStore: Send + Sync {
    /// Short backend name for diagnostics (`sqlite`, `memory`, `host`).
    fn backend(&self) -> &'static str;

    async fn create_session(
        &self,
        session_id: &str,
        model: &str,
        working_dir: &str,
    ) -> StorageResult<()>;

    async fn get_session(&self, session_id: &str) -> StorageResult<Option<SessionRecord>>;

    async fn list_sessions(&self, limit: usize) -> StorageResult<Vec<SessionRecord>>;

    async fn update_session(&self, session_id: &str, patch: &SessionPatch) -> StorageResult<()>;

    /// Remove the session with its messages and tool-result records.
    async fn delete_session(&self, session_id: &str) -> StorageResult<()>;

    async fn append_messages(&self, session_id: &str, messages: &[NewMessage])
        -> StorageResult<()>;

    /// Replace the whole transcript (compaction, undo, rewrite).
    async fn replace_messages(
        &self,
        session_id: &str,
        messages: &[NewMessage],
        summary: Option<&str>,
    ) -> StorageResult<()>;

    async fn load_messages(&self, session_id: &str) -> StorageResult<Vec<MessageRecord>>;

    async fn record_tool_result(&self, record: &ToolResultRecord) -> StorageResult<()>;

    /// Copy the first `message_limit` messages (all when `None`) of `source`
    /// into the new session `new_id`.
    async fn fork_session(
        &self,
        source: &str,
        new_id: &str,
        message_limit: Option<usize>,
    ) -> StorageResult<()>;
}

/// One stored event.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredEvent {
    pub seq: u64,
    pub event: Value,
}

/// An append-only, replayable event log.
///
/// Contract: `append` returns a sequence number that is strictly greater than
/// any earlier one of the same session; `since` returns the events of that
/// session with `seq > after_seq`, oldest first, at most `limit`;
/// `latest_seq` is the highest sequence of the session, `0` when it has none.
#[async_trait]
pub trait EventStore: Send + Sync {
    async fn append(&self, session_id: &str, event: &Value) -> StorageResult<u64>;

    async fn since(
        &self,
        session_id: &str,
        after_seq: u64,
        limit: usize,
    ) -> StorageResult<Vec<StoredEvent>>;

    async fn latest_seq(&self, session_id: &str) -> StorageResult<u64>;
}

/// Convert transcript rows into the `(role, content_json)` form used by
/// callers that still speak in tuples.
pub fn new_messages(pairs: &[(String, String)]) -> Vec<NewMessage> {
    pairs
        .iter()
        .map(|(role, content_json)| NewMessage {
            role: role.clone(),
            content_json: content_json.clone(),
        })
        .collect()
}
