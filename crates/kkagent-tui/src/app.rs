use crossterm::{
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyModifiers,
        MouseButton, MouseEventKind,
    },
    execute,
    terminal::{
        disable_raw_mode, enable_raw_mode, BeginSynchronizedUpdate, EndSynchronizedUpdate,
        EnterAlternateScreen, LeaveAlternateScreen,
    },
};
use kkagent_client::{KkagentClient, RpcConnectionState};
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

pub const TOOL_EXPAND_TURNS: usize = 5;
pub(crate) const SPINNER_TICKS_PER_FRAME: usize = 4;
const STREAM_DRAW_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

/// Default cadence for self-healing full repaints. Ratatui only rewrites cells
/// that differ from its own previous buffer, so once the physical terminal
/// diverges (torn frame over SSH, an injected escape byte, a dropped write) the
/// stale cells survive every later diff frame — "ghost" artifacts. Forcing a
/// clear + full repaint periodically resynchronizes the terminal with the
/// buffer model. The clear happens inside the synchronized-update window, so
/// compliant terminals never show an intermediate blank frame.
const DEFAULT_FULL_REPAINT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// `KKAGENT_FULL_REPAINT_SECS` overrides the full-repaint cadence; `0` disables
/// the periodic self-heal (Ctrl+L keeps working).
fn full_repaint_interval_from_env() -> std::time::Duration {
    std::env::var("KKAGENT_FULL_REPAINT_SECS")
        .ok()
        .as_deref()
        .map(parse_full_repaint_interval)
        .unwrap_or(DEFAULT_FULL_REPAINT_INTERVAL)
}

fn parse_full_repaint_interval(raw: &str) -> std::time::Duration {
    raw.trim()
        .parse::<u64>()
        .map(std::time::Duration::from_secs)
        .unwrap_or(DEFAULT_FULL_REPAINT_INTERVAL)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServerEventRedraw {
    None,
    Stream,
    Immediate,
}

fn server_event_redraw(frame: &Frame) -> ServerEventRedraw {
    let Frame::Event { event, data, .. } = frame else {
        return ServerEventRedraw::None;
    };
    if event == "mcp.status" {
        return ServerEventRedraw::Immediate;
    }
    match data.get("type").and_then(serde_json::Value::as_str) {
        Some("message_delta" | "thinking_delta" | "btw_delta" | "btw_thinking_delta") => {
            ServerEventRedraw::Stream
        }
        Some("heartbeat") | None => ServerEventRedraw::None,
        Some(_) => ServerEventRedraw::Immediate,
    }
}

fn stream_redraw_due(
    pending: bool,
    last_draw_at: std::time::Instant,
    now: std::time::Instant,
) -> bool {
    pending && now.saturating_duration_since(last_draw_at) >= STREAM_DRAW_INTERVAL
}

pub struct TuiApp {
    config: AppConfig,
    config_path: PathBuf,
    client: KkagentClient,
    state: AppState,
    mouse_mode: MouseMode,
    jobs: crate::async_jobs::AsyncJobHub,
    use_alt_screen: bool,
    remote_connection: bool,
    /// Standalone / --connect mode: Ctrl+B detaches without killing the server.
    allows_background_detach: bool,
    connection_alerted: bool,
    /// Wall-clock anchor for the fixed-rate tick counter.  Without this, bursts
    /// of input events (mouse movement, trackpad scrolling) spin the event loop
    /// faster and accelerate the spinner / loading animations.
    last_tick_at: std::time::Instant,
    /// Set by Ctrl+L (and after terminal-size changes): the next frame clears
    /// the screen and repaints every cell instead of diffing, healing any
    /// divergence between the terminal and ratatui's buffer model.
    force_full_redraw: bool,
}

fn tick_requires_redraw(previous: usize, current: usize, animation_active: bool) -> bool {
    (animation_active && previous / SPINNER_TICKS_PER_FRAME != current / SPINNER_TICKS_PER_FRAME)
        || previous / 80 != current / 80
        || previous / 100 != current / 100
}

#[derive(Debug, Clone, PartialEq)]
pub enum AppMode {
    Normal,
    Shell,
    Plan,
    Btw,
}

pub struct AppState {
    pub messages: Vec<DisplayMessage>,
    pub input: InputState,
    pub status: SessionStatus,
    pub permission_mode: PermissionMode,
    pub plan_mode: bool,
    pub session_id: Option<String>,
    /// Server-authoritative working directory for the active session.
    pub working_dir: std::path::PathBuf,
    pub mode: AppMode,
    pub should_quit: bool,
    pub quit_confirm: bool,
    /// Shown when quitting while a turn is still running (standalone mode).
    pub quit_dialog: Option<QuitDialogState>,
    pub thinking_text: String,
    /// Assistant display message receiving deltas for the current model step.
    pub active_assistant_message: Option<usize>,
    /// 离底部向上滚动的行数；0 = 贴底跟随新消息
    pub scroll_up: u16,
    pub content_lines: u16,
    pub viewport_height: u16,
    /// When true, keep transcript pinned to the latest content.
    pub follow_bottom: bool,
    /// Previous-frame content height. When the user has scrolled up
    /// (`!follow_bottom`) and new content streams in at the bottom, we
    /// grow `scroll_up` by the delta so the user's viewport stays anchored
    /// on what they were reading instead of jumping. `None` means "next
    /// render should not compensate" (e.g. just after a session switch).
    pub prev_content_lines: Option<u16>,
    /// Line index (from top) where each `messages[i]` starts — updated each frame.
    pub message_line_starts: Vec<u16>,
    /// Global Ctrl-O display mode for expandable tool output.
    pub tool_output_expanded: bool,
    /// Expand/collapse hints rendered on the last transcript frame.
    pub tool_expand_hits: Vec<ToolExpandHit>,
    /// Keep a clicked tool hint at the same viewport row after its height changes.
    pub pending_tool_click_anchor: Option<(usize, u16)>,
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
    /// Recent click history for double/triple click word/line selection.
    pub click_history: Vec<ClickRecord>,
    pub approx_tokens: u64,
    pub approval_pending: Option<PendingApproval>,
    /// True while BTW hid a visible approval modal on entry so it can be
    /// restored on exit. Distinguishes a BTW-driven hide from a user Esc fold.
    pub btw_hid_approval: bool,
    /// Additional approvals waiting behind the active one (FIFO).
    pub approval_queue: std::collections::VecDeque<PendingApproval>,
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
    /// Text entry shown inside the plugin manager (marketplace/source URL or path).
    pub plugin_prompt: Option<PluginPromptState>,
    /// Marketplace currently being browsed by the plugin picker.
    pub plugin_marketplace_source: Option<String>,
    /// Plugin currently being inspected by the plugin picker.
    pub plugin_selected_id: Option<String>,
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
    /// First Esc timestamp for opening history edit on double-Esc.
    pub pending_esc_ms: Option<u128>,
    /// Full prompts backing the history-edit picker.
    pub history_edit_turns: Vec<HistoryEditTurn>,
    /// One-shot composer text applied after the forked session finishes loading.
    pub pending_resume_prefill: Option<(String, String)>,
    /// Sticky todo panel (above input), latest TodoList state.
    pub todos: Vec<TodoItem>,
    /// Expand sticky todo beyond the collapsed max rows.
    pub todos_expanded: bool,
    /// Live / recent subagents for the sticky strip + `/agents` panel.
    pub subagents: crate::subagents::SubagentStore,
    /// Overlay browser for subagent detail (`/agents`).
    pub subagents_panel: Option<crate::subagents::SubagentsPanelState>,
    /// Toggleable full-screen BTW surface, advertised beside the git badge.
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
    /// Sessions whose tab was closed in this window (Ctrl-D). Guards against
    /// stale `sessions.list` responses (raced with the follow-up session
    /// switch) resurrecting the closed indicator in the footer strip.
    pub closed_tab_ids: std::collections::HashSet<String>,
    /// Ephemeral Tab group for this TUI window (`/new` siblings). Not persisted.
    pub open_session_group: Vec<String>,
    /// Preview pane while `/sessions` list is open.
    pub session_picker_preview: Option<SessionPickerPreview>,
    /// Unfiltered session records backing the current/all-workspace picker scopes.
    pub session_picker_entries: Vec<SessionPickerEntry>,
    /// False = current workspace only; true = sessions from every known workspace.
    pub session_picker_all_workspaces: bool,
    /// Pending delete confirmation inside `/sessions` (default = No).
    pub session_delete_confirm: Option<SessionDeleteConfirm>,
    /// When true, show the session picker on the first UI tick (used by `-r` without id).
    pub startup_session_picker: bool,
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
    /// Transcript plan.md block collapsed? Plan messages render as one summary
    /// line unless expanded (Ctrl-O / click), matching tool-output folding.
    pub plan_transcript_collapsed: bool,
    /// A click makes the plan block independent from the global Ctrl-O mode.
    pub plan_transcript_overridden: bool,
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
    /// Cached stable transcript layout; streaming keeps the active tail separate.
    pub transcript_layout_cache: crate::render_cache::TranscriptLayoutCache,
    /// Older transcript pages still loading after a lazy resume.
    pub history_loading: bool,
    /// Absolute index of the oldest message currently shown (for prepend pages).
    pub history_oldest_index: Option<usize>,
    /// Total message count known from the last resume/history response.
    pub history_total: Option<usize>,
    /// Per-session draft / scroll / search state (survives switches).
    pub session_views: std::collections::HashMap<String, crate::session_view::SessionViewState>,
    /// Full runtime state for inactive sessions opened in this TUI.
    pub session_runtime_states: std::collections::HashMap<String, SessionRuntimeState>,
    /// Agent events received while their session is not focused.
    pub background_session_events:
        std::collections::HashMap<String, std::collections::VecDeque<AgentEvent>>,
    /// Serialized byte estimate parallel to `background_session_events`.
    pub background_session_event_bytes: std::collections::HashMap<String, usize>,
    /// Debounced `/sessions` preview target.
    pub preview_debounce: Option<PreviewDebounce>,
    /// Debounced `@` file completion request (avoids sync walks on every key).
    pub file_complete_debounce: Option<FileCompleteDebounce>,
    /// LRU cache of session.preview JSON payloads.
    pub preview_cache: crate::session_view::PreviewLru,
    /// Queued prompts waiting for the current turn to finish.
    pub prompt_queue: crate::prompt_queue::PromptQueue,
    /// When session is busy, Enter queues by default (Ctrl-S steers immediately).
    pub queue_when_busy: bool,
    /// Last session-switch latency samples (ms) for regression awareness.
    pub last_switch_metrics: Option<SessionSwitchMetrics>,
    /// Cumulative token usage for the active session (server-authoritative).
    pub usage_session: SessionUsageTotals,
    /// Recent per-turn usage samples for `/usage`.
    pub usage_turns: Vec<TurnUsageSample>,
    /// Most recent single LLM call's usage (context-size anchor + cache stats
    /// for the "Latest request" section in `/usage`).
    pub last_step_usage: Option<kkagent_protocol::TokenUsage>,
    /// Server-authoritative per-part context breakdown (system/tools/…) for
    /// the active session.
    pub context_breakdown: Option<kkagent_protocol::ContextBreakdownInfo>,
    /// Transient copy feedback shown for 1.5s after a successful copy.
    pub copy_toast: Option<CopyToast>,
}

#[derive(Debug, Clone)]
pub struct CopyToast {
    pub message: String,
    pub until: std::time::Instant,
}

#[derive(Debug, Clone, Copy)]
pub struct ClickRecord {
    pub at: crate::selection::CellPos,
    pub when: std::time::Instant,
    pub count: u8,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionUsageTotals {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub steps: u64,
    pub turns: u64,
    /// Provider semantics of `input_tokens`: `Some(false)` = Anthropic
    /// (excludes cache buckets), `Some(true)` = OpenAI/Gemini (includes them),
    /// `None` = unknown — use heuristic (`cache_creation > 0` → add buckets).
    pub input_includes_cache: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct TurnUsageSample {
    pub model: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    /// Provider semantics of `input_tokens` for this turn's provider.
    pub input_includes_cache: Option<bool>,
    pub duration_ms: u64,
}

#[derive(Debug, Clone)]
pub struct SessionRuntimeState {
    pub messages: Vec<DisplayMessage>,
    pub status: SessionStatus,
    pub permission_mode: PermissionMode,
    pub plan_mode: bool,
    pub working_dir: std::path::PathBuf,
    pub mode: AppMode,
    pub thinking_text: String,
    pub active_assistant_message: Option<usize>,
    pub approx_tokens: u64,
    pub approval_queue: std::collections::VecDeque<PendingApproval>,
    pub todos: Vec<TodoItem>,
    pub subagents: crate::subagents::SubagentStore,
    pub model_alias: Option<String>,
    pub last_tool_name: Option<String>,
    pub plan_document: Option<PlanDocument>,
    pub plan_scroll_to_top: bool,
    pub plan_transcript_collapsed: bool,
    pub plan_transcript_overridden: bool,
    pub turn_started_at: Option<std::time::Instant>,
    pub tokens_at_turn_start: u64,
    pub history_oldest_index: Option<usize>,
    pub history_total: Option<usize>,
    pub prompt_queue: crate::prompt_queue::PromptQueue,
    pub usage_session: SessionUsageTotals,
    pub usage_turns: Vec<TurnUsageSample>,
    /// Most recent single LLM call's usage (server-authoritative snapshot
    /// `last_step_usage`); powers the "Latest request" section in `/usage`.
    pub last_step_usage: Option<kkagent_protocol::TokenUsage>,
    pub context_breakdown: Option<kkagent_protocol::ContextBreakdownInfo>,
    pub copy_toast: Option<CopyToast>,
    pub click_history: Vec<ClickRecord>,
}

impl SessionRuntimeState {
    fn capture(state: &AppState) -> Self {
        Self {
            messages: state.messages.clone(),
            status: state.status,
            permission_mode: state.permission_mode,
            plan_mode: state.plan_mode,
            working_dir: state.working_dir.clone(),
            mode: if state.mode == AppMode::Btw {
                if state.plan_mode {
                    AppMode::Plan
                } else {
                    AppMode::Normal
                }
            } else {
                state.mode.clone()
            },
            thinking_text: state.thinking_text.clone(),
            active_assistant_message: state.active_assistant_message,
            approx_tokens: state.approx_tokens,
            approval_queue: state.approval_queue.clone(),
            todos: state.todos.clone(),
            subagents: state.subagents.clone(),
            model_alias: state.model_alias.clone(),
            last_tool_name: state.last_tool_name.clone(),
            plan_document: state.plan_document.clone(),
            plan_scroll_to_top: state.plan_scroll_to_top,
            plan_transcript_collapsed: state.plan_transcript_collapsed,
            plan_transcript_overridden: state.plan_transcript_overridden,
            turn_started_at: state.turn_started_at,
            tokens_at_turn_start: state.tokens_at_turn_start,
            history_oldest_index: state.history_oldest_index,
            history_total: state.history_total,
            prompt_queue: state.prompt_queue.clone(),
            usage_session: state.usage_session.clone(),
            usage_turns: state.usage_turns.clone(),
            last_step_usage: state.last_step_usage.clone(),
            context_breakdown: state.context_breakdown.clone(),
            copy_toast: state.copy_toast.clone(),
            click_history: state.click_history.clone(),
        }
    }

    fn restore(self, state: &mut AppState) {
        state.messages = self.messages;
        state.status = self.status;
        state.permission_mode = self.permission_mode;
        state.plan_mode = self.plan_mode;
        state.working_dir = self.working_dir;
        state.mode = self.mode;
        state.thinking_text = self.thinking_text;
        state.active_assistant_message = self.active_assistant_message;
        state.approx_tokens = self.approx_tokens;
        state.approval_queue = self.approval_queue;
        state.todos = self.todos;
        state.subagents = self.subagents;
        state.model_alias = self.model_alias;
        state.last_tool_name = self.last_tool_name;
        state.plan_document = self.plan_document;
        state.plan_scroll_to_top = self.plan_scroll_to_top;
        state.plan_transcript_collapsed = self.plan_transcript_collapsed;
        state.plan_transcript_overridden = self.plan_transcript_overridden;
        state.turn_started_at = self.turn_started_at;
        state.tokens_at_turn_start = self.tokens_at_turn_start;
        state.history_loading = false;
        state.history_oldest_index = self.history_oldest_index;
        state.history_total = self.history_total;
        state.prompt_queue = self.prompt_queue;
        state.usage_session = self.usage_session;
        state.usage_turns = self.usage_turns;
        state.last_step_usage = self.last_step_usage;
        state.context_breakdown = self.context_breakdown;
        state.copy_toast = self.copy_toast;
        state.click_history = self.click_history;
        state.stream_cursor = crate::streaming::StreamingCursor::default();
        state.render_cache.invalidate_all();
        state.transcript_layout_cache.invalidate();
        state.status_bar.cache_hit = None;
        state.event_router.status = state.status;
        state.event_router.turn_active = matches!(
            state.status,
            SessionStatus::Thinking
                | SessionStatus::ToolExecuting
                | SessionStatus::WaitingApproval
                | SessionStatus::WaitingQuestion
                | SessionStatus::Compacting
                | SessionStatus::Cancelling
        );
        state.apply_tool_output_mode();
        state.tool_expand_hits.clear();
        state.pending_tool_click_anchor = None;
    }
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
pub struct FileCompleteDebounce {
    pub token_start: usize,
    pub query: String,
    pub quoted: bool,
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
    /// Decide whether fallback remains enabled when primary equals global fallback.
    FallbackDecision,
    /// Select a session-specific fallback model.
    FallbackModel,
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
    /// Session usage panel (`/usage`); `__turns__` row drills into per-turn detail.
    Usage,
    /// Per-turn usage detail (submenu of `/usage`).
    UsageTurns,
    /// Slash command catalogue (`/help`).
    Help,
    /// Prompt templates (`/prompts`).
    Prompts,
    /// Swarm mode actions (`/swarm`).
    Swarm,
    /// Select an earlier user prompt, fork before it, and edit the prompt.
    HistoryEdit,
    PluginHome,
    PluginInstalled,
    PluginInstalledDetail,
    PluginMarketplaces,
    PluginMarketplaceEntries,
    PluginMarketplaceDetail,
    PluginConfirm,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginPromptKind {
    AddMarketplace,
    InstallSource,
}

#[derive(Debug, Clone)]
pub struct PluginPromptState {
    pub kind: PluginPromptKind,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEditTurn {
    pub turn_index: usize,
    pub message_index: usize,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct ListPickerItem {
    pub id: String,
    pub label: String,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub struct SessionPickerEntry {
    pub item: ListPickerItem,
    pub workspace: String,
    pub same_workspace: bool,
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

/// Dialog when quitting while an agent turn is still running.
#[derive(Debug, Clone)]
pub struct QuitDialogState {
    /// 0 = Terminate, 1 = Background, 2 = Cancel
    pub selected: usize,
}

#[derive(Debug, Clone)]
pub struct ResumeSwitchCtx {
    pub target: String,
    pub leaving_id: Option<String>,
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
    pub command: String,
    pub elapsed_secs: u64,
}

#[derive(Debug, Clone)]
pub struct TaskDetailState {
    pub task_id: String,
    pub status: String,
    pub running: bool,
    pub exit_code: Option<i64>,
    pub description: String,
    pub command: String,
    pub elapsed_secs: u64,
    pub output: String,
    pub scroll: u16,
}

#[derive(Debug, Clone)]
pub struct TasksPanelState {
    pub tasks: Vec<TaskInfo>,
    pub selected: usize,
    /// When set, the panel shows this job's scrolling output view.
    pub detail: Option<TaskDetailState>,
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
    /// A mouse click makes this item independent from the global Ctrl-O mode.
    pub user_overridden: bool,
    pub tools: Vec<DisplayToolCall>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolExpandTarget {
    Part {
        message: usize,
        part: usize,
    },
    Legacy {
        message: usize,
        tool: usize,
    },
    /// Transcript plan.md block (summary line ↔ full document box).
    Plan {
        message: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolExpandHit {
    /// Absolute visual transcript line, before viewport scrolling.
    pub line: usize,
    pub target: ToolExpandTarget,
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
    /// Full plan.md contents shown after WritePlan in plan mode.
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
    /// A mouse click makes this item independent from the global Ctrl-O mode.
    pub user_overridden: bool,
    pub started_at: Option<std::time::Instant>,
    pub stopping: bool,
    /// When set, the scheduler has not started this tool yet.
    pub queued_behind: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TodoItem {
    pub id: String,
    pub content: String,
    pub status: String,
}

impl TodoItem {
    pub(crate) fn is_finished(&self) -> bool {
        matches!(self.status.as_str(), "completed" | "done" | "cancelled")
    }
}

fn all_todos_finished(todos: &[TodoItem]) -> bool {
    !todos.is_empty() && todos.iter().all(TodoItem::is_finished)
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
    /// The plan review stays pending on the server while its modal is folded.
    /// This lets users inspect the transcript without accidentally cancelling it.
    pub hidden: bool,
    /// The original ExitPlanMode waiter disappeared with the previous process;
    /// submit through the restart-safe resolver instead of `approval.respond`.
    pub resumed_plan_review: bool,
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
                scope: Some(kkagent_protocol::ApprovalScope::Once),
            },
            ApprovalChoice {
                label: "allow for turn".into(),
                decision: kkagent_protocol::ApprovalDecision::Approved,
                selected_label: "allow for turn".into(),
                requires_feedback: false,
                scope: Some(kkagent_protocol::ApprovalScope::Turn),
            },
            ApprovalChoice {
                label: "allow for session".into(),
                decision: kkagent_protocol::ApprovalDecision::Approved,
                selected_label: "allow for session".into(),
                requires_feedback: false,
                scope: Some(kkagent_protocol::ApprovalScope::Session),
            },
            ApprovalChoice {
                label: "always allow".into(),
                decision: kkagent_protocol::ApprovalDecision::Approved,
                selected_label: "always allow".into(),
                requires_feedback: false,
                scope: Some(kkagent_protocol::ApprovalScope::Always),
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
    pub selected: usize,
    pub toggled: Vec<bool>,
    pub free_text: String,
}

impl AppState {
    pub fn new(permission_mode: PermissionMode, plan_mode: bool) -> Self {
        let working_dir = std::env::current_dir()
            .ok()
            .and_then(|path| std::fs::canonicalize(path).ok())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        Self {
            messages: Vec::new(),
            input: InputState::new(),
            status: SessionStatus::Idle,
            permission_mode,
            plan_mode,
            session_id: None,
            working_dir: working_dir.clone(),
            mode: if plan_mode {
                AppMode::Plan
            } else {
                AppMode::Normal
            },
            should_quit: false,
            quit_confirm: false,
            quit_dialog: None,
            thinking_text: String::new(),
            active_assistant_message: None,
            scroll_up: 0,
            content_lines: 0,
            viewport_height: 0,
            follow_bottom: true,
            prev_content_lines: None,
            message_line_starts: Vec::new(),
            tool_output_expanded: false,
            tool_expand_hits: Vec::new(),
            pending_tool_click_anchor: None,
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
            click_history: Vec::new(),
            approx_tokens: 0,
            approval_pending: None,
            btw_hid_approval: false,
            approval_queue: std::collections::VecDeque::new(),
            parked_approvals: std::collections::HashMap::new(),
            question_pending: None,
            parked_questions: std::collections::HashMap::new(),
            slash_menu: None,
            skill_slash_commands: Vec::new(),
            skill_command_map: std::collections::HashMap::new(),
            file_menu: None,
            list_picker: None,
            list_picker_stack: Vec::new(),
            plugin_prompt: None,
            plugin_marketplace_source: None,
            plugin_selected_id: None,
            tasks_panel: None,
            pending_prompt: None,
            tick: 0,
            input_history: crate::input_history_store::load(&working_dir),
            history_index: None,
            history_draft: String::new(),
            pending_esc_ms: None,
            history_edit_turns: Vec::new(),
            pending_resume_prefill: None,
            todos: Vec::new(),
            todos_expanded: false,
            subagents: crate::subagents::SubagentStore::default(),
            subagents_panel: None,
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
            closed_tab_ids: std::collections::HashSet::new(),
            open_session_group: Vec::new(),
            session_picker_preview: None,
            session_picker_entries: Vec::new(),
            session_picker_all_workspaces: false,
            session_delete_confirm: None,
            startup_session_picker: false,
            resume_switch: None,
            event_router: SessionEventRouter::default(),
            search: SearchState::default(),
            last_tool_name: None,
            highlight_message: None,
            plan_document: None,
            plan_scroll_to_top: false,
            plan_transcript_collapsed: true,
            plan_transcript_overridden: false,
            turn_started_at: None,
            tokens_at_turn_start: 0,
            locale: crate::i18n::Locale::En,
            render_cache: crate::render_cache::RenderCache::new(),
            transcript_layout_cache: crate::render_cache::TranscriptLayoutCache::default(),
            history_loading: false,
            history_oldest_index: None,
            history_total: None,
            session_views: std::collections::HashMap::new(),
            session_runtime_states: std::collections::HashMap::new(),
            background_session_events: std::collections::HashMap::new(),
            background_session_event_bytes: std::collections::HashMap::new(),
            preview_debounce: None,
            file_complete_debounce: None,
            preview_cache: crate::session_view::PreviewLru::new(12),
            prompt_queue: crate::prompt_queue::PromptQueue::default(),
            queue_when_busy: true,
            last_switch_metrics: None,
            usage_session: SessionUsageTotals::default(),
            usage_turns: Vec::new(),
            last_step_usage: None,
            context_breakdown: None,
            copy_toast: None,
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
        // Fresh plan in transcript starts collapsed; plan-mode viewing goes
        // through the focus overlay, not the transcript block.
        self.plan_transcript_collapsed = true;
        self.plan_transcript_overridden = false;
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

    /// Leave plan mode enabled, but stop the previous plan from replacing the
    /// transcript while the agent is working on revision feedback. The next
    /// PlanFileUpdated event restores focus with the updated document.
    pub fn dismiss_plan_focus(&mut self) {
        self.plan_document = None;
        self.plan_scroll_to_top = false;
        self.follow_bottom = true;
        self.scroll_up = 0;
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
            // Leaving plan mode (shift-tab or plan execution): the transcript
            // plan block becomes collapsible and starts folded.
            self.plan_transcript_collapsed = true;
            self.plan_transcript_overridden = false;
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
        collapse_tools_in_turn(
            &mut self.messages,
            start,
            end,
            duration_ms,
            tokens,
            self.tool_output_expanded,
        );
    }

    /// Reset per-session context/usage statistics back to their fresh-session
    /// defaults. Called when switching to or starting a session so the footer
    /// context indicator reflects the *new* session instead of leaking the
    /// previous one's numbers until the next LLM usage event arrives.
    pub fn reset_context_usage_stats(&mut self) {
        self.approx_tokens = 0;
        self.tokens_at_turn_start = 0;
        self.usage_session = SessionUsageTotals::default();
        self.usage_turns.clear();
        self.last_step_usage = None;
        self.context_breakdown = None;
        self.status_bar.cache_hit = None;
    }

    /// Apply the global tool-output mode to unmodified items in the most recent
    /// `TOOL_EXPAND_TURNS` user turns.
    pub fn apply_tool_output_mode(&mut self) {
        let cutoff = recent_turn_cutoff(&self.messages, TOOL_EXPAND_TURNS);
        for message in &mut self.messages[cutoff..] {
            for part in &mut message.parts {
                match part {
                    DisplayPart::Tool(tool) if !tool.user_overridden => {
                        tool.collapsed = !self.tool_output_expanded;
                    }
                    DisplayPart::ToolHistory(history) if !history.user_overridden => {
                        history.expanded = self.tool_output_expanded;
                    }
                    _ => {}
                }
            }
            for tool in &mut message.tool_calls {
                if !tool.user_overridden {
                    tool.collapsed = !self.tool_output_expanded;
                }
            }
        }
        // The transcript plan block is a single one-off block (not per-turn
        // tool output), so Ctrl-O applies globally, ignoring the turn cutoff.
        if !self.plan_transcript_overridden {
            self.plan_transcript_collapsed = !self.tool_output_expanded;
        }
    }

    pub fn max_scroll_up(&self) -> u16 {
        self.content_lines
            .saturating_sub(self.viewport_height.max(1))
    }

    /// Compensate `scroll_up` for content-height changes so the viewport
    /// stays anchored on what the user is reading while new output streams
    /// in at the bottom. Returns the previous-frame content height to
    /// cache for the next call.
    ///
    /// Only active when `!follow_bottom` (user scrolled up). When
    /// `prev_content_lines` is `None` (e.g. just after a session switch),
    /// no compensation is applied — we only record the current height.
    pub fn compensate_scroll_anchor(&mut self, new_content_height: u16) {
        if !self.follow_bottom {
            if let Some(prev) = self.prev_content_lines {
                if new_content_height > prev {
                    // Content grew: push the anchor down by the same amount,
                    // saturating instead of truncating through a u16 cast.
                    self.scroll_up = self.scroll_up.saturating_add(new_content_height - prev);
                } else if new_content_height < prev {
                    // Content shrank: pull the anchor back, never below 0.
                    self.scroll_up = self.scroll_up.saturating_sub(prev - new_content_height);
                }
            }
            let max = new_content_height.saturating_sub(self.viewport_height.max(1));
            self.scroll_up = self.scroll_up.min(max);
        }
        self.prev_content_lines = Some(new_content_height);
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
        self.input_history = crate::input_history_store::push(&self.working_dir, t);
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
            self.file_complete_debounce = None;
            return;
        }
        let text = self.input.text.clone();
        let cursor = self.input.cursor.min(text.len());
        let Some((token_start, query)) = crate::pi::autocomplete::extract_at_token(&text, cursor)
        else {
            self.file_menu = None;
            self.file_complete_debounce = None;
            return;
        };
        let quoted = text[token_start..].starts_with("@\"");
        // Keep the previous menu visible while a new background scan runs, but
        // update the token metadata so Tab still applies against the live cursor.
        if let Some(menu) = self.file_menu.as_mut() {
            menu.token_start = token_start;
            menu.query = query.clone();
            menu.quoted = quoted;
        } else {
            self.file_menu = Some(FileMenuState {
                items: Vec::new(),
                selected: 0,
                token_start,
                query: query.clone(),
                quoted,
            });
        }
        self.file_complete_debounce = Some(FileCompleteDebounce {
            token_start,
            query,
            quoted,
            due_at: std::time::Instant::now() + std::time::Duration::from_millis(100),
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
            config_path: kkagent_config::default_config_path(),
            client,
            state: AppState::new(permission_mode, plan_mode),
            mouse_mode: MouseMode::from_env(),
            jobs: crate::async_jobs::AsyncJobHub::new(),
            use_alt_screen: true,
            remote_connection: false,
            allows_background_detach: false,
            connection_alerted: false,
            last_tick_at: std::time::Instant::now(),
            force_full_redraw: false,
        }
    }

    pub fn set_use_alt_screen(&mut self, enabled: bool) {
        self.use_alt_screen = enabled;
    }

    pub fn set_remote_connection(&mut self, enabled: bool) {
        self.remote_connection = enabled;
    }

    pub fn set_allows_background_detach(&mut self, enabled: bool) {
        self.allows_background_detach = enabled;
    }

    pub fn set_config_path(&mut self, path: PathBuf) {
        self.config_path = path;
    }

    /// New sessions inherit the global config default until `/model` overrides them.
    fn bind_config_default_model(&mut self) {
        self.state.model_alias = self.config.default_model_alias().map(|s| s.to_string());
    }

    /// Apply a user-triggered plan-mode change without letting a transient RPC
    /// failure tear down the TUI or leave its local state ahead of the server.
    async fn set_plan_mode_from_ui(&mut self, enabled: bool) -> bool {
        if self.state.resume_switch.is_some() {
            self.system_message(
                "Session switch is still loading; try plan mode again when it finishes.".into(),
            );
            return false;
        }
        let Some(session_id) = self.state.session_id.clone() else {
            self.system_message("Cannot change plan mode without an active session.".into());
            return false;
        };
        match self.client.set_plan_mode(&session_id, enabled).await {
            Ok(()) => {
                self.state.on_plan_mode_changed(enabled);
                true
            }
            Err(error) => {
                self.system_message(format!("Plan mode change failed: {error}"));
                false
            }
        }
    }

    pub async fn run(mut self, resume: Option<Option<String>>) -> anyhow::Result<()> {
        let startup_started = std::time::Instant::now();
        let startup_trust = if self.config.sandbox.is_disabled() {
            None
        } else {
            self.config
                .workspace_trust
                .matching(&self.state.working_dir)
                .cloned()
        };
        if let Some(trust) = startup_trust {
            self.client
                .rpc_call("workspace.trust", Some(serde_json::to_value(trust)?))
                .await?;
        }

        // Create / resume session BEFORE taking over the terminal, so RPC
        // failures don't leave the user's shell stuck in raw/alternate mode.
        let cwd = self.state.working_dir.to_string_lossy().into_owned();
        match resume {
            Some(Some(id)) => {
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
            }
            Some(None) => {
                // `-r` / `--resume` with no id: show the session picker once we enter the UI loop.
                self.state.startup_session_picker = true;
                let session_id = self
                    .client
                    .create_session(Some(&cwd), Some(self.state.permission_mode))
                    .await?;
                self.state.tab_strip.ensure_active(&session_id, "main");
                self.state.status_bar.session_id = Some(session_id.clone());
                self.state.session_id = Some(session_id);
                self.bind_config_default_model();
            }
            None => {
                let session_id = self
                    .client
                    .create_session(Some(&cwd), Some(self.state.permission_mode))
                    .await?;
                self.state.tab_strip.ensure_active(&session_id, "main");
                self.state.status_bar.session_id = Some(session_id.clone());
                self.state.session_id = Some(session_id);
                self.bind_config_default_model();
            }
        }
        tracing::info!(
            elapsed_ms = startup_started.elapsed().as_millis() as u64,
            "TUI session ready"
        );

        // With sandboxing enabled, startup review or static config must have
        // established trust before the server creates this session.
        let cwd_path = std::path::PathBuf::from(&cwd);
        if !self.config.sandbox.is_disabled()
            && self.config.workspace_trust.matching(&cwd_path).is_none()
        {
            self.system_message(format!(
                "Untrusted workspace {}. Restart kkagent and complete the workspace trust review.",
                cwd_path.display()
            ));
        }

        // Validate optional keybinding overrides without locking the user out.
        if let Err(e) = crate::pi::keybindings::validate_overrides(&self.config.ui.keybindings) {
            self.system_message(format!(
                "Keybinding config warning: {e} — defaults kept for interrupt/submit"
            ));
        }

        if let Some(hint) =
            crate::version_check::idle_hint(env!("CARGO_PKG_VERSION"), self.config.ui.check_updates)
        {
            self.system_message(hint);
        }
        if self.config.ui.check_updates && crate::version_check::cache_is_stale() {
            self.jobs.spawn_version_check();
        }

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

        // A panic in the event loop unwinds past the teardown at the bottom of
        // this function; the guard restores the terminal from the panic hook
        // so the shell stays usable (see panic_guard for the thread gating).
        crate::panic_guard::install();
        crate::panic_guard::set_active(true);
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
        if self.use_alt_screen {
            if let Err(e) = execute!(stdout, EnterAlternateScreen, EnableBracketedPaste) {
                let _ = disable_raw_mode();
                return Err(e.into());
            }
        } else if let Err(e) = execute!(stdout, EnableBracketedPaste) {
            let _ = disable_raw_mode();
            return Err(e.into());
        }
        tracing::info!(
            elapsed_ms = startup_started.elapsed().as_millis() as u64,
            alt_screen = self.use_alt_screen,
            "TUI first frame ready"
        );
        if let Err(e) = self.mouse_mode.enable(&mut stdout) {
            if self.use_alt_screen {
                let _ = execute!(stdout, DisableBracketedPaste, LeaveAlternateScreen);
            } else {
                let _ = execute!(stdout, DisableBracketedPaste);
            }
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

        // In-process mode: interrupt before aborting the paired server task.
        // Standalone / --connect: leave turns running so Ctrl+B and Background quit work.
        if let Some(ref id) = sid {
            if !self.allows_background_detach {
                let _ = self.client.interrupt(id).await;
            }
            if empty {
                let _ = self.discard_session_record(id).await;
            }
        }

        // Always restore the terminal, even if the loop failed.
        crate::panic_guard::set_active(false);
        let _ = disable_raw_mode();
        let _ = self.mouse_mode.disable(terminal.backend_mut());
        if self.use_alt_screen {
            let _ = execute!(
                terminal.backend_mut(),
                DisableBracketedPaste,
                LeaveAlternateScreen
            );
        } else {
            let _ = execute!(terminal.backend_mut(), DisableBracketedPaste);
        }
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
        self.jobs.refresh_busy_notices();
        self.state.status_bar.activity = self.jobs.active_notice_text();
        self.draw_frame(terminal)?;
        let mut last_draw_at = std::time::Instant::now();
        let mut last_full_redraw_at = std::time::Instant::now();
        let full_repaint_interval = full_repaint_interval_from_env();
        let mut stream_redraw_pending = false;

        loop {
            let mut redraw = false;
            let previous_activity = self.state.status_bar.activity.clone();
            self.jobs.refresh_busy_notices();
            // Expose async notice / MCP status to the renderer via status_bar activity.
            self.state.status_bar.activity = self.jobs.active_notice_text();
            redraw |= self.state.status_bar.activity != previous_activity;
            if self.state.startup_session_picker {
                self.state.startup_session_picker = false;
                let _ = self.open_session_picker().await;
                redraw = true;
            }

            // Drain the full event queue each frame so trackpad bursts stay in-app
            // (one-event-per-poll left a backlog that felt like lag / terminal scroll).
            let mut saw_event = false;
            let mut event_changed = false;
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
                        event_changed = true;
                    }
                    Event::Mouse(mouse) if self.mouse_mode == MouseMode::Capture => {
                        event_changed |= self.collect_mouse(mouse, &mut scroll_delta);
                    }
                    Event::Paste(text) => {
                        self.flush_pending_scroll(&mut scroll_delta);
                        if let Some(prompt) = self.state.plugin_prompt.as_mut() {
                            prompt.value.push_str(&text.replace(['\r', '\n'], ""));
                        } else {
                            let fold = self.state.mode != AppMode::Shell;
                            self.state.input.paste_chunk(&text);
                            self.state.input.force_flush_paste(fold);
                            self.state.refresh_slash_menu();
                        }
                        event_changed = true;
                    }
                    Event::Resize(_, _) => {
                        // Repaint immediately on size change instead of waiting
                        // for the next event / idle tick: the terminal already
                        // reflowed its cells, so the stale diff model would
                        // otherwise keep ghost content on screen.
                        self.force_full_redraw = true;
                        event_changed = true;
                    }
                    _ => {}
                }
            }
            self.flush_pending_scroll(&mut scroll_delta);
            redraw |= event_changed;

            if let Some(action) = self.state.pending_strip_action.take() {
                redraw = true;
                match action {
                    StripAction::Switch(id) => {
                        let _ = self.activate_workspace_target(&id).await;
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
                    redraw = true;
                }
            }

            if let Some(prompt) = self.state.pending_prompt.take() {
                self.state.input.set_text(prompt);
                self.submit_input().await?;
                redraw = true;
            }

            // Periodically persist the composer draft for the active session.
            if self.state.tick.is_multiple_of(20) {
                if let Some(sid) = self.state.session_id.clone() {
                    persist_composer_draft(&sid, &self.state.input);
                }
            }

            redraw |= self.drain_job_results().await;
            let current_activity = self.jobs.active_notice_text();
            if self.state.status_bar.activity != current_activity {
                self.state.status_bar.activity = current_activity;
                redraw = true;
            }

            while let Ok(frame) = self.client.event_rx.try_recv() {
                match server_event_redraw(&frame) {
                    ServerEventRedraw::None => {}
                    ServerEventRedraw::Stream => stream_redraw_pending = true,
                    ServerEventRedraw::Immediate => redraw = true,
                }
                self.handle_server_event(frame);
            }
            self.start_next_btw_question().await;
            redraw |= self.refresh_connection_notice();
            if self
                .state
                .copy_toast
                .as_ref()
                .is_some_and(|t| std::time::Instant::now() >= t.until)
            {
                self.state.copy_toast = None;
                redraw = true;
            }

            let previous_tick = self.state.tick;
            // Tick advances at a fixed 50 ms wall-clock cadence, not per loop
            // iteration.  Without this guard, bursts of input events (mouse
            // movement, trackpad scrolling) spin the loop faster and accelerate
            // the spinner / loading animations.
            let now = std::time::Instant::now();
            if now.duration_since(self.last_tick_at) >= std::time::Duration::from_millis(50) {
                self.last_tick_at = now;
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
                if self.state.tick.is_multiple_of(10) {
                    self.state
                        .subagents
                        .prune_finished(std::time::Instant::now());
                }
            }
            self.flush_preview_debounce();
            self.flush_file_complete_debounce();
            if matches!(
                self.state.status,
                SessionStatus::Thinking | SessionStatus::ToolExecuting
            ) {
                redraw |= self.state.stream_cursor.tick();
            }

            let animation_active = matches!(
                self.state.status,
                SessionStatus::Thinking
                    | SessionStatus::ToolExecuting
                    | SessionStatus::WaitingApproval
                    | SessionStatus::WaitingQuestion
                    | SessionStatus::Compacting
                    | SessionStatus::Cancelling
            ) || self.state.workspace_sessions.entries.iter().any(|entry| {
                matches!(
                    entry.status,
                    SessionStatus::Thinking
                        | SessionStatus::ToolExecuting
                        | SessionStatus::WaitingApproval
                        | SessionStatus::WaitingQuestion
                        | SessionStatus::Compacting
                        | SessionStatus::Cancelling
                )
            }) || self.state.subagents.any_active();
            redraw |= tick_requires_redraw(previous_tick, self.state.tick, animation_active);
            redraw |= stream_redraw_due(stream_redraw_pending, last_draw_at, now);
            redraw |= crate::git_badge::take_updated();
            // Periodic self-heal: even without any local trigger, resync the
            // terminal with the buffer model so ghost cells from a torn SSH
            // frame or injected escape byte cannot outlive one interval.
            if full_repaint_interval > std::time::Duration::ZERO
                && now.saturating_duration_since(last_full_redraw_at) >= full_repaint_interval
            {
                self.force_full_redraw = true;
            }
            redraw |= self.force_full_redraw;

            if self.state.should_quit {
                break;
            }
            if redraw {
                self.draw_frame(terminal)?;
                last_draw_at = std::time::Instant::now();
                stream_redraw_pending = false;
                if self.force_full_redraw {
                    self.force_full_redraw = false;
                    last_full_redraw_at = now;
                }
            }
        }
        Ok(())
    }

    fn draw_frame(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    ) -> anyhow::Result<()> {
        // Keep the panic-guard owner fresh: tokio may migrate this task to
        // another worker thread between awaits, and the guard only restores
        // the terminal for the thread that is actually driving the display.
        crate::panic_guard::note_owner_thread();
        // A forced frame clears the screen and repaints every cell. The clear
        // is queued after BeginSynchronizedUpdate so terminals supporting
        // synchronized updates (mode 2026) swap it in atomically — no blank
        // flash on compliant terminals, including over SSH.
        if self.force_full_redraw {
            terminal.clear()?;
        }
        crossterm::queue!(terminal.backend_mut(), BeginSynchronizedUpdate)?;
        let draw_result = terminal
            .draw(|frame| components::render_ui(frame, &mut self.state, &self.config))
            .map(|_| ());
        let end_result = execute!(terminal.backend_mut(), EndSynchronizedUpdate);
        draw_result?;
        end_result?;
        Ok(())
    }

    fn refresh_connection_notice(&mut self) -> bool {
        match self.client.connection_state() {
            RpcConnectionState::Connected => {
                self.connection_alerted = false;
                false
            }
            RpcConnectionState::Disconnected { reason } if !self.connection_alerted => {
                self.connection_alerted = true;
                self.system_message(connection_loss_message(self.remote_connection, &reason));
                true
            }
            RpcConnectionState::Disconnected { .. } => false,
        }
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

    async fn drain_job_results(&mut self) -> bool {
        let mut received = false;
        while let Some(outcome) = self.jobs.try_recv() {
            received = true;
            let may_finish_out_of_order = matches!(
                outcome.payload,
                crate::async_jobs::JobPayload::LocalShell { .. }
                    | crate::async_jobs::JobPayload::Prompt { .. }
            );
            // Local shells and prompts from different sessions may complete out of order.
            if !may_finish_out_of_order
                && !self.jobs.is_current(outcome.channel, outcome.generation)
            {
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
                                self.clear_failed_resume(&query);
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
                            self.clear_failed_resume(&query);
                            // If this was a startup resume and no session is
                            // active yet, create a fresh session so the user
                            // isn't left stranded with an empty TUI.
                            let needs_fallback = self.state.session_id.is_none();
                            if needs_fallback {
                                match self
                                    .client
                                    .create_session(
                                        Some(&self.state.working_dir.to_string_lossy()),
                                        Some(self.state.permission_mode),
                                    )
                                    .await
                                {
                                    Ok(session_id) => {
                                        self.state.tab_strip.ensure_active(&session_id, "main");
                                        self.state.status_bar.session_id = Some(session_id.clone());
                                        self.state.session_id = Some(session_id);
                                        self.bind_config_default_model();
                                        // Clear the stale marker so the next
                                        // launch doesn't repeat this failure.
                                        kkagent_config::clear_active_session();
                                        self.system_message(format!(
                                            "Previous session could not be resumed ({err}). Started a new session."
                                        ));
                                    }
                                    Err(create_err) => {
                                        self.jobs.push_error(
                                            Some(channel),
                                            Some(generation),
                                            format!("Resume failed: {err}; new session also failed: {create_err}"),
                                            true,
                                            0,
                                        );
                                    }
                                }
                            } else {
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
                    as_steer,
                    result,
                } => {
                    self.jobs.mark_done(channel, generation);
                    let is_current = self.state.session_id.as_deref() == Some(&session_id);
                    match result {
                        Ok(()) => {
                            if !as_steer {
                                self.jobs.mcp.waiting_for_prompt =
                                    self.jobs.mcp.configured && !self.jobs.mcp.initialized;
                            }
                            let messages = if is_current {
                                Some(&mut self.state.messages)
                            } else {
                                self.state
                                    .session_runtime_states
                                    .get_mut(&session_id)
                                    .map(|runtime| &mut runtime.messages)
                            };
                            if let Some(messages) = messages {
                                for msg in messages.iter_mut().filter(|m| {
                                    m.role == MessageRole::User
                                        && m.idempotency_key.as_deref()
                                            == Some(idempotency_key.as_str())
                                }) {
                                    msg.delivery = crate::prompt_queue::DeliveryState::Sent;
                                }
                            }
                            self.enqueue_workspace_sessions_refresh();
                        }
                        Err(err) => {
                            if !as_steer {
                                self.jobs.mcp.waiting_for_prompt = false;
                            }
                            let failed_text = if is_current {
                                if !as_steer {
                                    self.state.status = SessionStatus::Idle;
                                }
                                let mut failed = Vec::new();
                                for msg in self.state.messages.iter_mut().filter(|msg| {
                                    msg.role == MessageRole::User
                                        && msg.idempotency_key.as_deref()
                                            == Some(idempotency_key.as_str())
                                }) {
                                    msg.delivery = crate::prompt_queue::DeliveryState::Failed;
                                    failed.push(msg.content.clone());
                                }
                                (!failed.is_empty()).then(|| failed.join("\n\n"))
                            } else {
                                self.state
                                    .session_runtime_states
                                    .get_mut(&session_id)
                                    .and_then(|runtime| {
                                        if !as_steer {
                                            runtime.status = SessionStatus::Idle;
                                        }
                                        let mut failed = Vec::new();
                                        for msg in runtime.messages.iter_mut().filter(|msg| {
                                            msg.role == MessageRole::User
                                                && msg.idempotency_key.as_deref()
                                                    == Some(idempotency_key.as_str())
                                        }) {
                                            msg.delivery =
                                                crate::prompt_queue::DeliveryState::Failed;
                                            failed.push(msg.content.clone());
                                        }
                                        (!failed.is_empty()).then(|| failed.join("\n\n"))
                                    })
                            };
                            if let Some(text) = failed_text {
                                if is_current {
                                    if self.state.input.is_empty() {
                                        self.state.input.set_text(text);
                                    }
                                } else {
                                    let view = self
                                        .state
                                        .session_views
                                        .entry(session_id.clone())
                                        .or_default();
                                    view.draft = text;
                                    view.cursor = view.draft.len();
                                }
                            }
                            self.jobs.push_error(
                                Some(channel),
                                Some(generation),
                                format!(
                                    "{} failed: {err}",
                                    if as_steer { "Steer" } else { "Send" }
                                ),
                                true,
                                0,
                            );
                            if is_current {
                                self.system_message(format!(
                                    "{} error: {err}",
                                    if as_steer { "Steer" } else { "Send" }
                                ));
                            } else if let Some(runtime) =
                                self.state.session_runtime_states.get_mut(&session_id)
                            {
                                runtime.messages.push(DisplayMessage {
                                    role: MessageRole::System,
                                    content: format!("Error: {err}"),
                                    thinking: None,
                                    parts: Vec::new(),
                                    tool_calls: Vec::new(),
                                    delivery: crate::prompt_queue::DeliveryState::Sent,
                                    idempotency_key: None,
                                });
                            }
                        }
                    }
                }
                crate::async_jobs::JobPayload::LocalShell { command, result } => {
                    self.jobs.mark_done(channel, generation);
                    self.apply_local_shell_result(&command, result);
                }
                crate::async_jobs::JobPayload::VersionCheck { result } => match result {
                    Ok(release) => {
                        let already_shown = crate::version_check::release_is_cached(&release);
                        crate::version_check::record_latest(&release);
                        if !already_shown {
                            if let Some(hint) = crate::version_check::newer_release_hint(
                                env!("CARGO_PKG_VERSION"),
                                &release,
                            ) {
                                self.system_message(hint);
                            }
                        }
                    }
                    Err(error) => crate::version_check::record_check_error(&error),
                },
                crate::async_jobs::JobPayload::FileComplete {
                    token_start,
                    query,
                    quoted,
                    items,
                } => {
                    self.jobs.mark_done(channel, generation);
                    // Drop stale results if the cursor left this `@` token.
                    let text = self.state.input.text.clone();
                    let cursor = self.state.input.cursor.min(text.len());
                    let Some((live_start, live_query)) =
                        crate::pi::autocomplete::extract_at_token(&text, cursor)
                    else {
                        self.state.file_menu = None;
                        continue;
                    };
                    if live_start != token_start || live_query != query {
                        continue;
                    }
                    let selected = self
                        .state
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
                    self.state.file_menu = Some(FileMenuState {
                        items,
                        selected,
                        token_start,
                        query,
                        quoted,
                    });
                }
            }
        }
        received
    }

    fn apply_local_shell_result(
        &mut self,
        command: &str,
        result: Result<crate::async_jobs::LocalShellResult, String>,
    ) {
        match result {
            Ok(r) => {
                let code = r
                    .exit_code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "?".into());
                let status = if r.timed_out {
                    "timeout".into()
                } else {
                    format!("exit {code}")
                };
                let header = format!("$ {command}  · {status} · {}ms", r.duration_ms);
                let body = if r.output.trim().is_empty() {
                    header
                } else {
                    format!("{header}\n{}", r.output.trim_end())
                };
                self.state.messages.push(DisplayMessage {
                    role: MessageRole::System,
                    content: body,
                    thinking: None,
                    parts: Vec::new(),
                    tool_calls: Vec::new(),
                    delivery: crate::prompt_queue::DeliveryState::Sent,
                    idempotency_key: None,
                });
            }
            Err(err) => {
                self.system_message(format!("$ {command} failed: {err}"));
            }
        }
        self.state.follow_bottom = true;
        self.state.scroll_up = 0;
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

    /// Close the topmost transient UI (menus / pickers / search / shell).
    /// Returns true if something was dismissed. Does not touch the agent turn.
    fn dismiss_transient_ui(&mut self) -> bool {
        if self.state.quit_dialog.take().is_some() {
            self.state.quit_confirm = false;
            return true;
        }
        if self.state.plugin_prompt.take().is_some() {
            return true;
        }
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
        if self.state.mode == AppMode::Shell {
            self.state.mode = AppMode::Normal;
            return true;
        }
        false
    }

    fn has_transient_ui(&self) -> bool {
        self.state.quit_dialog.is_some()
            || self.state.plugin_prompt.is_some()
            || self.state.session_delete_confirm.is_some()
            || self.state.list_picker.is_some()
            || self.state.tasks_panel.is_some()
            || self.state.search.active
            || self.state.file_menu.is_some()
            || self.state.slash_menu.is_some()
            || self.state.mode == AppMode::Shell
    }

    fn persist_active_session_marker(&self) {
        if let Some(sid) = self.state.session_id.as_deref() {
            if let Err(error) = kkagent_config::save_active_session(sid) {
                tracing::warn!(%error, "failed to save active-session");
            }
        }
    }

    fn prompt_queue_items_json(&self) -> Vec<serde_json::Value> {
        self.state
            .prompt_queue
            .items
            .iter()
            .map(|item| {
                serde_json::json!({
                    "id": item.id,
                    "text": item.text,
                    "images": item.images.iter().map(|(media_type, data)| {
                        serde_json::json!({
                            "media_type": media_type,
                            "data": data,
                        })
                    }).collect::<Vec<_>>(),
                    "as_steer": item.as_steer,
                })
            })
            .collect()
    }

    /// Fire-and-forget sync so reconnect can restore the next-turn queue.
    fn enqueue_prompt_queue_sync(&self) {
        let Some(session_id) = self.state.session_id.clone() else {
            return;
        };
        if !self.allows_background_detach {
            return;
        }
        let items = self.prompt_queue_items_json();
        let selected = self.state.prompt_queue.selected;
        let requester = self.client.requester();
        tokio::spawn(async move {
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                requester.rpc_call(
                    "session.set_prompt_queue",
                    Some(serde_json::json!({
                        "session_id": session_id,
                        "selected": selected,
                        "items": items,
                    })),
                ),
            )
            .await;
        });
    }

    async fn sync_prompt_queue(&self) {
        let Some(session_id) = self.state.session_id.clone() else {
            return;
        };
        if !self.allows_background_detach {
            return;
        }
        let items = self.prompt_queue_items_json();
        let selected = self.state.prompt_queue.selected;
        match tokio::time::timeout(
            std::time::Duration::from_secs(2),
            self.client
                .set_prompt_queue_json(&session_id, selected, items),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!(%error, "failed to sync prompt queue"),
            Err(_) => tracing::warn!("timed out syncing prompt queue"),
        }
    }

    async fn has_server_active_turns(&self) -> bool {
        if session_status_has_active_agent_loop(self.state.status) {
            return true;
        }
        if self
            .state
            .workspace_sessions
            .entries
            .iter()
            .any(|entry| session_status_has_active_agent_loop(entry.status))
        {
            return true;
        }
        match self.client.has_active_turns().await {
            Ok(active) => active,
            Err(error) => {
                tracing::warn!(%error, "failed to query active turns");
                false
            }
        }
    }

    async fn apply_quit_dialog(&mut self, choice: usize) -> anyhow::Result<()> {
        self.state.quit_dialog = None;
        match choice {
            0 => {
                // Terminate: interrupt then exit (server stays up).
                if let Some(sid) = self.state.session_id.clone() {
                    if let Err(error) = self.client.interrupt(&sid).await {
                        self.system_message(format!("Failed to interrupt turn: {error}"));
                    } else {
                        self.state.status = SessionStatus::Cancelling;
                    }
                }
                self.sync_prompt_queue().await;
                self.state.should_quit = true;
            }
            1 => {
                // Background: keep turn, save session, exit.
                self.sync_prompt_queue().await;
                self.persist_active_session_marker();
                self.state.should_quit = true;
            }
            _ => {
                // Cancel: stay in TUI.
                self.state.quit_confirm = false;
            }
        }
        Ok(())
    }

    /// Esc: return to parent picker if any, otherwise close.
    fn pop_list_picker_level(&mut self) {
        let closing_history_edit = self
            .state
            .list_picker
            .as_ref()
            .is_some_and(|picker| picker.kind == ListPickerKind::HistoryEdit);
        self.state.session_picker_preview = None;
        self.state.session_delete_confirm = None;
        if let Some(prev) = self.state.list_picker_stack.pop() {
            self.state.list_picker = Some(prev);
        } else {
            self.state.list_picker = None;
        }
        if closing_history_edit {
            self.state.history_edit_turns.clear();
        }
    }

    fn clear_list_pickers(&mut self) {
        self.state.list_picker = None;
        self.state.list_picker_stack.clear();
        self.state.plugin_prompt = None;
        self.state.plugin_marketplace_source = None;
        self.state.plugin_selected_id = None;
        self.state.session_picker_preview = None;
        self.state.session_delete_confirm = None;
        self.state.history_edit_turns.clear();
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
        self.state.plugin_prompt = None;
        self.state.plugin_marketplace_source = None;
        self.state.plugin_selected_id = None;
        self.state.session_picker_preview = None;
        self.state.session_delete_confirm = None;
        self.state.history_edit_turns.clear();
    }

    fn flush_pending_scroll(&mut self, scroll_delta: &mut i32) {
        if *scroll_delta != 0 {
            let load_earlier = *scroll_delta > 0;
            self.state.scroll_lines(*scroll_delta);
            *scroll_delta = 0;
            // While dragging, remap focus to the same screen cell under a new scroll.
            if self.state.selection_dragging {
                self.update_selection_focus_from_last_mouse();
            }
            if load_earlier {
                self.enqueue_earlier_history_if_at_top();
            }
        }
    }

    fn enqueue_earlier_history_if_at_top(&mut self) {
        if self.state.mode == AppMode::Btw
            || self.state.history_loading
            || self.state.scroll_up != self.state.max_scroll_up()
        {
            return;
        }
        let Some(oldest) = self.state.history_oldest_index.filter(|oldest| *oldest > 0) else {
            return;
        };
        let Some(session_id) = self.state.session_id.clone() else {
            return;
        };
        // Even when the currently loaded page fits in the viewport, scrolling
        // upward expresses an intent to read older content. Keep that anchor
        // instead of snapping back to the bottom when the page arrives.
        self.state.follow_bottom = false;
        self.state.history_loading = true;
        self.jobs
            .spawn_session_history(self.client.requester(), session_id, oldest, 40);
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
        self.state.click_history.clear();
    }

    /// Classify a click as single/double/triple based on recent click history
    /// and return a selection matching the click count (word / line).
    fn classify_click(
        &mut self,
        pos: crate::selection::CellPos,
    ) -> (u8, crate::selection::TextSelection) {
        const MULTI_CLICK_MS: u64 = 500;
        let now = std::time::Instant::now();
        // Drop stale click history first.
        self.state
            .click_history
            .retain(|c| now.duration_since(c.when).as_millis() <= MULTI_CLICK_MS as u128 * 2);
        let last = self.state.click_history.last();
        let same_line = last.map(|c| c.at.line == pos.line).unwrap_or(false);
        let distance_ok = last
            .map(|c| c.at.line == pos.line && c.at.col.abs_diff(pos.col) <= 5)
            .unwrap_or(false);
        let count = match last {
            Some(c)
                if same_line
                    && distance_ok
                    && c.count == 1
                    && now.duration_since(c.when).as_millis() <= MULTI_CLICK_MS as u128 =>
            {
                2
            }
            Some(c)
                if same_line
                    && distance_ok
                    && c.count == 2
                    && now.duration_since(c.when).as_millis() <= MULTI_CLICK_MS as u128 =>
            {
                3
            }
            _ => 1,
        };
        (
            count,
            crate::selection::select_by_click(&self.state.select_rows, pos, count),
        )
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
                self.state.copy_toast = Some(CopyToast {
                    message: format!("Copied {n} chars."),
                    until: std::time::Instant::now() + std::time::Duration::from_millis(1500),
                });
                self.clear_selection();
                true
            }
            Err(e) => {
                self.state.copy_toast = Some(CopyToast {
                    message: format!("Copy failed: {e}"),
                    until: std::time::Instant::now() + std::time::Duration::from_millis(1500),
                });
                true // still consume the copy shortcut — selection was intentional
            }
        }
    }

    fn collect_mouse(
        &mut self,
        mouse: crossterm::event::MouseEvent,
        scroll_delta: &mut i32,
    ) -> bool {
        let previous_hover = self.state.strip_hover_title.clone();
        let previous_selection = self.state.selection;
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
            }
            MouseEventKind::ScrollDown if over_strip => {
                self.state.pending_strip_action = Some(StripAction::Cycle(1));
            }
            MouseEventKind::ScrollUp => {
                if self.state.mode == AppMode::Btw {
                    self.state.btw.scroll_lines(3);
                } else {
                    *scroll_delta = scroll_delta.saturating_add(3);
                }
            }
            MouseEventKind::ScrollDown => {
                if self.state.mode == AppMode::Btw {
                    self.state.btw.scroll_lines(-3);
                } else {
                    *scroll_delta = scroll_delta.saturating_sub(3);
                }
            }
            MouseEventKind::Down(MouseButton::Left) if over_strip => {
                self.flush_pending_scroll(scroll_delta);
                if let Some(hit) = self.hit_session_strip(mouse.column) {
                    self.state.pending_strip_action =
                        Some(StripAction::Switch(hit.session_id.clone()));
                }
                self.clear_selection();
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.flush_pending_scroll(scroll_delta);
                if let Some(pos) = self.mouse_to_cell(&mouse) {
                    if self.toggle_clicked_tool_output(pos.line) {
                        self.state.pending_tool_click_anchor = Some((
                            pos.line,
                            mouse.row.saturating_sub(self.state.transcript_area.y),
                        ));
                        self.clear_selection();
                        return true;
                    }
                    let (count, selection) = self.classify_click(pos);
                    self.state.selection = Some(selection);
                    self.state.selection_dragging = true;
                    self.state.click_history = vec![ClickRecord {
                        at: pos,
                        when: std::time::Instant::now(),
                        count,
                    }];
                } else {
                    // Click outside transcript clears selection.
                    self.clear_selection();
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if self.state.selection_dragging => {
                // A drag is no longer part of a multi-click sequence. Keeping
                // this record would make the next click look like a double click.
                self.state.click_history.clear();
                if let Some(pos) = self.mouse_to_cell(&mouse) {
                    if let Some(sel) = self.state.selection.as_mut() {
                        sel.focus = pos;
                    }
                }
            }
            MouseEventKind::Up(MouseButton::Left) if self.state.selection_dragging => {
                let click_count = self
                    .state
                    .click_history
                    .last()
                    .map(|click| click.count)
                    .unwrap_or(1);
                // Word/line selections already have exact boundaries. Only a
                // single-click drag should use the mouse-up cell as its focus.
                if click_count == 1 {
                    if let Some(pos) = self.mouse_to_cell(&mouse) {
                        if let Some(sel) = self.state.selection.as_mut() {
                            sel.focus = pos;
                        }
                    }
                }
                self.state.selection_dragging = false;
                // Only drop the selection if it is still empty after mouse up
                // (i.e. a plain click without drag or multi-click). Word/line
                // selections from double/triple click have non-empty ranges and
                // are kept.
                let drop = match self.state.selection {
                    Some(s) => s.is_empty(),
                    None => true,
                };
                if drop {
                    // Preserve the first click so the next one can be classified
                    // as a double click. Other clear paths intentionally reset it.
                    self.state.selection = None;
                }
            }
            _ => {}
        }

        match mouse.kind {
            MouseEventKind::Moved => previous_hover != self.state.strip_hover_title,
            MouseEventKind::Drag(MouseButton::Left) => {
                previous_hover != self.state.strip_hover_title
                    || previous_selection != self.state.selection
            }
            _ => true,
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

    fn toggle_clicked_tool_output(&mut self, line: usize) -> bool {
        let Some(hit) = self
            .state
            .tool_expand_hits
            .iter()
            .find(|hit| hit.line == line)
            .copied()
        else {
            return false;
        };
        let Some(message) = self.state.messages.get_mut(match hit.target {
            ToolExpandTarget::Part { message, .. }
            | ToolExpandTarget::Legacy { message, .. }
            | ToolExpandTarget::Plan { message } => message,
        }) else {
            return false;
        };
        match hit.target {
            ToolExpandTarget::Plan { .. } => {
                if message.role != MessageRole::Plan {
                    return false;
                }
                self.state.plan_transcript_collapsed = !self.state.plan_transcript_collapsed;
                self.state.plan_transcript_overridden = true;
            }
            ToolExpandTarget::Part { part, .. } => match message.parts.get_mut(part) {
                Some(DisplayPart::Tool(tool)) => {
                    tool.collapsed = !tool.collapsed;
                    tool.user_overridden = true;
                }
                Some(DisplayPart::ToolHistory(history)) => {
                    history.expanded = !history.expanded;
                    history.user_overridden = true;
                }
                _ => return false,
            },
            ToolExpandTarget::Legacy { tool, .. } => {
                let Some(tool) = message.tool_calls.get_mut(tool) else {
                    return false;
                };
                tool.collapsed = !tool.collapsed;
                tool.user_overridden = true;
            }
        }
        true
    }

    fn hit_session_strip(&self, column: u16) -> Option<&crate::chrome::SessionStripHit> {
        let col = column as usize;
        self.state
            .session_strip_hits
            .iter()
            .find(|h| col >= h.x0 && col < h.x1)
    }

    async fn handle_key(&mut self, key: KeyEvent) -> anyhow::Result<()> {
        if !crate::platform_keys::is_actionable_key_event(&key) {
            return Ok(());
        }
        let key = crate::platform_keys::normalize_key_event(key);

        // F5: force a full repaint in any mode. Ratatui diffs against its own
        // buffer model, so ghost cells from a torn SSH frame or an injected
        // escape byte survive normal redraws; this resyncs the terminal.
        // (Ctrl+L stays bound to the editor's input-box ToggleExpand.)
        if key.code == KeyCode::F(5) {
            self.force_full_redraw = true;
            return Ok(());
        }

        // Quit dialog (turn still running): Terminate / Background / Cancel
        if self.state.quit_dialog.is_some() {
            match key.code {
                KeyCode::Up | KeyCode::BackTab | KeyCode::Left => {
                    if let Some(dialog) = self.state.quit_dialog.as_mut() {
                        dialog.selected = dialog.selected.saturating_sub(1);
                    }
                }
                KeyCode::Down | KeyCode::Tab | KeyCode::Right => {
                    if let Some(dialog) = self.state.quit_dialog.as_mut() {
                        dialog.selected = (dialog.selected + 1).min(2);
                    }
                }
                KeyCode::Char('t') | KeyCode::Char('T') => {
                    self.apply_quit_dialog(0).await?;
                }
                KeyCode::Char('b') | KeyCode::Char('B') => {
                    self.apply_quit_dialog(1).await?;
                }
                KeyCode::Esc => {
                    self.apply_quit_dialog(2).await?;
                }
                KeyCode::Enter => {
                    let selected = self
                        .state
                        .quit_dialog
                        .as_ref()
                        .map(|dialog| dialog.selected)
                        .unwrap_or(2);
                    self.apply_quit_dialog(selected).await?;
                }
                _ => {}
            }
            return Ok(());
        }

        // Ctrl+B: background detach (standalone / --connect only)
        if key.code == KeyCode::Char('b')
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && self.allows_background_detach
        {
            if self.state.mode != AppMode::Normal {
                return Ok(());
            }
            if self.has_transient_ui() {
                let _ = self.dismiss_transient_ui();
                return Ok(());
            }
            self.sync_prompt_queue().await;
            self.persist_active_session_marker();
            self.state.should_quit = true;
            return Ok(());
        }

        // Esc while a menu/overlay is open: only dismiss that UI — never interrupt
        // an in-flight turn. Ctrl-C still cancels the turn below.
        if matches!(key.code, KeyCode::Esc) && self.dismiss_transient_ui() {
            self.state.pending_esc_ms = None;
            self.state.quit_confirm = false;
            return Ok(());
        }

        if self.state.plugin_prompt.is_some() {
            match key.code {
                KeyCode::Enter => self.submit_plugin_prompt().await?,
                KeyCode::Backspace => {
                    if let Some(prompt) = self.state.plugin_prompt.as_mut() {
                        prompt.value.pop();
                    }
                }
                KeyCode::Char(c)
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && !key.modifiers.contains(KeyModifiers::ALT) =>
                {
                    if let Some(prompt) = self.state.plugin_prompt.as_mut() {
                        prompt.value.push(c);
                    }
                }
                _ => {}
            }
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

        // A plan review is a blocking approval, but Esc should only fold its modal.
        // Keep the server waiter alive so Enter can restore the same approval.
        // Ctrl+C uses the quit-confirm / dialog path below instead of interrupting here.
        let plan_review_pending = self
            .state
            .approval_pending
            .as_ref()
            .is_some_and(|approval| approval.is_plan_review);
        if plan_review_pending && matches!(key.code, KeyCode::Esc) {
            if let Some(approval) = self.state.approval_pending.as_mut() {
                if approval.feedback_mode {
                    approval.feedback_mode = false;
                    approval.feedback.clear();
                } else {
                    approval.hidden = true;
                }
            }
            self.state.pending_esc_ms = None;
            self.state.quit_confirm = false;
            return Ok(());
        }
        if matches!(key.code, KeyCode::Enter)
            && self
                .state
                .approval_pending
                .as_ref()
                .is_some_and(|approval| approval.is_plan_review && approval.hidden)
        {
            if let Some(approval) = self.state.approval_pending.as_mut() {
                approval.hidden = false;
            }
            self.state.pending_esc_ms = None;
            self.state.quit_confirm = false;
            return Ok(());
        }

        // Busy turn with no overlay: Esc interrupts and stays in the TUI.
        // Ctrl+C is intentionally not handled here — it drives quit confirm / dialog.
        if !matches!(self.state.status, SessionStatus::Idle)
            && matches!(key.code, KeyCode::Esc)
            && !plan_review_pending
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
            return self.handle_search_key(key).await;
        }

        // Ctrl+G (BTW toggle) punches through a visible approval / plan-review
        // modal. BTW is an independent side question that does not touch the
        // pending approval on the server, so entering it while the modal is up
        // is safe — `enter_btw_view` hides the modal (stack-like), and
        // `exit_btw_view` restores it so the user can resume the approval.
        if key.code == KeyCode::Char('g')
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && self
                .state
                .approval_pending
                .as_ref()
                .is_some_and(|a| !a.hidden)
        {
            if self.state.mode == AppMode::Btw {
                self.exit_btw_view();
            } else {
                self.enter_btw_view();
            }
            return Ok(());
        }

        // Handle approval panel first (plan review / permission approval).
        // The agent loop ensures AskUserQuestion and ExitPlanMode never run in
        // the same step, so in practice only one is pending. If both somehow
        // exist (e.g. across turns), the approval modal takes precedence.
        // When BTW owns the surface `enter_btw_view` has already set
        // `approval.hidden = true`, so this branch is skipped and keystrokes
        // reach the BTW composer instead.
        if self
            .state
            .approval_pending
            .as_ref()
            .is_some_and(|approval| !approval.hidden)
        {
            let approval = self
                .state
                .approval_pending
                .as_mut()
                .expect("approval checked above");
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

        // Handle question panel (only when no visible approval modal).
        if self.state.question_pending.is_some() {
            return self.handle_question_key(key).await;
        }

        // Subagents browser overlay (`/agents`)
        if self.state.subagents_panel.is_some() {
            match key.code {
                KeyCode::Up => {
                    if let Some(ref mut p) = self.state.subagents_panel {
                        if p.detail {
                            // no-op in detail for now
                        } else if p.selected > 0 {
                            p.selected -= 1;
                        }
                    }
                    return Ok(());
                }
                KeyCode::Down => {
                    if let Some(ref mut p) = self.state.subagents_panel {
                        if !p.detail && !self.state.subagents.entries.is_empty() {
                            p.selected = (p.selected + 1) % self.state.subagents.entries.len();
                        }
                    }
                    return Ok(());
                }
                KeyCode::Enter => {
                    if let Some(ref mut p) = self.state.subagents_panel {
                        if !self.state.subagents.entries.is_empty() {
                            p.detail = !p.detail;
                        }
                    }
                    return Ok(());
                }
                KeyCode::Esc | KeyCode::Char('q') => {
                    if let Some(ref mut p) = self.state.subagents_panel {
                        if p.detail {
                            p.detail = false;
                        } else {
                            self.state.subagents_panel = None;
                        }
                    }
                    return Ok(());
                }
                _ => {}
            }
        }

        // Tasks browser overlay (`/tasks` and `/ps`)
        if self.state.tasks_panel.is_some() {
            // Detail view: scroll output, refresh, stop, or go back.
            let in_detail = self
                .state
                .tasks_panel
                .as_ref()
                .is_some_and(|p| p.detail.is_some());
            if in_detail {
                match key.code {
                    KeyCode::Up => {
                        if let Some(ref mut p) = self.state.tasks_panel {
                            if let Some(ref mut d) = p.detail {
                                d.scroll = d.scroll.saturating_sub(1);
                            }
                        }
                        return Ok(());
                    }
                    KeyCode::Down => {
                        if let Some(ref mut p) = self.state.tasks_panel {
                            if let Some(ref mut d) = p.detail {
                                d.scroll = d.scroll.saturating_add(1);
                            }
                        }
                        return Ok(());
                    }
                    KeyCode::PageUp => {
                        if let Some(ref mut p) = self.state.tasks_panel {
                            if let Some(ref mut d) = p.detail {
                                d.scroll = d.scroll.saturating_sub(10);
                            }
                        }
                        return Ok(());
                    }
                    KeyCode::PageDown => {
                        if let Some(ref mut p) = self.state.tasks_panel {
                            if let Some(ref mut d) = p.detail {
                                d.scroll = d.scroll.saturating_add(10);
                            }
                        }
                        return Ok(());
                    }
                    KeyCode::Char('g') | KeyCode::Home => {
                        if let Some(ref mut p) = self.state.tasks_panel {
                            if let Some(ref mut d) = p.detail {
                                d.scroll = 0;
                            }
                        }
                        return Ok(());
                    }
                    KeyCode::Char('G') | KeyCode::End => {
                        // Large sentinel; renderer clamps to max scroll.
                        if let Some(ref mut p) = self.state.tasks_panel {
                            if let Some(ref mut d) = p.detail {
                                d.scroll = u16::MAX;
                            }
                        }
                        return Ok(());
                    }
                    KeyCode::Char('r') | KeyCode::Char('R') => {
                        if let Some(task_id) = self
                            .state
                            .tasks_panel
                            .as_ref()
                            .and_then(|p| p.detail.as_ref().map(|d| d.task_id.clone()))
                        {
                            self.fetch_task_detail(&task_id).await;
                        }
                        return Ok(());
                    }
                    KeyCode::Enter | KeyCode::Esc | KeyCode::Backspace => {
                        if let Some(ref mut p) = self.state.tasks_panel {
                            p.detail = None;
                        }
                        return Ok(());
                    }
                    KeyCode::Char('q') => {
                        self.state.tasks_panel = None;
                        return Ok(());
                    }
                    KeyCode::Char('x') | KeyCode::Char('s') | KeyCode::Char('S') => {
                        let id = self
                            .state
                            .tasks_panel
                            .as_ref()
                            .and_then(|p| p.detail.as_ref().map(|d| d.task_id.clone()));
                        if let Some(task_id) = id {
                            self.stop_background_task(&task_id, true).await;
                        }
                        return Ok(());
                    }
                    _ => {}
                }
                return Ok(());
            }

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
                    if let Some(task_id) = self
                        .state
                        .tasks_panel
                        .as_ref()
                        .and_then(|p| p.tasks.get(p.selected))
                        .map(|t| t.task_id.clone())
                    {
                        self.fetch_task_detail(&task_id).await;
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
                        self.stop_background_task(&task_id, false).await;
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
                KeyCode::Tab
                    if self
                        .state
                        .list_picker
                        .as_ref()
                        .is_some_and(|p| p.kind == ListPickerKind::Session) =>
                {
                    self.state.session_picker_all_workspaces =
                        !self.state.session_picker_all_workspaces;
                    self.apply_session_picker_filter();
                    self.refresh_session_picker_preview();
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
                match paste_clipboard_into_workspace(&self.state.working_dir) {
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
            // Ctrl-C: empty input uses double-tap quit. With an active turn the
            // second press opens Terminate / Background / Cancel (no interrupt yet).
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if !self.state.input.is_empty() {
                    self.state.input.clear();
                    self.state.slash_menu = None;
                    self.state.file_menu = None;
                    self.state.list_picker = None;
                } else if self.state.quit_confirm {
                    if self.has_server_active_turns().await {
                        self.state.quit_dialog = Some(QuitDialogState { selected: 1 });
                        self.state.quit_confirm = false;
                    } else {
                        self.sync_prompt_queue().await;
                        self.persist_active_session_marker();
                        self.state.should_quit = true;
                    }
                } else {
                    let mut reasons = Vec::new();
                    if self.state.approval_pending.is_some()
                        || !self.state.approval_queue.is_empty()
                    {
                        reasons.push("approval");
                    }
                    if self.state.question_pending.is_some() {
                        reasons.push("question");
                    }
                    if session_status_has_active_agent_loop(self.state.status)
                        || self
                            .state
                            .workspace_sessions
                            .entries
                            .iter()
                            .any(|entry| session_status_has_active_agent_loop(entry.status))
                    {
                        reasons.push("running");
                    }
                    self.state.quit_confirm = true;
                    if !reasons.is_empty() {
                        self.system_message(format!(
                            "Quit? pending: {} — Ctrl-C again (Terminate / Background if a turn is running).",
                            reasons.join(", ")
                        ));
                    }
                }
            }
            // Ctrl-D in the virtual BTW workspace clears that workspace; the
            // fixed entry remains available for a fresh side conversation.
            KeyCode::Char('d')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && self.state.input.is_empty()
                    && self.state.mode == AppMode::Btw =>
            {
                self.delete_btw_workspace().await;
            }
            // Ctrl-D: close current multi-session tab (with confirm), else quit if empty
            KeyCode::Char('d')
                if key.modifiers.contains(KeyModifiers::CONTROL) && self.state.input.is_empty() =>
            {
                if self.can_close_current_session_tab() {
                    if session_status_has_active_agent_loop(self.state.status) {
                        if let Some(sid) = self.state.session_id.clone() {
                            match self.client.interrupt(&sid).await {
                                Ok(()) => {
                                    self.state.status = SessionStatus::Cancelling;
                                    self.state.status_bar.status = SessionStatus::Cancelling;
                                    self.state
                                        .tab_strip
                                        .set_status(&sid, SessionStatus::Cancelling);
                                    self.system_message(
                                        "Stopping this session… press Ctrl-D again after it becomes idle to close it."
                                            .into(),
                                    );
                                }
                                Err(e) => {
                                    self.system_message(format!("Failed to stop session: {e}"));
                                }
                            }
                        }
                    } else {
                        self.begin_close_current_session_confirm();
                        self.confirm_delete_session(true).await?;
                    }
                } else if self.state.quit_confirm {
                    self.sync_prompt_queue().await;
                    self.persist_active_session_marker();
                    self.state.should_quit = true;
                } else {
                    self.state.quit_confirm = true;
                }
            }
            // Escape: dismiss overlays already handled above; here interrupt /
            // double-Esc history edit.
            KeyCode::Esc => {
                if self.state.status != SessionStatus::Idle {
                    if let Some(sid) = &self.state.session_id {
                        self.state.status = SessionStatus::Cancelling;
                        self.client.interrupt(sid).await?;
                        self.system_message(
                            "Cancelling… partial output kept. After idle: edit & retry, or /fork."
                                .into(),
                        );
                    }
                    self.state.pending_esc_ms = None;
                } else {
                    // Idle: double-Esc within 600ms opens the prompt history
                    // selector. Selecting a turn forks before it, preserving
                    // the original session and workspace files.
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis())
                        .unwrap_or(0);
                    if let Some(prev) = self.state.pending_esc_ms {
                        if now.saturating_sub(prev) <= 600 {
                            self.state.pending_esc_ms = None;
                            self.open_history_edit_picker().await?;
                        } else {
                            self.state.pending_esc_ms = Some(now);
                            self.system_message(
                                "Press Esc again to browse earlier prompts and edit from one."
                                    .into(),
                            );
                        }
                    } else {
                        self.state.pending_esc_ms = Some(now);
                        self.system_message(
                            "Press Esc again to browse earlier prompts and edit from one.".into(),
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
                if self.set_plan_mode_from_ui(enabled).await {
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
            // Shift-Enter inserts a newline. Steering deliberately uses Ctrl-S
            // because many terminals cannot distinguish Shift-Enter from Enter.
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.state.input.insert_char('\n');
                self.state.refresh_slash_menu();
            }
            KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.state.input.insert_char('\n');
                self.state.refresh_slash_menu();
            }
            // Ctrl-S steers the active turn immediately.
            KeyCode::Char('s')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && self.can_steer_current_turn()
                    && (!self.state.input.is_empty() || !self.state.prompt_queue.is_empty()) =>
            {
                self.submit_steer_input().await?;
            }
            // Ctrl-F / idle Ctrl-S: open transcript search
            KeyCode::Char('f') | KeyCode::Char('s')
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.state.search.open();
                self.state.slash_menu = None;
                self.state.file_menu = None;
                self.state.list_picker = None;
            }
            // Ctrl-G is the sole show/hide control for the BTW surface. BTW is
            // advertised beside the git badge, not as a session-strip entry.
            KeyCode::Char('g') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.state.mode == AppMode::Btw {
                    self.exit_btw_view();
                } else {
                    self.enter_btw_view();
                }
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
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && (self.state.todos.len() > 5 || all_todos_finished(&self.state.todos)) =>
            {
                self.state.todos_expanded = !self.state.todos_expanded;
            }
            KeyCode::Char('t') | KeyCode::Char('T')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && key.modifiers.contains(KeyModifiers::SHIFT) =>
            {
                self.navigate_turns(-1, false);
                let stale = self
                    .state
                    .todos
                    .iter()
                    .filter(|t| t.status == "blocked" || t.status == "pending")
                    .count();
                self.system_message(format!(
                    "Todo jump · {} pending/blocked kept after session turns",
                    stale
                ));
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
            // Empty-input turn navigation: [ ] user turns, { } tool errors
            KeyCode::Char('[')
                if self.state.input.is_empty()
                    && !key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.navigate_turns(-1, false);
            }
            KeyCode::Char(']')
                if self.state.input.is_empty()
                    && !key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.navigate_turns(1, false);
            }
            KeyCode::Char('{') if self.state.input.is_empty() => {
                self.navigate_turns(-1, true);
            }
            KeyCode::Char('}') if self.state.input.is_empty() => {
                self.navigate_turns(1, true);
            }
            // Ctrl-X: cancel the latest running tool (stopping…) without full turn interrupt when possible
            KeyCode::Char('x') | KeyCode::Char('X')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(
                        self.state.status,
                        SessionStatus::ToolExecuting | SessionStatus::Thinking
                    ) =>
            {
                if let Some(id) = self
                    .state
                    .messages
                    .iter()
                    .rev()
                    .flat_map(|m| m.parts.iter().rev())
                    .find_map(|p| match p {
                        DisplayPart::Tool(tc) if tc.output.is_none() && !tc.stopping => {
                            Some(tc.id.clone())
                        }
                        _ => None,
                    })
                {
                    self.cancel_running_tool(&id).await?;
                }
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
                    if let EditorAction::Insert(ch) = map_key(key) {
                        self.state.input.insert_char(ch);
                        self.state.refresh_slash_menu();
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
            KeyCode::PageUp => {
                if self.state.mode == AppMode::Btw {
                    self.state.btw.scroll_lines(10);
                } else {
                    self.state.scroll_lines(10);
                    self.enqueue_earlier_history_if_at_top();
                }
            }
            KeyCode::PageDown => {
                if self.state.mode == AppMode::Btw {
                    self.state.btw.scroll_lines(-10);
                } else {
                    self.state.scroll_lines(-10);
                }
            }
            KeyCode::Home if self.state.input.is_empty() => {
                if self.state.mode == AppMode::Btw {
                    self.state.btw.scroll_offset = self.state.btw.max_scroll_offset();
                } else {
                    self.state.scroll_up = self.state.max_scroll_up();
                    self.state.follow_bottom = false;
                    self.enqueue_earlier_history_if_at_top();
                }
            }
            KeyCode::End if self.state.input.is_empty() => {
                if self.state.mode == AppMode::Btw {
                    self.state.btw.scroll_offset = 0;
                } else {
                    self.state.scroll_up = 0;
                    self.state.follow_bottom = true;
                }
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
        let opens_immediately = slash_command_opens_immediately(&item.name);

        if !submit_if_ready || (needs_args && !opens_immediately) {
            self.state.input.set_text(format!("/{} ", item.name));
            self.state.slash_menu = None;
            self.state.refresh_slash_menu();
            return Ok(());
        }

        // Enter accepted this suggestion as a command. Consume the composer
        // before any async work so partial text such as `/sess` cannot remain
        // visible or come back from a persisted draft while the picker opens.
        if let Some(session_id) = self.state.session_id.clone() {
            self.clear_session_composer_draft(&session_id);
        } else {
            self.state.input.clear();
            self.state.history_index = None;
            self.state.history_draft.clear();
        }
        self.state.slash_menu = None;

        if opens_immediately {
            self.state.list_picker_stack.clear();
            self.state.session_picker_preview = None;
            self.state.session_delete_confirm = None;
            match item.name.as_str() {
                "model" => self.open_model_picker(),
                "sessions" | "resume" => self.open_session_picker().await?,
                "tasks" | "task" | "ps" => self.open_tasks_panel().await?,
                "agents" | "agent" => self.open_agents_panel(),
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
            self.handle_slash_command(&format!("/{}", item.name))
                .await?;
        }
        Ok(())
    }

    async fn open_tasks_panel(&mut self) -> anyhow::Result<()> {
        let Some(sid) = self.state.session_id.clone() else {
            self.system_message("No active session.".into());
            return Ok(());
        };
        match self
            .client
            .rpc_call("ps.list", Some(serde_json::json!({ "session_id": sid })))
            .await
        {
            Ok(data) => {
                let mut tasks = Vec::new();
                if let Some(arr) = data.get("processes").and_then(|v| v.as_array()) {
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
                            command: t
                                .get("command")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                            elapsed_secs: t
                                .get("elapsed_secs")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0),
                        });
                    }
                }
                let prev = self.state.tasks_panel.take();
                let selected = prev
                    .as_ref()
                    .map(|p| p.selected.min(tasks.len().saturating_sub(1)))
                    .unwrap_or(0);
                // Keep the detail view only if that job is still in the list.
                let detail = prev.and_then(|p| {
                    let d = p.detail?;
                    if tasks.iter().any(|t| t.task_id == d.task_id) {
                        Some(d)
                    } else {
                        None
                    }
                });
                self.state.tasks_panel = Some(TasksPanelState {
                    tasks,
                    selected,
                    detail,
                });
            }
            Err(e) => self.system_message(format!("Failed to list tasks: {}", e)),
        }
        Ok(())
    }

    /// Stop a background shell task; refresh list (and detail view when requested).
    async fn stop_background_task(&mut self, task_id: &str, refresh_detail: bool) {
        match self
            .client
            .rpc_call("ps.stop", Some(serde_json::json!({ "task_id": task_id })))
            .await
        {
            Ok(_) => self.system_message(format!("Stopped task {}", task_id)),
            Err(e) => self.system_message(format!("Stop failed: {}", e)),
        }
        if refresh_detail {
            self.fetch_task_detail(task_id).await;
        }
    }

    async fn fetch_task_detail(&mut self, task_id: &str) {
        match self
            .client
            .rpc_call("ps.output", Some(serde_json::json!({ "task_id": task_id })))
            .await
        {
            Ok(data) => {
                let detail = TaskDetailState {
                    task_id: data
                        .get("task_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or(task_id)
                        .to_string(),
                    status: data
                        .get("status")
                        .and_then(|v| v.as_str())
                        .unwrap_or("?")
                        .to_string(),
                    running: data
                        .get("running")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                    exit_code: data.get("exit_code").and_then(|v| v.as_i64()),
                    description: data
                        .get("description")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    command: data
                        .get("command")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    elapsed_secs: data
                        .get("elapsed_secs")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0),
                    output: data
                        .get("output")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    // Preserve scroll position on refresh; renderer clamps.
                    scroll: self
                        .state
                        .tasks_panel
                        .as_ref()
                        .and_then(|p| p.detail.as_ref())
                        .filter(|d| d.task_id == task_id)
                        .map(|d| d.scroll)
                        .unwrap_or(0),
                };
                if let Some(ref mut p) = self.state.tasks_panel {
                    p.detail = Some(detail);
                }
            }
            Err(e) => self.system_message(format!("Failed to read task output: {}", e)),
        }
    }

    fn open_agents_panel(&mut self) {
        let selected = self
            .state
            .subagents_panel
            .as_ref()
            .map(|p| {
                p.selected
                    .min(self.state.subagents.entries.len().saturating_sub(1))
            })
            .unwrap_or(0);
        self.state.subagents_panel = Some(crate::subagents::SubagentsPanelState {
            selected,
            detail: false,
        });
        if self.state.subagents.entries.is_empty() {
            self.system_message(
                "No subagents in this session yet. Spawned agents appear here live.".into(),
            );
        }
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
        // Preview: list recent Write/Edit tools that may be restored.
        let mut preview = Vec::new();
        for msg in self.state.messages.iter().rev() {
            for part in &msg.parts {
                if let DisplayPart::Tool(tc) = part {
                    if matches!(tc.name.as_str(), "Write" | "Edit") {
                        preview.push(format!("{} {}", tc.name, tc.input_summary));
                        if preview.len() >= 5 {
                            break;
                        }
                    }
                }
            }
            if preview.len() >= 5 {
                break;
            }
        }
        if !preview.is_empty() {
            self.system_message(format!(
                "Undo preview (files may restore; external shell/network not undone): {}",
                preview.join(" · ")
            ));
        }
        let params = serde_json::json!({"session_id": sid, "count": count});
        match self.client.rpc_call("session.undo", Some(params)).await {
            Ok(data) => {
                let undone = data.get("undone").and_then(|v| v.as_u64()).unwrap_or(0);
                if let Some(msgs) = data.get("messages").and_then(|v| v.as_array()) {
                    self.state.messages = transcript_messages_to_display(msgs);
                    self.state.apply_tool_output_mode();
                    self.state.active_assistant_message = None;
                }
                self.state.thinking_text.clear();
                self.state.follow_bottom = true;
                self.state.scroll_up = 0;
                self.system_message(format!(
                    "Undid {} turn(s). File changes restored where possible (redo not available; fork to keep branch).",
                    undone
                ));
            }
            Err(e) => self.system_message(format!("Undo failed: {}", e)),
        }
        Ok(())
    }

    async fn open_history_edit_picker(&mut self) -> anyhow::Result<()> {
        if self.state.status != SessionStatus::Idle {
            self.system_message("History editing is available after the turn becomes idle.".into());
            return Ok(());
        }
        let Some(session_id) = self.state.session_id.clone() else {
            self.system_message("No active session.".into());
            return Ok(());
        };
        let data = match self
            .client
            .rpc_call(
                "session.turns",
                Some(serde_json::json!({"session_id": session_id})),
            )
            .await
        {
            Ok(data) => data,
            Err(error) => {
                self.system_message(format!("Failed to load conversation history: {error}"));
                return Ok(());
            }
        };
        let turns = history_edit_turns_from_json(&data);
        if turns.is_empty() {
            self.system_message("No prior user prompts to edit.".into());
            return Ok(());
        }
        let items = turns
            .iter()
            .map(|turn| ListPickerItem {
                id: turn.message_index.to_string(),
                label: format!(
                    "#{}  {}",
                    turn.turn_index + 1,
                    history_turn_summary(&turn.text, 72)
                ),
                detail: String::new(),
            })
            .collect::<Vec<_>>();
        let selected = items.len().saturating_sub(1);
        self.begin_root_picker();
        self.state.history_edit_turns = turns;
        self.replace_list_picker(ListPickerState {
            kind: ListPickerKind::HistoryEdit,
            title: " Edit history · ↑↓ choose prompt · Enter fork & edit · Esc cancel ".into(),
            items: items.clone(),
            selected,
            filter: String::new(),
            all_items: items,
        });
        Ok(())
    }

    async fn fork_and_edit_turn(&mut self, turn: HistoryEditTurn) -> anyhow::Result<()> {
        let source_id = self
            .state
            .session_id
            .clone()
            .ok_or_else(|| anyhow::anyhow!("No active session"))?;
        let source_title = self
            .state
            .workspace_sessions
            .entries
            .iter()
            .find(|entry| entry.id == source_id)
            .map(|entry| entry.title.as_str())
            .or_else(|| {
                self.state
                    .tab_strip
                    .tabs
                    .iter()
                    .find(|tab| tab.id == source_id)
                    .map(|tab| tab.title.as_str())
            })
            .filter(|title| !title.trim().is_empty())
            .unwrap_or("session");
        let title = format!("Undo: {source_title}");
        let data = self
            .client
            .rpc_call(
                "sessions.fork",
                Some(serde_json::json!({
                    "session_id": source_id,
                    "title": title,
                    "message_limit": turn.message_index,
                })),
            )
            .await?;
        let target_id = data
            .get("session_id")
            .and_then(|value| value.as_str())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("fork response did not include a session id"))?
            .to_string();

        self.clear_list_pickers();
        self.link_open_sessions(&source_id, &target_id);
        self.state
            .tab_strip
            .ensure_tab(&target_id, format!("edit #{}", turn.turn_index + 1));
        self.state.pending_resume_prefill = Some((target_id.clone(), turn.text));
        self.resume_session(&target_id).await
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
                self.apply_model_selection(item.id).await;
            }
            ListPickerKind::FallbackDecision => match item.id.as_str() {
                "disabled" => {
                    self.apply_fallback_selection("disabled", None).await;
                    self.clear_list_pickers();
                }
                "choose" => {
                    self.state.list_picker_stack.push(picker);
                    self.open_fallback_model_picker();
                }
                _ => self.clear_list_pickers(),
            },
            ListPickerKind::FallbackModel => {
                self.apply_fallback_selection("model", Some(&item.id)).await;
                self.clear_list_pickers();
            }
            ListPickerKind::Session => {
                let workspace = self
                    .state
                    .session_picker_entries
                    .iter()
                    .find(|entry| entry.item.id == item.id)
                    .map(|entry| entry.workspace.clone());
                self.clear_list_pickers();
                if self.state.session_id.as_deref() == Some(item.id.as_str()) {
                    self.clear_session_composer_draft(&item.id);
                } else {
                    self.resume_session_in_workspace(&item.id, workspace.as_deref())
                        .await?;
                }
            }
            ListPickerKind::Permission => {
                self.apply_permission_mode_id(&item.id).await?;
                self.clear_list_pickers();
            }
            ListPickerKind::Config => match item.id.as_str() {
                "reload" => {
                    self.reload_config_from_disk().await;
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
            ListPickerKind::Usage => {
                if item.id == "__turns__" {
                    self.state.list_picker_stack.push(picker);
                    self.open_usage_turns_picker();
                } else if let Some(prev) = self.state.list_picker_stack.pop() {
                    self.state.list_picker = Some(prev);
                }
            }
            ListPickerKind::UsageTurns => {
                // Enter = same as Esc: back to the /usage panel.
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
                        | "ps"
                        | "agents"
                        | "agent"
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
                    self.reload_config_from_disk().await;
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
                "tasks" | "ps" => {
                    self.state.list_picker_stack.push(picker);
                    self.open_tasks_panel().await?;
                }
                _ => {}
            },
            ListPickerKind::HistoryEdit => {
                let message_index = item.id.parse::<usize>().ok();
                let turn = message_index.and_then(|message_index| {
                    self.state
                        .history_edit_turns
                        .iter()
                        .find(|turn| turn.message_index == message_index)
                        .cloned()
                });
                let Some(turn) = turn else {
                    self.state.list_picker = Some(picker);
                    self.system_message("The selected history turn is no longer available.".into());
                    return Ok(());
                };
                if let Err(error) = self.fork_and_edit_turn(turn).await {
                    self.state.list_picker = Some(picker);
                    self.system_message(format!("Failed to fork conversation history: {error}"));
                }
            }
            ListPickerKind::PluginHome => {
                self.state.list_picker_stack.push(picker);
                let result = match item.id.as_str() {
                    "installed" => self.open_installed_plugins_picker().await,
                    "marketplaces" => self.open_plugin_marketplaces_picker().await,
                    "add_marketplace" => {
                        self.pop_list_picker_level();
                        self.open_plugin_prompt(PluginPromptKind::AddMarketplace);
                        Ok(())
                    }
                    "install_source" => {
                        self.pop_list_picker_level();
                        self.open_plugin_prompt(PluginPromptKind::InstallSource);
                        Ok(())
                    }
                    "reload" => {
                        self.pop_list_picker_level();
                        self.reload_plugins_from_picker().await;
                        Ok(())
                    }
                    _ => {
                        self.pop_list_picker_level();
                        Ok(())
                    }
                };
                if let Err(error) = result {
                    self.pop_list_picker_level();
                    self.system_message(format!("Failed to open plugin manager: {error}"));
                }
            }
            ListPickerKind::PluginInstalled => {
                self.state.plugin_selected_id = Some(item.id.clone());
                self.state.list_picker_stack.push(picker);
                if let Err(error) = self.open_installed_plugin_detail(&item.id).await {
                    self.pop_list_picker_level();
                    self.system_message(format!("Failed to load plugin details: {error}"));
                }
            }
            ListPickerKind::PluginInstalledDetail => {
                self.apply_installed_plugin_action(picker, &item.id).await?;
            }
            ListPickerKind::PluginMarketplaces => {
                if item.id == "__add__" {
                    self.state.list_picker = Some(picker);
                    self.open_plugin_prompt(PluginPromptKind::AddMarketplace);
                } else {
                    let source = serde_json::from_str::<serde_json::Value>(&item.id)
                        .ok()
                        .and_then(|descriptor| {
                            descriptor
                                .get("source")
                                .and_then(|value| value.as_str())
                                .map(str::to_string)
                        });
                    if let Some(source) = source {
                        self.state.plugin_marketplace_source = Some(source.clone());
                        self.state.list_picker_stack.push(picker);
                        if let Err(error) = self.open_marketplace_entries_picker(&source).await {
                            self.pop_list_picker_level();
                            self.system_message(format!(
                                "Failed to load plugin marketplace: {error}"
                            ));
                        }
                    } else {
                        self.state.list_picker = Some(picker);
                        self.system_message("Plugin marketplace source is missing.".into());
                    }
                }
            }
            ListPickerKind::PluginMarketplaceEntries => {
                self.state.plugin_selected_id = Some(item.id.clone());
                self.state.list_picker_stack.push(picker);
                if let Err(error) = self.open_marketplace_plugin_detail(&item.id).await {
                    self.pop_list_picker_level();
                    self.system_message(format!("Failed to load marketplace plugin: {error}"));
                }
            }
            ListPickerKind::PluginMarketplaceDetail => {
                self.apply_marketplace_plugin_action(picker, &item.id)
                    .await?;
            }
            ListPickerKind::PluginConfirm => {
                self.apply_plugin_confirmation(picker, &item.id).await?;
            }
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

    /// Local precheck before switching models — surfaces missing capabilities early.
    fn model_capability_precheck(&self, alias: &str) -> Option<String> {
        let Some((model, _)) = self.config.resolve_model(alias) else {
            return Some(format!("Unknown model alias: {alias}"));
        };
        let caps: std::collections::HashSet<String> = model
            .capabilities
            .iter()
            .map(|c| c.to_lowercase())
            .collect();
        let has = |keys: &[&str]| keys.iter().any(|k| caps.contains(*k));
        let vision = has(&["vision", "image", "image_in", "multimodal"]);
        let tools = caps.is_empty()
            || has(&["tools", "tool_use", "function_calling"])
            || !has(&["no_tools"]);
        let mut issues = Vec::new();
        let has_images = self
            .state
            .messages
            .iter()
            .any(|m| m.content.contains("[image]") || m.content.contains("data:image"));
        if has_images && !vision {
            issues.push("session has images but model lacks vision/image_in".into());
        }
        if !tools {
            issues.push("model disables tool use — agent tools may fail".into());
        }
        if let Some(ctx) = model.max_context_size {
            if self.state.approx_tokens + 2048 > ctx {
                issues.push(format!(
                    "approx tokens {} may exceed context window {ctx}",
                    self.state.approx_tokens
                ));
            }
        }
        if issues.is_empty() {
            None
        } else {
            Some(format!("Model precheck ({alias}): {}", issues.join("; ")))
        }
    }

    fn navigate_turns(&mut self, direction: i32, errors_only: bool) {
        let mut indices: Vec<usize> = Vec::new();
        for (i, msg) in self.state.messages.iter().enumerate() {
            if errors_only {
                let has_err = msg
                    .parts
                    .iter()
                    .any(|p| matches!(p, DisplayPart::Tool(tc) if tc.is_error));
                if has_err {
                    indices.push(i);
                }
            } else if msg.role == MessageRole::User {
                indices.push(i);
            }
        }
        if indices.is_empty() {
            self.system_message(if errors_only {
                "No tool errors to jump to".into()
            } else {
                "No user turns to jump to".into()
            });
            return;
        }
        let current = self.state.highlight_message.unwrap_or(0);
        let pos = indices.iter().position(|&i| i >= current).unwrap_or(0);
        let next = if direction >= 0 {
            indices[(pos + 1).min(indices.len() - 1)]
        } else if pos == 0 {
            indices[0]
        } else {
            indices[pos.saturating_sub(1)]
        };
        self.state.highlight_message = Some(next);
        self.state.follow_bottom = false;
        if let Some(starts) = self.state.message_line_starts.get(next) {
            self.state.scroll_up = self
                .state
                .content_lines
                .saturating_sub(self.state.viewport_height)
                .saturating_sub(*starts);
        }
    }

    async fn cancel_running_tool(&mut self, tool_call_id: &str) -> anyhow::Result<()> {
        // Mark UI stopping… immediately; server confirms via ToolCancelled / ToolResult.
        for msg in &mut self.state.messages {
            for part in &mut msg.parts {
                if let DisplayPart::Tool(tc) = part {
                    if tc.id == tool_call_id {
                        tc.stopping = true;
                    }
                }
            }
        }
        let Some(sid) = self.state.session_id.clone() else {
            return Ok(());
        };
        let params = serde_json::json!({
            "session_id": sid,
            "tool_call_id": tool_call_id,
        });
        match self
            .client
            .rpc_call("session.cancel_tool", Some(params))
            .await
        {
            Ok(_) => self.system_message(format!("stopping… {tool_call_id}")),
            Err(e) => {
                // Fallback: interrupt whole turn if per-tool cancel unsupported.
                self.system_message(format!(
                    "Per-tool cancel unavailable ({e}); use Esc to interrupt turn"
                ));
            }
        }
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

    async fn apply_model_selection(&mut self, model: String) {
        // Bind model to this session only; keep config default for /new.
        if let Some(warn) = self.model_capability_precheck(&model) {
            self.system_message(warn);
        }
        let session_id = self.state.session_id.clone();
        if let Some(session_id) = session_id.as_deref() {
            if let Err(error) = self.client.set_model(session_id, &model).await {
                self.system_message(format!("Failed to set model on server: {error}"));
                self.clear_list_pickers();
                return;
            }
        }
        self.state.model_alias = Some(model.clone());
        self.clear_list_pickers();

        if model_matches_global_fallback(&self.config, &model) {
            self.open_fallback_decision_picker();
            self.system_message(format!(
                "Model set to: {model}. It matches global fallback_model; choose this session's fallback policy."
            ));
        } else {
            if let Some(session_id) = session_id.as_deref() {
                if let Err(error) = self
                    .client
                    .set_fallback_model(session_id, "inherit", None)
                    .await
                {
                    self.system_message(format!(
                        "Model set, but failed to restore global fallback: {error}"
                    ));
                    return;
                }
            }
            let suffix = self
                .config
                .fallback_model
                .as_deref()
                .map(|fallback| format!(" · fallback: {fallback}"))
                .unwrap_or_default();
            self.system_message(format!("Model set to: {model}{suffix}"));
        }
    }

    async fn apply_fallback_selection(&mut self, mode: &str, model: Option<&str>) {
        let Some(session_id) = self.state.session_id.clone() else {
            self.system_message("No active session for fallback selection".into());
            return;
        };
        match self
            .client
            .set_fallback_model(&session_id, mode, model)
            .await
        {
            Ok(()) if mode == "disabled" => {
                self.system_message("Fallback disabled for this session".into());
            }
            Ok(()) => {
                self.system_message(format!(
                    "Session fallback model set to: {}",
                    model.unwrap_or("global config")
                ));
            }
            Err(error) => {
                self.system_message(format!("Failed to set session fallback: {error}"));
            }
        }
    }

    fn open_fallback_decision_picker(&mut self) {
        let alternatives = self.fallback_model_items();
        let mut items = vec![ListPickerItem {
            id: "disabled".into(),
            label: "Disable fallback".into(),
            detail: "Use the selected model only for this session".into(),
        }];
        if !alternatives.is_empty() {
            items.push(ListPickerItem {
                id: "choose".into(),
                label: "Enable with another model".into(),
                detail: "Choose a different fallback for this session".into(),
            });
        }
        self.replace_list_picker(ListPickerState {
            kind: ListPickerKind::FallbackDecision,
            title: " Primary equals global fallback ".into(),
            items,
            selected: 0,
            filter: String::new(),
            all_items: Vec::new(),
        });
    }

    fn open_fallback_model_picker(&mut self) {
        self.replace_list_picker(ListPickerState {
            kind: ListPickerKind::FallbackModel,
            title: " Select session fallback model ".into(),
            items: self.fallback_model_items(),
            selected: 0,
            filter: String::new(),
            all_items: Vec::new(),
        });
    }

    fn fallback_model_items(&self) -> Vec<ListPickerItem> {
        let primary = self.state.model_alias.as_deref();
        let mut names = self
            .config
            .models
            .keys()
            .filter(|name| Some(name.as_str()) != primary)
            .cloned()
            .collect::<Vec<_>>();
        names.sort();
        names
            .into_iter()
            .map(|name| {
                let detail = self
                    .config
                    .resolve_model(&name)
                    .map(|(model, _)| format!("{} · {}", model.provider, model.model))
                    .unwrap_or_default();
                ListPickerItem {
                    id: name.clone(),
                    label: name,
                    detail,
                }
            })
            .collect()
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

    fn open_context_picker(&mut self) {
        let mut items = Vec::new();
        for src in crate::instruction_scan::scan_project_instructions(&self.state.working_dir) {
            let status = if !src.readable {
                "unreadable"
            } else if src.effective {
                "effective"
            } else {
                src.note.as_deref().unwrap_or("shadowed")
            };
            items.push(ListPickerItem {
                id: src.path.display().to_string(),
                label: src.path.display().to_string(),
                detail: format!("{} · {status}", src.kind),
            });
        }
        if items.is_empty() {
            items.push(ListPickerItem {
                id: "none".into(),
                label: "No project instruction files".into(),
                detail: String::new(),
            });
        }
        items.push(ListPickerItem {
            id: "skills".into(),
            label: "Skills (enabled only)".into(),
            detail: format!(
                "{} slash skills · disabled not injected into /context",
                self.state.skill_slash_commands.len()
            ),
        });
        // Token breakdown (estimated from local transcript display).
        let sys_est = 800u64; // system prompt not fully mirrored in TUI
        let mut conv = 0u64;
        let mut tools = 0u64;
        let media = 0u64;
        for msg in &self.state.messages {
            for part in &msg.parts {
                match part {
                    DisplayPart::Text(t) => {
                        conv += (t.chars().count() as u64).div_ceil(4);
                    }
                    DisplayPart::Tool(tc) => {
                        let n = tc.input_summary.len()
                            + tc.output.as_ref().map(|s| s.len()).unwrap_or(0);
                        tools += (n as u64).div_ceil(4);
                    }
                    DisplayPart::ToolHistory(h) => {
                        tools += 40 * h.tool_count as u64;
                    }
                    DisplayPart::SkillActivation { name, args } => {
                        conv += ((name.len() + args.as_ref().map(|a| a.len()).unwrap_or(0)) as u64)
                            .div_ceil(4);
                    }
                }
            }
        }
        let reserved = self
            .config
            .loop_control
            .as_ref()
            .map(|l| l.reserved_context_size)
            .unwrap_or(8_192);
        let used = sys_est + conv + tools + media;
        let max_ctx = self
            .state
            .model_alias
            .as_deref()
            .and_then(|a| self.config.models.get(a))
            .and_then(|m| m.max_context_size)
            .unwrap_or(200_000);
        let remain = max_ctx as i64 - used as i64 - reserved as i64;
        items.push(ListPickerItem {
            id: "bd-system".into(),
            label: "system (est.)".into(),
            detail: format!("~{sys_est} tok"),
        });
        items.push(ListPickerItem {
            id: "bd-conv".into(),
            label: "conversation (est.)".into(),
            detail: format!("~{conv} tok"),
        });
        items.push(ListPickerItem {
            id: "bd-tools".into(),
            label: "tools (est.)".into(),
            detail: format!("~{tools} tok"),
        });
        items.push(ListPickerItem {
            id: "bd-media".into(),
            label: "media / attachments (est.)".into(),
            detail: format!("~{media} tok"),
        });
        items.push(ListPickerItem {
            id: "bd-reserved".into(),
            label: "reserved output".into(),
            detail: format!("{reserved} tok"),
        });
        items.push(ListPickerItem {
            id: "bd-remain".into(),
            label: "remaining (est.)".into(),
            detail: format!("{remain} · window {max_ctx}"),
        });
        items.push(ListPickerItem {
            id: "tokens".into(),
            label: "Server approx tokens".into(),
            detail: self.state.approx_tokens.to_string(),
        });
        items.push(ListPickerItem {
            id: "tools".into(),
            label: "Last tool".into(),
            detail: self
                .state
                .last_tool_name
                .clone()
                .unwrap_or_else(|| "-".into()),
        });
        self.replace_list_picker(ListPickerState {
            kind: ListPickerKind::Browse,
            title: " /context ".into(),
            items,
            selected: 0,
            filter: String::new(),
            all_items: Vec::new(),
        });
    }

    fn open_changes_picker(&mut self) {
        let mut items = Vec::new();
        for msg in &self.state.messages {
            for part in &msg.parts {
                if let DisplayPart::Tool(tc) = part {
                    if matches!(tc.name.as_str(), "Write" | "Edit") {
                        let path = tc.input_summary.chars().take(48).collect::<String>();
                        let link = if path.contains('/') || path.ends_with(".rs") {
                            crate::test_summary::osc8_link(&format!("file://{path}"), &path)
                        } else {
                            path.clone()
                        };
                        items.push(ListPickerItem {
                            id: tc.id.clone(),
                            label: format!("{} · {}", tc.name, &tc.id[..8.min(tc.id.len())]),
                            detail: format!("{link} · agent"),
                        });
                    }
                }
            }
        }
        if items.is_empty() {
            items.push(ListPickerItem {
                id: "none".into(),
                label: "No file edits in this session".into(),
                detail: "shared / unknown external edits are not claimed".into(),
            });
        } else {
            items.insert(
                0,
                ListPickerItem {
                    id: "note".into(),
                    label: "Attribution".into(),
                    detail: "listed by tool_call_id · concurrent IDE edits = shared/unknown".into(),
                },
            );
        }
        self.replace_list_picker(ListPickerState {
            kind: ListPickerKind::Browse,
            title: " /changes (session tool edits) ".into(),
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
        let hit = kkagent_protocol::cache_hit_ratio_ex(
            u.input_tokens,
            u.cache_creation_tokens,
            u.cache_read_tokens,
            u.input_includes_cache,
        );
        let mut items = vec![
            ListPickerItem {
                id: "model".into(),
                label: "Model".into(),
                detail: model,
            },
            ListPickerItem {
                id: "sep_totals".into(),
                label: "── Session Totals ──".into(),
                detail: String::new(),
            },
            ListPickerItem {
                id: "input".into(),
                label: "Input tokens".into(),
                detail: fmt_thousands(u.input_tokens),
            },
            ListPickerItem {
                id: "output".into(),
                label: "Output tokens".into(),
                detail: fmt_thousands(u.output_tokens),
            },
            ListPickerItem {
                id: "total".into(),
                label: "Total tokens".into(),
                detail: fmt_thousands(effective_total_input(u).saturating_add(u.output_tokens)),
            },
            ListPickerItem {
                id: "cache_c".into(),
                label: "Cache creation".into(),
                detail: fmt_thousands(u.cache_creation_tokens),
            },
            ListPickerItem {
                id: "cache_r".into(),
                label: "Cache read".into(),
                detail: fmt_thousands(u.cache_read_tokens),
            },
            ListPickerItem {
                id: "hit".into(),
                label: "Cache hit ratio".into(),
                detail: hit
                    .map(|h| format!("{:.1}%", h * 100.0))
                    .unwrap_or_else(|| "n/a".into()),
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
            ListPickerItem {
                id: "sep_latest".into(),
                label: "── Latest Request ──".into(),
                detail: String::new(),
            },
            ListPickerItem {
                id: "latest_ctx".into(),
                label: "Context size".into(),
                detail: self
                    .state
                    .last_step_usage
                    .as_ref()
                    .map(|s| fmt_thousands(s.context_size()))
                    .unwrap_or_else(|| "n/a".into()),
            },
            ListPickerItem {
                id: "latest_hit".into(),
                label: "Cache hit ratio".into(),
                detail: self
                    .state
                    .last_step_usage
                    .as_ref()
                    .and_then(|s| s.cache_hit_ratio())
                    .map(|h| format!("{:.1}%", h * 100.0))
                    .unwrap_or_else(|| "n/a".into()),
            },
            ListPickerItem {
                id: "steps".into(),
                label: "Steps".into(),
                detail: u.steps.to_string(),
            },
            ListPickerItem {
                id: "turns".into(),
                label: "Turns".into(),
                detail: u.turns.to_string(),
            },
        ];
        // Hide the cache-write row when the provider did not report writes.
        if !cache_creation_is_real_semantics(u) {
            items.retain(|item| item.id != "cache_c");
        }

        // Context breakdown with progress bars (server-authoritative when
        // available; falls back to a local estimate otherwise).
        let max_ctx = self
            .state
            .model_alias
            .as_deref()
            .or_else(|| self.config.default_model_alias())
            .and_then(|a| self.config.resolve_model(a))
            .and_then(|(m, _)| m.max_context_size)
            .unwrap_or(200_000);
        let (ctx_system, ctx_conv, ctx_tools, ctx_media, ctx_reserved, ctx_est) =
            if let Some(c) = self.state.context_breakdown.as_ref() {
                (
                    c.system,
                    c.conversation,
                    c.tools,
                    c.media,
                    c.reserved_output,
                    c.estimated,
                )
            } else {
                (0, 0, 0, 0, 8_192, true)
            };
        let ctx_used = ctx_system
            .saturating_add(ctx_conv)
            .saturating_add(ctx_tools)
            .saturating_add(ctx_media);
        let free = max_ctx.saturating_sub(ctx_used.saturating_add(ctx_reserved));
        items.push(ListPickerItem {
            id: "sep_ctx".into(),
            label: format!(
                "── Context Breakdown ({} / {}) ──",
                fmt_thousands(ctx_used),
                fmt_thousands(max_ctx)
            ),
            detail: String::new(),
        });
        let rows = [
            ("system", "System prompt", ctx_system),
            ("conversation", "Conversation", ctx_conv),
            ("tools", "Tools", ctx_tools),
            ("media", "Media", ctx_media),
            ("reserved", "Reserved output", ctx_reserved),
            ("free", "Free", free),
        ];
        for (id, name, tokens) in rows {
            let pct = if max_ctx == 0 {
                0.0
            } else {
                tokens as f64 / max_ctx as f64 * 100.0
            };
            items.push(ListPickerItem {
                id: format!("ctx-{id}"),
                label: name.into(),
                detail: format!(
                    "{} {:>10} ({:4.1}%){}",
                    progress_bar(tokens, max_ctx, 24),
                    fmt_thousands(tokens),
                    pct,
                    if ctx_est { " ≈" } else { "" }
                ),
            });
        }

        items.push(ListPickerItem {
            id: "sep_more".into(),
            label: "── More ──".into(),
            detail: String::new(),
        });
        items.push(ListPickerItem {
            id: "__turns__".into(),
            label: "Recent Turns →".into(),
            detail: "(Enter to view)".into(),
        });
        self.replace_list_picker(ListPickerState {
            kind: ListPickerKind::Usage,
            title: " /usage ".into(),
            items,
            selected: 0,
            filter: String::new(),
            all_items: Vec::new(),
        });
    }

    fn open_usage_turns_picker(&mut self) {
        let mut items = Vec::new();
        for (i, turn) in self.state.usage_turns.iter().rev().take(20).enumerate() {
            let hit = kkagent_protocol::cache_hit_ratio_ex(
                turn.input_tokens,
                turn.cache_creation_tokens,
                turn.cache_read_tokens,
                turn.input_includes_cache,
            );
            items.push(ListPickerItem {
                id: format!("turn{i}"),
                label: format!("Turn −{}", i + 1),
                detail: format!(
                    "in={:>8} out={:>7} · cc={:>7} cr={:>8} · hit={:>5} · {:>6}ms",
                    fmt_thousands(turn.input_tokens),
                    fmt_thousands(turn.output_tokens),
                    fmt_thousands(turn.cache_creation_tokens),
                    fmt_thousands(turn.cache_read_tokens),
                    hit.map(|h| format!("{:.0}%", h * 100.0))
                        .unwrap_or_else(|| "n/a".into()),
                    fmt_thousands(turn.duration_ms),
                ),
            });
        }
        if items.is_empty() {
            items.push(ListPickerItem {
                id: "empty".into(),
                label: "No turn samples yet".into(),
                detail: String::new(),
            });
        }
        self.replace_list_picker(ListPickerState {
            kind: ListPickerKind::UsageTurns,
            title: " /usage · Recent Turns ".into(),
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
            label: "Web".into(),
            detail: if web_configured {
                "ok — configured".into()
            } else {
                "warning — [services.web_search] missing".into()
            },
        });
        items.push(ListPickerItem {
            id: "model".into(),
            label: "Default model".into(),
            detail: self.config.default_model_alias().unwrap_or("-").to_string(),
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
                detail: "Submit; queues while a turn is running".into(),
            },
            ListPickerItem {
                id: "ctrl_s_steer".into(),
                label: "Ctrl-S".into(),
                detail: "Steer a running turn immediately".into(),
            },
            ListPickerItem {
                id: "shift_enter".into(),
                label: "Shift-Enter".into(),
                detail: "Insert newline".into(),
            },
            ListPickerItem {
                id: "ctrl_j".into(),
                label: "Ctrl-J".into(),
                detail: "Insert newline".into(),
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
                detail: "Close / interrupt / Esc Esc edit history".into(),
            },
            ListPickerItem {
                id: "shell".into(),
                label: "!".into(),
                detail: "Local shell (no agent)".into(),
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
                detail: "Toggle the full-screen BTW workspace".into(),
            },
            ListPickerItem {
                id: "history".into(),
                label: "Ctrl-O / Ctrl-T / Ctrl-P/N".into(),
                detail: "Recent tool output · todos · input history".into(),
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
            "tasks" | "task" | "ps" => self.open_tasks_panel().await?,
            "agents" | "agent" => self.open_agents_panel(),
            "mcp" => self.open_mcp_manager().await?,
            "skills" => self.open_skill_manager().await?,
            "swarm" => self.open_swarm_picker(),
            "plugins" | "plugin" => self.open_plugins_picker().await?,
            "reload" => self.reload_config_from_disk().await,
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
        let items = vec![
            ListPickerItem {
                id: "installed".into(),
                label: "Installed plugins".into(),
                detail: "view, update, enable, disable, or remove".into(),
            },
            ListPickerItem {
                id: "marketplaces".into(),
                label: "Plugin marketplaces".into(),
                detail: "browse marketplaces and plugin details".into(),
            },
            ListPickerItem {
                id: "add_marketplace".into(),
                label: "Add marketplace".into(),
                detail: "register a catalog URL or local JSON path".into(),
            },
            ListPickerItem {
                id: "install_source".into(),
                label: "Install from source".into(),
                detail: "local directory, ZIP URL, or GitHub repository".into(),
            },
            ListPickerItem {
                id: "reload".into(),
                label: "Reload plugins".into(),
                detail: "rediscover plugins and restart plugin MCP servers".into(),
            },
        ];
        self.replace_list_picker(ListPickerState {
            kind: ListPickerKind::PluginHome,
            title: " Plugins · Enter select · Esc close ".into(),
            items,
            selected: 0,

            filter: String::new(),
            all_items: Vec::new(),
        });
        Ok(())
    }

    async fn open_installed_plugins_picker(&mut self) -> anyhow::Result<()> {
        let result = self.client.rpc_call("plugins.list", None).await?;
        let mut items = Vec::new();
        for plugin in result
            .get("plugins")
            .and_then(|value| value.as_array())
            .into_iter()
            .flatten()
        {
            let id = plugin
                .get("name")
                .and_then(|value| value.as_str())
                .unwrap_or("plugin");
            let display_name = plugin
                .get("display_name")
                .and_then(|value| value.as_str())
                .unwrap_or(id);
            let version = plugin
                .get("version")
                .and_then(|value| value.as_str())
                .unwrap_or("?");
            let enabled = plugin
                .get("enabled")
                .and_then(|value| value.as_bool())
                .unwrap_or(true);
            let managed = plugin
                .get("managed")
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            items.push(ListPickerItem {
                id: id.into(),
                label: display_name.into(),
                detail: format!(
                    "v{version} · {}{}",
                    if enabled { "enabled" } else { "disabled" },
                    if managed { " · managed" } else { "" }
                ),
            });
        }
        self.replace_list_picker(ListPickerState {
            kind: ListPickerKind::PluginInstalled,
            title: " Installed plugins · Enter details · Esc back ".into(),
            all_items: items.clone(),
            items,
            selected: 0,
            filter: String::new(),
        });
        Ok(())
    }

    async fn open_installed_plugin_detail(&mut self, id: &str) -> anyhow::Result<()> {
        let plugin = self
            .client
            .rpc_call("plugins.info", Some(serde_json::json!({"id": id})))
            .await?;
        let enabled = plugin
            .get("enabled")
            .and_then(|value| value.as_bool())
            .unwrap_or(true);
        let managed = plugin
            .get("managed")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        let mut items = vec![ListPickerItem {
            id: if enabled { "disable" } else { "enable" }.into(),
            label: if enabled { "Disable" } else { "Enable" }.into(),
            detail: "apply to this plugin and its MCP servers".into(),
        }];
        if managed {
            items.push(ListPickerItem {
                id: "update".into(),
                label: "Update".into(),
                detail: "reinstall from the recorded source".into(),
            });
            items.push(ListPickerItem {
                id: "remove".into(),
                label: "Remove".into(),
                detail: "remove it from the managed plugin set".into(),
            });
        }
        for (label, value) in [
            ("Version", plugin.get("version").and_then(|v| v.as_str())),
            (
                "Description",
                plugin.get("description").and_then(|v| v.as_str()),
            ),
            ("Path", plugin.get("path").and_then(|v| v.as_str())),
        ] {
            if let Some(value) = value.filter(|value| !value.is_empty()) {
                items.push(ListPickerItem {
                    id: "__info__".into(),
                    label: label.into(),
                    detail: value.into(),
                });
            }
        }
        let mcp = plugin
            .get("mcp_servers")
            .and_then(|value| value.as_array())
            .map(|servers| {
                servers
                    .iter()
                    .filter_map(|value| value.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        if !mcp.is_empty() {
            items.push(ListPickerItem {
                id: "__info__".into(),
                label: "MCP servers".into(),
                detail: mcp,
            });
        }
        self.replace_list_picker(ListPickerState {
            kind: ListPickerKind::PluginInstalledDetail,
            title: format!(" {id} · plugin details · Esc back "),
            all_items: items.clone(),
            items,
            selected: 0,
            filter: String::new(),
        });
        Ok(())
    }

    async fn open_plugin_marketplaces_picker(&mut self) -> anyhow::Result<()> {
        let result = self
            .client
            .rpc_call("plugins.marketplaces.list", None)
            .await?;
        let mut items = Vec::new();
        for marketplace in result
            .get("marketplaces")
            .and_then(|value| value.as_array())
            .into_iter()
            .flatten()
        {
            let source = marketplace
                .get("source")
                .and_then(|value| value.as_str())
                .unwrap_or_default();
            let descriptor = serde_json::json!({
                "id": marketplace.get("id").and_then(|value| value.as_str()),
                "source": source,
                "removable": marketplace.get("removable").and_then(|value| value.as_bool()).unwrap_or(false),
            });
            items.push(ListPickerItem {
                id: descriptor.to_string(),
                label: marketplace
                    .get("name")
                    .and_then(|value| value.as_str())
                    .unwrap_or("Marketplace")
                    .into(),
                detail: source.into(),
            });
        }
        items.push(ListPickerItem {
            id: "__add__".into(),
            label: "+ Add marketplace".into(),
            detail: "catalog URL or local marketplace.json".into(),
        });
        self.replace_list_picker(ListPickerState {
            kind: ListPickerKind::PluginMarketplaces,
            title: " Plugin marketplaces · Enter browse · Esc back ".into(),
            all_items: items.clone(),
            items,
            selected: 0,
            filter: String::new(),
        });
        Ok(())
    }

    async fn open_marketplace_entries_picker(&mut self, source: &str) -> anyhow::Result<()> {
        let result = self
            .client
            .rpc_call(
                "plugins.marketplace",
                Some(serde_json::json!({"source": source})),
            )
            .await?;
        let items = result
            .get("plugins")
            .and_then(|value| value.as_array())
            .into_iter()
            .flatten()
            .map(|plugin| {
                let id = plugin
                    .get("id")
                    .and_then(|value| value.as_str())
                    .unwrap_or("plugin");
                let name = plugin
                    .get("displayName")
                    .and_then(|value| value.as_str())
                    .unwrap_or(id);
                let version = plugin
                    .get("version")
                    .and_then(|value| value.as_str())
                    .map(|version| format!("v{version}"))
                    .unwrap_or_default();
                let status = if plugin
                    .get("updateAvailable")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false)
                {
                    "update available"
                } else if plugin
                    .get("installed")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false)
                {
                    "installed"
                } else {
                    "available"
                };
                ListPickerItem {
                    id: id.into(),
                    label: name.into(),
                    detail: format!("{version} · {status}"),
                }
            })
            .collect::<Vec<_>>();
        self.replace_list_picker(ListPickerState {
            kind: ListPickerKind::PluginMarketplaceEntries,
            title: " Marketplace plugins · Enter details · Esc back ".into(),
            all_items: items.clone(),
            items,
            selected: 0,
            filter: String::new(),
        });
        Ok(())
    }

    async fn marketplace_plugin(&self, id: &str) -> anyhow::Result<serde_json::Value> {
        let source = self
            .state
            .plugin_marketplace_source
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("no marketplace is selected"))?;
        let result = self
            .client
            .rpc_call(
                "plugins.marketplace",
                Some(serde_json::json!({"source": source})),
            )
            .await?;
        result
            .get("plugins")
            .and_then(|value| value.as_array())
            .and_then(|plugins| {
                plugins
                    .iter()
                    .find(|plugin| plugin.get("id").and_then(|value| value.as_str()) == Some(id))
            })
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("plugin {id} is no longer in the marketplace"))
    }

    async fn open_marketplace_plugin_detail(&mut self, id: &str) -> anyhow::Result<()> {
        let plugin = self.marketplace_plugin(id).await?;
        let installed = plugin
            .get("installed")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        let update = plugin
            .get("updateAvailable")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        let mut items = vec![ListPickerItem {
            id: if update { "update" } else { "install" }.into(),
            label: if update {
                "Update plugin"
            } else if installed {
                "Reinstall plugin"
            } else {
                "Install plugin"
            }
            .into(),
            detail: plugin
                .get("version")
                .and_then(|value| value.as_str())
                .map(|version| format!("marketplace version {version}"))
                .unwrap_or_default(),
        }];
        for (label, key) in [
            ("Description", "description"),
            ("Homepage", "homepage"),
            ("Keywords", "keywords"),
            ("Source", "source"),
        ] {
            let value = plugin.get(key).map(|value| {
                value
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| value.to_string())
            });
            if let Some(value) = value.filter(|value| !value.is_empty() && value != "[]") {
                items.push(ListPickerItem {
                    id: "__info__".into(),
                    label: label.into(),
                    detail: value,
                });
            }
        }
        self.replace_list_picker(ListPickerState {
            kind: ListPickerKind::PluginMarketplaceDetail,
            title: format!(" {id} · marketplace details · Esc back "),
            all_items: items.clone(),
            items,
            selected: 0,
            filter: String::new(),
        });
        Ok(())
    }

    fn open_plugin_prompt(&mut self, kind: PluginPromptKind) {
        self.state.plugin_prompt = Some(PluginPromptState {
            kind,
            value: String::new(),
        });
    }

    async fn submit_plugin_prompt(&mut self) -> anyhow::Result<()> {
        let Some(prompt) = self.state.plugin_prompt.take() else {
            return Ok(());
        };
        let value = prompt.value.trim();
        if value.is_empty() {
            self.state.plugin_prompt = Some(prompt);
            return Ok(());
        }
        let result = match prompt.kind {
            PluginPromptKind::AddMarketplace => {
                self.client
                    .rpc_call(
                        "plugins.marketplaces.add",
                        Some(serde_json::json!({"source": value})),
                    )
                    .await
            }
            PluginPromptKind::InstallSource => {
                self.client
                    .rpc_call(
                        "plugins.install",
                        Some(serde_json::json!({"source": value})),
                    )
                    .await
            }
        };
        match result {
            Ok(_) => {
                self.system_message(match prompt.kind {
                    PluginPromptKind::AddMarketplace => {
                        format!("Plugin marketplace added: {value}")
                    }
                    PluginPromptKind::InstallSource => format!("Plugin installed from {value}"),
                });
                self.begin_root_picker();
                self.open_plugins_picker().await?;
                let root = self.state.list_picker.take().expect("plugin root picker");
                self.state.list_picker_stack.push(root);
                let open_result = match prompt.kind {
                    PluginPromptKind::AddMarketplace => {
                        self.open_plugin_marketplaces_picker().await
                    }
                    PluginPromptKind::InstallSource => self.open_installed_plugins_picker().await,
                };
                if let Err(error) = open_result {
                    self.pop_list_picker_level();
                    self.system_message(format!("Failed to refresh plugin manager: {error}"));
                }
            }
            Err(error) => {
                self.system_message(format!("Plugin operation failed: {error}"));
                self.state.plugin_prompt = Some(prompt);
            }
        }
        Ok(())
    }

    async fn reload_plugins_from_picker(&mut self) {
        match self.client.rpc_call("plugins.reload", None).await {
            Ok(result) => self.system_message(format!(
                "Plugins reloaded: {} plugin(s), {} MCP server(s), {} tool(s)",
                result
                    .get("plugins")
                    .and_then(|value| value.as_u64())
                    .unwrap_or(0),
                result
                    .get("mcp_servers")
                    .and_then(|value| value.as_u64())
                    .unwrap_or(0),
                result
                    .get("tools")
                    .and_then(|value| value.as_u64())
                    .unwrap_or(0),
            )),
            Err(error) => self.system_message(format!("Plugin reload failed: {error}")),
        }
    }

    async fn apply_installed_plugin_action(
        &mut self,
        picker: ListPickerState,
        action: &str,
    ) -> anyhow::Result<()> {
        if action == "__info__" {
            self.state.list_picker = Some(picker);
            return Ok(());
        }
        let Some(id) = self.state.plugin_selected_id.clone() else {
            self.state.list_picker = Some(picker);
            return Ok(());
        };
        if action == "remove" {
            self.state.list_picker_stack.push(picker);
            let items = vec![
                ListPickerItem {
                    id: "cancel".into(),
                    label: "Cancel".into(),
                    detail: "keep the plugin installed".into(),
                },
                ListPickerItem {
                    id: "remove_plugin".into(),
                    label: "Remove plugin".into(),
                    detail: format!("remove {id} from the managed plugin set"),
                },
            ];
            self.replace_list_picker(ListPickerState {
                kind: ListPickerKind::PluginConfirm,
                title: format!(" Remove {id}? "),
                all_items: items.clone(),
                items,
                selected: 0,
                filter: String::new(),
            });
            return Ok(());
        }
        let method = match action {
            "enable" => "plugins.enable",
            "disable" => "plugins.disable",
            "update" => "plugins.update",
            _ => {
                self.state.list_picker = Some(picker);
                return Ok(());
            }
        };
        match self
            .client
            .rpc_call(method, Some(serde_json::json!({"id": id})))
            .await
        {
            Ok(_) => {
                self.system_message(format!("Plugin {action} succeeded: {id}"));
                if let Err(error) = self.open_installed_plugin_detail(&id).await {
                    self.state.list_picker = Some(picker);
                    self.system_message(format!("Failed to refresh plugin details: {error}"));
                }
            }
            Err(error) => {
                self.state.list_picker = Some(picker);
                self.system_message(format!("Plugin {action} failed: {error}"));
            }
        }
        Ok(())
    }

    async fn apply_marketplace_plugin_action(
        &mut self,
        picker: ListPickerState,
        action: &str,
    ) -> anyhow::Result<()> {
        if action == "__info__" {
            self.state.list_picker = Some(picker);
            return Ok(());
        }
        let Some(id) = self.state.plugin_selected_id.clone() else {
            self.state.list_picker = Some(picker);
            return Ok(());
        };
        let Some(source) = self.state.plugin_marketplace_source.clone() else {
            self.state.list_picker = Some(picker);
            return Ok(());
        };
        if !matches!(action, "install" | "update") {
            self.state.list_picker = Some(picker);
            return Ok(());
        }
        let params = serde_json::json!({"source": id, "marketplace": source});
        match self.client.rpc_call("plugins.install", Some(params)).await {
            Ok(_) => {
                self.system_message(format!("Plugin {action} succeeded: {id}"));
                if let Err(error) = self.open_marketplace_plugin_detail(&id).await {
                    self.state.list_picker = Some(picker);
                    self.system_message(format!("Failed to refresh marketplace plugin: {error}"));
                }
            }
            Err(error) => {
                self.state.list_picker = Some(picker);
                self.system_message(format!("Plugin {action} failed: {error}"));
            }
        }
        Ok(())
    }

    async fn apply_plugin_confirmation(
        &mut self,
        picker: ListPickerState,
        action: &str,
    ) -> anyhow::Result<()> {
        if action == "cancel" {
            self.pop_list_picker_level();
            return Ok(());
        }
        let Some(id) = self.state.plugin_selected_id.clone() else {
            self.state.list_picker = Some(picker);
            return Ok(());
        };
        if action != "remove_plugin" {
            self.state.list_picker = Some(picker);
            return Ok(());
        }
        match self
            .client
            .rpc_call("plugins.remove", Some(serde_json::json!({"id": id})))
            .await
        {
            Ok(_) => {
                self.system_message(format!("Plugin removed: {id}"));
                self.begin_root_picker();
                self.open_plugins_picker().await?;
                let root = self.state.list_picker.take().expect("plugin root picker");
                self.state.list_picker_stack.push(root);
                if let Err(error) = self.open_installed_plugins_picker().await {
                    self.pop_list_picker_level();
                    self.system_message(format!("Failed to refresh installed plugins: {error}"));
                }
            }
            Err(error) => {
                self.state.list_picker = Some(picker);
                self.system_message(format!("Plugin remove failed: {error}"));
            }
        }
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
                detail: "view background tasks".into(),
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

    async fn reload_config_from_disk(&mut self) {
        let local = kkagent_config::load_config(Some(&self.config_path));
        let server = self.client.rpc_call("config.reload", None).await;
        match (local, server) {
            (Ok(config), Ok(result)) => {
                self.config = config;
                let models = result
                    .get("models")
                    .and_then(|value| value.as_u64())
                    .unwrap_or(0);
                self.system_message(format!(
                    "Config reloaded from disk ({models} model(s)). MCP/hooks may still need a restart."
                ));
            }
            (Ok(config), Err(error)) => {
                self.config = config;
                self.system_message(format!(
                    "TUI config reloaded locally, but server reload failed: {error}"
                ));
            }
            (Err(error), Ok(_)) => {
                self.system_message(format!(
                    "Server config reloaded, but local TUI reload failed: {error}"
                ));
            }
            (Err(local_error), Err(server_error)) => {
                self.system_message(format!(
                    "Reload failed (server: {server_error}; local: {local_error})"
                ));
            }
        }
    }

    async fn open_session_picker(&mut self) -> anyhow::Result<()> {
        // Open a placeholder immediately; list fills when the background job returns.
        let opening = !self
            .state
            .list_picker
            .as_ref()
            .is_some_and(|p| p.kind == ListPickerKind::Session);
        if opening {
            self.state.session_picker_all_workspaces = false;
            self.state.session_picker_entries.clear();
            self.replace_list_picker(ListPickerState {
                kind: ListPickerKind::Session,
                title: " Sessions · current workspace · Tab show all · ↑↓ Enter · Ctrl-D delete "
                    .into(),
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
            Some(serde_json::json!({"limit": 1000})),
            Some("Loading sessions".into()),
            true,
        );
        Ok(())
    }

    fn apply_session_picker_list(&mut self, data: serde_json::Value) {
        let cwd = self.state.working_dir.clone();
        let cwd_key = normalized_workspace_key(&cwd);
        let mut entries = Vec::new();
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
                let same_workspace = !work.is_empty()
                    && (normalized_workspace_key(std::path::Path::new(work)) == cwd_key
                        || work == ".");
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
                    s.get("first_prompt").and_then(|v| v.as_str()),
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
                entries.push(SessionPickerEntry {
                    item: ListPickerItem {
                        id: id.clone(),
                        label: format!("{short} — {title}"),
                        detail: format!("{fork}{mark}"),
                    },
                    workspace: work.to_string(),
                    same_workspace,
                });
            }
        }
        if entries.is_empty() {
            self.state.list_picker = None;
            self.state.session_picker_preview = None;
            self.state.session_picker_entries.clear();
            self.system_message("No sessions found.".into());
            return;
        }
        // Preserve the currently *highlighted* session across background
        // `sessions.list` refreshes. The running session (`current`) is only a
        // fallback — otherwise navigating the picker gets yanked back to the
        // running session's position every time a refresh lands.
        let prior_selected_id = self
            .state
            .list_picker
            .as_ref()
            .filter(|p| p.kind == ListPickerKind::Session)
            .and_then(|p| p.items.get(p.selected).map(|i| i.id.clone()));
        let prior_filter = self
            .state
            .list_picker
            .as_ref()
            .filter(|p| p.kind == ListPickerKind::Session)
            .map(|p| p.filter.clone())
            .unwrap_or_default();
        self.state.session_picker_entries = entries;
        self.replace_list_picker(ListPickerState {
            kind: ListPickerKind::Session,
            title: String::new(),
            items: Vec::new(),
            selected: 0,
            filter: prior_filter,
            all_items: Vec::new(),
        });
        self.apply_session_picker_filter();
        if let Some(picker) = self.state.list_picker.as_mut() {
            picker.selected = prior_selected_id
                .as_deref()
                .and_then(|id| picker.items.iter().position(|item| item.id == *id))
                .or_else(|| {
                    current
                        .as_ref()
                        .and_then(|id| picker.items.iter().position(|item| item.id == *id))
                })
                .unwrap_or(0);
        }
        self.state.session_delete_confirm = None;
        self.refresh_session_picker_preview();
    }

    fn apply_session_picker_filter(&mut self) {
        let all_workspaces = self.state.session_picker_all_workspaces;
        let source = self
            .state
            .session_picker_entries
            .iter()
            .filter(|entry| all_workspaces || entry.same_workspace)
            .map(|entry| {
                let mut item = entry.item.clone();
                if all_workspaces {
                    item.detail =
                        format!("{} · {}", item.detail, display_workspace(&entry.workspace));
                }
                item
            })
            .collect::<Vec<_>>();
        let Some(picker) = self.state.list_picker.as_mut() else {
            return;
        };
        if picker.kind != ListPickerKind::Session {
            return;
        }
        // Remember the highlighted item so we can keep the cursor on it after
        // the visible set changes.
        let keep_id = picker.items.get(picker.selected).map(|i| i.id.clone());
        let q = picker.filter.to_ascii_lowercase();
        picker.all_items = source;
        if q.is_empty() {
            picker.items = picker.all_items.clone();
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
        }
        let scope = if all_workspaces {
            "all workspaces"
        } else {
            "current workspace"
        };
        let tab_action = if all_workspaces {
            "Tab current only"
        } else {
            "Tab show all"
        };
        picker.title = if picker.filter.is_empty() {
            format!(
                " Sessions · {scope} · {tab_action} · ↑↓ Enter · Ctrl-D delete · {}/{} ",
                picker.items.len(),
                picker.all_items.len()
            )
        } else {
            format!(
                " Sessions · {scope} · filter {:?} · {}/{} · {tab_action} ",
                picker.filter,
                picker.items.len(),
                picker.all_items.len()
            )
        };
        // Try to keep the cursor on the same item; fall back to clamp.
        let new_selected = keep_id
            .as_deref()
            .and_then(|id| picker.items.iter().position(|i| i.id == *id))
            .unwrap_or_else(|| {
                if picker.selected >= picker.items.len() {
                    picker.items.len().saturating_sub(1)
                } else {
                    picker.selected
                }
            });
        picker.selected = new_selected;
    }

    fn cache_active_session_state(&mut self, target: &str) -> Option<String> {
        let leaving_id = self.state.session_id.clone();

        if let Some(ref leaving) = leaving_id {
            if leaving != target {
                persist_composer_draft(leaving, &self.state.input);
                let view = crate::session_view::SessionViewState::capture(
                    &self.state.input,
                    self.state.scroll_up,
                    self.state.follow_bottom,
                    self.state.todos_expanded,
                    &self.state.search,
                    self.state.highlight_message,
                );
                self.state.session_views.insert(leaving.clone(), view);
                self.state
                    .session_runtime_states
                    .insert(leaving.clone(), SessionRuntimeState::capture(&self.state));
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
        leaving_id
    }

    fn take_cached_session_runtime(
        &mut self,
        query: &str,
    ) -> Option<(String, SessionRuntimeState)> {
        if let Some(cached) = self.state.session_runtime_states.remove(query) {
            return Some((query.to_string(), cached));
        }
        let matches: Vec<String> = self
            .state
            .session_runtime_states
            .keys()
            .filter(|id| *id == query || id.starts_with(query))
            .cloned()
            .collect();
        if matches.len() == 1 {
            let id = matches.into_iter().next()?;
            return self
                .state
                .session_runtime_states
                .remove(&id)
                .map(|cached| (id, cached));
        }
        None
    }

    fn clear_session_composer_draft(&mut self, session_id: &str) {
        if self.state.session_id.as_deref() == Some(session_id) {
            self.state.input.clear();
            self.state.history_index = None;
            self.state.history_draft.clear();
        }
        if let Some(view) = self.state.session_views.get_mut(session_id) {
            view.draft.clear();
            view.cursor = 0;
        }
        crate::draft_store::clear_draft(session_id);
    }

    fn restore_session_view(&mut self, session_id: &str) {
        if let Some(mut view) = self.state.session_views.remove(session_id) {
            if slash_draft_looks_consumed(&view.draft) {
                view.draft.clear();
                view.cursor = 0;
                crate::draft_store::clear_draft(session_id);
            }
            view.restore_into(
                &mut self.state.input,
                &mut self.state.scroll_up,
                &mut self.state.follow_bottom,
                &mut self.state.todos_expanded,
                &mut self.state.search,
                &mut self.state.highlight_message,
            );
        } else if let Some(draft) = crate::draft_store::load_draft(session_id) {
            if slash_draft_looks_consumed(&draft.text) {
                crate::draft_store::clear_draft(session_id);
                self.state.input.clear();
            } else {
                self.state.input.set_text(draft.text);
                self.state.input.cursor = draft.cursor.min(self.state.input.text.len());
            }
            self.state.scroll_up = 0;
            self.state.follow_bottom = true;
            self.state.todos_expanded = false;
            self.state.search = crate::search::SearchState::default();
            self.state.highlight_message = None;
        } else {
            self.state.input.clear();
            self.state.scroll_up = 0;
            self.state.follow_bottom = true;
            self.state.todos_expanded = false;
            self.state.search = crate::search::SearchState::default();
            self.state.highlight_message = None;
        }
        // Reset scroll-anchor tracking so the first render of the newly
        // activated session doesn't compute a bogus content-height delta
        // against the previous session.
        self.state.prev_content_lines = None;
    }

    fn replay_background_session_events(&mut self, session_id: &str) {
        self.state.background_session_event_bytes.remove(session_id);
        let Some(mut events) = self.state.background_session_events.remove(session_id) else {
            return;
        };
        while let Some(event) = events.pop_front() {
            let data = serde_json::to_value(event).unwrap_or_default();
            self.handle_server_event(Frame::Event {
                event: "agent".into(),
                scope: None,
                data,
            });
        }
    }

    fn queue_background_session_event(&mut self, session_id: String, event: AgentEvent) {
        const MAX_SESSIONS: usize = 16;
        const MAX_EVENTS: usize = 256;
        const MAX_BYTES: usize = 2 * 1024 * 1024;

        let event_bytes = serde_json::to_vec(&event).map_or(0, |data| data.len());
        if event_bytes > MAX_BYTES {
            return;
        }
        if !self
            .state
            .background_session_events
            .contains_key(&session_id)
            && self.state.background_session_events.len() >= MAX_SESSIONS
        {
            if let Some(evicted) = self.state.background_session_events.keys().next().cloned() {
                self.drop_background_session_events(&evicted);
            }
        }
        let queue = self
            .state
            .background_session_events
            .entry(session_id.clone())
            .or_default();
        let bytes = self
            .state
            .background_session_event_bytes
            .entry(session_id)
            .or_default();
        while queue.len() >= MAX_EVENTS || bytes.saturating_add(event_bytes) > MAX_BYTES {
            let Some(dropped) = queue.pop_front() else {
                break;
            };
            *bytes = bytes.saturating_sub(
                serde_json::to_vec(&dropped).map_or(0, |serialized| serialized.len()),
            );
        }
        queue.push_back(event);
        *bytes = bytes.saturating_add(event_bytes);
    }

    fn drop_background_session_events(&mut self, session_id: &str) {
        self.state.background_session_events.remove(session_id);
        self.state.background_session_event_bytes.remove(session_id);
    }

    fn sync_active_session_status(&mut self) {
        self.state.event_router.status = self.state.status;
        self.state.event_router.turn_active = matches!(
            self.state.status,
            SessionStatus::Thinking
                | SessionStatus::ToolExecuting
                | SessionStatus::WaitingApproval
                | SessionStatus::WaitingQuestion
                | SessionStatus::Compacting
                | SessionStatus::Cancelling
        );
    }

    fn activate_cached_session(
        &mut self,
        session_id: &str,
        cached: SessionRuntimeState,
        started_at: std::time::Instant,
    ) {
        use crate::async_jobs::JobChannel;

        let old_generation = self.jobs.current_generation(JobChannel::SessionResume);
        self.jobs
            .mark_done(JobChannel::SessionResume, old_generation);
        self.jobs.next_generation(JobChannel::SessionResume);
        self.state.resume_switch = None;
        self.state.session_id = Some(session_id.to_string());
        cached.restore(&mut self.state);
        self.state.approval_pending = self.state.parked_approvals.remove(session_id);
        self.state.question_pending = self.state.parked_questions.remove(session_id);
        if self.state.approval_pending.is_some() {
            self.state.status = SessionStatus::WaitingApproval;
        } else if self.state.question_pending.is_some() {
            self.state.status = SessionStatus::WaitingQuestion;
        }
        // Don't force the plan-document overlay when switching to a cached
        // session — show the normal transcript instead.  The plan content is
        // preserved as a MessageRole::Plan entry in `messages`, and the next
        // PlanFileUpdated event (replayed below or received live) restores the
        // focus view.  Exception: when the agent submitted a plan for review
        // and is waiting for the user to approve, the plan document must stay
        // visible.
        let needs_plan_review = self
            .state
            .approval_pending
            .as_ref()
            .is_some_and(|a| a.is_plan_review);
        if self.state.plan_mode && !needs_plan_review {
            self.state.dismiss_plan_focus();
        }
        self.state.status_bar.session_id = Some(session_id.to_string());
        // Reopening a session clears its closed-tab tombstone so the footer
        // strip (and future refreshes) may show it again.
        self.state.closed_tab_ids.remove(session_id);
        self.state.tab_strip.ensure_active(session_id, "session");
        self.state.list_picker = None;
        self.state.session_picker_preview = None;
        self.restore_session_view(session_id);
        self.replay_background_session_events(session_id);
        self.sync_active_session_status();
        self.state.last_switch_metrics = Some(SessionSwitchMetrics {
            target: session_id.to_string(),
            first_feedback_ms: 0,
            visible_ms: started_at.elapsed().as_millis() as u64,
        });
        self.enqueue_workspace_sessions_refresh();
    }

    async fn resume_session(&mut self, query: &str) -> anyhow::Result<()> {
        self.resume_session_in_workspace(query, None).await
    }

    async fn resume_session_in_workspace(
        &mut self,
        query: &str,
        workspace: Option<&str>,
    ) -> anyhow::Result<()> {
        // Session switches always leave the virtual BTW workspace.  Keep this
        // guard here as well as in strip activation because resume can also be
        // reached from pickers, attention cycling, and slash commands.
        if self.state.mode == AppMode::Btw {
            self.exit_btw_view();
        }
        if self
            .state
            .pending_resume_prefill
            .as_ref()
            .is_some_and(|(target, _)| target != query)
        {
            self.state.pending_resume_prefill = None;
        }
        let started_at = std::time::Instant::now();
        let leaving_id = self.cache_active_session_state(query);
        if let Some((cached_id, cached)) = self.take_cached_session_runtime(query) {
            self.activate_cached_session(&cached_id, cached, started_at);
            return Ok(());
        }

        self.state.resume_switch = Some(ResumeSwitchCtx {
            target: query.to_string(),
            leaving_id,
            started_at,
        });
        // Keep showing the current transcript until the target loads.
        self.jobs.spawn_session_resume(
            self.client.requester(),
            query.to_string(),
            workspace
                .map(str::to_owned)
                .unwrap_or_else(|| self.state.working_dir.to_string_lossy().into_owned()),
        );
        // First feedback is the non-blocking job notice (same tick).
        if let Some(ctx) = self.state.resume_switch.as_ref() {
            let _ = ctx.started_at.elapsed();
        }
        Ok(())
    }

    fn clear_failed_resume(&mut self, query: &str) {
        if self
            .state
            .resume_switch
            .as_ref()
            .is_some_and(|switch| switch.target == query)
        {
            self.state.resume_switch = None;
        }
        if self
            .state
            .pending_resume_prefill
            .as_ref()
            .is_some_and(|(target, _)| target == query)
        {
            self.state.pending_resume_prefill = None;
        }
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
        if leaving_id.as_deref() != Some(sid.as_str())
            && self.state.session_id.as_deref() == leaving_id.as_deref()
        {
            if let Some(ref leaving) = leaving_id {
                let view = crate::session_view::SessionViewState::capture(
                    &self.state.input,
                    self.state.scroll_up,
                    self.state.follow_bottom,
                    self.state.todos_expanded,
                    &self.state.search,
                    self.state.highlight_message,
                );
                self.state.session_views.insert(leaving.clone(), view);
                self.state
                    .session_runtime_states
                    .insert(leaving.clone(), SessionRuntimeState::capture(&self.state));
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
        self.state.session_id = Some(sid.clone());
        // Resuming a session re-opens its tab: clear any closed-tab tombstone
        // so subsequent refreshes show the indicator again.
        self.state.closed_tab_ids.remove(&sid);
        if let Some(working_dir) = data.get("working_dir").and_then(|v| v.as_str()) {
            self.state.working_dir = std::fs::canonicalize(working_dir)
                .unwrap_or_else(|_| std::path::PathBuf::from(working_dir));
        }
        self.state.messages.clear();
        self.state.active_assistant_message = None;
        self.state.todos.clear();
        self.state.subagents = crate::subagents::SubagentStore::default();
        self.state.subagents_panel = None;
        self.state.todos_expanded = false;
        self.state.thinking_text.clear();
        self.state.last_tool_name = None;
        self.state.plan_document = None;
        self.state.plan_scroll_to_top = false;
        self.state.turn_started_at = None;
        self.state.reset_context_usage_stats();
        self.state.approval_queue.clear();
        self.state.prompt_queue = crate::prompt_queue::PromptQueue::default();
        self.state.scroll_up = 0;
        self.state.follow_bottom = true;
        self.state.render_cache.invalidate_all();
        self.state.transcript_layout_cache.invalidate();
        self.state.history_loading = false;
        self.state.history_oldest_index = None;
        self.state.history_total = None;
        let turn_active = data
            .get("turn_active")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        self.state.status = data
            .get("status")
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or(if turn_active {
                SessionStatus::Thinking
            } else {
                SessionStatus::Idle
            });
        self.state.approval_pending = self.state.parked_approvals.remove(&sid);
        self.state.question_pending = self.state.parked_questions.remove(&sid);
        if let Some(value) = data.get("pending_approval") {
            self.state.approval_pending = if value.is_null() {
                None
            } else {
                serde_json::from_value::<kkagent_protocol::ApprovalRequest>(value.clone())
                    .ok()
                    .map(|request| {
                        pending_approval_from_request(
                            &request,
                            data.get("pending_approval_resumed")
                                .and_then(|value| value.as_bool())
                                .unwrap_or(false),
                        )
                    })
            };
        }
        if let Some(value) = data.get("pending_question") {
            self.state.question_pending = if value.is_null() {
                None
            } else {
                serde_json::from_value::<kkagent_protocol::QuestionPayload>(value.clone())
                    .ok()
                    .map(|question| {
                        let options: Vec<(String, String)> = question
                            .options
                            .into_iter()
                            .map(|o| (o.id, o.label))
                            .collect();
                        let toggled = vec![false; options.len()];
                        PendingQuestion {
                            question_id: question.question_id,
                            text: question.text,
                            options,
                            allow_free_text: question.allow_free_text,
                            allow_multiple: question.allow_multiple,
                            selected: 0,
                            toggled,
                            free_text: String::new(),
                        }
                    })
            };
        }
        if self.state.approval_pending.is_some() {
            self.state.status = SessionStatus::WaitingApproval;
        } else if self.state.question_pending.is_some() {
            self.state.status = SessionStatus::WaitingQuestion;
        }

        if let Some(msgs) = data.get("messages").and_then(|v| v.as_array()) {
            self.state.messages = transcript_messages_to_display(msgs);
            self.state.apply_tool_output_mode();
            self.state.active_assistant_message = None;
        }
        if let Some(live) = data.get("live_ui") {
            if let Some(thinking) = live.get("thinking_text").and_then(|v| v.as_str()) {
                self.state.thinking_text = thinking.to_string();
            }
            if let Some(assistant) = live
                .get("assistant_text")
                .and_then(|v| v.as_str())
                .filter(|text| !text.is_empty())
            {
                let mut msg = DisplayMessage {
                    role: MessageRole::Assistant,
                    content: String::new(),
                    thinking: if self.state.thinking_text.is_empty() {
                        None
                    } else {
                        Some(std::mem::take(&mut self.state.thinking_text))
                    },
                    parts: Vec::new(),
                    tool_calls: Vec::new(),
                    delivery: crate::prompt_queue::DeliveryState::Sent,
                    idempotency_key: None,
                };
                msg.append_assistant_text(assistant);
                self.state.messages.push(msg);
                self.state.active_assistant_message =
                    Some(self.state.messages.len().saturating_sub(1));
            }
            if let Some(retry) = live.get("llm_retry") {
                let number = retry
                    .get("retry_number")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let reason = retry
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or("LLM request failed");
                let remaining = retry
                    .get("remaining_seconds")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                self.system_message(format!(
                    "Model retry #{number} — {remaining}s left ({reason})"
                ));
            }
        }
        if let Some(btw) = data.get("pending_btw") {
            let agent_id = btw
                .get("agent_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let streaming = btw
                .get("streaming")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let question = btw
                .get("question")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let answer = btw
                .get("answer")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let thinking = btw
                .get("thinking")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let retry_status = btw
                .get("retry_status")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let turns = btw
                .get("turns")
                .and_then(|v| v.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| {
                            Some(crate::panes::BtwTurnView {
                                question: item.get("question")?.as_str()?.to_string(),
                                answer: item.get("answer")?.as_str()?.to_string(),
                                thinking: None,
                            })
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if streaming || !turns.is_empty() || !question.is_empty() {
                self.state.btw.open = true;
                self.state.btw.owner_session_id = Some(sid.clone());
                self.state.btw.current_session_id = Some(sid.clone());
                self.state.btw.current_agent_id = if agent_id.is_empty() {
                    None
                } else {
                    Some(agent_id)
                };
                self.state.btw.streaming = streaming;
                self.state.btw.current_question = question;
                self.state.btw.current_answer = answer;
                self.state.btw.current_thinking = thinking;
                self.state.btw.retry_status = retry_status;
                self.state.btw.turns = turns;
                self.state.btw.scroll_offset = 0;
                self.state.btw.error = None;
                // Keep the main transcript visible; Ctrl+G still toggles BTW.
                // Auto-open only when a stream is live so the answer isn't lost.
                if streaming {
                    self.state.mode = AppMode::Btw;
                }
            }
        }
        if let Some(queue) = data.get("prompt_queue") {
            let items = queue
                .get("items")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|item| {
                            let text = item.get("text")?.as_str()?.to_string();
                            let id = item
                                .get("id")
                                .and_then(|v| v.as_str())
                                .map(str::to_string)
                                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                            let images = item
                                .get("images")
                                .and_then(|v| v.as_array())
                                .map(|imgs| {
                                    imgs.iter()
                                        .filter_map(|img| {
                                            let media_type = img
                                                .get("media_type")
                                                .or_else(|| img.get("mime_type"))
                                                .and_then(|v| v.as_str())?
                                                .to_string();
                                            let data = img
                                                .get("data")
                                                .and_then(|v| v.as_str())?
                                                .to_string();
                                            Some((media_type, data))
                                        })
                                        .collect::<Vec<_>>()
                                })
                                .unwrap_or_default();
                            let as_steer = item
                                .get("as_steer")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false);
                            Some(crate::prompt_queue::QueuedPrompt {
                                id,
                                session_id: sid.clone(),
                                text,
                                images,
                                as_steer,
                            })
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let selected = queue.get("selected").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            self.state.prompt_queue = crate::prompt_queue::PromptQueue {
                selected: if items.is_empty() {
                    0
                } else {
                    selected.min(items.len() - 1)
                },
                items: items.clone(),
            };
            for item in &items {
                let already = self.state.messages.iter().any(|message| {
                    message.role == MessageRole::User
                        && message.delivery == crate::prompt_queue::DeliveryState::Queued
                        && message.content == item.text
                });
                if !already {
                    self.state.messages.push(DisplayMessage {
                        role: MessageRole::User,
                        content: item.text.clone(),
                        thinking: None,
                        parts: Vec::new(),
                        tool_calls: Vec::new(),
                        delivery: crate::prompt_queue::DeliveryState::Queued,
                        idempotency_key: None,
                    });
                }
            }
            if !items.is_empty() {
                self.system_message(format!(
                    "Restored {} queued prompt(s) from before disconnect.",
                    items.len()
                ));
            }
        }
        if let Some(subagents) = data.get("pending_subagents").and_then(|v| v.as_array()) {
            for item in subagents {
                let id = item
                    .get("subagent_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?")
                    .to_string();
                let name = item
                    .get("subagent_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("subagent")
                    .to_string();
                let status = item
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let desc = item
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                self.state
                    .subagents
                    .upsert_spawned(id.clone(), name, desc, status);
                if let Some(detail) = item.get("detail").and_then(|v| v.as_str()) {
                    if !detail.is_empty() {
                        self.state
                            .subagents
                            .set_status(&id, status, Some(detail.to_string()));
                    }
                }
                if let Some(children) = item.get("recent_child_events").and_then(|v| v.as_array()) {
                    for child in children.iter().rev().take(12).rev() {
                        if let Some(text) = child.as_str() {
                            self.state.subagents.note_child_event(&id, text.to_string());
                        }
                    }
                }
            }
        }
        if let Some(todos) = data.get("todos").and_then(|value| value.as_array()) {
            self.state.todos = todos
                .iter()
                .filter_map(|item| {
                    Some(TodoItem {
                        id: item.get("id")?.as_str()?.to_string(),
                        content: item.get("content")?.as_str()?.to_string(),
                        status: item.get("status")?.as_str()?.to_string(),
                    })
                })
                .collect();
            if self.state.todos.is_empty() || all_todos_finished(&self.state.todos) {
                self.state.todos_expanded = false;
            }
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
                steps: usage.get("steps").and_then(|v| v.as_u64()).unwrap_or(0),
                turns: usage.get("turns").and_then(|v| v.as_u64()).unwrap_or(0),
                input_includes_cache: usage.get("input_includes_cache").and_then(|v| v.as_bool()),
            };
            if let Some(ctx) = usage.get("context") {
                if let Ok(info) =
                    serde_json::from_value::<kkagent_protocol::ContextBreakdownInfo>(ctx.clone())
                {
                    self.state.context_breakdown = Some(info);
                }
            }
            // `approx_tokens` reflects the *current context size* (most recent
            // single LLM call), NOT the session-running total. Prefer the
            // dedicated `last_step_usage` field; fall back to `usage` only for
            // older servers that don't send it. Cache tokens are folded in via
            // the provider-aware helper so long cached sessions show their real
            // context size (Anthropic excludes cache from `input_tokens`).
            let step = data.get("last_step_usage");
            let step_field = |name: &str, fallback: u64| {
                step.and_then(|u| u.get(name))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(fallback)
            };
            let step_usage = kkagent_protocol::TokenUsage {
                input_tokens: step_field("input_tokens", input),
                output_tokens: step_field("output_tokens", output),
                cache_creation_input_tokens: step_field("cache_creation_tokens", 0),
                cache_read_input_tokens: step_field("cache_read_tokens", 0),
                // Server JSON may carry the explicit provider flag; otherwise
                // `total_input_tokens` falls back to its heuristic.
                input_includes_cache: step
                    .and_then(|u| u.get("input_includes_cache"))
                    .and_then(|v| v.as_bool()),
            };
            self.state.approx_tokens = step_usage.context_size();
            self.state.status_bar.cache_hit = step_usage.cache_hit_ratio();
            self.state.last_step_usage = Some(step_usage);
        }
        if let Some(plan) = data.get("plan_mode").and_then(|v| v.as_bool()) {
            self.state.on_plan_mode_changed(plan);
        }
        if let Some(plan) = plan_document_from_resume(&data) {
            self.state.apply_plan_document(plan.path, plan.content);
        } else if let Some(plan_msg) = self
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
        // Resuming a session should land on the normal transcript, not the
        // plan-document overlay — unless the agent submitted a plan for
        // review (ExitPlanMode approval) and is waiting for the user to
        // decide, in which case the plan document must stay visible.
        let needs_plan_review = self
            .state
            .approval_pending
            .as_ref()
            .is_some_and(|a| a.is_plan_review);
        if self.state.plan_mode && !needs_plan_review {
            self.state.dismiss_plan_focus();
        }

        if !self.state.open_session_group.iter().any(|id| id == &sid) {
            self.state.open_session_group.clear();
        }

        self.state.status_bar.session_id = Some(sid.clone());
        // Prefer a title we already know (e.g. from a previous `/title`), so
        // resuming a session does not temporarily revert the indicator to the
        // first prompt. The workspace-sessions refresh will reconcile with the
        // authoritative server state.
        let existing_title = self
            .state
            .tab_strip
            .tabs
            .iter()
            .find(|t| t.id == sid)
            .map(|t| t.title.clone())
            .filter(|t| !t.is_empty() && t != "main" && t != "session");
        let tab_title = if let Some(t) = existing_title {
            t
        } else {
            data.get("first_prompt")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .map(|s| s.to_string())
                .unwrap_or_else(|| "session".to_string())
        };
        self.state.tab_strip.ensure_active(&sid, &tab_title);
        self.state.list_picker = None;
        self.state.session_picker_preview = None;
        self.enqueue_workspace_sessions_refresh();

        // Restore UI context captured when we last left this session.
        self.restore_session_view(&sid);

        if self
            .state
            .pending_resume_prefill
            .as_ref()
            .is_some_and(|(target, _)| target == &sid)
        {
            if let Some((_, text)) = self.state.pending_resume_prefill.take() {
                self.state.input.set_text(text);
                self.state.history_index = None;
                self.state.history_draft.clear();
                self.system_message(
                    "Forked before the selected turn. Edit the restored prompt and press Enter; the original session is preserved."
                        .into(),
                );
            }
        }

        // Lazy history: show recent messages first, then backfill older pages
        // without forcing the viewport to the bottom.
        if let Some(hist) = data.get("history") {
            let total = hist.get("total").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let oldest = hist
                .get("oldest_index")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;
            self.state.history_total = Some(total);
            self.state.history_oldest_index = Some(oldest);
            self.state.history_loading = false;
        }
        if turn_active {
            self.replay_background_session_events(&sid);
        } else {
            self.drop_background_session_events(&sid);
        }
        self.sync_active_session_status();
        // Turn may have finished while detached; drain restored queue immediately.
        self.flush_prompt_queue_if_idle();
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
            // The render-time scroll-anchor compensator (render_messages)
            // measures total content growth and bumps scroll_up to match.
            // That logic exists for *bottom* growth (streaming output), but
            // here we just grew the *top*. The bump above already accounts
            // for the new top lines, so skip the next-frame compensation to
            // avoid double-counting.
            self.state.prev_content_lines = None;
        }

        let mut merged = older;
        merged.append(&mut self.state.messages);
        self.state.messages = merged;
        self.state.apply_tool_output_mode();
        self.state.active_assistant_message = self
            .state
            .active_assistant_message
            .map(|index| index.saturating_add(added));

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
        self.state.history_loading = false;
        if !older_available {
            self.state.history_oldest_index = Some(0);
        }
    }

    async fn discard_session_record(&mut self, session_id: &str) -> anyhow::Result<()> {
        let params = serde_json::json!({"session_id": session_id});
        let _ = self.client.rpc_call("sessions.delete", Some(params)).await;
        self.state.tab_strip.tabs.retain(|t| t.id != session_id);
        self.state.session_views.remove(session_id);
        self.state.session_runtime_states.remove(session_id);
        self.drop_background_session_events(session_id);
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
            return;
        };

        let mut rows: Vec<(String, String, Option<String>)> = Vec::new();
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
                let is_open = self.state.tab_strip.tabs.iter().any(|tab| tab.id == id);
                if empty && id != current_id && !is_open {
                    continue;
                }
                // Tabs closed in this window (Ctrl-D) stay closed: drop rows for
                // them so a stale refresh cannot resurrect the indicator.
                if self.state.closed_tab_ids.contains(&id) {
                    continue;
                }
                let title = crate::chrome::session_display_title(
                    s.get("title").and_then(|v| v.as_str()),
                    s.get("is_custom_title")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                    s.get("first_prompt").and_then(|v| v.as_str()),
                    &id,
                );
                let working_dir = s
                    .get("working_dir")
                    .and_then(|v| v.as_str())
                    .filter(|dir| !dir.is_empty())
                    .map(|dir| dir.to_string());
                rows.push((id, title, working_dir));
            }
        }

        // Ensure current session is in the graph even if list is momentarily stale.
        // Skip when the current id was just closed (Ctrl-D) and the follow-up
        // session switch has not landed yet — it must not re-enter the strip.
        let current_open = !self.state.closed_tab_ids.contains(&current_id);
        if current_open && !rows.iter().any(|(id, _, _)| id == &current_id) {
            // Prefer the tab title we already know (e.g. a `/title` set earlier),
            // so a transient missing-row does not revert to the first prompt.
            let existing_title = self
                .state
                .tab_strip
                .tabs
                .iter()
                .find(|t| t.id == current_id)
                .map(|t| t.title.clone())
                .filter(|t| !t.is_empty() && t != "main" && t != "session");
            let title = if let Some(t) = existing_title {
                t
            } else {
                let first_prompt = self.state.messages.iter().find_map(|m| {
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
                });
                crate::chrome::session_display_title(
                    None,
                    false,
                    first_prompt.as_deref(),
                    &current_id,
                )
            };
            rows.push((current_id.clone(), title, None));
        }

        // The footer is an open-tab strip: refreshes may update metadata and
        // status, but never close an indicator. Ctrl-D is the only close path.
        let mut ids = self
            .state
            .tab_strip
            .tabs
            .iter()
            .map(|tab| tab.id.clone())
            .filter(|id| !self.state.closed_tab_ids.contains(id))
            .collect::<Vec<_>>();
        if current_open && !ids.contains(&current_id) {
            ids.insert(0, current_id.clone());
        }

        let entries: Vec<crate::chrome::WorkspaceSessionEntry> = ids
            .into_iter()
            .map(|id| {
                let tab = self.state.tab_strip.tabs.iter().find(|t| t.id == id);
                let status = tab
                    .map(|t| t.status)
                    .or_else(|| {
                        self.state
                            .session_runtime_states
                            .get(&id)
                            .map(|runtime| runtime.status)
                    })
                    .unwrap_or(SessionStatus::Idle);
                let dirty = tab.map(|t| t.dirty).unwrap_or(false);
                let needs_attention = self.state.parked_approvals.contains_key(&id)
                    || self.state.parked_questions.contains_key(&id);
                if let Some((_, title, working_dir)) = rows.iter().find(|(rid, _, _)| rid == &id) {
                    crate::chrome::WorkspaceSessionEntry {
                        id,
                        title: title.clone(),
                        status,
                        dirty,
                        needs_attention,
                        working_dir: working_dir.clone(),
                    }
                } else {
                    let title = tab
                        .map(|tab| tab.title.clone())
                        .filter(|title| !title.is_empty())
                        .unwrap_or_else(|| {
                            if id == current_id {
                                "session".into()
                            } else {
                                id[..id.len().min(8)].to_string()
                            }
                        });
                    crate::chrome::WorkspaceSessionEntry {
                        id,
                        title,
                        status,
                        dirty,
                        needs_attention,
                        working_dir: None,
                    }
                }
            })
            .collect();

        self.state.workspace_sessions.set_entries_stable(
            entries,
            if current_open {
                Some(current_id.as_str())
            } else {
                None
            },
        );

        for e in self.state.workspace_sessions.entries.iter() {
            self.state.tab_strip.ensure_tab(&e.id, e.title.clone());
        }
        if current_open {
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
                        .map(|(_, title, _)| title.clone())
                })
                .unwrap_or_else(|| "main".into());
            self.state.tab_strip.ensure_active(&current_id, title);
        }
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
        self.activate_workspace_target(&next_id).await
    }

    fn enter_btw_view(&mut self) {
        // Stack a visible approval/plan-review modal: hide it while BTW owns
        // the surface so it is neither drawn nor swallowing keystrokes. We
        // remember that *we* hid it so `exit_btw_view` can restore it — a
        // modal the user already folded with Esc stays folded.
        if let Some(approval) = self.state.approval_pending.as_mut() {
            if !approval.hidden {
                approval.hidden = true;
                self.state.btw_hid_approval = true;
            }
        }
        self.state.mode = AppMode::Btw;
        self.state.btw.open = true;
        self.state.btw.scroll_offset = 0;
        self.clear_selection();
    }

    fn exit_btw_view(&mut self) {
        // Restore an approval modal that BTW hid on entry (but not one the
        // user folded themselves with Esc).
        if self.state.btw_hid_approval {
            if let Some(approval) = self.state.approval_pending.as_mut() {
                approval.hidden = false;
            }
            self.state.btw_hid_approval = false;
        }
        self.state.mode = if self.state.plan_mode {
            AppMode::Plan
        } else {
            AppMode::Normal
        };
        self.state.btw.open = false;
        if let Some(session_id) = self.state.session_id.as_deref() {
            self.state.workspace_sessions.active = self
                .state
                .workspace_sessions
                .entries
                .iter()
                .position(|entry| entry.id == session_id)
                .unwrap_or(0);
        }
    }

    async fn delete_btw_workspace(&mut self) {
        let owner_session_id = self.state.btw.owner_session_id.clone();
        self.state.btw = crate::panes::BtwPanelState::default();
        self.exit_btw_view();
        if let Some(session_id) = owner_session_id {
            if let Err(error) = self.client.delete_btw(&session_id).await {
                self.system_message(format!("Failed to delete BTW session: {error}"));
            }
        }
    }

    async fn activate_workspace_target(&mut self, id: &str) -> anyhow::Result<()> {
        if self.state.mode == AppMode::Btw {
            // BTW is not a session tab. Keep it visible until Ctrl-G (or the
            // explicit Ctrl-D delete action) so clicking the real-session
            // strip cannot accidentally dismiss the side conversation.
            return Ok(());
        }
        if self.state.session_id.as_deref() == Some(id) {
            return Ok(());
        }
        // The strip may surface sessions from other workspaces; pass the
        // session's own working directory so the server accepts the switch
        // instead of rejecting it with a directory-mismatch error.
        let working_dir = self
            .state
            .workspace_sessions
            .entries
            .iter()
            .find(|entry| entry.id == id)
            .and_then(|entry| entry.working_dir.clone());
        self.resume_session_in_workspace(id, working_dir.as_deref())
            .await
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
                    if (e.needs_attention || e.dirty) && current.as_deref() != Some(e.id.as_str()) {
                        Some((e.id.clone(), e.working_dir.clone()))
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
                        Some((tab.id.clone(), None))
                    } else {
                        None
                    }
                })
            }
        };
        if let Some((id, working_dir)) = target {
            return self
                .resume_session_in_workspace(&id, working_dir.as_deref())
                .await;
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

    fn flush_file_complete_debounce(&mut self) {
        let Some(pending) = self.state.file_complete_debounce.as_ref() else {
            return;
        };
        if std::time::Instant::now() < pending.due_at {
            return;
        }
        let token_start = pending.token_start;
        let query = pending.query.clone();
        let quoted = pending.quoted;
        self.state.file_complete_debounce = None;

        // Cursor may have left the `@` token while we waited.
        let text = self.state.input.text.clone();
        let cursor = self.state.input.cursor.min(text.len());
        let Some((live_start, live_query)) =
            crate::pi::autocomplete::extract_at_token(&text, cursor)
        else {
            self.state.file_menu = None;
            return;
        };
        if live_start != token_start || live_query != query {
            return;
        }

        let mut tools = self.config.tools.clone();
        tools.merge_project_overrides(&self.state.working_dir);
        self.jobs.spawn_file_complete(
            self.state.working_dir.clone(),
            tools.effective_heavy_dirs(),
            self.config.ui.experimental_smart_at_complete,
            token_start,
            query,
            quoted,
        );
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
            self.state
                .workspace_sessions
                .entries
                .retain(|entry| entry.id != deleted_id);
            // Tombstone the closed tab so an in-flight/stale `sessions.list`
            // response cannot resurrect it (state.session_id still points at
            // it until the follow-up switch lands).
            self.state.closed_tab_ids.insert(deleted_id.clone());
            if self.state.workspace_sessions.active >= self.state.workspace_sessions.entries.len() {
                self.state.workspace_sessions.active = self
                    .state
                    .workspace_sessions
                    .entries
                    .len()
                    .saturating_sub(1);
            }
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
                    let cwd = self.state.working_dir.to_string_lossy().into_owned();
                    let session_id = self
                        .client
                        .create_session(Some(&cwd), Some(self.state.permission_mode))
                        .await?;
                    self.state.messages.clear();
                    self.state.active_assistant_message = None;
                    self.state.todos.clear();
                    self.state.subagents = crate::subagents::SubagentStore::default();
                    self.state.subagents_panel = None;
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
                // Tombstone the deleted tab against stale `sessions.list`
                // responses racing the follow-up switch.
                self.state.closed_tab_ids.insert(deleted_id.clone());
                self.state.parked_approvals.remove(&deleted_id);
                self.state.parked_questions.remove(&deleted_id);
                self.state.session_views.remove(&deleted_id);
                self.state.session_runtime_states.remove(&deleted_id);
                self.drop_background_session_events(&deleted_id);
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
                        let cwd = self.state.working_dir.to_string_lossy().into_owned();
                        let session_id = self
                            .client
                            .create_session(Some(&cwd), Some(self.state.permission_mode))
                            .await?;
                        self.state.messages.clear();
                        self.state.active_assistant_message = None;
                        self.state.todos.clear();
                        self.state.subagents = crate::subagents::SubagentStore::default();
                        self.state.subagents_panel = None;
                        self.state.status = SessionStatus::Idle;
                        self.state.approval_pending = None;
                        self.state.question_pending = None;
                        self.state.session_id = Some(session_id.clone());
                        self.state.status_bar.session_id = Some(session_id.clone());
                        self.state.tab_strip.ensure_active(&session_id, "main");
                        self.bind_config_default_model();
                    }
                    // The active-session marker may still point at the deleted
                    // session; refresh it to the new current session (or clear it).
                    self.persist_active_session_marker();
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
        if self.state.mode == AppMode::Btw {
            return self.submit_btw_input().await;
        }
        self.submit_input_with_delivery().await
    }

    async fn submit_btw_input(&mut self) -> anyhow::Result<()> {
        let raw = self.state.input.take();
        let question = self.state.input.expand_pastes(&raw).trim().to_string();
        if question.is_empty() {
            return Ok(());
        }
        self.state.slash_menu = None;
        self.state.file_menu = None;
        self.state.list_picker = None;
        self.state.push_input_history(&question);

        if let Some(args) = question
            .strip_prefix("/btw")
            .filter(|args| args.is_empty() || args.starts_with(char::is_whitespace))
            .map(str::trim)
        {
            if args.is_empty() {
                return Ok(());
            }
            let Some(session_id) = self.state.session_id.clone() else {
                self.state.input.set_text(question);
                self.state.btw.error = Some("No active session for BTW.".into());
                return Ok(());
            };
            self.replace_btw_question(session_id, args.to_string())
                .await;
            return Ok(());
        }

        let Some(session_id) = self
            .state
            .btw
            .owner_session_id
            .clone()
            .or_else(|| self.state.session_id.clone())
        else {
            self.state.input.set_text(question);
            self.state.btw.error = Some("No active session for BTW.".into());
            return Ok(());
        };
        if self.state.btw.streaming {
            self.state.btw.enqueue(session_id, question);
            return Ok(());
        }
        self.start_btw_question(session_id, question).await;
        Ok(())
    }

    async fn start_btw_question(&mut self, session_id: String, question: String) {
        self.state.btw.owner_session_id = Some(session_id.clone());
        self.state.btw.begin_question(&question);
        self.state.btw.current_session_id = Some(session_id.clone());
        match self.client.start_btw(&session_id, &question).await {
            Ok(agent_id) => self.state.btw.current_agent_id = Some(agent_id),
            Err(error) => self.state.btw.finish(Some(error.to_string())),
        }
    }

    async fn start_next_btw_question(&mut self) {
        if let Some(next) = self.state.btw.take_next() {
            self.start_btw_question(next.session_id, next.question)
                .await;
        }
    }

    async fn replace_btw_question(&mut self, session_id: String, question: String) {
        let previous_owner = self.state.btw.owner_session_id.clone();
        self.state.btw = crate::panes::BtwPanelState::default();
        if let Some(owner) = previous_owner {
            if let Err(error) = self.client.delete_btw(&owner).await {
                self.state.btw.error = Some(format!("Failed to clear previous BTW: {error}"));
                return;
            }
        }
        self.start_btw_question(session_id, question).await;
    }

    async fn submit_steer_input(&mut self) -> anyhow::Result<()> {
        let raw = self.state.input.take();
        let draft = self.state.input.expand_pastes(&raw);
        self.state.slash_menu = None;
        self.state.file_menu = None;
        self.state.list_picker = None;

        let Some(session_id) = self.state.session_id.clone() else {
            self.state.input.set_text(draft);
            self.system_message("No active session.".into());
            return Ok(());
        };

        // Match Kimi's steer behavior: queued user prompts are injected before
        // the current editor draft, preserving their original order. Prompts
        // for another session are retained defensively (normally each runtime
        // state already owns an isolated queue).
        let mut queued_for_session = Vec::new();
        self.state.prompt_queue.items.retain(|item| {
            if item.session_id == session_id {
                queued_for_session.push(item.clone());
                false
            } else {
                true
            }
        });
        if self.state.prompt_queue.items.is_empty() {
            self.state.prompt_queue.selected = 0;
        } else {
            self.state.prompt_queue.selected = self
                .state
                .prompt_queue
                .selected
                .min(self.state.prompt_queue.items.len() - 1);
        }

        let mut text_items = queued_for_session
            .iter()
            .map(|item| item.text.trim())
            .filter(|text| !text.is_empty())
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();
        let draft = draft.trim().to_string();
        if !draft.is_empty() {
            text_items.push(draft.clone());
        }
        if text_items.is_empty() {
            return Ok(());
        }
        let text = text_items.join("\n\n");
        let images = queued_for_session
            .iter()
            .flat_map(|item| item.images.iter().cloned())
            .collect::<Vec<_>>();
        let idem = uuid::Uuid::new_v4().to_string();

        // Queued messages are already visible in the transcript. Promote all
        // of them to the same in-flight steer request, then append the fresh
        // editor draft (if any) as its own user entry.
        for item in &queued_for_session {
            if let Some(message) = self.state.messages.iter_mut().find(|message| {
                message.role == MessageRole::User
                    && message.delivery == crate::prompt_queue::DeliveryState::Queued
                    && message.content == item.text
            }) {
                message.delivery = crate::prompt_queue::DeliveryState::Sending;
                message.idempotency_key = Some(idem.clone());
            }
        }
        if !draft.is_empty() {
            self.state.push_input_history(&draft);
            self.state.messages.push(DisplayMessage {
                role: MessageRole::User,
                content: draft,
                thinking: None,
                parts: Vec::new(),
                tool_calls: Vec::new(),
                delivery: crate::prompt_queue::DeliveryState::Sending,
                idempotency_key: Some(idem.clone()),
            });
        }
        if let Some(warn) = crate::draft_store::redact_sensitive_preview(&text) {
            self.system_message(warn);
        }
        self.state.scroll_up = 0;
        self.state.follow_bottom = true;
        self.jobs.spawn_steer(
            self.client.requester(),
            session_id.clone(),
            text,
            images,
            idem,
        );
        crate::draft_store::clear_draft(&session_id);
        self.enqueue_prompt_queue_sync();
        self.system_message("Steer sent — applying at the next model step.".into());
        Ok(())
    }

    fn can_steer_current_turn(&self) -> bool {
        matches!(
            self.state.status,
            SessionStatus::Thinking
                | SessionStatus::ToolExecuting
                | SessionStatus::WaitingApproval
                | SessionStatus::WaitingQuestion
                | SessionStatus::Compacting
        )
    }

    async fn submit_input_with_delivery(&mut self) -> anyhow::Result<()> {
        let raw = self.state.input.take();
        if raw.is_empty() {
            return Ok(());
        }
        // Clear the persisted draft — input has been submitted and must not
        // resurface when switching back to this session later (e.g. `/new`).
        if let Some(sid) = self.state.session_id.clone() {
            crate::draft_store::clear_draft(&sid);
        }
        // Expand kimi-style `[Pasted text #n]` markers before send / display.
        let text = self.state.input.expand_pastes(&raw);
        self.state.slash_menu = None;
        self.state.file_menu = None;
        self.state.list_picker = None;

        if text.starts_with('/') {
            return self.handle_slash_command(&text).await;
        }

        // Local shell: `!cmd` or Shell mode — never send to the agent loop.
        let shell_cmd = if self.state.mode == AppMode::Shell {
            Some(text.trim().to_string())
        } else {
            text.strip_prefix('!')
                .map(|rest| rest.trim_start().to_string())
        };
        if let Some(cmd) = shell_cmd {
            if cmd.is_empty() {
                return Ok(());
            }
            return self.submit_local_shell(cmd);
        }

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
            let Some(session_id) = self.state.session_id.clone() else {
                self.state.input.set_text(text);
                self.system_message("No active session.".into());
                return Ok(());
            };
            self.state
                .prompt_queue
                .push(crate::prompt_queue::QueuedPrompt::next_turn(
                    session_id,
                    text.clone(),
                ));
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
            self.enqueue_prompt_queue_sync();
            return Ok(());
        }

        let idem = uuid::Uuid::new_v4().to_string();
        if let Some(warn) = crate::draft_store::redact_sensitive_preview(&text) {
            self.system_message(warn);
        }
        self.state.messages.push(DisplayMessage {
            role: MessageRole::User,
            content: text.clone(),
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
            self.jobs
                .spawn_prompt(self.client.requester(), sid.clone(), text, Vec::new(), idem);
            crate::draft_store::clear_draft(&sid);
        } else {
            self.state.status = SessionStatus::Idle;
            if let Some(msg) = self.state.messages.last_mut() {
                msg.delivery = crate::prompt_queue::DeliveryState::Failed;
            }
            self.system_message("No active session.".into());
        }

        Ok(())
    }

    /// Execute `!cmd` / shell-mode input locally without involving the LLM.
    fn submit_local_shell(&mut self, cmd: String) -> anyhow::Result<()> {
        self.state.push_input_history(&cmd);
        // Stay in shell mode so consecutive commands are easy (Esc / empty Backspace exits).
        let display = format!("!{cmd}");
        self.state.messages.push(DisplayMessage {
            role: MessageRole::User,
            content: display,
            thinking: None,
            parts: Vec::new(),
            tool_calls: Vec::new(),
            delivery: crate::prompt_queue::DeliveryState::Sent,
            idempotency_key: None,
        });
        self.state.follow_bottom = true;
        self.state.scroll_up = 0;
        self.jobs
            .spawn_local_shell(cmd, self.state.working_dir.clone());
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
            self.state.prompt_queue.items.insert(0, item);
            return;
        };
        if item.session_id != sid {
            self.state.prompt_queue.items.insert(0, item);
            return;
        }
        let idem = uuid::Uuid::new_v4().to_string();
        if let Some(msg) = self.state.messages.iter_mut().rev().find(|m| {
            m.role == MessageRole::User
                && m.delivery == crate::prompt_queue::DeliveryState::Queued
                && m.content == item.text
        }) {
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
        self.jobs
            .spawn_prompt(self.client.requester(), sid, item.text, item.images, idem);
        self.enqueue_prompt_queue_sync();
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
                let clear = args.eq_ignore_ascii_case("clear");
                let enabled = if clear { false } else { !self.state.plan_mode };
                if self.set_plan_mode_from_ui(enabled).await {
                    if clear {
                        self.state.plan_document = None;
                        self.state.messages.retain(|m| m.role != MessageRole::Plan);
                    }
                    if enabled {
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
            }
            "exit" | "quit" | "q" => {
                self.sync_prompt_queue().await;
                self.persist_active_session_marker();
                self.state.should_quit = true;
            }
            "new" | "clear" => {
                let prev = self.state.session_id.clone();
                let prev_status = self.state.status;
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
                self.state.active_assistant_message = None;
                self.state.todos.clear();
                self.state.subagents = crate::subagents::SubagentStore::default();
                self.state.subagents_panel = None;
                self.state.todos_expanded = false;
                self.state.thinking_text.clear();
                self.state.plan_document = None;
                self.state.plan_scroll_to_top = false;
                self.state.status = SessionStatus::Idle;
                self.state.turn_started_at = None;
                self.state.reset_context_usage_stats();
                self.state.render_cache.invalidate_all();
                self.state.transcript_layout_cache.invalidate();
                let cwd = self.state.working_dir.to_string_lossy().into_owned();
                let session_id = self
                    .client
                    .create_session(Some(&cwd), Some(self.state.permission_mode))
                    .await?;
                self.bind_config_default_model();
                if self.state.plan_mode {
                    let _ = self.client.set_plan_mode(&session_id, true).await;
                }
                if prev.as_ref().is_some_and(|p| p != &session_id) {
                    if let Some(ref p) = prev {
                        self.link_open_sessions(p, &session_id);
                        self.state.tab_strip.ensure_tab(p, "background");
                        self.state.tab_strip.set_status(p, prev_status);
                    }
                    self.state.tab_strip.ensure_active(&session_id, "new");
                    self.system_message(
                        "New session started. Previous session remains open — Tab / ←→ to switch."
                            .into(),
                    );
                } else {
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
                if args.trim().is_empty() {
                    self.open_history_edit_picker().await?;
                } else if let Ok(count) = args.trim().parse::<usize>() {
                    self.undo_turns(count.max(1)).await?;
                } else {
                    self.system_message("Usage: /undo [turn_count]".into());
                }
            }
            "timeline" | "tl" => {
                let mut lines = Vec::new();
                let status = format!("{:?}", self.state.status);
                lines.push(format!("status: {status}"));
                if let Some(tool) = &self.state.last_tool_name {
                    lines.push(format!("last tool: {tool}"));
                }
                let mut tool_n = 0u32;
                let mut err_n = 0u32;
                for msg in &self.state.messages {
                    for part in &msg.parts {
                        if let DisplayPart::Tool(tc) = part {
                            tool_n += 1;
                            if tc.is_error {
                                err_n += 1;
                            }
                            if tc.output.is_none() {
                                let secs =
                                    tc.started_at.map(|t| t.elapsed().as_secs()).unwrap_or(0);
                                if let Some(behind) = &tc.queued_behind {
                                    lines.push(format!("queued: {} behind {behind}", tc.name));
                                } else {
                                    lines.push(format!(
                                        "running: {} ({secs}s){}",
                                        tc.name,
                                        if tc.stopping { " stopping…" } else { "" }
                                    ));
                                }
                            }
                        }
                    }
                }
                lines.push(format!("tools seen: {tool_n} · errors: {err_n}"));
                lines.push(format!("approx tokens: {}", self.state.approx_tokens));
                self.system_message(lines.join(" · "));
            }
            "edit" | "rerun" => {
                let fork = args.trim() == "fork";
                let last_user = self
                    .state
                    .messages
                    .iter()
                    .rev()
                    .find(|m| m.role == MessageRole::User)
                    .map(|m| m.content.clone());
                if let Some(text) = last_user {
                    self.state.input.set_text(text);
                    if fork {
                        self.system_message(
                            "Loaded last prompt — submit to continue; use /fork first to branch history".into(),
                        );
                    } else {
                        self.system_message(
                            "Loaded last prompt for edit & re-run (does not auto-truncate history; /undo then submit, or /fork)".into(),
                        );
                    }
                } else {
                    self.system_message("No prior user prompt to edit".into());
                }
            }
            "model" => {
                if args.is_empty() {
                    self.begin_root_picker();
                    self.open_model_picker();
                } else if self.config.resolve_model(&args).is_some() {
                    self.apply_model_selection(args.clone()).await;
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
                let Some(session_id) = self.state.session_id.clone() else {
                    self.system_message("No active session.".into());
                    return Ok(());
                };
                let mut parts = args.splitn(2, char::is_whitespace);
                let sub = parts.next().unwrap_or("").trim();
                let rest = parts.next().unwrap_or("").trim();
                match sub {
                    "" | "status" => {
                        match self
                            .client
                            .rpc_call(
                                "session.goal",
                                Some(serde_json::json!({
                                    "session_id": session_id,
                                    "action": "status",
                                })),
                            )
                            .await
                        {
                            Ok(body) => {
                                if body.get("goal").and_then(|g| g.as_object()).is_none() {
                                    self.system_message("No active goal.".into());
                                } else {
                                    self.system_message(
                                        serde_json::to_string_pretty(&body).unwrap_or_default(),
                                    );
                                }
                            }
                            Err(e) => self.system_message(format!("Goal status failed: {e}")),
                        }
                    }
                    "pause" | "resume" | "cancel" => {
                        match self
                            .client
                            .rpc_call(
                                "session.goal",
                                Some(serde_json::json!({
                                    "session_id": session_id,
                                    "action": sub,
                                })),
                            )
                            .await
                        {
                            Ok(body) => self.system_message(format!(
                                "Goal {sub}: {}",
                                serde_json::to_string_pretty(&body).unwrap_or_default()
                            )),
                            Err(e) => self.system_message(format!("Goal {sub} failed: {e}")),
                        }
                    }
                    "replace" => {
                        if rest.is_empty() {
                            self.system_message("Usage: /goal replace <objective>".into());
                        } else {
                            match self
                                .client
                                .rpc_call(
                                    "session.goal",
                                    Some(serde_json::json!({
                                        "session_id": session_id,
                                        "action": "replace",
                                        "objective": rest,
                                    })),
                                )
                                .await
                            {
                                Ok(_) => self.system_message(format!("Goal replaced: {rest}")),
                                Err(e) => self.system_message(format!("Goal replace failed: {e}")),
                            }
                        }
                    }
                    _ => {
                        let objective = if rest.is_empty() { sub } else { args.trim() };
                        match self
                            .client
                            .rpc_call(
                                "session.goal",
                                Some(serde_json::json!({
                                    "session_id": session_id,
                                    "action": "create",
                                    "objective": objective,
                                })),
                            )
                            .await
                        {
                            Ok(_) => {
                                self.system_message(format!("Goal started: {objective}"));
                            }
                            Err(e) => self.system_message(format!("Goal start failed: {e}")),
                        }
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
            "context" => {
                self.begin_root_picker();
                self.open_context_picker();
            }
            "changes" => {
                self.begin_root_picker();
                self.open_changes_picker();
            }
            "doctor" => {
                self.begin_root_picker();
                self.open_doctor_picker().await?;
            }
            "mcp" => {
                self.begin_root_picker();
                self.open_mcp_manager().await?;
            }
            "tasks" | "task" | "ps" => {
                self.begin_root_picker();
                self.open_tasks_panel().await?;
            }
            "agents" | "agent" => {
                self.begin_root_picker();
                self.open_agents_panel();
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
                let plugin_args = args.trim();
                if plugin_args == "reload" {
                    match self.client.rpc_call("plugins.reload", None).await {
                        Ok(result) => self.system_message(format!(
                            "Plugins reloaded: {} plugin(s), {} MCP server(s), {} tool(s)",
                            result
                                .get("plugins")
                                .and_then(|value| value.as_u64())
                                .unwrap_or(0),
                            result
                                .get("mcp_servers")
                                .and_then(|value| value.as_u64())
                                .unwrap_or(0),
                            result
                                .get("tools")
                                .and_then(|value| value.as_u64())
                                .unwrap_or(0),
                        )),
                        Err(error) => self.system_message(format!("Plugin reload failed: {error}")),
                    }
                    return Ok(());
                }
                if plugin_args == "list" {
                    self.begin_root_picker();
                    self.open_plugins_picker().await?;
                    return Ok(());
                }
                if plugin_args == "marketplace" || plugin_args.starts_with("marketplace ") {
                    let source = plugin_args
                        .strip_prefix("marketplace")
                        .map(str::trim)
                        .filter(|value| !value.is_empty());
                    let params = source.map(|source| serde_json::json!({"source": source}));
                    match self.client.rpc_call("plugins.marketplace", params).await {
                        Ok(result) => {
                            let catalog_source = result
                                .get("source")
                                .and_then(|value| value.as_str())
                                .unwrap_or("configured marketplace");
                            let mut lines = vec![format!("Plugin marketplace: {catalog_source}")];
                            if let Some(entries) =
                                result.get("plugins").and_then(|value| value.as_array())
                            {
                                for entry in entries {
                                    let id = entry
                                        .get("id")
                                        .and_then(|value| value.as_str())
                                        .unwrap_or("plugin");
                                    let version = entry
                                        .get("version")
                                        .and_then(|value| value.as_str())
                                        .map(|value| format!(" v{value}"))
                                        .unwrap_or_default();
                                    let description = entry
                                        .get("description")
                                        .and_then(|value| value.as_str())
                                        .unwrap_or("");
                                    let status = if entry
                                        .get("updateAvailable")
                                        .and_then(|value| value.as_bool())
                                        .unwrap_or(false)
                                    {
                                        " · update available"
                                    } else if entry
                                        .get("installed")
                                        .and_then(|value| value.as_bool())
                                        .unwrap_or(false)
                                    {
                                        " · installed"
                                    } else {
                                        ""
                                    };
                                    lines.push(format!("- {id}{version}{status}: {description}"));
                                }
                            }
                            lines.push("Install with /plugins install <id>".into());
                            self.system_message(lines.join("\n"));
                        }
                        Err(error) => {
                            self.system_message(format!("Plugin marketplace failed: {error}"))
                        }
                    }
                    return Ok(());
                }
                for (command, method) in [
                    ("install", "plugins.install"),
                    ("update", "plugins.update"),
                    ("enable", "plugins.enable"),
                    ("disable", "plugins.disable"),
                    ("remove", "plugins.remove"),
                    ("info", "plugins.info"),
                ] {
                    if let Some(value) = plugin_args
                        .strip_prefix(command)
                        .filter(|rest| rest.chars().next().is_some_and(char::is_whitespace))
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                    {
                        let params = if command == "install" {
                            serde_json::json!({"source": value})
                        } else {
                            serde_json::json!({"id": value})
                        };
                        match self.client.rpc_call(method, Some(params)).await {
                            Ok(result) if command == "info" => {
                                self.system_message(
                                    serde_json::to_string_pretty(&result)
                                        .unwrap_or_else(|_| result.to_string()),
                                );
                            }
                            Ok(result) => {
                                let id = result
                                    .get("plugin")
                                    .and_then(|plugin| plugin.get("id"))
                                    .or_else(|| result.get("id"))
                                    .and_then(|value| value.as_str())
                                    .unwrap_or(value);
                                self.system_message(format!("Plugin {command} succeeded: {id}"));
                            }
                            Err(error) => {
                                self.system_message(format!("Plugin {command} failed: {error}"));
                            }
                        }
                        return Ok(());
                    }
                }
                if !plugin_args.is_empty() {
                    self.system_message(
                        "Usage: /plugins [list|reload|marketplace [source]|install <id-or-source>|update <id>|enable <id>|disable <id>|remove <id>|info <id>]".into(),
                    );
                    return Ok(());
                }
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
                self.reload_config_from_disk().await;
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
                        self.state.working_dir.join(path)
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
                self.enter_btw_view();
                if args.is_empty() {
                    self.state.btw.error = None;
                } else if let Some(sid) = self.state.session_id.clone() {
                    self.replace_btw_question(sid, args).await;
                } else {
                    self.state.btw.error = Some("No active session for BTW.".into());
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
                    self.refresh_search_hits().await;
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

    async fn handle_search_key(&mut self, key: KeyEvent) -> anyhow::Result<()> {
        match key.code {
            KeyCode::Esc => {
                self.state.search.close();
                self.state.highlight_message = None;
            }
            KeyCode::Enter => {
                if let Some(hit) = self.state.search.current().cloned() {
                    if let Some(session_id) = hit.session_id.clone() {
                        self.state.search.close();
                        self.state.highlight_message = None;
                        self.resume_session(&session_id).await?;
                        self.system_message(format!(
                            "Opened session from search · {}",
                            hit.title.as_deref().unwrap_or(session_id.as_str())
                        ));
                    } else {
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
            }
            KeyCode::Tab if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.state.search.toggle_scope();
                self.refresh_search_hits().await;
            }
            KeyCode::Up | KeyCode::BackTab => {
                self.state.search.prev();
                if let Some(hit) = self.state.search.current() {
                    if hit.session_id.is_none() {
                        self.state.highlight_message = Some(hit.message_index);
                    }
                }
            }
            KeyCode::Down | KeyCode::Tab => {
                self.state.search.next();
                if let Some(hit) = self.state.search.current() {
                    if hit.session_id.is_none() {
                        self.state.highlight_message = Some(hit.message_index);
                    }
                }
            }
            KeyCode::Backspace => {
                self.state.search.query.pop();
                self.refresh_search_hits().await;
            }
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.state.search.query.push(c);
                self.refresh_search_hits().await;
            }
            _ => {}
        }
        Ok(())
    }

    async fn refresh_search_hits(&mut self) {
        use crate::search::{parse_search_query, SearchHit, SearchScope};
        match self.state.search.scope {
            SearchScope::Local => {
                let msgs = self.state.messages.clone();
                self.state.search.recompute(&msgs);
            }
            SearchScope::Global => {
                let (needle, title, tool) = parse_search_query(&self.state.search.query);
                if needle.is_empty() && title.is_none() && tool.is_none() {
                    self.state.search.hits.clear();
                    self.state.search.selected = 0;
                    return;
                }
                let mut params = serde_json::json!({
                    "query": needle,
                    "limit": 40,
                });
                if let Some(title) = title {
                    params["title"] = serde_json::Value::String(title);
                }
                if let Some(tool) = tool {
                    params["tool_name"] = serde_json::Value::String(tool);
                }
                match self.client.rpc_call("sessions.search", Some(params)).await {
                    Ok(value) => {
                        let hits = value
                            .get("hits")
                            .and_then(|v| v.as_array())
                            .into_iter()
                            .flatten()
                            .enumerate()
                            .map(|(i, hit)| SearchHit {
                                message_index: i,
                                preview: hit
                                    .get("preview")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string(),
                                role: hit
                                    .get("role")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("?")
                                    .to_string(),
                                session_id: hit
                                    .get("session_id")
                                    .and_then(|v| v.as_str())
                                    .map(str::to_string),
                                title: hit
                                    .get("title")
                                    .and_then(|v| v.as_str())
                                    .filter(|s| !s.is_empty())
                                    .map(str::to_string),
                            })
                            .collect();
                        self.state.search.apply_global_hits(hits);
                    }
                    Err(error) => {
                        self.state.search.hits.clear();
                        self.state.search.selected = 0;
                        self.system_message(format!("Search failed: {error}"));
                    }
                }
            }
        }
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

    fn update_llm_retry_message(
        &mut self,
        retry_number: u32,
        reason: &str,
        wait_seconds: u64,
        remaining_seconds: u64,
        initial: bool,
    ) {
        let prefix = format!("↻ LLM retry #{retry_number}");
        let reason = reason.trim();
        let content = if remaining_seconds > 0 {
            format!("{prefix} in {remaining_seconds}s (wait {wait_seconds}s) · {reason}")
        } else {
            format!("{prefix} now · {reason}")
        };
        if !initial {
            if let Some(message) = self.state.messages.iter_mut().rev().find(|message| {
                message.role == MessageRole::System && message.content.starts_with(&prefix)
            }) {
                message.content = content;
                self.state.follow_bottom = true;
                self.state.scroll_up = 0;
                return;
            }
        }
        self.system_message(content);
        self.state.follow_bottom = true;
        self.state.scroll_up = 0;
    }

    async fn respond_approval_choice(
        &mut self,
        choice: ApprovalChoice,
        feedback: Option<String>,
    ) -> anyhow::Result<()> {
        let Some(mut approval) = self.state.approval_pending.take() else {
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
        if choice.requires_feedback && feedback.is_none() {
            approval.feedback_mode = true;
            self.state.approval_pending = Some(approval);
            self.jobs.push_info("请先输入修改意见，再按 Enter 提交");
            return Ok(());
        }
        let revising_plan = approval.is_plan_review && choice.requires_feedback;
        let resumed_plan_review = approval.resumed_plan_review;
        let response = kkagent_protocol::ApprovalResponse {
            approval_id: approval.approval_id.clone(),
            decision: choice.decision,
            scope: choice.scope,
            feedback,
            selected_label: Some(choice.selected_label.clone()),
        };
        let result = if resumed_plan_review {
            let mut params = serde_json::to_value(response)?;
            params["session_id"] = sid.clone().into();
            self.client
                .rpc_call("session.resolve_pending_plan_review", Some(params))
                .await
                .map(Some)
        } else {
            self.client
                .respond_approval(&sid, response)
                .await
                .map(|()| None)
        };

        if let Err(e) = result {
            // Restore panel on failure
            self.state.approval_pending = Some(approval);
            self.system_message(format!("Approval failed: {}", e));
        } else {
            let resumed_result = result.ok().flatten();
            if let Some(plan_mode) = resumed_result
                .as_ref()
                .and_then(|value| value.get("plan_mode"))
                .and_then(|value| value.as_bool())
            {
                self.state.on_plan_mode_changed(plan_mode);
            }
            if revising_plan {
                self.state.dismiss_plan_focus();
                self.jobs.push_info("修改意见已提交，正在更新计划…");
            }
            if let Some(next) = self.state.approval_queue.pop_front() {
                self.state.approval_pending = Some(next);
                self.state.status = SessionStatus::WaitingApproval;
            } else {
                self.state.status = if resumed_result.as_ref().is_some_and(|value| {
                    !value
                        .get("turn_started")
                        .and_then(|started| started.as_bool())
                        .unwrap_or(false)
                }) {
                    SessionStatus::Idle
                } else {
                    SessionStatus::Thinking
                };
            }
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
        } else if cancelled {
            self.state.status = SessionStatus::Cancelling;
            self.sync_active_session_status();
        } else {
            let preview: String = answer_preview.chars().take(80).collect();
            self.system_message(format!("Answered: {preview}"));
            // AskUserQuestion itself does not emit Thinking; keep the spinner alive
            // until the next StatusUpdate / tool event arrives.
            self.state.status = SessionStatus::Thinking;
            self.sync_active_session_status();
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

                // BTW is a single toggleable surface. Only events from its
                // current owner may update it; stale events from a replaced
                // conversation are ignored.
                match &evt {
                    AgentEvent::BtwDelta {
                        session_id,
                        agent_id,
                        text,
                    } if self.state.btw.current_session_id.as_deref()
                        == Some(session_id.as_str())
                        && self.state.btw.current_agent_id.as_deref()
                            == Some(agent_id.as_str()) =>
                    {
                        self.state.btw.append_delta(text);
                        return;
                    }
                    AgentEvent::BtwThinkingDelta {
                        session_id,
                        agent_id,
                        text,
                    } if self.state.btw.current_session_id.as_deref()
                        == Some(session_id.as_str())
                        && self.state.btw.current_agent_id.as_deref()
                            == Some(agent_id.as_str()) =>
                    {
                        self.state.btw.append_thinking_delta(text);
                        return;
                    }
                    AgentEvent::BtwRetry {
                        session_id,
                        agent_id,
                        retry_number,
                        reason,
                        remaining_seconds,
                        ..
                    } if self.state.btw.current_session_id.as_deref()
                        == Some(session_id.as_str())
                        && self.state.btw.current_agent_id.as_deref()
                            == Some(agent_id.as_str()) =>
                    {
                        self.state
                            .btw
                            .update_retry(*retry_number, reason, *remaining_seconds);
                        return;
                    }
                    AgentEvent::BtwEnd {
                        session_id,
                        agent_id,
                        error,
                    } if self.state.btw.current_session_id.as_deref()
                        == Some(session_id.as_str())
                        && self.state.btw.current_agent_id.as_deref()
                            == Some(agent_id.as_str()) =>
                    {
                        self.state.btw.finish(error.clone());
                        return;
                    }
                    AgentEvent::BtwDelta { .. }
                    | AgentEvent::BtwThinkingDelta { .. }
                    | AgentEvent::BtwRetry { .. }
                    | AgentEvent::BtwEnd { .. } => return,
                    _ => {}
                }

                if !is_current {
                    match &evt {
                        AgentEvent::ApprovalRequested { request, .. } => {
                            let pending = pending_approval_from_request(request, false);
                            self.state.parked_approvals.insert(evt_sid.clone(), pending);
                            self.state.tab_strip.ensure_tab(&evt_sid, "needs approval");
                            self.state
                                .tab_strip
                                .set_status(&evt_sid, SessionStatus::WaitingApproval);
                            self.state.tab_strip.mark_dirty(&evt_sid, true);
                        }
                        AgentEvent::QuestionAsked { question, .. } => {
                            let options: Vec<(String, String)> = question
                                .options
                                .iter()
                                .map(|o| (o.id.clone(), o.label.clone()))
                                .collect();
                            let toggled = vec![false; options.len()];
                            self.state.parked_questions.insert(
                                evt_sid.clone(),
                                PendingQuestion {
                                    question_id: question.question_id.clone(),
                                    text: question.text.clone(),
                                    options,
                                    allow_free_text: question.allow_free_text,
                                    allow_multiple: question.allow_multiple,
                                    selected: 0,
                                    toggled,
                                    free_text: String::new(),
                                },
                            );
                            self.state.tab_strip.ensure_tab(&evt_sid, "needs question");
                            self.state
                                .tab_strip
                                .set_status(&evt_sid, SessionStatus::WaitingQuestion);
                            self.state.tab_strip.mark_dirty(&evt_sid, true);
                        }
                        AgentEvent::Error { message, .. } if message != "Interrupted" => {
                            self.state.tab_strip.mark_dirty(&evt_sid, true);
                        }
                        AgentEvent::LlmRetry { initial: true, .. } => {
                            self.state.tab_strip.mark_dirty(&evt_sid, true);
                        }
                        AgentEvent::CompactCompleted { .. } => {
                            self.state.tab_strip.mark_dirty(&evt_sid, true);
                        }
                        _ => {}
                    }
                    if !matches!(
                        &evt,
                        AgentEvent::ApprovalRequested { .. }
                            | AgentEvent::QuestionAsked { .. }
                            | AgentEvent::Heartbeat { .. }
                    ) {
                        self.queue_background_session_event(evt_sid, evt);
                    }
                    return;
                }

                match evt {
                    AgentEvent::BtwDelta { .. }
                    | AgentEvent::BtwThinkingDelta { .. }
                    | AgentEvent::BtwRetry { .. }
                    | AgentEvent::BtwEnd { .. } => unreachable!("BTW events handled above"),
                    AgentEvent::SteerInput {
                        text,
                        idempotency_key,
                        ..
                    } => {
                        let mut found = false;
                        if let Some(key) = idempotency_key.as_deref() {
                            for message in self.state.messages.iter_mut().filter(|message| {
                                message.role == MessageRole::User
                                    && message.idempotency_key.as_deref() == Some(key)
                            }) {
                                message.delivery = crate::prompt_queue::DeliveryState::Sent;
                                found = true;
                            }
                        }
                        if !found {
                            self.state.messages.push(DisplayMessage {
                                role: MessageRole::User,
                                content: text,
                                thinking: None,
                                parts: Vec::new(),
                                tool_calls: Vec::new(),
                                delivery: crate::prompt_queue::DeliveryState::Sent,
                                idempotency_key,
                            });
                        }
                        self.state.follow_bottom = true;
                        self.state.scroll_up = 0;
                    }
                    AgentEvent::MessageDelta { text, .. } => {
                        let pending_thinking = if !self.state.thinking_text.is_empty() {
                            Some(std::mem::take(&mut self.state.thinking_text))
                        } else {
                            None
                        };

                        if let Some(message) = self
                            .state
                            .active_assistant_message
                            .and_then(|index| self.state.messages.get_mut(index))
                            .filter(|message| message.role == MessageRole::Assistant)
                        {
                            if message.thinking.is_none() {
                                message.thinking = pending_thinking;
                            }
                            message.append_assistant_text(&text);
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
                        msg.append_assistant_text(&text);
                        self.state.messages.push(msg);
                        self.state.active_assistant_message =
                            Some(self.state.messages.len().saturating_sub(1));
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
                            queued_behind: None,
                            name: tool_name,
                            input_summary: summary,
                            output: None,
                            is_error: false,
                            collapsed: !self.state.tool_output_expanded,
                            user_overridden: false,
                        };
                        if let Some(message) = self
                            .state
                            .active_assistant_message
                            .and_then(|index| self.state.messages.get_mut(index))
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
                        self.state.messages.push(msg);
                        self.state.active_assistant_message =
                            Some(self.state.messages.len().saturating_sub(1));
                    }
                    AgentEvent::ToolExecutionStatus {
                        tool_call_id,
                        status,
                        queued_behind,
                        ..
                    } => {
                        if let Some(tc) = self
                            .state
                            .messages
                            .iter_mut()
                            .rev()
                            .find_map(|message| message.find_tool_by_id_mut(&tool_call_id))
                        {
                            if status == "queued" {
                                tc.queued_behind = queued_behind;
                            } else {
                                tc.queued_behind = None;
                                if tc.started_at.is_none() {
                                    tc.started_at = Some(std::time::Instant::now());
                                }
                            }
                        }
                    }
                    AgentEvent::ToolResult {
                        tool_call_id,
                        tool_name,
                        output,
                        is_error,
                        ..
                    } => {
                        // A steer user message may now be the transcript tail;
                        // update the assistant tool call by id instead of only
                        // inspecting the final display message.
                        if let Some(tc) = self.state.messages.iter_mut().rev().find_map(|message| {
                            message.find_tool_for_result_mut(&tool_call_id, &tool_name)
                        }) {
                            tc.output = Some(output);
                            tc.is_error = is_error;
                            tc.stopping = false;
                            tc.queued_behind = None;
                            if is_error {
                                tc.collapsed = false;
                            }
                        }
                    }
                    AgentEvent::StatusUpdate { status, .. } => {
                        self.state.status = status;
                        if matches!(status, SessionStatus::Idle) {
                            self.flush_prompt_queue_if_idle();
                        }
                    }
                    AgentEvent::Heartbeat { .. } => {}
                    AgentEvent::LlmRetry {
                        retry_number,
                        reason,
                        wait_seconds,
                        remaining_seconds,
                        initial,
                        ..
                    } => {
                        self.update_llm_retry_message(
                            retry_number,
                            &reason,
                            wait_seconds,
                            remaining_seconds,
                            initial,
                        );
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
                        let pending = pending_approval_from_request(&request, false);
                        if self.state.approval_pending.is_some() {
                            self.state.approval_queue.push_back(pending);
                            self.system_message(format!(
                                "Approval queued ({} waiting).",
                                self.state.approval_queue.len()
                            ));
                        } else {
                            self.state.approval_pending = Some(pending);
                            self.state.status = SessionStatus::WaitingApproval;
                        }
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
                            selected: 0,
                            toggled,
                            free_text: String::new(),
                        };
                        self.state.question_pending = Some(pending);
                        self.state.status = SessionStatus::WaitingQuestion;
                    }
                    AgentEvent::UsageUpdate {
                        usage,
                        context,
                        steps,
                        turns,
                        ..
                    } => {
                        // `usage` is per-call; session totals accumulate across
                        // calls so cost/usage reflects the whole session.
                        // Context indicator = full prompt actually sent (cache
                        // tokens included via provider-aware helper) + output.
                        self.state.approx_tokens = usage.context_size();
                        // Latest-request snapshot for /usage and the footer
                        // cache indicator — without this the "Latest request"
                        // section goes stale until the next session switch.
                        self.state.last_step_usage = Some(usage.clone());
                        let s = &mut self.state.usage_session;
                        s.input_tokens = s.input_tokens.saturating_add(usage.input_tokens);
                        s.output_tokens = s.output_tokens.saturating_add(usage.output_tokens);
                        s.cache_creation_tokens = s
                            .cache_creation_tokens
                            .saturating_add(usage.cache_creation_input_tokens);
                        s.cache_read_tokens = s
                            .cache_read_tokens
                            .saturating_add(usage.cache_read_input_tokens);
                        // Track the provider's input semantics across calls so
                        // session-level ratios use the right denominator
                        // (Anthropic pure-read totals would otherwise inflate
                        // past 100% via the None heuristic).
                        if usage.input_includes_cache.is_some() {
                            s.input_includes_cache = usage.input_includes_cache;
                        }
                        s.steps = steps;
                        s.turns = turns;
                        if let Some(ctx) = context {
                            self.state.context_breakdown = Some(ctx);
                        }
                        self.state.usage_turns.push(TurnUsageSample {
                            model: self.state.model_alias.clone(),
                            input_tokens: usage.input_tokens,
                            output_tokens: usage.output_tokens,
                            cache_creation_tokens: usage.cache_creation_input_tokens,
                            cache_read_tokens: usage.cache_read_input_tokens,
                            input_includes_cache: usage.input_includes_cache,
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
                        if self.state.todos.is_empty() || all_todos_finished(&self.state.todos) {
                            self.state.todos_expanded = false;
                        }
                    }
                    AgentEvent::GoalUpdated { goal, change, .. } => {
                        let status = goal
                            .as_ref()
                            .and_then(|g| g.get("status"))
                            .and_then(|s| s.as_str())
                            .unwrap_or("none");
                        let objective = goal
                            .as_ref()
                            .and_then(|g| g.get("description"))
                            .and_then(|s| s.as_str())
                            .unwrap_or("");
                        if objective.is_empty() {
                            self.system_message(format!("Goal {change} ({status})"));
                        } else {
                            let preview: String = objective.chars().take(80).collect();
                            self.system_message(format!("Goal {change} ({status}): {preview}"));
                        }
                    }
                    AgentEvent::TurnEnd { .. } => {
                        let active_assistant = self.state.active_assistant_message.take();
                        if !self.state.thinking_text.is_empty() {
                            let t = std::mem::take(&mut self.state.thinking_text);
                            let attached = active_assistant
                                .and_then(|index| self.state.messages.get_mut(index))
                                .filter(|message| {
                                    message.role == MessageRole::Assistant
                                        && message.thinking.is_none()
                                })
                                .map(|message| message.thinking = Some(t.clone()))
                                .is_some();
                            if !attached {
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
                        // Soft bell when the window may not be focused (best-effort).
                        if std::env::var("KKAGENT_NOTIFY")
                            .map(|v| v != "0" && v != "off")
                            .unwrap_or(true)
                        {
                            let _ = crossterm::execute!(
                                std::io::stdout(),
                                crossterm::event::EnableBracketedPaste
                            );
                            print!("\x07");
                            let _ = std::io::Write::flush(&mut std::io::stdout());
                        }
                        // Optional turn completion summary from recent bash/test tools.
                        if let Some(summary) = recent_test_summary(&self.state.messages) {
                            self.system_message(format!("{summary} · /changes for file edits"));
                        }
                        self.flush_prompt_queue_if_idle();
                    }
                    AgentEvent::TurnStart { .. } => {
                        self.state.active_assistant_message = None;
                        self.state.thinking_text.clear();
                        self.state.turn_started_at = Some(std::time::Instant::now());
                        self.state.tokens_at_turn_start = self.state.approx_tokens;
                        self.jobs.mcp.waiting_for_prompt = false;
                    }
                    AgentEvent::SubagentSpawned {
                        subagent_id,
                        subagent_name,
                        description,
                        ..
                    } => {
                        let desc = description.unwrap_or_default();
                        self.state.subagents.upsert_spawned(
                            subagent_id,
                            subagent_name,
                            desc,
                            "pending",
                        );
                    }
                    AgentEvent::SubagentStarted { subagent_id, .. } => {
                        self.state
                            .subagents
                            .set_status(&subagent_id, "running", None);
                    }
                    AgentEvent::SubagentCompleted {
                        subagent_id,
                        result_summary,
                        ..
                    } => {
                        self.state.subagents.set_status(
                            &subagent_id,
                            "complete",
                            Some(result_summary.chars().take(240).collect()),
                        );
                    }
                    AgentEvent::SubagentFailed {
                        subagent_id, error, ..
                    } => {
                        self.state.subagents.set_status(
                            &subagent_id,
                            "failed",
                            Some(error.chars().take(240).collect()),
                        );
                    }
                    AgentEvent::SubagentChildEvent {
                        subagent_id, event, ..
                    } => match *event {
                        AgentEvent::ToolCall {
                            tool_name, input, ..
                        } => {
                            let line = crate::subagents::format_tool_activity(&tool_name, &input);
                            self.state.subagents.note_child_event(&subagent_id, line);
                        }
                        AgentEvent::ToolResult {
                            tool_name,
                            is_error,
                            ..
                        } => {
                            let mark = if is_error { "failed" } else { "ok" };
                            self.state
                                .subagents
                                .note_child_event(&subagent_id, format!("{tool_name} [{mark}]"));
                        }
                        AgentEvent::Error { message, .. } => {
                            self.state.subagents.note_child_event(
                                &subagent_id,
                                format!("error: {}", message.chars().take(80).collect::<String>()),
                            );
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
                    AgentEvent::CompactCompleted {
                        deleted,
                        kept_user_message_count,
                        messages,
                        error,
                        ..
                    } => {
                        if let Some(err) = error {
                            self.system_message(format!(
                                "Compact failed: {err} — current prompt kept; try /compact again or continue."
                            ));
                        } else {
                            self.state.messages = transcript_messages_to_display(&messages);
                            self.state.apply_tool_output_mode();
                            self.state.active_assistant_message = None;
                            self.state.follow_bottom = true;
                            self.state.scroll_up = 0;
                            self.system_message(format!(
                                "Compacted: {deleted} removed · kept {kept_user_message_count} recent user msgs (file undo/checkpoints may be limited after compact)"
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
                    AgentEvent::SessionConfigChanged {
                        permission_mode,
                        model,
                        plan_mode,
                        working_dir,
                        source,
                        ..
                    } => {
                        if let Some(mode) = permission_mode
                            .as_deref()
                            .and_then(parse_permission_mode_str)
                        {
                            self.state.permission_mode = mode;
                        }
                        if let Some(ref m) = model {
                            self.state.model_alias = Some(m.clone());
                        }
                        if let Some(enabled) = plan_mode {
                            self.state.on_plan_mode_changed(enabled);
                        }
                        let src = source.unwrap_or_else(|| "other client".into());
                        let mut bits = Vec::new();
                        if permission_mode.is_some() {
                            bits.push("permission");
                        }
                        if model.is_some() {
                            bits.push("model");
                        }
                        if plan_mode.is_some() {
                            bits.push("plan");
                        }
                        if working_dir.is_some() {
                            bits.push("cwd");
                        }
                        if !bits.is_empty() {
                            self.system_message(format!(
                                "Session settings synced from {src}: {}",
                                bits.join(", ")
                            ));
                        }
                    }
                    AgentEvent::ToolCancelled {
                        tool_call_id,
                        reason,
                        ..
                    } => {
                        for msg in &mut self.state.messages {
                            for part in &mut msg.parts {
                                if let DisplayPart::Tool(tc) = part {
                                    if tc.id == tool_call_id {
                                        tc.stopping = false;
                                        tc.is_error = true;
                                        tc.output = Some(
                                            reason.clone().unwrap_or_else(|| "cancelled".into()),
                                        );
                                        tc.collapsed = false;
                                    }
                                }
                            }
                        }
                        self.system_message(format!("Tool {tool_call_id} cancelled"));
                    }
                }
            }
        }
    }

    fn toggle_tool_folding(&mut self) {
        self.state.tool_output_expanded = !self.state.tool_output_expanded;
        self.state.apply_tool_output_mode();
    }
}

fn connection_loss_message(remote: bool, reason: &str) -> String {
    if remote {
        format!(
            "Connection to the --connect server was lost ({reason}). Exit kkagent and reconnect after the server is available."
        )
    } else {
        format!(
            "Embedded agent connection closed unexpectedly ({reason}). Exit and restart kkagent."
        )
    }
}

fn recent_test_summary(messages: &[DisplayMessage]) -> Option<String> {
    for msg in messages.iter().rev().take(6) {
        for part in msg.parts.iter().rev() {
            if let DisplayPart::Tool(tc) = part {
                if tc.name == "Bash" {
                    if let Some(out) = &tc.output {
                        if let Some(s) = crate::test_summary::parse_test_output(out) {
                            if s.passed + s.failed > 0 {
                                return Some(s.one_line());
                            }
                        }
                    }
                }
            }
        }
    }
    None
}

fn parse_permission_mode_str(raw: &str) -> Option<PermissionMode> {
    let s = raw.trim().trim_matches('"');
    s.parse()
        .ok()
        .or_else(|| match s.to_ascii_lowercase().as_str() {
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

/// Provider-normalized effective input for session totals. With explicit
/// flag: Anthropic adds both cache buckets, OpenAI uses `input_tokens` alone
/// (cached subset). Without flag (legacy JSON): heuristic on cache_creation.
fn effective_total_input(u: &SessionUsageTotals) -> u64 {
    let includes = u
        .input_includes_cache
        .unwrap_or(u.cache_creation_tokens == 0);
    if includes {
        u.input_tokens
    } else {
        u.input_tokens
            .saturating_add(u.cache_creation_tokens)
            .saturating_add(u.cache_read_tokens)
    }
}

/// Whether the provider reported a billable cache-write bucket. Anthropic
/// reports it outside input_tokens; newer OpenAI models report it as a subset.
fn cache_creation_is_real_semantics(u: &SessionUsageTotals) -> bool {
    u.cache_creation_tokens > 0
}

/// Session totals carry provider-native semantics:
/// - Anthropic: `input_tokens` excludes both cache buckets.
/// - OpenAI: `input_tokens` already includes cached tokens.
///
/// Normalize to disjoint billable buckets before pricing: for OpenAI-style
/// totals, cached tokens are a subset of `input_tokens`, so bill only the
/// remainder at the full input price.
fn estimate_usd(
    u: &SessionUsageTotals,
    in_price: f64,
    out_price: f64,
    cache_c: f64,
    cache_r: f64,
) -> f64 {
    let uncached_input = effective_total_input(u)
        .saturating_sub(u.cache_read_tokens)
        .saturating_sub(u.cache_creation_tokens);
    (uncached_input as f64) * in_price / 1_000_000.0
        + (u.output_tokens as f64) * out_price / 1_000_000.0
        + (u.cache_creation_tokens as f64) * cache_c / 1_000_000.0
        + (u.cache_read_tokens as f64) * cache_r / 1_000_000.0
}

/// `1234567` → `"1,234,567"` for readable token counts.
fn fmt_thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    let bytes = s.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

/// Fixed-width Unicode progress bar: `████░░░` (`width` cells total).
fn progress_bar(used: u64, max: u64, width: usize) -> String {
    if max == 0 || width == 0 {
        return "░".repeat(width);
    }
    let ratio = (used.min(max) as f64 / max as f64).clamp(0.0, 1.0);
    let filled = (ratio * width as f64).round() as usize;
    let filled = filled.min(width);
    let mut out = String::with_capacity(width * 3);
    for _ in 0..filled {
        out.push('█');
    }
    for _ in filled..width {
        out.push('░');
    }
    out
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

fn pending_approval_from_request(
    request: &kkagent_protocol::ApprovalRequest,
    resumed_plan_review: bool,
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
        hidden: false,
        resumed_plan_review: resumed_plan_review && is_plan_review,
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

fn plan_document_from_resume(data: &serde_json::Value) -> Option<PlanDocument> {
    let plan = data.get("plan")?;
    let path = plan.get("path")?.as_str()?.trim();
    let content = plan.get("content")?.as_str()?;
    if path.is_empty() || content.trim().is_empty() {
        return None;
    }
    Some(PlanDocument {
        path: path.to_string(),
        content: content.to_string(),
    })
}

/// Fold tool calls in `[start, end)` into one `ToolHistory` overview.
fn collapse_tools_in_turn(
    messages: &mut Vec<DisplayMessage>,
    start: usize,
    end: usize,
    duration_ms: u64,
    tokens: u64,
    expanded: bool,
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
        expanded,
        user_overridden: false,
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
        collapse_tools_in_turn(messages, start, end, 0, 0, false);
    }
}

fn recent_turn_cutoff(messages: &[DisplayMessage], turns: usize) -> usize {
    if turns == 0 {
        return messages.len();
    }
    messages
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, message)| message.role == MessageRole::User)
        .nth(turns.saturating_sub(1))
        .map(|(index, _)| index)
        .unwrap_or(0)
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

fn session_status_has_active_agent_loop(status: SessionStatus) -> bool {
    !matches!(status, SessionStatus::Idle)
}

fn persist_composer_draft(session_id: &str, input: &crate::input::InputState) {
    if input.is_empty() {
        crate::draft_store::clear_draft(session_id);
    } else {
        let _ = crate::draft_store::save_draft(session_id, &input.text, input.cursor);
    }
}

fn slash_command_opens_immediately(name: &str) -> bool {
    matches!(
        name,
        "model"
            | "sessions"
            | "resume"
            | "tasks"
            | "task"
            | "ps"
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
            | "doctor"
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
}

/// Old builds could persist a command-only composer after Enter accepted its
/// slash completion. Do not resurrect those consumed commands on resume.
fn slash_draft_looks_consumed(text: &str) -> bool {
    let Some((command, args)) = parse_slash_input(text) else {
        return false;
    };
    if command.is_empty() || !args.is_empty() {
        return false;
    }
    let canonical = find_slash_command(&command)
        .map(|entry| entry.name.to_string())
        .or_else(|| {
            let matches = crate::slash::filter_slash_commands(&format!("/{command}"));
            (matches.len() == 1).then(|| matches[0].name.clone())
        });
    let Some(canonical) = canonical else {
        return false;
    };
    find_slash_command(&canonical).is_some_and(|entry| slash_command_opens_immediately(entry.name))
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
                                user_overridden: false,
                                queued_behind: None,
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

fn paste_clipboard_into_workspace(root: &std::path::Path) -> anyhow::Result<Option<PathBuf>> {
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
    Ok(Some(path.strip_prefix(root).unwrap_or(&path).to_path_buf()))
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

fn history_edit_turns_from_json(data: &serde_json::Value) -> Vec<HistoryEditTurn> {
    let mut turns = data
        .get("turns")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|value| {
            let turn_index = usize::try_from(value.get("turn_index")?.as_u64()?).ok()?;
            let message_index = usize::try_from(value.get("message_index")?.as_u64()?).ok()?;
            let text = value.get("text")?.as_str()?.trim().to_string();
            (!text.is_empty()).then_some(HistoryEditTurn {
                turn_index,
                message_index,
                text,
            })
        })
        .collect::<Vec<_>>();
    turns.sort_by_key(|turn| (turn.turn_index, turn.message_index));
    turns.dedup_by_key(|turn| turn.message_index);
    turns
}

fn normalized_workspace_key(path: &std::path::Path) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

fn display_workspace(workspace: &str) -> String {
    let path = std::path::Path::new(workspace);
    if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
        return format!("{name} ({workspace})");
    }
    workspace.to_string()
}

fn history_turn_summary(text: &str, max_chars: usize) -> String {
    let first_line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("(empty prompt)");
    let count = first_line.chars().count();
    if count <= max_chars {
        first_line.to_string()
    } else {
        let keep = max_chars.saturating_sub(1);
        format!("{}…", first_line.chars().take(keep).collect::<String>())
    }
}

fn model_matches_global_fallback(config: &AppConfig, model: &str) -> bool {
    config.fallback_model.as_deref() == Some(model)
}

#[cfg(test)]
mod usage_cost_tests {
    use super::*;

    /// OpenAI-style totals: cached tokens are a subset of input_tokens, so
    /// they must be billed once (cache price), not twice.
    #[test]
    fn estimate_usd_openai_style_no_double_billing() {
        let u = SessionUsageTotals {
            input_tokens: 100_000,
            output_tokens: 50_000,
            cache_creation_tokens: 0,
            cache_read_tokens: 95_000,
            steps: 1,
            turns: 1,
            input_includes_cache: Some(true),
        };
        // 5k full input + 95k cached + 50k output.
        let usd = estimate_usd(&u, 3.0, 15.0, 3.75, 0.3);
        let expected = (5_000.0 * 3.0 + 95_000.0 * 0.3 + 50_000.0 * 15.0) / 1_000_000.0;
        assert!((usd - expected).abs() < 1e-9);
    }

    /// Anthropic-style totals: input excludes cache buckets; bill each of the
    /// three buckets at its own price.
    #[test]
    fn estimate_usd_anthropic_style_buckets_disjoint() {
        let u = SessionUsageTotals {
            input_tokens: 5_000,
            output_tokens: 50_000,
            cache_creation_tokens: 2_000,
            cache_read_tokens: 93_000,
            steps: 1,
            turns: 1,
            input_includes_cache: Some(false),
        };
        let usd = estimate_usd(&u, 3.0, 15.0, 3.75, 0.3);
        let expected =
            (5_000.0 * 3.0 + 2_000.0 * 3.75 + 93_000.0 * 0.3 + 50_000.0 * 15.0) / 1_000_000.0;
        assert!((usd - expected).abs() < 1e-9);
        // Effective input adds the cache bucket.
        assert_eq!(effective_total_input(&u), 100_000);
    }

    /// Legacy totals without the explicit flag fall back to the
    /// cache_creation heuristic (Anthropic when any creation is reported).
    #[test]
    fn effective_total_input_legacy_heuristic() {
        let legacy_anthropic = SessionUsageTotals {
            input_tokens: 5_000,
            output_tokens: 1_000,
            cache_creation_tokens: 3_000,
            cache_read_tokens: 95_000,
            steps: 1,
            turns: 1,
            input_includes_cache: None,
        };
        assert_eq!(effective_total_input(&legacy_anthropic), 103_000);

        let legacy_openai = SessionUsageTotals {
            input_tokens: 100_000,
            output_tokens: 1_000,
            cache_creation_tokens: 0,
            cache_read_tokens: 95_000,
            steps: 1,
            turns: 1,
            input_includes_cache: None,
        };
        assert_eq!(effective_total_input(&legacy_openai), 100_000);
    }

    /// /usage shows the cache-write row only when a provider reports writes.
    #[test]
    fn cache_creation_row_hidden_for_non_anthropic_semantics() {
        let openai_style = SessionUsageTotals {
            input_tokens: 100_000,
            output_tokens: 1_000,
            cache_creation_tokens: 0,
            cache_read_tokens: 95_000,
            steps: 2,
            turns: 1,
            input_includes_cache: Some(true),
        };
        assert!(!cache_creation_is_real_semantics(&openai_style));

        let openai_with_writes = SessionUsageTotals {
            cache_creation_tokens: 4_000,
            ..openai_style.clone()
        };
        assert!(cache_creation_is_real_semantics(&openai_with_writes));

        let legacy = SessionUsageTotals {
            input_tokens: 100_000,
            output_tokens: 1_000,
            cache_creation_tokens: 0,
            cache_read_tokens: 95_000,
            steps: 2,
            turns: 1,
            input_includes_cache: None,
        };
        assert!(!cache_creation_is_real_semantics(&legacy));

        let anthropic = SessionUsageTotals {
            input_tokens: 5_000,
            output_tokens: 1_000,
            cache_creation_tokens: 0,
            cache_read_tokens: 95_000,
            steps: 2,
            turns: 1,
            input_includes_cache: Some(false),
        };
        assert!(!cache_creation_is_real_semantics(&anthropic));
    }

    #[test]
    fn estimate_usd_openai_cache_writes_are_not_double_billed() {
        let u = SessionUsageTotals {
            input_tokens: 100_000,
            output_tokens: 0,
            cache_creation_tokens: 20_000,
            cache_read_tokens: 70_000,
            steps: 1,
            turns: 1,
            input_includes_cache: Some(true),
        };
        let usd = estimate_usd(&u, 3.0, 15.0, 3.75, 0.3);
        let expected = (10_000.0 * 3.0 + 20_000.0 * 3.75 + 70_000.0 * 0.3) / 1_000_000.0;
        assert!((usd - expected).abs() < 1e-9);
    }
}

#[cfg(test)]
mod app_state_tests {
    use super::*;
    use std::sync::Arc;

    fn test_tui_app() -> TuiApp {
        let (client_transport, _server_transport) =
            kkagent_rpc::transport::memory::create_memory_pair();
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(16);
        let rpc = kkagent_rpc::RpcClient::new(client_transport, event_tx);
        let client = kkagent_client::KkagentClient::new(rpc, event_rx);
        TuiApp::new(AppConfig::default(), client)
    }

    #[tokio::test]
    async fn selecting_global_fallback_offers_disable_or_alternate_model() {
        let mut app = test_tui_app();
        app.config.fallback_model = Some("backup".into());
        for model in ["primary", "backup", "alternate"] {
            app.config.models.insert(
                model.into(),
                kkagent_config::ModelConfig {
                    provider: "test".into(),
                    model: model.into(),
                    max_context_size: None,
                    max_output_size: None,
                    capabilities: Vec::new(),
                    display_name: None,
                    support_efforts: Vec::new(),
                    default_effort: None,
                    pricing: None,
                    experimental_adaptive_thinking: false,
                    experimental_visible_empty_retries: 0,
                    experimental_bad_toolcall_auto_retries: 0,
                    first_token_timeout_ms: None,
                },
            );
        }
        app.state.model_alias = Some("backup".into());

        assert!(model_matches_global_fallback(&app.config, "backup"));
        app.open_fallback_decision_picker();
        let decision = app.state.list_picker.as_ref().unwrap();
        assert_eq!(decision.kind, ListPickerKind::FallbackDecision);
        assert_eq!(
            decision
                .items
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            ["disabled", "choose"]
        );

        app.open_fallback_model_picker();
        let picker = app.state.list_picker.as_ref().unwrap();
        assert_eq!(picker.kind, ListPickerKind::FallbackModel);
        assert!(picker.items.iter().all(|item| item.id != "backup"));
        assert_eq!(
            picker
                .items
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            ["alternate", "primary"]
        );
    }

    #[tokio::test]
    async fn plugins_command_opens_management_home_and_marketplace_prompt() {
        let mut app = test_tui_app();
        app.open_plugins_picker().await.unwrap();
        let picker = app.state.list_picker.as_ref().unwrap();
        assert_eq!(picker.kind, ListPickerKind::PluginHome);
        assert_eq!(
            picker
                .items
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            [
                "installed",
                "marketplaces",
                "add_marketplace",
                "install_source",
                "reload"
            ]
        );

        app.state.list_picker.as_mut().unwrap().selected = 2;
        app.apply_list_picker().await.unwrap();
        assert_eq!(
            app.state.plugin_prompt.as_ref().map(|prompt| &prompt.kind),
            Some(&PluginPromptKind::AddMarketplace)
        );
        assert_eq!(
            app.state.list_picker.as_ref().map(|picker| &picker.kind),
            Some(&ListPickerKind::PluginHome)
        );

        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
            .await
            .unwrap();
        assert_eq!(
            app.state
                .plugin_prompt
                .as_ref()
                .map(|prompt| prompt.value.as_str()),
            Some("x")
        );
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .unwrap();
        assert!(app.state.plugin_prompt.is_none());
        assert_eq!(
            app.state.list_picker.as_ref().map(|picker| &picker.kind),
            Some(&ListPickerKind::PluginHome)
        );
    }

    #[tokio::test]
    async fn llm_retry_countdown_updates_one_message_and_each_retry_gets_a_new_one() {
        let mut app = test_tui_app();

        app.update_llm_retry_message(1, "HTTP 429 Too Many Requests", 5, 5, true);
        assert_eq!(app.state.messages.len(), 1);
        assert!(app.state.messages[0].content.contains("in 5s"));

        app.update_llm_retry_message(1, "HTTP 429 Too Many Requests", 5, 4, false);
        assert_eq!(app.state.messages.len(), 1);
        assert!(app.state.messages[0].content.contains("in 4s"));

        app.update_llm_retry_message(1, "HTTP 429 Too Many Requests", 5, 0, false);
        assert_eq!(app.state.messages.len(), 1);
        assert!(app.state.messages[0].content.contains("now"));

        app.update_llm_retry_message(2, "connection reset", 1, 1, true);
        assert_eq!(app.state.messages.len(), 2);
        assert!(app.state.messages[1].content.contains("retry #2 in 1s"));

        let long_reason = format!("HTTP 429: {}tail", "x".repeat(240));
        app.update_llm_retry_message(3, &long_reason, 1, 1, true);
        assert!(app.state.messages[2].content.ends_with("tail"));
    }

    #[tokio::test]
    async fn ctrl_h_from_terminal_is_handled_as_backspace() {
        let mut app = test_tui_app();
        app.state.input.set_text("hello".into());

        app.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL))
            .await
            .unwrap();

        assert_eq!(app.state.input.text, "hell");
    }

    #[tokio::test]
    async fn unrecognized_control_chord_is_not_inserted_as_text() {
        let mut app = test_tui_app();
        app.state.input.set_text("hello".into());

        app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL))
            .await
            .unwrap();

        assert_eq!(app.state.input.text, "hello");
    }

    #[tokio::test]
    async fn release_events_are_ignored_while_repeat_events_keep_typing() {
        let mut app = test_tui_app();
        let release = KeyEvent::new_with_kind(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
            crossterm::event::KeyEventKind::Release,
        );
        let repeat = KeyEvent::new_with_kind(
            KeyCode::Char('y'),
            KeyModifiers::NONE,
            crossterm::event::KeyEventKind::Repeat,
        );

        app.handle_key(release).await.unwrap();
        app.handle_key(repeat).await.unwrap();

        assert_eq!(app.state.input.text, "y");
    }

    #[tokio::test]
    async fn double_and_triple_click_select_word_and_line() {
        let mut app = test_tui_app();
        app.state.transcript_area = ratatui::layout::Rect::new(0, 0, 80, 10);
        app.state.select_rows = vec![crate::selection::SelectRow {
            plain: "  hello world".into(),
            content_col: 2,
        }];
        let mouse = |kind| crossterm::event::MouseEvent {
            kind,
            column: 4,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        let mut scroll_delta = 0;

        app.collect_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left)),
            &mut scroll_delta,
        );
        app.collect_mouse(
            mouse(MouseEventKind::Up(MouseButton::Left)),
            &mut scroll_delta,
        );
        assert!(app.state.selection.is_none());
        assert_eq!(app.state.click_history.last().unwrap().count, 1);

        app.collect_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left)),
            &mut scroll_delta,
        );
        app.collect_mouse(
            mouse(MouseEventKind::Up(MouseButton::Left)),
            &mut scroll_delta,
        );
        assert_eq!(app.selection_copy_text().as_deref(), Some("hello"));
        assert_eq!(app.state.click_history.last().unwrap().count, 2);

        app.collect_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left)),
            &mut scroll_delta,
        );
        app.collect_mouse(
            mouse(MouseEventKind::Up(MouseButton::Left)),
            &mut scroll_delta,
        );
        assert_eq!(app.selection_copy_text().as_deref(), Some("hello world"));
        assert_eq!(app.state.click_history.last().unwrap().count, 3);
    }

    #[test]
    fn idle_ticks_do_not_request_high_frequency_redraws() {
        assert!(!tick_requires_redraw(1, 2, false));
        assert!(!tick_requires_redraw(1, 2, true));
        assert!(tick_requires_redraw(3, 4, true));
        assert!(tick_requires_redraw(79, 80, false));
        assert!(tick_requires_redraw(99, 100, false));
    }

    #[test]
    fn stream_events_are_rate_limited_but_boundaries_render_immediately() {
        let frame_for = |event: AgentEvent| Frame::Event {
            event: "agent".into(),
            scope: None,
            data: serde_json::to_value(event).unwrap(),
        };
        assert_eq!(
            server_event_redraw(&frame_for(AgentEvent::MessageDelta {
                session_id: "s".into(),
                text: "chunk".into(),
            })),
            ServerEventRedraw::Stream
        );
        assert_eq!(
            server_event_redraw(&frame_for(AgentEvent::ThinkingDelta {
                session_id: "s".into(),
                text: "thought".into(),
            })),
            ServerEventRedraw::Stream
        );
        assert_eq!(
            server_event_redraw(&frame_for(AgentEvent::Heartbeat {
                session_id: "s".into(),
            })),
            ServerEventRedraw::None
        );
        assert_eq!(
            server_event_redraw(&frame_for(AgentEvent::ToolCall {
                session_id: "s".into(),
                tool_call_id: "tool".into(),
                tool_name: "Read".into(),
                input: serde_json::json!({}),
            })),
            ServerEventRedraw::Immediate
        );
    }

    #[test]
    fn pending_stream_draw_becomes_due_at_twenty_fps() {
        let start = std::time::Instant::now();
        assert!(!stream_redraw_due(
            true,
            start,
            start + STREAM_DRAW_INTERVAL - std::time::Duration::from_millis(1)
        ));
        assert!(stream_redraw_due(true, start, start + STREAM_DRAW_INTERVAL));
        assert!(!stream_redraw_due(
            false,
            start,
            start + STREAM_DRAW_INTERVAL
        ));
    }

    #[test]
    fn full_repaint_interval_parses_from_env_semantics() {
        // Default / invalid / zero-disable all behave as documented.
        assert_eq!(
            parse_full_repaint_interval("10"),
            std::time::Duration::from_secs(10)
        );
        assert_eq!(
            parse_full_repaint_interval(" 7 "),
            std::time::Duration::from_secs(7)
        );
        assert_eq!(
            parse_full_repaint_interval("not-a-number"),
            DEFAULT_FULL_REPAINT_INTERVAL
        );
        assert_eq!(parse_full_repaint_interval("0"), std::time::Duration::ZERO);
    }

    #[tokio::test]
    async fn f5_forces_a_full_redraw_flag() {
        let mut app = test_tui_app();
        assert!(!app.force_full_redraw);
        app.handle_key(KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE))
            .await
            .unwrap();
        assert!(app.force_full_redraw);
    }

    #[tokio::test]
    async fn ctrl_l_still_reaches_the_editor_for_toggle_expand() {
        // Ctrl+L must NOT be stolen by the global redraw path: it toggles the
        // expanded input box. F5 owns the repaint action instead.
        let mut app = test_tui_app();
        app.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL))
            .await
            .unwrap();
        assert!(!app.force_full_redraw);
    }

    #[tokio::test]
    async fn plain_mouse_movement_does_not_request_a_redraw() {
        let mut app = test_tui_app();
        let mut scroll_delta = 0;

        let changed = app.collect_mouse(
            crossterm::event::MouseEvent {
                kind: MouseEventKind::Moved,
                column: 12,
                row: 4,
                modifiers: KeyModifiers::NONE,
            },
            &mut scroll_delta,
        );

        assert!(!changed);
        assert_eq!(app.state.last_mouse, Some((12, 4)));
    }

    #[tokio::test]
    async fn reaching_loaded_history_top_requests_only_one_page() {
        let mut app = test_tui_app();
        app.state.session_id = Some("history-session".into());
        app.state.history_oldest_index = Some(80);
        app.state.content_lines = 100;
        app.state.viewport_height = 20;
        app.state.scroll_up = 80;

        app.enqueue_earlier_history_if_at_top();
        app.enqueue_earlier_history_if_at_top();

        assert!(app.state.history_loading);
        assert!(!app.state.follow_bottom);
        assert!(app
            .jobs
            .pending
            .contains_key(&crate::async_jobs::JobChannel::SessionHistory));
    }

    #[tokio::test]
    async fn scrolling_down_does_not_request_earlier_history() {
        let mut app = test_tui_app();
        app.state.session_id = Some("history-session".into());
        app.state.history_oldest_index = Some(80);
        let mut scroll_delta = -3;

        app.flush_pending_scroll(&mut scroll_delta);

        assert!(!app.state.history_loading);
        assert!(!app
            .jobs
            .pending
            .contains_key(&crate::async_jobs::JobChannel::SessionHistory));
    }

    #[tokio::test]
    async fn loaded_history_page_waits_for_another_scroll_before_continuing() {
        let mut app = test_tui_app();
        app.state.session_id = Some("history-session".into());
        app.state.history_oldest_index = Some(80);
        app.state.history_loading = true;

        app.apply_session_history_page(
            "history-session",
            80,
            serde_json::json!({
                "messages": [{
                    "role": "user",
                    "content": [{"type": "text", "text": "older prompt"}]
                }],
                "history": {
                    "oldest_index": 40,
                    "older_available": true
                }
            }),
        );

        assert_eq!(app.state.messages.len(), 1);
        assert_eq!(app.state.history_oldest_index, Some(40));
        assert!(!app.state.history_loading);
        assert!(!app
            .jobs
            .pending
            .contains_key(&crate::async_jobs::JobChannel::SessionHistory));
    }

    #[test]
    fn reconnect_guidance_is_only_used_for_remote_connections() {
        let remote = connection_loss_message(true, "peer closed");
        assert!(remote.contains("--connect"));
        assert!(remote.contains("reconnect"));

        let embedded = connection_loss_message(false, "peer closed");
        assert!(embedded.contains("Embedded agent"));
        assert!(!embedded.contains("--connect"));
        assert!(!embedded.contains("reconnect"));
    }

    #[tokio::test]
    async fn ctrl_g_toggles_the_btw_workspace_without_cancelling_streaming() {
        let mut app = test_tui_app();
        app.state.session_id = Some("session-btw".into());
        app.state.btw.streaming = true;

        app.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL))
            .await
            .unwrap();
        assert_eq!(app.state.mode, AppMode::Btw);
        assert!(app.state.btw.open);
        assert!(app.state.btw.streaming);

        app.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL))
            .await
            .unwrap();
        assert_eq!(app.state.mode, AppMode::Normal);
        assert!(!app.state.btw.open);
        assert!(app.state.btw.streaming);
    }

    #[tokio::test]
    async fn enter_queues_another_btw_question_while_streaming() {
        let mut app = test_tui_app();
        app.state.session_id = Some("session-btw".into());
        app.enter_btw_view();
        app.state.btw.streaming = true;
        app.state.input.set_text("follow-up".into());

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();

        assert!(app.state.input.is_empty());
        assert_eq!(app.state.btw.pending_questions.len(), 1);
        assert_eq!(app.state.btw.pending_questions[0].question, "follow-up");
        assert!(app.state.prompt_queue.is_empty());
    }

    #[tokio::test]
    async fn slash_btw_without_a_question_opens_the_workspace() {
        let mut app = test_tui_app();
        app.state.input.set_text("/btw".into());

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();

        assert_eq!(app.state.mode, AppMode::Btw);
        assert!(app.state.btw.open);
    }

    #[tokio::test]
    async fn accepted_partial_slash_command_clears_the_composer_immediately() {
        let mut app = test_tui_app();
        let session_id = format!("session-slash-{}", uuid::Uuid::new_v4());
        app.state.session_id = Some(session_id.clone());
        app.state.session_views.insert(
            session_id.clone(),
            crate::session_view::SessionViewState {
                draft: "/sess".into(),
                cursor: 5,
                ..Default::default()
            },
        );
        crate::draft_store::save_draft(&session_id, "/sess", 5).unwrap();
        app.state.input.set_text("/sess".into());
        app.state.refresh_slash_menu();

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();

        assert!(app.state.input.is_empty());
        assert!(app.state.slash_menu.is_none());
        assert!(app
            .state
            .session_views
            .get(&session_id)
            .is_some_and(|view| view.draft.is_empty() && view.cursor == 0));
        assert!(crate::draft_store::load_draft(&session_id).is_none());
        assert!(app
            .state
            .list_picker
            .as_ref()
            .is_some_and(|picker| picker.kind == ListPickerKind::Session));
    }

    #[tokio::test]
    async fn session_picker_tab_toggles_all_workspaces_and_filters_by_workspace() {
        let mut app = test_tui_app();
        let current_workspace = app.state.working_dir.to_string_lossy().into_owned();
        let other_workspace = std::env::temp_dir()
            .join("kkagent-other-workspace-search-token")
            .to_string_lossy()
            .into_owned();
        app.state.session_id = Some("current-session".into());
        app.replace_list_picker(ListPickerState {
            kind: ListPickerKind::Session,
            title: String::new(),
            items: Vec::new(),
            selected: 0,
            filter: String::new(),
            all_items: Vec::new(),
        });
        app.apply_session_picker_list(serde_json::json!({
            "sessions": [
                {
                    "session_id": "current-session",
                    "working_dir": current_workspace,
                    "title": "current project",
                    "is_custom_title": true,
                    "empty": false
                },
                {
                    "session_id": "other-session",
                    "working_dir": other_workspace,
                    "title": "another project",
                    "is_custom_title": true,
                    "empty": false
                }
            ]
        }));

        let picker = app.state.list_picker.as_ref().unwrap();
        assert_eq!(picker.items.len(), 1);
        assert_eq!(picker.items[0].id, "current-session");
        assert!(picker.title.contains("current workspace"));

        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .await
            .unwrap();

        let picker = app.state.list_picker.as_mut().unwrap();
        assert_eq!(picker.items.len(), 2);
        assert!(picker.title.contains("all workspaces"));
        picker.filter = "other-workspace-search-token".into();
        app.apply_session_picker_filter();
        let picker = app.state.list_picker.as_ref().unwrap();
        assert_eq!(picker.items.len(), 1);
        assert_eq!(picker.items[0].id, "other-session");
    }

    #[tokio::test]
    async fn session_picker_stays_open_when_only_other_workspaces_have_sessions() {
        let mut app = test_tui_app();
        let other_workspace = std::env::temp_dir()
            .join("kkagent-only-other-workspace")
            .to_string_lossy()
            .into_owned();
        app.replace_list_picker(ListPickerState {
            kind: ListPickerKind::Session,
            title: String::new(),
            items: Vec::new(),
            selected: 0,
            filter: String::new(),
            all_items: Vec::new(),
        });

        app.apply_session_picker_list(serde_json::json!({
            "sessions": [{
                "session_id": "other-session",
                "working_dir": other_workspace,
                "title": "another project",
                "is_custom_title": true,
                "empty": false
            }]
        }));

        let picker = app.state.list_picker.as_ref().unwrap();
        assert!(picker.items.is_empty());
        assert!(picker.title.contains("Tab show all"));
        assert_eq!(app.state.session_picker_entries.len(), 1);
    }

    #[tokio::test]
    async fn cross_workspace_session_selection_resumes_in_its_own_workspace() {
        use futures::FutureExt;
        use std::sync::Arc;

        let target_workspace = std::env::temp_dir()
            .join("kkagent-resume-other-workspace")
            .to_string_lossy()
            .into_owned();
        let (client_transport, server_transport) =
            kkagent_rpc::transport::memory::create_memory_pair();
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(16);
        let rpc = kkagent_rpc::RpcClient::new(client_transport, event_tx);
        let client = kkagent_client::KkagentClient::new(rpc, event_rx);
        let (params_tx, mut params_rx) = tokio::sync::mpsc::unbounded_channel();
        let handler: kkagent_rpc::server::RequestHandler =
            Arc::new(move |_id, method, params, _event_tx| {
                let params_tx = params_tx.clone();
                async move {
                    if method == "session.resume" {
                        let _ = params_tx.send(params.unwrap_or_default());
                    }
                    Err((-32602, "test stop".into()))
                }
                .boxed()
            });
        tokio::spawn(async move {
            kkagent_rpc::RpcServer::new(handler)
                .serve(server_transport)
                .await;
        });
        let mut app = TuiApp::new(AppConfig::default(), client);
        app.state.session_id = Some("current-session".into());
        app.state.session_picker_entries = vec![SessionPickerEntry {
            item: ListPickerItem {
                id: "other-session".into(),
                label: "other".into(),
                detail: String::new(),
            },
            workspace: target_workspace.clone(),
            same_workspace: false,
        }];
        app.replace_list_picker(ListPickerState {
            kind: ListPickerKind::Session,
            title: String::new(),
            items: vec![app.state.session_picker_entries[0].item.clone()],
            selected: 0,
            filter: String::new(),
            all_items: Vec::new(),
        });

        app.apply_list_picker().await.unwrap();

        let params = tokio::time::timeout(std::time::Duration::from_secs(1), params_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(params["session_id"], "other-session");
        assert_eq!(params["workspace"], target_workspace);
    }

    #[tokio::test]
    async fn session_restore_drops_legacy_consumed_slash_drafts() {
        let mut app = test_tui_app();
        let session_id = format!("legacy-slash-{}", uuid::Uuid::new_v4());
        crate::draft_store::save_draft(&session_id, "/sessions ", 10).unwrap();

        app.restore_session_view(&session_id);

        assert!(app.state.input.is_empty());
        assert!(crate::draft_store::load_draft(&session_id).is_none());

        let argument_session = format!("argument-slash-{}", uuid::Uuid::new_v4());
        crate::draft_store::save_draft(&argument_session, "/compact ", 9).unwrap();
        app.restore_session_view(&argument_session);
        assert_eq!(app.state.input.text, "/compact ");
        crate::draft_store::clear_draft(&argument_session);
    }

    #[test]
    fn empty_composer_removes_the_persisted_session_draft() {
        let session_id = format!("empty-draft-{}", uuid::Uuid::new_v4());
        crate::draft_store::save_draft(&session_id, "stale input", 4).unwrap();
        let input = crate::input::InputState::new();

        persist_composer_draft(&session_id, &input);

        assert!(crate::draft_store::load_draft(&session_id).is_none());
    }

    #[tokio::test]
    async fn selecting_the_current_session_does_not_resume_stale_cached_state() {
        let mut app = test_tui_app();
        app.state.session_id = Some("current-session".into());
        app.state.session_runtime_states.insert(
            "current-session".into(),
            SessionRuntimeState::capture(&app.state),
        );
        app.replace_list_picker(ListPickerState {
            kind: ListPickerKind::Session,
            title: "Sessions".into(),
            items: vec![ListPickerItem {
                id: "current-session".into(),
                label: "current-session".into(),
                detail: String::new(),
            }],
            selected: 0,
            filter: String::new(),
            all_items: Vec::new(),
        });

        app.apply_list_picker().await.unwrap();

        assert!(app.state.input.is_empty());
        assert!(app
            .state
            .session_runtime_states
            .contains_key("current-session"));
        assert!(app.state.list_picker.is_none());
    }

    #[tokio::test]
    async fn partial_slash_command_that_needs_arguments_keeps_the_completion() {
        let mut app = test_tui_app();
        app.state.input.set_text("/comp".into());
        app.state.refresh_slash_menu();

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();

        assert_eq!(app.state.input.text, "/compact ");
        assert!(app.state.slash_menu.is_none());
    }

    #[tokio::test]
    async fn selecting_a_real_session_does_not_hide_the_btw_surface() {
        let mut app = test_tui_app();
        app.state.session_id = Some("session-btw".into());
        app.state.workspace_sessions.set_entries(
            vec![crate::chrome::WorkspaceSessionEntry {
                id: "session-btw".into(),
                title: "main".into(),
                status: SessionStatus::Idle,
                dirty: false,
                needs_attention: false,
                working_dir: None,
            }],
            Some("session-btw"),
        );
        app.enter_btw_view();

        app.activate_workspace_target("session-btw").await.unwrap();

        assert_eq!(app.state.mode, AppMode::Btw);
        assert!(app.state.btw.open);
        assert_eq!(
            app.state.workspace_sessions.active_id(),
            Some("session-btw")
        );
    }

    #[tokio::test]
    async fn ctrl_d_clears_the_btw_workspace_without_deleting_the_real_session() {
        let mut app = test_tui_app();
        app.state.session_id = Some("real-session".into());
        app.enter_btw_view();
        app.state.btw.turns.push(crate::panes::BtwTurnView {
            question: "old question".into(),
            answer: "old answer".into(),
            thinking: Some("old thinking".into()),
        });
        app.state
            .btw
            .enqueue("real-session".into(), "queued".into());

        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL))
            .await
            .unwrap();

        assert_eq!(app.state.mode, AppMode::Normal);
        assert_eq!(app.state.session_id.as_deref(), Some("real-session"));
        assert!(app.state.btw.turns.is_empty());
        assert!(app.state.btw.pending_questions.is_empty());
        assert!(app.state.workspace_sessions.entries.is_empty());
    }

    #[tokio::test]
    async fn workspace_strip_keeps_all_open_sessions_until_ctrl_d_closes_them() {
        let mut app = test_tui_app();
        app.state.session_id = Some("current".into());
        app.state.tab_strip.ensure_active("current", "current");
        app.state.tab_strip.ensure_tab("running", "running");
        app.state
            .tab_strip
            .set_status("running", SessionStatus::ToolExecuting);
        app.state.tab_strip.ensure_tab("idle", "idle");

        app.apply_workspace_sessions_list(Some(serde_json::json!({
            "sessions": [
                {"session_id": "current", "title": "current", "empty": false}
            ]
        })));

        let visible_ids = app
            .state
            .workspace_sessions
            .entries
            .iter()
            .map(|entry| entry.id.as_str())
            .collect::<Vec<_>>();
        assert!(visible_ids.contains(&"current"));
        assert!(visible_ids.contains(&"running"));
        assert!(visible_ids.contains(&"idle"));
    }

    #[tokio::test]
    async fn stale_sessions_list_cannot_resurrect_a_closed_tab() {
        // Ctrl-D closed "current" (tombstoned) and the follow-up switch to
        // another session has not landed yet: state.session_id is still the
        // closed id. A stale sessions.list response must neither re-add the
        // row nor re-open the tab in tab_strip.
        let mut app = test_tui_app();
        app.state.session_id = Some("closed".into());
        app.state.tab_strip.ensure_active("closed", "closed");
        app.state.tab_strip.ensure_tab("other", "other");
        // Mirror confirm_delete_session's non-permanent branch: tab removed
        // from the strip, tombstone recorded, session switch still pending.
        app.state.tab_strip.tabs.retain(|t| t.id != "closed");
        app.state.closed_tab_ids.insert("closed".into());

        app.apply_workspace_sessions_list(Some(serde_json::json!({
            "sessions": [
                {"session_id": "closed", "title": "closed", "empty": false},
                {"session_id": "other", "title": "other", "empty": false}
            ]
        })));

        let visible_ids = app
            .state
            .workspace_sessions
            .entries
            .iter()
            .map(|entry| entry.id.as_str())
            .collect::<Vec<_>>();
        assert!(!visible_ids.contains(&"closed"));
        assert!(visible_ids.contains(&"other"));
        assert!(!app
            .state
            .tab_strip
            .tabs
            .iter()
            .any(|tab| tab.id == "closed"));
    }

    #[tokio::test]
    async fn resuming_a_session_clears_its_closed_tab_tombstone() {
        let mut app = test_tui_app();
        app.state.session_id = Some("current".into());
        app.state.tab_strip.ensure_active("current", "current");
        app.state.closed_tab_ids.insert("reopened".into());

        // Cached-switch path clears the tombstone when the session lands.
        app.activate_cached_session(
            "reopened",
            SessionRuntimeState::capture(&app.state),
            std::time::Instant::now(),
        );

        assert!(!app.state.closed_tab_ids.contains("reopened"));
        assert!(app
            .state
            .tab_strip
            .tabs
            .iter()
            .any(|tab| tab.id == "reopened"));
    }

    #[tokio::test]
    async fn switching_preserves_an_empty_open_session() {
        let mut app = test_tui_app();
        app.state.session_id = Some("running".into());
        app.state.status = SessionStatus::Idle;
        app.state.tab_strip.ensure_active("running", "running");

        let leaving_id = app.cache_active_session_state("target");

        assert_eq!(leaving_id.as_deref(), Some("running"));
        assert!(app
            .state
            .tab_strip
            .tabs
            .iter()
            .any(|tab| tab.id == "running"));
        assert!(app
            .state
            .session_runtime_states
            .get("running")
            .is_some_and(|runtime| runtime.status == SessionStatus::Idle));
    }

    #[tokio::test]
    async fn ctrl_d_stops_a_running_session_before_the_next_press_closes_it() {
        use futures::FutureExt;
        use std::sync::Arc;

        let (client_transport, server_transport) =
            kkagent_rpc::transport::memory::create_memory_pair();
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(16);
        let rpc = kkagent_rpc::RpcClient::new(client_transport, event_tx);
        let client = kkagent_client::KkagentClient::new(rpc, event_rx);
        let (interrupt_tx, mut interrupt_rx) = tokio::sync::mpsc::unbounded_channel();
        let handler: kkagent_rpc::server::RequestHandler =
            Arc::new(move |_id, method, params, _event_tx| {
                let interrupt_tx = interrupt_tx.clone();
                async move {
                    match method.as_str() {
                        "session.interrupt" => {
                            interrupt_tx.send(params.unwrap()).unwrap();
                            Ok(serde_json::json!({"ok": true}))
                        }
                        "sessions.list" => Ok(serde_json::json!({"sessions": []})),
                        other => panic!("unexpected RPC method: {other}"),
                    }
                }
                .boxed()
            });
        tokio::spawn(async move {
            kkagent_rpc::RpcServer::new(handler)
                .serve(server_transport)
                .await;
        });

        let mut app = TuiApp::new(AppConfig::default(), client);
        let mut fallback_state = AppState::new(PermissionMode::Manual, false);
        fallback_state.session_id = Some("fallback".into());
        fallback_state.status = SessionStatus::Idle;
        app.state.session_runtime_states.insert(
            "fallback".into(),
            SessionRuntimeState::capture(&fallback_state),
        );

        app.state.session_id = Some("running".into());
        app.state.status = SessionStatus::Thinking;
        app.state.messages.push(DisplayMessage {
            role: MessageRole::User,
            content: "keep this session".into(),
            thinking: None,
            parts: Vec::new(),
            tool_calls: Vec::new(),
            delivery: crate::prompt_queue::DeliveryState::Sent,
            idempotency_key: None,
        });
        app.state.tab_strip.ensure_active("running", "running");
        app.state.tab_strip.ensure_tab("fallback", "fallback");
        app.state
            .tab_strip
            .set_status("running", SessionStatus::Thinking);
        app.state.workspace_sessions.set_entries(
            vec![
                crate::chrome::WorkspaceSessionEntry {
                    id: "running".into(),
                    title: "running".into(),
                    status: SessionStatus::Thinking,
                    dirty: false,
                    needs_attention: false,
                    working_dir: None,
                },
                crate::chrome::WorkspaceSessionEntry {
                    id: "fallback".into(),
                    title: "fallback".into(),
                    status: SessionStatus::Idle,
                    dirty: false,
                    needs_attention: false,
                    working_dir: None,
                },
            ],
            Some("running"),
        );

        let ctrl_d = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);
        app.handle_key(ctrl_d).await.unwrap();

        assert_eq!(app.state.status, SessionStatus::Cancelling);
        assert_eq!(app.state.session_id.as_deref(), Some("running"));
        assert!(app
            .state
            .workspace_sessions
            .entries
            .iter()
            .any(|entry| entry.id == "running"));
        assert_eq!(
            interrupt_rx
                .recv()
                .await
                .and_then(|params| params.get("session_id").cloned()),
            Some(serde_json::json!("running"))
        );

        app.handle_server_event(Frame::Event {
            event: "agent".into(),
            scope: None,
            data: serde_json::to_value(AgentEvent::TurnEnd {
                session_id: "running".into(),
            })
            .unwrap(),
        });
        assert_eq!(app.state.status, SessionStatus::Idle);

        app.handle_key(ctrl_d).await.unwrap();

        assert_eq!(app.state.session_id.as_deref(), Some("fallback"));
        assert!(!app
            .state
            .workspace_sessions
            .entries
            .iter()
            .any(|entry| entry.id == "running"));
        assert!(!app
            .state
            .tab_strip
            .tabs
            .iter()
            .any(|tab| tab.id == "running"));
        assert!(app.state.session_delete_confirm.is_none());
    }

    #[tokio::test]
    async fn usage_update_refreshes_latest_request_and_session_semantics() {
        let mut app = test_tui_app();
        app.state.session_id = Some("s".into());

        // Anthropic-style pure cache-read request: input excludes cache
        // buckets, no cache write happened, nearly everything hit the cache.
        let usage = kkagent_protocol::TokenUsage {
            input_tokens: 500,
            output_tokens: 200,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 95_000,
            input_includes_cache: Some(false),
        };
        app.handle_server_event(Frame::Event {
            event: "agent".into(),
            scope: None,
            data: serde_json::to_value(AgentEvent::UsageUpdate {
                session_id: "s".into(),
                usage: usage.clone(),
                context: None,
                steps: 3,
                turns: 1,
            })
            .unwrap(),
        });

        // Latest-request snapshot must update live (powers /usage "Latest
        // request" and the footer cache indicator), not only on session
        // switch.
        assert_eq!(app.state.last_step_usage, Some(usage));
        // Footer cache hit: denominator must include the cache buckets, so a
        // pure-read request stays ≤ 100%.
        let hit = app.state.status_bar.cache_hit.expect("cache hit shown");
        assert!(
            hit <= 1.0,
            "cache hit ratio must not exceed 100% (got {hit})"
        );
        // Session totals track the provider semantics flag for their own
        // ratio display.
        assert_eq!(app.state.usage_session.input_includes_cache, Some(false));
        assert_eq!(app.state.usage_session.cache_read_tokens, 95_000);
    }

    #[tokio::test]
    async fn background_btw_events_continue_updating_the_fixed_workspace() {
        let mut app = test_tui_app();
        app.state.session_id = Some("another-session".into());
        app.state.btw.begin_question("side question");
        app.state.btw.current_session_id = Some("original-session".into());
        app.state.btw.current_agent_id = Some("btw-agent".into());

        for event in [
            AgentEvent::BtwThinkingDelta {
                session_id: "original-session".into(),
                agent_id: "btw-agent".into(),
                text: "reasoning".into(),
            },
            AgentEvent::BtwDelta {
                session_id: "original-session".into(),
                agent_id: "btw-agent".into(),
                text: "answer".into(),
            },
            AgentEvent::BtwEnd {
                session_id: "original-session".into(),
                agent_id: "btw-agent".into(),
                error: None,
            },
        ] {
            app.handle_server_event(Frame::Event {
                event: "agent".into(),
                scope: None,
                data: serde_json::to_value(event).unwrap(),
            });
        }

        assert!(!app.state.btw.streaming);
        assert_eq!(app.state.btw.turns.len(), 1);
        assert_eq!(app.state.btw.turns[0].answer, "answer");
        assert_eq!(
            app.state.btw.turns[0].thinking.as_deref(),
            Some("reasoning")
        );
        assert!(!app
            .state
            .background_session_events
            .contains_key("original-session"));
    }

    #[test]
    fn btw_workspace_is_not_captured_as_a_per_session_mode() {
        let mut state = AppState::new(PermissionMode::Manual, false);
        state.mode = AppMode::Btw;
        assert_eq!(SessionRuntimeState::capture(&state).mode, AppMode::Normal);

        state.plan_mode = true;
        assert_eq!(SessionRuntimeState::capture(&state).mode, AppMode::Plan);
    }

    #[tokio::test]
    async fn unrelated_btw_events_do_not_corrupt_the_active_answer() {
        let mut app = test_tui_app();
        app.state.btw.begin_question("active");
        app.state.btw.current_session_id = Some("active-session".into());
        app.state.btw.current_agent_id = Some("active-agent".into());

        app.handle_server_event(Frame::Event {
            event: "agent".into(),
            scope: None,
            data: serde_json::to_value(AgentEvent::BtwDelta {
                session_id: "other-session".into(),
                agent_id: "other-agent".into(),
                text: "wrong answer".into(),
            })
            .unwrap(),
        });

        assert!(app.state.btw.current_answer.is_empty());
        assert!(app.state.btw.streaming);
    }

    #[tokio::test]
    async fn stale_btw_agent_events_do_not_corrupt_a_replacement_in_the_same_session() {
        let mut app = test_tui_app();
        app.state.btw.begin_question("replacement");
        app.state.btw.current_session_id = Some("same-session".into());
        app.state.btw.current_agent_id = Some("new-agent".into());

        app.handle_server_event(Frame::Event {
            event: "agent".into(),
            scope: None,
            data: serde_json::to_value(AgentEvent::BtwEnd {
                session_id: "same-session".into(),
                agent_id: "old-agent".into(),
                error: Some("cancelled".into()),
            })
            .unwrap(),
        });

        assert!(app.state.btw.streaming);
        assert_eq!(app.state.btw.current_question, "replacement");
        assert!(app.state.btw.turns.is_empty());
    }

    #[tokio::test]
    async fn ctrl_s_steers_instead_of_queueing_while_turn_runs() {
        let mut app = test_tui_app();
        app.state.session_id = Some("session-steer".into());
        app.state.status = SessionStatus::Thinking;
        app.state.input.set_text("focus on the failing test".into());

        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
            .await
            .unwrap();

        assert!(app.state.input.is_empty());
        assert!(app.state.prompt_queue.is_empty());
        assert_eq!(app.state.status, SessionStatus::Thinking);
        let user = app
            .state
            .messages
            .iter()
            .find(|message| message.role == MessageRole::User)
            .unwrap();
        assert_eq!(user.content, "focus on the failing test");
        assert_eq!(user.delivery, crate::prompt_queue::DeliveryState::Sending);
        assert_eq!(
            app.jobs
                .pending
                .get(&crate::async_jobs::JobChannel::Prompt)
                .and_then(|job| job.retry_method.as_deref()),
            Some("session.steer")
        );
    }

    #[tokio::test]
    async fn ctrl_s_steers_queued_prompts_before_the_editor_draft() {
        use futures::FutureExt;
        use std::sync::Arc;

        let (client_transport, server_transport) =
            kkagent_rpc::transport::memory::create_memory_pair();
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(16);
        let rpc = kkagent_rpc::RpcClient::new(client_transport, event_tx);
        let client = kkagent_client::KkagentClient::new(rpc, event_rx);
        let (params_tx, mut params_rx) = tokio::sync::mpsc::unbounded_channel();
        let handler: kkagent_rpc::server::RequestHandler =
            Arc::new(move |_id, method, params, _event_tx| {
                let params_tx = params_tx.clone();
                async move {
                    assert_eq!(method, "session.steer");
                    params_tx.send(params.unwrap()).unwrap();
                    Ok(serde_json::json!({"ok": true}))
                }
                .boxed()
            });
        tokio::spawn(async move {
            kkagent_rpc::RpcServer::new(handler)
                .serve(server_transport)
                .await;
        });

        let mut app = TuiApp::new(AppConfig::default(), client);
        app.state.session_id = Some("session-steer-queue".into());
        app.state.status = SessionStatus::ToolExecuting;
        for text in ["queued first", "queued second"] {
            app.state
                .prompt_queue
                .push(crate::prompt_queue::QueuedPrompt::next_turn(
                    "session-steer-queue",
                    text,
                ));
            app.state.messages.push(DisplayMessage {
                role: MessageRole::User,
                content: text.into(),
                thinking: None,
                parts: Vec::new(),
                tool_calls: Vec::new(),
                delivery: crate::prompt_queue::DeliveryState::Queued,
                idempotency_key: None,
            });
        }
        app.state.input.set_text("editor draft".into());

        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
            .await
            .unwrap();

        let params = tokio::time::timeout(std::time::Duration::from_secs(1), params_rx.recv())
            .await
            .expect("steer RPC should arrive")
            .expect("parameter sender should stay alive");
        assert_eq!(
            params["text"],
            "queued first\n\nqueued second\n\neditor draft"
        );
        assert!(app.state.prompt_queue.is_empty());
        assert!(app.state.input.is_empty());
        let user_messages = app
            .state
            .messages
            .iter()
            .filter(|message| message.role == MessageRole::User)
            .collect::<Vec<_>>();
        assert_eq!(user_messages.len(), 3);
        assert!(user_messages.iter().all(|message| {
            message.delivery == crate::prompt_queue::DeliveryState::Sending
                && message.idempotency_key == user_messages[0].idempotency_key
        }));
    }

    #[tokio::test]
    async fn ctrl_s_steers_an_existing_queue_with_an_empty_editor() {
        let mut app = test_tui_app();
        app.state.session_id = Some("session-steer-queue".into());
        app.state.status = SessionStatus::WaitingQuestion;
        app.state
            .prompt_queue
            .push(crate::prompt_queue::QueuedPrompt::next_turn(
                "session-steer-queue",
                "answer without waiting",
            ));
        app.state.messages.push(DisplayMessage {
            role: MessageRole::User,
            content: "answer without waiting".into(),
            thinking: None,
            parts: Vec::new(),
            tool_calls: Vec::new(),
            delivery: crate::prompt_queue::DeliveryState::Queued,
            idempotency_key: None,
        });

        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
            .await
            .unwrap();

        assert!(app.state.prompt_queue.is_empty());
        assert_eq!(app.state.status, SessionStatus::WaitingQuestion);
        assert_eq!(
            app.jobs
                .pending
                .get(&crate::async_jobs::JobChannel::Prompt)
                .and_then(|job| job.retry_method.as_deref()),
            Some("session.steer")
        );
    }

    #[tokio::test]
    async fn stream_deltas_after_steer_stay_on_the_original_assistant_message() {
        let mut app = test_tui_app();
        app.state.session_id = Some("session-steer-stream".into());
        app.state.status = SessionStatus::Thinking;
        for event in [
            AgentEvent::TurnStart {
                session_id: "session-steer-stream".into(),
            },
            AgentEvent::MessageDelta {
                session_id: "session-steer-stream".into(),
                text: "initial".into(),
            },
        ] {
            app.handle_server_event(Frame::Event {
                event: "agent".into(),
                scope: None,
                data: serde_json::to_value(event).unwrap(),
            });
        }
        app.handle_server_event(Frame::Event {
            event: "agent".into(),
            scope: None,
            data: serde_json::to_value(AgentEvent::ToolCall {
                session_id: "session-steer-stream".into(),
                tool_call_id: "tool-1".into(),
                tool_name: "Read".into(),
                input: serde_json::json!({"path": "src/main.rs"}),
            })
            .unwrap(),
        });
        app.state.input.set_text("new direction".into());
        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
            .await
            .unwrap();

        app.handle_server_event(Frame::Event {
            event: "agent".into(),
            scope: None,
            data: serde_json::to_value(AgentEvent::ToolResult {
                session_id: "session-steer-stream".into(),
                tool_call_id: "tool-1".into(),
                tool_name: "Read".into(),
                output: "file contents".into(),
                is_error: false,
            })
            .unwrap(),
        });
        app.handle_server_event(Frame::Event {
            event: "agent".into(),
            scope: None,
            data: serde_json::to_value(AgentEvent::MessageDelta {
                session_id: "session-steer-stream".into(),
                text: " tail".into(),
            })
            .unwrap(),
        });

        let assistants = app
            .state
            .messages
            .iter()
            .filter(|message| message.role == MessageRole::Assistant)
            .collect::<Vec<_>>();
        assert_eq!(assistants.len(), 1);
        assert_eq!(assistants[0].content, "initial tail");
        assert!(assistants[0].parts.iter().any(|part| matches!(
            part,
            DisplayPart::Tool(tool) if tool.id == "tool-1"
                && tool.output.as_deref() == Some("file contents")
        )));

        app.handle_server_event(Frame::Event {
            event: "agent".into(),
            scope: None,
            data: serde_json::to_value(AgentEvent::TurnStart {
                session_id: "session-steer-stream".into(),
            })
            .unwrap(),
        });
        app.handle_server_event(Frame::Event {
            event: "agent".into(),
            scope: None,
            data: serde_json::to_value(AgentEvent::MessageDelta {
                session_id: "session-steer-stream".into(),
                text: "guided".into(),
            })
            .unwrap(),
        });
        assert_eq!(
            app.state
                .messages
                .iter()
                .filter(|message| message.role == MessageRole::Assistant)
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn shift_enter_inserts_newline_even_while_turn_runs() {
        let mut app = test_tui_app();
        app.state.session_id = Some("session-running".into());
        app.state.status = SessionStatus::Thinking;
        app.state.input.set_text("first line".into());

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT))
            .await
            .unwrap();

        assert_eq!(app.state.input.text, "first line\n");
        assert!(app.state.messages.is_empty());
        assert!(app.state.prompt_queue.is_empty());
    }

    #[tokio::test]
    async fn enter_still_queues_next_turn_while_busy() {
        let mut app = test_tui_app();
        app.state.session_id = Some("session-queue".into());
        app.state.status = SessionStatus::ToolExecuting;
        app.state.input.set_text("do this afterward".into());

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();

        assert_eq!(app.state.prompt_queue.items.len(), 1);
        assert!(!app.state.prompt_queue.items[0].as_steer);
        assert_eq!(app.state.prompt_queue.items[0].text, "do this afterward");
    }

    #[test]
    fn resume_payload_restores_full_plan_document() {
        let data = serde_json::json!({
            "plan_mode": true,
            "plan": {
                "id": "2026-08-11_resume_plan",
                "path": "/sessions/s1/agents/main/plans/2026-08-11_resume_plan.md",
                "content": "# Resume plan\n\n1. Keep every step.\n2. Restore after restart.\n"
            }
        });
        let plan = plan_document_from_resume(&data).expect("plan should be restored");
        assert_eq!(
            plan.path,
            "/sessions/s1/agents/main/plans/2026-08-11_resume_plan.md"
        );
        assert!(plan.content.contains("2. Restore after restart."));
    }

    #[test]
    fn resume_payload_ignores_empty_plan_placeholder() {
        let data = serde_json::json!({
            "plan": {"path": "/plans/empty.md", "content": "  \n"}
        });
        assert!(plan_document_from_resume(&data).is_none());
    }

    #[tokio::test]
    async fn resume_restores_plan_confirmation_and_todo_progress() {
        let mut app = test_tui_app();
        let request: kkagent_protocol::ApprovalRequest =
            serde_json::from_value(serde_json::json!({
                "approval_id": "approval-resumed",
                "session_id": "session-resumed",
                "tool_call_id": "exit-plan",
                "tool_name": "ExitPlanMode",
                "action": "review",
                "tool_input_display": {
                "kind": "plan_review",
                "plan": "# Resume plan\n\nContinue safely.",
                "path": "/plans/resume.md",
                },
                "created_at": "2026-08-11T00:00:00Z",
            }))
            .unwrap();
        app.apply_session_resume_data(
            "session-resumed",
            serde_json::json!({
                "session_id": "session-resumed",
                "messages": [],
                "plan_mode": true,
                "plan": {
                    "path": "/plans/resume.md",
                    "content": "# Resume plan\n\nContinue safely."
                },
                "pending_approval": request,
                "pending_approval_resumed": true,
                "todos": [
                    {"id": "1", "content": "Done", "status": "completed"},
                    {"id": "2", "content": "Continue", "status": "in_progress"}
                ]
            }),
        )
        .unwrap();

        assert!(app.state.plan_focus_active());
        assert_eq!(app.state.status, SessionStatus::WaitingApproval);
        assert!(app
            .state
            .approval_pending
            .as_ref()
            .is_some_and(|approval| approval.resumed_plan_review));
        assert_eq!(app.state.todos.len(), 2);
        assert_eq!(app.state.todos[1].status, "in_progress");
    }

    #[tokio::test]
    async fn resume_with_empty_todos_resets_expanded_panel_state() {
        let mut app = test_tui_app();
        app.state.todos_expanded = true;

        app.apply_session_resume_data(
            "session-empty-todos",
            serde_json::json!({
                "session_id": "session-empty-todos",
                "messages": [],
                "todos": []
            }),
        )
        .unwrap();

        assert!(app.state.todos.is_empty());
        assert!(!app.state.todos_expanded);
    }

    #[tokio::test]
    async fn resume_restores_ask_user_question_panel() {
        let mut app = test_tui_app();
        app.apply_session_resume_data(
            "session-question",
            serde_json::json!({
                "session_id": "session-question",
                "messages": [],
                "turn_active": true,
                "status": "thinking",
                "pending_question": {
                    "question_id": "q-1",
                    "text": "Pick a path",
                    "options": [
                        {"id": "a", "label": "Alpha"},
                        {"id": "b", "label": "Beta"}
                    ],
                    "allow_free_text": false,
                    "allow_multiple": false
                }
            }),
        )
        .unwrap();

        assert_eq!(app.state.status, SessionStatus::WaitingQuestion);
        let question = app.state.question_pending.expect("question panel");
        assert_eq!(question.question_id, "q-1");
        assert_eq!(question.text, "Pick a path");
        assert_eq!(question.options.len(), 2);
        assert_eq!(question.options[0].1, "Alpha");
    }

    #[tokio::test]
    async fn resume_restores_live_stream_and_btw_panel() {
        let mut app = test_tui_app();
        app.apply_session_resume_data(
            "session-live",
            serde_json::json!({
                "session_id": "session-live",
                "messages": [],
                "turn_active": true,
                "status": "tool_executing",
                "live_ui": {
                    "thinking_text": "",
                    "assistant_text": "partial answer",
                    "llm_retry": {
                        "retry_number": 2,
                        "reason": "HTTP 429",
                        "remaining_seconds": 3
                    }
                },
                "pending_btw": {
                    "agent_id": "btw-1",
                    "question": "side q",
                    "answer": "side a",
                    "thinking": "",
                    "streaming": true,
                    "turns": [{"question": "old", "answer": "done"}],
                    "retry_status": null
                }
            }),
        )
        .unwrap();

        assert_eq!(app.state.status, SessionStatus::ToolExecuting);
        assert!(app
            .state
            .messages
            .iter()
            .any(|m| { m.role == MessageRole::Assistant && m.content == "partial answer" }));
        assert!(app.state.active_assistant_message.is_some());
        assert_eq!(app.state.mode, AppMode::Btw);
        assert!(app.state.btw.streaming);
        assert_eq!(app.state.btw.current_question, "side q");
        assert_eq!(app.state.btw.current_answer, "side a");
        assert_eq!(app.state.btw.turns.len(), 1);
    }

    #[tokio::test]
    async fn resume_restores_prompt_queue_and_subagent_summary() {
        let mut app = test_tui_app();
        app.apply_session_resume_data(
            "session-queue",
            serde_json::json!({
                "session_id": "session-queue",
                "messages": [],
                "turn_active": true,
                "status": "thinking",
                "prompt_queue": {
                    "selected": 0,
                    "items": [{
                        "id": "q1",
                        "text": "follow up later",
                        "images": [],
                        "as_steer": false
                    }]
                },
                "pending_subagents": [{
                    "subagent_id": "sa-1",
                    "subagent_name": "explore",
                    "parent_tool_call_id": "tool-1",
                    "description": "scan files",
                    "status": "running",
                    "detail": null,
                    "recent_child_events": ["tool Read {\"path\":\"a.rs\"}"]
                }]
            }),
        )
        .unwrap();

        assert_eq!(app.state.prompt_queue.items.len(), 1);
        assert_eq!(app.state.prompt_queue.items[0].text, "follow up later");
        assert!(app.state.messages.iter().any(|m| {
            m.role == MessageRole::User
                && m.delivery == crate::prompt_queue::DeliveryState::Queued
                && m.content == "follow up later"
        }));
        assert_eq!(app.state.subagents.entries.len(), 1);
        assert_eq!(app.state.subagents.entries[0].id, "sa-1");
        assert_eq!(app.state.subagents.entries[0].name, "explore");
        assert_eq!(app.state.subagents.entries[0].status, "running");
        assert!(app.state.subagents.entries[0]
            .events
            .iter()
            .any(|line| line.contains("Read")));
        assert!(!app
            .state
            .messages
            .iter()
            .any(|m| m.role == MessageRole::System && m.content.contains("sa-1")));
    }

    fn pending_plan_revision() -> (PendingApproval, ApprovalChoice) {
        let choice = ApprovalChoice {
            label: "修改意见".into(),
            decision: kkagent_protocol::ApprovalDecision::Rejected,
            selected_label: "修改意见".into(),
            requires_feedback: true,
            scope: None,
        };
        (
            PendingApproval {
                approval_id: "approval-1".into(),
                tool_name: "ExitPlanMode".into(),
                action: "review".into(),
                detail: String::new(),
                selected: 0,
                choices: vec![choice.clone()],
                is_plan_review: true,
                hidden: false,
                resumed_plan_review: false,
                feedback_mode: true,
                feedback: String::new(),
            },
            choice,
        )
    }

    #[tokio::test]
    async fn plan_review_escape_folds_and_enter_restores_same_approval() {
        let mut app = test_tui_app();
        app.state.session_id = Some("session-1".into());
        app.state.status = SessionStatus::WaitingApproval;
        let (mut approval, _) = pending_plan_revision();
        approval.feedback_mode = false;
        app.state.approval_pending = Some(approval);

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .unwrap();

        assert_eq!(app.state.status, SessionStatus::WaitingApproval);
        assert!(app
            .state
            .approval_pending
            .as_ref()
            .is_some_and(|approval| approval.approval_id == "approval-1" && approval.hidden));

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();

        assert_eq!(app.state.status, SessionStatus::WaitingApproval);
        assert!(app
            .state
            .approval_pending
            .as_ref()
            .is_some_and(|approval| !approval.hidden));
    }

    #[tokio::test]
    async fn plan_review_feedback_escape_returns_to_choices_without_folding() {
        let mut app = test_tui_app();
        app.state.session_id = Some("session-1".into());
        app.state.status = SessionStatus::WaitingApproval;
        let (mut approval, _) = pending_plan_revision();
        approval.feedback = "draft feedback".into();
        app.state.approval_pending = Some(approval);

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .unwrap();

        let approval = app
            .state
            .approval_pending
            .as_ref()
            .expect("plan review should remain pending");
        assert!(!approval.hidden);
        assert!(!approval.feedback_mode);
        assert!(approval.feedback.is_empty());
        assert_eq!(app.state.status, SessionStatus::WaitingApproval);
    }

    #[tokio::test]
    async fn empty_plan_revision_feedback_keeps_editor_open() {
        let mut app = test_tui_app();
        app.state.session_id = Some("session-1".into());
        let (approval, choice) = pending_plan_revision();
        app.state.approval_pending = Some(approval);

        app.respond_approval_choice(choice, Some("   ".into()))
            .await
            .unwrap();

        assert!(app
            .state
            .approval_pending
            .as_ref()
            .is_some_and(|pending| pending.feedback_mode));
        assert!(app
            .jobs
            .notices
            .iter()
            .any(|notice| notice.text.contains("请先输入修改意见")));
    }

    #[tokio::test]
    async fn submitted_plan_revision_dismisses_old_plan_until_update() {
        use futures::FutureExt;
        use std::sync::Arc;

        let (client_transport, server_transport) =
            kkagent_rpc::transport::memory::create_memory_pair();
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(16);
        let rpc = kkagent_rpc::RpcClient::new(client_transport, event_tx);
        let client = kkagent_client::KkagentClient::new(rpc, event_rx);
        let handler: kkagent_rpc::server::RequestHandler =
            Arc::new(|_id, method, params, _event_tx| {
                async move {
                    assert_eq!(method, "approval.respond");
                    assert_eq!(
                        params
                            .as_ref()
                            .and_then(|value| value.get("feedback"))
                            .and_then(|value| value.as_str()),
                        Some("please revise step two")
                    );
                    Ok(serde_json::json!({"ok": true}))
                }
                .boxed()
            });
        tokio::spawn(async move {
            kkagent_rpc::RpcServer::new(handler)
                .serve(server_transport)
                .await;
        });
        let mut app = TuiApp::new(AppConfig::default(), client);
        app.state.session_id = Some("session-1".into());
        app.state.status = SessionStatus::WaitingApproval;
        app.state.on_plan_mode_changed(true);
        app.state.apply_plan_document(
            "/plans/old.md".into(),
            "# Old plan\n\nPrevious content.".into(),
        );
        assert!(app.state.plan_focus_active());
        let (approval, choice) = pending_plan_revision();
        app.state.approval_pending = Some(approval);

        app.respond_approval_choice(choice, Some("please revise step two".into()))
            .await
            .unwrap();

        assert!(app.state.approval_pending.is_none());
        assert_eq!(app.state.status, SessionStatus::Thinking);
        assert!(app.state.plan_mode);
        assert!(app.state.plan_document.is_none());
        assert!(!app.state.plan_focus_active());
        assert!(app.state.follow_bottom);
        assert!(app
            .jobs
            .notices
            .iter()
            .any(|notice| notice.text.contains("正在更新计划")));

        app.state.apply_plan_document(
            "/plans/revised.md".into(),
            "# Revised plan\n\nUpdated content.".into(),
        );
        assert!(app.state.plan_focus_active());
        assert_eq!(
            app.state
                .plan_document
                .as_ref()
                .map(|plan| plan.path.as_str()),
            Some("/plans/revised.md")
        );
    }

    #[tokio::test]
    async fn resumed_plan_revision_uses_restart_safe_resolver() {
        use futures::FutureExt;
        use std::sync::Arc;

        let (client_transport, server_transport) =
            kkagent_rpc::transport::memory::create_memory_pair();
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(16);
        let rpc = kkagent_rpc::RpcClient::new(client_transport, event_tx);
        let client = kkagent_client::KkagentClient::new(rpc, event_rx);
        let handler: kkagent_rpc::server::RequestHandler =
            Arc::new(|_id, method, params, _event_tx| {
                async move {
                    assert_eq!(method, "session.resolve_pending_plan_review");
                    assert_eq!(params.as_ref().unwrap()["session_id"], "session-1");
                    assert_eq!(
                        params.as_ref().unwrap()["feedback"],
                        "please revise after restart"
                    );
                    Ok(serde_json::json!({
                        "ok": true,
                        "turn_started": true,
                        "plan_mode": true,
                    }))
                }
                .boxed()
            });
        tokio::spawn(async move {
            kkagent_rpc::RpcServer::new(handler)
                .serve(server_transport)
                .await;
        });
        let mut app = TuiApp::new(AppConfig::default(), client);
        app.state.session_id = Some("session-1".into());
        app.state.status = SessionStatus::WaitingApproval;
        app.state.on_plan_mode_changed(true);
        app.state
            .apply_plan_document("/plans/old.md".into(), "# Old plan".into());
        let (mut approval, choice) = pending_plan_revision();
        approval.resumed_plan_review = true;
        app.state.approval_pending = Some(approval);

        app.respond_approval_choice(choice, Some("please revise after restart".into()))
            .await
            .unwrap();

        assert!(app.state.approval_pending.is_none());
        assert_eq!(app.state.status, SessionStatus::Thinking);
        assert!(app.state.plan_mode);
        assert!(app.state.plan_document.is_none());
    }

    #[tokio::test]
    async fn failed_plan_revision_keeps_old_plan_and_approval_visible() {
        use futures::FutureExt;
        use std::sync::Arc;

        let (client_transport, server_transport) =
            kkagent_rpc::transport::memory::create_memory_pair();
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(16);
        let rpc = kkagent_rpc::RpcClient::new(client_transport, event_tx);
        let client = kkagent_client::KkagentClient::new(rpc, event_rx);
        let handler: kkagent_rpc::server::RequestHandler =
            Arc::new(|_id, method, _params, _event_tx| {
                async move {
                    assert_eq!(method, "approval.respond");
                    Err((-32000, "delivery failed".into()))
                }
                .boxed()
            });
        tokio::spawn(async move {
            kkagent_rpc::RpcServer::new(handler)
                .serve(server_transport)
                .await;
        });
        let mut app = TuiApp::new(AppConfig::default(), client);
        app.state.session_id = Some("session-1".into());
        app.state.status = SessionStatus::WaitingApproval;
        app.state.on_plan_mode_changed(true);
        app.state
            .apply_plan_document("/plans/old.md".into(), "# Old plan".into());
        let (approval, choice) = pending_plan_revision();
        app.state.approval_pending = Some(approval);

        app.respond_approval_choice(choice, Some("please revise".into()))
            .await
            .unwrap();

        assert!(app.state.approval_pending.is_some());
        assert_eq!(app.state.status, SessionStatus::WaitingApproval);
        assert!(app.state.plan_focus_active());
        assert_eq!(
            app.state
                .plan_document
                .as_ref()
                .map(|plan| plan.path.as_str()),
            Some("/plans/old.md")
        );
        assert!(app.state.messages.iter().any(|message| {
            message.role == MessageRole::System && message.content.contains("Approval failed")
        }));
    }

    #[tokio::test]
    async fn failed_plan_mode_rpc_keeps_tui_state_and_session_alive() {
        use futures::FutureExt;
        use std::sync::Arc;

        let (client_transport, server_transport) =
            kkagent_rpc::transport::memory::create_memory_pair();
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(16);
        let rpc = kkagent_rpc::RpcClient::new(client_transport, event_tx);
        let client = kkagent_client::KkagentClient::new(rpc, event_rx);
        let handler: kkagent_rpc::server::RequestHandler =
            Arc::new(|_id, method, _params, _event_tx| {
                async move {
                    assert_eq!(method, "session.set_plan_mode");
                    Err((-32602, "Session not found: session-1".into()))
                }
                .boxed()
            });
        tokio::spawn(async move {
            kkagent_rpc::RpcServer::new(handler)
                .serve(server_transport)
                .await;
        });
        let mut app = TuiApp::new(AppConfig::default(), client);
        app.state.session_id = Some("session-1".into());
        app.state.plan_mode = false;

        assert!(!app.set_plan_mode_from_ui(true).await);
        assert!(!app.state.plan_mode);
        assert!(!app.state.should_quit);
        assert!(app.state.messages.iter().any(|message| {
            message.role == MessageRole::System
                && message.content.contains("Plan mode change failed")
        }));
    }

    #[test]
    fn parses_and_summarizes_history_edit_turns() {
        let turns = history_edit_turns_from_json(&serde_json::json!({
            "turns": [
                {"turn_index": 1, "message_index": 4, "text": "second\nline"},
                {"turn_index": 0, "message_index": 0, "text": "first"},
                {"turn_index": 2, "message_index": 9, "text": "  "},
            ]
        }));
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].text, "first");
        assert_eq!(turns[1].message_index, 4);
        assert_eq!(history_turn_summary("second\nline", 20), "second");
        assert_eq!(history_turn_summary("abcdef", 4), "abc…");
    }

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

    #[tokio::test]
    async fn background_session_events_are_replayed_without_polluting_active_content() {
        let mut app = test_tui_app();
        app.state.session_id = Some("a".into());
        app.state.status = SessionStatus::Thinking;
        app.state.messages.push(DisplayMessage {
            role: MessageRole::User,
            content: "run a".into(),
            thinking: None,
            parts: Vec::new(),
            tool_calls: Vec::new(),
            delivery: crate::prompt_queue::DeliveryState::Sent,
            idempotency_key: None,
        });
        app.cache_active_session_state("b");

        app.state.session_id = Some("b".into());
        app.state.status = SessionStatus::Idle;
        app.state.messages = vec![DisplayMessage {
            role: MessageRole::User,
            content: "stay in b".into(),
            thinking: None,
            parts: Vec::new(),
            tool_calls: Vec::new(),
            delivery: crate::prompt_queue::DeliveryState::Sent,
            idempotency_key: None,
        }];

        for event in [
            AgentEvent::MessageDelta {
                session_id: "a".into(),
                text: "answer".into(),
            },
            AgentEvent::ToolCall {
                session_id: "a".into(),
                tool_call_id: "tool-1".into(),
                tool_name: "Read".into(),
                input: serde_json::json!({"path": "a.rs"}),
            },
            AgentEvent::ToolResult {
                session_id: "a".into(),
                tool_call_id: "tool-1".into(),
                tool_name: "Read".into(),
                output: "file body".into(),
                is_error: false,
            },
            AgentEvent::TodoUpdated {
                session_id: "a".into(),
                items: vec![kkagent_protocol::TodoItemEvent {
                    id: "todo-1".into(),
                    content: "finish".into(),
                    status: "in_progress".into(),
                }],
            },
            AgentEvent::StatusUpdate {
                session_id: "a".into(),
                status: SessionStatus::Idle,
            },
        ] {
            app.handle_server_event(Frame::Event {
                event: "agent".into(),
                scope: None,
                data: serde_json::to_value(event).unwrap(),
            });
        }

        assert_eq!(app.state.messages.len(), 1);
        assert_eq!(app.state.messages[0].content, "stay in b");

        app.resume_session("a").await.unwrap();

        assert_eq!(app.state.messages.len(), 2);
        let assistant = app.state.messages.last().unwrap();
        assert_eq!(assistant.content, "answer");
        assert!(matches!(
            assistant.parts.get(1),
            Some(DisplayPart::Tool(tool)) if tool.output.as_deref() == Some("file body")
        ));
        assert_eq!(app.state.todos.len(), 1);
        assert_eq!(app.state.todos[0].content, "finish");
        assert_eq!(app.state.status, SessionStatus::Idle);
        assert!(!app.state.background_session_events.contains_key("a"));
    }

    #[tokio::test]
    async fn completed_todo_update_collapses_the_panel() {
        let mut app = test_tui_app();
        app.state.session_id = Some("todo-session".into());
        app.state.todos_expanded = true;

        app.handle_server_event(Frame::Event {
            event: "agent".into(),
            scope: None,
            data: serde_json::to_value(AgentEvent::TodoUpdated {
                session_id: "todo-session".into(),
                items: vec![kkagent_protocol::TodoItemEvent {
                    id: "todo-1".into(),
                    content: "finish".into(),
                    status: "completed".into(),
                }],
            })
            .unwrap(),
        });

        assert_eq!(app.state.todos.len(), 1);
        assert!(!app.state.todos_expanded);
    }

    #[tokio::test]
    async fn background_session_event_queue_is_bounded() {
        let mut app = test_tui_app();
        for index in 0..300 {
            app.queue_background_session_event(
                "background".into(),
                AgentEvent::MessageDelta {
                    session_id: "background".into(),
                    text: format!("event-{index}"),
                },
            );
        }
        let events = &app.state.background_session_events["background"];
        assert_eq!(events.len(), 256);
        assert!(app.state.background_session_event_bytes["background"] < 2 * 1024 * 1024);

        app.queue_background_session_event(
            "background".into(),
            AgentEvent::MessageDelta {
                session_id: "background".into(),
                text: "x".repeat(2 * 1024 * 1024),
            },
        );
        assert_eq!(app.state.background_session_events["background"].len(), 256);

        for index in 0..20 {
            app.queue_background_session_event(
                format!("session-{index}"),
                AgentEvent::StatusUpdate {
                    session_id: format!("session-{index}"),
                    status: SessionStatus::Idle,
                },
            );
        }
        assert!(app.state.background_session_events.len() <= 16);
        assert!(app.state.background_session_event_bytes.len() <= 16);
    }

    #[test]
    fn session_runtime_state_keeps_prompt_queues_isolated() {
        let mut state = AppState::new(PermissionMode::Manual, false);
        state.session_id = Some("a".into());
        state
            .prompt_queue
            .push(crate::prompt_queue::QueuedPrompt::next_turn(
                "a",
                "next for a",
            ));
        state.todos.push(TodoItem {
            id: "a-todo".into(),
            content: "only a".into(),
            status: "pending".into(),
        });
        let cached_a = SessionRuntimeState::capture(&state);

        state.session_id = Some("b".into());
        state.prompt_queue = crate::prompt_queue::PromptQueue::default();
        state.todos.clear();
        assert!(state.prompt_queue.is_empty());

        cached_a.restore(&mut state);
        assert_eq!(state.prompt_queue.items.len(), 1);
        assert_eq!(state.prompt_queue.items[0].session_id, "a");
        assert_eq!(state.todos[0].content, "only a");
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
            user_overridden: false,
            queued_behind: None,
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
            user_overridden: false,
            queued_behind: None,
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

    fn tool_history_message(id: usize) -> DisplayMessage {
        DisplayMessage {
            role: MessageRole::Assistant,
            content: String::new(),
            thinking: None,
            parts: vec![DisplayPart::ToolHistory(ToolHistorySummary {
                tool_count: 1,
                duration_ms: 0,
                tokens: 0,
                expanded: false,
                user_overridden: false,
                tools: vec![DisplayToolCall {
                    id: format!("tool-{id}"),
                    name: "Bash".into(),
                    input_summary: "printf test".into(),
                    output: Some("one\ntwo".into()),
                    is_error: false,
                    collapsed: true,
                    user_overridden: false,
                    started_at: None,
                    stopping: false,
                    queued_behind: None,
                }],
            })],
            tool_calls: Vec::new(),
            delivery: crate::prompt_queue::DeliveryState::Sent,
            idempotency_key: None,
        }
    }

    #[tokio::test]
    async fn ctrl_o_updates_only_the_latest_five_turns_and_live_tools() {
        let mut app = test_tui_app();
        for turn in 0..6 {
            app.state.messages.push(DisplayMessage {
                role: MessageRole::User,
                content: format!("turn {turn}"),
                thinking: None,
                parts: Vec::new(),
                tool_calls: Vec::new(),
                delivery: crate::prompt_queue::DeliveryState::Sent,
                idempotency_key: None,
            });
            app.state.messages.push(tool_history_message(turn));
        }
        if let Some(DisplayPart::ToolHistory(history)) =
            app.state.messages.last_mut().unwrap().parts.first_mut()
        {
            let mut live = history.tools[0].clone();
            live.id = "live-tool".into();
            app.state
                .messages
                .last_mut()
                .unwrap()
                .parts
                .push(DisplayPart::Tool(live));
        }

        app.toggle_tool_folding();

        let histories = app
            .state
            .messages
            .iter()
            .flat_map(|message| message.parts.iter())
            .filter_map(|part| match part {
                DisplayPart::ToolHistory(history) => Some(history),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(histories.len(), 6);
        assert!(
            !histories[0].expanded,
            "the sixth-oldest turn is outside the window"
        );
        assert!(histories[1..].iter().all(|history| history.expanded));
        assert!(matches!(
            app.state.messages.last().unwrap().parts.last(),
            Some(DisplayPart::Tool(tool)) if !tool.collapsed
        ));

        app.toggle_tool_folding();
        assert!(app
            .state
            .messages
            .iter()
            .flat_map(|message| message.parts.iter())
            .filter_map(|part| match part {
                DisplayPart::ToolHistory(history) => Some(history),
                _ => None,
            })
            .all(|history| !history.expanded));
    }

    #[tokio::test]
    async fn clicking_tool_hint_creates_a_persistent_per_item_override() {
        let mut app = test_tui_app();
        app.state.messages.push(DisplayMessage {
            role: MessageRole::User,
            content: "run it".into(),
            thinking: None,
            parts: Vec::new(),
            tool_calls: Vec::new(),
            delivery: crate::prompt_queue::DeliveryState::Sent,
            idempotency_key: None,
        });
        app.state.messages.push(tool_history_message(0));
        app.state.transcript_area = ratatui::layout::Rect::new(0, 0, 80, 10);
        app.state.content_lines = 20;
        app.state.viewport_height = 10;
        app.state.scroll_up = 5;
        app.state.tool_expand_hits.push(ToolExpandHit {
            line: 7,
            target: ToolExpandTarget::Part {
                message: 1,
                part: 0,
            },
        });

        let mut scroll_delta = 0;
        app.collect_mouse(
            crossterm::event::MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 10,
                row: 2,
                modifiers: KeyModifiers::NONE,
            },
            &mut scroll_delta,
        );

        let history = match &app.state.messages[1].parts[0] {
            DisplayPart::ToolHistory(history) => history,
            _ => panic!("expected tool history"),
        };
        assert!(history.expanded);
        assert!(history.user_overridden);
        assert!(app.state.selection.is_none());

        app.toggle_tool_folding();
        app.toggle_tool_folding();
        let history = match &app.state.messages[1].parts[0] {
            DisplayPart::ToolHistory(history) => history,
            _ => panic!("expected tool history"),
        };
        assert!(
            history.expanded,
            "global Ctrl-O must not reset a mouse override"
        );
    }

    #[tokio::test]
    async fn clicking_a_live_tool_hint_toggles_only_that_tool() {
        let mut app = test_tui_app();
        let mut first = match tool_history_message(0).parts.remove(0) {
            DisplayPart::ToolHistory(history) => history.tools.into_iter().next().unwrap(),
            _ => unreachable!(),
        };
        first.id = "first".into();
        let mut second = first.clone();
        second.id = "second".into();
        app.state.messages.push(DisplayMessage {
            role: MessageRole::Assistant,
            content: String::new(),
            thinking: None,
            parts: vec![DisplayPart::Tool(first), DisplayPart::Tool(second)],
            tool_calls: Vec::new(),
            delivery: crate::prompt_queue::DeliveryState::Sent,
            idempotency_key: None,
        });
        app.state.transcript_area = ratatui::layout::Rect::new(0, 0, 80, 10);
        app.state.content_lines = 10;
        app.state.viewport_height = 10;
        app.state.tool_expand_hits.push(ToolExpandHit {
            line: 1,
            target: ToolExpandTarget::Part {
                message: 0,
                part: 0,
            },
        });

        let mut scroll_delta = 0;
        app.collect_mouse(
            crossterm::event::MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 5,
                row: 1,
                modifiers: KeyModifiers::NONE,
            },
            &mut scroll_delta,
        );

        assert!(matches!(
            &app.state.messages[0].parts[0],
            DisplayPart::Tool(tool) if !tool.collapsed && tool.user_overridden
        ));
        assert!(matches!(
            &app.state.messages[0].parts[1],
            DisplayPart::Tool(tool) if tool.collapsed && !tool.user_overridden
        ));
        app.toggle_tool_folding();
        app.toggle_tool_folding();
        assert!(matches!(
            &app.state.messages[0].parts[0],
            DisplayPart::Tool(tool) if !tool.collapsed
        ));
    }

    #[tokio::test]
    async fn long_tool_output_can_expand_and_collapse_at_the_same_mouse_row() {
        use ratatui::{backend::TestBackend, Terminal};

        let mut app = test_tui_app();
        let mut tool = match tool_history_message(0).parts.remove(0) {
            DisplayPart::ToolHistory(history) => history.tools.into_iter().next().unwrap(),
            _ => unreachable!(),
        };
        tool.output = Some(
            (1..=100)
                .map(|line| format!("long output line {line}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        app.state.messages.push(DisplayMessage {
            role: MessageRole::Assistant,
            content: String::new(),
            thinking: None,
            parts: vec![DisplayPart::Tool(tool)],
            tool_calls: Vec::new(),
            delivery: crate::prompt_queue::DeliveryState::Sent,
            idempotency_key: None,
        });
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| components::render_ui(frame, &mut app.state, &app.config))
            .unwrap();
        let hit = app.state.tool_expand_hits[0];
        let initial_top = app
            .state
            .max_scroll_up()
            .saturating_sub(app.state.scroll_up) as usize;
        let row = app.state.transcript_area.y + (hit.line - initial_top) as u16;
        let click = crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 10,
            row,
            modifiers: KeyModifiers::NONE,
        };

        let mut scroll_delta = 0;
        app.collect_mouse(click, &mut scroll_delta);
        terminal
            .draw(|frame| components::render_ui(frame, &mut app.state, &app.config))
            .unwrap();
        assert!(matches!(
            &app.state.messages[0].parts[0],
            DisplayPart::Tool(tool) if !tool.collapsed
        ));
        let expanded_hit = app.state.tool_expand_hits[0];
        let expanded_top = app
            .state
            .max_scroll_up()
            .saturating_sub(app.state.scroll_up) as usize;
        assert_eq!(
            app.state.transcript_area.y + (expanded_hit.line - expanded_top) as u16,
            row,
            "the collapse hint should remain under the pointer"
        );

        app.collect_mouse(click, &mut scroll_delta);
        terminal
            .draw(|frame| components::render_ui(frame, &mut app.state, &app.config))
            .unwrap();
        assert!(matches!(
            &app.state.messages[0].parts[0],
            DisplayPart::Tool(tool) if tool.collapsed
        ));
    }

    #[test]
    fn completed_tool_history_inherits_the_global_expand_mode() {
        let mut state = AppState::new(PermissionMode::Manual, false);
        state.tool_output_expanded = true;
        state.messages.push(DisplayMessage {
            role: MessageRole::User,
            content: "run it".into(),
            thinking: None,
            parts: Vec::new(),
            tool_calls: Vec::new(),
            delivery: crate::prompt_queue::DeliveryState::Sent,
            idempotency_key: None,
        });
        let mut assistant = tool_history_message(0);
        assistant.parts = assistant
            .parts
            .into_iter()
            .flat_map(|part| match part {
                DisplayPart::ToolHistory(history) => history
                    .tools
                    .into_iter()
                    .map(DisplayPart::Tool)
                    .collect::<Vec<_>>(),
                other => vec![other],
            })
            .collect();
        state.messages.push(assistant);

        state.collapse_completed_turn_tools();

        assert!(matches!(
            state.messages[1].parts.first(),
            Some(DisplayPart::ToolHistory(history)) if history.expanded
        ));
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
                    user_overridden: false,
                    started_at: None,
                    stopping: false,
                    queued_behind: None,
                }),
                DisplayPart::Tool(DisplayToolCall {
                    id: "b".into(),
                    name: "Bash".into(),
                    input_summary: "two".into(),
                    output: None,
                    is_error: false,
                    collapsed: true,
                    user_overridden: false,
                    started_at: None,
                    stopping: false,
                    queued_behind: None,
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

    #[tokio::test]
    async fn ctrl_b_quits_without_interrupting_when_detach_enabled() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use futures::FutureExt;
        use std::sync::Arc;

        let (client_transport, server_transport) =
            kkagent_rpc::transport::memory::create_memory_pair();
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(16);
        let rpc = kkagent_rpc::RpcClient::new(client_transport, event_tx);
        let client = kkagent_client::KkagentClient::new(rpc, event_rx);
        let (interrupt_tx, mut interrupt_rx) = tokio::sync::mpsc::unbounded_channel();
        let handler: kkagent_rpc::server::RequestHandler =
            Arc::new(move |_id, method, params, _event_tx| {
                let interrupt_tx = interrupt_tx.clone();
                async move {
                    match method.as_str() {
                        "session.interrupt" => {
                            interrupt_tx.send(params.unwrap()).unwrap();
                            Ok(serde_json::json!({"ok": true}))
                        }
                        "session.set_prompt_queue" => Ok(serde_json::json!({"ok": true})),
                        other => panic!("unexpected RPC method: {other}"),
                    }
                }
                .boxed()
            });
        tokio::spawn(async move {
            kkagent_rpc::RpcServer::new(handler)
                .serve(server_transport)
                .await;
        });

        let mut app = TuiApp::new(AppConfig::default(), client);
        app.set_allows_background_detach(true);
        app.state.session_id = Some("running".into());
        app.state.status = SessionStatus::Thinking;
        app.state.mode = AppMode::Normal;

        app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL))
            .await
            .unwrap();

        assert!(app.state.should_quit);
        assert!(
            interrupt_rx.try_recv().is_err(),
            "Ctrl+B must not interrupt the in-flight turn"
        );
    }

    #[tokio::test]
    async fn ctrl_c_while_busy_opens_quit_confirm_without_interrupt() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use futures::FutureExt;
        use std::sync::Arc;

        let (client_transport, server_transport) =
            kkagent_rpc::transport::memory::create_memory_pair();
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(16);
        let rpc = kkagent_rpc::RpcClient::new(client_transport, event_tx);
        let client = kkagent_client::KkagentClient::new(rpc, event_rx);
        let (interrupt_tx, mut interrupt_rx) = tokio::sync::mpsc::unbounded_channel();
        let handler: kkagent_rpc::server::RequestHandler =
            Arc::new(move |_id, method, params, _event_tx| {
                let interrupt_tx = interrupt_tx.clone();
                async move {
                    match method.as_str() {
                        "session.interrupt" => {
                            interrupt_tx.send(params.unwrap()).unwrap();
                            Ok(serde_json::json!({"ok": true}))
                        }
                        "runtime.has_active_turns" => {
                            Ok(serde_json::json!({"active": true, "sessions": ["running"]}))
                        }
                        "session.set_prompt_queue" => Ok(serde_json::json!({"ok": true})),
                        other => panic!("unexpected RPC method: {other}"),
                    }
                }
                .boxed()
            });
        tokio::spawn(async move {
            kkagent_rpc::RpcServer::new(handler)
                .serve(server_transport)
                .await;
        });

        let mut app = TuiApp::new(AppConfig::default(), client);
        app.set_allows_background_detach(true);
        app.state.session_id = Some("running".into());
        app.state.status = SessionStatus::Thinking;

        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
            .await
            .unwrap();
        assert!(app.state.quit_confirm);
        assert!(interrupt_rx.try_recv().is_err());

        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
            .await
            .unwrap();
        assert!(app.state.quit_dialog.is_some());
        assert!(interrupt_rx.try_recv().is_err());
    }

    #[test]
    fn compensate_scroll_anchor_keeps_viewport_stable_on_bottom_growth() {
        let mut state = AppState::new(PermissionMode::Manual, false);

        // Simulate a 30-line transcript in a 10-line viewport.
        state.content_lines = 30;
        state.viewport_height = 10;
        state.scroll_up = 10; // user scrolled up 10 lines from bottom
        state.follow_bottom = false;
        state.prev_content_lines = None;

        // Frame 1: first render after switch — should record height, not
        // compensate (delta unknown).
        state.compensate_scroll_anchor(30);
        assert_eq!(
            state.scroll_up, 10,
            "first frame should not compensate when prev is None"
        );

        // Frame 2: 5 new lines stream in at the bottom.
        // Without compensation, scroll_up stays 10 and the viewport
        // drifts down by 5. With compensation, scroll_up grows to 15.
        state.compensate_scroll_anchor(35);
        assert_eq!(
            state.scroll_up, 15,
            "scroll_up should grow by the content delta to anchor the view"
        );

        // Frame 3: another 3 lines stream in.
        state.compensate_scroll_anchor(38);
        assert_eq!(state.scroll_up, 18);

        // Frame 4: content shrinks (e.g. a message collapsed) — scroll_up
        // should decrease too, clamped at 0.
        state.compensate_scroll_anchor(20);
        assert_eq!(
            state.scroll_up, 0,
            "scroll_up should decrease when content shrinks"
        );
    }

    #[test]
    fn compensate_scroll_anchor_skipped_when_following_bottom() {
        let mut state = AppState::new(PermissionMode::Manual, false);
        state.content_lines = 30;
        state.viewport_height = 10;
        state.scroll_up = 0;
        state.follow_bottom = true;
        state.prev_content_lines = Some(30);

        // Even though content grew by 5, follow_bottom means we stay
        // pinned to the latest — compensate should NOT touch scroll_up.
        state.compensate_scroll_anchor(35);
        assert_eq!(
            state.scroll_up, 0,
            "follow_bottom should bypass compensation"
        );
        // prev_content_lines still updated for continuity.
        assert_eq!(state.prev_content_lines, Some(35));
    }

    #[test]
    fn compensate_scroll_anchor_clamps_when_content_shrinks_below_viewport() {
        let mut state = AppState::new(PermissionMode::Manual, false);
        state.content_lines = 30;
        state.viewport_height = 10;
        state.scroll_up = 20; // at max (30 - 10)
        state.follow_bottom = false;
        state.prev_content_lines = Some(30);

        // Content shrinks drastically (e.g. messages collapsed) — delta is
        // -25, scroll_up would go to -5, clamped to 0. Max is 0 (5 - 10).
        state.compensate_scroll_anchor(5);
        assert_eq!(
            state.scroll_up, 0,
            "scroll_up should be clamped to 0 when content fits in viewport"
        );
    }

    #[test]
    fn compensate_scroll_anchor_saturates_instead_of_truncating() {
        let mut state = AppState::new(PermissionMode::Manual, false);
        state.viewport_height = 1;
        // Extreme-but-legal values: scroll_up near u16::MAX with growth delta.
        // Old i32->u16 cast would wrap 65540 -> 4; saturation must clamp.
        state.scroll_up = u16::MAX - 5;
        state.follow_bottom = false;
        state.prev_content_lines = Some(u16::MAX - 10);

        // Growth of 10 lines saturates at u16::MAX, then the max-scroll clamp
        // lands at u16::MAX - 1 (content minus the 1-line viewport).
        state.compensate_scroll_anchor(u16::MAX);
        assert_eq!(
            state.scroll_up,
            u16::MAX - 1,
            "growth near u16::MAX must saturate, not wrap to a small value"
        );
    }

    #[tokio::test]
    async fn new_session_resets_context_and_usage_stats() {
        use futures::FutureExt;

        let (client_transport, server_transport) =
            kkagent_rpc::transport::memory::create_memory_pair();
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(16);
        let rpc = kkagent_rpc::RpcClient::new(client_transport, event_tx);
        let client = kkagent_client::KkagentClient::new(rpc, event_rx);
        let handler: kkagent_rpc::server::RequestHandler =
            Arc::new(move |_id, method, _params, _event_tx| {
                async move {
                    match method.as_str() {
                        "sessions.create" => Ok(serde_json::json!({"session_id": "fresh"})),
                        "sessions.list" => Ok(serde_json::json!({"sessions": []})),
                        other => panic!("unexpected RPC method: {other}"),
                    }
                }
                .boxed()
            });
        tokio::spawn(async move {
            kkagent_rpc::RpcServer::new(handler)
                .serve(server_transport)
                .await;
        });

        let mut app = TuiApp::new(AppConfig::default(), client);
        app.state.session_id = Some("old".into());
        app.state.tab_strip.ensure_active("old", "old");
        // Simulate an active session that already burned context: the footer
        // indicator reads these fields, so a stale value here reproduces the
        // "new session still shows the old context meter" bug.
        app.state.approx_tokens = 12345;
        app.state.tokens_at_turn_start = 12000;
        app.state.usage_session = SessionUsageTotals {
            input_tokens: 1000,
            output_tokens: 500,
            cache_creation_tokens: 200,
            cache_read_tokens: 800,
            steps: 3,
            turns: 2,
            input_includes_cache: None,
        };
        app.state.usage_turns.push(crate::app::TurnUsageSample {
            model: None,
            input_tokens: 500,
            output_tokens: 250,
            cache_creation_tokens: 100,
            cache_read_tokens: 400,
            input_includes_cache: None,
            duration_ms: 1_500,
        });
        app.state.last_step_usage = Some(kkagent_protocol::TokenUsage {
            input_tokens: 1,
            output_tokens: 1,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            input_includes_cache: None,
        });
        app.state.context_breakdown = Some(kkagent_protocol::ContextBreakdownInfo::default());

        app.handle_slash_command("/new").await.unwrap();

        assert_eq!(app.state.session_id.as_deref(), Some("fresh"));
        assert_eq!(
            app.state.approx_tokens, 0,
            "context indicator must reset for the new session"
        );
        assert_eq!(app.state.tokens_at_turn_start, 0);
        assert_eq!(app.state.usage_session, SessionUsageTotals::default());
        assert!(app.state.usage_turns.is_empty());
        assert!(app.state.last_step_usage.is_none());
        assert!(app.state.context_breakdown.is_none());
    }

    #[test]
    fn restore_runtime_state_recovers_last_step_usage() {
        let mut state = AppState::new(PermissionMode::Manual, false);
        state.last_step_usage = Some(kkagent_protocol::TokenUsage {
            input_tokens: 1,
            output_tokens: 1,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            input_includes_cache: None,
        });
        let captured = SessionRuntimeState::capture(&state);

        // Emulate switching to another session (stats reset)…
        state.reset_context_usage_stats();
        assert!(state.last_step_usage.is_none());
        // …and coming back: the captured snapshot must restore it.
        captured.restore(&mut state);
        assert!(state.last_step_usage.is_some());
    }
}
