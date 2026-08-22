use chrono::Utc;
use rusqlite::{params, params_from_iter, Connection};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

/// Shared SQLite handle used by transcript / durable HTTP / subagent stores.
pub type SharedSqlite = Arc<Mutex<Connection>>;

/// Serializes `migrate()` across connections within this process. Multiple
/// connections running `CREATE ... IF NOT EXISTS` / `ALTER TABLE`
/// concurrently on the same SQLite file can hit `SQLITE_SCHEMA` ("The
/// database schema changed") when one connection's DDL invalidates another's
/// prepared statement mid-flight.
static MIGRATE_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();

/// Advisory cross-process lock guarding SQLite first-time schema creation.
/// Held for the duration of `migrate()` so two kkagent processes racing on a
/// fresh `transcripts.db` cannot interleave DDL.
struct MigrateFileLock {
    file: std::fs::File,
}

impl MigrateFileLock {
    #[cfg(unix)]
    fn acquire(path: &Path) -> anyhow::Result<Self> {
        use std::os::unix::io::AsRawFd;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)?;
        let fd = file.as_raw_fd();
        // SAFETY: flock(2) on a valid fd; retried on EINTR.
        let rc = unsafe { libc::flock(fd, libc::LOCK_EX) };
        if rc != 0 {
            return Err(anyhow::anyhow!("lock {}: {rc}", path.display(),));
        }
        Ok(Self { file })
    }

    #[cfg(windows)]
    fn acquire(path: &Path) -> anyhow::Result<Self> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::HANDLE;
        use windows_sys::Win32::Storage::FileSystem::{LockFileEx, LOCKFILE_EXCLUSIVE_LOCK};
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)?;
        let handle = file.as_raw_handle() as HANDLE;
        // SAFETY: LockFileEx on a valid file handle; blocks until exclusive.
        let mut overlapped: windows_sys::Win32::System::IO::OVERLAPPED =
            unsafe { std::mem::zeroed() };
        let ok = unsafe {
            LockFileEx(
                handle,
                LOCKFILE_EXCLUSIVE_LOCK,
                0,
                u32::MAX,
                u32::MAX,
                &mut overlapped,
            )
        };
        if ok == 0 {
            return Err(anyhow::anyhow!("lock {}: failed", path.display()));
        }
        Ok(Self { file })
    }
}

impl Drop for MigrateFileLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            // SAFETY: unlock on the same fd we locked; best-effort.
            unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::Foundation::HANDLE;
            use windows_sys::Win32::Storage::FileSystem::UnlockFileEx;
            let handle = self.file.as_raw_handle() as HANDLE;
            let mut overlapped: windows_sys::Win32::System::IO::OVERLAPPED =
                unsafe { std::mem::zeroed() };
            // SAFETY: mirrors the LockFileEx range (0..u32::MAX).
            unsafe { UnlockFileEx(handle, 0, u32::MAX, u32::MAX, &mut overlapped) };
        }
    }
}

/// Cheap to clone: the SQLite connection is behind an `Arc`.
#[derive(Clone)]
pub struct TranscriptDb {
    conn: SharedSqlite,
}

#[derive(Debug, Clone)]
pub struct SessionRecord {
    pub session_id: String,
    pub title: Option<String>,
    pub model: String,
    /// `NULL` inherits global config, `""` disables fallback, otherwise an alias.
    pub fallback_model: Option<String>,
    pub working_dir: String,
    pub created_at: String,
    pub updated_at: String,
    pub message_count: u32,
    pub is_archived: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct MessageRecord {
    pub id: i64,
    pub session_id: String,
    pub role: String,
    pub content_json: String,
    pub token_count: Option<u32>,
    pub created_at: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchHit {
    pub session_id: String,
    pub role: String,
    pub preview: String,
    pub created_at: String,
    pub title: String,
    pub tool_name: String,
}

fn searchable_body(content_json: &str) -> String {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(content_json) {
        let mut parts = Vec::new();
        collect_text(&value, &mut parts);
        if !parts.is_empty() {
            return parts.join("\n");
        }
    }
    content_json.to_string()
}

fn collect_text(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(s) => out.push(s.clone()),
        serde_json::Value::Array(items) => {
            for item in items {
                collect_text(item, out);
            }
        }
        serde_json::Value::Object(map) => {
            for (key, item) in map {
                if matches!(
                    key.as_str(),
                    "text" | "content" | "thinking" | "name" | "path" | "command" | "query"
                ) {
                    collect_text(item, out);
                } else if key == "type" {
                    continue;
                } else {
                    collect_text(item, out);
                }
            }
        }
        _ => {}
    }
}

fn extract_tool_name(content_json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(content_json).ok()?;
    if let Some(arr) = value.as_array() {
        for item in arr {
            if item.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                if let Some(name) = item.get("name").and_then(|v| v.as_str()) {
                    return Some(name.to_string());
                }
            }
        }
    }
    None
}

fn fts_query_from_user(query: &str) -> String {
    query
        .split_whitespace()
        .filter(|t| !t.is_empty())
        .filter_map(|token| {
            let cleaned: String = token
                .chars()
                .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-' || *c == '.')
                .collect();
            // Skip tokens without at least one alphanumeric/underscore character:
            // punctuation-only prefixes like `"."*` match nothing on modern FTS5 and
            // raise syntax errors on older builds.
            if !cleaned.chars().any(|c| c.is_alphanumeric() || c == '_') {
                return None;
            }
            Some(format!("\"{cleaned}\"*"))
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Open `transcripts.db` once; callers can share the Arc across stores.
pub fn open_shared_sqlite(db_path: &Path) -> anyhow::Result<SharedSqlite> {
    // Skip for in-memory / empty parents (Path(":memory:").parent() == Some(""))
    if let Some(parent) = db_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let conn = Connection::open(db_path)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(Arc::new(Mutex::new(conn)))
}

pub fn open_shared_sqlite_memory() -> anyhow::Result<SharedSqlite> {
    let conn = Connection::open_in_memory()?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(Arc::new(Mutex::new(conn)))
}

impl TranscriptDb {
    fn lock(&self) -> anyhow::Result<MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|_| anyhow::anyhow!("transcript db lock poisoned"))
    }

    pub fn shared(&self) -> SharedSqlite {
        Arc::clone(&self.conn)
    }

    pub fn from_shared(conn: SharedSqlite) -> anyhow::Result<Self> {
        let db = Self { conn };
        db.migrate()?;
        Ok(db)
    }

    /// Absolute path of the main database file backing this connection, if
    /// it is a real file (not `:memory:`).
    fn database_path(&self) -> anyhow::Result<Option<std::path::PathBuf>> {
        let conn = self.lock()?;
        let path: String = conn.query_row("PRAGMA database_list", [], |row| row.get(2))?;
        if path.is_empty() || path == ":memory:" {
            return Ok(None);
        }
        Ok(Some(std::path::PathBuf::from(path)))
    }

    pub fn open(db_path: &Path) -> anyhow::Result<Self> {
        Self::from_shared(open_shared_sqlite(db_path)?)
    }

    pub fn open_in_memory() -> anyhow::Result<Self> {
        Self::from_shared(open_shared_sqlite_memory()?)
    }

    pub fn open_default() -> anyhow::Result<Self> {
        let dir = kkagent_config::default_config_dir();
        let db_path = dir.join("transcripts.db");
        Self::open(&db_path)
    }

    fn migrate(&self) -> anyhow::Result<()> {
        // Serialize schema creation both in-process (global mutex) and
        // cross-process (advisory file lock next to the db). Without this,
        // concurrent first opens of the same database race their DDL and can
        // fail with SQLITE_SCHEMA ("The database schema changed").
        let _inproc = MIGRATE_MUTEX
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _file_lock = match self.database_path()? {
            Some(path) => {
                let lock_path = path.with_extension("mlock");
                Some(MigrateFileLock::acquire(&lock_path)?)
            }
            None => None, // :memory: connections never race another process
        };
        let conn = self.lock()?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
            PRAGMA synchronous = FULL;
            CREATE TABLE IF NOT EXISTS sessions (
                session_id TEXT PRIMARY KEY,
                title TEXT,
                model TEXT NOT NULL DEFAULT '',
                fallback_model TEXT,
                working_dir TEXT NOT NULL DEFAULT '.',
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                message_count INTEGER NOT NULL DEFAULT 0,
                is_archived INTEGER NOT NULL DEFAULT 0,
                summary TEXT
            );

            CREATE TABLE IF NOT EXISTS messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL REFERENCES sessions(session_id),
                role TEXT NOT NULL,
                content_json TEXT NOT NULL,
                token_count INTEGER,
                created_at TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_messages_session
                ON messages(session_id, id);

            CREATE INDEX IF NOT EXISTS idx_sessions_updated
                ON sessions(updated_at DESC);

            CREATE TABLE IF NOT EXISTS tool_results (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                turn_id TEXT,
                tool_call_id TEXT NOT NULL,
                tool_name TEXT NOT NULL,
                file_path TEXT NOT NULL,
                output_size_chars INTEGER NOT NULL,
                output_size_bytes INTEGER NOT NULL,
                created_at INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_tool_results_session_id
                ON tool_results(session_id);

            CREATE INDEX IF NOT EXISTS idx_tool_results_tool_call_id
                ON tool_results(tool_call_id);

            CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
                session_id UNINDEXED,
                role UNINDEXED,
                body,
                tool_name UNINDEXED,
                created_at UNINDEXED,
                title UNINDEXED,
                tokenize = 'unicode61'
            );
            ",
        )?;
        let has_fallback_model: bool = conn.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM pragma_table_info('sessions') WHERE name = 'fallback_model'
            )",
            [],
            |row| row.get(0),
        )?;
        if !has_fallback_model {
            conn.execute("ALTER TABLE sessions ADD COLUMN fallback_model TEXT", [])?;
        }
        drop(conn);
        self.ensure_fts_populated()?;
        Ok(())
    }

    fn ensure_fts_populated(&self) -> anyhow::Result<()> {
        let conn = self.lock()?;
        let fts_count: i64 =
            conn.query_row("SELECT COUNT(*) FROM messages_fts", [], |row| row.get(0))?;
        if fts_count > 0 {
            return Ok(());
        }
        let msg_count: i64 =
            conn.query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))?;
        if msg_count == 0 {
            return Ok(());
        }
        drop(conn);
        self.rebuild_fts()?;
        Ok(())
    }

    /// Rebuild the full-text index from `messages` (blocking; call off agent hot path).
    pub fn rebuild_fts(&self) -> anyhow::Result<()> {
        let conn = self.lock()?;
        let tx = conn.unchecked_transaction()?;
        tx.execute("DELETE FROM messages_fts", [])?;
        {
            let mut stmt = tx.prepare(
                "SELECT m.session_id, m.role, m.content_json, m.created_at,
                        COALESCE(s.title, '')
                 FROM messages m
                 LEFT JOIN sessions s ON s.session_id = m.session_id
                 ORDER BY m.id",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })?;
            let mut insert = tx.prepare(
                "INSERT INTO messages_fts(session_id, role, body, tool_name, created_at, title)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for row in rows {
                let (session_id, role, content_json, created_at, title) = row?;
                let body = searchable_body(&content_json);
                let tool_name = extract_tool_name(&content_json).unwrap_or_default();
                insert.execute(params![
                    session_id, role, body, tool_name, created_at, title
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    fn index_message_locked(
        conn: &rusqlite::Connection,
        session_id: &str,
        role: &str,
        content_json: &str,
        created_at: &str,
    ) -> anyhow::Result<()> {
        let title: String = conn
            .query_row(
                "SELECT COALESCE(title, '') FROM sessions WHERE session_id = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .unwrap_or_default();
        let tool_name = extract_tool_name(content_json).unwrap_or_default();
        let body = searchable_body(content_json);
        conn.execute(
            "INSERT INTO messages_fts(session_id, role, body, tool_name, created_at, title)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![session_id, role, body, tool_name, created_at, title],
        )?;
        Ok(())
    }

    /// Cross-session full-text search with optional filters.
    pub fn search_messages(
        &self,
        query: &str,
        limit: usize,
        title_contains: Option<&str>,
        tool_name: Option<&str>,
        since: Option<&str>,
        until: Option<&str>,
    ) -> anyhow::Result<Vec<SearchHit>> {
        let q = query.trim();
        if q.is_empty() {
            return Ok(Vec::new());
        }
        let fts_query = fts_query_from_user(q);
        if fts_query.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT f.session_id, f.role, snippet(messages_fts, 2, '«', '»', '…', 32),
                    f.created_at, f.title, f.tool_name
             FROM messages_fts f
             WHERE messages_fts MATCH ?1
             ORDER BY rank
             LIMIT ?2",
        )?;
        let fetch_limit = (limit.clamp(1, 200) * 4) as i64;
        let rows = stmt.query_map(params![fts_query, fetch_limit], |row| {
            Ok(SearchHit {
                session_id: row.get(0)?,
                role: row.get(1)?,
                preview: row.get(2)?,
                created_at: row.get(3)?,
                title: row.get(4)?,
                tool_name: row.get(5)?,
            })
        })?;
        let mut hits = Vec::new();
        for row in rows {
            let hit = row?;
            if let Some(title) = title_contains.filter(|s| !s.is_empty()) {
                if !hit.title.to_lowercase().contains(&title.to_lowercase()) {
                    continue;
                }
            }
            if let Some(tool) = tool_name.filter(|s| !s.is_empty()) {
                if !hit.tool_name.eq_ignore_ascii_case(tool) {
                    continue;
                }
            }
            if let Some(since) = since.filter(|s| !s.is_empty()) {
                if hit.created_at.as_str() < since {
                    continue;
                }
            }
            if let Some(until) = until.filter(|s| !s.is_empty()) {
                if hit.created_at.as_str() > until {
                    continue;
                }
            }
            hits.push(hit);
            if hits.len() >= limit.clamp(1, 200) {
                break;
            }
        }
        Ok(hits)
    }

    pub fn create_session(
        &self,
        session_id: &str,
        model: &str,
        working_dir: &str,
    ) -> anyhow::Result<()> {
        let now = Utc::now().to_rfc3339();
        self.lock()?.execute(
            "INSERT INTO sessions (session_id, model, working_dir, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![session_id, model, working_dir, now, now],
        )?;
        Ok(())
    }

    pub fn set_title(&self, session_id: &str, title: &str) -> anyhow::Result<()> {
        let changed = self.lock()?.execute(
            "UPDATE sessions SET title = ?1, updated_at = ?2 WHERE session_id = ?3",
            params![title, Utc::now().to_rfc3339(), session_id],
        )?;
        if changed == 0 {
            anyhow::bail!("session not found: {session_id}");
        }
        Ok(())
    }

    pub fn set_model(&self, session_id: &str, model: &str) -> anyhow::Result<()> {
        let changed = self.lock()?.execute(
            "UPDATE sessions SET model = ?1, updated_at = ?2 WHERE session_id = ?3",
            params![model, Utc::now().to_rfc3339(), session_id],
        )?;
        if changed == 0 {
            anyhow::bail!("session not found: {session_id}");
        }
        Ok(())
    }

    pub fn set_fallback_model(
        &self,
        session_id: &str,
        fallback_model: Option<&str>,
    ) -> anyhow::Result<()> {
        let changed = self.lock()?.execute(
            "UPDATE sessions SET fallback_model = ?1, updated_at = ?2 WHERE session_id = ?3",
            params![fallback_model, Utc::now().to_rfc3339(), session_id],
        )?;
        if changed == 0 {
            anyhow::bail!("session not found: {session_id}");
        }
        Ok(())
    }

    pub fn append_message(
        &self,
        session_id: &str,
        role: &str,
        content_json: &str,
        token_count: Option<u32>,
    ) -> anyhow::Result<i64> {
        let now = Utc::now().to_rfc3339();
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO messages (session_id, role, content_json, token_count, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![session_id, role, content_json, token_count, now],
        )?;
        let id = conn.last_insert_rowid();
        let _ = Self::index_message_locked(&conn, session_id, role, content_json, &now);

        conn.execute(
            "UPDATE sessions SET message_count = message_count + 1, updated_at = ?1
             WHERE session_id = ?2",
            params![now, session_id],
        )?;
        Ok(id)
    }

    /// Atomically append a batch of messages in the supplied order.
    pub fn append_messages(
        &self,
        session_id: &str,
        messages: &[(String, String)],
    ) -> anyhow::Result<()> {
        if messages.is_empty() {
            return Ok(());
        }
        let conn = self.lock()?;
        let transaction = conn.unchecked_transaction()?;
        let now = Utc::now().to_rfc3339();
        {
            let mut statement = transaction.prepare(
                "INSERT INTO messages (session_id, role, content_json, token_count, created_at)
                 VALUES (?1, ?2, ?3, NULL, ?4)",
            )?;
            for (role, content_json) in messages {
                statement.execute(params![session_id, role, content_json, now])?;
                let _ =
                    Self::index_message_locked(&transaction, session_id, role, content_json, &now);
            }
        }
        transaction.execute(
            "UPDATE sessions SET message_count = message_count + ?1, updated_at = ?2
             WHERE session_id = ?3",
            params![messages.len() as u32, now, session_id],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Atomically replace a session transcript in the supplied order.
    pub fn replace_messages(
        &self,
        session_id: &str,
        messages: &[(String, String)],
        summary: Option<&str>,
    ) -> anyhow::Result<()> {
        let conn = self.lock()?;
        let transaction = conn.unchecked_transaction()?;
        transaction.execute(
            "DELETE FROM messages WHERE session_id = ?1",
            params![session_id],
        )?;
        let _ = transaction.execute(
            "DELETE FROM messages_fts WHERE session_id = ?1",
            params![session_id],
        );
        let now = Utc::now().to_rfc3339();
        {
            let mut statement = transaction.prepare(
                "INSERT INTO messages (session_id, role, content_json, token_count, created_at)
                 VALUES (?1, ?2, ?3, NULL, ?4)",
            )?;
            for (role, content_json) in messages {
                statement.execute(params![session_id, role, content_json, now])?;
                let _ =
                    Self::index_message_locked(&transaction, session_id, role, content_json, &now);
            }
        }
        transaction.execute(
            "UPDATE sessions SET summary = ?1, message_count = ?2, updated_at = ?3
             WHERE session_id = ?4",
            params![summary, messages.len() as u32, now, session_id],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn load_messages(&self, session_id: &str) -> anyhow::Result<Vec<MessageRecord>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, session_id, role, content_json, token_count, created_at
             FROM messages WHERE session_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![session_id], |row| {
            Ok(MessageRecord {
                id: row.get(0)?,
                session_id: row.get(1)?,
                role: row.get(2)?,
                content_json: row.get(3)?,
                token_count: row.get(4)?,
                created_at: row.get(5)?,
            })
        })?;
        let mut messages = Vec::new();
        for row in rows {
            messages.push(row?);
        }
        Ok(messages)
    }

    pub fn list_sessions(&self, limit: usize) -> anyhow::Result<Vec<SessionRecord>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT session_id, title, model, fallback_model, working_dir, created_at, updated_at,
                    message_count, is_archived
             FROM sessions WHERE is_archived = 0
             ORDER BY updated_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as u32], |row| {
            Ok(SessionRecord {
                session_id: row.get(0)?,
                title: row.get(1)?,
                model: row.get(2)?,
                fallback_model: row.get(3)?,
                working_dir: row.get(4)?,
                created_at: row.get(5)?,
                updated_at: row.get(6)?,
                message_count: row.get(7)?,
                is_archived: row.get::<_, i32>(8)? != 0,
            })
        })?;
        let mut sessions = Vec::new();
        for row in rows {
            sessions.push(row?);
        }
        Ok(sessions)
    }

    /// Fetch records for a session-list page with a bounded number of SQL
    /// statements instead of preparing one query per session.
    pub fn sessions_by_ids(
        &self,
        session_ids: &[String],
    ) -> anyhow::Result<HashMap<String, SessionRecord>> {
        const SQLITE_BATCH_SIZE: usize = 500;

        let conn = self.lock()?;
        let mut sessions = HashMap::with_capacity(session_ids.len());
        for batch in session_ids.chunks(SQLITE_BATCH_SIZE) {
            if batch.is_empty() {
                continue;
            }
            let placeholders = std::iter::repeat_n("?", batch.len())
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "SELECT session_id, title, model, fallback_model, working_dir, created_at, updated_at,
                        message_count, is_archived
                 FROM sessions WHERE session_id IN ({placeholders})"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params_from_iter(batch.iter()), |row| {
                Ok(SessionRecord {
                    session_id: row.get(0)?,
                    title: row.get(1)?,
                    model: row.get(2)?,
                    fallback_model: row.get(3)?,
                    working_dir: row.get(4)?,
                    created_at: row.get(5)?,
                    updated_at: row.get(6)?,
                    message_count: row.get(7)?,
                    is_archived: row.get::<_, i32>(8)? != 0,
                })
            })?;
            for row in rows {
                let session = row?;
                sessions.insert(session.session_id.clone(), session);
            }
        }
        Ok(sessions)
    }

    pub fn get_session(&self, session_id: &str) -> anyhow::Result<Option<SessionRecord>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT session_id, title, model, fallback_model, working_dir, created_at, updated_at,
                    message_count, is_archived
             FROM sessions WHERE session_id = ?1",
        )?;
        let mut rows = stmt.query_map(params![session_id], |row| {
            Ok(SessionRecord {
                session_id: row.get(0)?,
                title: row.get(1)?,
                model: row.get(2)?,
                fallback_model: row.get(3)?,
                working_dir: row.get(4)?,
                created_at: row.get(5)?,
                updated_at: row.get(6)?,
                message_count: row.get(7)?,
                is_archived: row.get::<_, i32>(8)? != 0,
            })
        })?;
        Ok(rows.next().and_then(|r| r.ok()))
    }

    pub fn archive_session(&self, session_id: &str) -> anyhow::Result<()> {
        self.set_archived(session_id, true)
    }

    /// Record a persisted oversized tool result.
    pub fn record_tool_result(&self, record: &ToolResultRecord) -> anyhow::Result<()> {
        self.lock()?.execute(
            "INSERT INTO tool_results
             (id, session_id, turn_id, tool_call_id, tool_name, file_path,
              output_size_chars, output_size_bytes, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                record.id,
                record.session_id,
                record.turn_id,
                record.tool_call_id,
                record.tool_name,
                record.file_path,
                record.output_size_chars as u32,
                record.output_size_bytes as u32,
                record.created_at
            ],
        )?;
        Ok(())
    }

    pub fn list_tool_results(&self, session_id: &str) -> anyhow::Result<Vec<ToolResultRecord>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, session_id, turn_id, tool_call_id, tool_name, file_path,
                    output_size_chars, output_size_bytes, created_at
             FROM tool_results WHERE session_id = ?1 ORDER BY created_at",
        )?;
        let rows = stmt.query_map(params![session_id], |row| {
            Ok(ToolResultRecord {
                id: row.get(0)?,
                session_id: row.get(1)?,
                turn_id: row.get(2)?,
                tool_call_id: row.get(3)?,
                tool_name: row.get(4)?,
                file_path: row.get(5)?,
                output_size_chars: row.get::<_, i64>(6)? as usize,
                output_size_bytes: row.get::<_, i64>(7)? as usize,
                created_at: row.get(8)?,
            })
        })?;
        let mut records = Vec::new();
        for row in rows {
            records.push(row?);
        }
        Ok(records)
    }

    /// Delete one tool-result record (DB row only; for migration/tests).
    pub fn delete_tool_result_record(&self, id: &str) -> anyhow::Result<()> {
        self.lock()?
            .execute("DELETE FROM tool_results WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Delete every DB row belonging to `session_id` in a single transaction:
    /// `tool_results`, `messages`, `messages_quarantine`, `sessions`.
    ///
    /// Called by the trash archival flow after the JSONL export succeeded.
    /// `messages_quarantine` is created lazily by integrity checks, so its
    /// delete is skipped when the table does not exist yet.
    pub fn purge_session(&self, session_id: &str) -> anyhow::Result<()> {
        let conn = self.lock()?;
        let has_quarantine: bool = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'table' AND name = 'messages_quarantine'",
            [],
            |row| row.get::<_, i64>(0),
        )? > 0;
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM tool_results WHERE session_id = ?1",
            params![session_id],
        )?;
        if has_quarantine {
            tx.execute(
                "DELETE FROM messages_quarantine WHERE session_id = ?1",
                params![session_id],
            )?;
        }
        tx.execute(
            "DELETE FROM messages WHERE session_id = ?1",
            params![session_id],
        )?;
        let changed = tx.execute(
            "DELETE FROM sessions WHERE session_id = ?1",
            params![session_id],
        )?;
        tx.commit()?;
        if changed == 0 {
            anyhow::bail!("session not found: {session_id}");
        }
        Ok(())
    }

    pub fn set_archived(&self, session_id: &str, archived: bool) -> anyhow::Result<()> {
        let changed = self.lock()?.execute(
            "UPDATE sessions SET is_archived = ?1, updated_at = ?2 WHERE session_id = ?3",
            params![archived, Utc::now().to_rfc3339(), session_id],
        )?;
        if changed == 0 {
            anyhow::bail!("session not found: {session_id}");
        }
        Ok(())
    }

    /// Clone a resumable transcript. `turn_index` keeps the historical API's
    /// approximation of two transcript messages per completed turn.
    pub fn fork_session(
        &self,
        source_id: &str,
        target_id: &str,
        title: Option<&str>,
        turn_index: Option<usize>,
    ) -> anyhow::Result<()> {
        let message_limit = turn_index.map(|turn| turn.saturating_add(1).saturating_mul(2));
        self.fork_session_inner(source_id, target_id, title, message_limit, false)
    }

    /// Clone exactly the messages before a selected editable prompt.
    pub fn fork_session_with_message_limit(
        &self,
        source_id: &str,
        target_id: &str,
        title: Option<&str>,
        message_limit: usize,
    ) -> anyhow::Result<()> {
        self.fork_session_inner(source_id, target_id, title, Some(message_limit), true)
    }

    fn fork_session_inner(
        &self,
        source_id: &str,
        target_id: &str,
        title: Option<&str>,
        message_limit: Option<usize>,
        strict_message_limit: bool,
    ) -> anyhow::Result<()> {
        let source = self
            .get_session(source_id)?
            .ok_or_else(|| anyhow::anyhow!("session not found: {source_id}"))?;
        if self.get_session(target_id)?.is_some() {
            anyhow::bail!("session already exists: {target_id}");
        }
        let mut messages = self.load_messages(source_id)?;
        if let Some(message_limit) = message_limit {
            if strict_message_limit && message_limit > messages.len() {
                anyhow::bail!(
                    "message limit {message_limit} exceeds transcript length {}",
                    messages.len()
                );
            }
            messages.truncate(message_limit);
        }

        let conn = self.lock()?;
        let transaction = conn.unchecked_transaction()?;
        let now = Utc::now().to_rfc3339();
        transaction.execute(
            "INSERT INTO sessions
             (session_id, title, model, fallback_model, working_dir, created_at, updated_at, message_count, is_archived, summary)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7, 0, ?8)",
            params![
                target_id,
                title.or(source.title.as_deref()),
                source.model,
                source.fallback_model,
                source.working_dir,
                now,
                messages.len() as u32,
                Option::<String>::None,
            ],
        )?;
        {
            let mut statement = transaction.prepare(
                "INSERT INTO messages (session_id, role, content_json, token_count, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for message in &messages {
                statement.execute(params![
                    target_id,
                    message.role,
                    message.content_json,
                    message.token_count,
                    message.created_at,
                ])?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    /// Keep the first `keep_count` messages; delete the rest.
    pub fn truncate_messages(&self, session_id: &str, keep_count: usize) -> anyhow::Result<()> {
        let messages = self.load_messages(session_id)?;
        if messages.len() <= keep_count {
            return Ok(());
        }
        let cutoff_id = messages[keep_count].id;
        let conn = self.lock()?;
        conn.execute(
            "DELETE FROM messages WHERE session_id = ?1 AND id >= ?2",
            params![session_id, cutoff_id],
        )?;
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "UPDATE sessions SET message_count = ?1, updated_at = ?2 WHERE session_id = ?3",
            params![keep_count as u32, now, session_id],
        )?;
        Ok(())
    }

    /// Compact: replace messages older than `keep_last_n` with a summary
    pub fn compact_session(
        &self,
        session_id: &str,
        keep_last_n: usize,
        summary: &str,
    ) -> anyhow::Result<u32> {
        let messages = self.load_messages(session_id)?;
        if messages.len() <= keep_last_n {
            return Ok(0);
        }

        let cutoff = messages.len() - keep_last_n;
        let summary_json = serde_json::json!([{"type": "text", "text": summary}]).to_string();
        let mut replacement = Vec::with_capacity(keep_last_n + 1);
        replacement.push(("system".to_string(), summary_json));
        replacement.extend(
            messages[cutoff..]
                .iter()
                .map(|message| (message.role.clone(), message.content_json.clone())),
        );
        self.replace_messages(session_id, &replacement, Some(summary))?;

        Ok(cutoff as u32)
    }

    /// Read-only integrity scan. Corrupt message rows are reported but not deleted.
    pub fn check_integrity(&self) -> anyhow::Result<IntegrityReport> {
        let conn = self.lock()?;
        let mut report = IntegrityReport::default();
        let mut stmt = conn.prepare("SELECT session_id FROM sessions")?;
        let sessions = stmt.query_map([], |row| row.get::<_, String>(0))?;
        for s in sessions {
            match s {
                Ok(id) => report.ok_sessions.push(id),
                Err(e) => report
                    .bad_sessions
                    .push(format!("unreadable session row: {e}")),
            }
        }
        let mut msg_stmt =
            conn.prepare("SELECT id, session_id, role, content_json FROM messages")?;
        let rows = msg_stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        for row in rows {
            match row {
                Ok((id, sid, role, content)) => {
                    if role.trim().is_empty() {
                        report.isolated_messages.push(IsolatedMessage {
                            id,
                            session_id: sid,
                            reason: "empty role".into(),
                        });
                        continue;
                    }
                    if serde_json::from_str::<serde_json::Value>(&content).is_err() {
                        report.isolated_messages.push(IsolatedMessage {
                            id,
                            session_id: sid,
                            reason: "invalid content_json".into(),
                        });
                    } else {
                        report.ok_messages += 1;
                    }
                }
                Err(e) => report
                    .bad_sessions
                    .push(format!("unreadable message row: {e}")),
            }
        }
        Ok(report)
    }

    /// Backup DB then move corrupt message rows into `messages_quarantine`.
    pub fn repair_with_backup(&self, backup_path: &Path) -> anyhow::Result<IntegrityReport> {
        {
            let conn = self.lock()?;
            // rusqlite Path::new for file DBs; memory DBs get a marker file.
            if let Some(path) = conn.path() {
                if !path.is_empty() && path != ":memory:" {
                    let _ = std::fs::copy(path, backup_path);
                } else {
                    std::fs::write(backup_path, b"memory-db-no-file-backup")?;
                }
            } else {
                std::fs::write(backup_path, b"memory-db-no-file-backup")?;
            }
        }
        let mut report = self.check_integrity()?;
        let conn = self.lock()?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS messages_quarantine (
                id INTEGER PRIMARY KEY,
                session_id TEXT,
                role TEXT,
                content_json TEXT,
                reason TEXT,
                quarantined_at TEXT
            );",
        )?;
        let now = Utc::now().to_rfc3339();
        for iso in &report.isolated_messages {
            conn.execute(
                "INSERT OR REPLACE INTO messages_quarantine (id, session_id, role, content_json, reason, quarantined_at)
                 SELECT id, session_id, role, content_json, ?1, ?2 FROM messages WHERE id = ?3",
                params![iso.reason, now, iso.id],
            )?;
            conn.execute("DELETE FROM messages WHERE id = ?1", params![iso.id])?;
        }
        report.repaired = report.isolated_messages.len();
        Ok(report)
    }
}

#[derive(Debug, Default, Clone)]
pub struct IntegrityReport {
    pub ok_sessions: Vec<String>,
    pub bad_sessions: Vec<String>,
    pub ok_messages: u64,
    pub isolated_messages: Vec<IsolatedMessage>,
    pub repaired: usize,
}

#[derive(Debug, Clone)]
pub struct IsolatedMessage {
    pub id: i64,
    pub session_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolResultRecord {
    pub id: String,
    pub session_id: String,
    pub turn_id: Option<String>,
    pub tool_call_id: String,
    pub tool_name: String,
    pub file_path: String,
    pub output_size_chars: usize,
    pub output_size_bytes: usize,
    /// Unix timestamp (seconds).
    pub created_at: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_db() -> TranscriptDb {
        TranscriptDb::open(&PathBuf::from(":memory:")).unwrap()
    }

    /// Concurrent first opens of the same database file used to race their
    /// DDL and fail with SQLITE_SCHEMA ("The database schema changed") —
    /// observed as flaky `messages_fts` vtable creation when tests started
    /// sharing a fresh per-process home. The migrate mutex + advisory file
    /// lock serialize schema creation.
    #[test]
    fn concurrent_first_open_serializes_schema_creation() {
        let dir = std::env::temp_dir().join(format!("kkagent-db-race-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("transcripts.db");

        let mut handles = Vec::new();
        for _ in 0..8 {
            let path = db_path.clone();
            handles.push(std::thread::spawn(move || {
                TranscriptDb::open(&path).map(|db| {
                    // Touch FTS right after open: the racing failure mode.
                    db.search_messages("needle", 1, None, None, None, None)
                })
            }));
        }
        for handle in handles {
            let result = handle.join().expect("thread panicked");
            // FTS query result may legitimately be empty; we only care that
            // open + migrate + search did not error under concurrency.
            result.expect("concurrent open/migrate failed").unwrap();
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_create_and_load_session() {
        let db = test_db();
        db.create_session("s1", "claude", ".").unwrap();

        let session = db.get_session("s1").unwrap().unwrap();
        assert_eq!(session.session_id, "s1");
        assert_eq!(session.model, "claude");
        assert!(session.fallback_model.is_none());
        assert_eq!(session.message_count, 0);
    }

    #[test]
    fn test_append_and_load_messages() {
        let db = test_db();
        db.create_session("s1", "claude", ".").unwrap();

        db.append_message("s1", "user", r#"[{"type":"text","text":"hello"}]"#, Some(5))
            .unwrap();
        db.append_message(
            "s1",
            "assistant",
            r#"[{"type":"text","text":"hi"}]"#,
            Some(3),
        )
        .unwrap();

        let messages = db.load_messages("s1").unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[1].role, "assistant");

        let session = db.get_session("s1").unwrap().unwrap();
        assert_eq!(session.message_count, 2);
    }

    #[test]
    fn test_list_sessions() {
        let db = test_db();
        db.create_session("s1", "claude", ".").unwrap();
        db.create_session("s2", "gpt", ".").unwrap();

        let sessions = db.list_sessions(10).unwrap();
        assert_eq!(sessions.len(), 2);
    }

    #[test]
    fn test_sessions_by_ids_batches_records_and_ignores_missing_ids() {
        let db = test_db();
        db.create_session("s1", "claude", ".").unwrap();
        db.create_session("s2", "gpt", ".").unwrap();
        db.append_message("s2", "user", r#"[{"type":"text","text":"hello"}]"#, None)
            .unwrap();

        let records = db
            .sessions_by_ids(&["s2".into(), "missing".into(), "s1".into()])
            .unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records["s1"].message_count, 0);
        assert_eq!(records["s2"].message_count, 1);
    }

    #[test]
    fn test_archive_session() {
        let db = test_db();
        db.create_session("s1", "claude", ".").unwrap();
        db.archive_session("s1").unwrap();

        let sessions = db.list_sessions(10).unwrap();
        assert_eq!(sessions.len(), 0); // archived sessions don't show in list
    }

    #[test]
    fn test_compact_session() {
        let db = test_db();
        db.create_session("s1", "claude", ".").unwrap();

        for i in 0..10 {
            db.append_message(
                "s1",
                "user",
                &format!(r#"[{{"type":"text","text":"msg{}"}}]"#, i),
                None,
            )
            .unwrap();
        }

        let deleted = db
            .compact_session("s1", 3, "Summary of first 7 messages")
            .unwrap();
        assert_eq!(deleted, 7);

        let messages = db.load_messages("s1").unwrap();
        assert_eq!(messages.len(), 4); // 3 kept + 1 summary
        assert_eq!(messages[0].role, "system");
        assert!(messages[0].content_json.contains("Summary of first 7"));
        assert!(messages[1].content_json.contains("msg7"));
        assert!(messages[3].content_json.contains("msg9"));

        let session = db.get_session("s1").unwrap().unwrap();
        assert_eq!(session.message_count, 4);
    }

    #[test]
    fn test_set_title() {
        let db = test_db();
        db.create_session("s1", "claude", ".").unwrap();
        db.set_title("s1", "My Session").unwrap();

        let session = db.get_session("s1").unwrap().unwrap();
        assert_eq!(session.title.as_deref(), Some("My Session"));
    }

    #[test]
    fn test_set_model_persists() {
        let db = test_db();
        db.create_session("s1", "claude", ".").unwrap();
        db.set_model("s1", "kimi-code/kimi-k2.5").unwrap();
        let session = db.get_session("s1").unwrap().unwrap();
        assert_eq!(session.model, "kimi-code/kimi-k2.5");
        assert!(db.set_model("missing", "x").is_err());
    }

    #[test]
    fn test_set_fallback_model_persists_all_session_modes() {
        let db = test_db();
        db.create_session("s1", "primary", ".").unwrap();

        db.set_fallback_model("s1", Some("backup")).unwrap();
        assert_eq!(
            db.get_session("s1")
                .unwrap()
                .unwrap()
                .fallback_model
                .as_deref(),
            Some("backup")
        );

        db.set_fallback_model("s1", Some("")).unwrap();
        assert_eq!(
            db.get_session("s1")
                .unwrap()
                .unwrap()
                .fallback_model
                .as_deref(),
            Some("")
        );

        db.set_fallback_model("s1", None).unwrap();
        assert!(db
            .get_session("s1")
            .unwrap()
            .unwrap()
            .fallback_model
            .is_none());
    }

    #[test]
    fn migration_adds_fallback_model_to_existing_session_table() {
        let shared = open_shared_sqlite_memory().unwrap();
        shared
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TABLE sessions (
                    session_id TEXT PRIMARY KEY,
                    title TEXT,
                    model TEXT NOT NULL DEFAULT '',
                    working_dir TEXT NOT NULL DEFAULT '.',
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    message_count INTEGER NOT NULL DEFAULT 0,
                    is_archived INTEGER NOT NULL DEFAULT 0,
                    summary TEXT
                );",
            )
            .unwrap();
        let db = TranscriptDb::from_shared(shared).unwrap();
        db.create_session("legacy", "primary", ".").unwrap();
        db.set_fallback_model("legacy", Some("backup")).unwrap();
        assert_eq!(
            db.get_session("legacy")
                .unwrap()
                .unwrap()
                .fallback_model
                .as_deref(),
            Some("backup")
        );
    }

    #[test]
    fn test_fork_session_is_resumable_and_independent() {
        let db = test_db();
        db.create_session("source", "model", "/workspace").unwrap();
        for index in 0..6 {
            db.append_message(
                "source",
                if index % 2 == 0 { "user" } else { "assistant" },
                &format!(r#"[{{"type":"text","text":"msg{index}"}}]"#),
                None,
            )
            .unwrap();
        }

        db.fork_session("source", "fork", Some("Forked"), Some(1))
            .unwrap();

        let fork = db.get_session("fork").unwrap().unwrap();
        assert_eq!(fork.title.as_deref(), Some("Forked"));
        assert_eq!(fork.working_dir, "/workspace");
        assert_eq!(fork.message_count, 4);
        assert_eq!(db.load_messages("fork").unwrap().len(), 4);
        db.append_message("source", "user", "[]", None).unwrap();
        assert_eq!(db.load_messages("fork").unwrap().len(), 4);
    }

    #[test]
    fn test_fork_session_with_exact_message_limit_supports_editing_first_turn() {
        let db = test_db();
        db.create_session("source", "model", "/workspace").unwrap();
        for index in 0..5 {
            db.append_message(
                "source",
                if index == 0 || index == 4 {
                    "user"
                } else {
                    "assistant"
                },
                &format!(r#"[{{"type":"text","text":"msg{index}"}}]"#),
                None,
            )
            .unwrap();
        }

        db.fork_session_with_message_limit("source", "empty-fork", Some("Edit"), 0)
            .unwrap();
        assert!(db.load_messages("empty-fork").unwrap().is_empty());

        db.fork_session_with_message_limit("source", "partial-fork", Some("Edit"), 4)
            .unwrap();
        let fork = db.load_messages("partial-fork").unwrap();
        assert_eq!(fork.len(), 4);
        assert!(fork.last().unwrap().content_json.contains("msg3"));
        assert!(db
            .fork_session_with_message_limit("source", "invalid-fork", None, 6)
            .is_err());
    }

    #[test]
    fn metadata_updates_reject_missing_sessions() {
        let db = test_db();
        assert!(db.set_title("missing", "title").is_err());
        assert!(db.set_archived("missing", true).is_err());
    }

    #[test]
    fn shared_connection_sees_same_tables() {
        let shared = open_shared_sqlite_memory().unwrap();
        let db = TranscriptDb::from_shared(Arc::clone(&shared)).unwrap();
        db.create_session("s1", "m", ".").unwrap();
        let conn = shared.lock().unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_fts_search_across_sessions() {
        let db = test_db();
        db.create_session("s1", "claude", ".").unwrap();
        db.set_title("s1", "alpha project").unwrap();
        db.create_session("s2", "gpt", ".").unwrap();
        db.set_title("s2", "beta project").unwrap();
        db.append_message(
            "s1",
            "user",
            r#"[{"type":"text","text":"unique-fts-apple-token"}]"#,
            None,
        )
        .unwrap();
        db.append_message(
            "s2",
            "assistant",
            r#"[{"type":"text","text":"unique-fts-orange-token"}]"#,
            None,
        )
        .unwrap();
        let hits = db
            .search_messages("unique-fts-apple", 10, None, None, None, None)
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, "s1");
        let titled = db
            .search_messages("unique-fts-orange", 10, Some("beta"), None, None, None)
            .unwrap();
        assert_eq!(titled.len(), 1);
        assert_eq!(titled[0].session_id, "s2");
    }

    #[test]
    fn test_fts_query_from_user_skips_punctuation_only_tokens() {
        assert_eq!(fts_query_from_user("hello"), "\"hello\"*");
        assert_eq!(fts_query_from_user("foo bar"), "\"foo\"* \"bar\"*");
        // Punctuation-only tokens are dropped instead of producing `"."*`
        // which matches nothing (and errors on older FTS5 builds).
        assert_eq!(fts_query_from_user("."), "");
        assert_eq!(fts_query_from_user("- -- ..."), "");
        assert_eq!(fts_query_from_user("!!! ???"), "");
        // Mixed tokens keep their alphanumeric core.
        assert_eq!(fts_query_from_user("foo-bar."), "\"foo-bar.\"*");
        // A query that is entirely punctuation yields no hits, not an error.
        let db = test_db();
        db.create_session("s1", "claude", ".").unwrap();
        db.append_message("s1", "user", r#"[{"type":"text","text":"hello"}]"#, None)
            .unwrap();
        let hits = db
            .search_messages("--- ... !!!", 10, None, None, None, None)
            .unwrap();
        assert!(hits.is_empty());
    }
}
