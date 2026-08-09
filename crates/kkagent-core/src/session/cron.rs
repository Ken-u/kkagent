//! Session-scoped cron job registry (persist + list/create/delete).

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::RwLock;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionCronJob {
    pub id: String,
    pub expr: String,
    pub prompt: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fired_at: Option<i64>,
}

#[derive(Default)]
pub struct SessionCronService {
    jobs: RwLock<Vec<SessionCronJob>>,
}

impl SessionCronService {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn load(&self, session_dir: &Path) -> anyhow::Result<()> {
        let path = session_dir.join("cron.json");
        if !path.exists() {
            return Ok(());
        }
        let text = std::fs::read_to_string(path)?;
        let jobs: Vec<SessionCronJob> = serde_json::from_str(&text)?;
        *self.jobs.write().unwrap_or_else(|e| e.into_inner()) = jobs;
        Ok(())
    }

    pub fn persist(&self, session_dir: &Path) -> anyhow::Result<()> {
        let jobs = self.list();
        std::fs::write(
            session_dir.join("cron.json"),
            serde_json::to_string_pretty(&jobs)?,
        )?;
        Ok(())
    }

    pub fn list(&self) -> Vec<SessionCronJob> {
        self.jobs.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn create(&self, expr: impl Into<String>, prompt: impl Into<String>) -> SessionCronJob {
        let job = SessionCronJob {
            id: uuid::Uuid::new_v4().to_string(),
            expr: expr.into(),
            prompt: prompt.into(),
            enabled: true,
            last_fired_at: None,
        };
        self.jobs
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .push(job.clone());
        job
    }

    pub fn delete(&self, id: &str) -> bool {
        let mut jobs = self.jobs.write().unwrap_or_else(|e| e.into_inner());
        let before = jobs.len();
        jobs.retain(|j| j.id != id);
        jobs.len() != before
    }
}
