//! Flat agent registry for a session (main / sub / independent).

use crate::session::metadata::{AgentKind, AgentMeta};
use kkagent_protocol::PermissionMode;
use std::collections::HashMap;
use std::sync::RwLock;

pub const MAIN_AGENT_ID: &str = "main";

#[derive(Debug, Clone)]
pub struct AgentHandle {
    pub id: String,
    pub kind: AgentKind,
    pub forked_from: Option<String>,
    pub labels: HashMap<String, String>,
    pub permission_mode: PermissionMode,
    pub model_alias: Option<String>,
    pub disposed: bool,
}

#[derive(Debug, Clone, Default)]
pub struct CreateAgentOptions {
    pub agent_id: Option<String>,
    pub kind: Option<AgentKind>,
    pub forked_from: Option<String>,
    pub labels: HashMap<String, String>,
    pub model_alias: Option<String>,
    pub permission_mode: Option<PermissionMode>,
}

#[derive(Default)]
pub struct AgentLifecycleService {
    agents: RwLock<HashMap<String, AgentHandle>>,
    next_suffix: RwLock<u64>,
}

impl AgentLifecycleService {
    pub fn new() -> Self {
        let svc = Self::default();
        let _ = svc.create(CreateAgentOptions {
            agent_id: Some(MAIN_AGENT_ID.into()),
            kind: Some(AgentKind::Main),
            ..Default::default()
        });
        svc
    }

    pub fn create(&self, opts: CreateAgentOptions) -> AgentHandle {
        let mut agents = self.agents.write().unwrap_or_else(|e| e.into_inner());
        if let Some(ref id) = opts.agent_id {
            if let Some(existing) = agents.get(id) {
                if !existing.disposed {
                    return existing.clone();
                }
            }
        }
        let id = opts.agent_id.unwrap_or_else(|| {
            let mut n = self.next_suffix.write().unwrap_or_else(|e| e.into_inner());
            let candidate = *n;
            *n += 1;
            format!("agent-{candidate}")
        });
        let handle = AgentHandle {
            id: id.clone(),
            kind: opts.kind.unwrap_or(AgentKind::Sub),
            forked_from: opts.forked_from,
            labels: opts.labels,
            permission_mode: opts.permission_mode.unwrap_or(PermissionMode::Manual),
            model_alias: opts.model_alias,
            disposed: false,
        };
        agents.insert(id, handle.clone());
        handle
    }

    pub fn fork(&self, source_id: &str, opts: CreateAgentOptions) -> anyhow::Result<AgentHandle> {
        let source = self
            .get(source_id)
            .ok_or_else(|| anyhow::anyhow!("source agent not found: {source_id}"))?;
        let mut opts = opts;
        opts.forked_from = Some(source.id.clone());
        if opts.kind.is_none() {
            opts.kind = Some(AgentKind::Sub);
        }
        if opts.permission_mode.is_none() {
            opts.permission_mode = Some(source.permission_mode);
        }
        if opts.model_alias.is_none() {
            opts.model_alias = source.model_alias.clone();
        }
        Ok(self.create(opts))
    }

    pub fn get(&self, agent_id: &str) -> Option<AgentHandle> {
        self.agents
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(agent_id)
            .filter(|a| !a.disposed)
            .cloned()
    }

    pub fn list(&self, prefix: Option<&str>) -> Vec<AgentHandle> {
        self.agents
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .filter(|a| !a.disposed)
            .filter(|a| prefix.map(|p| a.id.starts_with(p)).unwrap_or(true))
            .cloned()
            .collect()
    }

    pub fn broadcast_permission_mode(&self, mode: PermissionMode) {
        let mut agents = self.agents.write().unwrap_or_else(|e| e.into_inner());
        for a in agents.values_mut() {
            if !a.disposed {
                a.permission_mode = mode;
            }
        }
    }

    pub fn remove(&self, agent_id: &str) -> bool {
        if agent_id == MAIN_AGENT_ID {
            return false;
        }
        let mut agents = self.agents.write().unwrap_or_else(|e| e.into_inner());
        if let Some(a) = agents.get_mut(agent_id) {
            a.disposed = true;
            return true;
        }
        false
    }

    pub fn to_agent_meta(&self, handle: &AgentHandle) -> AgentMeta {
        AgentMeta {
            homedir: None,
            kind: Some(handle.kind.clone()),
            parent_agent_id: handle.forked_from.clone(),
            forked_from: handle.forked_from.clone(),
            labels: handle.labels.clone(),
            swarm_item: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn main_and_fork() {
        let svc = AgentLifecycleService::new();
        assert!(svc.get(MAIN_AGENT_ID).is_some());
        let child = svc.fork(MAIN_AGENT_ID, CreateAgentOptions::default()).unwrap();
        assert_eq!(child.forked_from.as_deref(), Some(MAIN_AGENT_ID));
        assert_eq!(svc.list(None).len(), 2);
    }
}
