//! Runs the conformance suite against every shipped backend.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{json, Value};

use super::conformance::{event_store_contract, session_store_contract};
use super::*;
use crate::host_channel::{HostChannel, HostChannelError, HostChannelSlot, RequestOptions};
use crate::transcript::TranscriptDb;

#[tokio::test]
async fn memory_store_satisfies_the_contracts() {
    let store = MemoryStore::new();
    session_store_contract(&store, "mem").await;
    event_store_contract(&store, "mem").await;
}

#[tokio::test]
async fn sqlite_store_satisfies_the_session_contract() {
    let store = SqliteSessionStore::new(TranscriptDb::open_in_memory().unwrap());
    assert_eq!(store.backend(), "sqlite");
    session_store_contract(&store, "sql").await;
}

/// A host that keeps its data in a [`MemoryStore`] and answers the
/// `_kkagent/store/*` wire, recording which methods were called.
struct LoopbackHost {
    data: MemoryStore,
    methods: Mutex<Vec<String>>,
}

impl LoopbackHost {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            data: MemoryStore::new(),
            methods: Mutex::new(Vec::new()),
        })
    }

    fn methods(&self) -> Vec<String> {
        self.methods.lock().unwrap().clone()
    }
}

#[async_trait]
impl HostChannel for LoopbackHost {
    async fn request(
        &self,
        method: &str,
        params: Value,
        _options: RequestOptions,
    ) -> Result<Value, HostChannelError> {
        self.methods.lock().unwrap().push(method.to_string());
        match handle_store_request(&self.data, &self.data, method, &params).await {
            Some(Ok(value)) => Ok(value),
            Some(Err(error)) => Err(HostChannelError::Rpc {
                code: error.code,
                message: error.message,
            }),
            None => Err(HostChannelError::Rpc {
                code: -32601,
                message: format!("Method not found: {method}"),
            }),
        }
    }

    async fn notify(&self, _method: &str, _params: Value) {}
}

#[tokio::test]
async fn host_stores_satisfy_the_contracts_over_the_wire() {
    let host = LoopbackHost::new();
    let sessions = HostSessionStore::new(host.clone());
    let events = HostEventStore::new(host.clone());
    assert_eq!(sessions.backend(), "host");

    session_store_contract(&sessions, "host").await;
    event_store_contract(&events, "host").await;

    // Everything went through the host, nothing was kept locally.
    let methods = host.methods();
    for expected in [
        "_kkagent/store/session/create",
        "_kkagent/store/session/get",
        "_kkagent/store/session/list",
        "_kkagent/store/session/update",
        "_kkagent/store/session/delete",
        "_kkagent/store/session/fork",
        "_kkagent/store/messages/append",
        "_kkagent/store/messages/replace",
        "_kkagent/store/messages/load",
        "_kkagent/store/toolResult/record",
        "_kkagent/store/events/append",
        "_kkagent/store/events/since",
        "_kkagent/store/events/latestSeq",
    ] {
        assert!(methods.iter().any(|m| m == expected), "{expected} not used");
    }
    assert_eq!(
        host.data.tool_results().len(),
        0,
        "deleted with its session"
    );
}

#[tokio::test]
async fn host_store_fails_closed_without_a_connected_host() {
    let slot = Arc::new(HostChannelSlot::new());
    let sessions = HostSessionStore::new(slot.clone());
    let events = HostEventStore::new(slot.clone());

    let error = sessions
        .create_session("s", "m", "/w")
        .await
        .expect_err("no host, no write");
    assert!(matches!(error, StorageError::Unavailable(_)), "{error:?}");
    let error = sessions.get_session("s").await.expect_err("no silent miss");
    assert!(matches!(error, StorageError::Unavailable(_)), "{error:?}");
    let error = events.append("s", &json!({})).await.unwrap_err();
    assert!(matches!(error, StorageError::Unavailable(_)), "{error:?}");

    // Once the host is bound the same store works.
    slot.bind(LoopbackHost::new());
    sessions.create_session("s", "m", "/w").await.unwrap();
    assert!(sessions.get_session("s").await.unwrap().is_some());
}

#[tokio::test]
async fn host_errors_and_bad_responses_are_typed() {
    struct Scripted(Mutex<Vec<Result<Value, HostChannelError>>>);

    #[async_trait]
    impl HostChannel for Scripted {
        async fn request(
            &self,
            _method: &str,
            _params: Value,
            _options: RequestOptions,
        ) -> Result<Value, HostChannelError> {
            self.0.lock().unwrap().remove(0)
        }
        async fn notify(&self, _method: &str, _params: Value) {}
    }

    let channel = Arc::new(Scripted(Mutex::new(vec![
        Err(HostChannelError::Rpc {
            code: -32002,
            message: "gone".into(),
        }),
        Err(HostChannelError::Rpc {
            code: -32003,
            message: "dup".into(),
        }),
        Err(HostChannelError::Rpc {
            code: -32000,
            message: "disk full".into(),
        }),
        Err(HostChannelError::TimedOut),
        Ok(json!({"messages": "not a list"})),
    ])));
    let sessions = HostSessionStore::new(channel);

    assert_eq!(
        sessions.load_messages("s1").await.map(|_| ()),
        Err(StorageError::NotFound("s1".into()))
    );
    assert_eq!(
        sessions.create_session("s2", "m", "/w").await,
        Err(StorageError::AlreadyExists("s2".into()))
    );
    let backend = sessions.delete_session("s3").await.unwrap_err();
    assert!(
        matches!(&backend, StorageError::Backend(message) if message.contains("disk full")),
        "{backend:?}"
    );
    let timeout = sessions.delete_session("s4").await.unwrap_err();
    assert!(
        matches!(timeout, StorageError::Unavailable(_)),
        "{timeout:?}"
    );
    let malformed = sessions.load_messages("s5").await.unwrap_err();
    assert!(
        matches!(&malformed, StorageError::Backend(message) if message.contains("invalid")),
        "{malformed:?}"
    );
}

#[tokio::test]
async fn store_request_dispatcher_rejects_unknown_and_malformed_requests() {
    let store = MemoryStore::new();
    assert!(
        handle_store_request(&store, &store, "session/new", &json!({}))
            .await
            .is_none()
    );
    let unknown = handle_store_request(&store, &store, "_kkagent/store/nope", &json!({}))
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(unknown.code, -32601);
    let malformed = handle_store_request(&store, &store, "_kkagent/store/session/get", &json!({}))
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(malformed.code, -32602);
    let missing = handle_store_request(
        &store,
        &store,
        "_kkagent/store/messages/load",
        &json!({"sessionId": "nope"}),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert_eq!(missing.code, -32002);
}
