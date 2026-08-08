use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::{Tool, ToolContext, ToolOutput};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronJob {
    pub id: String,
    pub expression_or_delay: String,
    pub prompt: String,
    pub created_at: DateTime<Utc>,
    pub next_run: DateTime<Utc>,
    pub enabled: bool,
}

pub struct CronManager {
    jobs: Mutex<HashMap<String, CronJob>>,
}

impl CronManager {
    pub fn new() -> Self {
        Self {
            jobs: Mutex::new(HashMap::new()),
        }
    }

    pub async fn list(&self) -> Vec<CronJob> {
        self.jobs.lock().await.values().cloned().collect()
    }

    pub async fn create(&self, expr: String, prompt: String) -> CronJob {
        let id = Uuid::new_v4().to_string();
        let next = parse_next_run(&expr);
        let job = CronJob {
            id: id.clone(),
            expression_or_delay: expr,
            prompt,
            created_at: Utc::now(),
            next_run: next,
            enabled: true,
        };
        self.jobs.lock().await.insert(id, job.clone());
        job
    }

    pub async fn delete(&self, id: &str) -> bool {
        self.jobs.lock().await.remove(id).is_some()
    }

    /// Return due job prompts and advance/disable them.
    pub async fn take_due(&self) -> Vec<(String, String)> {
        let now = Utc::now();
        let mut jobs = self.jobs.lock().await;
        let mut due = Vec::new();
        let mut remove = Vec::new();
        for (id, job) in jobs.iter_mut() {
            if job.enabled && job.next_run <= now {
                due.push((id.clone(), job.prompt.clone()));
                // One-shot delays are disabled after fire; cron-like keep hourly for simplicity.
                if job.expression_or_delay.starts_with("in ")
                    || job.expression_or_delay.ends_with('s')
                    || job.expression_or_delay.ends_with('m')
                    || job.expression_or_delay.ends_with('h')
                {
                    job.enabled = false;
                    remove.push(id.clone());
                } else {
                    job.next_run = now + Duration::hours(1);
                }
            }
        }
        for id in remove {
            jobs.remove(&id);
        }
        due
    }
}

fn parse_next_run(expr: &str) -> DateTime<Utc> {
    let e = expr.trim().to_lowercase();
    let now = Utc::now();
    if let Some(rest) = e.strip_prefix("in ") {
        return now + parse_duration_token(rest.trim());
    }
    if e.ends_with('s') || e.ends_with('m') || e.ends_with('h') || e.ends_with('d') {
        return now + parse_duration_token(&e);
    }
    // Fallback: 1 hour
    now + Duration::hours(1)
}

fn parse_duration_token(s: &str) -> Duration {
    let s = s.trim();
    if let Some(n) = s.strip_suffix('s') {
        return Duration::seconds(n.trim().parse().unwrap_or(60));
    }
    if let Some(n) = s.strip_suffix('m') {
        return Duration::minutes(n.trim().parse().unwrap_or(1));
    }
    if let Some(n) = s.strip_suffix('h') {
        return Duration::hours(n.trim().parse().unwrap_or(1));
    }
    if let Some(n) = s.strip_suffix('d') {
        return Duration::days(n.trim().parse().unwrap_or(1));
    }
    Duration::minutes(5)
}

pub struct CronCreateTool {
    mgr: Arc<CronManager>,
}
pub struct CronListTool {
    mgr: Arc<CronManager>,
}
pub struct CronDeleteTool {
    mgr: Arc<CronManager>,
}

impl CronCreateTool {
    pub fn new(mgr: Arc<CronManager>) -> Self {
        Self { mgr }
    }
}
impl CronListTool {
    pub fn new(mgr: Arc<CronManager>) -> Self {
        Self { mgr }
    }
}
impl CronDeleteTool {
    pub fn new(mgr: Arc<CronManager>) -> Self {
        Self { mgr }
    }
}

#[async_trait]
impl Tool for CronCreateTool {
    fn name(&self) -> &str {
        "CronCreate"
    }
    fn description(&self) -> &str {
        "Schedule a prompt to run later. Use delay like `in 5m`, `30s`, `2h`, or a cron-ish token."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "delay": {"type": "string", "description": "When to run, e.g. 'in 5m' or '1h'"},
                "prompt": {"type": "string", "description": "Prompt to inject when due"}
            },
            "required": ["delay", "prompt"]
        })
    }
    async fn execute(&self, input: Value, _ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let delay = input
            .get("delay")
            .or_else(|| input.get("expression"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let prompt = input
            .get("prompt")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if delay.is_empty() || prompt.is_empty() {
            return Ok(ToolOutput::error("delay and prompt are required"));
        }
        let job = self.mgr.create(delay, prompt).await;
        Ok(ToolOutput::success(format!(
            "Cron job created id={} next_run={}",
            job.id,
            job.next_run.to_rfc3339()
        )))
    }
}

#[async_trait]
impl Tool for CronListTool {
    fn name(&self) -> &str {
        "CronList"
    }
    fn description(&self) -> &str {
        "List scheduled cron jobs."
    }
    fn read_only(&self) -> bool {
        true
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    async fn execute(&self, _input: Value, _ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let jobs = self.mgr.list().await;
        if jobs.is_empty() {
            return Ok(ToolOutput::success("No cron jobs."));
        }
        let lines: Vec<String> = jobs
            .iter()
            .map(|j| {
                format!(
                    "- {} [{}] next={} :: {}",
                    j.id,
                    j.expression_or_delay,
                    j.next_run.to_rfc3339(),
                    j.prompt.chars().take(80).collect::<String>()
                )
            })
            .collect();
        Ok(ToolOutput::success(lines.join("\n")))
    }
}

#[async_trait]
impl Tool for CronDeleteTool {
    fn name(&self) -> &str {
        "CronDelete"
    }
    fn description(&self) -> &str {
        "Delete a cron job by id."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": {"type": "string"}
            },
            "required": ["id"]
        })
    }
    async fn execute(&self, input: Value, _ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let id = input.get("id").and_then(|v| v.as_str()).unwrap_or("");
        if id.is_empty() {
            return Ok(ToolOutput::error("Missing id"));
        }
        if self.mgr.delete(id).await {
            Ok(ToolOutput::success(format!("Deleted cron job {}", id)))
        } else {
            Ok(ToolOutput::error(format!("Unknown cron id {}", id)))
        }
    }
}
