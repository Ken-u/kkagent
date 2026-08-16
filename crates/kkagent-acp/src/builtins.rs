use serde_json::{json, Value};

use crate::{AcpHost, AcpSessionStore};

pub fn builtin_command_list() -> Vec<Value> {
    vec![
        json!({"name": "ping", "description": "Health check"}),
        json!({"name": "help", "description": "Show available ACP commands"}),
        json!({"name": "status", "description": "Show current session status"}),
        json!({"name": "usage", "description": "Show session token usage (stub)"}),
        json!({"name": "mcp", "description": "Show MCP server status"}),
        json!({"name": "sessions", "description": "List ACP sessions"}),
        json!({"name": "compact", "description": "Compact conversation (host-dependent)"}),
        json!({"name": "version", "description": "Show ACP server version"}),
    ]
}

pub async fn run_builtin(
    name: &str,
    args: &str,
    session_id: &str,
    store: &AcpSessionStore,
    host: &dyn AcpHost,
) -> Result<Value, String> {
    match name {
        "ping" => Ok(json!({"pong": true})),
        "help" => Ok(json!({
            "text": "Available ACP commands",
            "commands": builtin_command_list(),
        })),
        "version" => Ok(json!({
            "name": "kkagent-acp",
            "version": env!("CARGO_PKG_VERSION"),
        })),
        "status" => {
            let sessions = store.sessions.lock().await;
            let Some(sess) = sessions.get(session_id) else {
                return Err(format!("session not found: {session_id}"));
            };
            Ok(json!({
                "text": format!("session {session_id}"),
                "session": sess.clone(),
            }))
        }
        "sessions" => {
            let sessions: Vec<Value> = store.sessions.lock().await.values().cloned().collect();
            Ok(json!({"sessions": sessions}))
        }
        "usage" => Ok(json!({
            "text": "Usage is tracked by the agent host; request session/prompt events for live counters.",
            "inputTokens": null,
            "outputTokens": null,
        })),
        "mcp" => Ok(host.list_mcp().await),
        "compact" => {
            let instruction = if args.trim().is_empty() {
                "Compact the conversation context.".to_string()
            } else {
                args.trim().to_string()
            };
            host.prompt(session_id, &format!("/compact {instruction}"))
                .await
        }
        other => Err(format!("unknown builtin command: {other}")),
    }
}
