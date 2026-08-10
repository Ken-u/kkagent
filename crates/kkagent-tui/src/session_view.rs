//! Per-session TUI view state that must not leak across session switches.

use crate::input::InputState;
use crate::search::SearchState;

#[derive(Debug, Clone)]
pub struct SessionViewState {
    pub draft: String,
    pub cursor: usize,
    pub scroll_up: u16,
    pub follow_bottom: bool,
    pub todos_expanded: bool,
    pub search: SearchState,
    pub highlight_message: Option<usize>,
}

impl Default for SessionViewState {
    fn default() -> Self {
        Self {
            draft: String::new(),
            cursor: 0,
            scroll_up: 0,
            follow_bottom: true,
            todos_expanded: false,
            search: SearchState::default(),
            highlight_message: None,
        }
    }
}

impl SessionViewState {
    pub fn capture(
        input: &InputState,
        scroll_up: u16,
        follow_bottom: bool,
        todos_expanded: bool,
        search: &SearchState,
        highlight_message: Option<usize>,
    ) -> Self {
        Self {
            draft: input.text.clone(),
            cursor: input.cursor,
            scroll_up,
            follow_bottom,
            todos_expanded,
            search: search.clone(),
            highlight_message,
        }
    }

    pub fn restore_into(
        self,
        input: &mut InputState,
        scroll_up: &mut u16,
        follow_bottom: &mut bool,
        todos_expanded: &mut bool,
        search: &mut SearchState,
        highlight_message: &mut Option<usize>,
    ) {
        input.set_text(self.draft);
        input.cursor = self.cursor.min(input.text.len());
        *scroll_up = self.scroll_up;
        *follow_bottom = self.follow_bottom;
        *todos_expanded = self.todos_expanded;
        *search = self.search;
        *highlight_message = self.highlight_message;
    }
}

/// Small LRU for `/sessions` preview payloads.
#[derive(Debug, Default)]
pub struct PreviewLru {
    entries: Vec<(String, serde_json::Value)>,
    cap: usize,
}

impl PreviewLru {
    pub fn new(cap: usize) -> Self {
        Self {
            entries: Vec::new(),
            cap: cap.max(1),
        }
    }

    pub fn get(&mut self, session_id: &str) -> Option<serde_json::Value> {
        if let Some(idx) = self.entries.iter().position(|(id, _)| id == session_id) {
            let entry = self.entries.remove(idx);
            let value = entry.1.clone();
            self.entries.push(entry);
            return Some(value);
        }
        None
    }

    pub fn put(&mut self, session_id: String, value: serde_json::Value) {
        self.entries.retain(|(id, _)| id != &session_id);
        self.entries.push((session_id, value));
        while self.entries.len() > self.cap {
            self.entries.remove(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lru_evicts_oldest() {
        let mut lru = PreviewLru::new(2);
        lru.put("a".into(), serde_json::json!(1));
        lru.put("b".into(), serde_json::json!(2));
        lru.put("c".into(), serde_json::json!(3));
        assert!(lru.get("a").is_none());
        assert!(lru.get("b").is_some());
        assert!(lru.get("c").is_some());
    }
}
