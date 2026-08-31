use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::paste_placeholders::PastePlaceholders;
use crate::pi::{move_word_left, move_word_right, EditorSnapshot, KillRing, PasteBurst, UndoStack};

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
    /// Kimi-style folded paste payloads keyed by marker id.
    pub pastes: PastePlaceholders,
    /// 鼠标选区，归一化后的 `(start, end)` 字节区间（半开区间）。
    pub selection: Option<(usize, usize)>,
    /// 拖拽选区的锚点字节；`None` 表示当前没有在拖拽。
    pub selection_anchor: Option<usize>,
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
            pastes: self.pastes.clone(),
            selection: self.selection,
            selection_anchor: self.selection_anchor,
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
            pastes: PastePlaceholders::new(),
            selection: None,
            selection_anchor: None,
            last_was_kill: false,
        }
    }

    fn snapshot(&self) -> EditorSnapshot {
        EditorSnapshot {
            text: self.text.clone(),
            cursor: self.cursor,
        }
    }

    /// Clamp a byte offset onto UTF-8 boundaries and the text length.
    fn clamp_boundary(text: &str, at: usize) -> usize {
        let mut at = at.min(text.len());
        while at > 0 && !text.is_char_boundary(at) {
            at -= 1;
        }
        at
    }

    /// Move the caret without touching the selection (clicks start a fresh
    /// selection immediately afterwards).
    pub fn move_cursor_to(&mut self, at: usize) {
        self.cursor = Self::clamp_boundary(&self.text, at);
    }

    /// Start a mouse selection: drop any old selection, park the anchor and
    /// place the caret at the click position.
    pub fn begin_selection(&mut self, at: usize) {
        let at = Self::clamp_boundary(&self.text, at);
        self.selection_anchor = Some(at);
        self.selection = Some((at, at));
        self.cursor = at;
    }

    /// Extend the drag selection to `at` (anchor stays fixed).
    pub fn update_selection(&mut self, at: usize) {
        let Some(anchor) = self.selection_anchor else {
            return;
        };
        let at = Self::clamp_boundary(&self.text, at);
        self.selection = Some((anchor.min(at), anchor.max(at)));
        self.cursor = at;
    }

    /// Finish a drag: keep the selection when non-empty, otherwise leave just
    /// the caret at the click position (plain click behavior).
    pub fn end_selection(&mut self) {
        self.selection_anchor = None;
        if let Some((s, e)) = self.selection {
            if s >= e {
                self.selection = None;
                self.cursor = s;
            }
        }
    }

    pub fn clear_selection(&mut self) {
        self.selection = None;
        self.selection_anchor = None;
    }

    pub fn selection_active(&self) -> bool {
        self.selection.is_some_and(|(s, e)| s < e)
    }

    /// Copyable plain text of the active selection (`None` when empty).
    pub fn selected_text(&self) -> Option<String> {
        let (s, e) = self.selection?;
        (s < e).then(|| self.text[s..e].to_string())
    }

    /// Double click: select the word / CJK token under the byte offset.
    pub fn select_word_at(&mut self, at: usize) {
        let at = Self::clamp_boundary(&self.text, at);
        let (start, end) = word_boundaries(&self.text, at);
        self.selection = Some((start, end));
        self.selection_anchor = None;
        self.cursor = end;
    }

    /// Triple click: select the whole logical line under the byte offset.
    pub fn select_line_at(&mut self, at: usize) {
        let at = Self::clamp_boundary(&self.text, at);
        let start = self.text[..at].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let end = self.text[at..]
            .find('\n')
            .map(|i| at + i)
            .unwrap_or(self.text.len());
        self.selection = Some((start, end));
        self.selection_anchor = None;
        self.cursor = end;
    }

    /// Replace the active selection, moving the caret to the seam. Returns the
    /// number of bytes removed (0 when nothing was selected).
    fn delete_selection(&mut self) -> usize {
        let Some((s, e)) = self.selection else {
            return 0;
        };
        self.selection_anchor = None;
        if s >= e {
            self.selection = None;
            return 0;
        }
        let removed = e - s;
        self.text.replace_range(s..e, "");
        self.cursor = s;
        self.selection = None;
        removed
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
        self.delete_selection();
        self.push_undo();
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    pub fn insert_char(&mut self, c: char) {
        self.delete_selection();
        self.push_undo();
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    pub fn backspace(&mut self) {
        if self.selection_active() {
            self.push_undo();
            self.delete_selection();
            self.last_was_kill = false;
            return;
        }
        if self.cursor > 0 {
            self.push_undo();
            // Deleting a folded paste / image marker removes it whole —
            // chipping away "[Pasted text #1 +15 lines]" char by char is
            // pointless and would leave an orphaned entry behind.
            if let Some((start, end, id)) = self.pastes.marker_at_cursor(&self.text, self.cursor) {
                self.pastes.forget(id);
                self.text.replace_range(start..end, "");
                self.cursor = start;
                self.last_was_kill = false;
                return;
            }
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
        if self.selection_active() {
            self.push_undo();
            self.delete_selection();
            self.last_was_kill = false;
            return;
        }
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
        if let Some(next_line_start) = self.text[line_start..]
            .find('\n')
            .map(|i| line_start + i + 1)
        {
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

    /// Home 键：回到整个输入的开头（多行时也与 Ctrl-A 的行内跳转区分开）
    pub fn move_buffer_home(&mut self) {
        self.cursor = 0;
        self.last_was_kill = false;
    }

    /// End 键：跳到整个输入的末尾
    pub fn move_buffer_end(&mut self) {
        self.cursor = self.text.len();
        self.last_was_kill = false;
    }

    /// Kill from cursor to end of line (Ctrl-K).
    pub fn kill_line(&mut self) {
        if self.selection_active() {
            self.push_undo();
            self.delete_selection();
            self.last_was_kill = false;
            return;
        }
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
                self.kill_ring.push("\n", false, self.last_was_kill);
                self.text.replace_range(self.cursor..self.cursor + 1, "");
                self.last_was_kill = true;
            }
            return;
        }
        self.push_undo();
        let killed = self.text[self.cursor..end].to_string();
        self.kill_ring.push(&killed, false, self.last_was_kill);
        self.text.replace_range(self.cursor..end, "");
        self.last_was_kill = true;
    }

    /// Kill previous word (Ctrl-W).
    pub fn kill_word(&mut self) {
        if self.selection_active() {
            self.push_undo();
            self.delete_selection();
            self.last_was_kill = false;
            return;
        }
        let start = move_word_left(&self.text, self.cursor);
        if start >= self.cursor {
            return;
        }
        self.push_undo();
        let killed = self.text[start..self.cursor].to_string();
        self.kill_ring.push(&killed, true, self.last_was_kill);
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
        self.last_was_kill = true;
    }

    pub fn yank(&mut self) {
        if let Some(text) = self.kill_ring.yank() {
            self.delete_selection();
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
            self.clear_selection();
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
            self.clear_selection();
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

    pub fn flush_paste(&mut self, fold: bool) -> bool {
        if let Some(s) = self.paste.take() {
            self.insert_paste(&s, fold);
            true
        } else {
            false
        }
    }

    pub fn force_flush_paste(&mut self, fold: bool) {
        let s = self.paste.force_take();
        if !s.is_empty() {
            self.insert_paste(&s, fold);
        }
    }

    /// Insert clipboard/bracketed paste. Large pastes fold into a one-line overview
    /// when `fold` is true (agent/plan mode). If the cursor sits on an existing
    /// paste marker, expand that marker instead (kimi second-paste behavior).
    pub fn insert_paste(&mut self, raw: &str, fold: bool) {
        if raw.is_empty() {
            return;
        }
        if self.expand_paste_marker_at_cursor() {
            return;
        }
        // Pasted bytes are external input (clipboard, terminal); strip escape
        // sequences before they can be stored and later rendered verbatim.
        let raw = crate::sanitize::sanitize_text(raw);
        let raw = raw.as_ref();
        let insert = if fold {
            self.pastes.maybe_fold(raw)
        } else {
            crate::paste_placeholders::normalize_pasted_text(raw)
        };
        self.insert_str(&insert);
    }

    /// Expand the paste marker under the cursor back to full text.
    pub fn expand_paste_marker_at_cursor(&mut self) -> bool {
        let Some((start, end, id)) = self.pastes.marker_at_cursor(&self.text, self.cursor) else {
            return false;
        };
        let Some(full) = self.pastes.get(id).map(|s| s.to_string()) else {
            return false;
        };
        self.replace_range(start, end, &full);
        true
    }

    /// Expand folded paste markers for submit / history display.
    pub fn expand_pastes(&self, text: &str) -> String {
        self.pastes.expand(text)
    }

    /// Register a pasted image and insert its `[Image-N]` marker. The marker
    /// expands back to `@<relative path>` on submit, so the composer stays
    /// readable instead of showing the full attachments path.
    pub fn insert_image_mention(&mut self, at_path: &str) {
        let id = self.pastes.next_image_id();
        self.pastes.store_image(id, at_path.to_string());
        let marker = format!("[Pasted Image #{id}]");
        self.replace_range(self.cursor, self.cursor, &marker);
    }

    pub fn clear(&mut self) {
        self.clear_selection();
        if !self.text.is_empty() {
            self.push_undo();
        }
        self.text.clear();
        self.cursor = 0;
        self.horizontal_scroll = 0;
        self.last_was_kill = false;
    }

    pub fn set_text(&mut self, text: String) {
        self.clear_selection();
        self.push_undo();
        self.cursor = text.len();
        self.text = text;
        self.horizontal_scroll = 0;
        self.last_was_kill = false;
    }

    /// Replace `text[start..end]` with `insert` and place cursor after the insert.
    pub fn replace_range(&mut self, start: usize, end: usize, insert: &str) {
        self.clear_selection();
        let start = start.min(self.text.len());
        let mut end = end.min(self.text.len()).max(start);
        while end < self.text.len() && !self.text.is_char_boundary(end) {
            end += 1;
        }
        self.push_undo();
        let mut new = String::with_capacity(self.text.len() - (end - start) + insert.len());
        new.push_str(&self.text[..start]);
        new.push_str(insert);
        new.push_str(&self.text[end..]);
        self.cursor = start + insert.len();
        self.text = new;
        self.horizontal_scroll = 0;
        self.last_was_kill = false;
    }

    pub fn take(&mut self) -> String {
        self.clear_selection();
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

/// UAX #29 词边界（双击选词）。标点 / 空白 / emoji 只选中所在的单个字素，
/// 与 transcript 选择行为保持一致。
fn word_boundaries(text: &str, at: usize) -> (usize, usize) {
    let clicked = text
        .grapheme_indices(true)
        .find(|(start, g)| at >= *start && at < *start + g.len())
        .map(|(byte, _)| byte)
        .unwrap_or(at.min(text.len()));
    let word = text
        .split_word_bound_indices()
        .find(|(start, seg)| clicked >= *start && clicked < *start + seg.len());
    match word {
        Some((start, seg)) if seg.chars().any(|c| c.is_alphanumeric() || c == '_') => {
            (start, start + seg.len())
        }
        _ => {
            let (start, g) = text
                .grapheme_indices(true)
                .find(|(start, g)| clicked >= *start && clicked < *start + g.len())
                .unwrap_or((clicked, ""));
            (start, start + g.len())
        }
    }
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
    fn home_end_jump_to_buffer_edges() {
        let mut s = InputState::new();
        s.set_text("hello".to_string());
        s.move_left();
        s.move_buffer_home();
        assert_eq!(s.cursor, 0);
        s.move_buffer_end();
        assert_eq!(s.cursor, s.text.len());

        // 多行时 Home/End 仍指向整段输入的开头/末尾（区别于 Ctrl-A/E 的行内跳转）
        s.set_text("ab\ncdef".to_string());
        s.move_buffer_end();
        assert_eq!(s.cursor, s.text.len());
        s.move_buffer_home();
        assert_eq!(s.cursor, 0);
        // Ctrl-A/E 的行级行为不受影响
        s.cursor = s.text.find('d').unwrap();
        s.move_home();
        assert_eq!(s.cursor, 3);
        s.move_end();
        assert_eq!(s.cursor, s.text.len());
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
    fn backspace_removes_image_marker_whole() {
        let mut s = InputState::new();
        s.insert_image_mention(".kkagent/attachments/abc.png");
        assert_eq!(s.text, "[Pasted Image #1]");
        // Cursor sits after the marker; one backspace clears it entirely.
        s.backspace();
        assert_eq!(s.text, "");
        assert_eq!(s.cursor, 0);
        // The stored mapping is gone with the marker.
        assert_eq!(s.expand_pastes("[Pasted Image #1]"), "[Pasted Image #1]");
    }

    #[test]
    fn backspace_removes_text_paste_marker_whole() {
        let mut s = InputState::new();
        s.insert_paste(&"x\n".repeat(20), true);
        assert!(s.text.starts_with("[Pasted text #1"));
        let len_before = s.text.len();
        s.backspace();
        assert_eq!(s.text, "");
        assert_eq!(len_before, "[Pasted text #1 +20 lines]".len());
    }

    #[test]
    fn backspace_still_deletes_single_char_outside_markers() {
        let mut s = InputState::new();
        s.insert_image_mention(".kkagent/attachments/a.png");
        s.insert_str(" tail");
        s.backspace(); // removes 'l'
        assert!(s.text.ends_with(" tai"));
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

    #[test]
    fn large_paste_folds_to_overview() {
        let mut s = InputState::new();
        let big = "line\n".repeat(20);
        s.insert_paste(&big, true);
        assert!(s.text.starts_with("[Pasted text #1"));
        assert!(s.text.contains("lines]"));
        let expanded = s.expand_pastes(&s.text);
        assert!(expanded.lines().count() >= 20);
    }

    #[test]
    fn second_paste_expands_marker() {
        let mut s = InputState::new();
        let big = "alpha\n".repeat(16);
        s.insert_paste(&big, true);
        let marker = s.text.clone();
        s.cursor = marker.len() / 2;
        assert!(s.expand_paste_marker_at_cursor());
        assert!(s.text.contains("alpha"));
        assert!(!s.text.contains("[Pasted text"));
    }

    #[test]
    fn mouse_drag_selects_and_copies() {
        let mut s = InputState::new();
        s.set_text("hello 世界 wide".to_string());
        let wide = s.text.find("wide").unwrap();
        s.begin_selection(0);
        assert!(!s.selection_active());
        s.update_selection(wide);
        assert!(s.selection_active());
        assert_eq!(s.selected_text().as_deref(), Some("hello 世界 "));
        s.end_selection();
        assert!(s.selection_active());
        // 反向拖拽也能归一化
        s.begin_selection(wide);
        s.update_selection(1);
        s.end_selection();
        assert_eq!(s.selected_text().as_deref(), Some("ello 世界 "));
        // 纯点击（无拖动）不留选区
        s.begin_selection(2);
        s.end_selection();
        assert!(!s.selection_active());
        assert_eq!(s.cursor, 2);
    }

    #[test]
    fn typing_replaces_selection() {
        let mut s = InputState::new();
        s.set_text("hello world".to_string());
        s.begin_selection(6);
        s.update_selection(11);
        s.end_selection();
        s.insert_char('X');
        assert_eq!(s.text, "hello X");
        assert_eq!(s.cursor, 7);
        assert!(!s.selection_active());
    }

    #[test]
    fn editing_clears_selection() {
        let mut s = InputState::new();
        s.set_text("hello world".to_string());
        s.begin_selection(0);
        s.update_selection(5);
        s.end_selection();
        // 选中态下 backspace 只删除选区本身
        s.backspace();
        assert_eq!(s.text, " world");
        assert_eq!(s.cursor, 0);
        s.set_text("hello world".to_string());
        s.begin_selection(3);
        s.update_selection(8);
        s.end_selection();
        s.clear();
        assert!(s.text.is_empty());
        assert!(!s.selection_active());
    }

    #[test]
    fn double_click_selects_word_cjk_punct() {
        let mut s = InputState::new();
        s.set_text("can't stop 你好，世界".to_string());
        let stop = s.text.find("stop").unwrap();
        s.select_word_at(stop + 1);
        assert_eq!(s.selected_text().as_deref(), Some("stop"));

        let comma = s.text.find('，').unwrap();
        s.select_word_at(comma);
        assert_eq!(s.selected_text().as_deref(), Some("，"));

        let shi = s.text.rfind('世').unwrap();
        s.select_word_at(shi);
        // UAX #29 将每个 CJK 表意字视作独立 token，与 transcript 选词一致。
        assert_eq!(s.selected_text().as_deref(), Some("世"));
    }

    #[test]
    fn triple_click_selects_logical_line() {
        let mut s = InputState::new();
        s.set_text("alpha\nbeta\ngamma".to_string());
        let beta = s.text.find("beta").unwrap();
        s.select_line_at(beta + 1);
        assert_eq!(s.selected_text().as_deref(), Some("beta"));
        // 首行 / 末行
        s.select_line_at(0);
        assert_eq!(s.selected_text().as_deref(), Some("alpha"));
        let gamma = s.text.rfind("gamma").unwrap();
        s.select_line_at(gamma);
        assert_eq!(s.selected_text().as_deref(), Some("gamma"));
    }

    #[test]
    fn yank_replaces_selection() {
        let mut s = InputState::new();
        s.set_text("hello world".to_string());
        s.cursor = 5;
        s.kill_line(); // kill " world"
        s.begin_selection(1);
        s.update_selection(3);
        s.end_selection(); // 选中 "el"
        s.yank();
        assert_eq!(s.text, "h worldlo");
    }
}
