//! Host-backed storage: an embedding host owns sessions, messages and events
//! and serves them through `_kkagent/store/*` requests.
//!
//! [`HostSessionStore`] / [`HostEventStore`] are the agent side (they send the
//! requests). [`handle_store_request`] is the matching host side: it maps the
//! same wire onto any [`SessionStore`] / [`EventStore`], which is how Rust
//! hosts and the tests implement a host.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use kkagent_protocol::host::{
    store_method, StoreEvent, StoreMessage, StoreNewMessage, StoreSession, StoreSessionPatch,
    StoreToolResult, ERROR_SESSION_EXISTS, ERROR_SESSION_NOT_FOUND,
};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{
    EventStore, MessageRecord, NewMessage, SessionPatch, SessionRecord, SessionStore, StorageError,
    StorageResult, StoredEvent, ToolResultRecord,
};
use crate::host_channel::{HostChannel, HostChannelError, RequestOptions};

/// How long a storage request may take before the store reports itself
/// unavailable. A stalled host must fail the turn, not hang it.
const STORE_TIMEOUT: Duration = Duration::from_secs(60);

fn map_channel_error(error: HostChannelError, session_id: &str) -> StorageError {
    match error {
        HostChannelError::Rpc { code, .. } if code == ERROR_SESSION_NOT_FOUND => {
            StorageError::NotFound(session_id.to_string())
        }
        HostChannelError::Rpc { code, .. } if code == ERROR_SESSION_EXISTS => {
            StorageError::AlreadyExists(session_id.to_string())
        }
        HostChannelError::Rpc { code, message } => {
            StorageError::Backend(format!("host error {code}: {message}"))
        }
        other => StorageError::Unavailable(other.to_string()),
    }
}

async fn call<T: DeserializeOwned>(
    channel: &Arc<dyn HostChannel>,
    method: &str,
    params: Value,
    session_id: &str,
) -> StorageResult<T> {
    let value = channel
        .request(method, params, RequestOptions::with_timeout(STORE_TIMEOUT))
        .await
        .map_err(|error| map_channel_error(error, session_id))?;
    serde_json::from_value(value).map_err(|error| {
        StorageError::Backend(format!(
            "host returned an invalid `{method}` response: {error}"
        ))
    })
}

fn session_from_wire(session: StoreSession) -> SessionRecord {
    SessionRecord {
        session_id: session.session_id,
        title: session.title,
        model: session.model,
        fallback_model: session.fallback_model,
        working_dir: session.working_dir,
        created_at: session.created_at,
        updated_at: session.updated_at,
        message_count: session.message_count,
        is_archived: session.is_archived,
        parent_session_id: session.parent_session_id,
    }
}

fn session_to_wire(session: SessionRecord) -> StoreSession {
    StoreSession {
        session_id: session.session_id,
        title: session.title,
        model: session.model,
        fallback_model: session.fallback_model,
        working_dir: session.working_dir,
        created_at: session.created_at,
        updated_at: session.updated_at,
        message_count: session.message_count,
        is_archived: session.is_archived,
        parent_session_id: session.parent_session_id,
    }
}

fn message_from_wire(session_id: &str, message: StoreMessage) -> MessageRecord {
    MessageRecord {
        id: message.id,
        session_id: session_id.to_string(),
        role: message.role,
        content_json: message.content_json,
        token_count: message.token_count,
        created_at: message.created_at,
    }
}

fn message_to_wire(message: MessageRecord) -> StoreMessage {
    StoreMessage {
        id: message.id,
        role: message.role,
        content_json: message.content_json,
        token_count: message.token_count,
        created_at: message.created_at,
    }
}

fn tool_result_to_wire(record: &ToolResultRecord) -> StoreToolResult {
    StoreToolResult {
        id: record.id.clone(),
        session_id: record.session_id.clone(),
        turn_id: record.turn_id.clone(),
        tool_call_id: record.tool_call_id.clone(),
        tool_name: record.tool_name.clone(),
        file_path: record.file_path.clone(),
        output_size_chars: record.output_size_chars,
        output_size_bytes: record.output_size_bytes,
        created_at: record.created_at,
    }
}

fn tool_result_from_wire(record: StoreToolResult) -> ToolResultRecord {
    ToolResultRecord {
        id: record.id,
        session_id: record.session_id,
        turn_id: record.turn_id,
        tool_call_id: record.tool_call_id,
        tool_name: record.tool_name,
        file_path: record.file_path,
        output_size_chars: record.output_size_chars,
        output_size_bytes: record.output_size_bytes,
        created_at: record.created_at,
    }
}

/// [`SessionStore`] served by the embedding host.
#[derive(Clone)]
pub struct HostSessionStore {
    channel: Arc<dyn HostChannel>,
}

impl HostSessionStore {
    pub fn new(channel: Arc<dyn HostChannel>) -> Self {
        Self { channel }
    }
}

#[derive(Deserialize)]
struct SessionResponse {
    session: Option<StoreSession>,
}

#[derive(Deserialize)]
struct SessionsResponse {
    sessions: Vec<StoreSession>,
}

#[derive(Deserialize)]
struct MessagesResponse {
    messages: Vec<StoreMessage>,
}

#[async_trait]
impl SessionStore for HostSessionStore {
    fn backend(&self) -> &'static str {
        "host"
    }

    async fn create_session(
        &self,
        session_id: &str,
        model: &str,
        working_dir: &str,
    ) -> StorageResult<()> {
        call::<Value>(
            &self.channel,
            store_method::SESSION_CREATE,
            json!({"sessionId": session_id, "model": model, "workingDir": working_dir}),
            session_id,
        )
        .await
        .map(|_| ())
    }

    async fn get_session(&self, session_id: &str) -> StorageResult<Option<SessionRecord>> {
        let response: SessionResponse = call(
            &self.channel,
            store_method::SESSION_GET,
            json!({"sessionId": session_id}),
            session_id,
        )
        .await?;
        Ok(response.session.map(session_from_wire))
    }

    async fn list_sessions(&self, limit: usize) -> StorageResult<Vec<SessionRecord>> {
        let response: SessionsResponse = call(
            &self.channel,
            store_method::SESSION_LIST,
            json!({"limit": limit}),
            "",
        )
        .await?;
        Ok(response
            .sessions
            .into_iter()
            .map(session_from_wire)
            .collect())
    }

    async fn update_session(&self, session_id: &str, patch: &SessionPatch) -> StorageResult<()> {
        call::<Value>(
            &self.channel,
            store_method::SESSION_UPDATE,
            json!({"sessionId": session_id, "patch": patch}),
            session_id,
        )
        .await
        .map(|_| ())
    }

    async fn delete_session(&self, session_id: &str) -> StorageResult<()> {
        call::<Value>(
            &self.channel,
            store_method::SESSION_DELETE,
            json!({"sessionId": session_id}),
            session_id,
        )
        .await
        .map(|_| ())
    }

    async fn append_messages(
        &self,
        session_id: &str,
        messages: &[NewMessage],
    ) -> StorageResult<()> {
        call::<Value>(
            &self.channel,
            store_method::MESSAGES_APPEND,
            json!({"sessionId": session_id, "messages": messages}),
            session_id,
        )
        .await
        .map(|_| ())
    }

    async fn replace_messages(
        &self,
        session_id: &str,
        messages: &[NewMessage],
        summary: Option<&str>,
    ) -> StorageResult<()> {
        call::<Value>(
            &self.channel,
            store_method::MESSAGES_REPLACE,
            json!({"sessionId": session_id, "messages": messages, "summary": summary}),
            session_id,
        )
        .await
        .map(|_| ())
    }

    async fn load_messages(&self, session_id: &str) -> StorageResult<Vec<MessageRecord>> {
        let response: MessagesResponse = call(
            &self.channel,
            store_method::MESSAGES_LOAD,
            json!({"sessionId": session_id}),
            session_id,
        )
        .await?;
        Ok(response
            .messages
            .into_iter()
            .map(|message| message_from_wire(session_id, message))
            .collect())
    }

    async fn record_tool_result(&self, record: &ToolResultRecord) -> StorageResult<()> {
        call::<Value>(
            &self.channel,
            store_method::TOOL_RESULT_RECORD,
            json!({"sessionId": record.session_id, "record": tool_result_to_wire(record)}),
            &record.session_id,
        )
        .await
        .map(|_| ())
    }

    async fn fork_session(
        &self,
        source: &str,
        new_id: &str,
        message_limit: Option<usize>,
    ) -> StorageResult<()> {
        let params = json!({
            "sourceSessionId": source,
            "sessionId": new_id,
            "messageLimit": message_limit,
        });
        match self
            .channel
            .request(
                store_method::SESSION_FORK,
                params,
                RequestOptions::with_timeout(STORE_TIMEOUT),
            )
            .await
        {
            Ok(_) => Ok(()),
            // "Exists" concerns the target, "not found" the source.
            Err(HostChannelError::Rpc { code, .. }) if code == ERROR_SESSION_EXISTS => {
                Err(StorageError::AlreadyExists(new_id.to_string()))
            }
            Err(error) => Err(map_channel_error(error, source)),
        }
    }
}

/// [`EventStore`] served by the embedding host.
#[derive(Clone)]
pub struct HostEventStore {
    channel: Arc<dyn HostChannel>,
}

impl HostEventStore {
    pub fn new(channel: Arc<dyn HostChannel>) -> Self {
        Self { channel }
    }
}

#[derive(Deserialize)]
struct SeqResponse {
    seq: u64,
}

#[derive(Deserialize)]
struct EventsResponse {
    events: Vec<StoreEvent>,
}

#[async_trait]
impl EventStore for HostEventStore {
    async fn append(&self, session_id: &str, event: &Value) -> StorageResult<u64> {
        let response: SeqResponse = call(
            &self.channel,
            store_method::EVENTS_APPEND,
            json!({"sessionId": session_id, "event": event}),
            session_id,
        )
        .await?;
        Ok(response.seq)
    }

    async fn since(
        &self,
        session_id: &str,
        after_seq: u64,
        limit: usize,
    ) -> StorageResult<Vec<StoredEvent>> {
        let response: EventsResponse = call(
            &self.channel,
            store_method::EVENTS_SINCE,
            json!({"sessionId": session_id, "afterSeq": after_seq, "limit": limit}),
            session_id,
        )
        .await?;
        Ok(response
            .events
            .into_iter()
            .map(|event| StoredEvent {
                seq: event.seq,
                event: event.event,
            })
            .collect())
    }

    async fn latest_seq(&self, session_id: &str) -> StorageResult<u64> {
        let response: SeqResponse = call(
            &self.channel,
            store_method::EVENTS_LATEST_SEQ,
            json!({"sessionId": session_id}),
            session_id,
        )
        .await?;
        Ok(response.seq)
    }
}

/// A JSON-RPC error produced while serving a store request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreRpcError {
    pub code: i64,
    pub message: String,
}

impl From<StorageError> for StoreRpcError {
    fn from(error: StorageError) -> Self {
        let code = match &error {
            StorageError::NotFound(_) => ERROR_SESSION_NOT_FOUND,
            StorageError::AlreadyExists(_) => ERROR_SESSION_EXISTS,
            _ => -32000,
        };
        Self {
            code,
            message: error.to_string(),
        }
    }
}

fn invalid(message: impl Into<String>) -> StoreRpcError {
    StoreRpcError {
        code: -32602,
        message: message.into(),
    }
}

fn field<T: DeserializeOwned>(params: &Value, name: &str) -> Result<T, StoreRpcError> {
    let value = params
        .get(name)
        .ok_or_else(|| invalid(format!("`{name}` is required")))?;
    serde_json::from_value(value.clone()).map_err(|error| invalid(format!("`{name}`: {error}")))
}

fn optional_field<T: DeserializeOwned>(
    params: &Value,
    name: &str,
) -> Result<Option<T>, StoreRpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => serde_json::from_value(value.clone())
            .map(Some)
            .map_err(|error| invalid(format!("`{name}`: {error}"))),
    }
}

/// Serve one `_kkagent/store/*` request against local stores: the host side
/// of the wire used by [`HostSessionStore`] and [`HostEventStore`]. Returns
/// `None` when `method` is not a store method.
pub async fn handle_store_request(
    sessions: &dyn SessionStore,
    events: &dyn EventStore,
    method: &str,
    params: &Value,
) -> Option<Result<Value, StoreRpcError>> {
    if !method.starts_with(kkagent_protocol::host::METHOD_STORE_PREFIX) {
        return None;
    }
    Some(dispatch(sessions, events, method, params).await)
}

async fn dispatch(
    sessions: &dyn SessionStore,
    events: &dyn EventStore,
    method: &str,
    params: &Value,
) -> Result<Value, StoreRpcError> {
    match method {
        store_method::SESSION_CREATE => {
            let session_id: String = field(params, "sessionId")?;
            let model: String = field(params, "model")?;
            let working_dir: String = field(params, "workingDir")?;
            sessions
                .create_session(&session_id, &model, &working_dir)
                .await?;
            Ok(json!({}))
        }
        store_method::SESSION_GET => {
            let session_id: String = field(params, "sessionId")?;
            let session = sessions.get_session(&session_id).await?;
            Ok(json!({"session": session.map(session_to_wire)}))
        }
        store_method::SESSION_LIST => {
            let limit: usize = optional_field(params, "limit")?.unwrap_or(100);
            let list = sessions.list_sessions(limit).await?;
            let wire: Vec<StoreSession> = list.into_iter().map(session_to_wire).collect();
            Ok(json!({"sessions": wire}))
        }
        store_method::SESSION_UPDATE => {
            let session_id: String = field(params, "sessionId")?;
            let patch: StoreSessionPatch = field(params, "patch")?;
            sessions.update_session(&session_id, &patch).await?;
            Ok(json!({}))
        }
        store_method::SESSION_DELETE => {
            let session_id: String = field(params, "sessionId")?;
            sessions.delete_session(&session_id).await?;
            Ok(json!({}))
        }
        store_method::SESSION_FORK => {
            let source: String = field(params, "sourceSessionId")?;
            let new_id: String = field(params, "sessionId")?;
            let limit: Option<usize> = optional_field(params, "messageLimit")?;
            sessions.fork_session(&source, &new_id, limit).await?;
            Ok(json!({}))
        }
        store_method::MESSAGES_APPEND => {
            let session_id: String = field(params, "sessionId")?;
            let messages: Vec<StoreNewMessage> = field(params, "messages")?;
            sessions.append_messages(&session_id, &messages).await?;
            Ok(json!({}))
        }
        store_method::MESSAGES_REPLACE => {
            let session_id: String = field(params, "sessionId")?;
            let messages: Vec<StoreNewMessage> = field(params, "messages")?;
            let summary: Option<String> = optional_field(params, "summary")?;
            sessions
                .replace_messages(&session_id, &messages, summary.as_deref())
                .await?;
            Ok(json!({}))
        }
        store_method::MESSAGES_LOAD => {
            let session_id: String = field(params, "sessionId")?;
            let messages = sessions.load_messages(&session_id).await?;
            let wire: Vec<StoreMessage> = messages.into_iter().map(message_to_wire).collect();
            Ok(json!({"messages": wire}))
        }
        store_method::TOOL_RESULT_RECORD => {
            let record: StoreToolResult = field(params, "record")?;
            sessions
                .record_tool_result(&tool_result_from_wire(record))
                .await?;
            Ok(json!({}))
        }
        store_method::EVENTS_APPEND => {
            let session_id: String = field(params, "sessionId")?;
            let event: Value = field(params, "event")?;
            let seq = events.append(&session_id, &event).await?;
            Ok(json!({"seq": seq}))
        }
        store_method::EVENTS_SINCE => {
            let session_id: String = field(params, "sessionId")?;
            let after_seq: u64 = optional_field(params, "afterSeq")?.unwrap_or(0);
            let limit: usize = optional_field(params, "limit")?.unwrap_or(1000);
            let found = events.since(&session_id, after_seq, limit).await?;
            let wire: Vec<StoreEvent> = found
                .into_iter()
                .map(|event| StoreEvent {
                    seq: event.seq,
                    event: event.event,
                })
                .collect();
            Ok(json!({"events": wire}))
        }
        store_method::EVENTS_LATEST_SEQ => {
            let session_id: String = field(params, "sessionId")?;
            let seq = events.latest_seq(&session_id).await?;
            Ok(json!({"seq": seq}))
        }
        other => Err(StoreRpcError {
            code: -32601,
            message: format!("Method not found: {other}"),
        }),
    }
}
