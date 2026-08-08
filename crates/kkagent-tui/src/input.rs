use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// 输入状态，支持多行与正确的字节级光标
#[derive(Debug, Clone)]
pub struct InputState {
    pub text: String,
    /// 光标在 `text` 中的字节位置，始终在 UTF-8 边界上
    pub cursor: usize,
    /// 渲染用的水平滚偏移（列数），用于处理极宽行
    pub horizontal_scroll: usize,
}

impl Default for InputState {
    fn default() -> Self {
        Self::new()
    }
}

impl InputState {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            horizontal_scroll: 0,
        }
    }

    pub fn insert_char(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            let prev = self.text[..self.cursor]
                .char_indices()
                .last()
                .map(|(i, c)| i + c.len_utf8())
                .unwrap_or(0);
            let start = self.text[..self.cursor]
                .char_indices()
                .last()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.text.replace_range(start..prev, "");
            self.cursor = start;
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.text.len() {
            let next = self.text[self.cursor..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| self.cursor + i)
                .unwrap_or(self.text.len());
            self.text.replace_range(self.cursor..next, "");
        }
    }

    pub fn move_left(&mut self) {
        if self.cursor > 0 {
            self.cursor = self.text[..self.cursor]
                .char_indices()
                .last()
                .map(|(i, _)| i)
                .unwrap_or(0);
        }
    }

    pub fn move_right(&mut self) {
        if self.cursor < self.text.len() {
            self.cursor = self.text[self.cursor..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| self.cursor + i)
                .unwrap_or(self.text.len());
        }
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
    }

    pub fn move_down(&mut self) {
        let (line_start, col) = self.current_line_start_and_col();
        if let Some(next_line_start) = self.text[line_start..].find('\n').map(|i| line_start + i + 1) {
            self.cursor = byte_offset_at_col(&self.text, next_line_start, col);
        } else {
            self.cursor = self.text.len();
        }
    }

    pub fn move_home(&mut self) {
        self.cursor = self.current_line_start_and_col().0;
    }

    pub fn move_end(&mut self) {
        let (line_start, _) = self.current_line_start_and_col();
        self.cursor = self.text[line_start..]
            .find('\n')
            .map(|i| line_start + i)
            .unwrap_or(self.text.len());
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.horizontal_scroll = 0;
    }

    pub fn set_text(&mut self, text: String) {
        self.cursor = text.len();
        self.text = text;
        self.horizontal_scroll = 0;
    }

    pub fn take(&mut self) -> String {
        let text = std::mem::take(&mut self.text);
        self.cursor = 0;
        self.horizontal_scroll = 0;
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
        // cursor at end (after g), col=4
        s.move_up();
        assert_eq!(s.cursor, 3); // after c
        s.move_down();
        // from col=3 on "defg" lands after f (byte 7)
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
}
