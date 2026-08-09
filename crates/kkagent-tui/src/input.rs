use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::pi::{
    move_word_left, move_word_right, EditorSnapshot, KillRing, PasteBurst, UndoStack,
};

/// 输入状态，支持多行与正确的字节级光标（pi-tui editor 对齐）
#[derive(Debug)]
pub struct InputState {
    pub text: String,
    /// 光标在 `text` 中的字节位置，始终在 UTF-8 边界上
    pub cursor: usize,
    /// 渲染用的水平滚偏移（列数），用于处理极宽行
    pub horizontal_scroll: usize,
    pub kill_ring: KillRing,
    pub undo: UndoStack,
    pub paste: PasteBurst,
    last_was_kill: bool,
}

impl Default for InputState {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for InputState {
    fn clone(&self) -> Self {
        Self {
            text: self.text.clone(),
            cursor: self.cursor,
            horizontal_scroll: self.horizontal_scroll,
            kill_ring: KillRing::new(),
            undo: UndoStack::new(64),
            paste: PasteBurst::new(),
            last_was_kill: false,
        }
    }
}

impl InputState {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            horizontal_scroll: 0,
            kill_ring: KillRing::new(),
            undo: UndoStack::new(64),
            paste: PasteBurst::new(),
            last_was_kill: false,
        }
    }

    fn snapshot(&self) -> EditorSnapshot {
        EditorSnapshot {
            text: self.text.clone(),
            cursor: self.cursor,
        }
    }

    fn push_undo(&mut self) {
        let snap = self.snapshot();
        self.undo.push(snap);
        self.last_was_kill = false;
    }

    pub fn insert_str(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        self.push_undo();
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    pub fn insert_char(&mut self, c: char) {
        self.push_undo();
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.push_undo();
            let start = self.text[..self.cursor]
                .char_indices()
                .last()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.text.replace_range(start..self.cursor, "");
            self.cursor = start;
        }
        self.last_was_kill = false;
    }

    pub fn delete(&mut self) {
        if self.cursor < self.text.len() {
            self.push_undo();
            let next = self.text[self.cursor..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| self.cursor + i)
                .unwrap_or(self.text.len());
            self.text.replace_range(self.cursor..next, "");
        }
        self.last_was_kill = false;
    }

    pub fn move_left(&mut self) {
        if self.cursor > 0 {
            self.cursor = self.text[..self.cursor]
                .char_indices()
                .last()
                .map(|(i, _)| i)
                .unwrap_or(0);
        }
        self.last_was_kill = false;
    }

    pub fn move_right(&mut self) {
        if self.cursor < self.text.len() {
            self.cursor = self.text[self.cursor..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| self.cursor + i)
                .unwrap_or(self.text.len());
        }
        self.last_was_kill = false;
    }

    pub fn move_word_left(&mut self) {
        self.cursor = move_word_left(&self.text, self.cursor);
        self.last_was_kill = false;
    }

    pub fn move_word_right(&mut self) {
        self.cursor = move_word_right(&self.text, self.cursor);
        self.last_was_kill = false;
    }

    pub fn move_up(&mut self) {
        let (line_start, col) = self.current_line_start_and_col();
        if line_start == 0 {
            self.cursor = 0;
            return;
        }
        let prev_line_end = line_start - 1; // skip '\n'
        let prev_line_start = self.text[..prev_line_end]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        self.cursor = byte_offset_at_col(&self.text, prev_line_start, col);
        self.last_was_kill = false;
    }

    pub fn move_down(&mut self) {
        let (line_start, col) = self.current_line_start_and_col();
        if let Some(next_line_start) = self.text[line_start..].find('\n').map(|i| line_start + i + 1) {
            self.cursor = byte_offset_at_col(&self.text, next_line_start, col);
        } else {
            self.cursor = self.text.len();
        }
        self.last_was_kill = false;
    }

    pub fn move_home(&mut self) {
        self.cursor = self.current_line_start_and_col().0;
        self.last_was_kill = false;
    }

    pub fn move_end(&mut self) {
        let (line_start, _) = self.current_line_start_and_col();
        self.cursor = self.text[line_start..]
            .find('\n')
            .map(|i| line_start + i)
            .unwrap_or(self.text.len());
        self.last_was_kill = false;
    }

    /// Kill from cursor to end of line (Ctrl-K).
    pub fn kill_line(&mut self) {
        let end = {
            let (line_start, _) = self.current_line_start_and_col();
            self.text[line_start..]
                .find('\n')
                .map(|i| line_start + i)
                .unwrap_or(self.text.len())
        };
        if self.cursor >= end {
            // at EOL: kill the newline if present
            if self.cursor < self.text.len() && self.text.as_bytes()[self.cursor] == b'\n' {
                self.push_undo();
                self.kill_ring
                    .push("\n", false, self.last_was_kill);
                self.text.replace_range(self.cursor..self.cursor + 1, "");
                self.last_was_kill = true;
            }
            return;
        }
        self.push_undo();
        let killed = self.text[self.cursor..end].to_string();
        self.kill_ring
            .push(&killed, false, self.last_was_kill);
        self.text.replace_range(self.cursor..end, "");
        self.last_was_kill = true;
    }

    /// Kill previous word (Ctrl-W).
    pub fn kill_word(&mut self) {
        let start = move_word_left(&self.text, self.cursor);
        if start >= self.cursor {
            return;
        }
        self.push_undo();
        let killed = self.text[start..self.cursor].to_string();
        self.kill_ring
            .push(&killed, true, self.last_was_kill);
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
        self.last_was_kill = true;
    }

    pub fn yank(&mut self) {
        if let Some(text) = self.kill_ring.yank() {
            self.push_undo();
            self.text.insert_str(self.cursor, &text);
            self.cursor += text.len();
        }
        self.last_was_kill = false;
    }

    pub fn undo_edit(&mut self) -> bool {
        let current = self.snapshot();
        if let Some(prev) = self.undo.undo(current) {
            self.text = prev.text;
            self.cursor = prev.cursor.min(self.text.len());
            self.last_was_kill = false;
            true
        } else {
            false
        }
    }

    pub fn redo_edit(&mut self) -> bool {
        let current = self.snapshot();
        if let Some(next) = self.undo.redo(current) {
            self.text = next.text;
            self.cursor = next.cursor.min(self.text.len());
            self.last_was_kill = false;
            true
        } else {
            false
        }
    }

    /// Buffer a paste chunk; flush when debounce window elapses.
    pub fn paste_chunk(&mut self, chunk: &str) {
        self.paste.push(chunk);
    }

    pub fn flush_paste(&mut self) -> bool {
        if let Some(s) = self.paste.take() {
            self.insert_str(&s);
            true
        } else {
            false
        }
    }

    pub fn force_flush_paste(&mut self) {
        let s = self.paste.force_take();
        if !s.is_empty() {
            self.insert_str(&s);
        }
    }

    pub fn clear(&mut self) {
        if !self.text.is_empty() {
            self.push_undo();
        }
        self.text.clear();
        self.cursor = 0;
        self.horizontal_scroll = 0;
        self.last_was_kill = false;
    }

    pub fn set_text(&mut self, text: String) {
        self.push_undo();
        self.cursor = text.len();
        self.text = text;
        self.horizontal_scroll = 0;
        self.last_was_kill = false;
    }

    pub fn take(&mut self) -> String {
        let text = std::mem::take(&mut self.text);
        self.cursor = 0;
        self.horizontal_scroll = 0;
        self.last_was_kill = false;
        text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// 当前所在行的起始字节位置与光标在该行的显示列宽
    fn current_line_start_and_col(&self) -> (usize, usize) {
        let line_start = self.text[..self.cursor]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let col = UnicodeWidthStr::width(&self.text[line_start..self.cursor]);
        (line_start, col)
    }
}

/// 从 `line_start` 开始找到第 `target_col` 显示列所在的字节位置
fn byte_offset_at_col(text: &str, line_start: usize, target_col: usize) -> usize {
    let line = &text[line_start..];
    let mut col = 0;
    for (i, c) in line.char_indices() {
        let w = c.width().unwrap_or(0);
        if col + w > target_col {
            return line_start + i;
        }
        col += w;
        if c == '\n' {
            return line_start + i;
        }
    }
    text.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_navigation() {
        let mut s = InputState::new();
        s.set_text("hello".to_string());
        s.move_left();
        assert_eq!(s.cursor, 4);
        s.move_left();
        assert_eq!(s.cursor, 3);
        s.move_right();
        assert_eq!(s.cursor, 4);
    }

    #[test]
    fn multiline_up_down() {
        let mut s = InputState::new();
        s.set_text("abc\ndefg".to_string());
        s.move_up();
        assert_eq!(s.cursor, 3);
        s.move_down();
        assert_eq!(s.cursor, 7);
    }

    #[test]
    fn delete_char() {
        let mut s = InputState::new();
        s.set_text("abc".to_string());
        s.move_left();
        s.delete();
        assert_eq!(s.text, "ab");
        assert_eq!(s.cursor, 2);
    }

    #[test]
    fn kill_yank_undo() {
        let mut s = InputState::new();
        s.set_text("hello world".to_string());
        s.cursor = 5;
        s.kill_line();
        assert_eq!(s.text, "hello");
        s.yank();
        assert_eq!(s.text, "hello world");
        assert!(s.undo_edit());
    }
}
