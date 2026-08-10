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
    build_skill_slash_commands, filter_slash_commands_with_extras, find_slash_command,
    is_slash_name_completion, parse_slash_input, SlashSuggestion,
};

pub struct TuiApp {
    config: AppConfig,
    client: KkagentClient,
    state: AppState,
    mouse_mode: MouseMode,
    jobs: crate::async_jobs::AsyncJobHub,
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
    /// Last rendered transcript area (for mouse → cell mapping).
    pub transcript_area: ratatui::layout::Rect,
    /// Footer chrome area (status + session strip).
    pub footer_area: ratatui::layout::Rect,
    /// Absolute terminal hit boxes for footer session strip entries.
    pub session_strip_hits: Vec<crate::chrome::SessionStripHit>,
    /// Absolute column where the session strip text begins.
    pub session_strip_origin_x: u16,
    /// Pending mouse action on the session strip (processed after poll).
    pub pending_strip_action: Option<StripAction>,
    /// Hovered strip title shown in the tip line.
    pub strip_hover_title: Option<String>,
    /// Plain-text rows parallel to the last rendered transcript lines.
    pub select_rows: Vec<crate::selection::SelectRow>,
    /// In-app text selection (absolute visual line + display column).
    pub selection: Option<crate::selection::TextSelection>,
    /// True while left button is held for drag-select.
    pub selection_dragging: bool,
    /// Last mouse cell (terminal column/row) for scroll-during-drag updates.
    pub last_mouse: Option<(u16, u16)>,
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
    /// Dynamic skill slash entries (`skill:name` / bare name).
    pub skill_slash_commands: Vec<SlashSuggestion>,
    /// Maps slash command name → skill name.
    pub skill_command_map: std::collections::HashMap<String, String>,
    /// `@` file path autocomplete popup
    pub file_menu: Option<FileMenuState>,
    /// Model / session list picker overlay
    pub list_picker: Option<ListPickerState>,
    /// Parents for nested pickers; Esc pops one level instead of closing all.
    pub list_picker_stack: Vec<ListPickerState>,
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
    /// In-flight non-blocking session switch context (last selection wins).
    pub resume_switch: Option<ResumeSwitchCtx>,
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
    /// Cached markdown/layout lines for completed transcript messages.
    pub render_cache: crate::render_cache::RenderCache,
    /// Older transcript pages still loading after a lazy resume.
    pub history_loading: bool,
    /// Absolute index of the oldest message currently shown (for prepend pages).
    pub history_oldest_index: Option<usize>,
    /// Total message count known from the last resume/history response.
    pub history_total: Option<usize>,
    /// Per-session draft / scroll / search state (survives switches).
    pub session_views: std::collections::HashMap<String, crate::session_view::SessionViewState>,
    /// Debounced `/sessions` preview target.
    pub preview_debounce: Option<PreviewDebounce>,
    /// LRU cache of session.preview JSON payloads.
    pub preview_cache: crate::session_view::PreviewLru,
    /// Queued prompts waiting for the current turn to finish.
    pub prompt_queue: crate::prompt_queue::PromptQueue,
    /// When session is busy, Enter queues by default (Shift-Enter steers if supported).
    pub queue_when_busy: bool,
    /// Last session-switch latency samples (ms) for regression awareness.
    pub last_switch_metrics: Option<SessionSwitchMetrics>,
    /// Cumulative token usage for the active session (server-authoritative).
    pub usage_session: SessionUsageTotals,
    /// Recent per-turn usage samples for `/usage`.
    pub usage_turns: Vec<TurnUsageSample>,
}

#[derive(Debug, Clone, Default)]
pub struct SessionUsageTotals {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
}

#[derive(Debug, Clone)]
pub struct TurnUsageSample {
    pub model: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub duration_ms: u64,
}

#[derive(Debug, Clone)]
pub struct SessionSwitchMetrics {
    pub target: String,
    /// Time until footer “switching…” / job notice is visible.
    pub first_feedback_ms: u64,
    /// Time until target transcript is applied.
    pub visible_ms: u64,
}

#[derive(Debug, Clone)]
pub enum StripAction {
    Switch(String),
    Cycle(i8),
}

#[derive(Debug, Clone)]
pub struct PreviewDebounce {
    pub session_id: String,
    pub due_at: std::time::Instant,
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
    /// Toggle skills on/off (Enter toggles, Esc goes back).
    SkillManage,
    /// Toggle MCP servers on/off (Enter toggles, Esc goes back).
    McpManage,
    /// Top-level settings menu (`/config`).
    Config,
    /// Pick an LLM provider, then drill into its models (`/provider`).
    Provider,
    /// Thinking effort levels (`/effort`).
    Effort,
    /// Browse-only key/value rows (status / auth / flags / plugins / info).
    Browse,
    /// Slash command catalogue (`/help`).
    Help,
    /// Prompt templates (`/prompts`).
    Prompts,
    /// Swarm mode actions (`/swarm`).
    Swarm,
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
    /// true = permanently delete history; false = close TUI tab only.
    pub permanent: bool,
    /// Session is currently busy (running / waiting).
    pub busy: bool,
}

#[derive(Debug, Clone)]
pub struct ResumeSwitchCtx {
    pub target: String,
    pub leaving_id: Option<String>,
    pub leaving_empty: bool,
    pub started_at: std::time::Instant,
}

#[derive(Debug, Clone)]
pub struct ListPickerState {
    pub kind: ListPickerKind,
    pub title: String,
    pub items: Vec<ListPickerItem>,
    pub selected: usize,
    /// Optional fuzzy filter (used by `/sessions`).
    pub filter: String,
    /// Unfiltered source items for session search.
    pub all_items: Vec<ListPickerItem>,
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
    /// Delivery lifecycle for user prompts (ignored for other roles).
    pub delivery: crate::prompt_queue::DeliveryState,
    /// Client idempotency key used when sending this user prompt.
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Clone)]
pub enum DisplayPart {
    Text(String),
    Tool(DisplayToolCall),
    /// Collapsed tool-call history between formal assistant outputs (after turn end).
    ToolHistory(ToolHistorySummary),
    /// kimi-style skill activation card (`▶ Activated skill: name`).
    SkillActivation {
        name: String,
        args: Option<String>,
    },
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

    pub fn find_tool_by_id_mut(&mut self, tool_call_id: &str) -> Option<&mut DisplayToolCall> {
        for part in self.parts.iter_mut().rev() {
            match part {
                DisplayPart::Tool(tc) if tc.id == tool_call_id => return Some(tc),
                DisplayPart::ToolHistory(hist) => {
                    if let Some(tc) = hist.tools.iter_mut().find(|t| t.id == tool_call_id) {
                        return Some(tc);
                    }
                }
                _ => {}
            }
        }
        self.tool_calls.iter_mut().find(|t| t.id == tool_call_id)
    }

    pub fn find_tool_for_result_mut(
        &mut self,
        tool_call_id: &str,
        tool_name: &str,
    ) -> Option<&mut DisplayToolCall> {
        #[derive(Clone, Copy)]
        enum Loc {
            Part(usize),
            Hist { part: usize, tool: usize },
            Legacy(usize),
        }
        let mut loc: Option<Loc> = None;
        if !tool_call_id.is_empty() {
            for (i, part) in self.parts.iter().enumerate().rev() {
                match part {
                    DisplayPart::Tool(tc) if tc.id == tool_call_id => {
                        loc = Some(Loc::Part(i));
                        break;
                    }
                    DisplayPart::ToolHistory(hist) => {
                        if let Some(j) = hist.tools.iter().position(|t| t.id == tool_call_id) {
                            loc = Some(Loc::Hist { part: i, tool: j });
                            break;
                        }
                    }
                    _ => {}
                }
            }
            if loc.is_none() {
                if let Some(i) = self.tool_calls.iter().position(|t| t.id == tool_call_id) {
                    loc = Some(Loc::Legacy(i));
                }
            }
        }
        if loc.is_none() {
            for (i, part) in self.parts.iter().enumerate().rev() {
                if let DisplayPart::Tool(tc) = part {
                    if tc.name == tool_name && tc.output.is_none() {
                        loc = Some(Loc::Part(i));
                        break;
                    }
                }
            }
        }
        if loc.is_none() {
            if let Some(i) = self
                .tool_calls
                .iter()
                .rposition(|t| t.name == tool_name && t.output.is_none())
            {
                loc = Some(Loc::Legacy(i));
            }
        }
        match loc? {
            Loc::Part(i) => match self.parts.get_mut(i)? {
                DisplayPart::Tool(tc) => Some(tc),
                _ => None,
            },
            Loc::Hist { part, tool } => match self.parts.get_mut(part)? {
                DisplayPart::ToolHistory(hist) => hist.tools.get_mut(tool),
                _ => None,
            },
            Loc::Legacy(i) => self.tool_calls.get_mut(i),
        }
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
    /// Compact skill activation card (not a normal chat bubble).
    Skill,
}

#[derive(Debug, Clone)]
pub struct DisplayToolCall {
    pub id: String,
    pub name: String,
    pub input_summary: String,
    pub output: Option<String>,
    pub is_error: bool,
    pub collapsed: bool,
    pub started_at: Option<std::time::Instant>,
    pub stopping: bool,
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
    pub allow_multiple: bool,
    pub background: bool,
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
            transcript_area: ratatui::layout::Rect::default(),
            footer_area: ratatui::layout::Rect::default(),
            session_strip_hits: Vec::new(),
            session_strip_origin_x: 0,
            pending_strip_action: None,
            strip_hover_title: None,
            select_rows: Vec::new(),
            selection: None,
            selection_dragging: false,
            last_mouse: None,
            approx_tokens: 0,
            approval_pending: None,
            parked_approvals: std::collections::HashMap::new(),
            question_pending: None,
            parked_questions: std::collections::HashMap::new(),
            slash_menu: None,
            skill_slash_commands: Vec::new(),
            skill_command_map: std::collections::HashMap::new(),
            file_menu: None,
            list_picker: None,
            list_picker_stack: Vec::new(),
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
            resume_switch: None,
            event_router: SessionEventRouter::default(),
            search: SearchState::default(),
            last_tool_name: None,
            highlight_message: None,
            plan_document: None,
            plan_scroll_to_top: false,
            turn_started_at: None,
            tokens_at_turn_start: 0,
            locale: crate::i18n::Locale::En,
            render_cache: crate::render_cache::RenderCache::new(),
            history_loading: false,
            history_oldest_index: None,
            history_total: None,
            session_views: std::collections::HashMap::new(),
            preview_debounce: None,
            preview_cache: crate::session_view::PreviewLru::new(12),
            prompt_queue: crate::prompt_queue::PromptQueue::default(),
            queue_when_busy: true,
            last_switch_metrics: None,
            usage_session: SessionUsageTotals::default(),
            usage_turns: Vec::new(),
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
                    delivery: crate::prompt_queue::DeliveryState::Sent,
            idempotency_key: None,
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
        let tokens = self.approx_tokens.saturating_sub(self.tokens_at_turn_start);
        self.turn_started_at = None;

        let start = self
            .messages
            .iter()
            .rposition(|m| m.role == MessageRole::User)
            .map(|i| i + 1)
            .unwrap_or(0);
        let end = self.messages.len();
        collapse_tools_in_turn(&mut self.messages, start, end, duration_ms, tokens);
    }

    pub fn max_scroll_up(&self) -> u16 {
        self.content_lines
            .saturating_sub(self.viewport_height.max(1))
    }

    pub fn scroll_lines(&mut self, delta: i32) {
        let max = self.max_scroll_up();
        if delta > 0 {
            self.scroll_up = (self.scroll_up as i32 + delta).clamp(0, max as i32) as u16;
        } else if delta < 0 {
            self.scroll_up = self.scroll_up.saturating_sub((-delta) as u16);
        }
        // Content can shrink between frames; never leave scroll past the end.
        self.scroll_up = self.scroll_up.min(max);
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
        let items = filter_slash_commands_with_extras(&text, &self.skill_slash_commands);
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
            jobs: crate::async_jobs::AsyncJobHub::new(),
        }
    }

    /// New sessions inherit the global config default until `/model` overrides them.
    fn bind_config_default_model(&mut self) {
        self.state.model_alias = self.config.default_model_alias().map(|s| s.to_string());
    }

    pub async fn run(mut self, resume: Option<String>) -> anyhow::Result<()> {
        let startup_started = std::time::Instant::now();
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
                self.bind_config_default_model();
            }
        } else {
            let session_id = self
                .client
                .create_session(Some(&cwd), Some(self.state.permission_mode))
                .await?;
            self.state.tab_strip.ensure_active(&session_id, "main");
            self.state.status_bar.session_id = Some(session_id.clone());
            self.state.session_id = Some(session_id);
            self.bind_config_default_model();
        }
        tracing::info!(
            elapsed_ms = startup_started.elapsed().as_millis() as u64,
            "TUI session ready"
        );

        // Sync CLI / config plan mode onto the server session (create starts with plan_mode=false).
        if self.state.plan_mode {
            if let Some(ref sid) = self.state.session_id.clone() {
                if let Err(e) = self.client.set_plan_mode(sid, true).await {
                    eprintln!("Failed to enable plan mode: {}", e);
                }
            }
        }

        // Skills / sessions / MCP status are useful, but none are required for the
        // first interactive frame. Fetch on a request-only client so slow disks
        // or MCP handshakes never hold the alternate screen hostage.
        let requester = self.client.requester();
        self.jobs.spawn_rpc(
            requester.clone(),
            crate::async_jobs::JobChannel::SkillsList,
            "skills.list",
            None,
            Some("Loading skills".into()),
            true,
        );
        self.jobs.spawn_rpc(
            requester.clone(),
            crate::async_jobs::JobChannel::SessionsList,
            "sessions.list",
            Some(serde_json::json!({"limit": 80, "include_archived": false})),
            Some("Loading sessions".into()),
            true,
        );
        if !self.config.mcp_servers.is_empty() {
            self.jobs.mcp.configured = true;
            self.jobs.mcp.total = self.config.mcp_servers.len();
            self.jobs.spawn_rpc(
                requester,
                crate::async_jobs::JobChannel::McpStatus,
                "mcp.status",
                None,
                Some("Connecting MCP".into()),
                true,
            );
        }

        enable_raw_mode().map_err(|e| {
            anyhow::anyhow!(
                "Failed to enter raw mode (is stdin a TTY?): {}. \
                 Run kkagent in a real terminal, or use `kkagent -p \"...\"` for non-interactive mode.",
                e
            )
        })?;
        let mut stdout = io::stdout();
        // Capture mouse so the wheel and in-app drag-select stay inside the TUI.
        // Capture remains on for the whole session (no disable-on-click hacks).
        // `KKAGENT_MOUSE_MODE=off` disables mouse reporting.
        if let Err(e) = execute!(stdout, EnterAlternateScreen, EnableBracketedPaste) {
            let _ = disable_raw_mode();
            return Err(e.into());
        }
        tracing::info!(
            elapsed_ms = startup_started.elapsed().as_millis() as u64,
            "TUI first frame ready"
        );
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
            self.jobs.refresh_busy_notices();
            // Expose async notice / MCP status to the renderer via status_bar activity.
            self.state.status_bar.activity = self.jobs.active_notice_text();

            terminal.draw(|f| {
                components::render_ui(f, &mut self.state, &self.config);
            })?;

            self.drain_job_results();

            // Drain the full event queue each frame so trackpad bursts stay in-app
            // (one-event-per-poll left a backlog that felt like lag / terminal scroll).
            let mut saw_event = false;
            let mut scroll_delta = 0i32;
            while event::poll(if saw_event {
                std::time::Duration::ZERO
            } else {
                std::time::Duration::from_millis(50)
            })? {
                saw_event = true;
                match event::read()? {
                    Event::Key(key) => {
                        self.flush_pending_scroll(&mut scroll_delta);
                        self.handle_key(key).await?;
                    }
                    Event::Mouse(mouse) if self.mouse_mode == MouseMode::Capture => {
                        self.collect_mouse(mouse, &mut scroll_delta);
                    }
                    Event::Paste(text) => {
                        self.flush_pending_scroll(&mut scroll_delta);
                        let fold = self.state.mode != AppMode::Shell;
                        self.state.input.paste_chunk(&text);
                        self.state.input.force_flush_paste(fold);
                        self.state.refresh_slash_menu();
                    }
                    Event::Resize(_, _) => {
                        // Selection is clamped on the next render against new rows.
                    }
                    _ => {}
                }
            }
            self.flush_pending_scroll(&mut scroll_delta);

            if let Some(action) = self.state.pending_strip_action.take() {
                match action {
                    StripAction::Switch(id) => {
                        if self.state.session_id.as_deref() != Some(id.as_str()) {
                            let _ = self.resume_session(&id).await;
                        }
                    }
                    StripAction::Cycle(dir) => {
                        if self.can_cycle_fork_sessions() {
                            let _ = self.cycle_workspace_session(dir).await;
                        }
                    }
                }
            }

            if !saw_event {
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
            // Periodic background refresh — never await on the UI loop.
            if self.state.tick.is_multiple_of(100) {
                self.enqueue_workspace_sessions_refresh();
            }
            if self.state.tick.is_multiple_of(20)
                && self.jobs.mcp.configured
                && !self.jobs.mcp.initialized
            {
                self.enqueue_mcp_status_poll();
            }
            self.flush_preview_debounce();
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

    fn enqueue_workspace_sessions_refresh(&mut self) {
        if self
            .jobs
            .pending
            .contains_key(&crate::async_jobs::JobChannel::SessionsList)
        {
            return;
        }
        self.jobs.spawn_rpc(
            self.client.requester(),
            crate::async_jobs::JobChannel::SessionsList,
            "sessions.list",
            Some(serde_json::json!({"limit": 80, "include_archived": false})),
            Some("Refreshing sessions".into()),
            true,
        );
    }

    fn enqueue_mcp_status_poll(&mut self) {
        if self
            .jobs
            .pending
            .contains_key(&crate::async_jobs::JobChannel::McpStatus)
        {
            return;
        }
        self.jobs.spawn_rpc(
            self.client.requester(),
            crate::async_jobs::JobChannel::McpStatus,
            "mcp.status",
            None,
            Some("Connecting MCP".into()),
            true,
        );
    }

    fn drain_job_results(&mut self) {
        while let Some(outcome) = self.jobs.try_recv() {
            if !self.jobs.is_current(outcome.channel, outcome.generation) {
                continue;
            }
            let channel = outcome.channel;
            let generation = outcome.generation;
            match outcome.payload {
                crate::async_jobs::JobPayload::Rpc { method, result } => {
                    self.jobs.mark_done(channel, generation);
                    match result {
                        Ok(data) => self.apply_rpc_job_ok(channel, &method, data),
                        Err(err) => self.apply_rpc_job_err(channel, generation, &method, err),
                    }
                }
                crate::async_jobs::JobPayload::SessionPreview { session_id, result } => {
                    self.jobs.mark_done(channel, generation);
                    match result {
                        Ok(data) => self.apply_session_preview_data(&session_id, data),
                        Err(err) => {
                            self.jobs.push_error(
                                Some(channel),
                                Some(generation),
                                format!("Preview failed: {err}"),
                                true,
                                0,
                            );
                        }
                    }
                }
                crate::async_jobs::JobPayload::SessionResume { query, result } => {
                    self.jobs.mark_done(channel, generation);
                    match result {
                        Ok(data) => {
                            if let Err(e) = self.apply_session_resume_data(&query, data) {
                                self.jobs.push_error(
                                    Some(channel),
                                    Some(generation),
                                    format!("Resume failed: {e}"),
                                    true,
                                    0,
                                );
                            }
                        }
                        Err(err) => {
                            self.jobs.push_error(
                                Some(channel),
                                Some(generation),
                                format!("Resume failed: {err}"),
                                true,
                                0,
                            );
                        }
                    }
                }
                crate::async_jobs::JobPayload::SessionHistory {
                    session_id,
                    before,
                    result,
                } => {
                    self.jobs.mark_done(channel, generation);
                    match result {
                        Ok(data) => self.apply_session_history_page(&session_id, before, data),
                        Err(err) => {
                            self.state.history_loading = false;
                            self.jobs.push_error(
                                Some(channel),
                                Some(generation),
                                format!("History load failed: {err}"),
                                true,
                                0,
                            );
                        }
                    }
                }
                crate::async_jobs::JobPayload::Prompt {
                    session_id,
                    idempotency_key,
                    result,
                } => {
                    self.jobs.mark_done(channel, generation);
                    match result {
                        Ok(()) => {
                            self.jobs.mcp.waiting_for_prompt =
                                self.jobs.mcp.configured && !self.jobs.mcp.initialized;
                            if let Some(msg) = self.state.messages.iter_mut().rev().find(|m| {
                                m.role == MessageRole::User
                                    && m.idempotency_key.as_deref() == Some(idempotency_key.as_str())
                            }) {
                                msg.delivery = crate::prompt_queue::DeliveryState::Sent;
                            }
                            self.enqueue_workspace_sessions_refresh();
                            let _ = session_id;
                        }
                        Err(err) => {
                            self.state.status = SessionStatus::Idle;
                            self.jobs.mcp.waiting_for_prompt = false;
                            if let Some(msg) = self.state.messages.iter_mut().rev().find(|m| {
                                m.role == MessageRole::User
                                    && m.idempotency_key.as_deref() == Some(idempotency_key.as_str())
                            }) {
                                msg.delivery = crate::prompt_queue::DeliveryState::Failed;
                                // Restore draft for edit/retry without losing the failed bubble.
                                self.state.input.set_text(msg.content.clone());
                            }
                            self.jobs.push_error(
                                Some(channel),
                                Some(generation),
                                format!("Send failed: {err}"),
                                true,
                                0,
                            );
                            self.system_message(format!("Error: {err}"));
                        }
                    }
                }
            }
        }
    }

    fn apply_rpc_job_ok(
        &mut self,
        channel: crate::async_jobs::JobChannel,
        method: &str,
        data: serde_json::Value,
    ) {
        match channel {
            crate::async_jobs::JobChannel::SessionsList => {
                if method == "sessions.list"
                    && self
                        .state
                        .list_picker
                        .as_ref()
                        .is_some_and(|p| p.kind == ListPickerKind::Session)
                {
                    self.apply_session_picker_list(data);
                } else {
                    self.apply_workspace_sessions_list(Some(data));
                }
            }
            crate::async_jobs::JobChannel::SkillsList => {
                self.apply_skills_list(Some(data));
            }
            crate::async_jobs::JobChannel::McpStatus | crate::async_jobs::JobChannel::McpList => {
                self.jobs.apply_mcp_status(&data);
                if channel == crate::async_jobs::JobChannel::McpList {
                    // Manager open path may still want the picker rebuilt — handled by callers.
                }
            }
            crate::async_jobs::JobChannel::TasksList => {
                self.apply_tasks_list_data(data);
            }
            _ => {}
        }
    }

    fn apply_rpc_job_err(
        &mut self,
        channel: crate::async_jobs::JobChannel,
        generation: u64,
        method: &str,
        err: String,
    ) {
        // Background session refresh failures are soft — show retryable notice.
        let retryable = matches!(
            channel,
            crate::async_jobs::JobChannel::SessionsList
                | crate::async_jobs::JobChannel::McpStatus
                | crate::async_jobs::JobChannel::SkillsList
                | crate::async_jobs::JobChannel::TasksList
                | crate::async_jobs::JobChannel::SessionPreview
                | crate::async_jobs::JobChannel::SessionResume
                | crate::async_jobs::JobChannel::Prompt
        );
        self.jobs.push_error(
            Some(channel),
            Some(generation),
            format!("{method} failed: {err}"),
            retryable,
            0,
        );
        // Auto-backoff retry for list/status polls only.
        if matches!(
            channel,
            crate::async_jobs::JobChannel::SessionsList
                | crate::async_jobs::JobChannel::McpStatus
                | crate::async_jobs::JobChannel::SkillsList
        ) && self.jobs.can_auto_retry(0)
        {
            // Leave the error visible; user can press `r`, and the next periodic
            // poll will also retry after the pending slot clears.
        }
    }

    fn apply_tasks_list_data(&mut self, data: serde_json::Value) {
        let mut tasks = Vec::new();
        if let Some(arr) = data.get("tasks").and_then(|v| v.as_array()) {
            for t in arr {
                tasks.push(TaskInfo {
                    task_id: t
                        .get("task_id")
                        .or_else(|| t.get("id"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    description: t
                        .get("description")
                        .or_else(|| t.get("command"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    status: t
                        .get("status")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                        .to_string(),
                    result: t
                        .get("result")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string()),
                    error: t
                        .get("error")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string()),
                });
            }
        }
        self.state.tasks_panel = Some(TasksPanelState { tasks, selected: 0 });
    }

    /// Close the topmost transient UI (menus / pickers / search / btw / shell).
    /// Returns true if something was dismissed. Does not touch the agent turn.
    fn dismiss_transient_ui(&mut self) -> bool {
        if self.state.session_delete_confirm.take().is_some() {
            return true;
        }
        if self.state.list_picker.is_some() {
            self.pop_list_picker_level();
            return true;
        }
        if self.state.tasks_panel.take().is_some() {
            // Swarm (or similar) may have been pushed before opening the panel.
            if self.state.list_picker.is_none() {
                if let Some(prev) = self.state.list_picker_stack.pop() {
                    self.state.list_picker = Some(prev);
                }
            }
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

    /// Esc: return to parent picker if any, otherwise close.
    fn pop_list_picker_level(&mut self) {
        self.state.session_picker_preview = None;
        self.state.session_delete_confirm = None;
        if let Some(prev) = self.state.list_picker_stack.pop() {
            self.state.list_picker = Some(prev);
        } else {
            self.state.list_picker = None;
        }
    }

    fn clear_list_pickers(&mut self) {
        self.state.list_picker = None;
        self.state.list_picker_stack.clear();
        self.state.session_picker_preview = None;
        self.state.session_delete_confirm = None;
    }

    /// Replace the active picker without touching the stack (refresh same surface).
    fn replace_list_picker(&mut self, next: ListPickerState) {
        self.state.session_picker_preview = None;
        self.state.session_delete_confirm = None;
        self.state.list_picker = Some(next);
    }

    /// Reset picker navigation before opening a slash-command root surface.
    fn begin_root_picker(&mut self) {
        self.state.list_picker_stack.clear();
        self.state.session_picker_preview = None;
        self.state.session_delete_confirm = None;
    }

    fn flush_pending_scroll(&mut self, scroll_delta: &mut i32) {
        if *scroll_delta != 0 {
            self.state.scroll_lines(*scroll_delta);
            *scroll_delta = 0;
            // While dragging, remap focus to the same screen cell under a new scroll.
            if self.state.selection_dragging {
                self.update_selection_focus_from_last_mouse();
            }
        }
    }

    fn update_selection_focus_from_last_mouse(&mut self) {
        let Some((column, row)) = self.state.last_mouse else {
            return;
        };
        let fake = crossterm::event::MouseEvent {
            kind: MouseEventKind::Moved,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        if let Some(pos) = self.mouse_to_cell(&fake) {
            if let Some(sel) = self.state.selection.as_mut() {
                sel.focus = pos;
            }
        }
    }

    /// Map a mouse event to an absolute transcript cell, if inside the pane.
    fn mouse_to_cell(
        &self,
        mouse: &crossterm::event::MouseEvent,
    ) -> Option<crate::selection::CellPos> {
        let area = self.state.transcript_area;
        if area.width == 0 || area.height == 0 {
            return None;
        }
        if mouse.column < area.x
            || mouse.row < area.y
            || mouse.column >= area.x.saturating_add(area.width)
            || mouse.row >= area.y.saturating_add(area.height)
        {
            return None;
        }
        let local_x = mouse.column.saturating_sub(area.x);
        let local_y = mouse.row.saturating_sub(area.y);
        let max_scroll = self.state.max_scroll_up();
        let scroll_from_top = max_scroll.saturating_sub(self.state.scroll_up);
        let line = scroll_from_top as usize + local_y as usize;
        Some(crate::selection::CellPos { line, col: local_x })
    }

    fn clear_selection(&mut self) {
        self.state.selection = None;
        self.state.selection_dragging = false;
    }

    fn selection_copy_text(&self) -> Option<String> {
        let sel = self.state.selection?;
        let text = crate::selection::extract_text(&self.state.select_rows, sel);
        if text.is_empty() {
            None
        } else {
            Some(text)
        }
    }

    fn copy_selection_or_msg(&mut self) -> bool {
        let Some(text) = self.selection_copy_text() else {
            return false;
        };
        match copy_to_clipboard(&text) {
            Ok(()) => {
                let n = text.chars().count();
                self.system_message(format!("Copied {n} chars."));
                true
            }
            Err(e) => {
                self.system_message(format!("Copy failed: {e}"));
                true // still consume the copy shortcut — selection was intentional
            }
        }
    }

    fn collect_mouse(&mut self, mouse: crossterm::event::MouseEvent, scroll_delta: &mut i32) {
        self.state.last_mouse = Some((mouse.column, mouse.row));

        let over_strip = self.mouse_over_session_strip(&mouse);
        if over_strip {
            self.state.strip_hover_title = self
                .hit_session_strip(mouse.column)
                .map(|h| h.full_title.clone());
        } else {
            self.state.strip_hover_title = None;
        }

        match mouse.kind {
            MouseEventKind::ScrollUp if over_strip => {
                self.state.pending_strip_action = Some(StripAction::Cycle(-1));
                return;
            }
            MouseEventKind::ScrollDown if over_strip => {
                self.state.pending_strip_action = Some(StripAction::Cycle(1));
                return;
            }
            MouseEventKind::ScrollUp => {
                *scroll_delta = scroll_delta.saturating_add(3);
            }
            MouseEventKind::ScrollDown => {
                *scroll_delta = scroll_delta.saturating_sub(3);
            }
            MouseEventKind::Down(MouseButton::Left) if over_strip => {
                self.flush_pending_scroll(scroll_delta);
                if let Some(hit) = self.hit_session_strip(mouse.column) {
                    if self.state.session_id.as_deref() != Some(hit.session_id.as_str()) {
                        self.state.pending_strip_action =
                            Some(StripAction::Switch(hit.session_id.clone()));
                    }
                }
                self.clear_selection();
                return;
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.flush_pending_scroll(scroll_delta);
                if let Some(pos) = self.mouse_to_cell(&mouse) {
                    self.state.selection = Some(crate::selection::TextSelection::new(pos));
                    self.state.selection_dragging = true;
                } else {
                    // Click outside transcript clears selection.
                    self.clear_selection();
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if self.state.selection_dragging => {
                if let Some(pos) = self.mouse_to_cell(&mouse) {
                    if let Some(sel) = self.state.selection.as_mut() {
                        sel.focus = pos;
                    }
                }
            }
            MouseEventKind::Up(MouseButton::Left) if self.state.selection_dragging => {
                if let Some(pos) = self.mouse_to_cell(&mouse) {
                    if let Some(sel) = self.state.selection.as_mut() {
                        sel.focus = pos;
                    }
                }
                self.state.selection_dragging = false;
                // Plain click (no drag span) → clear; keep existing click semantics.
                let drop = match self.state.selection {
                    Some(s) if s.is_empty() => true,
                    Some(_) => self.selection_copy_text().is_none(),
                    None => true,
                };
                if drop {
                    self.clear_selection();
                }
            }
            _ => {}
        }
    }

    fn mouse_over_session_strip(&self, mouse: &crossterm::event::MouseEvent) -> bool {
        let area = self.state.footer_area;
        if area.width == 0 || area.height < 2 {
            return false;
        }
        // Session strip lives on the second footer row.
        let strip_row = area.y.saturating_add(1);
        mouse.row == strip_row
            && mouse.column >= area.x
            && mouse.column < area.x.saturating_add(area.width)
            && !self.state.session_strip_hits.is_empty()
    }

    fn hit_session_strip(&self, column: u16) -> Option<&crate::chrome::SessionStripHit> {
        let col = column as usize;
        self.state
            .session_strip_hits
            .iter()
            .find(|h| col >= h.x0 && col < h.x1)
    }

    async fn handle_key(&mut self, key: KeyEvent) -> anyhow::Result<()> {
        // Esc while a menu/overlay is open: only dismiss that UI — never interrupt
        // an in-flight turn. Ctrl-C still cancels the turn below.
        if matches!(key.code, KeyCode::Esc) && self.dismiss_transient_ui() {
            self.state.pending_esc_ms = None;
            self.state.quit_confirm = false;
            return Ok(());
        }

        // Close/delete session confirm (Tab strip Ctrl-D or /sessions Ctrl-D).
        if self.state.session_delete_confirm.is_some() {
            match key.code {
                KeyCode::Up | KeyCode::BackTab | KeyCode::Left => {
                    if let Some(ref mut confirm) = self.state.session_delete_confirm {
                        confirm.selected = 0;
                    }
                }
                KeyCode::Down | KeyCode::Tab | KeyCode::Right => {
                    if let Some(ref mut confirm) = self.state.session_delete_confirm {
                        confirm.selected = 1;
                    }
                }
                KeyCode::Enter => {
                    let yes = self
                        .state
                        .session_delete_confirm
                        .as_ref()
                        .is_some_and(|c| c.selected == 1);
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

        // Esc clears in-app selection before interrupt / other Esc handling.
        if matches!(key.code, KeyCode::Esc) && self.state.selection.is_some() {
            self.clear_selection();
            self.state.pending_esc_ms = None;
            self.state.quit_confirm = false;
            return Ok(());
        }

        // Platform copy shortcut with a non-empty selection copies; otherwise
        // fall through to interrupt / quit. macOS prefers ⌘C but also accepts
        // Ctrl+C (stock Terminal.app does not forward ⌘C); other platforms
        // use Ctrl+C.
        if crate::platform_keys::is_copy_shortcut(&key) && self.copy_selection_or_msg() {
            self.state.quit_confirm = false;
            return Ok(());
        }

        // Busy turn with no overlay: Esc / Ctrl-C interrupt the agent (always Ctrl-C
        // even on macOS so the key to stop a running turn is consistent).
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
            self.jobs.mcp.waiting_for_prompt = false;
            return Ok(());
        }

        // Retry the latest failed background RPC when the composer is empty.
        if matches!(key.code, KeyCode::Char('r') | KeyCode::Char('R'))
            && self.state.input.is_empty()
            && self.state.list_picker.is_none()
            && self.state.tasks_panel.is_none()
            && self.state.slash_menu.is_none()
            && self.state.file_menu.is_none()
            && self.state.approval_pending.is_none()
            && self.state.question_pending.is_none()
        {
            if let Some((channel, _gen, method, params, retry_count)) =
                self.jobs.take_retryable_error()
            {
                if self.jobs.can_auto_retry(retry_count) {
                    self.jobs.spawn_rpc(
                        self.client.requester(),
                        channel,
                        method,
                        params,
                        Some(format!("Retrying {}", channel.label())),
                        true,
                    );
                } else {
                    self.jobs.push_error(
                        Some(channel),
                        None,
                        format!("{}: retry limit reached", channel.label()),
                        false,
                        retry_count,
                    );
                }
                return Ok(());
            }
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
                        self.refresh_session_picker_preview();
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
                        self.refresh_session_picker_preview();
                    }
                    return Ok(());
                }
                KeyCode::Enter => {
                    if let Some(picker) = self.state.list_picker.as_ref() {
                        if matches!(
                            picker.kind,
                            ListPickerKind::SkillManage | ListPickerKind::McpManage
                        ) {
                            self.toggle_manage_picker().await?;
                            return Ok(());
                        }
                    }
                    self.apply_list_picker().await?;
                    return Ok(());
                }
                KeyCode::Esc => {
                    self.pop_list_picker_level();
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
                            let busy = self
                                .state
                                .tab_strip
                                .tabs
                                .iter()
                                .find(|t| t.id == item.id)
                                .is_some_and(|t| {
                                    matches!(
                                        t.status,
                                        SessionStatus::Thinking
                                            | SessionStatus::ToolExecuting
                                            | SessionStatus::WaitingApproval
                                            | SessionStatus::WaitingQuestion
                                            | SessionStatus::Compacting
                                    ) || t.dirty
                                });
                            self.state.session_delete_confirm = Some(SessionDeleteConfirm {
                                session_id: item.id.clone(),
                                label: item.label.clone(),
                                selected: 0, // default No
                                permanent: true,
                                busy,
                            });
                        }
                    }
                    return Ok(());
                }
                KeyCode::Backspace
                    if self
                        .state
                        .list_picker
                        .as_ref()
                        .is_some_and(|p| p.kind == ListPickerKind::Session) =>
                {
                    if let Some(ref mut p) = self.state.list_picker {
                        p.filter.pop();
                    }
                    self.apply_session_picker_filter();
                    self.refresh_session_picker_preview();
                    return Ok(());
                }
                KeyCode::Char(c)
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && !key.modifiers.contains(KeyModifiers::ALT)
                        && self
                            .state
                            .list_picker
                            .as_ref()
                            .is_some_and(|p| p.kind == ListPickerKind::Session) =>
                {
                    if let Some(ref mut p) = self.state.list_picker {
                        p.filter.push(c);
                    }
                    self.apply_session_picker_filter();
                    self.refresh_session_picker_preview();
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
            // Ctrl-D: close current multi-session tab (with confirm), else quit if empty
            KeyCode::Char('d')
                if key.modifiers.contains(KeyModifiers::CONTROL) && self.state.input.is_empty() =>
            {
                if self.can_close_current_session_tab() {
                    self.begin_close_current_session_confirm();
                } else if self.state.quit_confirm {
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
            // Ctrl+Shift-Tab: previous related session (real resume)
            KeyCode::BackTab if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.cycle_workspace_session(-1).await?;
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
                self.cycle_workspace_session(1).await?;
            }
            // Ctrl+Shift+N / Ctrl+N: jump to next session needing attention
            KeyCode::Char('n') | KeyCode::Char('N')
                if key.modifiers.contains(KeyModifiers::CONTROL) && self.state.input.is_empty() =>
            {
                self.cycle_attention_session().await?;
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
                "model"
                    | "sessions"
                    | "resume"
                    | "tasks"
                    | "task"
                    | "permission"
                    | "config"
                    | "provider"
                    | "providers"
                    | "effort"
                    | "thinking"
                    | "auth"
                    | "help"
                    | "h"
                    | "?"
                    | "info"
                    | "status"
                    | "usage"
                    | "prompts"
                    | "prompt"
                    | "experimental-flags"
                    | "flags"
                    | "plugins"
                    | "plugin"
                    | "swarm"
                    | "mcp"
                    | "skills"
            )
        {
            self.state.input.clear();
            self.state.list_picker_stack.clear();
            self.state.session_picker_preview = None;
            self.state.session_delete_confirm = None;
            match item.name.as_str() {
                "model" => self.open_model_picker(),
                "sessions" | "resume" => self.open_session_picker().await?,
                "tasks" | "task" => self.open_tasks_panel().await?,
                "permission" => self.open_permission_picker(),
                "config" => self.open_config_picker(),
                "provider" | "providers" => self.open_provider_picker(),
                "effort" | "thinking" => self.open_effort_picker(),
                "auth" => self.open_auth_picker(),
                "help" | "h" | "?" => self.open_help_picker(),
                "info" | "status" => self.open_status_picker(),
                "usage" => self.open_usage_picker(),
                "doctor" => self.open_doctor_picker().await?,
                "prompts" | "prompt" => self.open_prompts_picker(),
                "experimental-flags" | "flags" => self.open_flags_picker(),
                "plugins" | "plugin" => self.open_plugins_picker().await?,
                "swarm" => self.open_swarm_picker(),
                "mcp" => self.open_mcp_manager().await?,
                "skills" => self.open_skill_manager().await?,
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
                // Bind model to this session only; keep config default for /new.
                self.state.model_alias = Some(item.id.clone());
                if let Some(sid) = &self.state.session_id {
                    if let Err(e) = self.client.set_model(sid, &item.id).await {
                        self.system_message(format!("Failed to set model on server: {}", e));
                    }
                }
                self.system_message(format!("Model set to: {}", item.id));
                self.clear_list_pickers();
            }
            ListPickerKind::Session => {
                self.clear_list_pickers();
                self.resume_session(&item.id).await?;
            }
            ListPickerKind::Permission => {
                self.apply_permission_mode_id(&item.id).await?;
                self.clear_list_pickers();
            }
            ListPickerKind::Config => match item.id.as_str() {
                "reload" => {
                    self.reload_config_from_disk();
                    self.open_config_picker();
                }
                id => {
                    self.state.list_picker_stack.push(picker);
                    match id {
                        "model" => self.open_model_picker(),
                        "permission" => self.open_permission_picker(),
                        "effort" => self.open_effort_picker(),
                        "provider" => self.open_provider_picker(),
                        "auth" => self.open_auth_picker(),
                        "mcp" => self.open_mcp_manager().await?,
                        "skills" => self.open_skill_manager().await?,
                        "status" => self.open_status_picker(),
                        _ => {
                            if let Some(prev) = self.state.list_picker_stack.pop() {
                                self.state.list_picker = Some(prev);
                            }
                        }
                    }
                }
            },
            ListPickerKind::Provider => {
                self.state.list_picker_stack.push(picker);
                self.open_model_picker_for_provider(&item.id);
            }
            ListPickerKind::Effort => {
                self.apply_effort_level(&item.id);
                self.clear_list_pickers();
            }
            ListPickerKind::Browse => {
                // Enter = same as Esc: back to parent if any.
                if let Some(prev) = self.state.list_picker_stack.pop() {
                    self.state.list_picker = Some(prev);
                }
            }
            ListPickerKind::Help => {
                let nested = matches!(
                    item.id.as_str(),
                    "__shortcuts__"
                        | "model"
                        | "permission"
                        | "config"
                        | "provider"
                        | "providers"
                        | "effort"
                        | "thinking"
                        | "auth"
                        | "info"
                        | "status"
                        | "usage"
                        | "version"
                        | "prompts"
                        | "prompt"
                        | "experimental-flags"
                        | "flags"
                        | "sessions"
                        | "resume"
                        | "tasks"
                        | "task"
                        | "mcp"
                        | "skills"
                        | "swarm"
                        | "plugins"
                        | "plugin"
                );
                if nested {
                    self.state.list_picker_stack.push(picker);
                    self.apply_help_command(&item.id).await?;
                } else if item.id == "reload" {
                    self.state.list_picker_stack.push(picker);
                    self.reload_config_from_disk();
                    self.pop_list_picker_level();
                } else {
                    self.clear_list_pickers();
                    self.apply_help_command(&item.id).await?;
                }
            }
            ListPickerKind::Prompts => {
                self.apply_prompt_template(&item.id);
                self.clear_list_pickers();
            }
            ListPickerKind::Swarm => match item.id.as_str() {
                "enter" | "exit" => {
                    self.apply_swarm_action(&item.id).await?;
                    self.clear_list_pickers();
                }
                "tasks" => {
                    self.state.list_picker_stack.push(picker);
                    self.open_tasks_panel().await?;
                }
                _ => {}
            },
            ListPickerKind::SkillManage | ListPickerKind::McpManage => {
                // Enter is handled by toggle_manage_picker; keep picker open.
                self.state.list_picker = Some(picker);
            }
        }
        Ok(())
    }

    async fn toggle_manage_picker(&mut self) -> anyhow::Result<()> {
        let Some(picker) = self.state.list_picker.as_ref() else {
            return Ok(());
        };
        if picker.items.is_empty() {
            return Ok(());
        }
        let idx = picker.selected.min(picker.items.len() - 1);
        let item = picker.items[idx].clone();
        let kind = picker.kind.clone();
        // detail encodes current enabled as "on" / "off" prefix.
        let currently_on = item.detail.starts_with("on");
        let enable = !currently_on;
        match kind {
            ListPickerKind::SkillManage => {
                let params = serde_json::json!({"name": item.id, "enabled": enable});
                match self
                    .client
                    .rpc_call("skills.set_enabled", Some(params))
                    .await
                {
                    Ok(_) => {
                        self.config.set_skill_disabled(&item.id, !enable);
                        let _ = self.refresh_skill_commands().await;
                        self.open_skill_manager().await?;
                        self.system_message(format!(
                            "Skill {} {}",
                            item.id,
                            if enable {
                                "enabled"
                            } else {
                                "disabled (saved)"
                            }
                        ));
                    }
                    Err(e) => self.system_message(format!("Failed to update skill: {e}")),
                }
            }
            ListPickerKind::McpManage => {
                let params = serde_json::json!({"name": item.id, "enabled": enable});
                match self.client.rpc_call("mcp.set_enabled", Some(params)).await {
                    Ok(_) => {
                        self.config.set_mcp_disabled(&item.id, !enable);
                        self.open_mcp_manager().await?;
                        self.system_message(format!(
                            "MCP {} {}",
                            item.id,
                            if enable {
                                "enabled"
                            } else {
                                "disabled (saved)"
                            }
                        ));
                    }
                    Err(e) => self.system_message(format!("Failed to update MCP: {e}")),
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn open_skill_manager(&mut self) -> anyhow::Result<()> {
        let mut items = Vec::new();
        match self.client.rpc_call("skills.list", None).await {
            Ok(data) => {
                if let Some(arr) = data.get("skills").and_then(|v| v.as_array()) {
                    for s in arr {
                        let name = s
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        if name.is_empty() {
                            continue;
                        }
                        let enabled = s.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
                        let desc = s
                            .get("description")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .trim();
                        let mark = if enabled { "●" } else { "○" };
                        let state = if enabled { "on" } else { "off" };
                        items.push(ListPickerItem {
                            id: name.clone(),
                            label: format!("{mark} {name}"),
                            detail: if desc.is_empty() {
                                state.to_string()
                            } else {
                                format!("{state}  {desc}")
                            },
                        });
                    }
                }
            }
            Err(e) => {
                self.system_message(format!("skills: {e}"));
                return Ok(());
            }
        }
        self.replace_list_picker(ListPickerState {
            kind: ListPickerKind::SkillManage,
            title: " Skills — Enter toggle · Esc back ".into(),
            selected: {
                let prev = self
                    .state
                    .list_picker
                    .as_ref()
                    .filter(|p| p.kind == ListPickerKind::SkillManage)
                    .map(|p| p.selected)
                    .unwrap_or(0);
                if items.is_empty() {
                    0
                } else {
                    prev.min(items.len() - 1)
                }
            },
            items,

            filter: String::new(),
            all_items: Vec::new(),
        });
        Ok(())
    }

    async fn open_mcp_manager(&mut self) -> anyhow::Result<()> {
        let mut items = Vec::new();
        match self.client.rpc_call("mcp.list", None).await {
            Ok(data) => {
                if let Some(arr) = data.get("servers").and_then(|v| v.as_array()) {
                    for s in arr {
                        let name = s
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        if name.is_empty() {
                            continue;
                        }
                        let enabled = s.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
                        let connected = s
                            .get("connected")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        let transport = s.get("transport").and_then(|v| v.as_str()).unwrap_or("?");
                        let mark = if enabled { "●" } else { "○" };
                        let state = if enabled { "on" } else { "off" };
                        let link = if connected { "connected" } else { "idle" };
                        items.push(ListPickerItem {
                            id: name.clone(),
                            label: format!("{mark} {name}"),
                            detail: format!("{state}  {transport} · {link}"),
                        });
                    }
                }
            }
            Err(e) => {
                // Fallback: show config entries if RPC missing.
                for (name, cfg) in &self.config.mcp_servers {
                    let enabled = !self.config.is_mcp_disabled(name);
                    let mark = if enabled { "●" } else { "○" };
                    let state = if enabled { "on" } else { "off" };
                    let kind = cfg
                        .transport_type
                        .as_deref()
                        .unwrap_or(if cfg.url.is_some() { "http" } else { "stdio" });
                    items.push(ListPickerItem {
                        id: name.clone(),
                        label: format!("{mark} {name}"),
                        detail: format!("{state}  {kind} (local config; server: {e})"),
                    });
                }
                if items.is_empty() {
                    self.system_message(format!("mcp: {e}"));
                    return Ok(());
                }
            }
        }
        self.replace_list_picker(ListPickerState {
            kind: ListPickerKind::McpManage,
            title: " MCP servers — Enter toggle · Esc back ".into(),
            selected: {
                let prev = self
                    .state
                    .list_picker
                    .as_ref()
                    .filter(|p| p.kind == ListPickerKind::McpManage)
                    .map(|p| p.selected)
                    .unwrap_or(0);
                if items.is_empty() {
                    0
                } else {
                    prev.min(items.len() - 1)
                }
            },
            items,

            filter: String::new(),
            all_items: Vec::new(),
        });
        Ok(())
    }

    async fn apply_permission_mode_id(&mut self, id: &str) -> anyhow::Result<()> {
        let new_mode: PermissionMode = id.parse().map_err(|e: String| anyhow::anyhow!(e))?;
        if new_mode == self.state.permission_mode {
            self.system_message(format!("Permission mode already: {new_mode}"));
            return Ok(());
        }
        let previous = self.state.permission_mode;
        self.state.permission_mode = new_mode;
        if let Some(sid) = &self.state.session_id {
            if let Err(e) = self.client.set_permission_mode(sid, new_mode).await {
                self.state.permission_mode = previous;
                self.system_message(format!("Permission update failed, rolled back: {e}"));
                return Ok(());
            }
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
        self.replace_list_picker(ListPickerState {
            kind: ListPickerKind::Permission,
            title: " Select permission mode ".into(),
            items,
            selected,

            filter: String::new(),
            all_items: Vec::new(),
        });
    }

    fn open_model_picker(&mut self) {
        self.open_model_picker_for_provider("");
    }

    fn open_model_picker_for_provider(&mut self, provider: &str) {
        let current = self
            .state
            .model_alias
            .clone()
            .or_else(|| self.config.default_model_alias().map(|s| s.to_string()))
            .unwrap_or_default();
        let mut names: Vec<_> = self
            .config
            .models
            .iter()
            .filter(|(_, m)| provider.is_empty() || m.provider == provider)
            .map(|(name, _)| name.clone())
            .collect();
        names.sort();
        let mut selected = 0;
        let items: Vec<ListPickerItem> = names
            .into_iter()
            .enumerate()
            .map(|(i, name)| {
                if name == current {
                    selected = i;
                }
                let (detail, label) = self
                    .config
                    .resolve_model(&name)
                    .map(|(m, _)| {
                        let detail = if provider.is_empty() {
                            format!("{} · {}", m.provider, m.model)
                        } else {
                            m.model.clone()
                        };
                        (detail, name.clone())
                    })
                    .unwrap_or_else(|| (String::new(), name.clone()));
                ListPickerItem {
                    id: name,
                    label,
                    detail,
                }
            })
            .collect();
        let title = if provider.is_empty() {
            " Select model ".into()
        } else {
            format!(" Models · {provider} ")
        };
        self.replace_list_picker(ListPickerState {
            kind: ListPickerKind::Model,
            title,
            items,
            selected,

            filter: String::new(),
            all_items: Vec::new(),
        });
    }

    fn open_provider_picker(&mut self) {
        let mut names: Vec<_> = self.config.providers.keys().cloned().collect();
        names.sort();
        let current_provider = self
            .state
            .model_alias
            .as_deref()
            .or_else(|| self.config.default_model_alias())
            .and_then(|a| self.config.models.get(a))
            .map(|m| m.provider.clone());
        let mut selected = 0;
        let items: Vec<ListPickerItem> = names
            .into_iter()
            .enumerate()
            .map(|(i, name)| {
                if current_provider.as_deref() == Some(name.as_str()) {
                    selected = i;
                }
                let p = &self.config.providers[&name];
                let model_count = self
                    .config
                    .models
                    .values()
                    .filter(|m| m.provider == name)
                    .count();
                let has_key = p.api_key.as_ref().is_some_and(|k| !k.is_empty());
                ListPickerItem {
                    id: name.clone(),
                    label: name,
                    detail: format!(
                        "{} · {} model{} · key:{}",
                        p.provider_type,
                        model_count,
                        if model_count == 1 { "" } else { "s" },
                        if has_key { "set" } else { "missing" }
                    ),
                }
            })
            .collect();
        self.replace_list_picker(ListPickerState {
            kind: ListPickerKind::Provider,
            title: " Select provider ".into(),
            items,
            selected,

            filter: String::new(),
            all_items: Vec::new(),
        });
    }

    fn open_config_picker(&mut self) {
        let model = self
            .state
            .model_alias
            .as_deref()
            .or_else(|| self.config.default_model_alias())
            .unwrap_or("-");
        let effort = self
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
        let items = vec![
            ListPickerItem {
                id: "model".into(),
                label: "Model".into(),
                detail: model.to_string(),
            },
            ListPickerItem {
                id: "permission".into(),
                label: "Permission".into(),
                detail: self.state.permission_mode.to_string(),
            },
            ListPickerItem {
                id: "effort".into(),
                label: "Thinking".into(),
                detail: effort,
            },
            ListPickerItem {
                id: "provider".into(),
                label: "Providers".into(),
                detail: format!("{} configured", self.config.providers.len()),
            },
            ListPickerItem {
                id: "auth".into(),
                label: "Auth".into(),
                detail: "API key status".into(),
            },
            ListPickerItem {
                id: "mcp".into(),
                label: "MCP servers".into(),
                detail: format!("{} configured", self.config.mcp_servers.len()),
            },
            ListPickerItem {
                id: "skills".into(),
                label: "Skills".into(),
                detail: "enable / disable".into(),
            },
            ListPickerItem {
                id: "status".into(),
                label: "Session info".into(),
                detail: format!(
                    "{} msgs · ~{} tok",
                    self.state.messages.len(),
                    self.state.approx_tokens
                ),
            },
            ListPickerItem {
                id: "reload".into(),
                label: "Reload config".into(),
                detail: "from disk (in-process)".into(),
            },
        ];
        self.replace_list_picker(ListPickerState {
            kind: ListPickerKind::Config,
            title: " Configuration ".into(),
            items,
            selected: 0,

            filter: String::new(),
            all_items: Vec::new(),
        });
    }

    fn open_effort_picker(&mut self) {
        let current = self
            .config
            .thinking
            .as_ref()
            .map(|t| {
                if !t.enabled {
                    "off".to_string()
                } else {
                    t.effort
                        .clone()
                        .unwrap_or_else(|| "high".into())
                        .to_lowercase()
                }
            })
            .unwrap_or_else(|| "off".into());
        let levels = ["off", "low", "medium", "high"];
        let mut selected = 0;
        let items: Vec<ListPickerItem> = levels
            .iter()
            .enumerate()
            .map(|(i, level)| {
                if *level == current {
                    selected = i;
                }
                let detail = match *level {
                    "off" => "disable thinking",
                    "low" => "light reasoning",
                    "medium" => "balanced",
                    "high" => "deep reasoning",
                    _ => "",
                };
                ListPickerItem {
                    id: (*level).into(),
                    label: (*level).into(),
                    detail: detail.into(),
                }
            })
            .collect();
        self.replace_list_picker(ListPickerState {
            kind: ListPickerKind::Effort,
            title: " Thinking effort ".into(),
            items,
            selected,

            filter: String::new(),
            all_items: Vec::new(),
        });
    }

    fn apply_effort_level(&mut self, level: &str) {
        match level {
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
            "low" | "medium" | "high" => {
                self.config.thinking = Some(kkagent_config::ThinkingConfig {
                    enabled: true,
                    effort: Some(level.to_string()),
                    keep: None,
                });
                self.system_message(format!("Thinking: on ({level})"));
            }
            _ => self.system_message(format!("Unknown effort level: {level}")),
        }
    }

    fn open_auth_picker(&mut self) {
        let mut names: Vec<_> = self.config.providers.keys().cloned().collect();
        names.sort();
        let items: Vec<ListPickerItem> = names
            .into_iter()
            .map(|name| {
                let p = &self.config.providers[&name];
                let has_key = p.api_key.as_ref().is_some_and(|k| !k.is_empty());
                ListPickerItem {
                    id: name.clone(),
                    label: name,
                    detail: format!(
                        "type={} · api_key={}",
                        p.provider_type,
                        if has_key { "set" } else { "missing" }
                    ),
                }
            })
            .collect();
        self.replace_list_picker(ListPickerState {
            kind: ListPickerKind::Browse,
            title: " Auth status ".into(),
            items,
            selected: 0,

            filter: String::new(),
            all_items: Vec::new(),
        });
    }

    fn open_usage_picker(&mut self) {
        let u = &self.state.usage_session;
        let model = self
            .state
            .model_alias
            .clone()
            .or_else(|| self.config.default_model_alias().map(|s| s.to_string()))
            .unwrap_or_else(|| "-".into());
        let (in_price, out_price, cache_c, cache_r, generic) =
            model_pricing(&self.config, model.as_str());
        let cost = estimate_usd(u, in_price, out_price, cache_c, cache_r);
        let mut items = vec![
            ListPickerItem {
                id: "model".into(),
                label: "Model".into(),
                detail: model,
            },
            ListPickerItem {
                id: "input".into(),
                label: "Input tokens".into(),
                detail: u.input_tokens.to_string(),
            },
            ListPickerItem {
                id: "output".into(),
                label: "Output tokens".into(),
                detail: u.output_tokens.to_string(),
            },
            ListPickerItem {
                id: "cache_c".into(),
                label: "Cache creation".into(),
                detail: u.cache_creation_tokens.to_string(),
            },
            ListPickerItem {
                id: "cache_r".into(),
                label: "Cache read".into(),
                detail: u.cache_read_tokens.to_string(),
            },
            ListPickerItem {
                id: "cost".into(),
                label: if generic {
                    "Est. cost (generic)".into()
                } else {
                    "Est. cost".into()
                },
                detail: format!("${cost:.4}"),
            },
        ];
        for (i, turn) in self.state.usage_turns.iter().rev().take(8).enumerate() {
            items.push(ListPickerItem {
                id: format!("turn{i}"),
                label: format!(
                    "Turn −{}",
                    i + 1
                ),
                detail: format!(
                    "in={} out={} · {}ms",
                    turn.input_tokens, turn.output_tokens, turn.duration_ms
                ),
            });
        }
        self.replace_list_picker(ListPickerState {
            kind: ListPickerKind::Browse,
            title: " /usage ".into(),
            items,
            selected: 0,
            filter: String::new(),
            all_items: Vec::new(),
        });
    }

    async fn open_doctor_picker(&mut self) -> anyhow::Result<()> {
        let path = kkagent_config::default_config_path();
        let mut items = vec![ListPickerItem {
            id: "config".into(),
            label: "Config".into(),
            detail: path.display().to_string(),
        }];
        let web_configured = self
            .config
            .services
            .as_ref()
            .map(|s| s.web_search.is_some() || s.moonshot_search.is_some())
            .unwrap_or(false);
        items.push(ListPickerItem {
            id: "web".into(),
            label: "WebSearch".into(),
            detail: if web_configured {
                "ok — configured".into()
            } else {
                "warning — [services.web_search] missing".into()
            },
        });
        items.push(ListPickerItem {
            id: "model".into(),
            label: "Default model".into(),
            detail: self
                .config
                .default_model_alias()
                .unwrap_or("-")
                .to_string(),
        });
        items.push(ListPickerItem {
            id: "hint".into(),
            label: "CLI".into(),
            detail: "kkagent doctor — full checks".into(),
        });
        self.replace_list_picker(ListPickerState {
            kind: ListPickerKind::Browse,
            title: " /doctor ".into(),
            items,
            selected: 0,
            filter: String::new(),
            all_items: Vec::new(),
        });
        Ok(())
    }

    fn open_status_picker(&mut self) {
        let model = self
            .state
            .model_alias
            .as_deref()
            .or_else(|| self.config.default_model_alias())
            .unwrap_or("-");
        let sid = self.state.session_id.as_deref().unwrap_or("-");
        let sid_short = &sid[..8.min(sid.len())];
        let max = self
            .state
            .model_alias
            .as_deref()
            .or_else(|| self.config.default_model_alias())
            .and_then(|a| self.config.resolve_model(a))
            .and_then(|(m, _)| m.max_context_size)
            .unwrap_or(256_000);
        let used = self.state.approx_tokens;
        let pct = used
            .saturating_mul(100)
            .checked_div(max)
            .unwrap_or(0)
            .min(100);
        let home = dirs::home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join(".kkagent");
        let items = vec![
            ListPickerItem {
                id: "version".into(),
                label: "Version".into(),
                detail: format!("kkagent {}", env!("CARGO_PKG_VERSION")),
            },
            ListPickerItem {
                id: "session".into(),
                label: "Session".into(),
                detail: sid_short.to_string(),
            },
            ListPickerItem {
                id: "model".into(),
                label: "Model".into(),
                detail: model.to_string(),
            },
            ListPickerItem {
                id: "permission".into(),
                label: "Permission".into(),
                detail: self.state.permission_mode.to_string(),
            },
            ListPickerItem {
                id: "plan".into(),
                label: "Plan mode".into(),
                detail: if self.state.plan_mode { "on" } else { "off" }.into(),
            },
            ListPickerItem {
                id: "status".into(),
                label: "Status".into(),
                detail: format!("{:?}", self.state.status),
            },
            ListPickerItem {
                id: "messages".into(),
                label: "Messages".into(),
                detail: self.state.messages.len().to_string(),
            },
            ListPickerItem {
                id: "usage".into(),
                label: "Context".into(),
                detail: format!("{pct}% ({used}/{max})"),
            },
            ListPickerItem {
                id: "config_dir".into(),
                label: "Config dir".into(),
                detail: home.display().to_string(),
            },
        ];
        self.replace_list_picker(ListPickerState {
            kind: ListPickerKind::Browse,
            title: " Session / system ".into(),
            items,
            selected: 0,

            filter: String::new(),
            all_items: Vec::new(),
        });
    }

    fn open_help_picker(&mut self) {
        let mut items = vec![ListPickerItem {
            id: "__shortcuts__".into(),
            label: "Keyboard shortcuts".into(),
            detail: "Enter · Esc · Tab · Ctrl-F · …".into(),
        }];
        let mut cmds: Vec<_> = crate::slash::BUILTIN_SLASH_COMMANDS.iter().collect();
        cmds.sort_by(|a, b| b.priority.cmp(&a.priority).then(a.name.cmp(b.name)));
        items.extend(cmds.into_iter().map(|c| ListPickerItem {
            id: c.name.to_string(),
            label: format!("/{}", c.name),
            detail: c.description.to_string(),
        }));
        self.replace_list_picker(ListPickerState {
            kind: ListPickerKind::Help,
            title: " Commands ".into(),
            items,
            selected: 0,

            filter: String::new(),
            all_items: Vec::new(),
        });
    }

    fn open_shortcuts_picker(&mut self) {
        let copy = crate::platform_keys::copy_shortcut_label();
        let items = vec![
            ListPickerItem {
                id: "enter".into(),
                label: "Enter".into(),
                detail: "Submit / confirm slash".into(),
            },
            ListPickerItem {
                id: "tab".into(),
                label: "Tab / ← →".into(),
                detail: "Empty input: cycle related sessions".into(),
            },
            ListPickerItem {
                id: "ctrl_d".into(),
                label: "Ctrl-D".into(),
                detail: "Close session tab / quit if empty".into(),
            },
            ListPickerItem {
                id: "shift_tab".into(),
                label: "Shift-Tab".into(),
                detail: "Toggle plan mode".into(),
            },
            ListPickerItem {
                id: "arrows".into(),
                label: "↑ ↓".into(),
                detail: "Input history / menus".into(),
            },
            ListPickerItem {
                id: "scroll".into(),
                label: "PgUp / PgDn / wheel".into(),
                detail: "Scroll transcript".into(),
            },
            ListPickerItem {
                id: "select".into(),
                label: "Drag select".into(),
                detail: format!("Select text; {copy} copies"),
            },
            ListPickerItem {
                id: "esc".into(),
                label: "Esc".into(),
                detail: "Close overlay / interrupt / Esc Esc undo".into(),
            },
            ListPickerItem {
                id: "shell".into(),
                label: "!".into(),
                detail: "Shell mode".into(),
            },
            ListPickerItem {
                id: "at".into(),
                label: "@".into(),
                detail: "File path picker".into(),
            },
            ListPickerItem {
                id: "search".into(),
                label: "Ctrl-F / Ctrl-S".into(),
                detail: "Search transcript".into(),
            },
            ListPickerItem {
                id: "btw".into(),
                label: "Ctrl-G".into(),
                detail: "Toggle /btw side pane".into(),
            },
            ListPickerItem {
                id: "history".into(),
                label: "Ctrl-O / Ctrl-T / Ctrl-P/N".into(),
                detail: "Tool history · todos · input history".into(),
            },
        ];
        self.replace_list_picker(ListPickerState {
            kind: ListPickerKind::Browse,
            title: " Keyboard shortcuts ".into(),
            items,
            selected: 0,

            filter: String::new(),
            all_items: Vec::new(),
        });
    }

    async fn apply_help_command(&mut self, name: &str) -> anyhow::Result<()> {
        // Prefer opening the real UI for picker-backed commands.
        match name {
            "__shortcuts__" => self.open_shortcuts_picker(),
            "model" => self.open_model_picker(),
            "permission" => self.open_permission_picker(),
            "config" => self.open_config_picker(),
            "provider" | "providers" => self.open_provider_picker(),
            "effort" | "thinking" => self.open_effort_picker(),
            "auth" => self.open_auth_picker(),
            "info" | "status" | "version" => self.open_status_picker(),
            "usage" => self.open_usage_picker(),
            "doctor" => {
                self.open_doctor_picker().await?;
            }
            "prompts" | "prompt" => self.open_prompts_picker(),
            "experimental-flags" | "flags" => self.open_flags_picker(),
            "sessions" | "resume" => self.open_session_picker().await?,
            "tasks" | "task" => self.open_tasks_panel().await?,
            "mcp" => self.open_mcp_manager().await?,
            "skills" => self.open_skill_manager().await?,
            "swarm" => self.open_swarm_picker(),
            "plugins" | "plugin" => self.open_plugins_picker().await?,
            "reload" => self.reload_config_from_disk(),
            other => {
                let hint = find_slash_command(other)
                    .and_then(|c| c.argument_hint)
                    .unwrap_or("");
                if hint.is_empty() {
                    self.state.input.set_text(format!("/{other}"));
                } else {
                    self.state.input.set_text(format!("/{other} "));
                }
                self.state.refresh_slash_menu();
            }
        }
        Ok(())
    }

    fn open_prompts_picker(&mut self) {
        let items = vec![
            ListPickerItem {
                id: "init".into(),
                label: "/init".into(),
                detail: "Generate / update AGENTS.md".into(),
            },
            ListPickerItem {
                id: "compact".into(),
                label: "/compact".into(),
                detail: "Compress conversation context".into(),
            },
            ListPickerItem {
                id: "goal".into(),
                label: "/goal".into(),
                detail: "Start an autonomous goal".into(),
            },
            ListPickerItem {
                id: "web".into(),
                label: "/web".into(),
                detail: "Queue a web search prompt".into(),
            },
        ];
        self.replace_list_picker(ListPickerState {
            kind: ListPickerKind::Prompts,
            title: " Prompt templates ".into(),
            items,
            selected: 0,

            filter: String::new(),
            all_items: Vec::new(),
        });
    }

    fn apply_prompt_template(&mut self, id: &str) {
        match id {
            "init" => {
                self.state.pending_prompt = Some(
                    "Analyze this codebase and create or update AGENTS.md with project conventions, build/test commands, and important paths.".into(),
                );
            }
            "compact" => {
                self.state.input.set_text("/compact ".into());
            }
            "goal" => {
                self.state.input.set_text("/goal ".into());
            }
            "web" => {
                self.state.input.set_text("/web ".into());
            }
            _ => {}
        }
    }

    fn open_flags_picker(&mut self) {
        let auto_compact = self
            .config
            .loop_control
            .as_ref()
            .map(|l| format!("{:?}", l.auto_compact))
            .unwrap_or_else(|| "-".into());
        let items = vec![
            ListPickerItem {
                id: "git_worktree".into(),
                label: "KKAGENT_GIT_WORKTREE".into(),
                detail: std::env::var("KKAGENT_GIT_WORKTREE").unwrap_or_else(|_| "0".into()),
            },
            ListPickerItem {
                id: "telemetry_cloud".into(),
                label: "KKAGENT_TELEMETRY_CLOUD".into(),
                detail: std::env::var("KKAGENT_TELEMETRY_CLOUD").unwrap_or_else(|_| "0".into()),
            },
            ListPickerItem {
                id: "auto_compact".into(),
                label: "auto_compact".into(),
                detail: auto_compact,
            },
        ];
        self.replace_list_picker(ListPickerState {
            kind: ListPickerKind::Browse,
            title: " Experimental flags ".into(),
            items,
            selected: 0,

            filter: String::new(),
            all_items: Vec::new(),
        });
    }

    async fn open_plugins_picker(&mut self) -> anyhow::Result<()> {
        let mut items = Vec::new();
        if let Ok(v) = self.client.rpc_call("plugins.list", None).await {
            if let Some(arr) = v.get("plugins").and_then(|p| p.as_array()) {
                for p in arr {
                    let name = p
                        .get("name")
                        .or_else(|| p.get("id"))
                        .and_then(|x| x.as_str())
                        .unwrap_or("plugin")
                        .to_string();
                    let detail = p
                        .get("path")
                        .or_else(|| p.get("version"))
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    items.push(ListPickerItem {
                        id: name.clone(),
                        label: name,
                        detail,
                    });
                }
            } else if let Some(obj) = v.as_object() {
                for (k, val) in obj {
                    items.push(ListPickerItem {
                        id: k.clone(),
                        label: k.clone(),
                        detail: val.to_string(),
                    });
                }
            }
        }
        if items.is_empty() {
            let dir = kkagent_config::default_config_dir().join("plugins");
            items.push(ListPickerItem {
                id: "hint".into(),
                label: "(none loaded)".into(),
                detail: format!("drop plugin.json under {}", dir.display()),
            });
        }
        self.replace_list_picker(ListPickerState {
            kind: ListPickerKind::Browse,
            title: " Plugins ".into(),
            items,
            selected: 0,

            filter: String::new(),
            all_items: Vec::new(),
        });
        Ok(())
    }

    fn open_swarm_picker(&mut self) {
        let items = vec![
            ListPickerItem {
                id: "enter".into(),
                label: "Enter swarm".into(),
                detail: "enable multi-agent mode".into(),
            },
            ListPickerItem {
                id: "exit".into(),
                label: "Exit swarm".into(),
                detail: "return to single-agent".into(),
            },
            ListPickerItem {
                id: "tasks".into(),
                label: "Background tasks".into(),
                detail: "view subagents / tasks".into(),
            },
        ];
        self.replace_list_picker(ListPickerState {
            kind: ListPickerKind::Swarm,
            title: " Swarm ".into(),
            items,
            selected: 0,

            filter: String::new(),
            all_items: Vec::new(),
        });
    }

    async fn apply_swarm_action(&mut self, action: &str) -> anyhow::Result<()> {
        match action {
            "enter" => {
                let mut params = serde_json::json!({ "trigger": "slash" });
                if let Some(sid) = &self.state.session_id {
                    params["session_id"] = serde_json::json!(sid);
                }
                match self.client.rpc_call("swarm.enter", Some(params)).await {
                    Ok(_) => self.system_message("Swarm mode ON".into()),
                    Err(e) => self.system_message(format!("swarm enter failed: {e}")),
                }
            }
            "exit" => {
                let params = self
                    .state
                    .session_id
                    .as_ref()
                    .map(|sid| serde_json::json!({ "session_id": sid }));
                match self.client.rpc_call("swarm.exit", params).await {
                    Ok(_) => self.system_message("Swarm mode OFF".into()),
                    Err(e) => self.system_message(format!("swarm exit failed: {e}")),
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn reload_config_from_disk(&mut self) {
        match kkagent_config::load_config(None) {
            Ok(config) => {
                self.config = config;
                self.system_message(
                    "Config reloaded from disk. Server-side MCP/hooks may need a restart.".into(),
                );
            }
            Err(e) => self.system_message(format!("Reload failed: {e}")),
        }
    }

    async fn open_session_picker(&mut self) -> anyhow::Result<()> {
        // Open a placeholder immediately; list fills when the background job returns.
        if !self
            .state
            .list_picker
            .as_ref()
            .is_some_and(|p| p.kind == ListPickerKind::Session)
        {
            self.replace_list_picker(ListPickerState {
                kind: ListPickerKind::Session,
                title: " Sessions (this workspace)  ↑↓  Enter open  Ctrl-D delete ".into(),
                items: Vec::new(),
                selected: 0,

                filter: String::new(),
                all_items: Vec::new(),
            });
        }
        self.state.session_delete_confirm = None;
        self.jobs.spawn_rpc(
            self.client.requester(),
            crate::async_jobs::JobChannel::SessionsList,
            "sessions.list",
            Some(serde_json::json!({"limit": 80})),
            Some("Loading sessions".into()),
            true,
        );
        Ok(())
    }

    fn apply_session_picker_list(&mut self, data: serde_json::Value) {
        let cwd = std::env::current_dir()
            .ok()
            .and_then(|p| std::fs::canonicalize(&p).ok())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let cwd_key = cwd.to_string_lossy().to_string();
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
                let work = s.get("working_dir").and_then(|v| v.as_str()).unwrap_or("");
                let same_workspace = if work.is_empty() {
                    false
                } else {
                    std::fs::canonicalize(work)
                        .map(|p| p.to_string_lossy() == cwd_key)
                        .unwrap_or_else(|_| {
                            std::path::Path::new(work) == cwd || work == cwd_key || work == "."
                        })
                };
                if !same_workspace {
                    continue;
                }
                let empty = s.get("empty").and_then(|v| v.as_bool()).unwrap_or_else(|| {
                    s.get("message_count").and_then(|v| v.as_u64()).unwrap_or(0) == 0
                        && s.get("last_prompt")
                            .and_then(|v| v.as_str())
                            .map(|p| {
                                p.trim().is_empty()
                                    || kkagent_protocol::is_harness_only_user_text(p)
                            })
                            .unwrap_or(true)
                });
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
            return;
        }
        let selected = current
            .as_ref()
            .and_then(|c| items.iter().position(|i| &i.id == c))
            .unwrap_or(0);
        let prior_filter = self
            .state
            .list_picker
            .as_ref()
            .filter(|p| p.kind == ListPickerKind::Session)
            .map(|p| p.filter.clone())
            .unwrap_or_default();
        self.replace_list_picker(ListPickerState {
            kind: ListPickerKind::Session,
            title: " Sessions (this workspace)  type to filter · ↑↓ Enter · Ctrl-D delete ".into(),
            items: items.clone(),
            selected,
            filter: prior_filter,
            all_items: items,
        });
        self.apply_session_picker_filter();
        self.state.session_delete_confirm = None;
        self.refresh_session_picker_preview();
    }

    fn apply_session_picker_filter(&mut self) {
        let Some(picker) = self.state.list_picker.as_mut() else {
            return;
        };
        if picker.kind != ListPickerKind::Session {
            return;
        }
        let q = picker.filter.to_ascii_lowercase();
        if q.is_empty() {
            picker.items = picker.all_items.clone();
            picker.title =
                " Sessions (this workspace)  type to filter · ↑↓ Enter · Ctrl-D delete ".into();
        } else {
            picker.items = picker
                .all_items
                .iter()
                .filter(|i| {
                    i.label.to_ascii_lowercase().contains(&q)
                        || i.detail.to_ascii_lowercase().contains(&q)
                        || i.id.to_ascii_lowercase().contains(&q)
                })
                .cloned()
                .collect();
            picker.title = format!(
                " Sessions · filter {:?} · {}/{} · workspace scope ",
                picker.filter,
                picker.items.len(),
                picker.all_items.len()
            );
        }
        if picker.selected >= picker.items.len() {
            picker.selected = picker.items.len().saturating_sub(1);
        }
    }

    async fn resume_session(&mut self, query: &str) -> anyhow::Result<()> {
        let leaving_id = self.state.session_id.clone();
        let leaving_empty = leaving_id
            .as_ref()
            .map(|id| id != query && !session_has_retained_io(&self.state.messages))
            .unwrap_or(false);

        // Persist UI context for the session we leave.
        if let Some(ref leaving) = leaving_id {
            if leaving != query {
                let view = crate::session_view::SessionViewState::capture(
                    &self.state.input,
                    self.state.scroll_up,
                    self.state.follow_bottom,
                    self.state.todos_expanded,
                    &self.state.search,
                    self.state.highlight_message,
                );
                self.state.session_views.insert(leaving.clone(), view);
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

        self.state.resume_switch = Some(ResumeSwitchCtx {
            target: query.to_string(),
            leaving_id,
            leaving_empty,
            started_at: std::time::Instant::now(),
        });
        // Keep showing the current transcript until the target loads.
        self.jobs
            .spawn_session_resume(self.client.requester(), query.to_string());
        // First feedback is the non-blocking job notice (same tick).
        if let Some(ctx) = self.state.resume_switch.as_ref() {
            let _ = ctx.started_at.elapsed();
        }
        Ok(())
    }

    fn apply_session_resume_data(
        &mut self,
        query: &str,
        data: serde_json::Value,
    ) -> anyhow::Result<()> {
        let ctx = match self.state.resume_switch.as_ref() {
            Some(c) if c.target == query => self.state.resume_switch.take(),
            _ => None,
        };
        let leaving_id = ctx.as_ref().and_then(|c| c.leaving_id.clone());
        let leaving_empty = ctx.as_ref().map(|c| c.leaving_empty).unwrap_or(false);
        if let Some(ref switch) = ctx {
            let visible_ms = switch.started_at.elapsed().as_millis() as u64;
            // Busy notice threshold is ~150ms; treat that as first feedback ceiling.
            let first_feedback_ms = visible_ms.min(150);
            tracing::debug!(
                target = %switch.target,
                first_feedback_ms,
                visible_ms,
                "session switch metrics"
            );
            self.state.last_switch_metrics = Some(SessionSwitchMetrics {
                target: switch.target.clone(),
                first_feedback_ms,
                visible_ms,
            });
        }

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
        self.state.render_cache.invalidate_all();
        self.state.history_loading = false;
        self.state.history_oldest_index = None;
        self.state.history_total = None;
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
                self.state.model_alias = Some(model.to_string());
            }
        }
        if let Some(perm) = data
            .get("permission_mode")
            .and_then(|v| v.as_str())
            .and_then(parse_permission_mode_str)
        {
            self.state.permission_mode = perm;
            self.state.status_bar.permission = perm;
        }
        if let Some(usage) = data.get("usage") {
            let input = usage
                .get("input_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let output = usage
                .get("output_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            self.state.approx_tokens = input.saturating_add(output);
            self.state.usage_session = SessionUsageTotals {
                input_tokens: input,
                output_tokens: output,
                cache_creation_tokens: usage
                    .get("cache_creation_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                cache_read_tokens: usage
                    .get("cache_read_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
            };
        }
        if let Some(plan) = data.get("plan_mode").and_then(|v| v.as_bool()) {
            self.state.on_plan_mode_changed(plan);
        }
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

        if leaving_empty {
            if let Some(prev) = leaving_id {
                if prev != sid {
                    let requester = self.client.requester();
                    let prev_id = prev.clone();
                    tokio::spawn(async move {
                        let params = serde_json::json!({"session_id": prev_id});
                        let _ = requester.rpc_call("sessions.delete", Some(params)).await;
                    });
                    self.state.tab_strip.tabs.retain(|t| t.id != prev);
                    self.state.open_session_group.retain(|id| id != &prev);
                }
            }
        }

        if !self.state.open_session_group.iter().any(|id| id == &sid) {
            self.state.open_session_group.clear();
        }

        self.state.status_bar.session_id = Some(sid.clone());
        self.state.tab_strip.ensure_active(&sid, "session");
        self.state.list_picker = None;
        self.state.session_picker_preview = None;
        self.enqueue_workspace_sessions_refresh();

        // Restore UI context captured when we last left this session.
        if let Some(view) = self.state.session_views.remove(&sid) {
            view.restore_into(
                &mut self.state.input,
                &mut self.state.scroll_up,
                &mut self.state.follow_bottom,
                &mut self.state.todos_expanded,
                &mut self.state.search,
                &mut self.state.highlight_message,
            );
        } else {
            self.state.input.clear();
            self.state.scroll_up = 0;
            self.state.follow_bottom = true;
            self.state.todos_expanded = false;
            self.state.search = crate::search::SearchState::default();
            self.state.highlight_message = None;
        }

        // Lazy history: show recent messages first, then backfill older pages
        // without forcing the viewport to the bottom.
        if let Some(hist) = data.get("history") {
            let total = hist.get("total").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let oldest = hist
                .get("oldest_index")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;
            let older = hist
                .get("older_available")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            self.state.history_total = Some(total);
            self.state.history_oldest_index = Some(oldest);
            if older && oldest > 0 {
                self.state.history_loading = true;
                self.jobs
                    .spawn_session_history(self.client.requester(), sid.clone(), oldest, 40);
            }
        }
        Ok(())
    }

    fn apply_session_history_page(
        &mut self,
        session_id: &str,
        before: usize,
        data: serde_json::Value,
    ) {
        if self.state.session_id.as_deref() != Some(session_id) {
            return;
        }
        // Ignore stale pages if a newer history request superseded them.
        if self
            .state
            .history_oldest_index
            .is_some_and(|idx| idx != before)
        {
            return;
        }

        let Some(msgs) = data.get("messages").and_then(|v| v.as_array()) else {
            self.state.history_loading = false;
            return;
        };
        let older = transcript_messages_to_display(msgs);
        let added = older.len();
        if added == 0 {
            self.state.history_loading = false;
            return;
        }

        // Preserve the user's reading position: when prepending, keep the same
        // visual offset from the bottom by increasing scroll_up by the new
        // content's approximate line count once the next frame measures it.
        // For now bump scroll_up by a conservative estimate (2 lines/msg) so we
        // do not snap to the bottom; follow_bottom stays false if the user was
        // reading history.
        let was_following = self.state.follow_bottom;
        if !was_following || self.state.scroll_up > 0 {
            self.state.follow_bottom = false;
            self.state.scroll_up = self
                .state
                .scroll_up
                .saturating_add((added as u16).saturating_mul(3));
        }

        let mut merged = older;
        merged.append(&mut self.state.messages);
        self.state.messages = merged;

        let new_oldest = data
            .get("history")
            .and_then(|h| h.get("oldest_index"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;
        let older_available = data
            .get("history")
            .and_then(|h| h.get("older_available"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        self.state.history_oldest_index = Some(new_oldest);
        if older_available && new_oldest > 0 {
            self.state.history_loading = true;
            self.jobs.spawn_session_history(
                self.client.requester(),
                session_id.to_string(),
                new_oldest,
                40,
            );
        } else {
            self.state.history_loading = false;
        }
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
        self.enqueue_workspace_sessions_refresh();
        Ok(())
    }

    fn apply_workspace_sessions_list(&mut self, data: Option<serde_json::Value>) {
        let Some(current_id) = self.state.session_id.clone() else {
            self.state.workspace_sessions.set_entries(Vec::new(), None);
            return;
        };
        let Some(data) = data else {
            self.state.workspace_sessions.set_entries(Vec::new(), None);
            return;
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
        self.state
            .open_session_group
            .retain(|id| id == &current_id || rows.iter().any(|(rid, _, _)| rid == id));
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
                    let tab = self.state.tab_strip.tabs.iter().find(|t| t.id == id);
                    let status = tab.map(|t| t.status).unwrap_or(SessionStatus::Idle);
                    let dirty = tab.map(|t| t.dirty).unwrap_or(false);
                    let needs_attention = self.state.parked_approvals.contains_key(&id)
                        || self.state.parked_questions.contains_key(&id);
                    if let Some((_, _, title)) = rows.iter().find(|(rid, _, _)| rid == &id) {
                        Some(crate::chrome::WorkspaceSessionEntry {
                            id,
                            title: title.clone(),
                            status,
                            dirty,
                            needs_attention,
                        })
                    } else if id == current_id {
                        Some(crate::chrome::WorkspaceSessionEntry {
                            id,
                            title: "session".into(),
                            status,
                            dirty,
                            needs_attention,
                        })
                    } else {
                        None
                    }
                })
                .collect()
        };

        self.state
            .workspace_sessions
            .set_entries_stable(entries, Some(current_id.as_str()));

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
    }

    fn link_open_sessions(&mut self, a: &str, b: &str) {
        for id in [a, b] {
            if !self.state.open_session_group.iter().any(|x| x == id) {
                self.state.open_session_group.push(id.to_string());
            }
        }
    }

    fn can_close_current_session_tab(&self) -> bool {
        self.state.input.is_empty()
            && self.state.session_id.is_some()
            && self.state.workspace_sessions.entries.len() >= 2
            && self.state.list_picker.is_none()
            && self.state.tasks_panel.is_none()
            && self.state.slash_menu.is_none()
            && self.state.file_menu.is_none()
            && !self.state.search.active
            && self.state.session_delete_confirm.is_none()
    }

    fn begin_close_current_session_confirm(&mut self) {
        let Some(sid) = self.state.session_id.clone() else {
            return;
        };
        let label = self
            .state
            .workspace_sessions
            .entries
            .iter()
            .find(|e| e.id == sid)
            .map(|e| e.title.clone())
            .unwrap_or_else(|| {
                if sid.len() > 8 {
                    sid[..8].to_string()
                } else {
                    sid.clone()
                }
            });
        let busy = matches!(
            self.state.status,
            SessionStatus::Thinking
                | SessionStatus::ToolExecuting
                | SessionStatus::WaitingApproval
                | SessionStatus::WaitingQuestion
                | SessionStatus::Compacting
        );
        self.state.quit_confirm = false;
        self.state.session_delete_confirm = Some(SessionDeleteConfirm {
            session_id: sid,
            label,
            selected: 0,
            permanent: false,
            busy,
        });
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

    async fn cycle_attention_session(&mut self) -> anyhow::Result<()> {
        self.enqueue_workspace_sessions_refresh();
        let current = self.state.session_id.clone();
        let target = {
            let entries = &self.state.workspace_sessions.entries;
            if !entries.is_empty() {
                let start = current
                    .as_ref()
                    .and_then(|c| entries.iter().position(|e| &e.id == c))
                    .unwrap_or(0);
                (1..=entries.len()).find_map(|offset| {
                    let idx = (start + offset) % entries.len();
                    let e = &entries[idx];
                    if (e.needs_attention || e.dirty) && current.as_deref() != Some(e.id.as_str())
                    {
                        Some(e.id.clone())
                    } else {
                        None
                    }
                })
            } else {
                let tabs = &self.state.tab_strip.tabs;
                let start = current
                    .as_ref()
                    .and_then(|c| tabs.iter().position(|t| &t.id == c))
                    .unwrap_or(0);
                (1..=tabs.len()).find_map(|offset| {
                    let idx = (start + offset) % tabs.len();
                    let tab = &tabs[idx];
                    let needs = tab.dirty
                        || self.state.parked_approvals.contains_key(&tab.id)
                        || self.state.parked_questions.contains_key(&tab.id)
                        || matches!(
                            tab.status,
                            SessionStatus::WaitingApproval | SessionStatus::WaitingQuestion
                        );
                    if needs && current.as_deref() != Some(tab.id.as_str()) {
                        Some(tab.id.clone())
                    } else {
                        None
                    }
                })
            }
        };
        if let Some(id) = target {
            return self.resume_session(&id).await;
        }
        self.system_message("No session needs attention.".into());
        Ok(())
    }

    fn refresh_session_picker_preview(&mut self) {
        let Some(picker) = self.state.list_picker.as_ref() else {
            self.state.session_picker_preview = None;
            self.state.preview_debounce = None;
            return;
        };
        if picker.kind != ListPickerKind::Session {
            self.state.session_picker_preview = None;
            self.state.preview_debounce = None;
            return;
        }
        let Some(item) = picker.items.get(picker.selected) else {
            self.state.session_picker_preview = None;
            self.state.preview_debounce = None;
            return;
        };
        let sid = item.id.clone();

        // Current session: reuse live transcript (same look as normal chat).
        if self.state.session_id.as_deref() == Some(sid.as_str()) {
            self.state.preview_debounce = None;
            self.state.session_picker_preview = Some(SessionPickerPreview {
                session_id: sid,
                messages: self.state.messages.clone(),
            });
            self.state.follow_bottom = true;
            self.state.scroll_up = 0;
            return;
        }

        // Immediate highlight movement: never show the previous session's
        // preview under a newly selected row.
        if self
            .state
            .session_picker_preview
            .as_ref()
            .is_none_or(|p| p.session_id != sid)
        {
            if let Some(cached) = self.state.preview_cache.get(&sid) {
                self.apply_session_preview_data(&sid, cached);
                self.state.preview_debounce = None;
                return;
            }
            self.state.session_picker_preview = Some(SessionPickerPreview {
                session_id: sid.clone(),
                messages: vec![DisplayMessage {
                    role: MessageRole::System,
                    content: "Loading preview…".into(),
                    thinking: None,
                    parts: Vec::new(),
                    tool_calls: Vec::new(),
                            delivery: crate::prompt_queue::DeliveryState::Sent,
            idempotency_key: None,
        }],
            });
        }

        // Debounce the network fetch so rapid ↑↓ only loads the final item.
        self.state.preview_debounce = Some(PreviewDebounce {
            session_id: sid,
            due_at: std::time::Instant::now() + std::time::Duration::from_millis(120),
        });
    }

    fn flush_preview_debounce(&mut self) {
        let Some(pending) = self.state.preview_debounce.as_ref() else {
            return;
        };
        if std::time::Instant::now() < pending.due_at {
            return;
        }
        let sid = pending.session_id.clone();
        self.state.preview_debounce = None;

        // Selection may have moved away while we waited.
        let still_selected = self.state.list_picker.as_ref().is_some_and(|p| {
            p.kind == ListPickerKind::Session
                && p.items.get(p.selected).is_some_and(|i| i.id == sid)
        });
        if !still_selected {
            return;
        }
        if let Some(cached) = self.state.preview_cache.get(&sid) {
            self.apply_session_preview_data(&sid, cached);
            return;
        }
        self.jobs
            .spawn_session_preview(self.client.requester(), sid);
    }

    fn apply_session_preview_data(&mut self, session_id: &str, data: serde_json::Value) {
        // Ignore if the user has already moved the highlight.
        let still_selected = self.state.list_picker.as_ref().is_some_and(|p| {
            p.kind == ListPickerKind::Session
                && p.items.get(p.selected).is_some_and(|i| i.id == session_id)
        });
        if !still_selected {
            return;
        }
        self.state
            .preview_cache
            .put(session_id.to_string(), data.clone());
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
                            delivery: crate::prompt_queue::DeliveryState::Sent,
            idempotency_key: None,
        });
            }
        }
        self.state.session_picker_preview = Some(SessionPickerPreview {
            session_id: session_id.to_string(),
            messages,
        });
        self.state.follow_bottom = true;
        self.state.scroll_up = 0;
    }

    async fn confirm_delete_session(&mut self, yes: bool) -> anyhow::Result<()> {
        let Some(confirm) = self.state.session_delete_confirm.take() else {
            return Ok(());
        };
        if !yes {
            return Ok(());
        }
        let deleted_id = confirm.session_id.clone();
        let reopen_picker = self
            .state
            .list_picker
            .as_ref()
            .is_some_and(|p| p.kind == ListPickerKind::Session);

        if !confirm.permanent {
            // Close TUI tab only — keep history, optionally leave turn running.
            self.state.open_session_group.retain(|id| id != &deleted_id);
            self.state.tab_strip.tabs.retain(|t| t.id != deleted_id);
            if self.state.tab_strip.active >= self.state.tab_strip.tabs.len() {
                self.state.tab_strip.active = self.state.tab_strip.tabs.len().saturating_sub(1);
            }
            let was_current = self.state.session_id.as_deref() == Some(deleted_id.as_str());
            if was_current {
                let view = crate::session_view::SessionViewState::capture(
                    &self.state.input,
                    self.state.scroll_up,
                    self.state.follow_bottom,
                    self.state.todos_expanded,
                    &self.state.search,
                    self.state.highlight_message,
                );
                self.state.session_views.insert(deleted_id.clone(), view);
                if let Some(approval) = self.state.approval_pending.take() {
                    self.state
                        .parked_approvals
                        .insert(deleted_id.clone(), approval);
                }
                if let Some(question) = self.state.question_pending.take() {
                    self.state
                        .parked_questions
                        .insert(deleted_id.clone(), question);
                }
                self.enqueue_workspace_sessions_refresh();
                let fallback = self
                    .state
                    .workspace_sessions
                    .entries
                    .iter()
                    .map(|e| e.id.clone())
                    .find(|id| id != &deleted_id)
                    .or_else(|| {
                        self.state
                            .open_session_group
                            .iter()
                            .find(|id| *id != &deleted_id)
                            .cloned()
                    })
                    .or_else(|| {
                        self.state
                            .tab_strip
                            .tabs
                            .iter()
                            .map(|t| t.id.clone())
                            .find(|id| id != &deleted_id)
                    });
                if let Some(id) = fallback {
                    self.resume_session(&id).await?;
                } else {
                    let cwd = std::env::current_dir()?.to_string_lossy().to_string();
                    let session_id = self
                        .client
                        .create_session(Some(&cwd), Some(self.state.permission_mode))
                        .await?;
                    self.state.messages.clear();
                    self.state.todos.clear();
                    self.state.status = SessionStatus::Idle;
                    self.state.approval_pending = None;
                    self.state.question_pending = None;
                    self.state.session_id = Some(session_id.clone());
                    self.state.status_bar.session_id = Some(session_id.clone());
                    self.state.tab_strip.ensure_active(&session_id, "main");
                    self.bind_config_default_model();
                }
                self.system_message(if confirm.busy {
                    "Tab closed — previous turn continues in the background.".into()
                } else {
                    "Session tab closed (history kept).".into()
                });
            }
            return Ok(());
        }

        // Permanent delete — interrupt busy turns first.
        if self.state.session_id.as_deref() == Some(deleted_id.as_str())
            && !matches!(self.state.status, SessionStatus::Idle)
        {
            let _ = self.client.interrupt(&deleted_id).await;
        }

        let params = serde_json::json!({"session_id": deleted_id});
        match self.client.rpc_call("sessions.delete", Some(params)).await {
            Ok(_) => {
                self.state.open_session_group.retain(|id| id != &deleted_id);
                self.state.tab_strip.tabs.retain(|t| t.id != deleted_id);
                self.state.parked_approvals.remove(&deleted_id);
                self.state.parked_questions.remove(&deleted_id);
                self.state.session_views.remove(&deleted_id);
                let was_current = self.state.session_id.as_deref() == Some(deleted_id.as_str());
                if was_current {
                    self.refresh_workspace_sessions().await?;
                    let fallback = self
                        .state
                        .workspace_sessions
                        .entries
                        .iter()
                        .map(|e| e.id.clone())
                        .find(|id| id != &deleted_id)
                        .or_else(|| {
                            self.state
                                .open_session_group
                                .iter()
                                .find(|id| *id != &deleted_id)
                                .cloned()
                        });
                    if let Some(id) = fallback {
                        self.resume_session(&id).await?;
                    } else {
                        let cwd = std::env::current_dir()?.to_string_lossy().to_string();
                        let session_id = self
                            .client
                            .create_session(Some(&cwd), Some(self.state.permission_mode))
                            .await?;
                        self.state.messages.clear();
                        self.state.todos.clear();
                        self.state.status = SessionStatus::Idle;
                        self.state.approval_pending = None;
                        self.state.question_pending = None;
                        self.state.session_id = Some(session_id.clone());
                        self.state.status_bar.session_id = Some(session_id.clone());
                        self.state.tab_strip.ensure_active(&session_id, "main");
                        self.bind_config_default_model();
                    }
                }
                if reopen_picker {
                    self.open_session_picker().await?;
                }
                let _ = self.refresh_workspace_sessions().await;
                self.system_message("Session permanently deleted.".into());
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

        let busy = matches!(
            self.state.status,
            SessionStatus::Thinking
                | SessionStatus::ToolExecuting
                | SessionStatus::WaitingApproval
                | SessionStatus::WaitingQuestion
                | SessionStatus::Compacting
        );
        if busy && self.state.queue_when_busy {
            self.state
                .prompt_queue
                .push(crate::prompt_queue::QueuedPrompt::next_turn(prompt_text));
            self.state.messages.push(DisplayMessage {
                role: MessageRole::User,
                content: text,
                thinking: None,
                parts: Vec::new(),
                tool_calls: Vec::new(),
                delivery: crate::prompt_queue::DeliveryState::Queued,
                idempotency_key: None,
            });
            self.system_message(format!(
                "Queued ({} waiting) — will send after current turn.",
                self.state.prompt_queue.items.len()
            ));
            return Ok(());
        }

        let idem = uuid::Uuid::new_v4().to_string();
        self.state.messages.push(DisplayMessage {
            role: MessageRole::User,
            content: text,
            thinking: None,
            parts: Vec::new(),
            tool_calls: Vec::new(),
            delivery: crate::prompt_queue::DeliveryState::Sending,
            idempotency_key: Some(idem.clone()),
        });
        self.state.status = SessionStatus::Thinking;
        self.state.thinking_text.clear();
        self.state.scroll_up = 0;
        self.state.follow_bottom = true;

        // Send to server — never block the UI loop on the prompt RPC.
        if let Some(sid) = self.state.session_id.clone() {
            if self.jobs.mcp.configured && !self.jobs.mcp.initialized {
                self.jobs.mcp.waiting_for_prompt = true;
                self.enqueue_mcp_status_poll();
            }
            self.jobs.spawn_prompt(
                self.client.requester(),
                sid,
                prompt_text,
                Vec::new(),
                idem,
            );
        } else {
            self.state.status = SessionStatus::Idle;
            if let Some(msg) = self.state.messages.last_mut() {
                msg.delivery = crate::prompt_queue::DeliveryState::Failed;
            }
            self.system_message("No active session.".into());
        }

        Ok(())
    }

    fn flush_prompt_queue_if_idle(&mut self) {
        if !matches!(self.state.status, SessionStatus::Idle) {
            return;
        }
        let Some(item) = self.state.prompt_queue.pop_front() else {
            return;
        };
        let Some(sid) = self.state.session_id.clone() else {
            return;
        };
        let idem = uuid::Uuid::new_v4().to_string();
        if let Some(msg) = self
            .state
            .messages
            .iter_mut()
            .rev()
            .find(|m| m.role == MessageRole::User && m.delivery == crate::prompt_queue::DeliveryState::Queued && m.content == item.text)
        {
            msg.delivery = crate::prompt_queue::DeliveryState::Sending;
            msg.idempotency_key = Some(idem.clone());
        } else {
            self.state.messages.push(DisplayMessage {
                role: MessageRole::User,
                content: item.text.clone(),
                thinking: None,
                parts: Vec::new(),
                tool_calls: Vec::new(),
                delivery: crate::prompt_queue::DeliveryState::Sending,
                idempotency_key: Some(idem.clone()),
            });
        }
        self.state.status = SessionStatus::Thinking;
        self.jobs.spawn_prompt(
            self.client.requester(),
            sid,
            item.text,
            item.images,
            idem,
        );
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

        // Dynamic skill slash (`/skill:foo` or bare `/foo` mapped to a skill).
        if let Some(skill_name) = self.resolve_skill_command(&command) {
            return self.activate_skill(&skill_name, &args).await;
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
                self.begin_root_picker();
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
                self.bind_config_default_model();
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
                    self.begin_root_picker();
                    self.open_session_picker().await?;
                } else if let Err(e) = self.resume_session(&args).await {
                    self.system_message(format!("Failed to resume: {}", e));
                }
            }
            "compact" => {
                if let Some(sid) = &self.state.session_id {
                    let params = serde_json::json!({"session_id": sid, "instruction": args});
                    // RPC returns immediately (`started: true`); completion arrives as
                    // CompactCompleted so the TUI event loop stays responsive.
                    match self.client.rpc_call("session.compact", Some(params)).await {
                        Ok(_) => {
                            self.state.status = SessionStatus::Compacting;
                            self.state.status_bar.status = SessionStatus::Compacting;
                            self.system_message("Compacting conversation…".into());
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
                    self.begin_root_picker();
                    self.open_model_picker();
                } else if self.config.resolve_model(&args).is_some() {
                    // Session-scoped; do not rewrite global config.default_model.
                    self.state.model_alias = Some(args.clone());
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
                    self.begin_root_picker();
                    self.open_effort_picker();
                } else {
                    let effort = args.to_lowercase();
                    match effort.as_str() {
                        "off" | "low" | "medium" | "high" => self.apply_effort_level(&effort),
                        "on" => self.apply_effort_level("high"),
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
            "status" | "info" => {
                self.begin_root_picker();
                self.open_status_picker();
            }
            "usage" => {
                self.begin_root_picker();
                self.open_usage_picker();
            }
            "doctor" => {
                self.begin_root_picker();
                self.open_doctor_picker().await?;
            }
            "mcp" => {
                self.begin_root_picker();
                self.open_mcp_manager().await?;
            }
            "tasks" | "task" => {
                self.begin_root_picker();
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
                    let sid = sid.clone();
                    let title = args.to_string();
                    // Optimistic local update — roll back on RPC failure.
                    let prev_title = self
                        .state
                        .workspace_sessions
                        .entries
                        .iter()
                        .find(|e| e.id == sid)
                        .map(|e| e.title.clone());
                    if let Some(entry) = self
                        .state
                        .workspace_sessions
                        .entries
                        .iter_mut()
                        .find(|e| e.id == sid)
                    {
                        entry.title = title.clone();
                    }
                    self.state.tab_strip.ensure_tab(&sid, title.clone());
                    let params = serde_json::json!({"session_id": sid, "title": title});
                    match self
                        .client
                        .rpc_call("session.set_title", Some(params))
                        .await
                    {
                        Ok(_) => {
                            self.system_message(format!("Session title set to: {}", title));
                            self.enqueue_workspace_sessions_refresh();
                        }
                        Err(e) => {
                            if let Some(prev) = prev_title {
                                if let Some(entry) = self
                                    .state
                                    .workspace_sessions
                                    .entries
                                    .iter_mut()
                                    .find(|e| e.id == sid)
                                {
                                    entry.title = prev.clone();
                                }
                                self.state.tab_strip.ensure_tab(&sid, prev);
                            }
                            self.system_message(format!("Failed to set title: {}", e));
                        }
                    }
                }
            }
            "config" => {
                self.begin_root_picker();
                self.open_config_picker();
            }
            "auth" => {
                self.begin_root_picker();
                self.open_auth_picker();
            }
            "plugins" | "plugin" => {
                self.begin_root_picker();
                self.open_plugins_picker().await?;
            }
            "skills" => {
                let _ = self.refresh_skill_commands().await;
                if !args.is_empty() {
                    let mut parts = args.splitn(2, char::is_whitespace);
                    let name = parts.next().unwrap_or("").trim();
                    let skill_args = parts.next().unwrap_or("").trim();
                    if !name.is_empty() {
                        return self.activate_skill(name, skill_args).await;
                    }
                }
                self.begin_root_picker();
                self.open_skill_manager().await?;
            }
            "swarm" => match args.as_str() {
                "enter" | "on" => self.apply_swarm_action("enter").await?,
                "exit" | "off" => self.apply_swarm_action("exit").await?,
                _ => {
                    self.begin_root_picker();
                    self.open_swarm_picker();
                }
            },
            "provider" | "providers" => {
                self.begin_root_picker();
                self.open_provider_picker();
            }
            "reload" => {
                self.reload_config_from_disk();
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
                    self.system_message(
                        "Usage: /btw <question> — side Q&A (does not alter main chat)".into(),
                    );
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
                self.begin_root_picker();
                self.open_prompts_picker();
            }
            "experimental-flags" | "flags" => {
                self.begin_root_picker();
                self.open_flags_picker();
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
                        MessageRole::Skill => "Skill",
                    };
                    md.push_str(&format!("## {}\n\n{}\n\n", role, msg.content));
                }
                match std::fs::write(&path, md) {
                    Ok(()) => self.system_message(format!("Exported to {}", path.display())),
                    Err(e) => self.system_message(format!("Export failed: {}", e)),
                }
            }
            "version" => {
                self.begin_root_picker();
                self.open_status_picker();
            }
            "help" | "h" | "?" => {
                self.begin_root_picker();
                self.open_help_picker();
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

    fn resolve_skill_command(&self, command: &str) -> Option<String> {
        if let Some(name) = self.state.skill_command_map.get(command) {
            return Some(name.clone());
        }
        if let Some(rest) = command.strip_prefix("skill:") {
            let name = rest.trim();
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
        None
    }

    async fn refresh_skill_commands(&mut self) -> anyhow::Result<()> {
        let data = self.client.rpc_call("skills.list", None).await.ok();
        self.apply_skills_list(data);
        Ok(())
    }

    fn apply_skills_list(&mut self, data: Option<serde_json::Value>) {
        let Some(data) = data else {
            return;
        };
        let mut pairs = Vec::new();
        if let Some(arr) = data.get("skills").and_then(|v| v.as_array()) {
            for s in arr {
                let name = s
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if name.is_empty() {
                    continue;
                }
                let enabled = s.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
                if !enabled {
                    continue;
                }
                let desc = s
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                pairs.push((name, desc));
            }
        }
        let (commands, map) = build_skill_slash_commands(&pairs);
        self.state.skill_slash_commands = commands;
        self.state.skill_command_map = map;
    }

    async fn activate_skill(&mut self, name: &str, args: &str) -> anyhow::Result<()> {
        let Some(sid) = self.state.session_id.clone() else {
            self.system_message("No active session.".into());
            return Ok(());
        };
        // Optimistic card — server also emits SkillActivated (dedupe by name+pending).
        self.push_skill_activation(name, if args.is_empty() { None } else { Some(args) });
        self.state.status = SessionStatus::Thinking;
        self.state.follow_bottom = true;
        self.state.scroll_up = 0;

        let params = serde_json::json!({
            "session_id": sid,
            "name": name,
            "args": args,
        });
        match self.client.rpc_call("skills.activate", Some(params)).await {
            Ok(_) => {
                let _ = self.refresh_workspace_sessions().await;
            }
            Err(e) => {
                self.state.status = SessionStatus::Idle;
                // Drop optimistic card on failure.
                if let Some(last) = self.state.messages.last() {
                    if last.role == MessageRole::Skill {
                        self.state.messages.pop();
                    }
                }
                self.system_message(format!("Skill \"{name}\" failed: {e}"));
            }
        }
        Ok(())
    }

    fn push_skill_activation(&mut self, name: &str, args: Option<&str>) {
        // Avoid duplicate cards when both optimistic UI and SkillActivated arrive.
        if let Some(last) = self.state.messages.last() {
            if last.role == MessageRole::Skill {
                if let Some(DisplayPart::SkillActivation { name: n, .. }) = last.parts.first() {
                    if n == name {
                        return;
                    }
                }
            }
        }
        self.state.messages.push(DisplayMessage {
            role: MessageRole::Skill,
            content: format!("Activated skill: {name}"),
            thinking: None,
            parts: vec![DisplayPart::SkillActivation {
                name: name.to_string(),
                args: args.map(str::to_string).filter(|s| !s.trim().is_empty()),
            }],
            tool_calls: Vec::new(),
                    delivery: crate::prompt_queue::DeliveryState::Sent,
            idempotency_key: None,
        });
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
                    delivery: crate::prompt_queue::DeliveryState::Sent,
            idempotency_key: None,
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
            KeyCode::Char(' ') if q.selected < n && q.allow_multiple => {
                if let Some(t) = q.toggled.get_mut(q.selected) {
                    *t = !*t;
                }
            }
            KeyCode::Enter => {
                if !q.allow_multiple && q.selected < n && !q.toggled.iter().any(|t| *t) {
                    if let Some(t) = q.toggled.get_mut(q.selected) {
                        *t = true;
                    }
                }
                if q.allow_multiple
                    && q.selected < n
                    && !q.toggled.iter().any(|t| *t)
                    && q.free_text.trim().is_empty()
                {
                    // Require at least one selection or free text for multi.
                    return Ok(());
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
                    if q.allow_multiple {
                        q.selected = idx;
                        if let Some(t) = q.toggled.get_mut(idx) {
                            *t = !*t;
                        }
                    } else {
                        q.selected = idx;
                        q.toggled.iter_mut().for_each(|t| *t = false);
                        if let Some(t) = q.toggled.get_mut(idx) {
                            *t = true;
                        }
                        self.respond_question(false).await?;
                    }
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
        let answer_preview = {
            let labels: Vec<&str> = q
                .options
                .iter()
                .zip(q.toggled.iter())
                .filter_map(|((_, label), on)| if *on { Some(label.as_str()) } else { None })
                .collect();
            let mut parts = labels;
            let free = q.free_text.trim();
            if !free.is_empty() {
                parts.push(free);
            }
            parts.join(", ")
        };
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
        } else if !cancelled {
            let preview: String = answer_preview.chars().take(80).collect();
            self.system_message(format!("Answered: {preview}"));
        }
        Ok(())
    }

    fn handle_server_event(&mut self, frame: Frame) {
        if let Frame::Event {
            event: event_name,
            data,
            ..
        } = frame
        {
            if event_name == "mcp.status" {
                self.jobs.apply_mcp_status(&data);
                return;
            }
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
                                    allow_multiple: question.allow_multiple,
                                    background: question.background,
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
                        AgentEvent::CompactCompleted { deleted, error, .. } => {
                            self.state.tab_strip.mark_dirty(&evt_sid, true);
                            if let Some(err) = error {
                                self.system_message(format!(
                                    "Background session {} compact failed: {err}",
                                    &evt_sid[..8.min(evt_sid.len())]
                                ));
                            } else {
                                self.system_message(format!(
                                    "Background session {} compacted ({deleted} removed).",
                                    &evt_sid[..8.min(evt_sid.len())]
                                ));
                            }
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
                                    delivery: crate::prompt_queue::DeliveryState::Sent,
            idempotency_key: None,
        };
                        msg.append_assistant_text(&text);
                        self.state.messages.push(msg);
                    }
                    AgentEvent::ThinkingDelta { text, .. } => {
                        self.state.thinking_text.push_str(&text);
                    }
                    AgentEvent::ToolCall {
                        tool_call_id,
                        tool_name,
                        input,
                        ..
                    } => {
                        self.state.last_tool_name = Some(tool_name.clone());
                        let summary = summarize_tool_input(&input);
                        let pending_thinking = if !self.state.thinking_text.is_empty() {
                            Some(std::mem::take(&mut self.state.thinking_text))
                        } else {
                            None
                        };
                        let tc = DisplayToolCall {
                            id: tool_call_id,
                            started_at: Some(std::time::Instant::now()),
                            stopping: false,
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
                            delivery: crate::prompt_queue::DeliveryState::Sent,
                            idempotency_key: None,
                        };
                        msg.push_tool(tc);
                        self.state.messages.push(msg);
                    }
                    AgentEvent::ToolResult {
                        tool_call_id,
                        tool_name,
                        output,
                        is_error,
                        ..
                    } => {
                        if let Some(last) = self.state.messages.last_mut() {
                            if let Some(tc) =
                                last.find_tool_for_result_mut(&tool_call_id, &tool_name)
                            {
                                tc.output = Some(output);
                                tc.is_error = is_error;
                                tc.stopping = false;
                                if is_error {
                                    tc.collapsed = false;
                                }
                            }
                        }
                    }
                    AgentEvent::StatusUpdate { status, .. } => {
                        self.state.status = status;
                        if matches!(status, SessionStatus::Idle) {
                            self.flush_prompt_queue_if_idle();
                        }
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
                        self.state.approval_pending = Some(pending_approval_from_request(&request));
                        self.state.status = SessionStatus::WaitingApproval;
                    }
                    AgentEvent::QuestionAsked { question, .. } => {
                        let options: Vec<(String, String)> = question
                            .options
                            .into_iter()
                            .map(|o| (o.id, o.label))
                            .collect();
                        let toggled = vec![false; options.len()];
                        let pending = PendingQuestion {
                            question_id: question.question_id,
                            text: question.text,
                            options,
                            allow_free_text: question.allow_free_text,
                            allow_multiple: question.allow_multiple,
                            background: question.background,
                            selected: 0,
                            toggled,
                            free_text: String::new(),
                        };
                        if question.background {
                            if let Some(sid) = self.state.session_id.clone() {
                                self.state.parked_questions.insert(sid.clone(), pending);
                                self.state.tab_strip.mark_dirty(&sid, true);
                            }
                            self.system_message(
                                "Background question waiting — open via session attention (Ctrl-N)."
                                    .into(),
                            );
                        } else {
                            self.state.question_pending = Some(pending);
                            self.state.status = SessionStatus::WaitingQuestion;
                        }
                    }
                    AgentEvent::UsageUpdate { usage, .. } => {
                        self.state.approx_tokens =
                            usage.input_tokens.saturating_add(usage.output_tokens);
                        self.state.usage_session.input_tokens = self
                            .state
                            .usage_session
                            .input_tokens
                            .max(usage.input_tokens);
                        self.state.usage_session.output_tokens = self
                            .state
                            .usage_session
                            .output_tokens
                            .max(usage.output_tokens);
                        self.state.usage_session.cache_creation_tokens = self
                            .state
                            .usage_session
                            .cache_creation_tokens
                            .max(usage.cache_creation_input_tokens);
                        self.state.usage_session.cache_read_tokens = self
                            .state
                            .usage_session
                            .cache_read_tokens
                            .max(usage.cache_read_input_tokens);
                        self.state.usage_turns.push(TurnUsageSample {
                            model: self.state.model_alias.clone(),
                            input_tokens: usage.input_tokens,
                            output_tokens: usage.output_tokens,
                            cache_creation_tokens: usage.cache_creation_input_tokens,
                            cache_read_tokens: usage.cache_read_input_tokens,
                            duration_ms: self
                                .state
                                .turn_started_at
                                .map(|t| t.elapsed().as_millis() as u64)
                                .unwrap_or(0),
                        });
                        if self.state.usage_turns.len() > 32 {
                            let drain = self.state.usage_turns.len() - 32;
                            self.state.usage_turns.drain(0..drain);
                        }
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
                                                delivery: crate::prompt_queue::DeliveryState::Sent,
            idempotency_key: None,
        });
                                }
                            } else {
                                self.state.messages.push(DisplayMessage {
                                    role: MessageRole::Assistant,
                                    content: String::new(),
                                    thinking: Some(t),
                                    parts: Vec::new(),
                                    tool_calls: Vec::new(),
                                            delivery: crate::prompt_queue::DeliveryState::Sent,
            idempotency_key: None,
        });
                            }
                        }
                        self.state.collapse_completed_turn_tools();
                        self.state.status = SessionStatus::Idle;
                        self.flush_prompt_queue_if_idle();
                    }
                    AgentEvent::TurnStart { .. } => {
                        self.state.thinking_text.clear();
                        self.state.turn_started_at = Some(std::time::Instant::now());
                        self.state.tokens_at_turn_start = self.state.approx_tokens;
                        self.jobs.mcp.waiting_for_prompt = false;
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
                    AgentEvent::CompactCompleted {
                        deleted,
                        kept_user_message_count,
                        messages,
                        error,
                        ..
                    } => {
                        if let Some(err) = error {
                            self.system_message(format!("Compact failed: {err}"));
                        } else {
                            self.state.messages = transcript_messages_to_display(&messages);
                            self.state.follow_bottom = true;
                            self.state.scroll_up = 0;
                            self.system_message(format!(
                                "Compacted: {deleted} messages removed (kept {kept_user_message_count} user messages)"
                            ));
                        }
                    }
                    AgentEvent::SkillActivated {
                        skill_name,
                        skill_args,
                        ..
                    } => {
                        self.push_skill_activation(&skill_name, skill_args.as_deref());
                        self.state.follow_bottom = true;
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

fn parse_permission_mode_str(raw: &str) -> Option<PermissionMode> {
    let s = raw.trim().trim_matches('"');
    s.parse().ok().or_else(|| match s.to_ascii_lowercase().as_str() {
        "manual" => Some(PermissionMode::Manual),
        "yolo" => Some(PermissionMode::Yolo),
        "auto" => Some(PermissionMode::Auto),
        _ => None,
    })
}

/// Returns (input, output, cache_creation, cache_read) USD per 1M tokens, and whether fallback.
fn model_pricing(config: &kkagent_config::AppConfig, alias: &str) -> (f64, f64, f64, f64, bool) {
    if let Some((model, _)) = config.resolve_model(alias) {
        if let Some(p) = model.pricing.as_ref() {
            return (
                p.input_per_mtok.unwrap_or(0.50),
                p.output_per_mtok.unwrap_or(2.00),
                p.cache_creation_per_mtok.unwrap_or(0.50),
                p.cache_read_per_mtok.unwrap_or(0.05),
                p.input_per_mtok.is_none() && p.output_per_mtok.is_none(),
            );
        }
    }
    (0.50, 2.00, 0.50, 0.05, true)
}

fn estimate_usd(
    u: &SessionUsageTotals,
    in_price: f64,
    out_price: f64,
    cache_c: f64,
    cache_r: f64,
) -> f64 {
    (u.input_tokens as f64) * in_price / 1_000_000.0
        + (u.output_tokens as f64) * out_price / 1_000_000.0
        + (u.cache_creation_tokens as f64) * cache_c / 1_000_000.0
        + (u.cache_read_tokens as f64) * cache_r / 1_000_000.0
}

fn summarize_tool_input(input: &serde_json::Value) -> String {
    // Prefer human-readable fields; keep enough text for full-width chips.
    for key in [
        "command",
        "path",
        "pattern",
        "query",
        "url",
        "description",
        "prompt",
        "name",
        "skill",
        "args",
    ] {
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

fn pending_approval_from_request(request: &kkagent_protocol::ApprovalRequest) -> PendingApproval {
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

/// Fold tool calls in `[start, end)` into one `ToolHistory` overview.
fn collapse_tools_in_turn(
    messages: &mut Vec<DisplayMessage>,
    start: usize,
    end: usize,
    duration_ms: u64,
    tokens: u64,
) {
    if start >= messages.len() || start >= end {
        return;
    }
    let end = end.min(messages.len());

    let mut collected: Vec<DisplayToolCall> = Vec::new();
    for msg in &mut messages[start..end] {
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
    if let Some(msg) = messages[start..end]
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
        messages.insert(
            end,
            DisplayMessage {
                role: MessageRole::Assistant,
                content: String::new(),
                thinking: None,
                parts: vec![summary],
                tool_calls: Vec::new(),
                        delivery: crate::prompt_queue::DeliveryState::Sent,
            idempotency_key: None,
        },
        );
    }
}

/// After resume / session preview rebuild, fold every completed turn's tools
/// the same way live `TurnEnd` does (duration/tokens unknown → 0).
fn collapse_all_historical_turn_tools(messages: &mut Vec<DisplayMessage>) {
    let user_idxs: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == MessageRole::User)
        .map(|(i, _)| i)
        .collect();

    let mut ranges: Vec<(usize, usize)> = Vec::new();
    match user_idxs.first() {
        Some(&first) if first > 0 => ranges.push((0, first)),
        None => ranges.push((0, messages.len())),
        _ => {}
    }
    for (i, &user_i) in user_idxs.iter().enumerate() {
        let start = user_i + 1;
        let end = user_idxs.get(i + 1).copied().unwrap_or(messages.len());
        if start < end {
            ranges.push((start, end));
        }
    }

    // Reverse so an insert at a later turn does not shift earlier ranges.
    for (start, end) in ranges.into_iter().rev() {
        collapse_tools_in_turn(messages, start, end, 0, 0);
    }
}

/// True when the transcript has user/assistant/plan content worth keeping.
/// Sessions with only UI system notices (or nothing) are treated as empty and
/// discarded when the user leaves the page.
fn session_has_retained_io(messages: &[DisplayMessage]) -> bool {
    messages.iter().any(|m| match m.role {
        MessageRole::System => false,
        MessageRole::Skill => true,
        MessageRole::User => {
            !m.content.trim().is_empty() && !kkagent_protocol::is_harness_only_user_text(&m.content)
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

/// Convert a list of serialized ChatMessages into display bubbles,
/// pairing tool_result blocks onto preceding tool_use entries.
/// Completed turns are folded into tool-history overviews (same as live TurnEnd).
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
                    let content = if visible.is_empty() { text } else { visible };
                    if content.trim().is_empty() {
                        continue;
                    }
                    out.push(DisplayMessage {
                        role: MessageRole::User,
                        content,
                        thinking: None,
                        parts: Vec::new(),
                        tool_calls: Vec::new(),
                                delivery: crate::prompt_queue::DeliveryState::Sent,
            idempotency_key: None,
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
                                tool_index.insert(id.clone(), (msg_idx, pi));
                            }
                            parts.push(DisplayPart::Tool(DisplayToolCall {
                                id,
                                started_at: None,
                                stopping: false,
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
                                delivery: crate::prompt_queue::DeliveryState::Sent,
            idempotency_key: None,
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
                                delivery: crate::prompt_queue::DeliveryState::Sent,
            idempotency_key: None,
        });
                }
            }
            _ => {}
        }
    }
    collapse_all_historical_turn_tools(&mut out);
    out
}

fn copy_to_clipboard(text: &str) -> anyhow::Result<()> {
    // OSC 52 reaches the client clipboard over SSH / tmux; never print the
    // payload into the UI. Failures are ignored so a missing OSC capability
    // does not crash the TUI — we still try the local native clipboard.
    let osc_ok = crate::selection::write_osc52(text).is_ok();
    let native_ok = copy_to_clipboard_native(text).is_ok();
    if osc_ok || native_ok {
        return Ok(());
    }
    anyhow::bail!("clipboard unavailable (OSC 52 and native helpers both failed)")
}

fn copy_to_clipboard_native(text: &str) -> anyhow::Result<()> {
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
        let _ = path;
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
                    delivery: crate::prompt_queue::DeliveryState::Sent,
            idempotency_key: None,
        }]));
        assert!(session_has_retained_io(&[DisplayMessage {
            role: MessageRole::User,
            content: "hello".into(),
            thinking: None,
            parts: Vec::new(),
            tool_calls: Vec::new(),
                    delivery: crate::prompt_queue::DeliveryState::Sent,
            idempotency_key: None,
        }]));
        assert!(!session_has_retained_io(&[DisplayMessage {
            role: MessageRole::User,
            content: "<system-reminder>\nToday's date\n</system-reminder>".into(),
            thinking: None,
            parts: Vec::new(),
            tool_calls: Vec::new(),
                    delivery: crate::prompt_queue::DeliveryState::Sent,
            idempotency_key: None,
        }]));
    }

    #[test]
    fn collapse_turn_tools_into_overview() {
        let mut state = AppState::new(PermissionMode::Manual, false);
        state.turn_started_at =
            Some(std::time::Instant::now() - std::time::Duration::from_secs(90));
        state.tokens_at_turn_start = 100;
        state.approx_tokens = 2500;
        state.messages.push(DisplayMessage {
            role: MessageRole::User,
            content: "do it".into(),
            thinking: None,
            parts: Vec::new(),
            tool_calls: Vec::new(),
                    delivery: crate::prompt_queue::DeliveryState::Sent,
            idempotency_key: None,
        });
        let mut asst = DisplayMessage {
            role: MessageRole::Assistant,
            content: "done".into(),
            thinking: None,
            parts: Vec::new(),
            tool_calls: Vec::new(),
                    delivery: crate::prompt_queue::DeliveryState::Sent,
            idempotency_key: None,
        };
        asst.parts.push(DisplayPart::Tool(DisplayToolCall {
                            id: String::new(),
                            started_at: None,
                            stopping: false,
            name: "Read".into(),
            input_summary: "a.rs".into(),
            output: Some("ok".into()),
            is_error: false,
            collapsed: true,
        }));
        asst.parts.push(DisplayPart::Tool(DisplayToolCall {
                            id: String::new(),
                            started_at: None,
                            stopping: false,
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
        assert!(
            matches!(parts.first(), Some(DisplayPart::ToolHistory(h)) if h.tool_count == 2 && !h.expanded)
        );
        assert!(matches!(parts.last(), Some(DisplayPart::Text(t)) if t == "done"));
        assert!(!parts.iter().any(|p| matches!(p, DisplayPart::Tool(_))));
    }

    #[test]
    fn resume_transcript_collapses_tool_history() {
        let msgs = vec![
            serde_json::json!({
                "role": "user",
                "content": [{"type": "text", "text": "read it"}]
            }),
            serde_json::json!({
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "id": "t1", "name": "Read", "input": {"path": "a.rs"}},
                    {"type": "text", "text": "ok"}
                ]
            }),
            serde_json::json!({
                "role": "user",
                "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "fn main() {}", "is_error": false}
                ]
            }),
            serde_json::json!({
                "role": "user",
                "content": [{"type": "text", "text": "again"}]
            }),
            serde_json::json!({
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "id": "t2", "name": "Bash", "input": {"command": "ls"}},
                    {"type": "tool_use", "id": "t3", "name": "Read", "input": {"path": "b.rs"}},
                    {"type": "text", "text": "done"}
                ]
            }),
            serde_json::json!({
                "role": "user",
                "content": [
                    {"type": "tool_result", "tool_use_id": "t2", "content": "a", "is_error": false},
                    {"type": "tool_result", "tool_use_id": "t3", "content": "b", "is_error": false}
                ]
            }),
        ];
        let display = transcript_messages_to_display(&msgs);
        let histories: Vec<_> = display
            .iter()
            .flat_map(|m| m.parts.iter())
            .filter_map(|p| match p {
                DisplayPart::ToolHistory(h) => Some(h),
                _ => None,
            })
            .collect();
        assert_eq!(histories.len(), 2);
        assert_eq!(histories[0].tool_count, 1);
        assert_eq!(histories[1].tool_count, 2);
        assert!(!display
            .iter()
            .flat_map(|m| m.parts.iter())
            .any(|p| matches!(p, DisplayPart::Tool(_))));
    }

    #[test]
    fn tool_result_matches_by_id_not_name() {
        let mut msg = DisplayMessage {
            role: MessageRole::Assistant,
            content: String::new(),
            thinking: None,
            parts: vec![
                DisplayPart::Tool(DisplayToolCall {
                    id: "a".into(),
                    name: "Bash".into(),
                    input_summary: "one".into(),
                    output: None,
                    is_error: false,
                    collapsed: true,
                    started_at: None,
                    stopping: false,
                }),
                DisplayPart::Tool(DisplayToolCall {
                    id: "b".into(),
                    name: "Bash".into(),
                    input_summary: "two".into(),
                    output: None,
                    is_error: false,
                    collapsed: true,
                    started_at: None,
                    stopping: false,
                }),
            ],
            tool_calls: Vec::new(),
            delivery: crate::prompt_queue::DeliveryState::Sent,
            idempotency_key: None,
        };
        let tc = msg.find_tool_for_result_mut("b", "Bash").unwrap();
        tc.output = Some("second".into());
        assert!(msg.parts.iter().any(|p| matches!(
            p,
            DisplayPart::Tool(t) if t.id == "b" && t.output.as_deref() == Some("second")
        )));
        assert!(msg.parts.iter().any(|p| matches!(
            p,
            DisplayPart::Tool(t) if t.id == "a" && t.output.is_none()
        )));
    }
}
