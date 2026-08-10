use kkagent_config::AppConfig;
use kkagent_protocol::{PermissionMode, SessionStatus};
use ratatui::{
    layout::{Constraint, Direction, Layout, Margin, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{
    AppMode, AppState, DisplayPart, ListPickerState, MessageRole, PendingApproval, PendingQuestion,
    TodoItem, ToolHistorySummary,
};
use crate::chrome;
use crate::git_badge;
use crate::i18n::{self, Locale};
use crate::panes::{self, BtwPane};
use crate::theme::Theme;

const TIPS: &[&str] = &[
    "/compact compresses context when it gets long",
    "@ opens file picker — tab to insert a path",
    "ctrl+f searches the transcript",
    "ctrl+o expands turn tool history",
    "tab / ←→ cycle fork sessions when input is empty",
    "ctrl+g toggles /btw side Q&A",
    "shift-tab toggles plan mode (scroll locks to plan)",
    "! enters shell mode",
    "/yolo auto-approves tools",
    "large pastes collapse to [Pasted text #n]",
    "scroll to review earlier messages",
];

pub fn render_ui(f: &mut Frame, state: &mut AppState, config: &AppConfig) {
    let theme = Theme::default();
    let size = f.area();

    // Reserve space for slash / list picker popup above the input box
    let slash_height = state
        .slash_menu
        .as_ref()
        .map(menu_height)
        .or_else(|| state.file_menu.as_ref().map(file_menu_height))
        .or_else(|| state.list_picker.as_ref().map(|p| picker_height(p, state)))
        .unwrap_or(0);

    // Sticky todo sits above the input (highest visual priority).
    let todo_height = todo_panel_height(state);

    // kimi 布局：tabs(可选) | 消息区 | todo(可选) | 带边框输入框 | footer 两行
    let tab_height = if state.tab_strip.tabs.len() > 1 { 1 } else { 0 };
    let input_inner = input_inner_height(state, size.width);
    let input_box = input_inner + 2; // borders
    let bottom_stack = todo_height + input_box + slash_height;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(tab_height),
            Constraint::Min(1),
            Constraint::Length(bottom_stack),
            Constraint::Length(2),
        ])
        .split(size);

    let msg_area = chunks[1];
    let bottom = chunks[2];
    let bottom_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(todo_height),
            Constraint::Length(slash_height),
            Constraint::Length(input_box),
        ])
        .split(bottom);

    let todo_area = bottom_chunks[0];
    let slash_area = bottom_chunks[1];
    let input_area = bottom_chunks[2];

    if tab_height > 0 {
        chrome::draw_tab_strip(f, chunks[0], &state.tab_strip, &theme);
    }
    // Keep status_bar in sync for chrome consumers / future status line.
    state.status_bar.permission = state.permission_mode;
    state.status_bar.plan_mode = state.plan_mode;
    state.status_bar.status = state.status;
    state.status_bar.tokens = state.approx_tokens;
    state.status_bar.model = state.model_alias.clone();
    state.status_bar.session_id = state.session_id.clone();

    render_messages(f, msg_area, state, &theme);
    render_scroll_hint(f, msg_area, state, &theme);
    if todo_height > 0 {
        render_todo_panel(f, todo_area, state, &theme);
    }
    render_input(f, input_area, state, &theme);
    render_footer(f, chunks[3], state, config, &theme);

    if state.btw.open {
        let pane_w = (size.width / 3).clamp(28, 56);
        let pane_h = state
            .btw
            .line_budget()
            .saturating_add(2)
            .clamp(6, size.height.saturating_sub(4));
        let area = Rect {
            x: size.width.saturating_sub(pane_w + 1),
            y: tab_height.saturating_add(1),
            width: pane_w,
            height: pane_h,
        };
        panes::render_btw(
            f,
            area,
            &BtwPane {
                state: state.btw.clone(),
            },
            &theme,
        );
    }

    if let Some(ref mut approval) = state.approval_pending {
        render_approval_panel(f, size, approval, &theme);
    }

    if let Some(ref mut question) = state.question_pending {
        render_question_panel(f, size, question, &theme);
    }

    if state.slash_menu.is_some() {
        render_slash_menu(f, slash_area, state, &theme);
    } else if state.file_menu.is_some() {
        render_file_menu(f, slash_area, state, &theme);
    } else if state.list_picker.is_some() {
        render_list_picker(f, slash_area, state, &theme);
    }

    if state.tasks_panel.is_some() {
        render_tasks_panel(f, size, state, &theme);
    }

    if state.search.active {
        render_search_overlay(f, size, state, &theme);
    }
}

fn menu_height(menu_state: &crate::app::SlashMenuState) -> u16 {
    let max_visible = 8u16;
    let rows = if menu_state.items.is_empty() {
        1
    } else {
        (menu_state.items.len() as u16).min(max_visible)
    };
    rows + 2 // borders
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

fn picker_height(picker: &ListPickerState, state: &AppState) -> u16 {
    let max_visible = 10u16;
    let rows = if picker.items.is_empty() {
        1
    } else {
        (picker.items.len() as u16).min(max_visible)
    };
    let h = rows + 2;
    // Delete confirm replaces the list with a small choice panel.
    if picker.kind == crate::app::ListPickerKind::Session && state.session_delete_confirm.is_some()
    {
        return 7;
    }
    h.min(28)
}

fn input_prefix_str(state: &AppState) -> &'static str {
    match state.mode {
        AppMode::Shell => "! ",
        AppMode::Plan => "plan > ",
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
    rows.max(cursor_y.saturating_add(1)).clamp(1, 8)
}

fn render_messages(f: &mut Frame, area: Rect, state: &mut AppState, theme: &Theme) {
    let width = area.width.max(1);
    let lines = build_transcript_lines(state, theme, width);
    let content_height = lines.len() as u16;
    let visible_height = area.height.max(1);

    state.content_lines = content_height;
    state.viewport_height = visible_height;

    let max_scroll_up = content_height.saturating_sub(visible_height);
    if state.plan_scroll_to_top && state.plan_focus_active() {
        state.scroll_up = max_scroll_up;
        state.follow_bottom = max_scroll_up == 0;
        state.plan_scroll_to_top = false;
    } else if state.follow_bottom {
        state.scroll_up = 0;
    } else if state.scroll_up > max_scroll_up {
        state.scroll_up = max_scroll_up;
    }

    // scroll_up = 离底部的行数；0 表示贴底跟随
    let scroll_from_top = max_scroll_up.saturating_sub(state.scroll_up);

    // Lines are already width-wrapped — do NOT wrap again (that desyncs scroll).
    let paragraph = Paragraph::new(Text::from(lines)).scroll((scroll_from_top, 0));
    f.render_widget(paragraph, area);
}

fn build_transcript_lines(state: &mut AppState, theme: &Theme, width: u16) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();
    state.message_line_starts.clear();

    let browsing_sessions = state
        .list_picker
        .as_ref()
        .map(|p| p.kind == crate::app::ListPickerKind::Session)
        .unwrap_or(false);

    if !browsing_sessions && state.plan_focus_active() {
        if let Some(doc) = state.plan_document.clone() {
            // Single synthetic index so scroll helpers stay consistent.
            state.message_line_starts.push(0);
            push_plan_focus_lines(&mut lines, &doc.path, &doc.content, width, theme);
            return lines;
        }
    }

    // While /sessions is open, the main pane shows the highlighted session's
    // normal transcript (not a separate preview widget).
    let browse_msgs = if browsing_sessions {
        state
            .session_picker_preview
            .as_ref()
            .map(|p| p.messages.as_slice())
    } else {
        None
    };
    let messages: &[crate::app::DisplayMessage] = browse_msgs.unwrap_or(&state.messages);

    if messages.is_empty() {
        if browsing_sessions {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  (empty session)",
                Style::default().fg(theme.text_muted),
            )));
            lines.push(Line::from(""));
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
        return lines;
    }

    for (msg_idx, msg) in messages.iter().enumerate() {
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
                push_plan_box_lines(&mut lines, &path, &body, width, theme, false);
                lines.push(Line::from(""));
            }
            MessageRole::Assistant => {
                // Thinking block (dim), like kimi
                if let Some(ref thinking) = msg.thinking {
                    if !thinking.is_empty() {
                        lines.push(Line::from(Span::styled(
                            "● thinking",
                            Style::default()
                                .fg(theme.text_dim)
                                .add_modifier(Modifier::ITALIC),
                        )));
                        let think_lines: Vec<&str> = thinking.lines().collect();
                        let max_show = 20;
                        let show = think_lines.len().min(max_show);
                        for l in &think_lines[..show] {
                            lines.push(Line::from(Span::styled(
                                format!("  {}", l),
                                Style::default().fg(theme.text_muted),
                            )));
                        }
                        if think_lines.len() > max_show {
                            lines.push(Line::from(Span::styled(
                                format!(
                                    "  ... ({} more lines, ctrl+o to expand)",
                                    think_lines.len() - max_show
                                ),
                                Style::default().fg(theme.text_muted),
                            )));
                        }
                        lines.push(Line::from(""));
                    }
                }

                // Chronological parts: tools stay where they were called;
                // later text (final answer) naturally ends at the bottom.
                let mut first_bullet = true;
                let mut rendered_any = false;

                let parts: Vec<&DisplayPart> = if !msg.parts.is_empty() {
                    msg.parts.iter().collect()
                } else {
                    // Legacy fallback: tools then content (prefer tools above text).
                    Vec::new()
                };

                if !parts.is_empty() {
                    for part in parts {
                        match part {
                            DisplayPart::Text(text) => {
                                if text.is_empty() {
                                    continue;
                                }
                                for (i, line) in text.lines().enumerate() {
                                    if first_bullet && i == 0 {
                                        push_assistant_wrapped(
                                            &mut lines, line, width, theme, true,
                                        );
                                        first_bullet = false;
                                    } else {
                                        push_assistant_wrapped(
                                            &mut lines, line, width, theme, false,
                                        );
                                    }
                                }
                                rendered_any = true;
                            }
                            DisplayPart::Tool(tc) => {
                                render_tool_call_lines(&mut lines, tc, width, theme, first_bullet);
                                first_bullet = false;
                                rendered_any = true;
                            }
                            DisplayPart::ToolHistory(hist) => {
                                render_tool_history_lines(
                                    &mut lines, hist, width, theme, state.locale, first_bullet,
                                );
                                first_bullet = false;
                                rendered_any = true;
                            }
                        }
                    }
                } else {
                    // Fallback for old messages without parts: tools first, then content.
                    for tc in &msg.tool_calls {
                        render_tool_call_lines(&mut lines, tc, width, theme, first_bullet);
                        first_bullet = false;
                        rendered_any = true;
                    }
                    if !msg.content.is_empty() {
                        for (i, line) in msg.content.lines().enumerate() {
                            if first_bullet && i == 0 {
                                push_assistant_wrapped(&mut lines, line, width, theme, true);
                                first_bullet = false;
                            } else {
                                push_assistant_wrapped(&mut lines, line, width, theme, false);
                            }
                        }
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
                    lines.push(Line::from(vec![
                        Span::styled("● ", Style::default().fg(theme.text_dim)),
                        Span::styled(line.to_string(), style),
                    ]));
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

    match state.status {
        SessionStatus::Thinking | SessionStatus::WaitingApproval => {
            let frames = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
            let ch = frames[(state.tick / 2) % frames.len()];
            if state.status == SessionStatus::WaitingApproval {
                lines.push(Line::from(Span::styled(
                    format!("● {} waiting for approval", ch),
                    Style::default()
                        .fg(theme.warning)
                        .add_modifier(Modifier::ITALIC),
                )));
            } else if !state.thinking_text.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!("● {} thinking", ch),
                    Style::default()
                        .fg(theme.text_dim)
                        .add_modifier(Modifier::ITALIC),
                )));
                // Live streaming thinking (last ~12 lines)
                let all: Vec<&str> = state.thinking_text.lines().collect();
                let start = all.len().saturating_sub(12);
                for l in &all[start..] {
                    lines.push(Line::from(Span::styled(
                        format!("  {}", l),
                        Style::default().fg(theme.text_muted),
                    )));
                }
                lines.push(Line::from(Span::styled(
                    format!("  {}", state.stream_cursor.glyph()),
                    Style::default().fg(theme.primary),
                )));
            } else {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("● {} thinking ", ch),
                        Style::default()
                            .fg(theme.text_dim)
                            .add_modifier(Modifier::ITALIC),
                    ),
                    Span::styled(
                        state.stream_cursor.glyph().to_string(),
                        Style::default().fg(theme.primary),
                    ),
                ]));
            }
        }
        SessionStatus::ToolExecuting => {
            let frames = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
            let ch = frames[(state.tick / 2) % frames.len()];
            let tool = state.last_tool_name.as_deref().unwrap_or("tool");
            lines.push(Line::from(Span::styled(
                format!("● {} running {tool}", ch),
                Style::default()
                    .fg(theme.primary)
                    .add_modifier(Modifier::ITALIC),
            )));
        }
        _ => {}
    }

    lines
}

fn assistant_first_line(line: &str, theme: &Theme) -> Line<'static> {
    let trimmed = line.trim_start();
    // 第一条助手行带 ●
    if trimmed.starts_with('#') {
        let rest = trimmed.trim_start_matches('#').trim_start();
        return Line::from(vec![
            Span::styled("● ", Style::default().fg(theme.text)),
            Span::styled(
                rest.to_string(),
                Style::default()
                    .fg(theme.primary)
                    .add_modifier(Modifier::BOLD),
            ),
        ]);
    }
    Line::from(vec![
        Span::styled("● ", Style::default().fg(theme.text)),
        Span::styled(line.to_string(), Style::default().fg(theme.text)),
    ])
}

fn status_icon(tc: &crate::app::DisplayToolCall) -> &'static str {
    if tc.output.is_some() {
        if tc.is_error {
            "✗"
        } else {
            "✓"
        }
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
) {
    use crate::tool_renderers::ToolRenderRegistry;

    if first_bullet {
        lines.push(Line::from(vec![
            Span::styled("● ", Style::default().fg(theme.text)),
            Span::styled(
                format!("{} {}", status_icon(tc), ToolRenderRegistry::chip_label(tc, width)),
                ToolRenderRegistry::chip_style(tc, theme),
            ),
        ]));
    } else {
        lines.push(tool_continuation_line(tc, width, theme));
    }

    if !tc.collapsed {
        lines.extend(ToolRenderRegistry::summary_lines(tc, width, theme, 12));
    } else if tc.output.as_ref().map(|o| o.lines().count()).unwrap_or(0) > 1 {
        let n = tc.output.as_ref().map(|o| o.lines().count()).unwrap_or(0);
        lines.push(Line::from(Span::styled(
            format!("  … ({n} more lines, ctrl+o to expand)"),
            Style::default().fg(theme.text_muted),
        )));
    }
}

fn render_tool_history_lines(
    lines: &mut Vec<Line<'static>>,
    hist: &ToolHistorySummary,
    width: u16,
    theme: &Theme,
    locale: Locale,
    first_bullet: bool,
) {
    let overview = i18n::tool_history_overview(
        locale,
        hist.tool_count,
        hist.duration_ms,
        hist.tokens,
    );
    let hint = if hist.expanded {
        i18n::tool_history_collapse_hint(locale)
    } else {
        i18n::tool_history_expand_hint(locale)
    };
    let prefix = if first_bullet { "● " } else { "  " };
    lines.push(Line::from(vec![
        Span::styled(prefix, Style::default().fg(theme.text_dim)),
        Span::styled(
            format!("… {overview}"),
            Style::default().fg(theme.text_muted),
        ),
        Span::styled(
            format!(" ({hint})"),
            Style::default().fg(theme.text_dim),
        ),
    ]));

    if hist.expanded {
        for tc in &hist.tools {
            let mut shown = tc.clone();
            shown.collapsed = false;
            render_tool_call_lines(lines, &shown, width, theme, false);
        }
    }
}

const TODO_MAX_VISIBLE: usize = 5;

fn todo_panel_height(state: &AppState) -> u16 {
    if state.todos.is_empty() {
        return 0;
    }
    // separator + title + rows (+ optional overflow hint)
    let rows = if state.todos_expanded {
        state.todos.len()
    } else {
        state.todos.len().min(TODO_MAX_VISIBLE)
    };
    let overflow = !state.todos_expanded && state.todos.len() > TODO_MAX_VISIBLE;
    (2 + rows + if overflow { 1 } else { 0 }) as u16
}

fn render_todo_panel(f: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    if area.height == 0 || state.todos.is_empty() {
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

    f.render_widget(Paragraph::new(Text::from(lines)), area);
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

fn style_markdown_line(line: &str, theme: &Theme) -> Line<'static> {
    let trimmed = line.trim_start();
    let owned = line.to_string();

    if trimmed.starts_with("```") {
        return Line::from(Span::styled(
            format!("  {}", owned),
            Style::default().fg(theme.text_muted),
        ));
    }
    if let Some(rest) = trimmed
        .strip_prefix("### ")
        .or_else(|| trimmed.strip_prefix("## "))
        .or_else(|| trimmed.strip_prefix("# "))
    {
        return Line::from(Span::styled(
            format!("  {}", rest),
            Style::default()
                .fg(theme.primary)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if let Some(rest) = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
    {
        return Line::from(vec![
            Span::raw("  "),
            Span::styled("• ", Style::default().fg(theme.text)),
            Span::styled(rest.to_string(), Style::default().fg(theme.text)),
        ]);
    }
    Line::from(Span::styled(
        format!("  {}", owned),
        Style::default().fg(theme.text),
    ))
}

fn truncate(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{}…", t)
    }
}

fn render_input(f: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let border = match state.mode {
        AppMode::Shell => theme.shell_mode,
        AppMode::Plan => theme.plan_mode,
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
        AppMode::Normal => ("> ", Style::default().fg(theme.text)),
    };

    let title = match state.mode {
        AppMode::Shell => " shell ",
        AppMode::Plan => " plan ",
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

    // Soft-wrap each logical line; prefix only the first visual row of the buffer.
    let mut visual: Vec<Line> = Vec::new();
    for (li, logical) in input_logical_lines(&state.input.text).into_iter().enumerate() {
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

    let (cursor_x, cursor_y) =
        cursor_position(&state.input.text, state.input.cursor, content_width, prefix_w as u16);
    // Exact wrap boundary can place the cursor on a fresh visual row — pad so it paints.
    while (visual.len() as u16) <= cursor_y {
        visual.push(Line::from(vec![
            Span::raw(indent.clone()),
            Span::styled(String::new(), Style::default().fg(theme.text)),
        ]));
    }

    let view_h = inner.height.max(1);
    let scroll = if cursor_y + 1 > view_h {
        cursor_y + 1 - view_h
    } else {
        0
    };

    let paragraph = Paragraph::new(Text::from(visual)).scroll((scroll, 0));
    f.render_widget(paragraph, inner);

    let abs_x = inner.x + cursor_x;
    let abs_y = inner.y + cursor_y.saturating_sub(scroll);
    if abs_x < inner.x + inner.width && abs_y < inner.y + inner.height {
        f.set_cursor_position((abs_x, abs_y));
    }
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
            let col = UnicodeWidthStr::width(&logical[..col_end]);
            let wrap_row = (col / width) as u16;
            let x = prefix_w + (col % width) as u16;
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

fn render_footer(f: &mut Frame, area: Rect, state: &AppState, config: &AppConfig, theme: &Theme) {
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

    // Live spinner when agent is busy
    if matches!(
        state.status,
        SessionStatus::Thinking | SessionStatus::ToolExecuting | SessionStatus::WaitingApproval
    ) {
        let frames = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
        let ch = frames[(state.tick / 2) % frames.len()];
        left.push(Span::styled(
            format!("{ch} "),
            Style::default().fg(theme.primary),
        ));
    }

    left.push(Span::styled(
        model_label(config),
        Style::default().fg(theme.text),
    ));

    if thinking_label(config).is_some() {
        left.push(Span::raw("  "));
        left.push(Span::styled(
            thinking_label(config).unwrap(),
            Style::default().fg(theme.text_dim),
        ));
    }

    if let Ok(cwd) = std::env::current_dir() {
        left.push(Span::raw("  "));
        left.push(Span::styled(
            shorten_path(&cwd.to_string_lossy()),
            Style::default().fg(theme.text_dim),
        ));
        let git = git_badge::git_badge(&cwd);
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

    if state.btw.open || !state.btw.turns.is_empty() {
        left.push(Span::raw("  "));
        let label = if state.btw.streaming {
            "btw…".to_string()
        } else {
            format!("btw:{}", state.btw.turns.len())
        };
        left.push(Span::styled(label, Style::default().fg(theme.accent)));
    }

    let tip = if state.quit_confirm {
        "press ctrl-c again to quit".to_string()
    } else if state.search.active {
        "↑↓ navigate · enter jump · esc close".to_string()
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
            Style::default().fg(if state.quit_confirm {
                theme.warning
            } else {
                theme.text_muted
            }),
        ));
    }

    // Line 2: workspace session strip (left) + context meter (right)
    let context = format_context(state, config);
    let ctx_w = UnicodeWidthStr::width(context.as_str());
    let gap = 2usize;
    let strip_budget = (area.width as usize)
        .saturating_sub(ctx_w)
        .saturating_sub(gap);
    let session_spans = state
        .workspace_sessions
        .render_spans(strip_budget, theme);
    let strip_text: String = session_spans.iter().map(|s| s.content.clone()).collect();
    let strip_w = UnicodeWidthStr::width(strip_text.as_str());
    let pad2 = area
        .width
        .saturating_sub(strip_w as u16)
        .saturating_sub(ctx_w as u16);
    let mut line2_spans = session_spans;
    if pad2 > 0 {
        line2_spans.push(Span::raw(" ".repeat(pad2 as usize)));
    }
    line2_spans.push(Span::styled(
        context,
        Style::default().fg(theme.text),
    ));
    let line2 = Line::from(line2_spans);

    f.render_widget(
        Paragraph::new(Text::from(vec![Line::from(line1_spans), line2])),
        area,
    );
}

fn render_scroll_hint(f: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    if state.follow_bottom || state.scroll_up == 0 || area.height < 2 {
        return;
    }
    let max = state.max_scroll_up();
    if max == 0 {
        return;
    }
    let label = format!(" ↑ {} / {} ", state.scroll_up, max);
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
    let width = size.width.saturating_sub(4).clamp(24, 72);
    let hit_rows = state.search.hits.len().min(10) as u16;
    let height = hit_rows + 4; // title + query + hits + hint
    let area = Rect {
        x: size.x + (size.width.saturating_sub(width)) / 2,
        y: size.y + (size.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    f.render_widget(Clear, area);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(
            " find ",
            Style::default()
                .fg(theme.primary)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" {} ", state.search.query),
            Style::default().fg(theme.text_strong).bg(theme.border),
        ),
        Span::styled(
            if state.search.query.is_empty() {
                " type to search…".into()
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
        "─".repeat(width.saturating_sub(2) as usize),
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
            lines.push(Line::from(vec![
                Span::styled(format!(" {marker} "), style),
                Span::styled(
                    format!("{:8} ", hit.role),
                    Style::default().fg(if selected {
                        theme.accent
                    } else {
                        theme.text_muted
                    }),
                ),
                Span::styled(
                    truncate(&hit.preview, (width as usize).saturating_sub(14)),
                    style,
                ),
            ]));
        }
    }

    lines.push(Line::from(Span::styled(
        " ↑↓ select · enter jump · esc close",
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

fn model_label(config: &AppConfig) -> String {
    let alias = config.default_model_alias().unwrap_or("?");
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

fn format_context(state: &AppState, config: &AppConfig) -> String {
    let max = config
        .default_model_alias()
        .and_then(|a| config.resolve_model(a))
        .and_then(|(m, _)| m.max_context_size)
        .unwrap_or(256_000);
    // 粗略估计：尚未接真实 usage 时用消息长度估算
    let used = state
        .messages
        .iter()
        .map(|m| m.content.len() as u64 / 4)
        .sum::<u64>()
        .saturating_add(state.approx_tokens);
    let pct = used
        .saturating_mul(100)
        .checked_div(max)
        .unwrap_or(0)
        .min(100);
    format!(
        "context: {}% ({}/{})",
        pct,
        format_tokens(used),
        format_tokens(max)
    )
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
            // Primary column ~28, then description
            let primary_w = 28usize;
            let padded = if label.len() < primary_w {
                format!("{:width$}", label, width = primary_w)
            } else {
                let t: String = label.chars().take(primary_w.saturating_sub(1)).collect();
                format!("{}…", t)
            };
            lines.push(Line::from(vec![
                Span::styled(prefix, name_style),
                Span::styled(padded, name_style),
                Span::styled(
                    truncate(
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
        format!(" @ {} ", truncate(&menu.query, 40))
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
                    truncate(path, (inner.width as usize).saturating_sub(10)),
                    style,
                ),
            ]));
        }
    }

    f.render_widget(Paragraph::new(Text::from(lines)).block(block), area);
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

    let inner = area.inner(Margin::new(1, 1));
    f.render_widget(Clear, area);

    // Delete confirm: ↑↓ between No / Yes, Enter to confirm (default No).
    if picker.kind == crate::app::ListPickerKind::Session {
        if let Some(confirm) = state.session_delete_confirm.as_ref() {
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.warning))
                .title(" Delete session ");
            let mut lines: Vec<Line> = Vec::new();
            lines.push(Line::from(Span::styled(
                format!(
                    " {}",
                    truncate(&confirm.label, inner.width.saturating_sub(2) as usize)
                ),
                Style::default().fg(theme.text),
            )));
            lines.push(Line::from(""));
            for (i, label) in ["No — keep session", "Yes — delete permanently"]
                .iter()
                .enumerate()
            {
                let selected = confirm.selected == i;
                let prefix = if selected { "> " } else { "  " };
                let style = if selected {
                    Style::default()
                        .fg(if i == 1 { theme.error } else { theme.primary })
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.text_dim)
                };
                lines.push(Line::from(Span::styled(
                    format!("{prefix}{label}"),
                    style,
                )));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                " ↑↓ select · Enter confirm · Esc cancel ",
                Style::default().fg(theme.text_muted),
            )));
            f.render_widget(Paragraph::new(Text::from(lines)).block(block), area);
            return;
        }
    }

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
            let primary_w = 36usize;
            let padded = if item.label.len() < primary_w {
                format!("{:width$}", item.label, width = primary_w)
            } else {
                let t: String = item
                    .label
                    .chars()
                    .take(primary_w.saturating_sub(1))
                    .collect();
                format!("{}…", t)
            };
            lines.push(Line::from(vec![
                Span::styled(prefix, name_style),
                Span::styled(padded, name_style),
                Span::styled(
                    truncate(
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

fn render_tasks_panel(f: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let Some(panel) = state.tasks_panel.as_ref() else {
        return;
    };

    let panel_w = area.width.saturating_sub(4).clamp(40, 90);
    let panel_h = area.height.saturating_sub(4).clamp(12, 28);
    let x = (area.width.saturating_sub(panel_w)) / 2;
    let y = (area.height.saturating_sub(panel_h)) / 2;
    let panel_area = Rect::new(x, y, panel_w, panel_h);

    f.render_widget(Clear, panel_area);
    let block = Block::default()
        .title(" background tasks ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border));

    let inner = panel_area.inner(Margin::new(1, 1));
    let mut lines: Vec<Line> = Vec::new();
    if panel.tasks.is_empty() {
        lines.push(Line::from(Span::styled(
            "No background tasks yet.",
            Style::default().fg(theme.text_muted),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Tasks launched via the Task tool appear here.",
            Style::default().fg(theme.text_dim),
        )));
    } else {
        let max_visible = inner.height.saturating_sub(1) as usize;
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
            let status = &task.status;
            let label = format!(
                "{}[{}] {} — {}",
                prefix,
                status,
                truncate(&task.task_id, 10),
                truncate(&task.description, 40)
            );
            lines.push(Line::from(Span::styled(label, style)));
            if selected {
                if let Some(ref r) = task.result {
                    lines.push(Line::from(Span::styled(
                        format!(
                            "    {}",
                            truncate(r, (inner.width as usize).saturating_sub(4))
                        ),
                        Style::default().fg(theme.text_dim),
                    )));
                }
                if let Some(ref e) = task.error {
                    lines.push(Line::from(Span::styled(
                        format!(
                            "    err: {}",
                            truncate(e, (inner.width as usize).saturating_sub(8))
                        ),
                        Style::default().fg(theme.error),
                    )));
                }
            }
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "↑↓ navigate · r refresh · Esc close",
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

    // Full plan body — never truncate.
    let body_lines: Vec<&str> = if content.is_empty() {
        vec![""]
    } else {
        content.lines().collect()
    };
    for raw in body_lines {
        let chunks = if raw.is_empty() {
            vec![String::new()]
        } else {
            wrap_str(raw, content_width)
        };
        for chunk in chunks {
            let styled = plan_body_spans(&chunk, theme);
            let chunk_w: usize = styled
                .iter()
                .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
                .sum();
            let pad = content_width.saturating_sub(chunk_w);
            let mut spans = Vec::with_capacity(styled.len() + 4);
            spans.push(Span::raw(indent.clone()));
            spans.push(Span::styled("│", border));
            spans.push(Span::raw(" "));
            spans.extend(styled);
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

fn plan_body_spans(line: &str, theme: &Theme) -> Vec<Span<'static>> {
    let trimmed = line.trim_start();
    if let Some(rest) = trimmed
        .strip_prefix("### ")
        .or_else(|| trimmed.strip_prefix("## "))
        .or_else(|| trimmed.strip_prefix("# "))
    {
        return vec![Span::styled(
            rest.to_string(),
            Style::default()
                .fg(theme.primary)
                .add_modifier(Modifier::BOLD),
        )];
    }
    if let Some(rest) = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
    {
        return vec![
            Span::styled("• ", Style::default().fg(theme.text)),
            Span::styled(rest.to_string(), Style::default().fg(theme.text)),
        ];
    }
    if trimmed.starts_with("```") {
        return vec![Span::styled(
            line.to_string(),
            Style::default().fg(theme.text_muted),
        )];
    }
    vec![Span::styled(
        line.to_string(),
        Style::default().fg(theme.text),
    )]
}

fn push_wrapped_prefixed(
    lines: &mut Vec<Line<'static>>,
    prefix: &str,
    text: &str,
    width: u16,
    prefix_style: Style,
    text_style: Style,
) {
    let prefix_w = UnicodeWidthStr::width(prefix);
    let avail = (width as usize).saturating_sub(prefix_w).max(8);
    let indent = " ".repeat(prefix_w);
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
        for chunk in wrap_str(para, avail) {
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

fn push_assistant_wrapped(
    lines: &mut Vec<Line<'static>>,
    line: &str,
    width: u16,
    theme: &Theme,
    is_first: bool,
) {
    let prefix = if is_first { "● " } else { "  " };
    let prefix_w = UnicodeWidthStr::width(prefix);
    let avail = (width as usize).saturating_sub(prefix_w).max(8);
    let mut first_chunk = true;
    for chunk in wrap_str(line, avail) {
        if is_first && first_chunk {
            lines.push(assistant_first_line(&chunk, theme));
            first_chunk = false;
        } else {
            lines.push(style_markdown_line(&chunk, theme));
        }
    }
}

fn wrap_str(s: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return vec![s.to_string()];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    for ch in s.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if cur_w + w > max_width && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
            cur_w = 0;
        }
        cur.push(ch);
        cur_w += w;
    }
    if !cur.is_empty() || out.is_empty() {
        out.push(cur);
    }
    out
}

fn render_approval_panel(f: &mut Frame, area: Rect, approval: &mut PendingApproval, theme: &Theme) {
    let panel_width = 72.min(area.width.saturating_sub(4)).max(40);
    let choice_count = approval.choices.len().max(1) as u16;
    let detail_lines = if approval.is_plan_review {
        0
    } else {
        approval.detail.lines().count().min(8) as u16
    };
    let feedback_extra: u16 = if approval.feedback_mode { 3 } else { 0 };
    let panel_height = (8 + choice_count + detail_lines + feedback_extra)
        .min(area.height.saturating_sub(2))
        .max(10);
    let x = (area.width.saturating_sub(panel_width)) / 2;
    let y = if approval.is_plan_review {
        // Sit near the bottom so the plan document stays visible above.
        area.height.saturating_sub(panel_height + 1)
    } else {
        (area.height.saturating_sub(panel_height)) / 2
    };
    let panel_area = Rect::new(x, y, panel_width, panel_height);

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
            let truncated: String = l
                .chars()
                .take((panel_width.saturating_sub(4)) as usize)
                .collect();
            lines.push(Line::from(Span::styled(
                truncated,
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
        lines.push(Line::from(vec![
            Span::styled(marker, style),
            Span::styled(format!("{key}  "), Style::default().fg(theme.text_muted)),
            Span::styled(choice.label.clone(), style),
        ]));
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
            if approval.is_plan_review {
                "  1·2·3… / ↑↓ / enter · esc 取消"
            } else {
                "  1·2·3 / enter"
            },
            Style::default().fg(theme.text_muted),
        )));
    }

    f.render_widget(Paragraph::new(Text::from(lines)).block(block), panel_area);
}

fn render_question_panel(f: &mut Frame, area: Rect, question: &mut PendingQuestion, theme: &Theme) {
    let panel_width = 72.min(area.width.saturating_sub(4)).max(40);
    let opt_count = question.options.len() as u16;
    let free_lines: u16 = if question.allow_free_text { 2 } else { 0 };
    let panel_height = (6 + opt_count + free_lines)
        .min(area.height.saturating_sub(2))
        .max(8);
    let x = (area.width.saturating_sub(panel_width)) / 2;
    let y = (area.height.saturating_sub(panel_height)) / 2;
    let panel_area = Rect::new(x, y, panel_width, panel_height);

    f.render_widget(Clear, panel_area);

    let block = Block::default()
        .title(" question ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.primary));

    let mut lines: Vec<Line> = vec![
        Line::from(Span::styled(
            question.text.clone(),
            Style::default()
                .fg(theme.text_strong)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];

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
        let boxc = if checked { "[x]" } else { "[ ]" };
        lines.push(Line::from(vec![
            Span::styled(marker, style),
            Span::styled(
                format!("{} {}  ", i + 1, boxc),
                Style::default().fg(theme.text_muted),
            ),
            Span::styled(label.clone(), style),
        ]));
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
        lines.push(Line::from(vec![
            Span::styled(marker, style),
            Span::styled("text: ", Style::default().fg(theme.text_muted)),
            Span::styled(question.free_text.clone(), style),
            Span::styled("▌", style),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  1-9 / space toggle / enter confirm / esc cancel",
        Style::default().fg(theme.text_muted),
    )));

    f.render_widget(Paragraph::new(Text::from(lines)).block(block), panel_area);
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
    use crate::app::AppState;
    use kkagent_protocol::PermissionMode;

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
        assert!(joined.contains("more lines") == false);
        assert!(joined.contains("scroll = full plan only"));
        // No earlier transcript noise
        assert!(!joined.contains("kkagent"));
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
    fn wrap_wide() {
        let s = "你好世界abcdef";
        let parts = wrap_str(s, 4);
        assert!(!parts.is_empty());
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
    fn trailing_newline_keeps_empty_visual_row() {
        let text = "hi\n";
        assert_eq!(input_logical_lines(text), vec!["hi", ""]);
        assert_eq!(input_visual_row_count(text, 40), 2);
        let (x, y) = cursor_position(text, text.len(), 40, 2);
        assert_eq!((x, y), (2, 1));
    }
}
