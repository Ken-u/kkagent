//! In-transcript and cross-session (FTS) search overlay (Ctrl-F).

use crate::app::{DisplayMessage, DisplayPart, MessageRole};
use crate::pi::fuzzy_match;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SearchScope {
    /// Search only the currently loaded transcript messages.
    #[default]
    Local,
    /// Cross-session FTS via `sessions.search`.
    Global,
}

#[derive(Debug, Clone, Default)]
pub struct SearchState {
    pub query: String,
    pub hits: Vec<SearchHit>,
    pub selected: usize,
    pub active: bool,
    pub scope: SearchScope,
    /// Optional filters for global FTS (parsed from query prefixes).
    pub title_filter: Option<String>,
    pub tool_filter: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub message_index: usize,
    pub preview: String,
    pub role: String,
    /// Set for global FTS hits so Enter can resume that session.
    pub session_id: Option<String>,
    pub title: Option<String>,
}

impl SearchState {
    pub fn open(&mut self) {
        self.active = true;
        self.query.clear();
        self.hits.clear();
        self.selected = 0;
        self.scope = SearchScope::Local;
        self.title_filter = None;
        self.tool_filter = None;
    }

    pub fn close(&mut self) {
        self.active = false;
        self.query.clear();
        self.hits.clear();
        self.selected = 0;
        self.title_filter = None;
        self.tool_filter = None;
    }

    pub fn toggle_scope(&mut self) {
        self.scope = match self.scope {
            SearchScope::Local => SearchScope::Global,
            SearchScope::Global => SearchScope::Local,
        };
        self.hits.clear();
        self.selected = 0;
    }

    pub fn recompute(&mut self, messages: &[DisplayMessage]) {
        let (needle, title, tool) = parse_search_query(&self.query);
        self.title_filter = title;
        self.tool_filter = tool;
        self.hits.clear();
        if needle.is_empty() && self.title_filter.is_none() && self.tool_filter.is_none() {
            self.selected = 0;
            return;
        }
        if self.scope != SearchScope::Local {
            return;
        }
        for (i, msg) in messages.iter().enumerate() {
            let text = message_search_text(msg);
            if text.is_empty() {
                continue;
            }
            let m = fuzzy_match(&needle, &text);
            if !needle.is_empty()
                && !m.matches
                && !text.to_lowercase().contains(&needle.to_lowercase())
            {
                continue;
            }
            if let Some(tool) = self.tool_filter.as_deref() {
                let tool_l = tool.to_lowercase();
                let has_tool = msg.tool_calls.iter().any(|t| {
                    t.name.eq_ignore_ascii_case(tool) || t.name.to_lowercase().contains(&tool_l)
                }) || msg.parts.iter().any(|p| match p {
                    DisplayPart::Tool(tc) => {
                        tc.name.eq_ignore_ascii_case(tool)
                            || tc.name.to_lowercase().contains(&tool_l)
                    }
                    DisplayPart::ToolHistory(hist) => hist.tools.iter().any(|t| {
                        t.name.eq_ignore_ascii_case(tool) || t.name.to_lowercase().contains(&tool_l)
                    }),
                    _ => false,
                });
                if !has_tool && !text.to_lowercase().contains(&tool_l) {
                    continue;
                }
            }
            let preview = preview_line(&text, &needle, 72);
            let role = match msg.role {
                MessageRole::User => "user",
                MessageRole::Assistant => "assistant",
                MessageRole::System => "system",
                MessageRole::Plan => "plan",
                MessageRole::Skill => "skill",
            };
            self.hits.push(SearchHit {
                message_index: i,
                preview,
                role: role.into(),
                session_id: None,
                title: None,
            });
            if self.hits.len() >= 40 {
                break;
            }
        }
        if self.selected >= self.hits.len() {
            self.selected = self.hits.len().saturating_sub(1);
        }
    }

    /// Apply FTS hits from `sessions.search`.
    pub fn apply_global_hits(&mut self, hits: Vec<SearchHit>) {
        self.hits = hits;
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

/// Parse `title:foo tool:Bash rest of query` style filters.
pub fn parse_search_query(raw: &str) -> (String, Option<String>, Option<String>) {
    let mut title = None;
    let mut tool = None;
    let mut rest = Vec::new();
    for token in raw.split_whitespace() {
        if let Some(v) = token.strip_prefix("title:") {
            if !v.is_empty() {
                title = Some(v.to_string());
            }
        } else if let Some(v) = token
            .strip_prefix("tool:")
            .or_else(|| token.strip_prefix("tool_name:"))
        {
            if !v.is_empty() {
                tool = Some(v.to_string());
            }
        } else {
            rest.push(token);
        }
    }
    (rest.join(" "), title, tool)
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
            DisplayPart::ToolHistory(hist) => {
                out.push('\n');
                out.push_str(&format!("{} tool calls", hist.tool_count));
                for tc in &hist.tools {
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
            DisplayPart::SkillActivation { name, args } => {
                out.push('\n');
                out.push_str("Activated skill: ");
                out.push_str(name);
                if let Some(a) = args {
                    out.push('\n');
                    out.push_str(a);
                }
            }
        }
    }
    out
}

fn preview_line(text: &str, query: &str, max: usize) -> String {
    let lower = text.to_lowercase();
    let q = query.to_lowercase();
    let idx = if q.is_empty() {
        0
    } else {
        lower.find(&q).unwrap_or(0)
    };
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
    fn parse_filters_from_query() {
        let (q, title, tool) = parse_search_query("title:demo tool:Bash hello world");
        assert_eq!(q, "hello world");
        assert_eq!(title.as_deref(), Some("demo"));
        assert_eq!(tool.as_deref(), Some("Bash"));
    }

    #[test]
    fn finds_user() {
        let msgs = vec![DisplayMessage {
            role: MessageRole::User,
            content: "please fix the login bug".into(),
            thinking: None,
            parts: vec![],
            tool_calls: vec![],
            delivery: crate::prompt_queue::DeliveryState::Sent,
            idempotency_key: None,
        }];
        let mut s = SearchState {
            query: "login".into(),
            ..SearchState::default()
        };
        s.recompute(&msgs);
        assert_eq!(s.hits.len(), 1);
    }
}
