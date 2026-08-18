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
