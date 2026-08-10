//! TUI controllers — session events, streaming UI, tasks (kimi-code controllers).

use kkagent_protocol::{AgentEvent, SessionStatus};

use crate::chrome::{StatusBarModel, TabStrip};
use crate::streaming::StreamingCursor;

#[derive(Debug)]
pub struct SessionEventRouter {
    pub status: SessionStatus,
    pub last_error: Option<String>,
    pub turn_active: bool,
}

impl Default for SessionEventRouter {
    fn default() -> Self {
        Self {
            status: SessionStatus::Idle,
            last_error: None,
            turn_active: false,
        }
    }
}

impl SessionEventRouter {
    pub fn on_event(
        &mut self,
        ev: &AgentEvent,
        tabs: &mut TabStrip,
        status: &mut StatusBarModel,
        current_session_id: Option<&str>,
    ) {
        let sid = ev.session_id();
        let is_current = current_session_id == Some(sid);
        match ev {
            AgentEvent::StatusUpdate {
                session_id,
                status: s,
            } => {
                tabs.set_status(session_id, *s);
                if is_current {
                    self.status = *s;
                    status.status = *s;
                    if matches!(s, SessionStatus::Idle | SessionStatus::Compacting) {
                        self.turn_active = false;
                    }
                }
            }
            AgentEvent::TurnStart { session_id } => {
                tabs.mark_dirty(session_id, true);
                if is_current {
                    self.turn_active = true;
                    status.status = SessionStatus::Thinking;
                }
            }
            AgentEvent::TurnEnd { session_id, .. } => {
                tabs.mark_dirty(session_id, false);
                if is_current {
                    self.turn_active = false;
                    tabs.mark_dirty(session_id, false);
                    status.status = SessionStatus::Idle;
                    self.status = SessionStatus::Idle;
                }
            }
            AgentEvent::Error { message, .. } => {
                if is_current {
                    self.last_error = Some(message.clone());
                }
            }
            AgentEvent::UsageUpdate { usage, .. } => {
                if is_current {
                    status.tokens = status
                        .tokens
                        .saturating_add(usage.input_tokens.saturating_add(usage.output_tokens));
                    if usage.cache_read_input_tokens > 0 {
                        let total = usage.input_tokens.max(1);
                        status.cache_hit =
                            Some(usage.cache_read_input_tokens as f32 / total as f32);
                    }
                }
            }
            _ => {}
        }
    }
}

#[derive(Debug, Default)]
pub struct StreamingUiController {
    pub cursor: StreamingCursor,
    pub thinking: String,
    pub assistant_buf: String,
}

impl StreamingUiController {
    pub fn on_delta(&mut self, text: &str) {
        self.assistant_buf.push_str(text);
        self.cursor.tick();
    }

    pub fn on_thinking(&mut self, text: &str) {
        self.thinking.push_str(text);
    }

    pub fn reset_turn(&mut self) {
        self.cursor = StreamingCursor::default();
        self.thinking.clear();
        self.assistant_buf.clear();
    }
}

#[derive(Debug, Clone)]
pub struct CacheHint {
    pub hit_ratio: Option<f32>,
    pub message: String,
}

impl CacheHint {
    pub fn from_usage(cached: u64, total: u64) -> Self {
        if total == 0 {
            return Self {
                hit_ratio: None,
                message: String::new(),
            };
        }
        let ratio = cached as f32 / total as f32;
        Self {
            hit_ratio: Some(ratio),
            message: if ratio > 0.5 {
                format!("cache hit {:.0}%", ratio * 100.0)
            } else {
                String::new()
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_hint() {
        let h = CacheHint::from_usage(80, 100);
        assert!((h.hit_ratio.unwrap() - 0.8).abs() < 0.01);
    }
}
