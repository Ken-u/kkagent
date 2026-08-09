use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::task::AbortHandle;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum SubagentStatus {
    Pending,
    Running,
    Complete,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentConfig {
    pub agent_id: String,
    pub description: String,
    pub prompt: String,
    pub model: Option<String>,
    pub working_dir: String,
    /// Optional profile: explore | coder | general (default).
    #[serde(default)]
    pub profile: Option<String>,
    /// Parent session / tool call for TUI mirroring.
    #[serde(default)]
    pub parent_session_id: Option<String>,
    #[serde(default)]
    pub parent_tool_call_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentState {
    pub agent_id: String,
    pub description: String,
    pub status: SubagentStatus,
    pub result: Option<String>,
    pub error: Option<String>,
    pub turns_used: u32,
}

pub struct SubagentManager {
    agents: Arc<Mutex<HashMap<String, SubagentState>>>,
    aborts: Arc<Mutex<HashMap<String, AbortHandle>>>,
    max_concurrent: usize,
}

impl SubagentManager {
    pub fn new(max_concurrent: usize) -> Self {
        Self {
            agents: Arc::new(Mutex::new(HashMap::new())),
            aborts: Arc::new(Mutex::new(HashMap::new())),
            max_concurrent,
        }
    }

    pub async fn spawn(&self, config: SubagentConfig) -> anyhow::Result<String> {
        let agents = self.agents.lock().await;
        let running = agents
            .values()
            .filter(|a| a.status == SubagentStatus::Running)
            .count();
        if running >= self.max_concurrent {
            anyhow::bail!(
                "Maximum concurrent subagents reached ({})",
                self.max_concurrent
            );
        }
        drop(agents);

        let state = SubagentState {
            agent_id: config.agent_id.clone(),
            description: config.description,
            status: SubagentStatus::Running,
            result: None,
            error: None,
            turns_used: 0,
        };

        self.agents
            .lock()
            .await
            .insert(config.agent_id.clone(), state);
        Ok(config.agent_id)
    }

    pub async fn set_abort_handle(&self, agent_id: &str, handle: AbortHandle) {
        self.aborts
            .lock()
            .await
            .insert(agent_id.to_string(), handle);
    }

    pub async fn complete(&self, agent_id: &str, result: String) {
        let mut agents = self.agents.lock().await;
        if let Some(agent) = agents.get_mut(agent_id) {
            if agent.status == SubagentStatus::Running {
                agent.status = SubagentStatus::Complete;
                agent.result = Some(result);
            }
        }
        self.aborts.lock().await.remove(agent_id);
    }

    pub async fn fail(&self, agent_id: &str, error: String) {
        let mut agents = self.agents.lock().await;
        if let Some(agent) = agents.get_mut(agent_id) {
            if agent.status == SubagentStatus::Running {
                agent.status = SubagentStatus::Failed;
                agent.error = Some(error);
            }
        }
        self.aborts.lock().await.remove(agent_id);
    }

    /// Mark cancelled and abort the running tokio task if present.
    pub async fn stop(&self, agent_id: &str) -> anyhow::Result<SubagentState> {
        let mut agents = self.agents.lock().await;
        let agent = agents
            .get_mut(agent_id)
            .ok_or_else(|| anyhow::anyhow!("Unknown task_id: {agent_id}"))?;

        match agent.status {
            SubagentStatus::Running | SubagentStatus::Pending => {
                agent.status = SubagentStatus::Cancelled;
                agent.error = Some("Stopped by TaskStop".into());
                let snapshot = agent.clone();
                drop(agents);
                if let Some(handle) = self.aborts.lock().await.remove(agent_id) {
                    handle.abort();
                }
                Ok(snapshot)
            }
            other => Err(anyhow::anyhow!(
                "Task {agent_id} is not running (status={other:?})"
            )),
        }
    }

    pub async fn cancel(&self, agent_id: &str) {
        let _ = self.stop(agent_id).await;
    }

    pub async fn get_state(&self, agent_id: &str) -> Option<SubagentState> {
        self.agents.lock().await.get(agent_id).cloned()
    }

    pub async fn list_running(&self) -> Vec<SubagentState> {
        self.agents
            .lock()
            .await
            .values()
            .filter(|a| a.status == SubagentStatus::Running)
            .cloned()
            .collect()
    }

    pub async fn list_all(&self) -> Vec<SubagentState> {
        self.agents.lock().await.values().cloned().collect()
    }
}
