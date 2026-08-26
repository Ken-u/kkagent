//! Swarm mode — enter/exit reminders + AgentSwarm exclusivity guard.

use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwarmModeTrigger {
    Tool,
    Slash,
    Auto,
}

#[derive(Debug, Clone)]
pub struct SwarmMember {
    pub id: String,
    pub role: String,
    pub status: String,
}

#[derive(Debug, Default)]
pub struct SwarmService {
    active: bool,
    trigger: Option<SwarmModeTrigger>,
    roster: Vec<SwarmMember>,
    auto_exit_on_turn_end: bool,
}

impl SwarmService {
    pub fn new() -> Self {
        Self {
            auto_exit_on_turn_end: true,
            ..Default::default()
        }
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn roster(&self) -> &[SwarmMember] {
        &self.roster
    }

    pub fn enter(&mut self, trigger: SwarmModeTrigger) -> Option<&'static str> {
        if self.active {
            return None;
        }
        self.active = true;
        self.trigger = Some(trigger);
        Some(ENTER_REMINDER)
    }

    pub fn exit(&mut self) -> Option<&'static str> {
        if !self.active {
            return None;
        }
        self.active = false;
        self.trigger = None;
        self.roster.clear();
        Some(EXIT_REMINDER)
    }

    pub fn on_turn_end(&mut self) -> Option<&'static str> {
        if self.active && self.auto_exit_on_turn_end {
            self.exit()
        } else {
            None
        }
    }

    pub fn upsert_member(&mut self, member: SwarmMember) {
        if let Some(m) = self.roster.iter_mut().find(|m| m.id == member.id) {
            *m = member;
        } else {
            self.roster.push(member);
        }
    }

    /// An Agent fan-out / AgentSwarm call must be the sole tool call in a batch.
    pub fn veto_mixed_agent_swarm(tool_names: &[String]) -> Option<String> {
        let count = tool_names
            .iter()
            .filter(|n| *n == "Agent" || *n == "AgentSwarm")
            .count();
        if count == 0 {
            return None;
        }
        if count == 1 && tool_names.len() == 1 {
            return None;
        }
        if count > 1 {
            Some(
                "Multiple Agent / AgentSwarm fan-out calls in one step are not allowed. \
                 Run a single Agent or AgentSwarm invocation."
                    .into(),
            )
        } else {
            Some(
                "An Agent / AgentSwarm fan-out cannot be mixed with other tools in the same \
                 step. Call Agent or AgentSwarm alone."
                    .into(),
            )
        }
    }

    pub fn member_ids(&self) -> HashSet<String> {
        self.roster.iter().map(|m| m.id.clone()).collect()
    }
}

pub const ENTER_REMINDER: &str = "\
<system-reminder>
Swarm mode is active. Coordinate sub-agents carefully; prefer AgentSwarm for \
parallel fan-out and wait for results before concluding.
</system-reminder>";

pub const EXIT_REMINDER: &str = "\
<system-reminder>
Swarm mode ended. Resume single-agent execution.
</system-reminder>";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusivity() {
        assert!(SwarmService::veto_mixed_agent_swarm(&["Agent".into()]).is_none());
        assert!(SwarmService::veto_mixed_agent_swarm(&["AgentSwarm".into()]).is_none());
        assert!(SwarmService::veto_mixed_agent_swarm(&["Agent".into(), "Read".into()]).is_some());
        assert!(
            SwarmService::veto_mixed_agent_swarm(&["AgentSwarm".into(), "Read".into()]).is_some()
        );
        assert!(
            SwarmService::veto_mixed_agent_swarm(&["Agent".into(), "AgentSwarm".into()]).is_some()
        );
    }
}
