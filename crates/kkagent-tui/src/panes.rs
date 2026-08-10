//! Side panes: activity / btw / queue.

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
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
}

#[derive(Debug, Clone, Default)]
pub struct BtwPanelState {
    pub open: bool,
    pub streaming: bool,
    pub current_question: String,
    pub current_answer: String,
    pub turns: Vec<BtwTurnView>,
    pub error: Option<String>,
}

impl BtwPanelState {
    pub fn begin_question(&mut self, question: &str) {
        self.open = true;
        self.streaming = true;
        self.current_question = question.to_string();
        self.current_answer.clear();
        self.error = None;
    }

    pub fn append_delta(&mut self, text: &str) {
        self.current_answer.push_str(text);
    }

    pub fn finish(&mut self, error: Option<String>) {
        self.streaming = false;
        if let Some(err) = error {
            self.error = Some(err);
            return;
        }
        if !self.current_question.is_empty() {
            self.turns.push(BtwTurnView {
                question: std::mem::take(&mut self.current_question),
                answer: std::mem::take(&mut self.current_answer),
            });
        }
    }

    pub fn line_budget(&self) -> u16 {
        let mut n = 2u16;
        for t in &self.turns {
            n = n.saturating_add(2);
            n = n.saturating_add((t.answer.lines().count() as u16).max(1));
        }
        if self.streaming || !self.current_question.is_empty() {
            n = n.saturating_add(2);
            n = n.saturating_add((self.current_answer.lines().count() as u16).max(1));
        }
        if self.error.is_some() {
            n = n.saturating_add(1);
        }
        n.max(4)
    }
}

#[derive(Debug, Clone, Default)]
pub struct BtwPane {
    pub state: BtwPanelState,
}

#[derive(Debug, Clone, Default)]
pub struct QueuePane {
    pub items: Vec<String>,
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

pub fn render_btw(f: &mut Frame, area: Rect, pane: &BtwPane, theme: &Theme) {
    f.render_widget(Clear, area);
    let mut lines: Vec<Line> = Vec::new();
    if pane.state.turns.is_empty()
        && !pane.state.streaming
        && pane.state.current_question.is_empty()
        && pane.state.error.is_none()
    {
        lines.push(Line::from(Span::styled(
            " /btw <question> — side Q&A",
            Style::default().fg(theme.text_muted),
        )));
        lines.push(Line::from(Span::styled(
            " Ctrl-G closes · does not alter main chat",
            Style::default().fg(theme.text_muted),
        )));
    }
    for turn in &pane.state.turns {
        lines.push(Line::from(Span::styled(
            format!(" Q: {}", turn.question),
            Style::default().fg(theme.accent),
        )));
        for al in turn.answer.lines() {
            lines.push(Line::from(Span::styled(
                format!(" {al}"),
                Style::default().fg(theme.text),
            )));
        }
        if turn.answer.is_empty() {
            lines.push(Line::from(Span::styled(
                " (empty)",
                Style::default().fg(theme.text_muted),
            )));
        }
    }
    if pane.state.streaming || !pane.state.current_question.is_empty() {
        lines.push(Line::from(Span::styled(
            format!(" Q: {}", pane.state.current_question),
            Style::default().fg(theme.accent),
        )));
        if pane.state.current_answer.is_empty() {
            lines.push(Line::from(Span::styled(
                if pane.state.streaming {
                    " …"
                } else {
                    " (waiting)"
                },
                Style::default().fg(theme.text_muted),
            )));
        } else {
            for al in pane.state.current_answer.lines() {
                lines.push(Line::from(Span::styled(
                    format!(" {al}"),
                    Style::default().fg(theme.text),
                )));
            }
            if pane.state.streaming {
                lines.push(Line::from(Span::styled(
                    " ▍",
                    Style::default().fg(theme.accent),
                )));
            }
        }
    }
    if let Some(err) = &pane.state.error {
        lines.push(Line::from(Span::styled(
            format!(" error: {err}"),
            Style::default().fg(theme.error),
        )));
    }
    let title = if pane.state.streaming {
        " btw · streaming "
    } else {
        " btw "
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(theme.border));
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(block),
        area,
    );
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
            .map(|n| {
                Line::from(Span::styled(
                    format!(" ▸ {n}"),
                    Style::default().fg(theme.text_dim),
                ))
            })
            .collect()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" queue ")
        .border_style(Style::default().fg(theme.border));
    f.render_widget(Paragraph::new(lines).block(block), area);
}
