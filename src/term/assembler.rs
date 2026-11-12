use ratatui::style::{Color, Modifier, Style};
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
    style: Style,
    utf8: [u8; 4],
    utf8_len: usize,
    invalid_utf8: u64,
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
            utf8: [0; 4],
            utf8_len: 0,
            invalid_utf8: 0,
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

    /// Total invalid bytes escaped since this assembler was created.
    pub fn invalid_utf8(&self) -> u64 {
        self.invalid_utf8
    }

    pub fn reset(&mut self) {
        self.current.clear();
        self.utf8_len = 0;
        self.csi.clear();
        self.cursor = 0;
        self.style = PLAIN;
        self.state = State::Ground;
    }

    fn ground(&mut self, byte: u8) {
        if self.utf8_len > 0 {
            let len = self.utf8_len;
            self.utf8[len] = byte;
            match std::str::from_utf8(&self.utf8[..=len]) {
                Ok(text) => {
                    self.utf8_len = 0;
                    if let Some(ch) = text.chars().next() {
                        self.put(ch);
                    }
                    return;
                }
                Err(error) if error.error_len().is_none() => {
                    self.utf8_len += 1;
                    return;
                }
                Err(_) => {
                    self.utf8_len = 0;
                    for index in 0..len {
                        self.escape_byte(self.utf8[index]);
                    }
                }
            }
        }
        if byte >= 0x80 {
            if (0xc2..=0xf4).contains(&byte) {
                self.utf8[0] = byte;
                self.utf8_len = 1;
            } else {
                self.escape_byte(byte);
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

    fn escape_byte(&mut self, byte: u8) {
        const HEX: &[u8; 16] = b"0123456789ABCDEF";
        self.invalid_utf8 = self.invalid_utf8.saturating_add(1);
        self.put('\\');
        self.put('x');
        self.put(HEX[usize::from(byte >> 4)] as char);
        self.put(HEX[usize::from(byte & 0x0f)] as char);
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
        let mut parts = params.split(';');
        while let Some(part) = parts.next() {
            let code = if part.is_empty() {
                Some(0)
            } else {
                part.parse::<u16>().ok()
            };
            match code {
                Some(0) => self.style = PLAIN,
                Some(1) => self.style = self.style.add_modifier(Modifier::BOLD),
                Some(4) => self.style = self.style.add_modifier(Modifier::UNDERLINED),
                Some(22) => self.style.add_modifier.remove(Modifier::BOLD),
                Some(24) => self.style.add_modifier.remove(Modifier::UNDERLINED),
                Some(code @ (30..=37 | 90..=97)) => {
                    self.style.fg = ansi_color(code);
                }
                Some(code @ (40..=47 | 100..=107)) => {
                    self.style.bg = ansi_color(code - 10);
                }
                Some(39) => self.style.fg = None,
                Some(49) => self.style.bg = None,
                Some(code @ (38 | 48)) => {
                    let color = match parts.next() {
                        Some("5") => parts
                            .next()
                            .and_then(|part| part.parse().ok())
                            .map(Color::Indexed),
                        Some("2") => {
                            let red = parts.next().and_then(|part| part.parse().ok());
                            let green = parts.next().and_then(|part| part.parse().ok());
                            let blue = parts.next().and_then(|part| part.parse().ok());
                            match (red, green, blue) {
                                (Some(r), Some(g), Some(b)) => Some(Color::Rgb(r, g, b)),
                                _ => None,
                            }
                        }
                        _ => None,
                    };
                    if let Some(color) = color {
                        if code == 38 {
                            self.style.fg = Some(color);
                        } else {
                            self.style.bg = Some(color);
                        }
                    }
                }
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

fn ansi_color(code: u16) -> Option<Color> {
    Some(match code {
        30 => Color::Black,
        31 => Color::Red,
        32 => Color::Green,
        33 => Color::Yellow,
        34 => Color::Blue,
        35 => Color::Magenta,
        36 => Color::Cyan,
        37 => Color::Gray,
        90 => Color::DarkGray,
        91 => Color::LightRed,
        92 => Color::LightGreen,
        93 => Color::LightYellow,
        94 => Color::LightBlue,
        95 => Color::LightMagenta,
        96 => Color::LightCyan,
        97 => Color::White,
        _ => return None,
    })
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
        assert_eq!(line[0].style, PLAIN.fg(Color::Red));
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
        assert_eq!(out, vec![r"ok\xFF!"]);
    }

    #[test]
    fn csi_split_across_packets_still_applies() {
        let mut a = Assembler::new();
        a.feed(b"\x1b[3");
        a.feed(b"2mGREEN\n");
        let line = a.take().remove(0);
        assert_eq!(text(&line), "GREEN");
        assert_eq!(line[0].style, PLAIN.fg(Color::Green));
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
    fn sgr_supports_all_sixteen_foreground_and_background_colors() {
        let colors = [
            Color::Black,
            Color::Red,
            Color::Green,
            Color::Yellow,
            Color::Blue,
            Color::Magenta,
            Color::Cyan,
            Color::Gray,
            Color::DarkGray,
            Color::LightRed,
            Color::LightGreen,
            Color::LightYellow,
            Color::LightBlue,
            Color::LightMagenta,
            Color::LightCyan,
            Color::White,
        ];
        for (index, color) in colors.into_iter().enumerate() {
            let fg = if index < 8 {
                30 + index
            } else {
                90 + index - 8
            };
            let mut a = Assembler::new();
            a.feed(format!("\x1b[{fg};{}mX", fg + 10).as_bytes());
            assert_eq!(a.pending()[0].style, PLAIN.fg(color).bg(color));
        }
    }

    #[test]
    fn extended_colors_consume_components_instead_of_applying_them_as_sgr() {
        let mut a = Assembler::new();
        a.feed(b"\x1b[38;2;1;4;0;48;2;31;49;22mA\x1b[38;5;196;48;5;0mB");
        let cells = a.pending();
        assert_eq!(
            cells[0].style,
            PLAIN.fg(Color::Rgb(1, 4, 0)).bg(Color::Rgb(31, 49, 22))
        );
        assert_eq!(
            cells[1].style,
            PLAIN.fg(Color::Indexed(196)).bg(Color::Indexed(0))
        );
    }

    #[test]
    fn malformed_color_components_are_consumed_without_becoming_modifiers() {
        let mut a = Assembler::new();
        a.feed(b"\x1b[31m\x1b[38;2;999;1;4mX\x1b[48;5;999mY");
        assert!(
            a.pending()
                .iter()
                .all(|cell| cell.style == PLAIN.fg(Color::Red))
        );
    }

    #[test]
    fn sgr_selective_and_full_resets_preserve_unrelated_attributes() {
        let mut a = Assembler::new();
        a.feed(b"\x1b[31;44;1;4mA\x1b[22mB\x1b[24mC\x1b[39mD\x1b[49mE\x1b[0mF\x1b[1;4mG\x1b[mH");
        let cells = a.pending();
        assert!(
            cells[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD | Modifier::UNDERLINED)
        );
        assert_eq!(cells[1].style.fg, Some(Color::Red));
        assert_eq!(cells[1].style.bg, Some(Color::Blue));
        assert!(!cells[1].style.add_modifier.contains(Modifier::BOLD));
        assert!(cells[1].style.add_modifier.contains(Modifier::UNDERLINED));
        assert!(cells[2].style.add_modifier.is_empty());
        assert_eq!(cells[2].style.fg, Some(Color::Red));
        assert_eq!(cells[2].style.bg, Some(Color::Blue));
        assert_eq!(cells[3].style.fg, None);
        assert_eq!(cells[3].style.bg, Some(Color::Blue));
        assert_eq!(cells[4].style.bg, None);
        assert_eq!(cells[5].style, PLAIN);
        assert!(
            cells[6]
                .style
                .add_modifier
                .contains(Modifier::BOLD | Modifier::UNDERLINED)
        );
        assert_eq!(cells[7].style, PLAIN);
    }

    #[test]
    fn bad_prefix_preserves_ascii_newlines_and_escape_controls() {
        let mut a = Assembler::new();
        a.feed(b"\xe2\x82A\n\xf0\x9f\n\xe2\x1b[31mR\n");
        let rows = a.take();
        assert_eq!(
            rows.iter().map(|row| text(row)).collect::<Vec<_>>(),
            vec![r"\xE2\x82A", r"\xF0\x9F", r"\xE2R"]
        );
        assert_eq!(rows[2].last().unwrap().style.fg, Some(Color::Red));
        assert_eq!(a.invalid_utf8(), 5);
    }

    #[test]
    fn invalid_byte_count_is_cumulative_across_rows_and_reset() {
        let mut a = Assembler::new();
        a.feed(b"\xff\xc0\xaf\n");
        assert_eq!(text(&a.take()[0]), r"\xFF\xC0\xAF");
        assert_eq!(a.invalid_utf8(), 3);
        a.reset();
        a.feed(b"\x80\n");
        assert_eq!(a.invalid_utf8(), 4);
    }

    #[test]
    fn text_styles_and_invalid_bytes_are_independent_of_chunk_boundaries() {
        let bytes = b"A\xe6\xb8\xa9\xf0\x9f\x98\x80\xe2\x82!\xff\x1b[38;2;1;4;0mR\n";
        let mut whole = Assembler::new();
        whole.feed(bytes);
        let expected = whole.take();
        assert_eq!(text(&expected[0]), "A温😀\\xE2\\x82!\\xFFR");
        for split in 0..=bytes.len() {
            let mut a = Assembler::new();
            a.feed(&bytes[..split]);
            a.feed(&bytes[split..]);
            assert_eq!(a.take(), expected, "split at {split}");
            assert_eq!(a.invalid_utf8(), 3, "split at {split}");
        }
        let mut a = Assembler::new();
        for byte in bytes {
            a.feed(std::slice::from_ref(byte));
        }
        assert_eq!(a.take(), expected);
        assert_eq!(a.invalid_utf8(), 3);
    }

    #[test]
    fn trailing_blank_cells_are_trimmed() {
        let mut a = Assembler::new();
        assert_eq!(lines(&mut a, "x   \n"), vec!["x"]);
    }
}
