use tokio::sync::{mpsc, oneshot};
use kkagent_llm::{ChatMessage, ChatContent};
use kkagent_protocol::{ApprovalResponse, PermissionMode, QuestionResponse};
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
    /// Shared Arc so `/model` can update mid-turn while the session is out of the map.
    pub model_alias: Arc<std::sync::Mutex<String>>,
    /// How many messages have already been written to the transcript DB.
    pub persisted_message_count: usize,
    pub approval_waiters: HashMap<String, oneshot::Sender<ApprovalResponse>>,
    approval_rx: mpsc::Receiver<ApprovalResponse>,
    pub approval_tx: mpsc::Sender<ApprovalResponse>,
    question_rx: mpsc::Receiver<QuestionResponse>,
    pub question_tx: mpsc::Sender<QuestionResponse>,
    /// Set by session.interrupt — agent loop checks between stream/tool steps.
    pub interrupted: Arc<AtomicBool>,
    /// Index of the current turn's user message (set by `begin_turn`).
    turn_message_start: Option<usize>,
    /// File mutations during the in-flight turn.
    pub current_turn_changes: Vec<FileChange>,
    /// Completed turns available for undo (most recent last).
    pub undo_stack: Vec<TurnCheckpoint>,
    /// Progressive tool disclosure filter (None = all tools).
    pub enabled_tools: Option<std::collections::HashSet<String>>,
    /// Turns since last TodoList write (for reminder).
    pub turns_since_todo: u32,
    /// Cross-turn tool dedupe tracker.
    pub tool_dedupe: crate::tool_dedupe::ToolDedupeTracker,
    /// Session token counter (measured anchors + estimates).
    pub token_counter: crate::token_counting::TokenCounter,
}

impl Session {
    pub fn new(
        id: String,
        working_dir: PathBuf,
        permission_mode: PermissionMode,
        model_alias: String,
    ) -> Self {
        let (approval_tx, approval_rx) = mpsc::channel(16);
        let (question_tx, question_rx) = mpsc::channel(16);
        let plan_file_path = working_dir
            .join(".kkagent")
            .join("plans")
            .join(format!("{}.md", id));
        Self {
            id,
            title: None,
            messages: Vec::new(),
            system_prompt: default_system_prompt(),
            working_dir,
            permission_mode,
            plan_mode: false,
            plan_file_path,
            model_alias: Arc::new(std::sync::Mutex::new(model_alias)),
            persisted_message_count: 0,
            approval_waiters: HashMap::new(),
            approval_rx,
            approval_tx,
            question_rx,
            question_tx,
            interrupted: Arc::new(AtomicBool::new(false)),
            turn_message_start: None,
            current_turn_changes: Vec::new(),
            undo_stack: Vec::new(),
            enabled_tools: None,
            turns_since_todo: 0,
            tool_dedupe: crate::tool_dedupe::ToolDedupeTracker::new(),
            token_counter: crate::token_counting::TokenCounter::new(
                crate::token_counting::TokenCountingStrategy::MeasuredPlusEstimated,
            ),
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
        let _ = self.question_tx.try_send(QuestionResponse {
            question_id: String::new(),
            selected_option_ids: Vec::new(),
            free_text: None,
            cancelled: true,
        });
    }

    pub fn is_interrupted(&self) -> bool {
        self.interrupted.load(Ordering::SeqCst)
    }

    pub fn get_model_alias(&self) -> String {
        self.model_alias
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn set_model_alias(&self, alias: impl Into<String>) {
        *self.model_alias.lock().unwrap_or_else(|e| e.into_inner()) = alias.into();
    }

    pub fn add_user_message(&mut self, text: String) {
        self.messages.push(ChatMessage {
            role: "user".into(),
            content: vec![ChatContent::Text { text }],
        });
    }

    pub fn build_messages(&self) -> Vec<ChatMessage> {
        messages_for_llm(self)
    }

    pub fn effective_system_prompt(&self) -> String {
        // Plan-mode constraints are injected as a fresh `<system-reminder>` user
        // message each turn (kimi-code style), not baked into the system prompt.
        self.system_prompt.clone()
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

    pub async fn wait_question(&mut self, question_id: &str) -> QuestionResponse {
        loop {
            if self.is_interrupted() {
                return QuestionResponse {
                    question_id: question_id.to_string(),
                    selected_option_ids: Vec::new(),
                    free_text: None,
                    cancelled: true,
                };
            }
            match tokio::time::timeout(
                std::time::Duration::from_millis(200),
                self.question_rx.recv(),
            )
            .await
            {
                Ok(Some(resp)) => {
                    if resp.question_id == question_id
                        || resp.question_id.is_empty()
                        || resp.cancelled
                    {
                        return resp;
                    }
                }
                Ok(None) => {
                    return QuestionResponse {
                        question_id: question_id.to_string(),
                        selected_option_ids: Vec::new(),
                        free_text: Some("question channel closed".into()),
                        cancelled: true,
                    };
                }
                Err(_) => continue,
            }
        }
    }

    pub fn submit_question(&self, response: QuestionResponse) {
        let _ = self.question_tx.try_send(response);
    }

    /// Append AGENTS.md / `.kkagent/AGENTS.md` into the system prompt (kimi-style workspace instructions).
    pub async fn inject_workspace_instructions(&mut self) {
        const MAX_CHARS: usize = 80_000;
        let candidates = [
            self.working_dir.join("AGENTS.md"),
            self.working_dir.join(".kkagent").join("AGENTS.md"),
            self.working_dir.join("CLAUDE.md"),
        ];
        for path in &candidates {
            let Ok(content) = tokio::fs::read_to_string(path).await else {
                continue;
            };
            let content = content.trim();
            if content.is_empty() {
                continue;
            }
            let truncated: String = content.chars().take(MAX_CHARS).collect();
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("instructions");
            self.system_prompt.push_str(&format!(
                "\n\n# Project instructions ({name})\n\n{truncated}"
            ));
            if content.chars().count() > MAX_CHARS {
                self.system_prompt
                    .push_str("\n\n… (truncated; open the file for the full text)");
            }
            // Prefer the first existing instructions file.
            break;
        }
    }

    pub fn inject_git_context(&mut self) {
        if let Some(ctx) = crate::git_context::collect_git_context(&self.working_dir) {
            self.system_prompt.push_str(&ctx);
        }
    }

    pub fn inject_date_reminder(&mut self) {
        let today = chrono::Local::now().format("%Y-%m-%d (%A)");
        self.system_prompt.push_str(&format!(
            "\n\n# Date\n\nToday's date is {today}.\n"
        ));
    }
}

fn default_system_prompt() -> String {
    // Aligned with kimi-code `profile/default/system.md` (trimmed for CLI v1 scope).
    r#"You are kkagent (Kimi Code CLI compatible), an interactive general AI agent running on a user's computer.

Your primary goal is to help users with software engineering tasks by taking action — use the tools available to you to make real changes on the user's system. You should also answer questions when asked. Always adhere strictly to the following system instructions and the user's requirements.

# Language

Write in the user's language unless they explicitly ask for a different one. Determine it from their most recent messages — if they switch languages mid-session, switch with them. This applies to everything user-visible: your replies, your reasoning and thinking, progress notes before and between tool calls, and questions you ask. Keep code, commands, identifiers, file paths, and technical terms in their original form.

# Prompt and Tool Use

For simple questions/greetings that do not involve any information in the working directory, you may simply reply directly. For anything else, default to taking action with tools. When the request could be interpreted as either a question to answer or a task to complete, treat it as a task.

When handling the user's request, if it involves creating, modifying, or running code or files, you MUST use the appropriate tools available to you to make actual changes — do not just describe the solution in text. When calling tools, do not provide detailed explanations or chain-of-thought. For non-trivial or multi-step tasks, first emit one short user-visible sentence describing what you will do next, then call the tool(s).

For broad codebase exploration (map a module, find call sites across many files, compare alternatives), prefer launching a `Task` subagent with a focused prompt so work can proceed in parallel; then use `TaskOutput` / `TaskList` to collect results. Do the exploration yourself only when it is a small, single-path lookup.

When a dedicated tool fits the job, reach for it before raw shell: `Read` a known path, `Glob` to find files by name, and `Grep` to search file contents.

Your text replies render as Markdown in the user's terminal. Use light Markdown: short paragraphs, `-` bullets, backticks for code/paths, fenced blocks for multi-line code. Do not use emoji unless the user does first.

You have the capability to output any number of tool calls in a single response. If you anticipate making multiple non-interfering tool calls, make them in parallel.

Tool calls run behind the user's permission settings. A rejected or denied call means the user or their policy declined that specific action — adjust your approach. Do not retry the same call unchanged.

# General Guidelines for Coding

When working on an existing codebase:
- Understand it by reading with tools (`Read`, `Glob`, `Grep`) before making changes.
- Make MINIMAL changes to achieve the goal.
- Keep edits scoped to the files and modules the request actually implies.
- Make new code read like the code around it.

DO NOT run `git commit`, `git push`, `git reset`, `git rebase` or other git mutations unless explicitly asked. Ask for confirmation each time.

Weigh reversibility and blast radius before destructive actions (`rm -rf`, dropping databases, force-pushing). Confirm first when the action is hard to undo or reaches beyond the local workspace.

# Context Management

When the conversation grows long, older turns may be compacted into a summary. Treat that summary as an accurate record of what already happened: do not redo work it reports as done.

Tool results and user messages may include `<system-reminder>` tags. These are authoritative system directives that you MUST follow — they may override normal behavior (e.g., restricting you to read-only actions during plan mode).
"#
    .to_string()
}

/// Mirrors kimi-code plan-mode injector `fullReminder`.
pub fn plan_mode_reminder(plan_file: &std::path::Path) -> String {
    format!(
        r#"<system-reminder>
Plan mode is active. You MUST NOT make any edits (with the exception of the current plan file) or otherwise make changes to the system unless a tool request is explicitly approved. Prefer read-only tools. Use Bash only when needed; Bash follows the normal permission mode and rules. This supersedes any other instructions you have received.

Plan file: {plan}

Workflow:
  1. Understand — explore the codebase with Glob, Grep, Read.
  2. Design — converge on the best approach; consider trade-offs but aim for a single recommendation.
  3. Review — re-read key files to verify understanding.
  4. Write Plan — create/update the plan file with Write or Edit (full markdown plan covering goal, approach, steps, risks, and verification).
  5. Exit — call ExitPlanMode for user approval.

## Plan quality
Write a COMPLETE plan in the plan file before exiting. Include:
- Goal / problem statement
- Chosen approach (and briefly why)
- Concrete implementation steps (files to touch, key changes)
- Risks / edge cases
- How to verify (tests / manual checks)

Keep at most 2-3 meaningfully different approaches. If one is clearly superior, just propose that one.
Your turn must end with either clarifying questions in text, or ExitPlanMode after writing the plan file.
Do NOT start implementing source changes. Do NOT edit files other than the plan file.
</system-reminder>"#,
        plan = plan_file.display()
    )
}

/// Build LLM-facing messages; when plan mode is on, append a fresh system-reminder
/// (kimi injects these into the conversation, not only the system prompt).
pub fn messages_for_llm(session: &Session) -> Vec<ChatMessage> {
    let mut messages = session.messages.clone();
    if session.plan_mode {
        messages.push(ChatMessage {
            role: "user".into(),
            content: vec![ChatContent::Text {
                text: plan_mode_reminder(&session.plan_file_path),
            }],
        });
    }
    messages
}
