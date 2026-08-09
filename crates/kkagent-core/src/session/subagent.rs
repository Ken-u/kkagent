//! Session subagent run surface — drive turns on other agents.

use crate::session::agent_lifecycle::AgentLifecycleService;
use std::sync::RwLock;

#[derive(Debug, Clone)]
pub enum AgentRunRequest {
    Prompt { prompt: String },
    Retry { trigger: Option<String> },
}

#[derive(Debug, Clone)]
pub struct AgentTaskEvent {
    pub agent_name: String,
    pub prompt: Option<String>,
    pub response: Option<String>,
}

#[derive(Default)]
pub struct SessionSubagentService {
    last_events: RwLock<Vec<AgentTaskEvent>>,
}

impl SessionSubagentService {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn notify_start(&self, agent_name: impl Into<String>, prompt: impl Into<String>) {
        self.last_events
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .push(AgentTaskEvent {
                agent_name: agent_name.into(),
                prompt: Some(prompt.into()),
                response: None,
            });
    }

    pub fn notify_stop(&self, agent_name: impl Into<String>, response: impl Into<String>) {
        self.last_events
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .push(AgentTaskEvent {
                agent_name: agent_name.into(),
                prompt: None,
                response: Some(response.into()),
            });
    }

    pub fn events(&self) -> Vec<AgentTaskEvent> {
        self.last_events
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Resolve whether `agent_id` exists before a run is scheduled.
    pub fn ensure_agent(
        &self,
        agents: &AgentLifecycleService,
        agent_id: &str,
    ) -> anyhow::Result<()> {
        agents
            .get(agent_id)
            .map(|_| ())
            .ok_or_else(|| anyhow::anyhow!("agent not found: {agent_id}"))
    }
}
