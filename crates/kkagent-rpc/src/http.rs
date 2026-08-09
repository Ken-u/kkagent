//! REST API v1 + WebSocket (kap-server route matrix subset) with AgentLoop backend hooks.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::process::Child;
use tokio::sync::{broadcast, Mutex};

/// Pluggable backend so HTTP can bind to the live AgentLoop/ServerState.
#[async_trait::async_trait]
pub trait HttpBackend: Send + Sync {
    fn event_sender(&self) -> Option<broadcast::Sender<Value>> {
        None
    }

    async fn list_sessions(&self) -> Value;
    async fn create_session(
        &self,
        workspace: Option<String>,
        title: Option<String>,
    ) -> Result<Value, String>;
    async fn get_session(&self, id: &str) -> Option<Value>;
    async fn delete_session(&self, _id: &str) -> Result<(), String> {
        Err("session deletion is not supported by this backend".into())
    }
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
    async fn list_files(&self, _path: &str) -> Result<Value, String> {
        Err("file listing is not supported by this backend".into())
    }
    async fn search(&self, query: &str) -> Value;
    async fn workspace_info(&self) -> Value;
    async fn list_questions(&self) -> Value {
        json!({"questions": []})
    }
    async fn answer_question(&self, _id: &str, _response: Value) -> Result<Value, String> {
        Err("question responses are not supported by this backend".into())
    }
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
    async fn create_session(
        &self,
        workspace: Option<String>,
        title: Option<String>,
    ) -> Result<Value, String> {
        let id = uuid::Uuid::new_v4().to_string();
        let sess = json!({
            "session_id": id,
            "workspace": workspace.unwrap_or_else(|| ".".into()),
            "title": title,
            "created_at": chrono::Utc::now().to_rfc3339(),
            "messages": [],
        });
        self.sessions.lock().await.insert(id, sess.clone());
        Ok(sess)
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
    terminals: Arc<Mutex<HashMap<String, HttpTerminalSlot>>>,
}

struct HttpTerminalSlot {
    info: Value,
    child: Option<Child>,
    stdout: Arc<Mutex<Vec<u8>>>,
    stderr: Arc<Mutex<Vec<u8>>>,
}

impl HttpState {
    pub fn new(token: Option<String>) -> Self {
        Self::with_backend(Arc::new(MemoryBackend::default()), token)
    }

    pub fn with_backend(backend: Arc<dyn HttpBackend>, token: Option<String>) -> Self {
        let events = backend.event_sender().unwrap_or_else(|| {
            let (events, _) = broadcast::channel(512);
            events
        });
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

fn authorize_request(state: &HttpState, request: &mut Request) -> Result<(), StatusCode> {
    let Some(expected) = state.token.as_deref() else {
        return Ok(());
    };
    let query_token = url::Url::parse(&format!("http://localhost{}", request.uri()))
        .ok()
        .and_then(|url| {
            url.query_pairs()
                .find(|(name, _)| name == "token")
                .map(|(_, value)| value.into_owned())
        });
    if query_token.as_deref() == Some(expected) {
        return Ok(());
    }
    let bearer = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, token)| token);
    if bearer != Some(expected) {
        return Err(StatusCode::UNAUTHORIZED);
    }

    // Existing handlers also validate the legacy query token. Inject an encoded
    // copy after authenticating the header so both access paths share one check.
    let mut url = url::Url::parse(&format!("http://localhost{}", request.uri()))
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    url.query_pairs_mut().append_pair("token", expected);
    let path_and_query = match url.query() {
        Some(query) => format!("{}?{query}", url.path()),
        None => url.path().to_string(),
    };
    *request.uri_mut() = path_and_query
        .parse()
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    Ok(())
}

async fn auth_middleware(
    State(state): State<HttpState>,
    mut request: Request,
    next: Next,
) -> impl IntoResponse {
    match authorize_request(&state, &mut request) {
        Ok(()) => next.run(request).await,
        Err(status) => status.into_response(),
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
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?;
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

#[derive(Deserialize)]
struct FilesQuery {
    #[serde(default)]
    path: Option<String>,
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
    Query(query): Query<FilesQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &AuthQuery { token: query.token })?;
    state
        .backend
        .list_files(query.path.as_deref().unwrap_or("."))
        .await
        .map(Json)
        .map_err(|_| StatusCode::BAD_REQUEST)
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

async fn questions(
    State(state): State<HttpState>,
    Query(q): Query<AuthQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    Ok(Json(state.backend.list_questions().await))
}

async fn answer_question(
    State(state): State<HttpState>,
    Path(id): Path<String>,
    Query(q): Query<AuthQuery>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    state
        .backend
        .answer_question(&id, body)
        .await
        .map(Json)
        .map_err(|_| StatusCode::BAD_REQUEST)
}

async fn terminals_list(
    State(state): State<HttpState>,
    Query(q): Query<AuthQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    let map = state.terminals.lock().await;
    Ok(Json(json!({
        "terminals": map.values().map(|slot| slot.info.clone()).collect::<Vec<_>>()
    })))
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
    let command_text = body.command.unwrap_or_else(|| {
        if cfg!(windows) {
            "echo kkagent-terminal".into()
        } else {
            "printf kkagent-terminal".into()
        }
    });
    let cwd = body.cwd.unwrap_or_else(|| ".".into());
    let mut command = if cfg!(windows) {
        let mut command = tokio::process::Command::new("cmd");
        command.args(["/C", &command_text]);
        command
    } else {
        let mut command = tokio::process::Command::new("sh");
        command.args(["-lc", &command_text]);
        command
    };
    command
        .current_dir(&cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(|_| StatusCode::BAD_REQUEST)?;
    let stdout = Arc::new(Mutex::new(Vec::new()));
    let stderr = Arc::new(Mutex::new(Vec::new()));
    if let Some(mut pipe) = child.stdout.take() {
        let output = stdout.clone();
        tokio::spawn(async move {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes).await;
            *output.lock().await = bytes;
        });
    }
    if let Some(mut pipe) = child.stderr.take() {
        let output = stderr.clone();
        tokio::spawn(async move {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes).await;
            *output.lock().await = bytes;
        });
    }
    let terminal = json!({
        "terminal_id": id,
        "command": command_text,
        "cwd": cwd,
        "status": "running",
    });
    state.terminals.lock().await.insert(
        id,
        HttpTerminalSlot {
            info: terminal.clone(),
            child: Some(child),
            stdout,
            stderr,
        },
    );
    Ok(Json(terminal))
}

async fn terminal_get(
    State(state): State<HttpState>,
    Path(id): Path<String>,
    Query(q): Query<AuthQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    let mut terminals = state.terminals.lock().await;
    let slot = terminals.get_mut(&id).ok_or(StatusCode::NOT_FOUND)?;
    if let Some(child) = slot.child.as_mut() {
        if let Ok(Some(status)) = child.try_wait() {
            slot.info["status"] = json!("exited");
            slot.info["exit_code"] = json!(status.code());
            slot.child = None;
        }
    }
    let stdout = String::from_utf8_lossy(&slot.stdout.lock().await).into_owned();
    let stderr = String::from_utf8_lossy(&slot.stderr.lock().await).into_owned();
    Ok(Json(json!({
        "terminal": slot.info,
        "stdout": stdout,
        "stderr": stderr,
    })))
}

async fn terminal_delete(
    State(state): State<HttpState>,
    Path(id): Path<String>,
    Query(q): Query<AuthQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    let mut slot = state
        .terminals
        .lock()
        .await
        .remove(&id)
        .ok_or(StatusCode::NOT_FOUND)?;
    if let Some(mut child) = slot.child.take() {
        child
            .kill()
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    }
    Ok(Json(json!({"ok": true, "terminal_id": id})))
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
    state
        .backend
        .delete_session(&id)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    state.publish(json!({"type": "session.deleted", "session_id": id}));
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
        .route("/api/v1/questions", get(questions))
        .route("/api/v1/questions/{id}", post(answer_question))
        .route(
            "/api/v1/terminals",
            get(terminals_list).post(terminals_create),
        )
        .route(
            "/api/v1/terminals/{id}",
            get(terminal_get).delete(terminal_delete),
        )
        .route("/api/v1/connections", get(connections))
        .route("/api/v1/ws", get(ws_handler))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
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
    let listener = bind(addr, token.as_deref()).await?;
    serve_listener_with_backend(listener, backend, token).await
}

pub async fn bind(addr: &str, token: Option<&str>) -> anyhow::Result<tokio::net::TcpListener> {
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
    let listener = tokio::net::TcpListener::bind(addr).await?;
    Ok(listener)
}

pub async fn serve_listener_with_backend(
    listener: tokio::net::TcpListener,
    backend: Arc<dyn HttpBackend>,
    token: Option<String>,
) -> anyhow::Result<()> {
    let address = listener.local_addr()?;
    let state = HttpState::with_backend(backend, token);
    let app = router(state);
    tracing::info!("kkagent HTTP listening on http://{address}");
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

    #[test]
    fn accepts_bearer_auth_and_injects_legacy_query_token() {
        let state = HttpState::new(Some("complex token".into()));
        let mut request = Request::builder()
            .uri("/api/v1/sessions?view=all")
            .header("authorization", "Bearer complex token")
            .body(axum::body::Body::empty())
            .unwrap();
        authorize_request(&state, &mut request).unwrap();
        let query = request.uri().query().unwrap();
        assert!(query.contains("view=all"));
        assert!(query.contains("token=complex+token"));
    }

    #[test]
    fn rejects_missing_http_auth() {
        let state = HttpState::new(Some("secret".into()));
        let mut request = Request::builder()
            .uri("/api/v1/sessions")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(
            authorize_request(&state, &mut request),
            Err(StatusCode::UNAUTHORIZED)
        );
    }

    #[tokio::test]
    async fn terminal_endpoint_runs_and_captures_output() {
        let state = HttpState::new(None);
        let Json(created) = terminals_create(
            State(state.clone()),
            Query(AuthQuery::default()),
            Json(TerminalCreate {
                command: Some("echo terminal-ok".into()),
                cwd: None,
            }),
        )
        .await
        .unwrap();
        let id = created["terminal_id"].as_str().unwrap().to_string();
        for _ in 0..50 {
            let Json(snapshot) = terminal_get(
                State(state.clone()),
                Path(id.clone()),
                Query(AuthQuery::default()),
            )
            .await
            .unwrap();
            if snapshot["terminal"]["status"] == "exited"
                && snapshot["stdout"]
                    .as_str()
                    .unwrap_or("")
                    .contains("terminal-ok")
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("terminal did not exit with captured output");
    }
}
