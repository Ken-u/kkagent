//! Per-tool chip / summary / truncate render helpers (ref `tool-renderers`).

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::app::DisplayToolCall;
use crate::theme::Theme;

pub struct ToolRenderRegistry;

impl ToolRenderRegistry {
    pub fn chip_label(tc: &DisplayToolCall, width: u16) -> String {
        // Leave room for the transcript bullet and status icon. Never invent
        // a minimum wider than the terminal: mobile SSH clients commonly
        // report widths in the 20–40 column range.
        let budget = (width as usize).saturating_sub(4);
        fit(&Self::chip_text(tc), budget)
    }

    /// Full (unfitted) chip text: `$ cmd`, `Read path`, `grep pattern`, …
    /// input_summary carries the command line / file path — sanitize once
    /// here rather than at each fit() call below.
    fn chip_text(tc: &DisplayToolCall) -> String {
        let input_summary = crate::sanitize::sanitize_text(&tc.input_summary);
        let input_summary = input_summary.as_ref();
        // ACP-delegated cards may carry no argument summary at all; avoid
        // dangling whitespace like "Read File " in that case.
        if input_summary.is_empty() {
            return match tc.name.as_str() {
                "Bash" => "$ (no input)".into(),
                other => other.to_string(),
            };
        }
        match tc.name.as_str() {
            "Bash" => format!("$ {input_summary}"),
            "Read" | "Write" | "Edit" => format!("{} {input_summary}", tc.name),
            "Grep" => format!("grep {input_summary}"),
            "Glob" => format!("glob {input_summary}"),
            "Skill" => format!("Skill {input_summary}"),
            "Goal" => format!("goal {input_summary}"),
            other => format!("{other} {input_summary}"),
        }
    }

    /// True when the chip cannot show the whole input within `width`
    /// (i.e. the label ends in an ellipsis and hides part of the command).
    pub fn chip_truncated(tc: &DisplayToolCall, width: u16) -> bool {
        let budget = (width as usize).saturating_sub(4);
        UnicodeWidthStr::width(Self::chip_text(tc).as_str()) > budget
    }

    /// The full command/input, wrapped — used by the expanded (ctrl+o /
    /// click) view so a long command is never lost to the chip ellipsis.
    pub fn full_input_lines(tc: &DisplayToolCall, width: u16, theme: &Theme) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        let style = Style::default().fg(theme.text_muted);
        push_wrapped_output_line(&mut lines, &Self::chip_text(tc), width, style);
        lines
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
                "Goal" => Color::Cyan,
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
        // Tool output is untrusted (build logs, grep hits, file contents):
        // strip ANSI/OSC sequences before any wrapping can split them.
        let output = crate::sanitize::sanitize_text(output);
        let output = output.as_ref();
        match tc.name.as_str() {
            "Bash" => lines.extend(bash_summary(output, width, theme, max_preview)),
            "Grep" => lines.extend(grep_summary(output, width, theme, max_preview)),
            "Write" | "Edit" => lines.extend(diffish_summary(output, width, theme, max_preview)),
            "ReadMediaFile" => lines.extend(media_summary(output, width, theme, max_preview)),
            "Skill" => lines.extend(skill_summary(output, width, theme, max_preview)),
            "Goal" => lines.extend(goal_summary(output, width, theme, max_preview)),
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
    for grapheme in s.graphemes(true) {
        let cw = UnicodeWidthStr::width(grapheme);
        if w + cw > max_cols {
            if max_cols >= 1 && UnicodeWidthStr::width(out.as_str()) < max_cols {
                out.push('…');
            }
            break;
        }
        out.push_str(grapheme);
        w += cw;
    }
    out
}

fn push_wrapped_output_line(lines: &mut Vec<Line<'static>>, raw: &str, width: u16, style: Style) {
    let prefix = if width >= 2 { "  " } else { "" };
    let avail = (width as usize)
        .saturating_sub(UnicodeWidthStr::width(prefix))
        .max(1);
    for chunk in wrap_cols(raw, avail) {
        lines.push(Line::from(Span::styled(format!("{prefix}{chunk}"), style)));
    }
}

fn wrap_cols(s: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return vec![s.to_string()];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    for grapheme in s.graphemes(true) {
        let w = UnicodeWidthStr::width(grapheme);
        if cur_w + w > max_width && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
            cur_w = 0;
        }
        cur.push_str(grapheme);
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
    lines.extend(default_summary(output, width, theme, max_preview));
    lines
}

fn goal_summary(output: &str, width: u16, theme: &Theme, max_preview: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    lines.extend(default_summary(output, width, theme, max_preview));
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
            user_overridden: false,
            queued_behind: None,
        };
        let label = ToolRenderRegistry::chip_label(&tc, 80);
        assert!(UnicodeWidthStr::width(label.as_str()) <= 80);
        assert!(UnicodeWidthStr::width(label.as_str()) > 40);
    }

    #[test]
    fn chip_never_invents_a_wider_mobile_budget() {
        let tc = DisplayToolCall {
            id: String::new(),
            started_at: None,
            stopping: false,
            name: "Bash".into(),
            input_summary: "cargo test --workspace --all-targets 你好".into(),
            output: None,
            is_error: false,
            collapsed: true,
            user_overridden: false,
            queued_behind: None,
        };
        for width in 4..48 {
            let label = ToolRenderRegistry::chip_label(&tc, width);
            assert!(
                UnicodeWidthStr::width(label.as_str()) <= (width as usize).saturating_sub(4),
                "width={width}, label={label:?}"
            );
        }
    }

    fn bash_tc(cmd: &str) -> DisplayToolCall {
        DisplayToolCall {
            id: String::new(),
            started_at: None,
            stopping: false,
            name: "Bash".into(),
            input_summary: cmd.into(),
            output: None,
            is_error: false,
            collapsed: true,
            user_overridden: false,
            queued_behind: None,
        }
    }

    #[test]
    fn chip_truncated_reports_hidden_command() {
        let long = bash_tc(&format!("echo {}", "x".repeat(200)));
        assert!(ToolRenderRegistry::chip_truncated(&long, 80));
        let short = bash_tc("ls");
        assert!(!ToolRenderRegistry::chip_truncated(&short, 80));
        // Narrow viewport hides even modest commands ("$ ls" needs 4 cols
        // plus the icon/bullet reserve, so budget 3 truncates it).
        assert!(ToolRenderRegistry::chip_truncated(&short, 7));
    }

    #[test]
    fn full_input_lines_wrap_without_overflow() {
        let tc = bash_tc(&format!("echo {}", "y".repeat(120)));
        let theme = Theme::default();
        let lines = ToolRenderRegistry::full_input_lines(&tc, 40, &theme);
        assert!(lines.len() > 1, "expected wrapping, got {lines:?}");
        let joined = lines
            .iter()
            .map(|l| l.to_string().trim_start().to_string())
            .collect::<Vec<_>>()
            .join("");
        assert!(
            joined.contains("$ echo "),
            "command text must be present: {joined:?}"
        );
        assert!(
            joined.ends_with(&"y".repeat(120)),
            "full command must be present: {joined:?}"
        );
        for l in &lines {
            assert!(
                UnicodeWidthStr::width(l.to_string().as_str()) <= 40,
                "line overflows: {l:?}"
            );
        }
    }
}
