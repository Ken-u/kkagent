use crossterm::{
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyModifiers,
        MouseButton, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use kkagent_client::KkagentClient;
use kkagent_config::AppConfig;
use kkagent_protocol::{AgentEvent, Frame, PermissionMode, SessionStatus};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io;
use std::path::PathBuf;

use crate::chrome::{StatusBarModel, TabStrip};
use crate::components;
use crate::controllers::SessionEventRouter;
use crate::input::InputState;
use crate::mouse_mode::MouseMode;
use crate::pi::{map_key, EditorAction};
use crate::search::SearchState;
use crate::slash::{
    self, filter_slash_commands, find_slash_command, is_slash_name_completion, parse_slash_input,
    SlashSuggestion,
};

pub struct TuiApp {
    config: AppConfig,
    client: KkagentClient,
    state: AppState,
    mouse_mode: MouseMode,
    /// Capture temporarily released after a click so the terminal can drag-select.
    mouse_select_mode: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AppMode {
    Normal,
    Shell,
    Plan,
}

pub struct AppState {
    pub messages: Vec<DisplayMessage>,
    pub input: InputState,
    pub status: SessionStatus,
    pub permission_mode: PermissionMode,
    pub plan_mode: bool,
    pub session_id: Option<String>,
    pub mode: AppMode,
    pub should_quit: bool,
    pub quit_confirm: bool,
    pub thinking_text: String,
    /// 离底部向上滚动的行数；0 = 贴底跟随新消息
    pub scroll_up: u16,
    pub content_lines: u16,
    pub viewport_height: u16,
    /// When true, keep transcript pinned to the latest content.
    pub follow_bottom: bool,
    /// Line index (from top) where each `messages[i]` starts — updated each frame.
    pub message_line_starts: Vec<u16>,
    pub approx_tokens: u64,
    pub approval_pending: Option<PendingApproval>,
    /// Approvals for sessions that are open in this window but not currently focused.
    pub parked_approvals: std::collections::HashMap<String, PendingApproval>,
    /// AskUserQuestion panel
    pub question_pending: Option<PendingQuestion>,
    /// Questions for background sessions in this window.
    pub parked_questions: std::collections::HashMap<String, PendingQuestion>,
    /// `/` command autocomplete popup
    pub slash_menu: Option<SlashMenuState>,
    /// `@` file path autocomplete popup
    pub file_menu: Option<FileMenuState>,
    /// Model / session list picker overlay
    pub list_picker: Option<ListPickerState>,
    /// Background tasks browser overlay
    pub tasks_panel: Option<TasksPanelState>,
    /// Queued user prompt to send after a slash command (avoids async recursion)
    pub pending_prompt: Option<String>,
    /// Spinner / redraw tick (increments every poll)
    pub tick: usize,
    /// Submitted user prompts for ↑↓ recall
    pub input_history: Vec<String>,
    /// None = not browsing history; Some(i) = viewing history[i]
    pub history_index: Option<usize>,
    /// Draft saved when entering history browse
    pub history_draft: String,
    /// First Esc timestamp for double-Esc undo (millis since epoch)
    pub pending_esc_ms: Option<u128>,
    /// Sticky todo panel (above input), latest TodoList state.
    pub todos: Vec<TodoItem>,
    /// Expand sticky todo beyond the collapsed max rows.
    pub todos_expanded: bool,
    /// BTW side-question panel (`/btw`).
    pub btw: crate::panes::BtwPanelState,
    /// Active model alias (best-effort).
    pub model_alias: Option<String>,
    /// Streaming cursor for live assistant deltas.
    pub stream_cursor: crate::streaming::StreamingCursor,
    /// Multi-session tab strip (chrome).
    pub tab_strip: TabStrip,
    /// Compact status model for chrome / footer sync.
    pub status_bar: StatusBarModel,
    /// Workspace sessions for footer strip + empty-input Tab cycling.
    pub workspace_sessions: crate::chrome::WorkspaceSessionStrip,
    /// Ephemeral Tab group for this TUI window (`/new` siblings). Not persisted.
    pub open_session_group: Vec<String>,
    /// Preview pane while `/sessions` list is open.
    pub session_picker_preview: Option<SessionPickerPreview>,
    /// Pending delete confirmation inside `/sessions` (default = No).
    pub session_delete_confirm: Option<SessionDeleteConfirm>,
    /// Session event router (controllers).
    pub event_router: SessionEventRouter,
    /// Ctrl-F transcript search overlay.
    pub search: SearchState,
    /// Show activity side hints in footer when streaming.
    pub last_tool_name: Option<String>,
    /// Message index to highlight (from search).
    pub highlight_message: Option<usize>,
    /// Latest plan.md snapshot; while `plan_mode` is on, TUI locks scroll to this doc.
    pub plan_document: Option<PlanDocument>,
    /// Jump once to the top of the focused plan after it appears / is replaced.
    pub plan_scroll_to_top: bool,
    /// Wall-clock start of the active agent turn (for tool-history duration).
    pub turn_started_at: Option<std::time::Instant>,
    /// `approx_tokens` snapshot at turn start (for delta in tool-history).
    pub tokens_at_turn_start: u64,
    /// UI locale for overview strings (English today).
    pub locale: crate::i18n::Locale,
}

#[derive(Debug, Clone)]
pub struct PlanDocument {
    pub path: String,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct SlashMenuState {
    pub items: Vec<SlashSuggestion>,
    pub selected: usize,
}

#[derive(Debug, Clone)]
pub struct FileMenuState {
    pub items: Vec<crate::pi::autocomplete::CompletionItem>,
    pub selected: usize,
    /// Byte offset of the `@` that started this token.
    pub token_start: usize,
    pub query: String,
    pub quoted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListPickerKind {
    Model,
    Session,
    Permission,
}

#[derive(Debug, Clone)]
pub struct ListPickerItem {
    pub id: String,
    pub label: String,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub struct SessionPickerPreview {
    pub session_id: String,
    /// Normal transcript bubbles shown in the main message area while browsing.
    pub messages: Vec<DisplayMessage>,
}

#[derive(Debug, Clone)]
pub struct SessionDeleteConfirm {
    pub session_id: String,
    pub label: String,
    /// 0 = No (default), 1 = Yes
    pub selected: usize,
}

#[derive(Debug, Clone)]
pub struct ListPickerState {
    pub kind: ListPickerKind,
    pub title: String,
    pub items: Vec<ListPickerItem>,
    pub selected: usize,
}

#[derive(Debug, Clone)]
pub struct TaskInfo {
    pub task_id: String,
    pub description: String,
    pub status: String,
    pub result: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TasksPanelState {
    pub tasks: Vec<TaskInfo>,
    pub selected: usize,
}

#[derive(Debug, Clone)]
pub struct DisplayMessage {
    pub role: MessageRole,
    pub content: String,
    pub thinking: Option<String>,
    /// Chronological assistant segments (text + tools interleaved by event order).
    pub parts: Vec<DisplayPart>,
    /// Legacy parallel list — kept empty for new assistant bubbles; resume may fill both.
    pub tool_calls: Vec<DisplayToolCall>,
}

#[derive(Debug, Clone)]
pub enum DisplayPart {
    Text(String),
    Tool(DisplayToolCall),
    /// Collapsed tool-call history between formal assistant outputs (after turn end).
    ToolHistory(ToolHistorySummary),
}

#[derive(Debug, Clone)]
pub struct ToolHistorySummary {
    pub tool_count: u32,
    pub duration_ms: u64,
    pub tokens: u64,
    pub expanded: bool,
    pub tools: Vec<DisplayToolCall>,
}

impl DisplayMessage {
    pub fn append_assistant_text(&mut self, text: &str) {
        if let Some(DisplayPart::Text(existing)) = self.parts.last_mut() {
            existing.push_str(text);
        } else {
            self.parts.push(DisplayPart::Text(text.to_string()));
        }
        // Keep content mirror for export / simple searches.
        self.content.push_str(text);
    }

    pub fn push_tool(&mut self, tc: DisplayToolCall) {
        self.parts.push(DisplayPart::Tool(tc));
    }

    pub fn find_pending_tool_mut(&mut self, name: &str) -> Option<&mut DisplayToolCall> {
        for part in self.parts.iter_mut().rev() {
            if let DisplayPart::Tool(tc) = part {
                if tc.name == name && tc.output.is_none() {
                    return Some(tc);
                }
            }
        }
        self.tool_calls
            .iter_mut()
            .rev()
            .find(|t| t.name == name && t.output.is_none())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum MessageRole {
    User,
    Assistant,
    System,
    /// Full plan.md contents shown after Write/Edit in plan mode.
    Plan,
}

#[derive(Debug, Clone)]
pub struct DisplayToolCall {
    pub name: String,
    pub input_summary: String,
    pub output: Option<String>,
    pub is_error: bool,
    pub collapsed: bool,
}

#[derive(Debug, Clone)]
pub struct TodoItem {
    pub id: String,
    pub content: String,
    pub status: String,
}

#[derive(Debug, Clone)]
pub struct ApprovalChoice {
    pub label: String,
    pub decision: kkagent_protocol::ApprovalDecision,
    pub selected_label: String,
    pub requires_feedback: bool,
    pub scope: Option<kkagent_protocol::ApprovalScope>,
}

#[derive(Debug, Clone)]
pub struct PendingApproval {
    pub approval_id: String,
    pub tool_name: String,
    pub action: String,
    pub detail: String,
    pub selected: usize,
    pub choices: Vec<ApprovalChoice>,
    pub is_plan_review: bool,
    pub feedback_mode: bool,
    pub feedback: String,
}

impl PendingApproval {
    pub fn default_tool_choices() -> Vec<ApprovalChoice> {
        vec![
            ApprovalChoice {
                label: "allow once".into(),
                decision: kkagent_protocol::ApprovalDecision::Approved,
                selected_label: "allow once".into(),
                requires_feedback: false,
                scope: None,
            },
            ApprovalChoice {
                label: "allow for session".into(),
                decision: kkagent_protocol::ApprovalDecision::Approved,
                selected_label: "allow for session".into(),
                requires_feedback: false,
                scope: Some(kkagent_protocol::ApprovalScope::Session),
            },
            ApprovalChoice {
                label: "reject".into(),
                decision: kkagent_protocol::ApprovalDecision::Rejected,
                selected_label: "reject".into(),
                requires_feedback: false,
                scope: None,
            },
        ]
    }

    pub fn plan_review_choices(display: &serde_json::Value) -> Vec<ApprovalChoice> {
        let mut choices = Vec::new();
        let options = display
            .get("options")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        if options.len() >= 2 {
            for opt in options {
                let label = opt
                    .get("label")
                    .and_then(|v| v.as_str())
                    .unwrap_or("option")
                    .to_string();
                choices.push(ApprovalChoice {
                    label: label.clone(),
                    decision: kkagent_protocol::ApprovalDecision::Approved,
                    selected_label: label,
                    requires_feedback: false,
                    scope: None,
                });
            }
        } else {
            choices.push(ApprovalChoice {
                label: "执行".into(),
                decision: kkagent_protocol::ApprovalDecision::Approved,
                selected_label: "执行".into(),
                requires_feedback: false,
                scope: None,
            });
        }
        choices.push(ApprovalChoice {
            label: "修改意见".into(),
            decision: kkagent_protocol::ApprovalDecision::Rejected,
            selected_label: "修改意见".into(),
            requires_feedback: true,
            scope: None,
        });
        choices.push(ApprovalChoice {
            label: "拒绝".into(),
            decision: kkagent_protocol::ApprovalDecision::Rejected,
            selected_label: "拒绝".into(),
            requires_feedback: false,
            scope: None,
        });
        choices
    }
}

#[derive(Debug, Clone)]
pub struct PendingQuestion {
    pub question_id: String,
    pub text: String,
    pub options: Vec<(String, String)>, // id, label
    pub allow_free_text: bool,
    pub selected: usize,
    pub toggled: Vec<bool>,
    pub free_text: String,
}

impl AppState {
    pub fn new(permission_mode: PermissionMode, plan_mode: bool) -> Self {
        Self {
            messages: Vec::new(),
            input: InputState::new(),
            status: SessionStatus::Idle,
            permission_mode,
            plan_mode,
            session_id: None,
            mode: if plan_mode {
                AppMode::Plan
            } else {
                AppMode::Normal
            },
            should_quit: false,
            quit_confirm: false,
            thinking_text: String::new(),
            scroll_up: 0,
            content_lines: 0,
            viewport_height: 0,
            follow_bottom: true,
            message_line_starts: Vec::new(),
            approx_tokens: 0,
            approval_pending: None,
            parked_approvals: std::collections::HashMap::new(),
            question_pending: None,
            parked_questions: std::collections::HashMap::new(),
            slash_menu: None,
            file_menu: None,
            list_picker: None,
            tasks_panel: None,
            pending_prompt: None,
            tick: 0,
            input_history: Vec::new(),
            history_index: None,
            history_draft: String::new(),
            pending_esc_ms: None,
            todos: Vec::new(),
            todos_expanded: false,
            btw: crate::panes::BtwPanelState::default(),
            model_alias: None,
            stream_cursor: crate::streaming::StreamingCursor::default(),
            tab_strip: TabStrip::default(),
            status_bar: StatusBarModel {
                permission: permission_mode,
                plan_mode,
                ..Default::default()
            },
            workspace_sessions: crate::chrome::WorkspaceSessionStrip::default(),
            open_session_group: Vec::new(),
            session_picker_preview: None,
            session_delete_confirm: None,
            event_router: SessionEventRouter::default(),
            search: SearchState::default(),
            last_tool_name: None,
            highlight_message: None,
            plan_document: None,
            plan_scroll_to_top: false,
            turn_started_at: None,
            tokens_at_turn_start: 0,
            locale: crate::i18n::Locale::En,
        }
    }

    /// Plan mode + a written plan → transcript shows only the full plan and
    /// scroll cannot leave that document until the user exits plan mode.
    pub fn plan_focus_active(&self) -> bool {
        self.plan_mode
            && self
                .plan_document
                .as_ref()
                .is_some_and(|p| !p.content.trim().is_empty())
    }

    pub fn apply_plan_document(&mut self, path: String, content: String) {
        let first = self.plan_document.is_none();
        self.plan_document = Some(PlanDocument {
            path: path.clone(),
            content: content.clone(),
        });
        self.messages.retain(|m| m.role != MessageRole::Plan);
        self.messages.push(DisplayMessage {
            role: MessageRole::Plan,
            content: format!("file: {}\n\n{}", path, content),
            thinking: None,
            parts: Vec::new(),
            tool_calls: Vec::new(),
        });
        if self.plan_mode {
            if first {
                self.plan_scroll_to_top = true;
                self.follow_bottom = false;
            } else {
                // Subsequent edits: keep the latest end of the plan in view.
                self.follow_bottom = true;
                self.scroll_up = 0;
            }
        }
    }

    pub fn on_plan_mode_changed(&mut self, enabled: bool) {
        self.plan_mode = enabled;
        self.mode = if enabled {
            AppMode::Plan
        } else {
            AppMode::Normal
        };
        self.status_bar.plan_mode = enabled;
        if enabled {
            if self.plan_document.is_some() {
                self.plan_scroll_to_top = true;
                self.follow_bottom = false;
            }
        } else {
            self.plan_scroll_to_top = false;
            self.follow_bottom = true;
            self.scroll_up = 0;
        }
    }

    /// After a completed agent loop, fold tool calls between formal assistant
    /// outputs into a single overview line (expandable with Ctrl+O).
    pub fn collapse_completed_turn_tools(&mut self) {
        let duration_ms = self
            .turn_started_at
            .map(|t| t.elapsed().as_millis() as u64)
            .unwrap_or(0);
        let tokens = self
            .approx_tokens
            .saturating_sub(self.tokens_at_turn_start);
        self.turn_started_at = None;

        let start = self
            .messages
            .iter()
            .rposition(|m| m.role == MessageRole::User)
            .map(|i| i + 1)
            .unwrap_or(0);

        let mut collected: Vec<DisplayToolCall> = Vec::new();
        for msg in &mut self.messages[start..] {
            if msg.role != MessageRole::Assistant {
                continue;
            }
            let mut kept = Vec::with_capacity(msg.parts.len());
            for part in msg.parts.drain(..) {
                match part {
                    DisplayPart::Tool(tc) => collected.push(tc),
                    DisplayPart::ToolHistory(mut hist) => {
                        // Already collapsed earlier in the same turn — merge.
                        collected.append(&mut hist.tools);
                    }
                    other => kept.push(other),
                }
            }
            // Legacy list
            collected.append(&mut msg.tool_calls);
            msg.parts = kept;
        }

        if collected.is_empty() {
            return;
        }

        let summary = DisplayPart::ToolHistory(ToolHistorySummary {
            tool_count: collected.len() as u32,
            duration_ms,
            tokens,
            expanded: false,
            tools: collected,
        });

        // Place the overview just before the last formal text of the turn,
        // so the final answer stays at the bottom; otherwise append.
        if let Some(msg) = self.messages[start..]
            .iter_mut()
            .rev()
            .find(|m| m.role == MessageRole::Assistant)
        {
            if let Some(idx) = msg
                .parts
                .iter()
                .rposition(|p| matches!(p, DisplayPart::Text(t) if !t.trim().is_empty()))
            {
                msg.parts.insert(idx, summary);
            } else {
                msg.parts.push(summary);
            }
        } else {
            self.messages.push(DisplayMessage {
                role: MessageRole::Assistant,
                content: String::new(),
                thinking: None,
                parts: vec![summary],
                tool_calls: Vec::new(),
            });
        }
    }

    pub fn max_scroll_up(&self) -> u16 {
        self.content_lines
            .saturating_sub(self.viewport_height.max(1))
    }

    pub fn scroll_lines(&mut self, delta: i32) {
        if delta > 0 {
            let max = self.max_scroll_up();
            self.scroll_up = (self.scroll_up as i32 + delta).clamp(0, max as i32) as u16;
        } else if delta < 0 {
            self.scroll_up = self.scroll_up.saturating_sub((-delta) as u16);
        }
        self.follow_bottom = self.scroll_up == 0;
    }

    pub fn push_input_history(&mut self, text: &str) {
        let t = text.trim();
        if t.is_empty() {
            return;
        }
        if self.input_history.last().map(|s| s.as_str()) != Some(t) {
            self.input_history.push(t.to_string());
        }
        self.history_index = None;
        self.history_draft.clear();
    }

    pub fn history_prev(&mut self) {
        if self.input_history.is_empty() {
            return;
        }
        match self.history_index {
            None => {
                self.history_draft = self.input.text.clone();
                let i = self.input_history.len() - 1;
                self.history_index = Some(i);
                self.input.set_text(self.input_history[i].clone());
            }
            Some(0) => {}
            Some(i) => {
                let i = i - 1;
                self.history_index = Some(i);
                self.input.set_text(self.input_history[i].clone());
            }
        }
    }

    pub fn history_next(&mut self) {
        let Some(i) = self.history_index else {
            return;
        };
        if i + 1 >= self.input_history.len() {
            self.history_index = None;
            self.input.set_text(std::mem::take(&mut self.history_draft));
        } else {
            let i = i + 1;
            self.history_index = Some(i);
            self.input.set_text(self.input_history[i].clone());
        }
    }

    pub fn refresh_slash_menu(&mut self) {
        let text = self.input.text.clone();
        if !is_slash_name_completion(&text) {
            self.slash_menu = None;
            // Fall through to @ file completion when not in slash mode.
            self.refresh_file_menu();
            return;
        }
        self.file_menu = None;
        let items = filter_slash_commands(&text);
        if items.is_empty() {
            self.slash_menu = Some(SlashMenuState {
                items: Vec::new(),
                selected: 0,
            });
        } else {
            let selected = self
                .slash_menu
                .as_ref()
                .map(|m| m.selected.min(items.len().saturating_sub(1)))
                .unwrap_or(0);
            self.slash_menu = Some(SlashMenuState { items, selected });
        }
    }

    pub fn refresh_file_menu(&mut self) {
        if self.mode == AppMode::Shell {
            self.file_menu = None;
            return;
        }
        let text = self.input.text.clone();
        let cursor = self.input.cursor.min(text.len());
        let Some((token_start, query)) = crate::pi::autocomplete::extract_at_token(&text, cursor)
        else {
            self.file_menu = None;
            return;
        };
        let quoted = text[token_start..].starts_with("@\"");
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let items = crate::pi::autocomplete::complete_at_files(&cwd, &query, 24);
        let selected = self
            .file_menu
            .as_ref()
            .filter(|m| m.token_start == token_start)
            .map(|m| {
                if items.is_empty() {
                    0
                } else {
                    m.selected.min(items.len() - 1)
                }
            })
            .unwrap_or(0);
        self.file_menu = Some(FileMenuState {
            items,
            selected,
            token_start,
            query,
            quoted,
        });
    }
}

impl TuiApp {
    pub fn new(config: AppConfig, client: KkagentClient) -> Self {
        let permission_mode = config
            .effective_permission_mode()
            .parse()
            .unwrap_or(PermissionMode::Manual);
        let plan_mode = config.default_plan_mode;

        Self {
            config,
            client,
            state: AppState::new(permission_mode, plan_mode),
            mouse_mode: MouseMode::from_env(),
            mouse_select_mode: false,
        }
    }

    pub async fn run(mut self, resume: Option<String>) -> anyhow::Result<()> {
        // Create / resume session BEFORE taking over the terminal, so RPC
        // failures don't leave the user's shell stuck in raw/alternate mode.
        let cwd = std::env::current_dir()?.to_string_lossy().to_string();
        if let Some(id) = resume {
            if let Err(e) = self.resume_session(&id).await {
                eprintln!("Resume failed ({}): {}. Starting a new session.", id, e);
                let session_id = self
                    .client
                    .create_session(Some(&cwd), Some(self.state.permission_mode))
                    .await?;
                self.state.tab_strip.ensure_active(&session_id, "main");
                self.state.status_bar.session_id = Some(session_id.clone());
                self.state.session_id = Some(session_id);
            }
        } else {
            let session_id = self
                .client
                .create_session(Some(&cwd), Some(self.state.permission_mode))
                .await?;
            self.state.tab_strip.ensure_active(&session_id, "main");
            self.state.status_bar.session_id = Some(session_id.clone());
            self.state.session_id = Some(session_id);
        }

        // Sync CLI / config plan mode onto the server session (create starts with plan_mode=false).
        if self.state.plan_mode {
            if let Some(ref sid) = self.state.session_id.clone() {
                if let Err(e) = self.client.set_plan_mode(sid, true).await {
                    eprintln!("Failed to enable plan mode: {}", e);
                }
            }
        }

        let _ = self.refresh_workspace_sessions().await;

        enable_raw_mode().map_err(|e| {
            anyhow::anyhow!(
                "Failed to enter raw mode (is stdin a TTY?): {}. \
                 Run kkagent in a real terminal, or use `kkagent -p \"...\"` for non-interactive mode.",
                e
            )
        })?;
        let mut stdout = io::stdout();
        // Capture mouse for wheel scroll. Left-click temporarily releases capture so
        // the terminal can drag-select; the next keypress restores capture. ↑↓ stay
        // on input history. `KKAGENT_MOUSE_MODE=off` disables mouse reporting.
        if let Err(e) = execute!(stdout, EnterAlternateScreen, EnableBracketedPaste) {
            let _ = disable_raw_mode();
            return Err(e.into());
        }
        if let Err(e) = self.mouse_mode.enable(&mut stdout) {
            let _ = execute!(stdout, DisableBracketedPaste, LeaveAlternateScreen);
            let _ = disable_raw_mode();
            return Err(e.into());
        }
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = match Terminal::new(backend) {
            Ok(t) => t,
            Err(e) => {
                let _ = disable_raw_mode();
                return Err(e.into());
            }
        };

        let result = self.main_loop(&mut terminal).await;
        let sid = self.state.session_id.clone();
        let empty = !session_has_retained_io(&self.state.messages);

        // Interrupt any in-flight turn before tearing down the paired server.
        if let Some(ref id) = sid {
            let _ = self.client.interrupt(id).await;
            if empty {
                let _ = self.discard_session_record(id).await;
            }
        }

        // Always restore the terminal, even if the loop failed.
        let _ = disable_raw_mode();
        let _ = self.mouse_mode.disable(terminal.backend_mut());
        let _ = execute!(
            terminal.backend_mut(),
            DisableBracketedPaste,
            LeaveAlternateScreen
        );
        let _ = terminal.show_cursor();

        if let Some(id) = sid {
            if !empty {
                println!();
                println!("Session: {}", id);
                println!("Resume:  kkagent --resume {}", id);
                println!();
            }
        }

        result
    }

    async fn main_loop(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    ) -> anyhow::Result<()> {
        loop {
            terminal.draw(|f| {
                components::render_ui(f, &mut self.state, &self.config);
            })?;

            if event::poll(std::time::Duration::from_millis(50))? {
                match event::read()? {
                    Event::Key(key) => {
                        self.restore_mouse_capture();
                        self.handle_key(key).await?;
                    }
                    Event::Mouse(mouse)
                        if self.mouse_mode == MouseMode::Capture && !self.mouse_select_mode =>
                    {
                        self.handle_mouse(mouse);
                    }
                    Event::Paste(text) => {
                        self.restore_mouse_capture();
                        let fold = self.state.mode != AppMode::Shell;
                        self.state.input.paste_chunk(&text);
                        self.state.input.force_flush_paste(fold);
                        self.state.refresh_slash_menu();
                    }
                    Event::Resize(_, _) => {}
                    _ => {}
                }
            } else {
                // Debounced paste flush (pi-tui paste-burst)
                let fold = self.state.mode != AppMode::Shell;
                if self.state.input.flush_paste(fold) {
                    self.state.refresh_slash_menu();
                }
            }

            if let Some(prompt) = self.state.pending_prompt.take() {
                self.state.input.set_text(prompt);
                self.submit_input().await?;
            }

            while let Ok(frame) = self.client.event_rx.try_recv() {
                self.handle_server_event(frame);
            }

            self.state.tick = self.state.tick.wrapping_add(1);
            if self.state.tick % 100 == 0 {
                let _ = self.refresh_workspace_sessions().await;
            }
            if matches!(
                self.state.status,
                SessionStatus::Thinking | SessionStatus::ToolExecuting
            ) {
                self.state.stream_cursor.tick();
            }

            if self.state.should_quit {
                break;
            }
        }
        Ok(())
    }

    /// Close the topmost transient UI (menus / pickers / search / btw / shell).
    /// Returns true if something was dismissed. Does not touch the agent turn.
    fn dismiss_transient_ui(&mut self) -> bool {
        if self.state.session_delete_confirm.take().is_some() {
            return true;
        }
        if self.state.list_picker.take().is_some() {
            self.state.session_picker_preview = None;
            return true;
        }
        if self.state.tasks_panel.take().is_some() {
            return true;
        }
        if self.state.search.active {
            self.state.search.close();
            self.state.highlight_message = None;
            return true;
        }
        if self.state.file_menu.take().is_some() {
            return true;
        }
        if self.state.slash_menu.take().is_some() {
            return true;
        }
        if self.state.btw.open {
            self.state.btw.open = false;
            return true;
        }
        if self.state.mode == AppMode::Shell {
            self.state.mode = AppMode::Normal;
            return true;
        }
        false
    }

    /// Re-enable wheel capture after the user finished selecting (any key/paste).
    fn restore_mouse_capture(&mut self) {
        if !self.mouse_select_mode || self.mouse_mode != MouseMode::Capture {
            return;
        }
        let _ = self.mouse_mode.enable(&mut io::stdout());
        self.mouse_select_mode = false;
    }

    /// Release capture on click so the terminal owns drag-select/copy.
    fn release_mouse_for_select(&mut self) {
        if self.mouse_select_mode || self.mouse_mode != MouseMode::Capture {
            return;
        }
        let _ = self.mouse_mode.disable(&mut io::stdout());
        self.mouse_select_mode = true;
    }

    fn handle_mouse(&mut self, mouse: crossterm::event::MouseEvent) {
        match mouse.kind {
            MouseEventKind::ScrollUp => self.state.scroll_lines(3),
            MouseEventKind::ScrollDown => self.state.scroll_lines(-3),
            MouseEventKind::Down(MouseButton::Left) => self.release_mouse_for_select(),
            _ => {}
        }
    }

    async fn handle_key(&mut self, key: KeyEvent) -> anyhow::Result<()> {
        // Esc while a menu/overlay is open: only dismiss that UI — never interrupt
        // an in-flight turn. Ctrl-C still cancels the turn below.
        if matches!(key.code, KeyCode::Esc) && self.dismiss_transient_ui() {
            self.state.pending_esc_ms = None;
            self.state.quit_confirm = false;
            return Ok(());
        }

        // Busy turn with no overlay: Esc / Ctrl-C interrupt the agent.
        if !matches!(self.state.status, SessionStatus::Idle)
            && (matches!(key.code, KeyCode::Esc)
                || (matches!(key.code, KeyCode::Char('c'))
                    && key.modifiers.contains(KeyModifiers::CONTROL)))
        {
            if let Some(sid) = self.state.session_id.clone() {
                match self.client.interrupt(&sid).await {
                    Ok(()) => {
                        self.system_message("Interrupted — cancelling in-flight tools…".into());
                    }
                    Err(e) => {
                        self.system_message(format!("Interrupt failed: {e}"));
                    }
                }
            }
            self.state.pending_esc_ms = None;
            self.state.quit_confirm = false;
            return Ok(());
        }

        // Transcript search overlay (Ctrl-F)
        if self.state.search.active {
            return self.handle_search_key(key);
        }

        // Handle question panel first
        if self.state.question_pending.is_some() {
            return self.handle_question_key(key).await;
        }

        // Handle approval panel
        if let Some(ref mut approval) = self.state.approval_pending {
            let n = approval.choices.len().max(1);
            if approval.feedback_mode {
                match key.code {
                    KeyCode::Esc => {
                        approval.feedback_mode = false;
                        approval.feedback.clear();
                    }
                    KeyCode::Enter => {
                        let feedback = approval.feedback.clone();
                        let choice = approval.choices.get(approval.selected).cloned();
                        if let Some(choice) = choice {
                            self.respond_approval_choice(choice, Some(feedback)).await?;
                        }
                    }
                    KeyCode::Backspace => {
                        approval.feedback.pop();
                    }
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        approval.feedback.push(c);
                    }
                    KeyCode::Up | KeyCode::Down => {
                        approval.feedback_mode = false;
                    }
                    _ => {}
                }
                return Ok(());
            }
            match key.code {
                KeyCode::Char(c) if c.is_ascii_digit() => {
                    let idx = (c as u8 - b'1') as usize;
                    if idx < approval.choices.len() {
                        let choice = approval.choices[idx].clone();
                        if choice.requires_feedback {
                            approval.selected = idx;
                            approval.feedback_mode = true;
                            approval.feedback.clear();
                        } else {
                            self.respond_approval_choice(choice, None).await?;
                        }
                    }
                }
                KeyCode::Esc => {
                    self.respond_approval_choice(
                        ApprovalChoice {
                            label: "cancel".into(),
                            decision: kkagent_protocol::ApprovalDecision::Cancelled,
                            selected_label: "cancel".into(),
                            requires_feedback: false,
                            scope: None,
                        },
                        None,
                    )
                    .await?;
                }
                KeyCode::Up if approval.selected > 0 => {
                    approval.selected -= 1;
                }
                KeyCode::Down if approval.selected + 1 < n => {
                    approval.selected += 1;
                }
                KeyCode::Enter => {
                    let choice = approval.choices.get(approval.selected).cloned();
                    if let Some(choice) = choice {
                        if choice.requires_feedback {
                            approval.feedback_mode = true;
                            approval.feedback.clear();
                        } else {
                            self.respond_approval_choice(choice, None).await?;
                        }
                    }
                }
                _ => {}
            }
            return Ok(());
        }

        // Tasks browser overlay
        if self.state.tasks_panel.is_some() {
            match key.code {
                KeyCode::Up => {
                    if let Some(ref mut p) = self.state.tasks_panel {
                        if p.selected > 0 {
                            p.selected -= 1;
                        }
                    }
                    return Ok(());
                }
                KeyCode::Down => {
                    if let Some(ref mut p) = self.state.tasks_panel {
                        if !p.tasks.is_empty() {
                            p.selected = (p.selected + 1) % p.tasks.len();
                        }
                    }
                    return Ok(());
                }
                KeyCode::Char('r') | KeyCode::Char('R') => {
                    self.open_tasks_panel().await?;
                    return Ok(());
                }
                KeyCode::Enter => {
                    if let Some(ref p) = self.state.tasks_panel {
                        if let Some(t) = p.tasks.get(p.selected) {
                            let mut detail =
                                format!("task {} [{}]\n{}\n", t.task_id, t.status, t.description);
                            if let Some(ref r) = t.result {
                                detail.push_str("\nresult:\n");
                                detail.push_str(r);
                            }
                            if let Some(ref e) = t.error {
                                detail.push_str("\nerror:\n");
                                detail.push_str(e);
                            }
                            self.system_message(detail);
                        }
                    }
                    return Ok(());
                }
                KeyCode::Char('x') | KeyCode::Char('s') | KeyCode::Char('S') => {
                    let id = self
                        .state
                        .tasks_panel
                        .as_ref()
                        .and_then(|p| p.tasks.get(p.selected))
                        .map(|t| t.task_id.clone());
                    if let Some(task_id) = id {
                        match self
                            .client
                            .rpc_call(
                                "tasks.stop",
                                Some(serde_json::json!({ "task_id": task_id })),
                            )
                            .await
                        {
                            Ok(_) => {
                                self.system_message(format!("Stopped task {}", task_id));
                                self.open_tasks_panel().await?;
                            }
                            Err(e) => self.system_message(format!("Stop failed: {}", e)),
                        }
                    }
                    return Ok(());
                }
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.state.tasks_panel = None;
                    return Ok(());
                }
                _ => {}
            }
        }

        // List picker (model / sessions)
        if self.state.list_picker.is_some() {
            // Delete confirmation: ↑↓ choose · Enter confirm (default No)
            if let Some(ref mut confirm) = self.state.session_delete_confirm {
                match key.code {
                    KeyCode::Up | KeyCode::BackTab => {
                        confirm.selected = 0;
                    }
                    KeyCode::Down | KeyCode::Tab => {
                        confirm.selected = 1;
                    }
                    KeyCode::Left => {
                        confirm.selected = 0;
                    }
                    KeyCode::Right => {
                        confirm.selected = 1;
                    }
                    KeyCode::Enter => {
                        let yes = confirm.selected == 1;
                        self.confirm_delete_session(yes).await?;
                    }
                    KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                        self.confirm_delete_session(false).await?;
                    }
                    KeyCode::Char('y') | KeyCode::Char('Y') => {
                        self.confirm_delete_session(true).await?;
                    }
                    _ => {}
                }
                return Ok(());
            }
            match key.code {
                KeyCode::Up => {
                    if let Some(ref mut p) = self.state.list_picker {
                        if p.selected > 0 {
                            p.selected -= 1;
                        } else if !p.items.is_empty() {
                            p.selected = p.items.len() - 1;
                        }
                    }
                    if self
                        .state
                        .list_picker
                        .as_ref()
                        .map(|p| p.kind == ListPickerKind::Session)
                        .unwrap_or(false)
                    {
                        self.refresh_session_picker_preview().await?;
                    }
                    return Ok(());
                }
                KeyCode::Down => {
                    if let Some(ref mut p) = self.state.list_picker {
                        if !p.items.is_empty() {
                            p.selected = (p.selected + 1) % p.items.len();
                        }
                    }
                    if self
                        .state
                        .list_picker
                        .as_ref()
                        .map(|p| p.kind == ListPickerKind::Session)
                        .unwrap_or(false)
                    {
                        self.refresh_session_picker_preview().await?;
                    }
                    return Ok(());
                }
                KeyCode::Enter => {
                    self.apply_list_picker().await?;
                    return Ok(());
                }
                KeyCode::Esc => {
                    self.state.list_picker = None;
                    self.state.session_picker_preview = None;
                    self.state.session_delete_confirm = None;
                    return Ok(());
                }
                KeyCode::Char('d') | KeyCode::Char('D')
                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        && self
                            .state
                            .list_picker
                            .as_ref()
                            .map(|p| p.kind == ListPickerKind::Session)
                            .unwrap_or(false) =>
                {
                    if let Some(picker) = self.state.list_picker.as_ref() {
                        if let Some(item) = picker.items.get(picker.selected) {
                            self.state.session_delete_confirm = Some(SessionDeleteConfirm {
                                session_id: item.id.clone(),
                                label: item.label.clone(),
                                selected: 0, // default No
                            });
                        }
                    }
                    return Ok(());
                }
                _ => {}
            }
        }

        // `@` file autocomplete popup
        if self.state.file_menu.is_some() && self.state.slash_menu.is_none() {
            match key.code {
                KeyCode::Up => {
                    if let Some(ref mut menu) = self.state.file_menu {
                        if menu.selected > 0 {
                            menu.selected -= 1;
                        } else if !menu.items.is_empty() {
                            menu.selected = menu.items.len() - 1;
                        }
                    }
                    return Ok(());
                }
                KeyCode::Down => {
                    if let Some(ref mut menu) = self.state.file_menu {
                        if !menu.items.is_empty() {
                            menu.selected = (menu.selected + 1) % menu.items.len();
                        }
                    }
                    return Ok(());
                }
                KeyCode::Tab => {
                    if self
                        .state
                        .file_menu
                        .as_ref()
                        .map(|m| !m.items.is_empty())
                        .unwrap_or(false)
                    {
                        self.apply_file_completion()?;
                    }
                    return Ok(());
                }
                KeyCode::Enter if !key.modifiers.contains(KeyModifiers::SHIFT) => {
                    if self
                        .state
                        .file_menu
                        .as_ref()
                        .map(|m| !m.items.is_empty())
                        .unwrap_or(false)
                    {
                        self.apply_file_completion()?;
                        return Ok(());
                    }
                    // No matches — dismiss menu and fall through to submit.
                    self.state.file_menu = None;
                }
                KeyCode::Esc => {
                    self.state.file_menu = None;
                    return Ok(());
                }
                _ => {}
            }
        }

        // Slash command autocomplete popup
        if self.state.slash_menu.is_some() {
            match key.code {
                KeyCode::Up => {
                    if let Some(ref mut menu) = self.state.slash_menu {
                        if menu.selected > 0 {
                            menu.selected -= 1;
                        } else if !menu.items.is_empty() {
                            menu.selected = menu.items.len() - 1;
                        }
                    }
                    return Ok(());
                }
                KeyCode::Down => {
                    if let Some(ref mut menu) = self.state.slash_menu {
                        if !menu.items.is_empty() {
                            menu.selected = (menu.selected + 1) % menu.items.len();
                        }
                    }
                    return Ok(());
                }
                KeyCode::Tab => {
                    self.apply_slash_completion(false).await?;
                    return Ok(());
                }
                KeyCode::Enter if !key.modifiers.contains(KeyModifiers::SHIFT) => {
                    // Complete then submit if command needs no args, else just complete
                    self.apply_slash_completion(true).await?;
                    return Ok(());
                }
                KeyCode::Esc => {
                    self.state.slash_menu = None;
                    return Ok(());
                }
                _ => {}
            }
        }

        match key.code {
            // Kimi-style media paste. Text paste still falls back to the platform clipboard.
            KeyCode::Char('v')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    || (cfg!(target_os = "windows")
                        && key.modifiers.contains(KeyModifiers::ALT)) =>
            {
                match paste_clipboard_into_workspace() {
                    Ok(Some(path)) => {
                        let mention = format!("@{}", path.to_string_lossy());
                        self.state.input.insert_str(&mention);
                        self.state.refresh_slash_menu();
                        self.system_message(format!(
                            "Attached clipboard image: {}",
                            path.display()
                        ));
                    }
                    Ok(None) => {
                        if let Ok(text) = read_clipboard_text() {
                            let fold = self.state.mode != AppMode::Shell;
                            self.state.input.insert_paste(&text, fold);
                            self.state.refresh_slash_menu();
                        }
                    }
                    Err(error) => self.system_message(format!("Image paste failed: {error}")),
                }
            }
            // Ctrl-C: interrupt or quit
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.state.status != SessionStatus::Idle {
                    if let Some(sid) = &self.state.session_id {
                        self.client.interrupt(sid).await?;
                        self.system_message("Interrupted.".into());
                    }
                } else if self.state.input.is_empty() {
                    if self.state.quit_confirm {
                        self.state.should_quit = true;
                    } else {
                        self.state.quit_confirm = true;
                    }
                } else {
                    self.state.input.clear();
                    self.state.slash_menu = None;
                    self.state.file_menu = None;
                    self.state.list_picker = None;
                }
            }
            // Ctrl-D: quit if empty
            KeyCode::Char('d')
                if key.modifiers.contains(KeyModifiers::CONTROL) && self.state.input.is_empty() =>
            {
                if self.state.quit_confirm {
                    self.state.should_quit = true;
                } else {
                    self.state.quit_confirm = true;
                }
            }
            // Escape: dismiss overlays already handled above; here interrupt / double-Esc undo
            KeyCode::Esc => {
                if self.state.status != SessionStatus::Idle {
                    if let Some(sid) = &self.state.session_id {
                        self.client.interrupt(sid).await?;
                        self.system_message("Interrupted.".into());
                    }
                    self.state.pending_esc_ms = None;
                } else {
                    // Idle: double-Esc within 600ms → undo last turn (messages + files)
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis())
                        .unwrap_or(0);
                    if let Some(prev) = self.state.pending_esc_ms {
                        if now.saturating_sub(prev) <= 600 {
                            self.state.pending_esc_ms = None;
                            self.undo_turns(1).await?;
                        } else {
                            self.state.pending_esc_ms = Some(now);
                            self.system_message(
                                "Press Esc again to undo the last turn (messages + file changes)."
                                    .into(),
                            );
                        }
                    } else {
                        self.state.pending_esc_ms = Some(now);
                        self.system_message(
                            "Press Esc again to undo the last turn (messages + file changes)."
                                .into(),
                        );
                    }
                }
                self.state.quit_confirm = false;
            }
            // Ctrl+Shift-Tab: previous session tab
            KeyCode::BackTab if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.state.tab_strip.prev();
            }
            // Shift-Tab: toggle plan mode
            KeyCode::BackTab => {
                let enabled = !self.state.plan_mode;
                self.state.on_plan_mode_changed(enabled);
                if let Some(sid) = &self.state.session_id {
                    self.client.set_plan_mode(sid, enabled).await?;
                }
                if enabled {
                    self.system_message(
                        "Plan mode ON — explore & write plan only. \
                         Source edits are denied until you ExitPlanMode. \
                         After a plan is written, scroll stays within the plan \
                         until you exit plan mode."
                            .into(),
                    );
                } else {
                    self.system_message("Plan mode OFF.".into());
                }
            }
            // Enter: submit
            KeyCode::Enter if !key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.submit_input().await?;
            }
            // Empty input Tab / ← →: cycle related sessions (/new or /fork group)
            KeyCode::Tab
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::SHIFT)
                    && !key.modifiers.contains(KeyModifiers::ALT)
                    && self.can_cycle_fork_sessions() =>
            {
                self.cycle_workspace_session(1).await?;
            }
            KeyCode::Left
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT)
                    && self.can_cycle_fork_sessions() =>
            {
                self.cycle_workspace_session(-1).await?;
            }
            KeyCode::Right
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT)
                    && self.can_cycle_fork_sessions() =>
            {
                self.cycle_workspace_session(1).await?;
            }
            // Shift-Enter or Ctrl-J: newline
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.state.input.insert_char('\n');
                self.state.refresh_slash_menu();
            }
            KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.state.input.insert_char('\n');
                self.state.refresh_slash_menu();
            }
            // Ctrl-F / Ctrl-S: open transcript search
            KeyCode::Char('f') | KeyCode::Char('s')
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.state.search.open();
                self.state.slash_menu = None;
                self.state.file_menu = None;
                self.state.list_picker = None;
            }
            // Ctrl-G: toggle / cancel btw pane
            KeyCode::Char('g') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.state.btw.open && self.state.btw.streaming {
                    if let Some(sid) = self.state.session_id.clone() {
                        let _ = self.client.cancel_btw(&sid).await;
                    }
                    self.state.btw.streaming = false;
                    self.state.btw.error = Some("cancelled".into());
                }
                self.state.btw.open = !self.state.btw.open;
            }
            // Ctrl-O: toggle tool output folding
            KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.toggle_tool_folding();
            }
            // Ctrl-P / Ctrl-N: input history (same as ↑↓ at line edges)
            KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.state.history_prev();
            }
            KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.state.history_next();
            }
            // Ctrl-T: expand/collapse sticky todo panel
            KeyCode::Char('t')
                if key.modifiers.contains(KeyModifiers::CONTROL) && self.state.todos.len() > 5 =>
            {
                self.state.todos_expanded = !self.state.todos_expanded;
            }
            // Emacs/pi-tui editor bindings (kill/yank/undo/word nav)
            KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.state.input.kill_line();
                self.state.refresh_slash_menu();
            }
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.state.input.kill_word();
                self.state.refresh_slash_menu();
            }
            KeyCode::Char('y') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.state.input.yank();
                self.state.refresh_slash_menu();
            }
            KeyCode::Char('z') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let _ = self.state.input.undo_edit();
                self.state.refresh_slash_menu();
            }
            KeyCode::Char('Z') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let _ = self.state.input.redo_edit();
                self.state.refresh_slash_menu();
            }
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.state.input.move_home();
            }
            KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.state.input.move_end();
            }
            KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.state.input.move_left();
            }
            KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.state.input.move_right();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.state.input.clear();
                self.state.slash_menu = None;
            }
            KeyCode::Left
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    || key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.state.input.move_word_left();
            }
            KeyCode::Right
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    || key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.state.input.move_word_right();
            }
            KeyCode::Delete => {
                self.state.input.delete();
                self.state.refresh_slash_menu();
            }
            KeyCode::Tab if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.state.tab_strip.next();
            }
            // Normal character input
            KeyCode::Char(c) => {
                self.state.pending_esc_ms = None;
                self.state.quit_confirm = false;
                // Shell mode trigger
                if c == '!' && self.state.input.is_empty() && self.state.mode == AppMode::Normal {
                    self.state.mode = AppMode::Shell;
                    self.state.slash_menu = None;
                } else {
                    // Prefer pi key map for plain inserts
                    match map_key(key) {
                        EditorAction::Insert(ch) => {
                            self.state.input.insert_char(ch);
                            self.state.refresh_slash_menu();
                        }
                        _ => {
                            self.state.input.insert_char(c);
                            self.state.refresh_slash_menu();
                        }
                    }
                }
            }
            KeyCode::Backspace => {
                if self.state.input.is_empty() && self.state.mode == AppMode::Shell {
                    self.state.mode = AppMode::Normal;
                } else if key.modifiers.contains(KeyModifiers::ALT)
                    || key.modifiers.contains(KeyModifiers::CONTROL)
                {
                    self.state.input.kill_word();
                    self.state.refresh_slash_menu();
                } else {
                    self.state.input.backspace();
                    self.state.refresh_slash_menu();
                }
            }
            KeyCode::Left => self.state.input.move_left(),
            KeyCode::Right => self.state.input.move_right(),
            // ↑↓: multiline move first, otherwise input history
            KeyCode::Up => {
                let cursor = self.state.input.cursor.min(self.state.input.text.len());
                let cursor = {
                    let mut c = cursor;
                    while c > 0 && !self.state.input.text.is_char_boundary(c) {
                        c -= 1;
                    }
                    c
                };
                let at_first_line = !self.state.input.text[..cursor].contains('\n');
                if self.state.input.text.contains('\n') && !at_first_line {
                    self.state.input.move_up();
                } else {
                    self.state.history_prev();
                }
            }
            KeyCode::Down => {
                let cursor = self.state.input.cursor.min(self.state.input.text.len());
                let cursor = {
                    let mut c = cursor;
                    while c > 0 && !self.state.input.text.is_char_boundary(c) {
                        c -= 1;
                    }
                    c
                };
                let after = &self.state.input.text[cursor..];
                let at_last_line = !after.contains('\n');
                if self.state.input.text.contains('\n') && !at_last_line {
                    self.state.input.move_down();
                } else {
                    self.state.history_next();
                }
            }
            KeyCode::PageUp => self.state.scroll_lines(10),
            KeyCode::PageDown => self.state.scroll_lines(-10),
            KeyCode::Home if self.state.input.is_empty() => {
                self.state.scroll_up = self.state.max_scroll_up();
                self.state.follow_bottom = false;
            }
            KeyCode::End if self.state.input.is_empty() => {
                self.state.scroll_up = 0;
                self.state.follow_bottom = true;
            }
            _ => {}
        }
        Ok(())
    }

    /// Apply selected `@` file suggestion into the input buffer.
    fn apply_file_completion(&mut self) -> anyhow::Result<()> {
        let Some(menu) = self.state.file_menu.as_ref() else {
            return Ok(());
        };
        if menu.items.is_empty() {
            return Ok(());
        }
        let item = menu.items[menu.selected.min(menu.items.len() - 1)].clone();
        let token_start = menu.token_start;
        let quoted = menu.quoted;
        let cursor = self.state.input.cursor.min(self.state.input.text.len());
        let (replacement, keep_open) = crate::pi::autocomplete::format_at_completion(&item, quoted);
        // For directories ending with `/`, drop trailing space from format helper when keep_open —
        // format_at_completion already omits space for dirs.
        self.state
            .input
            .replace_range(token_start, cursor, &replacement);
        if keep_open {
            self.state.refresh_file_menu();
        } else {
            self.state.file_menu = None;
        }
        Ok(())
    }

    /// Apply selected slash suggestion into the input buffer.
    /// If `submit_if_ready`, run the command immediately when it has no required args.
    async fn apply_slash_completion(&mut self, submit_if_ready: bool) -> anyhow::Result<()> {
        let Some(menu) = self.state.slash_menu.as_ref() else {
            return Ok(());
        };
        if menu.items.is_empty() {
            return Ok(());
        }
        let item = menu.items[menu.selected.min(menu.items.len() - 1)].clone();
        let needs_args = item.argument_hint.is_some();
        let completed = format!("/{} ", item.name);
        self.state.input.set_text(completed);
        self.state.slash_menu = None;

        if submit_if_ready && !needs_args {
            self.submit_input().await?;
        } else if submit_if_ready
            && matches!(
                item.name.as_str(),
                "model" | "sessions" | "resume" | "tasks" | "task" | "permission"
            )
        {
            self.state.input.clear();
            match item.name.as_str() {
                "model" => self.open_model_picker(),
                "sessions" | "resume" => self.open_session_picker().await?,
                "tasks" | "task" => self.open_tasks_panel().await?,
                "permission" => self.open_permission_picker(),
                _ => {}
            }
        } else {
            // Keep menu closed; user can type args. Re-open only for name completion.
            self.state.refresh_slash_menu();
        }
        Ok(())
    }

    async fn open_tasks_panel(&mut self) -> anyhow::Result<()> {
        match self.client.rpc_call("tasks.list", None).await {
            Ok(data) => {
                let mut tasks = Vec::new();
                if let Some(arr) = data.get("tasks").and_then(|v| v.as_array()) {
                    for t in arr {
                        tasks.push(TaskInfo {
                            task_id: t
                                .get("task_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("?")
                                .to_string(),
                            description: t
                                .get("description")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                            status: t
                                .get("status")
                                .map(|v| match v {
                                    serde_json::Value::String(s) => s.clone(),
                                    other => other.to_string().trim_matches('"').to_string(),
                                })
                                .unwrap_or_else(|| "?".into()),
                            result: t.get("result").and_then(|v| v.as_str()).map(String::from),
                            error: t.get("error").and_then(|v| v.as_str()).map(String::from),
                        });
                    }
                }
                let selected = self
                    .state
                    .tasks_panel
                    .as_ref()
                    .map(|p| p.selected.min(tasks.len().saturating_sub(1)))
                    .unwrap_or(0);
                self.state.tasks_panel = Some(TasksPanelState { tasks, selected });
            }
            Err(e) => self.system_message(format!("Failed to list tasks: {}", e)),
        }
        Ok(())
    }

    async fn undo_turns(&mut self, count: usize) -> anyhow::Result<()> {
        if self.state.status != SessionStatus::Idle {
            self.system_message("Cannot undo while streaming — press Esc or Ctrl-C first.".into());
            return Ok(());
        }
        let Some(sid) = self.state.session_id.clone() else {
            self.system_message("No active session.".into());
            return Ok(());
        };
        let params = serde_json::json!({"session_id": sid, "count": count});
        match self.client.rpc_call("session.undo", Some(params)).await {
            Ok(data) => {
                let undone = data.get("undone").and_then(|v| v.as_u64()).unwrap_or(0);
                if let Some(msgs) = data.get("messages").and_then(|v| v.as_array()) {
                    self.state.messages = transcript_messages_to_display(msgs);
                }
                self.state.thinking_text.clear();
                self.state.follow_bottom = true;
                self.state.scroll_up = 0;
                self.system_message(format!(
                    "Undid {} turn(s). File changes restored where possible.",
                    undone
                ));
            }
            Err(e) => self.system_message(format!("Undo failed: {}", e)),
        }
        Ok(())
    }

    async fn apply_list_picker(&mut self) -> anyhow::Result<()> {
        let Some(picker) = self.state.list_picker.take() else {
            return Ok(());
        };
        self.state.session_picker_preview = None;
        self.state.session_delete_confirm = None;
        if picker.items.is_empty() {
            return Ok(());
        }
        let item = picker.items[picker.selected.min(picker.items.len() - 1)].clone();
        match picker.kind {
            ListPickerKind::Model => {
                self.config.default_model = Some(item.id.clone());
                if let Some(sid) = &self.state.session_id {
                    if let Err(e) = self.client.set_model(sid, &item.id).await {
                        self.system_message(format!("Failed to set model on server: {}", e));
                    }
                }
                self.system_message(format!("Model set to: {}", item.id));
            }
            ListPickerKind::Session => {
                self.resume_session(&item.id).await?;
            }
            ListPickerKind::Permission => {
                self.apply_permission_mode_id(&item.id).await?;
            }
        }
        Ok(())
    }

    async fn apply_permission_mode_id(&mut self, id: &str) -> anyhow::Result<()> {
        let new_mode: PermissionMode = id
            .parse()
            .map_err(|e: String| anyhow::anyhow!(e))?;
        if new_mode == self.state.permission_mode {
            self.system_message(format!("Permission mode already: {new_mode}"));
            return Ok(());
        }
        self.state.permission_mode = new_mode;
        if let Some(sid) = &self.state.session_id {
            self.client.set_permission_mode(sid, new_mode).await?;
        }
        self.system_message(format!("Permission mode: {new_mode}"));
        Ok(())
    }

    fn open_permission_picker(&mut self) {
        let current = self.state.permission_mode;
        let modes = [
            (
                PermissionMode::Manual,
                "manual",
                "Ask before tools / writes",
            ),
            (
                PermissionMode::Yolo,
                "yolo",
                "Auto-approve tools (still careful)",
            ),
            (
                PermissionMode::Auto,
                "auto",
                "Fully autonomous — agent decides",
            ),
        ];
        let mut selected = 0;
        let items: Vec<ListPickerItem> = modes
            .into_iter()
            .enumerate()
            .map(|(i, (mode, id, detail))| {
                if mode == current {
                    selected = i;
                }
                let label = if mode == current {
                    format!("{id}  (current)")
                } else {
                    id.to_string()
                };
                ListPickerItem {
                    id: id.to_string(),
                    label,
                    detail: detail.to_string(),
                }
            })
            .collect();
        self.state.list_picker = Some(ListPickerState {
            kind: ListPickerKind::Permission,
            title: " Select permission mode ".into(),
            items,
            selected,
        });
    }

    fn open_model_picker(&mut self) {
        let current = self.config.default_model_alias().unwrap_or("").to_string();
        let mut names: Vec<_> = self.config.models.keys().cloned().collect();
        names.sort();
        let mut selected = 0;
        let items: Vec<ListPickerItem> = names
            .into_iter()
            .enumerate()
            .map(|(i, name)| {
                if name == current {
                    selected = i;
                }
                let detail = self
                    .config
                    .resolve_model(&name)
                    .map(|(m, _)| m.model.clone())
                    .unwrap_or_default();
                ListPickerItem {
                    id: name.clone(),
                    label: name,
                    detail,
                }
            })
            .collect();
        self.state.list_picker = Some(ListPickerState {
            kind: ListPickerKind::Model,
            title: " Select model ".into(),
            items,
            selected,
        });
    }

    async fn open_session_picker(&mut self) -> anyhow::Result<()> {
        let cwd = std::env::current_dir()
            .ok()
            .and_then(|p| std::fs::canonicalize(&p).ok())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let cwd_key = cwd.to_string_lossy().to_string();
        let params = serde_json::json!({"limit": 80});
        match self.client.rpc_call("sessions.list", Some(params)).await {
            Ok(data) => {
                let mut items = Vec::new();
                let current = self.state.session_id.clone();
                if let Some(sessions) = data.get("sessions").and_then(|v| v.as_array()) {
                    for s in sessions {
                        let id = s
                            .get("session_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        if id.is_empty() {
                            continue;
                        }
                        let work = s
                            .get("working_dir")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let same_workspace = if work.is_empty() {
                            false
                        } else {
                            std::fs::canonicalize(work)
                                .map(|p| p.to_string_lossy() == cwd_key)
                                .unwrap_or_else(|_| {
                                    std::path::Path::new(work) == cwd
                                        || work == cwd_key
                                        || work == "."
                                })
                        };
                        if !same_workspace {
                            continue;
                        }
                        let empty = s
                            .get("empty")
                            .and_then(|v| v.as_bool())
                            .unwrap_or_else(|| {
                                s.get("message_count")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0)
                                    == 0
                                    && s.get("last_prompt")
                                        .and_then(|v| v.as_str())
                                        .map(|p| {
                                            p.trim().is_empty()
                                                || kkagent_protocol::is_harness_only_user_text(p)
                                        })
                                        .unwrap_or(true)
                            });
                        // Keep empty sessions only while they are the active page.
                        if empty && current.as_deref() != Some(id.as_str()) {
                            continue;
                        }
                        let title = crate::chrome::session_display_title(
                            s.get("title").and_then(|v| v.as_str()),
                            s.get("is_custom_title")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false),
                            s.get("last_prompt").and_then(|v| v.as_str()),
                            &id,
                        );
                        let short = &id[..8.min(id.len())];
                        let fork = s
                            .get("forked_from")
                            .and_then(|v| v.as_str())
                            .map(|p| format!("fork←{}", &p[..8.min(p.len())]))
                            .unwrap_or_else(|| "session".into());
                        let mark = if current.as_deref() == Some(id.as_str()) {
                            "·current"
                        } else {
                            ""
                        };
                        items.push(ListPickerItem {
                            id: id.clone(),
                            label: format!("{short} — {title}"),
                            detail: format!("{fork}{mark}"),
                        });
                    }
                }
                if items.is_empty() {
                    self.state.list_picker = None;
                    self.state.session_picker_preview = None;
                    self.system_message("No sessions in this workspace.".into());
                } else {
                    let selected = current
                        .as_ref()
                        .and_then(|c| items.iter().position(|i| &i.id == c))
                        .unwrap_or(0);
                    self.state.list_picker = Some(ListPickerState {
                        kind: ListPickerKind::Session,
                        title: " Sessions (this workspace)  ↑↓  Enter open  Ctrl-D delete ".into(),
                        items,
                        selected,
                    });
                    self.state.session_delete_confirm = None;
                    self.refresh_session_picker_preview().await?;
                }
            }
            Err(e) => self.system_message(format!("Failed to list sessions: {}", e)),
        }
        Ok(())
    }

    async fn resume_session(&mut self, query: &str) -> anyhow::Result<()> {
        let leaving_id = self.state.session_id.clone();
        let leaving_empty = leaving_id
            .as_ref()
            .map(|id| id != query && !session_has_retained_io(&self.state.messages))
            .unwrap_or(false);

        // Park interactive prompts for the session we leave so background turns can resume them.
        if let Some(ref leaving) = leaving_id {
            if leaving != query {
                if let Some(approval) = self.state.approval_pending.take() {
                    self.state
                        .parked_approvals
                        .insert(leaving.clone(), approval);
                }
                if let Some(question) = self.state.question_pending.take() {
                    self.state
                        .parked_questions
                        .insert(leaving.clone(), question);
                }
            }
        }

        let params = serde_json::json!({"session_id": query});
        let data = self
            .client
            .rpc_call("session.resume", Some(params))
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;

        let sid = data
            .get("session_id")
            .and_then(|v| v.as_str())
            .unwrap_or(query)
            .to_string();
        self.state.session_id = Some(sid.clone());
        self.state.messages.clear();
        self.state.todos.clear();
        self.state.todos_expanded = false;
        self.state.thinking_text.clear();
        self.state.scroll_up = 0;
        self.state.follow_bottom = true;
        self.state.status = SessionStatus::Idle;
        self.state.approval_pending = self.state.parked_approvals.remove(&sid);
        self.state.question_pending = self.state.parked_questions.remove(&sid);
        if self.state.approval_pending.is_some() {
            self.state.status = SessionStatus::WaitingApproval;
        } else if self.state.question_pending.is_some() {
            self.state.status = SessionStatus::WaitingQuestion;
        }

        if let Some(msgs) = data.get("messages").and_then(|v| v.as_array()) {
            self.state.messages = transcript_messages_to_display(msgs);
        }

        if let Some(model) = data.get("model").and_then(|v| v.as_str()) {
            if !model.is_empty() {
                self.config.default_model = Some(model.to_string());
            }
        }
        if let Some(plan) = data.get("plan_mode").and_then(|v| v.as_bool()) {
            self.state.on_plan_mode_changed(plan);
        }
        // Restore plan document from transcript so plan-focus scroll works after resume.
        if let Some(plan_msg) = self
            .state
            .messages
            .iter()
            .rev()
            .find(|m| m.role == MessageRole::Plan)
        {
            let (path, body) = split_plan_message_content(&plan_msg.content);
            if !body.trim().is_empty() {
                self.state.plan_document = Some(PlanDocument {
                    path,
                    content: body,
                });
                if self.state.plan_mode {
                    self.state.plan_scroll_to_top = true;
                    self.state.follow_bottom = false;
                }
            }
        }

        // Drop the session we left if it never had any real I/O.
        if leaving_empty {
            if let Some(prev) = leaving_id {
                if prev != sid {
                    let _ = self.discard_session_record(&prev).await;
                    self.state.open_session_group.retain(|id| id != &prev);
                }
            }
        }

        // Cold resume of an unrelated session: drop ephemeral window group.
        if !self
            .state
            .open_session_group
            .iter()
            .any(|id| id == &sid)
        {
            self.state.open_session_group.clear();
        }

        self.system_message(format!(
            "Resumed session {} ({} bubbles).",
            &sid[..8.min(sid.len())],
            self.state.messages.len()
        ));
        let _ = self.refresh_workspace_sessions().await;
        Ok(())
    }

    /// True when the transcript has user/assistant/plan content worth keeping.
    fn current_session_has_io(&self) -> bool {
        session_has_retained_io(&self.state.messages)
    }

    async fn discard_session_record(&mut self, session_id: &str) -> anyhow::Result<()> {
        let params = serde_json::json!({"session_id": session_id});
        let _ = self.client.rpc_call("sessions.delete", Some(params)).await;
        self.state.tab_strip.tabs.retain(|t| t.id != session_id);
        if self.state.tab_strip.active >= self.state.tab_strip.tabs.len() {
            self.state.tab_strip.active = self.state.tab_strip.tabs.len().saturating_sub(1);
        }
        Ok(())
    }

    async fn refresh_workspace_sessions(&mut self) -> anyhow::Result<()> {
        let Some(current_id) = self.state.session_id.clone() else {
            self.state.workspace_sessions.set_entries(Vec::new(), None);
            return Ok(());
        };
        let params = serde_json::json!({"limit": 80, "include_archived": false});
        let data = match self.client.rpc_call("sessions.list", Some(params)).await {
            Ok(v) => v,
            Err(_) => {
                self.state.workspace_sessions.set_entries(Vec::new(), None);
                return Ok(());
            }
        };

        let mut rows: Vec<(String, Option<String>, String)> = Vec::new();
        if let Some(sessions) = data.get("sessions").and_then(|v| v.as_array()) {
            for s in sessions {
                let id = s
                    .get("session_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if id.is_empty() {
                    continue;
                }
                let empty = s.get("empty").and_then(|v| v.as_bool()).unwrap_or(false);
                let in_group = self.state.open_session_group.iter().any(|g| g == &id);
                if empty && id != current_id && !in_group {
                    continue;
                }
                let forked_from = s
                    .get("forked_from")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let title = crate::chrome::session_display_title(
                    s.get("title").and_then(|v| v.as_str()),
                    s.get("is_custom_title")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                    s.get("last_prompt").and_then(|v| v.as_str()),
                    &id,
                );
                rows.push((id, forked_from, title));
            }
        }

        // Ensure current session is in the graph even if list is momentarily stale.
        if !rows.iter().any(|(id, _, _)| id == &current_id) {
            let title = self
                .state
                .messages
                .iter()
                .find_map(|m| {
                    if m.role != MessageRole::User {
                        return None;
                    }
                    if kkagent_protocol::is_harness_only_user_text(&m.content) {
                        return None;
                    }
                    let visible = kkagent_protocol::visible_user_text(&m.content);
                    let pick = if visible.is_empty() {
                        m.content.trim()
                    } else {
                        visible.as_str()
                    };
                    let snippet: String = pick.chars().take(40).collect();
                    if snippet.trim().is_empty() {
                        None
                    } else {
                        Some(snippet)
                    }
                })
                .unwrap_or_else(|| {
                    if current_id.len() > 8 {
                        current_id[..8].to_string()
                    } else {
                        current_id.clone()
                    }
                });
            rows.push((current_id.clone(), None, title));
        }

        let parent_rows: Vec<(String, Option<String>)> = rows
            .iter()
            .map(|(id, parent, _)| (id.clone(), parent.clone()))
            .collect();
        let family_ids = crate::chrome::fork_family_ids(&parent_rows, &current_id);

        // Ephemeral window group (`/new` siblings) — not persisted across process restarts.
        self.state.open_session_group.retain(|id| {
            id == &current_id || rows.iter().any(|(rid, _, _)| rid == id)
        });
        let mut ids: Vec<String> = if family_ids.is_empty() {
            Vec::new()
        } else {
            family_ids
        };
        if self
            .state
            .open_session_group
            .iter()
            .any(|id| id == &current_id)
            && self.state.open_session_group.len() >= 2
        {
            for id in &self.state.open_session_group {
                if !ids.contains(id) {
                    ids.push(id.clone());
                }
            }
        }

        let entries: Vec<crate::chrome::WorkspaceSessionEntry> = if ids.len() < 2 {
            Vec::new()
        } else {
            ids.into_iter()
                .filter_map(|id| {
                    if let Some((_, _, title)) = rows.iter().find(|(rid, _, _)| rid == &id) {
                        Some(crate::chrome::WorkspaceSessionEntry {
                            id,
                            title: title.clone(),
                        })
                    } else if id == current_id {
                        Some(crate::chrome::WorkspaceSessionEntry {
                            id,
                            title: "session".into(),
                        })
                    } else {
                        None
                    }
                })
                .collect()
        };

        self.state
            .workspace_sessions
            .set_entries(entries, Some(current_id.as_str()));

        for e in &self.state.workspace_sessions.entries {
            self.state.tab_strip.ensure_tab(&e.id, e.title.clone());
        }
        let title = self
            .state
            .workspace_sessions
            .entries
            .iter()
            .find(|e| e.id == current_id)
            .map(|e| e.title.clone())
            .or_else(|| {
                rows.iter()
                    .find(|(id, _, _)| id == &current_id)
                    .map(|(_, _, t)| t.clone())
            })
            .unwrap_or_else(|| "main".into());
        self.state.tab_strip.ensure_active(&current_id, title);
        Ok(())
    }

    fn link_open_sessions(&mut self, a: &str, b: &str) {
        for id in [a, b] {
            if !self.state.open_session_group.iter().any(|x| x == id) {
                self.state.open_session_group.push(id.to_string());
            }
        }
    }

    fn can_cycle_fork_sessions(&self) -> bool {
        self.state.input.is_empty()
            && self.state.mode == AppMode::Normal
            && self.state.slash_menu.is_none()
            && self.state.file_menu.is_none()
            && self.state.list_picker.is_none()
            && self.state.tasks_panel.is_none()
            && !self.state.search.active
            && self.state.approval_pending.is_none()
            && self.state.question_pending.is_none()
            && self.state.session_delete_confirm.is_none()
            && self.state.workspace_sessions.entries.len() >= 2
    }

    async fn cycle_workspace_session(&mut self, direction: i8) -> anyhow::Result<()> {
        self.refresh_workspace_sessions().await?;
        if self.state.workspace_sessions.entries.len() < 2 {
            return Ok(());
        }
        let next_id = if direction < 0 {
            self.state.workspace_sessions.prev_id()
        } else {
            self.state.workspace_sessions.next_id()
        };
        let Some(next_id) = next_id else {
            return Ok(());
        };
        if self.state.session_id.as_deref() == Some(next_id.as_str()) {
            return Ok(());
        }
        self.resume_session(&next_id).await
    }

    async fn refresh_session_picker_preview(&mut self) -> anyhow::Result<()> {
        let Some(picker) = self.state.list_picker.as_ref() else {
            self.state.session_picker_preview = None;
            return Ok(());
        };
        if picker.kind != ListPickerKind::Session {
            self.state.session_picker_preview = None;
            return Ok(());
        }
        let Some(item) = picker.items.get(picker.selected) else {
            self.state.session_picker_preview = None;
            return Ok(());
        };
        let sid = item.id.clone();

        // Current session: reuse live transcript (same look as normal chat).
        if self.state.session_id.as_deref() == Some(sid.as_str()) {
            self.state.session_picker_preview = Some(SessionPickerPreview {
                session_id: sid,
                messages: self.state.messages.clone(),
            });
            self.state.follow_bottom = true;
            self.state.scroll_up = 0;
            return Ok(());
        }

        let params = serde_json::json!({"session_id": sid, "limit": 80});
        match self.client.rpc_call("session.preview", Some(params)).await {
            Ok(data) => {
                let mut messages = Vec::new();
                if let Some(msgs) = data.get("messages").and_then(|v| v.as_array()) {
                    for m in msgs {
                        let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("");
                        let text = m.get("text").and_then(|v| v.as_str()).unwrap_or("");
                        if text.trim().is_empty() {
                            continue;
                        }
                        let role = match role {
                            "user" => MessageRole::User,
                            "assistant" => MessageRole::Assistant,
                            "system" => MessageRole::System,
                            _ => continue,
                        };
                        let content = if role == MessageRole::User {
                            if kkagent_protocol::is_harness_only_user_text(text) {
                                continue;
                            }
                            let visible = kkagent_protocol::visible_user_text(text);
                            if visible.is_empty() {
                                text.to_string()
                            } else {
                                visible
                            }
                        } else {
                            text.to_string()
                        };
                        if content.trim().is_empty() {
                            continue;
                        }
                        messages.push(DisplayMessage {
                            role,
                            content,
                            thinking: None,
                            parts: Vec::new(),
                            tool_calls: Vec::new(),
                        });
                    }
                }
                self.state.session_picker_preview = Some(SessionPickerPreview {
                    session_id: sid,
                    messages,
                });
                self.state.follow_bottom = true;
                self.state.scroll_up = 0;
            }
            Err(e) => {
                self.state.session_picker_preview = Some(SessionPickerPreview {
                    session_id: sid,
                    messages: vec![DisplayMessage {
                        role: MessageRole::System,
                        content: format!("Failed to load session history: {e}"),
                        thinking: None,
                        parts: Vec::new(),
                        tool_calls: Vec::new(),
                    }],
                });
            }
        }
        Ok(())
    }

    async fn confirm_delete_session(&mut self, yes: bool) -> anyhow::Result<()> {
        let Some(confirm) = self.state.session_delete_confirm.take() else {
            return Ok(());
        };
        if !yes {
            return Ok(());
        }
        let deleted_id = confirm.session_id.clone();
        let params = serde_json::json!({"session_id": deleted_id});
        match self.client.rpc_call("sessions.delete", Some(params)).await {
            Ok(_) => {
                self.system_message(format!("Deleted session {}", &confirm.session_id[..8.min(confirm.session_id.len())]));
                let was_current = self.state.session_id.as_deref() == Some(deleted_id.as_str());
                if was_current {
                    // Prefer another fork-family member; otherwise start fresh.
                    self.refresh_workspace_sessions().await?;
                    let fallback = self
                        .state
                        .workspace_sessions
                        .entries
                        .iter()
                        .map(|e| e.id.clone())
                        .find(|id| id != &deleted_id);
                    if let Some(id) = fallback {
                        self.resume_session(&id).await?;
                    } else {
                        let cwd = std::env::current_dir()?.to_string_lossy().to_string();
                        let session_id = self
                            .client
                            .create_session(Some(&cwd), Some(self.state.permission_mode))
                            .await?;
                        self.state.session_id = Some(session_id.clone());
                        self.state.status_bar.session_id = Some(session_id.clone());
                        self.state.tab_strip.ensure_active(&session_id, "main");
                        self.state.messages.clear();
                        self.system_message("Current session was deleted; started a new one.".into());
                    }
                }
                self.open_session_picker().await?;
                let _ = self.refresh_workspace_sessions().await;
            }
            Err(e) => self.system_message(format!("Failed to delete session: {e}")),
        }
        Ok(())
    }

    async fn submit_input(&mut self) -> anyhow::Result<()> {
        let raw = self.state.input.take();
        if raw.is_empty() {
            return Ok(());
        }
        // Expand kimi-style `[Pasted text #n]` markers before send / display.
        let text = self.state.input.expand_pastes(&raw);
        self.state.slash_menu = None;
        self.state.file_menu = None;
        self.state.list_picker = None;

        // Handle slash commands
        if text.starts_with('/') {
            return self.handle_slash_command(&text).await;
        }

        // Shell mode: prepend ! for the server
        let prompt_text = if self.state.mode == AppMode::Shell {
            self.state.mode = AppMode::Normal;
            format!("!{}", text)
        } else {
            text.clone()
        };

        self.state.push_input_history(&text);

        // Add user message to display
        self.state.messages.push(DisplayMessage {
            role: MessageRole::User,
            content: text,
            thinking: None,
            parts: Vec::new(),
            tool_calls: Vec::new(),
        });
        self.state.status = SessionStatus::Thinking;
        self.state.thinking_text.clear();
        self.state.scroll_up = 0;
        self.state.follow_bottom = true;

        // Send to server — show errors in UI instead of crashing the TUI
        if let Some(sid) = &self.state.session_id {
            if let Err(e) = self.client.send_prompt(sid, &prompt_text).await {
                self.state.status = SessionStatus::Idle;
                self.system_message(format!("Error: {}", e));
            } else {
                // Update footer title from first/latest prompt.
                let _ = self.refresh_workspace_sessions().await;
            }
        } else {
            self.state.status = SessionStatus::Idle;
            self.system_message("No active session.".into());
        }

        Ok(())
    }

    async fn handle_slash_command(&mut self, cmd: &str) -> anyhow::Result<()> {
        self.state.slash_menu = None;

        let Some((command, args)) = parse_slash_input(cmd) else {
            return Ok(());
        };

        if command.is_empty() {
            // Bare `/` — open full menu in input
            self.state.input.set_text("/".into());
            self.state.refresh_slash_menu();
            return Ok(());
        }

        // Resolve aliases via registry
        let resolved = find_slash_command(&command)
            .map(|c| c.name)
            .unwrap_or(command.as_str());

        match resolved {
            "yolo" | "yes" => {
                let new_mode = if self.state.permission_mode == PermissionMode::Yolo {
                    PermissionMode::Manual
                } else {
                    PermissionMode::Yolo
                };
                self.state.permission_mode = new_mode;
                if let Some(sid) = &self.state.session_id {
                    self.client.set_permission_mode(sid, new_mode).await?;
                }
                self.system_message(format!("Permission mode: {}", new_mode));
            }
            "auto" => {
                let new_mode = if self.state.permission_mode == PermissionMode::Auto {
                    PermissionMode::Manual
                } else {
                    PermissionMode::Auto
                };
                self.state.permission_mode = new_mode;
                if let Some(sid) = &self.state.session_id {
                    self.client.set_permission_mode(sid, new_mode).await?;
                }
                self.system_message(format!("Permission mode: {}", new_mode));
            }
            "permission" => {
                self.open_permission_picker();
            }
            "plan" => {
                if args.eq_ignore_ascii_case("clear") {
                    self.state.on_plan_mode_changed(false);
                    self.state.plan_document = None;
                    self.state.messages.retain(|m| m.role != MessageRole::Plan);
                } else {
                    self.state.on_plan_mode_changed(!self.state.plan_mode);
                }
                if let Some(sid) = &self.state.session_id {
                    self.client.set_plan_mode(sid, self.state.plan_mode).await?;
                }
                if self.state.plan_mode {
                    self.system_message(
                        "Plan mode ON — explore & write plan only. \
                         Source edits are denied until ExitPlanMode. \
                         Scroll locks to the full plan after it is written."
                            .into(),
                    );
                } else {
                    self.system_message("Plan mode OFF.".into());
                }
            }
            "exit" | "quit" | "q" => {
                self.state.should_quit = true;
            }
            "new" | "clear" => {
                let prev = self.state.session_id.clone();
                let prev_busy = matches!(
                    self.state.status,
                    SessionStatus::Thinking
                        | SessionStatus::ToolExecuting
                        | SessionStatus::WaitingApproval
                        | SessionStatus::WaitingQuestion
                );
                let prev_empty = !self.current_session_has_io();
                // Keep the previous turn running in the background — do not interrupt.
                if let Some(ref leaving) = prev {
                    if let Some(approval) = self.state.approval_pending.take() {
                        self.state
                            .parked_approvals
                            .insert(leaving.clone(), approval);
                    }
                    if let Some(question) = self.state.question_pending.take() {
                        self.state
                            .parked_questions
                            .insert(leaving.clone(), question);
                    }
                }
                self.state.messages.clear();
                self.state.todos.clear();
                self.state.todos_expanded = false;
                self.state.thinking_text.clear();
                self.state.plan_document = None;
                self.state.plan_scroll_to_top = false;
                self.state.status = SessionStatus::Idle;
                self.state.turn_started_at = None;
                let cwd = std::env::current_dir()?.to_string_lossy().to_string();
                let session_id = self
                    .client
                    .create_session(Some(&cwd), Some(self.state.permission_mode))
                    .await?;
                if self.state.plan_mode {
                    let _ = self.client.set_plan_mode(&session_id, true).await;
                }
                let keep_prev = prev
                    .as_ref()
                    .is_some_and(|p| p != &session_id && (prev_busy || !prev_empty));
                if keep_prev {
                    if let Some(ref p) = prev {
                        self.link_open_sessions(p, &session_id);
                        self.state.tab_strip.ensure_tab(p, "background");
                    }
                    self.state.tab_strip.ensure_active(&session_id, "new");
                    self.system_message(
                        "New session started. Previous session keeps running — Tab / ←→ to switch."
                            .into(),
                    );
                } else {
                    if prev_empty {
                        if let Some(prev) = prev {
                            if prev != session_id {
                                let _ = self.discard_session_record(&prev).await;
                            }
                        }
                    }
                    self.state.tab_strip.ensure_active(&session_id, "main");
                    self.system_message("New session started.".into());
                }
                self.state.status_bar.session_id = Some(session_id.clone());
                self.state.session_id = Some(session_id);
                let _ = self.refresh_workspace_sessions().await;
            }
            "sessions" | "resume" => {
                if args.is_empty() {
                    self.open_session_picker().await?;
                } else if let Err(e) = self.resume_session(&args).await {
                    self.system_message(format!("Failed to resume: {}", e));
                }
            }
            "compact" => {
                if let Some(sid) = &self.state.session_id {
                    let params = serde_json::json!({"session_id": sid, "instruction": args});
                    match self.client.rpc_call("session.compact", Some(params)).await {
                        Ok(data) => {
                            let deleted = data.get("deleted").and_then(|v| v.as_u64()).unwrap_or(0);
                            self.system_message(format!("Compacted: {} messages removed", deleted));
                        }
                        Err(e) => self.system_message(format!("Failed to compact: {}", e)),
                    }
                }
            }
            "undo" => {
                let count = args.trim().parse::<usize>().unwrap_or(1).max(1);
                self.undo_turns(count).await?;
            }
            "model" => {
                if args.is_empty() {
                    self.open_model_picker();
                } else if self.config.resolve_model(&args).is_some() {
                    self.config.default_model = Some(args.clone());
                    if let Some(sid) = &self.state.session_id {
                        if let Err(e) = self.client.set_model(sid, &args).await {
                            self.system_message(format!("Failed to set model on server: {}", e));
                        }
                    }
                    self.system_message(format!("Model set to: {}", args));
                } else {
                    self.system_message(format!(
                        "Unknown model '{}'. Use /model to pick from the list.",
                        args
                    ));
                }
            }
            "effort" | "thinking" => {
                if args.is_empty() {
                    let label = self
                        .config
                        .thinking
                        .as_ref()
                        .map(|t| {
                            if t.enabled {
                                format!("on ({})", t.effort.as_deref().unwrap_or("high"))
                            } else {
                                "off".into()
                            }
                        })
                        .unwrap_or_else(|| "off".into());
                    self.system_message(format!(
                        "Thinking: {}\nUsage: /effort [off|low|medium|high]",
                        label
                    ));
                } else {
                    let effort = args.to_lowercase();
                    match effort.as_str() {
                        "off" => {
                            if let Some(ref mut t) = self.config.thinking {
                                t.enabled = false;
                            } else {
                                self.config.thinking = Some(kkagent_config::ThinkingConfig {
                                    enabled: false,
                                    effort: None,
                                    keep: None,
                                });
                            }
                            self.system_message("Thinking: off".into());
                        }
                        "low" | "medium" | "high" | "on" => {
                            let e = if effort == "on" {
                                "high".to_string()
                            } else {
                                effort
                            };
                            self.config.thinking = Some(kkagent_config::ThinkingConfig {
                                enabled: true,
                                effort: Some(e.clone()),
                                keep: None,
                            });
                            self.system_message(format!("Thinking: on ({})", e));
                        }
                        _ => self.system_message("Usage: /effort [off|low|medium|high]".into()),
                    }
                }
            }
            "goal" => {
                let sub = args.split_whitespace().next().unwrap_or("");
                match sub {
                    "" | "status" => {
                        self.system_message(
                            "Goal: (client-side status — use CreateGoal tool in chat for now)"
                                .into(),
                        );
                    }
                    "pause" | "resume" | "cancel" => {
                        self.system_message(format!(
                            "Goal {} — send via agent tools (UpdateGoal).",
                            sub
                        ));
                    }
                    _ => {
                        // Treat as objective — queue a create-goal prompt
                        self.state.pending_prompt = Some(format!(
                            "Create a goal with this objective and start working on it:\n\n{}",
                            args
                        ));
                    }
                }
            }
            "status" => {
                let model = self.config.default_model_alias().unwrap_or("?");
                let sid = self.state.session_id.as_deref().unwrap_or("-");
                self.system_message(format!(
                    "session: {}\nmodel: {}\npermission: {}\nplan: {}\nstatus: {:?}\nmessages: {}\ntokens≈ {}",
                    &sid[..8.min(sid.len())],
                    model,
                    self.state.permission_mode,
                    if self.state.plan_mode { "on" } else { "off" },
                    self.state.status,
                    self.state.messages.len(),
                    self.state.approx_tokens,
                ));
            }
            "usage" => {
                let max = self
                    .config
                    .default_model_alias()
                    .and_then(|a| self.config.resolve_model(a))
                    .and_then(|(m, _)| m.max_context_size)
                    .unwrap_or(256_000);
                let used = self.state.approx_tokens;
                let pct = used
                    .saturating_mul(100)
                    .checked_div(max)
                    .unwrap_or(0)
                    .min(100);
                self.system_message(format!(
                    "context: {}% ({}/{})\napprox tokens used: {}",
                    pct, used, max, used
                ));
            }
            "mcp" => {
                let n = self.config.mcp_servers.len();
                let mut lines = format!("MCP servers configured: {}\n", n);
                for (name, cfg) in &self.config.mcp_servers {
                    let kind = cfg
                        .transport_type
                        .as_deref()
                        .unwrap_or(if cfg.url.is_some() { "http" } else { "stdio" });
                    let detail = if let Some(url) = &cfg.url {
                        url.clone()
                    } else {
                        format!("{} {:?}", cfg.command.as_deref().unwrap_or("?"), cfg.args)
                    };
                    let oauth = if cfg.oauth.is_some() { " oauth" } else { "" };
                    lines.push_str(&format!("  {name} [{kind}{oauth}] — {detail}\n"));
                }
                if n == 0 {
                    lines.push_str("  (none — add [mcp_servers.*] in config.toml)");
                }
                self.system_message(lines);
            }
            "tasks" | "task" => {
                self.open_tasks_panel().await?;
            }
            "init" => {
                self.state.pending_prompt = Some(
                    "Analyze this codebase and create or update AGENTS.md with project conventions, build/test commands, and important paths.".into(),
                );
            }
            "title" | "rename" => {
                if args.is_empty() {
                    let title = self
                        .state
                        .messages
                        .iter()
                        .find(|m| m.role == MessageRole::User)
                        .map(|m| m.content.chars().take(40).collect::<String>())
                        .unwrap_or_else(|| "(untitled)".into());
                    self.system_message(format!(
                        "Current title hint: {}\nUsage: /title <name>",
                        title
                    ));
                } else if let Some(sid) = &self.state.session_id {
                    let params = serde_json::json!({"session_id": sid, "title": args});
                    match self
                        .client
                        .rpc_call("session.set_title", Some(params))
                        .await
                    {
                        Ok(_) => {
                            self.system_message(format!("Session title set to: {}", args));
                            let _ = self.refresh_workspace_sessions().await;
                        }
                        Err(e) => self.system_message(format!("Failed to set title: {}", e)),
                    }
                }
            }
            "config" => {
                let models = self.config.models.len();
                let providers = self.config.providers.len();
                let mcp = self.config.mcp_servers.len();
                self.system_message(format!(
                    "config: providers={} models={} mcp={} default_model={} secondary={:?} trusted={}",
                    providers,
                    models,
                    mcp,
                    self.config.default_model.as_deref().unwrap_or("-"),
                    self.config.secondary_model,
                    self.config.trusted_workspaces.len()
                ));
            }
            "auth" => {
                let mut lines = String::from("Auth status (secrets redacted):\n");
                for (name, p) in &self.config.providers {
                    let has_key = p.api_key.as_ref().map(|k| !k.is_empty()).unwrap_or(false);
                    lines.push_str(&format!(
                        "  {name}: type={} api_key={}\n",
                        p.provider_type,
                        if has_key { "set" } else { "missing" }
                    ));
                }
                self.system_message(lines);
            }
            "plugins" | "plugin" => match self.client.rpc_call("plugins.list", None).await {
                Ok(v) => self.system_message(format!("plugins: {}", v)),
                Err(_) => self.system_message(
                    "plugins: check ~/.kkagent/plugins/*/plugin.json (RPC list optional)".into(),
                ),
            },
            "skills" | "skill" => match self.client.rpc_call("skills.list", None).await {
                Ok(v) => self.system_message(format!("{v}")),
                Err(e) => self.system_message(format!("skills: {e}")),
            },
            "swarm" => match args.as_str() {
                "enter" | "on" => {
                    let mut params = serde_json::json!({ "trigger": "slash" });
                    if let Some(sid) = &self.state.session_id {
                        params["session_id"] = serde_json::json!(sid);
                    }
                    match self.client.rpc_call("swarm.enter", Some(params)).await {
                        Ok(v) => self.system_message(format!("swarm enter: {v}")),
                        Err(e) => self.system_message(format!("swarm enter failed: {e}")),
                    }
                }
                "exit" | "off" => {
                    let params = self
                        .state
                        .session_id
                        .as_ref()
                        .map(|sid| serde_json::json!({ "session_id": sid }));
                    match self.client.rpc_call("swarm.exit", params).await {
                        Ok(v) => self.system_message(format!("swarm exit: {v}")),
                        Err(e) => self.system_message(format!("swarm exit failed: {e}")),
                    }
                }
                _ => {
                    self.open_tasks_panel().await?;
                    self.system_message(
                        "swarm: panel open. Use /swarm enter|exit to toggle mode.".into(),
                    );
                }
            },
            "provider" | "providers" => {
                let mut lines = String::from("Providers / models:\n");
                for (alias, m) in &self.config.models {
                    lines.push_str(&format!(
                        "  {alias} → {} ({}) ctx={:?}\n",
                        m.model, m.provider, m.max_context_size
                    ));
                }
                self.system_message(lines);
            }
            "reload" => {
                self.system_message(
                    "reload: restart kkagent to pick up config.toml changes (hot-reload next)."
                        .into(),
                );
            }
            "web" => {
                if args.is_empty() {
                    self.system_message("Usage: /web <query>".into());
                } else {
                    self.state.input.set_text(format!(
                        "Search the web for: {args}\nUse WebSearch then summarize."
                    ));
                    self.system_message("Queued web search prompt in input — press Enter.".into());
                }
            }
            "info" => {
                let home = dirs::home_dir()
                    .unwrap_or_else(|| std::path::PathBuf::from("."))
                    .join(".kkagent");
                self.system_message(format!(
                    "kkagent {}\nconfig_dir: {}\nsession: {}\nmessages: {}\napprox_tokens: {}\nmodel: {}",
                    env!("CARGO_PKG_VERSION"),
                    home.display(),
                    self.state.session_id.as_deref().unwrap_or("-"),
                    self.state.messages.len(),
                    self.state.approx_tokens,
                    self.state.model_alias.as_deref().unwrap_or("-"),
                ));
            }
            "add-dir" | "add_dir" => {
                if args.is_empty() {
                    self.system_message("Usage: /add-dir <path>".into());
                } else {
                    let path = std::path::PathBuf::from(args);
                    let abs = if path.is_absolute() {
                        path
                    } else {
                        std::env::current_dir()
                            .unwrap_or_else(|_| std::path::PathBuf::from("."))
                            .join(path)
                    };
                    if abs.is_dir() {
                        self.system_message(format!(
                            "Noted extra directory: {}\n\
                             Tip: also add under [permissions].trusted_workspaces in config.toml for persistence.",
                            abs.display()
                        ));
                    } else {
                        self.system_message(format!("Not a directory: {}", abs.display()));
                    }
                }
            }
            "btw" => {
                if args.is_empty() {
                    self.state.btw.open = true;
                    self.system_message("Usage: /btw <question> — side Q&A (does not alter main chat)".into());
                } else if self.state.btw.streaming {
                    self.system_message(
                        "Wait for /btw to finish before sending another question.".into(),
                    );
                } else if let Some(sid) = self.state.session_id.clone() {
                    self.state.btw.begin_question(&args);
                    match self.client.start_btw(&sid, &args).await {
                        Ok(_) => {}
                        Err(e) => {
                            self.state.btw.streaming = false;
                            self.state.btw.error = Some(e.to_string());
                            self.system_message(format!("Failed to start /btw: {e}"));
                        }
                    }
                } else {
                    self.system_message("No active session for /btw".into());
                }
            }
            "fork" => {
                if let Some(sid) = self.state.session_id.clone() {
                    let title = if args.is_empty() { None } else { Some(args) };
                    match self.client.fork_session(&sid, title.as_deref()).await {
                        Ok(data) => {
                            let fork_id = data
                                .get("session_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("?")
                                .to_string();
                            if fork_id != "?" {
                                self.link_open_sessions(&sid, &fork_id);
                                self.state.tab_strip.ensure_tab(&fork_id, "fork");
                            }
                            self.system_message(format!(
                                "Session forked ({fork_id}). Still in the original session; switch to the fork via Tab or /sessions."
                            ));
                            let _ = self.refresh_workspace_sessions().await;
                        }
                        Err(e) => self.system_message(format!("Failed to fork session: {e}")),
                    }
                } else {
                    self.system_message("No active session to fork".into());
                }
            }
            "search" | "find" => {
                self.state.search.open();
                if !args.is_empty() {
                    self.state.search.query = args.to_string();
                    let msgs = self.state.messages.clone();
                    self.state.search.recompute(&msgs);
                }
                self.state.slash_menu = None;
                self.state.list_picker = None;
            }
            "prompts" | "prompt" => {
                self.system_message(
                    "prompts:\n  /init — generate AGENTS.md\n  /compact — compress context\n  /goal — autonomous goal\n  /web — web search"
                        .into(),
                );
            }
            "experimental-flags" | "flags" => {
                self.system_message(format!(
                    "flags:\n  KKAGENT_GIT_WORKTREE={}\n  KKAGENT_TELEMETRY_CLOUD={}\n  auto_compact={:?}",
                    std::env::var("KKAGENT_GIT_WORKTREE").unwrap_or_else(|_| "0".into()),
                    std::env::var("KKAGENT_TELEMETRY_CLOUD").unwrap_or_else(|_| "0".into()),
                    self.config
                        .loop_control
                        .as_ref()
                        .map(|l| l.auto_compact)
                ));
            }
            "copy" => {
                let text = self
                    .state
                    .messages
                    .iter()
                    .rev()
                    .find(|m| m.role == MessageRole::Assistant && !m.content.is_empty())
                    .map(|m| m.content.clone())
                    .unwrap_or_default();
                if text.is_empty() {
                    self.system_message("No assistant message to copy.".into());
                } else {
                    match copy_to_clipboard(&text) {
                        Ok(()) => self.system_message(format!(
                            "Copied last assistant message ({} chars) to clipboard.",
                            text.len()
                        )),
                        Err(e) => self.system_message(format!(
                            "Clipboard failed: {}. Message length: {} chars.",
                            e,
                            text.len()
                        )),
                    }
                }
            }
            "export-md" | "export" => {
                let path = dirs::home_dir()
                    .unwrap_or_else(|| std::path::PathBuf::from("."))
                    .join(".kkagent")
                    .join(format!(
                        "export-{}.md",
                        self.state
                            .session_id
                            .as_deref()
                            .unwrap_or("session")
                            .chars()
                            .take(8)
                            .collect::<String>()
                    ));
                let mut md = String::from("# kkagent session export\n\n");
                for msg in &self.state.messages {
                    let role = match msg.role {
                        MessageRole::User => "User",
                        MessageRole::Assistant => "Assistant",
                        MessageRole::System => "System",
                        MessageRole::Plan => "Plan",
                    };
                    md.push_str(&format!("## {}\n\n{}\n\n", role, msg.content));
                }
                match std::fs::write(&path, md) {
                    Ok(()) => self.system_message(format!("Exported to {}", path.display())),
                    Err(e) => self.system_message(format!("Export failed: {}", e)),
                }
            }
            "version" => {
                self.system_message(format!("kkagent {}", env!("CARGO_PKG_VERSION")));
            }
            "help" | "h" | "?" => {
                self.system_message(slash_help_text());
            }
            _ => {
                if find_slash_command(&command).is_some() {
                    self.system_message(format!(
                        "/{} is recognized but not fully wired yet.",
                        command
                    ));
                } else {
                    self.system_message(format!(
                        "Unknown command: /{}. Type / for suggestions or /help.",
                        command
                    ));
                }
            }
        }
        Ok(())
    }

    fn handle_search_key(&mut self, key: KeyEvent) -> anyhow::Result<()> {
        match key.code {
            KeyCode::Esc => {
                self.state.search.close();
                self.state.highlight_message = None;
            }
            KeyCode::Enter => {
                if let Some(hit) = self.state.search.current().cloned() {
                    self.state.highlight_message = Some(hit.message_index);
                    self.state.follow_bottom = false;
                    // Jump roughly toward the selected message.
                    let n = self.state.messages.len().max(1);
                    let from_end = n.saturating_sub(hit.message_index + 1);
                    let approx_lines = (from_end as u16).saturating_mul(6);
                    self.state.scroll_up = approx_lines.min(self.state.max_scroll_up());
                    self.state.search.close();
                }
            }
            KeyCode::Up | KeyCode::BackTab => {
                self.state.search.prev();
                if let Some(hit) = self.state.search.current() {
                    self.state.highlight_message = Some(hit.message_index);
                }
            }
            KeyCode::Down | KeyCode::Tab => {
                self.state.search.next();
                if let Some(hit) = self.state.search.current() {
                    self.state.highlight_message = Some(hit.message_index);
                }
            }
            KeyCode::Backspace => {
                self.state.search.query.pop();
                let msgs = self.state.messages.clone();
                self.state.search.recompute(&msgs);
            }
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.state.search.query.push(c);
                let msgs = self.state.messages.clone();
                self.state.search.recompute(&msgs);
            }
            _ => {}
        }
        Ok(())
    }

    fn system_message(&mut self, content: String) {
        self.state.messages.push(DisplayMessage {
            role: MessageRole::System,
            content,
            thinking: None,
            parts: Vec::new(),
            tool_calls: Vec::new(),
        });
    }

    async fn respond_approval_choice(
        &mut self,
        choice: ApprovalChoice,
        feedback: Option<String>,
    ) -> anyhow::Result<()> {
        let Some(approval) = self.state.approval_pending.take() else {
            return Ok(());
        };
        let Some(sid) = self.state.session_id.clone() else {
            self.system_message("No session for approval.".into());
            return Ok(());
        };

        let feedback = feedback.and_then(|f| {
            let t = f.trim().to_string();
            if t.is_empty() {
                None
            } else {
                Some(t)
            }
        });

        if let Err(e) = self
            .client
            .respond_approval(
                &sid,
                kkagent_protocol::ApprovalResponse {
                    approval_id: approval.approval_id.clone(),
                    decision: choice.decision,
                    scope: choice.scope,
                    feedback,
                    selected_label: Some(choice.selected_label.clone()),
                },
            )
            .await
        {
            // Restore panel on failure
            self.state.approval_pending = Some(approval);
            self.system_message(format!("Approval failed: {}", e));
        }
        Ok(())
    }

    async fn handle_question_key(&mut self, key: KeyEvent) -> anyhow::Result<()> {
        let Some(ref mut q) = self.state.question_pending else {
            return Ok(());
        };
        let n = q.options.len();
        let free_row = if q.allow_free_text { 1 } else { 0 };
        let max_row = n.saturating_add(free_row).saturating_sub(1);

        match key.code {
            KeyCode::Esc => {
                self.respond_question(true).await?;
            }
            KeyCode::Up if q.selected > 0 => {
                q.selected -= 1;
            }
            KeyCode::Down if q.selected < max_row => {
                q.selected += 1;
            }
            KeyCode::Char(' ') if q.selected < n => {
                if let Some(t) = q.toggled.get_mut(q.selected) {
                    *t = !*t;
                }
            }
            KeyCode::Enter => {
                if q.selected < n && !q.toggled.iter().any(|t| *t) {
                    if let Some(t) = q.toggled.get_mut(q.selected) {
                        *t = true;
                    }
                }
                self.respond_question(false).await?;
            }
            KeyCode::Char(c) if q.allow_free_text && q.selected >= n => {
                q.free_text.push(c);
            }
            KeyCode::Backspace if q.allow_free_text && q.selected >= n => {
                q.free_text.pop();
            }
            KeyCode::Char(c) if c.is_ascii_digit() && n > 0 => {
                let idx = (c as u8 - b'1') as usize;
                if idx < n {
                    q.selected = idx;
                    q.toggled.iter_mut().for_each(|t| *t = false);
                    if let Some(t) = q.toggled.get_mut(idx) {
                        *t = true;
                    }
                    self.respond_question(false).await?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn respond_question(&mut self, cancelled: bool) -> anyhow::Result<()> {
        let Some(q) = self.state.question_pending.take() else {
            return Ok(());
        };
        let Some(sid) = self.state.session_id.clone() else {
            self.system_message("No session for question.".into());
            return Ok(());
        };

        let selected_option_ids: Vec<String> = q
            .options
            .iter()
            .zip(q.toggled.iter())
            .filter_map(|((id, _), on)| if *on { Some(id.clone()) } else { None })
            .collect();
        let free_text = {
            let t = q.free_text.trim();
            if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            }
        };

        if let Err(e) = self
            .client
            .respond_question(
                &sid,
                kkagent_protocol::QuestionResponse {
                    question_id: q.question_id.clone(),
                    selected_option_ids,
                    free_text,
                    cancelled,
                },
            )
            .await
        {
            self.state.question_pending = Some(q);
            self.system_message(format!("Question reply failed: {}", e));
        }
        Ok(())
    }

    fn handle_server_event(&mut self, frame: Frame) {
        if let Frame::Event { event: _, data, .. } = frame {
            if let Ok(evt) = serde_json::from_value::<AgentEvent>(data) {
                let evt_sid = evt.session_id().to_string();
                let is_current = self.state.session_id.as_deref() == Some(evt_sid.as_str());
                self.state.event_router.on_event(
                    &evt,
                    &mut self.state.tab_strip,
                    &mut self.state.status_bar,
                    self.state.session_id.as_deref(),
                );

                if !is_current {
                    match evt {
                        AgentEvent::ApprovalRequested { request, .. } => {
                            let pending = pending_approval_from_request(&request);
                            self.state.parked_approvals.insert(evt_sid.clone(), pending);
                            self.state.tab_strip.mark_dirty(&evt_sid, true);
                            self.state.tab_strip.ensure_tab(&evt_sid, "needs approval");
                            self.system_message(format!(
                                "Background session {} needs approval — Tab / ←→ to switch.",
                                &evt_sid[..8.min(evt_sid.len())]
                            ));
                        }
                        AgentEvent::QuestionAsked { question, .. } => {
                            let options: Vec<(String, String)> = question
                                .options
                                .into_iter()
                                .map(|o| (o.id, o.label))
                                .collect();
                            let toggled = vec![false; options.len()];
                            self.state.parked_questions.insert(
                                evt_sid.clone(),
                                PendingQuestion {
                                    question_id: question.question_id,
                                    text: question.text,
                                    options,
                                    allow_free_text: question.allow_free_text,
                                    selected: 0,
                                    toggled,
                                    free_text: String::new(),
                                },
                            );
                            self.state.tab_strip.mark_dirty(&evt_sid, true);
                            self.system_message(format!(
                                "Background session {} asked a question — Tab / ←→ to switch.",
                                &evt_sid[..8.min(evt_sid.len())]
                            ));
                        }
                        AgentEvent::Error { message, .. } if message != "Interrupted" => {
                            self.system_message(format!(
                                "Background session {}: {message}",
                                &evt_sid[..8.min(evt_sid.len())]
                            ));
                        }
                        _ => {}
                    }
                    return;
                }

                match evt {
                    AgentEvent::MessageDelta { text, .. } => {
                        let pending_thinking = if !self.state.thinking_text.is_empty() {
                            Some(std::mem::take(&mut self.state.thinking_text))
                        } else {
                            None
                        };

                        if let Some(last) = self.state.messages.last_mut() {
                            if last.role == MessageRole::Assistant {
                                if last.thinking.is_none() {
                                    last.thinking = pending_thinking;
                                }
                                last.append_assistant_text(&text);
                                return;
                            }
                        }
                        let mut msg = DisplayMessage {
                            role: MessageRole::Assistant,
                            content: String::new(),
                            thinking: pending_thinking,
                            parts: Vec::new(),
                            tool_calls: Vec::new(),
                        };
                        msg.append_assistant_text(&text);
                        self.state.messages.push(msg);
                    }
                    AgentEvent::ThinkingDelta { text, .. } => {
                        self.state.thinking_text.push_str(&text);
                    }
                    AgentEvent::ToolCall {
                        tool_name, input, ..
                    } => {
                        self.state.last_tool_name = Some(tool_name.clone());
                        let summary = summarize_tool_input(&input);
                        let pending_thinking = if !self.state.thinking_text.is_empty() {
                            Some(std::mem::take(&mut self.state.thinking_text))
                        } else {
                            None
                        };
                        let tc = DisplayToolCall {
                            name: tool_name,
                            input_summary: summary,
                            output: None,
                            is_error: false,
                            collapsed: true,
                        };
                        if let Some(last) = self.state.messages.last_mut() {
                            if last.role == MessageRole::Assistant {
                                if last.thinking.is_none() {
                                    last.thinking = pending_thinking;
                                }
                                last.push_tool(tc);
                                return;
                            }
                        }
                        let mut msg = DisplayMessage {
                            role: MessageRole::Assistant,
                            content: String::new(),
                            thinking: pending_thinking,
                            parts: Vec::new(),
                            tool_calls: Vec::new(),
                        };
                        msg.push_tool(tc);
                        self.state.messages.push(msg);
                    }
                    AgentEvent::ToolResult {
                        tool_name,
                        output,
                        is_error,
                        ..
                    } => {
                        if let Some(last) = self.state.messages.last_mut() {
                            if let Some(tc) = last.find_pending_tool_mut(&tool_name) {
                                tc.output = Some(output);
                                tc.is_error = is_error;
                            }
                        }
                    }
                    AgentEvent::StatusUpdate { status, .. } => {
                        self.state.status = status;
                    }
                    AgentEvent::ApprovalRequested { request, .. } => {
                        let is_plan_review = request.tool_name == "ExitPlanMode"
                            || request
                                .tool_input_display
                                .as_ref()
                                .and_then(|d| d.get("kind"))
                                .and_then(|v| v.as_str())
                                == Some("plan_review");
                        if is_plan_review {
                            if let Some(display) = request.tool_input_display.as_ref() {
                                if let Some(plan) = display.get("plan").and_then(|v| v.as_str()) {
                                    let path = display
                                        .get("path")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    if !plan.trim().is_empty() {
                                        self.state.apply_plan_document(path, plan.to_string());
                                    }
                                }
                            }
                        }
                        self.state.approval_pending =
                            Some(pending_approval_from_request(&request));
                        self.state.status = SessionStatus::WaitingApproval;
                    }
                    AgentEvent::QuestionAsked { question, .. } => {
                        let options: Vec<(String, String)> = question
                            .options
                            .into_iter()
                            .map(|o| (o.id, o.label))
                            .collect();
                        let toggled = vec![false; options.len()];
                        self.state.question_pending = Some(PendingQuestion {
                            question_id: question.question_id,
                            text: question.text,
                            options,
                            allow_free_text: question.allow_free_text,
                            selected: 0,
                            toggled,
                            free_text: String::new(),
                        });
                        self.state.status = SessionStatus::WaitingQuestion;
                    }
                    AgentEvent::UsageUpdate { usage, .. } => {
                        self.state.approx_tokens =
                            usage.input_tokens.saturating_add(usage.output_tokens);
                    }
                    AgentEvent::Error { message, .. } => {
                        self.state.status = SessionStatus::Idle;
                        if message != "Interrupted" {
                            self.system_message(format!("Error: {}", message));
                        }
                    }
                    AgentEvent::PlanModeChanged { enabled, .. } => {
                        self.state.on_plan_mode_changed(enabled);
                        self.system_message(format!(
                            "Plan mode: {}",
                            if enabled { "on" } else { "off" }
                        ));
                    }
                    AgentEvent::PlanFileUpdated { path, content, .. } => {
                        self.state.apply_plan_document(path, content);
                    }
                    AgentEvent::TodoUpdated { items, .. } => {
                        self.state.todos = items
                            .into_iter()
                            .map(|i| TodoItem {
                                id: i.id,
                                content: i.content,
                                status: i.status,
                            })
                            .collect();
                        if self.state.todos.is_empty() {
                            self.state.todos_expanded = false;
                        }
                    }
                    AgentEvent::TurnEnd { .. } => {
                        if !self.state.thinking_text.is_empty() {
                            let t = std::mem::take(&mut self.state.thinking_text);
                            if let Some(last) = self.state.messages.last_mut() {
                                if last.role == MessageRole::Assistant && last.thinking.is_none() {
                                    last.thinking = Some(t);
                                } else {
                                    self.state.messages.push(DisplayMessage {
                                        role: MessageRole::Assistant,
                                        content: String::new(),
                                        thinking: Some(t),
                                        parts: Vec::new(),
                                        tool_calls: Vec::new(),
                                    });
                                }
                            } else {
                                self.state.messages.push(DisplayMessage {
                                    role: MessageRole::Assistant,
                                    content: String::new(),
                                    thinking: Some(t),
                                    parts: Vec::new(),
                                    tool_calls: Vec::new(),
                                });
                            }
                        }
                        self.state.collapse_completed_turn_tools();
                        self.state.status = SessionStatus::Idle;
                    }
                    AgentEvent::TurnStart { .. } => {
                        self.state.thinking_text.clear();
                        self.state.turn_started_at = Some(std::time::Instant::now());
                        self.state.tokens_at_turn_start = self.state.approx_tokens;
                    }
                    AgentEvent::SubagentSpawned {
                        subagent_id,
                        subagent_name,
                        parent_tool_call_id,
                        description,
                        ..
                    } => {
                        let desc = description.unwrap_or_default();
                        self.system_message(format!(
                            "⊟ subagent spawned [{subagent_name}] id={subagent_id} under {parent_tool_call_id}: {desc}"
                        ));
                    }
                    AgentEvent::SubagentStarted { subagent_id, .. } => {
                        self.system_message(format!("⊟ subagent started: {subagent_id}"));
                    }
                    AgentEvent::SubagentCompleted {
                        subagent_id,
                        result_summary,
                        ..
                    } => {
                        self.system_message(format!(
                            "⊟ subagent completed [{subagent_id}]: {}",
                            result_summary.chars().take(200).collect::<String>()
                        ));
                    }
                    AgentEvent::SubagentFailed {
                        subagent_id, error, ..
                    } => {
                        self.system_message(format!("⊟ subagent failed [{subagent_id}]: {error}"));
                    }
                    AgentEvent::SubagentChildEvent {
                        subagent_id, event, ..
                    } => match *event {
                        AgentEvent::ToolCall {
                            tool_name, input, ..
                        } => {
                            let brief = serde_json::to_string(&input)
                                .unwrap_or_default()
                                .chars()
                                .take(80)
                                .collect::<String>();
                            self.system_message(format!(
                                "  ↳ [{subagent_id}] tool {tool_name} {brief}"
                            ));
                        }
                        AgentEvent::ToolResult {
                            tool_name,
                            output,
                            is_error,
                            ..
                        } => {
                            let mark = if is_error { "!" } else { "ok" };
                            self.system_message(format!(
                                "  ↳ [{subagent_id}] {tool_name} [{mark}] {}",
                                output.chars().take(120).collect::<String>()
                            ));
                        }
                        AgentEvent::MessageDelta { .. } => {}
                        AgentEvent::Error { message, .. } => {
                            self.system_message(format!("  ↳ [{subagent_id}] error: {message}"));
                        }
                        _ => {}
                    },
                    AgentEvent::McpAuthRequired {
                        server_name,
                        authorization_url,
                        ..
                    } => {
                        self.system_message(format!(
                            "MCP OAuth required for `{server_name}`.\nOpen: {authorization_url}"
                        ));
                    }
                    AgentEvent::BtwDelta { text, .. } => {
                        self.state.btw.open = true;
                        self.state.btw.append_delta(&text);
                    }
                    AgentEvent::BtwThinkingDelta { .. } => {}
                    AgentEvent::BtwEnd { error, .. } => {
                        self.state.btw.finish(error);
                        self.state.btw.open = true;
                    }
                }
            }
        }
    }

    fn toggle_tool_folding(&mut self) {
        // Prefer expanding/collapsing the latest turn tool-history overview.
        for msg in self.state.messages.iter_mut().rev() {
            for part in msg.parts.iter_mut().rev() {
                if let DisplayPart::ToolHistory(hist) = part {
                    hist.expanded = !hist.expanded;
                    return;
                }
            }
        }
        // Fallback: toggle per-tool output folding while a turn is still live.
        for msg in &mut self.state.messages {
            for part in &mut msg.parts {
                if let DisplayPart::Tool(tc) = part {
                    tc.collapsed = !tc.collapsed;
                }
            }
            for tc in &mut msg.tool_calls {
                tc.collapsed = !tc.collapsed;
            }
        }
    }
}

fn summarize_tool_input(input: &serde_json::Value) -> String {
    // Prefer human-readable fields; keep enough text for full-width chips.
    for key in ["command", "path", "pattern", "query", "url", "description", "prompt"] {
        if let Some(s) = input.get(key).and_then(|v| v.as_str()) {
            let t = s.trim();
            if !t.is_empty() {
                return t.chars().take(512).collect();
            }
        }
    }
    serde_json::to_string(input)
        .unwrap_or_default()
        .chars()
        .take(512)
        .collect()
}

fn pending_approval_from_request(
    request: &kkagent_protocol::ApprovalRequest,
) -> PendingApproval {
    let display = request
        .tool_input_display
        .clone()
        .unwrap_or(serde_json::Value::Null);
    let is_plan_review = request.tool_name == "ExitPlanMode"
        || display.get("kind").and_then(|v| v.as_str()) == Some("plan_review");
    let choices = if is_plan_review {
        PendingApproval::plan_review_choices(&display)
    } else {
        PendingApproval::default_tool_choices()
    };
    let detail = if is_plan_review {
        String::new()
    } else {
        request
            .tool_input_display
            .as_ref()
            .map(|v| serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string()))
            .unwrap_or_default()
    };
    PendingApproval {
        approval_id: request.approval_id.clone(),
        tool_name: request.tool_name.clone(),
        action: if is_plan_review {
            "按此计划开始执行？".into()
        } else {
            request.action.clone()
        },
        detail,
        selected: 0,
        choices,
        is_plan_review,
        feedback_mode: false,
        feedback: String::new(),
    }
}

fn split_plan_message_content(content: &str) -> (String, String) {
    let mut body = content;
    let mut path = String::new();
    if let Some(rest) = body.strip_prefix("file: ") {
        if let Some((path_line, rest_body)) = rest.split_once('\n') {
            path = path_line.trim().to_string();
            body = rest_body.trim_start_matches('\n');
        }
    }
    (path, body.to_string())
}

fn slash_help_text() -> String {
    let mut s = String::from(
        "Keyboard shortcuts:\n\
  Enter         - Submit / confirm slash\n\
  Tab / ← →    - Empty input: cycle related sessions (/new or /fork)\n\
  Shift-Tab     - Toggle plan mode (scroll locks to full plan until exit)\n\
  ↑↓            - Input history / slash menu\n\
  PgUp/PgDn     - Scroll transcript\n\
  Mouse wheel   - Scroll transcript\n\
  Click + drag  - Select/copy (releases mouse; next key restores scroll)\n\
  Esc           - Close menu/overlay only; if none, interrupt turn / Esc Esc undo\n\
  !             - Shell mode\n\
  @             - File path picker (Tab/Enter insert)\n\
  Ctrl-F / Ctrl-S - Search transcript\n\
  Ctrl-G        - Toggle / cancel /btw side pane\n\
  Ctrl-O        - Expand/collapse turn tool history (or tool output)\n\
  Ctrl-T        - Expand/collapse todo panel\n\
  Ctrl-P / Ctrl-N - Input history\n\
  Large paste   - Collapses to [Pasted text #n] overview\n\n\
Slash commands:\n",
    );
    for cmd in slash::BUILTIN_SLASH_COMMANDS {
        let hint = cmd
            .argument_hint
            .map(|h| format!(" {}", h))
            .unwrap_or_default();
        s.push_str(&format!("  /{}{}  — {}\n", cmd.name, hint, cmd.description));
    }
    s
}

/// Convert a list of serialized ChatMessages into display bubbles,
/// pairing tool_result blocks onto preceding tool_use entries.
/// Sessions with only UI system notices (or nothing) are treated as empty and
/// discarded when the user leaves the page.
fn session_has_retained_io(messages: &[DisplayMessage]) -> bool {
    messages.iter().any(|m| match m.role {
        MessageRole::System => false,
        MessageRole::User => {
            !m.content.trim().is_empty()
                && !kkagent_protocol::is_harness_only_user_text(&m.content)
        }
        MessageRole::Plan => !m.content.trim().is_empty(),
        MessageRole::Assistant => {
            !m.content.trim().is_empty()
                || !m.parts.is_empty()
                || !m.tool_calls.is_empty()
                || m.thinking
                    .as_ref()
                    .map(|t| !t.trim().is_empty())
                    .unwrap_or(false)
        }
    })
}

fn transcript_messages_to_display(msgs: &[serde_json::Value]) -> Vec<DisplayMessage> {
    let mut out: Vec<DisplayMessage> = Vec::new();
    // tool_use id -> (msg_idx, part_idx)
    let mut tool_index: std::collections::HashMap<String, (usize, usize)> =
        std::collections::HashMap::new();

    for m in msgs {
        let Some(role_str) = m.get("role").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(content) = m.get("content").and_then(|v| v.as_array()) else {
            continue;
        };

        match role_str {
            "user" => {
                let mut text = String::new();
                let mut only_tool_results = true;
                for block in content {
                    let ty = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    match ty {
                        "text" => {
                            only_tool_results = false;
                            if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                                if !text.is_empty() {
                                    text.push('\n');
                                }
                                text.push_str(t);
                            }
                        }
                        "tool_result" => {
                            let id = block
                                .get("tool_use_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            let result = block
                                .get("content")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let is_error = block
                                .get("is_error")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false);
                            if let Some(&(mi, pi)) = tool_index.get(id) {
                                if let Some(msg) = out.get_mut(mi) {
                                    if let Some(DisplayPart::Tool(tc)) = msg.parts.get_mut(pi) {
                                        tc.output = Some(result);
                                        tc.is_error = is_error;
                                        tc.collapsed = true;
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
                if !only_tool_results || !text.is_empty() {
                    if kkagent_protocol::is_harness_only_user_text(&text) {
                        continue;
                    }
                    let visible = kkagent_protocol::visible_user_text(&text);
                    let content = if visible.is_empty() {
                        text
                    } else {
                        visible
                    };
                    if content.trim().is_empty() {
                        continue;
                    }
                    out.push(DisplayMessage {
                        role: MessageRole::User,
                        content,
                        thinking: None,
                        parts: Vec::new(),
                        tool_calls: Vec::new(),
                    });
                }
            }
            "assistant" => {
                let mut thinking = None;
                let mut parts: Vec<DisplayPart> = Vec::new();
                let mut content_mirror = String::new();
                let msg_idx = out.len();
                for block in content {
                    let ty = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    match ty {
                        "text" => {
                            if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                                if let Some(DisplayPart::Text(existing)) = parts.last_mut() {
                                    existing.push_str(t);
                                } else {
                                    parts.push(DisplayPart::Text(t.to_string()));
                                }
                                content_mirror.push_str(t);
                            }
                        }
                        "thinking" => {
                            if let Some(t) = block.get("thinking").and_then(|v| v.as_str()) {
                                thinking = Some(t.to_string());
                            }
                        }
                        "tool_use" => {
                            let id = block
                                .get("id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let name = block
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("tool")
                                .to_string();
                            let input = block.get("input").cloned().unwrap_or_default();
                            let summary = serde_json::to_string(&input).unwrap_or_default();
                            let summary: String = summary.chars().take(120).collect();
                            let pi = parts.len();
                            if !id.is_empty() {
                                tool_index.insert(id, (msg_idx, pi));
                            }
                            parts.push(DisplayPart::Tool(DisplayToolCall {
                                name,
                                input_summary: summary,
                                output: None,
                                is_error: false,
                                collapsed: true,
                            }));
                        }
                        _ => {}
                    }
                }
                if !content_mirror.is_empty() || thinking.is_some() || !parts.is_empty() {
                    out.push(DisplayMessage {
                        role: MessageRole::Assistant,
                        content: content_mirror,
                        thinking,
                        parts,
                        tool_calls: Vec::new(),
                    });
                }
            }
            "system" => {
                let mut text = String::new();
                for block in content {
                    if block.get("type").and_then(|v| v.as_str()) == Some("text") {
                        if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                            if !text.is_empty() {
                                text.push('\n');
                            }
                            text.push_str(t);
                        }
                    }
                }
                if !text.is_empty() {
                    out.push(DisplayMessage {
                        role: MessageRole::System,
                        content: text,
                        thinking: None,
                        parts: Vec::new(),
                        tool_calls: Vec::new(),
                    });
                }
            }
            _ => {}
        }
    }
    out
}

fn copy_to_clipboard(text: &str) -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let mut child = Command::new("pbcopy")
            .stdin(Stdio::piped())
            .spawn()
            .map_err(|e| anyhow::anyhow!("pbcopy: {}", e))?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(text.as_bytes())?;
        }
        let status = child.wait()?;
        if status.success() {
            return Ok(());
        }
        anyhow::bail!("pbcopy exited with {}", status);
    }
    #[cfg(target_os = "linux")]
    {
        use std::io::Write;
        use std::process::{Command, Stdio};
        for cmd in ["wl-copy", "xclip"] {
            let mut c = Command::new(cmd);
            if cmd == "xclip" {
                c.args(["-selection", "clipboard"]);
            }
            if let Ok(mut child) = c.stdin(Stdio::piped()).spawn() {
                if let Some(mut stdin) = child.stdin.take() {
                    let _ = stdin.write_all(text.as_bytes());
                }
                if child.wait().map(|s| s.success()).unwrap_or(false) {
                    return Ok(());
                }
            }
        }
        anyhow::bail!("no wl-copy/xclip available");
    }
    #[cfg(target_os = "windows")]
    {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let mut child = Command::new("clip")
            .stdin(Stdio::piped())
            .spawn()
            .map_err(|e| anyhow::anyhow!("clip: {}", e))?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(text.as_bytes())?;
        }
        let status = child.wait()?;
        if status.success() {
            return Ok(());
        }
        anyhow::bail!("clip exited with {}", status);
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = text;
        anyhow::bail!("clipboard unsupported on this platform");
    }
}

fn paste_clipboard_into_workspace() -> anyhow::Result<Option<PathBuf>> {
    let root = std::env::current_dir()?;
    let dir = root.join(".kkagent").join("attachments");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("pasted-{}.png", uuid::Uuid::new_v4()));

    let bytes = read_clipboard_png(&path)?;
    let Some(bytes) = bytes else {
        let _ = std::fs::remove_file(&path);
        return Ok(None);
    };
    if bytes.len() > 100 * 1024 * 1024 {
        let _ = std::fs::remove_file(&path);
        anyhow::bail!("clipboard image exceeds the 100 MiB input limit");
    }
    let image = image::load_from_memory(&bytes).map_err(|error| {
        anyhow::anyhow!("clipboard does not contain a supported image: {error}")
    })?;
    image.save_with_format(&path, image::ImageFormat::Png)?;
    Ok(Some(
        path.strip_prefix(&root).unwrap_or(&path).to_path_buf(),
    ))
}

fn read_clipboard_png(path: &std::path::Path) -> anyhow::Result<Option<Vec<u8>>> {
    #[cfg(target_os = "macos")]
    {
        let script = r#"try
set pngData to the clipboard as «class PNGf»
set targetPath to system attribute "KKAGENT_CLIP_PATH"
set fileRef to open for access POSIX file targetPath with write permission
set eof fileRef to 0
write pngData to fileRef
close access fileRef
on error
try
close access POSIX file (system attribute "KKAGENT_CLIP_PATH")
end try
return "no-image"
end try"#;
        let status = std::process::Command::new("osascript")
            .args(["-e", script])
            .env("KKAGENT_CLIP_PATH", path)
            .status()?;
        if !status.success() || !path.is_file() {
            return Ok(None);
        }
        Ok(Some(std::fs::read(path)?))
    }
    #[cfg(target_os = "linux")]
    {
        for (command, args) in [
            ("wl-paste", vec!["--no-newline", "--type", "image/png"]),
            (
                "xclip",
                vec!["-selection", "clipboard", "-t", "image/png", "-o"],
            ),
        ] {
            if let Ok(output) = std::process::Command::new(command).args(args).output() {
                if output.status.success() && !output.stdout.is_empty() {
                    return Ok(Some(output.stdout));
                }
            }
        }
        Ok(None)
    }
    #[cfg(target_os = "windows")]
    {
        let script = "Add-Type -AssemblyName System.Windows.Forms; Add-Type -AssemblyName System.Drawing; $img=[Windows.Forms.Clipboard]::GetImage(); if ($null -eq $img) { exit 2 }; $img.Save($env:KKAGENT_CLIP_PATH,[Drawing.Imaging.ImageFormat]::Png)";
        let status = std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .env("KKAGENT_CLIP_PATH", path)
            .status()?;
        if !status.success() || !path.is_file() {
            return Ok(None);
        }
        Ok(Some(std::fs::read(path)?))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = path;
        Ok(None)
    }
}

fn read_clipboard_text() -> anyhow::Result<String> {
    #[cfg(target_os = "macos")]
    let output = std::process::Command::new("pbpaste").output()?;
    #[cfg(target_os = "linux")]
    let output = std::process::Command::new("sh")
        .args(["-c", "command -v wl-paste >/dev/null && wl-paste --no-newline || xclip -selection clipboard -o"])
        .output()?;
    #[cfg(target_os = "windows")]
    let output = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Get-Clipboard -Raw",
        ])
        .output()?;
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    anyhow::bail!("clipboard unsupported on this platform");
    if !output.status.success() {
        anyhow::bail!("clipboard command failed");
    }
    Ok(String::from_utf8(output.stdout)?)
}

#[cfg(test)]
mod app_state_tests {
    use super::*;

    #[test]
    fn empty_session_ignores_system_notices() {
        assert!(!session_has_retained_io(&[]));
        assert!(!session_has_retained_io(&[DisplayMessage {
            role: MessageRole::System,
            content: "tip".into(),
            thinking: None,
            parts: Vec::new(),
            tool_calls: Vec::new(),
        }]));
        assert!(session_has_retained_io(&[DisplayMessage {
            role: MessageRole::User,
            content: "hello".into(),
            thinking: None,
            parts: Vec::new(),
            tool_calls: Vec::new(),
        }]));
        assert!(!session_has_retained_io(&[DisplayMessage {
            role: MessageRole::User,
            content: "<system-reminder>\nToday's date\n</system-reminder>".into(),
            thinking: None,
            parts: Vec::new(),
            tool_calls: Vec::new(),
        }]));
    }

    #[test]
    fn collapse_turn_tools_into_overview() {
        let mut state = AppState::new(PermissionMode::Manual, false);
        state.turn_started_at = Some(std::time::Instant::now() - std::time::Duration::from_secs(90));
        state.tokens_at_turn_start = 100;
        state.approx_tokens = 2500;
        state.messages.push(DisplayMessage {
            role: MessageRole::User,
            content: "do it".into(),
            thinking: None,
            parts: Vec::new(),
            tool_calls: Vec::new(),
        });
        let mut asst = DisplayMessage {
            role: MessageRole::Assistant,
            content: "done".into(),
            thinking: None,
            parts: Vec::new(),
            tool_calls: Vec::new(),
        };
        asst.parts.push(DisplayPart::Tool(DisplayToolCall {
            name: "Read".into(),
            input_summary: "a.rs".into(),
            output: Some("ok".into()),
            is_error: false,
            collapsed: true,
        }));
        asst.parts.push(DisplayPart::Tool(DisplayToolCall {
            name: "Bash".into(),
            input_summary: "ls".into(),
            output: Some("x".into()),
            is_error: false,
            collapsed: true,
        }));
        asst.parts.push(DisplayPart::Text("done".into()));
        state.messages.push(asst);

        state.collapse_completed_turn_tools();
        let parts = &state.messages.last().unwrap().parts;
        assert!(matches!(parts.first(), Some(DisplayPart::ToolHistory(h)) if h.tool_count == 2 && !h.expanded));
        assert!(matches!(parts.last(), Some(DisplayPart::Text(t)) if t == "done"));
        assert!(!parts.iter().any(|p| matches!(p, DisplayPart::Tool(_))));
    }
}
