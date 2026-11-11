//! Pre-execution review of tool calls by the embedding host.
//!
//! A [`ToolGate`] sits between "kkagent's own permission chain let this call
//! through" and "the tool runs". It is how a host that drives kkagent in
//! `yolo` / `auto` mode still sees every call first and can veto it, without
//! having to know tool names in advance.
//!
//! The gate lives in the tool registry and is stamped onto every subagent
//! launch, so delegating work cannot step around it.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use serde_json::Value;

/// One tool call as the reviewer sees it.
#[derive(Debug, Clone)]
pub struct ToolCallReview {
    pub session_id: String,
    pub tool_call_id: String,
    pub name: String,
    pub input: Value,
    pub cwd: String,
    pub read_only: bool,
    /// Cooperative cancellation flag of the session: a review that is still
    /// pending when it flips is abandoned (and denied).
    pub interrupted: Option<Arc<AtomicBool>>,
}

/// The reviewer's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateVerdict {
    Allow,
    /// The call is refused; the text is returned to the model.
    Deny(String),
}

/// Boxed future returned by [`ToolGate::review`].
pub type GateFuture<'a> = Pin<Box<dyn Future<Output = GateVerdict> + Send + 'a>>;

/// Decides whether a tool call may run.
pub trait ToolGate: Send + Sync {
    /// Whether calls to the tool `name` (read-only or not) are reviewed at
    /// all.
    fn reviews(&self, name: &str, read_only: bool) -> bool;

    /// Review one call. Implementations fail closed: when no verdict can be
    /// obtained the answer is [`GateVerdict::Deny`].
    fn review<'a>(&'a self, call: &'a ToolCallReview) -> GateFuture<'a>;
}

/// A clonable, debuggable handle to a gate that can ride along in
/// configuration structs. It is never serialized: a persisted configuration
/// that is revived later simply has no gate.
#[derive(Clone)]
pub struct ToolGateHandle(pub Arc<dyn ToolGate>);

impl fmt::Debug for ToolGateHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ToolGateHandle(..)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DenyAll;

    impl ToolGate for DenyAll {
        fn reviews(&self, _name: &str, _read_only: bool) -> bool {
            true
        }
        fn review<'a>(&'a self, call: &'a ToolCallReview) -> GateFuture<'a> {
            Box::pin(async move { GateVerdict::Deny(format!("no {}", call.name)) })
        }
    }

    #[tokio::test]
    async fn a_handle_is_cloneable_and_callable() {
        let handle = ToolGateHandle(Arc::new(DenyAll));
        let copy = handle.clone();
        let call = ToolCallReview {
            session_id: "s".into(),
            tool_call_id: "c".into(),
            name: "Bash".into(),
            input: Value::Null,
            cwd: "/w".into(),
            read_only: false,
            interrupted: None,
        };
        assert_eq!(
            copy.0.review(&call).await,
            GateVerdict::Deny("no Bash".into())
        );
        assert_eq!(format!("{handle:?}"), "ToolGateHandle(..)");
    }
}
