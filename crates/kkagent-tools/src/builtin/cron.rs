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
    mutation: Mutex<()>,
    persist_path: Option<PathBuf>,
}

impl CronManager {
    pub fn new() -> Self {
        Self {
            jobs: Mutex::new(HashMap::new()),
            mutation: Mutex::new(()),
            persist_path: None,
        }
    }

    /// Load from `path` if present; subsequent mutations persist there.
    pub async fn with_persist(path: PathBuf) -> Self {
        let mgr = Self {
            jobs: Mutex::new(HashMap::new()),
            mutation: Mutex::new(()),
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
        let _mutation = self.mutation.lock().await;
        {
            let mut jobs = self.jobs.lock().await;
            if jobs.len() >= 50 {
                anyhow::bail!("session cron job limit reached (50)");
            }
            jobs.insert(id.clone(), job.clone());
        }
        if let Err(e) = self.save_to_disk().await {
            self.jobs.lock().await.remove(&id);
            return Err(e.context("cron job creation rolled back: persist failed"));
        }
        Ok(job)
    }

    /// Delete by id. `Ok(false)` means the id was unknown; an error means the
    /// delete could not be persisted and the job was restored.
    pub async fn delete(&self, id: &str) -> anyhow::Result<bool> {
        let _mutation = self.mutation.lock().await;
        let removed = self.jobs.lock().await.remove(id);
        let Some(job) = removed else {
            return Ok(false);
        };
        if let Err(e) = self.save_to_disk().await {
            self.jobs.lock().await.insert(id.to_string(), job);
            return Err(e.context("cron job deletion rolled back: persist failed"));
        }
        Ok(true)
    }

    /// Return due job prompts and advance/disable them. When persisting fails,
    /// schedule mutations are rolled back and nothing is returned, so a
    /// restart cannot fire the same prompts again.
    pub async fn take_due(&self) -> anyhow::Result<Vec<(String, String, bool)>> {
        let _mutation = self.mutation.lock().await;
        let now = Utc::now();
        let mut jobs = self.jobs.lock().await;
        let mut due = Vec::new();
        let mut remove = Vec::new();
        let mut previous = Vec::new();
        for (id, job) in jobs.iter_mut() {
            if job.enabled && job.next_run <= now {
                due.push((id.clone(), job.prompt.clone(), job.recurring));
                previous.push((id.clone(), job.clone()));
                if !job.recurring || looks_like_delay(&job.expression_or_delay) {
                    job.enabled = false;
                    remove.push(id.clone());
                } else if let Ok(next) =
                    parse_next_run_with_jitter(&job.expression_or_delay, now, id)
                {
                    job.next_run = next;
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
                let mut jobs = self.jobs.lock().await;
                for (id, job) in previous {
                    jobs.insert(id, job);
                }
                return Err(e.context("cron dispatch rolled back: persist failed"));
            }
        }
        Ok(due)
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
        let duration = parse_duration_token(token.trim())?;
        return after
            .checked_add_signed(duration)
            .ok_or_else(|| anyhow::anyhow!("delay `{expr}` is too large"));
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

fn parse_next_run_with_jitter(
    expr: &str,
    after: DateTime<Utc>,
    job_id: &str,
) -> anyhow::Result<DateTime<Utc>> {
    let jitter_minutes = (job_id
        .as_bytes()
        .iter()
        .map(|byte| *byte as i64)
        .sum::<i64>()
        % 7)
        - 3;
    let jitter = Duration::minutes(jitter_minutes);
    let schedule_after = after
        .checked_sub_signed(jitter)
        .ok_or_else(|| anyhow::anyhow!("cron jitter is out of range"))?;
    let base = parse_next_run_after(expr, schedule_after)?;
    let next = base
        .checked_add_signed(jitter)
        .ok_or_else(|| anyhow::anyhow!("cron jitter is out of range"))?;
    if next <= after {
        anyhow::bail!("jittered cron fire time is not in the future")
    }
    Ok(next)
}

fn parse_duration_token(s: &str) -> anyhow::Result<Duration> {
    let s = s.trim();
    let (number, unit) = s.split_at(s.len().saturating_sub(1));
    let value = number.parse::<i64>().map_err(|_| {
        anyhow::anyhow!("invalid delay `{s}`; use a positive integer plus s, m, h, or d")
    })?;
    if value <= 0 {
        anyhow::bail!("delay must be greater than zero")
    }
    let duration = match unit {
        "s" => Duration::try_seconds(value),
        "m" => Duration::try_minutes(value),
        "h" => Duration::try_hours(value),
        "d" => Duration::try_days(value),
        _ => None,
    };
    duration.ok_or_else(|| anyhow::anyhow!("invalid or too-large delay `{s}`"))
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
        match self.mgr.delete(id).await {
            Ok(true) => ToolOutput::success(format!("Deleted cron job {}", id)),
            Ok(false) => ToolOutput::error(format!("Unknown cron id {}", id)),
            Err(error) => ToolOutput::error(error.to_string()),
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

    #[test]
    fn delay_parser_rejects_invalid_non_positive_and_huge_values() {
        let now = Utc::now();
        for expr in ["in nonsense", "0s", "-1m", "in 999999999999999999999d"] {
            assert!(
                parse_next_run_after(expr, now).is_err(),
                "unexpectedly accepted {expr}"
            );
        }
        assert_eq!(
            parse_next_run_after("in 2m", now).unwrap(),
            now + Duration::minutes(2)
        );
    }

    #[test]
    fn negative_jitter_never_schedules_in_the_past() {
        let now = "2026-01-01T00:00:30Z".parse::<DateTime<Utc>>().unwrap();
        // Byte sum 0 mod 7 gives the maximum negative jitter (-3 minutes).
        let id = "bbbbbbb";
        let next = parse_next_run_with_jitter("* * * * *", now, id).unwrap();
        assert!(
            next > now,
            "jittered next run was not in the future: {next}"
        );
        assert_eq!(
            next,
            "2026-01-01T00:01:00Z".parse::<DateTime<Utc>>().unwrap()
        );
    }

    #[tokio::test]
    async fn create_delete_roll_back_when_persist_fails() {
        let dir = std::env::temp_dir().join(format!("kkagent-cron-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cron.json");
        let mgr = CronManager::with_persist(path.clone()).await;
        mgr.create("in 5m".into(), "seed".into(), false)
            .await
            .unwrap();
        // Make the persistence target unwritable: replace the file with a
        // directory so the atomic rename always fails.
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir_all(&path).unwrap();

        assert!(mgr
            .create("in 5m".into(), "should roll back".into(), false)
            .await
            .is_err());
        assert_eq!(mgr.list().await.len(), 1);

        let job_id = mgr.list().await[0].id.clone();
        assert!(mgr.delete(&job_id).await.is_err());
        assert_eq!(mgr.list().await.len(), 1);

        std::fs::remove_dir_all(&path).unwrap();
        drop(mgr);
        let _ = std::fs::remove_dir_all(dir);
    }

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
            model_alias: None,
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
