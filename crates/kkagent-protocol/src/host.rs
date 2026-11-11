//! Wire types for the embedding-host extension of ACP.
//!
//! A host (for example a supervisor process driving kkagent over ACP) talks
//! standard ACP v1 plus a small set of `_kkagent/*` methods. Capabilities are
//! negotiated through `_meta["kkagent/host"]` on `initialize`, and per-session
//! settings travel in `_meta["kkagent/host"]` on `session/new`, `session/load`
//! and `session/resume`. Everything here is optional: a client that never sends
//! these fields gets the unchanged default behavior.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Key under `_meta` that carries host-extension data.
pub const HOST_META_KEY: &str = "kkagent/host";

/// Agent→client request: execute a host-provided tool.
pub const METHOD_HOST_TOOL_CALL: &str = "_kkagent/tool/call";
/// Agent→client notification: a host tool call was cancelled.
pub const METHOD_HOST_TOOL_CANCEL: &str = "_kkagent/tool/cancel";
/// Agent→client request: may this tool call run? Sent before every reviewed
/// call, whatever permission mode let it through.
pub const METHOD_HOST_TOOL_REVIEW: &str = "_kkagent/tool/review";
/// Agent→client request prefix for host-backed storage.
pub const METHOD_STORE_PREFIX: &str = "_kkagent/store/";

/// Capabilities a host (client side) can offer; also echoed by the agent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostCapabilities {
    /// The host can execute tools it injected (`_kkagent/tool/call`).
    #[serde(default)]
    pub tools: bool,
    /// The host answers `_kkagent/tool/review` requests.
    #[serde(default)]
    pub tool_review: bool,
    /// The host can serve session/event storage (`_kkagent/store/*`).
    #[serde(default)]
    pub store: HostStoreCapabilities,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostStoreCapabilities {
    /// Sessions and messages.
    #[serde(default)]
    pub sessions: bool,
    /// Durable event log.
    #[serde(default)]
    pub events: bool,
}

impl HostCapabilities {
    /// Read capabilities from a `_meta` object (`_meta["kkagent/host"]`).
    pub fn from_meta(meta: Option<&Value>) -> Self {
        meta.and_then(|meta| meta.get(HOST_META_KEY))
            .and_then(|value| serde_json::from_value(value.clone()).ok())
            .unwrap_or_default()
    }
}

/// Who authorizes a host tool call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HostToolApproval {
    /// The host is the executor and applies its own policy; kkagent does not
    /// ask again.
    #[default]
    Host,
    /// Route the call through kkagent's permission chain first.
    Ask,
}

/// A tool injected by the host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HostToolSpec {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// JSON Schema for the tool input. Defaults to an empty object schema.
    #[serde(default = "default_input_schema")]
    pub input_schema: Value,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub approval: HostToolApproval,
    /// How long one call may take before kkagent gives up and cancels it.
    /// Defaults to [`DEFAULT_HOST_TOOL_TIMEOUT_SECS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

/// Default time budget of one host tool call.
pub const DEFAULT_HOST_TOOL_TIMEOUT_SECS: u64 = 600;
/// Longest budget a host tool may ask for.
pub const MAX_HOST_TOOL_TIMEOUT_SECS: u64 = 24 * 60 * 60;

fn default_input_schema() -> Value {
    serde_json::json!({"type": "object", "properties": {}})
}

impl HostToolSpec {
    /// A tool name is exposed to the model verbatim, so keep it to the
    /// character set every provider accepts.
    pub fn validate(&self) -> Result<(), String> {
        let name = self.name.as_str();
        let ok = !name.is_empty()
            && name.len() <= 64
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        if !ok {
            return Err(format!(
                "invalid host tool name `{name}`: use 1-64 characters from [A-Za-z0-9_-]"
            ));
        }
        if !self.input_schema.is_object() {
            return Err(format!(
                "host tool `{name}` inputSchema must be a JSON object"
            ));
        }
        if let Some(seconds) = self.timeout_secs {
            if seconds == 0 || seconds > MAX_HOST_TOOL_TIMEOUT_SECS {
                return Err(format!(
                    "host tool `{name}` timeoutSecs must be between 1 and {MAX_HOST_TOOL_TIMEOUT_SECS}"
                ));
            }
        }
        Ok(())
    }
}

/// Per-session extension toggles from the host. Each flag can only turn an
/// extension off; `None` leaves it as configured.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExtensionOverrides {
    #[serde(default)]
    pub load_project_instructions: Option<bool>,
    #[serde(default)]
    pub load_hooks: Option<bool>,
    #[serde(default)]
    pub load_plugins: Option<bool>,
}

/// Which tool calls the host reviews before they run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolReviewMode {
    /// No review (default).
    #[default]
    Off,
    /// Review every call of a tool that is not read-only.
    Mutating,
    /// Review every call.
    All,
}

impl ToolReviewMode {
    fn strictness(self) -> u8 {
        match self {
            Self::Off => 0,
            Self::Mutating => 1,
            Self::All => 2,
        }
    }

    /// The stricter of two modes (a review can be tightened, never loosened).
    pub fn strictest(self, other: Self) -> Self {
        if other.strictness() > self.strictness() {
            other
        } else {
            self
        }
    }

    /// Whether a call of a tool with this read-only flag is reviewed.
    pub fn reviews(self, read_only: bool) -> bool {
        match self {
            Self::Off => false,
            Self::Mutating => !read_only,
            Self::All => true,
        }
    }
}

/// Default time the host has to answer one review.
pub const DEFAULT_TOOL_REVIEW_TIMEOUT_SECS: u64 = 300;
/// Longest time a host may be given to answer one review.
pub const MAX_TOOL_REVIEW_TIMEOUT_SECS: u64 = 24 * 60 * 60;

/// Host review of tool calls: `_meta["kkagent/host"].toolReview`.
///
/// kkagent sends `_kkagent/tool/review` *after* its own permission chain
/// (including `yolo`) let a call through and *before* the tool runs. The host
/// answers `{decision: "allow" | "deny", reason?}`. Anything else (no answer,
/// timeout, a disconnected host, an error) is a denial.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolReviewSpec {
    #[serde(default)]
    pub mode: ToolReviewMode,
    /// How long the host may take per call. Defaults to
    /// [`DEFAULT_TOOL_REVIEW_TIMEOUT_SECS`]; an unanswered review is a denial.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

impl ToolReviewSpec {
    pub fn validate(&self) -> Result<(), String> {
        if let Some(seconds) = self.timeout_secs {
            if seconds == 0 || seconds > MAX_TOOL_REVIEW_TIMEOUT_SECS {
                return Err(format!(
                    "toolReview.timeoutSecs must be between 1 and {MAX_TOOL_REVIEW_TIMEOUT_SECS}"
                ));
            }
        }
        Ok(())
    }

    pub fn timeout_secs(&self) -> u64 {
        self.timeout_secs
            .unwrap_or(DEFAULT_TOOL_REVIEW_TIMEOUT_SECS)
    }
}

/// Params of `_kkagent/tool/review`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolReviewRequest {
    pub session_id: String,
    pub tool_call_id: String,
    pub name: String,
    pub input: Value,
    pub cwd: String,
    pub read_only: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolReviewDecision {
    Allow,
    Deny,
}

/// Result of `_kkagent/tool/review`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolReviewReply {
    pub decision: ToolReviewDecision,
    /// Shown to the model when the call is denied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Per-session settings carried in `_meta["kkagent/host"]`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HostSessionMeta {
    /// Name of a role from the trusted config (`[agent_roles.NAME]`).
    #[serde(default)]
    pub role: Option<String>,
    /// Extra tool allowlist for this session. Intersected with the role and
    /// process ceilings; it can narrow but never widen them.
    #[serde(default)]
    pub tool_allowlist: Option<Vec<String>>,
    /// Tools the host injects for this session.
    #[serde(default)]
    pub host_tools: Vec<HostToolSpec>,
    #[serde(default)]
    pub extensions: Option<ExtensionOverrides>,
    /// Initial permission mode (`manual`, `yolo`, `auto`).
    #[serde(default)]
    pub permission_mode: Option<String>,
    /// Ask the host before tool calls run (works under `yolo`/`auto` too).
    #[serde(default)]
    pub tool_review: Option<ToolReviewSpec>,
    /// `session/load` only: `false` skips replaying the stored history to the
    /// client (for a host that already persists the conversation itself).
    /// Default `true`; ignored by `session/new` and `session/resume`.
    #[serde(default)]
    pub replay: Option<bool>,
}

impl HostSessionMeta {
    /// Read the host section of a `_meta` object. A missing section yields the
    /// default (no overrides); a malformed one is an error so a typo cannot
    /// silently drop a restriction.
    pub fn from_meta(meta: Option<&Value>) -> Result<Self, String> {
        let Some(section) = meta.and_then(|meta| meta.get(HOST_META_KEY)) else {
            return Ok(Self::default());
        };
        if section.is_null() {
            return Ok(Self::default());
        }
        let parsed: Self = serde_json::from_value(section.clone())
            .map_err(|error| format!("invalid _meta[\"{HOST_META_KEY}\"]: {error}"))?;
        let mut seen = std::collections::HashSet::new();
        if let Some(review) = &parsed.tool_review {
            review.validate()?;
        }
        for tool in &parsed.host_tools {
            tool.validate()?;
            if !seen.insert(tool.name.as_str()) {
                return Err(format!("duplicate host tool `{}`", tool.name));
            }
        }
        Ok(parsed)
    }
}

/// Method names of the host-backed storage extension. Every one is an
/// agent→client request.
pub mod store_method {
    pub const SESSION_CREATE: &str = "_kkagent/store/session/create";
    pub const SESSION_GET: &str = "_kkagent/store/session/get";
    pub const SESSION_LIST: &str = "_kkagent/store/session/list";
    pub const SESSION_UPDATE: &str = "_kkagent/store/session/update";
    pub const SESSION_DELETE: &str = "_kkagent/store/session/delete";
    pub const SESSION_FORK: &str = "_kkagent/store/session/fork";
    pub const MESSAGES_APPEND: &str = "_kkagent/store/messages/append";
    pub const MESSAGES_REPLACE: &str = "_kkagent/store/messages/replace";
    pub const MESSAGES_LOAD: &str = "_kkagent/store/messages/load";
    pub const TOOL_RESULT_RECORD: &str = "_kkagent/store/toolResult/record";
    pub const EVENTS_APPEND: &str = "_kkagent/store/events/append";
    pub const EVENTS_SINCE: &str = "_kkagent/store/events/since";
    pub const EVENTS_LATEST_SEQ: &str = "_kkagent/store/events/latestSeq";
}

/// JSON-RPC error code a host answers with when a session does not exist.
pub const ERROR_SESSION_NOT_FOUND: i64 = -32002;
/// JSON-RPC error code a host answers with when a session already exists.
pub const ERROR_SESSION_EXISTS: i64 = -32003;

/// A stored session as exchanged with a storage host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoreSession {
    pub session_id: String,
    #[serde(default)]
    pub title: Option<String>,
    pub model: String,
    /// `null` inherits the global setting, `""` disables fallback, otherwise
    /// a model alias.
    #[serde(default)]
    pub fallback_model: Option<String>,
    pub working_dir: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub message_count: u32,
    #[serde(default)]
    pub is_archived: bool,
    #[serde(default)]
    pub parent_session_id: Option<String>,
}

/// A message to append or to replace a transcript with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoreNewMessage {
    pub role: String,
    pub content_json: String,
}

/// A stored message as returned by a storage host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoreMessage {
    pub id: i64,
    pub role: String,
    pub content_json: String,
    #[serde(default)]
    pub token_count: Option<u32>,
    pub created_at: String,
}

/// A persisted oversized tool result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoreToolResult {
    pub id: String,
    pub session_id: String,
    #[serde(default)]
    pub turn_id: Option<String>,
    pub tool_call_id: String,
    pub tool_name: String,
    pub file_path: String,
    pub output_size_chars: usize,
    pub output_size_bytes: usize,
    /// Unix timestamp (seconds).
    pub created_at: i64,
}

/// Which session fields a `session/update` changes. Absent = untouched.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoreSessionPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Outer `None` leaves the field alone; `Some(None)` writes `null`.
    #[serde(
        default,
        deserialize_with = "double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub fallback_model: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
}

fn double_option<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

/// An event kept by an event store.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoreEvent {
    pub seq: u64,
    pub event: Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn missing_section_yields_defaults() {
        assert_eq!(
            HostSessionMeta::from_meta(None).unwrap(),
            HostSessionMeta::default()
        );
        assert_eq!(
            HostSessionMeta::from_meta(Some(&json!({"other": 1}))).unwrap(),
            HostSessionMeta::default()
        );
        assert_eq!(
            HostSessionMeta::from_meta(Some(&json!({"kkagent/host": null}))).unwrap(),
            HostSessionMeta::default()
        );
    }

    #[test]
    fn parses_full_session_meta() {
        let meta = json!({"kkagent/host": {
            "role": "supervisor",
            "toolAllowlist": ["Read", "task_*"],
            "hostTools": [{
                "name": "task_create",
                "description": "Create a task",
                "inputSchema": {"type": "object", "properties": {"title": {"type": "string"}}},
                "readOnly": false,
                "approval": "ask"
            }, {"name": "task_list"}],
            "extensions": {"loadHooks": false},
            "permissionMode": "manual"
        }});
        let parsed = HostSessionMeta::from_meta(Some(&meta)).unwrap();
        assert_eq!(parsed.role.as_deref(), Some("supervisor"));
        assert_eq!(parsed.tool_allowlist.as_ref().unwrap().len(), 2);
        assert_eq!(parsed.host_tools.len(), 2);
        assert_eq!(parsed.host_tools[0].approval, HostToolApproval::Ask);
        assert_eq!(parsed.host_tools[1].approval, HostToolApproval::Host);
        assert_eq!(
            parsed.host_tools[1].input_schema,
            json!({"type": "object", "properties": {}})
        );
        assert_eq!(
            parsed.extensions,
            Some(ExtensionOverrides {
                load_hooks: Some(false),
                ..Default::default()
            })
        );
        assert_eq!(parsed.permission_mode.as_deref(), Some("manual"));
    }

    #[test]
    fn replay_flag_is_a_known_session_field() {
        let parse = |value: serde_json::Value| {
            HostSessionMeta::from_meta(Some(&json!({"kkagent/host": value})))
        };
        assert_eq!(parse(json!({})).unwrap().replay, None);
        assert_eq!(parse(json!({"replay": false})).unwrap().replay, Some(false));
        assert_eq!(parse(json!({"replay": true})).unwrap().replay, Some(true));
        // A typo is rejected like every other unknown field.
        assert!(parse(json!({"replays": false})).is_err());
        assert!(parse(json!({"replay": "no"})).is_err());
    }

    #[test]
    fn malformed_section_is_an_error_not_a_silent_default() {
        let meta = json!({"kkagent/host": {"toolAllowlist": "Read"}});
        assert!(HostSessionMeta::from_meta(Some(&meta)).is_err());
    }

    #[test]
    fn unknown_fields_are_rejected_at_every_level() {
        // A misspelled restriction must not be silently dropped.
        for meta in [
            json!({"kkagent/host": {"toolAllowlst": ["Read"]}}),
            json!({"kkagent/host": {"extensions": {"loadHook": false}}}),
            json!({"kkagent/host": {"hostTools": [{"name": "a", "readOnlyy": true}]}}),
            json!({"kkagent/host": {"toolReview": {"mod": "all"}}}),
        ] {
            let error = HostSessionMeta::from_meta(Some(&meta)).unwrap_err();
            assert!(error.contains("unknown field"), "{error}");
        }
    }

    #[test]
    fn tool_review_parses_and_is_bounded() {
        let meta = json!({"kkagent/host": {"toolReview": {"mode": "mutating", "timeoutSecs": 20}}});
        let review = HostSessionMeta::from_meta(Some(&meta))
            .unwrap()
            .tool_review
            .unwrap();
        assert_eq!(review.mode, ToolReviewMode::Mutating);
        assert_eq!(review.timeout_secs(), 20);
        assert_eq!(
            ToolReviewSpec::default().timeout_secs(),
            DEFAULT_TOOL_REVIEW_TIMEOUT_SECS
        );
        for bad in [0u64, MAX_TOOL_REVIEW_TIMEOUT_SECS + 1] {
            let meta = json!({"kkagent/host": {"toolReview": {"mode": "all", "timeoutSecs": bad}}});
            assert!(HostSessionMeta::from_meta(Some(&meta))
                .unwrap_err()
                .contains("timeoutSecs"));
        }
        let unknown_mode = json!({"kkagent/host": {"toolReview": {"mode": "sometimes"}}});
        assert!(HostSessionMeta::from_meta(Some(&unknown_mode)).is_err());
    }

    #[test]
    fn review_modes_select_calls_and_only_tighten() {
        assert!(!ToolReviewMode::Off.reviews(false));
        assert!(ToolReviewMode::Mutating.reviews(false));
        assert!(!ToolReviewMode::Mutating.reviews(true));
        assert!(ToolReviewMode::All.reviews(true));
        assert_eq!(
            ToolReviewMode::Mutating.strictest(ToolReviewMode::Off),
            ToolReviewMode::Mutating
        );
        assert_eq!(
            ToolReviewMode::Mutating.strictest(ToolReviewMode::All),
            ToolReviewMode::All
        );
        assert_eq!(
            ToolReviewMode::All.strictest(ToolReviewMode::Mutating),
            ToolReviewMode::All
        );
    }

    #[test]
    fn review_reply_wire_shape() {
        let reply: ToolReviewReply =
            serde_json::from_value(json!({"decision": "deny", "reason": "no"})).unwrap();
        assert_eq!(reply.decision, ToolReviewDecision::Deny);
        assert_eq!(reply.reason.as_deref(), Some("no"));
        let allow: ToolReviewReply = serde_json::from_value(json!({"decision": "allow"})).unwrap();
        assert_eq!(allow.decision, ToolReviewDecision::Allow);
        assert!(serde_json::from_value::<ToolReviewReply>(json!({"decision": "maybe"})).is_err());
        assert!(serde_json::from_value::<ToolReviewReply>(json!({})).is_err());
    }

    #[test]
    fn rejects_bad_and_duplicate_host_tools() {
        let bad = json!({"kkagent/host": {"hostTools": [{"name": "has space"}]}});
        assert!(HostSessionMeta::from_meta(Some(&bad)).is_err());
        let dup = json!({"kkagent/host": {"hostTools": [{"name": "a"}, {"name": "a"}]}});
        assert!(HostSessionMeta::from_meta(Some(&dup))
            .unwrap_err()
            .contains("duplicate"));
        let schema = json!({"kkagent/host": {"hostTools": [{"name": "a", "inputSchema": 1}]}});
        assert!(HostSessionMeta::from_meta(Some(&schema)).is_err());
    }

    #[test]
    fn host_tool_timeout_is_bounded() {
        let ok = json!({"kkagent/host": {"hostTools": [{"name": "a", "timeoutSecs": 30}]}});
        assert_eq!(
            HostSessionMeta::from_meta(Some(&ok)).unwrap().host_tools[0].timeout_secs,
            Some(30)
        );
        for bad in [0u64, MAX_HOST_TOOL_TIMEOUT_SECS + 1] {
            let meta = json!({"kkagent/host": {"hostTools": [{"name": "a", "timeoutSecs": bad}]}});
            assert!(HostSessionMeta::from_meta(Some(&meta))
                .unwrap_err()
                .contains("timeoutSecs"));
        }
    }

    #[test]
    fn patch_distinguishes_untouched_null_and_value_fallback_model() {
        let untouched: StoreSessionPatch = serde_json::from_value(json!({"title": "t"})).unwrap();
        assert_eq!(untouched.fallback_model, None);
        let cleared: StoreSessionPatch =
            serde_json::from_value(json!({"fallbackModel": null})).unwrap();
        assert_eq!(cleared.fallback_model, Some(None));
        let set: StoreSessionPatch =
            serde_json::from_value(json!({"fallbackModel": "a/b"})).unwrap();
        assert_eq!(set.fallback_model, Some(Some("a/b".into())));
        // Round trip keeps the three states apart.
        assert_eq!(
            serde_json::to_value(&untouched).unwrap(),
            json!({"title": "t"})
        );
        assert_eq!(
            serde_json::to_value(&cleared).unwrap(),
            json!({"fallbackModel": null})
        );
    }

    #[test]
    fn store_session_uses_camel_case_on_the_wire() {
        let session = StoreSession {
            session_id: "s".into(),
            title: None,
            model: "m".into(),
            fallback_model: None,
            working_dir: "/w".into(),
            created_at: "t0".into(),
            updated_at: "t1".into(),
            message_count: 2,
            is_archived: false,
            parent_session_id: None,
        };
        let value = serde_json::to_value(&session).unwrap();
        assert_eq!(value["sessionId"], "s");
        assert_eq!(value["workingDir"], "/w");
        assert_eq!(
            serde_json::from_value::<StoreSession>(value).unwrap(),
            session
        );
    }

    #[test]
    fn capabilities_read_from_initialize_meta() {
        let meta = json!({"kkagent/host": {
            "tools": true, "toolReview": true, "store": {"sessions": true}
        }});
        let caps = HostCapabilities::from_meta(Some(&meta));
        assert!(caps.tools);
        assert!(caps.tool_review);
        assert!(caps.store.sessions);
        assert!(!caps.store.events);
        assert_eq!(
            HostCapabilities::from_meta(None),
            HostCapabilities::default()
        );
    }
}
