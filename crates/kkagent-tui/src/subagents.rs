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
/// A running/pending entry that has seen no events for this long is treated
/// as lost (e.g. the SubagentCompleted event was dropped across a
/// disconnect): it stops counting as active and leaves the strip. Long but
/// legitimate delegations stay visible because any child event refreshes
/// `last_event_at`.
const STALE_ACTIVE_SECS: u64 = 30 * 60;

#[derive(Debug, Clone)]
pub struct SubagentUiEntry {
    pub id: String,
    pub name: String,
    pub description: String,
    pub status: String,
    /// Model the subagent runs with — resolved alias for internal/built-in
    /// runs, or the external agent name for ACP delegations. Shown in the
    /// status bar while the subagent view is active.
    pub model: Option<String>,
    /// Latest short activity (tool name / summary).
    pub activity: String,
    pub started_at: Instant,
    pub finished_at: Option<Instant>,
    /// Last time a child event / status change touched this entry; used by
    /// the stale-active fallback in [`Self::is_active`].
    last_event_at: Instant,
    pub events: VecDeque<String>,
    pub result_or_error: Option<String>,
    /// Full conversation transcript rendered like a normal session view.
    pub transcript: Vec<crate::app::DisplayMessage>,
    /// Thinking text accumulated since the last assistant message flush.
    pending_thinking: String,
    /// Index of the streaming assistant message inside `transcript`.
    active_assistant: Option<usize>,
}

impl SubagentUiEntry {
    pub fn is_active(&self) -> bool {
        if !matches!(self.status.as_str(), "pending" | "running") {
            return false;
        }
        // Stale-active fallback: a run that has gone silent far longer than
        // any legitimate delegation is almost certainly lost (its terminal
        // event was dropped, e.g. across a disconnect). Stop advertising it
        // as active so the strip timer halts and the entry can age out.
        self.last_event_at.elapsed().as_secs() < STALE_ACTIVE_SECS
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
        self.last_event_at = Instant::now();
        self.events.push_back(line);
        while self.events.len() > MAX_EVENT_LOG {
            self.events.pop_front();
        }
    }

    /// Fold a mirrored child-agent event into the full transcript so the
    /// subagent view renders exactly like a normal session.
    pub fn apply_child_event(&mut self, event: &kkagent_protocol::AgentEvent) {
        use crate::app::{DisplayMessage, DisplayToolCall, MessageRole};
        use kkagent_protocol::AgentEvent;

        self.last_event_at = Instant::now();
        match event {
            AgentEvent::ThinkingDelta { text, .. } => {
                self.pending_thinking.push_str(text);
            }
            AgentEvent::MessageDelta { text, .. } => {
                let pending_thinking = if self.pending_thinking.is_empty() {
                    None
                } else {
                    Some(std::mem::take(&mut self.pending_thinking))
                };
                if let Some(message) = self
                    .active_assistant
                    .and_then(|index| self.transcript.get_mut(index))
                    .filter(|message| message.role == MessageRole::Assistant)
                {
                    if message.thinking.is_none() {
                        message.thinking = pending_thinking;
                    }
                    message.append_assistant_text(text);
                    return;
                }
                let mut msg = DisplayMessage {
                    role: MessageRole::Assistant,
                    content: String::new(),
                    thinking: pending_thinking,
                    parts: Vec::new(),
                    tool_calls: Vec::new(),
                    delivery: crate::prompt_queue::DeliveryState::Sent,
                    idempotency_key: None,
                };
                msg.append_assistant_text(text);
                self.transcript.push(msg);
                self.active_assistant = Some(self.transcript.len() - 1);
            }
            AgentEvent::ToolCall {
                tool_call_id,
                tool_name,
                input,
                ..
            } => {
                let pending_thinking = if self.pending_thinking.is_empty() {
                    None
                } else {
                    Some(std::mem::take(&mut self.pending_thinking))
                };
                let tc = DisplayToolCall {
                    id: tool_call_id.clone(),
                    started_at: Some(Instant::now()),
                    stopping: false,
                    queued_behind: None,
                    name: tool_name.clone(),
                    input_summary: crate::app::summarize_tool_input(input),
                    output: None,
                    is_error: false,
                    collapsed: true,
                    user_overridden: false,
                };
                if let Some(message) = self
                    .active_assistant
                    .and_then(|index| self.transcript.get_mut(index))
                    .filter(|message| message.role == MessageRole::Assistant)
                {
                    if message.thinking.is_none() {
                        message.thinking = pending_thinking;
                    }
                    message.push_tool(tc);
                    return;
                }
                let mut msg = DisplayMessage {
                    role: MessageRole::Assistant,
                    content: String::new(),
                    thinking: pending_thinking,
                    parts: Vec::new(),
                    tool_calls: Vec::new(),
                    delivery: crate::prompt_queue::DeliveryState::Sent,
                    idempotency_key: None,
                };
                msg.push_tool(tc);
                self.transcript.push(msg);
                self.active_assistant = Some(self.transcript.len() - 1);
            }
            AgentEvent::ToolResult {
                tool_call_id,
                tool_name,
                output,
                is_error,
                ..
            } => {
                if let Some(tc) =
                    self.transcript.iter_mut().rev().find_map(|message| {
                        message.find_tool_for_result_mut(tool_call_id, tool_name)
                    })
                {
                    tc.output = Some(output.clone());
                    tc.is_error = *is_error;
                    if *is_error {
                        tc.collapsed = false;
                    }
                }
            }
            AgentEvent::TurnEnd { .. } => {
                // Turn boundary: the next assistant output starts a new bubble.
                self.active_assistant = None;
            }
            _ => {}
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
        model: Option<String>,
    ) {
        if let Some(existing) = self.entries.iter_mut().find(|e| e.id == id) {
            existing.name = name;
            if !description.is_empty() {
                existing.description = description.clone();
            }
            existing.status = status.into();
            if model.is_some() {
                existing.model = model.clone();
            }
            // Resume snapshots can restore a terminal status directly; mark
            // the finish time so the strip stops counting elapsed time and
            // the entry leaves after the grace window.
            if matches!(
                existing.status.as_str(),
                "complete" | "failed" | "cancelled"
            ) && existing.finished_at.is_none()
            {
                existing.finished_at = Some(Instant::now());
            }
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
        // Resume snapshots can restore a terminal status directly; set the
        // finish time so elapsed stops and the entry leaves the strip.
        let status = status.into();
        let finished_at =
            matches!(status.as_str(), "complete" | "failed" | "cancelled").then(Instant::now);
        self.entries.push(SubagentUiEntry {
            id,
            name,
            description,
            status,
            model,
            activity,
            started_at: Instant::now(),
            finished_at,
            last_event_at: Instant::now(),
            events: VecDeque::new(),
            result_or_error: None,
            transcript: Vec::new(),
            pending_thinking: String::new(),
            active_assistant: None,
        });
    }

    pub fn set_status(&mut self, id: &str, status: &str, detail: Option<String>) {
        let Some(entry) = self.entries.iter_mut().find(|e| e.id == id) else {
            return;
        };
        entry.status = status.to_string();
        entry.last_event_at = Instant::now();
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

    /// Fold a mirrored child event into the agent's full transcript.
    pub fn apply_child_event(&mut self, id: &str, event: &kkagent_protocol::AgentEvent) {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.id == id) {
            if entry.status == "pending" {
                entry.status = "running".into();
            }
            entry.apply_child_event(event);
        }
    }

    /// Seed the session-style transcript with the delegation prompt so the
    /// subagent view opens on the "user" message that started the run.
    pub fn seed_prompt(&mut self, id: &str, prompt: &str) {
        use crate::app::{DisplayMessage, MessageRole};
        let Some(entry) = self.entries.iter_mut().find(|e| e.id == id) else {
            return;
        };
        if entry.transcript.is_empty() && !prompt.trim().is_empty() {
            entry.transcript.push(DisplayMessage {
                role: MessageRole::User,
                content: prompt.to_string(),
                thinking: None,
                parts: Vec::new(),
                tool_calls: Vec::new(),
                delivery: crate::prompt_queue::DeliveryState::Sent,
                idempotency_key: None,
            });
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
            Some("test/explore".into()),
        );
        store.note_child_event("abcdef12-9999", "Read src/main.rs".into());
        let line = store.entries[0].strip_line(Instant::now());
        assert!(line.contains("Agent explore(abcdef12)"));
        assert!(line.contains("Read src/main.rs"));
        assert!(line.ends_with("s"));
    }

    #[test]
    fn upsert_spawned_stores_and_updates_model() {
        let mut store = SubagentStore::default();
        // First spawn records the model.
        store.upsert_spawned(
            "ag-m1".into(),
            "kk-wiki-agent.search".into(),
            "wiki lookup".into(),
            "pending",
            Some("test/fast".into()),
        );
        assert_eq!(store.entries[0].model.as_deref(), Some("test/fast"));
        // A later re-spawn without a model keeps the previous value.
        store.upsert_spawned(
            "ag-m1".into(),
            "kk-wiki-agent.search".into(),
            "wiki lookup".into(),
            "running",
            None,
        );
        assert_eq!(store.entries[0].model.as_deref(), Some("test/fast"));
        // ACP entries carry the agent name as their display "model".
        store.upsert_spawned(
            "ag-m2".into(),
            "cursor".into(),
            "delegate to cursor".into(),
            "pending",
            Some("cursor".into()),
        );
        assert_eq!(store.entries[1].model.as_deref(), Some("cursor"));
    }

    fn child_event(event: kkagent_protocol::AgentEvent) -> Box<kkagent_protocol::AgentEvent> {
        Box::new(event)
    }

    #[test]
    fn transcript_folds_child_events_like_a_session() {
        use kkagent_protocol::AgentEvent;
        let mut store = SubagentStore::default();
        store.upsert_spawned(
            "ag-1".into(),
            "explore".into(),
            "scan".into(),
            "pending",
            None,
        );
        store.seed_prompt("ag-1", "find all callers of foo()");

        let sid = "parent".to_string();
        store.apply_child_event(
            "ag-1",
            &AgentEvent::ThinkingDelta {
                session_id: sid.clone(),
                text: "pondering".into(),
            },
        );
        store.apply_child_event(
            "ag-1",
            &AgentEvent::MessageDelta {
                session_id: sid.clone(),
                text: "Scanning files…".into(),
            },
        );
        store.apply_child_event(
            "ag-1",
            &AgentEvent::ToolCall {
                session_id: sid.clone(),
                tool_call_id: "tc-1".into(),
                tool_name: "Grep".into(),
                input: serde_json::json!({"pattern": "foo\\("}),
            },
        );
        store.apply_child_event(
            "ag-1",
            &AgentEvent::ToolResult {
                session_id: sid.clone(),
                tool_call_id: "tc-1".into(),
                tool_name: "Grep".into(),
                output: "3 matches".into(),
                is_error: false,
            },
        );
        store.apply_child_event(
            "ag-1",
            &AgentEvent::MessageDelta {
                session_id: sid.clone(),
                text: " found 3 call sites".into(),
            },
        );

        let entry = &store.entries[0];
        assert_eq!(
            entry.transcript.len(),
            2,
            "user prompt + one assistant bubble"
        );
        let user = &entry.transcript[0];
        assert!(user.content.contains("callers of foo()"));
        let assistant = &entry.transcript[1];
        assert!(assistant.content.contains("Scanning files…"));
        assert!(assistant.content.contains("found 3 call sites"));
        assert_eq!(assistant.thinking.as_deref(), Some("pondering"));
        assert_eq!(assistant.parts.len(), 3, "text + tool + trailing text");
        assert!(
            matches!(&assistant.parts[0], crate::app::DisplayPart::Text(t) if t.contains("Scanning files…"))
        );
        assert!(matches!(
            &assistant.parts[1],
            crate::app::DisplayPart::Tool(_)
        ));
        assert!(
            matches!(&assistant.parts[2], crate::app::DisplayPart::Text(t) if t.contains("found 3 call sites"))
        );
        let _ = child_event(AgentEvent::TurnEnd {
            session_id: sid.clone(),
        });
    }

    #[test]
    fn tool_result_marks_error_and_expands() {
        use kkagent_protocol::AgentEvent;
        let mut store = SubagentStore::default();
        store.upsert_spawned("ag-2".into(), "coder".into(), "fix".into(), "running", None);
        store.apply_child_event(
            "ag-2",
            &AgentEvent::ToolCall {
                session_id: "p".into(),
                tool_call_id: "tc-9".into(),
                tool_name: "Bash".into(),
                input: serde_json::json!({"command": "make"}),
            },
        );
        store.apply_child_event(
            "ag-2",
            &AgentEvent::ToolResult {
                session_id: "p".into(),
                tool_call_id: "tc-9".into(),
                tool_name: "Bash".into(),
                output: "error E0432".into(),
                is_error: true,
            },
        );
        let entry = &store.entries[0];
        let assistant = &entry.transcript[0];
        let crate::app::DisplayPart::Tool(tool) = &assistant.parts[0] else {
            panic!("expected tool part");
        };
        assert!(tool.is_error);
        assert_eq!(tool.output.as_deref(), Some("error E0432"));
        assert!(!tool.collapsed, "failed tools auto-expand");
    }

    #[test]
    fn finished_agents_leave_strip_after_grace() {
        let mut entry = SubagentUiEntry {
            id: "a".into(),
            name: "x".into(),
            description: String::new(),
            status: "complete".into(),
            model: None,
            activity: "done".into(),
            started_at: Instant::now(),
            finished_at: Some(Instant::now() - std::time::Duration::from_secs(20)),
            last_event_at: Instant::now(),
            events: VecDeque::new(),
            result_or_error: None,
            transcript: Vec::new(),
            pending_thinking: String::new(),
            active_assistant: None,
        };
        assert!(!entry.show_on_strip(Instant::now()));
        entry.finished_at = Some(Instant::now());
        assert!(entry.show_on_strip(Instant::now()));
    }

    #[test]
    fn resume_with_terminal_status_stops_the_timer() {
        // Regression: a resume snapshot carrying status "complete" restored
        // the entry without a finish time, leaving the strip timer counting
        // forever even though nothing was running.
        let mut store = SubagentStore::default();
        store.upsert_spawned(
            "abcdef12-7777".into(),
            "delegate".into(),
            "long task".into(),
            "complete",
            None,
        );
        assert_eq!(store.entries[0].status, "complete");
        assert!(
            store.entries[0].finished_at.is_some(),
            "terminal status must set finished_at"
        );
        assert!(!store.entries[0].is_active());
        // A fresh entry (no events) still shows on the strip inside the
        // grace window, then ages out.
        assert!(store.entries[0].show_on_strip(Instant::now()));
    }

    #[test]
    fn silent_running_entry_is_not_active_after_stale_window() {
        let mut entry = SubagentUiEntry {
            id: "b".into(),
            name: "delegate".into(),
            description: "lost run".into(),
            status: "running".into(),
            model: None,
            activity: "working…".into(),
            started_at: Instant::now() - std::time::Duration::from_secs(STALE_ACTIVE_SECS + 60),
            finished_at: None,
            last_event_at: Instant::now() - std::time::Duration::from_secs(STALE_ACTIVE_SECS + 60),
            events: VecDeque::new(),
            result_or_error: None,
            transcript: Vec::new(),
            pending_thinking: String::new(),
            active_assistant: None,
        };
        assert!(
            !entry.is_active(),
            "silence beyond the stale window must deactivate"
        );
        assert!(!entry.show_on_strip(Instant::now()));
        // A fresh event (or terminal status) makes it visible again.
        entry.push_event("still alive");
        assert!(entry.is_active());
    }
}
