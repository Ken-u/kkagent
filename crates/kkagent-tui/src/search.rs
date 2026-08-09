//! In-transcript search overlay (Ctrl-F).

use crate::app::{DisplayMessage, DisplayPart, MessageRole};
use crate::pi::fuzzy_match;

#[derive(Debug, Clone, Default)]
pub struct SearchState {
    pub query: String,
    pub hits: Vec<SearchHit>,
    pub selected: usize,
    pub active: bool,
}

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub message_index: usize,
    pub preview: String,
    pub role: String,
}

impl SearchState {
    pub fn open(&mut self) {
        self.active = true;
        self.query.clear();
        self.hits.clear();
        self.selected = 0;
    }

    pub fn close(&mut self) {
        self.active = false;
        self.query.clear();
        self.hits.clear();
        self.selected = 0;
    }

    pub fn recompute(&mut self, messages: &[DisplayMessage]) {
        let q = self.query.trim();
        self.hits.clear();
        if q.is_empty() {
            self.selected = 0;
            return;
        }
        for (i, msg) in messages.iter().enumerate() {
            let text = message_search_text(msg);
            if text.is_empty() {
                continue;
            }
            let m = fuzzy_match(q, &text);
            if !m.matches && !text.to_lowercase().contains(&q.to_lowercase()) {
                continue;
            }
            let preview = preview_line(&text, q, 72);
            let role = match msg.role {
                MessageRole::User => "user",
                MessageRole::Assistant => "assistant",
                MessageRole::System => "system",
                MessageRole::Plan => "plan",
            };
            self.hits.push(SearchHit {
                message_index: i,
                preview,
                role: role.into(),
            });
            if self.hits.len() >= 40 {
                break;
            }
        }
        if self.selected >= self.hits.len() {
            self.selected = self.hits.len().saturating_sub(1);
        }
    }

    pub fn next(&mut self) {
        if !self.hits.is_empty() {
            self.selected = (self.selected + 1) % self.hits.len();
        }
    }

    pub fn prev(&mut self) {
        if !self.hits.is_empty() {
            self.selected = if self.selected == 0 {
                self.hits.len() - 1
            } else {
                self.selected - 1
            };
        }
    }

    pub fn current(&self) -> Option<&SearchHit> {
        self.hits.get(self.selected)
    }
}

fn message_search_text(msg: &DisplayMessage) -> String {
    let mut out = String::new();
    if !msg.content.is_empty() {
        out.push_str(&msg.content);
    }
    if let Some(t) = &msg.thinking {
        out.push('\n');
        out.push_str(t);
    }
    for part in &msg.parts {
        match part {
            DisplayPart::Text(t) => {
                out.push('\n');
                out.push_str(t);
            }
            DisplayPart::Tool(tc) => {
                out.push('\n');
                out.push_str(&tc.name);
                out.push(' ');
                out.push_str(&tc.input_summary);
                if let Some(o) = &tc.output {
                    out.push('\n');
                    out.push_str(o);
                }
            }
        }
    }
    out
}

fn preview_line(text: &str, query: &str, max: usize) -> String {
    let lower = text.to_lowercase();
    let q = query.to_lowercase();
    let idx = lower.find(&q).unwrap_or(0);
    let start = idx.saturating_sub(20);
    let slice: String = text.chars().skip(start).take(max).collect();
    let mut s = if start > 0 {
        format!("…{slice}")
    } else {
        slice
    };
    if s.chars().count() >= max {
        s.push('…');
    }
    s.replace('\n', " ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::DisplayMessage;

    #[test]
    fn finds_user() {
        let msgs = vec![DisplayMessage {
            role: MessageRole::User,
            content: "please fix the login bug".into(),
            thinking: None,
            parts: vec![],
            tool_calls: vec![],
        }];
        let mut s = SearchState::default();
        s.query = "login".into();
        s.recompute(&msgs);
        assert_eq!(s.hits.len(), 1);
    }
}
