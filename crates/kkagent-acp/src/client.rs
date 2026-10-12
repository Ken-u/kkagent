//! ACP **client** — drives an external agent (e.g. Cursor CLI `agent acp`)
//! over stdio newline-delimited JSON-RPC 2.0.
//!
//! This is the inverse of the ACP server in `lib.rs` (where kkagent *is* the
//! agent behind an editor). Here kkagent acts as the ACP *client* and spawns
//! the external agent as a child process.
//!
//! Handshake: `initialize` → (`authenticate`, skipped when the child inherits
//! credentials from the environment) → `session/new` → `session/prompt`.
//! Streaming progress arrives as `session/update` notifications; the prompt
//! request resolves with a `stopReason`.
//!
//! Reverse requests (agent → client) are answered in one of two ways:
//!
//! - **Legacy / unattended** (no [`AcpRequestHandler`]): permission requests
//!   are answered from a per-connection [`PermissionPolicy`] so external
//!   subagents never deadlock on approval; unknown requests get
//!   `-32601` (which makes Cursor fall back to its own non-interactive path).
//! - **Interactive** ([`AcpClientOptions::handler`] set): permission requests,
//!   `cursor/ask_question` and `cursor/create_plan` are handed to the host in
//!   spawned tasks, so the reader loop never blocks while a human decides.
//!
//! `cursor/update_todos` is accepted in both modes, as a request *or* a
//! notification (the published docs call it a notification, Cursor sends it
//! as a request with an id).
//!
//! Cursor treats a second `session/prompt` on a running session as "cancel the
//! running turn and start a new one", so this client refuses to send a prompt
//! while another is in flight on the same connection.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::task::JoinHandle;

/// Default timeout for a single ACP request (handshake, session setup, and
/// the whole `session/prompt` of the legacy unattended path).
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

/// Largest tool output / diff text kept per tool call (chars).
const MAX_TOOL_OUTPUT_CHARS: usize = 16 * 1024;
/// Stderr lines kept to explain an unexpected exit.
const STDERR_TAIL_LINES: usize = 40;

/// Behavior for `session/request_permission` reverse requests when no
/// [`AcpRequestHandler`] is installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionPolicy {
    /// Auto-approve every tool invocation (default for external subagents).
    AutoApprove,
    /// Deny every tool invocation; the external agent must degrade gracefully.
    Deny,
}

impl PermissionPolicy {
    /// Build the response payload. ACP permission options carry string
    /// `optionId`s; agents order them most-permissive first, so auto-approve
    /// selects the first option.
    fn response(self, params: &Value) -> Value {
        match self {
            PermissionPolicy::AutoApprove => {
                let first_option = params
                    .get("options")
                    .and_then(Value::as_array)
                    .and_then(|opts| opts.first())
                    .and_then(|opt| opt.get("optionId"))
                    .cloned()
                    .unwrap_or_else(|| json!(0));
                json!({ "outcome": { "outcome": "selected", "optionId": first_option } })
            }
            PermissionPolicy::Deny => {
                json!({ "outcome": { "outcome": "cancelled" } })
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Interactive reverse requests
// ---------------------------------------------------------------------------

/// One selectable answer of a `session/request_permission`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpPermissionOption {
    pub option_id: String,
    pub name: String,
    /// `allow_once` | `allow_always` | `reject_once` | `reject_always`.
    pub kind: String,
}

/// What the host wants to answer a permission request with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcpPermissionChoice {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
}

impl AcpPermissionChoice {
    fn kinds(self) -> &'static [&'static str] {
        match self {
            Self::AllowOnce => &["allow_once"],
            Self::AllowAlways => &["allow_always"],
            Self::RejectOnce => &["reject_once"],
            Self::RejectAlways => &["reject_always"],
        }
    }

    /// Option ids Cursor documents (`allow-once` …) for agents that omit `kind`.
    fn ids(self) -> &'static [&'static str] {
        match self {
            Self::AllowOnce => &["allow_once", "allow"],
            Self::AllowAlways => &["allow_always"],
            Self::RejectOnce => &["reject_once", "reject", "deny"],
            Self::RejectAlways => &["reject_always"],
        }
    }
}

fn normalize_token(raw: &str) -> String {
    raw.trim().to_ascii_lowercase().replace('-', "_")
}

/// Decoded `session/request_permission`.
#[derive(Debug, Clone, Default)]
pub struct AcpPermissionRequest {
    pub session_id: String,
    pub tool_call_id: String,
    pub title: String,
    pub tool_kind: Option<String>,
    pub raw_input: Option<Value>,
    pub locations: Vec<String>,
    /// Text preview carried by the tool call (command output, diff…).
    pub preview: Option<String>,
    pub options: Vec<AcpPermissionOption>,
}

impl AcpPermissionRequest {
    pub(crate) fn from_params(params: &Value) -> Self {
        let tool_call = params.get("toolCall").unwrap_or(&Value::Null);
        let options = params
            .get("options")
            .and_then(Value::as_array)
            .map(|opts| {
                opts.iter()
                    .filter_map(|opt| {
                        let option_id = value_to_id(opt.get("optionId")?)?;
                        Some(AcpPermissionOption {
                            name: opt
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or(&option_id)
                                .to_string(),
                            kind: opt
                                .get("kind")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            option_id,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self {
            session_id: str_field(params, "sessionId"),
            tool_call_id: str_field(tool_call, "toolCallId"),
            title: str_field(tool_call, "title"),
            tool_kind: opt_str_field(tool_call, "kind"),
            raw_input: tool_call.get("rawInput").filter(|v| !v.is_null()).cloned(),
            locations: parse_locations(tool_call.get("locations")),
            preview: tool_output_text(tool_call),
            options,
        }
    }

    /// Best matching option for `choice`, matched by `kind` first and by the
    /// documented option ids second. Falls back from `AllowAlways` to
    /// `AllowOnce` (and `RejectAlways` to `RejectOnce`) when the agent does
    /// not offer the persistent variant.
    pub fn option_for(&self, choice: AcpPermissionChoice) -> Option<&AcpPermissionOption> {
        let by_kind = self
            .options
            .iter()
            .find(|o| choice.kinds().contains(&normalize_token(&o.kind).as_str()));
        let by_id = || {
            self.options.iter().find(|o| {
                choice
                    .ids()
                    .contains(&normalize_token(&o.option_id).as_str())
            })
        };
        by_kind.or_else(by_id).or_else(|| match choice {
            AcpPermissionChoice::AllowAlways => self.option_for(AcpPermissionChoice::AllowOnce),
            AcpPermissionChoice::RejectAlways => self.option_for(AcpPermissionChoice::RejectOnce),
            _ => None,
        })
    }
}

/// Answer to a permission request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcpPermissionOutcome {
    Selected(String),
    Cancelled,
}

impl AcpPermissionOutcome {
    fn to_result(&self) -> Value {
        match self {
            Self::Selected(option_id) => {
                json!({ "outcome": { "outcome": "selected", "optionId": option_id } })
            }
            Self::Cancelled => json!({ "outcome": { "outcome": "cancelled" } }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpAskOption {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpAskQuestion {
    pub id: String,
    pub prompt: String,
    pub options: Vec<AcpAskOption>,
    pub allow_multiple: bool,
}

/// Decoded `cursor/ask_question`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AcpAskQuestionRequest {
    pub tool_call_id: String,
    pub title: Option<String>,
    pub questions: Vec<AcpAskQuestion>,
}

impl AcpAskQuestionRequest {
    pub(crate) fn from_params(params: &Value) -> Self {
        let questions = params
            .get("questions")
            .and_then(Value::as_array)
            .map(|qs| {
                qs.iter()
                    .enumerate()
                    .map(|(idx, q)| AcpAskQuestion {
                        id: opt_str_field(q, "id").unwrap_or_else(|| format!("q{}", idx + 1)),
                        prompt: str_field(q, "prompt"),
                        options: q
                            .get("options")
                            .and_then(Value::as_array)
                            .map(|opts| {
                                opts.iter()
                                    .filter_map(|o| {
                                        let id = value_to_id(o.get("id")?)?;
                                        Some(AcpAskOption {
                                            label: opt_str_field(o, "label")
                                                .unwrap_or_else(|| id.clone()),
                                            id,
                                        })
                                    })
                                    .collect()
                            })
                            .unwrap_or_default(),
                        allow_multiple: q
                            .get("allowMultiple")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self {
            tool_call_id: str_field(params, "toolCallId"),
            title: opt_str_field(params, "title"),
            questions,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpQuestionAnswer {
    pub question_id: String,
    pub selected_option_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcpAskQuestionOutcome {
    Answered(Vec<AcpQuestionAnswer>),
    Skipped(Option<String>),
    Cancelled,
}

impl AcpAskQuestionOutcome {
    fn to_result(&self) -> Value {
        match self {
            Self::Answered(answers) => json!({
                "outcome": {
                    "outcome": "answered",
                    "answers": answers.iter().map(|a| json!({
                        "questionId": a.question_id,
                        "selectedOptionIds": a.selected_option_ids,
                    })).collect::<Vec<_>>(),
                }
            }),
            Self::Skipped(reason) => {
                let mut outcome = json!({ "outcome": "skipped" });
                if let Some(reason) = reason {
                    outcome["reason"] = json!(reason);
                }
                json!({ "outcome": outcome })
            }
            Self::Cancelled => json!({ "outcome": { "outcome": "cancelled" } }),
        }
    }
}

/// One todo of `cursor/update_todos` / `cursor/create_plan`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcpPlanTodo {
    pub id: String,
    pub content: String,
    /// `pending` | `in_progress` | `completed` | `cancelled`.
    pub status: String,
}

fn parse_plan_todos(value: Option<&Value>) -> Vec<AcpPlanTodo> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .enumerate()
                .map(|(idx, item)| AcpPlanTodo {
                    id: item
                        .get("id")
                        .and_then(value_to_id)
                        .unwrap_or_else(|| format!("todo-{}", idx + 1)),
                    content: str_field(item, "content"),
                    status: opt_str_field(item, "status").unwrap_or_else(|| "pending".into()),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Decoded `cursor/create_plan`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AcpCreatePlanRequest {
    pub tool_call_id: String,
    pub name: Option<String>,
    pub overview: Option<String>,
    /// Markdown body of the plan.
    pub plan: String,
    pub todos: Vec<AcpPlanTodo>,
    pub is_project: bool,
}

impl AcpCreatePlanRequest {
    pub(crate) fn from_params(params: &Value) -> Self {
        Self {
            tool_call_id: str_field(params, "toolCallId"),
            name: opt_str_field(params, "name"),
            overview: opt_str_field(params, "overview"),
            plan: str_field(params, "plan"),
            todos: parse_plan_todos(params.get("todos")),
            is_project: params
                .get("isProject")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcpCreatePlanOutcome {
    Accepted { plan_uri: Option<String> },
    Rejected { reason: Option<String> },
    Cancelled,
}

impl AcpCreatePlanOutcome {
    fn to_result(&self) -> Value {
        match self {
            Self::Accepted { plan_uri } => {
                let mut outcome = json!({ "outcome": "accepted" });
                if let Some(uri) = plan_uri {
                    outcome["planUri"] = json!(uri);
                }
                json!({ "outcome": outcome })
            }
            Self::Rejected { reason } => {
                let mut outcome = json!({ "outcome": "rejected" });
                if let Some(reason) = reason {
                    outcome["reason"] = json!(reason);
                }
                json!({ "outcome": outcome })
            }
            Self::Cancelled => json!({ "outcome": { "outcome": "cancelled" } }),
        }
    }
}

/// Host side of the interactive reverse requests. Every method may take as
/// long as a human needs; the client calls them from spawned tasks and the
/// agent stays blocked meanwhile. Implementations must answer `Cancelled`
/// when the turn is interrupted or nobody is listening.
#[async_trait]
pub trait AcpRequestHandler: Send + Sync + 'static {
    async fn request_permission(&self, request: AcpPermissionRequest) -> AcpPermissionOutcome;
    async fn ask_question(&self, request: AcpAskQuestionRequest) -> AcpAskQuestionOutcome;
    async fn create_plan(&self, request: AcpCreatePlanRequest) -> AcpCreatePlanOutcome;
}

// ---------------------------------------------------------------------------
// Progress
// ---------------------------------------------------------------------------

/// Streamed progress from a prompt turn, translated from `session/update`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExternalProgress {
    /// Incremental agent text output.
    Text { delta: String },
    /// Incremental reasoning text (`agent_thought_chunk`).
    Thought { delta: String },
    /// A tool call started (or progressed) inside the external agent. The
    /// client merges successive `tool_call_update`s, so every event carries
    /// the latest known state of the call.
    ToolCall {
        tool_call_id: String,
        title: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_kind: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
        /// `rawInput` of the call (may be `{}` until the agent fills it in).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        raw_input: Option<Value>,
        /// Text rendering of `rawOutput` / `content`, bounded.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<String>,
        /// Files the call touches (`locations[].path`).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        locations: Vec<String>,
    },
    /// A plan / todo list update (Cursor's `plan` update variant).
    Plan {
        #[serde(default)]
        entries: Vec<Value>,
    },
    /// The agent's todo list after applying a `cursor/update_todos`.
    Todos { todos: Vec<AcpPlanTodo> },
    /// The agent switched mode (`current_mode_update`).
    ModeChanged { mode_id: String },
    /// Unrecognized or vendor-specific update, kept for observability.
    Unknown { update: Value },
}

/// Final result of a completed prompt turn.
#[derive(Debug, Clone)]
pub struct PromptOutcome {
    /// `stopReason` from the `session/prompt` response.
    pub stop_reason: String,
    /// Final agent text (accumulated from streamed chunks).
    pub text: String,
}

/// Options for spawning an external ACP agent.
#[derive(Clone)]
pub struct AcpClientOptions {
    /// Executable and arguments, e.g. `["agent", "acp"]` (Cursor CLI) or
    /// `["cursor-agent", "acp"]`.
    pub command: Vec<String>,
    /// Working directory for the child process.
    pub cwd: PathBuf,
    /// Extra environment variables for the child (e.g. `CURSOR_API_KEY`).
    pub env: HashMap<String, String>,
    /// Per-request timeout. Defaults to [`DEFAULT_REQUEST_TIMEOUT`].
    pub request_timeout: Option<Duration>,
    /// Reverse-request permission policy used when no `handler` is set.
    /// Defaults to [`PermissionPolicy::AutoApprove`].
    pub permission: Option<PermissionPolicy>,
    /// Interactive reverse-request handler. When set, permission requests,
    /// `cursor/ask_question` and `cursor/create_plan` go to the host.
    pub handler: Option<Arc<dyn AcpRequestHandler>>,
    /// Declare `_meta.parameterizedModelPicker` in `initialize`. Cursor then
    /// reports the model and each of its parameters (thinking level, context
    /// size, …) as separate session config options, so the thinking level can
    /// be changed on its own; without it Cursor only offers one fixed variant
    /// per model (`claude-sonnet-5-5[context=300k,reasoning_effort=high]`).
    /// Other agents ignore the flag.
    pub parameterized_model_picker: bool,
}

impl std::fmt::Debug for AcpClientOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcpClientOptions")
            .field("command", &self.command)
            .field("cwd", &self.cwd)
            .field("env_keys", &self.env.keys().collect::<Vec<_>>())
            .field("request_timeout", &self.request_timeout)
            .field("permission", &self.permission)
            .field("handler", &self.handler.is_some())
            .field(
                "parameterized_model_picker",
                &self.parameterized_model_picker,
            )
            .finish()
    }
}

impl AcpClientOptions {
    /// Build options for the Cursor CLI ACP mode.
    pub fn cursor_cli(cwd: PathBuf) -> Self {
        Self {
            command: vec!["agent".into(), "acp".into()],
            cwd,
            env: HashMap::new(),
            request_timeout: None,
            permission: None,
            handler: None,
            parameterized_model_picker: false,
        }
    }
}

/// `initialize` result payload (subset we rely on).
#[derive(Debug, Clone, Deserialize)]
pub struct InitializeResult {
    #[serde(default, rename = "protocolVersion")]
    pub protocol_version: Option<Value>,
    #[serde(default, rename = "agentCapabilities")]
    pub agent_capabilities: Option<Value>,
    #[serde(default, rename = "authMethods")]
    pub auth_methods: Vec<Value>,
}

impl InitializeResult {
    /// True when the agent advertises at least one auth method. Callers that
    /// injected credentials via the environment may still skip `authenticate`
    /// (the CLI accepts pre-seeded `CURSOR_API_KEY`/`CURSOR_AUTH_TOKEN`).
    pub fn requires_auth(&self) -> bool {
        !self.auth_methods.is_empty()
    }

    /// True when the agent can restore a session with `session/load`.
    pub fn supports_load_session(&self) -> bool {
        self.agent_capabilities
            .as_ref()
            .and_then(|caps| caps.get("loadSession"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }
}

/// `session/new` result (subset).
#[derive(Debug, Clone)]
pub struct AcpSessionInfo {
    pub session_id: String,
    /// `modes.currentModeId`.
    pub current_mode: Option<String>,
    /// `modes.availableModes[].id`.
    pub available_modes: Vec<String>,
    /// Raw `models` block (shape differs between agents).
    pub models: Option<Value>,
    /// Session config options the agent offers (`configOptions`): mode,
    /// model, thinking level, …
    pub config_options: Vec<AcpConfigOption>,
}

impl AcpSessionInfo {
    /// The model the session runs on, parameters stripped: the `model` config
    /// option's value, else `models.currentModelId`.
    pub fn current_model(&self) -> Option<String> {
        current_model_of(&self.config_options, self.models.as_ref())
    }

    /// The model id exactly as the agent reported it, variant suffix
    /// included (`gpt-6.1-sol[high]`).
    pub fn current_model_raw(&self) -> Option<String> {
        current_model_raw_of(&self.config_options, self.models.as_ref())
    }
}

/// One value of a select-type [`AcpConfigOption`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpConfigValue {
    pub value: String,
    pub name: String,
}

/// A session config option of an ACP agent (`configOptions[]` of
/// `session/new`): a named select with a current value. `category` is the
/// standard ACP classification (`mode`, `model`, `thought_level`, …); ids
/// are agent-specific (Cursor names its thinking level `reasoning_effort`
/// for one model and `effort` for another), so look options up by category.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpConfigOption {
    pub id: String,
    pub name: String,
    pub category: Option<String>,
    /// `currentValue` of a select option.
    pub current: Option<String>,
    pub values: Vec<AcpConfigValue>,
}

impl AcpConfigOption {
    /// The value ids, in the agent's order.
    pub fn value_ids(&self) -> Vec<String> {
        self.values.iter().map(|v| v.value.clone()).collect()
    }
}

/// The option of `category` (`thought_level`, `model`, …), if the agent
/// offers one.
pub fn config_option_by_category<'a>(
    options: &'a [AcpConfigOption],
    category: &str,
) -> Option<&'a AcpConfigOption> {
    options
        .iter()
        .find(|option| option.category.as_deref() == Some(category))
}

/// Model id of a session as the agent reports it: the `model` option's
/// current value, else `models.currentModelId` (variant suffix included).
pub fn current_model_raw_of(options: &[AcpConfigOption], models: Option<&Value>) -> Option<String> {
    config_option_by_category(options, "model")
        .and_then(|option| option.current.clone())
        .or_else(|| {
            models
                .and_then(|models| models.get("currentModelId"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
}

/// [`current_model_raw_of`] with a `[param=…]` variant suffix cut off.
pub fn current_model_of(options: &[AcpConfigOption], models: Option<&Value>) -> Option<String> {
    current_model_raw_of(options, models).and_then(|raw| base_model_id(&raw))
}

/// A model id without its variant suffix (`claude-sonnet-5-5[context=300k]`
/// → `claude-sonnet-5-5`); `None` when nothing is left.
pub fn base_model_id(raw: &str) -> Option<String> {
    let base = raw.split_once('[').map_or(raw, |(base, _)| base).trim();
    (!base.is_empty()).then(|| base.to_string())
}

/// The bare level of a variant id (`gpt-6.1-sol[high]` → `high`). `None`
/// for ids without a variant and for parameter lists (`[a=b,c=d]`).
pub fn variant_level_of(raw: &str) -> Option<String> {
    let (_, rest) = raw.split_once('[')?;
    let level = rest.strip_suffix(']')?.trim();
    (!level.is_empty() && !level.contains(['=', ','])).then(|| level.to_string())
}

/// Parse the `configOptions` array of a `session/new` or
/// `session/set_config_option` result. Options that are not selects (no
/// `options`) keep an empty value list.
pub fn parse_config_options(value: Option<&Value>) -> Vec<AcpConfigOption> {
    let Some(items) = value.and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let id = item.get("id").and_then(Value::as_str)?.to_string();
            let text = |key: &str| item.get(key).and_then(Value::as_str).map(str::to_string);
            let mut values = Vec::new();
            for entry in item
                .get("options")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                // Flat `{value, name}` entries, or groups `{group, options}`.
                let grouped = entry.get("options").and_then(Value::as_array);
                for leaf in grouped
                    .map(Vec::as_slice)
                    .unwrap_or(std::slice::from_ref(entry))
                {
                    let Some(value) = leaf.get("value").and_then(Value::as_str) else {
                        continue;
                    };
                    values.push(AcpConfigValue {
                        value: value.to_string(),
                        name: leaf
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or(value)
                            .to_string(),
                    });
                }
            }
            Some(AcpConfigOption {
                name: text("name").unwrap_or_else(|| id.clone()),
                category: text("category"),
                current: item
                    .get("currentValue")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                id,
                values,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Connection internals
// ---------------------------------------------------------------------------

type Writer = Box<dyn AsyncWrite + Send + Unpin>;
type ProgressTx = mpsc::Sender<(String, ExternalProgress)>;
type PendingMap = HashMap<i64, oneshot::Sender<Result<Value, Value>>>;

/// Latest known state of one ACP tool call, merged from the original
/// `tool_call` and every `tool_call_update`.
#[derive(Default)]
struct ToolState {
    title: String,
    tool_kind: Option<String>,
    status: Option<String>,
    raw_input: Option<Value>,
    output: Option<String>,
    locations: Vec<String>,
}

/// How long a prompt may take.
#[derive(Clone, Copy)]
enum PromptLimit {
    /// Whole request (legacy unattended subagents).
    Total(Duration),
    /// No agent activity for this long; time spent waiting for a human does
    /// not count.
    Idle(Duration),
}

/// Shared connection state mutated by the reader loop and the handler tasks.
/// Sync mutexes only guard short critical sections that never span an await.
struct Conn {
    writer: Mutex<Option<Writer>>,
    pending: StdMutex<PendingMap>,
    /// Progress sink for the in-flight prompt: (session id, sender).
    progress_sink: StdMutex<Option<(String, ProgressTx)>>,
    /// Accumulated agent text for the active prompt turn.
    text_acc: StdMutex<String>,
    tools: StdMutex<HashMap<String, ToolState>>,
    todos: StdMutex<Vec<AcpPlanTodo>>,
    permission: PermissionPolicy,
    handler: Option<Arc<dyn AcpRequestHandler>>,
    next_id: AtomicI64,
    prompt_active: AtomicBool,
    closed: AtomicBool,
    epoch: Instant,
    last_activity_ms: AtomicU64,
    /// Reverse requests currently waiting on the host (a human).
    user_blocked: AtomicUsize,
    stderr_tail: StdMutex<VecDeque<String>>,
}

fn lock<T>(mutex: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

impl Conn {
    fn new(opts: &AcpClientOptions) -> Self {
        Self {
            writer: Mutex::new(None),
            pending: StdMutex::new(HashMap::new()),
            progress_sink: StdMutex::new(None),
            text_acc: StdMutex::new(String::new()),
            tools: StdMutex::new(HashMap::new()),
            todos: StdMutex::new(Vec::new()),
            permission: opts.permission.unwrap_or(PermissionPolicy::AutoApprove),
            handler: opts.handler.clone(),
            next_id: AtomicI64::new(1),
            prompt_active: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            epoch: Instant::now(),
            last_activity_ms: AtomicU64::new(0),
            user_blocked: AtomicUsize::new(0),
            stderr_tail: StdMutex::new(VecDeque::new()),
        }
    }

    async fn write_frame(&self, frame: &Value) -> anyhow::Result<()> {
        let mut line = serde_json::to_string(frame)?;
        line.push('\n');
        let mut guard = self.writer.lock().await;
        let writer = guard
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("external agent stdin closed"))?;
        writer.write_all(line.as_bytes()).await?;
        writer.flush().await?;
        Ok(())
    }

    fn touch(&self) {
        let now = self.epoch.elapsed().as_millis() as u64;
        self.last_activity_ms.store(now, Ordering::Relaxed);
    }

    fn idle_for(&self) -> Duration {
        let now = self.epoch.elapsed().as_millis() as u64;
        Duration::from_millis(now.saturating_sub(self.last_activity_ms.load(Ordering::Relaxed)))
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// The agent is gone (EOF) or we shut it down: fail everything waiting.
    fn mark_closed(&self) {
        self.closed.store(true, Ordering::SeqCst);
        lock(&self.pending).clear();
        lock(&self.progress_sink).take();
    }

    fn reset_turn_state(&self) {
        lock(&self.text_acc).clear();
        lock(&self.tools).clear();
    }

    fn exit_description(&self) -> String {
        let tail: Vec<String> = lock(&self.stderr_tail).iter().cloned().collect();
        if tail.is_empty() {
            "the external agent process ended unexpectedly".to_string()
        } else {
            format!(
                "the external agent process ended unexpectedly; last stderr:\n{}",
                tail.join("\n")
            )
        }
    }

    /// Apply a `cursor/update_todos` payload and return the resulting list.
    fn apply_todos(&self, params: &Value) -> Vec<AcpPlanTodo> {
        let incoming = parse_plan_todos(params.get("todos"));
        let merge = params
            .get("merge")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mut list = lock(&self.todos);
        if merge {
            for todo in incoming {
                match list.iter_mut().find(|existing| existing.id == todo.id) {
                    Some(existing) => *existing = todo,
                    None => list.push(todo),
                }
            }
        } else {
            *list = incoming;
        }
        list.clone()
    }

    /// Fold one translated update into the connection state and return the
    /// event to forward (tool calls become cumulative).
    fn absorb(&self, progress: ExternalProgress) -> ExternalProgress {
        match progress {
            ExternalProgress::Text { delta } => {
                lock(&self.text_acc).push_str(&delta);
                ExternalProgress::Text { delta }
            }
            ExternalProgress::ToolCall {
                tool_call_id,
                title,
                tool_kind,
                status,
                raw_input,
                output,
                locations,
            } => {
                let mut tools = lock(&self.tools);
                let state = tools.entry(tool_call_id.clone()).or_default();
                if !title.trim().is_empty() {
                    state.title = title;
                }
                if tool_kind.is_some() {
                    state.tool_kind = tool_kind;
                }
                if status.is_some() {
                    state.status = status;
                }
                if raw_input.is_some() {
                    state.raw_input = raw_input;
                }
                if output.is_some() {
                    state.output = output;
                }
                if !locations.is_empty() {
                    state.locations = locations;
                }
                ExternalProgress::ToolCall {
                    tool_call_id,
                    title: state.title.clone(),
                    tool_kind: state.tool_kind.clone(),
                    status: state.status.clone(),
                    raw_input: state.raw_input.clone(),
                    output: state.output.clone(),
                    locations: state.locations.clone(),
                }
            }
            other => other,
        }
    }

    /// Forward an event to the in-flight prompt. Events without a session id
    /// (`cursor/update_todos` carries none) belong to the prompt's session.
    async fn emit(&self, session: String, progress: ExternalProgress) {
        let sink = lock(&self.progress_sink).clone();
        if let Some((sink_session, tx)) = sink {
            if session.is_empty() {
                let _ = tx.send((sink_session, progress)).await;
            } else if sink_session.is_empty() || sink_session == session {
                let _ = tx.send((session, progress)).await;
            }
        }
    }
}

/// Removes a pending request entry when the waiting future is dropped.
struct PendingGuard {
    conn: Arc<Conn>,
    id: i64,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        lock(&self.conn.pending).remove(&self.id);
    }
}

/// Clears the progress sink and the single-prompt flag on every exit path.
struct PromptGuard {
    conn: Arc<Conn>,
}

impl Drop for PromptGuard {
    fn drop(&mut self) {
        lock(&self.conn.progress_sink).take();
        self.conn.prompt_active.store(false, Ordering::SeqCst);
    }
}

/// A running connection to one external ACP agent process.
pub struct AcpClient {
    conn: Arc<Conn>,
    request_timeout: Duration,
    child: Mutex<Option<Child>>,
    pid: Option<u32>,
    /// Session id created during [`AcpClient::start_session`].
    session_id: StdMutex<Option<String>>,
    initialize: StdMutex<Option<InitializeResult>>,
    reader_task: StdMutex<Option<JoinHandle<()>>>,
    shut_down: AtomicBool,
    /// Declared to the agent in `initialize` ([`AcpClientOptions`]).
    parameterized_model_picker: bool,
}

impl AcpClient {
    /// Spawn the external agent process and perform the `initialize`
    /// handshake. Does **not** authenticate or create a session yet.
    pub async fn spawn(opts: AcpClientOptions) -> anyhow::Result<Arc<Self>> {
        let (command, args) = match opts.command.split_first() {
            Some((cmd, args)) => (cmd.clone(), args.to_vec()),
            None => anyhow::bail!("external subagent command is empty"),
        };
        let program = resolve_program(&command, &opts);
        let mut cmd = Command::new(&program);
        cmd.args(&args)
            .current_dir(&opts.cwd)
            .envs(&opts.env)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        {
            cmd.process_group(0);
        }

        let mut child = cmd.spawn().map_err(|e| {
            anyhow::anyhow!(
                "failed to spawn external subagent `{command}`: {e}; \
                 is it on PATH and authenticated (`{command} login`)?"
            )
        })?;
        let pid = child.id();

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("external agent stdout unavailable"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("external agent stdin unavailable"))?;
        let stderr = child.stderr.take();

        let client = Self::from_io(stdout, stdin, Some(child), pid, &opts);
        if let Some(stderr) = stderr {
            spawn_stderr_reader(client.conn.clone(), stderr);
        }
        if let Err(error) = client.handshake().await {
            client.shutdown().await;
            return Err(error);
        }
        Ok(client)
    }

    /// Connect over an existing byte stream pair instead of a child process
    /// (in-process stub agents in tests, or an already-open transport).
    pub async fn connect_streams<R, W>(
        reader: R,
        writer: W,
        opts: AcpClientOptions,
    ) -> anyhow::Result<Arc<Self>>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let client = Self::from_io(reader, writer, None, None, &opts);
        if let Err(error) = client.handshake().await {
            client.shutdown().await;
            return Err(error);
        }
        Ok(client)
    }

    fn from_io<R, W>(
        reader: R,
        writer: W,
        child: Option<Child>,
        pid: Option<u32>,
        opts: &AcpClientOptions,
    ) -> Arc<Self>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let conn = Arc::new(Conn::new(opts));
        conn.touch();
        let boxed: Writer = Box::new(writer);
        // The writer mutex has no contention yet; `try_lock` cannot fail.
        if let Ok(mut slot) = conn.writer.try_lock() {
            *slot = Some(boxed);
        }
        let reader_task = spawn_reader(conn.clone(), reader);
        Arc::new(Self {
            conn,
            request_timeout: opts.request_timeout.unwrap_or(DEFAULT_REQUEST_TIMEOUT),
            child: Mutex::new(child),
            pid,
            session_id: StdMutex::new(None),
            initialize: StdMutex::new(None),
            reader_task: StdMutex::new(Some(reader_task)),
            shut_down: AtomicBool::new(false),
            parameterized_model_picker: opts.parameterized_model_picker,
        })
    }

    /// The `clientCapabilities` of `initialize`.
    fn client_capabilities(&self) -> Value {
        let mut capabilities = json!({
            "fs": { "readTextFile": false, "writeTextFile": false },
            "terminal": false,
        });
        if self.parameterized_model_picker {
            capabilities["_meta"] = json!({ "parameterizedModelPicker": true });
        }
        capabilities
    }

    /// `initialize`. Clients MUST declare the fs/terminal capabilities they
    /// implement; we implement none — external agents fall back to their own
    /// tooling.
    async fn handshake(&self) -> anyhow::Result<()> {
        let result = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": 1,
                    "clientCapabilities": self.client_capabilities(),
                    "clientInfo": {
                        "name": "kkagent",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                }),
            )
            .await?;
        if let Ok(init) = serde_json::from_value::<InitializeResult>(result) {
            *lock(&self.initialize) = Some(init);
        }
        Ok(())
    }

    /// The parsed `initialize` response, once the handshake succeeded.
    pub fn initialize_result(&self) -> Option<InitializeResult> {
        lock(&self.initialize).clone()
    }

    /// Id of the session created by the last `start_session*` call.
    pub fn session_id(&self) -> Option<String> {
        lock(&self.session_id).clone()
    }

    /// True while the agent process is connected (its stdout has not closed).
    pub fn is_alive(&self) -> bool {
        !self.conn.is_closed() && !self.shut_down.load(Ordering::SeqCst)
    }

    /// Perform `authenticate` using the agent's first advertised method.
    /// Only needed when the child process has no ambient credentials; callers
    /// may skip it when `env` injected `CURSOR_API_KEY`/`CURSOR_AUTH_TOKEN`.
    pub async fn authenticate(&self) -> anyhow::Result<()> {
        let method_id = self.initialize_result().and_then(|init| {
            init.auth_methods
                .first()
                .and_then(|method| method.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string)
        });
        let params = match method_id {
            Some(id) => json!({ "methodId": id }),
            None => json!({}),
        };
        self.request("authenticate", params).await?;
        Ok(())
    }

    /// Create a session bound to a working directory (and optional mode).
    /// `mode` is sent in the request body; agents that select modes only via
    /// `session/set_mode` (Cursor) ignore it — use [`AcpClient::set_mode`].
    pub async fn start_session(&self, cwd: &Path, mode: Option<&str>) -> anyhow::Result<String> {
        let mut params = json!({ "cwd": cwd.display().to_string(), "mcpServers": [] });
        if let Some(mode) = mode {
            params["mode"] = json!(mode);
        }
        let result = self.request("session/new", params).await?;
        let info = parse_session_info(&result)?;
        self.on_session_started(&info.session_id);
        Ok(info.session_id)
    }

    /// Like [`AcpClient::start_session`], but returns the modes/models block
    /// of the response as well.
    pub async fn start_session_info(&self, cwd: &Path) -> anyhow::Result<AcpSessionInfo> {
        let params = json!({ "cwd": cwd.display().to_string(), "mcpServers": [] });
        let result = self.request("session/new", params).await?;
        let info = parse_session_info(&result)?;
        self.on_session_started(&info.session_id);
        Ok(info)
    }

    fn on_session_started(&self, session_id: &str) {
        *lock(&self.session_id) = Some(session_id.to_string());
        lock(&self.conn.todos).clear();
    }

    /// Switch the session mode (`agent` / `plan` / `ask` for Cursor).
    pub async fn set_mode(&self, session_id: &str, mode_id: &str) -> anyhow::Result<()> {
        self.request(
            "session/set_mode",
            json!({ "sessionId": session_id, "modeId": mode_id }),
        )
        .await?;
        Ok(())
    }

    /// Set a session config option (`session/set_config_option`) and return
    /// the agent's updated option list (empty when the agent did not send
    /// one). The agent may persist the choice as its own default.
    pub async fn set_config_option(
        &self,
        session_id: &str,
        config_id: &str,
        value: &str,
    ) -> anyhow::Result<Vec<AcpConfigOption>> {
        let result = self
            .request(
                "session/set_config_option",
                json!({ "sessionId": session_id, "configId": config_id, "value": value }),
            )
            .await?;
        Ok(parse_config_options(result.get("configOptions")))
    }

    /// Run one prompt turn to completion, forwarding streamed progress to
    /// `progress_tx`. Resolves when the agent replies with a `stopReason`.
    /// The whole request is bounded by the client's request timeout (the
    /// unattended subagent path); use [`AcpClient::prompt_interactive`] for
    /// turns a human may take part in.
    pub async fn prompt(
        &self,
        session_id: &str,
        text: &str,
        progress_tx: mpsc::Sender<(String, ExternalProgress)>,
    ) -> anyhow::Result<PromptOutcome> {
        self.run_prompt(
            session_id,
            text,
            progress_tx,
            PromptLimit::Total(self.request_timeout),
        )
        .await
    }

    /// Like [`AcpClient::prompt`] for a turn with a human in the loop: there
    /// is no total time limit, only `idle_timeout` without any activity from
    /// the agent. Time spent while the host answers a reverse request
    /// (approval, question, plan review) does not count as idle.
    pub async fn prompt_interactive(
        &self,
        session_id: &str,
        text: &str,
        progress_tx: mpsc::Sender<(String, ExternalProgress)>,
        idle_timeout: Duration,
    ) -> anyhow::Result<PromptOutcome> {
        self.run_prompt(
            session_id,
            text,
            progress_tx,
            PromptLimit::Idle(idle_timeout),
        )
        .await
    }

    async fn run_prompt(
        &self,
        session_id: &str,
        text: &str,
        progress_tx: ProgressTx,
        limit: PromptLimit,
    ) -> anyhow::Result<PromptOutcome> {
        // Cursor answers a second prompt on a running session by cancelling
        // the running turn; never let that happen by accident.
        if self.conn.prompt_active.swap(true, Ordering::SeqCst) {
            anyhow::bail!("session/prompt failed: another prompt is already running");
        }
        let _guard = PromptGuard {
            conn: self.conn.clone(),
        };
        self.conn.reset_turn_state();
        self.conn.touch();
        *lock(&self.conn.progress_sink) = Some((session_id.to_string(), progress_tx));

        let (id, mut rx) = self
            .send_request(
                "session/prompt",
                json!({
                    "sessionId": session_id,
                    "prompt": [ { "type": "text", "text": text } ]
                }),
            )
            .await
            .map_err(|error| anyhow::anyhow!("session/prompt failed: {error}"))?;
        let _pending = PendingGuard {
            conn: self.conn.clone(),
            id,
        };

        let started = Instant::now();
        let response = loop {
            let wait = match limit {
                PromptLimit::Total(total) => total.saturating_sub(started.elapsed()),
                PromptLimit::Idle(_) => Duration::from_millis(250),
            };
            tokio::select! {
                response = &mut rx => break response,
                _ = tokio::time::sleep(wait) => match limit {
                    PromptLimit::Total(total) if started.elapsed() >= total => {
                        anyhow::bail!("session/prompt failed: timed out after {}s", total.as_secs());
                    }
                    PromptLimit::Idle(idle)
                        if self.conn.user_blocked.load(Ordering::SeqCst) == 0
                            && self.conn.idle_for() >= idle =>
                    {
                        anyhow::bail!(
                            "session/prompt failed: no output from the external agent for {}s",
                            idle.as_secs()
                        );
                    }
                    _ => {}
                },
            }
        };

        match response {
            Ok(Ok(result)) => {
                let stop_reason = result
                    .get("stopReason")
                    .and_then(Value::as_str)
                    .unwrap_or("end_turn")
                    .to_string();
                let text = std::mem::take(&mut *lock(&self.conn.text_acc));
                Ok(PromptOutcome { stop_reason, text })
            }
            Ok(Err(error)) => Err(anyhow::anyhow!(
                "session/prompt failed: {}",
                rpc_error_message(&error)
            )),
            Err(_) => Err(anyhow::anyhow!(
                "session/prompt failed: {}",
                self.conn.exit_description()
            )),
        }
    }

    /// Cancel the current prompt turn on a session (fire-and-forget). The
    /// agent answers the pending `session/prompt` with `stopReason:
    /// "cancelled"`.
    pub async fn cancel(&self, session_id: &str) {
        let _ = self
            .conn
            .write_frame(&json!({
                "jsonrpc": "2.0",
                "method": "session/cancel",
                "params": { "sessionId": session_id }
            }))
            .await;
    }

    /// Terminate the external agent process (and the processes it started)
    /// and stop the reader loop.
    pub async fn shutdown(&self) {
        if self.shut_down.swap(true, Ordering::SeqCst) {
            return;
        }
        self.conn.mark_closed();
        // Closing stdin first lets agents that watch it exit on their own.
        self.conn.writer.lock().await.take();
        if let Some(pid) = self.pid {
            kill_process_tree(pid);
        }
        if let Some(mut child) = self.child.lock().await.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        if let Some(task) = lock(&self.reader_task).take() {
            task.abort();
        }
    }

    /// Send a request and await its response with the configured timeout.
    async fn request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        let (id, rx) = self
            .send_request(method, params)
            .await
            .map_err(|error| anyhow::anyhow!("{method} failed: {error}"))?;
        let _pending = PendingGuard {
            conn: self.conn.clone(),
            id,
        };
        match tokio::time::timeout(self.request_timeout, rx).await {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(error))) => Err(anyhow::anyhow!(
                "{method} failed: {}",
                rpc_error_message(&error)
            )),
            Ok(Err(_)) => Err(anyhow::anyhow!(
                "{method} failed: {}",
                self.conn.exit_description()
            )),
            Err(_) => Err(anyhow::anyhow!(
                "{method} failed: timed out after {}s",
                self.request_timeout.as_secs()
            )),
        }
    }

    /// Register a pending entry and write the request frame.
    async fn send_request(
        &self,
        method: &str,
        params: Value,
    ) -> Result<(i64, oneshot::Receiver<Result<Value, Value>>), String> {
        if self.conn.is_closed() {
            return Err(self.conn.exit_description());
        }
        let id = self.conn.next_id.fetch_add(1, Ordering::Relaxed);
        let frame = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let (tx, rx) = oneshot::channel();
        lock(&self.conn.pending).insert(id, tx);
        if let Err(error) = self.conn.write_frame(&frame).await {
            lock(&self.conn.pending).remove(&id);
            return Err(error.to_string());
        }
        Ok((id, rx))
    }
}

impl Drop for AcpClient {
    fn drop(&mut self) {
        if !self.shut_down.swap(true, Ordering::SeqCst) {
            if let Some(pid) = self.pid {
                kill_process_tree(pid);
            }
        }
        if let Some(task) = lock(&self.reader_task).take() {
            task.abort();
        }
    }
}

/// Human-readable text of a JSON-RPC `error` object (`message` plus the
/// agent's `data.details` / string `data` when it adds information).
fn rpc_error_message(error: &Value) -> String {
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("unknown JSON-RPC error");
    let detail = error
        .pointer("/data/details")
        .and_then(Value::as_str)
        .or_else(|| error.get("data").and_then(Value::as_str))
        .filter(|detail| !detail.is_empty() && *detail != message);
    match detail {
        Some(detail) => format!("{message}: {detail}"),
        None => message.to_string(),
    }
}

fn parse_session_info(result: &Value) -> anyhow::Result<AcpSessionInfo> {
    let session_id = result
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("session/new response missing sessionId"))?
        .to_string();
    let current_mode = result
        .pointer("/modes/currentModeId")
        .and_then(Value::as_str)
        .map(str::to_string);
    let available_modes = result
        .pointer("/modes/availableModes")
        .and_then(Value::as_array)
        .map(|modes| {
            modes
                .iter()
                .filter_map(|m| m.get("id").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    Ok(AcpSessionInfo {
        session_id,
        current_mode,
        available_modes,
        models: result.get("models").cloned(),
        config_options: parse_config_options(result.get("configOptions")),
    })
}

/// Resolve `command` through `PATH` (honoring a `PATH` override from the
/// options and, on Windows, `PATHEXT` so `agent.cmd` shims are found).
fn resolve_program(command: &str, opts: &AcpClientOptions) -> PathBuf {
    let direct = Path::new(command);
    if direct.is_absolute() || direct.components().count() > 1 {
        return direct.to_path_buf();
    }
    let search_path = opts
        .env
        .get("PATH")
        .cloned()
        .or_else(|| std::env::var("PATH").ok());
    let found = match search_path {
        Some(path) => which::which_in(command, Some(path), &opts.cwd),
        None => which::which(command),
    };
    found.unwrap_or_else(|_| PathBuf::from(command))
}

/// Kill a process and everything in its process group / tree.
fn kill_process_tree(pid: u32) {
    #[cfg(unix)]
    {
        // The agent was spawned into its own process group (`process_group(0)`).
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
    }
}

fn spawn_reader<R>(conn: Arc<Conn>, reader: R) -> JoinHandle<()>
where
    R: AsyncRead + Send + Unpin + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.trim().is_empty() {
                continue;
            }
            conn.touch();
            let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                tracing::warn!(target: "acp_client", "non-JSON frame from external agent: {line}");
                continue;
            };
            route_message(&conn, msg).await;
        }
        // EOF (or a read error): fail every waiting request instead of
        // leaving it to its timeout.
        conn.mark_closed();
    })
}

fn spawn_stderr_reader<R>(conn: Arc<Conn>, stderr: R)
where
    R: AsyncRead + Send + Unpin + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            tracing::debug!(target: "acp_client", "external agent stderr: {line}");
            let mut tail = lock(&conn.stderr_tail);
            if tail.len() >= STDERR_TAIL_LINES {
                tail.pop_front();
            }
            tail.push_back(line);
        }
    });
}

// ---------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------

/// Route one parsed JSON-RPC message: responses to pending requests,
/// reverse requests to the permission policy / host handler, notifications
/// to the progress sink.
async fn route_message(conn: &Arc<Conn>, msg: Value) {
    let id = msg.get("id").filter(|v| !v.is_null()).cloned();
    let method = msg
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_string);
    match (id, method) {
        (Some(id), None) => deliver_response(conn, &id, &msg),
        (Some(id), Some(method)) => {
            let params = msg.get("params").cloned().unwrap_or(Value::Null);
            handle_reverse_request(conn, id, &method, params).await;
        }
        (None, Some(method)) => {
            let params = msg.get("params").cloned().unwrap_or(Value::Null);
            handle_notification(conn, &method, params).await;
        }
        (None, None) => {}
    }
}

fn deliver_response(conn: &Conn, id: &Value, msg: &Value) {
    let Some(id) = id.as_i64() else {
        return;
    };
    let Some(tx) = lock(&conn.pending).remove(&id) else {
        return;
    };
    let result = match (msg.get("result"), msg.get("error")) {
        (Some(result), _) => Ok(result.clone()),
        (_, Some(error)) => Err(error.clone()),
        _ => Err(json!({ "message": "malformed response" })),
    };
    let _ = tx.send(result);
}

async fn handle_notification(conn: &Arc<Conn>, method: &str, params: Value) {
    match method {
        "session/update" => {
            if let Some(progress) = translate_update(&params) {
                let session = params
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let progress = conn.absorb(progress);
                conn.emit(session, progress).await;
            }
        }
        "cursor/update_todos" => {
            // Documented as a notification; Cursor also sends it as a request.
            let todos = conn.apply_todos(&params);
            conn.emit(String::new(), ExternalProgress::Todos { todos })
                .await;
        }
        other => {
            tracing::trace!(target: "acp_client", "ignoring notification: {other}");
        }
    }
}

async fn reply_result(conn: &Conn, id: &Value, result: Value) {
    let frame = json!({ "jsonrpc": "2.0", "id": id, "result": result });
    if let Err(error) = conn.write_frame(&frame).await {
        tracing::warn!(target: "acp_client", "failed to answer reverse request: {error}");
    }
}

async fn reply_error(conn: &Conn, id: &Value, code: i64, message: String) {
    let frame = json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    });
    if let Err(error) = conn.write_frame(&frame).await {
        tracing::warn!(target: "acp_client", "failed to answer reverse request: {error}");
    }
}

/// Run a host callback for a reverse request in its own task so the reader
/// loop keeps draining the agent's output. A panicking callback is answered
/// with `cancelled` instead of leaving the agent blocked forever.
fn spawn_host_task<F>(conn: &Arc<Conn>, id: Value, fut: F)
where
    F: Future<Output = Value> + Send + 'static,
{
    conn.user_blocked.fetch_add(1, Ordering::SeqCst);
    let conn = conn.clone();
    tokio::spawn(async move {
        let joined = tokio::spawn(fut).await;
        conn.user_blocked.fetch_sub(1, Ordering::SeqCst);
        conn.touch();
        let result = joined.unwrap_or_else(|_| json!({ "outcome": { "outcome": "cancelled" } }));
        reply_result(&conn, &id, result).await;
    });
}

async fn handle_reverse_request(conn: &Arc<Conn>, id: Value, method: &str, params: Value) {
    match method {
        "session/request_permission" => match conn.handler.clone() {
            Some(handler) => {
                let request = AcpPermissionRequest::from_params(&params);
                spawn_host_task(conn, id, async move {
                    handler.request_permission(request).await.to_result()
                });
            }
            None => {
                let response = conn.permission.response(&params);
                tracing::debug!(target: "acp_client", "answered external agent permission request");
                reply_result(conn, &id, response).await;
            }
        },
        "cursor/ask_question" => match conn.handler.clone() {
            Some(handler) => {
                let request = AcpAskQuestionRequest::from_params(&params);
                spawn_host_task(conn, id, async move {
                    handler.ask_question(request).await.to_result()
                });
            }
            // Unattended: Cursor falls back to permission-style questions.
            None => method_not_found(conn, &id, method).await,
        },
        "cursor/create_plan" => match conn.handler.clone() {
            Some(handler) => {
                let request = AcpCreatePlanRequest::from_params(&params);
                spawn_host_task(conn, id, async move {
                    handler.create_plan(request).await.to_result()
                });
            }
            // Unattended: Cursor falls back to a local plan artifact.
            None => method_not_found(conn, &id, method).await,
        },
        "cursor/update_todos" => {
            let todos = conn.apply_todos(&params);
            conn.emit(
                String::new(),
                ExternalProgress::Todos {
                    todos: todos.clone(),
                },
            )
            .await;
            reply_result(
                conn,
                &id,
                json!({ "outcome": { "outcome": "accepted", "todos": todos } }),
            )
            .await;
        }
        // We neither run subagent tasks nor generate images for the agent.
        "cursor/task" | "cursor/generate_image" => {
            reply_result(
                conn,
                &id,
                json!({ "outcome": {
                    "outcome": "rejected",
                    "reason": format!("{method} is not supported by this client"),
                } }),
            )
            .await;
        }
        other => method_not_found(conn, &id, other).await,
    }
}

async fn method_not_found(conn: &Conn, id: &Value, method: &str) {
    // Unknown reverse request: answer with a JSON-RPC error so the agent does
    // not hang awaiting a response.
    reply_error(conn, id, -32601, format!("method not supported: {method}")).await;
}

// ---------------------------------------------------------------------------
// Update translation
// ---------------------------------------------------------------------------

fn str_field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn opt_str_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// ACP ids are strings; tolerate numbers from sloppy agents.
fn value_to_id(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn parse_locations(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    item.get("path")
                        .and_then(Value::as_str)
                        .filter(|p| !p.is_empty())
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push_str("\n… [truncated]");
    out
}

/// The Cursor CLI reports a created file as the two halves of a patch header
/// (`oldText: "-- /dev/null"`, `newText: "++ b/<path>\n<content>"`): show the
/// content without the header line. Plain old/new texts pass through.
fn diff_body<'a>(old_text: &str, new_text: &'a str) -> &'a str {
    if !old_text.starts_with("-- ") {
        return new_text;
    }
    match new_text.split_once('\n') {
        Some((header, body)) if header.starts_with("++ ") => body,
        _ => new_text,
    }
}

/// Readable text for the small result objects the Cursor CLI reports for
/// its read and search tools (`{"content": …}`, `{"totalMatches": 3, …}`,
/// `{"totalFiles": 2, …}`); `None` leaves other shapes to the JSON fallback.
fn summarize_object_output(map: &serde_json::Map<String, Value>) -> Option<String> {
    if let Some(content) = map.get("content").and_then(Value::as_str) {
        return Some(content.to_string());
    }
    let truncated = map
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let (count, singular, plural) = if let Some(n) = map.get("totalMatches").and_then(Value::as_u64)
    {
        (n, "match", "matches")
    } else if let Some(n) = map.get("totalFiles").and_then(Value::as_u64) {
        (n, "file", "files")
    } else {
        return None;
    };
    let noun = if count == 1 { singular } else { plural };
    let suffix = if truncated { " (truncated)" } else { "" };
    Some(format!("{count} {noun}{suffix}"))
}

/// Text rendering of a tool call's result: `rawOutput` first (shell results
/// carry `stdout`/`stderr`/`exitCode`), else the text / diff `content`.
fn tool_output_text(update: &Value) -> Option<String> {
    if let Some(raw) = update.get("rawOutput").filter(|v| !v.is_null()) {
        let text = match raw {
            Value::String(s) => s.clone(),
            Value::Object(map) if map.contains_key("stdout") || map.contains_key("stderr") => {
                let stdout = raw.get("stdout").and_then(Value::as_str).unwrap_or("");
                let stderr = raw.get("stderr").and_then(Value::as_str).unwrap_or("");
                let mut out = stdout.to_string();
                if !stderr.trim().is_empty() {
                    if !out.is_empty() && !out.ends_with('\n') {
                        out.push('\n');
                    }
                    out.push_str("[stderr]\n");
                    out.push_str(stderr);
                }
                if let Some(code) = raw.get("exitCode").and_then(Value::as_i64) {
                    if code != 0 {
                        if !out.is_empty() && !out.ends_with('\n') {
                            out.push('\n');
                        }
                        out.push_str(&format!("[exit code {code}]"));
                    }
                }
                out
            }
            Value::Object(map) => summarize_object_output(map)
                .unwrap_or_else(|| serde_json::to_string(raw).unwrap_or_default()),
            other => serde_json::to_string(other).unwrap_or_default(),
        };
        return (!text.trim().is_empty()).then(|| truncate_chars(&text, MAX_TOOL_OUTPUT_CHARS));
    }
    let parts: Vec<String> = update
        .get("content")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(|part| match part.get("type").and_then(Value::as_str) {
            Some("content") => part
                .pointer("/content/text")
                .and_then(Value::as_str)
                .map(str::to_string),
            Some("diff") => {
                let path = part.get("path").and_then(Value::as_str).unwrap_or("");
                let old_text = part.get("oldText").and_then(Value::as_str).unwrap_or("");
                let new_text = part.get("newText").and_then(Value::as_str).unwrap_or("");
                Some(format!("edit {path}\n{}", diff_body(old_text, new_text)))
            }
            _ => None,
        })
        .collect();
    let text = parts.join("\n");
    (!text.trim().is_empty()).then(|| truncate_chars(&text, MAX_TOOL_OUTPUT_CHARS))
}

/// Translate a `session/update` notification into [`ExternalProgress`].
fn translate_update(params: &Value) -> Option<ExternalProgress> {
    let update = params.get("update")?;
    match update.get("sessionUpdate").and_then(Value::as_str) {
        Some("agent_message_chunk") => {
            let delta = update
                .pointer("/content/text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            Some(ExternalProgress::Text { delta })
        }
        Some("agent_thought_chunk") => {
            let delta = update
                .pointer("/content/text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            Some(ExternalProgress::Thought { delta })
        }
        Some("tool_call") | Some("tool_call_update") => Some(ExternalProgress::ToolCall {
            tool_call_id: str_field(update, "toolCallId"),
            title: str_field(update, "title"),
            tool_kind: opt_str_field(update, "kind"),
            status: opt_str_field(update, "status"),
            raw_input: update.get("rawInput").filter(|v| !v.is_null()).cloned(),
            output: tool_output_text(update),
            locations: parse_locations(update.get("locations")),
        }),
        Some("plan") => Some(ExternalProgress::Plan {
            entries: update
                .get("entries")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
        }),
        Some("current_mode_update") => Some(ExternalProgress::ModeChanged {
            mode_id: str_field(update, "currentModeId"),
        }),
        _ => Some(ExternalProgress::Unknown {
            update: update.clone(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{duplex, DuplexStream, Lines};

    #[test]
    fn permission_auto_approve_selects_first_option() {
        let policy = PermissionPolicy::AutoApprove;
        let params = serde_json::json!({
            "sessionId": "s",
            "options": [
                { "optionId": "allow-once", "name": "Allow", "kind": "allow_once" },
                { "optionId": "reject-once", "name": "Reject", "kind": "reject_once" }
            ]
        });
        let response = policy.response(&params);
        assert_eq!(response["outcome"]["optionId"], "allow-once");
        assert_eq!(response["outcome"]["outcome"], "selected");
    }

    #[test]
    fn permission_deny_cancels() {
        let policy = PermissionPolicy::Deny;
        let response = policy.response(&serde_json::json!({ "options": [] }));
        assert_eq!(response["outcome"]["outcome"], "cancelled");
    }

    #[test]
    fn translate_text_chunk() {
        let params = serde_json::json!({
            "sessionId": "s1",
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": "hello " }
            }
        });
        let progress = translate_update(&params).expect("translated");
        match progress {
            ExternalProgress::Text { delta } => assert_eq!(delta, "hello "),
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn translate_thought_chunk_and_mode_update() {
        let thought = serde_json::json!({
            "sessionId": "s1",
            "update": {
                "sessionUpdate": "agent_thought_chunk",
                "content": { "type": "text", "text": "hmm" }
            }
        });
        match translate_update(&thought).expect("translated") {
            ExternalProgress::Thought { delta } => assert_eq!(delta, "hmm"),
            other => panic!("unexpected variant: {other:?}"),
        }
        let mode = serde_json::json!({
            "sessionId": "s1",
            "update": { "sessionUpdate": "current_mode_update", "currentModeId": "plan" }
        });
        match translate_update(&mode).expect("translated") {
            ExternalProgress::ModeChanged { mode_id } => assert_eq!(mode_id, "plan"),
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn translate_tool_call() {
        let params = serde_json::json!({
            "sessionId": "s1",
            "update": {
                "sessionUpdate": "tool_call",
                "toolCallId": "tc1",
                "title": "Read main.rs",
                "kind": "read",
                "status": "pending"
            }
        });
        match translate_update(&params).expect("translated") {
            ExternalProgress::ToolCall {
                tool_call_id,
                title,
                tool_kind,
                status,
                raw_input,
                output,
                locations,
            } => {
                assert_eq!(tool_call_id, "tc1");
                assert_eq!(title, "Read main.rs");
                assert_eq!(tool_kind.as_deref(), Some("read"));
                assert_eq!(status.as_deref(), Some("pending"));
                assert!(raw_input.is_none());
                assert!(output.is_none());
                assert!(locations.is_empty());
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn translate_unknown_update_is_preserved() {
        let params = serde_json::json!({
            "sessionId": "s1",
            "update": { "sessionUpdate": "something_new", "payload": {"x": 1} }
        });
        match translate_update(&params).expect("translated") {
            ExternalProgress::Unknown { update } => {
                assert_eq!(update["payload"]["x"], 1);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn initialize_result_auth_detection() {
        let empty: InitializeResult =
            serde_json::from_value(serde_json::json!({ "authMethods": [] })).unwrap();
        assert!(!empty.requires_auth());
        let with_auth: InitializeResult = serde_json::from_value(serde_json::json!({
            "authMethods": [ { "id": "cursor_login", "name": "Cursor Login" } ]
        }))
        .unwrap();
        assert!(with_auth.requires_auth());
        assert_eq!(with_auth.auth_methods[0]["id"], "cursor_login");
    }

    #[test]
    fn initialize_result_reads_protocol_version_and_load_session() {
        let init: InitializeResult = serde_json::from_value(serde_json::json!({
            "protocolVersion": 1,
            "agentCapabilities": { "loadSession": true },
        }))
        .unwrap();
        assert_eq!(init.protocol_version, Some(serde_json::json!(1)));
        assert!(init.supports_load_session());
    }

    #[test]
    fn tool_state_merges_updates_into_cumulative_events() {
        let conn = Conn::new(&AcpClientOptions::cursor_cli(PathBuf::from(".")));
        let first = translate_update(&json!({
            "sessionId": "s",
            "update": {
                "sessionUpdate": "tool_call", "toolCallId": "t1",
                "title": "Edit File", "kind": "edit", "status": "pending", "rawInput": {}
            }
        }))
        .unwrap();
        let _ = conn.absorb(first);
        let second = translate_update(&json!({
            "sessionId": "s",
            "update": {
                "sessionUpdate": "tool_call_update", "toolCallId": "t1",
                "title": "Edit `/w/a.txt`", "rawInput": { "path": "/w/a.txt" },
                "locations": [ { "path": "/w/a.txt" } ]
            }
        }))
        .unwrap();
        let _ = conn.absorb(second);
        // A bare status update (no title, kind, input) still carries it all.
        let done = translate_update(&json!({
            "sessionId": "s",
            "update": {
                "sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed",
                "content": [ { "type": "diff", "path": "/w/a.txt", "oldText": "", "newText": "hi" } ]
            }
        }))
        .unwrap();
        match conn.absorb(done) {
            ExternalProgress::ToolCall {
                title,
                tool_kind,
                status,
                raw_input,
                output,
                locations,
                ..
            } => {
                assert_eq!(title, "Edit `/w/a.txt`");
                assert_eq!(tool_kind.as_deref(), Some("edit"));
                assert_eq!(status.as_deref(), Some("completed"));
                assert_eq!(raw_input.unwrap()["path"], "/w/a.txt");
                assert_eq!(output.as_deref(), Some("edit /w/a.txt\nhi"));
                assert_eq!(locations, vec!["/w/a.txt".to_string()]);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn tool_output_formats_shell_results() {
        let ok = tool_output_text(&json!({
            "rawOutput": { "exitCode": 0, "stdout": "hi", "stderr": "" }
        }));
        assert_eq!(ok.as_deref(), Some("hi"));
        let failed = tool_output_text(&json!({
            "rawOutput": { "exitCode": 2, "stdout": "out", "stderr": "boom" }
        }));
        assert_eq!(
            failed.as_deref(),
            Some("out\n[stderr]\nboom\n[exit code 2]")
        );
        let text = tool_output_text(&json!({
            "content": [ { "type": "content", "content": { "type": "text", "text": "Plan saved" } } ]
        }));
        assert_eq!(text.as_deref(), Some("Plan saved"));
        assert!(tool_output_text(&json!({ "status": "completed" })).is_none());
    }

    #[test]
    fn tool_output_formats_the_cursor_read_and_search_results() {
        let read = tool_output_text(&json!({ "rawOutput": { "content": "hi-there" } }));
        assert_eq!(read.as_deref(), Some("hi-there"));
        let grep = tool_output_text(&json!({
            "rawOutput": { "totalMatches": 1, "truncated": false }
        }));
        assert_eq!(grep.as_deref(), Some("1 match"));
        let find = tool_output_text(&json!({
            "rawOutput": { "totalFiles": 12, "truncated": true }
        }));
        assert_eq!(find.as_deref(), Some("12 files (truncated)"));
        // Unknown shapes still show up verbatim.
        let other = tool_output_text(&json!({ "rawOutput": { "linesAdded": 3 } }));
        assert_eq!(other.as_deref(), Some(r#"{"linesAdded":3}"#));
    }

    #[test]
    fn a_created_file_diff_drops_the_patch_header_line() {
        let created = tool_output_text(&json!({
            "content": [ {
                "type": "diff", "path": "/w/notes.txt",
                "oldText": "-- /dev/null", "newText": "++ b//w/notes.txt\nhi-there"
            } ]
        }));
        assert_eq!(created.as_deref(), Some("edit /w/notes.txt\nhi-there"));
        // A real before/after pair is shown as it is.
        let edited = tool_output_text(&json!({
            "content": [ {
                "type": "diff", "path": "/w/a.rs", "oldText": "-- keep", "newText": "plain\nline"
            } ]
        }));
        assert_eq!(edited.as_deref(), Some("edit /w/a.rs\nplain\nline"));
    }

    #[test]
    fn permission_request_picks_options_by_kind_then_id() {
        let request = AcpPermissionRequest::from_params(&json!({
            "sessionId": "s",
            "toolCall": {
                "toolCallId": "t", "title": "`rm -rf x`", "kind": "execute",
                "rawInput": { "command": "rm -rf x" }, "locations": [ { "path": "/w/x" } ]
            },
            "options": [
                { "optionId": "opt-a", "name": "Allow", "kind": "allow_once" },
                { "optionId": "opt-b", "name": "Always", "kind": "allow_always" },
                { "optionId": "opt-c", "name": "Reject", "kind": "reject_once" }
            ]
        }));
        assert_eq!(request.title, "`rm -rf x`");
        assert_eq!(request.tool_kind.as_deref(), Some("execute"));
        assert_eq!(request.locations, vec!["/w/x".to_string()]);
        let id = |choice| request.option_for(choice).map(|o| o.option_id.as_str());
        assert_eq!(id(AcpPermissionChoice::AllowOnce), Some("opt-a"));
        assert_eq!(id(AcpPermissionChoice::AllowAlways), Some("opt-b"));
        assert_eq!(id(AcpPermissionChoice::RejectOnce), Some("opt-c"));
        // No persistent reject offered: falls back to the one-shot reject.
        assert_eq!(id(AcpPermissionChoice::RejectAlways), Some("opt-c"));

        // Documented ids without `kind`.
        let by_id = AcpPermissionRequest::from_params(&json!({
            "options": [
                { "optionId": "allow-once", "name": "Allow" },
                { "optionId": "reject-once", "name": "Reject" }
            ]
        }));
        assert_eq!(
            by_id
                .option_for(AcpPermissionChoice::AllowAlways)
                .map(|o| o.option_id.as_str()),
            Some("allow-once")
        );
        assert!(by_id.option_for(AcpPermissionChoice::RejectOnce).is_some());
    }

    #[test]
    fn outcomes_serialize_to_the_cursor_wire_shapes() {
        assert_eq!(
            AcpPermissionOutcome::Selected("x".into()).to_result(),
            json!({ "outcome": { "outcome": "selected", "optionId": "x" } })
        );
        assert_eq!(
            AcpAskQuestionOutcome::Answered(vec![AcpQuestionAnswer {
                question_id: "q1".into(),
                selected_option_ids: vec!["a".into()],
            }])
            .to_result(),
            json!({ "outcome": { "outcome": "answered",
                "answers": [ { "questionId": "q1", "selectedOptionIds": ["a"] } ] } })
        );
        assert_eq!(
            AcpAskQuestionOutcome::Skipped(Some("no".into())).to_result(),
            json!({ "outcome": { "outcome": "skipped", "reason": "no" } })
        );
        assert_eq!(
            AcpCreatePlanOutcome::Accepted { plan_uri: None }.to_result(),
            json!({ "outcome": { "outcome": "accepted" } })
        );
        assert_eq!(
            AcpCreatePlanOutcome::Rejected {
                reason: Some("redo".into())
            }
            .to_result(),
            json!({ "outcome": { "outcome": "rejected", "reason": "redo" } })
        );
        assert_eq!(
            AcpCreatePlanOutcome::Cancelled.to_result(),
            json!({ "outcome": { "outcome": "cancelled" } })
        );
    }

    #[test]
    fn todos_merge_by_id_or_replace() {
        let conn = Conn::new(&AcpClientOptions::cursor_cli(PathBuf::from(".")));
        let first = conn.apply_todos(&json!({
            "todos": [
                { "id": "a", "content": "A", "status": "pending" },
                { "id": "b", "content": "B", "status": "pending" }
            ],
            "merge": false
        }));
        assert_eq!(first.len(), 2);
        let merged = conn.apply_todos(&json!({
            "todos": [
                { "id": "b", "content": "B", "status": "completed" },
                { "id": "c", "content": "C", "status": "pending" }
            ],
            "merge": true
        }));
        let ids: Vec<_> = merged.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
        assert_eq!(merged[1].status, "completed");
        let replaced = conn.apply_todos(&json!({
            "todos": [ { "id": "z", "content": "Z", "status": "pending" } ],
            "merge": false
        }));
        assert_eq!(replaced.len(), 1);
    }

    // ---- in-process stub agent -------------------------------------------

    /// The agent side of a duplex pair: reads the client's frames and writes
    /// scripted replies, notifications and reverse requests.
    struct StubAgent {
        lines: Lines<BufReader<DuplexStream>>,
        out: DuplexStream,
    }

    impl StubAgent {
        async fn recv(&mut self) -> Value {
            let line = tokio::time::timeout(Duration::from_secs(10), self.lines.next_line())
                .await
                .expect("stub agent timed out waiting for a client frame")
                .expect("read")
                .expect("client closed the stream");
            serde_json::from_str(&line).expect("json frame")
        }

        async fn send(&mut self, frame: Value) {
            let mut line = frame.to_string();
            line.push('\n');
            self.out.write_all(line.as_bytes()).await.expect("write");
            self.out.flush().await.expect("flush");
        }

        async fn expect_method(&mut self, method: &str) -> Value {
            let frame = self.recv().await;
            assert_eq!(frame["method"], method, "unexpected frame: {frame}");
            frame
        }

        async fn reply(&mut self, request: &Value, result: Value) {
            self.send(json!({ "jsonrpc": "2.0", "id": request["id"], "result": result }))
                .await;
        }

        async fn update(&mut self, update: Value) {
            self.send(json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": { "sessionId": "sess-1", "update": update }
            }))
            .await;
        }
    }

    /// Connect a client (with `handler`) to a fresh stub agent and finish the
    /// `initialize` handshake plus `session/new`.
    async fn connect(
        handler: Option<Arc<dyn AcpRequestHandler>>,
        request_timeout: Option<Duration>,
    ) -> (Arc<AcpClient>, StubAgent, String) {
        let (client_read, agent_out) = duplex(1 << 16);
        let (agent_in, client_write) = duplex(1 << 16);
        let mut opts = AcpClientOptions::cursor_cli(PathBuf::from("."));
        opts.handler = handler;
        opts.request_timeout = request_timeout;
        let connecting = tokio::spawn(AcpClient::connect_streams(client_read, client_write, opts));
        let mut agent = StubAgent {
            lines: BufReader::new(agent_in).lines(),
            out: agent_out,
        };
        let init = agent.expect_method("initialize").await;
        assert_eq!(init["params"]["clientInfo"]["name"], "kkagent");
        agent
            .reply(
                &init,
                json!({
                    "protocolVersion": 1,
                    "agentCapabilities": { "loadSession": true },
                    "authMethods": [ { "id": "cursor_login", "name": "Cursor Login" } ]
                }),
            )
            .await;
        let client = connecting.await.expect("join").expect("connect");

        let session_client = client.clone();
        let starting = tokio::spawn(async move {
            session_client
                .start_session_info(Path::new("/work"))
                .await
                .expect("session/new")
        });
        let new = agent.expect_method("session/new").await;
        assert_eq!(new["params"]["cwd"], "/work");
        agent
            .reply(
                &new,
                json!({
                    "sessionId": "sess-1",
                    "modes": {
                        "currentModeId": "agent",
                        "availableModes": [ { "id": "agent" }, { "id": "plan" }, { "id": "ask" } ]
                    },
                    "models": { "currentModelId": "m1", "availableModels": [] }
                }),
            )
            .await;
        let info = starting.await.expect("join");
        assert_eq!(info.session_id, "sess-1");
        assert_eq!(info.current_mode.as_deref(), Some("agent"));
        assert_eq!(info.available_modes, vec!["agent", "plan", "ask"]);
        (client, agent, info.session_id)
    }

    #[derive(Default)]
    struct ScriptedHandler {
        permissions: StdMutex<Vec<AcpPermissionRequest>>,
        questions: StdMutex<Vec<AcpAskQuestionRequest>>,
        plans: StdMutex<Vec<AcpCreatePlanRequest>>,
        delay: Duration,
    }

    #[async_trait]
    impl AcpRequestHandler for ScriptedHandler {
        async fn request_permission(&self, request: AcpPermissionRequest) -> AcpPermissionOutcome {
            tokio::time::sleep(self.delay).await;
            let choice = request
                .option_for(AcpPermissionChoice::AllowAlways)
                .map(|o| o.option_id.clone());
            lock(&self.permissions).push(request);
            match choice {
                Some(id) => AcpPermissionOutcome::Selected(id),
                None => AcpPermissionOutcome::Cancelled,
            }
        }

        async fn ask_question(&self, request: AcpAskQuestionRequest) -> AcpAskQuestionOutcome {
            let answers = request
                .questions
                .iter()
                .map(|q| AcpQuestionAnswer {
                    question_id: q.id.clone(),
                    selected_option_ids: q.options.iter().take(1).map(|o| o.id.clone()).collect(),
                })
                .collect();
            lock(&self.questions).push(request);
            AcpAskQuestionOutcome::Answered(answers)
        }

        async fn create_plan(&self, request: AcpCreatePlanRequest) -> AcpCreatePlanOutcome {
            lock(&self.plans).push(request);
            AcpCreatePlanOutcome::Rejected {
                reason: Some("needs changes".into()),
            }
        }
    }

    fn prompt_in_background(
        client: &Arc<AcpClient>,
        idle: Duration,
    ) -> (
        JoinHandle<anyhow::Result<PromptOutcome>>,
        mpsc::Receiver<(String, ExternalProgress)>,
    ) {
        let (tx, rx) = mpsc::channel(64);
        let client = client.clone();
        let handle =
            tokio::spawn(
                async move { client.prompt_interactive("sess-1", "do it", tx, idle).await },
            );
        (handle, rx)
    }

    #[tokio::test]
    async fn interactive_prompt_streams_progress_and_returns_stop_reason() {
        let (client, mut agent, _) = connect(None, None).await;
        let (handle, mut rx) = prompt_in_background(&client, Duration::from_secs(5));
        let prompt = agent.expect_method("session/prompt").await;
        assert_eq!(prompt["params"]["sessionId"], "sess-1");
        assert_eq!(prompt["params"]["prompt"][0]["text"], "do it");

        agent
            .update(json!({ "sessionUpdate": "agent_thought_chunk",
                "content": { "type": "text", "text": "thinking" } }))
            .await;
        agent
            .update(json!({ "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": "Hel" } }))
            .await;
        agent
            .update(json!({ "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": "lo" } }))
            .await;
        agent
            .reply(&prompt, json!({ "stopReason": "end_turn" }))
            .await;

        let outcome = handle.await.unwrap().unwrap();
        assert_eq!(outcome.stop_reason, "end_turn");
        assert_eq!(outcome.text, "Hello");
        // Every notification that preceded the response is already queued.
        let mut kinds = Vec::new();
        while let Ok((_, progress)) = rx.try_recv() {
            kinds.push(match progress {
                ExternalProgress::Thought { .. } => "thought",
                ExternalProgress::Text { .. } => "text",
                _ => "other",
            });
        }
        assert_eq!(kinds, vec!["thought", "text", "text"]);
        client.shutdown().await;
    }

    #[tokio::test]
    async fn second_prompt_is_refused_while_one_is_running() {
        let (client, mut agent, _) = connect(None, None).await;
        let (handle, _rx) = prompt_in_background(&client, Duration::from_secs(5));
        let prompt = agent.expect_method("session/prompt").await;

        let (tx, _rx2) = mpsc::channel(4);
        let error = client
            .prompt_interactive("sess-1", "again", tx, Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("already running"), "{error}");

        agent
            .reply(&prompt, json!({ "stopReason": "end_turn" }))
            .await;
        handle.await.unwrap().unwrap();
        // The flag is released once the first prompt finished.
        let (tx, _rx3) = mpsc::channel(4);
        let again = {
            let client = client.clone();
            tokio::spawn(async move {
                client
                    .prompt_interactive("sess-1", "next", tx, Duration::from_secs(5))
                    .await
            })
        };
        let second = agent.expect_method("session/prompt").await;
        agent
            .reply(&second, json!({ "stopReason": "end_turn" }))
            .await;
        again.await.unwrap().unwrap();
        client.shutdown().await;
    }

    #[tokio::test]
    async fn permission_request_goes_to_the_handler_and_back() {
        let handler = Arc::new(ScriptedHandler::default());
        let (client, mut agent, _) = connect(Some(handler.clone()), None).await;
        let (handle, _rx) = prompt_in_background(&client, Duration::from_secs(5));
        let prompt = agent.expect_method("session/prompt").await;

        agent
            .send(json!({
                "jsonrpc": "2.0", "id": 0, "method": "session/request_permission",
                "params": {
                    "sessionId": "sess-1",
                    "toolCall": { "toolCallId": "t1", "title": "`ls`", "kind": "execute",
                                  "rawInput": { "command": "ls" } },
                    "options": [
                        { "optionId": "allow-once", "name": "Allow", "kind": "allow_once" },
                        { "optionId": "allow-always", "name": "Always", "kind": "allow_always" },
                        { "optionId": "reject-once", "name": "Reject", "kind": "reject_once" }
                    ]
                }
            }))
            .await;
        let reply = agent.recv().await;
        // JSON-RPC id 0 must round-trip (it is falsy in some languages).
        assert_eq!(reply["id"], 0);
        assert_eq!(reply["result"]["outcome"]["outcome"], "selected");
        assert_eq!(reply["result"]["outcome"]["optionId"], "allow-always");
        assert_eq!(lock(&handler.permissions)[0].title, "`ls`");

        agent
            .reply(&prompt, json!({ "stopReason": "end_turn" }))
            .await;
        handle.await.unwrap().unwrap();
        client.shutdown().await;
    }

    #[tokio::test]
    async fn ask_question_and_create_plan_round_trip() {
        let handler = Arc::new(ScriptedHandler::default());
        let (client, mut agent, _) = connect(Some(handler.clone()), None).await;
        let (handle, _rx) = prompt_in_background(&client, Duration::from_secs(5));
        let prompt = agent.expect_method("session/prompt").await;

        agent
            .send(json!({
                "jsonrpc": "2.0", "id": 1, "method": "cursor/ask_question",
                "params": {
                    "toolCallId": "call_1", "title": "Need input",
                    "questions": [
                        { "id": "q1", "prompt": "Which?", "allowMultiple": false,
                          "options": [ { "id": "x", "label": "X" }, { "id": "y", "label": "Y" } ] },
                        { "id": "q2", "prompt": "More?", "allowMultiple": true,
                          "options": [ { "id": "m", "label": "M" } ] }
                    ]
                }
            }))
            .await;
        let answered = agent.recv().await;
        assert_eq!(answered["id"], 1);
        assert_eq!(answered["result"]["outcome"]["outcome"], "answered");
        assert_eq!(
            answered["result"]["outcome"]["answers"],
            json!([
                { "questionId": "q1", "selectedOptionIds": ["x"] },
                { "questionId": "q2", "selectedOptionIds": ["m"] }
            ])
        );
        let asked = lock(&handler.questions)[0].clone();
        assert_eq!(asked.title.as_deref(), Some("Need input"));
        assert!(asked.questions[1].allow_multiple);

        agent
            .send(json!({
                "jsonrpc": "2.0", "id": 2, "method": "cursor/create_plan",
                "params": {
                    "toolCallId": "call_2", "name": "Plan", "overview": "Do things",
                    "plan": "# Plan\n- a",
                    "todos": [ { "id": "t1", "content": "a", "status": "pending" } ],
                    "isProject": false, "phases": []
                }
            }))
            .await;
        let rejected = agent.recv().await;
        assert_eq!(rejected["id"], 2);
        assert_eq!(
            rejected["result"],
            json!({ "outcome": { "outcome": "rejected", "reason": "needs changes" } })
        );
        let plan = lock(&handler.plans)[0].clone();
        assert_eq!(plan.plan, "# Plan\n- a");
        assert_eq!(plan.todos.len(), 1);

        agent
            .reply(&prompt, json!({ "stopReason": "end_turn" }))
            .await;
        handle.await.unwrap().unwrap();
        client.shutdown().await;
    }

    #[tokio::test]
    async fn update_todos_as_request_and_as_notification_is_merged() {
        let (client, mut agent, _) = connect(None, None).await;
        let (handle, mut rx) = prompt_in_background(&client, Duration::from_secs(5));
        let prompt = agent.expect_method("session/prompt").await;

        // Request form (what Cursor sends): must be answered, not -32601.
        agent
            .send(json!({
                "jsonrpc": "2.0", "id": 0, "method": "cursor/update_todos",
                "params": { "toolCallId": "t", "merge": false, "todos": [
                    { "id": "a", "content": "A", "status": "pending" },
                    { "id": "b", "content": "B", "status": "pending" } ] }
            }))
            .await;
        let reply = agent.recv().await;
        assert_eq!(reply["result"]["outcome"]["outcome"], "accepted");
        assert_eq!(
            reply["result"]["outcome"]["todos"]
                .as_array()
                .unwrap()
                .len(),
            2
        );

        // Notification form (what the docs describe): no reply expected.
        agent
            .send(json!({
                "jsonrpc": "2.0", "method": "cursor/update_todos",
                "params": { "toolCallId": "t", "merge": true, "todos": [
                    { "id": "b", "content": "B", "status": "completed" } ] }
            }))
            .await;
        agent
            .reply(&prompt, json!({ "stopReason": "end_turn" }))
            .await;
        handle.await.unwrap().unwrap();

        let mut lists = Vec::new();
        while let Ok((_, progress)) = rx.try_recv() {
            if let ExternalProgress::Todos { todos } = progress {
                lists.push(todos);
            }
        }
        assert_eq!(lists.len(), 2);
        assert_eq!(lists[1].len(), 2);
        assert_eq!(lists[1][1].status, "completed");
        client.shutdown().await;
    }

    #[tokio::test]
    async fn unattended_mode_keeps_the_legacy_answers() {
        // No handler: permission goes through the policy, interactive
        // extensions are "method not found" so Cursor falls back.
        let (client, mut agent, _) = connect(None, None).await;
        let (handle, _rx) = prompt_in_background(&client, Duration::from_secs(5));
        let prompt = agent.expect_method("session/prompt").await;

        agent
            .send(json!({
                "jsonrpc": "2.0", "id": 7, "method": "session/request_permission",
                "params": { "sessionId": "sess-1", "options": [
                    { "optionId": "allow-once", "name": "Allow", "kind": "allow_once" } ] }
            }))
            .await;
        let allowed = agent.recv().await;
        assert_eq!(allowed["result"]["outcome"]["optionId"], "allow-once");

        for (id, method) in [
            (8, "cursor/ask_question"),
            (9, "cursor/create_plan"),
            (10, "fs/read_text_file"),
        ] {
            agent
                .send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": {} }))
                .await;
            let reply = agent.recv().await;
            assert_eq!(reply["error"]["code"], -32601, "{method}");
        }
        for (id, method) in [(11, "cursor/task"), (12, "cursor/generate_image")] {
            agent
                .send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": {} }))
                .await;
            let reply = agent.recv().await;
            assert_eq!(
                reply["result"]["outcome"]["outcome"], "rejected",
                "{method}"
            );
        }

        agent
            .reply(&prompt, json!({ "stopReason": "end_turn" }))
            .await;
        handle.await.unwrap().unwrap();
        client.shutdown().await;
    }

    #[tokio::test]
    async fn idle_timeout_fires_without_activity_but_not_while_the_host_decides() {
        // The handler takes longer than the idle timeout to answer: the turn
        // must survive because a human is the one holding it up.
        let handler = Arc::new(ScriptedHandler {
            delay: Duration::from_millis(900),
            ..Default::default()
        });
        let (client, mut agent, _) = connect(Some(handler), None).await;
        let (handle, _rx) = prompt_in_background(&client, Duration::from_millis(400));
        let prompt = agent.expect_method("session/prompt").await;
        agent
            .send(json!({
                "jsonrpc": "2.0", "id": 3, "method": "session/request_permission",
                "params": { "sessionId": "sess-1", "options": [
                    { "optionId": "allow-always", "name": "A", "kind": "allow_always" } ] }
            }))
            .await;
        let reply = agent.recv().await;
        assert_eq!(reply["result"]["outcome"]["outcome"], "selected");
        agent
            .reply(&prompt, json!({ "stopReason": "end_turn" }))
            .await;
        handle.await.unwrap().unwrap();

        // Pure silence from the agent: idle timeout.
        let (handle, _rx) = prompt_in_background(&client, Duration::from_millis(400));
        let _silent = agent.expect_method("session/prompt").await;
        let error = handle.await.unwrap().unwrap_err();
        assert!(error.to_string().contains("no output"), "{error}");
        client.shutdown().await;
    }

    #[tokio::test]
    async fn cancel_is_a_notification_and_cancelled_stop_reason_is_returned() {
        let (client, mut agent, _) = connect(None, None).await;
        let (handle, _rx) = prompt_in_background(&client, Duration::from_secs(5));
        let prompt = agent.expect_method("session/prompt").await;
        client.cancel("sess-1").await;
        let cancel = agent.expect_method("session/cancel").await;
        assert!(cancel.get("id").is_none(), "cancel must be a notification");
        assert_eq!(cancel["params"]["sessionId"], "sess-1");
        agent
            .reply(&prompt, json!({ "stopReason": "cancelled" }))
            .await;
        let outcome = handle.await.unwrap().unwrap();
        assert_eq!(outcome.stop_reason, "cancelled");
        client.shutdown().await;
    }

    #[tokio::test]
    async fn set_mode_and_authenticate_use_the_documented_params() {
        let (client, mut agent, _) = connect(None, None).await;
        let c = client.clone();
        let mode = tokio::spawn(async move { c.set_mode("sess-1", "plan").await });
        let request = agent.expect_method("session/set_mode").await;
        assert_eq!(
            request["params"],
            json!({ "sessionId": "sess-1", "modeId": "plan" })
        );
        agent.reply(&request, json!({})).await;
        mode.await.unwrap().unwrap();

        let c = client.clone();
        let auth = tokio::spawn(async move { c.authenticate().await });
        let request = agent.expect_method("authenticate").await;
        assert_eq!(request["params"]["methodId"], "cursor_login");
        agent.reply(&request, json!({})).await;
        auth.await.unwrap().unwrap();
        client.shutdown().await;
    }

    #[tokio::test]
    async fn agent_exit_fails_the_running_prompt_and_marks_the_client_dead() {
        let (client, mut agent, _) = connect(None, None).await;
        let (handle, _rx) = prompt_in_background(&client, Duration::from_secs(5));
        let _prompt = agent.expect_method("session/prompt").await;
        assert!(client.is_alive());
        // The agent process goes away: its stdout closes.
        drop(agent);
        let error = handle.await.unwrap().unwrap_err();
        assert!(error.to_string().contains("ended unexpectedly"), "{error}");
        assert!(!client.is_alive());
        // A dead connection refuses new requests immediately.
        let (tx, _rx) = mpsc::channel(1);
        assert!(client
            .prompt_interactive("sess-1", "x", tx, Duration::from_secs(1))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn legacy_prompt_is_bounded_by_the_request_timeout() {
        let (client, mut agent, _) = connect(None, Some(Duration::from_millis(300))).await;
        let (tx, _rx) = mpsc::channel(4);
        let c = client.clone();
        let handle = tokio::spawn(async move { c.prompt("sess-1", "slow", tx).await });
        let _prompt = agent.expect_method("session/prompt").await;
        let error = handle.await.unwrap().unwrap_err();
        assert!(error.to_string().contains("timed out"), "{error}");
        client.shutdown().await;
    }

    /// What Cursor reports for Claude Sonnet 5.5 when the client declares the
    /// parameterized model picker.
    fn cursor_config_options(effort: &str) -> Value {
        json!([
            {
                "id": "mode", "name": "Mode", "category": "mode", "type": "select",
                "currentValue": "agent",
                "options": [
                    { "value": "agent", "name": "Agent" },
                    { "value": "plan", "name": "Plan" }
                ]
            },
            {
                "id": "model", "name": "Model", "category": "model", "type": "select",
                "currentValue": "claude-sonnet-5-5",
                "options": [
                    { "value": "claude-sonnet-5-5", "name": "Claude Sonnet 5.5" },
                    { "value": "claude-haiku-5-5", "name": "Claude Haiku 5.5" }
                ]
            },
            {
                "id": "reasoning_effort", "name": "Reasoning effort",
                "category": "thought_level", "type": "select",
                "currentValue": effort,
                "options": [
                    { "value": "low", "name": "Low" },
                    { "value": "medium", "name": "Medium" },
                    { "value": "high", "name": "High" },
                    { "value": "xhigh", "name": "Extra High" },
                    { "value": "max", "name": "Max" }
                ]
            }
        ])
    }

    #[test]
    fn config_options_parse_flat_and_grouped_selects() {
        let options = parse_config_options(Some(&cursor_config_options("high")));
        assert_eq!(
            options.iter().map(|o| o.id.as_str()).collect::<Vec<_>>(),
            vec!["mode", "model", "reasoning_effort"]
        );
        let level = config_option_by_category(&options, "thought_level").expect("thought level");
        assert_eq!(level.id, "reasoning_effort");
        assert_eq!(level.current.as_deref(), Some("high"));
        assert_eq!(
            level.value_ids(),
            vec!["low", "medium", "high", "xhigh", "max"]
        );
        assert_eq!(level.values[3].name, "Extra High");
        assert!(config_option_by_category(&options, "model_config").is_none());

        // Grouped selects flatten; entries without a value are skipped, a
        // missing name falls back to the value, a non-select has no values.
        let grouped = parse_config_options(Some(&json!([
            {
                "id": "model", "category": "model", "currentValue": "b",
                "options": [
                    { "group": "g1", "name": "Group 1", "options": [
                        { "value": "a", "name": "A" }, { "name": "no value" }
                    ] },
                    { "value": "b" }
                ]
            },
            { "id": "flag", "name": "Flag", "type": "boolean", "currentValue": true },
            { "name": "no id" }
        ])));
        assert_eq!(grouped.len(), 2);
        assert_eq!(grouped[0].value_ids(), vec!["a", "b"]);
        assert_eq!(grouped[0].values[1].name, "b");
        assert_eq!(grouped[0].name, "model");
        assert_eq!(grouped[1].current, None);
        assert!(grouped[1].values.is_empty());
        assert!(parse_config_options(None).is_empty());
        assert!(parse_config_options(Some(&json!({"not": "an array"}))).is_empty());
    }

    #[test]
    fn current_model_drops_the_variant_parameters() {
        let flat =
            json!({ "currentModelId": "claude-sonnet-5-5[context=300k,reasoning_effort=high]" });
        // Without a model option the `models` block answers.
        assert_eq!(
            current_model_of(&[], Some(&flat)).as_deref(),
            Some("claude-sonnet-5-5")
        );
        // The option wins over `models`.
        let options = parse_config_options(Some(&cursor_config_options("high")));
        assert_eq!(
            current_model_of(&options, Some(&flat)).as_deref(),
            Some("claude-sonnet-5-5")
        );
        assert_eq!(current_model_of(&[], None), None);
        assert_eq!(
            current_model_of(&[], Some(&json!({ "currentModelId": "[x]" }))),
            None
        );
        let info = AcpSessionInfo {
            session_id: "s".into(),
            current_mode: None,
            available_modes: Vec::new(),
            models: Some(json!({ "currentModelId": "gpt-6.1-sol[high]" })),
            config_options: Vec::new(),
        };
        assert_eq!(info.current_model().as_deref(), Some("gpt-6.1-sol"));
        assert_eq!(
            info.current_model_raw().as_deref(),
            Some("gpt-6.1-sol[high]")
        );
    }

    #[test]
    fn variant_ids_split_into_model_and_level() {
        assert_eq!(
            base_model_id("gpt-6.1-sol[high]").as_deref(),
            Some("gpt-6.1-sol")
        );
        assert_eq!(
            base_model_id("claude-sonnet-5-5").as_deref(),
            Some("claude-sonnet-5-5")
        );
        assert_eq!(base_model_id("[high]"), None);
        // Only a bare `[level]` is a level; parameter lists are not.
        assert_eq!(
            variant_level_of("gpt-6.1-sol[high]").as_deref(),
            Some("high")
        );
        assert_eq!(
            variant_level_of("gpt-6-astra[xhigh]").as_deref(),
            Some("xhigh")
        );
        assert_eq!(
            variant_level_of("claude-sonnet-5-5[context=300k,reasoning_effort=high]"),
            None
        );
        assert_eq!(variant_level_of("composer-2.5[fast=true]"), None);
        assert_eq!(variant_level_of("default[]"), None);
        assert_eq!(variant_level_of("claude-sonnet-5-5"), None);
        assert_eq!(variant_level_of("broken[high"), None);
    }

    #[tokio::test]
    async fn set_config_option_sends_the_documented_params_and_parses_the_update() {
        let (client, mut agent, _) = connect(None, None).await;
        let c = client.clone();
        let call = tokio::spawn(async move {
            c.set_config_option("sess-1", "reasoning_effort", "max")
                .await
        });
        let request = agent.expect_method("session/set_config_option").await;
        assert_eq!(
            request["params"],
            json!({ "sessionId": "sess-1", "configId": "reasoning_effort", "value": "max" })
        );
        agent
            .reply(
                &request,
                json!({ "configOptions": cursor_config_options("max") }),
            )
            .await;
        let options = call.await.unwrap().unwrap();
        let level = config_option_by_category(&options, "thought_level").unwrap();
        assert_eq!(level.current.as_deref(), Some("max"));

        // An agent that omits the list yields an empty one, an error surfaces.
        let c = client.clone();
        let call = tokio::spawn(async move { c.set_config_option("sess-1", "model", "x").await });
        let request = agent.expect_method("session/set_config_option").await;
        agent.reply(&request, json!({})).await;
        assert!(call.await.unwrap().unwrap().is_empty());

        let c = client.clone();
        let call = tokio::spawn(async move {
            c.set_config_option("sess-1", "reasoning_effort", "nope")
                .await
        });
        let request = agent.expect_method("session/set_config_option").await;
        agent
            .send(json!({ "jsonrpc": "2.0", "id": request["id"], "error": {
                "code": -32602, "message": "Invalid params",
                "data": { "message": "Unknown value nope" } } }))
            .await;
        let error = call.await.unwrap().unwrap_err().to_string();
        assert!(error.contains("Invalid params"), "{error}");
        client.shutdown().await;
    }

    /// `initialize` params of a client built with the given picker setting.
    async fn initialize_params(parameterized_model_picker: bool) -> Value {
        let (client_read, agent_out) = duplex(1 << 16);
        let (agent_in, client_write) = duplex(1 << 16);
        let mut opts = AcpClientOptions::cursor_cli(PathBuf::from("."));
        opts.parameterized_model_picker = parameterized_model_picker;
        let connecting = tokio::spawn(AcpClient::connect_streams(client_read, client_write, opts));
        let mut agent = StubAgent {
            lines: BufReader::new(agent_in).lines(),
            out: agent_out,
        };
        let init = agent.expect_method("initialize").await;
        agent.reply(&init, json!({ "protocolVersion": 1 })).await;
        let client = connecting.await.expect("join").expect("connect");
        client.shutdown().await;
        init["params"].clone()
    }

    #[tokio::test]
    async fn the_parameterized_model_picker_is_declared_only_on_request() {
        let plain = initialize_params(false).await;
        assert_eq!(plain["clientCapabilities"]["terminal"], false);
        assert!(plain["clientCapabilities"].get("_meta").is_none());

        let picker = initialize_params(true).await;
        assert_eq!(picker["clientCapabilities"]["terminal"], false);
        assert_eq!(
            picker["clientCapabilities"]["_meta"]["parameterizedModelPicker"],
            true
        );
    }
}
