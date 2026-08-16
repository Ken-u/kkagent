//! Visual debug timeline export shape.

use kkagent_core::transcript::TranscriptDb;

#[test]
fn timeline_messages_are_ordered() {
    let db = TranscriptDb::open_in_memory().unwrap();
    db.create_session("vis-1", "m", ".").unwrap();
    db.append_message("vis-1", "user", r#"[{"type":"text","text":"one"}]"#, None)
        .unwrap();
    db.append_message(
        "vis-1",
        "assistant",
        r#"[{"type":"text","text":"two"}]"#,
        None,
    )
    .unwrap();
    let messages = db.load_messages("vis-1").unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].role, "user");
    assert_eq!(messages[1].role, "assistant");
}
