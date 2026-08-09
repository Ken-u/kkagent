//! Session swarm batch scheduler — spawn/resume agent runs as a batch.

use crate::session::agent_lifecycle::{AgentLifecycleService, CreateAgentOptions};
use crate::session::metadata::AgentKind;
use std::collections::HashMap;
use std::sync::RwLock;

#[derive(Debug, Clone)]
pub enum SwarmTaskKind {
    Spawn,
    Resume { agent_id: String },
}

#[derive(Debug, Clone)]
pub struct SessionSwarmTask {
    pub kind: SwarmTaskKind,
    pub profile_name: String,
    pub parent_tool_call_id: String,
    pub prompt: String,
    pub description: String,
    pub swarm_index: Option<u32>,
    pub swarm_item: Option<String>,
    pub run_in_background: bool,
    pub model: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SessionSwarmRunResult {
    pub agent_id: Option<String>,
    pub status: SwarmRunStatus,
    pub result: Option<String>,
    pub error: Option<String>,
    pub description: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwarmRunStatus {
    Completed,
    Failed,
    Aborted,
}

#[derive(Default)]
pub struct SessionSwarmBatchService {
    /// caller_agent_id -> in-flight agent ids
    inflight: RwLock<HashMap<String, Vec<String>>>,
    cancelled: RwLock<HashMap<String, bool>>,
}

impl SessionSwarmBatchService {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn run(
        &self,
        agents: &AgentLifecycleService,
        caller_agent_id: &str,
        tasks: &[SessionSwarmTask],
    ) -> Vec<SessionSwarmRunResult> {
        {
            let cancelled = self.cancelled.read().unwrap_or_else(|e| e.into_inner());
            if cancelled.get(caller_agent_id).copied().unwrap_or(false) {
                return tasks
                    .iter()
                    .map(|t| SessionSwarmRunResult {
                        agent_id: None,
                        status: SwarmRunStatus::Aborted,
                        result: None,
                        error: Some("cancelled".into()),
                        description: t.description.clone(),
                    })
                    .collect();
            }
        }

        let mut results = Vec::new();
        let mut spawned = Vec::new();
        for task in tasks {
            if self
                .cancelled
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .get(caller_agent_id)
                .copied()
                .unwrap_or(false)
            {
                results.push(SessionSwarmRunResult {
                    agent_id: None,
                    status: SwarmRunStatus::Aborted,
                    result: None,
                    error: Some("cancelled".into()),
                    description: task.description.clone(),
                });
                continue;
            }
            let handle = match &task.kind {
                SwarmTaskKind::Resume { agent_id } => agents.get(agent_id),
                SwarmTaskKind::Spawn => {
                    let mut labels = HashMap::new();
                    labels.insert("swarm".into(), "1".into());
                    if let Some(item) = &task.swarm_item {
                        labels.insert("swarm_item".into(), item.clone());
                    }
                    Some(agents.create(CreateAgentOptions {
                        kind: Some(AgentKind::Sub),
                        labels,
                        model_alias: task.model.clone(),
                        forked_from: Some(caller_agent_id.to_string()),
                        ..Default::default()
                    }))
                }
            };
            match handle {
                Some(h) => {
                    spawned.push(h.id.clone());
                    results.push(SessionSwarmRunResult {
                        agent_id: Some(h.id),
                        // Actual turn execution is owned by agent_loop / subagent_runtime;
                        // here we only materialize the batch roster.
                        status: SwarmRunStatus::Completed,
                        result: Some(format!("queued: {}", task.prompt.chars().take(80).collect::<String>())),
                        error: None,
                        description: task.description.clone(),
                    });
                }
                None => results.push(SessionSwarmRunResult {
                    agent_id: None,
                    status: SwarmRunStatus::Failed,
                    result: None,
                    error: Some("agent unavailable".into()),
                    description: task.description.clone(),
                }),
            }
        }
        self.inflight
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(caller_agent_id.to_string(), spawned);
        results
    }

    pub fn cancel(&self, caller_agent_id: &str) {
        self.cancelled
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(caller_agent_id.to_string(), true);
    }

    pub fn get_swarm_item(
        &self,
        agents: &AgentLifecycleService,
        agent_id: &str,
    ) -> Option<String> {
        agents
            .get(agent_id)?
            .labels
            .get("swarm_item")
            .cloned()
    }
}
