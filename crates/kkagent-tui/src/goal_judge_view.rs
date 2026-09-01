//! TUI side of the goal completion judge: record views + the popup panel
//! opened by clicking the footer goal chip.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::theme::Theme;

/// One judge verdict as delivered by `AgentEvent::GoalJudge`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalJudgeRecordView {
    /// "approve" | "reject" | "failopen"
    pub verdict: String,
    pub gaps: Vec<String>,
    pub summary: String,
    pub model: String,
}

impl GoalJudgeRecordView {
    pub fn verdict_label(&self) -> String {
        match self.verdict.as_str() {
            "approve" => "approve ✓".to_string(),
            "reject" => "reject ✗".to_string(),
            other => other.to_string(),
        }
    }

    pub fn detail(&self) -> String {
        let mut detail = String::new();
        if !self.gaps.is_empty() {
            detail.push_str("gaps:\n");
            for (n, gap) in self.gaps.iter().enumerate() {
                detail.push_str(&format!("  {}. {gap}\n", n + 1));
            }
        }
        if !self.summary.is_empty() {
            detail.push_str(&self.summary);
        }
        detail
    }
}

/// Render the centered judge-records popup. Returns the panel rect so the
/// caller can route clicks / close keys while it is open.
pub fn render_judge_panel(
    f: &mut Frame,
    area: Rect,
    records: &[GoalJudgeRecordView],
    theme: &Theme,
) {
    let width = area.width.saturating_sub(10).clamp(30, 72);
    let height = area.height.saturating_sub(6).clamp(7, 20);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let rect = Rect::new(x, y, width, height);

    f.render_widget(Clear, rect);
    let block = ratatui::widgets::Block::default()
        .borders(ratatui::widgets::Borders::ALL)
        .title(" Goal judge records (esc to close) ")
        .border_style(Style::default().fg(theme.accent));
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    if records.is_empty() {
        let empty = Paragraph::new(
            "No judge records yet.\n\nThe completion judge runs only when [goal] judge_enabled \
             is set in the config; each review of a model-reported goal completion then lands \
             here.",
        )
        .style(Style::default().fg(theme.text_muted))
        .wrap(Wrap { trim: true });
        f.render_widget(empty, inner);
        return;
    }

    let mut lines: Vec<Line> = Vec::new();
    for (n, record) in records.iter().rev().enumerate() {
        let color = match record.verdict.as_str() {
            "approve" => theme.accent,
            "reject" => theme.error,
            _ => theme.text_muted,
        };
        if n > 0 {
            lines.push(Line::from(Span::raw("")));
        }
        lines.push(Line::from(vec![
            Span::styled(
                record.verdict_label(),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  ({})", record.model),
                Style::default().fg(theme.text_muted),
            ),
        ]));
        let detail = record.detail();
        for text in detail.lines() {
            lines.push(Line::from(Span::styled(
                text.to_string(),
                Style::default().fg(theme.text),
            )));
        }
    }
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    f.render_widget(paragraph, inner);
}
