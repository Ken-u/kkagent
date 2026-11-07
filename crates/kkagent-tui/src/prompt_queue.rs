//! Next-turn prompt queue and user-message delivery states.

use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryState {
    Draft,
    Queued,
    Sending,
    Sent,
    Failed,
    Cancelled,
}

impl DeliveryState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Queued => "queued",
            Self::Sending => "sending…",
            Self::Sent => "",
            Self::Failed => "failed — edit & retry",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone)]
pub struct QueuedPrompt {
    pub id: String,
    pub session_id: String,
    pub text: String,
    pub images: Vec<(String, String)>,
    /// When true, send as steer into the current turn instead of next-turn queue.
    pub as_steer: bool,
}

impl QueuedPrompt {
    pub fn next_turn(session_id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            session_id: session_id.into(),
            text: text.into(),
            images: Vec::new(),
            as_steer: false,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct PromptQueue {
    pub items: Vec<QueuedPrompt>,
    pub selected: usize,
    /// Set when the user interrupted the turn. Queued prompts then stay put
    /// instead of being flushed the instant the session turns idle: Esc means
    /// "stop", and sending the next queued prompt right away starts another
    /// LLM request the user never asked for. Released by a new submit or by
    /// Enter on an empty composer.
    pub held: bool,
    /// The "queue paused" hint was already shown for the current hold.
    pub held_notified: bool,
}

impl PromptQueue {
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Pause automatic flushing (after a user interrupt).
    pub fn hold(&mut self) {
        if !self.held {
            self.held = true;
            self.held_notified = false;
        }
    }

    /// Resume automatic flushing.
    pub fn release(&mut self) {
        self.held = false;
        self.held_notified = false;
    }

    pub fn push(&mut self, item: QueuedPrompt) {
        self.items.push(item);
        self.selected = self.items.len().saturating_sub(1);
    }

    pub fn remove_selected(&mut self) -> Option<QueuedPrompt> {
        if self.items.is_empty() {
            return None;
        }
        let idx = self.selected.min(self.items.len() - 1);
        let item = self.items.remove(idx);
        if self.selected > 0 && self.selected >= self.items.len() {
            self.selected = self.items.len().saturating_sub(1);
        }
        Some(item)
    }

    pub fn move_selected(&mut self, delta: i32) {
        if self.items.len() < 2 {
            return;
        }
        let idx = self.selected.min(self.items.len() - 1);
        let target = if delta < 0 {
            idx.saturating_sub(1)
        } else {
            (idx + 1).min(self.items.len() - 1)
        };
        if target != idx {
            self.items.swap(idx, target);
            self.selected = target;
        }
    }

    pub fn pop_front(&mut self) -> Option<QueuedPrompt> {
        if self.items.is_empty() {
            None
        } else {
            if self.selected > 0 {
                self.selected -= 1;
            }
            Some(self.items.remove(0))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_reorder_and_pop() {
        let mut q = PromptQueue::default();
        q.push(QueuedPrompt::next_turn("session", "a"));
        q.push(QueuedPrompt::next_turn("session", "b"));
        q.push(QueuedPrompt::next_turn("session", "c"));
        q.selected = 2;
        q.move_selected(-1);
        assert_eq!(q.items[1].text, "c");
        assert_eq!(q.items[2].text, "b");
        let front = q.pop_front().unwrap();
        assert_eq!(front.text, "a");
        assert_eq!(q.items.len(), 2);
    }

    #[test]
    fn hold_is_sticky_until_released() {
        let mut q = PromptQueue::default();
        assert!(!q.held);
        q.hold();
        assert!(q.held);
        q.held_notified = true;
        // A second interrupt must not re-arm the one-time hint.
        q.hold();
        assert!(q.held_notified);
        q.release();
        assert!(!q.held);
        assert!(!q.held_notified);
    }
}
