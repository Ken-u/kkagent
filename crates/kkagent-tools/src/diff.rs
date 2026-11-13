//! Building the structured file changes clients render as diffs.
//!
//! Every editing tool reports what it changed through
//! [`kkagent_protocol::FileChange`] instead of leaving clients to parse the
//! tool's prose. Diffs are capped so a runaway edit cannot bloat the event
//! stream or the host's event store; the cap keeps the head of each side and
//! sets `truncated`.

use kkagent_protocol::FileChange;

/// Longest side of a change kept in an event, in bytes (UTF-8 safe).
pub const MAX_CHANGE_BYTES: usize = 32 * 1024;

/// Build a [`FileChange`], trimming both sides to [`MAX_CHANGE_BYTES`].
///
/// `old_text` is `None` when the file did not exist before the call.
pub fn bounded_change(
    path: impl Into<String>,
    old_text: Option<String>,
    new_text: impl Into<String>,
) -> FileChange {
    let new_text = new_text.into();
    let (new_text, new_cut) = truncate_head(&new_text, MAX_CHANGE_BYTES);
    let (old_text, old_cut) = match old_text {
        Some(old) => {
            let (old, cut) = truncate_head(&old, MAX_CHANGE_BYTES);
            (Some(old), cut)
        }
        None => (None, false),
    };
    FileChange {
        path: path.into(),
        old_text,
        new_text,
        truncated: new_cut || old_cut,
    }
}

/// The head of `text`, cut on a character boundary, and whether it was cut.
fn truncate_head(text: &str, limit: usize) -> (String, bool) {
    if text.len() <= limit {
        return (text.to_string(), false);
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_small_changes_intact() {
        let change = bounded_change("a.txt", Some("old".into()), "new");
        assert_eq!(change.path, "a.txt");
        assert_eq!(change.old_text.as_deref(), Some("old"));
        assert_eq!(change.new_text, "new");
        assert!(!change.truncated);
    }

    #[test]
    fn a_created_file_has_no_old_side() {
        let change = bounded_change("new.txt", None, "hello");
        assert_eq!(change.old_text, None);
        assert_eq!(change.new_text, "hello");
    }

    #[test]
    fn cuts_both_sides_and_flags_it() {
        let long = "x".repeat(MAX_CHANGE_BYTES + 10);
        let change = bounded_change("big.txt", Some(long.clone()), long);
        assert!(change.truncated);
        assert_eq!(change.new_text.len(), MAX_CHANGE_BYTES);
        assert_eq!(
            change.old_text.as_deref().map(str::len),
            Some(MAX_CHANGE_BYTES)
        );
    }

    #[test]
    fn cuts_on_a_character_boundary() {
        // 3-byte characters: a byte-length cut would split one without this.
        let text = "あ".repeat(MAX_CHANGE_BYTES);
        let (head, cut) = truncate_head(&text, MAX_CHANGE_BYTES);
        assert!(cut);
        assert!(head.len() <= MAX_CHANGE_BYTES);
        assert!(text.starts_with(&head));
    }

    #[test]
    fn event_serializes_changes_without_empty_noise() {
        let event = kkagent_protocol::AgentEvent::ToolResult {
            session_id: "s".into(),
            tool_call_id: "c".into(),
            tool_name: "Edit".into(),
            output: "Replaced 1 occurrence(s) in a.txt".into(),
            is_error: false,
            changes: vec![bounded_change("a.txt", Some("old".into()), "new")],
        };
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(value["changes"][0]["path"], "a.txt");
        assert_eq!(
            value["changes"][0]["new_text"], "new",
            "event fields stay snake_case like the rest of the HTTP payload"
        );
        // `truncated: false` and a missing old side are omitted, not sent as null.
        assert!(value["changes"][0].get("truncated").is_none());
        let created = serde_json::to_value(kkagent_protocol::AgentEvent::ToolResult {
            session_id: "s".into(),
            tool_call_id: "c".into(),
            tool_name: "Write".into(),
            output: "Wrote 1 lines (5 bytes) to new.txt".into(),
            is_error: false,
            changes: vec![bounded_change("new.txt", None, "hello")],
        })
        .unwrap();
        assert!(created["changes"][0].get("old_text").is_none());
    }

    #[test]
    fn read_only_results_send_no_changes_field() {
        let value = serde_json::to_value(kkagent_protocol::AgentEvent::ToolResult {
            session_id: "s".into(),
            tool_call_id: "c".into(),
            tool_name: "Read".into(),
            output: "body".into(),
            is_error: false,
            changes: Vec::new(),
        })
        .unwrap();
        assert!(value.get("changes").is_none());
    }
}
