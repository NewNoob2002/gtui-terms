//! Fallback colors for unstyled ESP logs, parsed directly from terminal cells.
//!
//! Strict prefixes avoid coloring prose: IDF requires `L (digits) tag: ` with
//! a nonempty tag containing no whitespace or colons. Arduino requires
//! `[digits][L][file:digits] ` (timestamp padding with spaces is accepted).
//! Leading spaces/tabs are accepted; timestamps are decimal milliseconds only.
//! An incomplete prefix has no color until its final space/tab arrives. Callers
//! can recompute for pending rows each frame and must preserve explicit ANSI fg.

use ratatui::style::Color;

use crate::term::scrollback::Cell;

pub fn color(mut cells: &[Cell]) -> Option<Color> {
    trim_indent(&mut cells);
    if consume(&mut cells, '[') {
        while consume(&mut cells, ' ') {}
        if !decimal(&mut cells) {
            return None;
        }
        while consume(&mut cells, ' ') {}
        if !consume(&mut cells, ']') || !consume(&mut cells, '[') {
            return None;
        }
        let level = level(cells.first()?.ch)?;
        cells = &cells[1..];
        if !consume(&mut cells, ']') || !consume(&mut cells, '[') {
            return None;
        }
        let file_end = cells.iter().position(|cell| cell.ch == ':')?;
        if file_end == 0
            || cells[..file_end]
                .iter()
                .any(|cell| cell.ch.is_whitespace() || matches!(cell.ch, '[' | ']'))
        {
            return None;
        }
        cells = &cells[file_end + 1..];
        if decimal(&mut cells) && consume(&mut cells, ']') && separator(cells) {
            return Some(level);
        }
        return None;
    }

    let level = level(cells.first()?.ch)?;
    cells = &cells[1..];
    if !consume(&mut cells, ' ')
        || !consume(&mut cells, '(')
        || !decimal(&mut cells)
        || !consume(&mut cells, ')')
        || !consume(&mut cells, ' ')
    {
        return None;
    }
    let tag_end = cells.iter().position(|cell| cell.ch == ':')?;
    if tag_end == 0 || cells[..tag_end].iter().any(|cell| cell.ch.is_whitespace()) {
        return None;
    }
    separator(&cells[tag_end + 1..]).then_some(level)
}

fn level(ch: char) -> Option<Color> {
    match ch {
        'I' => Some(Color::Green),
        'W' => Some(Color::Yellow),
        'E' => Some(Color::Red),
        'D' => Some(Color::Cyan),
        'V' => Some(Color::DarkGray),
        _ => None,
    }
}

fn consume(cells: &mut &[Cell], ch: char) -> bool {
    if cells.first().is_some_and(|cell| cell.ch == ch) {
        *cells = &cells[1..];
        true
    } else {
        false
    }
}

fn decimal(cells: &mut &[Cell]) -> bool {
    let count = cells
        .iter()
        .take_while(|cell| cell.ch.is_ascii_digit())
        .count();
    *cells = &cells[count..];
    count != 0
}

fn trim_indent(cells: &mut &[Cell]) {
    while separator(cells) {
        *cells = &cells[1..];
    }
}

fn separator(cells: &[Cell]) -> bool {
    cells
        .first()
        .is_some_and(|cell| matches!(cell.ch, ' ' | '\t'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::term::scrollback::PLAIN;

    fn cells(text: &str) -> Vec<Cell> {
        text.chars().map(|ch| Cell::new(ch, PLAIN)).collect()
    }

    #[test]
    fn colors_plain_idf_boot_and_application_logs() {
        for (text, expected) in [
            (
                "I (29) boot: ESP-IDF v5.3.1 2nd stage bootloader",
                Color::Green,
            ),
            (
                "I (315) cpu_start: Starting scheduler on PRO CPU.",
                Color::Green,
            ),
            ("W (456) wifi: wifi nvs_open fail ret=4353", Color::Yellow),
            ("E (1024) esp-tls: [sock=54] select() timeout", Color::Red),
            (
                "D (1042) esp_netif_handlers: esp_netif action connected",
                Color::Cyan,
            ),
            ("V (12345) example: event received", Color::DarkGray),
            (" \tI (0) main_task: Calling app_main()", Color::Green),
        ] {
            assert_eq!(color(&cells(text)), Some(expected), "{text:?}");
        }
    }

    #[test]
    fn rejects_prose_and_malformed_idf_prefixes() {
        for text in [
            "",
            " ",
            "I",
            "I think this should work",
            "Error: connection lost",
            "E (today) application failed",
            "I (123) ordinary prose: hello",
            "I () tag: message",
            "I (-1) tag: message",
            "I (1.2) tag: message",
            "I ( 12) tag: message",
            "I (１２) tag: message",
            "I (12x) tag: message",
            "I (12 tag: message",
            "I(12) tag: message",
            "I (12)tag: message",
            "I (12) : message",
            "I (12) tag:message",
            "i (12) tag: message",
            "X (12) tag: message",
            "prefix I (12) tag: message",
        ] {
            assert_eq!(color(&cells(text)), None, "{text:?}");
        }
    }

    #[test]
    fn waits_for_a_complete_idf_prefix_during_chunked_input() {
        let prefix = cells("E (1024) esp-tls: ");
        for end in 0..prefix.len() {
            assert_eq!(color(&prefix[..end]), None, "prefix length {end}");
        }
        assert_eq!(color(&prefix), Some(Color::Red));
        assert_eq!(
            color(&cells("E (1024) esp-tls:\tpartial")),
            Some(Color::Red)
        );
    }

    #[test]
    fn colors_arduino_logs_only_after_complete_file_and_line() {
        for (text, expected) in [
            ("[  1234][I][main.cpp:42] setup(): Ready", Color::Green),
            (
                "[1234][W][WiFiGeneric.cpp:1035] _eventCallback(): Disconnected",
                Color::Yellow,
            ),
            ("[0][E][main.cpp:1] failed", Color::Red),
            (
                "[999999999999999999999][D][esp32-hal-cpu.c:244] clock",
                Color::Cyan,
            ),
            (" \t[ 123 ][V][main.cpp:2]\ttrace", Color::DarkGray),
        ] {
            assert_eq!(color(&cells(text)), Some(expected), "{text:?}");
        }
        let prefix = cells("[  1234][I][main.cpp:42] ");
        for end in 0..prefix.len() {
            assert_eq!(color(&prefix[..end]), None, "prefix length {end}");
        }
        for text in [
            "[today][I][main.cpp:42] hello",
            "[][I][main.cpp:42] hello",
            "[-1][I][main.cpp:42] hello",
            "[1.2][I][main.cpp:42] hello",
            "[1][INFO][main.cpp:42] hello",
            "[1][X][main.cpp:42] hello",
            "[1][I][:42] hello",
            "[1][I][main.cpp:] hello",
            "[1][I][main.cpp:-1] hello",
            "[1][I][main.cpp:42x] hello",
            "[1][I][main.cpp:42]hello",
            "[1][I][main file.cpp:42] hello",
            "[1][I][main].cpp:42] hello",
            "[1][I][main.cpp:42 hello",
        ] {
            assert_eq!(color(&cells(text)), None, "{text:?}");
        }
    }

    #[test]
    fn styles_do_not_affect_detection_or_get_modified() {
        let mut row = cells("I (123) example: ready");
        row[0].style = PLAIN.fg(Color::Magenta);
        let before = row.clone();
        assert_eq!(color(&row), Some(Color::Green));
        assert_eq!(row, before);
    }
}
