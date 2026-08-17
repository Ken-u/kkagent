use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

/// Goal lifecycle (kimi-aligned). Legacy wire value `failed` deserializes as [`Blocked`].
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum GoalStatus {
    Active,
    Paused,
    Blocked,
    Complete,
}

impl<'de> Deserialize<'de> for GoalStatus {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        match raw.as_str() {
            "active" => Ok(GoalStatus::Active),
            "paused" => Ok(GoalStatus::Paused),
            "blocked" | "failed" => Ok(GoalStatus::Blocked),
            "complete" => Ok(GoalStatus::Complete),
            other => Err(serde::de::Error::unknown_variant(
                other,
                &["active", "paused", "blocked", "complete"],
            )),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct GoalBudget {
    pub turn_budget: Option<u32>,
    pub token_budget: Option<u64>,
    pub wall_clock_budget_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GoalBudgetReport {
    pub turn_budget: Option<u32>,
    pub turns_used: u32,
    pub turns_remaining: Option<u32>,
    pub token_budget: Option<u64>,
    pub tokens_used: u64,
    pub tokens_remaining: Option<u64>,
    pub wall_clock_budget_ms: Option<u64>,
    pub wall_clock_ms: u64,
    pub wall_clock_remaining_ms: Option<u64>,
    pub budget_reached: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Goal {
    pub goal_id: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_criterion: Option<String>,
    pub status: GoalStatus,
    pub budget: GoalBudget,
    pub turns_used: u32,
    pub tokens_used: u64,
    pub wall_clock_ms: u64,
    pub created_at: String,
    pub updated_at: String,
    pub terminal_reason: Option<String>,
}

impl Goal {
    pub fn is_budget_exhausted(&self) -> bool {
        self.budget_report().budget_reached
    }

    /// Complete is terminal (cleared after update). Blocked/paused remain current & recoverable.
    pub fn is_terminal(&self) -> bool {
        matches!(self.status, GoalStatus::Complete)
    }

    pub fn budget_report(&self) -> GoalBudgetReport {
        let turns_remaining = self
            .budget
            .turn_budget
            .map(|b| b.saturating_sub(self.turns_used));
        let tokens_remaining = self
            .budget
            .token_budget
            .map(|b| b.saturating_sub(self.tokens_used));
        let wall_clock_remaining_ms = self
            .budget
            .wall_clock_budget_ms
            .map(|b| b.saturating_sub(self.wall_clock_ms));
        let budget_reached = self
            .budget
            .turn_budget
            .is_some_and(|b| self.turns_used >= b)
            || self
                .budget
                .token_budget
                .is_some_and(|b| self.tokens_used >= b)
            || self
                .budget
                .wall_clock_budget_ms
                .is_some_and(|b| self.wall_clock_ms >= b);
        GoalBudgetReport {
            turn_budget: self.budget.turn_budget,
            turns_used: self.turns_used,
            turns_remaining,
            token_budget: self.budget.token_budget,
            tokens_used: self.tokens_used,
            tokens_remaining,
            wall_clock_budget_ms: self.budget.wall_clock_budget_ms,
            wall_clock_ms: self.wall_clock_ms,
            wall_clock_remaining_ms,
            budget_reached,
        }
    }

    /// Wrap objective for model injection (prevents instruction override).
    pub fn untrusted_objective_xml(&self) -> String {
        format!(
            "<untrusted_objective>\n{}\n</untrusted_objective>",
            escape_untrusted(&self.description)
        )
    }

    pub fn active_reminder(&self) -> String {
        let report = self.budget_report();
        let criterion = self
            .completion_criterion
            .as_ref()
            .map(|c| {
                format!(
                    "\n<untrusted_completion_criterion>\n{}\n</untrusted_completion_criterion>",
                    escape_untrusted(c)
                )
            })
            .unwrap_or_default();
        format!(
            "<system-reminder>\nActive goal:\n{}\n{}\n\
Progress: turns={}/{:?} tokens={}/{:?} wall_ms={}/{:?}.\n\
Continue working toward this goal. Call UpdateGoal with complete or blocked when appropriate.\n\
</system-reminder>",
            self.untrusted_objective_xml(),
            criterion,
            report.turns_used,
            report.turn_budget,
            report.tokens_used,
            report.token_budget,
            report.wall_clock_ms,
            report.wall_clock_budget_ms
        )
    }
}

fn escape_untrusted(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Durable goal journal ops (wire-aligned names).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GoalOp {
    Create {
        goal_id: String,
        objective: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        completion_criterion: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        budget: Option<GoalBudget>,
        time: String,
    },
    Update {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        goal_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<GoalStatus>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turns_used: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tokens_used: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wall_clock_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        budget: Option<GoalBudget>,
        time: String,
    },
    Clear {
        time: String,
    },
    AccountUsage {
        goal_id: String,
        turns_delta: u32,
        tokens_delta: u64,
        wall_clock_ms: u64,
        time: String,
    },
}

pub const GOAL_CONTINUATION_PROMPT: &str = "Continue working toward the active goal. \
Keep the self-audit brief. Do not explore unrelated interpretations once the goal can be decided. \
If the objective is simple, already answered, impossible, unsafe, or contradictory, do not run another \
goal turn. Explain briefly if useful, then call UpdateGoal with `complete` or `blocked` in the same turn. \
Otherwise, choose one bounded, useful slice of work toward the objective. Most goal turns should not call \
UpdateGoal: after completing a useful slice, if material work remains, end the turn normally without calling \
UpdateGoal so the runtime can continue the goal in the next turn. Call UpdateGoal with `complete` only when \
all required work is done and any stated validation has passed. Use `blocked` only for a genuine impasse \
(external condition, required user input, missing credentials, or persistent technical failure).";

pub const GOAL_CANCELLED_REMINDER: &str = "The user cancelled the current goal. \
Ignore earlier active-goal reminders for that goal. Handle the next user request normally unless the user \
starts or resumes a goal.";

pub const GOAL_FORK_CLEARED_REMINDER: &str = "This fork does not have a current goal. \
Ignore earlier active-goal reminders from the source session. Handle requests normally unless the user \
starts a new goal.";

pub const GOAL_BUDGET_STOP_REMINDER: &str = "The goal's hard budget was reached and the goal is now blocked; \
the user can resume it with /goal resume. Stop immediately. Do not call any more tools. Write a brief final \
status message summarizing the progress so far.";

/// Headless /goal exit codes (kimi-aligned subset).
pub mod exit_codes {
    pub const SUCCESS_COMPLETE: i32 = 0;
    pub const BLOCKED: i32 = 10;
    pub const CANCELLED: i32 = 11;
    pub const PAUSED: i32 = 12;
    pub const ERROR: i32 = 1;
}

struct GoalInner {
    current: Option<Goal>,
    /// Accumulated wall-clock while active intervals run; paused/blocked do not advance.
    active_since: Option<std::time::Instant>,
    journal: Vec<GoalOp>,
}

/// On-disk snapshot of a session goal (atomic tmp+rename writes).
#[derive(Serialize, Deserialize)]
struct GoalFile {
    version: u32,
    goal: Option<Goal>,
}

const GOAL_FILE_VERSION: u32 = 1;

pub struct GoalManager {
    inner: Arc<Mutex<GoalInner>>,
    persist_path: Option<PathBuf>,
}

impl GoalManager {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(GoalInner {
                current: None,
                active_since: None,
                journal: Vec::new(),
            })),
            persist_path: None,
        }
    }

    /// Load from `path` if present; subsequent mutations persist there atomically.
    pub async fn with_persist(path: PathBuf) -> Self {
        let mgr = Self {
            inner: Arc::new(Mutex::new(GoalInner {
                current: None,
                active_since: None,
                journal: Vec::new(),
            })),
            persist_path: Some(path.clone()),
        };
        match tokio::fs::read(&path).await {
            Ok(raw) => match serde_json::from_slice::<GoalFile>(&raw) {
                Ok(file) => {
                    if let Some(goal) = file.goal {
                        mgr.load_snapshot(goal).await;
                    }
                }
                Err(error) => {
                    tracing::warn!("goal snapshot {}: parse error: {error}", path.display())
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!("goal snapshot {}: read error: {error}", path.display())
            }
        }
        mgr
    }

    /// Best-effort persist while holding the state lock (callers must hold the guard).
    async fn persist(&self, guard: &GoalInner) {
        let Some(path) = &self.persist_path else {
            return;
        };
        if let Err(error) = write_goal_file(path, guard.current.as_ref()).await {
            tracing::warn!("goal persist {}: {error}", path.display());
        }
    }

    fn fold_wall_clock(goal: &mut Goal, active_since: &mut Option<std::time::Instant>) {
        if let Some(start) = active_since.take() {
            goal.wall_clock_ms = goal
                .wall_clock_ms
                .saturating_add(start.elapsed().as_millis() as u64);
        }
    }

    fn live_wall_clock(goal: &Goal, active_since: Option<std::time::Instant>) -> u64 {
        let mut ms = goal.wall_clock_ms;
        if goal.status == GoalStatus::Active {
            if let Some(start) = active_since {
                ms = ms.saturating_add(start.elapsed().as_millis() as u64);
            }
        }
        ms
    }

    pub async fn create_goal(&self, description: &str, budget: GoalBudget) -> Goal {
        let now = Utc::now().to_rfc3339();
        let goal = Goal {
            goal_id: uuid::Uuid::new_v4().to_string(),
            description: description.to_string(),
            completion_criterion: None,
            status: GoalStatus::Active,
            budget: budget.clone(),
            turns_used: 0,
            tokens_used: 0,
            wall_clock_ms: 0,
            created_at: now.clone(),
            updated_at: now.clone(),
            terminal_reason: None,
        };
        let mut guard = self.inner.lock().await;
        guard.journal.push(GoalOp::Create {
            goal_id: goal.goal_id.clone(),
            objective: goal.description.clone(),
            completion_criterion: None,
            budget: Some(budget),
            time: now,
        });
        guard.current = Some(goal.clone());
        guard.active_since = Some(std::time::Instant::now());
        self.persist(&guard).await;
        goal
    }

    pub async fn get_goal(&self) -> Option<Goal> {
        let guard = self.inner.lock().await;
        let mut goal = guard.current.clone()?;
        goal.wall_clock_ms = Self::live_wall_clock(&goal, guard.active_since);
        Some(goal)
    }

    pub async fn snapshot_with_budget(&self) -> Option<(Goal, GoalBudgetReport)> {
        let goal = self.get_goal().await?;
        let report = goal.budget_report();
        Some((goal, report))
    }

    pub async fn journal(&self) -> Vec<GoalOp> {
        self.inner.lock().await.journal.clone()
    }

    pub async fn record_turn(&self, token_count: u64) {
        let mut guard = self.inner.lock().await;
        let active_since = guard.active_since;
        let Some(goal) = guard.current.as_mut() else {
            return;
        };
        if goal.status != GoalStatus::Active {
            return;
        }
        goal.turns_used = goal.turns_used.saturating_add(1);
        goal.tokens_used = goal.tokens_used.saturating_add(token_count);
        let wall = Self::live_wall_clock(goal, active_since);
        goal.updated_at = Utc::now().to_rfc3339();
        let op = GoalOp::AccountUsage {
            goal_id: goal.goal_id.clone(),
            turns_delta: 1,
            tokens_delta: token_count,
            wall_clock_ms: wall,
            time: goal.updated_at.clone(),
        };
        guard.journal.push(op);
        self.persist(&guard).await;
    }

    pub async fn should_continue(&self) -> bool {
        match self.get_goal().await {
            Some(goal) => goal.status == GoalStatus::Active && !goal.is_budget_exhausted(),
            None => false,
        }
    }

    /// Mark complete then clear current goal (transient complete, kimi-aligned).
    pub async fn complete_goal(&self, reason: &str) -> Option<Goal> {
        let mut guard = self.inner.lock().await;
        let mut goal = guard.current.take()?;
        Self::fold_wall_clock(&mut goal, &mut guard.active_since);
        goal.status = GoalStatus::Complete;
        goal.terminal_reason = Some(reason.to_string());
        goal.updated_at = Utc::now().to_rfc3339();
        guard.journal.push(GoalOp::Update {
            goal_id: Some(goal.goal_id.clone()),
            status: Some(GoalStatus::Complete),
            reason: Some(reason.to_string()),
            turns_used: Some(goal.turns_used),
            tokens_used: Some(goal.tokens_used),
            wall_clock_ms: Some(goal.wall_clock_ms),
            budget: None,
            time: goal.updated_at.clone(),
        });
        guard.journal.push(GoalOp::Clear {
            time: Utc::now().to_rfc3339(),
        });
        guard.active_since = None;
        self.persist(&guard).await;
        Some(goal)
    }

    /// Budget exhaustion and model/system blockers land here (recoverable).
    pub async fn block_goal(&self, reason: &str) {
        let mut guard = self.inner.lock().await;
        let Some(mut goal) = guard.current.take() else {
            return;
        };
        Self::fold_wall_clock(&mut goal, &mut guard.active_since);
        goal.status = GoalStatus::Blocked;
        goal.terminal_reason = Some(reason.to_string());
        goal.updated_at = Utc::now().to_rfc3339();
        let op = GoalOp::Update {
            goal_id: Some(goal.goal_id.clone()),
            status: Some(GoalStatus::Blocked),
            reason: Some(reason.to_string()),
            turns_used: None,
            tokens_used: None,
            wall_clock_ms: Some(goal.wall_clock_ms),
            budget: None,
            time: goal.updated_at.clone(),
        };
        guard.journal.push(op);
        guard.current = Some(goal);
        self.persist(&guard).await;
    }

    /// Legacy alias: map former Failed semantics onto Blocked.
    pub async fn fail_goal(&self, reason: &str) {
        self.block_goal(reason).await;
    }

    pub async fn set_completion_criterion(&self, criterion: &str) {
        let mut guard = self.inner.lock().await;
        if let Some(goal) = guard.current.as_mut() {
            goal.completion_criterion = Some(criterion.to_string());
            goal.updated_at = Utc::now().to_rfc3339();
            self.persist(&guard).await;
        }
    }

    pub async fn pause_goal(&self) {
        let mut guard = self.inner.lock().await;
        let Some(mut goal) = guard.current.take() else {
            return;
        };
        if goal.status == GoalStatus::Paused {
            guard.current = Some(goal);
            return;
        }
        Self::fold_wall_clock(&mut goal, &mut guard.active_since);
        goal.status = GoalStatus::Paused;
        goal.updated_at = Utc::now().to_rfc3339();
        let op = GoalOp::Update {
            goal_id: Some(goal.goal_id.clone()),
            status: Some(GoalStatus::Paused),
            reason: None,
            turns_used: None,
            tokens_used: None,
            wall_clock_ms: Some(goal.wall_clock_ms),
            budget: None,
            time: goal.updated_at.clone(),
        };
        guard.journal.push(op);
        guard.current = Some(goal);
        self.persist(&guard).await;
    }

    pub async fn resume_goal(&self) {
        let mut guard = self.inner.lock().await;
        let Some(mut goal) = guard.current.take() else {
            return;
        };
        if matches!(goal.status, GoalStatus::Paused | GoalStatus::Blocked) {
            goal.status = GoalStatus::Active;
            goal.terminal_reason = None;
            goal.updated_at = Utc::now().to_rfc3339();
            guard.active_since = Some(std::time::Instant::now());
            let op = GoalOp::Update {
                goal_id: Some(goal.goal_id.clone()),
                status: Some(GoalStatus::Active),
                reason: None,
                turns_used: None,
                tokens_used: None,
                wall_clock_ms: Some(goal.wall_clock_ms),
                budget: None,
                time: goal.updated_at.clone(),
            };
            guard.journal.push(op);
        }
        guard.current = Some(goal);
        self.persist(&guard).await;
    }

    pub async fn cancel_goal(&self) -> Option<Goal> {
        let mut guard = self.inner.lock().await;
        let mut goal = guard.current.take()?;
        Self::fold_wall_clock(&mut goal, &mut guard.active_since);
        goal.status = GoalStatus::Paused;
        goal.terminal_reason = Some("cancelled".into());
        goal.updated_at = Utc::now().to_rfc3339();
        guard.journal.push(GoalOp::Clear {
            time: goal.updated_at.clone(),
        });
        guard.active_since = None;
        self.persist(&guard).await;
        Some(goal)
    }

    pub async fn clear_goal(&self) {
        let mut guard = self.inner.lock().await;
        guard.current = None;
        guard.active_since = None;
        guard.journal.push(GoalOp::Clear {
            time: Utc::now().to_rfc3339(),
        });
        self.persist(&guard).await;
    }

    pub async fn update_budget(&self, budget: GoalBudget) {
        let mut guard = self.inner.lock().await;
        let Some(mut goal) = guard.current.take() else {
            return;
        };
        goal.budget = budget.clone();
        goal.updated_at = Utc::now().to_rfc3339();
        let op = GoalOp::Update {
            goal_id: Some(goal.goal_id.clone()),
            status: None,
            reason: None,
            turns_used: None,
            tokens_used: None,
            wall_clock_ms: None,
            budget: Some(budget),
            time: goal.updated_at.clone(),
        };
        guard.journal.push(op);
        guard.current = Some(goal);
        self.persist(&guard).await;
    }

    /// On session restore / fork recovery: active → paused (wall-clock stops).
    pub async fn on_restore(&self) {
        let mut guard = self.inner.lock().await;
        let Some(mut goal) = guard.current.take() else {
            return;
        };
        if goal.status == GoalStatus::Active {
            Self::fold_wall_clock(&mut goal, &mut guard.active_since);
            goal.status = GoalStatus::Paused;
            goal.updated_at = Utc::now().to_rfc3339();
            let op = GoalOp::Update {
                goal_id: Some(goal.goal_id.clone()),
                status: Some(GoalStatus::Paused),
                reason: Some("restored".into()),
                turns_used: None,
                tokens_used: None,
                wall_clock_ms: Some(goal.wall_clock_ms),
                budget: None,
                time: goal.updated_at.clone(),
            };
            guard.journal.push(op);
        }
        guard.current = Some(goal);
        self.persist(&guard).await;
    }

    /// Replace in-memory goal from a persisted snapshot (then [`on_restore`]).
    pub async fn load_snapshot(&self, mut goal: Goal) {
        let mut guard = self.inner.lock().await;
        if goal.status == GoalStatus::Active {
            // Will be paused by caller via on_restore; start without ticking until resume.
            goal.status = GoalStatus::Paused;
        }
        guard.current = Some(goal);
        guard.active_since = None;
    }

    pub async fn replace_goal(&self, description: &str, budget: GoalBudget) -> Goal {
        {
            let mut guard = self.inner.lock().await;
            if guard.current.is_some() {
                guard.current = None;
                guard.active_since = None;
                guard.journal.push(GoalOp::Clear {
                    time: Utc::now().to_rfc3339(),
                });
            }
        }
        self.create_goal(description, budget).await
    }
}

impl Default for GoalManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Atomic goal snapshot write: tmp file + rename; `None` removes the file.
async fn write_goal_file(path: &Path, goal: Option<&Goal>) -> std::io::Result<()> {
    match goal {
        Some(goal) => {
            let file = GoalFile {
                version: GOAL_FILE_VERSION,
                goal: Some(goal.clone()),
            };
            let bytes = serde_json::to_vec_pretty(&file).map_err(std::io::Error::other)?;
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            let tmp = path.with_extension("tmp");
            tokio::fs::write(&tmp, &bytes).await?;
            tokio::fs::rename(&tmp, path).await
        }
        None => match tokio::fs::remove_file(path).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn budget_exhaustion_blocks_not_fails() {
        let mgr = GoalManager::new();
        let _ = mgr
            .create_goal(
                "ship it",
                GoalBudget {
                    turn_budget: Some(1),
                    ..Default::default()
                },
            )
            .await;
        mgr.record_turn(10).await;
        assert!(!mgr.should_continue().await);
        let goal = mgr.get_goal().await.unwrap();
        assert!(goal.is_budget_exhausted());
        mgr.block_goal("budget").await;
        let goal = mgr.get_goal().await.unwrap();
        assert_eq!(goal.status, GoalStatus::Blocked);
        assert!(goal.terminal_reason.as_deref() == Some("budget"));
    }

    #[tokio::test]
    async fn complete_clears_current_goal() {
        let mgr = GoalManager::new();
        mgr.create_goal("done", GoalBudget::default()).await;
        let finished = mgr.complete_goal("ok").await.unwrap();
        assert_eq!(finished.status, GoalStatus::Complete);
        assert!(mgr.get_goal().await.is_none());
        let journal = mgr.journal().await;
        assert!(journal.iter().any(|op| matches!(op, GoalOp::Clear { .. })));
    }

    #[tokio::test]
    async fn pause_freezes_wall_clock() {
        let mgr = GoalManager::new();
        mgr.create_goal("clock", GoalBudget::default()).await;
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        mgr.pause_goal().await;
        let paused_ms = mgr.get_goal().await.unwrap().wall_clock_ms;
        assert!(paused_ms >= 20);
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        let still = mgr.get_goal().await.unwrap().wall_clock_ms;
        assert_eq!(still, paused_ms);
        mgr.resume_goal().await;
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        let after = mgr.get_goal().await.unwrap().wall_clock_ms;
        assert!(after >= paused_ms + 20);
    }

    #[tokio::test]
    async fn restore_pauses_active() {
        let mgr = GoalManager::new();
        mgr.create_goal("restore me", GoalBudget::default()).await;
        mgr.on_restore().await;
        let goal = mgr.get_goal().await.unwrap();
        assert_eq!(goal.status, GoalStatus::Paused);
    }

    #[tokio::test]
    async fn failed_wire_deserializes_as_blocked() {
        let raw = r#"{"goal_id":"g1","description":"x","status":"failed","budget":{},"turns_used":0,"tokens_used":0,"wall_clock_ms":0,"created_at":"t","updated_at":"t","terminal_reason":null}"#;
        let goal: Goal = serde_json::from_str(raw).unwrap();
        assert_eq!(goal.status, GoalStatus::Blocked);
    }

    #[tokio::test]
    async fn budget_report_remaining() {
        let mgr = GoalManager::new();
        mgr.create_goal(
            "r",
            GoalBudget {
                turn_budget: Some(5),
                token_budget: Some(100),
                wall_clock_budget_ms: Some(10_000),
            },
        )
        .await;
        mgr.record_turn(20).await;
        let report = mgr.get_goal().await.unwrap().budget_report();
        assert_eq!(report.turns_remaining, Some(4));
        assert_eq!(report.tokens_remaining, Some(80));
        assert!(!report.budget_reached);
    }

    #[tokio::test]
    async fn resume_from_blocked() {
        let mgr = GoalManager::new();
        mgr.create_goal("b", GoalBudget::default()).await;
        mgr.block_goal("wait").await;
        mgr.resume_goal().await;
        assert_eq!(mgr.get_goal().await.unwrap().status, GoalStatus::Active);
        assert!(mgr.should_continue().await);
    }

    fn temp_goal_path(tag: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let pid = std::process::id();
        path.push(format!("kkagent-goal-{tag}-{pid}-{nonce}.json"));
        path
    }

    #[tokio::test]
    async fn persists_and_restores_across_managers() {
        let path = temp_goal_path("roundtrip");
        let mgr = GoalManager::with_persist(path.clone()).await;
        mgr.create_goal(
            "persisted goal",
            GoalBudget {
                turn_budget: Some(3),
                token_budget: Some(42),
                wall_clock_budget_ms: None,
            },
        )
        .await;
        mgr.record_turn(7).await;
        assert!(path.is_file());

        // New manager instance restores the same goal (paused on restore semantics).
        let restored = GoalManager::with_persist(path.clone()).await;
        let goal = restored.get_goal().await.expect("goal restored");
        assert_eq!(goal.description, "persisted goal");
        assert_eq!(goal.budget.token_budget, Some(42));
        assert_eq!(goal.turns_used, 1);
        assert_eq!(goal.tokens_used, 7);
        // Restored active goals are paused so wall-clock does not tick unseen.
        assert_eq!(goal.status, GoalStatus::Paused);

        // Cancellation removes the snapshot entirely.
        restored.cancel_goal().await;
        assert!(!path.exists());
        drop(mgr);
    }

    #[tokio::test]
    async fn cancel_persists_removal() {
        let path = temp_goal_path("cancel");
        let mgr = GoalManager::with_persist(path.clone()).await;
        mgr.create_goal("gone soon", GoalBudget::default()).await;
        assert!(path.is_file());
        mgr.cancel_goal().await;
        assert!(!path.exists());
        // Clearing on an already-empty manager must not error.
        mgr.clear_goal().await;
    }

    #[tokio::test]
    async fn two_managers_do_not_share_state() {
        // Two managers (per-session) are fully isolated even with the same objective.
        let a = GoalManager::with_persist(temp_goal_path("iso-a")).await;
        let b = GoalManager::with_persist(temp_goal_path("iso-b")).await;
        a.create_goal("session A goal", GoalBudget::default()).await;
        b.create_goal("session B goal", GoalBudget::default()).await;
        b.record_turn(99).await;

        let goal_a = a.get_goal().await.unwrap();
        let goal_b = b.get_goal().await.unwrap();
        assert_eq!(goal_a.description, "session A goal");
        assert_eq!(goal_b.description, "session B goal");
        assert_eq!(goal_a.tokens_used, 0);
        assert_eq!(goal_b.tokens_used, 99);
        // Pausing B must not affect A.
        b.pause_goal().await;
        assert_eq!(a.get_goal().await.unwrap().status, GoalStatus::Active);
    }
}
