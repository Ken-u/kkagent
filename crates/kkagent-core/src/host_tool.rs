//! Tools injected by the embedding host.
//!
//! The host describes a tool in `_meta["kkagent/host"].hostTools` when it
//! creates a session. kkagent shows it to the model like any other tool and,
//! when the model calls it, forwards the call to the host as an
//! `_kkagent/tool/call` request and returns the host's answer.
//!
//! A host tool is an ordinary [`Tool`] in the per-turn registry, so it is
//! subject to everything else: the strict tool allowlist (an unlisted host tool
//! is never registered), the permission chain and plan mode. It can never take
//! the name of another tool.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use kkagent_protocol::host::{
    HostToolApproval, HostToolSpec, DEFAULT_HOST_TOOL_TIMEOUT_SECS, METHOD_HOST_TOOL_CALL,
    METHOD_HOST_TOOL_CANCEL,
};
use kkagent_tools::{
    tool_accesses, RegisterError, Tool, ToolAccesses, ToolContext, ToolOutput, ToolRegistry,
};
use serde_json::{json, Value};

use crate::host_channel::{HostChannel, HostChannelError, RequestOptions};
use crate::permission::HostToolPolicy;

/// Name prefixes that belong to other tool families.
const RESERVED_PREFIXES: &[&str] = &["mcp__", "acp:"];

/// A host-executed tool.
pub struct HostTool {
    spec: HostToolSpec,
    channel: Arc<dyn HostChannel>,
}

impl HostTool {
    pub fn new(spec: HostToolSpec, channel: Arc<dyn HostChannel>) -> Self {
        Self { spec, channel }
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(
            self.spec
                .timeout_secs
                .unwrap_or(DEFAULT_HOST_TOOL_TIMEOUT_SECS),
        )
    }
}

#[async_trait]
impl Tool for HostTool {
    fn name(&self) -> &str {
        &self.spec.name
    }

    fn description(&self) -> &str {
        &self.spec.description
    }

    fn parameters_schema(&self) -> Value {
        self.spec.input_schema.clone()
    }

    fn read_only(&self) -> bool {
        self.spec.read_only
    }

    fn default_approve(&self) -> bool {
        self.spec.approval == HostToolApproval::Host
    }

    fn accesses(&self, _input: &Value, _working_dir: &Path) -> ToolAccesses {
        // kkagent cannot see what the host does: reads may overlap freely,
        // anything else runs alone.
        if self.spec.read_only {
            tool_accesses::none()
        } else {
            tool_accesses::all()
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let tool_call_id = ctx.tool_call_id.clone().unwrap_or_default();
        let params = json!({
            "sessionId": ctx.session_id,
            "toolCallId": tool_call_id,
            "name": self.spec.name,
            "input": input,
            "cwd": ctx.working_dir,
        });
        let options = RequestOptions {
            cancel: ctx.interrupted.clone(),
            timeout: Some(self.timeout()),
        };
        match self
            .channel
            .request(METHOD_HOST_TOOL_CALL, params, options)
            .await
        {
            Ok(result) => Ok(tool_output_from_result(&result)),
            Err(error @ (HostChannelError::Cancelled | HostChannelError::TimedOut)) => {
                // Tell the host which call to stop (`$/cancel_request` already
                // told it which request).
                self.channel
                    .notify(
                        METHOD_HOST_TOOL_CANCEL,
                        json!({"sessionId": ctx.session_id, "toolCallId": tool_call_id}),
                    )
                    .await;
                Ok(ToolOutput::error(format!(
                    "Host tool `{}` {}.",
                    self.spec.name,
                    match error {
                        HostChannelError::Cancelled => "was cancelled",
                        _ => "timed out",
                    }
                )))
            }
            Err(error) => Ok(ToolOutput::error(format!(
                "Host tool `{}` failed: {error}",
                self.spec.name
            ))),
        }
    }
}

/// Map the host's `_kkagent/tool/call` result to a tool output.
///
/// Accepted shapes: `{"content": "text"}`, `{"content": [{"type":"text",
/// "text": ".."}, ..]}` (ACP content blocks, optionally wrapped as
/// `{"type":"content","content":{..}}`), and a bare string. `isError: true`
/// marks a failed call. Anything else is passed to the model as JSON.
pub fn tool_output_from_result(result: &Value) -> ToolOutput {
    let is_error = result
        .get("isError")
        .or_else(|| result.get("is_error"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let text = match result {
        Value::String(text) => text.clone(),
        Value::Object(object) => match object.get("content") {
            Some(Value::String(text)) => text.clone(),
            Some(Value::Array(blocks)) => blocks
                .iter()
                .map(content_block_text)
                .collect::<Vec<_>>()
                .join("\n"),
            Some(block @ Value::Object(_)) => content_block_text(block),
            _ => result.to_string(),
        },
        other => other.to_string(),
    };
    if is_error {
        ToolOutput::error(text)
    } else {
        ToolOutput::success(text)
    }
}

fn content_block_text(block: &Value) -> String {
    if let Some(text) = block.as_str() {
        return text.to_string();
    }
    if let Some(text) = block.get("text").and_then(Value::as_str) {
        return text.to_string();
    }
    if let Some(inner) = block.get("content") {
        return content_block_text(inner);
    }
    match block.get("type").and_then(Value::as_str) {
        Some(kind) => format!("[{kind} content omitted]"),
        None => block.to_string(),
    }
}

/// Why a host tool was not registered for a session.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HostToolError {
    #[error("host tool `{0}` uses a name reserved for another tool family")]
    ReservedName(String),
    #[error("host tool `{0}` conflicts with a built-in tool name")]
    BuiltinConflict(String),
}

/// Check the tool names a host proposes. `builtin_names` are the names
/// kkagent registers itself; a host tool may not shadow any of them.
pub fn validate_host_tool_names(
    specs: &[HostToolSpec],
    builtin_names: &[String],
) -> Result<(), HostToolError> {
    for spec in specs {
        if RESERVED_PREFIXES
            .iter()
            .any(|prefix| spec.name.starts_with(prefix))
        {
            return Err(HostToolError::ReservedName(spec.name.clone()));
        }
        if builtin_names.iter().any(|name| name == &spec.name) {
            return Err(HostToolError::BuiltinConflict(spec.name.clone()));
        }
    }
    Ok(())
}

/// Register the session's host tools in `registry`.
///
/// Allowlisted names only: a tool the allowlist does not permit is dropped by
/// the registry, and a name that is already taken is never replaced. Returns
/// the policies the permission chain needs and the refusals (for logging).
pub fn register_host_tools(
    registry: &mut ToolRegistry,
    specs: &[HostToolSpec],
    channel: &Arc<dyn HostChannel>,
) -> (Vec<HostToolPolicy>, Vec<RegisterError>) {
    let mut policies = Vec::new();
    let mut refused = Vec::new();
    for spec in specs {
        let tool = HostTool::new(spec.clone(), channel.clone());
        match registry.register_unique(Arc::new(tool)) {
            Ok(()) => policies.push(HostToolPolicy {
                name: spec.name.clone(),
                host_approves: spec.approval == HostToolApproval::Host,
                read_only: spec.read_only,
            }),
            Err(error) => refused.push(error),
        }
    }
    (policies, refused)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicBool;
    use std::sync::Mutex;

    use kkagent_protocol::ToolAllowlist;

    /// A scripted host: records requests and notifications, answers from a
    /// queue.
    #[derive(Default)]
    struct MockHost {
        answers: Mutex<VecDeque<Result<Value, HostChannelError>>>,
        requests: Mutex<Vec<(String, Value, RequestOptions)>>,
        notifications: Mutex<Vec<(String, Value)>>,
    }

    impl MockHost {
        fn answering(answers: Vec<Result<Value, HostChannelError>>) -> Arc<Self> {
            Arc::new(Self {
                answers: Mutex::new(answers.into()),
                ..Default::default()
            })
        }
    }

    #[async_trait]
    impl HostChannel for MockHost {
        async fn request(
            &self,
            method: &str,
            params: Value,
            options: RequestOptions,
        ) -> Result<Value, HostChannelError> {
            self.requests
                .lock()
                .unwrap()
                .push((method.to_string(), params, options));
            self.answers
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Err(HostChannelError::Closed))
        }

        async fn notify(&self, method: &str, params: Value) {
            self.notifications
                .lock()
                .unwrap()
                .push((method.to_string(), params));
        }
    }

    fn spec(name: &str) -> HostToolSpec {
        serde_json::from_value(json!({
            "name": name,
            "description": "does a thing",
            "inputSchema": {"type": "object", "properties": {"x": {"type": "string"}}},
        }))
        .unwrap()
    }

    fn context() -> ToolContext {
        ToolContext {
            working_dir: std::env::temp_dir(),
            session_id: "s1".into(),
            turn_id: "s1:1".into(),
            plan_file_path: None,
            image: Default::default(),
            tool_call_id: Some("call-7".into()),
            interrupted: Some(Arc::new(AtomicBool::new(false))),
            tools_config: Default::default(),
            model_alias: None,
        }
    }

    #[tokio::test]
    async fn call_round_trip_carries_session_call_id_and_input() {
        let host = MockHost::answering(vec![Ok(json!({"content": "done"}))]);
        let tool = HostTool::new(spec("host_run"), host.clone());
        assert_eq!(tool.name(), "host_run");
        assert_eq!(
            tool.parameters_schema()["properties"]["x"]["type"],
            "string"
        );

        let output = tool.execute(json!({"x": "1"}), &context()).await.unwrap();
        assert_eq!(output.content, "done");
        assert!(!output.is_error);

        let requests = host.requests.lock().unwrap();
        let (method, params, options) = &requests[0];
        assert_eq!(method, METHOD_HOST_TOOL_CALL);
        assert_eq!(params["sessionId"], "s1");
        assert_eq!(params["toolCallId"], "call-7");
        assert_eq!(params["name"], "host_run");
        assert_eq!(params["input"]["x"], "1");
        assert!(
            options.cancel.is_some(),
            "the turn's interrupt flag is wired in"
        );
        assert_eq!(
            options.timeout,
            Some(Duration::from_secs(DEFAULT_HOST_TOOL_TIMEOUT_SECS))
        );
    }

    #[tokio::test]
    async fn host_errors_become_error_outputs_the_model_can_read() {
        for (answer, expected) in [
            (
                Err(HostChannelError::Rpc {
                    code: -32000,
                    message: "boom".into(),
                }),
                "boom",
            ),
            (Err(HostChannelError::Closed), "disconnected"),
            (Err(HostChannelError::NotConnected), "not connected"),
        ] {
            let host = MockHost::answering(vec![answer]);
            let tool = HostTool::new(spec("host_run"), host);
            let output = tool.execute(json!({}), &context()).await.unwrap();
            assert!(output.is_error);
            assert!(output.content.contains(expected), "{}", output.content);
        }
    }

    #[tokio::test]
    async fn cancellation_and_timeout_notify_the_host_which_call_to_stop() {
        for (answer, wording) in [
            (Err(HostChannelError::Cancelled), "cancelled"),
            (Err(HostChannelError::TimedOut), "timed out"),
        ] {
            let host = MockHost::answering(vec![answer]);
            let tool = HostTool::new(spec("host_run"), host.clone());
            let output = tool.execute(json!({}), &context()).await.unwrap();
            assert!(output.is_error);
            assert!(output.content.contains(wording));
            let notifications = host.notifications.lock().unwrap();
            assert_eq!(notifications.len(), 1);
            assert_eq!(notifications[0].0, METHOD_HOST_TOOL_CANCEL);
            assert_eq!(notifications[0].1["toolCallId"], "call-7");
            assert_eq!(notifications[0].1["sessionId"], "s1");
        }
    }

    #[test]
    fn results_are_read_from_every_documented_shape() {
        let text = |value: Value| tool_output_from_result(&value).content;
        assert_eq!(text(json!("plain")), "plain");
        assert_eq!(text(json!({"content": "a"})), "a");
        assert_eq!(
            text(json!({"content": [
                {"type": "text", "text": "one"},
                {"type": "content", "content": {"type": "text", "text": "two"}},
                {"type": "image", "data": "..."},
            ]})),
            "one\ntwo\n[image content omitted]"
        );
        assert_eq!(
            text(json!({"content": {"type": "text", "text": "solo"}})),
            "solo"
        );
        assert_eq!(text(json!({"ok": true})), r#"{"ok":true}"#);
        assert!(tool_output_from_result(&json!({"content": "x", "isError": true})).is_error);
        assert!(!tool_output_from_result(&json!({"content": "x"})).is_error);
    }

    #[test]
    fn approval_and_access_follow_the_spec() {
        let host = MockHost::answering(vec![]);
        let mut mutating = spec("host_write");
        mutating.approval = HostToolApproval::Ask;
        let tool = HostTool::new(mutating, host.clone());
        assert!(!tool.default_approve());
        assert_eq!(
            tool.accesses(&json!({}), Path::new(".")),
            tool_accesses::all()
        );

        let mut reader = spec("host_read");
        reader.read_only = true;
        let tool = HostTool::new(reader, host);
        assert!(tool.default_approve() && tool.read_only());
        assert_eq!(
            tool.accesses(&json!({}), Path::new(".")),
            tool_accesses::none()
        );
    }

    #[test]
    fn registration_obeys_the_allowlist_and_never_shadows() {
        let host: Arc<dyn HostChannel> = MockHost::answering(vec![]);
        let specs = vec![spec("host_a"), spec("host_b"), spec("Bash")];

        // Unrestricted registry: everything but the name already taken.
        let mut registry = ToolRegistry::new();
        let builtin = Arc::new(HostTool::new(spec("Bash"), host.clone()));
        registry.register(builtin);
        let (policies, refused) = register_host_tools(&mut registry, &specs, &host);
        assert_eq!(
            policies.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            vec!["host_a", "host_b"]
        );
        assert_eq!(refused, vec![RegisterError::NameTaken("Bash".into())]);

        // Strict allowlist: only the listed host tool exists.
        let mut registry = ToolRegistry::new();
        registry.set_allowlist(&ToolAllowlist::new(["host_a"]));
        let (policies, refused) = register_host_tools(&mut registry, &specs, &host);
        assert_eq!(policies.len(), 1);
        assert!(registry.get("host_a").is_some());
        assert!(registry.get("host_b").is_none());
        assert_eq!(refused.len(), 2);

        // Deny-all: nothing.
        let mut registry = ToolRegistry::new();
        registry.set_allowlist(&ToolAllowlist::deny_all());
        let (policies, _) = register_host_tools(&mut registry, &specs, &host);
        assert!(policies.is_empty());
        assert!(registry.list().is_empty());
    }

    #[test]
    fn names_may_not_use_reserved_prefixes_or_shadow_built_ins() {
        let builtin = vec!["Bash".to_string(), "Read".to_string()];
        assert_eq!(
            validate_host_tool_names(&[spec("host_ok")], &builtin),
            Ok(())
        );
        assert_eq!(
            validate_host_tool_names(&[spec("Bash")], &builtin),
            Err(HostToolError::BuiltinConflict("Bash".into()))
        );
        assert_eq!(
            validate_host_tool_names(&[spec("mcp__evil__x")], &builtin),
            Err(HostToolError::ReservedName("mcp__evil__x".into()))
        );
        // The wire-level name check rejects `acp:`; the prefix guard is a
        // second line of defense for hosts built against older validation.
        let mut odd = spec("acp-ok");
        odd.name = "acp:cursor".into();
        assert_eq!(
            validate_host_tool_names(&[odd], &builtin),
            Err(HostToolError::ReservedName("acp:cursor".into()))
        );
    }
}
