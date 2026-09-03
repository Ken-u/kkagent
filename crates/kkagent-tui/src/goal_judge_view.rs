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

/// Render the full-screen judge window (records + discussion log). It fills
/// the whole message area exactly like the BTW workspace and shares the
/// standard `window_block` chrome. The composer lives in the main input box
/// below (`judge > ` prefix, accent border) while the panel is open; typing
/// goes to the `session.goal` `discuss` RPC and replies stream back as
/// `AgentEvent::GoalJudgeChat`.
pub fn render_judge_panel(
    f: &mut Frame,
    area: Rect,
    records: &[GoalJudgeRecordView],
    chat: &[JudgeChatEntry],
    chat_pending: bool,
    theme: &Theme,
) {
    f.render_widget(Clear, area);
    let block = crate::panes::window_block(
        " Goal judge · discuss acceptance criteria (esc closes, enter sends) ",
        theme,
    );
    f.render_widget(&block, area);
    let inner = block.inner(area);

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
    if chat_pending {
        if !lines.is_empty() {
            lines.push(Line::from(Span::raw("")));
        }
        lines.push(Line::from(Span::styled(
            "● judge thinking…",
            Style::default().fg(theme.text_muted),
        )));
    }
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    f.render_widget(paragraph, inner);
}
