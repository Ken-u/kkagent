use serde_json::{json, Value};

#[derive(Debug, Default)]
pub struct AuthTokenStore {
    token: Option<String>,
}

impl AuthTokenStore {
    pub fn set_token(&mut self, token: &str) {
        self.token = Some(token.to_string());
    }

    pub fn clear(&mut self) {
        self.token = None;
    }

    pub fn is_authenticated(&self) -> bool {
        self.token.as_ref().is_some_and(|t| !t.is_empty())
    }

    pub fn method(&self) -> &'static str {
        if self.is_authenticated() {
            "token"
        } else {
            "local"
        }
    }
}

/// Auth methods advertised during `initialize` (ACP / kimi-aligned).
pub fn auth_methods() -> Vec<Value> {
    vec![
        json!({
            "id": "local",
            "type": "none",
            "name": "Local (no auth)",
            "description": "Trust the local ACP stdio / socket connection.",
        }),
        json!({
            "id": "token",
            "type": "token",
            "name": "API token",
            "description": "Authenticate with a bearer token via auth/authenticate.",
        }),
        json!({
            "id": "login",
            "type": "terminal",
            "name": "Login with kkagent account",
            "description": "Open the device-code login flow in a terminal.",
            "args": ["auth", "login"],
            "_meta": {
                "terminal-auth": {
                    "type": "terminal",
                    "label": "Login with kkagent account",
                    "command": "kkagent",
                    "args": ["auth", "login"],
                }
            }
        }),
    ]
}
