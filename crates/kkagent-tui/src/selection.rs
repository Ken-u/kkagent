//! In-app transcript text selection (mouse capture stays on).
//!
//! Coordinates are absolute visual-line indices into the rendered transcript
//! (same order as `build_transcript_lines`) plus display columns (Unicode width).

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// One cell in the rendered transcript (visual line + display column).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CellPos {
    pub line: usize,
    /// Display column (0-based), counting double-width CJK as 2.
    pub col: u16,
}

/// Active drag / completed selection. Range is half-open `[anchor, focus)` in
/// document order after [`TextSelection::normalized`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextSelection {
    pub anchor: CellPos,
    pub focus: CellPos,
}

impl TextSelection {
    pub fn new(at: CellPos) -> Self {
        Self {
            anchor: at,
            focus: at,
        }
    }

    pub fn normalized(self) -> (CellPos, CellPos) {
        if self.anchor <= self.focus {
            (self.anchor, self.focus)
        } else {
            (self.focus, self.anchor)
        }
    }

    pub fn is_empty(self) -> bool {
        self.anchor == self.focus
    }
}

/// Parallel to a rendered visual line: plain text + where copyable content starts.
#[derive(Debug, Clone)]
pub struct SelectRow {
    pub plain: String,
    /// Display columns before copyable body (role markers / indent chrome).
    pub content_col: u16,
}

impl SelectRow {
    pub fn from_line(line: &Line<'_>) -> Self {
        let plain: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        let content_col = line
            .spans
            .first()
            .map(|s| {
                let t = s.content.as_ref();
                if is_chrome_prefix(t) {
                    UnicodeWidthStr::width(t) as u16
                } else {
                    0
                }
            })
            .unwrap_or(0);
        Self { plain, content_col }
    }

    pub fn width(&self) -> u16 {
        UnicodeWidthStr::width(self.plain.as_str()) as u16
    }
}

fn is_chrome_prefix(s: &str) -> bool {
    matches!(
        s,
        "✦ " | "● " | "○ " | "  " | "└ " | "├ " | "│ " | "▸ " | "• "
    ) || (s.len() <= 4 && s.chars().all(|c| c == '─' || c == ' '))
}

/// Build select rows matching rendered lines 1:1.
pub fn rows_from_lines(lines: &[Line<'_>]) -> Vec<SelectRow> {
    lines.iter().map(SelectRow::from_line).collect()
}

/// Apply selection highlight to rendered lines (in place).
pub fn apply_highlight(lines: &mut [Line<'static>], sel: TextSelection, style: Style) {
    if sel.is_empty() {
        return;
    }
    let (lo, hi) = sel.normalized();
    for (i, line) in lines.iter_mut().enumerate() {
        if i < lo.line || i > hi.line {
            continue;
        }
        let start = if i == lo.line { lo.col as usize } else { 0 };
        let end = if i == hi.line {
            hi.col as usize
        } else {
            usize::MAX
        };
        if start >= end {
            continue;
        }
        *line = highlight_columns(std::mem::take(line), start, end, style);
    }
}

fn highlight_columns(
    line: Line<'static>,
    start_col: usize,
    end_col: usize,
    sel_style: Style,
) -> Line<'static> {
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut col = 0usize;
    for span in line.spans {
        let text = span.content.as_ref();
        if text.is_empty() {
            out.push(span);
            continue;
        }
        let span_style = span.style;
        let mut buf = String::new();
        let mut buf_selected = false;
        let flush = |buf: &mut String, selected: bool, out: &mut Vec<Span<'static>>| {
            if buf.is_empty() {
                return;
            }
            let style = if selected {
                merge_sel(span_style, sel_style)
            } else {
                span_style
            };
            out.push(Span::styled(std::mem::take(buf), style));
        };

        for ch in text.chars() {
            let w = UnicodeWidthChar::width(ch).unwrap_or(0);
            let ch_start = col;
            let ch_end = col + w;
            let selected = ch_end > start_col && ch_start < end_col && w > 0;
            if buf.is_empty() {
                buf_selected = selected;
            } else if selected != buf_selected {
                flush(&mut buf, buf_selected, &mut out);
                buf_selected = selected;
            }
            buf.push(ch);
            col = ch_end;
        }
        flush(&mut buf, buf_selected, &mut out);
    }
    Line::from(out)
}

fn merge_sel(base: Style, sel: Style) -> Style {
    let mut out = base;
    if let Some(bg) = sel.bg {
        out = out.bg(bg);
    }
    if let Some(fg) = sel.fg {
        out = out.fg(fg);
    }
    out = out.add_modifier(sel.add_modifier);
    out = out.remove_modifier(sel.sub_modifier);
    out
}

/// Theme-aligned selection colors.
pub fn selection_style() -> Style {
    Style::default()
        .bg(Color::Rgb(0x2A, 0x4A, 0x6A))
        .fg(Color::Rgb(0xF5, 0xF5, 0xF5))
        .add_modifier(Modifier::BOLD)
}

/// Extract copyable plain text (no ANSI). Chrome prefixes are skipped.
pub fn extract_text(rows: &[SelectRow], sel: TextSelection) -> String {
    if sel.is_empty() || rows.is_empty() {
        return String::new();
    }
    let (lo, hi) = sel.normalized();
    if lo.line >= rows.len() {
        return String::new();
    }
    let last = hi.line.min(rows.len() - 1);
    let mut parts: Vec<String> = Vec::new();
    for (i, row) in rows.iter().enumerate().take(last + 1).skip(lo.line) {
        let row_w = row.width();
        let mut start = if i == lo.line { lo.col } else { 0 };
        let mut end = if i == hi.line { hi.col } else { row_w };
        start = start.max(row.content_col);
        end = end.min(row_w);
        if end <= start {
            parts.push(String::new());
        } else {
            parts.push(slice_by_columns(&row.plain, start as usize, end as usize));
        }
    }
    parts.join("\n")
}

pub fn select_by_click(rows: &[SelectRow], pos: CellPos, count: u8) -> TextSelection {
    match count {
        3 => select_line(rows, pos),
        2 => select_word(rows, pos),
        _ => TextSelection::new(pos),
    }
}

fn select_word(rows: &[SelectRow], pos: CellPos) -> TextSelection {
    let Some(row) = rows.get(pos.line) else {
        return TextSelection::new(pos);
    };
    let text = &row.plain;
    let col = pos.col as usize;
    if text.is_empty() {
        return TextSelection::new(pos);
    }
    let mut char_indices: Vec<(usize, usize)> = Vec::new(); // (byte_start, col)
    let mut col_acc = 0usize;
    for (idx, ch) in text.char_indices() {
        char_indices.push((idx, col_acc));
        col_acc += UnicodeWidthChar::width(ch).unwrap_or(1);
    }
    // Find the character under the click.
    let mut idx_under = None;
    for (i, &(_byte_start, _ch_start)) in char_indices.iter().enumerate() {
        let ch_end = char_indices.get(i + 1).map(|(_, c)| *c).unwrap_or(col_acc);
        if col < ch_end {
            idx_under = Some(i);
            break;
        }
    }
    let idx_under = idx_under.unwrap_or(char_indices.len().saturating_sub(1));
    let is_word_char = |ch: char| ch.is_alphanumeric() || ch == '_' || ch > '\x7f';
    let ch_under = text.chars().nth(idx_under).unwrap_or(' ');
    if !is_word_char(ch_under) {
        // Punctuation: select just that character.
        let byte_start = char_indices[idx_under].0;
        let _byte_end = char_indices
            .get(idx_under + 1)
            .map(|(b, _)| *b)
            .unwrap_or(text.len());
        let before = &text[..byte_start];
        let start_col = before.width() as u16;
        let end_col = start_col + UnicodeWidthChar::width(ch_under).unwrap_or(1) as u16;
        return TextSelection {
            anchor: CellPos {
                line: pos.line,
                col: start_col,
            },
            focus: CellPos {
                line: pos.line,
                col: end_col,
            },
        };
    }
    // Find word boundaries.
    let mut start_idx = idx_under;
    while start_idx > 0
        && text
            .chars()
            .nth(start_idx - 1)
            .map(is_word_char)
            .unwrap_or(false)
    {
        start_idx -= 1;
    }
    let mut end_idx = idx_under;
    while end_idx + 1 < char_indices.len()
        && text
            .chars()
            .nth(end_idx + 1)
            .map(is_word_char)
            .unwrap_or(false)
    {
        end_idx += 1;
    }
    let start_byte = char_indices[start_idx].0;
    let end_byte = char_indices
        .get(end_idx + 1)
        .map(|(b, _)| *b)
        .unwrap_or(text.len());
    let start_col = text[..start_byte].width() as u16;
    let end_col = text[..end_byte].width() as u16;
    TextSelection {
        anchor: CellPos {
            line: pos.line,
            col: start_col,
        },
        focus: CellPos {
            line: pos.line,
            col: end_col,
        },
    }
}

fn select_line(rows: &[SelectRow], pos: CellPos) -> TextSelection {
    let Some(row) = rows.get(pos.line) else {
        return TextSelection::new(pos);
    };
    let start_col = row.content_col;
    let end_col = row.width();
    TextSelection {
        anchor: CellPos {
            line: pos.line,
            col: start_col,
        },
        focus: CellPos {
            line: pos.line,
            col: end_col,
        },
    }
}

fn slice_by_columns(s: &str, start_col: usize, end_col: usize) -> String {
    let mut col = 0usize;
    let mut out = String::new();
    for ch in s.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        let ch_start = col;
        let ch_end = col + w;
        if ch_end > start_col && ch_start < end_col {
            out.push(ch);
        }
        col = ch_end;
        if col >= end_col {
            break;
        }
    }
    out
}

/// Clamp selection into current row count after resize / rebuild.
pub fn clamp_selection(sel: TextSelection, row_count: usize) -> Option<TextSelection> {
    if row_count == 0 {
        return None;
    }
    let clamp_pos = |p: CellPos| -> CellPos {
        CellPos {
            line: p.line.min(row_count - 1),
            col: p.col,
        }
    };
    Some(TextSelection {
        anchor: clamp_pos(sel.anchor),
        focus: clamp_pos(sel.focus),
    })
}

/// OSC 52 clipboard write (SSH / tmux friendly). Never panics.
pub fn write_osc52(text: &str) -> std::io::Result<()> {
    use base64::Engine;
    use std::io::Write;

    // Many terminals / tmux cap OSC 52 around ~100KB of payload.
    const MAX_BYTES: usize = 75_000;
    let bytes = text.as_bytes();
    let slice = if bytes.len() > MAX_BYTES {
        // Avoid splitting a UTF-8 codepoint.
        let mut end = MAX_BYTES;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        &text[..end]
    } else {
        text
    };
    let b64 = base64::engine::general_purpose::STANDARD.encode(slice.as_bytes());
    let mut out = std::io::stdout();
    // tmux needs DCS wrapping so the OSC reaches the outer terminal.
    if std::env::var_os("TMUX").is_some() {
        write!(out, "\x1bPtmux;\x1b\x1b]52;c;{b64}\x07\x1b\\")?;
    } else {
        write!(out, "\x1b]52;c;{b64}\x07")?;
    }
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reverse_drag_normalizes() {
        let sel = TextSelection {
            anchor: CellPos { line: 2, col: 5 },
            focus: CellPos { line: 1, col: 1 },
        };
        let (lo, hi) = sel.normalized();
        assert_eq!(lo, CellPos { line: 1, col: 1 });
        assert_eq!(hi, CellPos { line: 2, col: 5 });
    }

    #[test]
    fn cjk_slice_by_columns() {
        let s = "你好ABC";
        // 你=2, 好=2, A=1,B=1,C=1
        assert_eq!(slice_by_columns(s, 0, 2), "你");
        assert_eq!(slice_by_columns(s, 2, 4), "好");
        assert_eq!(slice_by_columns(s, 4, 7), "ABC");
    }

    #[test]
    fn extract_skips_chrome_prefix() {
        let rows = vec![SelectRow {
            plain: "✦ hello".into(),
            content_col: 2,
        }];
        let sel = TextSelection {
            anchor: CellPos { line: 0, col: 0 },
            focus: CellPos { line: 0, col: 7 },
        };
        assert_eq!(extract_text(&rows, sel), "hello");
    }
}
