pub mod db;

pub use db::{
    open_shared_sqlite, open_shared_sqlite_memory, IntegrityReport, IsolatedMessage, MessageRecord,
    SearchHit, SessionRecord, SharedSqlite, ToolResultRecord, TranscriptDb,
};

use kkagent_llm::{ChatContent, ChatMessage};

/// Persist any not-yet-written messages of `session` into the transcript DB.
///
/// Called at complete-message boundaries inside a turn (never per stream
/// chunk): if a rewrite is required the whole transcript is replaced,
/// otherwise only the tail after `persisted_message_count` is appended.
/// Auto-title from the first real user text is included so per-step callers
/// keep parity with the turn-end persistence path.
pub fn persist_session_delta(
    db: &TranscriptDb,
    session: &mut crate::session::Session,
) -> anyhow::Result<()> {
    if session.transcript_rewrite_required {
        let replacement = serialize_transcript_messages(&session.messages)?;
        db.replace_messages(&session.id, &replacement, None)?;
        session.persisted_message_count = session.messages.len();
        session.transcript_rewrite_required = false;
    }

    if session.persisted_message_count < session.messages.len() {
        let pending = &session.messages[session.persisted_message_count..];
        let serialized = serialize_transcript_messages(pending)?;
        db.append_messages(&session.id, &serialized)?;
        session.persisted_message_count = session.messages.len();
    }

    // Auto-title from first real user text (skip harness-only injections).
    // Same 200-char truncation as the disk-store label.
    if session.title.is_none() {
        if let Some(text) =
            kkagent_protocol::first_real_user_text(session.messages.iter().filter_map(|m| {
                if m.role != "user" {
                    return None;
                }
                m.content.iter().find_map(|c| match c {
                    ChatContent::Text { text } => Some(text.as_str()),
                    _ => None,
                })
            }))
        {
            let title: String = text.chars().take(200).collect();
            db.set_title(&session.id, &title)?;
            session.title = Some(title);
        }
    }
    Ok(())
}

fn serialize_transcript_messages(
    messages: &[ChatMessage],
) -> anyhow::Result<Vec<(String, String)>> {
    messages
        .iter()
        .map(|message| {
            Ok((
                message.role.clone(),
                serde_json::to_string(&message.content)?,
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Session;
    use kkagent_protocol::PermissionMode;
    use std::path::PathBuf;

    fn test_db() -> TranscriptDb {
        TranscriptDb::open(&PathBuf::from(":memory:")).unwrap()
    }

    fn make_session() -> Session {
        let workspace =
            std::env::temp_dir().join(format!("kkagent-delta-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).unwrap();
        Session::new(
            "delta-test".into(),
            workspace,
            PermissionMode::Auto,
            "test-model".into(),
        )
    }

    fn text_msg(role: &str, text: &str) -> ChatMessage {
        ChatMessage {
            role: role.into(),
            content: vec![ChatContent::Text { text: text.into() }],
        }
    }

    /// Append path: messages after `persisted_message_count` are written
    /// incrementally, matching the per-step `persist_step` call pattern.
    #[test]
    fn append_path_writes_incremental_messages() {
        let db = test_db();
        db.create_session("delta-test", "test-model", ".").unwrap();
        let mut session = make_session();

        session.messages.push(text_msg("user", "hello"));
        persist_session_delta(&db, &mut session).unwrap();

        let records = db.load_messages("delta-test").unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].role, "user");
        assert_eq!(session.persisted_message_count, 1);

        // Second message appended without rewriting the first.
        session.messages.push(text_msg("assistant", "hi there"));
        persist_session_delta(&db, &mut session).unwrap();

        let records = db.load_messages("delta-test").unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[1].role, "assistant");
        assert!(records[1].content_json.contains("hi there"));
        assert_eq!(session.persisted_message_count, 2);
    }

    /// Rewrite path: when `transcript_rewrite_required` is set (e.g. after
    /// compaction), the entire transcript is atomically replaced.
    #[test]
    fn rewrite_path_replaces_entire_transcript() {
        let db = test_db();
        db.create_session("delta-test", "test-model", ".").unwrap();
        let mut session = make_session();

        // Seed with 4 messages, persisted.
        for i in 0..4 {
            session.messages.push(text_msg("user", &format!("msg{i}")));
        }
        persist_session_delta(&db, &mut session).unwrap();
        assert_eq!(db.load_messages("delta-test").unwrap().len(), 4);

        // Compact: keep only 1 message + 1 summary, flag for rewrite.
        session.messages.truncate(1);
        session.messages.insert(0, text_msg("system", "summary"));
        session.transcript_rewrite_required = true;

        persist_session_delta(&db, &mut session).unwrap();

        let records = db.load_messages("delta-test").unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].role, "system");
        assert!(records[0].content_json.contains("summary"));
        assert!(!session.transcript_rewrite_required);
        assert_eq!(session.persisted_message_count, 2);
    }

    /// Auto-title: first real user text (skipping harness-only injections)
    /// populates the session title with 200-char truncation.
    #[test]
    fn auto_title_from_first_real_user_text() {
        let db = test_db();
        db.create_session("delta-test", "test-model", ".").unwrap();
        let mut session = make_session();
        assert!(session.title.is_none());

        // Harness-only text should NOT become the title.
        session.messages.push(text_msg(
            "user",
            "<system-reminder>\nsome context\n</system-reminder>",
        ));
        persist_session_delta(&db, &mut session).unwrap();
        assert!(session.title.is_none());

        // First real user text becomes the title.
        session.messages.push(text_msg("user", "what is 2+2?"));
        persist_session_delta(&db, &mut session).unwrap();
        assert_eq!(session.title.as_deref(), Some("what is 2+2?"));

        // Title is not overwritten on subsequent calls.
        session.messages.push(text_msg("assistant", "4"));
        persist_session_delta(&db, &mut session).unwrap();
        assert_eq!(session.title.as_deref(), Some("what is 2+2?"));

        // The DB also has the title set.
        let db_session = db.get_session("delta-test").unwrap().unwrap();
        assert_eq!(db_session.title.as_deref(), Some("what is 2+2?"));
    }

    /// Idempotency: calling persist_session_delta when nothing changed is a
    /// no-op — no duplicate rows, counters stay in sync.
    #[test]
    fn idempotent_when_nothing_changed() {
        let db = test_db();
        db.create_session("delta-test", "test-model", ".").unwrap();
        let mut session = make_session();

        session.messages.push(text_msg("user", "hello"));
        persist_session_delta(&db, &mut session).unwrap();
        assert_eq!(db.load_messages("delta-test").unwrap().len(), 1);

        // Call again — no new messages, should be a no-op.
        persist_session_delta(&db, &mut session).unwrap();
        let records = db.load_messages("delta-test").unwrap();
        assert_eq!(records.len(), 1);
    }

    /// 200-char truncation: long titles are cut to exactly 200 chars.
    #[test]
    fn auto_title_truncated_to_200_chars() {
        let db = test_db();
        db.create_session("delta-test", "test-model", ".").unwrap();
        let mut session = make_session();

        let long_text = "x".repeat(500);
        session.messages.push(text_msg("user", &long_text));
        persist_session_delta(&db, &mut session).unwrap();

        assert_eq!(session.title.as_ref().unwrap().chars().count(), 200);
    }
}
