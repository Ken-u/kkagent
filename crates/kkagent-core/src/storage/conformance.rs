//! Conformance suite for [`SessionStore`] and [`EventStore`] implementations.
//!
//! Every backend (SQLite, memory, host, and any store a Rust host plugs in)
//! must pass these checks; they encode the contract documented on the traits.
//! The functions panic on violation, so call them from a test.

use std::time::Duration;

use serde_json::json;

use super::{EventStore, NewMessage, SessionPatch, SessionStore, StorageError, ToolResultRecord};

fn message(role: &str, text: &str) -> NewMessage {
    NewMessage {
        role: role.to_string(),
        content_json: json!([{"type": "text", "text": text}]).to_string(),
    }
}

/// Wall-clock-ordered backends need a moment between writes to order them.
async fn tick() {
    tokio::time::sleep(Duration::from_millis(5)).await;
}

/// Run the session-store contract against `store`. `prefix` keeps session ids
/// unique when several runs share one backend.
pub async fn session_store_contract(store: &dyn SessionStore, prefix: &str) {
    let a = format!("{prefix}-a");
    let b = format!("{prefix}-b");
    let c = format!("{prefix}-c");
    let missing = format!("{prefix}-missing");

    // create / get
    store
        .create_session(&a, "model-1", "/work/a")
        .await
        .unwrap();
    assert_eq!(
        store.create_session(&a, "model-1", "/work/a").await,
        Err(StorageError::AlreadyExists(a.clone())),
        "creating an existing session"
    );
    let record = store
        .get_session(&a)
        .await
        .unwrap()
        .expect("created session");
    assert_eq!(record.session_id, a);
    assert_eq!(record.model, "model-1");
    assert_eq!(record.working_dir, "/work/a");
    assert_eq!(record.message_count, 0);
    assert!(!record.is_archived);
    assert!(record.title.is_none());
    assert!(store.get_session(&missing).await.unwrap().is_none());

    // unknown sessions
    let not_found = Err(StorageError::NotFound(missing.clone()));
    assert_eq!(
        store
            .append_messages(&missing, &[message("user", "x")])
            .await,
        not_found
    );
    assert_eq!(store.replace_messages(&missing, &[], None).await, not_found);
    assert_eq!(store.load_messages(&missing).await.map(|_| ()), not_found);
    assert_eq!(
        store
            .update_session(&missing, &SessionPatch::default())
            .await,
        not_found
    );
    assert_eq!(store.delete_session(&missing).await, not_found);
    assert_eq!(
        store.fork_session(&missing, &c, None).await,
        not_found,
        "forking an unknown source"
    );

    // messages keep order and the count stays exact
    store
        .append_messages(&a, &[message("user", "one"), message("assistant", "two")])
        .await
        .unwrap();
    store
        .append_messages(&a, &[message("user", "three")])
        .await
        .unwrap();
    let loaded = store.load_messages(&a).await.unwrap();
    let roles: Vec<&str> = loaded.iter().map(|m| m.role.as_str()).collect();
    assert_eq!(roles, ["user", "assistant", "user"]);
    assert!(loaded.windows(2).all(|pair| pair[0].id < pair[1].id));
    assert!(loaded.iter().all(|m| m.session_id == a));
    assert!(loaded[0].content_json.contains("one"));
    assert_eq!(
        store.get_session(&a).await.unwrap().unwrap().message_count,
        3
    );

    // replace rewrites the whole transcript
    store
        .replace_messages(
            &a,
            &[message("system", "summary"), message("user", "three")],
            Some("summary"),
        )
        .await
        .unwrap();
    let loaded = store.load_messages(&a).await.unwrap();
    assert_eq!(loaded.len(), 2);
    assert_eq!(loaded[0].role, "system");
    assert_eq!(
        store.get_session(&a).await.unwrap().unwrap().message_count,
        2
    );

    // update: each field on its own, untouched fields stay
    store
        .update_session(
            &a,
            &SessionPatch {
                title: Some("Title".into()),
                model: Some("model-2".into()),
                fallback_model: Some(Some("fb".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let record = store.get_session(&a).await.unwrap().unwrap();
    assert_eq!(record.title.as_deref(), Some("Title"));
    assert_eq!(record.model, "model-2");
    assert_eq!(record.fallback_model.as_deref(), Some("fb"));
    store
        .update_session(
            &a,
            &SessionPatch {
                fallback_model: Some(None),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let record = store.get_session(&a).await.unwrap().unwrap();
    assert_eq!(record.fallback_model, None);
    assert_eq!(record.title.as_deref(), Some("Title"), "untouched field");

    // listing: newest write first, archived hidden, limit honored
    tick().await;
    store
        .create_session(&b, "model-1", "/work/b")
        .await
        .unwrap();
    tick().await;
    store
        .append_messages(&b, &[message("user", "hello")])
        .await
        .unwrap();
    let listed: Vec<String> = store
        .list_sessions(100)
        .await
        .unwrap()
        .into_iter()
        .map(|s| s.session_id)
        .filter(|id| id.starts_with(prefix))
        .collect();
    assert_eq!(
        listed,
        [b.clone(), a.clone()],
        "most recently written first"
    );
    assert_eq!(store.list_sessions(1).await.unwrap().len(), 1);
    store
        .update_session(
            &b,
            &SessionPatch {
                archived: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let visible: Vec<String> = store
        .list_sessions(100)
        .await
        .unwrap()
        .into_iter()
        .map(|s| s.session_id)
        .filter(|id| id.starts_with(prefix))
        .collect();
    assert_eq!(
        visible,
        std::slice::from_ref(&a),
        "archived sessions are hidden"
    );
    assert!(store.get_session(&b).await.unwrap().unwrap().is_archived);
    store
        .update_session(
            &b,
            &SessionPatch {
                archived: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    // fork copies a prefix of the transcript and keeps the title
    store.fork_session(&a, &c, Some(1)).await.unwrap();
    let forked = store.get_session(&c).await.unwrap().unwrap();
    assert_eq!(forked.message_count, 1);
    assert_eq!(forked.title.as_deref(), Some("Title"));
    assert_eq!(store.load_messages(&c).await.unwrap().len(), 1);
    assert_eq!(
        store.fork_session(&a, &c, None).await,
        Err(StorageError::AlreadyExists(c.clone())),
        "forking onto an existing id"
    );
    let full = format!("{prefix}-full");
    store.fork_session(&a, &full, None).await.unwrap();
    assert_eq!(store.load_messages(&full).await.unwrap().len(), 2);
    let past_the_end = format!("{prefix}-long");
    store
        .fork_session(&a, &past_the_end, Some(99))
        .await
        .unwrap();
    assert_eq!(
        store.load_messages(&past_the_end).await.unwrap().len(),
        2,
        "a limit past the end copies everything"
    );
    // the source is untouched
    assert_eq!(store.load_messages(&a).await.unwrap().len(), 2);

    // tool results
    store
        .record_tool_result(&ToolResultRecord {
            id: format!("{prefix}-tr"),
            session_id: a.clone(),
            turn_id: Some("turn".into()),
            tool_call_id: "call".into(),
            tool_name: "Bash".into(),
            file_path: "/tmp/out.txt".into(),
            output_size_chars: 10,
            output_size_bytes: 12,
            created_at: 1,
        })
        .await
        .unwrap();

    // delete removes the session and everything under it
    store.delete_session(&a).await.unwrap();
    assert!(store.get_session(&a).await.unwrap().is_none());
    assert_eq!(
        store.load_messages(&a).await.map(|_| ()),
        Err(StorageError::NotFound(a.clone()))
    );
    // the fork is independent of its source
    assert_eq!(store.load_messages(&c).await.unwrap().len(), 1);
}

/// Run the event-store contract against `store`.
pub async fn event_store_contract(store: &dyn EventStore, prefix: &str) {
    let one = format!("{prefix}-e1");
    let two = format!("{prefix}-e2");

    assert_eq!(store.latest_seq(&one).await.unwrap(), 0, "no events yet");
    assert!(store.since(&one, 0, 10).await.unwrap().is_empty());

    let first = store
        .append(&one, &json!({"type": "message_delta", "text": "a"}))
        .await
        .unwrap();
    let second = store
        .append(&one, &json!({"type": "message_delta", "text": "b"}))
        .await
        .unwrap();
    let other = store.append(&two, &json!({"type": "other"})).await.unwrap();
    assert!(second > first, "sequences strictly increase");
    assert!(first > 0);

    let all = store.since(&one, 0, 10).await.unwrap();
    assert_eq!(
        all.iter().map(|e| e.seq).collect::<Vec<_>>(),
        [first, second]
    );
    assert_eq!(all[0].event["text"], "a");
    assert_eq!(all[1].event["text"], "b");

    let tail = store.since(&one, first, 10).await.unwrap();
    assert_eq!(tail.iter().map(|e| e.seq).collect::<Vec<_>>(), [second]);
    assert_eq!(store.since(&one, 0, 1).await.unwrap().len(), 1);
    assert!(store.since(&one, second, 10).await.unwrap().is_empty());

    assert_eq!(store.latest_seq(&one).await.unwrap(), second);
    assert_eq!(store.latest_seq(&two).await.unwrap(), other);
    let isolated = store.since(&two, 0, 10).await.unwrap();
    assert_eq!(isolated.len(), 1, "another session's events stay separate");
    assert_eq!(isolated[0].event["type"], "other");
}
