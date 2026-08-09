//! Session external hook event vocabulary (kimi-code session hooks aligned).

use serde_json::{json, Value};
use std::sync::Arc;

pub const HOOK_EVENT_TYPES: &[&str] = &[
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "PermissionRequest",
    "PermissionResult",
    "UserPromptSubmit",
    "Stop",
    "StopFailure",
    "Interrupt",
    "SessionStart",
    "SessionEnd",
    "SubagentStart",
    "SubagentStop",
    "PreCompact",
    "PostCompact",
    "Notification",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionHookEvent {
    SessionStart,
    SessionEnd,
    UserPromptSubmit,
    Interrupt,
    SubagentStart,
    SubagentStop,
    PreCompact,
    PostCompact,
    Notification,
    TurnStart,
    TurnEnd,
}

impl SessionHookEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SessionStart => "SessionStart",
            Self::SessionEnd => "SessionEnd",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::Interrupt => "Interrupt",
            Self::SubagentStart => "SubagentStart",
            Self::SubagentStop => "SubagentStop",
            Self::PreCompact => "PreCompact",
            Self::PostCompact => "PostCompact",
            Self::Notification => "Notification",
            Self::TurnStart => "TurnStart",
            Self::TurnEnd => "TurnEnd",
        }
    }

    /// Map onto the currently implemented MCP hook surface (subset).
    pub fn to_mcp(self) -> Option<kkagent_mcp::hooks::HookEvent> {
        use kkagent_mcp::hooks::HookEvent;
        match self {
            Self::SessionStart => Some(HookEvent::SessionStart),
            Self::SessionEnd => Some(HookEvent::SessionEnd),
            Self::TurnStart => Some(HookEvent::TurnStart),
            Self::TurnEnd => Some(HookEvent::TurnEnd),
            Self::Notification => Some(HookEvent::Notification),
            // Not yet in HookManager — still tracked for vocabulary parity.
            Self::UserPromptSubmit
            | Self::Interrupt
            | Self::SubagentStart
            | Self::SubagentStop
            | Self::PreCompact
            | Self::PostCompact => None,
        }
    }
}

/// Thin adapter that forwards session lifecycle into `kkagent_mcp::HookManager` when present.
pub struct SessionExternalHooks {
    hooks: Option<Arc<kkagent_mcp::HookManager>>,
}

impl SessionExternalHooks {
    pub fn new(hooks: Option<Arc<kkagent_mcp::HookManager>>) -> Self {
        Self { hooks }
    }

    pub async fn fire(&self, event: SessionHookEvent, data: Value) {
        let Some(hooks) = &self.hooks else {
            return;
        };
        let Some(mapped) = event.to_mcp() else {
            tracing::debug!(
                event = event.as_str(),
                "session hook event has no MCP mapping yet"
            );
            return;
        };
        let _ = hooks.fire(mapped, &data).await;
    }

    pub fn payload_session_start(session_id: &str, workspace: &str, source: &str) -> Value {
        json!({
            "session_id": session_id,
            "workspace": workspace,
            "source": source,
        })
    }
}
