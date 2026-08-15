//! Sticky subagent status strip + `/agents` detail panel.
//!
//! Child-agent tool/message floods stay out of the main transcript; the strip
//! only shows a compact live line (name, activity, elapsed). Full logs live in
//! the `/agents` overlay.

use std::collections::VecDeque;
use std::time::Instant;

const MAX_EVENT_LOG: usize = 120;
const MAX_STRIP_ROWS: usize = 3;
/// Keep finished agents on the strip briefly so completion is visible.
const FINISHED_STRIP_SECS: u64 = 8;

#[derive(Debug, Clone)]
pub struct SubagentUiEntry {
    pub id: String,
    pub name: String,
    pub description: String,
    pub status: String,
    /// Latest short activity (tool name / summary).
    pub activity: String,
    pub started_at: Instant,
    pub finished_at: Option<Instant>,
    pub events: VecDeque<String>,
    pub result_or_error: Option<String>,
}

impl SubagentUiEntry {
    pub fn is_active(&self) -> bool {
        matches!(self.status.as_str(), "pending" | "running")
    }

    pub fn show_on_strip(&self, now: Instant) -> bool {
        if self.is_active() {
            return true;
        }
        self.finished_at
            .is_some_and(|finished| now.duration_since(finished).as_secs() < FINISHED_STRIP_SECS)
    }

    pub fn elapsed_secs(&self, now: Instant) -> u64 {
        let end = self.finished_at.unwrap_or(now);
        end.duration_since(self.started_at).as_secs()
    }

    pub fn strip_line(&self, now: Instant) -> String {
        let short_id = short_id(&self.id);
        let activity = if self.activity.trim().is_empty() {
            if self.description.trim().is_empty() {
                match self.status.as_str() {
                    "complete" => "done".into(),
                    "failed" => "failed".into(),
                    "cancelled" => "cancelled".into(),
                    "pending" => "starting…".into(),
                    _ => "working…".into(),
                }
            } else {
                truncate_chars(&self.description, 48)
            }
        } else {
            truncate_chars(&self.activity, 48)
        };
        format!(
            "Agent {name}({id}): {activity} {secs}s",
            name = self.name,
            id = short_id,
            activity = activity,
            secs = self.elapsed_secs(now)
        )
    }

    pub fn push_event(&mut self, line: impl Into<String>) {
        let line = line.into();
        if line.trim().is_empty() {
            return;
        }
        self.activity = truncate_chars(&line, 64);
        self.events.push_back(line);
        while self.events.len() > MAX_EVENT_LOG {
            self.events.pop_front();
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SubagentStore {
    pub entries: Vec<SubagentUiEntry>,
}

#[derive(Debug, Clone)]
pub struct SubagentsPanelState {
    pub selected: usize,
    /// When true, show the selected agent's event log instead of the list.
    pub detail: bool,
}

impl SubagentStore {
    pub fn upsert_spawned(
        &mut self,
        id: String,
        name: String,
        description: String,
        status: impl Into<String>,
    ) {
        if let Some(existing) = self.entries.iter_mut().find(|e| e.id == id) {
            existing.name = name;
            if !description.is_empty() {
                existing.description = description.clone();
            }
            existing.status = status.into();
            if existing.activity.is_empty() && !description.is_empty() {
                existing.activity = truncate_chars(&description, 64);
            }
            return;
        }
        let activity = if description.is_empty() {
            String::new()
        } else {
            truncate_chars(&description, 64)
        };
        self.entries.push(SubagentUiEntry {
            id,
            name,
            description,
            status: status.into(),
            activity,
            started_at: Instant::now(),
            finished_at: None,
            events: VecDeque::new(),
            result_or_error: None,
        });
    }

    pub fn set_status(&mut self, id: &str, status: &str, detail: Option<String>) {
        let Some(entry) = self.entries.iter_mut().find(|e| e.id == id) else {
            return;
        };
        entry.status = status.to_string();
        if matches!(status, "complete" | "failed" | "cancelled") {
            entry.finished_at = Some(Instant::now());
        }
        if let Some(detail) = detail {
            if !detail.is_empty() {
                entry.result_or_error = Some(detail.clone());
                entry.push_event(detail);
            }
        }
        if status == "running" && entry.activity.is_empty() {
            entry.activity = if entry.description.is_empty() {
                "running…".into()
            } else {
                truncate_chars(&entry.description, 64)
            };
        }
    }

    pub fn note_child_event(&mut self, id: &str, line: String) {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.id == id) {
            if entry.status == "pending" {
                entry.status = "running".into();
            }
            entry.push_event(line);
        }
    }

    pub fn strip_lines(&self, now: Instant) -> Vec<String> {
        let visible: Vec<_> = self
            .entries
            .iter()
            .filter(|e| e.show_on_strip(now))
            .collect();
        if visible.is_empty() {
            return Vec::new();
        }
        let mut lines: Vec<String> = visible
            .iter()
            .take(MAX_STRIP_ROWS)
            .map(|e| e.strip_line(now))
            .collect();
        let extra = visible.len().saturating_sub(MAX_STRIP_ROWS);
        if extra > 0 {
            lines.push(format!("… +{extra} more · /agents"));
        } else if visible.iter().any(|e| e.is_active()) {
            // Hint once when there is only a single strip row budget left unused.
            if lines.len() == 1 {
                // keep compact; panel is discoverable via /agents
            }
        }
        lines
    }

    pub fn any_active(&self) -> bool {
        self.entries.iter().any(SubagentUiEntry::is_active)
    }

    pub fn prune_finished(&mut self, now: Instant) {
        self.entries.retain(|e| {
            e.is_active()
                || e.finished_at
                    .is_none_or(|finished| now.duration_since(finished).as_secs() < 300)
        });
    }
}

pub fn short_id(id: &str) -> &str {
    if id.len() > 8 {
        &id[..8]
    } else {
        id
    }
}

pub fn truncate_chars(s: &str, max: usize) -> String {
    let trimmed = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = trimmed.chars().take(max).collect();
    if trimmed.chars().count() > max {
        out.push('…');
    }
    out
}

pub fn format_tool_activity(tool_name: &str, input: &serde_json::Value) -> String {
    let brief = match tool_name {
        "Bash" | "Shell" => input
            .get("command")
            .and_then(|v| v.as_str())
            .map(|c| truncate_chars(c, 40))
            .unwrap_or_default(),
        "Read" | "ReadMediaFile" => input
            .get("path")
            .or_else(|| input.get("file_path"))
            .and_then(|v| v.as_str())
            .map(|p| truncate_chars(p, 40))
            .unwrap_or_default(),
        "Grep" | "Glob" => input
            .get("pattern")
            .or_else(|| input.get("glob"))
            .and_then(|v| v.as_str())
            .map(|p| truncate_chars(p, 40))
            .unwrap_or_default(),
        "Write" | "Edit" => input
            .get("path")
            .or_else(|| input.get("file_path"))
            .and_then(|v| v.as_str())
            .map(|p| truncate_chars(p, 40))
            .unwrap_or_default(),
        _ => {
            let raw = serde_json::to_string(input).unwrap_or_default();
            truncate_chars(&raw, 36)
        }
    };
    if brief.is_empty() {
        format!("{tool_name}…")
    } else {
        format!("{tool_name} {brief}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_line_includes_name_activity_and_elapsed() {
        let mut store = SubagentStore::default();
        store.upsert_spawned(
            "abcdef12-9999".into(),
            "explore".into(),
            "scan workspace".into(),
            "running",
        );
        store.note_child_event("abcdef12-9999", "Read src/main.rs".into());
        let line = store.entries[0].strip_line(Instant::now());
        assert!(line.contains("Agent explore(abcdef12)"));
        assert!(line.contains("Read src/main.rs"));
        assert!(line.ends_with("s"));
    }

    #[test]
    fn finished_agents_leave_strip_after_grace() {
        let mut entry = SubagentUiEntry {
            id: "a".into(),
            name: "x".into(),
            description: String::new(),
            status: "complete".into(),
            activity: "done".into(),
            started_at: Instant::now(),
            finished_at: Some(Instant::now() - std::time::Duration::from_secs(20)),
            events: VecDeque::new(),
            result_or_error: None,
        };
        assert!(!entry.show_on_strip(Instant::now()));
        entry.finished_at = Some(Instant::now());
        assert!(entry.show_on_strip(Instant::now()));
    }
}
