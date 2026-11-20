//! Model calls made by the embedding host (`_kkagent/llm/*`).
//!
//! A provider of type `host` has no endpoint, no key and no network: every
//! model call is handed to the host that drives this process over ACP, which
//! sends it wherever it sends model calls and streams the answer back. This is
//! how a host keeps credentials, metering and budgets in one place — the worker
//! never holds a secret to leak.
//!
//! The wire is plain agent→host requests, so it needs nothing the connection
//! does not already have:
//!
//! 1. `_kkagent/llm/start` — the request; the host answers `{streamId}`.
//! 2. `_kkagent/llm/next` — long poll: the events since the last call, and
//!    `done` once the stream has ended. Called again at once while it has not.
//! 3. `_kkagent/llm/cancel` — notification when this side stops caring (the
//!    turn was interrupted and the call task aborted).
//!
//! A host that did not offer `llm` in `initialize` is never asked: the call
//! fails with a sentence saying so, and nothing falls back to a direct HTTP
//! provider.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use kkagent_llm::{
    register_provider_type, FirstTokenTimeoutError, LlmProvider, LlmRequest, ProviderSettings,
    StreamEvent, TokenUsage,
};
use kkagent_protocol::host::{
    HostLlmBatch, HostLlmEvent, HostLlmNext, HostLlmStart, HostLlmStarted, HostLlmThinking,
    METHOD_HOST_LLM_CANCEL, METHOD_HOST_LLM_NEXT, METHOD_HOST_LLM_START,
};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::host_channel::{HostChannel, HostChannelError, RequestOptions};

/// The provider type that selects this module in `[providers.X] type = "host"`.
pub const PROVIDER_TYPE: &str = "host";

/// How long the host may hold one `_kkagent/llm/next` while nothing happens.
const POLL_WAIT: Duration = Duration::from_secs(20);
/// Slack on top of [`POLL_WAIT`] before this side gives up on the host.
const POLL_SLACK: Duration = Duration::from_secs(40);
/// How long the host may take to accept a call.
const START_TIMEOUT: Duration = Duration::from_secs(60);

/// The host side of model calls: the channel, and whether the host offered it.
pub struct HostLlm {
    channel: Arc<dyn HostChannel>,
    offered: AtomicBool,
}

impl HostLlm {
    pub fn new(channel: Arc<dyn HostChannel>) -> Arc<Self> {
        Arc::new(Self {
            channel,
            offered: AtomicBool::new(false),
        })
    }

    /// Record what `initialize` said (`_meta["kkagent/host"].llm`).
    pub fn set_offered(&self, offered: bool) {
        self.offered.store(offered, Ordering::SeqCst);
    }

    pub fn offered(&self) -> bool {
        self.offered.load(Ordering::SeqCst)
    }

    /// Make `type = "host"` a provider type of this process.
    pub fn install(self: &Arc<Self>) -> anyhow::Result<()> {
        let host = self.clone();
        register_provider_type(
            PROVIDER_TYPE,
            Arc::new(move |_settings: &ProviderSettings, _model: &str| {
                Ok(Box::new(HostLlmProvider { host: host.clone() }) as Box<dyn LlmProvider>)
            }),
        )
    }
}

/// The provider behind `type = "host"`.
pub struct HostLlmProvider {
    host: Arc<HostLlm>,
}

impl HostLlmProvider {
    pub fn new(host: Arc<HostLlm>) -> Self {
        Self { host }
    }
}

/// Tells the host to drop a call this side abandoned: sent when the guard is
/// dropped before the stream ended (the turn was interrupted and the task that
/// holds the call was aborted, or the receiver went away).
struct CancelOnDrop {
    channel: Arc<dyn HostChannel>,
    stream_id: String,
    finished: bool,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let channel = self.channel.clone();
        let stream_id = std::mem::take(&mut self.stream_id);
        // Dropped inside the runtime (a task abort drops its futures there); if the
        // runtime is already gone nobody is left to tell.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                channel
                    .notify(METHOD_HOST_LLM_CANCEL, json!({ "streamId": stream_id }))
                    .await;
            });
        }
    }
}

fn describe(error: &HostChannelError) -> String {
    match error {
        HostChannelError::NotConnected => {
            "the host is not connected, so there is nobody to make the model call".to_string()
        }
        other => other.to_string(),
    }
}

fn usage_from_wire(usage: kkagent_protocol::host::HostLlmUsage) -> TokenUsage {
    TokenUsage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_creation_input_tokens: usage.cache_creation_input_tokens,
        cache_read_input_tokens: usage.cache_read_input_tokens,
        input_includes_cache: usage.input_includes_cache,
    }
}

fn stream_event(event: HostLlmEvent) -> StreamEvent {
    match event {
        HostLlmEvent::TextDelta { text } => StreamEvent::TextDelta(text),
        HostLlmEvent::ThinkingDelta { text } => StreamEvent::ThinkingDelta(text),
        HostLlmEvent::ToolUseStart { id, name } => StreamEvent::ToolUseStart { id, name },
        HostLlmEvent::ToolUseInputDelta { id, delta } => {
            StreamEvent::ToolUseInputDelta { id, delta }
        }
        HostLlmEvent::ToolUseEnd { id } => StreamEvent::ToolUseEnd { id },
        HostLlmEvent::MessageEnd { usage, stop_reason } => StreamEvent::MessageEnd {
            usage: usage_from_wire(usage),
            stop_reason,
        },
        HostLlmEvent::RateLimited {
            message,
            retry_after_ms,
        } => StreamEvent::RateLimited {
            message,
            retry_after: retry_after_ms.map(Duration::from_millis),
        },
        HostLlmEvent::Error { message } => StreamEvent::Error(message),
    }
}

/// Whether an event is real content, for the first-token deadline (keep-alives
/// and bookkeeping do not count).
fn is_content(event: &StreamEvent) -> bool {
    matches!(
        event,
        StreamEvent::TextDelta(_)
            | StreamEvent::ThinkingDelta(_)
            | StreamEvent::ToolUseStart { .. }
            | StreamEvent::ToolUseInputDelta { .. }
            | StreamEvent::ToolUseEnd { .. }
    )
}

fn start_params(request: &LlmRequest) -> anyhow::Result<HostLlmStart> {
    let messages = request
        .messages
        .iter()
        .map(serde_json::to_value)
        .collect::<Result<Vec<Value>, _>>()?;
    let tools = request
        .tools
        .iter()
        .map(serde_json::to_value)
        .collect::<Result<Vec<Value>, _>>()?;
    Ok(HostLlmStart {
        model: request.model.clone(),
        system: request.system.clone(),
        messages,
        tools,
        max_tokens: request.max_tokens,
        thinking: request.thinking.as_ref().map(|thinking| HostLlmThinking {
            budget_tokens: thinking.budget_tokens,
            adaptive: thinking.adaptive,
            effort: thinking.effort.clone(),
        }),
        prompt_cache_key: request.prompt_cache_key.clone(),
    })
}

#[async_trait]
impl LlmProvider for HostLlmProvider {
    async fn stream_chat(
        &self,
        request: LlmRequest,
        event_tx: mpsc::Sender<StreamEvent>,
    ) -> anyhow::Result<()> {
        if !self.host.offered() {
            anyhow::bail!(
                "provider type \"host\" needs the client to advertise `_meta[\"kkagent/host\"].llm` at \
                 initialize; the host did not offer model calls"
            );
        }
        let channel = self.host.channel.clone();
        let started: HostLlmStarted = {
            let params = serde_json::to_value(start_params(&request)?)?;
            let answer = channel
                .request(
                    METHOD_HOST_LLM_START,
                    params,
                    RequestOptions::with_timeout(START_TIMEOUT),
                )
                .await
                .map_err(|error| anyhow::anyhow!("host model call: {}", describe(&error)))?;
            serde_json::from_value(answer)
                .map_err(|error| anyhow::anyhow!("host model call: malformed start answer: {error}"))?
        };
        let mut guard = CancelOnDrop {
            channel: channel.clone(),
            stream_id: started.stream_id.clone(),
            finished: false,
        };

        let first_token = request
            .first_token_timeout
            .filter(|duration| !duration.is_zero());
        let deadline = first_token.map(|duration| Instant::now() + duration);
        let mut got_content = false;
        let mut ended = false;
        loop {
            let mut wait = POLL_WAIT;
            if let (Some(deadline), false) = (deadline, got_content) {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(FirstTokenTimeoutError {
                        timeout_ms: first_token.map(|d| d.as_millis() as u64).unwrap_or(0),
                        model: request.model.clone(),
                    }
                    .into());
                }
                wait = wait.min(remaining);
            }
            let next = HostLlmNext {
                stream_id: started.stream_id.clone(),
                wait_ms: Some(wait.as_millis() as u64),
            };
            let answer = channel
                .request(
                    METHOD_HOST_LLM_NEXT,
                    serde_json::to_value(&next)?,
                    RequestOptions::with_timeout(wait + POLL_SLACK),
                )
                .await
                .map_err(|error| anyhow::anyhow!("host model call: {}", describe(&error)))?;
            let batch: HostLlmBatch = serde_json::from_value(answer)
                .map_err(|error| anyhow::anyhow!("host model call: malformed batch: {error}"))?;
            for event in batch.events {
                let event = stream_event(event);
                got_content |= is_content(&event);
                ended |= matches!(event, StreamEvent::MessageEnd { .. } | StreamEvent::Error(_));
                if event_tx.send(event).await.is_err() {
                    // Nobody listens any more: the guard tells the host.
                    return Ok(());
                }
            }
            if batch.done {
                guard.finished = true;
                if !ended {
                    anyhow::bail!("host model call: the host ended the stream without a message end");
                }
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use kkagent_llm::{ChatMessage, ToolDef};
    use serde_json::json;

    /// A host that answers `start` with a fixed id and `next` from a script.
    struct ScriptedHost {
        script: Mutex<VecDeque<Result<Value, HostChannelError>>>,
        calls: Mutex<Vec<(String, Value)>>,
        notices: Mutex<Vec<(String, Value)>>,
    }

    impl ScriptedHost {
        fn new(script: Vec<Result<Value, HostChannelError>>) -> Arc<Self> {
            Arc::new(Self {
                script: Mutex::new(script.into()),
                calls: Mutex::new(Vec::new()),
                notices: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl HostChannel for ScriptedHost {
        async fn request(
            &self,
            method: &str,
            params: Value,
            _options: RequestOptions,
        ) -> Result<Value, HostChannelError> {
            self.calls.lock().unwrap().push((method.to_string(), params));
            if method == METHOD_HOST_LLM_START {
                return Ok(json!({"streamId": "s1"}));
            }
            self.script
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Err(HostChannelError::Closed))
        }
        async fn notify(&self, method: &str, params: Value) {
            self.notices.lock().unwrap().push((method.to_string(), params));
        }
    }

    fn request() -> LlmRequest {
        LlmRequest {
            model: "up-model".into(),
            messages: vec![ChatMessage::text("user", "hello")],
            tools: vec![ToolDef {
                name: "vfs__read_file".into(),
                description: "read".into(),
                input_schema: json!({"type": "object"}),
            }],
            max_tokens: Some(512),
            system: Some("be brief".into()),
            thinking: None,
            prompt_cache_key: None,
            first_token_timeout: None,
        }
    }

    async fn run(host: &Arc<ScriptedHost>, offered: bool, request: LlmRequest) -> (anyhow::Result<()>, Vec<StreamEvent>) {
        let llm = HostLlm::new(host.clone());
        llm.set_offered(offered);
        let provider = HostLlmProvider::new(llm);
        let (tx, mut rx) = mpsc::channel(64);
        let result = provider.stream_chat(request, tx).await;
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        (result, events)
    }

    #[tokio::test]
    async fn the_call_goes_to_the_host_and_its_events_come_back_in_order() {
        let host = ScriptedHost::new(vec![
            Ok(json!({"events": [{"type": "text_delta", "text": "he"}], "done": false})),
            Ok(json!({"events": [
                {"type": "text_delta", "text": "llo"},
                {"type": "tool_use_start", "id": "t1", "name": "vfs__read_file"},
                {"type": "tool_use_input_delta", "id": "t1", "delta": "{}"},
                {"type": "tool_use_end", "id": "t1"},
                {"type": "message_end", "usage": {"inputTokens": 7, "outputTokens": 2}, "stopReason": "tool_use"}
            ], "done": true})),
        ]);
        let (result, events) = run(&host, true, request()).await;
        result.unwrap();
        assert_eq!(events.len(), 6);
        assert!(matches!(&events[0], StreamEvent::TextDelta(t) if t == "he"));
        assert!(matches!(&events[2], StreamEvent::ToolUseStart { id, name } if id == "t1" && name == "vfs__read_file"));
        match &events[5] {
            StreamEvent::MessageEnd { usage, stop_reason } => {
                assert_eq!((usage.input_tokens, usage.output_tokens), (7, 2));
                assert_eq!(stop_reason.as_deref(), Some("tool_use"));
            }
            other => panic!("expected the message end, got {other:?}"),
        }
        // What the host was asked: the model, the system prompt, the messages and the tools, in the wire's names.
        let calls = host.calls.lock().unwrap();
        assert_eq!(calls[0].0, METHOD_HOST_LLM_START);
        assert_eq!(calls[0].1["model"], "up-model");
        assert_eq!(calls[0].1["system"], "be brief");
        assert_eq!(calls[0].1["maxTokens"], 512);
        assert_eq!(calls[0].1["messages"][0]["role"], "user");
        assert_eq!(calls[0].1["tools"][0]["name"], "vfs__read_file");
        assert!(calls.iter().skip(1).all(|(method, params)| method == METHOD_HOST_LLM_NEXT && params["streamId"] == "s1"));
        assert!(host.notices.lock().unwrap().is_empty(), "a call that ended is not cancelled");
    }

    #[tokio::test]
    async fn a_host_that_did_not_offer_model_calls_is_not_asked() {
        let host = ScriptedHost::new(vec![]);
        let (result, events) = run(&host, false, request()).await;
        let why = result.unwrap_err().to_string();
        assert!(why.contains("did not offer model calls"), "{why}");
        assert!(events.is_empty());
        assert!(host.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_host_that_goes_away_fails_the_call_and_says_so() {
        let host = ScriptedHost::new(vec![Ok(json!({"events": [{"type": "text_delta", "text": "x"}], "done": false})), Err(HostChannelError::Closed)]);
        let (result, events) = run(&host, true, request()).await;
        assert!(result.unwrap_err().to_string().contains("host disconnected"));
        assert_eq!(events.len(), 1, "what arrived before is kept");
    }

    #[tokio::test]
    async fn a_stream_that_ends_without_a_message_end_is_an_error() {
        let host = ScriptedHost::new(vec![Ok(json!({"events": [{"type": "text_delta", "text": "x"}], "done": true}))]);
        let (result, _events) = run(&host, true, request()).await;
        assert!(result.unwrap_err().to_string().contains("without a message end"));
    }

    #[tokio::test]
    async fn an_error_event_ends_the_stream_and_reaches_the_loop() {
        let host = ScriptedHost::new(vec![Ok(json!({"events": [{"type": "error", "message": "provider said no"}], "done": true}))]);
        let (result, events) = run(&host, true, request()).await;
        result.unwrap();
        assert!(matches!(&events[0], StreamEvent::Error(m) if m == "provider said no"));
    }

    #[tokio::test]
    async fn a_listener_that_went_away_makes_the_host_drop_the_call() {
        let host = ScriptedHost::new(vec![Ok(json!({"events": [{"type": "text_delta", "text": "x"}], "done": false}))]);
        let llm = HostLlm::new(host.clone());
        llm.set_offered(true);
        let provider = HostLlmProvider::new(llm);
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        provider.stream_chat(request(), tx).await.unwrap();
        // The cancel notice is sent from a spawned task: give it the turn it needs.
        for _ in 0..50 {
            if !host.notices.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let notices = host.notices.lock().unwrap();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].0, METHOD_HOST_LLM_CANCEL);
        assert_eq!(notices[0].1["streamId"], "s1");
    }

    #[tokio::test]
    async fn rate_limits_pass_through_with_their_wait() {
        let host = ScriptedHost::new(vec![Ok(json!({"events": [
            {"type": "rate_limited", "message": "slow", "retryAfterMs": 1500},
            {"type": "message_end"}
        ], "done": true}))]);
        let (result, events) = run(&host, true, request()).await;
        result.unwrap();
        assert!(matches!(&events[0], StreamEvent::RateLimited { retry_after: Some(d), .. } if *d == Duration::from_millis(1500)));
    }

    #[test]
    fn the_host_type_is_a_registered_provider_type_and_a_builtin_cannot_be_replaced() {
        let host = ScriptedHost::new(vec![]);
        let llm = HostLlm::new(host);
        llm.install().unwrap();
        let settings = ProviderSettings {
            provider_type: PROVIDER_TYPE.into(),
            ..Default::default()
        };
        assert!(kkagent_llm::create_provider(&settings, "m").is_ok());
        let replace = register_provider_type("openai", Arc::new(|_: &ProviderSettings, _: &str| anyhow::bail!("no")));
        assert!(replace.is_err());
    }
}
