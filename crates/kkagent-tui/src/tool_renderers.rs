//! Per-tool chip / summary / truncate render helpers (ref `tool-renderers`).

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use crate::app::DisplayToolCall;
use crate::theme::Theme;

pub struct ToolRenderRegistry;

impl ToolRenderRegistry {
    pub fn chip_label(tc: &DisplayToolCall) -> String {
        match tc.name.as_str() {
            "Bash" => format!("$ {}", short(&tc.input_summary, 48)),
            "Read" | "Write" | "Edit" => format!("{} {}", tc.name, short(&tc.input_summary, 56)),
            "Grep" => format!("grep {}", short(&tc.input_summary, 48)),
            "Glob" => format!("glob {}", short(&tc.input_summary, 48)),
            "CreateGoal" | "GetGoal" | "UpdateGoal" | "SetGoalBudget" => {
                format!("goal {}", short(&tc.input_summary, 40))
            }
            "WebSearch" | "FetchURL" => format!("{} {}", tc.name, short(&tc.input_summary, 40)),
            other => format!("{} {}", other, short(&tc.input_summary, 48)),
        }
    }

    pub fn chip_style(tc: &DisplayToolCall, theme: &Theme) -> Style {
        let fg = if tc.is_error {
            theme.error
        } else {
            match tc.name.as_str() {
                "Bash" => Color::Yellow,
                "Write" | "Edit" => Color::Magenta,
                "Read" | "Grep" | "Glob" => theme.text_dim,
                "CreateGoal" | "UpdateGoal" | "SetGoalBudget" | "GetGoal" => Color::Cyan,
                _ => theme.text_dim,
            }
        };
        Style::default().fg(fg)
    }

    pub fn summary_lines(
        tc: &DisplayToolCall,
        width: u16,
        theme: &Theme,
        max_preview: usize,
    ) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        let Some(ref output) = tc.output else {
            return lines;
        };
        match tc.name.as_str() {
            "Bash" => lines.extend(bash_summary(output, width, theme, max_preview)),
            "Grep" => lines.extend(grep_summary(output, width, theme, max_preview)),
            "Write" | "Edit" => lines.extend(diffish_summary(output, width, theme, max_preview)),
            _ => lines.extend(default_summary(output, width, theme, max_preview)),
        }
        lines
    }
}

fn short(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

fn default_summary(
    output: &str,
    width: u16,
    theme: &Theme,
    max_preview: usize,
) -> Vec<Line<'static>> {
    let all: Vec<&str> = output.lines().collect();
    let show = all.len().min(max_preview);
    let mut lines = Vec::new();
    for l in &all[..show] {
        let truncated = short(l, width.saturating_sub(4) as usize);
        lines.push(Line::from(Span::styled(
            format!("  {truncated}"),
            Style::default().fg(theme.text_muted),
        )));
    }
    if all.len() > max_preview {
        lines.push(truncate_hint(all.len() - max_preview, theme));
    }
    lines
}

fn bash_summary(output: &str, width: u16, theme: &Theme, max_preview: usize) -> Vec<Line<'static>> {
    let all: Vec<&str> = output.lines().collect();
    let show = all.len().min(max_preview);
    let mut lines = Vec::new();
    for l in &all[..show] {
        let truncated = short(l, width.saturating_sub(4) as usize);
        let fg = if l.starts_with("error:") || l.contains("FAILED") {
            theme.error
        } else {
            Color::Yellow
        };
        lines.push(Line::from(Span::styled(
            format!("  {truncated}"),
            Style::default().fg(fg),
        )));
    }
    if all.len() > max_preview {
        lines.push(truncate_hint(all.len() - max_preview, theme));
    }
    lines
}

fn grep_summary(output: &str, width: u16, theme: &Theme, max_preview: usize) -> Vec<Line<'static>> {
    let all: Vec<&str> = output.lines().collect();
    let show = all.len().min(max_preview);
    let mut lines = Vec::new();
    for l in &all[..show] {
        let truncated = short(l, width.saturating_sub(4) as usize);
        lines.push(Line::from(Span::styled(
            format!("  {truncated}"),
            Style::default().fg(Color::Green),
        )));
    }
    if all.len() > max_preview {
        lines.push(truncate_hint(all.len() - max_preview, theme));
    }
    lines
}

fn diffish_summary(
    output: &str,
    width: u16,
    theme: &Theme,
    max_preview: usize,
) -> Vec<Line<'static>> {
    let all: Vec<&str> = output.lines().collect();
    let show = all.len().min(max_preview);
    let mut lines = Vec::new();
    for l in &all[..show] {
        let truncated = short(l, width.saturating_sub(4) as usize);
        let fg = if l.starts_with('+') {
            Color::Green
        } else if l.starts_with('-') {
            theme.error
        } else {
            theme.text_muted
        };
        lines.push(Line::from(Span::styled(
            format!("  {truncated}"),
            Style::default().fg(fg),
        )));
    }
    if all.len() > max_preview {
        lines.push(truncate_hint(all.len() - max_preview, theme));
    }
    lines
}

fn truncate_hint(more: usize, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(
        format!("  … ({more} more lines, ctrl+o to expand)"),
        Style::default().fg(theme.text_muted),
    ))
}
