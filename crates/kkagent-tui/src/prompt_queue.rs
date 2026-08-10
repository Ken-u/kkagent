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
    pub text: String,
    pub images: Vec<(String, String)>,
    /// When true, send as steer into the current turn instead of next-turn queue.
    pub as_steer: bool,
}

impl QueuedPrompt {
    pub fn next_turn(text: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
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
}

impl PromptQueue {
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
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
        q.push(QueuedPrompt::next_turn("a"));
        q.push(QueuedPrompt::next_turn("b"));
        q.push(QueuedPrompt::next_turn("c"));
        q.selected = 2;
        q.move_selected(-1);
        assert_eq!(q.items[1].text, "c");
        assert_eq!(q.items[2].text, "b");
        let front = q.pop_front().unwrap();
        assert_eq!(front.text, "a");
        assert_eq!(q.items.len(), 2);
    }
}
