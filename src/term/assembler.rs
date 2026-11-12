use unicode_width::UnicodeWidthChar;

use super::scrollback::{Cell, PLAIN};

/// Longest single line we keep before forcing a break; guards against a device
/// that streams bytes without ever sending `\n`.
pub const MAX_CELLS: usize = 8192;
const MAX_CSI: usize = 64;
const TAB: usize = 8;

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    Esc,
    Csi,
    Osc,
}

pub struct Assembler {
    current: Vec<Cell>,
    cursor: usize,
    style: u8,
    utf8: Vec<u8>,
    state: State,
    csi: Vec<u8>,
    done: Vec<Vec<Cell>>,
}

impl Default for Assembler {
    fn default() -> Self {
        Self::new()
    }
}

impl Assembler {
    pub fn new() -> Self {
        Self {
            current: Vec::new(),
            cursor: 0,
            style: PLAIN,
            utf8: Vec::new(),
            state: State::Ground,
            csi: Vec::new(),
            done: Vec::new(),
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            match self.state {
                State::Ground => self.ground(byte),
                State::Esc => {
                    self.state = match byte {
                        b'[' => State::Csi,
                        b']' => State::Osc,
                        _ => State::Ground,
                    };
                    if self.state == State::Csi {
                        self.csi.clear();
                    }
                }
                State::Csi => self.csi_byte(byte),
                State::Osc => {
                    if byte == 0x07 || byte == 0x9c {
                        self.state = State::Ground;
                    }
                }
            }
        }
    }

    /// Completed lines, oldest first. Emptied by the call.
    pub fn take(&mut self) -> Vec<Vec<Cell>> {
        std::mem::take(&mut self.done)
    }

    /// Text of the line still being assembled.
    pub fn pending(&self) -> &[Cell] {
        &self.current
    }

    pub fn reset(&mut self) {
        self.current.clear();
        self.utf8.clear();
        self.csi.clear();
        self.cursor = 0;
        self.style = PLAIN;
        self.state = State::Ground;
    }

    fn ground(&mut self, byte: u8) {
        if !self.utf8.is_empty() || byte >= 0x80 {
            let mut bytes = std::mem::take(&mut self.utf8);
            bytes.push(byte);
            match std::str::from_utf8(&bytes) {
                Ok(text) => {
                    for ch in text.chars() {
                        self.put(ch);
                    }
                }
                Err(e) if e.error_len().is_some() => {
                    self.put('\u{fffd}');
                }
                Err(_) if bytes.len() >= 4 => {
                    self.put('\u{fffd}');
                }
                Err(_) => self.utf8 = bytes,
            }
            return;
        }
        match byte {
            b'\n' => self.newline(),
            b'\r' => self.cursor = 0,
            0x08 => self.cursor = self.cursor.saturating_sub(1),
            b'\t' => {
                let next = (self.cursor / TAB + 1) * TAB;
                while self.cursor < next {
                    self.put(' ');
                }
            }
            0x1b => self.state = State::Esc,
            0x00..=0x06 | 0x0b | 0x0c | 0x0e..=0x1a | 0x1c..=0x1f | 0x7f => {}
            _ => self.put(byte as char),
        }
    }

    fn csi_byte(&mut self, byte: u8) {
        if (0x40..=0x7e).contains(&byte) {
            let params = std::mem::take(&mut self.csi);
            let text = String::from_utf8_lossy(&params);
            match byte {
                b'm' => self.sgr(&text),
                b'K' => self.erase_line(&text),
                b'D' => self.cursor = self.cursor.saturating_sub(count(&text)),
                b'C' => self.cursor = (self.cursor + count(&text)).min(self.current.len()),
                b'G' => self.cursor = (count(&text) - 1).min(self.current.len()),
                _ => {}
            }
            self.state = State::Ground;
        } else if self.csi.len() < MAX_CSI {
            self.csi.push(byte);
        } else {
            self.state = State::Ground;
            self.csi.clear();
        }
    }

    fn sgr(&mut self, params: &str) {
        for part in params.split(';') {
            match part.parse::<u8>().unwrap_or(0) {
                0 => self.style = PLAIN,
                1 => self.style |= 8,
                22 => self.style &= 7,
                30 => self.style &= 8,
                31..=37 => self.style = (self.style & 8) | (part.parse::<u8>().unwrap() - 30),
                39 => self.style &= 8,
                90..=97 => self.style = 8 | (part.parse::<u8>().unwrap() - 90),
                _ => {}
            }
        }
    }

    fn erase_line(&mut self, params: &str) {
        match params.parse::<u8>().unwrap_or(0) {
            0 => self.current.truncate(self.cursor.min(self.current.len())),
            1 => {
                let end = self.cursor.min(self.current.len());
                for cell in &mut self.current[..end] {
                    *cell = Cell::new(' ', PLAIN);
                }
            }
            2 => {
                self.current.clear();
                self.cursor = 0;
            }
            _ => {}
        }
    }

    fn put(&mut self, ch: char) {
        if self.cursor < self.current.len() {
            self.current[self.cursor] = Cell::new(ch, self.style);
        } else {
            if self.current.len() >= MAX_CELLS {
                self.newline();
            }
            self.current.push(Cell::new(ch, self.style));
        }
        self.cursor = self.cursor.min(self.current.len());
        self.cursor += 1;
        if let Some(width) = UnicodeWidthChar::width(ch) {
            // Wide glyphs occupy two columns; keep the cursor aligned so
            // overwrites after a `\r` land where the user expects.
            self.cursor += usize::from(width > 1);
        }
    }

    fn newline(&mut self) {
        while self.current.last().is_some_and(Cell::is_blank) {
            self.current.pop();
        }
        self.done.push(std::mem::take(&mut self.current));
        self.cursor = 0;
    }
}

fn count(params: &str) -> usize {
    params.parse::<usize>().ok().filter(|n| *n > 0).unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(cells: &[Cell]) -> String {
        cells.iter().map(|c| c.ch).collect()
    }

    fn lines(a: &mut Assembler, input: &str) -> Vec<String> {
        a.feed(input.as_bytes());
        a.take().iter().map(|l| text(l)).collect()
    }

    #[test]
    fn carriage_return_overwrites_same_line() {
        let mut a = Assembler::new();
        assert!(lines(&mut a, "progress: 10%\r").is_empty());
        assert_eq!(lines(&mut a, "progress: 90%"), Vec::<String>::new());
        assert_eq!(text(a.pending()), "progress: 90%");
        assert_eq!(lines(&mut a, "\n"), vec!["progress: 90%"]);
        assert!(a.pending().is_empty());
    }

    #[test]
    fn backspace_moves_cursor_back() {
        let mut a = Assembler::new();
        assert_eq!(lines(&mut a, "ab\x08c\n"), vec!["ac"]);
    }

    #[test]
    fn sgr_color_is_stored_per_cell() {
        let mut a = Assembler::new();
        let out = lines(&mut a, "\x1b[31mE (1) err\x1b[0m ok\n");
        assert_eq!(out, vec!["E (1) err ok"]);
        let cells = &a.take();
        assert!(cells.is_empty());
        // re-feed to inspect styles
        let mut b = Assembler::new();
        b.feed(b"\x1b[31mE\x1b[0m ok\n");
        let line = b.take().remove(0);
        assert_eq!(line[0].style, 1);
        assert_eq!(line[1].style, PLAIN);
    }

    #[test]
    fn utf8_split_across_packets_is_rejoined() {
        let mut a = Assembler::new();
        let bytes = "温度 25C\n".as_bytes();
        a.feed(&bytes[..2]);
        a.feed(&bytes[2..5]);
        a.feed(&bytes[5..]);
        let out: Vec<String> = a.take().iter().map(|l| text(l)).collect();
        assert_eq!(out, vec!["温度 25C"]);
    }

    #[test]
    fn invalid_utf8_does_not_swallow_neighbours() {
        let mut a = Assembler::new();
        a.feed(b"ok\xff!");
        a.feed(b"\n");
        let out: Vec<String> = a.take().iter().map(|l| text(l)).collect();
        assert_eq!(out, vec!["ok\u{fffd}!"]);
    }

    #[test]
    fn csi_split_across_packets_still_applies() {
        let mut a = Assembler::new();
        a.feed(b"\x1b[3");
        a.feed(b"2mGREEN\n");
        let line = a.take().remove(0);
        assert_eq!(text(&line), "GREEN");
        assert_eq!(line[0].style, 2);
    }

    #[test]
    fn erase_line_to_cursor_truncates() {
        let mut a = Assembler::new();
        assert_eq!(lines(&mut a, "abc\x1b[2D\x1b[K\n"), vec!["a"]);
    }

    #[test]
    fn runaway_line_is_split_instead_of_growing() {
        let mut a = Assembler::new();
        let chunk = "x".repeat(MAX_CELLS + 100);
        a.feed(chunk.as_bytes());
        let out = a.take();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].len(), MAX_CELLS);
        assert_eq!(a.pending().len(), 100);
    }

    #[test]
    fn unknown_escape_sequences_are_ignored() {
        let mut a = Assembler::new();
        assert_eq!(lines(&mut a, "\x1b[2J\x1b[?25lhello\n"), vec!["hello"]);
    }

    #[test]
    fn osc_title_is_swallowed() {
        let mut a = Assembler::new();
        assert_eq!(lines(&mut a, "\x1b]0;tty\x07text\n"), vec!["text"]);
    }

    #[test]
    fn trailing_blank_cells_are_trimmed() {
        let mut a = Assembler::new();
        assert_eq!(lines(&mut a, "x   \n"), vec!["x"]);
    }
}
