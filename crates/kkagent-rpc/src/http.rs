//! REST API v1 + WebSocket (kap-server route matrix subset) with AgentLoop backend hooks.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex};

/// Pluggable backend so HTTP can bind to the live AgentLoop/ServerState.
#[async_trait::async_trait]
pub trait HttpBackend: Send + Sync {
    async fn list_sessions(&self) -> Value;
    async fn create_session(&self, workspace: Option<String>, title: Option<String>) -> Value;
    async fn get_session(&self, id: &str) -> Option<Value>;
    async fn post_message(&self, id: &str, text: &str) -> Result<Value, String>;
    async fn list_tools(&self) -> Value;
    async fn list_tasks(&self) -> Value;
    async fn list_skills(&self) -> Value;
    async fn list_models(&self) -> Value;
    async fn get_config(&self) -> Value;
    async fn approve(
        &self,
        id: &str,
        decision: &str,
        feedback: Option<String>,
    ) -> Result<Value, String>;
    async fn fs_read(&self, path: &str) -> Result<String, String>;
    async fn fs_write(&self, path: &str, content: &str) -> Result<(), String>;
    async fn search(&self, query: &str) -> Value;
    async fn workspace_info(&self) -> Value;
}

/// In-memory demo backend (used when not wired to AgentLoop).
#[derive(Default)]
pub struct MemoryBackend {
    sessions: Mutex<HashMap<String, Value>>,
}

#[async_trait::async_trait]
impl HttpBackend for MemoryBackend {
    async fn list_sessions(&self) -> Value {
        let map = self.sessions.lock().await;
        json!({"sessions": map.values().cloned().collect::<Vec<_>>()})
    }
    async fn create_session(&self, workspace: Option<String>, title: Option<String>) -> Value {
        let id = uuid::Uuid::new_v4().to_string();
        let sess = json!({
            "session_id": id,
            "workspace": workspace.unwrap_or_else(|| ".".into()),
            "title": title,
            "created_at": chrono::Utc::now().to_rfc3339(),
            "messages": [],
        });
        self.sessions.lock().await.insert(id, sess.clone());
        sess
    }
    async fn get_session(&self, id: &str) -> Option<Value> {
        self.sessions.lock().await.get(id).cloned()
    }
    async fn post_message(&self, id: &str, text: &str) -> Result<Value, String> {
        let mut map = self.sessions.lock().await;
        let sess = map.get_mut(id).ok_or_else(|| "not found".to_string())?;
        let msg = json!({"role": "user", "text": text, "at": chrono::Utc::now().to_rfc3339()});
        if let Some(arr) = sess.get_mut("messages").and_then(|v| v.as_array_mut()) {
            arr.push(msg.clone());
        }
        Ok(
            json!({"ok": true, "message": msg, "note": "memory backend — wire HttpBackend for AgentLoop"}),
        )
    }
    async fn list_tools(&self) -> Value {
        json!({"tools": [
            "Read","Write","Edit","Bash","Grep","Glob","TodoList","WebSearch","FetchURL",
            "Task","AskUserQuestion","SelectTools","Skill","CronCreate","ReadMediaFile"
        ]})
    }
    async fn list_tasks(&self) -> Value {
        json!({"tasks": []})
    }
    async fn list_skills(&self) -> Value {
        json!({"skills": []})
    }
    async fn list_models(&self) -> Value {
        json!({"models": kkagent_llm_catalog_stub()})
    }
    async fn get_config(&self) -> Value {
        json!({"config_dir": dirs_home(), "api": "v1"})
    }
    async fn approve(
        &self,
        id: &str,
        decision: &str,
        feedback: Option<String>,
    ) -> Result<Value, String> {
        Ok(json!({"ok": true, "approval_id": id, "decision": decision, "feedback": feedback}))
    }
    async fn fs_read(&self, path: &str) -> Result<String, String> {
        std::fs::read_to_string(path).map_err(|e| e.to_string())
    }
    async fn fs_write(&self, path: &str, content: &str) -> Result<(), String> {
        if let Some(parent) = std::path::Path::new(path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(path, content).map_err(|e| e.to_string())
    }
    async fn search(&self, query: &str) -> Value {
        json!({"query": query, "hits": []})
    }
    async fn workspace_info(&self) -> Value {
        json!({
            "cwd": std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_else(|_| ".".into()),
            "trusted": true,
        })
    }
}

fn dirs_home() -> String {
    dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".kkagent")
        .display()
        .to_string()
}

fn kkagent_llm_catalog_stub() -> Vec<Value> {
    // Avoid hard dep cycle: inline common catalog ids.
    [
        "gpt-4.1",
        "o4-mini",
        "claude-sonnet-4-20250514",
        "gemini-2.5-pro",
    ]
    .iter()
    .map(|id| json!({"id": id}))
    .collect()
}

#[derive(Clone)]
pub struct HttpState {
    pub backend: Arc<dyn HttpBackend>,
    pub meta: Value,
    pub events: broadcast::Sender<Value>,
    pub token: Option<String>,
    pub terminals: Arc<Mutex<HashMap<String, Value>>>,
}

impl HttpState {
    pub fn new(token: Option<String>) -> Self {
        Self::with_backend(Arc::new(MemoryBackend::default()), token)
    }

    pub fn with_backend(backend: Arc<dyn HttpBackend>, token: Option<String>) -> Self {
        let (events, _) = broadcast::channel(512);
        Self {
            backend,
            meta: json!({
                "name": "kkagent",
                "version": env!("CARGO_PKG_VERSION"),
                "api": ["v1"],
                "capabilities": [
                    "sessions","messages","approvals","ws","tools","tasks","skills",
                    "files","fs","workspaces","config","modelCatalog","search",
                    "terminals","questions","prompts","snapshot"
                ],
            }),
            events,
            token,
            terminals: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn publish(&self, event: Value) {
        let _ = self.events.send(event);
    }
}

#[derive(Deserialize, Default)]
pub struct AuthQuery {
    pub token: Option<String>,
}

fn check_auth(state: &HttpState, q: &AuthQuery) -> Result<(), StatusCode> {
    match &state.token {
        None => Ok(()),
        Some(expected) => {
            if q.token.as_deref() == Some(expected.as_str()) {
                Ok(())
            } else {
                Err(StatusCode::UNAUTHORIZED)
            }
        }
    }
}

async fn meta(State(state): State<HttpState>) -> Json<Value> {
    Json(state.meta.clone())
}

async fn list_sessions(
    State(state): State<HttpState>,
    Query(q): Query<AuthQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    Ok(Json(state.backend.list_sessions().await))
}

#[derive(Deserialize)]
struct CreateSessionBody {
    workspace: Option<String>,
    title: Option<String>,
}

async fn create_session(
    State(state): State<HttpState>,
    Query(q): Query<AuthQuery>,
    Json(body): Json<CreateSessionBody>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    let sess = state
        .backend
        .create_session(body.workspace, body.title)
        .await;
    if let Some(id) = sess.get("session_id").and_then(|v| v.as_str()) {
        state.publish(json!({"type": "session.created", "session_id": id}));
    }
    Ok(Json(sess))
}

async fn get_session(
    State(state): State<HttpState>,
    Path(id): Path<String>,
    Query(q): Query<AuthQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    state
        .backend
        .get_session(&id)
        .await
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

#[derive(Deserialize)]
struct PostMessageBody {
    text: String,
}

async fn post_message(
    State(state): State<HttpState>,
    Path(id): Path<String>,
    Query(q): Query<AuthQuery>,
    Json(body): Json<PostMessageBody>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    match state.backend.post_message(&id, &body.text).await {
        Ok(v) => {
            state.publish(json!({"type": "message", "session_id": id, "text": body.text}));
            Ok(Json(v))
        }
        Err(_) => Err(StatusCode::NOT_FOUND),
    }
}

#[derive(Deserialize)]
struct ApprovalBody {
    decision: String,
    #[serde(default)]
    feedback: Option<String>,
}

async fn post_approval(
    State(state): State<HttpState>,
    Path(id): Path<String>,
    Query(q): Query<AuthQuery>,
    Json(body): Json<ApprovalBody>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    let v = state
        .backend
        .approve(&id, &body.decision, body.feedback.clone())
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    state.publish(json!({
        "type": "approval",
        "approval_id": id,
        "decision": body.decision,
        "feedback": body.feedback,
    }));
    Ok(Json(v))
}

async fn tools(
    State(state): State<HttpState>,
    Query(q): Query<AuthQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    Ok(Json(state.backend.list_tools().await))
}

async fn tasks(
    State(state): State<HttpState>,
    Query(q): Query<AuthQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    Ok(Json(state.backend.list_tasks().await))
}

async fn skills(
    State(state): State<HttpState>,
    Query(q): Query<AuthQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    Ok(Json(state.backend.list_skills().await))
}

async fn model_catalog(
    State(state): State<HttpState>,
    Query(q): Query<AuthQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    Ok(Json(state.backend.list_models().await))
}

async fn config_get(
    State(state): State<HttpState>,
    Query(q): Query<AuthQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    Ok(Json(state.backend.get_config().await))
}

async fn workspaces(
    State(state): State<HttpState>,
    Query(q): Query<AuthQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    Ok(Json(state.backend.workspace_info().await))
}

#[derive(Deserialize)]
struct FsBody {
    path: String,
    #[serde(default)]
    content: Option<String>,
}

#[derive(Deserialize)]
struct FsPathQuery {
    path: String,
    #[serde(default)]
    token: Option<String>,
}

async fn fs_read(
    State(state): State<HttpState>,
    Query(path_q): Query<FsPathQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(
        &state,
        &AuthQuery {
            token: path_q.token.clone(),
        },
    )?;
    match state.backend.fs_read(&path_q.path).await {
        Ok(content) => Ok(Json(json!({"path": path_q.path, "content": content}))),
        Err(e) => Ok(Json(json!({"error": e}))),
    }
}

async fn fs_write(
    State(state): State<HttpState>,
    Query(q): Query<AuthQuery>,
    Json(body): Json<FsBody>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    let content = body.content.unwrap_or_default();
    state
        .backend
        .fs_write(&body.path, &content)
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    Ok(Json(json!({"ok": true, "path": body.path})))
}

async fn files_list(
    State(state): State<HttpState>,
    Query(q): Query<AuthQuery>,
    Query(path_q): Query<FsPathQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    let path = if path_q.path.is_empty() {
        ".".into()
    } else {
        path_q.path
    };
    let rd = std::fs::read_dir(&path).map_err(|_| StatusCode::NOT_FOUND)?;
    let entries: Vec<Value> = rd
        .flatten()
        .take(200)
        .map(|e| {
            json!({
                "name": e.file_name().to_string_lossy(),
                "path": e.path().display().to_string(),
                "is_dir": e.file_type().map(|t| t.is_dir()).unwrap_or(false),
            })
        })
        .collect();
    Ok(Json(json!({"entries": entries})))
}

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
    #[serde(default)]
    token: Option<String>,
}

async fn search(
    State(state): State<HttpState>,
    Query(q): Query<SearchQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(
        &state,
        &AuthQuery {
            token: q.token.clone(),
        },
    )?;
    Ok(Json(state.backend.search(&q.q).await))
}

async fn snapshot(
    State(state): State<HttpState>,
    Query(q): Query<AuthQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    Ok(Json(json!({
        "sessions": state.backend.list_sessions().await,
        "workspace": state.backend.workspace_info().await,
        "at": chrono::Utc::now().to_rfc3339(),
    })))
}

async fn prompts(
    State(state): State<HttpState>,
    Query(q): Query<AuthQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    Ok(Json(json!({"prompts": [
        {"id": "default", "title": "Default system"},
        {"id": "plan", "title": "Plan mode"},
    ]})))
}

async fn questions_stub(
    State(state): State<HttpState>,
    Query(q): Query<AuthQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    Ok(Json(json!({"questions": []})))
}

async fn terminals_list(
    State(state): State<HttpState>,
    Query(q): Query<AuthQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    let map = state.terminals.lock().await;
    Ok(Json(
        json!({"terminals": map.values().cloned().collect::<Vec<_>>()}),
    ))
}

#[derive(Deserialize)]
struct TerminalCreate {
    command: Option<String>,
    cwd: Option<String>,
}

async fn terminals_create(
    State(state): State<HttpState>,
    Query(q): Query<AuthQuery>,
    Json(body): Json<TerminalCreate>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    let id = uuid::Uuid::new_v4().to_string();
    let t = json!({
        "terminal_id": id,
        "command": body.command,
        "cwd": body.cwd.unwrap_or_else(|| ".".into()),
        "status": "created",
    });
    state.terminals.lock().await.insert(id, t.clone());
    Ok(Json(t))
}

async fn connections(
    State(state): State<HttpState>,
    Query(q): Query<AuthQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    Ok(Json(
        json!({"connections": [{"type": "ws", "path": "/api/v1/ws"}]}),
    ))
}

async fn export_session(
    State(state): State<HttpState>,
    Path(id): Path<String>,
    Query(q): Query<AuthQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    let sess = state
        .backend
        .get_session(&id)
        .await
        .ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(json!({"export": sess, "format": "json"})))
}

async fn delete_session(
    State(state): State<HttpState>,
    Path(id): Path<String>,
    Query(q): Query<AuthQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    // Memory backend has no delete — report ok for API shape.
    let _ = id;
    Ok(Json(json!({"ok": true})))
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<HttpState>,
    Query(q): Query<AuthQuery>,
) -> Result<impl IntoResponse, StatusCode> {
    check_auth(&state, &q)?;
    Ok(ws.on_upgrade(move |socket| ws_loop(socket, state)))
}

async fn ws_loop(mut socket: WebSocket, state: HttpState) {
    let mut rx = state.events.subscribe();
    let _ = socket
        .send(Message::Text(
            json!({"type": "hello", "api": "v1"}).to_string().into(),
        ))
        .await;
    loop {
        tokio::select! {
            evt = rx.recv() => {
                match evt {
                    Ok(v) => {
                        if socket.send(Message::Text(v.to_string().into())).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(t))) => {
                        if t.contains("ping") {
                            let _ = socket.send(Message::Text("{\"type\":\"pong\"}".into())).await;
                        } else if let Ok(v) = serde_json::from_str::<Value>(&t) {
                            if v.get("type").and_then(|x| x.as_str()) == Some("subscribe") {
                                let _ = socket.send(Message::Text(
                                    json!({"type":"subscribed","channels":["events","roster","fsWatch"]}).to_string().into()
                                )).await;
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {}
                }
            }
        }
    }
}

pub fn router(state: HttpState) -> Router {
    Router::new()
        .route("/api/v1/meta", get(meta))
        .route("/api/v1/sessions", get(list_sessions).post(create_session))
        .route(
            "/api/v1/sessions/{id}",
            get(get_session).delete(delete_session),
        )
        .route("/api/v1/sessions/{id}/messages", post(post_message))
        .route("/api/v1/sessions/{id}/export", get(export_session))
        .route("/api/v1/approvals/{id}", post(post_approval))
        .route("/api/v1/tools", get(tools))
        .route("/api/v1/tasks", get(tasks))
        .route("/api/v1/skills", get(skills))
        .route("/api/v1/modelCatalog", get(model_catalog))
        .route("/api/v1/models", get(model_catalog))
        .route("/api/v1/config", get(config_get))
        .route("/api/v1/workspaces", get(workspaces))
        .route("/api/v1/workspaceFs", get(workspaces))
        .route("/api/v1/fs", get(fs_read).post(fs_write))
        .route("/api/v1/files", get(files_list))
        .route("/api/v1/search", get(search))
        .route("/api/v1/snapshot", get(snapshot))
        .route("/api/v1/prompts", get(prompts))
        .route("/api/v1/questions", get(questions_stub))
        .route(
            "/api/v1/terminals",
            get(terminals_list).post(terminals_create),
        )
        .route("/api/v1/connections", get(connections))
        .route("/api/v1/ws", get(ws_handler))
        .with_state(state)
}

pub async fn serve(addr: &str, token: Option<String>) -> anyhow::Result<()> {
    serve_with_backend(addr, Arc::new(MemoryBackend::default()), token).await
}

pub async fn serve_with_backend(
    addr: &str,
    backend: Arc<dyn HttpBackend>,
    token: Option<String>,
) -> anyhow::Result<()> {
    let token = token.filter(|value| !value.trim().is_empty());
    let resolved: Vec<std::net::SocketAddr> = tokio::net::lookup_host(addr).await?.collect();
    if resolved.is_empty() {
        anyhow::bail!("HTTP listen address resolved to no socket addresses: {addr}");
    }
    if token.is_none() && resolved.iter().any(|socket| !socket.ip().is_loopback()) {
        anyhow::bail!(
            "refusing to expose unauthenticated HTTP API on non-loopback address {addr}; pass --http-token or KKAGENT_HTTP_TOKEN"
        );
    }
    let state = HttpState::with_backend(backend, token);
    let app = router(state);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("kkagent HTTP listening on http://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod security_tests {
    use super::*;

    #[tokio::test]
    async fn refuses_unauthenticated_non_loopback_listener() {
        let result =
            serve_with_backend("0.0.0.0:0", Arc::new(MemoryBackend::default()), None).await;
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("refusing to expose unauthenticated"));
    }
}
