//! 多行输入框的编辑缓冲区：按字符索引维护光标，按显示宽度折行。

use unicode_width::UnicodeWidthChar;

#[derive(Default, Clone, Debug)]
pub struct Editor {
    buf: Vec<char>,
    cursor: usize,
    limit: usize,
}

/// 一个显示行：`buf[start..end]`，不含折行处的换行符。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VisualRow {
    pub start: usize,
    pub end: usize,
}

fn width(ch: char) -> usize {
    UnicodeWidthChar::width(ch).unwrap_or(0)
}

fn is_word(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

impl Editor {
    pub fn new(limit: usize) -> Self {
        Self {
            limit,
            ..Self::default()
        }
    }

    pub fn chars(&self) -> &[char] {
        &self.buf
    }

    pub fn text(&self) -> String {
        self.buf.iter().collect()
    }

    #[cfg(test)]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn clear(&mut self) {
        self.buf.clear();
        self.cursor = 0;
    }

    pub fn set(&mut self, text: &str) {
        self.buf = text.chars().take(self.limit).collect();
        self.cursor = self.buf.len();
    }

    /// 插入文本；`\r\n` 和单独的 `\r` 统一为 `\n`，其余控制字符丢弃。
    /// 返回实际插入的字符数（超出上限的部分被截断）。
    pub fn insert_str(&mut self, text: &str) -> usize {
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        let room = self.limit.saturating_sub(self.buf.len());
        let chars: Vec<char> = normalized
            .chars()
            .filter(|ch| *ch == '\n' || *ch == '\t' || !ch.is_control())
            .take(room)
            .collect();
        let count = chars.len();
        self.buf.splice(self.cursor..self.cursor, chars);
        self.cursor += count;
        count
    }

    pub fn insert(&mut self, ch: char) -> bool {
        if self.buf.len() >= self.limit {
            return false;
        }
        self.buf.insert(self.cursor, ch);
        self.cursor += 1;
        true
    }

    pub fn backspace(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        self.cursor -= 1;
        self.buf.remove(self.cursor);
        true
    }

    pub fn delete(&mut self) -> bool {
        if self.cursor >= self.buf.len() {
            return false;
        }
        self.buf.remove(self.cursor);
        true
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.buf.len());
    }

    fn line_start(&self) -> usize {
        self.buf[..self.cursor]
            .iter()
            .rposition(|ch| *ch == '\n')
            .map_or(0, |pos| pos + 1)
    }

    fn line_end(&self) -> usize {
        self.buf[self.cursor..]
            .iter()
            .position(|ch| *ch == '\n')
            .map_or(self.buf.len(), |pos| self.cursor + pos)
    }

    /// Home/Ctrl+A：到当前逻辑行行首。
    pub fn home(&mut self) {
        self.cursor = self.line_start();
    }

    /// End/Ctrl+E：到当前逻辑行行尾。
    pub fn end(&mut self) {
        self.cursor = self.line_end();
    }

    fn word_left_pos(&self) -> usize {
        let mut pos = self.cursor;
        while pos > 0 && !is_word(self.buf[pos - 1]) {
            pos -= 1;
        }
        while pos > 0 && is_word(self.buf[pos - 1]) {
            pos -= 1;
        }
        pos
    }

    pub fn word_left(&mut self) {
        self.cursor = self.word_left_pos();
    }

    pub fn word_right(&mut self) {
        let len = self.buf.len();
        let mut pos = self.cursor;
        while pos < len && !is_word(self.buf[pos]) {
            pos += 1;
        }
        while pos < len && is_word(self.buf[pos]) {
            pos += 1;
        }
        self.cursor = pos;
    }

    /// Ctrl+W：删除光标前一个词（连同其前的分隔符）。
    pub fn delete_word_back(&mut self) -> bool {
        let start = self.word_left_pos();
        self.buf.drain(start..self.cursor);
        let changed = start != self.cursor;
        self.cursor = start;
        changed
    }

    /// Ctrl+U：删除到行首；已在行首时删除前面的换行，便于连续按键合并行。
    pub fn kill_to_line_start(&mut self) -> bool {
        let start = self.line_start();
        if start == self.cursor {
            return self.backspace();
        }
        self.buf.drain(start..self.cursor);
        self.cursor = start;
        true
    }

    /// Ctrl+K：删除到行尾；已在行尾时删除换行。
    pub fn kill_to_line_end(&mut self) -> bool {
        let end = self.line_end();
        if end == self.cursor {
            return self.delete();
        }
        self.buf.drain(self.cursor..end);
        true
    }

    /// 按 `width` 列折行，返回显示行。宽度为 0 时视为 1，保证进度。
    pub fn rows(&self, width: usize) -> Vec<VisualRow> {
        let width = width.max(1);
        let mut rows = Vec::new();
        let mut start = 0;
        let mut used = 0;
        for (index, ch) in self.buf.iter().enumerate() {
            if *ch == '\n' {
                rows.push(VisualRow { start, end: index });
                start = index + 1;
                used = 0;
                continue;
            }
            let w = width_of(*ch);
            if used + w > width && index > start {
                rows.push(VisualRow { start, end: index });
                start = index;
                used = 0;
            }
            used += w;
        }
        rows.push(VisualRow {
            start,
            end: self.buf.len(),
        });
        rows
    }

    /// 光标所在显示行与列（列按显示宽度）。光标恰好在满宽行末尾时放到下一行行首。
    pub fn cursor_position(&self, width: usize) -> (usize, usize) {
        let rows = self.rows(width);
        let row = Self::row_of(&rows, self.cursor);
        let col = self.buf[rows[row].start..self.cursor]
            .iter()
            .map(|ch| width_of(*ch))
            .sum();
        (row, col)
    }

    fn row_of(rows: &[VisualRow], cursor: usize) -> usize {
        // 折行边界上的光标属于后一行；换行符后的光标也属于后一行。
        rows.iter()
            .rposition(|row| row.start <= cursor)
            .unwrap_or(0)
    }

    /// 在显示行间上下移动并尽量保持列；已在首/末行时返回 false，由调用方翻历史。
    pub fn vertical(&mut self, up: bool, width: usize) -> bool {
        let rows = self.rows(width);
        let (row, col) = self.cursor_position(width);
        let target = if up {
            match row.checked_sub(1) {
                Some(target) => target,
                None => return false,
            }
        } else if row + 1 < rows.len() {
            row + 1
        } else {
            return false;
        };
        let VisualRow { start, end } = rows[target];
        let mut pos = start;
        let mut used = 0;
        while pos < end {
            let w = width_of(self.buf[pos]);
            if used + w > col {
                break;
            }
            used += w;
            pos += 1;
        }
        // 折行（非换行）行的末尾就是下一行起点，退一格避免光标跳到下一行。
        if pos == end && target + 1 < rows.len() && rows[target + 1].start == end && end > start {
            pos = end - 1;
        }
        self.cursor = pos;
        true
    }
}

/// 制表符按 4 列显示，其它字符按 Unicode 宽度。
pub fn width_of(ch: char) -> usize {
    if ch == '\t' { 4 } else { width(ch) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor(text: &str) -> Editor {
        let mut editor = Editor::new(4096);
        editor.set(text);
        editor
    }

    #[test]
    fn paste_normalizes_newlines_and_respects_limit() {
        let mut editor = Editor::new(8);
        assert_eq!(editor.insert_str("ab\r\ncd\ref\x07gh"), 8);
        assert_eq!(editor.text(), "ab\ncd\nef");
        assert!(!editor.insert('x'));
    }

    #[test]
    fn line_and_word_navigation() {
        let mut editor = editor("first line\nsecond_word here");
        editor.home();
        assert_eq!(editor.cursor(), 11);
        editor.word_right();
        assert_eq!(editor.cursor(), 22);
        editor.end();
        assert_eq!(editor.cursor(), editor.chars().len());
        assert!(editor.delete_word_back());
        assert_eq!(editor.text(), "first line\nsecond_word ");
        assert!(editor.kill_to_line_start());
        assert_eq!(editor.text(), "first line\n");
        assert!(editor.kill_to_line_start());
        assert_eq!(editor.text(), "first line");
        editor.home();
        editor.word_right();
        assert!(editor.kill_to_line_end());
        assert_eq!(editor.text(), "first");
    }

    #[test]
    fn wraps_by_display_width_and_moves_vertically() {
        let mut editor = editor("abcdef\n中文字符xy");
        let rows = editor.rows(4);
        let spans: Vec<String> = rows
            .iter()
            .map(|row| editor.chars()[row.start..row.end].iter().collect())
            .collect();
        assert_eq!(spans, ["abcd", "ef", "中文", "字符", "xy"]);
        assert_eq!(editor.cursor_position(4), (4, 2));
        assert!(editor.vertical(true, 4));
        assert_eq!(editor.cursor_position(4), (3, 2));
        assert!(editor.vertical(true, 4));
        assert!(editor.vertical(true, 4));
        assert_eq!(editor.cursor_position(4), (1, 2));
        assert!(editor.vertical(true, 4));
        assert_eq!(editor.cursor_position(4), (0, 2));
        assert!(!editor.vertical(true, 4));
        editor.end();
        assert_eq!(editor.cursor(), 6);
        assert!(!editor.vertical(false, 80) || editor.cursor() > 6);
    }

    #[test]
    fn empty_and_trailing_newline_have_a_row_for_the_cursor() {
        let editor = editor("");
        assert_eq!(editor.rows(10).len(), 1);
        let editor = self::editor("abc\n");
        assert_eq!(editor.rows(10).len(), 2);
        assert_eq!(editor.cursor_position(10), (1, 0));
    }
}
