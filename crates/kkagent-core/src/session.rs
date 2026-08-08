use tokio::sync::{mpsc, oneshot};
use kkagent_llm::{ChatMessage, ChatContent};
use kkagent_protocol::{ApprovalResponse, PermissionMode};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Pre-write snapshot so undo can restore files.
#[derive(Debug, Clone)]
pub struct FileChange {
    pub path: PathBuf,
    /// `None` means the file did not exist before the write.
    pub previous: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct TurnCheckpoint {
    /// Index of the user message that started this turn.
    pub message_start_index: usize,
    pub file_changes: Vec<FileChange>,
}

pub struct Session {
    pub id: String,
    pub title: Option<String>,
    pub messages: Vec<ChatMessage>,
    pub system_prompt: String,
    pub working_dir: PathBuf,
    pub permission_mode: PermissionMode,
    pub plan_mode: bool,
    /// Only this file may be written/edited while plan_mode is on.
    pub plan_file_path: PathBuf,
    /// Model alias from config (e.g. "local/claude-opus-4-8").
    pub model_alias: String,
    /// How many messages have already been written to the transcript DB.
    pub persisted_message_count: usize,
    pub approval_waiters: HashMap<String, oneshot::Sender<ApprovalResponse>>,
    approval_rx: mpsc::Receiver<ApprovalResponse>,
    pub approval_tx: mpsc::Sender<ApprovalResponse>,
    /// Set by session.interrupt — agent loop checks between stream/tool steps.
    pub interrupted: Arc<AtomicBool>,
    /// Index of the current turn's user message (set by `begin_turn`).
    turn_message_start: Option<usize>,
    /// File mutations during the in-flight turn.
    pub current_turn_changes: Vec<FileChange>,
    /// Completed turns available for undo (most recent last).
    pub undo_stack: Vec<TurnCheckpoint>,
}

impl Session {
    pub fn new(
        id: String,
        working_dir: PathBuf,
        permission_mode: PermissionMode,
        model_alias: String,
    ) -> Self {
        let (approval_tx, approval_rx) = mpsc::channel(16);
        let plan_file_path = working_dir.join(".kkagent").join("plan.md");
        Self {
            id,
            title: None,
            messages: Vec::new(),
            system_prompt: default_system_prompt(),
            working_dir,
            permission_mode,
            plan_mode: false,
            plan_file_path,
            model_alias,
            persisted_message_count: 0,
            approval_waiters: HashMap::new(),
            approval_rx,
            approval_tx,
            interrupted: Arc::new(AtomicBool::new(false)),
            turn_message_start: None,
            current_turn_changes: Vec::new(),
            undo_stack: Vec::new(),
        }
    }

    pub fn begin_turn(&mut self) {
        self.turn_message_start = Some(self.messages.len().saturating_sub(1));
        self.current_turn_changes.clear();
    }

    pub fn commit_turn(&mut self) {
        if let Some(start) = self.turn_message_start.take() {
            self.undo_stack.push(TurnCheckpoint {
                message_start_index: start,
                file_changes: std::mem::take(&mut self.current_turn_changes),
            });
        }
    }

    /// Snapshot file contents before Write/Edit (once per path per turn).
    pub async fn record_pre_change(&mut self, path: PathBuf) {
        if self
            .current_turn_changes
            .iter()
            .any(|c| c.path == path)
        {
            return;
        }
        let previous = tokio::fs::read(&path).await.ok();
        self.current_turn_changes.push(FileChange { path, previous });
    }

    /// Undo the last completed turn: restore files + truncate messages.
    /// Returns the new message count.
    pub fn undo_last_turn(&mut self) -> anyhow::Result<usize> {
        let cp = self
            .undo_stack
            .pop()
            .ok_or_else(|| anyhow::anyhow!("Nothing to undo"))?;

        for change in cp.file_changes.into_iter().rev() {
            match change.previous {
                Some(bytes) => {
                    if let Some(parent) = change.path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    std::fs::write(&change.path, bytes)?;
                }
                None => {
                    let _ = std::fs::remove_file(&change.path);
                }
            }
        }

        self.messages.truncate(cp.message_start_index);
        self.persisted_message_count = self.persisted_message_count.min(self.messages.len());
        Ok(self.messages.len())
    }

    pub fn clear_interrupt(&self) {
        self.interrupted.store(false, Ordering::SeqCst);
    }

    pub fn request_interrupt(&self) {
        self.interrupted.store(true, Ordering::SeqCst);
        // Unblock any in-flight approval wait
        let _ = self.approval_tx.try_send(ApprovalResponse {
            approval_id: String::new(),
            decision: kkagent_protocol::ApprovalDecision::Cancelled,
            scope: None,
            feedback: Some("interrupted".into()),
        });
    }

    pub fn is_interrupted(&self) -> bool {
        self.interrupted.load(Ordering::SeqCst)
    }

    pub fn add_user_message(&mut self, text: String) {
        self.messages.push(ChatMessage {
            role: "user".into(),
            content: vec![ChatContent::Text { text }],
        });
    }

    pub fn build_messages(&self) -> Vec<ChatMessage> {
        self.messages.clone()
    }

    pub fn effective_system_prompt(&self) -> String {
        let mut prompt = self.system_prompt.clone();
        if self.plan_mode {
            prompt.push_str("\n\n");
            prompt.push_str(&plan_mode_reminder(&self.plan_file_path));
        }
        prompt
    }

    pub async fn wait_approval(&mut self, approval_id: &str) -> ApprovalResponse {
        loop {
            if self.is_interrupted() {
                return ApprovalResponse {
                    approval_id: approval_id.to_string(),
                    decision: kkagent_protocol::ApprovalDecision::Cancelled,
                    scope: None,
                    feedback: Some("interrupted".into()),
                };
            }
            match tokio::time::timeout(
                std::time::Duration::from_millis(200),
                self.approval_rx.recv(),
            )
            .await
            {
                Ok(Some(resp)) => {
                    if resp.approval_id == approval_id
                        || resp.approval_id.is_empty()
                        || matches!(
                            resp.decision,
                            kkagent_protocol::ApprovalDecision::Cancelled
                        )
                    {
                        return resp;
                    }
                }
                Ok(None) => {
                    return ApprovalResponse {
                        approval_id: approval_id.to_string(),
                        decision: kkagent_protocol::ApprovalDecision::Rejected,
                        scope: None,
                        feedback: Some("approval channel closed".into()),
                    };
                }
                Err(_) => continue, // timeout — re-check interrupt
            }
        }
    }

    pub fn submit_approval(&self, response: ApprovalResponse) {
        let _ = self.approval_tx.try_send(response);
    }
}

fn default_system_prompt() -> String {
    r#"You are an AI coding assistant that helps users with software engineering tasks.
You can read and edit code, run shell commands, search files, and choose the next step based on feedback.

Guidelines:
- Be concise and focused on the task
- Use tools to accomplish work rather than just describing what to do
- Always verify your work when possible
- Ask for clarification when requirements are ambiguous
"#
    .to_string()
}

/// Mirrors kimi-code plan-mode-full-reminder.md (simplified).
pub fn plan_mode_reminder(plan_file: &std::path::Path) -> String {
    format!(
        r#"Plan mode is active. You MUST NOT make any edits (with the exception of the current plan file) or otherwise make changes to the system. Prefer read-only tools (Read, Grep, Glob). Use Bash only when needed for read-only exploration. This supersedes any other instructions you have received.

Current plan file (ONLY file you may Write/Edit): {plan}

Workflow:
  1. Understand — explore the codebase with Glob, Grep, Read.
  2. Design — converge on the best approach; consider trade-offs but aim for a single recommendation.
  3. Review — re-read key files to verify understanding.
  4. Write Plan — create/update the plan file with Write or Edit (full markdown plan).
  5. Exit — call ExitPlanMode when the plan is ready for user approval.

Do NOT edit source code, configs, or any file other than the plan file.
Do NOT start implementing. Your turn should end after writing a complete plan and calling ExitPlanMode.
Ask clarifying questions in text if requirements are ambiguous — then continue planning.
"#,
        plan = plan_file.display()
    )
}
