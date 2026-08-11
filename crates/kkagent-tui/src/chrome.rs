//! Chrome: tab strip, status bar, workspace session strip — aligned with kimi-code tui/chrome.

use kkagent_protocol::{PermissionMode, SessionStatus};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::theme::Theme;

const SESSION_TITLE_MAX_COLS: usize = 18;

fn session_status_mark(entry: &WorkspaceSessionEntry) -> &'static str {
    if entry.needs_attention {
        return "!";
    }
    if entry.dirty {
        return "*";
    }
    match entry.status {
        SessionStatus::Thinking
        | SessionStatus::ToolExecuting
        | SessionStatus::Compacting
        | SessionStatus::Cancelling => "…",
        SessionStatus::WaitingApproval | SessionStatus::WaitingQuestion => "?",
        SessionStatus::Idle => "",
    }
}

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
            // Refresh title when we know a better one.
            let title = title.into();
            if !title.is_empty() && title != "main" {
                self.tabs[i].title = title;
            }
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

    /// Ensure a tab exists without changing the active selection.
    pub fn ensure_tab(&mut self, id: &str, title: impl Into<String>) {
        if let Some(t) = self.tabs.iter_mut().find(|t| t.id == id) {
            let title = title.into();
            if !title.is_empty() && title != "main" {
                t.title = title;
            }
            return;
        }
        self.tabs.push(SessionTab {
            id: id.to_string(),
            title: title.into(),
            dirty: false,
            status: SessionStatus::Idle,
        });
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
                format!("{active}{mark}{}", truncate_cols(&t.title, 16))
            })
            .collect();
        scroll_join_labels(&labels, self.active, width)
    }
}

/// One workspace session shown in the footer context strip.
#[derive(Debug, Clone)]
pub struct WorkspaceSessionEntry {
    pub id: String,
    pub title: String,
    pub status: SessionStatus,
    pub dirty: bool,
    pub needs_attention: bool,
}

#[derive(Debug, Default, Clone)]
pub struct WorkspaceSessionStrip {
    pub entries: Vec<WorkspaceSessionEntry>,
    pub active: usize,
}

impl WorkspaceSessionStrip {
    /// Merge a freshly fetched entry list into the strip while preserving the
    /// previous relative order of still-present sessions (avoids tab jitter on
    /// periodic refresh). New ids are appended; missing ids are dropped.
    pub fn set_entries_stable(
        &mut self,
        mut incoming: Vec<WorkspaceSessionEntry>,
        active_id: Option<&str>,
    ) {
        if self.entries.is_empty() {
            self.set_entries(incoming, active_id);
            return;
        }
        let mut by_id: std::collections::HashMap<String, WorkspaceSessionEntry> =
            incoming.drain(..).map(|e| (e.id.clone(), e)).collect();
        let mut ordered = Vec::with_capacity(by_id.len());
        for prev in &self.entries {
            if let Some(e) = by_id.remove(&prev.id) {
                ordered.push(e);
            }
        }
        // Preserve relative order of brand-new ids as provided by the caller.
        let mut rest: Vec<_> = by_id.into_values().collect();
        rest.sort_by(|a, b| a.id.cmp(&b.id));
        ordered.extend(rest);
        self.set_entries(ordered, active_id);
    }

    pub fn set_entries(&mut self, entries: Vec<WorkspaceSessionEntry>, active_id: Option<&str>) {
        self.entries = entries;
        self.active = active_id
            .and_then(|id| self.entries.iter().position(|e| e.id == id))
            .unwrap_or(0);
    }

    pub fn active_id(&self) -> Option<&str> {
        self.entries.get(self.active).map(|e| e.id.as_str())
    }

    pub fn next_id(&mut self) -> Option<String> {
        if self.entries.len() < 2 {
            return None;
        }
        self.active = (self.active + 1) % self.entries.len();
        self.active_id().map(|s| s.to_string())
    }

    pub fn prev_id(&mut self) -> Option<String> {
        if self.entries.len() < 2 {
            return None;
        }
        self.active = if self.active == 0 {
            self.entries.len() - 1
        } else {
            self.active - 1
        };
        self.active_id().map(|s| s.to_string())
    }

    /// Render a scrolling strip that always keeps the active entry visible.
    /// Caller passes remaining width after reserving the context meter.
    /// Returns spans plus hit boxes relative to the strip's starting column (0-based).
    pub fn render_spans_with_hits(
        &self,
        max_cols: usize,
        theme: &Theme,
    ) -> (Vec<Span<'static>>, Vec<SessionStripHit>) {
        if self.entries.is_empty() || max_cols < 4 {
            return (Vec::new(), Vec::new());
        }
        let labels: Vec<(bool, String, String)> = self
            .entries
            .iter()
            .enumerate()
            .map(|(i, e)| {
                let mark = session_status_mark(e);
                let title = truncate_cols(&e.title, SESSION_TITLE_MAX_COLS);
                let label = if i == self.active {
                    format!("[{mark}{title}]")
                } else {
                    format!("{mark}{title}")
                };
                (i == self.active, label, e.id.clone())
            })
            .collect();

        let label_pairs: Vec<(bool, String)> =
            labels.iter().map(|(a, l, _)| (*a, l.clone())).collect();
        let (start, end, left_overflow, right_overflow) =
            visible_window(&label_pairs, self.active, max_cols);

        let mut spans = Vec::new();
        let mut hits = Vec::new();
        let mut col = 0usize;
        if left_overflow {
            let s = "‹ ".to_string();
            col += UnicodeWidthStr::width(s.as_str());
            spans.push(Span::styled(s, Style::default().fg(theme.text_muted)));
        }
        for (i, (is_active, label, id)) in labels[start..end].iter().enumerate() {
            if i > 0 {
                let sep = " · ".to_string();
                col += UnicodeWidthStr::width(sep.as_str());
                spans.push(Span::styled(sep, Style::default().fg(theme.text_muted)));
            }
            let w = UnicodeWidthStr::width(label.as_str());
            hits.push(SessionStripHit {
                session_id: id.clone(),
                full_title: self
                    .entries
                    .iter()
                    .find(|e| e.id == *id)
                    .map(|e| e.title.clone())
                    .unwrap_or_else(|| id.clone()),
                x0: col,
                x1: col + w,
            });
            col += w;
            if *is_active {
                spans.push(Span::styled(
                    label.clone(),
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                ));
            } else {
                spans.push(Span::styled(
                    label.clone(),
                    Style::default().fg(theme.text_dim),
                ));
            }
        }
        if right_overflow {
            spans.push(Span::styled(
                " ›".to_string(),
                Style::default().fg(theme.text_muted),
            ));
        }
        (spans, hits)
    }

    pub fn render_spans(&self, max_cols: usize, theme: &Theme) -> Vec<Span<'static>> {
        self.render_spans_with_hits(max_cols, theme).0
    }
}

#[derive(Debug, Clone)]
pub struct SessionStripHit {
    pub session_id: String,
    pub full_title: String,
    /// Column offsets relative to the start of the session strip text.
    pub x0: usize,
    pub x1: usize,
}

/// Prefer custom `/title`, else first/last prompt snippet, else short id.
/// Harness-only injections (`<system-reminder>`, …) never become the label.
pub fn session_display_title(
    title: Option<&str>,
    is_custom_title: bool,
    last_prompt: Option<&str>,
    session_id: &str,
) -> String {
    let clean = |s: &str| {
        s.chars()
            .map(|c| if c.is_whitespace() { ' ' } else { c })
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };
    let usable = |raw: &str| -> Option<String> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return None;
        }
        if kkagent_protocol::is_harness_only_user_text(trimmed) {
            return None;
        }
        let visible = kkagent_protocol::visible_user_text(trimmed);
        let pick = if visible.is_empty() {
            trimmed
        } else {
            visible.as_str()
        };
        if pick.is_empty() || kkagent_protocol::is_harness_only_user_text(pick) {
            return None;
        }
        Some(clean(pick))
    };
    if is_custom_title {
        if let Some(t) = title.and_then(usable) {
            return t;
        }
    }
    if let Some(p) = last_prompt.and_then(usable) {
        return p;
    }
    if let Some(t) = title.and_then(usable) {
        return t;
    }
    let short = if session_id.len() > 8 {
        &session_id[..8]
    } else {
        session_id
    };
    short.to_string()
}

/// Collect the fork family for `current_id` (root + all descendants).
/// Returns empty when there is no fork (family size < 2).
pub fn fork_family_ids(rows: &[(String, Option<String>)], current_id: &str) -> Vec<String> {
    if current_id.is_empty() || rows.is_empty() {
        return Vec::new();
    }
    let parent_of: std::collections::HashMap<&str, Option<&str>> = rows
        .iter()
        .map(|(id, parent)| (id.as_str(), parent.as_deref()))
        .collect();
    if !parent_of.contains_key(current_id) {
        return Vec::new();
    }

    let root_of = |start: &str| -> String {
        let mut cursor = start;
        let mut guard = 0usize;
        while let Some(Some(parent)) = parent_of.get(cursor).copied() {
            if !parent_of.contains_key(parent) {
                break;
            }
            cursor = parent;
            guard += 1;
            if guard > 64 {
                break;
            }
        }
        cursor.to_string()
    };

    let root = root_of(current_id);
    let mut family: Vec<String> = rows
        .iter()
        .filter(|(id, _)| root_of(id) == root)
        .map(|(id, _)| id.clone())
        .collect();
    family.sort();
    family.dedup();
    family.sort_by(|a, b| {
        if a == &root {
            std::cmp::Ordering::Less
        } else if b == &root {
            std::cmp::Ordering::Greater
        } else {
            a.cmp(b)
        }
    });
    if family.len() < 2 {
        Vec::new()
    } else {
        family
    }
}

fn visible_window(
    labels: &[(bool, String)],
    active: usize,
    max_cols: usize,
) -> (usize, usize, bool, bool) {
    if labels.is_empty() {
        return (0, 0, false, false);
    }
    let sep_w = UnicodeWidthStr::width(" · ");

    let width_of = |start: usize, end: usize, left: bool, right: bool| -> usize {
        let mut w = 0usize;
        for (i, (_, label)) in labels[start..end].iter().enumerate() {
            if i > 0 {
                w = w.saturating_add(sep_w);
            }
            w = w.saturating_add(UnicodeWidthStr::width(label.as_str()));
        }
        if left {
            w = w.saturating_add(UnicodeWidthStr::width("‹ "));
        }
        if right {
            w = w.saturating_add(UnicodeWidthStr::width(" ›"));
        }
        w
    };

    let active = active.min(labels.len() - 1);
    let mut start = active;
    let mut end = active + 1;
    loop {
        let left = start > 0;
        let right = end < labels.len();
        let cur = width_of(start, end, left, right);
        if cur > max_cols && end - start > 1 {
            if start < active {
                start += 1;
            } else if end > active + 1 {
                end -= 1;
            } else {
                break;
            }
            continue;
        }
        let mut expanded = false;
        if start > 0 {
            let trial = width_of(start - 1, end, start > 1, end < labels.len());
            if trial <= max_cols {
                start -= 1;
                expanded = true;
            }
        }
        if end < labels.len() {
            let trial = width_of(start, end + 1, start > 0, end + 1 < labels.len());
            if trial <= max_cols {
                end += 1;
                expanded = true;
            }
        }
        if !expanded {
            break;
        }
    }
    (start, end, start > 0, end < labels.len())
}

fn scroll_join_labels(labels: &[String], active: usize, width: usize) -> String {
    let joined = labels.join(" │ ");
    if UnicodeWidthStr::width(joined.as_str()) <= width.saturating_sub(2) {
        return format!(" {joined}");
    }
    let mut start = active.min(labels.len().saturating_sub(1));
    let mut end = (start + 1).min(labels.len());
    let mut guard = 0;
    while guard < 32 {
        guard += 1;
        let slice = labels[start..end].join(" │ ");
        let framed = format!("< {slice} >");
        if UnicodeWidthStr::width(framed.as_str()) <= width {
            let mut expanded = false;
            if start > 0 {
                let left = labels[start - 1..end].join(" │ ");
                if UnicodeWidthStr::width(format!("< {left} >").as_str()) <= width {
                    start -= 1;
                    expanded = true;
                }
            }
            if end < labels.len() {
                let right = labels[start..end + 1].join(" │ ");
                if UnicodeWidthStr::width(format!("< {right} >").as_str()) <= width {
                    end += 1;
                    expanded = true;
                }
            }
            if !expanded {
                return framed;
            }
        } else if end - start > 1 {
            if start < active {
                start += 1;
            } else if end > active + 1 {
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
    /// Non-blocking busy / error / MCP notice for the footer tip slot.
    pub activity: Option<String>,
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
            activity: None,
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
            SessionStatus::Cancelling => "cancelling",
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
                format!("│ {} ", truncate_cols(cwd, 28)),
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

fn truncate_cols(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(s) <= max {
        return s.to_string();
    }
    if max == 1 {
        return "…".into();
    }
    let mut out = String::new();
    let mut w = 0usize;
    for ch in s.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if w + cw > max - 1 {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
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

    #[test]
    fn display_title_prefers_custom() {
        let t = session_display_title(Some("My Name"), true, Some("first prompt here"), "abcdef12");
        assert_eq!(t, "My Name");
        let t = session_display_title(Some("auto"), false, Some("first prompt here"), "abcdef12");
        assert_eq!(t, "first prompt here");
    }

    #[test]
    fn display_title_skips_system_reminder() {
        let rem = "<system-reminder>\nToday's date is 2026-08-10.\n</system-reminder>";
        let t = session_display_title(Some(rem), false, Some(rem), "abcdef12");
        assert_eq!(t, "abcdef12");
        let t = session_display_title(Some(rem), false, Some("real question"), "abcdef12");
        assert_eq!(t, "real question");
    }

    #[test]
    fn fork_family_hides_when_alone() {
        let rows = vec![("a".into(), None), ("b".into(), None)];
        assert!(fork_family_ids(&rows, "a").is_empty());
    }

    #[test]
    fn fork_family_includes_root_and_forks() {
        let rows = vec![
            ("root".into(), None),
            ("f1".into(), Some("root".into())),
            ("f2".into(), Some("root".into())),
            ("other".into(), None),
        ];
        let family = fork_family_ids(&rows, "f1");
        assert_eq!(family, vec!["root".to_string(), "f1".into(), "f2".into()]);
        assert!(fork_family_ids(&rows, "other").is_empty());
    }

    #[test]
    fn workspace_strip_keeps_active_visible() {
        let theme = Theme::default();
        let mut strip = WorkspaceSessionStrip::default();
        let mut entries = Vec::new();
        for i in 0..12 {
            entries.push(WorkspaceSessionEntry {
                id: format!("id{i}"),
                title: format!("session-title-number-{i}"),
                status: SessionStatus::Idle,
                dirty: false,
                needs_attention: false,
            });
        }
        strip.set_entries(entries, Some("id8"));
        let spans = strip.render_spans(40, &theme);
        let text: String = spans.iter().map(|s| s.content.clone()).collect();
        assert!(text.contains('8') || text.contains('['));
        assert!(
            text.contains('‹') || text.contains('›') || text.contains('·') || text.contains('[')
        );
    }

    #[test]
    fn workspace_strip_stable_order_on_refresh() {
        let mut strip = WorkspaceSessionStrip::default();
        strip.set_entries(
            vec![
                WorkspaceSessionEntry {
                    id: "a".into(),
                    title: "A".into(),
                    status: SessionStatus::Idle,
                    dirty: false,
                    needs_attention: false,
                },
                WorkspaceSessionEntry {
                    id: "b".into(),
                    title: "B".into(),
                    status: SessionStatus::Idle,
                    dirty: false,
                    needs_attention: false,
                },
                WorkspaceSessionEntry {
                    id: "c".into(),
                    title: "C".into(),
                    status: SessionStatus::Idle,
                    dirty: false,
                    needs_attention: false,
                },
            ],
            Some("b"),
        );
        // Server returns shuffled order + one new id; relative order of a/b/c must hold.
        strip.set_entries_stable(
            vec![
                WorkspaceSessionEntry {
                    id: "c".into(),
                    title: "C2".into(),
                    status: SessionStatus::Thinking,
                    dirty: true,
                    needs_attention: false,
                },
                WorkspaceSessionEntry {
                    id: "a".into(),
                    title: "A2".into(),
                    status: SessionStatus::Idle,
                    dirty: false,
                    needs_attention: false,
                },
                WorkspaceSessionEntry {
                    id: "d".into(),
                    title: "D".into(),
                    status: SessionStatus::Idle,
                    dirty: false,
                    needs_attention: false,
                },
                WorkspaceSessionEntry {
                    id: "b".into(),
                    title: "B2".into(),
                    status: SessionStatus::Idle,
                    dirty: false,
                    needs_attention: true,
                },
            ],
            Some("b"),
        );
        let ids: Vec<_> = strip.entries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b", "c", "d"]);
        assert_eq!(strip.active_id(), Some("b"));
        assert_eq!(strip.entries[1].title, "B2");
        assert!(strip.entries[1].needs_attention);
    }
}
