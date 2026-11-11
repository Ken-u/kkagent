//! Host review of tool calls (`_kkagent/tool/review`).
//!
//! `yolo` and `auto` modes let kkagent run tools without asking anyone. A host
//! that still wants to see (and be able to veto) every call, without knowing
//! tool names in advance, asks for a review: right before a tool runs kkagent
//! sends the call to the host and runs it only if the host allows it.
//!
//! The gate fails closed. A host that does not answer in time, disconnects,
//! returns an error or returns something unreadable has denied the call.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use kkagent_protocol::host::{
    HostToolApproval, HostToolSpec, ToolReviewDecision, ToolReviewMode, ToolReviewReply,
    ToolReviewRequest, ToolReviewSpec, METHOD_HOST_TOOL_REVIEW,
};
use kkagent_protocol::{GateFuture, GateVerdict, ToolCallReview, ToolGate};

use crate::host_channel::{HostChannel, HostChannelError, RequestOptions};

/// A [`ToolGate`] backed by the host channel.
pub struct HostToolGate {
    channel: Arc<dyn HostChannel>,
    mode: ToolReviewMode,
    timeout: Duration,
    /// Host tools the host executes itself under its own policy
    /// (`approval: "host"`); the host already sees those calls.
    exempt: HashSet<String>,
}

impl HostToolGate {
    pub fn new(
        channel: Arc<dyn HostChannel>,
        spec: &ToolReviewSpec,
        host_tools: &[HostToolSpec],
    ) -> Self {
        Self {
            channel,
            mode: spec.mode,
            timeout: Duration::from_secs(spec.timeout_secs()),
            exempt: host_tools
                .iter()
                .filter(|tool| tool.approval == HostToolApproval::Host)
                .map(|tool| tool.name.clone())
                .collect(),
        }
    }

    /// Build the gate for a session, or `None` when it has no review.
    pub fn for_session(
        channel: &Arc<dyn HostChannel>,
        review: Option<&ToolReviewSpec>,
        host_tools: &[HostToolSpec],
    ) -> Option<Arc<dyn ToolGate>> {
        let review = review.filter(|review| review.mode != ToolReviewMode::Off)?;
        Some(Arc::new(Self::new(channel.clone(), review, host_tools)))
    }

    fn denial(reason: &str) -> GateVerdict {
        GateVerdict::Deny(format!(
            "The host did not approve this tool call: {reason}. The tool was not run."
        ))
    }
}

impl ToolGate for HostToolGate {
    fn reviews(&self, name: &str, read_only: bool) -> bool {
        self.mode.reviews(read_only) && !self.exempt.contains(name)
    }

    fn review<'a>(&'a self, call: &'a ToolCallReview) -> GateFuture<'a> {
        Box::pin(async move {
            let params = match serde_json::to_value(ToolReviewRequest {
                session_id: call.session_id.clone(),
                tool_call_id: call.tool_call_id.clone(),
                name: call.name.clone(),
                input: call.input.clone(),
                cwd: call.cwd.clone(),
                read_only: call.read_only,
            }) {
                Ok(params) => params,
                Err(error) => return Self::denial(&format!("review request not built: {error}")),
            };
            let options = RequestOptions {
                cancel: call.interrupted.clone(),
                timeout: Some(self.timeout),
            };
            match self
                .channel
                .request(METHOD_HOST_TOOL_REVIEW, params, options)
                .await
            {
                Ok(value) => match serde_json::from_value::<ToolReviewReply>(value) {
                    Ok(ToolReviewReply {
                        decision: ToolReviewDecision::Allow,
                        ..
                    }) => GateVerdict::Allow,
                    Ok(ToolReviewReply {
                        decision: ToolReviewDecision::Deny,
                        reason,
                    }) => Self::denial(
                        reason
                            .as_deref()
                            .filter(|reason| !reason.trim().is_empty())
                            .unwrap_or("denied by the host"),
                    ),
                    Err(error) => Self::denial(&format!("unreadable review answer ({error})")),
                },
                Err(HostChannelError::TimedOut) => Self::denial(&format!(
                    "no answer within {} seconds",
                    self.timeout.as_secs()
                )),
                Err(HostChannelError::Cancelled) => Self::denial("the turn was interrupted"),
                Err(error) => Self::denial(&format!("review failed ({error})")),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use async_trait::async_trait;

    #[derive(Default)]
    struct MockHost {
        answers: Mutex<VecDeque<Result<Value, HostChannelError>>>,
        requests: Mutex<Vec<(String, Value, RequestOptions)>>,
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
        async fn notify(&self, _method: &str, _params: Value) {}
    }

    fn call(name: &str, read_only: bool) -> ToolCallReview {
        ToolCallReview {
            session_id: "s1".into(),
            tool_call_id: "c1".into(),
            name: name.into(),
            input: json!({"command": "rm -rf build"}),
            cwd: "/work".into(),
            read_only,
            interrupted: None,
        }
    }

    fn gate(host: &Arc<MockHost>, mode: ToolReviewMode) -> Arc<dyn ToolGate> {
        let channel: Arc<dyn HostChannel> = host.clone();
        HostToolGate::for_session(
            &channel,
            Some(&ToolReviewSpec {
                mode,
                timeout_secs: Some(7),
            }),
            &[],
        )
        .unwrap()
    }

    #[tokio::test]
    async fn an_allowed_call_carries_the_whole_call_to_the_host() {
        let host = MockHost::answering(vec![Ok(json!({"decision": "allow"}))]);
        let gate = gate(&host, ToolReviewMode::All);
        assert_eq!(gate.review(&call("Bash", false)).await, GateVerdict::Allow);

        let requests = host.requests.lock().unwrap();
        let (method, params, options) = &requests[0];
        assert_eq!(method, METHOD_HOST_TOOL_REVIEW);
        assert_eq!(params["sessionId"], "s1");
        assert_eq!(params["toolCallId"], "c1");
        assert_eq!(params["name"], "Bash");
        assert_eq!(params["input"]["command"], "rm -rf build");
        assert_eq!(params["cwd"], "/work");
        assert_eq!(params["readOnly"], false);
        assert_eq!(options.timeout, Some(Duration::from_secs(7)));
    }

    #[tokio::test]
    async fn a_denial_reaches_the_model_with_the_hosts_reason() {
        let host = MockHost::answering(vec![Ok(json!({"decision": "deny", "reason": "no rm"}))]);
        let gate = gate(&host, ToolReviewMode::All);
        match gate.review(&call("Bash", false)).await {
            GateVerdict::Deny(text) => {
                assert!(text.contains("no rm"), "{text}");
                assert!(text.contains("not run"), "{text}");
            }
            other => panic!("expected a denial, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn every_failure_to_get_an_answer_is_a_denial() {
        let failures = [
            Err(HostChannelError::TimedOut),
            Err(HostChannelError::Closed),
            Err(HostChannelError::NotConnected),
            Err(HostChannelError::Cancelled),
            Err(HostChannelError::Rpc {
                code: -32601,
                message: "method not found".into(),
            }),
            Ok(json!({"decision": "maybe"})),
            Ok(json!("allow")),
            Ok(Value::Null),
        ];
        for failure in failures {
            let host = MockHost::answering(vec![failure.clone()]);
            let gate = gate(&host, ToolReviewMode::All);
            assert!(
                matches!(
                    gate.review(&call("Bash", false)).await,
                    GateVerdict::Deny(_)
                ),
                "{failure:?} must deny"
            );
        }
    }

    #[test]
    fn modes_choose_which_calls_are_reviewed() {
        let host = MockHost::answering(vec![]);
        let mutating = gate(&host, ToolReviewMode::Mutating);
        assert!(mutating.reviews("Bash", false));
        assert!(!mutating.reviews("Read", true));
        let all = gate(&host, ToolReviewMode::All);
        assert!(all.reviews("Read", true));
        assert!(all.reviews("Bash", false));
    }

    #[test]
    fn off_builds_no_gate_and_host_executed_tools_are_exempt() {
        let host = MockHost::answering(vec![]);
        let channel: Arc<dyn HostChannel> = host.clone();
        assert!(HostToolGate::for_session(&channel, None, &[]).is_none());
        assert!(HostToolGate::for_session(
            &channel,
            Some(&ToolReviewSpec {
                mode: ToolReviewMode::Off,
                timeout_secs: None
            }),
            &[]
        )
        .is_none());

        let tools = vec![
            HostToolSpec {
                name: "host_run".into(),
                description: String::new(),
                input_schema: json!({"type": "object"}),
                read_only: false,
                approval: HostToolApproval::Host,
                timeout_secs: None,
            },
            HostToolSpec {
                name: "host_ask".into(),
                description: String::new(),
                input_schema: json!({"type": "object"}),
                read_only: false,
                approval: HostToolApproval::Ask,
                timeout_secs: None,
            },
        ];
        let gate = HostToolGate::for_session(
            &channel,
            Some(&ToolReviewSpec {
                mode: ToolReviewMode::All,
                timeout_secs: None,
            }),
            &tools,
        )
        .unwrap();
        assert!(!gate.reviews("host_run", false));
        assert!(gate.reviews("host_ask", false));
        assert!(gate.reviews("Bash", false));
    }
}
