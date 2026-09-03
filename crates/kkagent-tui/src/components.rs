use kkagent_config::AppConfig;
use kkagent_protocol::{PermissionMode, SessionStatus};
use ratatui::{
    layout::{Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Padding, Paragraph, Wrap},
    Frame,
};
use std::hash::{Hash, Hasher};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{
    AppMode, AppState, DisplayPart, ListPickerState, MessageRole, PendingApproval, PendingQuestion,
    TaskDetailState, TodoItem, ToolExpandHit, ToolExpandTarget, ToolHistorySummary,
};
use crate::git_badge;
use crate::i18n::{self, Locale};
use crate::panes;
use crate::theme::Theme;

const TIPS: &[&str] = &[
    "/compact compresses context when it gets long",
    "@ opens file picker — tab to insert a path",
    "ctrl+f searches the transcript",
    "ctrl+o expands turn tool history",
    "tab / ←→ cycle related sessions · ctrl+d close session",
    "ctrl+g toggles the BTW workspace",
    "shift-tab toggles plan mode (scroll locks to plan)",
    "! shell (local, immediate)",
    "/yolo auto-approves tools",
    "large pastes collapse to [Pasted text #n]",
    "press Esc twice to fork from and edit an earlier prompt",
    "scroll to review earlier messages",
];

const NARROW_TERMINAL_WIDTH: u16 = 48;

fn is_narrow(width: u16) -> bool {
    width < NARROW_TERMINAL_WIDTH
}

fn footer_height(width: u16) -> u16 {
    if is_narrow(width) {
        3
    } else {
        2
    }
}

/// Return a centered popup that never extends beyond the supplied area.
///
/// Phone terminals need the horizontal space more than they need decorative
/// margins, so progressively drop the margin as the viewport gets smaller.
fn popup_rect(area: Rect, preferred_width: u16, preferred_height: u16) -> Rect {
    if area.width == 0 || area.height == 0 {
        return Rect::new(area.x, area.y, 0, 0);
    }
    let horizontal_margin = if area.width >= 16 { 2 } else { 0 };
    let vertical_margin = if area.height >= 8 { 1 } else { 0 };
    let width = preferred_width
        .min(area.width.saturating_sub(horizontal_margin * 2))
        .max(1)
        .min(area.width);
    let height = preferred_height
        .min(area.height.saturating_sub(vertical_margin * 2))
        .max(1)
        .min(area.height);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

fn wrapped_text_height(lines: &[Line<'_>], width: u16) -> u16 {
    let width = width.max(1) as usize;
    lines.iter().fold(0u16, |height, line| {
        let rows = line.width().max(1).div_ceil(width) as u16;
        height.saturating_add(rows.max(1))
    })
}

pub fn render_ui(f: &mut Frame, state: &mut AppState, config: &AppConfig) {
    let theme = Theme::default();
    let size = f.area();

    // Reserve space for slash / list picker popup above the input box
    let slash_height = state
        .plugin_prompt
        .as_ref()
        .map(|_| 7)
        .or_else(|| {
            state
                .slash_menu
                .as_ref()
                .map(|menu| menu_height(menu, size.width))
        })
        .or_else(|| state.file_menu.as_ref().map(file_menu_height))
        .or_else(|| {
            state
                .list_picker
                .as_ref()
                .map(|p| picker_height(p, state, size.width))
        })
        .or_else(|| {
            state
                .session_delete_confirm
                .as_ref()
                .map(|_| if is_narrow(size.width) { 16 } else { 7 })
        })
        .or_else(|| {
            state
                .quit_dialog
                .as_ref()
                .map(|_| if is_narrow(size.width) { 16 } else { 9 })
        })
        .unwrap_or(0);

    // Sticky todo sits above the input (highest visual priority).
    // Subagent strip sits above todo (and input) so child floods never enter the transcript.
    let agents_height = if state.mode == AppMode::Btw {
        0
    } else {
        subagent_strip_height(state)
    };
    let todo_height = if state.mode == AppMode::Btw {
        0
    } else {
        todo_panel_height(state, size.width)
    };
    let queue_height = if state.mode == AppMode::Btw || state.prompt_queue.is_empty() {
        0u16
    } else {
        (state.prompt_queue.items.len() as u16)
            .saturating_add(2)
            .min(6)
    };

    // kimi 布局：消息区 | agents(可选) | todo(可选) | queue(可选) | 带边框输入框 | footer 两行
    // Top TabStrip removed — session switching lives in the footer strip.
    let input_inner = input_inner_height(state, size.width);
    let input_box = input_inner + 2; // borders
    let bottom_stack = agents_height
        .saturating_add(todo_height)
        .saturating_add(queue_height)
        .saturating_add(input_box)
        .saturating_add(slash_height);
    let footer_height = footer_height(size.width).min(size.height);
    // Clamp the bottom stack so the message area always gets at least 2 rows.
    // On very short (phone) terminals an unclamped stack can consume the entire
    // viewport, causing the message area to flip between 0 and 1 rows every frame
    // — visible as vertical flicker.
    let max_bottom_stack = size.height.saturating_sub(footer_height).saturating_sub(2);
    let bottom_stack = bottom_stack.min(max_bottom_stack);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(bottom_stack),
            Constraint::Length(footer_height),
        ])
        .split(size);

    let msg_area = chunks[0];
    let bottom = chunks[1];
    let footer_area = chunks[2];
    let bottom_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(agents_height),
            Constraint::Length(todo_height),
            Constraint::Length(queue_height),
            Constraint::Length(slash_height),
            Constraint::Length(input_box),
        ])
        .split(bottom);

    let agents_area = bottom_chunks[0];
    let todo_area = bottom_chunks[1];
    let queue_area = bottom_chunks[2];
    let slash_area = bottom_chunks[3];
    let input_area = bottom_chunks[4];

    // Keep status_bar in sync for chrome consumers / future status line.
    state.status_bar.permission = state.permission_mode;
    state.status_bar.plan_mode = state.plan_mode;
    state.status_bar.status = state.status;
    state.status_bar.tokens = state.approx_tokens;
    state.status_bar.session_id = state.session_id.clone();
    state.status_bar.cwd = Some(state.working_dir.to_string_lossy().into_owned());
    state.status_bar.model = active_subagent_model(state).or_else(|| state.model_alias.clone());

    if state.mode == AppMode::Btw {
        state.transcript_area = Rect::default();
        panes::render_btw(
            f,
            msg_area,
            &mut state.btw,
            &theme,
            state.tick,
            state.tool_output_expanded,
            &mut state.render_cache,
        );
    } else if let Some(subagent_id) = state.active_subagent_view.clone() {
        render_subagent_view(f, msg_area, state, &theme, &subagent_id);
        render_scroll_hint(f, msg_area, state, &theme);
    } else {
        render_messages(f, msg_area, state, &theme);
        render_scroll_hint(f, msg_area, state, &theme);
    }
    if agents_height > 0 {
        render_subagent_strip(f, agents_area, state, &theme);
    }
    if todo_height > 0 {
        render_todo_panel(f, todo_area, state, &theme);
    }
    if queue_height > 0 {
        panes::render_queue(
            f,
            queue_area,
            &panes::QueuePane::from_prompt_queue(&state.prompt_queue),
            &theme,
        );
    }
    render_input(f, input_area, state, &theme);
    render_footer(f, footer_area, state, config, &theme);

    if let Some(ref mut approval) = state
        .approval_pending
        .as_mut()
        .filter(|approval| !approval.hidden)
    {
        render_approval_panel(f, size, approval, &theme);
    } else if let Some(ref mut question) = state.question_pending {
        render_question_panel(f, size, question, &theme);
    } else if state.goal_judge_panel_open {
        crate::goal_judge_view::render_judge_panel(
            f,
            size,
            &state.goal_judge_records,
            &state.judge_chat_log,
            &state.judge_chat_input,
            state.judge_chat_pending,
            &theme,
        );
    }

    if state.plugin_prompt.is_some() {
        render_plugin_prompt(f, slash_area, state, &theme);
    } else if state.slash_menu.is_some() {
        render_slash_menu(f, slash_area, state, &theme);
    } else if state.file_menu.is_some() {
        render_file_menu(f, slash_area, state, &theme);
    } else if state.list_picker.is_some() {
        render_list_picker(f, slash_area, state, &theme);
    } else if state.session_delete_confirm.is_some() {
        render_session_delete_confirm(f, slash_area, state, &theme);
    } else if state.quit_dialog.is_some() {
        render_quit_dialog(f, slash_area, state, &theme);
    }

    if state.tasks_panel.is_some() {
        render_tasks_panel(f, size, state, &theme);
    }
    if state.subagents_panel.is_some() {
        render_subagents_panel(f, size, state, &theme);
    }

    if state.search.active {
        render_search_overlay(f, size, state, &theme);
    }

    if let Some(ref toast) = state.copy_toast {
        render_copy_toast(f, size, toast, &theme);
    }
}

fn render_plugin_prompt(f: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let Some(prompt) = state.plugin_prompt.as_ref() else {
        return;
    };
    f.render_widget(Clear, area);
    let (title, help) = match prompt.kind {
        crate::app::PluginPromptKind::AddMarketplace => (
            " Add plugin marketplace ",
            "Enter a catalog URL or local marketplace.json path",
        ),
        crate::app::PluginPromptKind::InstallSource => (
            " Install plugin from source ",
            "Enter a directory, ZIP URL, or GitHub repository URL",
        ),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.primary))
        .title(title);
    let lines = vec![
        Line::from(Span::styled(help, Style::default().fg(theme.text_dim))),
        Line::from(""),
        Line::from(vec![
            Span::styled("> ", Style::default().fg(theme.primary)),
            Span::styled(&prompt.value, Style::default().fg(theme.text)),
            Span::styled("█", Style::default().fg(theme.primary)),
        ]),
        Line::from(Span::styled(
            "Enter confirm · Esc cancel · paste supported",
            Style::default().fg(theme.text_muted),
        )),
    ];
    f.render_widget(Paragraph::new(Text::from(lines)).block(block), area);
}

fn render_copy_toast(f: &mut Frame, area: Rect, toast: &crate::app::CopyToast, theme: &Theme) {
    let text = toast.message.as_str();
    let width = text.width() as u16 + 4; // 2 padding + 2 border
    let width = width.clamp(18, area.width.saturating_sub(4));
    let height = 3u16;
    if area.width < width + 2 || area.height < height + 1 {
        return;
    }
    let popup = centered_rect(area, width, height);
    f.render_widget(Clear, popup);
    let paragraph = Paragraph::new(Text::from(Line::from(Span::styled(
        text,
        Style::default().fg(theme.text_strong),
    ))))
    .alignment(ratatui::layout::Alignment::Center)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.border_focus))
            .style(Style::default().bg(theme.background)),
    );
    f.render_widget(paragraph, popup);
}

fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let x = area.x.saturating_add(area.width.saturating_sub(width) / 2);
    let y = area
        .y
        .saturating_add(area.height.saturating_sub(height) / 2);
    Rect::new(x, y, width.min(area.width), height.min(area.height))
}

fn menu_height(menu_state: &crate::app::SlashMenuState, width: u16) -> u16 {
    let max_visible = 8u16;
    let rows = if menu_state.items.is_empty() {
        1
    } else {
        (menu_state.items.len() as u16).min(max_visible)
    };
    rows.saturating_mul(if is_narrow(width) { 2 } else { 1 }) + 2 // borders
}

fn file_menu_height(menu: &crate::app::FileMenuState) -> u16 {
    let max_visible = 10u16;
    let rows = if menu.items.is_empty() {
        1
    } else {
        (menu.items.len() as u16).min(max_visible)
    };
    rows + 2
}

fn picker_height(picker: &ListPickerState, state: &AppState, width: u16) -> u16 {
    let max_visible = 10u16;
    let rows = if picker.items.is_empty() {
        1
    } else {
        (picker.items.len() as u16).min(max_visible)
    };
    let h = rows.saturating_mul(if is_narrow(width) { 2 } else { 1 }) + 2;
    // Delete confirm replaces the list with a small choice panel.
    if picker.kind == crate::app::ListPickerKind::Session && state.session_delete_confirm.is_some()
    {
        return if is_narrow(width) { 16 } else { 7 };
    }
    h.min(28)
}

fn input_prefix_str(state: &AppState) -> &'static str {
    match state.mode {
        AppMode::Shell => "! ",
        AppMode::Plan => "plan > ",
        AppMode::Btw => "btw > ",
        AppMode::Normal => "> ",
    }
}

/// Logical lines for the editor (`split` keeps a trailing empty line after final `\n`).
fn input_logical_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        vec![""]
    } else {
        text.split('\n').collect()
    }
}

/// Soft-wrap a single logical line into visual rows of at most `width` columns.
fn soft_wrap_line(line: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![line.to_string()];
    }
    if line.is_empty() {
        return vec![String::new()];
    }
    wrap_str(line, width)
}

/// Total visual rows for input text given content width (after prefix indent).
fn input_visual_row_count(text: &str, content_width: usize) -> u16 {
    let width = content_width.max(1);
    let mut rows = 0u16;
    for line in input_logical_lines(text) {
        let n = soft_wrap_line(line, width).len() as u16;
        rows = rows.saturating_add(n.max(1));
    }
    rows.max(1)
}

fn input_inner_height(state: &AppState, terminal_width: u16) -> u16 {
    let prefix_w = UnicodeWidthStr::width(input_prefix_str(state));
    // borders take 2 columns
    let inner_width = terminal_width.saturating_sub(2);
    let content_width = inner_width.saturating_sub(prefix_w as u16).max(1) as usize;
    let rows = input_visual_row_count(&state.input.text, content_width);
    let (_, cursor_y) = cursor_position(
        &state.input.text,
        state.input.cursor,
        content_width,
        prefix_w as u16,
    );
    // Include the cursor row (exact wrap boundary may need one extra visual line).
    // On narrow (phone) terminals cap lower so the input box cannot crowd out the
    // message area — a tall input box on a 20-row screen leaves almost nothing for
    // the transcript, and the resulting squeeze causes visible vertical oscillation.
    let cap = if is_narrow(terminal_width) { 4 } else { 8 };
    rows.max(cursor_y.saturating_add(1)).clamp(1, cap)
}

/// Render a subagent's event log as a full transcript view, mirroring the
/// main session's look (header, scrollable body, border). Activated when the
/// user presses Enter in the `/agents` panel.
fn render_subagent_view(
    f: &mut Frame,
    area: Rect,
    state: &mut AppState,
    theme: &Theme,
    subagent_id: &str,
) {
    let entry = state
        .subagents
        .entries
        .iter()
        .find(|e| e.id == subagent_id)
        .cloned();
    let Some(entry) = entry else {
        // Subagent no longer exists — show a placeholder.
        let para = Paragraph::new("Subagent no longer available.")
            .style(Style::default().fg(theme.text_muted));
        f.render_widget(para, area);
        return;
    };

    // Session-style view: one-line context header (child identity + status)
    // above the same transcript renderer the main session uses. The
    // transcript itself is selected in `build_transcript_lines_range`.
    let header_area = Rect { height: 1, ..area };
    let status_color = match entry.status.as_str() {
        "running" => theme.primary,
        "complete" => theme.success,
        _ => theme.error,
    };
    let header = Line::from(vec![
        Span::styled("↳ ", Style::default().fg(theme.accent)),
        Span::styled(
            entry.name.clone(),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" · {}", entry.description),
            Style::default().fg(theme.text_muted),
        ),
        Span::styled(
            format!(" · {}", entry.status),
            Style::default().fg(status_color),
        ),
        Span::styled(
            "  (Esc 返回主会话 · Tab 切换)",
            Style::default().fg(theme.text_dim),
        ),
    ]);
    f.render_widget(
        Paragraph::new(header).style(Style::default().bg(theme.border)),
        header_area,
    );

    let body = Rect {
        y: area.y + 1,
        height: area.height.saturating_sub(1),
        ..area
    };
    if body.height == 0 {
        return;
    }
    render_messages(f, body, state, theme);
}

/// Simple text wrapper for subagent event log lines.
#[allow(dead_code)]
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let mut result = Vec::new();
    for line in text.lines() {
        if line.chars().count() <= width {
            result.push(line.to_string());
        } else {
            let mut current = String::new();
            let mut len = 0;
            for ch in line.chars() {
                if len >= width {
                    result.push(std::mem::take(&mut current));
                    len = 0;
                }
                current.push(ch);
                len += 1;
            }
            if !current.is_empty() {
                result.push(current);
            }
        }
    }
    if result.is_empty() {
        result.push(String::new());
    }
    result
}

fn render_messages(f: &mut Frame, area: Rect, state: &mut AppState, theme: &Theme) {
    let width = area.width.max(1);
    let streaming_tail = streaming_transcript_tail_index(state);
    let fingerprint = transcript_layout_fingerprint(state, width);
    if !state.transcript_layout_cache.matches(fingerprint) {
        let lines = if let Some(tail_index) = streaming_tail {
            build_transcript_lines_range(state, theme, width, Some(0..tail_index), true)
        } else {
            build_transcript_lines(state, theme, width)
        };
        state.select_rows = crate::selection::rows_from_lines(&lines);
        state.transcript_layout_cache.replace(fingerprint, lines);
    }
    let static_line_count = state.transcript_layout_cache.lines().len();
    let mut dynamic_lines = if let Some(tail_index) = streaming_tail {
        let prefix_starts: Vec<u16> = state
            .message_line_starts
            .iter()
            .take(tail_index)
            .copied()
            .collect();
        let prefix_hits: Vec<ToolExpandHit> = state
            .tool_expand_hits
            .iter()
            .filter(|hit| tool_expand_target_message(hit.target) < tail_index)
            .copied()
            .collect();
        let tail_lines = build_transcript_lines_range(
            state,
            theme,
            width,
            Some(tail_index..tail_index + 1),
            false,
        );
        let mut tail_starts = std::mem::take(&mut state.message_line_starts);
        for start in &mut tail_starts {
            *start = static_line_count
                .saturating_add(*start as usize)
                .min(u16::MAX as usize) as u16;
        }
        let mut tail_hits = std::mem::take(&mut state.tool_expand_hits);
        for hit in &mut tail_hits {
            hit.line = hit.line.saturating_add(static_line_count);
        }
        state.message_line_starts = prefix_starts;
        state.message_line_starts.extend(tail_starts);
        state.tool_expand_hits = prefix_hits;
        state.tool_expand_hits.extend(tail_hits);
        tail_lines
    } else {
        Vec::new()
    };
    dynamic_lines.extend(build_transcript_status_lines(
        state,
        theme,
        width,
        area.height,
    ));
    state.select_rows.truncate(static_line_count);
    state
        .select_rows
        .extend(crate::selection::rows_from_lines(&dynamic_lines));
    state.transcript_area = area;

    if let Some(sel) = state.selection {
        state.selection = crate::selection::clamp_selection(sel, state.select_rows.len());
    }

    let content_height = state
        .transcript_layout_cache
        .lines()
        .len()
        .saturating_add(dynamic_lines.len())
        .min(u16::MAX as usize) as u16;
    let visible_height = area.height.max(1);

    state.content_lines = content_height;
    state.viewport_height = visible_height;

    let max_scroll_up = content_height.saturating_sub(visible_height);
    if let Some((line, viewport_row)) = state.pending_tool_click_anchor.take() {
        let scroll_from_top = line
            .saturating_sub(viewport_row as usize)
            .min(max_scroll_up as usize) as u16;
        state.scroll_up = max_scroll_up.saturating_sub(scroll_from_top);
        state.follow_bottom = false;
        state.prev_content_lines = Some(content_height);
    } else if state.plan_scroll_to_top && state.plan_focus_active() {
        state.scroll_up = max_scroll_up;
        state.follow_bottom = max_scroll_up == 0;
        state.plan_scroll_to_top = false;
        state.prev_content_lines = Some(content_height);
    } else if state.follow_bottom {
        state.scroll_up = 0;
        state.prev_content_lines = Some(content_height);
    } else {
        // User is reading history (!follow_bottom). Compensate scroll_up
        // for content growth so the viewport stays anchored on what they
        // were reading instead of drifting downward every frame.
        state.compensate_scroll_anchor(content_height);
    }

    // scroll_up = 离底部的行数；0 表示贴底跟随
    let scroll_from_top = max_scroll_up.saturating_sub(state.scroll_up);
    let start = scroll_from_top as usize;
    let end = start
        .saturating_add(visible_height as usize)
        .min(static_line_count.saturating_add(dynamic_lines.len()));
    let mut visible_lines = Vec::with_capacity(end.saturating_sub(start));
    let static_start = start.min(static_line_count);
    let static_end = end.min(static_line_count);
    visible_lines
        .extend_from_slice(&state.transcript_layout_cache.lines()[static_start..static_end]);
    if end > static_line_count {
        let dynamic_start = start.saturating_sub(static_line_count);
        let dynamic_end = end - static_line_count;
        visible_lines.extend_from_slice(&dynamic_lines[dynamic_start..dynamic_end]);
    }
    if let Some(sel) = state
        .selection
        .and_then(|selection| selection_in_viewport(selection, start, end))
    {
        crate::selection::apply_highlight(
            &mut visible_lines,
            sel,
            crate::selection::selection_style(),
        );
    }

    // Lines are already width-wrapped — do NOT wrap again (that desyncs scroll).
    let paragraph = Paragraph::new(Text::from(visible_lines));
    f.render_widget(paragraph, area);
}

fn tool_expand_target_message(target: ToolExpandTarget) -> usize {
    match target {
        ToolExpandTarget::Part { message, .. }
        | ToolExpandTarget::Legacy { message, .. }
        | ToolExpandTarget::Plan { message } => message,
    }
}

fn streaming_transcript_tail_index(state: &AppState) -> Option<usize> {
    let index = active_assistant_message_index(state)?;
    (index.checked_add(1) == Some(state.messages.len())).then_some(index)
}

fn active_assistant_message_index(state: &AppState) -> Option<usize> {
    let browsing_sessions = state
        .list_picker
        .as_ref()
        .is_some_and(|picker| picker.kind == crate::app::ListPickerKind::Session);
    if browsing_sessions
        || state.plan_focus_active()
        || state.active_subagent_view.is_some()
        || state.status == SessionStatus::Idle
    {
        return None;
    }
    let index = state.active_assistant_message?;
    state
        .messages
        .get(index)
        .is_some_and(|message| message.role == MessageRole::Assistant)
        .then_some(index)
}

/// Reasoning is treated as "live" only while it is still streaming. Once the
/// buffered thinking has been flushed onto the active assistant message (the
/// first body-text or tool-call delta does this) and no fresh deltas are
/// buffered, the block folds back into the normal transcript rendering even
/// though status remains `Thinking` while the answer itself streams.
fn live_thinking_message_index(state: &AppState) -> Option<usize> {
    if state.status != SessionStatus::Thinking {
        return None;
    }
    let index = active_assistant_message_index(state)?;
    let flushed_and_quiet = state
        .messages
        .get(index)
        .is_some_and(|message| message.thinking.is_some() && state.thinking_text.is_empty());
    (!flushed_and_quiet).then_some(index)
}

/// Whether the live status tail should draw the scrolling thinking body: the
/// buffer holds un-flushed deltas, or the active assistant message has not
/// received its flushed thinking block yet.
fn live_thinking_body_visible(state: &AppState) -> bool {
    !state.thinking_text.is_empty()
        || active_assistant_message_index(state)
            .and_then(|index| state.messages.get(index))
            .is_none_or(|message| message.thinking.is_none())
}

fn selection_in_viewport(
    selection: crate::selection::TextSelection,
    start: usize,
    end: usize,
) -> Option<crate::selection::TextSelection> {
    use crate::selection::{CellPos, TextSelection};

    if start >= end || selection.is_empty() {
        return None;
    }
    let (lo, hi) = selection.normalized();
    if hi.line < start || lo.line >= end {
        return None;
    }
    let visible_lo = CellPos {
        line: lo.line.max(start) - start,
        col: if lo.line < start { 0 } else { lo.col },
    };
    let visible_hi = CellPos {
        line: hi.line.min(end - 1) - start,
        col: if hi.line >= end { u16::MAX } else { hi.col },
    };
    Some(TextSelection {
        anchor: visible_lo,
        focus: visible_hi,
    })
}

fn transcript_layout_fingerprint(state: &AppState, width: u16) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    width.hash(&mut hasher);
    state.tool_output_expanded.hash(&mut hasher);
    state.plan_transcript_collapsed.hash(&mut hasher);
    state.highlight_message.hash(&mut hasher);
    std::mem::discriminant(&state.locale).hash(&mut hasher);

    let browsing_sessions = state
        .list_picker
        .as_ref()
        .is_some_and(|picker| picker.kind == crate::app::ListPickerKind::Session);
    browsing_sessions.hash(&mut hasher);
    // Which subagent transcript the main pane shows changes the rendered
    // content — it must be part of the cache key.
    state.active_subagent_view.hash(&mut hasher);
    if !browsing_sessions && state.plan_focus_active() {
        if let Some(document) = &state.plan_document {
            document.path.hash(&mut hasher);
            document.content.hash(&mut hasher);
        }
        return hasher.finish();
    }

    let messages = if browsing_sessions {
        state
            .session_picker_preview
            .as_ref()
            .map(|preview| preview.messages.as_slice())
            .unwrap_or(&state.messages)
    } else if let Some(subagent_id) = &state.active_subagent_view {
        state
            .subagents
            .entries
            .iter()
            .find(|e| e.id == *subagent_id)
            .map(|e| e.transcript.as_slice())
            .unwrap_or(&state.messages)
    } else {
        &state.messages
    };
    live_thinking_message_index(state).hash(&mut hasher);
    let streaming_tail = streaming_transcript_tail_index(state);
    streaming_tail.hash(&mut hasher);
    messages.len().hash(&mut hasher);
    for (index, message) in messages.iter().enumerate() {
        if streaming_tail == Some(index) {
            continue;
        }
        hash_display_message(message, &mut hasher);
    }

    state.history_loading.hash(&mut hasher);
    if messages.is_empty() && !browsing_sessions {
        (state.tick / 100).hash(&mut hasher);
    }
    hasher.finish()
}

fn hash_display_message(message: &crate::app::DisplayMessage, hasher: &mut impl Hasher) {
    std::mem::discriminant(&message.role).hash(hasher);
    match message.role {
        MessageRole::User => {
            message.content.hash(hasher);
            std::mem::discriminant(&message.delivery).hash(hasher);
            return;
        }
        MessageRole::Plan | MessageRole::System => {
            message.content.hash(hasher);
            return;
        }
        MessageRole::Skill => {
            message.content.hash(hasher);
        }
        MessageRole::Assistant => {
            message.thinking.hash(hasher);
            if message.parts.is_empty() {
                message.content.hash(hasher);
                message.tool_calls.len().hash(hasher);
                for tool in &message.tool_calls {
                    hash_display_tool(tool, hasher);
                }
                return;
            }
        }
    }

    message.parts.len().hash(hasher);
    for part in &message.parts {
        std::mem::discriminant(part).hash(hasher);
        match part {
            DisplayPart::Text(text) => text.hash(hasher),
            DisplayPart::Tool(tool) => hash_display_tool(tool, hasher),
            DisplayPart::ToolHistory(history) => {
                history.tool_count.hash(hasher);
                history.duration_ms.hash(hasher);
                history.tokens.hash(hasher);
                history.expanded.hash(hasher);
                history.user_overridden.hash(hasher);
                history.tools.len().hash(hasher);
                for tool in &history.tools {
                    hash_display_tool(tool, hasher);
                }
            }
            DisplayPart::SkillActivation { name, args } => {
                name.hash(hasher);
                args.hash(hasher);
            }
        }
    }
}

fn hash_display_tool(tool: &crate::app::DisplayToolCall, hasher: &mut impl Hasher) {
    tool.id.hash(hasher);
    tool.name.hash(hasher);
    tool.input_summary.hash(hasher);
    tool.output.hash(hasher);
    tool.is_error.hash(hasher);
    tool.collapsed.hash(hasher);
    tool.user_overridden.hash(hasher);
    tool.stopping.hash(hasher);
    tool.queued_behind.hash(hasher);
    if tool.output.is_none() {
        tool.started_at
            .map(|started| started.elapsed().as_secs())
            .hash(hasher);
    }
}

fn live_thinking_text(state: &AppState) -> &str {
    if !state.thinking_text.is_empty() {
        return &state.thinking_text;
    }
    state
        .active_assistant_message
        .and_then(|index| state.messages.get(index))
        .and_then(|message| message.thinking.as_deref())
        .unwrap_or("")
}

/// The live status tail is deliberately kept outside the full transcript
/// cache. Spinner and thinking updates can then redraw a few lines without
/// laying out the entire conversation again.
fn build_transcript_status_lines(
    state: &AppState,
    theme: &Theme,
    width: u16,
    viewport_height: u16,
) -> Vec<Line<'static>> {
    let browsing_sessions = state
        .list_picker
        .as_ref()
        .is_some_and(|picker| picker.kind == crate::app::ListPickerKind::Session);
    if !browsing_sessions && state.plan_focus_active() {
        return Vec::new();
    }
    let messages = if browsing_sessions {
        state
            .session_picker_preview
            .as_ref()
            .map(|preview| preview.messages.as_slice())
            .unwrap_or(&state.messages)
    } else {
        &state.messages
    };
    if messages.is_empty() {
        return Vec::new();
    }

    let body_height = live_thinking_body_height(viewport_height);
    let frames = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    let ch = frames[(state.tick / crate::app::SPINNER_TICKS_PER_FRAME) % frames.len()];
    let mut lines = Vec::new();
    match state.status {
        SessionStatus::Thinking | SessionStatus::WaitingApproval => {
            if state.status == SessionStatus::WaitingApproval {
                let hidden_plan_review = state
                    .approval_pending
                    .as_ref()
                    .is_some_and(|approval| approval.is_plan_review && approval.hidden);
                let label = if hidden_plan_review {
                    "plan review hidden · enter to reopen · ctrl-c to cancel"
                } else {
                    "waiting for approval"
                };
                lines.push(Line::from(Span::styled(
                    format!("● {ch} {label}"),
                    Style::default()
                        .fg(theme.warning)
                        .add_modifier(Modifier::ITALIC),
                )));
            } else {
                lines.push(Line::from(Span::styled(
                    format!("● {ch} thinking"),
                    Style::default()
                        .fg(theme.text_dim)
                        .add_modifier(Modifier::ITALIC),
                )));
                if live_thinking_body_visible(state) {
                    let mut tail: Vec<&str> =
                        live_thinking_text(state).lines().rev().take(12).collect();
                    tail.reverse();
                    let mut body = Vec::new();
                    for line in tail {
                        push_wrapped_indented_text(
                            &mut body,
                            line,
                            width,
                            2,
                            Style::default().fg(theme.text_muted),
                        );
                    }
                    if body.len() > body_height {
                        body.drain(..body.len() - body_height);
                    }
                    lines.extend(body);
                }
            }
        }
        SessionStatus::ToolExecuting => {
            let tool = state.last_tool_name.as_deref().unwrap_or("tool");
            lines.push(Line::from(Span::styled(
                format!("● {ch} running {tool}"),
                Style::default()
                    .fg(theme.primary)
                    .add_modifier(Modifier::ITALIC),
            )));
        }
        _ => {}
    }
    lines
}

fn live_thinking_body_height(viewport_height: u16) -> usize {
    let available = viewport_height.saturating_sub(1);
    let proportional = (viewport_height / 3).max(1);
    usize::from(available.min(proportional).min(12))
}

fn build_transcript_lines(state: &mut AppState, theme: &Theme, width: u16) -> Vec<Line<'static>> {
    build_transcript_lines_range(state, theme, width, None, true)
}

fn build_transcript_lines_range(
    state: &mut AppState,
    theme: &Theme,
    width: u16,
    message_range: Option<std::ops::Range<usize>>,
    include_prefix_chrome: bool,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();
    state.message_line_starts.clear();
    state.tool_expand_hits.clear();
    state.render_cache.clear_if_width_changed(width);
    let mut render_cache = std::mem::take(&mut state.render_cache);
    let mut tool_expand_hits = Vec::new();

    let browsing_sessions = state
        .list_picker
        .as_ref()
        .map(|p| p.kind == crate::app::ListPickerKind::Session)
        .unwrap_or(false);

    if include_prefix_chrome && !browsing_sessions && state.plan_focus_active() {
        if let Some(doc) = state.plan_document.clone() {
            // Single synthetic index so scroll helpers stay consistent.
            state.message_line_starts.push(0);
            push_plan_focus_lines(&mut lines, &doc.path, &doc.content, width, theme);
            state.render_cache = render_cache;
            return lines;
        }
    }

    // While /sessions is open, the main pane shows the highlighted session's
    // normal transcript (not a separate preview widget). A subagent view does
    // the same with the child's mirrored transcript.
    let browse_msgs: Option<&[crate::app::DisplayMessage]> = if browsing_sessions {
        state
            .session_picker_preview
            .as_ref()
            .map(|p| p.messages.as_slice())
    } else if let Some(subagent_id) = state.active_subagent_view.clone() {
        state
            .subagents
            .entries
            .iter()
            .find(|e| e.id == subagent_id)
            .map(|e| e.transcript.as_slice())
    } else {
        None
    };
    let messages: &[crate::app::DisplayMessage] = browse_msgs.unwrap_or(&state.messages);
    let interactive_tools = browse_msgs.is_none();
    let locale = state.locale;

    if include_prefix_chrome && messages.is_empty() {
        if browsing_sessions {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  (empty session)",
                Style::default().fg(theme.text_muted),
            )));
            lines.push(Line::from(""));
            state.render_cache = render_cache;
            return lines;
        }
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("● ", Style::default().fg(theme.primary)),
            Span::styled(
                "kkagent",
                Style::default()
                    .fg(theme.primary)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  v{}", env!("CARGO_PKG_VERSION")),
                Style::default().fg(theme.text_muted),
            ),
        ]));
        lines.push(Line::from(Span::styled(
            "  coding agent for your terminal",
            Style::default().fg(theme.text_dim),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  type a message · @file · /help · shift-tab plan · ! shell",
            Style::default().fg(theme.text_muted),
        )));
        lines.push(Line::from(Span::styled(
            "  ctrl+f search · ctrl+g btw · /compact · /model",
            Style::default().fg(theme.text_muted),
        )));
        lines.push(Line::from(""));
        let tip = TIPS[(state.tick / 100) % TIPS.len()];
        lines.push(Line::from(vec![
            Span::styled("  tip  ", Style::default().fg(theme.accent)),
            Span::styled(tip.to_string(), Style::default().fg(theme.text_dim)),
        ]));
        lines.push(Line::from(""));
        state.render_cache = render_cache;
        return lines;
    }

    if include_prefix_chrome && state.history_loading {
        lines.push(Line::from(Span::styled(
            "  Loading earlier messages…",
            Style::default().fg(theme.text_muted),
        )));
        lines.push(Line::from(""));
    }

    let range = message_range.unwrap_or(0..messages.len());
    for (msg_idx, msg) in messages
        .iter()
        .enumerate()
        .take(range.end.min(messages.len()))
        .skip(range.start)
    {
        state
            .message_line_starts
            .push(lines.len().min(u16::MAX as usize) as u16);
        let highlight = state.highlight_message == Some(msg_idx);
        if highlight {
            lines.push(Line::from(Span::styled(
                "─".repeat(width.min(40) as usize),
                Style::default().fg(theme.accent),
            )));
        }
        match msg.role {
            MessageRole::User => {
                // kimi: 用户标记 + 正文均为黄色（与 footer 的 yolo 同色）
                push_wrapped_prefixed(
                    &mut lines,
                    "✦ ",
                    &msg.content,
                    width,
                    Style::default()
                        .fg(theme.role_user)
                        .add_modifier(Modifier::BOLD),
                    Style::default().fg(theme.role_user),
                );
                let delivery_label = msg.delivery.label();
                if !delivery_label.is_empty() {
                    lines.push(Line::from(Span::styled(
                        format!("  ({delivery_label})"),
                        Style::default().fg(match msg.delivery {
                            crate::prompt_queue::DeliveryState::Failed => theme.error,
                            crate::prompt_queue::DeliveryState::Queued
                            | crate::prompt_queue::DeliveryState::Sending => theme.warning,
                            _ => theme.text_muted,
                        }),
                    )));
                }
                lines.push(Line::from(""));
            }
            MessageRole::Plan => {
                let (path, body) = {
                    let mut body = msg.content.as_str();
                    let mut path = "";
                    if let Some(rest) = body.strip_prefix("file: ") {
                        if let Some((path_line, rest_body)) = rest.split_once('\n') {
                            path = path_line.trim();
                            body = rest_body.trim_start_matches('\n');
                        }
                    }
                    (path.to_string(), body.to_string())
                };
                if state.plan_transcript_collapsed {
                    let line = lines.len();
                    let name = path.rsplit('/').next().unwrap_or(&path);
                    lines.push(Line::from(vec![
                        Span::styled("● ", Style::default().fg(theme.text)),
                        Span::styled(
                            format!("▸ plan: {name} ({} lines)", body.lines().count()),
                            Style::default()
                                .fg(theme.plan_mode)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            "  ctrl+o / click to expand",
                            Style::default().fg(theme.text_muted),
                        ),
                    ]));
                    if interactive_tools {
                        tool_expand_hits.push(ToolExpandHit {
                            line,
                            target: ToolExpandTarget::Plan { message: msg_idx },
                        });
                    }
                } else {
                    push_plan_box_lines(&mut lines, &path, &body, width, theme, false);
                    let line = lines.len();
                    lines.push(Line::from(Span::styled(
                        "  ▾ ctrl+o / click to collapse",
                        Style::default().fg(theme.text_muted),
                    )));
                    if interactive_tools {
                        tool_expand_hits.push(ToolExpandHit {
                            line,
                            target: ToolExpandTarget::Plan { message: msg_idx },
                        });
                    }
                }
                lines.push(Line::from(""));
            }
            MessageRole::Skill => {
                let (name, args) = msg
                    .parts
                    .iter()
                    .find_map(|p| match p {
                        DisplayPart::SkillActivation { name, args } => {
                            Some((name.as_str(), args.as_deref()))
                        }
                        _ => None,
                    })
                    .unwrap_or_else(|| {
                        let name = msg
                            .content
                            .strip_prefix("Activated skill: ")
                            .unwrap_or(msg.content.as_str());
                        (name, None)
                    });
                push_skill_activation_lines(&mut lines, name, args, width, theme);
                lines.push(Line::from(""));
            }
            MessageRole::Assistant => {
                // Thinking block (dim), like kimi
                let thinking_is_in_live_viewport =
                    live_thinking_message_index(state) == Some(msg_idx);
                if let Some(thinking) = msg
                    .thinking
                    .as_ref()
                    .filter(|_| !thinking_is_in_live_viewport)
                {
                    push_thinking_lines(
                        &mut lines,
                        thinking,
                        width,
                        theme,
                        state.tool_output_expanded,
                    );
                }

                // Chronological parts: tools stay where they were called;
                // later text (final answer) naturally ends at the bottom.
                let mut first_bullet = true;
                let mut rendered_any = false;

                if !msg.parts.is_empty() {
                    for (part_idx, part) in msg.parts.iter().enumerate() {
                        match part {
                            DisplayPart::Text(text) => {
                                if text.is_empty() {
                                    continue;
                                }
                                push_assistant_markdown(
                                    &mut lines,
                                    text,
                                    width,
                                    theme,
                                    &mut first_bullet,
                                    &mut render_cache,
                                );
                                rendered_any = true;
                            }
                            DisplayPart::Tool(tc) => {
                                if let Some(line) = render_tool_call_lines(
                                    &mut lines,
                                    tc,
                                    width,
                                    theme,
                                    first_bullet,
                                    true,
                                ) {
                                    if interactive_tools {
                                        tool_expand_hits.push(ToolExpandHit {
                                            line,
                                            target: ToolExpandTarget::Part {
                                                message: msg_idx,
                                                part: part_idx,
                                            },
                                        });
                                    }
                                }
                                first_bullet = false;
                                rendered_any = true;
                            }
                            DisplayPart::ToolHistory(hist) => {
                                let line = render_tool_history_lines(
                                    &mut lines,
                                    hist,
                                    width,
                                    theme,
                                    locale,
                                    first_bullet,
                                );
                                if interactive_tools {
                                    tool_expand_hits.push(ToolExpandHit {
                                        line,
                                        target: ToolExpandTarget::Part {
                                            message: msg_idx,
                                            part: part_idx,
                                        },
                                    });
                                }
                                first_bullet = false;
                                rendered_any = true;
                            }
                            DisplayPart::SkillActivation { name, args } => {
                                push_skill_activation_lines(
                                    &mut lines,
                                    name,
                                    args.as_deref(),
                                    width,
                                    theme,
                                );
                                first_bullet = false;
                                rendered_any = true;
                            }
                        }
                    }
                } else {
                    // Fallback for old messages without parts: tools first, then content.
                    for (tool_idx, tc) in msg.tool_calls.iter().enumerate() {
                        if let Some(line) =
                            render_tool_call_lines(&mut lines, tc, width, theme, first_bullet, true)
                        {
                            if interactive_tools {
                                tool_expand_hits.push(ToolExpandHit {
                                    line,
                                    target: ToolExpandTarget::Legacy {
                                        message: msg_idx,
                                        tool: tool_idx,
                                    },
                                });
                            }
                        }
                        first_bullet = false;
                        rendered_any = true;
                    }
                    if !msg.content.is_empty() {
                        push_assistant_markdown(
                            &mut lines,
                            &msg.content,
                            width,
                            theme,
                            &mut first_bullet,
                            &mut render_cache,
                        );
                        rendered_any = true;
                    }
                }

                if rendered_any {
                    lines.push(Line::from(""));
                }
            }
            MessageRole::System => {
                for line in msg.content.lines() {
                    let style = if line.starts_with("Error") || line.contains("error") {
                        Style::default().fg(theme.error)
                    } else {
                        Style::default().fg(theme.text_dim)
                    };
                    push_wrapped_prefixed(
                        &mut lines,
                        "● ",
                        line,
                        width,
                        Style::default().fg(theme.text_dim),
                        style,
                    );
                }
                lines.push(Line::from(""));
            }
        }
        if highlight {
            lines.push(Line::from(Span::styled(
                "─".repeat(width.min(40) as usize),
                Style::default().fg(theme.accent),
            )));
        }
    }

    state.render_cache = render_cache;
    state.tool_expand_hits = tool_expand_hits;
    lines
}

pub(crate) fn push_assistant_markdown(
    lines: &mut Vec<Line<'static>>,
    text: &str,
    width: u16,
    theme: &Theme,
    first_bullet: &mut bool,
    cache: &mut crate::render_cache::RenderCache,
) {
    let rendered = cache.get_or_insert_markdown(text, width, theme);
    if rendered.is_empty() {
        return;
    }
    for (i, content) in rendered.into_iter().enumerate() {
        let prefix = if *first_bullet && i == 0 {
            *first_bullet = false;
            Span::styled("● ", Style::default().fg(theme.text))
        } else {
            Span::raw("  ")
        };
        let mut spans = Vec::with_capacity(content.spans.len() + 1);
        spans.push(prefix);
        spans.extend(content.spans);
        lines.push(Line::from(spans));
    }
}

pub(crate) fn push_thinking_lines(
    lines: &mut Vec<Line<'static>>,
    thinking: &str,
    width: u16,
    theme: &Theme,
    expanded: bool,
) {
    if thinking.is_empty() {
        return;
    }
    lines.push(Line::from(Span::styled(
        "● thinking",
        Style::default()
            .fg(theme.text_dim)
            .add_modifier(Modifier::ITALIC),
    )));
    let think_lines: Vec<&str> = thinking.lines().collect();
    let max_show = if expanded { usize::MAX } else { 20 };
    let show = think_lines.len().min(max_show);
    for line in &think_lines[..show] {
        push_wrapped_indented_text(lines, line, width, 2, Style::default().fg(theme.text_muted));
    }
    if think_lines.len() > show {
        push_wrapped_indented_text(
            lines,
            &format!(
                "... ({} more lines, ctrl+o to expand)",
                think_lines.len() - show
            ),
            width,
            2,
            Style::default().fg(theme.text_muted),
        );
    } else if expanded && think_lines.len() > 20 {
        push_wrapped_indented_text(
            lines,
            "… (ctrl+o to collapse)",
            width,
            2,
            Style::default().fg(theme.text_muted),
        );
    }
    lines.push(Line::from(""));
}

fn status_icon(tc: &crate::app::DisplayToolCall) -> &'static str {
    if tc.stopping {
        return "…";
    }
    if tc.output.is_some() {
        if tc.is_error {
            "✗"
        } else {
            "✓"
        }
    } else if tc.queued_behind.is_some() {
        "…"
    } else {
        "·"
    }
}

fn render_tool_call_lines(
    lines: &mut Vec<Line<'static>>,
    tc: &crate::app::DisplayToolCall,
    width: u16,
    theme: &Theme,
    first_bullet: bool,
    include_toggle_hint: bool,
) -> Option<usize> {
    use crate::tool_renderers::ToolRenderRegistry;

    // Status suffixes are appended after the chip; reserve their columns so
    // the composed line never exceeds the viewport width.
    let mut suffix = String::new();
    if tc.output.is_none() {
        if let Some(behind) = tc.queued_behind.as_ref() {
            suffix.push_str(&format!(" queued behind {behind}"));
        } else if let Some(started) = tc.started_at {
            let secs = started.elapsed().as_secs();
            if secs >= 2 {
                suffix.push_str(&format!(" {secs}s"));
            }
        }
        if tc.stopping {
            suffix.push_str(" stopping…");
        }
    }
    let suffix_cols = UnicodeWidthStr::width(suffix.as_str()).min(u16::MAX as usize) as u16;
    let chip_width = width.saturating_sub(suffix_cols);
    let input_truncated = ToolRenderRegistry::chip_truncated(tc, chip_width);

    if first_bullet {
        let mut label = format!(
            "{} {}",
            status_icon(tc),
            ToolRenderRegistry::chip_label(tc, chip_width)
        );
        label.push_str(&suffix);
        // Degenerate narrow terminals can still overflow once the bullet is
        // prepended; clamp so nothing spills past the viewport edge.
        let label = truncate_display_width(&label, (width as usize).saturating_sub(2));
        lines.push(Line::from(vec![
            Span::styled("● ", Style::default().fg(theme.text)),
            Span::styled(label, ToolRenderRegistry::chip_style(tc, theme)),
        ]));
    } else {
        lines.push(tool_continuation_line(tc, width, theme));
    }

    let output_lines = tc.output.as_ref().map(|o| o.lines().count()).unwrap_or(0);
    if !tc.collapsed {
        // The chip ellipsizes long commands; the expanded view spells them out.
        if input_truncated {
            lines.extend(ToolRenderRegistry::full_input_lines(tc, width, theme));
        }
        let toggle_line = if include_toggle_hint && (output_lines > 1 || input_truncated) {
            let line = lines.len();
            lines.push(Line::from(Span::styled(
                "  … (ctrl+o to collapse)",
                Style::default().fg(theme.text_muted),
            )));
            Some(line)
        } else {
            None
        };
        let max_preview = if include_toggle_hint { usize::MAX } else { 12 };
        lines.extend(ToolRenderRegistry::summary_lines(
            tc,
            width,
            theme,
            max_preview,
        ));
        return toggle_line;
    } else if include_toggle_hint && (output_lines > 1 || input_truncated) {
        let line = lines.len();
        lines.push(Line::from(Span::styled(
            if output_lines > 1 {
                format!("  … ({output_lines} more lines, ctrl+o to expand)")
            } else {
                "  … (ctrl+o to expand)".to_string()
            },
            Style::default().fg(theme.text_muted),
        )));
        return Some(line);
    }
    None
}

const SKILL_ARGS_PREVIEW_MAX: usize = 200;

/// kimi `SkillActivationComponent`: `▶ Activated skill: name` + optional dim args.
fn push_skill_activation_lines(
    lines: &mut Vec<Line<'static>>,
    name: &str,
    args: Option<&str>,
    width: u16,
    theme: &Theme,
) {
    lines.push(Line::from(vec![
        Span::styled(
            "▶ Activated skill: ",
            Style::default()
                .fg(theme.primary)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            name.to_string(),
            Style::default()
                .fg(theme.role_user)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    if let Some(raw) = args {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            let preview: String = if trimmed.chars().count() > SKILL_ARGS_PREVIEW_MAX {
                format!(
                    "{}…",
                    trimmed
                        .chars()
                        .take(SKILL_ARGS_PREVIEW_MAX)
                        .collect::<String>()
                )
            } else {
                trimmed.to_string()
            };
            let avail = (width as usize).saturating_sub(2).max(1);
            for chunk in wrap_display_cols(&preview, avail) {
                lines.push(Line::from(Span::styled(
                    format!("  {chunk}"),
                    Style::default().fg(theme.text_dim),
                )));
            }
        }
    }
}

fn wrap_display_cols(s: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return vec![s.to_string()];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    for grapheme in s.graphemes(true) {
        let w = UnicodeWidthStr::width(grapheme);
        if cur_w + w > max_width && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
            cur_w = 0;
        }
        cur.push_str(grapheme);
        cur_w += w;
    }
    if !cur.is_empty() || out.is_empty() {
        let _ = UnicodeWidthStr::width(cur.as_str());
        out.push(cur);
    }
    out
}

fn render_tool_history_lines(
    lines: &mut Vec<Line<'static>>,
    hist: &ToolHistorySummary,
    width: u16,
    theme: &Theme,
    locale: Locale,
    first_bullet: bool,
) -> usize {
    let explore_only = !hist.tools.is_empty()
        && hist
            .tools
            .iter()
            .all(|t| matches!(t.name.as_str(), "Read" | "Grep" | "Glob") && !t.is_error);
    let overview = if explore_only {
        format!("Explored {} files ✓", hist.tool_count)
    } else {
        i18n::tool_history_overview(locale, hist.tool_count, hist.duration_ms, hist.tokens)
    };
    let hint = if hist.expanded {
        i18n::tool_history_collapse_hint(locale)
    } else {
        i18n::tool_history_expand_hint(locale)
    };
    let prefix = if first_bullet { "● " } else { "  " };
    let hint_line = lines.len();
    lines.push(Line::from(vec![
        Span::styled(prefix, Style::default().fg(theme.text_dim)),
        Span::styled(
            format!("… {overview}"),
            Style::default().fg(theme.text_muted),
        ),
        Span::styled(format!(" ({hint})"), Style::default().fg(theme.text_dim)),
    ]));

    if hist.expanded {
        for tc in &hist.tools {
            let mut shown = tc.clone();
            shown.collapsed = false;
            let _ = render_tool_call_lines(lines, &shown, width, theme, false, false);
        }
    }
    hint_line
}

const TODO_MAX_VISIBLE: usize = 5;

fn subagent_strip_height(state: &AppState) -> u16 {
    let lines = state.subagents.strip_lines(std::time::Instant::now());
    if lines.is_empty() {
        0
    } else {
        // separator + one row per strip line
        1u16.saturating_add(lines.len() as u16)
    }
}

fn render_subagent_strip(f: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    if area.height == 0 {
        return;
    }
    let lines_text = state.subagents.strip_lines(std::time::Instant::now());
    if lines_text.is_empty() {
        return;
    }
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        "─".repeat(area.width as usize),
        Style::default().fg(theme.border),
    )));
    for text in lines_text {
        lines.push(Line::from(Span::styled(
            truncate_display_width(&format!("  {text}"), area.width as usize),
            Style::default().fg(theme.text_dim),
        )));
    }
    f.render_widget(Paragraph::new(Text::from(lines)), area);
}

fn todo_panel_height(state: &AppState, terminal_width: u16) -> u16 {
    if !todo_panel_visible(state) {
        return 0;
    }
    // separator + title + rows (+ optional overflow hint)
    let rows = if state.todos_expanded {
        state.todos.len()
    } else {
        state.todos.len().min(TODO_MAX_VISIBLE)
    };
    let toggle_hint = state.todos.len() > TODO_MAX_VISIBLE;
    if !is_narrow(terminal_width) {
        return (2 + rows + usize::from(toggle_hint)) as u16;
    }

    let content_width = terminal_width.saturating_sub(4).max(1) as usize;
    let visible = if state.todos_expanded {
        (0..state.todos.len()).collect::<Vec<_>>()
    } else {
        select_visible_todos(&state.todos).indices
    };
    let wrapped_rows = visible.iter().fold(0u16, |height, index| {
        let width = state
            .todos
            .get(*index)
            .map(|todo| UnicodeWidthStr::width(todo.content.as_str()))
            .unwrap_or(0);
        height.saturating_add(width.max(1).div_ceil(content_width) as u16)
    });
    2u16.saturating_add(wrapped_rows)
        .saturating_add(u16::from(toggle_hint))
}

fn render_todo_panel(f: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    if area.height == 0 || !todo_panel_visible(state) {
        return;
    }

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        "─".repeat(area.width as usize),
        Style::default().fg(theme.border),
    )));
    lines.push(Line::from(Span::styled(
        "  Todo",
        Style::default()
            .fg(theme.primary)
            .add_modifier(Modifier::BOLD),
    )));

    let visible = if state.todos_expanded {
        VisibleTodos {
            indices: (0..state.todos.len()).collect(),
            hidden: 0,
            hidden_done: 0,
            hidden_progress: 0,
            hidden_pending: 0,
        }
    } else {
        select_visible_todos(&state.todos)
    };

    for &i in &visible.indices {
        if let Some(todo) = state.todos.get(i) {
            lines.push(todo_row_line(todo, theme));
        }
    }

    if state.todos_expanded && state.todos.len() > TODO_MAX_VISIBLE {
        lines.push(Line::from(Span::styled(
            format!("  all {} items · ctrl+t to collapse", state.todos.len()),
            Style::default().fg(theme.text_dim),
        )));
    } else if visible.hidden > 0 {
        let mut parts = Vec::new();
        if visible.hidden_done > 0 {
            parts.push(format!("{} done", visible.hidden_done));
        }
        if visible.hidden_progress > 0 {
            parts.push(format!("{} in progress", visible.hidden_progress));
        }
        if visible.hidden_pending > 0 {
            parts.push(format!("{} pending", visible.hidden_pending));
        }
        let suffix = if parts.is_empty() {
            String::new()
        } else {
            format!(" ({})", parts.join(" · "))
        };
        lines.push(Line::from(Span::styled(
            format!("  … +{} more{} · ctrl+t to expand", visible.hidden, suffix),
            Style::default().fg(theme.text_dim),
        )));
    }

    f.render_widget(
        Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }),
        area,
    );
}

fn todo_panel_visible(state: &AppState) -> bool {
    !state.todos.is_empty()
        && (state.todos_expanded || state.todos.iter().any(|todo| !todo.is_finished()))
}

fn todo_row_line(todo: &TodoItem, theme: &Theme) -> Line<'static> {
    let (marker, marker_style, title_style) = match normalize_todo_status(&todo.status) {
        "in_progress" => (
            "●",
            Style::default()
                .fg(theme.primary)
                .add_modifier(Modifier::BOLD),
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        ),
        "completed" => (
            "✓",
            Style::default().fg(theme.success),
            Style::default()
                .fg(theme.text_dim)
                .add_modifier(Modifier::CROSSED_OUT),
        ),
        "cancelled" => (
            "✗",
            Style::default().fg(theme.error),
            Style::default().fg(theme.text_dim),
        ),
        _ => (
            "○",
            Style::default().fg(theme.text_dim),
            Style::default().fg(theme.text),
        ),
    };
    Line::from(vec![
        Span::raw("  "),
        Span::styled(marker.to_string(), marker_style),
        Span::raw(" "),
        Span::styled(todo.content.clone(), title_style),
    ])
}

fn normalize_todo_status(status: &str) -> &str {
    match status {
        "done" => "completed",
        other => other,
    }
}

struct VisibleTodos {
    indices: Vec<usize>,
    hidden: usize,
    hidden_done: usize,
    hidden_progress: usize,
    hidden_pending: usize,
}

/// kimi-style collapsed selector: all in_progress, then pending + latest done.
fn select_visible_todos(todos: &[TodoItem]) -> VisibleTodos {
    if todos.len() <= TODO_MAX_VISIBLE {
        return VisibleTodos {
            indices: (0..todos.len()).collect(),
            hidden: 0,
            hidden_done: 0,
            hidden_progress: 0,
            hidden_pending: 0,
        };
    }

    let mut in_progress = Vec::new();
    let mut pending = Vec::new();
    let mut done = Vec::new();
    for (i, todo) in todos.iter().enumerate() {
        match normalize_todo_status(&todo.status) {
            "in_progress" => in_progress.push(i),
            "completed" => done.push(i),
            "cancelled" => {}
            _ => pending.push(i),
        }
    }

    let mut picked = std::collections::BTreeSet::new();
    for &i in in_progress.iter().take(TODO_MAX_VISIBLE) {
        picked.insert(i);
    }

    if picked.len() < TODO_MAX_VISIBLE {
        let remaining = TODO_MAX_VISIBLE - picked.len();
        let done_rev: Vec<usize> = done.iter().copied().rev().collect();
        let (done_count, pending_count) = if done_rev.is_empty() {
            (0, remaining.min(pending.len()))
        } else if pending.is_empty() {
            (remaining.min(done_rev.len()), 0)
        } else {
            let mut pending_count = (remaining - 1).min(pending.len());
            let mut done_count = 1;
            if pending_count < remaining - 1 {
                done_count = (remaining - pending_count).min(done_rev.len());
            }
            let _ = &mut pending_count;
            (done_count, pending_count)
        };
        for &i in done_rev.iter().take(done_count) {
            picked.insert(i);
        }
        for &i in pending.iter().take(pending_count) {
            picked.insert(i);
        }
    }

    let mut hidden_done = 0;
    let mut hidden_progress = 0;
    let mut hidden_pending = 0;
    for (i, todo) in todos.iter().enumerate() {
        if picked.contains(&i) {
            continue;
        }
        match normalize_todo_status(&todo.status) {
            "in_progress" => hidden_progress += 1,
            "completed" => hidden_done += 1,
            "cancelled" => {}
            _ => hidden_pending += 1,
        }
    }

    let indices: Vec<usize> = picked.into_iter().collect();
    let shown = indices.len();
    VisibleTodos {
        indices,
        hidden: todos.len().saturating_sub(shown),
        hidden_done,
        hidden_progress,
        hidden_pending,
    }
}

fn tool_continuation_line(
    tc: &crate::app::DisplayToolCall,
    width: u16,
    theme: &Theme,
) -> Line<'static> {
    use crate::tool_renderers::ToolRenderRegistry;
    let color = if tc.is_error {
        theme.error
    } else if tc.output.is_some() {
        theme.success
    } else {
        theme.text_dim
    };
    Line::from(vec![
        Span::raw("  "),
        Span::styled(
            format!(
                "{} {}",
                status_icon(tc),
                ToolRenderRegistry::chip_label(tc, width.saturating_sub(2))
            ),
            Style::default().fg(color),
        ),
    ])
}

fn truncate_display_width(s: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(s) <= max_width {
        return s.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    let content_width = max_width.saturating_sub(UnicodeWidthChar::width('…').unwrap_or(1));
    let mut width = 0usize;
    let mut out = String::new();
    for grapheme in s.graphemes(true) {
        let grapheme_width = UnicodeWidthStr::width(grapheme);
        if width.saturating_add(grapheme_width) > content_width {
            break;
        }
        out.push_str(grapheme);
        width = width.saturating_add(grapheme_width);
    }
    out.push('…');
    out
}

fn render_input(f: &mut Frame, area: Rect, state: &mut AppState, theme: &Theme) {
    let border = match state.mode {
        AppMode::Shell => theme.shell_mode,
        AppMode::Plan => theme.plan_mode,
        AppMode::Btw => theme.accent,
        AppMode::Normal => {
            if matches!(
                state.status,
                SessionStatus::Thinking
                    | SessionStatus::ToolExecuting
                    | SessionStatus::WaitingApproval
            ) {
                theme.primary
            } else {
                theme.border
            }
        }
    };

    let (prefix, prefix_style) = match state.mode {
        AppMode::Shell => (
            "! ",
            Style::default()
                .fg(theme.shell_mode)
                .add_modifier(Modifier::BOLD),
        ),
        AppMode::Plan => (
            "plan > ",
            Style::default()
                .fg(theme.plan_mode)
                .add_modifier(Modifier::BOLD),
        ),
        AppMode::Btw => (
            "btw > ",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        AppMode::Normal => ("> ", Style::default().fg(theme.text)),
    };

    let title = match state.mode {
        AppMode::Shell => " shell ",
        AppMode::Plan => " plan ",
        AppMode::Btw if state.btw.streaming => " BTW · answering… ",
        AppMode::Btw => " BTW question ",
        AppMode::Normal => match state.status {
            SessionStatus::Thinking => " thinking… ",
            SessionStatus::ToolExecuting => " running… ",
            SessionStatus::WaitingApproval => " approval ",
            _ => " message ",
        },
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .title_style(Style::default().fg(border))
        .border_style(Style::default().fg(border));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let prefix_w = UnicodeWidthStr::width(prefix);
    let content_width = (inner.width as usize).saturating_sub(prefix_w).max(1);
    let indent = " ".repeat(prefix_w);

    // Mouse selection state is byte offsets into `state.input.text`.
    let sel = state.input.selection.filter(|(s, e)| s < e);

    // Soft-wrap each logical line; prefix only the first visual row of the buffer.
    let mut visual: Vec<Line> = Vec::new();
    for (li, logical) in input_logical_lines(&state.input.text)
        .into_iter()
        .enumerate()
    {
        let chunks = soft_wrap_line(logical, content_width);
        for (ci, chunk) in chunks.into_iter().enumerate() {
            if li == 0 && ci == 0 {
                visual.push(Line::from(vec![
                    Span::styled(prefix, prefix_style),
                    Span::styled(chunk, Style::default().fg(theme.text)),
                ]));
            } else {
                visual.push(Line::from(vec![
                    Span::raw(indent.clone()),
                    Span::styled(chunk, Style::default().fg(theme.text)),
                ]));
            }
        }
    }
    if visual.is_empty() {
        visual.push(Line::from(vec![Span::styled(prefix, prefix_style)]));
    }

    let (cursor_x, cursor_y) = cursor_position(
        &state.input.text,
        state.input.cursor,
        content_width,
        prefix_w as u16,
    );
    // Exact wrap boundary can place the cursor on a fresh visual row — pad so it paints.
    while (visual.len() as u16) <= cursor_y {
        visual.push(Line::from(vec![
            Span::raw(indent.clone()),
            Span::styled(String::new(), Style::default().fg(theme.text)),
        ]));
    }

    // Highlight the active selection across visual rows. Byte offsets map
    // 1:1 onto visual rows because `soft_wrap_line` never drops characters.
    if let Some((sel_start, sel_end)) = sel {
        let sel_style = crate::selection::selection_style();
        let mut byte = 0usize; // byte offset where the current visual row starts
        let mut row = 0u16;
        for logical in input_logical_lines(&state.input.text) {
            let line_start = byte;
            let mut consumed = 0usize;
            for chunk in soft_wrap_line(logical, content_width) {
                let chunk_start = line_start + consumed;
                let chunk_end = chunk_start + chunk.len();
                if sel_end > chunk_start && sel_start < chunk_end {
                    if let Some(line) = visual.get_mut(row as usize) {
                        highlight_byte_span(
                            line,
                            chunk_start.max(sel_start),
                            chunk_end.min(sel_end),
                            sel_style,
                        );
                    }
                }
                consumed += chunk.len();
                row += 1;
            }
            // Skip the '\n' that joins this logical line to the next one.
            byte = line_start + consumed + 1;
            row += 1;
        }
    }

    let view_h = inner.height.max(1);
    let scroll = (cursor_y + 1).saturating_sub(view_h);

    let paragraph = Paragraph::new(Text::from(visual)).scroll((scroll, 0));
    f.render_widget(paragraph, inner);

    // Remember the content geometry so mouse events can map clicks back to
    // composer bytes (row within the visual layout + content column).
    state.input_area = inner;
    state.input_content_width = content_width as u16;
    state.input_scroll = scroll;

    // A visible selection hides the terminal caret (focus follows the caret,
    // which would otherwise render at the selection focus and look wrong).
    if !state.input.selection_active() {
        let abs_x = inner.x + cursor_x;
        let abs_y = inner.y + cursor_y.saturating_sub(scroll);
        if abs_x < inner.x + inner.width && abs_y < inner.y + inner.height {
            f.set_cursor_position((abs_x, abs_y));
        }
    }
}

/// Highlight `text[start_byte..end_byte]` inside a rendered input row,
/// splitting spans on the byte range (merged with the selection style).
/// The leading chrome span (mode prefix / continuation indent) is not part of
/// the selection byte space.
fn highlight_byte_span(line: &mut Line<'static>, start: usize, end: usize, sel: Style) {
    let mut out: Vec<Span<'static>> = Vec::with_capacity(line.spans.len() + 2);
    let mut spans = line.spans.drain(..);
    // Translate text-space targets into line-space (skip leading chrome).
    let chrome = match spans.next() {
        Some(chrome) => {
            let len = chrome.content.len();
            out.push(chrome);
            len
        }
        None => 0,
    };
    let (start, end) = (start + chrome, end + chrome);
    let mut byte = chrome;
    for span in spans {
        let span_end = byte + span.content.len();
        if span_end <= start || byte >= end {
            byte = span_end;
            out.push(span);
            continue;
        }
        let style = span.style;
        let content = span.content;
        // Portion before the selection.
        if start > byte {
            let head = content[..start - byte].to_string();
            out.push(Span::styled(head, style));
        }
        // Selected portion.
        let sel_from = start.max(byte);
        let sel_to = end.min(span_end);
        if sel_to > sel_from {
            let mid = content[sel_from - byte..sel_to - byte].to_string();
            let mut merged = style;
            if let Some(bg) = sel.bg {
                merged = merged.bg(bg);
            }
            if let Some(fg) = sel.fg {
                merged = merged.fg(fg);
            }
            out.push(Span::styled(mid, merged));
        }
        // Portion after the selection.
        if span_end > end {
            let tail = content[end - byte..].to_string();
            out.push(Span::styled(tail, style));
        }
        byte = span_end;
    }
    line.spans = out;
}

/// Visual-row layout of the composer: `(visual_row, byte_start, byte_end)` per
/// row, excluding prefix/indent chrome. Built with the same soft-wrap pass the
/// renderer uses, so mouse clicks can map cells back to byte offsets.
pub struct InputLayoutRow {
    pub row: u16,
    pub start: usize,
    pub end: usize,
}

pub fn input_layout(text: &str, content_width: usize) -> Vec<InputLayoutRow> {
    let width = content_width.max(1);
    let mut out = Vec::new();
    let mut byte = 0usize;
    for logical in input_logical_lines(text) {
        let line_start = byte;
        let mut consumed = 0usize;
        for chunk in soft_wrap_line(logical, width) {
            out.push(InputLayoutRow {
                row: out.len() as u16,
                start: line_start + consumed,
                end: line_start + consumed + chunk.len(),
            });
            consumed += chunk.len();
        }
        // Skip the '\n' that joins this logical line to the next one.
        byte = line_start + consumed + 1;
    }
    out
}

/// Map a click (row, display column) inside the composer content area to a
/// byte offset in `text`. Columns past the end of a row snap to its last
/// boundary so clicks on empty space park the caret at the row end.
pub fn byte_at_visual_cell(text: &str, content_width: usize, row: u16, col: u16) -> Option<usize> {
    let layout = input_layout(text, content_width);
    let target = layout.iter().find(|l| l.row == row)?;
    let segment = text.get(target.start..target.end)?;
    let mut col_seen = 0usize;
    for (i, c) in segment.char_indices() {
        if col_seen >= col as usize {
            return Some(target.start + i);
        }
        col_seen += c.width().unwrap_or(0);
    }
    Some(target.end)
}

/// Compute (x, y) relative to the input inner area for the cursor, with soft-wrap.
fn cursor_position(text: &str, cursor: usize, content_width: usize, prefix_w: u16) -> (u16, u16) {
    let width = content_width.max(1);
    if text.is_empty() {
        return (prefix_w, 0);
    }
    let mut safe = cursor.min(text.len());
    while safe > 0 && !text.is_char_boundary(safe) {
        safe -= 1;
    }

    let lines = input_logical_lines(text);
    let mut y: u16 = 0;
    let mut pos = 0;
    for (idx, logical) in lines.iter().enumerate() {
        let line_start = pos;
        let line_end = pos + logical.len();
        let on_this = if idx + 1 < lines.len() {
            // Non-final logical line owns [start, end] (not the trailing `\n`).
            safe >= line_start && safe <= line_end
        } else {
            safe >= line_start
        };

        if on_this {
            let mut col_end = safe.min(line_end).saturating_sub(line_start);
            while col_end > 0 && !logical.is_char_boundary(col_end) {
                col_end -= 1;
            }
            let (col, wrap_row) = wrapped_cursor_offset(&logical[..col_end], width);
            let x = prefix_w + col as u16;
            return (x, y.saturating_add(wrap_row));
        }

        let rows = soft_wrap_line(logical, width).len() as u16;
        y = y.saturating_add(rows.max(1));
        pos = line_end;
        if pos < text.len() && text.as_bytes()[pos] == b'\n' {
            pos += 1;
        }
    }
    (prefix_w, y.saturating_sub(1))
}

fn wrapped_cursor_offset(text: &str, width: usize) -> (usize, u16) {
    let width = width.max(1);
    let mut column = 0usize;
    let mut row = 0u16;
    for grapheme in text.graphemes(true) {
        let grapheme_width = UnicodeWidthStr::width(grapheme);
        if column.saturating_add(grapheme_width) > width && column > 0 {
            row = row.saturating_add(1);
            column = 0;
        }
        column = column.saturating_add(grapheme_width);
        if column >= width {
            row = row.saturating_add(1);
            column = 0;
        }
    }
    (column, row)
}

fn render_footer(
    f: &mut Frame,
    area: Rect,
    state: &mut AppState,
    config: &AppConfig,
    theme: &Theme,
) {
    if is_narrow(area.width) {
        render_narrow_footer(f, area, state, config, theme);
        return;
    }

    // Line 1: yolo  model  thinking  cwd  git ............ tip
    let mut left: Vec<Span> = Vec::new();

    match state.permission_mode {
        PermissionMode::Auto => {
            left.push(Span::styled(
                "auto",
                Style::default()
                    .fg(theme.warning)
                    .add_modifier(Modifier::BOLD),
            ));
            left.push(Span::raw("  "));
        }
        PermissionMode::Yolo => {
            left.push(Span::styled(
                "yolo",
                Style::default()
                    .fg(theme.warning)
                    .add_modifier(Modifier::BOLD),
            ));
            left.push(Span::raw("  "));
        }
        PermissionMode::Manual => {}
    }

    if state.plan_mode {
        left.push(Span::styled(
            "plan",
            Style::default()
                .fg(theme.plan_mode)
                .add_modifier(Modifier::BOLD),
        ));
        left.push(Span::raw("  "));
    }

    left.push(Span::styled(
        model_label(state, config),
        Style::default().fg(theme.text),
    ));

    if thinking_label(config).is_some() {
        left.push(Span::raw("  "));
        left.push(Span::styled(
            thinking_label(config).unwrap(),
            Style::default().fg(theme.text_dim),
        ));
    }

    {
        let cwd = &state.working_dir;
        left.push(Span::raw("  "));
        left.push(Span::styled(
            shorten_path(&cwd.to_string_lossy()),
            Style::default().fg(theme.text_dim),
        ));
        let git = git_badge::git_badge(cwd, config.workspace_trust.matching(cwd));
        if let Some(badge) = git.render() {
            left.push(Span::raw("  "));
            left.push(Span::styled(
                badge,
                Style::default().fg(if git.dirty {
                    theme.warning
                } else {
                    theme.text_dim
                }),
            ));
        }
    }

    left.push(Span::raw("  "));
    let label = if state.btw.streaming {
        "btw:answering (ctrl+g)".to_string()
    } else if state.btw.owner_session_id.is_some() || !state.btw.turns.is_empty() {
        format!("btw:{} (ctrl+g)", state.btw.turns.len())
    } else {
        "btw (ctrl+g)".to_string()
    };
    left.push(Span::styled(
        label,
        Style::default().fg(if state.mode == AppMode::Btw {
            theme.accent
        } else {
            theme.text_dim
        }),
    ));

    if let Some(ref goal) = state.status_bar.goal {
        left.push(Span::raw("  "));
        let color = match goal.status.as_str() {
            "active" => theme.accent,
            "paused" => theme.warning,
            "blocked" => theme.error,
            _ => theme.text_muted,
        };
        let label = if goal.status == "active" {
            "goal".to_string()
        } else {
            format!("goal:{}", goal.status)
        };
        // Record the chip's column span for click routing. The chip starts
        // after the "  " separator; measure everything rendered so far.
        let prefix: String = spans_to_string_approx(&left);
        let prefix_w = UnicodeWidthStr::width(prefix.as_str()) as u16;
        let chip_text = format!("{label} {}", truncate_display_width(&goal.description, 24));
        let chip_w = UnicodeWidthStr::width(chip_text.as_str()) as u16;
        state.footer_goal_chip = Some((
            area.y,
            area.x.saturating_add(prefix_w),
            area.x.saturating_add(prefix_w).saturating_add(chip_w),
        ));
        left.push(Span::styled(chip_text, Style::default().fg(color)));
    } else {
        state.footer_goal_chip = None;
    }

    let plan_review_hidden = state
        .approval_pending
        .as_ref()
        .is_some_and(|approval| approval.is_plan_review && approval.hidden);
    let approval_waiting = state.question_pending.is_some()
        && state
            .approval_pending
            .as_ref()
            .is_some_and(|approval| !approval.hidden);
    let tip = if plan_review_hidden {
        "plan review hidden · enter reopen · ctrl-c cancel".to_string()
    } else if approval_waiting {
        "1 approval waiting".to_string()
    } else if let Some(ref activity) = state.status_bar.activity {
        activity.clone()
    } else if let Some(ref hover) = state.strip_hover_title {
        hover.clone()
    } else if state.quit_dialog.is_some() {
        "T terminate · B background · Esc cancel".to_string()
    } else if state.quit_confirm {
        "press ctrl-c again to quit".to_string()
    } else if state.search.active {
        "↑↓ navigate · enter jump · esc close".to_string()
    } else if state.mode == AppMode::Btw && state.btw.scroll_offset > 0 {
        format!("↑{} lines · end to follow", state.btw.scroll_offset)
    } else if state.mode == AppMode::Btw {
        "ctrl+g back · ctrl+o fold thinking · ctrl+d delete BTW".to_string()
    } else if !state.follow_bottom && state.scroll_up > 0 {
        format!("↑{} lines · end to follow", state.scroll_up)
    } else {
        TIPS[(state.tick / 80) % TIPS.len()].to_string()
    };

    let left_line = spans_to_string_approx(&left);
    let left_w = UnicodeWidthStr::width(left_line.as_str()) as u16;
    let tip_w = UnicodeWidthStr::width(tip.as_str()) as u16;
    let pad = area.width.saturating_sub(left_w).saturating_sub(tip_w);

    let mut line1_spans = left;
    if pad > 0 && tip_w > 0 {
        line1_spans.push(Span::raw(" ".repeat(pad as usize)));
        line1_spans.push(Span::styled(
            tip,
            Style::default().fg(if state.quit_confirm || state.quit_dialog.is_some() {
                theme.warning
            } else if state.status_bar.activity.is_some() {
                theme.primary
            } else {
                theme.text_muted
            }),
        ));
    }

    // Line 2: per-session activity strip (left) + sandbox + context (right)
    sync_footer_session_entries(state);
    let context = format_context(state, config);
    let ctx_w = UnicodeWidthStr::width(context.as_str());
    let (sandbox, sandbox_color) = sandbox_indicator(&config.sandbox.mode, theme);
    let sandbox_w = UnicodeWidthStr::width(sandbox);
    let gap = 2usize;
    let right_w = sandbox_w.saturating_add(gap).saturating_add(ctx_w);
    let strip_budget = (area.width as usize)
        .saturating_sub(right_w)
        .saturating_sub(gap);
    let (session_spans, relative_hits) =
        state
            .workspace_sessions
            .render_spans_with_hits(strip_budget, theme, state.tick);
    let mut line2_spans: Vec<Span> = Vec::new();
    let strip_origin_x = area.x;
    state.footer_area = area;
    state.session_strip_origin_x = strip_origin_x;
    state.session_strip_hits = relative_hits
        .into_iter()
        .map(|mut h| {
            h.x0 = h.x0.saturating_add(strip_origin_x as usize);
            h.x1 = h.x1.saturating_add(strip_origin_x as usize);
            h
        })
        .collect();
    line2_spans.extend(session_spans);
    let strip_text: String = line2_spans.iter().map(|s| s.content.clone()).collect();
    let strip_w = UnicodeWidthStr::width(strip_text.as_str());
    let pad2 = area
        .width
        .saturating_sub(strip_w as u16)
        .saturating_sub(right_w as u16);
    if pad2 > 0 {
        line2_spans.push(Span::raw(" ".repeat(pad2 as usize)));
    }
    line2_spans.push(Span::styled(sandbox, Style::default().fg(sandbox_color)));
    line2_spans.push(Span::raw(" ".repeat(gap)));
    line2_spans.push(Span::styled(context, Style::default().fg(theme.text)));
    let line2 = Line::from(line2_spans);

    f.render_widget(
        Paragraph::new(Text::from(vec![Line::from(line1_spans), line2])),
        area,
    );
}

fn render_narrow_footer(
    f: &mut Frame,
    area: Rect,
    state: &mut AppState,
    config: &AppConfig,
    theme: &Theme,
) {
    sync_footer_session_entries(state);
    state.footer_area = area;
    state.session_strip_origin_x = area.x;

    let mut status_parts = Vec::new();
    match state.permission_mode {
        PermissionMode::Auto => status_parts.push("auto".to_string()),
        PermissionMode::Yolo => status_parts.push("yolo".to_string()),
        PermissionMode::Manual => {}
    }
    if state.plan_mode {
        status_parts.push("plan".to_string());
    }
    status_parts.push(model_label(state, config));
    if let Some(ref goal) = state.status_bar.goal {
        let label = if goal.status == "active" {
            format!("goal {}", goal.description)
        } else {
            format!("goal:{} {}", goal.status, goal.description)
        };
        status_parts.push(label);
    }
    if let Some(activity) = state.status_bar.activity.as_ref() {
        status_parts.push(activity.clone());
    } else if state.quit_confirm {
        status_parts.push("ctrl-c again to quit".into());
    } else {
        let status = match state.status {
            SessionStatus::Thinking => Some("thinking…"),
            SessionStatus::ToolExecuting => Some("running…"),
            SessionStatus::WaitingApproval => Some("approval"),
            SessionStatus::WaitingQuestion => Some("question"),
            SessionStatus::Compacting => Some("compacting…"),
            SessionStatus::Cancelling => Some("cancelling…"),
            _ => None,
        };
        if let Some(status) = status {
            status_parts.push(status.into());
        }
    }
    let status = truncate_display_width(&status_parts.join(" · "), area.width as usize);
    let line1 = Line::from(Span::styled(
        status,
        Style::default().fg(if state.status_bar.activity.is_some() {
            theme.primary
        } else {
            theme.text
        }),
    ));

    let (session_spans, relative_hits) =
        state
            .workspace_sessions
            .render_spans_with_hits(area.width as usize, theme, state.tick);
    state.session_strip_hits = relative_hits
        .into_iter()
        .map(|mut hit| {
            hit.x0 = hit.x0.saturating_add(area.x as usize);
            hit.x1 = hit.x1.saturating_add(area.x as usize);
            hit
        })
        .collect();
    let has_session_strip = !session_spans.is_empty();
    let line2 = if !has_session_strip {
        Line::from(Span::styled(
            truncate_display_width(
                &shorten_path(&state.working_dir.to_string_lossy()),
                area.width as usize,
            ),
            Style::default().fg(theme.text_dim),
        ))
    } else {
        Line::from(session_spans)
    };

    let context = format_context_compact(state, config);
    let (sandbox, sandbox_color) = sandbox_indicator_compact(&config.sandbox.mode, theme);
    let right = if area.width >= 32 {
        format!("{sandbox}  {context}")
    } else {
        context
    };
    let right = truncate_display_width(&right, area.width as usize);
    let right_width = UnicodeWidthStr::width(right.as_str());
    let mut line3_spans = Vec::new();
    let cwd_budget = (area.width as usize)
        .saturating_sub(right_width)
        .saturating_sub(2);
    if has_session_strip && cwd_budget > 3 {
        let cwd = shorten_path(&state.working_dir.to_string_lossy());
        line3_spans.push(Span::styled(
            truncate_display_width(&cwd, cwd_budget),
            Style::default().fg(theme.text_dim),
        ));
        let used = line3_spans
            .iter()
            .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
            .sum::<usize>();
        line3_spans.push(Span::raw(
            " ".repeat((area.width as usize).saturating_sub(used + right_width)),
        ));
    }
    line3_spans.push(Span::styled(
        right,
        Style::default().fg(if area.width >= 32 {
            sandbox_color
        } else {
            theme.text
        }),
    ));

    f.render_widget(
        Paragraph::new(Text::from(vec![line1, line2, Line::from(line3_spans)])),
        area,
    );
}

fn sync_footer_session_entries(state: &mut AppState) {
    let Some(current_id) = state.session_id.clone() else {
        return;
    };

    if !state
        .workspace_sessions
        .entries
        .iter()
        .any(|entry| entry.id == current_id)
    {
        let Some(current_tab) = state.tab_strip.tabs.iter().find(|tab| tab.id == current_id) else {
            return;
        };
        let title = if current_tab.title.is_empty() {
            "session".into()
        } else {
            current_tab.title.clone()
        };
        state
            .workspace_sessions
            .entries
            .push(crate::chrome::WorkspaceSessionEntry {
                id: current_id.clone(),
                title,
                status: state.status,
                dirty: false,
                needs_attention: false,
                working_dir: None,
                primary_workspace: true,
            });
    }

    for entry in &mut state.workspace_sessions.entries {
        if entry.id == current_id {
            entry.status = state.status;
        } else if let Some(tab) = state.tab_strip.tabs.iter().find(|tab| tab.id == entry.id) {
            entry.status = tab.status;
            entry.dirty = tab.dirty;
        }
        entry.needs_attention = state.parked_approvals.contains_key(&entry.id)
            || state.parked_questions.contains_key(&entry.id);
    }
    state.workspace_sessions.active = state
        .workspace_sessions
        .entries
        .iter()
        .position(|entry| entry.id == current_id)
        .unwrap_or(0);

    // Child agents get one compact group entry. The existing `/agents` browser
    // owns individual selection, keeping the session strip readable even for a
    // large swarm.
    let agent_count = state.subagents.entries.len();
    let running_agents = state
        .subagents
        .entries
        .iter()
        .filter(|entry| entry.is_active())
        .count();
    let failed_agents = state
        .subagents
        .entries
        .iter()
        .filter(|entry| matches!(entry.status.as_str(), "failed" | "cancelled"))
        .count();
    state.workspace_sessions.agent_group_entry =
        (agent_count > 0).then(|| crate::chrome::WorkspaceSessionEntry {
            id: "agents".into(),
            title: if running_agents > 0 {
                format!("agents {running_agents}/{agent_count}")
            } else {
                format!("agents {agent_count}")
            },
            status: if running_agents > 0 {
                SessionStatus::ToolExecuting
            } else {
                SessionStatus::Idle
            },
            dirty: false,
            needs_attention: failed_agents > 0,
            working_dir: None,
            primary_workspace: true,
        });
    state.workspace_sessions.agent_group_focused = state.active_subagent_view.is_some();
}

fn render_scroll_hint(f: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    if state.follow_bottom || state.scroll_up == 0 || area.height < 2 {
        return;
    }
    let max = state.max_scroll_up();
    if max == 0 {
        return;
    }
    let label = format!(" ↓ {} new lines · end to follow ", state.scroll_up);
    let w = UnicodeWidthStr::width(label.as_str()) as u16;
    if w >= area.width {
        return;
    }
    let hint_area = Rect {
        x: area.x + area.width.saturating_sub(w + 1),
        y: area.y,
        width: w,
        height: 1,
    };
    f.render_widget(
        Paragraph::new(Span::styled(
            label,
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        )),
        hint_area,
    );
}

fn render_search_overlay(f: &mut Frame, size: Rect, state: &AppState, theme: &Theme) {
    let hit_rows = state.search.hits.len().min(10) as u16;
    let area = popup_rect(size, 72, hit_rows.saturating_add(4));
    if area.width == 0 || area.height == 0 {
        return;
    }
    let inner_width = area.width.saturating_sub(2) as usize;
    f.render_widget(Clear, area);

    let mut lines: Vec<Line> = Vec::new();
    let query_budget = inner_width.saturating_sub(10).max(1);
    let scope_label = match state.search.scope {
        crate::search::SearchScope::Local => "local",
        crate::search::SearchScope::Global => "all",
    };
    lines.push(Line::from(vec![
        Span::styled(
            format!(" find[{scope_label}] "),
            Style::default()
                .fg(theme.primary)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                " {} ",
                truncate_display_width(&state.search.query, query_budget)
            ),
            Style::default().fg(theme.text_strong).bg(theme.border),
        ),
        Span::styled(
            if state.search.query.is_empty() {
                " type… Ctrl-Tab scope".into()
            } else {
                format!(
                    "  {}/{}",
                    state
                        .search
                        .selected
                        .saturating_add(1)
                        .min(state.search.hits.len().max(1)),
                    state.search.hits.len()
                )
            },
            Style::default().fg(theme.text_muted),
        ),
    ]));
    lines.push(Line::from(Span::styled(
        "─".repeat(inner_width),
        Style::default().fg(theme.border),
    )));

    if state.search.hits.is_empty() {
        lines.push(Line::from(Span::styled(
            if state.search.query.is_empty() {
                "  start typing…"
            } else {
                "  no matches"
            },
            Style::default().fg(theme.text_muted),
        )));
    } else {
        let start = state.search.selected.saturating_sub(4);
        for (i, hit) in state.search.hits.iter().enumerate().skip(start).take(10) {
            let selected = i == state.search.selected;
            let marker = if selected { "›" } else { " " };
            let style = if selected {
                Style::default()
                    .fg(theme.text_strong)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.text_dim)
            };
            let mut spans = vec![Span::styled(format!(" {marker} "), style)];
            let mut used = 3usize;
            if let Some(title) = hit.title.as_deref().filter(|s| !s.is_empty()) {
                let label = truncate_display_width(title, 14);
                used += label.chars().count() + 1;
                spans.push(Span::styled(
                    format!("{label} "),
                    Style::default().fg(if selected {
                        theme.accent
                    } else {
                        theme.text_muted
                    }),
                ));
            } else if area.width >= 32 {
                spans.push(Span::styled(
                    format!("{:8} ", truncate_display_width(&hit.role, 8)),
                    Style::default().fg(if selected {
                        theme.accent
                    } else {
                        theme.text_muted
                    }),
                ));
                used += 9;
            }
            let preview_budget = inner_width.saturating_sub(used).max(8);
            spans.push(Span::styled(
                truncate_display_width(&hit.preview, preview_budget),
                style,
            ));
            lines.push(Line::from(spans));
        }
    }

    lines.push(Line::from(Span::styled(
        if area.width < 42 {
            " ↑↓ · enter · Ctrl-Tab · esc"
        } else {
            " ↑↓ select · enter jump · Ctrl-Tab local/all · esc"
        },
        Style::default().fg(theme.text_muted),
    )));

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border_focus))
        .title(" search ");
    f.render_widget(Paragraph::new(Text::from(lines)).block(block), area);
}

fn spans_to_string_approx(spans: &[Span]) -> String {
    spans.iter().map(|s| s.content.as_ref()).collect()
}

/// The model of the subagent whose view is currently active (resolved
/// alias for internal/built-in runs, external agent name for ACP), if any.
fn active_subagent_model(state: &AppState) -> Option<String> {
    state
        .active_subagent_view
        .as_ref()
        .and_then(|id| state.subagents.entries.iter().find(|e| e.id == *id))
        .and_then(|entry| entry.model.clone())
}

fn model_label(state: &AppState, config: &AppConfig) -> String {
    // While a subagent view is active the footer reports that agent's
    // model; entries without model info fall back to the agent name so the
    // user still sees which agent they are looking at.
    if let Some(id) = state.active_subagent_view.as_ref() {
        if let Some(entry) = state.subagents.entries.iter().find(|e| e.id == *id) {
            let label = match entry.model.as_deref() {
                Some(model) => config
                    .resolve_model(model)
                    .map(|(m, _)| m.display_name.clone().unwrap_or_else(|| m.model.clone()))
                    .unwrap_or_else(|| model.to_string()),
                None => entry.name.clone(),
            };
            return label;
        }
    }
    let alias = state
        .model_alias
        .as_deref()
        .or_else(|| config.default_model_alias())
        .unwrap_or("?");
    if let Some((model, _)) = config.resolve_model(alias) {
        model
            .display_name
            .clone()
            .unwrap_or_else(|| model.model.clone())
    } else {
        alias.to_string()
    }
}

fn thinking_label(config: &AppConfig) -> Option<String> {
    let t = config.thinking.as_ref()?;
    if !t.enabled {
        return None;
    }
    Some(format!("thinking: {}", t.effort.as_deref().unwrap_or("on")))
}

fn sandbox_indicator(mode: &str, theme: &Theme) -> (&'static str, Color) {
    match mode.trim().to_ascii_lowercase().as_str() {
        "disabled" | "off" | "none" => ("● sandbox:off", theme.error),
        "process" => ("● sandbox:process", theme.warning),
        "workspace" | "strict" => ("● sandbox:workspace", theme.success),
        "auto" if cfg!(target_os = "windows") => ("● sandbox:process", theme.warning),
        "auto" => ("● sandbox:workspace", theme.success),
        _ => ("● sandbox:unknown", theme.text_muted),
    }
}

fn sandbox_indicator_compact(mode: &str, theme: &Theme) -> (&'static str, Color) {
    match mode.trim().to_ascii_lowercase().as_str() {
        "disabled" | "off" | "none" => ("sbx:off", theme.error),
        "process" => ("sbx:proc", theme.warning),
        "workspace" | "strict" => ("sbx:work", theme.success),
        "auto" if cfg!(target_os = "windows") => ("sbx:proc", theme.warning),
        "auto" => ("sbx:work", theme.success),
        _ => ("sbx:?", theme.text_muted),
    }
}

fn format_context(state: &AppState, config: &AppConfig) -> String {
    let max = state
        .model_alias
        .as_deref()
        .or_else(|| config.default_model_alias())
        .and_then(|a| config.resolve_model(a))
        .and_then(|(m, _)| m.max_context_size)
        .unwrap_or(256_000);
    // Prefer server-authoritative usage; fall back to char estimate only when empty.
    let used = if state.approx_tokens > 0 {
        state.approx_tokens
    } else {
        state
            .messages
            .iter()
            .map(|m| m.content.len() as u64 / 4)
            .sum::<u64>()
    };
    let pct = used
        .saturating_mul(100)
        .checked_div(max)
        .unwrap_or(0)
        .min(100);
    let warn = if pct >= 90 {
        " ! "
    } else if pct >= 70 {
        " ~ "
    } else {
        " "
    };
    format!(
        "context:{warn}{}% ({}/{})",
        pct,
        format_tokens(used),
        format_tokens(max)
    )
}

fn format_context_compact(state: &AppState, config: &AppConfig) -> String {
    let max = state
        .model_alias
        .as_deref()
        .or_else(|| config.default_model_alias())
        .and_then(|alias| config.resolve_model(alias))
        .and_then(|(model, _)| model.max_context_size)
        .unwrap_or(256_000);
    let used = if state.approx_tokens > 0 {
        state.approx_tokens
    } else {
        state
            .messages
            .iter()
            .map(|message| message.content.len() as u64 / 4)
            .sum::<u64>()
    };
    let pct = used
        .saturating_mul(100)
        .checked_div(max)
        .unwrap_or(0)
        .min(100);
    format!("ctx:{pct}% {}/{}", format_tokens(used), format_tokens(max))
}

fn format_tokens(n: u64) -> String {
    if n >= 1024 * 1024 {
        format!("{:.1}M", n as f64 / (1024.0 * 1024.0))
    } else if n >= 1024 {
        format!("{}k", (n + 512) / 1024)
    } else {
        n.to_string()
    }
}

fn render_slash_menu(f: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let Some(menu) = state.slash_menu.as_ref() else {
        return;
    };

    let max_visible = 8u16;
    let rows = if menu.items.is_empty() {
        1u16
    } else {
        (menu.items.len() as u16).min(max_visible)
    };

    // `area` is already the slash popup area reserved by the parent layout.
    // Use a small inner margin so items don't touch the border.
    let inner = area.inner(Margin::new(1, 1));

    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(" / ");

    let mut lines: Vec<Line> = Vec::new();
    if menu.items.is_empty() {
        lines.push(Line::from(Span::styled(
            "No matching commands",
            Style::default().fg(theme.text_muted),
        )));
    } else {
        let start = menu
            .selected
            .saturating_sub(max_visible as usize - 1)
            .min(menu.items.len().saturating_sub(rows as usize));
        let end = (start + rows as usize).min(menu.items.len());
        for (i, item) in menu.items[start..end].iter().enumerate() {
            let idx = start + i;
            let selected = idx == menu.selected;
            let prefix = if selected { "> " } else { "  " };
            let name_style = if selected {
                Style::default()
                    .fg(theme.primary)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.text)
            };
            let hint = item
                .argument_hint
                .as_deref()
                .map(|h| format!(" {}", h))
                .unwrap_or_default();
            let label = format!("/{}{}", item.name, hint);
            if is_narrow(area.width) {
                lines.push(Line::from(vec![
                    Span::styled(prefix, name_style),
                    Span::styled(
                        truncate_display_width(&label, (inner.width as usize).saturating_sub(2)),
                        name_style,
                    ),
                ]));
                lines.push(Line::from(Span::styled(
                    format!(
                        "  {}",
                        truncate_display_width(
                            &item.description,
                            (inner.width as usize).saturating_sub(2),
                        )
                    ),
                    Style::default().fg(theme.text_dim),
                )));
                continue;
            }
            // Primary column ~28, then description
            let primary_w = 28usize.min((inner.width as usize).saturating_sub(4).max(1));
            let label_width = UnicodeWidthStr::width(label.as_str());
            let padded = if label_width < primary_w {
                format!("{}{}", label, " ".repeat(primary_w - label_width))
            } else {
                truncate_display_width(&label, primary_w)
            };
            lines.push(Line::from(vec![
                Span::styled(prefix, name_style),
                Span::styled(padded, name_style),
                Span::styled(
                    truncate_display_width(
                        &item.description,
                        (inner.width as usize).saturating_sub(primary_w + 4),
                    ),
                    Style::default().fg(theme.text_dim),
                ),
            ]));
        }
    }

    f.render_widget(Paragraph::new(Text::from(lines)).block(block), area);
}

fn render_file_menu(f: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let Some(menu) = state.file_menu.as_ref() else {
        return;
    };

    let max_visible = 10u16;
    let rows = if menu.items.is_empty() {
        1u16
    } else {
        (menu.items.len() as u16).min(max_visible)
    };

    let inner = area.inner(Margin::new(1, 1));
    f.render_widget(Clear, area);

    let title = if menu.query.is_empty() {
        " @ files ".to_string()
    } else {
        format!(
            " @ {} ",
            truncate_display_width(&menu.query, area.width.saturating_sub(6) as usize)
        )
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border_focus))
        .title(title);

    let mut lines: Vec<Line> = Vec::new();
    if menu.items.is_empty() {
        lines.push(Line::from(Span::styled(
            "No matching files",
            Style::default().fg(theme.text_muted),
        )));
    } else {
        let start = menu
            .selected
            .saturating_sub(max_visible as usize - 1)
            .min(menu.items.len().saturating_sub(rows as usize));
        let end = (start + rows as usize).min(menu.items.len());
        for (i, item) in menu.items[start..end].iter().enumerate() {
            let idx = start + i;
            let selected = idx == menu.selected;
            let marker = if selected { "›" } else { " " };
            let style = if selected {
                Style::default()
                    .fg(theme.text_strong)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.text)
            };
            let kind = if item.is_directory { "dir " } else { "file" };
            let path = item.insert.trim_start_matches('@');
            lines.push(Line::from(vec![
                Span::styled(format!(" {marker} "), style),
                Span::styled(
                    format!("{kind} "),
                    Style::default().fg(if selected {
                        theme.accent
                    } else {
                        theme.text_muted
                    }),
                ),
                Span::styled(
                    truncate_display_width(path, (inner.width as usize).saturating_sub(10)),
                    style,
                ),
            ]));
        }
    }

    f.render_widget(Paragraph::new(Text::from(lines)).block(block), area);
}

fn render_session_delete_confirm(f: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let Some(confirm) = state.session_delete_confirm.as_ref() else {
        return;
    };
    let inner = area.inner(Margin::new(1, 1));
    f.render_widget(Clear, area);
    let title = if confirm.permanent {
        " Permanently delete session "
    } else {
        " Close session tab "
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.warning))
        .title(title);
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        format!(
            " {}",
            truncate_display_width(&confirm.label, inner.width.saturating_sub(2) as usize)
        ),
        Style::default().fg(theme.text),
    )));
    if confirm.busy {
        lines.push(Line::from(Span::styled(
            if confirm.permanent {
                " Busy turn will be interrupted, then history deleted."
            } else {
                " Busy turn keeps running in the background."
            },
            Style::default().fg(theme.warning),
        )));
    } else if confirm.permanent {
        lines.push(Line::from(Span::styled(
            " This permanently deletes transcript history.",
            Style::default().fg(theme.error),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            " Closes this tab only — history is kept (use /sessions Ctrl-D to delete).",
            Style::default().fg(theme.text_dim),
        )));
    }
    lines.push(Line::from(""));
    let yes = if confirm.permanent {
        "Yes — permanently delete"
    } else if confirm.busy {
        "Yes — close tab (keep running)"
    } else {
        "Yes — close tab"
    };
    for (i, label) in ["No — cancel", yes].iter().enumerate() {
        let selected = confirm.selected == i;
        let prefix = if selected { "> " } else { "  " };
        let style = if selected {
            Style::default()
                .fg(if i == 1 { theme.error } else { theme.primary })
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.text_dim)
        };
        lines.push(Line::from(Span::styled(format!("{prefix}{label}"), style)));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " ↑↓ select · Enter confirm · Esc cancel ",
        Style::default().fg(theme.text_muted),
    )));
    f.render_widget(
        Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: false })
            .block(block),
        area,
    );
}

fn render_quit_dialog(f: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let Some(dialog) = state.quit_dialog.as_ref() else {
        return;
    };
    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.warning))
        .title(" A turn is still running ");
    let options = [
        ("[T] Terminate", "kill the turn, exit TUI"),
        ("[B] Background", "keep it running, exit TUI"),
        ("[Esc] Cancel", "stay in TUI"),
    ];
    let mut lines: Vec<Line> = vec![
        Line::from(Span::styled(
            " Choose how to leave while work continues on the server.",
            Style::default().fg(theme.text),
        )),
        Line::from(""),
    ];
    for (i, (label, detail)) in options.iter().enumerate() {
        let selected = dialog.selected == i;
        let prefix = if selected { "> " } else { "  " };
        let style = if selected {
            Style::default()
                .fg(match i {
                    0 => theme.error,
                    1 => theme.primary,
                    _ => theme.text,
                })
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.text_dim)
        };
        lines.push(Line::from(Span::styled(
            format!("{prefix}{label}  — {detail}"),
            style,
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " ↑↓ select · Enter confirm · T / B / Esc ",
        Style::default().fg(theme.text_muted),
    )));
    f.render_widget(
        Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: false })
            .block(block),
        area,
    );
}

fn render_list_picker(f: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let Some(picker) = state.list_picker.as_ref() else {
        return;
    };

    let max_visible = 10u16;
    let rows = if picker.items.is_empty() {
        1u16
    } else {
        (picker.items.len() as u16).min(max_visible)
    };

    // Delete confirm: ↑↓ between No / Yes, Enter to confirm (default No).
    if picker.kind == crate::app::ListPickerKind::Session && state.session_delete_confirm.is_some()
    {
        render_session_delete_confirm(f, area, state, theme);
        return;
    }

    let inner = area.inner(Margin::new(1, 1));
    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border))
        .title(picker.title.as_str());

    let mut lines: Vec<Line> = Vec::new();

    if picker.items.is_empty() {
        lines.push(Line::from(Span::styled(
            "No items",
            Style::default().fg(theme.text_muted),
        )));
    } else {
        let start = picker
            .selected
            .saturating_sub(max_visible as usize - 1)
            .min(picker.items.len().saturating_sub(rows as usize));
        let end = (start + rows as usize).min(picker.items.len());
        for (i, item) in picker.items[start..end].iter().enumerate() {
            let idx = start + i;
            let selected = idx == picker.selected;
            let prefix = if selected { "> " } else { "  " };
            let name_style = if selected {
                Style::default()
                    .fg(theme.primary)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.text)
            };
            if is_narrow(area.width) {
                lines.push(Line::from(vec![
                    Span::styled(prefix, name_style),
                    Span::styled(
                        truncate_display_width(
                            &item.label,
                            (inner.width as usize).saturating_sub(2),
                        ),
                        name_style,
                    ),
                ]));
                lines.push(Line::from(Span::styled(
                    format!(
                        "  {}",
                        truncate_display_width(
                            &item.detail,
                            (inner.width as usize).saturating_sub(2),
                        )
                    ),
                    Style::default().fg(theme.text_dim),
                )));
                continue;
            }
            let primary_w = if picker.kind == crate::app::ListPickerKind::HistoryEdit {
                (inner.width as usize).saturating_sub(4)
            } else {
                36usize.min((inner.width as usize).saturating_sub(4))
            }
            .max(1);
            let label_width = UnicodeWidthStr::width(item.label.as_str());
            let padded = if label_width < primary_w {
                format!("{}{}", item.label, " ".repeat(primary_w - label_width))
            } else {
                truncate_display_width(&item.label, primary_w)
            };
            lines.push(Line::from(vec![
                Span::styled(prefix, name_style),
                Span::styled(padded, name_style),
                Span::styled(
                    truncate_display_width(
                        &item.detail,
                        (inner.width as usize).saturating_sub(primary_w + 4),
                    ),
                    Style::default().fg(theme.text_dim),
                ),
            ]));
        }
    }

    f.render_widget(Paragraph::new(Text::from(lines)).block(block), area);
}

fn format_elapsed_hms(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

fn render_tasks_panel(f: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let Some(panel) = state.tasks_panel.as_ref() else {
        return;
    };

    if let Some(detail) = panel.detail.as_ref() {
        render_task_detail(f, area, detail, theme);
        return;
    }

    let panel_area = popup_rect(area, 90, 28);
    if panel_area.width == 0 || panel_area.height == 0 {
        return;
    }

    f.render_widget(Clear, panel_area);
    let block = Block::default()
        .title(" background processes ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border));

    let inner = panel_area.inner(Margin::new(1, 1));
    let mut lines: Vec<Line> = Vec::new();
    if panel.tasks.is_empty() {
        lines.push(Line::from(Span::styled(
            "No running background processes in this session.",
            Style::default().fg(theme.text_muted),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Background shells (Bash run_in_background, or commands that hit the foreground timeout) appear here.",
            Style::default().fg(theme.text_dim),
        )));
    } else {
        let rows_per_task = if is_narrow(panel_area.width) { 2 } else { 1 };
        let max_visible = (inner.height.saturating_sub(1) as usize / rows_per_task).max(1);
        let start = panel
            .selected
            .saturating_sub(max_visible.saturating_sub(1))
            .min(panel.tasks.len().saturating_sub(max_visible.max(1)));
        let end = (start + max_visible).min(panel.tasks.len());
        for (i, task) in panel.tasks[start..end].iter().enumerate() {
            let idx = start + i;
            let selected = idx == panel.selected;
            let prefix = if selected { "> " } else { "  " };
            let style = if selected {
                Style::default()
                    .fg(theme.primary)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.text)
            };
            let elapsed = format_elapsed_hms(task.elapsed_secs);
            if is_narrow(panel_area.width) {
                lines.push(Line::from(Span::styled(
                    truncate_display_width(
                        &format!("{}{} {}", prefix, elapsed, task.task_id),
                        inner.width as usize,
                    ),
                    style,
                )));
                lines.push(Line::from(Span::styled(
                    format!(
                        "  {}",
                        truncate_display_width(
                            &task.description,
                            (inner.width as usize).saturating_sub(2),
                        )
                    ),
                    Style::default().fg(theme.text_dim),
                )));
            } else {
                let label = format!(
                    "{}{} {} — {}",
                    prefix,
                    elapsed,
                    truncate_display_width(&task.description, 40),
                    truncate_display_width(&task.command, 48)
                );
                lines.push(Line::from(Span::styled(
                    truncate_display_width(&label, inner.width as usize),
                    style,
                )));
            }
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        if panel_area.width < 40 {
            "↑↓ · ⏎ out · x stop · Esc"
        } else {
            "↑↓ select · ⏎ output · r refresh · x stop · Esc close"
        },
        Style::default().fg(theme.text_muted),
    )));

    f.render_widget(Paragraph::new(Text::from(lines)).block(block), panel_area);
}

/// Wrap `s` into lines of at most `width` display columns (word-ish split).
fn wrap_to_width(s: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    for raw in s.split('\n') {
        if raw.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut current_width = 0usize;
        let mut current = String::new();
        for grapheme in raw.graphemes(true) {
            let gw = UnicodeWidthStr::width(grapheme);
            if current_width + gw > width {
                out.push(std::mem::take(&mut current));
                current_width = 0;
            }
            current.push_str(grapheme);
            current_width += gw;
        }
        out.push(current);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

fn render_task_detail(f: &mut Frame, area: Rect, detail: &TaskDetailState, theme: &Theme) {
    let panel_area = popup_rect(area, 92, 40);
    if panel_area.width == 0 || panel_area.height == 0 {
        return;
    }

    f.render_widget(Clear, panel_area);
    let status_label = if detail.running {
        format!("running {}", format_elapsed_hms(detail.elapsed_secs))
    } else {
        match detail.exit_code {
            Some(code) => format!("{} (exit {})", detail.status, code),
            None => detail.status.clone(),
        }
    };
    let block = Block::default()
        .title(format!(" task {} · {} ", detail.task_id, status_label))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border));

    let inner = panel_area.inner(Margin::new(1, 1));
    // Fixed header + footer, scrolling output between them.
    let footer_height = 1u16;
    let header_lines = vec![
        Line::from(Span::styled(
            format!(
                "cmd: {}",
                truncate_display_width(&detail.command, inner.width as usize)
            ),
            Style::default().fg(theme.text),
        )),
        Line::from(Span::styled(
            format!(
                "desc: {}",
                truncate_display_width(&detail.description, inner.width as usize)
            ),
            Style::default().fg(theme.text_dim),
        )),
        Line::from(""),
    ];
    let header_height = header_lines.len() as u16;
    let output_area_height = inner
        .height
        .saturating_sub(header_height)
        .saturating_sub(footer_height);

    // Pre-wrap output so scroll offsets count wrapped lines consistently.
    let wrapped = wrap_to_width(&detail.output, inner.width as usize);
    let max_scroll = (wrapped.len() as u16).saturating_sub(output_area_height);
    let scroll = detail.scroll.min(max_scroll);
    let visible: Vec<Line> = wrapped
        .iter()
        .skip(scroll as usize)
        .take(output_area_height as usize)
        .map(|l| Line::from(Span::styled(l.clone(), Style::default().fg(theme.text))))
        .collect();

    let mut lines = header_lines;
    lines.extend(visible);
    lines.push(Line::from(Span::styled(
        if panel_area.width < 40 {
            "↑↓ scroll · r ref · Esc"
        } else {
            "↑↓/PgUp/PgDn scroll · r refresh · x stop · Esc back"
        },
        Style::default().fg(theme.text_muted),
    )));

    f.render_widget(Paragraph::new(Text::from(lines)).block(block), panel_area);
}

fn render_subagents_panel(f: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let Some(panel) = state.subagents_panel.as_ref() else {
        return;
    };

    let panel_area = popup_rect(area, 92, 40);
    if panel_area.width == 0 || panel_area.height == 0 {
        return;
    }

    f.render_widget(Clear, panel_area);
    let title = if panel.detail {
        " subagent detail "
    } else {
        " subagents "
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border));
    let inner = panel_area.inner(Margin::new(1, 1));
    let mut lines: Vec<Line> = Vec::new();
    let now = std::time::Instant::now();

    if state.subagents.entries.is_empty() {
        lines.push(Line::from(Span::styled(
            "No subagents in this session yet.",
            Style::default().fg(theme.text_muted),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "When the main agent spawns a child, status appears here and in the strip above Todo.",
            Style::default().fg(theme.text_dim),
        )));
    } else if panel.detail {
        let Some(entry) = state.subagents.entries.get(panel.selected) else {
            return;
        };
        lines.push(Line::from(Span::styled(
            truncate_display_width(
                &format!(
                    "[{}] {} ({})",
                    entry.status,
                    entry.name,
                    crate::subagents::short_id(&entry.id)
                ),
                inner.width as usize,
            ),
            Style::default()
                .fg(theme.primary)
                .add_modifier(Modifier::BOLD),
        )));
        if !entry.description.is_empty() {
            lines.push(Line::from(Span::styled(
                truncate_display_width(&entry.description, inner.width as usize),
                Style::default().fg(theme.text_dim),
            )));
        }
        lines.push(Line::from(Span::styled(
            format!("elapsed {}s", entry.elapsed_secs(now)),
            Style::default().fg(theme.text_muted),
        )));
        if let Some(ref result) = entry.result_or_error {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "result / error:",
                Style::default().fg(theme.text_muted),
            )));
            for chunk in result.lines().take(6) {
                lines.push(Line::from(Span::styled(
                    truncate_display_width(chunk, inner.width as usize),
                    Style::default().fg(theme.text),
                )));
            }
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "recent activity:",
            Style::default().fg(theme.text_muted),
        )));
        if entry.events.is_empty() {
            lines.push(Line::from(Span::styled(
                "(waiting for child output…)",
                Style::default().fg(theme.text_dim),
            )));
        } else {
            let max_events = inner.height.saturating_sub(10) as usize;
            let start = entry.events.len().saturating_sub(max_events.max(1));
            for event in entry.events.iter().skip(start) {
                lines.push(Line::from(Span::styled(
                    truncate_display_width(&format!("· {event}"), inner.width as usize),
                    Style::default().fg(theme.text_dim),
                )));
            }
        }
    } else {
        let max_visible = (inner.height.saturating_sub(2) as usize).max(1);
        let start = panel
            .selected
            .saturating_sub(max_visible.saturating_sub(1))
            .min(
                state
                    .subagents
                    .entries
                    .len()
                    .saturating_sub(max_visible.max(1)),
            );
        let end = (start + max_visible).min(state.subagents.entries.len());
        for (i, entry) in state.subagents.entries[start..end].iter().enumerate() {
            let idx = start + i;
            let selected = idx == panel.selected;
            let prefix = if selected { "> " } else { "  " };
            let style = if selected {
                Style::default()
                    .fg(theme.primary)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.text)
            };
            let label = format!("{}{}", prefix, entry.strip_line(now));
            lines.push(Line::from(Span::styled(
                truncate_display_width(&label, inner.width as usize),
                style,
            )));
            if selected {
                lines.push(Line::from(Span::styled(
                    truncate_display_width(
                        &format!("    status={} · Enter for log", entry.status),
                        inner.width as usize,
                    ),
                    Style::default().fg(theme.text_muted),
                )));
            }
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        if panel.detail {
            "Enter list · Esc back"
        } else {
            "↑↓ · Enter detail · Esc close"
        },
        Style::default().fg(theme.text_muted),
    )));
    f.render_widget(Paragraph::new(Text::from(lines)).block(block), panel_area);
}

/// Wrap text to `width`, preserving a styled prefix on the first visual line.
fn push_plan_focus_lines(
    lines: &mut Vec<Line<'static>>,
    path: &str,
    content: &str,
    width: u16,
    theme: &Theme,
) {
    lines.push(Line::from(""));
    push_plan_box_lines(lines, path, content, width, theme, true);
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  scroll = full plan only · shift-tab / /plan to exit",
        Style::default().fg(theme.text_muted),
    )));
    lines.push(Line::from(""));
}

/// Full plan document in a box (no line cap). Used in transcript and plan-focus mode.
fn push_plan_box_lines(
    lines: &mut Vec<Line<'static>>,
    path: &str,
    content: &str,
    width: u16,
    theme: &Theme,
    _focused: bool,
) {
    let safe_w = width.max(1) as usize;
    if safe_w < 8 {
        for line in content.lines() {
            lines.push(Line::from(Span::styled(
                line.to_string(),
                Style::default().fg(theme.text),
            )));
        }
        return;
    }

    let left_margin = 2usize;
    let side_pad = 1usize;
    // "  ┌" + horz + "┐"  ⇒ horz = width - 4
    let horz_len = safe_w.saturating_sub(left_margin + 2).max(2);
    let content_width = horz_len.saturating_sub(2 * side_pad).max(1);
    let indent = " ".repeat(left_margin);
    let border = Style::default().fg(theme.plan_mode);

    let basename = std::path::Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let title = if basename.is_empty() {
        " plan ".to_string()
    } else {
        format!(" plan: {basename} ")
    };
    let title = if UnicodeWidthStr::width(title.as_str()) + 1 > horz_len {
        " plan ".to_string()
    } else {
        title
    };
    let title_vis = UnicodeWidthStr::width(title.as_str());
    let dash_after = horz_len.saturating_sub(title_vis);

    lines.push(Line::from(vec![
        Span::raw(indent.clone()),
        Span::styled(format!("┌{title}{}┐", "─".repeat(dash_after)), border),
    ]));

    // Full plan body — never truncate. Render as markdown (display only).
    let md_lines = if content.is_empty() {
        vec![Line::from("")]
    } else {
        crate::markdown::render(content, content_width, theme)
    };
    if md_lines.is_empty() {
        let pad = content_width;
        lines.push(Line::from(vec![
            Span::raw(indent.clone()),
            Span::styled("│", border),
            Span::raw(" "),
            Span::raw(" ".repeat(pad)),
            Span::raw(" "),
            Span::styled("│", border),
        ]));
    } else {
        for content_line in md_lines {
            let chunk_w: usize = content_line
                .spans
                .iter()
                .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
                .sum();
            let pad = content_width.saturating_sub(chunk_w);
            let mut spans = Vec::with_capacity(content_line.spans.len() + 4);
            spans.push(Span::raw(indent.clone()));
            spans.push(Span::styled("│", border));
            spans.push(Span::raw(" "));
            spans.extend(content_line.spans);
            spans.push(Span::raw(" ".repeat(pad)));
            spans.push(Span::raw(" "));
            spans.push(Span::styled("│", border));
            lines.push(Line::from(spans));
        }
    }

    lines.push(Line::from(vec![
        Span::raw(indent),
        Span::styled(format!("└{}┘", "─".repeat(horz_len)), border),
    ]));
}

pub(crate) fn push_wrapped_prefixed(
    lines: &mut Vec<Line<'static>>,
    prefix: &str,
    text: &str,
    width: u16,
    prefix_style: Style,
    text_style: Style,
) {
    push_wrapped_prefixed_with_continuation_indent(
        lines,
        prefix,
        text,
        width,
        prefix_style,
        text_style,
        UnicodeWidthStr::width(prefix),
    );
}

fn push_wrapped_prefixed_with_continuation_indent(
    lines: &mut Vec<Line<'static>>,
    prefix: &str,
    text: &str,
    width: u16,
    prefix_style: Style,
    text_style: Style,
    continuation_indent: usize,
) {
    let width = width as usize;
    let first_width = width.saturating_sub(UnicodeWidthStr::width(prefix)).max(1);
    let continuation_indent = continuation_indent.min(width.saturating_sub(1));
    let continuation_width = width.saturating_sub(continuation_indent).max(1);
    let indent = " ".repeat(continuation_indent);
    let mut first = true;
    for para in text.split('\n') {
        if para.is_empty() {
            if first {
                lines.push(Line::from(vec![
                    Span::styled(prefix.to_string(), prefix_style),
                    Span::styled(String::new(), text_style),
                ]));
                first = false;
            } else {
                lines.push(Line::from(""));
            }
            continue;
        }
        let line_width = if first {
            first_width
        } else {
            continuation_width
        };
        for chunk in wrap_str_with_first_width(para, line_width, continuation_width) {
            if first {
                lines.push(Line::from(vec![
                    Span::styled(prefix.to_string(), prefix_style),
                    Span::styled(chunk, text_style),
                ]));
                first = false;
            } else {
                lines.push(Line::from(vec![
                    Span::raw(indent.clone()),
                    Span::styled(chunk, text_style),
                ]));
            }
        }
    }
}

fn wrap_str_with_first_width(
    s: &str,
    first_width: usize,
    continuation_width: usize,
) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    let mut max_width = first_width.max(1);
    for ch in s.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if cur_w + w > max_width && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
            cur_w = 0;
            max_width = continuation_width.max(1);
        }
        cur.push(ch);
        cur_w += w;
    }
    if !cur.is_empty() || out.is_empty() {
        out.push(cur);
    }
    out
}

fn wrap_str(s: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return vec![s.to_string()];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    for grapheme in s.graphemes(true) {
        let w = UnicodeWidthStr::width(grapheme);
        if cur_w + w > max_width && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
            cur_w = 0;
        }
        cur.push_str(grapheme);
        cur_w += w;
    }
    if !cur.is_empty() || out.is_empty() {
        out.push(cur);
    }
    out
}

fn render_approval_panel(f: &mut Frame, area: Rect, approval: &mut PendingApproval, theme: &Theme) {
    let panel_width = popup_rect(area, 72, 1).width;
    if panel_width == 0 {
        return;
    }

    let mut lines: Vec<Line> = vec![
        Line::from(Span::styled(
            approval.action.clone(),
            Style::default()
                .fg(theme.text_strong)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];

    if !approval.detail.is_empty() && !approval.is_plan_review {
        for l in approval.detail.lines().take(8) {
            lines.push(Line::from(Span::styled(
                l.to_string(),
                Style::default().fg(theme.text_dim),
            )));
        }
        lines.push(Line::from(""));
    }

    for (i, choice) in approval.choices.iter().enumerate() {
        let selected = i == approval.selected;
        let style = if selected {
            Style::default()
                .fg(theme.primary)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.text_dim)
        };
        let marker = if selected { "> " } else { "  " };
        let key = format!("{}", i + 1);
        push_wrapped_prefixed(
            &mut lines,
            &format!("{marker}{key}  "),
            &choice.label,
            panel_width.saturating_sub(2),
            style,
            style,
        );
    }

    if approval.feedback_mode {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  修改意见:",
            Style::default().fg(theme.accent),
        )));
        lines.push(Line::from(Span::styled(
            format!("  {}▌", approval.feedback),
            Style::default().fg(theme.text),
        )));
        lines.push(Line::from(Span::styled(
            "  enter 提交 · esc 取消输入",
            Style::default().fg(theme.text_muted),
        )));
    } else {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            if panel_width < 42 {
                "  1-9 / ↑↓ · enter · esc"
            } else if approval.is_plan_review {
                "  1·2·3… / ↑↓ / enter · esc 取消"
            } else {
                "  1·2·3 / enter"
            },
            Style::default().fg(theme.text_muted),
        )));
    }

    let desired_height = wrapped_text_height(&lines, panel_width.saturating_sub(2))
        .saturating_add(2)
        .max(4);
    let mut panel_area = popup_rect(area, 72, desired_height);
    if approval.is_plan_review && area.height > panel_area.height {
        // Sit near the bottom so the plan document stays visible above.
        panel_area.y = area.y + area.height.saturating_sub(panel_area.height + 1);
    }
    f.render_widget(Clear, panel_area);

    let title = if approval.is_plan_review {
        " plan review "
    } else {
        " permission "
    };
    let block = Block::default()
        .title(format!(
            "{title}· {} ",
            if approval.is_plan_review {
                "ExitPlanMode"
            } else {
                approval.tool_name.as_str()
            }
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if approval.is_plan_review {
            theme.plan_mode
        } else {
            theme.warning
        }));
    f.render_widget(
        Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: false })
            .block(block),
        panel_area,
    );
}

fn render_question_panel(f: &mut Frame, area: Rect, question: &mut PendingQuestion, theme: &Theme) {
    if area.width < 3 || area.height < 3 {
        return;
    }

    // Keep both borders visible on phone terminals while allowing the modal
    // to grow to its normal width on larger viewports.
    let panel_width = popup_rect(area, 72, 1).width;
    let content_width = panel_width.saturating_sub(4).max(1);

    let question_style = Style::default()
        .fg(theme.text_strong)
        .add_modifier(Modifier::BOLD);
    let mut lines: Vec<Line<'static>> = Vec::new();
    push_wrapped_text_with_first_line_indent(
        &mut lines,
        &question.text,
        content_width as usize,
        question_style,
        2,
        2,
    );
    lines.push(Line::from(""));
    let mut selected_range = (0usize, lines.len());

    for (i, (_id, label)) in question.options.iter().enumerate() {
        let selected = i == question.selected;
        let checked = question.toggled.get(i).copied().unwrap_or(false);
        let style = if selected {
            Style::default()
                .fg(theme.primary)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.text_dim)
        };
        let marker = if selected { "> " } else { "  " };
        let boxc = if question.allow_multiple {
            if checked {
                "[x]"
            } else {
                "[ ]"
            }
        } else if checked || selected {
            "(•)"
        } else {
            "( )"
        };
        let prefix = format!("{marker}{} {boxc}  ", i + 1);
        let start = lines.len();
        push_wrapped_prefixed_with_continuation_indent(
            &mut lines,
            &prefix,
            label,
            content_width,
            Style::default().fg(theme.text_muted),
            style,
            2,
        );
        if selected {
            selected_range = (start, lines.len());
        }
    }

    if question.allow_free_text {
        let selected = question.selected >= question.options.len();
        let style = if selected {
            Style::default()
                .fg(theme.primary)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.text_dim)
        };
        let marker = if selected { "> " } else { "  " };
        lines.push(Line::from(""));
        let start = lines.len();
        push_wrapped_prefixed(
            &mut lines,
            &format!("{marker}text: "),
            &format!("{}▌", question.free_text),
            content_width,
            Style::default().fg(theme.text_muted),
            style,
        );
        if selected {
            selected_range = (start, lines.len());
        }
    }

    lines.push(Line::from(""));
    let hint = if question.allow_multiple {
        if panel_width < 42 {
            "  1-9 / space · enter · esc"
        } else {
            "  1-9 / space toggle · enter confirm · esc cancel"
        }
    } else if panel_width < 42 {
        "  1-9 / ↑↓ · enter · esc"
    } else {
        "  1-9 submit · ↑↓ move · enter confirm · esc cancel"
    };
    push_wrapped_text(
        &mut lines,
        hint,
        content_width as usize,
        Style::default().fg(theme.text_muted),
    );

    // Size from visual rows after wrapping, not from the number of logical
    // options. When the content is taller than the screen, follow the selected
    // row so keyboard navigation makes every option reachable and visible.
    let max_panel_height = if area.height >= 10 {
        area.height.saturating_sub(2)
    } else {
        area.height
    };
    let desired_height = u16::try_from(lines.len())
        .unwrap_or(u16::MAX)
        .saturating_add(4);
    let panel_height = desired_height.min(max_panel_height).max(3).min(area.height);
    let inner_height = panel_height.saturating_sub(4) as usize;
    let selected_end_with_context = selected_range.1.saturating_add(2).min(lines.len());
    let scroll = selected_end_with_context
        .saturating_sub(inner_height)
        .min(lines.len().saturating_sub(inner_height));

    let x = area.x + area.width.saturating_sub(panel_width) / 2;
    let y = area.y + area.height.saturating_sub(panel_height) / 2;
    let panel_area = Rect::new(x, y, panel_width, panel_height);
    f.render_widget(Clear, panel_area);

    let block = Block::default()
        .title(" question ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.primary))
        .padding(Padding::uniform(1));

    f.render_widget(
        Paragraph::new(Text::from(lines))
            .scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0))
            .block(block),
        panel_area,
    );
}

fn push_wrapped_text_with_first_line_indent(
    lines: &mut Vec<Line<'static>>,
    text: &str,
    width: usize,
    style: Style,
    left_padding: usize,
    first_line_indent: usize,
) {
    let left_padding = left_padding.min(width.saturating_sub(1));
    let paragraph_width = width.saturating_sub(left_padding).max(1);
    let first_line_indent = first_line_indent.min(paragraph_width.saturating_sub(1));
    let padding = " ".repeat(left_padding);

    for logical_line in text.split('\n') {
        if logical_line.is_empty() {
            lines.push(Line::from(""));
            continue;
        }

        // Keep every visual row away from the panel edge, then prefix the
        // logical paragraph before wrapping so only its first row is indented
        // one additional level.
        let mut paragraph =
            String::with_capacity(first_line_indent.saturating_add(logical_line.len()));
        paragraph.push_str(&" ".repeat(first_line_indent));
        paragraph.push_str(logical_line);
        for chunk in wrap_str(&paragraph, paragraph_width) {
            lines.push(Line::from(Span::styled(format!("{padding}{chunk}"), style)));
        }
    }
}

pub(crate) fn push_wrapped_indented_text(
    lines: &mut Vec<Line<'static>>,
    text: &str,
    width: u16,
    indent: usize,
    style: Style,
) {
    // Last hop to the terminal: strip escape bytes from untrusted text
    // (thinking deltas, plan blocks, approval file contents) before wrapping
    // can split them.
    let sanitized = crate::sanitize::sanitize_text(text);
    push_wrapped_text_with_first_line_indent(
        lines,
        sanitized.as_ref(),
        width as usize,
        style,
        indent,
        0,
    );
}

fn push_wrapped_text(lines: &mut Vec<Line<'static>>, text: &str, width: usize, style: Style) {
    let sanitized = crate::sanitize::sanitize_text(text);
    let text = sanitized.as_ref();
    for logical_line in text.split('\n') {
        for chunk in wrap_str(logical_line, width) {
            lines.push(Line::from(Span::styled(chunk, style)));
        }
    }
}

fn shorten_path(path: &str) -> String {
    if let Some(home) = dirs::home_dir() {
        let home_str = home.to_string_lossy();
        if path == home_str.as_ref() {
            return "~".into();
        }
        if let Some(rest) = path.strip_prefix(home_str.as_ref()) {
            let rest = rest.strip_prefix('/').unwrap_or(rest);
            let segments: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
            if segments.len() > 3 {
                return format!("~/…/{}", segments[segments.len() - 3..].join("/"));
            }
            return format!("~/{}", rest);
        }
    }
    path.to_string()
}

#[cfg(test)]
mod render_smoke {
    use super::*;
    use crate::app::{
        AppState, DisplayMessage, DisplayPart, DisplayToolCall, MessageRole, ToolHistorySummary,
    };
    use kkagent_protocol::PermissionMode;
    use ratatui::{backend::TestBackend, buffer::Buffer, Terminal};

    fn buffer_rows(buffer: &Buffer) -> Vec<String> {
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .filter_map(|x| buffer.cell((x, y)))
                    .map(|cell| cell.symbol())
                    .collect::<String>()
            })
            .collect()
    }

    fn has_complete_inset_box(buffer: &Buffer) -> bool {
        buffer_rows(buffer).iter().any(|row| {
            let left = row.chars().position(|ch| ch == '┌');
            let right = row.chars().position(|ch| ch == '┐');
            matches!((left, right), (Some(left), Some(right)) if left > 0 && right < buffer.area.width.saturating_sub(1) as usize)
        })
    }

    fn pending_question(text: &str, options: Vec<(String, String)>) -> PendingQuestion {
        PendingQuestion {
            question_id: "question-1".into(),
            text: text.into(),
            toggled: vec![false; options.len()],
            options,
            allow_free_text: false,
            allow_multiple: false,
            selected: 0,
            free_text: String::new(),
        }
    }

    fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .fold(String::new(), |mut output, cell| {
                output.push_str(cell.symbol());
                output
            })
    }

    #[test]
    fn question_paragraphs_indent_only_the_first_visual_row() {
        let mut lines = Vec::new();
        push_wrapped_text_with_first_line_indent(
            &mut lines,
            "abcdefghij\n你好世界",
            8,
            Style::default(),
            2,
            2,
        );

        let rendered = lines.iter().map(Line::to_string).collect::<Vec<_>>();
        assert_eq!(rendered, ["    abcd", "  efghij", "    你好", "  世界"]);
    }

    #[test]
    fn question_option_continuations_use_a_compact_body_indent() {
        let mut lines = Vec::new();
        push_wrapped_prefixed_with_continuation_indent(
            &mut lines,
            "> 1 ( )  ",
            "abcdefghijklmnopqr",
            15,
            Style::default(),
            Style::default(),
            2,
        );

        let rendered = lines.iter().map(Line::to_string).collect::<Vec<_>>();
        assert_eq!(rendered, ["> 1 ( )  abcdef", "  ghijklmnopqr"]);
    }

    #[test]
    fn question_modal_sizes_for_wrapped_question_and_options() {
        let backend = TestBackend::new(60, 18);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut question = pending_question(
            "A long question that must wrap onto another visual row before the choices",
            vec![(
                "one".into(),
                "A long choice whose distinctive ending remains visible: OPTION_TAIL".into(),
            )],
        );

        terminal
            .draw(|frame| {
                let area = frame.area();
                render_question_panel(frame, area, &mut question, &Theme::default());
            })
            .unwrap();

        let rendered = buffer_text(&terminal);
        assert!(rendered.contains("wrap onto another"), "{rendered:?}");
        assert!(rendered.contains("OPTION_TAIL"), "{rendered:?}");
        assert!(rendered.contains("enter confirm"), "{rendered:?}");
    }

    #[test]
    fn question_modal_stays_within_a_narrow_terminal() {
        let backend = TestBackend::new(32, 16);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut question = pending_question(
            "Narrow terminal question",
            vec![(
                "one".into(),
                "This choice wraps across rows and ends with TAIL_OK".into(),
            )],
        );

        terminal
            .draw(|frame| {
                let area = frame.area();
                render_question_panel(frame, area, &mut question, &Theme::default());
            })
            .unwrap();

        let rendered = buffer_text(&terminal);
        assert!(rendered.contains("AIL_OK"), "{rendered:?}");
        assert!(has_complete_inset_box(terminal.backend().buffer()));
    }

    #[test]
    fn overflowing_question_modal_follows_the_selected_option() {
        let backend = TestBackend::new(50, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let options = (0..12)
            .map(|index| (format!("option-{index}"), format!("choice {index}")))
            .collect();
        let mut question = pending_question("Pick a choice", options);
        question.selected = 11;

        terminal
            .draw(|frame| {
                let area = frame.area();
                render_question_panel(frame, area, &mut question, &Theme::default());
            })
            .unwrap();

        let rendered = buffer_text(&terminal);
        assert!(rendered.contains("choice 11"), "{rendered:?}");
        assert!(rendered.contains("enter confirm"), "{rendered:?}");
        assert!(!rendered.contains("choice 0"), "{rendered:?}");
    }

    #[test]
    fn thinking_uses_the_same_ctrl_o_folding_in_all_views() {
        let thinking = (0..25)
            .map(|index| format!("reason {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let theme = Theme::default();
        let mut collapsed = Vec::new();
        push_thinking_lines(&mut collapsed, &thinking, 80, &theme, false);
        let collapsed_text = collapsed
            .iter()
            .map(|line| line.to_string())
            .collect::<String>();
        assert!(collapsed_text.contains("5 more lines, ctrl+o to expand"));
        assert!(!collapsed_text.contains("reason 24"));

        let mut expanded = Vec::new();
        push_thinking_lines(&mut expanded, &thinking, 80, &theme, true);
        let expanded_text = expanded
            .iter()
            .map(|line| line.to_string())
            .collect::<String>();
        assert!(expanded_text.contains("reason 24"));
        assert!(expanded_text.contains("ctrl+o to collapse"));
    }

    #[test]
    fn thinking_wraps_wide_text_within_the_terminal_width() {
        let mut lines = Vec::new();
        push_thinking_lines(
            &mut lines,
            "你好世界abcdefghij",
            10,
            &Theme::default(),
            false,
        );

        assert!(lines.len() > 3);
        for line in &lines[1..lines.len() - 1] {
            assert!(
                UnicodeWidthStr::width(line.to_string().as_str()) <= 10,
                "{:?}",
                line.to_string()
            );
        }
    }

    #[test]
    fn sandbox_indicator_distinguishes_effective_modes() {
        let theme = Theme::default();
        assert_eq!(
            sandbox_indicator("disabled", &theme),
            ("● sandbox:off", theme.error)
        );
        assert_eq!(
            sandbox_indicator("workspace", &theme),
            ("● sandbox:workspace", theme.success)
        );
        assert_eq!(
            sandbox_indicator("process", &theme),
            ("● sandbox:process", theme.warning)
        );
        assert_eq!(sandbox_indicator("off", &theme).1, theme.error);
        assert_eq!(
            sandbox_indicator("unexpected", &theme),
            ("● sandbox:unknown", theme.text_muted)
        );
        assert_eq!(
            sandbox_indicator("auto", &theme).0,
            if cfg!(target_os = "windows") {
                "● sandbox:process"
            } else {
                "● sandbox:workspace"
            }
        );
    }

    #[test]
    fn footer_places_disabled_sandbox_immediately_before_context() {
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = AppState::new(PermissionMode::Manual, false);
        let mut config = AppConfig::default();
        config.sandbox.mode = "disabled".into();

        terminal
            .draw(|frame| render_ui(frame, &mut state, &config))
            .unwrap();

        let buffer = terminal.backend().buffer();
        let footer = (0..buffer.area.width)
            .filter_map(|x| buffer.cell((x, buffer.area.height - 1)))
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(footer.contains("● sandbox:off  context:"), "{footer:?}");
    }

    #[test]
    fn footer_attaches_spinner_to_the_running_session() {
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = AppState::new(PermissionMode::Manual, false);
        state.session_id = Some("session-1".into());
        state.tab_strip.ensure_active("session-1", "main");
        state.status = SessionStatus::Thinking;
        state.tick = 0;

        terminal
            .draw(|frame| render_ui(frame, &mut state, &AppConfig::default()))
            .unwrap();

        let buffer = terminal.backend().buffer();
        let footer = (0..buffer.area.width)
            .filter_map(|x| buffer.cell((x, buffer.area.height - 1)))
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(footer.contains("[⠋ main]"), "{footer:?}");
    }

    #[test]
    fn build_empty_transcript() {
        let mut state = AppState::new(PermissionMode::Manual, false);
        let theme = Theme::default();
        let lines = build_transcript_lines(&mut state, &theme, 80);
        assert!(!lines.is_empty());
        let lines2 = build_transcript_lines(&mut state, &theme, 1);
        assert!(!lines2.is_empty());
    }

    #[test]
    fn completed_todo_panel_auto_hides_and_can_be_reopened() {
        let mut state = AppState::new(PermissionMode::Manual, false);
        state.todos = vec![
            TodoItem {
                id: "one".into(),
                content: "first".into(),
                status: "completed".into(),
            },
            TodoItem {
                id: "two".into(),
                content: "second".into(),
                status: "done".into(),
            },
            TodoItem {
                id: "three".into(),
                content: "third".into(),
                status: "cancelled".into(),
            },
        ];

        assert_eq!(todo_panel_height(&state, 80), 0);
        state.todos_expanded = true;
        assert!(todo_panel_height(&state, 80) > 0);

        state.todos_expanded = false;
        state.todos[0].status = "in_progress".into();
        assert!(todo_panel_height(&state, 80) > 0);

        state.todos = (0..6)
            .map(|index| TodoItem {
                id: index.to_string(),
                content: format!("item {index}"),
                status: "pending".into(),
            })
            .collect();
        state.todos_expanded = true;
        assert_eq!(todo_panel_height(&state, 80), 9);
    }

    #[test]
    fn long_system_errors_wrap_without_losing_content() {
        let mut state = AppState::new(PermissionMode::Manual, false);
        let reason =
            "HTTP 429 Too Many Requests: request limit reached; retry after the configured delay";
        state.messages.push(DisplayMessage {
            role: MessageRole::System,
            content: reason.into(),
            thinking: None,
            parts: Vec::new(),
            tool_calls: Vec::new(),
            delivery: crate::prompt_queue::DeliveryState::Sent,
            idempotency_key: None,
        });

        let lines = build_transcript_lines(&mut state, &Theme::default(), 24);
        let rendered = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(lines.len() > 3);
        assert!(rendered.contains("configured delay"));
    }

    #[test]
    fn btw_mode_uses_full_message_area_and_keeps_the_composer() {
        let backend = TestBackend::new(90, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = AppState::new(PermissionMode::Manual, false);
        // Pin a short cwd so the footer layout assertion isn't perturbed by the
        // real working directory (e.g. a long worktree path can squeeze the
        // `btw` label off the 90-col line).
        state.working_dir = std::path::PathBuf::from("/repo");
        state.mode = AppMode::Btw;
        state.btw.open = true;
        state.btw.turns.push(crate::panes::BtwTurnView {
            question: "side question".into(),
            answer: "# Side answer\n\nThis is **bold** and `code`.".into(),
            thinking: Some("reasoning about the side question".into()),
        });
        state.messages.push(DisplayMessage {
            role: MessageRole::Assistant,
            content: "main transcript must be hidden".into(),
            thinking: None,
            parts: Vec::new(),
            tool_calls: Vec::new(),
            delivery: crate::prompt_queue::DeliveryState::Sent,
            idempotency_key: None,
        });

        terminal
            .draw(|frame| render_ui(frame, &mut state, &AppConfig::default()))
            .unwrap();
        let rendered =
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .fold(String::new(), |mut output, cell| {
                    output.push_str(cell.symbol());
                    output
                });

        assert!(rendered.contains("✦ side question"), "{rendered:?}");
        assert!(rendered.contains("Side answer"), "{rendered:?}");
        assert!(
            rendered.contains("reasoning about the side question"),
            "{rendered:?}"
        );
        assert!(!rendered.contains("# Side answer"), "{rendered:?}");
        assert!(!rendered.contains("**bold**"), "{rendered:?}");
        assert!(rendered.contains("btw >"), "{rendered:?}");
        assert!(rendered.contains("btw:1 (ctrl+g)"), "{rendered:?}");
        assert!(state.workspace_sessions.entries.is_empty());
        assert!(!rendered.contains("main transcript must be hidden"));
    }

    #[test]
    fn transcript_records_click_targets_on_tool_toggle_hints() {
        let mut state = AppState::new(PermissionMode::Manual, false);
        state.messages.push(DisplayMessage {
            role: MessageRole::Assistant,
            content: String::new(),
            thinking: None,
            parts: vec![
                DisplayPart::Tool(DisplayToolCall {
                    id: "live".into(),
                    name: "Bash".into(),
                    input_summary: "printf test".into(),
                    output: Some("one\ntwo".into()),
                    is_error: false,
                    collapsed: true,
                    user_overridden: false,
                    started_at: None,
                    stopping: false,
                    queued_behind: None,
                }),
                DisplayPart::ToolHistory(ToolHistorySummary {
                    tool_count: 1,
                    duration_ms: 10,
                    tokens: 20,
                    expanded: false,
                    user_overridden: false,
                    tools: Vec::new(),
                }),
            ],
            tool_calls: Vec::new(),
            delivery: crate::prompt_queue::DeliveryState::Sent,
            idempotency_key: None,
        });

        let theme = Theme::default();
        let lines = build_transcript_lines(&mut state, &theme, 80);

        assert_eq!(state.tool_expand_hits.len(), 2);
        for hit in &state.tool_expand_hits {
            let text = lines[hit.line].to_string();
            assert!(text.contains("ctrl+o to"), "unexpected hit line: {text}");
        }
    }

    #[test]
    fn long_command_chip_is_clamped_and_expandable() {
        let long_cmd = format!("echo {}", "z".repeat(200));
        let theme = Theme::default();
        let width = 60u16;

        let mut state = AppState::new(PermissionMode::Manual, false);
        state.messages.push(DisplayMessage {
            role: MessageRole::Assistant,
            content: String::new(),
            thinking: None,
            parts: vec![DisplayPart::Tool(DisplayToolCall {
                id: "live".into(),
                name: "Bash".into(),
                input_summary: long_cmd.clone(),
                output: None,
                is_error: false,
                collapsed: true,
                user_overridden: false,
                started_at: None,
                stopping: false,
                queued_behind: None,
            })],
            tool_calls: Vec::new(),
            delivery: crate::prompt_queue::DeliveryState::Sent,
            idempotency_key: None,
        });

        let lines = build_transcript_lines(&mut state, &theme, width);
        let text = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");

        // The chip line must not overflow the viewport.
        for l in &lines {
            assert!(
                UnicodeWidthStr::width(l.to_string().as_str()) <= width as usize,
                "line overflows: {l:?}"
            );
        }
        // Collapsed: full command hidden, ctrl+o hint present and clickable.
        assert!(!text.contains(&long_cmd), "{text:?}");
        assert!(text.contains("ctrl+o to expand"), "{text:?}");
        assert_eq!(state.tool_expand_hits.len(), 1);
        let hit = state.tool_expand_hits[0];
        assert!(lines[hit.line].to_string().contains("ctrl+o to expand"));

        // Expanded (what ctrl+o / the click does): full command wrapped in,
        // still clamped to the viewport, collapse hint present.
        if let Some(DisplayPart::Tool(tc)) = state.messages[0].parts.first_mut() {
            tc.collapsed = false;
            tc.user_overridden = true;
        }
        let lines = build_transcript_lines(&mut state, &theme, width);
        // Wrapped continuation lines carry a 2-col indent; strip it so the
        // wrapped chunks reconstruct the original command string.
        let joined = lines
            .iter()
            .map(|l| l.to_string().trim_start().to_string())
            .collect::<Vec<_>>()
            .join("");
        assert!(
            joined.contains(&long_cmd),
            "expanded view must show the full command: {joined:?}"
        );
        let text = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("ctrl+o to collapse"), "{text:?}");
        for l in &lines {
            assert!(
                UnicodeWidthStr::width(l.to_string().as_str()) <= width as usize,
                "line overflows: {l:?}"
            );
        }
    }

    #[test]
    fn popup_rect_stays_inside_offset_viewports() {
        for width in [0, 1, 4, 12, 24, 40, 100] {
            for height in [0, 1, 6, 20, 80] {
                let viewport = Rect::new(7, 11, width, height);
                let popup = popup_rect(viewport, 72, 30);
                assert!(popup.x >= viewport.x);
                assert!(popup.y >= viewport.y);
                assert!(popup.right() <= viewport.right());
                assert!(popup.bottom() <= viewport.bottom());
            }
        }
    }

    #[test]
    fn expanded_live_tool_renders_full_output_and_a_clickable_collapse_hint() {
        let mut state = AppState::new(PermissionMode::Manual, false);
        let output = (1..=20)
            .map(|line| format!("output line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        state.messages.push(DisplayMessage {
            role: MessageRole::Assistant,
            content: String::new(),
            thinking: None,
            parts: vec![DisplayPart::Tool(DisplayToolCall {
                id: "live".into(),
                name: "Bash".into(),
                input_summary: "printf test".into(),
                output: Some(output),
                is_error: false,
                collapsed: false,
                user_overridden: true,
                started_at: None,
                stopping: false,
                queued_behind: None,
            })],
            tool_calls: Vec::new(),
            delivery: crate::prompt_queue::DeliveryState::Sent,
            idempotency_key: None,
        });

        let theme = Theme::default();
        let lines = build_transcript_lines(&mut state, &theme, 80);
        let text = lines
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("output line 20"));
        assert!(!text.contains("more lines, ctrl+o to expand"));
        assert_eq!(state.tool_expand_hits.len(), 1);
        assert!(lines[state.tool_expand_hits[0].line]
            .to_string()
            .contains("ctrl+o to collapse"));
    }

    #[test]
    fn narrow_ui_handles_cjk_emoji_input_and_keeps_mobile_footer() {
        for width in [1, 2, 4, 8, 16, 24, 32, 47] {
            let backend = TestBackend::new(width, 80);
            let mut terminal = Terminal::new(backend).unwrap();
            let mut state = AppState::new(PermissionMode::Manual, false);
            state
                .input
                .set_text("你好👨‍👩‍👧‍👦，这是一段会自动换行的手机输入内容".into());
            state.approval_pending = Some(PendingApproval {
                approval_id: "approval-mobile".into(),
                tool_name: "Bash".into(),
                action: "Run a command that needs confirmation on a phone terminal".into(),
                detail: "command: cargo test --workspace --all-targets".into(),
                selected: 0,
                choices: PendingApproval::default_tool_choices(),
                is_plan_review: false,
                resumed_plan_review: false,
                feedback_mode: false,
                feedback: String::new(),
                hidden: false,
            });

            terminal
                .draw(|frame| render_ui(frame, &mut state, &AppConfig::default()))
                .unwrap();
            assert_eq!(state.transcript_area.width, width);
            assert_eq!(state.footer_area.height, 3);
            assert!(state.footer_area.bottom() <= 80);
        }
    }

    #[test]
    fn narrow_approval_and_question_popups_keep_both_borders_visible() {
        let backend = TestBackend::new(24, 60);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = AppState::new(PermissionMode::Manual, false);
        state.approval_pending = Some(PendingApproval {
            approval_id: "approval-mobile".into(),
            tool_name: "Bash".into(),
            action: "Run a command requiring confirmation".into(),
            detail: "A long command detail that wraps instead of leaving the viewport".into(),
            selected: 0,
            choices: PendingApproval::default_tool_choices(),
            is_plan_review: false,
            resumed_plan_review: false,
            feedback_mode: false,
            feedback: String::new(),
            hidden: false,
        });
        terminal
            .draw(|frame| render_ui(frame, &mut state, &AppConfig::default()))
            .unwrap();
        assert!(has_complete_inset_box(terminal.backend().buffer()));
        assert!(buffer_rows(terminal.backend().buffer())
            .join("\n")
            .contains("allow once"));

        state.approval_pending = None;
        state.question_pending = Some(PendingQuestion {
            question_id: "question-mobile".into(),
            text: "Choose an option from a narrow phone terminal".into(),
            options: vec![
                ("first".into(), "first option".into()),
                ("second".into(), "second option".into()),
            ],
            allow_free_text: true,
            allow_multiple: false,
            selected: 0,
            toggled: vec![false, false],
            free_text: String::new(),
        });
        terminal
            .draw(|frame| render_ui(frame, &mut state, &AppConfig::default()))
            .unwrap();
        assert!(has_complete_inset_box(terminal.backend().buffer()));
        let rows = buffer_rows(terminal.backend().buffer());
        assert!(
            rows.iter().any(|row| row.contains("first"))
                && rows.iter().any(|row| row.contains("option")),
            "{rows:#?}"
        );
    }

    #[test]
    fn plan_focus_renders_full_document() {
        let mut state = AppState::new(PermissionMode::Manual, true);
        let long: String = (1..=40).map(|i| format!("- step {i}\n")).collect();
        state.apply_plan_document("/tmp/.kkagent/plans/s.md".into(), long.clone());
        assert!(state.plan_focus_active());
        let theme = Theme::default();
        let lines = build_transcript_lines(&mut state, &theme, 60);
        let joined: String = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("step 1"));
        assert!(joined.contains("step 40"));
        assert!(!joined.contains("more lines"));
        assert!(joined.contains("scroll = full plan only"));
        // No earlier transcript noise
        assert!(!joined.contains("kkagent"));
    }

    #[test]
    fn transcript_fingerprint_changes_with_visible_content() {
        let mut state = AppState::new(PermissionMode::Manual, false);
        state.messages.push(DisplayMessage {
            role: MessageRole::User,
            content: "before".into(),
            thinking: None,
            parts: Vec::new(),
            tool_calls: Vec::new(),
            delivery: crate::prompt_queue::DeliveryState::Sent,
            idempotency_key: None,
        });
        let before = transcript_layout_fingerprint(&state, 80);
        assert_eq!(before, transcript_layout_fingerprint(&state, 80));

        state.messages[0].content = "after".into();
        assert_ne!(before, transcript_layout_fingerprint(&state, 80));
        assert_ne!(before, transcript_layout_fingerprint(&state, 79));
    }

    #[test]
    fn spinner_updates_do_not_invalidate_full_transcript_layout() {
        let mut state = AppState::new(PermissionMode::Manual, false);
        state.messages.push(DisplayMessage {
            role: MessageRole::User,
            content: "hello".into(),
            thinking: None,
            parts: Vec::new(),
            tool_calls: Vec::new(),
            delivery: crate::prompt_queue::DeliveryState::Sent,
            idempotency_key: None,
        });
        state.status = SessionStatus::Thinking;
        let before = transcript_layout_fingerprint(&state, 80);
        let status_before = build_transcript_status_lines(&state, &Theme::default(), 80, 24);

        state.tick = crate::app::SPINNER_TICKS_PER_FRAME;
        assert_eq!(before, transcript_layout_fingerprint(&state, 80));
        assert_ne!(
            status_before[0].to_string(),
            build_transcript_status_lines(&state, &Theme::default(), 80, 24)[0].to_string()
        );
    }

    #[test]
    fn live_thinking_is_bounded_and_never_pads_empty_rows() {
        let mut state = AppState::new(PermissionMode::Manual, false);
        state.messages.push(DisplayMessage {
            role: MessageRole::User,
            content: "hello".into(),
            thinking: None,
            parts: Vec::new(),
            tool_calls: Vec::new(),
            delivery: crate::prompt_queue::DeliveryState::Sent,
            idempotency_key: None,
        });
        state.status = SessionStatus::Thinking;
        let max_height = 1 + live_thinking_body_height(24);

        assert_eq!(
            build_transcript_status_lines(&state, &Theme::default(), 24, 24).len(),
            1
        );
        state.thinking_text = "short thought".into();
        assert_eq!(
            build_transcript_status_lines(&state, &Theme::default(), 24, 24).len(),
            2
        );

        state.thinking_text = (0..20)
            .map(|index| format!("thought {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let lines = build_transcript_status_lines(&state, &Theme::default(), 24, 24);
        assert_eq!(lines.len(), max_height);
        assert!(lines.iter().all(|line| !line.to_string().is_empty()));
        assert!(lines.last().unwrap().to_string().contains("thought 19"));
        assert!(!lines
            .iter()
            .any(|line| line.to_string().contains("thought 0")));

        for status in [SessionStatus::ToolExecuting, SessionStatus::WaitingApproval] {
            state.status = status;
            assert_eq!(
                build_transcript_status_lines(&state, &Theme::default(), 24, 24).len(),
                1
            );
        }
        for status in [
            SessionStatus::WaitingQuestion,
            SessionStatus::Compacting,
            SessionStatus::Cancelling,
            SessionStatus::Idle,
        ] {
            state.status = status;
            assert!(build_transcript_status_lines(&state, &Theme::default(), 24, 24).is_empty());
        }
    }

    #[test]
    fn flushed_thinking_folds_back_into_transcript_while_answer_streams() {
        let mut state = AppState::new(PermissionMode::Manual, false);
        state.messages.push(DisplayMessage {
            role: MessageRole::User,
            content: "hello".into(),
            thinking: None,
            parts: Vec::new(),
            tool_calls: Vec::new(),
            delivery: crate::prompt_queue::DeliveryState::Sent,
            idempotency_key: None,
        });
        state.messages.push(DisplayMessage {
            role: MessageRole::Assistant,
            content: "answer".into(),
            thinking: None,
            parts: vec![DisplayPart::Text("answer".into())],
            tool_calls: Vec::new(),
            delivery: crate::prompt_queue::DeliveryState::Sent,
            idempotency_key: None,
        });
        state.active_assistant_message = Some(1);
        state.status = SessionStatus::Thinking;
        state.thinking_text = "moved thought".into();

        // Un-flushed reasoning streams only through the live status tail.
        let streaming_text =
            build_transcript_lines_range(&mut state, &Theme::default(), 80, Some(1..2), false)
                .iter()
                .map(Line::to_string)
                .collect::<Vec<_>>()
                .join("\n");
        assert!(streaming_text.contains("answer"));
        assert!(!streaming_text.contains("moved thought"));
        assert!(
            build_transcript_status_lines(&state, &Theme::default(), 80, 24)
                .iter()
                .any(|line| line.to_string().contains("moved thought"))
        );

        // The first body delta flushes the buffered reasoning onto the
        // message: it folds back into the transcript right away even though
        // status remains Thinking while the answer itself keeps streaming.
        state.messages[1].thinking = Some("moved thought".into());
        state.thinking_text.clear();
        let folded_text =
            build_transcript_lines_range(&mut state, &Theme::default(), 80, Some(1..2), false)
                .iter()
                .map(Line::to_string)
                .collect::<Vec<_>>()
                .join("\n");
        assert!(folded_text.contains("answer"));
        assert!(folded_text.contains("moved thought"));
        let tail_lines = build_transcript_status_lines(&state, &Theme::default(), 80, 24);
        assert!(!tail_lines
            .iter()
            .any(|line| line.to_string().contains("moved thought")));
        assert!(tail_lines.iter().any(|line| line.to_string().contains("●")));

        // Fresh interleaved reasoning deltas reopen the live tail: the new
        // batch streams at the bottom while the folded block is hidden from
        // the inline rendering again.
        state.thinking_text = "second thought".into();
        let interleaved_tail = build_transcript_status_lines(&state, &Theme::default(), 80, 24);
        assert!(interleaved_tail
            .iter()
            .any(|line| line.to_string().contains("second thought")));
        let interleaved_text =
            build_transcript_lines_range(&mut state, &Theme::default(), 80, Some(1..2), false)
                .iter()
                .map(Line::to_string)
                .collect::<Vec<_>>()
                .join("\n");
        assert!(!interleaved_text.contains("moved thought"));
        assert!(!interleaved_text.contains("second thought"));

        // The next body delta merges the buffered batch into the message
        // (mirrors the MessageDelta handler) and the block folds back again.
        state.messages[1].thinking = Some("moved thought\nsecond thought".into());
        state.thinking_text.clear();
        let merged_text =
            build_transcript_lines_range(&mut state, &Theme::default(), 80, Some(1..2), false)
                .iter()
                .map(Line::to_string)
                .collect::<Vec<_>>()
                .join("\n");
        assert!(merged_text.contains("moved thought"));
        assert!(merged_text.contains("second thought"));
        let merged_tail = build_transcript_status_lines(&state, &Theme::default(), 80, 24);
        assert!(!merged_tail
            .iter()
            .any(|line| line.to_string().contains("second thought")));

        // Once the turn ends the tail disappears entirely.
        state.thinking_text.clear();
        state.status = SessionStatus::Idle;
        let completed_text =
            build_transcript_lines_range(&mut state, &Theme::default(), 80, Some(1..2), false)
                .iter()
                .map(Line::to_string)
                .collect::<Vec<_>>()
                .join("\n");
        assert!(completed_text.contains("moved thought"));
        assert!(build_transcript_status_lines(&state, &Theme::default(), 80, 24).is_empty());
    }

    #[test]
    fn tool_status_keeps_active_thinking_visible_without_disabling_tail_cache() {
        let mut state = AppState::new(PermissionMode::Manual, false);
        state.messages.push(DisplayMessage {
            role: MessageRole::User,
            content: "hello".into(),
            thinking: None,
            parts: Vec::new(),
            tool_calls: Vec::new(),
            delivery: crate::prompt_queue::DeliveryState::Sent,
            idempotency_key: None,
        });
        state.messages.push(DisplayMessage {
            role: MessageRole::Assistant,
            content: "answer".into(),
            thinking: Some("visible while tool runs".into()),
            parts: vec![DisplayPart::Text("answer".into())],
            tool_calls: Vec::new(),
            delivery: crate::prompt_queue::DeliveryState::Sent,
            idempotency_key: None,
        });
        state.active_assistant_message = Some(1);
        state.status = SessionStatus::ToolExecuting;

        assert_eq!(streaming_transcript_tail_index(&state), Some(1));
        assert_eq!(live_thinking_message_index(&state), None);
        let lines =
            build_transcript_lines_range(&mut state, &Theme::default(), 80, Some(1..2), false);
        assert!(lines
            .iter()
            .any(|line| line.to_string().contains("visible while tool runs")));
    }

    #[test]
    fn non_tail_active_thinking_invalidates_layout_when_turn_ends() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = AppState::new(PermissionMode::Manual, false);
        state.messages = vec![
            DisplayMessage {
                role: MessageRole::User,
                content: "prompt".into(),
                thinking: None,
                parts: Vec::new(),
                tool_calls: Vec::new(),
                delivery: crate::prompt_queue::DeliveryState::Sent,
                idempotency_key: None,
            },
            DisplayMessage {
                role: MessageRole::Assistant,
                content: "answer".into(),
                thinking: Some("thinking restored after completion".into()),
                parts: vec![DisplayPart::Text("answer".into())],
                tool_calls: Vec::new(),
                delivery: crate::prompt_queue::DeliveryState::Sent,
                idempotency_key: None,
            },
            DisplayMessage {
                role: MessageRole::User,
                content: "steered follow-up".into(),
                thinking: None,
                parts: Vec::new(),
                tool_calls: Vec::new(),
                delivery: crate::prompt_queue::DeliveryState::Sent,
                idempotency_key: None,
            },
        ];
        state.active_assistant_message = Some(1);
        state.status = SessionStatus::Thinking;
        // Reasoning still buffered (not yet flushed) keeps the flushed block
        // out of the cached transcript until the turn ends.
        state.thinking_text = "still streaming".into();

        let active_fingerprint = transcript_layout_fingerprint(&state, 80);
        terminal
            .draw(|frame| render_ui(frame, &mut state, &AppConfig::default()))
            .unwrap();
        assert!(!state
            .transcript_layout_cache
            .lines()
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            .contains("thinking restored after completion"));

        state.active_assistant_message = None;
        state.status = SessionStatus::Idle;
        assert_ne!(
            active_fingerprint,
            transcript_layout_fingerprint(&state, 80)
        );
        terminal
            .draw(|frame| render_ui(frame, &mut state, &AppConfig::default()))
            .unwrap();
        assert!(state
            .transcript_layout_cache
            .lines()
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            .contains("thinking restored after completion"));
    }

    #[test]
    fn streaming_last_message_reuses_the_completed_transcript_prefix() {
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = AppState::new(PermissionMode::Manual, false);
        state.messages.push(DisplayMessage {
            role: MessageRole::User,
            content: "prompt".into(),
            thinking: None,
            parts: Vec::new(),
            tool_calls: Vec::new(),
            delivery: crate::prompt_queue::DeliveryState::Sent,
            idempotency_key: None,
        });
        state.messages.push(DisplayMessage {
            role: MessageRole::Assistant,
            content: "streaming".into(),
            thinking: None,
            parts: vec![DisplayPart::Text("streaming".into())],
            tool_calls: Vec::new(),
            delivery: crate::prompt_queue::DeliveryState::Sent,
            idempotency_key: None,
        });
        state.active_assistant_message = Some(1);
        state.status = SessionStatus::Thinking;

        let fingerprint = transcript_layout_fingerprint(&state, 80);
        terminal
            .draw(|frame| render_ui(frame, &mut state, &AppConfig::default()))
            .unwrap();
        let cached_prefix: Vec<String> = state
            .transcript_layout_cache
            .lines()
            .iter()
            .map(Line::to_string)
            .collect();
        let static_line_count = cached_prefix.len();
        assert_eq!(state.message_line_starts.len(), 2);
        assert_eq!(state.message_line_starts[1] as usize, static_line_count);

        state.messages[1].append_assistant_text(" continued");
        assert_eq!(fingerprint, transcript_layout_fingerprint(&state, 80));
        terminal
            .draw(|frame| render_ui(frame, &mut state, &AppConfig::default()))
            .unwrap();
        assert_eq!(
            cached_prefix,
            state
                .transcript_layout_cache
                .lines()
                .iter()
                .map(Line::to_string)
                .collect::<Vec<_>>()
        );
        assert!(buffer_rows(terminal.backend().buffer())
            .join("\n")
            .contains("streaming continued"));

        state.active_assistant_message = None;
        assert_ne!(fingerprint, transcript_layout_fingerprint(&state, 80));
    }

    #[test]
    fn selection_is_clipped_to_visible_transcript_lines() {
        use crate::selection::{CellPos, TextSelection};

        let selection = TextSelection {
            anchor: CellPos { line: 2, col: 4 },
            focus: CellPos { line: 8, col: 7 },
        };
        let visible = selection_in_viewport(selection, 4, 7).unwrap();
        assert_eq!(visible.anchor, CellPos { line: 0, col: 0 });
        assert_eq!(
            visible.focus,
            CellPos {
                line: 2,
                col: u16::MAX
            }
        );
        assert!(selection_in_viewport(selection, 9, 12).is_none());
    }

    #[test]
    fn leaving_plan_mode_unlocks_transcript() {
        let mut state = AppState::new(PermissionMode::Manual, true);
        state.apply_plan_document("plan.md".into(), "# Hello\n\nbody".into());
        assert!(state.plan_focus_active());
        state.on_plan_mode_changed(false);
        assert!(!state.plan_focus_active());
        assert!(state.follow_bottom);
    }

    #[test]
    fn plan_block_collapses_after_leaving_plan_mode() {
        let mut state = AppState::new(PermissionMode::Manual, false);
        state.apply_plan_document("plan.md".into(), "# Hello\n\nbody".into());
        // Fresh plan blocks start folded in the transcript.
        assert!(state.plan_transcript_collapsed);
        assert!(!state.plan_transcript_overridden);

        // Click-to-expand detaches from the global Ctrl-O mode.
        state.plan_transcript_collapsed = false;
        state.plan_transcript_overridden = true;
        state.tool_output_expanded = false;
        state.apply_tool_output_mode();
        assert!(!state.plan_transcript_collapsed, "override survives Ctrl-O");

        // Leaving plan mode re-folds and clears the override.
        state.on_plan_mode_changed(false);
        assert!(state.plan_transcript_collapsed);
        assert!(!state.plan_transcript_overridden);

        // Ctrl-O now flips the plan block globally.
        state.tool_output_expanded = true;
        state.apply_tool_output_mode();
        assert!(!state.plan_transcript_collapsed);
        state.tool_output_expanded = false;
        state.apply_tool_output_mode();
        assert!(state.plan_transcript_collapsed);
    }

    #[test]
    fn plan_transcript_renders_collapsed_summary_and_expanded_box() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = AppState::new(PermissionMode::Manual, false);
        state.apply_plan_document("plan.md".into(), "# Hello\n\nbody".into());

        terminal
            .draw(|frame| render_ui(frame, &mut state, &AppConfig::default()))
            .unwrap();
        let collapsed_text = buffer_text(&terminal);
        assert!(collapsed_text.contains("▸ plan: plan.md (3 lines)"));
        assert!(collapsed_text.contains("click to expand"));
        assert!(!collapsed_text.contains("# Hello"));

        // Expand via a registered hit: the summary line is hit-registered.
        assert!(state
            .tool_expand_hits
            .iter()
            .any(|hit| matches!(hit.target, ToolExpandTarget::Plan { .. })));
        state.plan_transcript_collapsed = false;
        terminal
            .draw(|frame| render_ui(frame, &mut state, &AppConfig::default()))
            .unwrap();
        let expanded_text = buffer_text(&terminal);
        assert!(expanded_text.contains("plan: plan.md"));
        assert!(expanded_text.contains("Hello"));
        assert!(expanded_text.contains("click to collapse"));
    }

    #[test]
    fn plan_mode_focus_overlay_shows_full_document() {
        let mut state = AppState::new(PermissionMode::Manual, true);
        state.apply_plan_document("plan.md".into(), "# Hello\n\nbody".into());
        // Focus overlay path is independent of transcript folding.
        assert!(state.plan_focus_active());
        assert!(state.plan_transcript_collapsed);
    }

    #[test]
    fn wrap_wide() {
        let s = "你好世界abcdef";
        let parts = wrap_str(s, 4);
        assert!(!parts.is_empty());
        assert_eq!(truncate_display_width(s, 5), "你好…");
        assert_eq!(
            UnicodeWidthStr::width(truncate_display_width(s, 5).as_str()),
            5
        );
    }

    #[test]
    fn input_soft_wrap_cursor_and_rows() {
        let text = "abcdefghijklmnopqrstuvwxyz";
        // content width 10 → three visual rows
        assert_eq!(input_visual_row_count(text, 10), 3);
        let (x, y) = cursor_position(text, text.len(), 10, 2);
        // 26 cols → wrap_row 2, x_off 6 + prefix 2
        assert_eq!(y, 2);
        assert_eq!(x, 2 + 6);

        let multi = "short\nabcdefghijklmnopqrstuvwxyz";
        let rows = input_visual_row_count(multi, 10);
        assert_eq!(rows, 1 + 3);
        let (_, y) = cursor_position(multi, multi.len(), 10, 2);
        assert_eq!(y, 1 + 2);
    }

    #[test]
    fn input_wrap_keeps_wide_graphemes_atomic_at_one_column() {
        let family = "👨‍👩‍👧‍👦";
        assert_eq!(soft_wrap_line(family, 1), vec![family.to_string()]);
        assert_eq!(input_visual_row_count("中文", 1), 2);
        assert_eq!(cursor_position("中文", "中文".len(), 1, 2), (2, 2));
        assert_eq!(cursor_position(family, family.len(), 1, 2), (2, 1));
        assert_eq!(
            truncate_display_width(&format!("{family}ab"), 3),
            format!("{family}…")
        );
    }

    #[test]
    fn trailing_newline_keeps_empty_visual_row() {
        let text = "hi\n";
        assert_eq!(input_logical_lines(text), vec!["hi", ""]);
        assert_eq!(input_visual_row_count(text, 40), 2);
        let (x, y) = cursor_position(text, text.len(), 40, 2);
        assert_eq!((x, y), (2, 1));
    }

    #[test]
    fn input_layout_covers_every_byte_exactly_once() {
        let text = "hello\n你好 world\n";
        let width = 4;
        let layout = input_layout(text, width);
        let total_rows = input_visual_row_count(text, width) as usize;
        assert_eq!(layout.len(), total_rows);
        // Byte ranges tile the text (every non-'\n' byte exactly once) and
        // row numbers are dense.
        let mut covered = String::new();
        for r in &layout {
            covered.push_str(&text[r.start..r.end]);
        }
        assert_eq!(covered, text.replace('\n', ""));
        for (i, r) in layout.iter().enumerate() {
            assert_eq!(r.row, i as u16);
        }
    }

    #[test]
    fn click_maps_to_byte_at_visual_cell() {
        let text = "hello\n你好 world";
        let width = 20;
        // Row 0 is "hello": clicking at col 0 → 'h', col 2 → 'l', col 99 → end of row.
        assert_eq!(byte_at_visual_cell(text, width, 0, 0), Some(0));
        assert_eq!(byte_at_visual_cell(text, width, 0, 2), Some(2));
        assert_eq!(byte_at_visual_cell(text, width, 0, 99), Some(5));
        // Row 1 starts after '\n' (byte 6); clicking 你 (col 0..2) → byte 6.
        assert_eq!(byte_at_visual_cell(text, width, 1, 0), Some(6));
        assert_eq!(byte_at_visual_cell(text, width, 1, 1), Some(9));
        assert_eq!(byte_at_visual_cell(text, width, 1, 2), Some(9));
        // Clicks beyond the last row snap nowhere useful — but the last row
        // still resolves to its end.
        assert_eq!(byte_at_visual_cell(text, width, 2, 0), None);
        let wide_end = text.len();
        assert_eq!(byte_at_visual_cell(text, width, 1, 99), Some(wide_end));
    }

    #[test]
    fn click_mapping_roundtrips_with_wrap() {
        let text = "abcdefghijklmnopqrstuvwxyz";
        let width = 10; // 3 visual rows
                        // Every byte offset lands back on itself when clicking its own cell.
        for (expected_byte, ch) in text.char_indices() {
            let layout = input_layout(text, width);
            let row = layout
                .iter()
                .find(|r| r.start <= expected_byte && expected_byte < r.end)
                .unwrap();
            let col: usize = text[row.start..expected_byte]
                .chars()
                .map(|c| c.width().unwrap_or(0))
                .sum();
            assert_eq!(
                byte_at_visual_cell(text, width, row.row, col as u16),
                Some(expected_byte),
                "char {ch}"
            );
        }
    }

    #[test]
    fn highlight_byte_span_splits_prefix_and_text() {
        use ratatui::style::{Color, Style};
        // Row layout mirrors render_input: prefix span + text span.
        let mut line = ratatui::text::Line::from(vec![
            ratatui::text::Span::styled("> ", Style::default().fg(Color::Red)),
            ratatui::text::Span::styled("hello world", Style::default()),
        ]);
        let sel = Style::default().bg(Color::Blue);
        // Bytes are measured on the text content (prefix excluded by callers):
        // select "lo wo" (bytes 3..8 of "hello world").
        highlight_byte_span(&mut line, 3, 8, sel);
        let texts: Vec<&str> = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(texts, vec!["> ", "hel", "lo wo", "rld"]);
        assert_eq!(line.spans[2].style.bg, Some(Color::Blue));
        assert_eq!(line.spans[0].style.bg, None);
        assert_eq!(line.spans[1].style.bg, None);
        assert_eq!(line.spans[3].style.bg, None);
        // Full-range selection keeps the prefix span intact.
        let mut line = ratatui::text::Line::from(vec![
            ratatui::text::Span::raw("  "),
            ratatui::text::Span::raw("abc"),
        ]);
        highlight_byte_span(&mut line, 0, 3, sel);
        assert_eq!(line.spans.len(), 2);
        assert_eq!(line.spans[1].style.bg, Some(Color::Blue));
    }
}
