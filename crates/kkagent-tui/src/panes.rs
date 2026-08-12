//! Auxiliary activity/queue panes and the full-screen BTW workspace.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::theme::Theme;

#[derive(Debug, Clone, Default)]
pub struct ActivityPane {
    pub lines: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct BtwTurnView {
    pub question: String,
    pub answer: String,
    pub thinking: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct BtwQueuedQuestion {
    pub session_id: String,
    pub question: String,
}

#[derive(Debug, Clone, Default)]
pub struct BtwPanelState {
    pub open: bool,
    pub streaming: bool,
    pub current_question: String,
    pub current_answer: String,
    pub current_thinking: String,
    pub turns: Vec<BtwTurnView>,
    pub error: Option<String>,
    pub pending_questions: std::collections::VecDeque<BtwQueuedQuestion>,
    pub current_session_id: Option<String>,
    /// Lines above the bottom; zero follows streaming output.
    pub scroll_offset: u16,
    pub viewport_height: u16,
    pub content_lines: u16,
}

impl BtwPanelState {
    pub fn begin_question(&mut self, question: &str) {
        self.open = true;
        self.streaming = true;
        self.scroll_offset = 0;
        self.current_question = question.to_string();
        self.current_answer.clear();
        self.current_thinking.clear();
        self.error = None;
    }

    pub fn append_delta(&mut self, text: &str) {
        self.current_answer.push_str(text);
        self.scroll_offset = 0;
    }

    pub fn append_thinking_delta(&mut self, text: &str) {
        self.current_thinking.push_str(text);
        self.scroll_offset = 0;
    }

    pub fn finish(&mut self, error: Option<String>) {
        self.streaming = false;
        if !self.current_question.is_empty() {
            let answer = match error.as_deref() {
                Some(err) if self.current_answer.is_empty() => format!("error: {err}"),
                _ => std::mem::take(&mut self.current_answer),
            };
            self.turns.push(BtwTurnView {
                question: std::mem::take(&mut self.current_question),
                answer,
                thinking: if self.current_thinking.is_empty() {
                    None
                } else {
                    Some(std::mem::take(&mut self.current_thinking))
                },
            });
        }
        self.current_answer.clear();
        self.current_thinking.clear();
        self.error = error;
        self.current_session_id = None;
        self.scroll_offset = 0;
    }

    pub fn enqueue(&mut self, session_id: String, question: String) {
        self.pending_questions.push_back(BtwQueuedQuestion {
            session_id,
            question,
        });
        self.scroll_offset = 0;
    }

    pub fn take_next(&mut self) -> Option<BtwQueuedQuestion> {
        if self.streaming {
            None
        } else {
            self.pending_questions.pop_front()
        }
    }

    pub fn max_scroll_offset(&self) -> u16 {
        self.content_lines
            .saturating_sub(self.viewport_height.max(1))
    }

    pub fn scroll_lines(&mut self, delta: i32) {
        let max = self.max_scroll_offset();
        if delta > 0 {
            self.scroll_offset = (self.scroll_offset as i32 + delta).clamp(0, max as i32) as u16;
        } else if delta < 0 {
            self.scroll_offset = self.scroll_offset.saturating_sub((-delta) as u16);
        }
        self.scroll_offset = self.scroll_offset.min(max);
    }
}

#[derive(Debug, Clone, Default)]
pub struct QueuePane {
    pub items: Vec<String>,
    pub selected: usize,
}

impl QueuePane {
    pub fn from_prompt_queue(q: &crate::prompt_queue::PromptQueue) -> Self {
        Self {
            items: q
                .items
                .iter()
                .map(|i| {
                    let kind = if i.as_steer { "steer" } else { "next" };
                    let preview: String = i.text.chars().take(48).collect();
                    format!("[{kind}] {preview}")
                })
                .collect(),
            selected: q.selected,
        }
    }
}

pub fn render_activity(f: &mut Frame, area: Rect, pane: &ActivityPane, theme: &Theme) {
    f.render_widget(Clear, area);
    let lines: Vec<Line> = if pane.lines.is_empty() {
        vec![Line::from(Span::styled(
            " (no activity yet)",
            Style::default().fg(theme.text_muted),
        ))]
    } else {
        pane.lines
            .iter()
            .rev()
            .take(area.height.saturating_sub(2) as usize)
            .rev()
            .map(|l| {
                Line::from(Span::styled(
                    format!(" {l}"),
                    Style::default().fg(theme.text_dim),
                ))
            })
            .collect()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" activity ")
        .border_style(Style::default().fg(theme.border));
    f.render_widget(Paragraph::new(lines).block(block), area);
}

pub fn render_btw(
    f: &mut Frame,
    area: Rect,
    state: &mut BtwPanelState,
    theme: &Theme,
    tick: usize,
    expanded: bool,
    render_cache: &mut crate::render_cache::RenderCache,
) {
    f.render_widget(Clear, area);
    let mut lines: Vec<Line> = Vec::new();
    if state.turns.is_empty()
        && !state.streaming
        && state.current_question.is_empty()
        && state.error.is_none()
    {
        lines.push(Line::from(Span::styled(
            " Ask a side question below. It won't affect the main chat.",
            Style::default().fg(theme.text_muted),
        )));
    }
    for (index, turn) in state.turns.iter().enumerate() {
        if index > 0 {
            lines.push(Line::default());
        }
        push_btw_question(&mut lines, &turn.question, area.width, theme);
        crate::components::push_thinking_lines(
            &mut lines,
            turn.thinking.as_deref().unwrap_or(""),
            theme,
            expanded,
        );
        push_btw_answer(&mut lines, &turn.answer, area.width, theme, render_cache);
    }
    if state.streaming || !state.current_question.is_empty() {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        push_btw_question(&mut lines, &state.current_question, area.width, theme);
        if state.current_answer.is_empty() && state.current_thinking.is_empty() {
            let frames = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
            let spinner = frames[(tick / 2) % frames.len()];
            lines.push(Line::from(Span::styled(
                if state.streaming {
                    format!("● {spinner} thinking")
                } else {
                    "● (waiting)".into()
                },
                Style::default().fg(theme.text_muted),
            )));
        } else if state.current_answer.is_empty() {
            let frames = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
            let spinner = frames[(tick / 2) % frames.len()];
            lines.push(Line::from(Span::styled(
                format!("● {spinner} thinking"),
                Style::default()
                    .fg(theme.text_dim)
                    .add_modifier(Modifier::ITALIC),
            )));
            let thinking_lines: Vec<&str> = state.current_thinking.lines().collect();
            let start = thinking_lines.len().saturating_sub(12);
            for line in &thinking_lines[start..] {
                lines.push(Line::from(Span::styled(
                    format!("  {line}"),
                    Style::default().fg(theme.text_muted),
                )));
            }
        } else {
            crate::components::push_thinking_lines(
                &mut lines,
                &state.current_thinking,
                theme,
                expanded,
            );
            push_btw_answer(
                &mut lines,
                &state.current_answer,
                area.width,
                theme,
                render_cache,
            );
            if state.streaming {
                lines.push(Line::from(Span::styled(
                    " ▍",
                    Style::default().fg(theme.accent),
                )));
            }
        }
    }
    for queued in &state.pending_questions {
        lines.push(Line::default());
        push_btw_question(&mut lines, &queued.question, area.width, theme);
        lines.push(Line::from(Span::styled(
            "  (queued)",
            Style::default().fg(theme.warning),
        )));
    }
    if let Some(err) = &state.error {
        lines.push(Line::from(Span::styled(
            format!(" error: {err}"),
            Style::default().fg(theme.error),
        )));
    }
    let title = if state.streaming {
        " BTW · streaming "
    } else {
        " BTW "
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(theme.border));
    let inner_height = area.height.saturating_sub(2);
    let content_lines = lines.len();
    let paragraph = Paragraph::new(lines).block(block);
    state.viewport_height = inner_height;
    state.content_lines = content_lines.min(u16::MAX as usize) as u16;
    state.scroll_offset = state.scroll_offset.min(state.max_scroll_offset());
    let from_top = state
        .max_scroll_offset()
        .saturating_sub(state.scroll_offset);
    f.render_widget(paragraph.scroll((from_top, 0)), area);

    if state.scroll_offset > 0 && area.width >= 10 && area.height >= 3 {
        let hint = " ↑ more ";
        let hint_area = Rect {
            x: area.x + area.width.saturating_sub(hint.len() as u16 + 1),
            y: area.y + area.height.saturating_sub(2),
            width: hint.len() as u16,
            height: 1,
        };
        f.render_widget(
            Paragraph::new(hint).style(Style::default().fg(theme.accent)),
            hint_area,
        );
    }
}

fn push_btw_question(lines: &mut Vec<Line<'static>>, question: &str, width: u16, theme: &Theme) {
    crate::components::push_wrapped_prefixed(
        lines,
        "✦ ",
        question,
        width.saturating_sub(2),
        Style::default()
            .fg(theme.role_user)
            .add_modifier(Modifier::BOLD),
        Style::default().fg(theme.role_user),
    );
    lines.push(Line::default());
}

fn push_btw_answer(
    lines: &mut Vec<Line<'static>>,
    answer: &str,
    width: u16,
    theme: &Theme,
    render_cache: &mut crate::render_cache::RenderCache,
) {
    if answer.is_empty() {
        return;
    }
    let mut first_bullet = true;
    crate::components::push_assistant_markdown(
        lines,
        answer,
        width.saturating_sub(2),
        theme,
        &mut first_bullet,
        render_cache,
    );
    lines.push(Line::default());
}

pub fn render_queue(f: &mut Frame, area: Rect, pane: &QueuePane, theme: &Theme) {
    f.render_widget(Clear, area);
    let lines: Vec<Line> = if pane.items.is_empty() {
        vec![Line::from(Span::styled(
            " (queue empty)",
            Style::default().fg(theme.text_muted),
        ))]
    } else {
        pane.items
            .iter()
            .enumerate()
            .map(|(i, n)| {
                let selected = i == pane.selected;
                Line::from(Span::styled(
                    format!("{} {n}", if selected { "▸" } else { " " }),
                    if selected {
                        Style::default()
                            .fg(theme.accent)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(theme.text_dim)
                    },
                ))
            })
            .collect()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" queue · {} · Ctrl-S steer ", pane.items.len()))
        .border_style(Style::default().fg(theme.border));
    f.render_widget(Paragraph::new(lines).block(block), area);
}

#[cfg(test)]
mod tests {
    use super::BtwPanelState;

    #[test]
    fn btw_questions_are_dequeued_serially_and_history_is_retained() {
        let mut state = BtwPanelState::default();
        state.begin_question("first");
        state.enqueue("session-a".into(), "second".into());

        assert!(state.take_next().is_none());
        state.append_delta("answer");
        state.finish(None);

        assert_eq!(state.turns.len(), 1);
        assert_eq!(state.turns[0].question, "first");
        assert_eq!(state.turns[0].answer, "answer");
        let queued = state.take_next().unwrap();
        assert_eq!(queued.session_id, "session-a");
        assert_eq!(queued.question, "second");
    }

    #[test]
    fn btw_scroll_is_clamped_to_the_rendered_content() {
        let mut state = BtwPanelState {
            content_lines: 30,
            viewport_height: 10,
            ..BtwPanelState::default()
        };

        state.scroll_lines(100);
        assert_eq!(state.scroll_offset, 20);
        state.scroll_lines(-7);
        assert_eq!(state.scroll_offset, 13);
        state.viewport_height = 25;
        state.scroll_lines(0);
        assert_eq!(state.scroll_offset, 5);
    }
}
