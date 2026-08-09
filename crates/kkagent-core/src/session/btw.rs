//! BTW side-question child agent (text-only fork of main).

use crate::session::agent_lifecycle::{AgentLifecycleService, CreateAgentOptions, MAIN_AGENT_ID};
use crate::session::metadata::AgentKind;
use std::collections::HashMap;
use std::sync::RwLock;

pub const TOOL_CALL_DISABLED_MESSAGE: &str =
    "Tool calls are disabled for side questions. Answer with text only.";

pub const SIDE_QUESTION_SYSTEM_REMINDER: &str = r#"
This is a side-channel conversation with the user. You should answer user questions directly based on what you already know.

IMPORTANT:
- You are a separate, lightweight instance.
- The main agent continues independently; do not reference being interrupted.
- Do not call any tools. All tool calls are disabled and will be rejected.
  Even though tool definitions are visible in this request, they exist only
  for technical reasons (prompt cache). You must not use them.
- Respond only with text based on what you already know from the conversation
  and this side-channel conversation.
- Follow-up turns may happen in this side-channel conversation.
- If you do not know the answer, say so directly.
"#;

#[derive(Default)]
pub struct SessionBtwService {
    active_agent_id: RwLock<Option<String>>,
    notes: RwLock<Vec<String>>,
}

impl SessionBtwService {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start (or reuse) the btw child agent; returns agent id.
    pub fn start(&self, agents: &AgentLifecycleService) -> String {
        if let Some(id) = self
            .active_agent_id
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            if agents.get(&id).is_some() {
                return id;
            }
        }
        let mut labels = HashMap::new();
        labels.insert("role".into(), "btw".into());
        let handle = agents
            .fork(
                MAIN_AGENT_ID,
                CreateAgentOptions {
                    kind: Some(AgentKind::Sub),
                    labels,
                    ..Default::default()
                },
            )
            .unwrap_or_else(|_| {
                agents.create(CreateAgentOptions {
                    agent_id: Some(format!("btw-{}", uuid::Uuid::new_v4())),
                    kind: Some(AgentKind::Sub),
                    ..Default::default()
                })
            });
        *self
            .active_agent_id
            .write()
            .unwrap_or_else(|e| e.into_inner()) = Some(handle.id.clone());
        handle.id
    }

    pub fn active_agent_id(&self) -> Option<String> {
        self.active_agent_id
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn add_note(&self, note: impl Into<String>) {
        self.notes
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .push(note.into());
    }

    pub fn notes(&self) -> Vec<String> {
        self.notes.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn system_reminder(&self) -> &'static str {
        SIDE_QUESTION_SYSTEM_REMINDER.trim()
    }
}
