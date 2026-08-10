//! Per-tool chip / summary / truncate render helpers (ref `tool-renderers`).

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::DisplayToolCall;
use crate::theme::Theme;

pub struct ToolRenderRegistry;

impl ToolRenderRegistry {
    pub fn chip_label(tc: &DisplayToolCall, width: u16) -> String {
        // Leave room for "● ✓ " / status prefix (~6 cols).
        let budget = (width as usize).saturating_sub(6).max(24);
        match tc.name.as_str() {
            "Bash" => format!("$ {}", fit(&tc.input_summary, budget.saturating_sub(2))),
            "Read" | "Write" | "Edit" => format!(
                "{} {}",
                tc.name,
                fit(&tc.input_summary, budget.saturating_sub(tc.name.len() + 1))
            ),
            "Grep" => format!("grep {}", fit(&tc.input_summary, budget.saturating_sub(5))),
            "Glob" => format!("glob {}", fit(&tc.input_summary, budget.saturating_sub(5))),
            "Skill" => format!("Skill {}", fit(&tc.input_summary, budget.saturating_sub(6))),
            "CreateGoal" | "GetGoal" | "UpdateGoal" | "SetGoalBudget" => {
                format!("goal {}", fit(&tc.input_summary, budget.saturating_sub(5)))
            }
            "WebSearch" | "FetchURL" => format!(
                "{} {}",
                tc.name,
                fit(&tc.input_summary, budget.saturating_sub(tc.name.len() + 1))
            ),
            other => format!(
                "{} {}",
                other,
                fit(&tc.input_summary, budget.saturating_sub(other.len() + 1))
            ),
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
                "Skill" => theme.primary,
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
            "ReadMediaFile" => lines.extend(media_summary(output, width, theme, max_preview)),
            "Skill" => lines.extend(skill_summary(output, width, theme, max_preview)),
            "CreateGoal" | "GetGoal" | "UpdateGoal" | "SetGoalBudget" => {
                lines.extend(goal_summary(output, width, theme, max_preview))
            }
            _ => lines.extend(default_summary(output, width, theme, max_preview)),
        }
        lines
    }
}

/// Fit to display columns (not char count) so CJK / wide glyphs don't overrun.
fn fit(s: &str, max_cols: usize) -> String {
    if max_cols == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut w = 0usize;
    for ch in s.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if w + cw > max_cols {
            if max_cols >= 1 && UnicodeWidthStr::width(out.as_str()) < max_cols {
                out.push('…');
            }
            break;
        }
        out.push(ch);
        w += cw;
    }
    out
}

fn push_wrapped_output_line(lines: &mut Vec<Line<'static>>, raw: &str, width: u16, style: Style) {
    let avail = (width as usize).saturating_sub(2).max(8);
    for chunk in wrap_cols(raw, avail) {
        lines.push(Line::from(Span::styled(format!("  {chunk}"), style)));
    }
}

fn wrap_cols(s: &str, max_width: usize) -> Vec<String> {
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

fn default_summary(
    output: &str,
    width: u16,
    theme: &Theme,
    max_preview: usize,
) -> Vec<Line<'static>> {
    let all: Vec<&str> = output.lines().collect();
    let show = all.len().min(max_preview);
    let mut lines = Vec::new();
    let style = Style::default().fg(theme.text_muted);
    for l in &all[..show] {
        push_wrapped_output_line(&mut lines, l, width, style);
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
    let style = Style::default().fg(theme.text_muted);
    for l in &all[..show] {
        push_wrapped_output_line(&mut lines, l, width, style);
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
    let style = Style::default().fg(theme.text_muted);
    for l in &all[..show] {
        push_wrapped_output_line(&mut lines, l, width, style);
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
        let style = if l.starts_with('+') {
            Style::default().fg(theme.success)
        } else if l.starts_with('-') {
            Style::default().fg(theme.error)
        } else {
            Style::default().fg(theme.text_muted)
        };
        push_wrapped_output_line(&mut lines, l, width, style);
    }
    if all.len() > max_preview {
        lines.push(truncate_hint(all.len() - max_preview, theme));
    }
    lines
}

fn media_summary(
    output: &str,
    width: u16,
    theme: &Theme,
    max_preview: usize,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    lines.extend(default_summary(output, width, theme, max_preview.min(6)));
    lines
}

fn goal_summary(output: &str, width: u16, theme: &Theme, max_preview: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    lines.extend(default_summary(output, width, theme, max_preview.min(8)));
    lines
}

/// Skill tool results are short (`loaded inline`); never dump legacy `# Skill:` bodies.
fn skill_summary(
    output: &str,
    width: u16,
    theme: &Theme,
    _max_preview: usize,
) -> Vec<Line<'static>> {
    let trimmed = output.trim();
    if trimmed.starts_with("# Skill:") || trimmed.starts_with("# Skill resource:") {
        let lines_n = output.lines().count();
        let name = trimmed
            .lines()
            .next()
            .unwrap_or("# Skill")
            .trim_start_matches('#')
            .trim();
        let mut lines = Vec::new();
        let style = Style::default().fg(theme.text_muted);
        push_wrapped_output_line(
            &mut lines,
            &format!("{name} loaded ({lines_n} lines hidden)"),
            width,
            style,
        );
        return lines;
    }
    default_summary(output, width, theme, 3)
}

fn truncate_hint(more: usize, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(
        format!("  … ({more} more lines, ctrl+o to expand)"),
        Style::default().fg(theme.text_muted),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chip_uses_available_width() {
        let tc = DisplayToolCall {
                            id: String::new(),
                            started_at: None,
                            stopping: false,
            name: "Bash".into(),
            input_summary: "x".repeat(200),
            output: None,
            is_error: false,
            collapsed: true,
        };
        let label = ToolRenderRegistry::chip_label(&tc, 80);
        assert!(UnicodeWidthStr::width(label.as_str()) <= 80);
        assert!(UnicodeWidthStr::width(label.as_str()) > 40);
    }
}
