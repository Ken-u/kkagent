use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
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
    #[serde(default = "default_recurring")]
    pub recurring: bool,
}

fn default_recurring() -> bool {
    true
}

pub struct CronManager {
    jobs: Mutex<HashMap<String, CronJob>>,
    persist_path: Option<PathBuf>,
}

impl CronManager {
    pub fn new() -> Self {
        Self {
            jobs: Mutex::new(HashMap::new()),
            persist_path: None,
        }
    }

    /// Load from `path` if present; subsequent mutations persist there.
    pub async fn with_persist(path: PathBuf) -> Self {
        let mgr = Self {
            jobs: Mutex::new(HashMap::new()),
            persist_path: Some(path.clone()),
        };
        if let Err(e) = mgr.load_from_disk(&path).await {
            tracing::warn!("Cron load from {}: {}", path.display(), e);
        }
        mgr
    }

    pub fn persist_path(&self) -> Option<&Path> {
        self.persist_path.as_deref()
    }

    async fn load_from_disk(&self, path: &Path) -> anyhow::Result<()> {
        if !path.exists() {
            return Ok(());
        }
        let text = tokio::fs::read_to_string(path).await?;
        let jobs: Vec<CronJob> = serde_json::from_str(&text)?;
        let mut map = self.jobs.lock().await;
        map.clear();
        for job in jobs {
            map.insert(job.id.clone(), job);
        }
        Ok(())
    }

    async fn save_to_disk(&self) -> anyhow::Result<()> {
        let Some(path) = &self.persist_path else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let jobs = self.list().await;
        let text = serde_json::to_string_pretty(&jobs)?;
        let tmp = path.with_extension("json.tmp");
        tokio::fs::write(&tmp, text).await?;
        tokio::fs::rename(&tmp, path).await?;
        Ok(())
    }

    pub async fn list(&self) -> Vec<CronJob> {
        self.jobs.lock().await.values().cloned().collect()
    }

    pub async fn create(
        &self,
        expr: String,
        prompt: String,
        recurring: bool,
    ) -> anyhow::Result<CronJob> {
        let id = Uuid::new_v4().to_string();
        let next = parse_next_run(&expr)?;
        let is_delay = looks_like_delay(&expr);
        let recurring = if is_delay { false } else { recurring };
        let job = CronJob {
            id: id.clone(),
            expression_or_delay: expr,
            prompt,
            created_at: Utc::now(),
            next_run: next,
            enabled: true,
            recurring,
        };
        {
            let mut jobs = self.jobs.lock().await;
            if jobs.len() >= 50 {
                anyhow::bail!("session cron job limit reached (50)");
            }
            jobs.insert(id, job.clone());
        }
        if let Err(e) = self.save_to_disk().await {
            tracing::warn!("Cron persist failed: {}", e);
        }
        Ok(job)
    }

    pub async fn delete(&self, id: &str) -> bool {
        let removed = self.jobs.lock().await.remove(id).is_some();
        if removed {
            if let Err(e) = self.save_to_disk().await {
                tracing::warn!("Cron persist failed: {}", e);
            }
        }
        removed
    }

    /// Return due job prompts and advance/disable them.
    pub async fn take_due(&self) -> Vec<(String, String, bool)> {
        let now = Utc::now();
        let mut jobs = self.jobs.lock().await;
        let mut due = Vec::new();
        let mut remove = Vec::new();
        for (id, job) in jobs.iter_mut() {
            if job.enabled && job.next_run <= now {
                due.push((id.clone(), job.prompt.clone(), job.recurring));
                if !job.recurring || looks_like_delay(&job.expression_or_delay) {
                    job.enabled = false;
                    remove.push(id.clone());
                } else if let Ok(next) = parse_next_run_after(&job.expression_or_delay, now) {
                    // Light jitter (±3 minutes) to avoid thundering herd.
                    let jitter = (id.as_bytes().iter().map(|b| *b as i64).sum::<i64>() % 7) - 3;
                    job.next_run = next + Duration::minutes(jitter);
                } else {
                    let jitter = (id.as_bytes().iter().map(|b| *b as i64).sum::<i64>() % 7) - 3;
                    job.next_run = now + Duration::hours(1) + Duration::minutes(jitter);
                }
            }
        }
        for id in remove {
            jobs.remove(&id);
        }
        drop(jobs);
        if !due.is_empty() {
            if let Err(e) = self.save_to_disk().await {
                tracing::warn!("Cron persist failed: {}", e);
            }
        }
        due
    }
}

impl Default for CronManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Render cron-fire XML injection (aligned with ref `cron-fire-xml`).
pub fn render_cron_fire_xml(
    job_id: &str,
    cron_expr: &str,
    prompt: &str,
    recurring: bool,
    coalesced_count: u32,
    stale: bool,
) -> String {
    let job_id = attr(job_id);
    let cron = attr(cron_expr);
    format!(
        "<cron-fire jobId=\"{job_id}\" cron=\"{cron}\" recurring=\"{}\" coalescedCount=\"{coalesced_count}\" stale=\"{}\">\n\
<prompt>\n\
{prompt}\n\
</prompt>\n\
</cron-fire>",
        if recurring { "true" } else { "false" },
        if stale { "true" } else { "false" },
    )
}

fn attr(value: &str) -> String {
    value.replace('&', "&amp;").replace('"', "&quot;")
}

fn looks_like_delay(expr: &str) -> bool {
    let e = expr.trim().to_lowercase();
    e.starts_with("in ")
        || ((e.ends_with('s') || e.ends_with('m') || e.ends_with('h') || e.ends_with('d'))
            && !e.contains('*')
            && e.split_whitespace().count() == 1)
}

fn parse_next_run(expr: &str) -> anyhow::Result<DateTime<Utc>> {
    parse_next_run_after(expr, Utc::now())
}

fn parse_next_run_after(expr: &str, after: DateTime<Utc>) -> anyhow::Result<DateTime<Utc>> {
    let e = expr.trim().to_lowercase();
    if looks_like_delay(&e) {
        let token = e.strip_prefix("in ").unwrap_or(&e);
        return Ok(after + parse_duration_token(token.trim()));
    }
    // 5-field cron → prepend seconds for `cron` crate
    let schedule_src = if e.split_whitespace().count() == 5 {
        format!("0 {e}")
    } else {
        e.clone()
    };
    let schedule: cron::Schedule = schedule_src
        .parse()
        .map_err(|err| anyhow::anyhow!("invalid cron expression `{expr}`: {err}"))?;
    schedule
        .after(&after)
        .next()
        .ok_or_else(|| anyhow::anyhow!("cron expression `{expr}` has no future fire time"))
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

/// Unified cron tool (`Cron`) — subsumes the former CronCreate / CronList /
/// CronDelete trio behind a single `action` parameter.
pub struct CronTool {
    mgr: Arc<CronManager>,
}

impl CronTool {
    pub fn new(mgr: Arc<CronManager>) -> Self {
        Self { mgr }
    }

    async fn create(&self, input: &Value) -> ToolOutput {
        let delay = input
            .get("delay")
            .or_else(|| input.get("cron"))
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
            return ToolOutput::error("delay/cron and prompt are required");
        }
        let recurring = input
            .get("recurring")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        match self.mgr.create(delay, prompt, recurring).await {
            Ok(job) => ToolOutput::success(format!(
                "Cron job created id={} recurring={} next_run={}",
                job.id,
                job.recurring,
                job.next_run.to_rfc3339()
            )),
            Err(e) => ToolOutput::error(e.to_string()),
        }
    }

    async fn list(&self) -> ToolOutput {
        let jobs = self.mgr.list().await;
        if jobs.is_empty() {
            return ToolOutput::success("No cron jobs.");
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
        ToolOutput::success(lines.join("\n"))
    }

    async fn delete(&self, input: &Value) -> ToolOutput {
        let id = input.get("id").and_then(|v| v.as_str()).unwrap_or("");
        if id.is_empty() {
            return ToolOutput::error("Missing id");
        }
        if self.mgr.delete(id).await {
            ToolOutput::success(format!("Deleted cron job {}", id))
        } else {
            ToolOutput::error(format!("Unknown cron id {}", id))
        }
    }
}

#[async_trait]
impl Tool for CronTool {
    fn name(&self) -> &str {
        "Cron"
    }
    fn description(&self) -> &str {
        "Schedule prompts to run later. Actions: create (delay like `in 5m` / `30s`, or a \
5-field cron expression with recurring=true default / false one-shot), list, delete. \
Subsumes the former CronCreate / CronList / CronDelete tools."
    }
    fn disclosure(&self) -> crate::ToolDisclosure {
        crate::ToolDisclosure::Deferred
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["create", "list", "delete"],
                    "description": "Which cron operation to perform"
                },
                "delay": {"type": "string", "description": "create: when to run, e.g. 'in 5m', '1h', or cron '*/5 * * * *'"},
                "cron": {"type": "string", "description": "create: alias for delay when using a 5-field cron expression"},
                "expression": {"type": "string", "description": "create: alias for delay"},
                "prompt": {"type": "string", "description": "create: prompt to inject when due"},
                "recurring": {
                    "type": "boolean",
                    "description": "create: true (default for cron) = fire on every match; false = one-shot. Delays are always one-shot."
                },
                "id": {"type": "string", "description": "delete: cron job id"}
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let action = input
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("list");
        Ok(match action {
            "create" => self.create(&input).await,
            "list" => self.list().await,
            "delete" => self.delete(&input).await,
            other => ToolOutput::error(format!(
                "Unknown action: {other}. Use create, list, or delete."
            )),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn persist_roundtrip() {
        let dir = std::env::temp_dir().join(format!("kkagent-cron-{}", Uuid::new_v4()));
        let path = dir.join("cron.json");
        let mgr = CronManager::with_persist(path.clone()).await;
        let job = mgr
            .create("in 5m".into(), "hello".into(), false)
            .await
            .unwrap();
        drop(mgr);
        let mgr2 = CronManager::with_persist(path).await;
        let list = mgr2.list().await;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, job.id);
        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    fn context() -> ToolContext {
        ToolContext {
            working_dir: std::env::temp_dir(),
            session_id: "cron-session".into(),
            turn_id: "t".into(),
            plan_file_path: None,
            image: kkagent_config::ImageConfig::default(),
            tool_call_id: None,
            interrupted: None,
            tools_config: kkagent_config::ToolsConfig::default(),
        }
    }

    #[tokio::test]
    async fn cron_tool_covers_the_former_trio() {
        let mgr = Arc::new(CronManager::new());
        let tool = CronTool::new(mgr);

        let created = tool
            .execute(
                serde_json::json!({"action": "create", "delay": "in 5m", "prompt": "hi"}),
                &context(),
            )
            .await
            .unwrap();
        assert!(!created.is_error);
        let id = created
            .content
            .split("id=")
            .nth(1)
            .and_then(|s| s.split_whitespace().next())
            .unwrap()
            .to_string();

        let list = tool
            .execute(serde_json::json!({"action": "list"}), &context())
            .await
            .unwrap();
        assert!(list.content.contains(&id));

        let deleted = tool
            .execute(
                serde_json::json!({"action": "delete", "id": id}),
                &context(),
            )
            .await
            .unwrap();
        assert!(!deleted.is_error);
    }
}
