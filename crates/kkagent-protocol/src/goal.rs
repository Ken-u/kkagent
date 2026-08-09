use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum GoalStatus {
    Active,
    Paused,
    Blocked,
    Complete,
    Failed,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GoalBudget {
    pub turn_budget: Option<u32>,
    pub token_budget: Option<u64>,
    pub wall_clock_budget_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Goal {
    pub goal_id: String,
    pub description: String,
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
        if let Some(turn_budget) = self.budget.turn_budget {
            if self.turns_used >= turn_budget {
                return true;
            }
        }
        if let Some(token_budget) = self.budget.token_budget {
            if self.tokens_used >= token_budget {
                return true;
            }
        }
        if let Some(wall_budget) = self.budget.wall_clock_budget_ms {
            if self.wall_clock_ms >= wall_budget {
                return true;
            }
        }
        false
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self.status, GoalStatus::Complete | GoalStatus::Failed)
    }
}

pub struct GoalManager {
    current_goal: Arc<Mutex<Option<Goal>>>,
    start_time: Arc<Mutex<Option<std::time::Instant>>>,
}

impl GoalManager {
    pub fn new() -> Self {
        Self {
            current_goal: Arc::new(Mutex::new(None)),
            start_time: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn create_goal(&self, description: &str, budget: GoalBudget) -> Goal {
        let now = Utc::now().to_rfc3339();
        let goal = Goal {
            goal_id: uuid::Uuid::new_v4().to_string(),
            description: description.to_string(),
            status: GoalStatus::Active,
            budget,
            turns_used: 0,
            tokens_used: 0,
            wall_clock_ms: 0,
            created_at: now.clone(),
            updated_at: now,
            terminal_reason: None,
        };
        *self.current_goal.lock().await = Some(goal.clone());
        *self.start_time.lock().await = Some(std::time::Instant::now());
        goal
    }

    pub async fn get_goal(&self) -> Option<Goal> {
        let mut goal = self.current_goal.lock().await.clone();
        if let Some(ref mut g) = goal {
            if g.status == GoalStatus::Active {
                if let Some(start) = *self.start_time.lock().await {
                    g.wall_clock_ms = start.elapsed().as_millis() as u64;
                }
            }
        }
        goal
    }

    pub async fn record_turn(&self, token_count: u64) {
        let mut guard = self.current_goal.lock().await;
        if let Some(ref mut goal) = *guard {
            if goal.status == GoalStatus::Active {
                goal.turns_used += 1;
                goal.tokens_used += token_count;
                if let Some(start) = *self.start_time.lock().await {
                    goal.wall_clock_ms = start.elapsed().as_millis() as u64;
                }
                goal.updated_at = Utc::now().to_rfc3339();
            }
        }
    }

    pub async fn should_continue(&self) -> bool {
        let guard = self.current_goal.lock().await;
        match &*guard {
            Some(goal) => goal.status == GoalStatus::Active && !goal.is_budget_exhausted(),
            None => false,
        }
    }

    pub async fn complete_goal(&self, reason: &str) {
        let mut guard = self.current_goal.lock().await;
        if let Some(ref mut goal) = *guard {
            goal.status = GoalStatus::Complete;
            goal.terminal_reason = Some(reason.to_string());
            goal.updated_at = Utc::now().to_rfc3339();
        }
    }

    pub async fn fail_goal(&self, reason: &str) {
        let mut guard = self.current_goal.lock().await;
        if let Some(ref mut goal) = *guard {
            goal.status = GoalStatus::Failed;
            goal.terminal_reason = Some(reason.to_string());
            goal.updated_at = Utc::now().to_rfc3339();
        }
    }

    pub async fn pause_goal(&self) {
        let mut guard = self.current_goal.lock().await;
        if let Some(ref mut goal) = *guard {
            goal.status = GoalStatus::Paused;
            goal.updated_at = Utc::now().to_rfc3339();
        }
    }

    pub async fn resume_goal(&self) {
        let mut guard = self.current_goal.lock().await;
        if let Some(ref mut goal) = *guard {
            if goal.status == GoalStatus::Paused {
                goal.status = GoalStatus::Active;
                goal.updated_at = Utc::now().to_rfc3339();
                *self.start_time.lock().await = Some(std::time::Instant::now());
            }
        }
    }

    pub async fn clear_goal(&self) {
        *self.current_goal.lock().await = None;
        *self.start_time.lock().await = None;
    }

    pub async fn update_budget(&self, budget: GoalBudget) {
        let mut guard = self.current_goal.lock().await;
        if let Some(ref mut goal) = *guard {
            goal.budget = budget;
            goal.updated_at = Utc::now().to_rfc3339();
        }
    }
}

impl Default for GoalManager {
    fn default() -> Self {
        Self::new()
    }
}
