use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
    max_concurrent: usize,
}

impl SubagentManager {
    pub fn new(max_concurrent: usize) -> Self {
        Self {
            agents: Arc::new(Mutex::new(HashMap::new())),
            max_concurrent,
        }
    }

    pub async fn spawn(&self, config: SubagentConfig) -> anyhow::Result<String> {
        let agents = self.agents.lock().await;
        let running = agents.values()
            .filter(|a| a.status == SubagentStatus::Running)
            .count();
        if running >= self.max_concurrent {
            anyhow::bail!("Maximum concurrent subagents reached ({})", self.max_concurrent);
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

        self.agents.lock().await.insert(config.agent_id.clone(), state);
        Ok(config.agent_id)
    }

    pub async fn complete(&self, agent_id: &str, result: String) {
        let mut agents = self.agents.lock().await;
        if let Some(agent) = agents.get_mut(agent_id) {
            agent.status = SubagentStatus::Complete;
            agent.result = Some(result);
        }
    }

    pub async fn fail(&self, agent_id: &str, error: String) {
        let mut agents = self.agents.lock().await;
        if let Some(agent) = agents.get_mut(agent_id) {
            agent.status = SubagentStatus::Failed;
            agent.error = Some(error);
        }
    }

    pub async fn cancel(&self, agent_id: &str) {
        let mut agents = self.agents.lock().await;
        if let Some(agent) = agents.get_mut(agent_id) {
            agent.status = SubagentStatus::Cancelled;
        }
    }

    pub async fn get_state(&self, agent_id: &str) -> Option<SubagentState> {
        self.agents.lock().await.get(agent_id).cloned()
    }

    pub async fn list_running(&self) -> Vec<SubagentState> {
        self.agents.lock().await.values()
            .filter(|a| a.status == SubagentStatus::Running)
            .cloned()
            .collect()
    }

    pub async fn list_all(&self) -> Vec<SubagentState> {
        self.agents.lock().await.values().cloned().collect()
    }
}
