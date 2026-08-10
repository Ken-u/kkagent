//! Kimi-style large paste folding: collapse long pastes into `[Pasted text #n]` markers.

use std::collections::HashMap;

const DEFAULT_CHAR_THRESHOLD: usize = 1000;
const DEFAULT_LINE_THRESHOLD: usize = 15;
const MARKER_PREFIX: &str = "[Pasted text #";

fn char_threshold() -> usize {
    std::env::var("KKAGENT_PASTE_CHAR_THRESHOLD")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_CHAR_THRESHOLD)
}

fn line_threshold() -> usize {
    std::env::var("KKAGENT_PASTE_LINE_THRESHOLD")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_LINE_THRESHOLD)
}

pub fn normalize_pasted_text(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

pub fn count_text_lines(text: &str) -> usize {
    if text.is_empty() {
        return 1;
    }
    text.chars().filter(|&c| c == '\n').count() + 1
}

pub fn should_fold_pasted_text(text: &str) -> bool {
    let normalized = normalize_pasted_text(text);
    normalized.chars().count() >= char_threshold()
        || count_text_lines(&normalized) >= line_threshold()
}

pub fn build_pasted_text_placeholder(paste_id: u32, text: &str) -> String {
    let line_count = count_text_lines(text);
    if line_count <= 1 {
        format!("[Pasted text #{paste_id}]")
    } else {
        format!("[Pasted text #{paste_id} +{line_count} lines]")
    }
}

/// Find next `[Pasted text #N …]` marker starting at or after `from`.
/// Returns (start, end, id).
fn find_next_marker(text: &str, from: usize) -> Option<(usize, usize, u32)> {
    let bytes = text.as_bytes();
    let mut i = from;
    while i < text.len() {
        let rest = &text[i..];
        let rel = rest.find(MARKER_PREFIX)?;
        let start = i + rel;
        let after_hash = start + MARKER_PREFIX.len();
        if after_hash >= text.len() {
            return None;
        }
        let Some((_, id_end)) = text[after_hash..]
            .char_indices()
            .take_while(|(_, c)| c.is_ascii_digit())
            .map(|(idx, c)| (idx, after_hash + idx + c.len_utf8()))
            .last()
        else {
            i = after_hash;
            continue;
        };
        if id_end == after_hash {
            i = after_hash;
            continue;
        }
        let Ok(id) = text[after_hash..id_end].parse::<u32>() else {
            i = start + 1;
            continue;
        };
        let mut j = id_end;
        // Optional ` +N lines` / ` +N line`
        if text[j..].starts_with(" +") {
            let num_start = j + 2;
            let num_end = text[num_start..]
                .char_indices()
                .take_while(|(_, c)| c.is_ascii_digit())
                .last()
                .map(|(idx, c)| num_start + idx + c.len_utf8())
                .unwrap_or(num_start);
            if num_end > num_start {
                j = num_end;
                if text[j..].starts_with(" lines") {
                    j += " lines".len();
                } else if text[j..].starts_with(" line") {
                    j += " line".len();
                } else {
                    i = start + 1;
                    continue;
                }
            } else {
                i = start + 1;
                continue;
            }
        }
        if j < text.len() && bytes[j] == b']' {
            return Some((start, j + 1, id));
        }
        i = start + 1;
    }
    None
}

#[derive(Debug, Default, Clone)]
pub struct PastePlaceholders {
    entries: HashMap<u32, String>,
    next_id: u32,
}

impl PastePlaceholders {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            next_id: 1,
        }
    }

    /// Fold large paste into a marker, or return the original text when below threshold.
    pub fn maybe_fold(&mut self, text: &str) -> String {
        let normalized = normalize_pasted_text(text);
        if !should_fold_pasted_text(&normalized) {
            return normalized;
        }
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let token = build_pasted_text_placeholder(id, &normalized);
        self.entries.insert(id, normalized);
        token
    }

    pub fn get(&self, id: u32) -> Option<&str> {
        self.entries.get(&id).map(|s| s.as_str())
    }

    /// Expand all known paste markers in `text` to their stored content.
    pub fn expand(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut last = 0;
        while let Some((start, end, id)) = find_next_marker(text, last) {
            out.push_str(&text[last..start]);
            if let Some(full) = self.entries.get(&id) {
                out.push_str(full);
            } else {
                out.push_str(&text[start..end]);
            }
            last = end;
        }
        out.push_str(&text[last..]);
        out
    }

    /// If `cursor` sits inside a paste marker that we know, return (start, end, id).
    pub fn marker_at_cursor(&self, text: &str, cursor: usize) -> Option<(usize, usize, u32)> {
        let cursor = cursor.min(text.len());
        let mut from = 0;
        while let Some((start, end, id)) = find_next_marker(text, from) {
            if cursor >= start && cursor <= end && self.entries.contains_key(&id) {
                return Some((start, end, id));
            }
            if start >= cursor {
                break;
            }
            from = end;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_by_line_count() {
        let mut p = PastePlaceholders::new();
        let text = format!("{}line", "line\n".repeat(14)); // exactly 15 lines
        assert_eq!(count_text_lines(&text), 15);
        let token = p.maybe_fold(&text);
        assert!(token.starts_with("[Pasted text #1"));
        assert!(token.contains("+15 lines"));
        assert_eq!(p.expand(&token), normalize_pasted_text(&text));
    }

    #[test]
    fn keeps_short_paste() {
        let mut p = PastePlaceholders::new();
        let text = "hello\nworld";
        assert_eq!(p.maybe_fold(text), text);
        assert!(p.entries.is_empty());
    }

    #[test]
    fn marker_at_cursor_detects() {
        let mut p = PastePlaceholders::new();
        let token = p.maybe_fold(&"x\n".repeat(20));
        let text = format!("before {token} after");
        let start = text.find('[').unwrap();
        assert!(p.marker_at_cursor(&text, start + 3).is_some());
        assert!(p.marker_at_cursor(&text, 0).is_none());
    }

    #[test]
    fn expand_preserves_unknown_marker() {
        let p = PastePlaceholders::new();
        let text = "see [Pasted text #99 +3 lines] ok";
        assert_eq!(p.expand(text), text);
    }
}
