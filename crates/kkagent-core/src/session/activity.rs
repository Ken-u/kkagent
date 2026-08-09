//! Session activity projection — busy / pending interaction / last outcome.

use crate::session::metadata::TurnReason;
use std::sync::RwLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PendingInteraction {
    #[default]
    None,
    Approval,
    Question,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionActivityState {
    pub busy: bool,
    pub main_turn_active: bool,
    pub pending_interaction: PendingInteraction,
    pub last_turn_reason: Option<TurnReason>,
    pub background_tasks: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionActivityCause {
    TurnStarted,
    TurnEnded,
    Background,
    Interaction,
    AgentLifecycle,
}

#[derive(Default)]
pub struct SessionActivityView {
    state: RwLock<SessionActivityState>,
}

impl SessionActivityView {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn state(&self) -> SessionActivityState {
        self.state.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn set_main_turn_active(&self, active: bool) {
        let mut s = self.state.write().unwrap_or_else(|e| e.into_inner());
        s.main_turn_active = active;
        s.busy = active || s.background_tasks > 0;
    }

    pub fn set_pending(&self, pending: PendingInteraction) {
        let mut s = self.state.write().unwrap_or_else(|e| e.into_inner());
        s.pending_interaction = pending;
    }

    pub fn set_background_tasks(&self, n: u32) {
        let mut s = self.state.write().unwrap_or_else(|e| e.into_inner());
        s.background_tasks = n;
        s.busy = s.main_turn_active || n > 0;
    }

    pub fn set_last_turn_reason(&self, reason: TurnReason) {
        let mut s = self.state.write().unwrap_or_else(|e| e.into_inner());
        s.last_turn_reason = Some(reason);
        s.main_turn_active = false;
        s.busy = s.background_tasks > 0;
    }
}
