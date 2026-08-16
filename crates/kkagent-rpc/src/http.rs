//! REST API v1 + WebSocket (kap-server route matrix subset) with AgentLoop backend hooks.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::AsyncReadExt;
use tokio::process::Child;
use tokio::sync::{broadcast, Mutex};

const MAX_HTTP_TERMINALS: usize = 64;
const MAX_TERMINAL_COMMAND_BYTES: usize = 64 * 1024;
const MAX_TERMINAL_OUTPUT_BYTES: usize = 1024 * 1024;
const EVENT_HISTORY_CAPACITY: usize = 2048;
const DEFAULT_TASK_MAX_ATTEMPTS: u32 = 3;
const MULTIMODAL_PROMPT_PREFIX: &str = "kkagent:multimodal:v1\n";

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
                ON durable_turns(state, lease_expires_at, created_at);",
            )?;
        }
        Ok(Self { connection })
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
    async fn post_message(
        &self,
        id: &str,
        text: &str,
        images: &[HttpImageInput],
        task_id: Option<&str>,
    ) -> Result<Value, String>;
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
    ) -> Result<Value, String>;
    async fn cancel_turn(&self, _task_id: &str) -> Result<Value, String> {
        Err("turn cancellation is not supported by this backend".into())
    }
    async fn fs_read(&self, path: &str) -> Result<String, String>;
    async fn fs_write(&self, path: &str, content: &str) -> Result<(), String>;
    async fn list_files(&self, _path: &str) -> Result<Value, String> {
        Err("file listing is not supported by this backend".into())
    }
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
            "created_at": chrono::Utc::now().to_rfc3339(),
            "messages": [],
        });
        self.sessions.lock().await.insert(id, sess.clone());
        Ok(sess)
    }
    async fn get_session(&self, id: &str) -> Option<Value> {
        self.sessions.lock().await.get(id).cloned()
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
    async fn list_tools(&self) -> Value {
        json!({"tools": [
            "Read","Write","Edit","Bash","Grep","Glob","TodoList","WebSearch","FetchURL",
            "Task","AskUserQuestion","SelectTools","Skill","CronCreate","ReadMediaFile"
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
        json!({"config_dir": dirs_home(), "api": "v1"})
    }
    async fn approve(
        &self,
        id: &str,
        decision: &str,
        feedback: Option<String>,
    ) -> Result<Value, String> {
        Ok(json!({"ok": true, "approval_id": id, "decision": decision, "feedback": feedback}))
    }
    async fn fs_read(&self, path: &str) -> Result<String, String> {
        std::fs::read_to_string(path).map_err(|e| e.to_string())
    }
    async fn fs_write(&self, path: &str, content: &str) -> Result<(), String> {
        if let Some(parent) = std::path::Path::new(path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(path, content).map_err(|e| e.to_string())
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

fn dirs_home() -> String {
    dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".kkagent")
        .display()
        .to_string()
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
        let state = Self {
            backend,
            meta: json!({
                "name": "kkagent",
                "version": env!("CARGO_PKG_VERSION"),
                "api": ["v1"],
                "capabilities": [
                    "sessions","messages","approvals","ws","tools","tasks","skills",
                    "files","fs","workspaces","config","modelCatalog","search",
                    "terminals","questions","prompts","snapshot","eventReplay","turnStatus",
                    "health","readiness","metrics","durableEvents","durableTasks","idempotency"
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
        let state = match event_type {
            "turn_start" => Some("running"),
            "turn_end" => Some("completed"),
            "approval_requested" => Some("waiting_approval"),
            "question_asked" => Some("waiting_question"),
            "error" => Some("failed"),
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
    check_auth(&state, &AuthQuery { token: query.token })?;
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

async fn cancel_turn(
    State(state): State<HttpState>,
    Path(task_id): Path<String>,
    Query(query): Query<AuthQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &query)?;
    let result = state
        .backend
        .cancel_turn(&task_id)
        .await
        .map_err(|_| StatusCode::CONFLICT)?;
    state.publish(json!({"type": "turn_cancelled", "task_id": task_id}));
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

async fn create_session(
    State(state): State<HttpState>,
    Query(q): Query<AuthQuery>,
    Json(body): Json<CreateSessionBody>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    let sess = state
        .backend
        .create_session(body.workspace, body.title)
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    if let Some(id) = sess.get("session_id").and_then(|v| v.as_str()) {
        state.publish(json!({"type": "session.created", "session_id": id}));
    }
    Ok(Json(sess))
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
            Err(StatusCode::NOT_FOUND)
        }
    }
}

#[derive(Deserialize)]
struct ApprovalBody {
    decision: String,
    #[serde(default)]
    feedback: Option<String>,
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
        .approve(&id, &body.decision, body.feedback.clone())
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    state.publish(json!({
        "type": "approval",
        "approval_id": id,
        "decision": body.decision,
        "feedback": body.feedback,
    }));
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

#[derive(Deserialize)]
struct FsPathQuery {
    path: String,
    #[serde(default)]
    token: Option<String>,
}

#[derive(Deserialize)]
struct FilesQuery {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    token: Option<String>,
}

async fn fs_read(
    State(state): State<HttpState>,
    Query(path_q): Query<FsPathQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(
        &state,
        &AuthQuery {
            token: path_q.token.clone(),
        },
    )?;
    match state.backend.fs_read(&path_q.path).await {
        Ok(content) => Ok(Json(json!({"path": path_q.path, "content": content}))),
        Err(e) => Ok(Json(json!({"error": e}))),
    }
}

async fn fs_write(
    State(state): State<HttpState>,
    Query(q): Query<AuthQuery>,
    Json(body): Json<FsBody>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    let content = body.content.unwrap_or_default();
    state
        .backend
        .fs_write(&body.path, &content)
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    Ok(Json(json!({"ok": true, "path": body.path})))
}

async fn files_list(
    State(state): State<HttpState>,
    Query(query): Query<FilesQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &AuthQuery { token: query.token })?;
    state
        .backend
        .list_files(query.path.as_deref().unwrap_or("."))
        .await
        .map(Json)
        .map_err(|_| StatusCode::BAD_REQUEST)
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
    let messages = sess
        .get("messages")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let events: Vec<Value> = messages
        .into_iter()
        .enumerate()
        .map(|(i, m)| {
            json!({
                "index": i,
                "role": m.get("role"),
                "preview": m.get("content").cloned().unwrap_or(Value::Null).to_string().chars().take(200).collect::<String>(),
            })
        })
        .collect();
    Ok(Json(json!({
        "session_id": id,
        "events": events,
        "format": "timeline/v1",
    })))
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
) -> Result<impl IntoResponse, StatusCode> {
    check_auth(&state, &AuthQuery { token: query.token })?;
    Ok(ws.on_upgrade(move |socket| ws_loop(socket, state, query.since, query.session_id)))
}

async fn ws_loop(
    mut socket: WebSocket,
    state: HttpState,
    since: u64,
    mut session_filter: Option<String>,
) {
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
        .route("/api/v1/events", get(events_history))
        .route("/api/v1/turns/{id}", get(turn_status).delete(cancel_turn))
        .route("/api/v1/sessions", get(list_sessions).post(create_session))
        .route(
            "/api/v1/sessions/{id}",
            get(get_session).delete(delete_session),
        )
        .route("/api/v1/sessions/{id}/messages", post(post_message))
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
        for _ in 0..50 {
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
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
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
}
