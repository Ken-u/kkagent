use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Margin, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph},
};
use kkagent_config::AppConfig;
use kkagent_protocol::{PermissionMode, SessionStatus};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{AppMode, AppState, DisplayPart, ListPickerState, MessageRole, PendingApproval, PendingQuestion, TodoItem};
use crate::theme::Theme;

const TIPS: &[&str] = &[
    "/compact compresses context when it gets long",
    "ctrl+o expands truncated tool output",
    "shift-tab toggles plan mode",
    "! enters shell mode",
    "/yolo auto-approves tools",
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
        .or_else(|| state.list_picker.as_ref().map(picker_height))
        .unwrap_or(0);

    // Sticky todo sits above the input (highest visual priority).
    let todo_height = todo_panel_height(state);

    // kimi 布局：消息区 | todo(可选) | 带边框输入框 | footer 两行
    let input_inner = input_inner_height(state);
    let input_box = input_inner + 2; // borders
    let bottom_stack = todo_height + input_box + slash_height;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(bottom_stack),
            Constraint::Length(2),
        ])
        .split(size);

    let bottom = chunks[1];
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

    render_messages(f, chunks[0], state, &theme);
    if todo_height > 0 {
        render_todo_panel(f, todo_area, state, &theme);
    }
    render_input(f, input_area, state, &theme);
    render_footer(f, chunks[2], state, config, &theme);

    if let Some(ref mut approval) = state.approval_pending {
        render_approval_panel(f, size, approval, &theme);
    }

    if let Some(ref mut question) = state.question_pending {
        render_question_panel(f, size, question, &theme);
    }

    if state.slash_menu.is_some() {
        render_slash_menu(f, slash_area, state, &theme);
    } else if state.list_picker.is_some() {
        render_list_picker(f, slash_area, state, &theme);
    }

    if state.tasks_panel.is_some() {
        render_tasks_panel(f, size, state, &theme);
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

fn picker_height(picker: &ListPickerState) -> u16 {
    let max_visible = 10u16;
    let rows = if picker.items.is_empty() {
        1
    } else {
        (picker.items.len() as u16).min(max_visible)
    };
    rows + 2
}

fn input_inner_height(state: &AppState) -> u16 {
    let lines = state.input.text.lines().count().max(1) as u16;
    lines.min(6).max(1)
}

fn render_messages(f: &mut Frame, area: Rect, state: &mut AppState, theme: &Theme) {
    let width = area.width.max(1);
    let lines = build_transcript_lines(state, theme, width);
    let content_height = lines.len() as u16;
    let visible_height = area.height.max(1);

    state.content_lines = content_height;
    state.viewport_height = visible_height;

    let max_scroll_up = content_height.saturating_sub(visible_height);
    if state.follow_bottom {
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

fn build_transcript_lines(state: &AppState, theme: &Theme, width: u16) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();

    if state.messages.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("● ", Style::default().fg(theme.primary)),
            Span::styled(
                "kkagent",
                Style::default()
                    .fg(theme.primary)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        lines.push(Line::from(Span::styled(
            "  coding agent for your terminal",
            Style::default().fg(theme.text_dim),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  type a message · /help · shift-tab plan · ! shell",
            Style::default().fg(theme.text_muted),
        )));
        lines.push(Line::from(""));
        return lines;
    }

    for msg in &state.messages {
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
                lines.push(Line::from(Span::styled(
                    "● plan",
                    Style::default()
                        .fg(theme.plan_mode)
                        .add_modifier(Modifier::BOLD),
                )));
                let mut body = msg.content.as_str();
                if let Some(rest) = body.strip_prefix("file: ") {
                    if let Some((path_line, rest_body)) = rest.split_once('\n') {
                        lines.push(Line::from(Span::styled(
                            format!("  {}", path_line.trim()),
                            Style::default().fg(theme.text_muted),
                        )));
                        body = rest_body.trim_start_matches('\n');
                    }
                }
                // Full plan — no line cap (unlike tool-result previews).
                for line in body.lines() {
                    push_assistant_wrapped(&mut lines, line, width, theme, false);
                }
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
                                        push_assistant_wrapped(&mut lines, line, width, theme, true);
                                        first_bullet = false;
                                    } else {
                                        push_assistant_wrapped(&mut lines, line, width, theme, false);
                                    }
                                }
                                rendered_any = true;
                            }
                            DisplayPart::Tool(tc) => {
                                render_tool_call_lines(
                                    &mut lines,
                                    tc,
                                    width,
                                    theme,
                                    first_bullet,
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
            } else {
                lines.push(Line::from(Span::styled(
                    format!("● {} thinking", ch),
                    Style::default()
                        .fg(theme.text_dim)
                        .add_modifier(Modifier::ITALIC),
                )));
            }
        }
        SessionStatus::ToolExecuting => {
            let frames = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
            let ch = frames[(state.tick / 2) % frames.len()];
            lines.push(Line::from(Span::styled(
                format!("● {} running", ch),
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
        let rest = trimmed
            .trim_start_matches('#')
            .trim_start();
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
    if first_bullet {
        lines.push(Line::from(vec![
            Span::styled("● ", Style::default().fg(theme.text)),
            Span::styled(
                format!("{} {}", status_icon(tc), tc.name),
                Style::default().fg(if tc.is_error {
                    theme.error
                } else {
                    theme.text_dim
                }),
            ),
            Span::styled(
                format!("  {}", truncate(&tc.input_summary, 64)),
                Style::default().fg(theme.text_muted),
            ),
        ]));
    } else {
        lines.push(tool_continuation_line(tc, theme));
    }

    if !tc.collapsed {
        if let Some(ref output) = tc.output {
            let max_preview = 12;
            let all: Vec<&str> = output.lines().collect();
            let show = all.len().min(max_preview);
            for l in &all[..show] {
                let truncated = truncate(l, width.saturating_sub(4) as usize);
                lines.push(Line::from(Span::styled(
                    format!("  {}", truncated),
                    Style::default().fg(theme.text_muted),
                )));
            }
            if all.len() > max_preview {
                lines.push(Line::from(Span::styled(
                    format!(
                        "  ... ({} more lines, ctrl+o to expand)",
                        all.len() - max_preview
                    ),
                    Style::default().fg(theme.text_muted),
                )));
            }
        }
    } else if tc.output.as_ref().map(|o| o.lines().count()).unwrap_or(0) > 1 {
        let n = tc.output.as_ref().map(|o| o.lines().count()).unwrap_or(0);
        lines.push(Line::from(Span::styled(
            format!("  ... ({} more lines, ctrl+o to expand)", n),
            Style::default().fg(theme.text_muted),
        )));
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
            Style::default()
                .fg(theme.text)
                .add_modifier(Modifier::BOLD),
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

fn tool_continuation_line(tc: &crate::app::DisplayToolCall, theme: &Theme) -> Line<'static> {
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
            format!("{} {}", status_icon(tc), tc.name),
            Style::default().fg(color),
        ),
        Span::styled(
            format!("  {}", truncate(&tc.input_summary, 64)),
            Style::default().fg(theme.text_muted),
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
    if let Some(rest) = trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* ")) {
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
                SessionStatus::Thinking | SessionStatus::ToolExecuting
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

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let prefix_w = UnicodeWidthStr::width(prefix) as u16;

    // Build text lines, prefixing only the first line and indenting continuations
    let lines: Vec<Line> = state
        .input
        .text
        .lines()
        .enumerate()
        .map(|(i, line)| {
            if i == 0 {
                Line::from(vec![
                    Span::styled(prefix, prefix_style),
                    Span::styled(line.to_string(), Style::default().fg(theme.text)),
                ])
            } else {
                Line::from(vec![
                    Span::raw(" ".repeat(prefix_w as usize)),
                    Span::styled(line.to_string(), Style::default().fg(theme.text)),
                ])
            }
        })
        .collect();

    // Handle empty input so we still show the prefix/cursor
    let paragraph = if lines.is_empty() {
        Paragraph::new(Line::from(vec![Span::styled(prefix, prefix_style)]))
    } else {
        Paragraph::new(Text::from(lines))
    };
    f.render_widget(paragraph, inner);

    // Cursor position: support multi-line wrapping within the input box
    let (cursor_x, cursor_y) =
        cursor_position(&state.input.text, state.input.cursor, inner, prefix_w);
    let abs_x = inner.x + cursor_x;
    let abs_y = inner.y + cursor_y;
    if abs_x < inner.x + inner.width && abs_y < inner.y + inner.height {
        f.set_cursor_position((abs_x, abs_y));
    }
}

/// Compute (x, y) relative to `inner` for the input cursor.
fn cursor_position(text: &str, cursor: usize, inner: Rect, prefix_w: u16) -> (u16, u16) {
    if text.is_empty() {
        return (prefix_w.min(inner.width.saturating_sub(1)), 0);
    }
    // Ensure we never slice mid-codepoint (would panic).
    let mut safe_cursor = cursor.min(text.len());
    while safe_cursor > 0 && !text.is_char_boundary(safe_cursor) {
        safe_cursor -= 1;
    }
    let line_start = text[..safe_cursor]
        .rfind('\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    let before_cursor = &text[line_start..safe_cursor];
    let col = UnicodeWidthStr::width(before_cursor) as u16;
    let line_index = text[..safe_cursor].matches('\n').count() as u16;
    let width = inner.width.saturating_sub(prefix_w).max(1);
    let x = prefix_w + (col % width);
    let y = line_index + (col / width);
    (x, y)
}

fn render_footer(f: &mut Frame, area: Rect, state: &AppState, config: &AppConfig, theme: &Theme) {
    // Line 1: yolo  model  thinking  cwd ............ tip
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
    }

    let tip = if state.quit_confirm {
        "press ctrl-c again to quit".to_string()
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

    // Line 2: context on the right
    let context = format_context(state, config);
    let ctx_w = UnicodeWidthStr::width(context.as_str()) as u16;
    let pad2 = area.width.saturating_sub(ctx_w);
    let line2 = Line::from(vec![
        Span::raw(" ".repeat(pad2 as usize)),
        Span::styled(context, Style::default().fg(theme.text)),
    ]);

    f.render_widget(
        Paragraph::new(Text::from(vec![Line::from(line1_spans), line2])),
        area,
    );
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
    Some(format!(
        "thinking: {}",
        t.effort.as_deref().unwrap_or("on")
    ))
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
    let pct = if max > 0 {
        ((used * 100) / max).min(100)
    } else {
        0
    };
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
                    truncate(&item.description, (inner.width as usize).saturating_sub(primary_w + 4)),
                    Style::default().fg(theme.text_dim),
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
                let t: String = item.label.chars().take(primary_w.saturating_sub(1)).collect();
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

    let panel_w = area.width.saturating_sub(4).max(40).min(90);
    let panel_h = area.height.saturating_sub(4).max(12).min(28);
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
                        format!("    {}", truncate(r, (inner.width as usize).saturating_sub(4))),
                        Style::default().fg(theme.text_dim),
                    )));
                }
                if let Some(ref e) = task.error {
                    lines.push(Line::from(Span::styled(
                        format!("    err: {}", truncate(e, (inner.width as usize).saturating_sub(8))),
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
    let detail_lines = approval.detail.lines().count().min(8) as u16;
    let panel_height = (9 + detail_lines).min(area.height.saturating_sub(2)).max(10);
    let x = (area.width.saturating_sub(panel_width)) / 2;
    let y = (area.height.saturating_sub(panel_height)) / 2;
    let panel_area = Rect::new(x, y, panel_width, panel_height);
    approval.panel_rect = Some((x, y, panel_width, panel_height));

    f.render_widget(Clear, panel_area);

    let block = Block::default()
        .title(format!(" permission · {} ", approval.tool_name))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.warning));

    let options = [
        ("1", "allow once"),
        ("2", "allow for session"),
        ("3", "reject"),
    ];

    let mut lines: Vec<Line> = vec![
        Line::from(Span::styled(
            approval.action.clone(),
            Style::default()
                .fg(theme.text_strong)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];

    if !approval.detail.is_empty() {
        for l in approval.detail.lines().take(8) {
            let truncated: String = l.chars().take((panel_width.saturating_sub(4)) as usize).collect();
            lines.push(Line::from(Span::styled(
                truncated,
                Style::default().fg(theme.text_dim),
            )));
        }
        lines.push(Line::from(""));
    }

    for (i, (key, label)) in options.iter().enumerate() {
        let selected = i == approval.selected;
        let style = if selected {
            Style::default()
                .fg(theme.primary)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.text_dim)
        };
        let marker = if selected { "> " } else { "  " };
        lines.push(Line::from(vec![
            Span::styled(marker, style),
            Span::styled(format!("{}  ", key), Style::default().fg(theme.text_muted)),
            Span::styled(*label, style),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  click / 1·2·3 / enter",
        Style::default().fg(theme.text_muted),
    )));

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
    question.panel_rect = Some((x, y, panel_width, panel_height));

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
            Span::styled(format!("{} {}  ", i + 1, boxc), Style::default().fg(theme.text_muted)),
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
        let state = AppState::new(PermissionMode::Manual, false);
        let theme = Theme::default();
        let lines = build_transcript_lines(&state, &theme, 80);
        assert!(!lines.is_empty());
        let lines2 = build_transcript_lines(&state, &theme, 1);
        assert!(!lines2.is_empty());
    }

    #[test]
    fn wrap_wide() {
        let s = "你好世界abcdef";
        let parts = wrap_str(s, 4);
        assert!(!parts.is_empty());
    }
}
