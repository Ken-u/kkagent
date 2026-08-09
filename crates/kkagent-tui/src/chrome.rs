//! Chrome: tab strip, status bar — aligned with kimi-code tui/chrome.

use kkagent_protocol::{PermissionMode, SessionStatus};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::theme::Theme;

#[derive(Debug, Clone)]
pub struct SessionTab {
    pub id: String,
    pub title: String,
    pub dirty: bool,
    pub status: SessionStatus,
}

#[derive(Debug, Default)]
pub struct TabStrip {
    pub tabs: Vec<SessionTab>,
    pub active: usize,
}

impl TabStrip {
    pub fn ensure_active(&mut self, id: &str, title: impl Into<String>) {
        if let Some(i) = self.tabs.iter().position(|t| t.id == id) {
            self.active = i;
            return;
        }
        self.tabs.push(SessionTab {
            id: id.to_string(),
            title: title.into(),
            dirty: false,
            status: SessionStatus::Idle,
        });
        self.active = self.tabs.len() - 1;
    }

    pub fn set_status(&mut self, id: &str, status: SessionStatus) {
        if let Some(t) = self.tabs.iter_mut().find(|t| t.id == id) {
            t.status = status;
        }
    }

    pub fn mark_dirty(&mut self, id: &str, dirty: bool) {
        if let Some(t) = self.tabs.iter_mut().find(|t| t.id == id) {
            t.dirty = dirty;
        }
    }

    pub fn next(&mut self) {
        if !self.tabs.is_empty() {
            self.active = (self.active + 1) % self.tabs.len();
        }
    }

    pub fn prev(&mut self) {
        if !self.tabs.is_empty() {
            self.active = if self.active == 0 {
                self.tabs.len() - 1
            } else {
                self.active - 1
            };
        }
    }

    pub fn active_id(&self) -> Option<&str> {
        self.tabs.get(self.active).map(|t| t.id.as_str())
    }

    /// Visible labels for a given width (with scroll markers).
    pub fn render_labels(&self, width: usize) -> String {
        if self.tabs.is_empty() {
            return String::new();
        }
        let labels: Vec<String> = self
            .tabs
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let mark = if t.dirty { "*" } else { "" };
                let active = if i == self.active { ">" } else { " " };
                format!("{active}{mark}{}", truncate(&t.title, 16))
            })
            .collect();
        let joined = labels.join(" │ ");
        if joined.chars().count() <= width.saturating_sub(2) {
            return format!(" {joined}");
        }
        let mut start = self.active;
        let mut end = (self.active + 1).min(labels.len());
        let mut guard = 0;
        while guard < 32 {
            guard += 1;
            let slice = labels[start..end].join(" │ ");
            let framed = format!("< {slice} >");
            if framed.chars().count() <= width {
                // try expand
                let mut expanded = false;
                if start > 0 {
                    let left = labels[start - 1..end].join(" │ ");
                    if format!("< {left} >").chars().count() <= width {
                        start -= 1;
                        expanded = true;
                    }
                }
                if end < labels.len() {
                    let right = labels[start..end + 1].join(" │ ");
                    if format!("< {right} >").chars().count() <= width {
                        end += 1;
                        expanded = true;
                    }
                }
                if !expanded {
                    return framed;
                }
            } else if end - start > 1 {
                if start < self.active {
                    start += 1;
                } else if end > self.active + 1 {
                    end -= 1;
                } else {
                    break;
                }
            } else {
                break;
            }
        }
        format!("< {} >", labels[start..end].join(" │ "))
    }
}

#[derive(Debug, Clone)]
pub struct StatusBarModel {
    pub session_id: Option<String>,
    pub model: Option<String>,
    pub permission: PermissionMode,
    pub plan_mode: bool,
    pub status: SessionStatus,
    pub tokens: u64,
    pub cache_hit: Option<f32>,
    pub cwd: Option<String>,
}

impl Default for StatusBarModel {
    fn default() -> Self {
        Self {
            session_id: None,
            model: None,
            permission: PermissionMode::Manual,
            plan_mode: false,
            status: SessionStatus::Idle,
            tokens: 0,
            cache_hit: None,
            cwd: None,
        }
    }
}

impl StatusBarModel {
    pub fn line(&self, theme: &Theme) -> Line<'static> {
        let mut spans = Vec::new();
        let mode = match self.permission {
            PermissionMode::Manual => "manual",
            PermissionMode::Yolo => "yolo",
            PermissionMode::Auto => "auto",
        };
        spans.push(Span::styled(
            format!(" {mode} "),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ));
        if self.plan_mode {
            spans.push(Span::styled(" plan ", Style::default().fg(theme.warning)));
        }
        let status = match self.status {
            SessionStatus::Idle => "idle",
            SessionStatus::Thinking => "thinking",
            SessionStatus::ToolExecuting => "tools",
            SessionStatus::WaitingApproval => "approval",
            SessionStatus::WaitingQuestion => "question",
            SessionStatus::Compacting => "compact",
        };
        spans.push(Span::raw(format!("│ {status} ")));
        if let Some(ref m) = self.model {
            spans.push(Span::raw(format!("│ {m} ")));
        }
        if self.tokens > 0 {
            spans.push(Span::styled(
                format!("│ ~{} tok ", format_tokens(self.tokens)),
                Style::default().fg(theme.text_muted),
            ));
        }
        if let Some(c) = self.cache_hit {
            spans.push(Span::styled(
                format!("│ cache {:.0}% ", c * 100.0),
                Style::default().fg(theme.text_muted),
            ));
        }
        if let Some(ref cwd) = self.cwd {
            spans.push(Span::styled(
                format!("│ {} ", truncate(cwd, 28)),
                Style::default().fg(theme.text_muted),
            ));
        }
        Line::from(spans)
    }
}

pub fn draw_tab_strip(f: &mut Frame, area: Rect, strip: &TabStrip, theme: &Theme) {
    let text = strip.render_labels(area.width as usize);
    let p = Paragraph::new(text).style(Style::default().fg(theme.accent));
    f.render_widget(p, area);
}

fn truncate(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        s.to_string()
    } else if max <= 1 {
        "…".into()
    } else {
        format!("{}…", chars[..max - 1].iter().collect::<String>())
    }
}

fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_scroll() {
        let mut s = TabStrip::default();
        for i in 0..8 {
            s.ensure_active(&format!("s{i}"), format!("session-{i}"));
        }
        let line = s.render_labels(40);
        assert!(line.contains('<') || line.contains("session"));
    }
}
