//! Integration smoke for transcript FTS search.

use kkagent_core::transcript::TranscriptDb;

#[test]
fn fts_indexes_appended_messages() {
    let db = TranscriptDb::open_in_memory().expect("open memory db");
    db.create_session("int-1", "model", ".").unwrap();
    db.append_message(
        "int-1",
        "user",
        r#"[{"type":"text","text":"integration-fts-marker-xyz"}]"#,
        None,
    )
    .unwrap();
    let hits = db
        .search_messages("integration-fts-marker", 10, None, None, None, None)
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].session_id, "int-1");
}
