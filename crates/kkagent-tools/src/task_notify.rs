//! Push-based background task notifications (the "task contract").
//!
//! Background tasks (`Agent` subagents, background Bash shells) must not be
//! discovered by polling: completions are pushed into the conversation as
//! `<task-notification>` blocks at the next model step, the same pattern
//! Claude Code uses (`enqueueTaskNotification` → message queue → next step).
//!
//! One process-global hub is keyed by session id, so it works as a completion
//! router for the `SubagentManager` sink (which only knows the parent session
//! id) without threading a per-session handle through every tool constructor:
//!
//! - tools call [`TaskNotificationHub::track`] when they launch a task;
//! - completion sinks call [`TaskNotificationHub::push_notification`]
//!   (or the typed helpers) when a task reaches a terminal state;
//! - a queued notification fires the host-registered wake hook
//!   ([`TaskNotificationHub::set_wake_hook`]) so an idle session is woken
//!   with a short turn that drains it — without the hook the notification
//!   would sit in the hub until the next unrelated user input;
//! - the agent loop drains a session's notifications before every model step;
//! - a synchronous result delivery (`Agent` sync wait, `TaskOutput` fetch)
//!   calls [`TaskNotificationHub::consume`] so the notification is not
//!   duplicated later.
//!
//! Notifications for a session that finishes while idle are kept until the
//! session's next turn starts (Claude Code's `later` priority) — they are
//! never dropped.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

/// How much of a finished task's output rides along in the notification.
const SUMMARY_EXCERPT_CHARS: usize = 800;

/// Which tool family a tracked background task belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    Agent,
    Bash,
}

impl TaskKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Bash => "bash",
        }
    }
}

/// One delivered background-task completion.
#[derive(Debug, Clone)]
pub struct TaskNotification {
    pub task_id: String,
    pub kind: TaskKind,
    /// `completed` | `failed` | `timed_out`
    pub status: String,
    pub description: String,
    /// Trailing excerpt of the task output, when available.
    pub summary: Option<String>,
}

struct TrackedTask {
    session_id: String,
    kind: TaskKind,
    description: String,
    /// Set when the task reached a terminal state; cleared when the
    /// notification is drained or consumed.
    notification: Option<TaskNotification>,
}

/// Host-registered callback fired whenever a notification is queued, so an
/// idle session can be woken to deliver it. The hub itself stays
/// runtime-agnostic and cannot depend on kkagent-core; the host (kkagent's
/// server state) installs the implementation. Receives the target session id.
pub type WakeHook = Arc<dyn Fn(&str) + Send + Sync>;

#[derive(Default)]
struct HubInner {
    tasks: HashMap<String, TrackedTask>,
    /// See [`TaskNotificationHub::set_wake_hook`].
    wake_hook: Option<WakeHook>,
}

/// Session-keyed registry of background tasks and their pending completion
/// notifications. Cheap to clone; use [`global_hub`] for the process-wide
/// instance.
#[derive(Clone, Default)]
pub struct TaskNotificationHub {
    inner: Arc<Mutex<HubInner>>,
}

impl TaskNotificationHub {
    /// Record a freshly launched background task so the turn-end reminder can
    /// report it while it runs.
    pub fn track(&self, session_id: &str, task_id: &str, kind: TaskKind, description: &str) {
        let mut inner = match self.inner.lock() {
            Ok(inner) => inner,
            Err(poisoned) => poisoned.into_inner(),
        };
        inner.tasks.insert(
            task_id.to_string(),
            TrackedTask {
                session_id: session_id.to_string(),
                kind,
                description: description.to_string(),
                notification: None,
            },
        );
    }

    /// Forget a task entirely (explicit stop / external cleanup) without
    /// delivering a notification.
    pub fn untrack(&self, task_id: &str) {
        let mut inner = match self.inner.lock() {
            Ok(inner) => inner,
            Err(poisoned) => poisoned.into_inner(),
        };
        inner.tasks.remove(task_id);
    }

    /// Queue a completion notification. Delivers to the entry recorded by
    /// [`TaskNotificationHub::track`] when present, otherwise creates one
    /// (robust for tasks launched before tracking existed).
    ///
    /// Fires the host-registered wake hook (outside the hub lock) so an idle
    /// session is woken to drain the notification instead of waiting for the
    /// next user input.
    pub fn push_notification(&self, session_id: &str, notification: TaskNotification) {
        let wake = {
            let mut inner = match self.inner.lock() {
                Ok(inner) => inner,
                Err(poisoned) => poisoned.into_inner(),
            };
            let entry = inner
                .tasks
                .entry(notification.task_id.clone())
                .or_insert_with(|| TrackedTask {
                    session_id: session_id.to_string(),
                    kind: notification.kind,
                    description: notification.description.clone(),
                    notification: None,
                });
            entry.session_id = session_id.to_string();
            entry.notification = Some(notification);
            inner.wake_hook.clone()
        };
        // Fired after the lock is dropped: the hook spawns an async wake task
        // and returns immediately; it only re-enters the hub from that task.
        if let Some(wake) = wake {
            wake(session_id);
        }
    }

    /// Register the host-provided wake callback, fired from
    /// [`TaskNotificationHub::push_notification`] whenever a notification is
    /// queued. Replaces any previously registered hook; passing `None` only
    /// makes sense in tests (a server without the hook would never wake idle
    /// sessions for background results).
    pub fn set_wake_hook(&self, hook: Option<WakeHook>) {
        let mut inner = match self.inner.lock() {
            Ok(inner) => inner,
            Err(poisoned) => poisoned.into_inner(),
        };
        inner.wake_hook = hook;
    }

    /// Whether `session_id` has at least one undelivered completion
    /// notification. The wake path re-checks this after acquiring the turn
    /// permit so a notification that was already consumed synchronously
    /// (`TaskOutput` fetch / sync `Agent` wait) does not spawn a no-op turn.
    pub fn has_pending(&self, session_id: &str) -> bool {
        let inner = match self.inner.lock() {
            Ok(inner) => inner,
            Err(poisoned) => poisoned.into_inner(),
        };
        inner
            .tasks
            .values()
            .any(|task| task.session_id == session_id && task.notification.is_some())
    }

    /// Typed entry point for the `SubagentManager` completion sink.
    pub fn on_subagent_completion(
        &self,
        parent_session_id: Option<&str>,
        agent_id: &str,
        description: &str,
        status: kkagent_protocol::subagent::SubagentStatus,
        summary: Option<String>,
    ) {
        use kkagent_protocol::subagent::SubagentStatus;
        let Some(session_id) = parent_session_id else {
            // Nowhere to deliver (e.g. nested manager without a parent
            // session): at least stop tracking so the reminder stays honest.
            self.untrack(agent_id);
            return;
        };
        match status {
            SubagentStatus::Complete => {
                self.push_notification(
                    session_id,
                    TaskNotification {
                        task_id: agent_id.to_string(),
                        kind: TaskKind::Agent,
                        status: "completed".into(),
                        description: description.to_string(),
                        summary: summary.map(|s| excerpt(&s, SUMMARY_EXCERPT_CHARS)),
                    },
                );
            }
            SubagentStatus::Failed => {
                self.push_notification(
                    session_id,
                    TaskNotification {
                        task_id: agent_id.to_string(),
                        kind: TaskKind::Agent,
                        status: "failed".into(),
                        description: description.to_string(),
                        summary: summary.map(|s| excerpt(&s, SUMMARY_EXCERPT_CHARS)),
                    },
                );
            }
            SubagentStatus::Cancelled => {
                // The model (or user) stopped it deliberately — no news.
                self.untrack(agent_id);
            }
            SubagentStatus::Pending | SubagentStatus::Running => {}
        }
    }

    /// Typed entry point for `BackgroundShellManager::finish`.
    pub(crate) fn on_bash_finished(
        &self,
        session_id: &str,
        task_id: &str,
        description: &str,
        status: crate::builtin::bash::ShellStatus,
        exit_code: Option<i32>,
        output: &str,
    ) {
        use crate::builtin::bash::ShellStatus;
        match status {
            ShellStatus::Cancelled => {
                self.untrack(task_id);
                return;
            }
            ShellStatus::Running => return,
            ShellStatus::Complete | ShellStatus::Failed | ShellStatus::TimedOut => {}
        }
        let status_text = match status {
            ShellStatus::TimedOut => "timed_out",
            ShellStatus::Failed => "failed",
            _ => "completed",
        };
        let mut summary = excerpt(output, SUMMARY_EXCERPT_CHARS);
        if summary.is_empty() {
            summary = "(no output)".to_string();
        }
        if let Some(code) = exit_code {
            summary.push_str(&format!("\nexit code: {code}"));
        }
        self.push_notification(
            session_id,
            TaskNotification {
                task_id: task_id.to_string(),
                kind: TaskKind::Bash,
                status: status_text.into(),
                description: description.to_string(),
                summary: Some(summary),
            },
        );
    }

    /// Take (and remove) every pending notification for `session_id`.
    pub fn drain_notifications(&self, session_id: &str) -> Vec<TaskNotification> {
        let mut inner = match self.inner.lock() {
            Ok(inner) => inner,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut drained = Vec::new();
        let done: Vec<String> = inner
            .tasks
            .iter()
            .filter(|(_, task)| task.session_id == session_id && task.notification.is_some())
            .map(|(id, _)| id.clone())
            .collect();
        for id in done {
            if let Some(task) = inner.tasks.remove(&id) {
                if let Some(notification) = task.notification {
                    drained.push(notification);
                }
            }
        }
        drained
    }

    /// Remove a task's pending notification because its result was already
    /// delivered through a synchronous path (sync `Agent` wait or a
    /// `TaskOutput` fetch). Returns the removed notification, if any. A
    /// terminal entry whose notification was consumed is dropped entirely so
    /// the running list stays accurate.
    pub fn consume(&self, task_id: &str) -> Option<TaskNotification> {
        let mut inner = match self.inner.lock() {
            Ok(inner) => inner,
            Err(poisoned) => poisoned.into_inner(),
        };
        let notification = inner.tasks.get_mut(task_id)?.notification.take()?;
        inner.tasks.remove(task_id);
        Some(notification)
    }

    /// Tasks launched for `session_id` that have not reached a terminal state.
    /// Used by the turn-end reminder.
    pub fn running_for(&self, session_id: &str) -> Vec<(String, TaskKind, String)> {
        let inner = match self.inner.lock() {
            Ok(inner) => inner,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut running: Vec<(String, TaskKind, String)> = inner
            .tasks
            .iter()
            .filter(|(_, task)| task.session_id == session_id && task.notification.is_none())
            .map(|(id, task)| (id.clone(), task.kind, task.description.clone()))
            .collect();
        running.sort_by(|a, b| a.0.cmp(&b.0));
        running
    }
}

/// Process-global hub instance.
pub fn global_hub() -> &'static TaskNotificationHub {
    static HUB: OnceLock<TaskNotificationHub> = OnceLock::new();
    HUB.get_or_init(TaskNotificationHub::default)
}

/// Trailing excerpt of `text`, at most `max_chars` characters.
fn excerpt(text: &str, max_chars: usize) -> String {
    let text = text.trim();
    let total = text.chars().count();
    if total <= max_chars {
        return text.to_string();
    }
    let start = total - max_chars;
    let tail: String = text.chars().skip(start).collect();
    // Align to a char boundary that starts a line when possible.
    let tail = match tail.find('\n') {
        Some(idx) if idx + 1 < tail.len() => tail[idx + 1..].to_string(),
        _ => tail,
    };
    format!("…{tail}")
}

/// Render drained notifications as one harness-injected user message.
pub fn format_notifications(notifications: &[TaskNotification]) -> String {
    let mut body =
        String::from("Background task results (delivered automatically — no polling needed):");
    for note in notifications {
        body.push_str("\n<task-notification>\n");
        body.push_str(&format!("<task-id>{}</task-id>\n", note.task_id));
        body.push_str(&format!("<task-type>{}</task-type>\n", note.kind.as_str()));
        body.push_str(&format!("<status>{}</status>\n", note.status));
        body.push_str(&format!(
            "<description>{}</description>\n",
            note.description
        ));
        if let Some(summary) = &note.summary {
            body.push_str(&format!("<summary>\n{summary}\n</summary>\n"));
        }
        body.push_str("</task-notification>");
    }
    wrap_reminder(&body)
}

/// Same wrapper as kkagent-core's `system_reminder::wrap`, duplicated here
/// because kkagent-tools must not depend on kkagent-core.
pub fn wrap_reminder(body: &str) -> String {
    format!("<system-reminder>\n{body}\n</system-reminder>")
}

/// Body of the turn-end reminder listing still-running tasks (caller wraps it
/// in `<system-reminder>`).
pub fn running_tasks_body(running: &[(String, TaskKind, String)]) -> String {
    let mut body = String::from(
        "Background task(s) still running for this session. Their results will be \
         delivered automatically as <task-notification> when each finishes — do not \
         poll or schedule checks for them; end the turn normally if nothing else \
         is pending:",
    );
    for (id, kind, description) in running {
        body.push_str(&format!("\n- {id} [{}] {description}", kind.as_str()));
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(id: &str, status: &str) -> TaskNotification {
        TaskNotification {
            task_id: id.to_string(),
            kind: TaskKind::Bash,
            status: status.into(),
            description: format!("desc {id}"),
            summary: Some("out".into()),
        }
    }

    #[test]
    fn notifications_are_session_scoped() {
        let hub = TaskNotificationHub::default();
        hub.track("s1", "t1", TaskKind::Bash, "build");
        hub.push_notification("s1", note("t1", "completed"));
        assert!(hub.drain_notifications("s2").is_empty());
        let drained = hub.drain_notifications("s1");
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].task_id, "t1");
        // Drained once — not redelivered.
        assert!(hub.drain_notifications("s1").is_empty());
        assert!(hub.running_for("s1").is_empty());
    }

    #[test]
    fn consume_removes_pending_notification_once() {
        let hub = TaskNotificationHub::default();
        hub.push_notification("s1", note("t9", "failed"));
        assert!(hub.consume("t9").is_some());
        assert!(hub.consume("t9").is_none());
        assert!(hub.drain_notifications("s1").is_empty());
    }

    #[test]
    fn running_list_excludes_completed_and_other_sessions() {
        let hub = TaskNotificationHub::default();
        hub.track("s1", "r1", TaskKind::Agent, "running agent");
        hub.track("s1", "r2", TaskKind::Bash, "done bash");
        hub.track("s2", "r3", TaskKind::Bash, "other session");
        hub.push_notification("s1", note("r2", "completed"));
        let running = hub.running_for("s1");
        assert_eq!(
            running,
            vec![(
                "r1".to_string(),
                TaskKind::Agent,
                "running agent".to_string()
            )]
        );
    }

    #[test]
    fn untracked_completion_still_delivers_with_fallback_entry() {
        let hub = TaskNotificationHub::default();
        hub.push_notification("s1", note("late", "completed"));
        assert_eq!(hub.drain_notifications("s1").len(), 1);
    }

    #[test]
    fn wake_hook_fires_once_per_push_with_session_id() {
        let hub = TaskNotificationHub::default();
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_for_hook = seen.clone();
        hub.set_wake_hook(Some(Arc::new(move |session_id: &str| {
            seen_for_hook.lock().unwrap().push(session_id.to_string());
        })));
        hub.track("s1", "t1", TaskKind::Bash, "build");
        hub.push_notification("s1", note("t1", "completed"));
        hub.push_notification("s1", note("t2", "failed"));
        assert_eq!(
            *seen.lock().unwrap(),
            vec!["s1".to_string(), "s1".to_string()]
        );
        // Draining or consuming must not fire the hook again.
        assert!(!hub.drain_notifications("s1").is_empty());
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    #[test]
    fn wake_hook_sessions_follow_the_push() {
        let hub = TaskNotificationHub::default();
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_for_hook = seen.clone();
        hub.set_wake_hook(Some(Arc::new(move |session_id: &str| {
            seen_for_hook.lock().unwrap().push(session_id.to_string());
        })));
        hub.push_notification("s2", note("t3", "completed"));
        assert_eq!(*seen.lock().unwrap(), vec!["s2".to_string()]);
        assert!(hub.has_pending("s2"));
        assert!(!hub.has_pending("s1"));
        hub.drain_notifications("s2");
        assert!(!hub.has_pending("s2"));
    }

    #[test]
    fn wake_hook_can_be_replaced_or_cleared() {
        let hub = TaskNotificationHub::default();
        hub.set_wake_hook(Some(Arc::new(|_| panic!("cleared hook fired"))));
        hub.set_wake_hook(None);
        hub.push_notification("s1", note("t4", "completed")); // must not panic
        assert_eq!(hub.drain_notifications("s1").len(), 1);
    }

    #[test]
    fn bash_outcome_maps_status_and_appends_exit_code() {
        let hub = TaskNotificationHub::default();
        hub.on_bash_finished(
            "s1",
            "b1",
            "npm test",
            crate::builtin::bash::ShellStatus::Complete,
            Some(0),
            "all good",
        );
        let drained = hub.drain_notifications("s1");
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].status, "completed");
        assert!(drained[0]
            .summary
            .as_deref()
            .unwrap_or("")
            .contains("exit code: 0"));

        hub.on_bash_finished(
            "s1",
            "b2",
            "sleep",
            crate::builtin::bash::ShellStatus::Cancelled,
            None,
            "",
        );
        assert!(hub.drain_notifications("s1").is_empty());
    }

    #[test]
    fn excerpt_keeps_the_tail() {
        let long = (0..2000).map(|i| format!("line{i}\n")).collect::<String>();
        let out = excerpt(&long, 100);
        assert!(out.chars().count() <= 120);
        assert!(out.starts_with('…'));
        assert!(out.contains("line1999"));
    }

    #[test]
    fn formatted_message_wraps_notifications_in_a_system_reminder() {
        let text = format_notifications(&[note("t1", "completed")]);
        assert!(text.starts_with("<system-reminder>"));
        assert!(text.contains("<task-notification>"));
        assert!(text.contains("<task-id>t1</task-id>"));
        assert!(text.ends_with("</system-reminder>"));
    }

    #[test]
    fn running_body_lists_tasks() {
        let body = running_tasks_body(&[("a1".into(), TaskKind::Agent, "explore repo".into())]);
        assert!(body.contains("do not poll"));
        assert!(body.contains("- a1 [agent] explore repo"));
    }
}
