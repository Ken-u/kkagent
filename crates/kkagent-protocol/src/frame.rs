use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Frame {
    Hello {
        #[serde(skip_serializing_if = "Option::is_none")]
        token: Option<String>,
    },
    Ready,
    Call {
        id: String,
        method: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        params: Option<serde_json::Value>,
    },
    Result {
        id: String,
        data: serde_json::Value,
    },
    Error {
        id: String,
        code: i32,
        message: String,
    },
    Listen {
        id: String,
        event: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scope: Option<String>,
    },
    ListenResult {
        id: String,
    },
    Unlisten {
        id: String,
    },
    Event {
        event: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scope: Option<String>,
        data: serde_json::Value,
    },
    StreamData {
        id: String,
        data: serde_json::Value,
    },
    StreamEnd {
        id: String,
    },
    StreamError {
        id: String,
        code: i32,
        message: String,
    },
}
