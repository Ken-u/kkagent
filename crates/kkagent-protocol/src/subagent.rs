use rusqlite::{params, Connection};
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
    /// Profiles this agent may delegate to. `None` means unrestricted.
    #[serde(default)]
    pub subagents: Option<Vec<String>>,
    /// Parent session / tool call for TUI mirroring.
    #[serde(default)]
    pub parent_session_id: Option<String>,
    #[serde(default)]
    pub parent_tool_call_id: Option<String>,
    /// Whether the agent was launched in background (fire-and-forget) mode.
    #[serde(default)]
    pub run_in_background: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentState {
    pub agent_id: String,
    pub description: String,
    pub status: SubagentStatus,
    pub result: Option<String>,
    pub error: Option<String>,
    pub turns_used: u32,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub subagents: Option<Vec<String>>,
}

/// Kimi-compatible delegation policy for built-in profiles.
pub fn allowed_subagents_for(profile: &str) -> Option<Vec<String>> {
    match profile.trim().to_ascii_lowercase().as_str() {
        "coder" => Some(vec!["coder".into(), "explore".into()]),
        "explore" => Some(Vec::new()),
        "agent" | "general" => None,
        _ => None,
    }
}

pub struct SubagentManager {
    agents: Arc<Mutex<HashMap<String, SubagentState>>>,
    aborts: Arc<Mutex<HashMap<String, AbortHandle>>>,
    max_concurrent: usize,
    persistence: Option<Arc<std::sync::Mutex<Connection>>>,
    /// Monotonic counter bumped on every state change so subscribers can react
    /// without polling at intervals.
    revision: tokio::sync::watch::Sender<u64>,
}

impl SubagentManager {
    pub fn new(max_concurrent: usize) -> Self {
        let (revision, _) = tokio::sync::watch::channel(0u64);
        Self {
            agents: Arc::new(Mutex::new(HashMap::new())),
            aborts: Arc::new(Mutex::new(HashMap::new())),
            max_concurrent,
            persistence: None,
            revision,
        }
    }

    pub fn new_persistent(max_concurrent: usize, path: &std::path::Path) -> anyhow::Result<Self> {
        let connection = Connection::open(path)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        Self::from_shared(max_concurrent, Arc::new(std::sync::Mutex::new(connection)))
    }

    /// Share an already-opened SQLite connection (e.g. with TranscriptDb / DurableHttpStore).
    pub fn from_shared(
        max_concurrent: usize,
        connection: Arc<std::sync::Mutex<Connection>>,
    ) -> anyhow::Result<Self> {
        {
            let connection = connection
                .lock()
                .map_err(|_| anyhow::anyhow!("subagent store lock poisoned"))?;
            connection.execute_batch(
                "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS durable_subagents (
                agent_id TEXT PRIMARY KEY,
                config_json TEXT NOT NULL,
                description TEXT NOT NULL,
                status TEXT NOT NULL,
                result TEXT,
                error TEXT,
                turns_used INTEGER NOT NULL DEFAULT 0,
                attempts INTEGER NOT NULL DEFAULT 0,
                max_attempts INTEGER NOT NULL DEFAULT 3,
                updated_at TEXT NOT NULL
             );
             UPDATE durable_subagents SET status = 'pending',
                error = 'recovered after unclean shutdown', updated_at = datetime('now')
                WHERE status = 'running' AND attempts < max_attempts;
             UPDATE durable_subagents SET status = 'failed',
                error = 'retry limit exhausted after unclean shutdown', updated_at = datetime('now')
                WHERE status = 'running' AND attempts >= max_attempts;",
            )?;
        }
        let mut agents = HashMap::new();
        {
            let connection = connection
                .lock()
                .map_err(|_| anyhow::anyhow!("subagent store lock poisoned"))?;
            let mut statement = connection.prepare(
                "SELECT agent_id, description, status, result, error, turns_used, config_json FROM durable_subagents",
            )?;
            let rows = statement.query_map([], |row| {
                let status: String = row.get(2)?;
                let config = row
                    .get::<_, String>(6)
                    .ok()
                    .and_then(|json| serde_json::from_str::<SubagentConfig>(&json).ok());
                Ok(SubagentState {
                    agent_id: row.get(0)?,
                    description: row.get(1)?,
                    status: parse_status(&status),
                    result: row.get(3)?,
                    error: row.get(4)?,
                    turns_used: row.get(5)?,
                    profile: config.as_ref().and_then(|config| config.profile.clone()),
                    subagents: config.and_then(|config| config.subagents),
                })
            })?;
            for row in rows {
                let state = row?;
                agents.insert(state.agent_id.clone(), state);
            }
        }
        Ok(Self {
            agents: Arc::new(Mutex::new(agents)),
            aborts: Arc::new(Mutex::new(HashMap::new())),
            max_concurrent,
            persistence: Some(connection),
            revision: tokio::sync::watch::channel(0u64).0,
        })
    }

    pub async fn spawn(&self, config: SubagentConfig) -> anyhow::Result<String> {
        let mut agents = self.agents.lock().await;
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
        let state = SubagentState {
            agent_id: config.agent_id.clone(),
            description: config.description.clone(),
            status: SubagentStatus::Running,
            result: None,
            error: None,
            turns_used: 0,
            profile: config.profile.clone(),
            subagents: config.subagents.clone(),
        };

        self.persist_spawn(&config)?;
        agents.insert(config.agent_id.clone(), state);
        self.notify();
        Ok(config.agent_id)
    }

    pub async fn recoverable_configs(&self) -> anyhow::Result<Vec<SubagentConfig>> {
        let Some(persistence) = &self.persistence else {
            return Ok(Vec::new());
        };
        let connection = persistence
            .lock()
            .map_err(|_| anyhow::anyhow!("subagent store lock poisoned"))?;
        let mut statement = connection.prepare(
            "SELECT config_json FROM durable_subagents WHERE status = 'pending' AND attempts < max_attempts ORDER BY updated_at",
        )?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut configs = Vec::new();
        for row in rows {
            configs.push(serde_json::from_str(&row?)?);
        }
        Ok(configs)
    }

    pub async fn resume(&self, agent_id: &str) -> anyhow::Result<()> {
        let mut agents = self.agents.lock().await;
        let running = agents
            .values()
            .filter(|agent| agent.status == SubagentStatus::Running)
            .count();
        if running >= self.max_concurrent {
            anyhow::bail!(
                "Maximum concurrent subagents reached ({})",
                self.max_concurrent
            );
        }
        let agent = agents
            .get_mut(agent_id)
            .ok_or_else(|| anyhow::anyhow!("Unknown task_id: {agent_id}"))?;
        if agent.status != SubagentStatus::Pending {
            anyhow::bail!("Task {agent_id} is not pending");
        }
        agent.status = SubagentStatus::Running;
        agent.error = None;
        drop(agents);
        self.persist_status(agent_id, "running", None, None, true)?;
        self.notify();
        Ok(())
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
                agent.result = Some(result.clone());
            }
        }
        self.aborts.lock().await.remove(agent_id);
        let _ = self.persist_status(agent_id, "complete", Some(&result), None, false);
        self.notify();
    }

    pub async fn fail(&self, agent_id: &str, error: String) {
        let mut agents = self.agents.lock().await;
        if let Some(agent) = agents.get_mut(agent_id) {
            if agent.status == SubagentStatus::Running {
                agent.status = SubagentStatus::Failed;
                agent.error = Some(error.clone());
            }
        }
        self.aborts.lock().await.remove(agent_id);
        let _ = self.persist_status(agent_id, "failed", None, Some(&error), false);
        self.notify();
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
                let _ = self.persist_status(
                    agent_id,
                    "cancelled",
                    None,
                    Some("Stopped by TaskStop"),
                    false,
                );
                self.notify();
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

    /// Subscribe to state-change notifications. The receiver receives a new
    /// value every time a subagent transitions (spawn / complete / fail / stop /
    /// resume), so callers can react instantly without 200 ms polling.
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.revision.subscribe()
    }

    /// Bump the revision counter to notify subscribers.
    fn notify(&self) {
        // `send` errors only when all receivers are dropped — safe to ignore.
        let _ = self.revision.send(self.revision.borrow().wrapping_add(1));
    }

    fn persist_spawn(&self, config: &SubagentConfig) -> anyhow::Result<()> {
        let Some(persistence) = &self.persistence else {
            return Ok(());
        };
        let connection = persistence
            .lock()
            .map_err(|_| anyhow::anyhow!("subagent store lock poisoned"))?;
        connection.execute(
            "INSERT INTO durable_subagents(agent_id, config_json, description, status, attempts, max_attempts, updated_at)
             VALUES (?1, ?2, ?3, 'running', 1, 3, ?4)",
            params![config.agent_id, serde_json::to_string(config)?, config.description, chrono::Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    fn persist_status(
        &self,
        agent_id: &str,
        status: &str,
        result: Option<&str>,
        error: Option<&str>,
        increment_attempts: bool,
    ) -> anyhow::Result<()> {
        let Some(persistence) = &self.persistence else {
            return Ok(());
        };
        let connection = persistence
            .lock()
            .map_err(|_| anyhow::anyhow!("subagent store lock poisoned"))?;
        connection.execute(
            "UPDATE durable_subagents SET status = ?1, result = COALESCE(?2, result), error = ?3,
             attempts = attempts + ?4, updated_at = ?5 WHERE agent_id = ?6",
            params![
                status,
                result,
                error,
                i64::from(increment_attempts),
                chrono::Utc::now().to_rfc3339(),
                agent_id
            ],
        )?;
        Ok(())
    }
}

fn parse_status(status: &str) -> SubagentStatus {
    match status {
        "pending" => SubagentStatus::Pending,
        "running" => SubagentStatus::Running,
        "complete" => SubagentStatus::Complete,
        "failed" => SubagentStatus::Failed,
        "cancelled" => SubagentStatus::Cancelled,
        _ => SubagentStatus::Failed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_profiles_define_delegation_allowlists() {
        assert_eq!(
            allowed_subagents_for("coder"),
            Some(vec!["coder".into(), "explore".into()])
        );
        assert_eq!(allowed_subagents_for("explore"), Some(Vec::new()));
        assert_eq!(allowed_subagents_for("general"), None);
    }

    #[test]
    fn old_subagent_configs_default_to_no_explicit_allowlist() {
        let config: SubagentConfig = serde_json::from_value(serde_json::json!({
            "agent_id": "legacy",
            "description": "old config",
            "prompt": "continue",
            "model": null,
            "working_dir": ".",
            "profile": "explore"
        }))
        .unwrap();

        assert_eq!(config.subagents, None);
        assert_eq!(
            allowed_subagents_for(config.profile.as_deref().unwrap()),
            Some(Vec::new())
        );
    }

    #[tokio::test]
    async fn persistent_subagent_recovers_after_restart() {
        let path =
            std::env::temp_dir().join(format!("kkagent-subagent-{}.db", uuid::Uuid::new_v4()));
        let config = SubagentConfig {
            agent_id: "agent-1".into(),
            description: "recover me".into(),
            prompt: "finish the task".into(),
            model: None,
            working_dir: ".".into(),
            profile: Some("coder".into()),
            subagents: allowed_subagents_for("coder"),
            parent_session_id: Some("session".into()),
            parent_tool_call_id: None,
            run_in_background: false,
        };
        {
            let manager = SubagentManager::new_persistent(2, &path).unwrap();
            manager.spawn(config.clone()).await.unwrap();
        }
        {
            let manager = SubagentManager::new_persistent(2, &path).unwrap();
            let pending = manager.recoverable_configs().await.unwrap();
            assert_eq!(pending.len(), 1);
            assert_eq!(pending[0].prompt, config.prompt);
            manager.resume("agent-1").await.unwrap();
            manager.complete("agent-1", "done".into()).await;
        }
        {
            let manager = SubagentManager::new_persistent(2, &path).unwrap();
            assert!(manager.recoverable_configs().await.unwrap().is_empty());
            let state = manager.get_state("agent-1").await.unwrap();
            assert_eq!(state.status, SubagentStatus::Complete);
            assert_eq!(state.result.as_deref(), Some("done"));
            assert_eq!(state.profile.as_deref(), Some("coder"));
            assert_eq!(state.subagents, allowed_subagents_for("coder"));
        }
        let _ = std::fs::remove_file(path);
    }
}
