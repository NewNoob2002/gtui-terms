use std::collections::VecDeque;

use ratatui::style::{Color, Style};

pub const PLAIN: Style = Style::new();
pub const HIGHLIGHT: Style = Style::new().bg(Color::Yellow).fg(Color::Black);
const CELL_BYTES: usize = std::mem::size_of::<Cell>();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub style: Style,
}

impl Cell {
    pub const fn new(ch: char, style: Style) -> Self {
        Self { ch, style }
    }

    pub fn is_blank(&self) -> bool {
        self.ch == ' ' && self.style == PLAIN
    }
}

/// Case-insensitive substring mask over `cells` (`needle` already lowercased).
pub fn match_mask(cells: &[Cell], needle: &[char]) -> Vec<bool> {
    let mut mask = vec![false; cells.len()];
    if needle.is_empty() || cells.len() < needle.len() {
        return mask;
    }
    let lowered: Vec<char> = cells.iter().map(|c| c.ch.to_ascii_lowercase()).collect();
    let last = cells.len() - needle.len();
    for start in 0..=last {
        if (0..needle.len()).all(|k| lowered[start + k] == needle[k]) {
            for slot in &mut mask[start..start + needle.len()] {
                *slot = true;
            }
        }
    }
    mask
}

pub struct Row {
    pub cells: Vec<Cell>,
    pub at: u64,
}

impl Row {
    pub fn text(&self) -> String {
        self.cells.iter().map(|cell| cell.ch).collect()
    }
}

pub struct Scrollback {
    rows: VecDeque<Row>,
    dropped: usize,
    bytes: usize,
    max_rows: usize,
    max_bytes: usize,
}

impl Scrollback {
    pub fn new(max_rows: usize, max_bytes: usize) -> Self {
        Self {
            rows: VecDeque::new(),
            dropped: 0,
            bytes: 0,
            max_rows,
            max_bytes,
        }
    }

    pub fn push(&mut self, mut row: Row) {
        let cap = (self.max_bytes / CELL_BYTES).max(1);
        if row.cells.len() > cap {
            row.cells.truncate(cap);
        }
        self.bytes += row.cells.len() * CELL_BYTES;
        self.rows.push_back(row);
        while self.rows.len() > self.max_rows || self.bytes > self.max_bytes {
            let Some(old) = self.rows.pop_front() else {
                break;
            };
            self.bytes -= old.cells.len() * CELL_BYTES;
            self.dropped += 1;
        }
    }

    pub fn clear(&mut self) {
        self.rows.clear();
        self.bytes = 0;
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn dropped(&self) -> usize {
        self.dropped
    }

    pub fn iter(&self) -> impl Iterator<Item = &Row> {
        self.rows.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(text: &str) -> Row {
        Row {
            cells: text.chars().map(|ch| Cell::new(ch, PLAIN)).collect(),
            at: 0,
        }
    }

    #[test]
    fn match_mask_is_case_insensitive_and_masks_every_match() {
        let cells: Vec<Cell> = "Hello hello"
            .chars()
            .map(|ch| Cell::new(ch, PLAIN))
            .collect();
        let needle: Vec<char> = "hello".chars().collect();
        let mask = match_mask(&cells, &needle);
        assert_eq!(mask.iter().filter(|hit| **hit).count(), 10);
        assert!(mask[0]);
        assert!(!mask[5]);
    }

    #[test]
    fn row_limit_drops_oldest_and_counts() {
        let mut sb = Scrollback::new(3, 1 << 20);
        for i in 0..5 {
            sb.push(row(&format!("line {i}")));
        }
        assert_eq!(sb.len(), 3);
        assert_eq!(sb.dropped(), 2);
        let first: String = sb
            .iter()
            .next()
            .unwrap()
            .cells
            .iter()
            .map(|c| c.ch)
            .collect();
        assert_eq!(first, "line 2");
    }

    #[test]
    fn byte_limit_drops_oldest() {
        let mut sb = Scrollback::new(1000, CELL_BYTES * 20);
        for _ in 0..10 {
            sb.push(row("0123456789"));
        }
        assert!(sb.len() <= 2);
        assert_eq!(sb.dropped(), 8);
    }

    #[test]
    fn byte_budget_accounts_for_the_full_styled_cell_size() {
        let cell_bytes = std::mem::size_of::<Cell>();
        let mut sb = Scrollback::new(10, 3 * cell_bytes);
        sb.push(Row {
            cells: vec![Cell::new('x', PLAIN.fg(Color::Rgb(1, 2, 3)).bg(Color::Indexed(200))); 3],
            at: 0,
        });
        assert_eq!(sb.bytes, 3 * cell_bytes);
        sb.push(row("y"));
        assert_eq!(sb.bytes, cell_bytes);
        assert_eq!(sb.dropped(), 1);
        sb.clear();
        assert_eq!(sb.bytes, 0);
    }

    #[test]
    fn oversized_row_is_truncated() {
        let mut sb = Scrollback::new(10, CELL_BYTES * 4);
        sb.push(row("abcdefgh"));
        assert_eq!(sb.len(), 1);
        assert_eq!(sb.iter().next().unwrap().cells.len(), 4);
        assert_eq!(sb.dropped(), 0);
    }
}
