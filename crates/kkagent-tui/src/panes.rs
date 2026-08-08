//! Side panes: activity / btw / queue.

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::theme::Theme;

#[derive(Debug, Clone, Default)]
pub struct ActivityPane {
    pub lines: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct BtwPane {
    pub notes: Vec<String>,
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
    let lines: Vec<Line> = pane
        .notes
        .iter()
        .map(|n| {
            Line::from(Span::styled(
                format!(" • {n}"),
                Style::default().fg(theme.text),
            ))
        })
        .collect();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" btw ")
        .border_style(Style::default().fg(theme.border));
    f.render_widget(Paragraph::new(lines).block(block), area);
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
