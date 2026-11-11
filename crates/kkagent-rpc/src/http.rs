//! REST API v1 + WebSocket (kap-server route matrix subset) with AgentLoop backend hooks.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::IntoResponse;
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::process::Child;
use tokio::sync::{broadcast, Mutex};

const MAX_HTTP_TERMINALS: usize = 64;
const MAX_TERMINAL_COMMAND_BYTES: usize = 64 * 1024;
const MAX_TERMINAL_OUTPUT_BYTES: usize = 1024 * 1024;
const EVENT_HISTORY_CAPACITY: usize = 2048;
const DEFAULT_TASK_MAX_ATTEMPTS: u32 = 3;
const MULTIMODAL_PROMPT_PREFIX: &str = "kkagent:multimodal:v1\n";
/// How long a `POST /sessions` idempotency key stays replayable.
const SESSION_CREATE_RETENTION_SECS: i64 = 7 * 24 * 3600;
/// A claim without a result younger than this belongs to a call still running.
const SESSION_CREATE_IN_PROGRESS_SECS: i64 = 30;

/// Answer of [`DurableHttpStore::begin_session_create`].
#[derive(Debug, Clone, PartialEq)]
pub enum SessionCreateClaim {
    /// First use of the key: go ahead and create the session.
    Fresh,
    /// The key already produced this response.
    Replay(Value),
    /// Another call with the key is still creating the session.
    InProgress,
    /// The key was used with different parameters.
    Conflict,
}

#[derive(Debug, Clone)]
pub struct DurableTurn {
    pub task_id: String,
    pub session_id: String,
    pub prompt: String,
    pub state: String,
    pub attempts: u32,
    pub max_attempts: u32,
    pub created_at: String,
    pub updated_at: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HttpImageInput {
    #[serde(alias = "mime_type")]
    pub media_type: String,
    pub data: String,
}

impl DurableTurn {
    pub fn message_input(&self) -> (String, Vec<HttpImageInput>) {
        self.prompt
            .strip_prefix(MULTIMODAL_PROMPT_PREFIX)
            .and_then(|payload| serde_json::from_str::<PostMessageBody>(payload).ok())
            .map(|body| (body.text, body.images))
            .unwrap_or_else(|| (self.prompt.clone(), Vec::new()))
    }
}

impl DurableTurn {
    fn as_json(&self) -> Value {
        json!({
            "task_id": self.task_id,
            "session_id": self.session_id,
            "state": self.state,
            "attempts": self.attempts,
            "max_attempts": self.max_attempts,
            "created_at": self.created_at,
            "updated_at": self.updated_at,
            "error": self.error,
        })
    }
}

#[derive(Clone)]
pub struct DurableHttpStore {
    connection: Arc<StdMutex<Connection>>,
}

impl DurableHttpStore {
    pub fn open(path: &std::path::Path) -> anyhow::Result<Self> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(path)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        Self::from_shared(Arc::new(StdMutex::new(connection)))
    }

    pub fn open_in_memory() -> anyhow::Result<Self> {
        let connection = Connection::open_in_memory()?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        Self::from_shared(Arc::new(StdMutex::new(connection)))
    }

    /// Share an already-opened SQLite connection (e.g. with TranscriptDb / SubagentManager).
    pub fn from_shared(connection: Arc<StdMutex<Connection>>) -> anyhow::Result<Self> {
        {
            let conn = connection
                .lock()
                .map_err(|_| anyhow::anyhow!("durable HTTP store lock poisoned"))?;
            conn.execute_batch(
                "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = FULL;
             CREATE TABLE IF NOT EXISTS http_events (
                event_seq INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT,
                event_json TEXT NOT NULL,
                emitted_at TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_http_events_session_seq
                ON http_events(session_id, event_seq);
             CREATE TABLE IF NOT EXISTS durable_turns (
                task_id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                prompt TEXT NOT NULL,
                prompt_hash TEXT NOT NULL,
                idempotency_key TEXT,
                state TEXT NOT NULL,
                attempts INTEGER NOT NULL DEFAULT 0,
                max_attempts INTEGER NOT NULL DEFAULT 3,
                lease_expires_at INTEGER,
                error TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
             );
             CREATE UNIQUE INDEX IF NOT EXISTS idx_durable_turns_idempotency
                ON durable_turns(session_id, idempotency_key)
                WHERE idempotency_key IS NOT NULL;
             CREATE INDEX IF NOT EXISTS idx_durable_turns_recovery
                ON durable_turns(state, lease_expires_at, created_at);
             CREATE TABLE IF NOT EXISTS http_session_creates (
                idempotency_key TEXT PRIMARY KEY,
                request_hash TEXT NOT NULL,
                response_json TEXT,
                claimed_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS http_server_meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
             );",
            )?;
            // Idempotency keys only need to outlive a client's retry window.
            conn.execute(
                "DELETE FROM http_session_creates WHERE claimed_at < ?1",
                params![chrono::Utc::now().timestamp() - SESSION_CREATE_RETENTION_SECS],
            )?;
        }
        Ok(Self { connection })
    }

    /// Identity of this server's data: created with the database, kept across
    /// restarts. A different value means the host is talking to another
    /// server (or a wiped database), a different `boot_id` only to a restart.
    pub fn instance_id(&self) -> anyhow::Result<String> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("durable HTTP store lock poisoned"))?;
        let existing: Option<String> = connection
            .query_row(
                "SELECT value FROM http_server_meta WHERE key = 'instance_id'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(existing) = existing {
            return Ok(existing);
        }
        let created = uuid::Uuid::new_v4().to_string();
        connection.execute(
            "INSERT OR IGNORE INTO http_server_meta(key, value) VALUES ('instance_id', ?1)",
            params![created],
        )?;
        Ok(connection.query_row(
            "SELECT value FROM http_server_meta WHERE key = 'instance_id'",
            [],
            |row| row.get(0),
        )?)
    }

    /// Claim `key` for one `POST /sessions` call. See [`SessionCreateClaim`].
    pub fn begin_session_create(
        &self,
        key: &str,
        request_hash: &str,
    ) -> anyhow::Result<SessionCreateClaim> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("durable HTTP store lock poisoned"))?;
        let transaction = connection.transaction()?;
        let now = chrono::Utc::now().timestamp();
        let existing: Option<(String, Option<String>, i64)> = transaction
            .query_row(
                "SELECT request_hash, response_json, claimed_at FROM http_session_creates
                 WHERE idempotency_key = ?1",
                params![key],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let claim = match existing {
            None => {
                transaction.execute(
                    "INSERT INTO http_session_creates(idempotency_key, request_hash, response_json, claimed_at)
                     VALUES (?1, ?2, NULL, ?3)",
                    params![key, request_hash, now],
                )?;
                SessionCreateClaim::Fresh
            }
            Some((hash, _, _)) if hash != request_hash => SessionCreateClaim::Conflict,
            Some((_, Some(response), _)) => {
                SessionCreateClaim::Replay(serde_json::from_str(&response)?)
            }
            Some((_, None, claimed_at)) if now - claimed_at < SESSION_CREATE_IN_PROGRESS_SECS => {
                SessionCreateClaim::InProgress
            }
            // An attempt that never finished (the process died): take it over.
            Some((_, None, _)) => {
                transaction.execute(
                    "UPDATE http_session_creates SET claimed_at = ?2 WHERE idempotency_key = ?1",
                    params![key, now],
                )?;
                SessionCreateClaim::Fresh
            }
        };
        transaction.commit()?;
        Ok(claim)
    }

    /// Record the session a claimed key produced, so retries replay it.
    pub fn complete_session_create(&self, key: &str, response: &Value) -> anyhow::Result<()> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("durable HTTP store lock poisoned"))?;
        connection.execute(
            "UPDATE http_session_creates SET response_json = ?2 WHERE idempotency_key = ?1",
            params![key, serde_json::to_string(response)?],
        )?;
        Ok(())
    }

    /// Release a claim whose creation failed, so the client may retry.
    pub fn abandon_session_create(&self, key: &str) -> anyhow::Result<()> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("durable HTTP store lock poisoned"))?;
        connection.execute(
            "DELETE FROM http_session_creates WHERE idempotency_key = ?1 AND response_json IS NULL",
            params![key],
        )?;
        Ok(())
    }

    /// Forget a key outright (its session no longer exists).
    pub fn forget_session_create(&self, key: &str) -> anyhow::Result<()> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("durable HTTP store lock poisoned"))?;
        connection.execute(
            "DELETE FROM http_session_creates WHERE idempotency_key = ?1",
            params![key],
        )?;
        Ok(())
    }

    fn append_event(&self, event: Value) -> anyhow::Result<Value> {
        let emitted_at = chrono::Utc::now().to_rfc3339();
        let session_id = event
            .get("session_id")
            .and_then(Value::as_str)
            .map(str::to_string);
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("durable HTTP store lock poisoned"))?;
        let transaction = connection.transaction()?;
        transaction.execute(
            "INSERT INTO http_events(session_id, event_json, emitted_at) VALUES (?1, '', ?2)",
            params![session_id, emitted_at],
        )?;
        let sequence = transaction.last_insert_rowid() as u64;
        let mut event = match event {
            Value::Object(object) => Value::Object(object),
            data => json!({"type": "event", "data": data}),
        };
        if let Some(object) = event.as_object_mut() {
            object.insert("event_seq".into(), json!(sequence));
            object.insert("emitted_at".into(), json!(emitted_at));
        }
        transaction.execute(
            "UPDATE http_events SET event_json = ?1 WHERE event_seq = ?2",
            params![serde_json::to_string(&event)?, sequence],
        )?;
        transaction.commit()?;
        Ok(event)
    }

    fn latest_event_sequence(&self) -> anyhow::Result<u64> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("durable HTTP store lock poisoned"))?;
        Ok(connection.query_row(
            "SELECT COALESCE(MAX(event_seq), 0) FROM http_events",
            [],
            |row| row.get(0),
        )?)
    }

    fn events_since(
        &self,
        since: u64,
        session_id: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<Vec<Value>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("durable HTTP store lock poisoned"))?;
        let mut events = Vec::new();
        if let Some(session_id) = session_id {
            let mut statement = connection.prepare(
                "SELECT event_json FROM http_events WHERE event_seq > ?1 AND session_id = ?2 ORDER BY event_seq LIMIT ?3",
            )?;
            let rows = statement.query_map(params![since, session_id, limit as u64], |row| {
                row.get::<_, String>(0)
            })?;
            for row in rows {
                events.push(serde_json::from_str(&row?)?);
            }
        } else {
            let mut statement = connection.prepare(
                "SELECT event_json FROM http_events WHERE event_seq > ?1 ORDER BY event_seq LIMIT ?2",
            )?;
            let rows =
                statement.query_map(params![since, limit as u64], |row| row.get::<_, String>(0))?;
            for row in rows {
                events.push(serde_json::from_str(&row?)?);
            }
        }
        Ok(events)
    }

    pub fn enqueue_turn(
        &self,
        session_id: &str,
        prompt: &str,
        idempotency_key: Option<&str>,
    ) -> anyhow::Result<(DurableTurn, bool)> {
        use sha2::{Digest, Sha256};
        let prompt_hash = hex::encode(Sha256::digest(prompt.as_bytes()));
        let key = idempotency_key.map(str::trim).filter(|key| !key.is_empty());
        if key.is_some_and(|key| key.len() > 256) {
            anyhow::bail!("idempotency key exceeds 256 bytes");
        }
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("durable HTTP store lock poisoned"))?;
        let transaction = connection.transaction()?;
        if let Some(key) = key {
            let existing = transaction
                .query_row(
                    "SELECT task_id, session_id, prompt, state, attempts, max_attempts, created_at, updated_at, error, prompt_hash
                     FROM durable_turns WHERE session_id = ?1 AND idempotency_key = ?2",
                    params![session_id, key],
                    |row| Ok((read_turn(row)?, row.get::<_, String>(9)?)),
                )
                .optional()?;
            if let Some((turn, existing_hash)) = existing {
                if existing_hash != prompt_hash {
                    anyhow::bail!("idempotency key was already used with a different request");
                }
                transaction.commit()?;
                return Ok((turn, true));
            }
        }
        let task_id = uuid::Uuid::new_v4().to_string();
        let now = chrono::Utc::now().to_rfc3339();
        transaction.execute(
            "INSERT INTO durable_turns(task_id, session_id, prompt, prompt_hash, idempotency_key, state, attempts, max_attempts, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'queued', 0, ?6, ?7, ?7)",
            params![task_id, session_id, prompt, prompt_hash, key, DEFAULT_TASK_MAX_ATTEMPTS, now],
        )?;
        transaction.commit()?;
        Ok((
            DurableTurn {
                task_id,
                session_id: session_id.into(),
                prompt: prompt.into(),
                state: "queued".into(),
                attempts: 0,
                max_attempts: DEFAULT_TASK_MAX_ATTEMPTS,
                created_at: now.clone(),
                updated_at: now,
                error: None,
            },
            false,
        ))
    }

    pub fn claim_turn(&self, task_id: &str) -> anyhow::Result<DurableTurn> {
        let now_unix = chrono::Utc::now().timestamp();
        let now = chrono::Utc::now().to_rfc3339();
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("durable HTTP store lock poisoned"))?;
        let transaction = connection.transaction()?;
        let changed = transaction.execute(
            "UPDATE durable_turns SET state = 'running', attempts = attempts + 1, lease_expires_at = ?1, updated_at = ?2, error = NULL
             WHERE task_id = ?3 AND state IN ('queued', 'recovery_pending') AND attempts < max_attempts",
            params![now_unix + 900, now, task_id],
        )?;
        if changed == 0 {
            anyhow::bail!("task {task_id} is not claimable");
        }
        let turn = query_turn(&transaction, task_id)?
            .ok_or_else(|| anyhow::anyhow!("task disappeared after claim"))?;
        transaction.commit()?;
        Ok(turn)
    }

    pub fn finish_turn(
        &self,
        task_id: &str,
        state: &str,
        error: Option<&str>,
    ) -> anyhow::Result<()> {
        if !matches!(
            state,
            "completed" | "failed" | "cancelled" | "waiting_approval"
        ) {
            anyhow::bail!("invalid durable turn state {state}");
        }
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("durable HTTP store lock poisoned"))?;
        let changed = connection.execute(
            "UPDATE durable_turns SET state = ?1, lease_expires_at = NULL, error = ?2, updated_at = ?3
             WHERE task_id = ?4 AND state != 'cancelled'",
            params![state, error, chrono::Utc::now().to_rfc3339(), task_id],
        )?;
        if changed == 0 {
            let existing: Option<String> = connection
                .query_row(
                    "SELECT state FROM durable_turns WHERE task_id = ?1",
                    params![task_id],
                    |row| row.get(0),
                )
                .optional()?;
            if existing.as_deref() == Some("cancelled") {
                return Ok(());
            }
            anyhow::bail!("unknown task {task_id}");
        }
        Ok(())
    }

    pub fn recoverable_turns(&self) -> anyhow::Result<Vec<DurableTurn>> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("durable HTTP store lock poisoned"))?;
        let transaction = connection.transaction()?;
        transaction.execute(
            "UPDATE durable_turns SET state = CASE WHEN attempts < max_attempts THEN 'recovery_pending' ELSE 'failed' END,
             error = CASE WHEN attempts < max_attempts THEN 'recovered after unclean shutdown' ELSE 'retry limit exhausted after unclean shutdown' END,
             lease_expires_at = NULL, updated_at = ?1 WHERE state IN ('running', 'waiting_approval')",
            params![chrono::Utc::now().to_rfc3339()],
        )?;
        let mut statement = transaction.prepare(
            "SELECT task_id, session_id, prompt, state, attempts, max_attempts, created_at, updated_at, error
             FROM durable_turns WHERE state IN ('queued', 'recovery_pending') AND attempts < max_attempts ORDER BY created_at",
        )?;
        let rows = statement.query_map([], read_turn)?;
        let mut turns = Vec::new();
        for row in rows {
            turns.push(row?);
        }
        drop(statement);
        transaction.commit()?;
        Ok(turns)
    }

    pub fn get_turn(&self, task_or_session_id: &str) -> anyhow::Result<Option<DurableTurn>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("durable HTTP store lock poisoned"))?;
        if let Some(turn) = query_turn(&connection, task_or_session_id)? {
            return Ok(Some(turn));
        }
        Ok(connection.query_row(
            "SELECT task_id, session_id, prompt, state, attempts, max_attempts, created_at, updated_at, error
             FROM durable_turns WHERE session_id = ?1 ORDER BY created_at DESC LIMIT 1",
            params![task_or_session_id], read_turn,
        ).optional()?)
    }

    pub fn list_turns(&self, limit: usize) -> anyhow::Result<Vec<DurableTurn>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("durable HTTP store lock poisoned"))?;
        let mut statement = connection.prepare(
            "SELECT task_id, session_id, prompt, state, attempts, max_attempts, created_at, updated_at, error
             FROM durable_turns ORDER BY created_at DESC LIMIT ?1",
        )?;
        let rows = statement.query_map(params![limit.min(1000) as u64], read_turn)?;
        let mut turns = Vec::new();
        for row in rows {
            turns.push(row?);
        }
        Ok(turns)
    }

    pub fn cancel_turn(&self, task_id: &str) -> anyhow::Result<DurableTurn> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("durable HTTP store lock poisoned"))?;
        let changed = connection.execute(
            "UPDATE durable_turns SET state = 'cancelled', lease_expires_at = NULL,
             error = 'cancelled by client', updated_at = ?1
             WHERE task_id = ?2 AND state IN ('queued', 'recovery_pending', 'running', 'waiting_approval')",
            params![chrono::Utc::now().to_rfc3339(), task_id],
        )?;
        if changed == 0 {
            anyhow::bail!("task {task_id} is not cancellable");
        }
        drop(connection);
        self.get_turn(task_id)?
            .ok_or_else(|| anyhow::anyhow!("task disappeared after cancellation"))
    }
}

impl DurableHttpStore {
    fn latest_event_sequence_for(&self, session_id: &str) -> anyhow::Result<u64> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("durable HTTP store lock poisoned"))?;
        Ok(connection.query_row(
            "SELECT COALESCE(MAX(event_seq), 0) FROM http_events WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )?)
    }
}

/// The durable HTTP event log doubles as the default SQLite
/// [`EventStore`](kkagent_core::storage::EventStore). Sequence numbers are
/// global to the log, which satisfies the per-session contract (strictly
/// increasing, gaps allowed).
#[async_trait::async_trait]
impl kkagent_core::storage::EventStore for DurableHttpStore {
    async fn append(
        &self,
        session_id: &str,
        event: &Value,
    ) -> kkagent_core::storage::StorageResult<u64> {
        let mut event = match event {
            Value::Object(object) => Value::Object(object.clone()),
            data => json!({"type": "event", "data": data}),
        };
        if let Some(object) = event.as_object_mut() {
            object.insert("session_id".into(), json!(session_id));
        }
        let stored = self
            .append_event(event)
            .map_err(|error| kkagent_core::storage::StorageError::Backend(error.to_string()))?;
        Ok(stored.get("event_seq").and_then(Value::as_u64).unwrap_or(0))
    }

    async fn since(
        &self,
        session_id: &str,
        after_seq: u64,
        limit: usize,
    ) -> kkagent_core::storage::StorageResult<Vec<kkagent_core::storage::StoredEvent>> {
        let events = self
            .events_since(after_seq, Some(session_id), limit)
            .map_err(|error| kkagent_core::storage::StorageError::Backend(error.to_string()))?;
        Ok(events
            .into_iter()
            .map(|event| kkagent_core::storage::StoredEvent {
                seq: event.get("event_seq").and_then(Value::as_u64).unwrap_or(0),
                event,
            })
            .collect())
    }

    async fn latest_seq(&self, session_id: &str) -> kkagent_core::storage::StorageResult<u64> {
        self.latest_event_sequence_for(session_id)
            .map_err(|error| kkagent_core::storage::StorageError::Backend(error.to_string()))
    }
}

fn read_turn(row: &rusqlite::Row<'_>) -> rusqlite::Result<DurableTurn> {
    Ok(DurableTurn {
        task_id: row.get(0)?,
        session_id: row.get(1)?,
        prompt: row.get(2)?,
        state: row.get(3)?,
        attempts: row.get(4)?,
        max_attempts: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
        error: row.get(8)?,
    })
}

fn query_turn(connection: &Connection, task_id: &str) -> anyhow::Result<Option<DurableTurn>> {
    Ok(connection.query_row(
        "SELECT task_id, session_id, prompt, state, attempts, max_attempts, created_at, updated_at, error FROM durable_turns WHERE task_id = ?1",
        params![task_id], read_turn,
    ).optional()?)
}

#[derive(Debug, Clone)]
pub struct HttpSecurityOptions {
    /// Additional token -> scopes. Scopes: read, write, terminal, admin.
    pub scoped_tokens: HashMap<String, Vec<String>>,
    pub allow_terminal_api: bool,
    pub allow_fs_write_api: bool,
    pub requests_per_minute: u32,
    pub audit_log: Option<std::path::PathBuf>,
}

impl Default for HttpSecurityOptions {
    fn default() -> Self {
        Self {
            scoped_tokens: HashMap::new(),
            allow_terminal_api: true,
            allow_fs_write_api: true,
            requests_per_minute: 600,
            audit_log: None,
        }
    }
}

#[derive(Debug, Default)]
struct RateWindow {
    minute: u64,
    count: u32,
}

#[derive(Debug, Default)]
struct HttpMetrics {
    requests_total: AtomicU64,
    unauthorized_total: AtomicU64,
    rate_limited_total: AtomicU64,
    status_counts: StdMutex<HashMap<u16, u64>>,
}

/// Pluggable backend so HTTP can bind to the live AgentLoop/ServerState.
#[async_trait::async_trait]
pub trait HttpBackend: Send + Sync {
    fn event_sender(&self) -> Option<broadcast::Sender<Value>> {
        None
    }

    async fn list_sessions(&self) -> Value;
    async fn create_session(
        &self,
        workspace: Option<String>,
        title: Option<String>,
    ) -> Result<Value, String>;
    async fn get_session(&self, id: &str) -> Option<Value>;
    async fn delete_session(&self, _id: &str) -> Result<(), String> {
        Err("session deletion is not supported by this backend".into())
    }
    async fn rename_session(&self, _id: &str, _title: &str) -> Result<Value, String> {
        Err("session rename is not supported by this backend".into())
    }
    async fn interrupt_session(&self, _id: &str) -> Result<Value, String> {
        Err("session interrupt is not supported by this backend".into())
    }
    async fn set_permission_mode(&self, _id: &str, _mode: &str) -> Result<Value, String> {
        Err("permission mode is not supported by this backend".into())
    }
    async fn set_plan_mode(&self, _id: &str, _enabled: bool) -> Result<Value, String> {
        Err("plan mode is not supported by this backend".into())
    }
    async fn set_model(&self, _id: &str, _model: &str) -> Result<Value, String> {
        Err("set model is not supported by this backend".into())
    }
    async fn compact_session(
        &self,
        _id: &str,
        _instruction: Option<&str>,
    ) -> Result<Value, String> {
        Err("compact is not supported by this backend".into())
    }
    async fn undo_session(&self, _id: &str, _count: usize) -> Result<Value, String> {
        Err("undo is not supported by this backend".into())
    }
    async fn restore_session(&self, _id: &str, _turn_index: usize) -> Result<Value, String> {
        Err("timeline restore is not supported by this backend".into())
    }
    async fn archive_session(&self, _id: &str, _archived: bool) -> Result<Value, String> {
        Err("archive is not supported by this backend".into())
    }
    async fn post_message(
        &self,
        id: &str,
        text: &str,
        images: &[HttpImageInput],
        task_id: Option<&str>,
    ) -> Result<Value, String>;
    async fn fork_session(
        &self,
        _id: &str,
        _title: Option<String>,
        _message_limit: Option<usize>,
    ) -> Result<Value, String> {
        Err("session fork is not supported by this backend".into())
    }
    async fn start_btw(&self, _id: &str, _question: &str) -> Result<Value, String> {
        Err("btw is not supported by this backend".into())
    }
    async fn cancel_btw(&self, _id: &str) -> Result<Value, String> {
        Err("btw is not supported by this backend".into())
    }
    async fn list_tools(&self) -> Value;
    async fn list_tasks(&self) -> Value;
    async fn list_skills(&self) -> Value;
    async fn list_models(&self) -> Value;
    async fn get_config(&self) -> Value;
    async fn approve(
        &self,
        id: &str,
        decision: &str,
        feedback: Option<String>,
        selected_label: Option<String>,
        scope: Option<String>,
    ) -> Result<Value, String>;

    /// Whitelisted plugin RPC used by the `/api/v1/plugins*` routes. The HTTP
    /// layer only ever forwards a fixed set of `plugins.*` method names.
    async fn plugin_rpc(&self, method: &str, params: Value) -> Result<Value, String> {
        let _ = (method, params);
        Err("plugins are not supported by this backend".into())
    }
    /// Cancel a turn. With `cancel_background` the session's background
    /// commands are cancelled too; the reply reports what actually stopped.
    async fn cancel_turn(&self, _task_id: &str, _cancel_background: bool) -> Result<Value, String> {
        Err("turn cancellation is not supported by this backend".into())
    }
    /// Turn a client-supplied path into the absolute path the file API may
    /// touch (`for_write` for `POST /fs`). Backends that confine the API to
    /// trusted workspaces refuse everything else here, with the matching
    /// [`FsErrorKind`]. The default trusts the path as given.
    async fn fs_resolve(
        &self,
        path: &str,
        _for_write: bool,
    ) -> Result<std::path::PathBuf, FsError> {
        if path.trim().is_empty() {
            return Err(FsError::bad_request("path must not be empty"));
        }
        Ok(std::path::PathBuf::from(path))
    }
    /// A request reached the HTTP layer; lets the host process know the
    /// server is in use (idle shutdown must not fire under live clients).
    fn note_http_activity(&self) {}
    /// A WebSocket event stream opened (`true`) or closed (`false`).
    fn ws_connection_changed(&self, _opened: bool) {}
    async fn search(&self, query: &str) -> Value;
    async fn workspace_info(&self) -> Value;
    async fn list_questions(&self) -> Value {
        json!({"questions": []})
    }
    async fn answer_question(&self, _id: &str, _response: Value) -> Result<Value, String> {
        Err("question responses are not supported by this backend".into())
    }
    async fn health(&self) -> Value {
        json!({"status": "ok"})
    }
    async fn readiness(&self) -> Result<Value, String> {
        Ok(json!({"status": "ready"}))
    }
}

/// In-memory demo backend (used when not wired to AgentLoop).
#[derive(Default)]
pub struct MemoryBackend {
    sessions: Mutex<HashMap<String, Value>>,
}

#[async_trait::async_trait]
impl HttpBackend for MemoryBackend {
    async fn list_sessions(&self) -> Value {
        let map = self.sessions.lock().await;
        json!({"sessions": map.values().cloned().collect::<Vec<_>>()})
    }
    async fn create_session(
        &self,
        workspace: Option<String>,
        title: Option<String>,
    ) -> Result<Value, String> {
        let id = uuid::Uuid::new_v4().to_string();
        let sess = json!({
            "session_id": id,
            "workspace": workspace.unwrap_or_else(|| ".".into()),
            "title": title,
            "permission_mode": "manual",
            "plan_mode": false,
            "created_at": chrono::Utc::now().to_rfc3339(),
            "messages": [],
        });
        self.sessions.lock().await.insert(id, sess.clone());
        Ok(sess)
    }
    async fn get_session(&self, id: &str) -> Option<Value> {
        self.sessions.lock().await.get(id).cloned()
    }
    async fn delete_session(&self, id: &str) -> Result<(), String> {
        self.sessions
            .lock()
            .await
            .remove(id)
            .map(|_| ())
            .ok_or_else(|| "not found".into())
    }
    async fn rename_session(&self, id: &str, title: &str) -> Result<Value, String> {
        memory_update_session(&self.sessions, id, |sess| {
            sess.insert("title".into(), json!(title));
        })
        .await
    }
    async fn interrupt_session(&self, id: &str) -> Result<Value, String> {
        let _ = self
            .get_session(id)
            .await
            .ok_or_else(|| "not found".to_string())?;
        Ok(json!({"ok": true, "session_id": id}))
    }
    async fn set_permission_mode(&self, id: &str, mode: &str) -> Result<Value, String> {
        let mode = normalize_permission_mode(mode)?;
        memory_update_session(&self.sessions, id, |sess| {
            sess.insert("permission_mode".into(), json!(mode));
        })
        .await
    }
    async fn set_plan_mode(&self, id: &str, enabled: bool) -> Result<Value, String> {
        memory_update_session(&self.sessions, id, |sess| {
            sess.insert("plan_mode".into(), json!(enabled));
        })
        .await
    }
    async fn set_model(&self, id: &str, model: &str) -> Result<Value, String> {
        if model.trim().is_empty() {
            return Err("model must not be empty".into());
        }
        memory_update_session(&self.sessions, id, |sess| {
            sess.insert("model".into(), json!(model));
        })
        .await
    }
    async fn compact_session(&self, id: &str, _instruction: Option<&str>) -> Result<Value, String> {
        let _ = self
            .get_session(id)
            .await
            .ok_or_else(|| "not found".to_string())?;
        Ok(json!({"ok": true, "started": false, "session_id": id}))
    }
    async fn undo_session(&self, id: &str, count: usize) -> Result<Value, String> {
        let mut map = self.sessions.lock().await;
        let sess = map.get_mut(id).ok_or_else(|| "not found".to_string())?;
        let mut remaining = count.max(1);
        let mut undone = 0usize;
        {
            let Some(messages) = sess.get_mut("messages").and_then(Value::as_array_mut) else {
                return Err("not found".into());
            };
            while remaining > 0 && !messages.is_empty() {
                let is_user = messages
                    .last()
                    .and_then(|message| message.get("role"))
                    .and_then(Value::as_str)
                    == Some("user");
                messages.pop();
                if is_user {
                    remaining -= 1;
                    undone += 1;
                }
            }
        }
        if undone == 0 {
            return Err("Nothing to undo".into());
        }
        Ok(json!({
            "ok": true,
            "undone": undone,
            "messages": sess.get("messages").cloned().unwrap_or_else(|| json!([])),
        }))
    }
    async fn restore_session(&self, id: &str, turn_index: usize) -> Result<Value, String> {
        let mut map = self.sessions.lock().await;
        let sess = map.get_mut(id).ok_or_else(|| "not found".to_string())?;
        let messages = sess
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let original_len = messages.len();
        let starts = visible_user_indices(&messages);
        let keep_start = *starts
            .get(turn_index)
            .ok_or_else(|| "turn not found".to_string())?;
        let end = starts.get(turn_index + 1).copied().unwrap_or(original_len);
        let kept: Vec<Value> = messages.into_iter().take(end).collect();
        if let Some(object) = sess.as_object_mut() {
            object.insert("messages".into(), json!(kept.clone()));
        }
        Ok(json!({
            "ok": true,
            "restored": original_len > end,
            "turn_index": turn_index,
            "message_index": keep_start,
            "messages": kept,
        }))
    }
    async fn archive_session(&self, id: &str, archived: bool) -> Result<Value, String> {
        memory_update_session(&self.sessions, id, |sess| {
            sess.insert("archived".into(), json!(archived));
        })
        .await
    }
    async fn post_message(
        &self,
        id: &str,
        text: &str,
        images: &[HttpImageInput],
        _task_id: Option<&str>,
    ) -> Result<Value, String> {
        let mut map = self.sessions.lock().await;
        let sess = map.get_mut(id).ok_or_else(|| "not found".to_string())?;
        let msg = json!({"role": "user", "text": text, "images": images.len(), "at": chrono::Utc::now().to_rfc3339()});
        if let Some(arr) = sess.get_mut("messages").and_then(|v| v.as_array_mut()) {
            arr.push(msg.clone());
        }
        Ok(
            json!({"ok": true, "message": msg, "note": "memory backend — wire HttpBackend for AgentLoop"}),
        )
    }
    async fn fork_session(
        &self,
        id: &str,
        title: Option<String>,
        message_limit: Option<usize>,
    ) -> Result<Value, String> {
        let mut map = self.sessions.lock().await;
        let source = map
            .get(id)
            .cloned()
            .ok_or_else(|| "not found".to_string())?;
        let new_id = uuid::Uuid::new_v4().to_string();
        let mut forked = source;
        let messages = forked
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let kept = match message_limit {
            Some(limit) => messages.into_iter().take(limit).collect::<Vec<_>>(),
            None => messages,
        };
        if let Some(object) = forked.as_object_mut() {
            object.insert("session_id".into(), json!(new_id));
            object.insert("forked_from".into(), json!(id));
            object.insert("parent_id".into(), json!(id));
            object.insert("messages".into(), json!(kept));
            object.insert("created_at".into(), json!(chrono::Utc::now().to_rfc3339()));
            let title = title.unwrap_or_else(|| {
                object
                    .get("title")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .map(|value| format!("Fork: {value}"))
                    .unwrap_or_else(|| "Fork".into())
            });
            object.insert("title".into(), json!(title));
        }
        map.insert(new_id, forked.clone());
        Ok(forked)
    }
    async fn start_btw(&self, id: &str, question: &str) -> Result<Value, String> {
        let _ = self
            .get_session(id)
            .await
            .ok_or_else(|| "not found".to_string())?;
        if question.trim().is_empty() {
            return Err("BTW question must not be empty".into());
        }
        Ok(json!({
            "ok": true,
            "agent_id": uuid::Uuid::new_v4().to_string(),
            "question": question,
            "answer": format!("关于「{}」的补充说明（memory backend）。", question.chars().take(40).collect::<String>()),
        }))
    }
    async fn cancel_btw(&self, id: &str) -> Result<Value, String> {
        let _ = self
            .get_session(id)
            .await
            .ok_or_else(|| "not found".to_string())?;
        Ok(json!({"ok": true}))
    }
    async fn list_tools(&self) -> Value {
        json!({"tools": [
            "Read","Write","Edit","Bash","Grep","Glob","TodoList","Web",
            "Agent","TaskOutput","AskUserQuestion","SelectTools","Skill","Cron","Goal","ReadMediaFile"
        ]})
    }
    async fn list_tasks(&self) -> Value {
        json!({"tasks": []})
    }
    async fn list_skills(&self) -> Value {
        json!({"skills": []})
    }
    async fn list_models(&self) -> Value {
        json!({"models": kkagent_llm_catalog_stub()})
    }
    async fn get_config(&self) -> Value {
        json!({"config_dir": dirs_home(), "api": "v1", "default_permission_mode": "manual"})
    }
    async fn approve(
        &self,
        id: &str,
        decision: &str,
        feedback: Option<String>,
        selected_label: Option<String>,
        scope: Option<String>,
    ) -> Result<Value, String> {
        Ok(
            json!({"ok": true, "approval_id": id, "decision": decision, "feedback": feedback, "selected_label": selected_label, "scope": scope}),
        )
    }
    async fn search(&self, query: &str) -> Value {
        json!({"query": query, "hits": []})
    }
    async fn workspace_info(&self) -> Value {
        json!({
            "cwd": std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_else(|_| ".".into()),
            "trusted": true,
        })
    }
}

async fn memory_update_session<F>(
    sessions: &Mutex<HashMap<String, Value>>,
    id: &str,
    update: F,
) -> Result<Value, String>
where
    F: FnOnce(&mut serde_json::Map<String, Value>),
{
    let mut map = sessions.lock().await;
    let sess = map.get_mut(id).ok_or_else(|| "not found".to_string())?;
    if let Some(object) = sess.as_object_mut() {
        update(object);
    }
    Ok(sess.clone())
}

fn normalize_permission_mode(mode: &str) -> Result<&'static str, String> {
    match mode.trim().to_ascii_lowercase().as_str() {
        "manual" => Ok("manual"),
        "yolo" => Ok("yolo"),
        "auto" => Ok("auto"),
        other => Err(format!("invalid permission mode: {other}")),
    }
}

fn backend_error_status(error: String) -> StatusCode {
    let lower = error.to_ascii_lowercase();
    if lower.contains("not found") {
        StatusCode::NOT_FOUND
    } else if lower.contains("cannot compact")
        || lower.contains("wait for")
        || lower.contains("turn is active")
        || lower.contains("busy")
    {
        StatusCode::CONFLICT
    } else {
        StatusCode::BAD_REQUEST
    }
}

fn dirs_home() -> String {
    kkagent_config::default_config_dir().display().to_string()
}

fn kkagent_llm_catalog_stub() -> Vec<Value> {
    // Avoid hard dep cycle: inline common catalog ids.
    [
        "gpt-4.1",
        "o4-mini",
        "claude-sonnet-4-20250514",
        "gemini-2.5-pro",
    ]
    .iter()
    .map(|id| json!({"id": id}))
    .collect()
}

#[derive(Clone)]
pub struct HttpState {
    pub backend: Arc<dyn HttpBackend>,
    pub meta: Value,
    pub events: broadcast::Sender<Value>,
    pub token: Option<String>,
    terminals: Arc<Mutex<HashMap<String, HttpTerminalSlot>>>,
    event_sequence: Arc<AtomicU64>,
    event_history: Arc<StdMutex<VecDeque<Value>>>,
    turn_states: Arc<StdMutex<HashMap<String, Value>>>,
    token_scopes: Arc<HashMap<String, Vec<String>>>,
    security: Arc<HttpSecurityOptions>,
    rate_windows: Arc<StdMutex<HashMap<String, RateWindow>>>,
    audit_lock: Arc<StdMutex<()>>,
    metrics: Arc<HttpMetrics>,
    persistence: Option<DurableHttpStore>,
    persistence_error: Arc<StdMutex<Option<String>>>,
}

struct HttpTerminalSlot {
    info: Value,
    child: Option<Child>,
    stdout: Arc<Mutex<Vec<u8>>>,
    stderr: Arc<Mutex<Vec<u8>>>,
}

impl HttpState {
    pub fn new(token: Option<String>) -> Self {
        Self::with_backend(Arc::new(MemoryBackend::default()), token)
    }

    pub fn with_backend(backend: Arc<dyn HttpBackend>, token: Option<String>) -> Self {
        Self::with_backend_and_security(backend, token, HttpSecurityOptions::default())
    }

    pub fn with_backend_and_security(
        backend: Arc<dyn HttpBackend>,
        token: Option<String>,
        security: HttpSecurityOptions,
    ) -> Self {
        Self::with_backend_security_and_persistence(backend, token, security, None)
    }

    pub fn with_backend_security_and_persistence(
        backend: Arc<dyn HttpBackend>,
        token: Option<String>,
        security: HttpSecurityOptions,
        persistence: Option<DurableHttpStore>,
    ) -> Self {
        let upstream = backend.event_sender();
        let (events, _) = broadcast::channel(1024);
        let mut token_scopes = security.scoped_tokens.clone();
        if let Some(token) = token.as_ref().filter(|token| !token.trim().is_empty()) {
            token_scopes.insert(token.clone(), vec!["admin".into()]);
        }
        let latest_sequence = persistence
            .as_ref()
            .and_then(|store| store.latest_event_sequence().ok())
            .unwrap_or(0);
        let boot_id = uuid::Uuid::new_v4().to_string();
        // Without a database there is nothing that outlives the process, so
        // the instance is just this boot.
        let instance_id = persistence
            .as_ref()
            .and_then(|store| store.instance_id().ok())
            .unwrap_or_else(|| boot_id.clone());
        let state = Self {
            backend,
            meta: json!({
                "name": "kkagent",
                "version": env!("CARGO_PKG_VERSION"),
                "api": ["v1"],
                "instance_id": instance_id,
                "boot_id": boot_id,
                "started_at": chrono::Utc::now().to_rfc3339(),
                "capabilities": [
                    "sessions","messages","approvals","ws","tools","tasks","skills",
                    "files","fs","workspaces","config","modelCatalog","search",
                    "terminals","questions","prompts","snapshot","eventReplay","turnStatus",
                    "health","readiness","metrics","durableEvents","durableTasks","idempotency",
                    "fork","btw","timeline",
                    "fsRange","fsBinary","filesPaging","sessionIdempotency","instanceId",
                    "taskFinished","approvalSessionId","backgroundCleanupReport"
                ],
            }),
            events,
            token,
            terminals: Arc::new(Mutex::new(HashMap::new())),
            event_sequence: Arc::new(AtomicU64::new(latest_sequence)),
            event_history: Arc::new(StdMutex::new(VecDeque::with_capacity(
                EVENT_HISTORY_CAPACITY,
            ))),
            turn_states: Arc::new(StdMutex::new(HashMap::new())),
            token_scopes: Arc::new(token_scopes),
            security: Arc::new(security),
            rate_windows: Arc::new(StdMutex::new(HashMap::new())),
            audit_lock: Arc::new(StdMutex::new(())),
            metrics: Arc::new(HttpMetrics::default()),
            persistence,
            persistence_error: Arc::new(StdMutex::new(None)),
        };
        if let (Some(upstream), Ok(runtime)) = (upstream, tokio::runtime::Handle::try_current()) {
            let forward_state = state.clone();
            runtime.spawn(async move {
                let mut receiver = upstream.subscribe();
                loop {
                    match receiver.recv().await {
                        Ok(event) => forward_state.publish(event),
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            forward_state.publish(json!({
                                "type": "upstream_lagged",
                                "skipped": skipped,
                            }));
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
        }
        state
    }

    pub fn publish(&self, event: Value) {
        let event = if let Some(store) = &self.persistence {
            match store.append_event(event) {
                Ok(event) => event,
                Err(error) => {
                    tracing::error!("failed to persist HTTP event: {error}");
                    if let Ok(mut state) = self.persistence_error.lock() {
                        *state = Some(error.to_string());
                    }
                    return;
                }
            }
        } else {
            let sequence = self.event_sequence.fetch_add(1, Ordering::SeqCst) + 1;
            let emitted_at = chrono::Utc::now().to_rfc3339();
            let mut event = match event {
                Value::Object(object) => Value::Object(object),
                data => json!({"type": "event", "data": data}),
            };
            if let Some(object) = event.as_object_mut() {
                object.insert("event_seq".into(), json!(sequence));
                object.insert("emitted_at".into(), json!(emitted_at));
            }
            event
        };
        let sequence = event.get("event_seq").and_then(Value::as_u64).unwrap_or(0);
        self.event_sequence.store(sequence, Ordering::SeqCst);
        self.update_turn_state(&event);
        if let Ok(mut history) = self.event_history.lock() {
            if history.len() == EVENT_HISTORY_CAPACITY {
                history.pop_front();
            }
            history.push_back(event.clone());
        }
        let _ = self.events.send(event);
    }

    fn update_turn_state(&self, event: &Value) {
        let Some(session_id) = event.get("session_id").and_then(Value::as_str) else {
            return;
        };
        let event_type = event.get("type").and_then(Value::as_str).unwrap_or("");
        let sequence = event.get("event_seq").and_then(Value::as_u64).unwrap_or(0);
        // `turn_start` / `turn_end` bracket one model round and can repeat
        // inside a task; only `task_finished` is the task's final state.
        let state = match event_type {
            "turn_start" => Some("running"),
            "turn_end" => Some("completed"),
            "approval_requested" => Some("waiting_approval"),
            "question_asked" => Some("waiting_question"),
            "error" => Some("failed"),
            "task_finished" => event.get("state").and_then(Value::as_str),
            _ => None,
        };
        if let Ok(mut turns) = self.turn_states.lock() {
            if event_type == "session.deleted" {
                turns.remove(session_id);
                return;
            }
            let entry = turns
                .entry(session_id.to_string())
                .or_insert_with(|| json!({"session_id": session_id, "state": "idle"}));
            if let Some(object) = entry.as_object_mut() {
                if let Some(state) = state {
                    object.insert("state".into(), json!(state));
                }
                if event_type == "status_update" {
                    object.insert(
                        "status".into(),
                        event.get("status").cloned().unwrap_or(Value::Null),
                    );
                }
                if event_type == "error" {
                    object.insert(
                        "error".into(),
                        event.get("message").cloned().unwrap_or(Value::Null),
                    );
                }
                object.insert("last_event_seq".into(), json!(sequence));
                object.insert("updated_at".into(), json!(chrono::Utc::now().to_rfc3339()));
            }
        }
    }

    fn events_since(&self, since: u64, session_id: Option<&str>, limit: usize) -> Vec<Value> {
        if let Some(store) = &self.persistence {
            match store.events_since(since, session_id, limit.min(10_000)) {
                Ok(events) => return events,
                Err(error) => {
                    tracing::error!("failed to read durable HTTP events: {error}");
                    if let Ok(mut state) = self.persistence_error.lock() {
                        *state = Some(error.to_string());
                    }
                    return Vec::new();
                }
            }
        }
        self.event_history
            .lock()
            .map(|history| {
                history
                    .iter()
                    .filter(|event| {
                        event.get("event_seq").and_then(Value::as_u64).unwrap_or(0) > since
                    })
                    .filter(|event| event_matches_session(event, session_id))
                    .take(limit.min(EVENT_HISTORY_CAPACITY))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[derive(Deserialize, Default)]
pub struct AuthQuery {
    pub token: Option<String>,
    /// Optional passthrough for endpoints that need one extra query param
    /// (e.g. `?source=` for marketplace browse) without a second extractor.
    #[serde(default)]
    pub extra: Option<String>,
}

fn check_auth(state: &HttpState, q: &AuthQuery) -> Result<(), StatusCode> {
    if state.token_scopes.is_empty() {
        return Ok(());
    }
    q.token
        .as_ref()
        .filter(|token| state.token_scopes.contains_key(*token))
        .map(|_| ())
        .ok_or(StatusCode::UNAUTHORIZED)
}

fn authorize_request(state: &HttpState, request: &mut Request) -> Result<String, StatusCode> {
    // Cross-origin browser requests are rejected outright (DNS-rebinding and
    // cross-site token abuse). Non-browser clients simply omit Origin.
    if let Some(origin) = request
        .headers()
        .get(axum::http::header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    {
        let host = request
            .headers()
            .get(axum::http::header::HOST)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        let same = origin
            .strip_prefix("http://")
            .or_else(|| origin.strip_prefix("https://"))
            .map(|rest| rest == host)
            .unwrap_or(false);
        if !same {
            return Err(StatusCode::FORBIDDEN);
        }
    }
    if state.token_scopes.is_empty() {
        return Ok("anonymous".into());
    }
    let path = request.uri().path();
    // The Web UI shell is public: without a token the page itself renders a
    // token prompt instead of a raw 401, so users can paste a token in the
    // browser. All /api requests behind it still require the token.
    if matches!(path, "/" | "/ui" | "/ui/" | "/ui/app.js") {
        return Ok("anonymous".into());
    }
    let query_token = url::Url::parse(&format!("http://localhost{}", request.uri()))
        .ok()
        .and_then(|url| {
            url.query_pairs()
                .find(|(name, _)| name == "token")
                .map(|(_, value)| value.into_owned())
        });
    let bearer = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, token)| token);
    let presented = bearer
        .map(str::to_string)
        .or(query_token)
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let scopes = state
        .token_scopes
        .get(&presented)
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let required_scope = required_scope(request.method().as_str(), request.uri().path());
    if !scopes
        .iter()
        .any(|scope| scope == "admin" || scope == required_scope)
    {
        return Err(StatusCode::FORBIDDEN);
    }

    // Existing handlers also validate the legacy query token. Inject an encoded
    // copy after authenticating the header so both access paths share one check.
    let mut url = url::Url::parse(&format!("http://localhost{}", request.uri()))
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    let preserved = url
        .query_pairs()
        .filter(|(name, _)| name != "token")
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    url.set_query(None);
    {
        let mut query = url.query_pairs_mut();
        for (name, value) in preserved {
            query.append_pair(&name, &value);
        }
        query.append_pair("token", &presented);
    }
    let path_and_query = match url.query() {
        Some(query) => format!("{}?{query}", url.path()),
        None => url.path().to_string(),
    };
    *request.uri_mut() = path_and_query
        .parse()
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    Ok(token_fingerprint(&presented))
}

async fn auth_middleware(
    State(state): State<HttpState>,
    mut request: Request,
    next: Next,
) -> impl IntoResponse {
    state.metrics.requests_total.fetch_add(1, Ordering::Relaxed);
    state.backend.note_http_activity();
    let path = request.uri().path().to_string();
    let method = request.method().to_string();
    let request_id = request
        .headers()
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty() && value.len() <= 128)
        .map(str::to_string)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    if path.starts_with("/api/v1/terminals") && !state.security.allow_terminal_api {
        record_status(&state, StatusCode::FORBIDDEN);
        audit_request(
            &state,
            &request_id,
            "disabled",
            &method,
            &path,
            StatusCode::FORBIDDEN,
        );
        return StatusCode::FORBIDDEN.into_response();
    }
    if path == "/api/v1/fs" && method == "POST" && !state.security.allow_fs_write_api {
        record_status(&state, StatusCode::FORBIDDEN);
        audit_request(
            &state,
            &request_id,
            "disabled",
            &method,
            &path,
            StatusCode::FORBIDDEN,
        );
        return StatusCode::FORBIDDEN.into_response();
    }
    match authorize_request(&state, &mut request) {
        Ok(identity) => {
            if !consume_rate_limit(&state, &identity) {
                state
                    .metrics
                    .rate_limited_total
                    .fetch_add(1, Ordering::Relaxed);
                record_status(&state, StatusCode::TOO_MANY_REQUESTS);
                audit_request(
                    &state,
                    &request_id,
                    &identity,
                    &method,
                    &path,
                    StatusCode::TOO_MANY_REQUESTS,
                );
                return StatusCode::TOO_MANY_REQUESTS.into_response();
            }
            let mut response = next.run(request).await;
            if let Ok(value) = axum::http::HeaderValue::from_str(&request_id) {
                response.headers_mut().insert("x-request-id", value);
            }
            record_status(&state, response.status());
            audit_request(
                &state,
                &request_id,
                &identity,
                &method,
                &path,
                response.status(),
            );
            response
        }
        Err(status) => {
            state
                .metrics
                .unauthorized_total
                .fetch_add(1, Ordering::Relaxed);
            record_status(&state, status);
            audit_request(&state, &request_id, "unauthorized", &method, &path, status);
            status.into_response()
        }
    }
}

fn required_scope(method: &str, path: &str) -> &'static str {
    if path.starts_with("/api/v1/terminals") {
        "terminal"
    } else if method == "GET" {
        "read"
    } else {
        "write"
    }
}

fn token_fingerprint(token: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(token.as_bytes());
    format!("token:{}", hex::encode(&digest[..6]))
}

fn consume_rate_limit(state: &HttpState, identity: &str) -> bool {
    let limit = state.security.requests_per_minute;
    if limit == 0 {
        return true;
    }
    let minute = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        / 60;
    let Ok(mut windows) = state.rate_windows.lock() else {
        return false;
    };
    let window = windows.entry(identity.to_string()).or_default();
    if window.minute != minute {
        window.minute = minute;
        window.count = 0;
    }
    if window.count >= limit {
        return false;
    }
    window.count += 1;
    true
}

fn audit_request(
    state: &HttpState,
    request_id: &str,
    identity: &str,
    method: &str,
    path: &str,
    status: StatusCode,
) {
    tracing::info!(
        target: "kkagent.http.audit",
        request_id,
        identity,
        method,
        path,
        status = status.as_u16(),
        "http request"
    );
    let Some(pathname) = state.security.audit_log.as_ref() else {
        return;
    };
    let Ok(_guard) = state.audit_lock.lock() else {
        return;
    };
    if let Some(parent) = pathname.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let record = json!({
        "at": chrono::Utc::now().to_rfc3339(),
        "request_id": request_id,
        "identity": identity,
        "method": method,
        "path": path,
        "status": status.as_u16(),
    });
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    if let Ok(mut file) = options.open(pathname) {
        use std::io::Write;
        let _ = writeln!(file, "{record}");
    }
}

fn record_status(state: &HttpState, status: StatusCode) {
    if let Ok(mut counts) = state.metrics.status_counts.lock() {
        *counts.entry(status.as_u16()).or_default() += 1;
    }
}

async fn meta(State(state): State<HttpState>) -> Json<Value> {
    let mut meta = state.meta.clone();
    meta["latest_event_seq"] = json!(state.event_sequence.load(Ordering::SeqCst));
    Json(meta)
}

async fn health(State(state): State<HttpState>) -> Json<Value> {
    Json(state.backend.health().await)
}

async fn readiness(State(state): State<HttpState>) -> impl IntoResponse {
    if let Some(error) = state
        .persistence_error
        .lock()
        .ok()
        .and_then(|error| error.clone())
    {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"status": "not_ready", "error": error})),
        )
            .into_response();
    }
    match state.backend.readiness().await {
        Ok(value) => (StatusCode::OK, Json(value)).into_response(),
        Err(error) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"status": "not_ready", "error": error})),
        )
            .into_response(),
    }
}

/// `GET /api/v1/usage/tokens?days=30` — durable cross-session token usage.
///
/// Reads the per-day aggregates (`usage_daily`): rows are kept forever, so
/// windows beyond the 30-day `usage_events` retention still resolve. The
/// response carries chart-ready day totals plus model/location breakdowns
/// and, within the retention window, per-session drill-down.
async fn usage_tokens(Query(params): Query<UsageQueryParams>) -> impl IntoResponse {
    let days = params.days.unwrap_or(30).clamp(1, 365);
    let Some(store) = kkagent_core::usage_store::global_snapshot() else {
        return Json(json!({
            "days": days,
            "available": false,
            "note": "usage history is not installed (in-memory/degraded mode)",
            "by_day": [],
            "by_model": [],
            "by_location": [],
            "by_session": [],
        }));
    };
    let retention = kkagent_core::usage_store::EVENT_RETENTION_DAYS;
    // Aggregates carry no session id; the drill-down query reads the
    // retention-bounded event table instead.
    let by_session = store
        .sessions_from_events(days.min(retention))
        .unwrap_or_default();
    Json(json!({
        "days": days,
        "available": true,
        "event_retention_days": retention,
        "by_day": store.totals_by_day(days).unwrap_or_default(),
        "by_model": store.totals_by_model(days).unwrap_or_default(),
        "by_location": store.totals_by_location(days).unwrap_or_default(),
        "by_session": by_session,
    }))
}

#[derive(Debug, Deserialize)]
struct UsageQueryParams {
    days: Option<i64>,
}

async fn metrics(State(state): State<HttpState>) -> impl IntoResponse {
    let history_len = state
        .event_history
        .lock()
        .map(|history| history.len())
        .unwrap_or_default();
    let turn_states = state
        .turn_states
        .lock()
        .map(|turns| turns.len())
        .unwrap_or_default();
    let terminals = state.terminals.lock().await.len();
    let mut output = format!(
        "# TYPE kkagent_http_requests_total counter\nkkagent_http_requests_total {}\n\
# TYPE kkagent_http_unauthorized_total counter\nkkagent_http_unauthorized_total {}\n\
# TYPE kkagent_http_rate_limited_total counter\nkkagent_http_rate_limited_total {}\n\
# TYPE kkagent_event_sequence gauge\nkkagent_event_sequence {}\n\
# TYPE kkagent_event_history_size gauge\nkkagent_event_history_size {}\n\
# TYPE kkagent_turn_states gauge\nkkagent_turn_states {}\n\
# TYPE kkagent_http_terminals gauge\nkkagent_http_terminals {}\n",
        state.metrics.requests_total.load(Ordering::Relaxed),
        state.metrics.unauthorized_total.load(Ordering::Relaxed),
        state.metrics.rate_limited_total.load(Ordering::Relaxed),
        state.event_sequence.load(Ordering::Relaxed),
        history_len,
        turn_states,
        terminals,
    );
    if let Ok(status_counts) = state.metrics.status_counts.lock() {
        output.push_str("# TYPE kkagent_http_responses_total counter\n");
        let mut counts = status_counts.iter().collect::<Vec<_>>();
        counts.sort_by_key(|(status, _)| **status);
        for (status, count) in counts {
            output.push_str(&format!(
                "kkagent_http_responses_total{{status=\"{status}\"}} {count}\n"
            ));
        }
    }
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        output,
    )
}

#[derive(Deserialize, Default)]
struct EventsQuery {
    #[serde(default)]
    since: u64,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default = "default_event_limit")]
    limit: usize,
    #[serde(default)]
    token: Option<String>,
}

fn default_event_limit() -> usize {
    500
}

async fn events_history(
    State(state): State<HttpState>,
    Query(query): Query<EventsQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(
        &state,
        &AuthQuery {
            token: query.token,
            ..Default::default()
        },
    )?;
    let events = state.events_since(query.since, query.session_id.as_deref(), query.limit);
    Ok(Json(json!({
        "events": events,
        "latest_event_seq": state.event_sequence.load(Ordering::SeqCst),
        "history_capacity": EVENT_HISTORY_CAPACITY,
    })))
}

async fn turn_status(
    State(state): State<HttpState>,
    Path(session_id): Path<String>,
    Query(query): Query<AuthQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &query)?;
    if let Some(store) = &state.persistence {
        return store
            .get_turn(&session_id)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
            .map(|turn| Json(turn.as_json()))
            .ok_or(StatusCode::NOT_FOUND);
    }
    state
        .turn_states
        .lock()
        .ok()
        .and_then(|turns| turns.get(&session_id).cloned())
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

#[derive(Deserialize, Default)]
struct CancelTurnQuery {
    #[serde(default)]
    token: Option<String>,
    /// Also cancel the session's background commands.
    #[serde(default)]
    cancel_background: Option<bool>,
}

async fn cancel_turn(
    State(state): State<HttpState>,
    Path(task_id): Path<String>,
    Query(query): Query<CancelTurnQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(
        &state,
        &AuthQuery {
            token: query.token.clone(),
            ..Default::default()
        },
    )?;
    let result = state
        .backend
        .cancel_turn(&task_id, query.cancel_background.unwrap_or(false))
        .await
        .map_err(|error| {
            if error.to_ascii_lowercase().contains("not found") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::CONFLICT
            }
        })?;
    state.publish(json!({
        "type": "turn_cancelled",
        "task_id": task_id,
        "session_id": result.get("session_id").cloned().unwrap_or(Value::Null),
    }));
    Ok(Json(result))
}

async fn list_sessions(
    State(state): State<HttpState>,
    Query(q): Query<AuthQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    Ok(Json(state.backend.list_sessions().await))
}

#[derive(Deserialize)]
struct CreateSessionBody {
    workspace: Option<String>,
    title: Option<String>,
}

/// `Idempotency-Key` of a request: `None` when absent, `400` when unusable.
fn idempotency_key_of(headers: &HeaderMap) -> Result<Option<String>, StatusCode> {
    let Some(value) = headers.get("idempotency-key") else {
        return Ok(None);
    };
    let key = value.to_str().map_err(|_| StatusCode::BAD_REQUEST)?.trim();
    if key.is_empty() || key.len() > 256 {
        return Err(StatusCode::BAD_REQUEST);
    }
    Ok(Some(key.to_string()))
}

fn with_replayed(mut response: Value, replayed: bool) -> Value {
    if let Some(object) = response.as_object_mut() {
        object.insert("replayed".into(), json!(replayed));
    }
    response
}

/// `POST /sessions`. With an `Idempotency-Key` the same request returns the
/// session the first call created (`replayed: true`) instead of a second one;
/// the same key with other parameters is a `409`.
async fn create_session(
    State(state): State<HttpState>,
    Query(q): Query<AuthQuery>,
    headers: HeaderMap,
    Json(body): Json<CreateSessionBody>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    let claimed_key = match (idempotency_key_of(&headers)?, &state.persistence) {
        (Some(key), Some(store)) => {
            use sha2::{Digest, Sha256};
            let request_hash = hex::encode(Sha256::digest(
                json!([body.workspace, body.title]).to_string().as_bytes(),
            ));
            let mut claim = store
                .begin_session_create(&key, &request_hash)
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            // A replay of a session that has since been deleted would hand
            // back a dead id: make a new one instead.
            if let SessionCreateClaim::Replay(previous) = &claim {
                let alive = match previous.get("session_id").and_then(Value::as_str) {
                    Some(id) => state.backend.get_session(id).await.is_some(),
                    None => false,
                };
                if !alive {
                    let _ = store.forget_session_create(&key);
                    claim = store
                        .begin_session_create(&key, &request_hash)
                        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
                }
            }
            match claim {
                SessionCreateClaim::Fresh => Some(key),
                SessionCreateClaim::Replay(previous) => {
                    return Ok(Json(with_replayed(previous, true)));
                }
                SessionCreateClaim::InProgress | SessionCreateClaim::Conflict => {
                    return Err(StatusCode::CONFLICT);
                }
            }
        }
        _ => None,
    };
    let sess = match state
        .backend
        .create_session(body.workspace, body.title)
        .await
    {
        Ok(sess) => sess,
        Err(_) => {
            if let (Some(key), Some(store)) = (&claimed_key, &state.persistence) {
                let _ = store.abandon_session_create(key);
            }
            return Err(StatusCode::BAD_REQUEST);
        }
    };
    if let (Some(key), Some(store)) = (&claimed_key, &state.persistence) {
        if let Err(error) = store.complete_session_create(key, &sess) {
            tracing::warn!("could not record the idempotent session create: {error}");
        }
    }
    if let Some(id) = sess.get("session_id").and_then(|v| v.as_str()) {
        state.publish(json!({"type": "session.created", "session_id": id}));
    }
    Ok(Json(if claimed_key.is_some() {
        with_replayed(sess, false)
    } else {
        sess
    }))
}

async fn get_session(
    State(state): State<HttpState>,
    Path(id): Path<String>,
    Query(q): Query<AuthQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    state
        .backend
        .get_session(&id)
        .await
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

#[derive(Deserialize, Default)]
struct PatchSessionBody {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    permission_mode: Option<String>,
    #[serde(default)]
    plan_mode: Option<bool>,
    #[serde(default)]
    model: Option<String>,
}

async fn patch_session(
    State(state): State<HttpState>,
    Path(id): Path<String>,
    Query(q): Query<AuthQuery>,
    Json(body): Json<PatchSessionBody>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    if body.title.is_none()
        && body.permission_mode.is_none()
        && body.plan_mode.is_none()
        && body.model.is_none()
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    if let Some(title) = body.title.as_deref() {
        if title.trim().is_empty() {
            return Err(StatusCode::BAD_REQUEST);
        }
        state
            .backend
            .rename_session(&id, title.trim())
            .await
            .map_err(backend_error_status)?;
    }
    if let Some(mode) = body.permission_mode.as_deref() {
        state
            .backend
            .set_permission_mode(&id, mode)
            .await
            .map_err(backend_error_status)?;
    }
    if let Some(enabled) = body.plan_mode {
        state
            .backend
            .set_plan_mode(&id, enabled)
            .await
            .map_err(backend_error_status)?;
    }
    if let Some(model) = body.model.as_deref() {
        state
            .backend
            .set_model(&id, model)
            .await
            .map_err(backend_error_status)?;
    }
    let sess = state
        .backend
        .get_session(&id)
        .await
        .ok_or(StatusCode::NOT_FOUND)?;
    state.publish(json!({
        "type": "session.updated",
        "session_id": id,
        "title": sess.get("title").cloned().unwrap_or(Value::Null),
        "permission_mode": sess.get("permission_mode").cloned().unwrap_or(Value::Null),
        "plan_mode": sess.get("plan_mode").cloned().unwrap_or(Value::Null),
        "model": sess.get("model").cloned().unwrap_or(Value::Null),
    }));
    state.publish(json!({
        "type": "session_config_changed",
        "session_id": id,
        "permission_mode": sess.get("permission_mode").cloned().unwrap_or(Value::Null),
        "model": sess.get("model").cloned().unwrap_or(Value::Null),
        "plan_mode": sess.get("plan_mode").cloned().unwrap_or(Value::Null),
    }));
    Ok(Json(sess))
}

async fn interrupt_session(
    State(state): State<HttpState>,
    Path(id): Path<String>,
    Query(q): Query<AuthQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    let result = state
        .backend
        .interrupt_session(&id)
        .await
        .map_err(backend_error_status)?;
    let source = result
        .get("source")
        .and_then(|v| v.as_str())
        .unwrap_or("http");
    state.publish(json!({
        "type": "session.interrupted",
        "session_id": id,
        "source": source,
    }));
    Ok(Json(result))
}

#[derive(Deserialize, Default)]
struct CompactBody {
    #[serde(default)]
    instruction: Option<String>,
}

async fn compact_session(
    State(state): State<HttpState>,
    Path(id): Path<String>,
    Query(q): Query<AuthQuery>,
    Json(body): Json<CompactBody>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    let result = state
        .backend
        .compact_session(&id, body.instruction.as_deref())
        .await
        .map_err(backend_error_status)?;
    Ok(Json(result))
}

#[derive(Deserialize, Default)]
struct UndoBody {
    #[serde(default)]
    count: Option<usize>,
}

async fn undo_session(
    State(state): State<HttpState>,
    Path(id): Path<String>,
    Query(q): Query<AuthQuery>,
    Json(body): Json<UndoBody>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    let result = state
        .backend
        .undo_session(&id, body.count.unwrap_or(1))
        .await
        .map_err(backend_error_status)?;
    Ok(Json(result))
}

#[derive(Deserialize, Default)]
struct RestoreBody {
    #[serde(default)]
    turn_index: Option<usize>,
}

async fn restore_session(
    State(state): State<HttpState>,
    Path(id): Path<String>,
    Query(q): Query<AuthQuery>,
    Json(body): Json<RestoreBody>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    let turn_index = body.turn_index.ok_or(StatusCode::BAD_REQUEST)?;
    let result = state
        .backend
        .restore_session(&id, turn_index)
        .await
        .map_err(backend_error_status)?;
    state.publish(json!({
        "type": "session.restored",
        "session_id": id,
        "turn_index": turn_index,
    }));
    Ok(Json(result))
}

#[derive(Deserialize, Default)]
struct ArchiveBody {
    #[serde(default)]
    archived: Option<bool>,
}

async fn archive_session(
    State(state): State<HttpState>,
    Path(id): Path<String>,
    Query(q): Query<AuthQuery>,
    Json(body): Json<ArchiveBody>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    let archived = body.archived.unwrap_or(true);
    let result = state
        .backend
        .archive_session(&id, archived)
        .await
        .map_err(backend_error_status)?;
    state.publish(json!({
        "type": "session.archived",
        "session_id": id,
        "archived": archived,
    }));
    Ok(Json(result))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PostMessageBody {
    text: String,
    #[serde(default)]
    images: Vec<HttpImageInput>,
}

async fn post_message(
    State(state): State<HttpState>,
    Path(id): Path<String>,
    Query(q): Query<AuthQuery>,
    headers: HeaderMap,
    Json(body): Json<PostMessageBody>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    let idempotency_key = headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok());
    let (task_id, replayed) = if let Some(store) = &state.persistence {
        let durable_prompt = if body.images.is_empty() {
            body.text.clone()
        } else {
            format!(
                "{MULTIMODAL_PROMPT_PREFIX}{}",
                serde_json::to_string(&body).map_err(|_| StatusCode::BAD_REQUEST)?
            )
        };
        let (turn, replayed) = store
            .enqueue_turn(&id, &durable_prompt, idempotency_key)
            .map_err(|error| {
                if error.to_string().contains("different request") {
                    StatusCode::CONFLICT
                } else {
                    StatusCode::BAD_REQUEST
                }
            })?;
        if replayed {
            return Ok(Json(json!({
                "ok": true,
                "queued": turn.state == "queued" || turn.state == "recovery_pending",
                "replayed": true,
                "task": turn.as_json(),
            })));
        }
        (Some(turn.task_id), false)
    } else {
        (None, false)
    };
    match state
        .backend
        .post_message(&id, &body.text, &body.images, task_id.as_deref())
        .await
    {
        Ok(v) => {
            state.publish(
                json!({"type": "message", "session_id": id, "task_id": task_id, "text": body.text}),
            );
            let mut response = v;
            if let Some(object) = response.as_object_mut() {
                object.insert("task_id".into(), json!(task_id));
                object.insert("replayed".into(), json!(replayed));
            }
            Ok(Json(response))
        }
        Err(error) => {
            if let (Some(store), Some(task_id)) = (&state.persistence, task_id.as_deref()) {
                let _ = store.finish_turn(task_id, "failed", Some(&error));
            }
            // Distinguish "unknown session" from "known session but busy":
            // conflating both into 404 made mid-turn submissions look like the
            // session had vanished.
            if error.contains("not found") {
                Err(StatusCode::NOT_FOUND)
            } else if error.contains("busy with another turn") {
                Err(StatusCode::CONFLICT)
            } else {
                Err(StatusCode::BAD_REQUEST)
            }
        }
    }
}

#[derive(Deserialize, Default)]
struct ForkSessionBody {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    message_limit: Option<usize>,
}

async fn fork_session(
    State(state): State<HttpState>,
    Path(id): Path<String>,
    Query(q): Query<AuthQuery>,
    Json(body): Json<ForkSessionBody>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    let sess = state
        .backend
        .fork_session(&id, body.title, body.message_limit)
        .await
        .map_err(|error| {
            if error.contains("not found") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::BAD_REQUEST
            }
        })?;
    if let Some(forked_id) = sess.get("session_id").and_then(|v| v.as_str()) {
        state.publish(json!({
            "type": "session.forked",
            "session_id": forked_id,
            "forked_from": id,
        }));
    }
    Ok(Json(sess))
}

#[derive(Deserialize, Default)]
struct BtwBody {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    question: Option<String>,
}

async fn start_btw(
    State(state): State<HttpState>,
    Path(id): Path<String>,
    Query(q): Query<AuthQuery>,
    Json(body): Json<BtwBody>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    let question = body.text.or(body.question).unwrap_or_default();
    let result = state
        .backend
        .start_btw(&id, &question)
        .await
        .map_err(|error| {
            if error.contains("not found") {
                StatusCode::NOT_FOUND
            } else if error.contains("Wait for") {
                StatusCode::CONFLICT
            } else {
                StatusCode::BAD_REQUEST
            }
        })?;
    state.publish(json!({
        "type": "btw_start",
        "session_id": id,
        "question": question,
        "agent_id": result.get("agent_id").cloned().unwrap_or(Value::Null),
    }));
    Ok(Json(result))
}

async fn cancel_btw(
    State(state): State<HttpState>,
    Path(id): Path<String>,
    Query(q): Query<AuthQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    let result = state
        .backend
        .cancel_btw(&id)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    state.publish(json!({"type": "btw_cancel", "session_id": id}));
    Ok(Json(result))
}

#[derive(Deserialize)]
struct ApprovalBody {
    decision: String,
    #[serde(default)]
    feedback: Option<String>,
    /// Plan-review option label (multi-approach plans).
    #[serde(default)]
    selected_label: Option<String>,
    /// "session" to remember the decision for the rest of the session.
    #[serde(default)]
    scope: Option<String>,
}

async fn post_approval(
    State(state): State<HttpState>,
    Path(id): Path<String>,
    Query(q): Query<AuthQuery>,
    Json(body): Json<ApprovalBody>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    let v = state
        .backend
        .approve(
            &id,
            &body.decision,
            body.feedback.clone(),
            body.selected_label.clone(),
            body.scope.clone(),
        )
        .await
        .map_err(|error| {
            // An approval that timed out, was answered, or never existed is
            // "not found", not a malformed request.
            let lower = error.to_ascii_lowercase();
            if lower.contains("no longer pending") || lower.contains("not found") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::BAD_REQUEST
            }
        })?;
    // `session_id` lets a client subscribed to one session see the answer.
    state.publish(json!({
        "type": "approval",
        "approval_id": id,
        "session_id": v.get("session_id").cloned().unwrap_or(Value::Null),
        "decision": body.decision,
        "feedback": body.feedback,
        "selected_label": body.selected_label,
    }));
    Ok(Json(v))
}

#[derive(Deserialize)]
struct PluginInstallBody {
    source: String,
    #[serde(default)]
    marketplace: Option<String>,
}

#[derive(Deserialize)]
struct PluginEnableBody {
    enabled: bool,
}

#[derive(Deserialize)]
struct MarketplaceAddBody {
    source: String,
    #[serde(default)]
    name: Option<String>,
}

async fn plugins_list(
    State(state): State<HttpState>,
    Query(q): Query<AuthQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    let v = state
        .backend
        .plugin_rpc("plugins.list", json!({}))
        .await
        .map_err(backend_error_status)?;
    Ok(Json(v))
}

async fn plugins_install(
    State(state): State<HttpState>,
    Query(q): Query<AuthQuery>,
    Json(body): Json<PluginInstallBody>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    let v = state
        .backend
        .plugin_rpc(
            "plugins.install",
            json!({"source": body.source, "marketplace": body.marketplace}),
        )
        .await
        .map_err(backend_error_status)?;
    state.publish(json!({"type": "plugins.changed", "action": "install"}));
    Ok(Json(v))
}

async fn plugin_update(
    State(state): State<HttpState>,
    Path(id): Path<String>,
    Query(q): Query<AuthQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    let v = state
        .backend
        .plugin_rpc("plugins.update", json!({"id": id}))
        .await
        .map_err(backend_error_status)?;
    state.publish(json!({"type": "plugins.changed", "action": "update", "id": id}));
    Ok(Json(v))
}

async fn plugin_set_enabled(
    State(state): State<HttpState>,
    Path(id): Path<String>,
    Query(q): Query<AuthQuery>,
    Json(body): Json<PluginEnableBody>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    let method = if body.enabled {
        "plugins.enable"
    } else {
        "plugins.disable"
    };
    let v = state
        .backend
        .plugin_rpc(method, json!({"id": id}))
        .await
        .map_err(backend_error_status)?;
    let action = if body.enabled { "enable" } else { "disable" };
    state.publish(json!({
        "type": "plugins.changed",
        "action": action,
        "id": id,
    }));
    Ok(Json(v))
}

async fn plugin_remove(
    State(state): State<HttpState>,
    Path(id): Path<String>,
    Query(q): Query<AuthQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    let v = state
        .backend
        .plugin_rpc("plugins.remove", json!({"id": id}))
        .await
        .map_err(backend_error_status)?;
    state.publish(json!({"type": "plugins.changed", "action": "remove", "id": id}));
    Ok(Json(v))
}

async fn marketplaces_list(
    State(state): State<HttpState>,
    Query(q): Query<AuthQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    let v = state
        .backend
        .plugin_rpc("plugins.marketplaces.list", json!({}))
        .await
        .map_err(backend_error_status)?;
    Ok(Json(v))
}

async fn marketplaces_add(
    State(state): State<HttpState>,
    Query(q): Query<AuthQuery>,
    Json(body): Json<MarketplaceAddBody>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    let v = state
        .backend
        .plugin_rpc(
            "plugins.marketplaces.add",
            json!({"source": body.source, "name": body.name}),
        )
        .await
        .map_err(backend_error_status)?;
    state.publish(json!({"type": "plugins.changed", "action": "marketplace.add"}));
    Ok(Json(v))
}

async fn marketplace_remove(
    State(state): State<HttpState>,
    Path(id): Path<String>,
    Query(q): Query<AuthQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    let v = state
        .backend
        .plugin_rpc("plugins.marketplaces.remove", json!({"id": id}))
        .await
        .map_err(backend_error_status)?;
    state.publish(json!({"type": "plugins.changed", "action": "marketplace.remove", "id": id}));
    Ok(Json(v))
}

async fn marketplace_browse(
    State(state): State<HttpState>,
    Query(q): Query<AuthQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    let source = q
        .extra
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let v = state
        .backend
        .plugin_rpc("plugins.marketplace", json!({"source": source}))
        .await
        .map_err(backend_error_status)?;
    Ok(Json(v))
}

async fn tools(
    State(state): State<HttpState>,
    Query(q): Query<AuthQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    Ok(Json(state.backend.list_tools().await))
}

async fn tasks(
    State(state): State<HttpState>,
    Query(q): Query<AuthQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    Ok(Json(state.backend.list_tasks().await))
}

async fn skills(
    State(state): State<HttpState>,
    Query(q): Query<AuthQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    Ok(Json(state.backend.list_skills().await))
}

async fn model_catalog(
    State(state): State<HttpState>,
    Query(q): Query<AuthQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    Ok(Json(state.backend.list_models().await))
}

async fn config_get(
    State(state): State<HttpState>,
    Query(q): Query<AuthQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    Ok(Json(state.backend.get_config().await))
}

async fn workspaces(
    State(state): State<HttpState>,
    Query(q): Query<AuthQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    Ok(Json(state.backend.workspace_info().await))
}

#[derive(Deserialize)]
struct FsBody {
    path: String,
    #[serde(default)]
    content: Option<String>,
}

/// Largest slice of a file one `GET /api/v1/fs` call returns. A larger file is
/// read in ranges (`offset` + `limit`).
pub const FS_READ_MAX_BYTES: u64 = 4 * 1024 * 1024;
/// Default and largest page of `GET /api/v1/files`.
pub const FILES_DEFAULT_LIMIT: usize = 200;
pub const FILES_MAX_LIMIT: usize = 1000;

/// What went wrong in a file-system API call; each kind maps to one HTTP
/// status so clients never have to parse a `200` body to find a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsErrorKind {
    /// Malformed request: empty path, a directory where a file was expected, a
    /// bad range or encoding. `400`.
    BadRequest,
    /// The path is outside the trusted workspaces or the OS refused. `403`.
    Forbidden,
    /// Nothing at that path. `404`.
    NotFound,
    /// The file is larger than one response may carry. `413`.
    TooLarge,
    /// The bytes are not valid UTF-8 and `encoding=utf8` was asked for. `415`.
    NotText,
    /// Anything else. `500`.
    Internal,
}

#[derive(Debug, Clone)]
pub struct FsError {
    pub kind: FsErrorKind,
    pub message: String,
}

impl FsError {
    pub fn new(kind: FsErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(FsErrorKind::BadRequest, message)
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(FsErrorKind::Forbidden, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(FsErrorKind::NotFound, message)
    }

    pub fn from_io(error: &std::io::Error) -> Self {
        let kind = match error.kind() {
            std::io::ErrorKind::NotFound => FsErrorKind::NotFound,
            std::io::ErrorKind::PermissionDenied => FsErrorKind::Forbidden,
            std::io::ErrorKind::InvalidInput => FsErrorKind::BadRequest,
            _ => FsErrorKind::Internal,
        };
        Self::new(kind, error.to_string())
    }

    pub fn status(&self) -> StatusCode {
        match self.kind {
            FsErrorKind::BadRequest => StatusCode::BAD_REQUEST,
            FsErrorKind::Forbidden => StatusCode::FORBIDDEN,
            FsErrorKind::NotFound => StatusCode::NOT_FOUND,
            FsErrorKind::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            FsErrorKind::NotText => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            FsErrorKind::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn code(&self) -> &'static str {
        match self.kind {
            FsErrorKind::BadRequest => "bad_request",
            FsErrorKind::Forbidden => "forbidden",
            FsErrorKind::NotFound => "not_found",
            FsErrorKind::TooLarge => "too_large",
            FsErrorKind::NotText => "not_text",
            FsErrorKind::Internal => "internal",
        }
    }
}

impl std::fmt::Display for FsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl IntoResponse for FsError {
    fn into_response(self) -> axum::response::Response {
        (
            self.status(),
            Json(json!({"error": self.message, "code": self.code()})),
        )
            .into_response()
    }
}

fn rfc3339_of(time: std::io::Result<SystemTime>) -> Value {
    time.ok()
        .map(|time| json!(chrono::DateTime::<chrono::Utc>::from(time).to_rfc3339()))
        .unwrap_or(Value::Null)
}

/// How the bytes of `GET /api/v1/fs` are put in the JSON body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsEncoding {
    Utf8,
    Base64,
}

impl FsEncoding {
    fn parse(value: Option<&str>) -> Result<Self, FsError> {
        match value.map(str::trim).filter(|value| !value.is_empty()) {
            None | Some("utf8") | Some("utf-8") => Ok(Self::Utf8),
            Some("base64") => Ok(Self::Base64),
            Some(other) => Err(FsError::bad_request(format!(
                "unsupported encoding {other:?}; use utf8 or base64"
            ))),
        }
    }
}

/// Read `limit` bytes of `path` from `offset`.
///
/// Without `limit` the whole rest of the file is returned, and a file that
/// does not fit in [`FS_READ_MAX_BYTES`] is refused (`413`) rather than cut
/// short. With `limit` (capped at [`FS_READ_MAX_BYTES`]) a partial read is
/// normal and `truncated` / `next_offset` say where to continue.
pub async fn read_file_range(
    path: &std::path::Path,
    offset: u64,
    limit: Option<u64>,
    encoding: FsEncoding,
) -> Result<Value, FsError> {
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|error| FsError::from_io(&error))?;
    if metadata.is_dir() {
        return Err(FsError::bad_request(
            "path is a directory; list it with GET /api/v1/files",
        ));
    }
    let size = metadata.len();
    if offset > size {
        return Err(FsError::bad_request(format!(
            "offset {offset} is past the end of the file ({size} bytes)"
        )));
    }
    let remaining = size - offset;
    let wanted = match limit {
        Some(limit) => limit.min(FS_READ_MAX_BYTES),
        None if remaining > FS_READ_MAX_BYTES => {
            return Err(FsError::new(
                FsErrorKind::TooLarge,
                format!(
                    "file has {remaining} bytes from offset {offset}, over the \
                     {FS_READ_MAX_BYTES}-byte limit of one read; pass offset and limit"
                ),
            ));
        }
        None => remaining,
    };
    let to_read = wanted.min(remaining);
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|error| FsError::from_io(&error))?;
    if offset > 0 {
        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(|error| FsError::from_io(&error))?;
    }
    let mut bytes = Vec::with_capacity(to_read as usize);
    file.take(to_read)
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| FsError::from_io(&error))?;

    let more = offset + (bytes.len() as u64) < size;
    let (content, encoding_name, bytes_read) = match encoding {
        FsEncoding::Base64 => {
            use base64::Engine as _;
            (
                base64::engine::general_purpose::STANDARD.encode(&bytes),
                "base64",
                bytes.len() as u64,
            )
        }
        FsEncoding::Utf8 => {
            let usable = match std::str::from_utf8(&bytes) {
                Ok(_) => bytes.len(),
                // A range may end inside a multi-byte character: stop before
                // it and let `next_offset` point at it.
                Err(error) if error.error_len().is_none() && more && error.valid_up_to() > 0 => {
                    error.valid_up_to()
                }
                Err(_) => {
                    return Err(FsError::new(
                        FsErrorKind::NotText,
                        "the requested bytes are not valid UTF-8 text; ask for encoding=base64",
                    ));
                }
            };
            bytes.truncate(usable);
            let text = String::from_utf8(bytes).map_err(|_| {
                FsError::new(
                    FsErrorKind::NotText,
                    "the requested bytes are not valid UTF-8 text",
                )
            })?;
            (text, "utf8", usable as u64)
        }
    };
    let truncated = offset + bytes_read < size;
    Ok(json!({
        "encoding": encoding_name,
        "content": content,
        "offset": offset,
        "bytes_read": bytes_read,
        "size": size,
        "modified_at": rfc3339_of(metadata.modified()),
        "truncated": truncated,
        "next_offset": if truncated { json!(offset + bytes_read) } else { Value::Null },
    }))
}

/// One page of a directory, sorted by name. `cursor` is the last name of the
/// previous page; entries are returned with size and modification time.
pub async fn list_directory_page(
    path: &std::path::Path,
    cursor: Option<&str>,
    limit: usize,
) -> Result<Value, FsError> {
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|error| FsError::from_io(&error))?;
    if !metadata.is_dir() {
        return Err(FsError::bad_request("path is not a directory"));
    }
    let limit = limit.clamp(1, FILES_MAX_LIMIT);
    let mut directory = tokio::fs::read_dir(path)
        .await
        .map_err(|error| FsError::from_io(&error))?;
    let mut names: Vec<std::ffi::OsString> = Vec::new();
    while let Some(entry) = directory
        .next_entry()
        .await
        .map_err(|error| FsError::from_io(&error))?
    {
        names.push(entry.file_name());
    }
    names.sort();
    let total = names.len();
    let remaining: Vec<&std::ffi::OsString> = names
        .iter()
        .filter(|name| cursor.is_none_or(|cursor| name.to_string_lossy().as_ref() > cursor))
        .collect();
    let truncated = remaining.len() > limit;
    let mut entries = Vec::new();
    for name in remaining.into_iter().take(limit) {
        let entry_path = path.join(name);
        // The entry may vanish between listing and stat: skip it quietly.
        let Ok(entry_metadata) = tokio::fs::symlink_metadata(&entry_path).await else {
            continue;
        };
        let file_type = entry_metadata.file_type();
        let kind = if file_type.is_symlink() {
            "symlink"
        } else if file_type.is_dir() {
            "dir"
        } else if file_type.is_file() {
            "file"
        } else {
            "other"
        };
        entries.push(json!({
            "name": name.to_string_lossy(),
            "path": entry_path.display().to_string(),
            "is_dir": file_type.is_dir(),
            "type": kind,
            "size": if file_type.is_file() { json!(entry_metadata.len()) } else { Value::Null },
            "modified_at": rfc3339_of(entry_metadata.modified()),
        }));
    }
    let next_cursor = if truncated {
        entries
            .last()
            .and_then(|entry| entry.get("name"))
            .cloned()
            .unwrap_or(Value::Null)
    } else {
        Value::Null
    };
    Ok(json!({
        "entries": entries,
        "count": entries.len(),
        "total": total,
        "truncated": truncated,
        "next_cursor": next_cursor,
    }))
}

#[derive(Deserialize)]
struct FsPathQuery {
    path: String,
    /// First byte to read (default 0).
    #[serde(default)]
    offset: Option<u64>,
    /// Bytes to read (default: to the end, refused above the per-call cap).
    #[serde(default)]
    limit: Option<u64>,
    /// `utf8` (default) or `base64` for binary files.
    #[serde(default)]
    encoding: Option<String>,
    #[serde(default)]
    token: Option<String>,
}

#[derive(Deserialize)]
struct FilesQuery {
    #[serde(default)]
    path: Option<String>,
    /// Entries per page (default 200, at most 1000).
    #[serde(default)]
    limit: Option<usize>,
    /// Last name of the previous page (`next_cursor`).
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default)]
    token: Option<String>,
}

async fn fs_read(
    State(state): State<HttpState>,
    Query(query): Query<FsPathQuery>,
) -> Result<Json<Value>, FsErrorOrStatus> {
    check_auth(
        &state,
        &AuthQuery {
            token: query.token.clone(),
            ..Default::default()
        },
    )?;
    let encoding = FsEncoding::parse(query.encoding.as_deref())?;
    let resolved = state.backend.fs_resolve(&query.path, false).await?;
    let mut body =
        read_file_range(&resolved, query.offset.unwrap_or(0), query.limit, encoding).await?;
    body["path"] = json!(query.path);
    Ok(Json(body))
}

async fn fs_write(
    State(state): State<HttpState>,
    Query(q): Query<AuthQuery>,
    Json(body): Json<FsBody>,
) -> Result<Json<Value>, FsErrorOrStatus> {
    check_auth(&state, &q)?;
    let content = body.content.unwrap_or_default();
    let resolved = state.backend.fs_resolve(&body.path, true).await?;
    if let Some(parent) = resolved.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| FsError::from_io(&error))?;
    }
    tokio::fs::write(&resolved, content)
        .await
        .map_err(|error| FsError::from_io(&error))?;
    Ok(Json(json!({"ok": true, "path": body.path})))
}

async fn files_list(
    State(state): State<HttpState>,
    Query(query): Query<FilesQuery>,
) -> Result<Json<Value>, FsErrorOrStatus> {
    check_auth(
        &state,
        &AuthQuery {
            token: query.token.clone(),
            ..Default::default()
        },
    )?;
    let requested = query.path.as_deref().unwrap_or(".");
    let resolved = state.backend.fs_resolve(requested, false).await?;
    let mut body = list_directory_page(
        &resolved,
        query.cursor.as_deref(),
        query.limit.unwrap_or(FILES_DEFAULT_LIMIT),
    )
    .await?;
    body["path"] = json!(requested);
    Ok(Json(body))
}

/// Handler failure that is either a bare status (auth) or a file-system
/// error with a JSON body.
#[derive(Debug)]
enum FsErrorOrStatus {
    Status(StatusCode),
    Fs(FsError),
}

impl From<StatusCode> for FsErrorOrStatus {
    fn from(status: StatusCode) -> Self {
        Self::Status(status)
    }
}

impl From<FsError> for FsErrorOrStatus {
    fn from(error: FsError) -> Self {
        Self::Fs(error)
    }
}

impl IntoResponse for FsErrorOrStatus {
    fn into_response(self) -> axum::response::Response {
        match self {
            Self::Status(status) => status.into_response(),
            Self::Fs(error) => error.into_response(),
        }
    }
}

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
    #[serde(default)]
    token: Option<String>,
}

async fn search(
    State(state): State<HttpState>,
    Query(q): Query<SearchQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(
        &state,
        &AuthQuery {
            token: q.token.clone(),
            ..Default::default()
        },
    )?;
    Ok(Json(state.backend.search(&q.q).await))
}

async fn snapshot(
    State(state): State<HttpState>,
    Query(q): Query<AuthQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    Ok(Json(json!({
        "sessions": state.backend.list_sessions().await,
        "workspace": state.backend.workspace_info().await,
        "at": chrono::Utc::now().to_rfc3339(),
    })))
}

async fn prompts(
    State(state): State<HttpState>,
    Query(q): Query<AuthQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    Ok(Json(json!({"prompts": [
        {"id": "default", "title": "Default system"},
        {"id": "plan", "title": "Plan mode"},
    ]})))
}

async fn questions(
    State(state): State<HttpState>,
    Query(q): Query<AuthQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    Ok(Json(state.backend.list_questions().await))
}

async fn answer_question(
    State(state): State<HttpState>,
    Path(id): Path<String>,
    Query(q): Query<AuthQuery>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    state
        .backend
        .answer_question(&id, body)
        .await
        .map(Json)
        .map_err(|_| StatusCode::BAD_REQUEST)
}

async fn terminals_list(
    State(state): State<HttpState>,
    Query(q): Query<AuthQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    let map = state.terminals.lock().await;
    Ok(Json(json!({
        "terminals": map.values().map(|slot| slot.info.clone()).collect::<Vec<_>>()
    })))
}

#[derive(Deserialize)]
struct TerminalCreate {
    command: Option<String>,
    cwd: Option<String>,
}

async fn terminals_create(
    State(state): State<HttpState>,
    Query(q): Query<AuthQuery>,
    Json(body): Json<TerminalCreate>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    let mut terminals = state.terminals.lock().await;
    if terminals.len() >= MAX_HTTP_TERMINALS {
        return Err(StatusCode::TOO_MANY_REQUESTS);
    }
    let id = uuid::Uuid::new_v4().to_string();
    let command_text = body.command.unwrap_or_else(|| {
        if cfg!(windows) {
            "echo kkagent-terminal".into()
        } else {
            "printf kkagent-terminal".into()
        }
    });
    if command_text.len() > MAX_TERMINAL_COMMAND_BYTES {
        return Err(StatusCode::PAYLOAD_TOO_LARGE);
    }
    let cwd = body.cwd.unwrap_or_else(|| ".".into());
    let mut command = if cfg!(windows) {
        let mut command = tokio::process::Command::new("cmd");
        command.args(["/C", &command_text]);
        command
    } else {
        let mut command = tokio::process::Command::new("sh");
        command.args(["-lc", &command_text]);
        command
    };
    command
        .current_dir(&cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(|_| StatusCode::BAD_REQUEST)?;
    let stdout = Arc::new(Mutex::new(Vec::new()));
    let stderr = Arc::new(Mutex::new(Vec::new()));
    if let Some(mut pipe) = child.stdout.take() {
        let output = stdout.clone();
        tokio::spawn(async move {
            *output.lock().await = read_bounded_output(&mut pipe).await;
        });
    }
    if let Some(mut pipe) = child.stderr.take() {
        let output = stderr.clone();
        tokio::spawn(async move {
            *output.lock().await = read_bounded_output(&mut pipe).await;
        });
    }
    let terminal = json!({
        "terminal_id": id,
        "command": command_text,
        "cwd": cwd,
        "status": "running",
    });
    terminals.insert(
        id,
        HttpTerminalSlot {
            info: terminal.clone(),
            child: Some(child),
            stdout,
            stderr,
        },
    );
    Ok(Json(terminal))
}

async fn read_bounded_output(reader: &mut (impl tokio::io::AsyncRead + Unpin)) -> Vec<u8> {
    let mut output = Vec::new();
    let mut buffer = [0_u8; 8192];
    let mut truncated = false;
    loop {
        let count = match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(count) => count,
        };
        if output.len() < MAX_TERMINAL_OUTPUT_BYTES {
            let remaining = MAX_TERMINAL_OUTPUT_BYTES - output.len();
            output.extend_from_slice(&buffer[..count.min(remaining)]);
            truncated |= count > remaining;
        } else {
            truncated = true;
        }
    }
    if truncated {
        output.extend_from_slice(b"\n... terminal output truncated ...\n");
    }
    output
}

async fn terminal_get(
    State(state): State<HttpState>,
    Path(id): Path<String>,
    Query(q): Query<AuthQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    let mut terminals = state.terminals.lock().await;
    let slot = terminals.get_mut(&id).ok_or(StatusCode::NOT_FOUND)?;
    if let Some(child) = slot.child.as_mut() {
        if let Ok(Some(status)) = child.try_wait() {
            slot.info["status"] = json!("exited");
            slot.info["exit_code"] = json!(status.code());
            slot.child = None;
        }
    }
    let stdout = String::from_utf8_lossy(&slot.stdout.lock().await).into_owned();
    let stderr = String::from_utf8_lossy(&slot.stderr.lock().await).into_owned();
    Ok(Json(json!({
        "terminal": slot.info,
        "stdout": stdout,
        "stderr": stderr,
    })))
}

async fn terminal_delete(
    State(state): State<HttpState>,
    Path(id): Path<String>,
    Query(q): Query<AuthQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    let mut slot = state
        .terminals
        .lock()
        .await
        .remove(&id)
        .ok_or(StatusCode::NOT_FOUND)?;
    if let Some(mut child) = slot.child.take() {
        child
            .kill()
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    }
    Ok(Json(json!({"ok": true, "terminal_id": id})))
}

async fn connections(
    State(state): State<HttpState>,
    Query(q): Query<AuthQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    Ok(Json(
        json!({"connections": [{"type": "ws", "path": "/api/v1/ws"}]}),
    ))
}

async fn export_session(
    State(state): State<HttpState>,
    Path(id): Path<String>,
    Query(q): Query<AuthQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    let sess = state
        .backend
        .get_session(&id)
        .await
        .ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(json!({"export": sess, "format": "json"})))
}

async fn session_timeline(
    State(state): State<HttpState>,
    Path(id): Path<String>,
    Query(q): Query<AuthQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    let sess = state
        .backend
        .get_session(&id)
        .await
        .ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(build_session_timeline(&id, &sess)))
}

fn message_text(message: &Value) -> String {
    if let Some(text) = message.get("text").and_then(Value::as_str) {
        if !text.is_empty() {
            return kkagent_protocol::visible_user_text(text);
        }
    }
    match message.get("content") {
        Some(Value::String(text)) => kkagent_protocol::visible_user_text(text),
        Some(Value::Array(parts)) => {
            let joined = parts
                .iter()
                .filter(|part| part.get("type").and_then(Value::as_str) != Some("tool_result"))
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n");
            kkagent_protocol::visible_user_text(&joined)
        }
        _ => String::new(),
    }
}

fn message_has_tool_result(message: &Value) -> bool {
    message
        .get("content")
        .and_then(Value::as_array)
        .is_some_and(|parts| {
            parts
                .iter()
                .any(|part| part.get("type").and_then(Value::as_str) == Some("tool_result"))
        })
}

fn message_has_user_image(message: &Value) -> bool {
    message
        .get("content")
        .and_then(Value::as_array)
        .is_some_and(|parts| {
            parts
                .iter()
                .any(|part| part.get("type").and_then(Value::as_str) == Some("image"))
        })
}

fn message_tool_calls(message: &Value) -> Vec<Value> {
    let mut tools = Vec::new();
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        tools.extend(calls.iter().cloned());
    }
    if let Some(parts) = message.get("content").and_then(Value::as_array) {
        for part in parts {
            if part.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            tools.push(json!({
                "id": part.get("id"),
                "name": part.get("name"),
                "input": part.get("input"),
            }));
        }
    }
    tools
}

fn is_visible_user_message(message: &Value) -> bool {
    if message.get("role").and_then(Value::as_str) != Some("user") {
        return false;
    }
    if message_has_tool_result(message) {
        return false;
    }
    !message_text(message).is_empty() || message_has_user_image(message)
}

fn visible_user_indices(messages: &[Value]) -> Vec<usize> {
    messages
        .iter()
        .enumerate()
        .filter(|(_, message)| is_visible_user_message(message))
        .map(|(index, _)| index)
        .collect()
}

fn is_mutating_tool_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "edit"
            | "write"
            | "replace"
            | "str_replace"
            | "strreplace"
            | "apply_patch"
            | "applypatch"
            | "write_file"
            | "edit_file"
    )
}

fn unified_replace_diff(path: &str, old: &str, new: &str) -> String {
    const MAX_LINES: usize = 80;
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let old_shown = old_lines.len().min(MAX_LINES);
    let new_shown = new_lines.len().min(MAX_LINES);
    let mut out = format!("--- a/{path}\n+++ b/{path}\n@@ -1,{old_shown} +1,{new_shown} @@\n");
    for line in old_lines.iter().take(MAX_LINES) {
        out.push('-');
        out.push_str(line);
        out.push('\n');
    }
    if old_lines.len() > MAX_LINES {
        out.push_str(&format!(
            "... {} more deleted lines\n",
            old_lines.len() - MAX_LINES
        ));
    }
    for line in new_lines.iter().take(MAX_LINES) {
        out.push('+');
        out.push_str(line);
        out.push('\n');
    }
    if new_lines.len() > MAX_LINES {
        out.push_str(&format!(
            "... {} more added lines\n",
            new_lines.len() - MAX_LINES
        ));
    }
    out
}

fn diff_stats(diff: &str) -> (usize, usize) {
    let mut additions = 0usize;
    let mut deletions = 0usize;
    for line in diff.lines() {
        if line.starts_with("+++") || line.starts_with("---") || line.starts_with("@@") {
            continue;
        }
        if let Some(rest) = line.strip_prefix('+') {
            if !rest.starts_with("++") {
                additions += 1;
            }
        } else if let Some(rest) = line.strip_prefix('-') {
            if !rest.starts_with("--") {
                deletions += 1;
            }
        }
    }
    (additions, deletions)
}

fn file_diff_from_tool(tool: &Value) -> Option<Value> {
    let name = tool.get("name").and_then(Value::as_str).unwrap_or_default();
    if !is_mutating_tool_name(name) {
        return None;
    }
    let input = tool.get("input").cloned().unwrap_or(Value::Null);
    let path = input
        .get("path")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())?;
    let lower = name.to_ascii_lowercase();
    let (action, diff) = if lower == "write" || lower == "write_file" {
        let content = input
            .get("content")
            .or_else(|| input.get("contents"))
            .and_then(Value::as_str)
            .unwrap_or("");
        ("write", unified_replace_diff(path, "", content))
    } else {
        let old = input
            .get("old_string")
            .and_then(Value::as_str)
            .unwrap_or("");
        let new = input
            .get("new_string")
            .and_then(Value::as_str)
            .unwrap_or("");
        ("edit", unified_replace_diff(path, old, new))
    };
    let (additions, deletions) = diff_stats(&diff);
    Some(json!({
        "type": action,
        "path": path,
        "additions": additions,
        "deletions": deletions,
        "diff": diff,
    }))
}

fn build_session_timeline(id: &str, sess: &Value) -> Value {
    let messages = sess
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut events = Vec::new();
    if let Some(parent) = sess
        .get("forked_from")
        .or_else(|| sess.get("parent_id"))
        .and_then(Value::as_str)
    {
        events.push(json!({
            "kind": "fork",
            "title": "Fork 会话",
            "desc": parent,
            "time": sess.get("created_at"),
        }));
    }
    let starts = visible_user_indices(&messages);
    for (turn_index, &start) in starts.iter().enumerate() {
        let end = starts
            .get(turn_index + 1)
            .copied()
            .unwrap_or(messages.len());
        let user = &messages[start];
        let time = user
            .get("created_at")
            .cloned()
            .or_else(|| user.get("at").cloned())
            .unwrap_or(Value::Null);
        let preview: String = message_text(user).chars().take(200).collect();
        let mut changes = Vec::new();
        for message in &messages[start + 1..end] {
            for tool in message_tool_calls(message) {
                if let Some(change) = file_diff_from_tool(&tool) {
                    changes.push(change);
                }
            }
        }
        let additions: usize = changes
            .iter()
            .filter_map(|change| change.get("additions").and_then(Value::as_u64))
            .map(|value| value as usize)
            .sum();
        let deletions: usize = changes
            .iter()
            .filter_map(|change| change.get("deletions").and_then(Value::as_u64))
            .map(|value| value as usize)
            .sum();
        events.push(json!({
            "kind": "turn",
            "turn_index": turn_index,
            "index": start,
            "message_index": start,
            "time": time,
            "title": if preview.is_empty() {
                format!("第 {} 轮", turn_index + 1)
            } else {
                preview.clone()
            },
            "label": format!("第 {} 轮", turn_index + 1),
            "desc": preview,
            "preview": preview,
            "changes": changes,
            "additions": additions,
            "deletions": deletions,
            "can_restore": turn_index + 1 < starts.len(),
        }));
    }
    json!({
        "session_id": id,
        "events": events,
        "format": "timeline/v1",
    })
}

async fn web_ui_index() -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
        WEB_UI_HTML,
    )
}

async fn web_ui_app_js() -> impl IntoResponse {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        WEB_UI_JS,
    )
}

async fn web_ui_usage_html() -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
        WEB_UI_USAGE_HTML,
    )
}

async fn web_ui_usage_js() -> impl IntoResponse {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        WEB_UI_USAGE_JS,
    )
}

async fn debug_panel(State(state): State<HttpState>) -> impl IntoResponse {
    let sessions = state.backend.list_sessions().await;
    let health = state.backend.health().await;
    let body = format!(
        r#"<!DOCTYPE html><html><head><meta charset="utf-8"><title>kkagent debug</title>
<style>body{{font:13px/1.4 ui-monospace,monospace;background:#111;color:#ddd;padding:16px}}
pre{{background:#1b1b1b;padding:12px;overflow:auto;border-radius:8px}}</style></head>
<body><h1>kkagent debug</h1><h2>health</h2><pre>{health}</pre>
<h2>sessions</h2><pre>{sessions}</pre>
<p><a href="/ui/" style="color:#8cf">Open Web UI</a></p></body></html>"#,
        health = serde_json::to_string_pretty(&health).unwrap_or_default(),
        sessions = serde_json::to_string_pretty(&sessions).unwrap_or_default()
    );
    (
        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
        body,
    )
}

const WEB_UI_HTML: &str = include_str!("../../../apps/web-ui/index.html");
const WEB_UI_JS: &str = include_str!("../../../apps/web-ui/app.js");
const WEB_UI_USAGE_HTML: &str = include_str!("../../../apps/web-ui/usage.html");
const WEB_UI_USAGE_JS: &str = include_str!("../../../apps/web-ui/usage.js");

async fn delete_session(
    State(state): State<HttpState>,
    Path(id): Path<String>,
    Query(q): Query<AuthQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    state
        .backend
        .delete_session(&id)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    state.publish(json!({"type": "session.deleted", "session_id": id}));
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize, Default)]
struct WsQuery {
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    since: u64,
    #[serde(default)]
    session_id: Option<String>,
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<HttpState>,
    Query(query): Query<WsQuery>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, StatusCode> {
    let token = query.token.or_else(|| {
        headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split_once(' '))
            .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
            .map(|(_, token)| token.to_string())
    });
    check_auth(
        &state,
        &AuthQuery {
            token,
            ..Default::default()
        },
    )?;
    Ok(ws.on_upgrade(move |socket| ws_loop(socket, state, query.since, query.session_id)))
}

/// Tells the backend a WebSocket stream is open for as long as it lives.
struct WsConnection(Arc<dyn HttpBackend>);

impl WsConnection {
    fn open(backend: &Arc<dyn HttpBackend>) -> Self {
        backend.ws_connection_changed(true);
        Self(backend.clone())
    }
}

impl Drop for WsConnection {
    fn drop(&mut self) {
        self.0.ws_connection_changed(false);
    }
}

async fn ws_loop(
    mut socket: WebSocket,
    state: HttpState,
    since: u64,
    mut session_filter: Option<String>,
) {
    let _connection = WsConnection::open(&state.backend);
    let mut rx = state.events.subscribe();
    let latest = state.event_sequence.load(Ordering::SeqCst);
    let _ = socket
        .send(Message::Text(
            json!({
                "type": "hello",
                "api": "v1",
                "latest_event_seq": latest,
                "history_capacity": EVENT_HISTORY_CAPACITY,
            })
            .to_string()
            .into(),
        ))
        .await;
    let replay = state.events_since(since, session_filter.as_deref(), EVENT_HISTORY_CAPACITY);
    let replay_through = replay
        .last()
        .and_then(|event| event.get("event_seq"))
        .and_then(Value::as_u64)
        .unwrap_or(since);
    for event in replay {
        if socket
            .send(Message::Text(event.to_string().into()))
            .await
            .is_err()
        {
            return;
        }
    }
    loop {
        tokio::select! {
            evt = rx.recv() => {
                match evt {
                    Ok(v) => {
                        let sequence = v.get("event_seq").and_then(Value::as_u64).unwrap_or(0);
                        if sequence <= replay_through || !event_matches_session(&v, session_filter.as_deref()) {
                            continue;
                        }
                        if socket.send(Message::Text(v.to_string().into())).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        let resync = json!({
                            "type": "resync_required",
                            "skipped": skipped,
                            "latest_event_seq": state.event_sequence.load(Ordering::SeqCst),
                        });
                        if socket.send(Message::Text(resync.to_string().into())).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(t))) => {
                        if t.contains("ping") {
                            let _ = socket.send(Message::Text("{\"type\":\"pong\"}".into())).await;
                        } else if let Ok(v) = serde_json::from_str::<Value>(&t) {
                            if v.get("type").and_then(|x| x.as_str()) == Some("subscribe") {
                                session_filter = v
                                    .get("session_id")
                                    .and_then(Value::as_str)
                                    .map(str::to_string)
                                    .or(session_filter);
                                let _ = socket.send(Message::Text(
                                    json!({
                                        "type":"subscribed",
                                        "channels":["events"],
                                        "session_id": session_filter,
                                        "latest_event_seq": state.event_sequence.load(Ordering::SeqCst),
                                    }).to_string().into()
                                )).await;
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {}
                }
            }
        }
    }
}

fn event_matches_session(event: &Value, session_id: Option<&str>) -> bool {
    match session_id {
        None => true,
        Some(expected) => event.get("session_id").and_then(Value::as_str) == Some(expected),
    }
}

pub fn router(state: HttpState) -> Router {
    Router::new()
        .route("/api/v1/meta", get(meta))
        .route("/api/v1/health", get(health))
        .route("/api/v1/ready", get(readiness))
        .route("/api/v1/metrics", get(metrics))
        .route("/api/v1/usage/tokens", get(usage_tokens))
        .route("/api/v1/events", get(events_history))
        .route("/api/v1/turns/{id}", get(turn_status).delete(cancel_turn))
        .route("/api/v1/sessions", get(list_sessions).post(create_session))
        .route(
            "/api/v1/sessions/{id}",
            get(get_session).patch(patch_session).delete(delete_session),
        )
        .route("/api/v1/sessions/{id}/messages", post(post_message))
        .route("/api/v1/sessions/{id}/fork", post(fork_session))
        .route("/api/v1/sessions/{id}/btw", post(start_btw))
        .route("/api/v1/sessions/{id}/btw/cancel", post(cancel_btw))
        .route("/api/v1/sessions/{id}/interrupt", post(interrupt_session))
        .route("/api/v1/sessions/{id}/compact", post(compact_session))
        .route("/api/v1/sessions/{id}/undo", post(undo_session))
        .route("/api/v1/sessions/{id}/restore", post(restore_session))
        .route("/api/v1/sessions/{id}/archive", post(archive_session))
        .route("/api/v1/sessions/{id}/export", get(export_session))
        .route("/api/v1/approvals/{id}", post(post_approval))
        .route("/api/v1/tools", get(tools))
        .route("/api/v1/tasks", get(tasks))
        .route("/api/v1/skills", get(skills))
        .route("/api/v1/modelCatalog", get(model_catalog))
        .route("/api/v1/models", get(model_catalog))
        .route("/api/v1/config", get(config_get))
        .route("/api/v1/workspaces", get(workspaces))
        .route("/api/v1/workspaceFs", get(workspaces))
        .route("/api/v1/fs", get(fs_read).post(fs_write))
        .route("/api/v1/files", get(files_list))
        .route("/api/v1/search", get(search))
        .route("/api/v1/snapshot", get(snapshot))
        .route("/api/v1/prompts", get(prompts))
        .route("/api/v1/questions", get(questions))
        .route("/api/v1/questions/{id}", post(answer_question))
        .route("/api/v1/plugins", get(plugins_list))
        .route("/api/v1/plugins/install", post(plugins_install))
        .route(
            "/api/v1/plugins/{id}",
            patch(plugin_set_enabled)
                .delete(plugin_remove)
                .post(plugin_update),
        )
        .route(
            "/api/v1/plugins/marketplaces",
            get(marketplaces_list).post(marketplaces_add),
        )
        .route(
            "/api/v1/plugins/marketplaces/{id}",
            delete(marketplace_remove),
        )
        .route("/api/v1/plugins/marketplace", get(marketplace_browse))
        .route(
            "/api/v1/terminals",
            get(terminals_list).post(terminals_create),
        )
        .route(
            "/api/v1/terminals/{id}",
            get(terminal_get).delete(terminal_delete),
        )
        .route("/api/v1/connections", get(connections))
        .route("/api/v1/ws", get(ws_handler))
        .route("/", get(web_ui_index))
        .route("/ui", get(web_ui_index))
        .route("/ui/", get(web_ui_index))
        .route("/ui/app.js", get(web_ui_app_js))
        .route("/ui/usage", get(web_ui_usage_html))
        .route("/ui/usage/", get(web_ui_usage_html))
        .route("/ui/usage.js", get(web_ui_usage_js))
        .route("/debug", get(debug_panel))
        .route("/api/v1/sessions/{id}/timeline", get(session_timeline))
        // A 100 MiB source image expands by roughly 4/3 when base64 encoded.
        .layer(DefaultBodyLimit::max(140 * 1024 * 1024))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state)
}

pub async fn serve(addr: &str, token: Option<String>) -> anyhow::Result<()> {
    serve_with_backend(addr, Arc::new(MemoryBackend::default()), token).await
}

pub async fn serve_with_backend(
    addr: &str,
    backend: Arc<dyn HttpBackend>,
    token: Option<String>,
) -> anyhow::Result<()> {
    let listener = bind(addr, token.as_deref()).await?;
    serve_listener_with_backend(listener, backend, token).await
}

pub async fn bind(addr: &str, token: Option<&str>) -> anyhow::Result<tokio::net::TcpListener> {
    let token = token.filter(|value| !value.trim().is_empty());
    let resolved: Vec<std::net::SocketAddr> = tokio::net::lookup_host(addr).await?.collect();
    if resolved.is_empty() {
        anyhow::bail!("HTTP listen address resolved to no socket addresses: {addr}");
    }
    if token.is_none() && resolved.iter().any(|socket| !socket.ip().is_loopback()) {
        anyhow::bail!(
            "refusing to expose unauthenticated HTTP API on non-loopback address {addr}; pass --http-token or KKAGENT_HTTP_TOKEN"
        );
    }
    let listener = tokio::net::TcpListener::bind(addr).await?;
    Ok(listener)
}

pub async fn serve_listener_with_backend(
    listener: tokio::net::TcpListener,
    backend: Arc<dyn HttpBackend>,
    token: Option<String>,
) -> anyhow::Result<()> {
    serve_listener_with_backend_and_security(
        listener,
        backend,
        token,
        HttpSecurityOptions::default(),
    )
    .await
}

pub async fn serve_listener_with_backend_and_security(
    listener: tokio::net::TcpListener,
    backend: Arc<dyn HttpBackend>,
    token: Option<String>,
    security: HttpSecurityOptions,
) -> anyhow::Result<()> {
    let address = listener.local_addr()?;
    let state = HttpState::with_backend_and_security(backend, token, security);
    let app = router(state);
    tracing::info!("kkagent HTTP listening on http://{address}");
    axum::serve(listener, app).await?;
    Ok(())
}

pub async fn serve_listener_with_backend_security_and_persistence(
    listener: tokio::net::TcpListener,
    backend: Arc<dyn HttpBackend>,
    token: Option<String>,
    security: HttpSecurityOptions,
    persistence: DurableHttpStore,
    ready: Option<tokio::sync::oneshot::Sender<()>>,
) -> anyhow::Result<()> {
    let address = listener.local_addr()?;
    let state = HttpState::with_backend_security_and_persistence(
        backend,
        token,
        security,
        Some(persistence),
    );
    let app = router(state);
    if let Some(ready) = ready {
        let _ = ready.send(());
    }
    tracing::info!("kkagent HTTP listening on http://{address} with durable events/tasks");
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod security_tests {
    use super::*;

    fn temporary_database() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("kkagent-http-{}.db", uuid::Uuid::new_v4()))
    }

    #[tokio::test]
    async fn durable_http_store_satisfies_the_event_store_contract() {
        let store = DurableHttpStore::open_in_memory().unwrap();
        kkagent_core::storage::conformance::event_store_contract(&store, "durable").await;
    }

    #[test]
    fn durable_events_survive_restart() {
        let path = temporary_database();
        {
            let store = DurableHttpStore::open(&path).unwrap();
            let state = HttpState::with_backend_security_and_persistence(
                Arc::new(MemoryBackend::default()),
                None,
                HttpSecurityOptions::default(),
                Some(store),
            );
            state.publish(json!({"type": "turn_start", "session_id": "durable"}));
            state.publish(json!({"type": "turn_end", "session_id": "durable"}));
            assert_eq!(state.event_sequence.load(Ordering::SeqCst), 2);
        }
        {
            let store = DurableHttpStore::open(&path).unwrap();
            let state = HttpState::with_backend_security_and_persistence(
                Arc::new(MemoryBackend::default()),
                None,
                HttpSecurityOptions::default(),
                Some(store),
            );
            let events = state.events_since(0, Some("durable"), 10);
            assert_eq!(events.len(), 2);
            assert_eq!(events[1]["event_seq"], 2);
            assert_eq!(state.event_sequence.load(Ordering::SeqCst), 2);
        }
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn durable_turns_are_idempotent_and_recoverable() {
        let path = temporary_database();
        let task_id;
        {
            let store = DurableHttpStore::open(&path).unwrap();
            let (turn, replayed) = store
                .enqueue_turn("session", "hello", Some("key-1"))
                .unwrap();
            assert!(!replayed);
            task_id = turn.task_id.clone();
            let (same, replayed) = store
                .enqueue_turn("session", "hello", Some("key-1"))
                .unwrap();
            assert!(replayed);
            assert_eq!(same.task_id, task_id);
            assert!(store
                .enqueue_turn("session", "different", Some("key-1"))
                .is_err());
            let claimed = store.claim_turn(&task_id).unwrap();
            assert_eq!(claimed.state, "running");
            assert_eq!(claimed.attempts, 1);
        }
        {
            let store = DurableHttpStore::open(&path).unwrap();
            let pending = store.recoverable_turns().unwrap();
            assert_eq!(pending.len(), 1);
            assert_eq!(pending[0].state, "recovery_pending");
            let claimed = store.claim_turn(&task_id).unwrap();
            assert_eq!(claimed.attempts, 2);
            store.finish_turn(&task_id, "completed", None).unwrap();
            assert_eq!(
                store.get_turn(&task_id).unwrap().unwrap().state,
                "completed"
            );
            assert!(store.cancel_turn(&task_id).is_err());
            let (cancelled, _) = store.enqueue_turn("session", "cancel", None).unwrap();
            store.claim_turn(&cancelled.task_id).unwrap();
            store.cancel_turn(&cancelled.task_id).unwrap();
            store
                .finish_turn(&cancelled.task_id, "completed", None)
                .unwrap();
            assert_eq!(
                store.get_turn(&cancelled.task_id).unwrap().unwrap().state,
                "cancelled"
            );
        }
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn durable_multimodal_prompt_roundtrips_without_misreading_plain_json() {
        let body = PostMessageBody {
            text: "inspect".into(),
            images: vec![HttpImageInput {
                media_type: "image/png".into(),
                data: "AQID".into(),
            }],
        };
        let turn = DurableTurn {
            task_id: "task".into(),
            session_id: "session".into(),
            prompt: format!(
                "{MULTIMODAL_PROMPT_PREFIX}{}",
                serde_json::to_string(&body).unwrap()
            ),
            state: "queued".into(),
            attempts: 0,
            max_attempts: 3,
            created_at: String::new(),
            updated_at: String::new(),
            error: None,
        };
        assert_eq!(turn.message_input(), ("inspect".into(), body.images));
        let mut plain = turn;
        plain.prompt = r#"{"text":"literal user JSON","images":[]}"#.into();
        assert_eq!(plain.message_input().0, plain.prompt);
    }

    #[tokio::test]
    async fn message_endpoint_returns_stable_task_for_idempotent_retry() {
        let path = temporary_database();
        let backend = Arc::new(MemoryBackend::default());
        let session = backend.create_session(None, None).await.unwrap();
        let session_id = session["session_id"].as_str().unwrap().to_string();
        let state = HttpState::with_backend_security_and_persistence(
            backend,
            None,
            HttpSecurityOptions::default(),
            Some(DurableHttpStore::open(&path).unwrap()),
        );
        let mut headers = HeaderMap::new();
        headers.insert("idempotency-key", "stable-key".parse().unwrap());
        let Json(first) = post_message(
            State(state.clone()),
            Path(session_id.clone()),
            Query(AuthQuery::default()),
            headers.clone(),
            Json(PostMessageBody {
                text: "hello".into(),
                images: Vec::new(),
            }),
        )
        .await
        .unwrap();
        let Json(second) = post_message(
            State(state),
            Path(session_id),
            Query(AuthQuery::default()),
            headers,
            Json(PostMessageBody {
                text: "hello".into(),
                images: Vec::new(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(first["task_id"], second["task"]["task_id"]);
        assert_eq!(second["replayed"], true);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn event_history_is_sequenced_filtered_and_updates_turn_state() {
        let state = HttpState::new(Some("secret".into()));
        state.publish(json!({"type": "turn_start", "session_id": "a"}));
        state.publish(json!({"type": "message_delta", "session_id": "b", "text": "x"}));
        state.publish(json!({"type": "turn_end", "session_id": "a"}));

        let events = state.events_since(0, Some("a"), 10);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["event_seq"], 1);
        assert_eq!(events[1]["event_seq"], 3);
        let turns = state.turn_states.lock().unwrap();
        assert_eq!(turns["a"]["state"], "completed");
        assert_eq!(turns["a"]["last_event_seq"], 3);
    }

    #[tokio::test]
    async fn refuses_unauthenticated_non_loopback_listener() {
        let result =
            serve_with_backend("0.0.0.0:0", Arc::new(MemoryBackend::default()), None).await;
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("refusing to expose unauthenticated"));
    }

    #[test]
    fn accepts_bearer_auth_and_injects_legacy_query_token() {
        let state = HttpState::new(Some("complex token".into()));
        let mut request = Request::builder()
            .uri("/api/v1/sessions?view=all")
            .header("authorization", "Bearer complex token")
            .body(axum::body::Body::empty())
            .unwrap();
        authorize_request(&state, &mut request).unwrap();
        let query = request.uri().query().unwrap();
        assert!(query.contains("view=all"));
        assert!(query.contains("token=complex+token"));
    }

    #[test]
    fn rejects_missing_http_auth() {
        let state = HttpState::new(Some("secret".into()));
        let mut request = Request::builder()
            .uri("/api/v1/sessions")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(
            authorize_request(&state, &mut request),
            Err(StatusCode::UNAUTHORIZED)
        );
    }

    #[test]
    fn rejects_cross_origin_browser_requests() {
        let state = HttpState::new(Some("secret".into()));
        let mut request = Request::builder()
            .uri("/api/v1/sessions")
            .header("origin", "https://evil.example")
            .header("host", "127.0.0.1:7788")
            .header("authorization", "Bearer secret")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(
            authorize_request(&state, &mut request),
            Err(StatusCode::FORBIDDEN)
        );
    }

    #[test]
    fn allows_same_origin_browser_requests() {
        let state = HttpState::new(Some("secret".into()));
        let mut request = Request::builder()
            .uri("/api/v1/sessions")
            .header("origin", "http://127.0.0.1:7788")
            .header("host", "127.0.0.1:7788")
            .header("authorization", "Bearer secret")
            .body(axum::body::Body::empty())
            .unwrap();
        assert!(authorize_request(&state, &mut request).is_ok());
    }

    #[tokio::test]
    async fn plugins_endpoints_fail_gracefully_without_live_backend() {
        // MemoryBackend does not implement plugin_rpc; the routes must return a
        // mapped error instead of panicking.
        let backend = Arc::new(MemoryBackend::default());
        let err = backend
            .plugin_rpc("plugins.list", json!({}))
            .await
            .unwrap_err();
        assert!(err.contains("not supported"));
        // Unknown methods are rejected by the live backend whitelist.
        let state = HttpState::new(None);
        let v = marketplace_browse(
            State(state.clone()),
            Query(AuthQuery {
                token: None,
                extra: None,
            }),
        )
        .await;
        assert_eq!(v.map(|_| ()), Err(StatusCode::BAD_REQUEST));
    }

    #[test]
    fn web_ui_shell_is_public_while_api_requires_token() {
        let state = HttpState::new(Some("secret".into()));
        for path in ["/", "/ui", "/ui/", "/ui/app.js"] {
            let mut request = Request::builder()
                .uri(path)
                .body(axum::body::Body::empty())
                .unwrap();
            assert!(
                authorize_request(&state, &mut request).is_ok(),
                "{path} must stay reachable without a token so the browser can show a token prompt"
            );
        }
        // The API surface behind the shell is still protected.
        let mut request = Request::builder()
            .uri("/api/v1/meta")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(
            authorize_request(&state, &mut request),
            Err(StatusCode::UNAUTHORIZED)
        );
    }

    #[test]
    fn scoped_tokens_enforce_read_write_and_terminal_boundaries() {
        let mut scoped_tokens = HashMap::new();
        scoped_tokens.insert("reader".into(), vec!["read".into()]);
        scoped_tokens.insert("writer".into(), vec!["read".into(), "write".into()]);
        let state = HttpState::with_backend_and_security(
            Arc::new(MemoryBackend::default()),
            None,
            HttpSecurityOptions {
                scoped_tokens,
                ..HttpSecurityOptions::default()
            },
        );
        let mut read = Request::builder()
            .uri("/api/v1/sessions")
            .header("authorization", "Bearer reader")
            .body(axum::body::Body::empty())
            .unwrap();
        assert!(authorize_request(&state, &mut read).is_ok());
        let mut write = Request::builder()
            .method("POST")
            .uri("/api/v1/sessions")
            .header("authorization", "Bearer reader")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(
            authorize_request(&state, &mut write),
            Err(StatusCode::FORBIDDEN)
        );
        let mut terminal = Request::builder()
            .method("POST")
            .uri("/api/v1/terminals")
            .header("authorization", "Bearer writer")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(
            authorize_request(&state, &mut terminal),
            Err(StatusCode::FORBIDDEN)
        );
    }

    #[test]
    fn rate_limit_is_per_identity() {
        let state = HttpState::with_backend_and_security(
            Arc::new(MemoryBackend::default()),
            None,
            HttpSecurityOptions {
                requests_per_minute: 2,
                ..HttpSecurityOptions::default()
            },
        );
        assert!(consume_rate_limit(&state, "a"));
        assert!(consume_rate_limit(&state, "a"));
        assert!(!consume_rate_limit(&state, "a"));
        assert!(consume_rate_limit(&state, "b"));
    }

    #[tokio::test]
    async fn terminal_endpoint_runs_and_captures_output() {
        let state = HttpState::new(None);
        let Json(created) = terminals_create(
            State(state.clone()),
            Query(AuthQuery::default()),
            Json(TerminalCreate {
                command: Some("echo terminal-ok".into()),
                cwd: None,
            }),
        )
        .await
        .unwrap();
        let id = created["terminal_id"].as_str().unwrap().to_string();
        for _ in 0..200 {
            let Json(snapshot) = terminal_get(
                State(state.clone()),
                Path(id.clone()),
                Query(AuthQuery::default()),
            )
            .await
            .unwrap();
            if snapshot["terminal"]["status"] == "exited"
                && snapshot["stdout"]
                    .as_str()
                    .unwrap_or("")
                    .contains("terminal-ok")
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("terminal did not exit with captured output");
    }

    #[tokio::test]
    async fn terminal_output_is_bounded_while_the_pipe_is_drained() {
        use tokio::io::AsyncWriteExt;

        let (mut writer, mut reader) = tokio::io::duplex(16 * 1024);
        let write = tokio::spawn(async move {
            let chunk = vec![b'x'; 8192];
            for _ in 0..((MAX_TERMINAL_OUTPUT_BYTES / chunk.len()) + 8) {
                writer.write_all(&chunk).await.unwrap();
            }
        });
        let output = read_bounded_output(&mut reader).await;
        write.await.unwrap();
        assert!(output.len() <= MAX_TERMINAL_OUTPUT_BYTES + 40);
        assert!(output.ends_with(b"... terminal output truncated ...\n"));
    }

    #[tokio::test]
    async fn fork_endpoint_copies_history_and_records_parent() {
        let backend = Arc::new(MemoryBackend::default());
        let created = backend
            .create_session(Some(".".into()), Some("source".into()))
            .await
            .unwrap();
        let session_id = created["session_id"].as_str().unwrap().to_string();
        backend
            .post_message(&session_id, "hello", &[], None)
            .await
            .unwrap();
        let state = HttpState::with_backend(backend, None);
        let Json(forked) = fork_session(
            State(state),
            Path(session_id.clone()),
            Query(AuthQuery::default()),
            Json(ForkSessionBody {
                title: Some("branched".into()),
                message_limit: None,
            }),
        )
        .await
        .unwrap();
        assert_ne!(forked["session_id"], session_id);
        assert_eq!(forked["forked_from"], session_id);
        assert_eq!(forked["title"], "branched");
        assert_eq!(forked["messages"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn rename_endpoint_updates_title() {
        let backend = Arc::new(MemoryBackend::default());
        let created = backend
            .create_session(None, Some("old".into()))
            .await
            .unwrap();
        let session_id = created["session_id"].as_str().unwrap().to_string();
        let state = HttpState::with_backend(backend.clone(), None);
        let Json(updated) = patch_session(
            State(state),
            Path(session_id.clone()),
            Query(AuthQuery::default()),
            Json(PatchSessionBody {
                title: Some("new title".into()),
                ..PatchSessionBody::default()
            }),
        )
        .await
        .unwrap();
        assert_eq!(updated["title"], "new title");
        assert_eq!(
            backend.get_session(&session_id).await.unwrap()["title"],
            "new title"
        );
    }

    #[tokio::test]
    async fn delete_endpoint_removes_session() {
        let backend = Arc::new(MemoryBackend::default());
        let created = backend.create_session(None, None).await.unwrap();
        let session_id = created["session_id"].as_str().unwrap().to_string();
        let state = HttpState::with_backend(backend.clone(), None);
        let Json(result) = delete_session(
            State(state),
            Path(session_id.clone()),
            Query(AuthQuery::default()),
        )
        .await
        .unwrap();
        assert_eq!(result["ok"], true);
        assert!(backend.get_session(&session_id).await.is_none());
    }

    #[tokio::test]
    async fn permission_and_undo_endpoints_update_memory_session() {
        let backend = Arc::new(MemoryBackend::default());
        let created = backend.create_session(None, None).await.unwrap();
        let session_id = created["session_id"].as_str().unwrap().to_string();
        backend
            .post_message(&session_id, "hello", &[], None)
            .await
            .unwrap();
        let state = HttpState::with_backend(backend.clone(), None);
        let Json(patched) = patch_session(
            State(state.clone()),
            Path(session_id.clone()),
            Query(AuthQuery::default()),
            Json(PatchSessionBody {
                permission_mode: Some("yolo".into()),
                plan_mode: Some(true),
                ..PatchSessionBody::default()
            }),
        )
        .await
        .unwrap();
        assert_eq!(patched["permission_mode"], "yolo");
        assert_eq!(patched["plan_mode"], true);
        let Json(undone) = undo_session(
            State(state),
            Path(session_id.clone()),
            Query(AuthQuery::default()),
            Json(UndoBody { count: Some(1) }),
        )
        .await
        .unwrap();
        assert_eq!(undone["undone"], 1);
        assert!(undone["messages"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn btw_endpoint_returns_side_answer_for_memory_backend() {
        let backend = Arc::new(MemoryBackend::default());
        let created = backend.create_session(None, None).await.unwrap();
        let session_id = created["session_id"].as_str().unwrap().to_string();
        let state = HttpState::with_backend(backend, None);
        let Json(result) = start_btw(
            State(state),
            Path(session_id),
            Query(AuthQuery::default()),
            Json(BtwBody {
                text: Some("还有别的方案吗".into()),
                question: None,
            }),
        )
        .await
        .unwrap();
        assert_eq!(result["ok"], true);
        assert!(result["agent_id"].as_str().is_some());
        assert!(result["answer"]
            .as_str()
            .unwrap()
            .contains("还有别的方案吗"));
    }

    #[test]
    fn timeline_groups_file_diffs_by_user_turn() {
        let sess = json!({
            "forked_from": "parent",
            "created_at": "2026-08-17T10:00:00Z",
            "messages": [
                {"role": "user", "text": "fix the renderer", "created_at": "2026-08-17T10:00:01Z"},
                {
                    "role": "assistant",
                    "content": [
                        {"type": "text", "text": "looking"},
                        {"type": "tool_use", "id": "t1", "name": "bash", "input": {"command": "ls"}},
                        {"type": "tool_use", "id": "t2", "name": "Edit", "input": {
                            "path": "app.js",
                            "old_string": "foo",
                            "new_string": "bar"
                        }}
                    ],
                    "created_at": "2026-08-17T10:00:02Z"
                },
                {
                    "role": "user",
                    "content": [
                        {"type": "tool_result", "tool_use_id": "t1", "content": "ok"},
                        {"type": "tool_result", "tool_use_id": "t2", "content": "updated app.js"}
                    ]
                },
                {
                    "role": "assistant",
                    "content": [
                        {"type": "tool_use", "id": "t3", "name": "Edit", "input": {
                            "path": "main.rs",
                            "old_string": "a",
                            "new_string": "b"
                        }}
                    ]
                },
                {
                    "role": "user",
                    "content": [
                        {"type": "tool_result", "tool_use_id": "t3", "content": "updated main.rs"}
                    ]
                },
                {"role": "assistant", "content": [{"type": "text", "text": "done"}]},
                {"role": "user", "text": "also tweak css", "created_at": "2026-08-17T10:00:03Z"},
                {
                    "role": "assistant",
                    "content": [
                        {"type": "tool_use", "id": "t4", "name": "Write", "input": {
                            "path": "app.css",
                            "content": "body { color: red; }"
                        }}
                    ]
                }
            ]
        });
        let timeline = build_session_timeline("child", &sess);
        let events = timeline["events"].as_array().unwrap();
        assert_eq!(events[0]["kind"], "fork");
        assert_eq!(events[1]["kind"], "turn");
        assert_eq!(events[1]["turn_index"], 0);
        assert_eq!(events[1]["title"], "fix the renderer");
        assert_eq!(events[1]["can_restore"], true);
        let first_changes = events[1]["changes"].as_array().unwrap();
        assert_eq!(first_changes.len(), 2);
        assert_eq!(first_changes[0]["path"], "app.js");
        assert_eq!(first_changes[1]["path"], "main.rs");
        assert!(first_changes[0]["diff"].as_str().unwrap().contains("-foo"));
        assert!(first_changes[0]["diff"].as_str().unwrap().contains("+bar"));
        assert_eq!(events[2]["kind"], "turn");
        assert_eq!(events[2]["title"], "also tweak css");
        assert_eq!(events[2]["can_restore"], false);
        assert_eq!(events[2]["changes"][0]["path"], "app.css");
    }

    #[tokio::test]
    async fn restore_endpoint_truncates_later_turns() {
        let backend = Arc::new(MemoryBackend::default());
        let created = backend.create_session(None, None).await.unwrap();
        let session_id = created["session_id"].as_str().unwrap().to_string();
        backend
            .post_message(&session_id, "first", &[], None)
            .await
            .unwrap();
        backend
            .post_message(&session_id, "second", &[], None)
            .await
            .unwrap();
        let state = HttpState::with_backend(backend.clone(), None);
        let Json(result) = restore_session(
            State(state),
            Path(session_id.clone()),
            Query(AuthQuery::default()),
            Json(RestoreBody {
                turn_index: Some(0),
            }),
        )
        .await
        .unwrap();
        assert_eq!(result["ok"], true);
        assert_eq!(result["restored"], true);
        assert_eq!(result["messages"].as_array().unwrap().len(), 1);
    }

    fn scratch_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("kkagent-fs-api-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn file_reads_are_ranged_bounded_and_binary_safe() {
        let dir = scratch_dir();
        let text = dir.join("note.txt");
        std::fs::write(&text, "héllo wörld").unwrap();

        // Whole file: text, size and mtime come back with it.
        let whole = read_file_range(&text, 0, None, FsEncoding::Utf8)
            .await
            .unwrap();
        assert_eq!(whole["content"], "héllo wörld");
        assert_eq!(whole["encoding"], "utf8");
        assert_eq!(whole["size"], 13);
        assert_eq!(whole["truncated"], false);
        assert_eq!(whole["next_offset"], Value::Null);
        assert!(whole["modified_at"].as_str().is_some());

        // A range that would end inside "é" (bytes 1..3) stops before it and
        // says where to continue.
        let cut = read_file_range(&text, 0, Some(2), FsEncoding::Utf8)
            .await
            .unwrap();
        assert_eq!(cut["content"], "h");
        assert_eq!(cut["bytes_read"], 1);
        assert_eq!(cut["truncated"], true);
        assert_eq!(cut["next_offset"], 1);
        let rest = read_file_range(&text, 1, None, FsEncoding::Utf8)
            .await
            .unwrap();
        assert_eq!(rest["content"], "éllo wörld");

        // `limit=0` is a stat.
        let stat = read_file_range(&text, 0, Some(0), FsEncoding::Utf8)
            .await
            .unwrap();
        assert_eq!(stat["content"], "");
        assert_eq!(stat["size"], 13);
        assert_eq!(stat["truncated"], true);

        // Binary data: refused as text (415), served as base64.
        let blob = dir.join("blob.bin");
        std::fs::write(&blob, [0u8, 159, 146, 150, 255]).unwrap();
        let refused = read_file_range(&blob, 0, None, FsEncoding::Utf8)
            .await
            .unwrap_err();
        assert_eq!(refused.kind, FsErrorKind::NotText);
        assert_eq!(refused.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        let encoded = read_file_range(&blob, 1, Some(3), FsEncoding::Base64)
            .await
            .unwrap();
        assert_eq!(encoded["encoding"], "base64");
        assert_eq!(encoded["content"], "n5KW");
        assert_eq!(encoded["bytes_read"], 3);
        assert_eq!(encoded["next_offset"], 4);

        // A file over the per-call cap is refused whole, readable in ranges.
        let big = dir.join("big.bin");
        let file = std::fs::File::create(&big).unwrap();
        file.set_len(FS_READ_MAX_BYTES + 10).unwrap();
        let too_large = read_file_range(&big, 0, None, FsEncoding::Base64)
            .await
            .unwrap_err();
        assert_eq!(too_large.kind, FsErrorKind::TooLarge);
        assert_eq!(too_large.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let tail = read_file_range(&big, FS_READ_MAX_BYTES, None, FsEncoding::Base64)
            .await
            .unwrap();
        assert_eq!(tail["bytes_read"], 10);
        assert_eq!(tail["truncated"], false);
        let capped = read_file_range(&big, 0, Some(u64::MAX), FsEncoding::Base64)
            .await
            .unwrap();
        assert_eq!(capped["bytes_read"], FS_READ_MAX_BYTES);
        assert_eq!(capped["truncated"], true);

        // Errors carry the status that describes them.
        let missing = read_file_range(&dir.join("nope"), 0, None, FsEncoding::Utf8)
            .await
            .unwrap_err();
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        let directory = read_file_range(&dir, 0, None, FsEncoding::Utf8)
            .await
            .unwrap_err();
        assert_eq!(directory.status(), StatusCode::BAD_REQUEST);
        let past_end = read_file_range(&text, 99, None, FsEncoding::Utf8)
            .await
            .unwrap_err();
        assert_eq!(past_end.status(), StatusCode::BAD_REQUEST);
        assert!(FsEncoding::parse(Some("latin1")).is_err());
        assert_eq!(FsEncoding::parse(None).unwrap(), FsEncoding::Utf8);
        assert_eq!(
            FsEncoding::parse(Some("base64")).unwrap(),
            FsEncoding::Base64
        );
        assert_eq!(FsError::forbidden("no").status(), StatusCode::FORBIDDEN);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn fs_endpoint_answers_errors_with_their_status_not_200() {
        let state = HttpState::new(None);
        let dir = scratch_dir();
        let missing = fs_read(
            State(state.clone()),
            Query(FsPathQuery {
                path: dir.join("absent.txt").display().to_string(),
                offset: None,
                limit: None,
                encoding: None,
                token: None,
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(missing.into_response().status(), StatusCode::NOT_FOUND);

        let empty = fs_read(
            State(state.clone()),
            Query(FsPathQuery {
                path: " ".into(),
                offset: None,
                limit: None,
                encoding: None,
                token: None,
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(empty.into_response().status(), StatusCode::BAD_REQUEST);

        let bad_encoding = fs_read(
            State(state.clone()),
            Query(FsPathQuery {
                path: dir.display().to_string(),
                offset: None,
                limit: None,
                encoding: Some("rot13".into()),
                token: None,
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(
            bad_encoding.into_response().status(),
            StatusCode::BAD_REQUEST
        );

        let file = dir.join("ok.txt");
        std::fs::write(&file, "fine").unwrap();
        let Json(read) = fs_read(
            State(state.clone()),
            Query(FsPathQuery {
                path: file.display().to_string(),
                offset: None,
                limit: None,
                encoding: None,
                token: None,
            }),
        )
        .await
        .unwrap();
        assert_eq!(read["content"], "fine");
        assert_eq!(read["path"], file.display().to_string());

        let not_a_dir = files_list(
            State(state),
            Query(FilesQuery {
                path: Some(file.display().to_string()),
                limit: None,
                cursor: None,
                token: None,
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(not_a_dir.into_response().status(), StatusCode::BAD_REQUEST);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn directory_listing_pages_with_size_and_mtime() {
        let dir = scratch_dir();
        for index in 0..5 {
            std::fs::write(dir.join(format!("f{index}.txt")), vec![b'x'; index + 1]).unwrap();
        }
        std::fs::create_dir(dir.join("sub")).unwrap();

        let first = list_directory_page(&dir, None, 4).await.unwrap();
        assert_eq!(first["total"], 6);
        assert_eq!(first["count"], 4);
        assert_eq!(first["truncated"], true);
        assert_eq!(first["next_cursor"], "f3.txt");
        let entries = first["entries"].as_array().unwrap();
        assert_eq!(entries[0]["name"], "f0.txt");
        assert_eq!(entries[0]["size"], 1);
        assert_eq!(entries[0]["type"], "file");
        assert_eq!(entries[0]["is_dir"], false);
        assert!(entries[0]["modified_at"].as_str().is_some());

        let second = list_directory_page(&dir, Some("f3.txt"), 4).await.unwrap();
        let names: Vec<&str> = second["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["f4.txt", "sub"]);
        assert_eq!(second["truncated"], false);
        assert_eq!(second["next_cursor"], Value::Null);
        let directory = &second["entries"][1];
        assert_eq!(directory["type"], "dir");
        assert_eq!(directory["is_dir"], true);
        assert_eq!(directory["size"], Value::Null);

        // The page size is bounded, and a file is not a directory.
        let clamped = list_directory_page(&dir, None, usize::MAX).await.unwrap();
        assert_eq!(clamped["count"], 6);
        let error = list_directory_page(&dir.join("f0.txt"), None, 10)
            .await
            .unwrap_err();
        assert_eq!(error.kind, FsErrorKind::BadRequest);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn session_creation_is_idempotent_per_key() {
        let backend = Arc::new(MemoryBackend::default());
        let state = HttpState::with_backend_security_and_persistence(
            backend.clone(),
            None,
            HttpSecurityOptions::default(),
            Some(DurableHttpStore::open_in_memory().unwrap()),
        );
        let create = |key: Option<&'static str>, title: &'static str| {
            let state = state.clone();
            async move {
                let mut headers = HeaderMap::new();
                if let Some(key) = key {
                    headers.insert("idempotency-key", key.parse().unwrap());
                }
                create_session(
                    State(state),
                    Query(AuthQuery::default()),
                    headers,
                    Json(CreateSessionBody {
                        workspace: Some(".".into()),
                        title: Some(title.into()),
                    }),
                )
                .await
            }
        };

        let Json(first) = create(Some("job-1"), "review").await.unwrap();
        assert_eq!(first["replayed"], false);
        let Json(retry) = create(Some("job-1"), "review").await.unwrap();
        assert_eq!(retry["replayed"], true);
        assert_eq!(retry["session_id"], first["session_id"]);
        assert_eq!(
            backend.list_sessions().await["sessions"]
                .as_array()
                .map(Vec::len),
            Some(1),
            "the retry did not create a second session"
        );

        // Same key, other parameters: conflict.
        assert_eq!(
            create(Some("job-1"), "other title").await.unwrap_err(),
            StatusCode::CONFLICT
        );
        // A different key, or no key at all, is a new session.
        let Json(second) = create(Some("job-2"), "review").await.unwrap();
        assert_ne!(second["session_id"], first["session_id"]);
        let Json(plain) = create(None, "review").await.unwrap();
        assert!(plain.get("replayed").is_none());
        assert_ne!(plain["session_id"], first["session_id"]);

        // A key whose session is gone makes a fresh one rather than echoing
        // a dead id.
        backend
            .delete_session(first["session_id"].as_str().unwrap())
            .await
            .unwrap();
        let Json(again) = create(Some("job-1"), "review").await.unwrap();
        assert_eq!(again["replayed"], false);
        assert_ne!(again["session_id"], first["session_id"]);

        // Keys must be usable.
        let mut headers = HeaderMap::new();
        headers.insert("idempotency-key", "x".repeat(300).parse().unwrap());
        assert_eq!(idempotency_key_of(&headers), Err(StatusCode::BAD_REQUEST));
    }

    #[test]
    fn session_create_claims_cover_in_flight_failed_and_stale_attempts() {
        let store = DurableHttpStore::open_in_memory().unwrap();
        assert_eq!(
            store.begin_session_create("k", "h").unwrap(),
            SessionCreateClaim::Fresh
        );
        // Still creating: a concurrent twin must wait, not create another.
        assert_eq!(
            store.begin_session_create("k", "h").unwrap(),
            SessionCreateClaim::InProgress
        );
        assert_eq!(
            store.begin_session_create("k", "other").unwrap(),
            SessionCreateClaim::Conflict
        );
        // A failed creation releases the key.
        store.abandon_session_create("k").unwrap();
        assert_eq!(
            store.begin_session_create("k", "h").unwrap(),
            SessionCreateClaim::Fresh
        );
        store
            .complete_session_create("k", &json!({"session_id": "s1"}))
            .unwrap();
        assert_eq!(
            store.begin_session_create("k", "h").unwrap(),
            SessionCreateClaim::Replay(json!({"session_id": "s1"}))
        );
        // `abandon` never discards a finished creation.
        store.abandon_session_create("k").unwrap();
        assert!(matches!(
            store.begin_session_create("k", "h").unwrap(),
            SessionCreateClaim::Replay(_)
        ));

        // An attempt abandoned by a dead process is taken over after a while.
        store.begin_session_create("stale", "h").unwrap();
        {
            let connection = store.connection.lock().unwrap();
            connection
                .execute(
                    "UPDATE http_session_creates SET claimed_at = claimed_at - 3600
                     WHERE idempotency_key = 'stale'",
                    [],
                )
                .unwrap();
        }
        assert_eq!(
            store.begin_session_create("stale", "h").unwrap(),
            SessionCreateClaim::Fresh
        );
    }

    #[test]
    fn meta_tells_a_restart_from_a_replaced_server() {
        let path = temporary_database();
        let boot = |path: &std::path::Path| {
            let state = HttpState::with_backend_security_and_persistence(
                Arc::new(MemoryBackend::default()),
                None,
                HttpSecurityOptions::default(),
                Some(DurableHttpStore::open(path).unwrap()),
            );
            (
                state.meta["instance_id"].as_str().unwrap().to_string(),
                state.meta["boot_id"].as_str().unwrap().to_string(),
            )
        };
        let (instance_a, boot_a) = boot(&path);
        let (instance_b, boot_b) = boot(&path);
        assert_eq!(instance_a, instance_b, "same database, same server");
        assert_ne!(boot_a, boot_b, "every start is a new boot");
        let other = temporary_database();
        let (instance_c, _) = boot(&other);
        assert_ne!(instance_a, instance_c, "another database, another server");

        let ephemeral = HttpState::new(None);
        assert_eq!(ephemeral.meta["instance_id"], ephemeral.meta["boot_id"]);
        for capability in [
            "fsRange",
            "filesPaging",
            "sessionIdempotency",
            "taskFinished",
        ] {
            assert!(
                ephemeral.meta["capabilities"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|value| value == capability),
                "{capability}"
            );
        }
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(other);
    }

    #[test]
    fn task_finished_is_the_final_task_state() {
        let state = HttpState::new(None);
        state.publish(json!({"type": "turn_start", "session_id": "s"}));
        state.publish(json!({"type": "turn_end", "session_id": "s"}));
        // `turn_end` is only the end of a model round; goals keep going.
        state.publish(json!({"type": "turn_start", "session_id": "s"}));
        assert_eq!(state.turn_states.lock().unwrap()["s"]["state"], "running");
        state.publish(json!({
            "type": "task_finished", "session_id": "s", "task_id": "t", "state": "failed",
        }));
        assert_eq!(state.turn_states.lock().unwrap()["s"]["state"], "failed");
        let events = state.events_since(0, Some("s"), 10);
        assert_eq!(events.last().unwrap()["type"], "task_finished");
    }
}
