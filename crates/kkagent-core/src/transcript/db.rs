use chrono::Utc;
use rusqlite::{params, Connection};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

/// Shared SQLite handle used by transcript / durable HTTP / subagent stores.
pub type SharedSqlite = Arc<Mutex<Connection>>;

pub struct TranscriptDb {
    conn: SharedSqlite,
}

#[derive(Debug, Clone)]
pub struct SessionRecord {
    pub session_id: String,
    pub title: Option<String>,
    pub model: String,
    pub working_dir: String,
    pub created_at: String,
    pub updated_at: String,
    pub message_count: u32,
    pub is_archived: bool,
}

#[derive(Debug, Clone)]
pub struct MessageRecord {
    pub id: i64,
    pub session_id: String,
    pub role: String,
    pub content_json: String,
    pub token_count: Option<u32>,
    pub created_at: String,
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
        let conn = self.lock()?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
            PRAGMA synchronous = FULL;
            CREATE TABLE IF NOT EXISTS sessions (
                session_id TEXT PRIMARY KEY,
                title TEXT,
                model TEXT NOT NULL DEFAULT '',
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
            ",
        )?;
        Ok(())
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
        let now = Utc::now().to_rfc3339();
        {
            let mut statement = transaction.prepare(
                "INSERT INTO messages (session_id, role, content_json, token_count, created_at)
                 VALUES (?1, ?2, ?3, NULL, ?4)",
            )?;
            for (role, content_json) in messages {
                statement.execute(params![session_id, role, content_json, now])?;
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
            "SELECT session_id, title, model, working_dir, created_at, updated_at,
                    message_count, is_archived
             FROM sessions WHERE is_archived = 0
             ORDER BY updated_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as u32], |row| {
            Ok(SessionRecord {
                session_id: row.get(0)?,
                title: row.get(1)?,
                model: row.get(2)?,
                working_dir: row.get(3)?,
                created_at: row.get(4)?,
                updated_at: row.get(5)?,
                message_count: row.get(6)?,
                is_archived: row.get::<_, i32>(7)? != 0,
            })
        })?;
        let mut sessions = Vec::new();
        for row in rows {
            sessions.push(row?);
        }
        Ok(sessions)
    }

    pub fn get_session(&self, session_id: &str) -> anyhow::Result<Option<SessionRecord>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT session_id, title, model, working_dir, created_at, updated_at,
                    message_count, is_archived
             FROM sessions WHERE session_id = ?1",
        )?;
        let mut rows = stmt.query_map(params![session_id], |row| {
            Ok(SessionRecord {
                session_id: row.get(0)?,
                title: row.get(1)?,
                model: row.get(2)?,
                working_dir: row.get(3)?,
                created_at: row.get(4)?,
                updated_at: row.get(5)?,
                message_count: row.get(6)?,
                is_archived: row.get::<_, i32>(7)? != 0,
            })
        })?;
        Ok(rows.next().and_then(|r| r.ok()))
    }

    pub fn archive_session(&self, session_id: &str) -> anyhow::Result<()> {
        self.set_archived(session_id, true)
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
             (session_id, title, model, working_dir, created_at, updated_at, message_count, is_archived, summary)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6, 0, ?7)",
            params![
                target_id,
                title.or(source.title.as_deref()),
                source.model,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_db() -> TranscriptDb {
        TranscriptDb::open(&PathBuf::from(":memory:")).unwrap()
    }

    #[test]
    fn test_create_and_load_session() {
        let db = test_db();
        db.create_session("s1", "claude", ".").unwrap();

        let session = db.get_session("s1").unwrap().unwrap();
        assert_eq!(session.session_id, "s1");
        assert_eq!(session.model, "claude");
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
}
