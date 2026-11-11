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
            Ok(result) => Ok(tool_output_from_result(&result, &ctx.image)),
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

/// Largest image a host tool result may carry, measured on the decoded bytes
/// before kkagent re-encodes it to the provider-safe size. Bigger images are
/// replaced by a placeholder; the host should downscale or crop them.
pub const MAX_HOST_IMAGE_BYTES: usize = 10 * 1024 * 1024;
/// Most images kept from one result; the rest are named in the text.
pub const MAX_HOST_IMAGES_PER_RESULT: usize = 8;

/// Map the host's `_kkagent/tool/call` result to a tool output.
///
/// Accepted shapes: `{"content": "text"}`, `{"content": [{"type":"text",
/// "text": ".."}, ..]}` (ACP content blocks, optionally wrapped as
/// `{"type":"content","content":{..}}`), and a bare string. `isError: true`
/// marks a failed call. Anything else is passed to the model as JSON.
///
/// Inline image blocks (`{"type":"image","data":"<base64>","mimeType":".."}`,
/// also accepted as a `data:` URI) reach the model as images, normalized like
/// the images of any other tool and capped by [`MAX_HOST_IMAGE_BYTES`] and
/// [`MAX_HOST_IMAGES_PER_RESULT`]. An image that is refused leaves a
/// `[image omitted: ...]` line in the text instead.
pub fn tool_output_from_result(
    result: &Value,
    image_config: &kkagent_config::ImageConfig,
) -> ToolOutput {
    let is_error = result
        .get("isError")
        .or_else(|| result.get("is_error"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut images = Vec::new();
    let mut blocks = BlockReader {
        image_config,
        images: &mut images,
        seen: 0,
    };
    let text = match result {
        Value::String(text) => text.clone(),
        Value::Object(object) => match object.get("content") {
            Some(Value::String(text)) => text.clone(),
            Some(Value::Array(items)) => items
                .iter()
                .map(|block| blocks.text(block))
                .collect::<Vec<_>>()
                .join("\n"),
            Some(block @ Value::Object(_)) => blocks.text(block),
            _ => result.to_string(),
        },
        other => other.to_string(),
    };
    let mut output = if is_error {
        ToolOutput::error(text)
    } else {
        ToolOutput::success(text)
    };
    output.images = images;
    output
}

/// Walks content blocks, collecting images and rendering everything else as
/// text.
struct BlockReader<'a> {
    image_config: &'a kkagent_config::ImageConfig,
    images: &'a mut Vec<kkagent_tools::MediaOutput>,
    /// Image blocks met so far, accepted or not.
    seen: usize,
}

impl BlockReader<'_> {
    fn text(&mut self, block: &Value) -> String {
        if let Some(text) = block.as_str() {
            return text.to_string();
        }
        if let Some(text) = block.get("text").and_then(Value::as_str) {
            return text.to_string();
        }
        if block.get("type").and_then(Value::as_str) == Some("image") {
            return self.image(block);
        }
        if let Some(inner) = block.get("content") {
            return self.text(inner);
        }
        match block.get("type").and_then(Value::as_str) {
            Some(kind) => format!("[{kind} content omitted]"),
            None => block.to_string(),
        }
    }

    fn image(&mut self, block: &Value) -> String {
        self.seen += 1;
        let index = self.seen;
        let omitted = |reason: &str| format!("[image {index} omitted: {reason}]");
        if self.images.len() >= MAX_HOST_IMAGES_PER_RESULT {
            return omitted(&format!(
                "a result carries at most {MAX_HOST_IMAGES_PER_RESULT} images"
            ));
        }
        let Some(data) = block.get("data").and_then(Value::as_str) else {
            return omitted("only inline base64 `data` is supported");
        };
        // A `data:` URI is accepted as well as plain base64.
        let data = data
            .strip_prefix("data:")
            .and_then(|rest| rest.split_once(','))
            .map_or(data, |(_, payload)| payload);
        // Base64 is 4 characters per 3 bytes: refuse before decoding anything.
        let decoded_len = data.len() / 4 * 3;
        if decoded_len > MAX_HOST_IMAGE_BYTES {
            return omitted(&format!(
                "about {decoded_len} bytes exceeds the {MAX_HOST_IMAGE_BYTES}-byte limit"
            ));
        }
        match kkagent_tools::builtin::media::normalize_external_image(data, self.image_config) {
            Ok(image) => {
                self.images.push(image);
                format!("[image {index} attached]")
            }
            Err(error) => omitted(&error.to_string()),
        }
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
        let config = kkagent_config::ImageConfig::default();
        let text = |value: Value| tool_output_from_result(&value, &config).content;
        assert_eq!(text(json!("plain")), "plain");
        assert_eq!(text(json!({"content": "a"})), "a");
        assert_eq!(
            text(json!({"content": [
                {"type": "text", "text": "one"},
                {"type": "content", "content": {"type": "text", "text": "two"}},
                {"type": "resource", "uri": "file:///x"},
            ]})),
            "one\ntwo\n[resource content omitted]"
        );
        assert_eq!(
            text(json!({"content": {"type": "text", "text": "solo"}})),
            "solo"
        );
        assert_eq!(text(json!({"ok": true})), r#"{"ok":true}"#);
        assert!(
            tool_output_from_result(&json!({"content": "x", "isError": true}), &config).is_error
        );
        assert!(!tool_output_from_result(&json!({"content": "x"}), &config).is_error);
    }

    fn png_base64(width: u32, height: u32) -> String {
        use base64::Engine;
        let mut png = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::new(width, height))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        base64::engine::general_purpose::STANDARD.encode(png)
    }

    #[test]
    fn image_blocks_reach_the_model_as_images() {
        let config = kkagent_config::ImageConfig::default();
        let data = png_base64(4, 4);
        let output = tool_output_from_result(
            &json!({"content": [
                {"type": "text", "text": "chart:"},
                {"type": "image", "data": data, "mimeType": "image/png"},
                {"type": "content", "content": {"type": "image", "data": format!("data:image/png;base64,{data}")}},
            ]}),
            &config,
        );
        assert!(!output.is_error);
        assert_eq!(output.images.len(), 2, "{}", output.content);
        assert!(output
            .images
            .iter()
            .all(|image| image.media_type == "image/jpeg"));
        assert_eq!(
            output.content,
            "chart:\n[image 1 attached]\n[image 2 attached]"
        );
        assert!(
            !output.content.contains(&data),
            "the base64 payload never enters the text"
        );
    }

    #[test]
    fn refused_images_leave_a_reason_not_a_silent_drop() {
        let config = kkagent_config::ImageConfig::default();
        // Not an image at all.
        let broken = tool_output_from_result(
            &json!({"content": [{"type": "image", "data": "bm90IGFuIGltYWdl"}]}),
            &config,
        );
        assert!(broken.images.is_empty());
        assert!(
            broken.content.starts_with("[image 1 omitted: "),
            "{}",
            broken.content
        );
        // Referenced by URI only.
        let by_uri = tool_output_from_result(
            &json!({"content": [{"type": "image", "uri": "file:///x.png"}]}),
            &config,
        );
        assert!(by_uri.images.is_empty());
        assert!(
            by_uri.content.contains("inline base64"),
            "{}",
            by_uri.content
        );
        // Over the size limit: refused before anything is decoded.
        let oversized = "A".repeat((MAX_HOST_IMAGE_BYTES / 3 + 1) * 4);
        let big = tool_output_from_result(
            &json!({"content": [{"type": "image", "data": oversized}]}),
            &config,
        );
        assert!(big.images.is_empty());
        assert!(big.content.contains("limit"), "{}", big.content);
    }

    #[test]
    fn a_result_keeps_at_most_a_bounded_number_of_images() {
        let config = kkagent_config::ImageConfig::default();
        let data = png_base64(2, 2);
        let blocks: Vec<Value> = (0..MAX_HOST_IMAGES_PER_RESULT + 2)
            .map(|_| json!({"type": "image", "data": data}))
            .collect();
        let output = tool_output_from_result(&json!({"content": blocks}), &config);
        assert_eq!(output.images.len(), MAX_HOST_IMAGES_PER_RESULT);
        assert!(output.content.contains("at most"), "{}", output.content);
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
