//! TUI side of the goal completion judge: record views + the popup panel
//! opened by clicking the footer goal chip or pressing Ctrl+J. The panel
//! doubles as the judge discussion window: typed input goes to the
//! `session.goal` `discuss` RPC and the judge's reply streams back as
//! `AgentEvent::GoalJudgeChat`.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::theme::Theme;

/// One judge verdict as delivered by `AgentEvent::GoalJudge`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalJudgeRecordView {
    /// "approve" | "reject" | "accepted_unreviewed"
    pub verdict: String,
    pub gaps: Vec<String>,
    pub summary: String,
    pub model: String,
}

/// One discussion exchange as delivered to / from the `discuss` RPC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JudgeChatEntry {
    /// Outgoing user message (echoed locally when submitted).
    User(String),
    /// Judge reply; `Some(note)` when the criterion was updated.
    Judge { text: String, note: Option<String> },
    /// Submission error (delivery failed).
    Error(String),
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

/// Render the centered judge window (records + discussion composer). Returns
/// the panel rect so the caller can route clicks / close keys while open.
pub fn render_judge_panel(
    f: &mut Frame,
    area: Rect,
    records: &[GoalJudgeRecordView],
    chat: &[JudgeChatEntry],
    chat_input: &str,
    chat_pending: bool,
    theme: &Theme,
) {
    let width = area.width.saturating_sub(10).clamp(40, 76);
    let height = area.height.saturating_sub(6).clamp(9, 24);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let rect = Rect::new(x, y, width, height);

    f.render_widget(Clear, rect);
    let block = ratatui::widgets::Block::default()
        .borders(ratatui::widgets::Borders::ALL)
        .title(" Goal judge · discuss acceptance criteria (esc closes, enter sends) ")
        .border_style(Style::default().fg(theme.accent));
    let inner = block.inner(rect);

    // Bottom composer line(s) + status line; log gets the rest.
    let wrap_width = width.saturating_sub(2) as usize;
    let input_rows = chat_input
        .chars()
        .count()
        .checked_div(wrap_width)
        .map(|rows| (rows + 1).min(3))
        .unwrap_or(1);
    let composer_height = (1 + input_rows as u16).min(4).min(inner.height);
    let status_height = 1.min(inner.height.saturating_sub(composer_height));
    let log_height = inner.height.saturating_sub(composer_height + status_height);
    let log_rect = Rect::new(inner.x, inner.y, inner.width, log_height);
    let status_rect = Rect::new(inner.x, inner.y + log_height, inner.width, status_height);
    let composer_rect = Rect::new(
        inner.x,
        inner.y + log_height + status_height,
        inner.width,
        composer_height,
    );

    let mut lines: Vec<Line> = Vec::new();
    if records.is_empty() && chat.is_empty() {
        lines.push(Line::from(Span::styled(
            "No judge records or discussion yet.",
            Style::default().fg(theme.text_muted),
        )));
        lines.push(Line::from(Span::styled(
            "Discuss what \"done\" means for the current goal below; agreed criteria are \
             applied to future completion reviews ([goal] judge_enabled required).",
            Style::default().fg(theme.text_muted),
        )));
    }
    for (n, record) in records.iter().rev().enumerate() {
        let color = match record.verdict.as_str() {
            "approve" => theme.accent,
            "reject" => theme.error,
            _ => theme.text_muted,
        };
        if n > 0 || !chat.is_empty() {
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
    for entry in chat.iter() {
        lines.push(Line::from(Span::raw("")));
        match entry {
            JudgeChatEntry::User(text) => {
                lines.push(Line::from(Span::styled(
                    "you ›".to_string(),
                    Style::default()
                        .fg(theme.primary)
                        .add_modifier(Modifier::BOLD),
                )));
                for text in text.lines() {
                    lines.push(Line::from(Span::styled(
                        format!("  {text}"),
                        Style::default().fg(theme.text),
                    )));
                }
            }
            JudgeChatEntry::Judge { text, note } => {
                lines.push(Line::from(Span::styled(
                    "judge ›".to_string(),
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                )));
                for text in text.lines() {
                    lines.push(Line::from(Span::styled(
                        format!("  {text}"),
                        Style::default().fg(theme.text),
                    )));
                }
                if let Some(note) = note {
                    lines.push(Line::from(Span::styled(
                        format!("  [criterion updated: {note}]"),
                        Style::default().fg(theme.accent),
                    )));
                }
            }
            JudgeChatEntry::Error(text) => {
                lines.push(Line::from(Span::styled(
                    format!("  ! {text}"),
                    Style::default().fg(theme.error),
                )));
            }
        }
    }
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    f.render_widget(paragraph, log_rect);

    let status = if chat_pending {
        Span::styled(" judge thinking…", Style::default().fg(theme.text_muted))
    } else {
        Span::raw("")
    };
    f.render_widget(Paragraph::new(Line::from(status)), status_rect);

    let composer = Paragraph::new(format!("> {chat_input}")).style(Style::default().fg(theme.text));
    f.render_widget(composer, composer_rect);
}
