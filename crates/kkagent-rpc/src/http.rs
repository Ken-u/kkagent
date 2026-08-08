//! REST API v1 + WebSocket event bridge (kap-server subset).

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
use tower_http::cors::CorsLayer;

#[derive(Clone)]
pub struct HttpState {
    pub sessions: Arc<Mutex<HashMap<String, Value>>>,
    pub meta: Value,
    pub events: broadcast::Sender<Value>,
    pub token: Option<String>,
}

impl HttpState {
    pub fn new(token: Option<String>) -> Self {
        let (events, _) = broadcast::channel(256);
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            meta: json!({
                "name": "kkagent",
                "version": env!("CARGO_PKG_VERSION"),
                "api": ["v1"],
                "capabilities": ["sessions", "messages", "approvals", "ws"],
            }),
            events,
            token,
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
    let map = state.sessions.lock().await;
    let list: Vec<Value> = map.values().cloned().collect();
    Ok(Json(json!({"sessions": list})))
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
    let id = uuid::Uuid::new_v4().to_string();
    let sess = json!({
        "session_id": id,
        "workspace": body.workspace.unwrap_or_else(|| ".".into()),
        "title": body.title,
        "created_at": chrono::Utc::now().to_rfc3339(),
        "messages": [],
    });
    state.sessions.lock().await.insert(id.clone(), sess.clone());
    state.publish(json!({"type": "session.created", "session_id": id}));
    Ok(Json(sess))
}

async fn get_session(
    State(state): State<HttpState>,
    Path(id): Path<String>,
    Query(q): Query<AuthQuery>,
) -> Result<Json<Value>, StatusCode> {
    check_auth(&state, &q)?;
    let map = state.sessions.lock().await;
    map.get(&id)
        .cloned()
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
    let mut map = state.sessions.lock().await;
    let sess = map.get_mut(&id).ok_or(StatusCode::NOT_FOUND)?;
    let msg = json!({"role": "user", "text": body.text, "at": chrono::Utc::now().to_rfc3339()});
    if let Some(arr) = sess.get_mut("messages").and_then(|v| v.as_array_mut()) {
        arr.push(msg.clone());
    }
    state.publish(json!({"type": "message", "session_id": id, "message": msg}));
    Ok(Json(json!({"ok": true})))
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
    state.publish(json!({
        "type": "approval",
        "approval_id": id,
        "decision": body.decision,
        "feedback": body.feedback,
    }));
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
        .route("/api/v1/sessions/{id}", get(get_session))
        .route("/api/v1/sessions/{id}/messages", post(post_message))
        .route("/api/v1/approvals/{id}", post(post_approval))
        .route("/api/v1/ws", get(ws_handler))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

pub async fn serve(addr: &str, token: Option<String>) -> anyhow::Result<()> {
    let state = HttpState::new(token);
    let app = router(state);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("kkagent HTTP listening on http://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
}
