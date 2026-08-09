//! Structured activity stream for TUI activity-pane / RPC.

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ActivityItem {
    Thinking {
        text: String,
    },
    ToolStart {
        id: String,
        name: String,
        summary: String,
    },
    ToolEnd {
        id: String,
        name: String,
        is_error: bool,
    },
    Message {
        role: String,
        preview: String,
    },
    Goal {
        status: String,
        detail: String,
    },
    Subagent {
        id: String,
        status: String,
    },
    Note {
        text: String,
    },
}

#[derive(Debug, Default)]
pub struct ActivityView {
    items: VecDeque<ActivityItem>,
    cap: usize,
}

impl ActivityView {
    pub fn new(cap: usize) -> Self {
        Self {
            items: VecDeque::new(),
            cap: cap.max(8),
        }
    }

    pub fn push(&mut self, item: ActivityItem) {
        self.items.push_back(item);
        while self.items.len() > self.cap {
            self.items.pop_front();
        }
    }

    pub fn list(&self) -> Vec<ActivityItem> {
        self.items.iter().cloned().collect()
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }
}
