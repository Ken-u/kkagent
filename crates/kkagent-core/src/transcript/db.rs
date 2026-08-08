use rusqlite::{Connection, params};
use std::path::Path;
use chrono::Utc;

pub struct TranscriptDb {
    conn: Connection,
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

impl TranscriptDb {
    pub fn open(db_path: &Path) -> anyhow::Result<Self> {
        // Skip for in-memory / empty parents (Path(":memory:").parent() == Some(""))
        if let Some(parent) = db_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let conn = Connection::open(db_path)?;
        let db = Self { conn };
        db.migrate()?;
        Ok(db)
    }

    pub fn open_in_memory() -> anyhow::Result<Self> {
        let conn = Connection::open_in_memory()?;
        let db = Self { conn };
        db.migrate()?;
        Ok(db)
    }

    pub fn open_default() -> anyhow::Result<Self> {
        let dir = kkagent_config::default_config_dir();
        let db_path = dir.join("transcripts.db");
        Self::open(&db_path)
    }

    fn migrate(&self) -> anyhow::Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sessions (
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
            "
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
        self.conn.execute(
            "INSERT INTO sessions (session_id, model, working_dir, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![session_id, model, working_dir, now, now],
        )?;
        Ok(())
    }

    pub fn set_title(&self, session_id: &str, title: &str) -> anyhow::Result<()> {
        self.conn.execute(
            "UPDATE sessions SET title = ?1, updated_at = ?2 WHERE session_id = ?3",
            params![title, Utc::now().to_rfc3339(), session_id],
        )?;
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
        self.conn.execute(
            "INSERT INTO messages (session_id, role, content_json, token_count, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![session_id, role, content_json, token_count, now],
        )?;
        let id = self.conn.last_insert_rowid();

        self.conn.execute(
            "UPDATE sessions SET message_count = message_count + 1, updated_at = ?1
             WHERE session_id = ?2",
            params![now, session_id],
        )?;
        Ok(id)
    }

    pub fn load_messages(&self, session_id: &str) -> anyhow::Result<Vec<MessageRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, role, content_json, token_count, created_at
             FROM messages WHERE session_id = ?1 ORDER BY id"
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
        let mut stmt = self.conn.prepare(
            "SELECT session_id, title, model, working_dir, created_at, updated_at,
                    message_count, is_archived
             FROM sessions WHERE is_archived = 0
             ORDER BY updated_at DESC LIMIT ?1"
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
        let mut stmt = self.conn.prepare(
            "SELECT session_id, title, model, working_dir, created_at, updated_at,
                    message_count, is_archived
             FROM sessions WHERE session_id = ?1"
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
        self.conn.execute(
            "UPDATE sessions SET is_archived = 1, updated_at = ?1 WHERE session_id = ?2",
            params![Utc::now().to_rfc3339(), session_id],
        )?;
        Ok(())
    }

    /// Keep the first `keep_count` messages; delete the rest.
    pub fn truncate_messages(&self, session_id: &str, keep_count: usize) -> anyhow::Result<()> {
        let messages = self.load_messages(session_id)?;
        if messages.len() <= keep_count {
            return Ok(());
        }
        let cutoff_id = messages[keep_count].id;
        self.conn.execute(
            "DELETE FROM messages WHERE session_id = ?1 AND id >= ?2",
            params![session_id, cutoff_id],
        )?;
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
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
        let cutoff_id = messages[cutoff].id;

        let deleted = self.conn.execute(
            "DELETE FROM messages WHERE session_id = ?1 AND id < ?2",
            params![session_id, cutoff_id],
        )?;

        // Insert summary as system message at the beginning
        let now = Utc::now().to_rfc3339();
        let summary_json = serde_json::json!([{"type": "text", "text": summary}]).to_string();
        self.conn.execute(
            "INSERT INTO messages (session_id, role, content_json, token_count, created_at)
             VALUES (?1, 'system', ?2, NULL, ?3)",
            params![session_id, summary_json, now],
        )?;

        // Update summary in session
        self.conn.execute(
            "UPDATE sessions SET summary = ?1, message_count = ?2, updated_at = ?3
             WHERE session_id = ?4",
            params![
                summary,
                keep_last_n as u32 + 1, // +1 for summary message
                now,
                session_id,
            ],
        )?;

        Ok(deleted as u32)
    }
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

        db.append_message("s1", "user", r#"[{"type":"text","text":"hello"}]"#, Some(5)).unwrap();
        db.append_message("s1", "assistant", r#"[{"type":"text","text":"hi"}]"#, Some(3)).unwrap();

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
            db.append_message("s1", "user", &format!(r#"[{{"type":"text","text":"msg{}"}}]"#, i), None).unwrap();
        }

        let deleted = db.compact_session("s1", 3, "Summary of first 7 messages").unwrap();
        assert_eq!(deleted, 7);

        let messages = db.load_messages("s1").unwrap();
        assert_eq!(messages.len(), 4); // 3 kept + 1 summary
        // Last 3 original messages are kept, summary is inserted with a new ID
        let has_summary = messages.iter().any(|m| m.role == "system");
        assert!(has_summary, "Should have a summary system message");
    }

    #[test]
    fn test_set_title() {
        let db = test_db();
        db.create_session("s1", "claude", ".").unwrap();
        db.set_title("s1", "My Session").unwrap();

        let session = db.get_session("s1").unwrap().unwrap();
        assert_eq!(session.title.as_deref(), Some("My Session"));
    }
}
