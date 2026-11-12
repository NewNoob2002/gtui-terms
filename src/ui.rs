use chrono::{Local, TimeZone};
use ratatui::Frame;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, Paragraph};
use unicode_width::UnicodeWidthChar;

use crate::app::{App, Eol, Mode, View};
use crate::term::scrollback::{Cell, HIGHLIGHT, PLAIN, match_mask, style_of};

pub fn draw(frame: &mut Frame, app: &mut App) {
    match app.mode {
        Mode::Picker => draw_picker(frame, app),
        Mode::Terminal => draw_terminal(frame, app),
    }
}

fn draw_picker(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let block = Block::bordered().title(" GTUI-TERMS | select port (up/down, Enter, q) ");
    let inner = block.inner(area);
    let items: Vec<ListItem> = app
        .ports
        .iter()
        .enumerate()
        .map(|(i, port)| {
            let style = if i == app.selected {
                Style::default()
                    .bg(Color::Blue)
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            ListItem::new(format!("{:<12} {}", port.name, port.label)).style(style)
        })
        .collect();
    frame.render_widget(block, area);
    frame.render_widget(List::new(items), inner);
}

fn draw_terminal(frame: &mut Frame, app: &mut App) {
    let chunks = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(frame.area());

    app.view_height = chunks[0].height.max(1);
    match app.view {
        View::Hex => draw_hex(frame, app, chunks[0]),
        View::Text => {
            let height = usize::from(app.view_height);
            let scroll_back = app.scroll_back;
            let rows = cached_rows(app, chunks[0].width.max(1));
            let max_top = rows.len().saturating_sub(height);
            let top = max_top.saturating_sub(scroll_back.min(max_top));
            let scroll = u16::try_from(top).unwrap_or(u16::MAX);
            frame.render_widget(Paragraph::new(rows).scroll((scroll, 0)), chunks[0]);
        }
    }

    let mut status = format!(
        " {} | {} | {} lines",
        app.label,
        app.serial_label(),
        app.scrollback.len()
    );
    if app.show_timestamps {
        status.push_str(" | ts");
    }
    if app.view == View::Hex {
        status.push_str(" | HEX (F8)");
    }
    if app.raw_mode {
        status.push_str(" | RAW");
    }
    if app.scrollback.dropped() > 0 {
        status.push_str(&format!(" | dropped {}", app.scrollback.dropped()));
    }
    status.push_str(&format!(
        " | rx {} B {} B/s tx {} B",
        app.rx_bytes, app.rx_rate, app.tx_bytes
    ));
    if app.eol != Eol::CrLf {
        status.push_str(&format!(" | eol {}", app.eol.label()));
    }
    if app.levels.iter().any(|allowed| !allowed) {
        let shown: String = ['I', 'W', 'E']
            .iter()
            .zip(app.levels)
            .filter(|(_, allowed)| *allowed)
            .map(|(letter, _)| *letter)
            .collect();
        status.push_str(&format!(" | filter {shown}"));
    }
    if app.reconnecting {
        status.push_str(" | retrying");
    }
    if let Some(rep) = &app.repeat {
        let left = rep.left.map_or("inf".to_string(), |n| n.to_string());
        status.push_str(&format!(" | repeat {}ms x{left}", rep.every.as_millis()));
    }
    if let Some(query) = &app.search_query {
        status.push_str(&format!(" | /{query} ({} hits, Ctrl+N/P)", app.hits.len()));
    }
    if let Some(path) = &app.log_path {
        status.push_str(&format!(" | log {path}"));
    }
    if !app.status.is_empty() {
        status.push_str(&format!(" | {}", app.status));
    }
    frame.render_widget(
        Paragraph::new(status).style(Style::default().fg(Color::DarkGray)),
        chunks[1],
    );

    let (prompt, text): (Span, String) = if app.raw_mode {
        (
            Span::raw(""),
            "RAW — keys go to the device, Ctrl+] to leave".to_string(),
        )
    } else {
        match &app.search_input {
            Some(buf) => (Span::raw("/"), buf.iter().collect()),
            None => (Span::raw("> "), app.input.iter().collect()),
        }
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![prompt, Span::raw(text)])),
        chunks[2],
    );

    if !app.raw_mode {
        let prompt_width = if app.search_input.is_some() { 1 } else { 2 };
        let used: usize = match &app.search_input {
            Some(buf) => buf
                .iter()
                .map(|ch| UnicodeWidthChar::width(*ch).unwrap_or(0))
                .sum(),
            None => app.input[..app.cursor]
                .iter()
                .map(|ch| UnicodeWidthChar::width(*ch).unwrap_or(0))
                .sum(),
        };
        let cursor_x = u16::try_from(prompt_width + used).unwrap_or(u16::MAX);
        let cursor_x = cursor_x.min(chunks[2].width.saturating_sub(1));
        frame.set_cursor_position((chunks[2].x + cursor_x, chunks[2].y));
    }

    if app.help {
        draw_help(frame, app);
    }
}

fn key_label(entry: &crate::config::Macro) -> String {
    match entry.key {
        KeyCode::F(n) => format!("F{n}"),
        KeyCode::Char(ch) => format!("Alt+{ch}"),
        _ => "?".to_string(),
    }
}

fn draw_help(frame: &mut Frame, app: &App) {
    let mut lines = vec![
        Line::from("F1 help  F2 line/raw  F7 search  F8 hex  Ctrl+N/P next/prev hit"),
        Line::from("PageUp/PageDown scroll  Ctrl+L clear  Ctrl+T timestamps  Ctrl+Q quit"),
        Line::from("/repeat <ms> <count|0> <text>  /stop  // literal slash"),
        Line::from(""),
    ];
    if app.macros.is_empty() {
        lines.push(Line::from("no macros: add [[macros]] to .gtui-terms.toml"));
    } else {
        lines.push(Line::from("macros:"));
        for entry in &app.macros {
            lines.push(Line::from(format!(
                "  {}  {} -> {:?}",
                key_label(entry),
                entry.name,
                entry.text
            )));
        }
    }
    let area = frame.area();
    let width = 68.min(area.width.saturating_sub(4)).max(20);
    let height = u16::try_from(lines.len() + 2)
        .unwrap_or(u16::MAX)
        .min(area.height.saturating_sub(2));
    let rect = ratatui::layout::Rect::new(
        area.x + (area.width.saturating_sub(width)) / 2,
        area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    );
    frame.render_widget(ratatui::widgets::Clear, rect);
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::bordered().title(" help (Esc to close) "))
            .style(Style::default().bg(Color::Black).fg(Color::White)),
        rect,
    );
}

fn draw_hex(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let width = usize::from(area.width.max(1));
    let per_row = ((width.saturating_sub(10)) / 4).clamp(4, 16);
    let len = app.raw.len();
    let total = len.div_ceil(per_row);
    let height = usize::from(area.height.max(1));
    let max_top = total.saturating_sub(height);
    let top = max_top.saturating_sub(app.scroll_back.min(max_top));
    let base = app.raw_total - len as u64;

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(total.saturating_sub(top).min(height));
    for row in top..total.min(top + height) {
        let start = row * per_row;
        let end = (start + per_row).min(len);
        let mut text = format!("{:08x} ", base + start as u64);
        for i in start..start + per_row {
            if i < end {
                text.push_str(&format!("{:02x} ", app.raw[i]));
            } else {
                text.push_str("   ");
            }
        }
        text.push('|');
        for i in start..end {
            let byte = app.raw[i];
            text.push(if (0x20..0x7f).contains(&byte) {
                char::from(byte)
            } else {
                '.'
            });
        }
        text.push('|');
        lines.push(Line::from(Span::styled(
            text,
            Style::default().fg(Color::Gray),
        )));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn cached_rows(app: &mut App, width: u16) -> &[Line<'static>] {
    if app.cache_width != width || app.cache_version != app.version {
        let (rows, starts, total) = build_rows(app, width);
        app.cache = rows;
        app.starts = starts;
        app.total_rows = total;
        app.cache_width = width;
        app.cache_version = app.version;
    }
    &app.cache
}

fn build_rows(app: &App, width: u16) -> (Vec<Line<'static>>, Vec<usize>, usize) {
    let limit = usize::from(width.max(1));
    let mut out = Vec::with_capacity(app.scrollback.len() + 1);
    let mut starts = Vec::with_capacity(app.scrollback.len());
    let needle: Vec<char> = app
        .search_query
        .as_deref()
        .map(|q| {
            q.to_lowercase()
                .chars()
                .map(|ch| ch.to_ascii_lowercase())
                .collect()
        })
        .unwrap_or_default();
    let mut cells: Vec<(char, u8)> = Vec::new();
    for row in app.scrollback.iter() {
        starts.push(out.len());
        cells.clear();
        if app.show_timestamps {
            for ch in format_time(row.at).chars() {
                cells.push((ch, PLAIN));
            }
            cells.push((' ', PLAIN));
        }
        if needle.is_empty() && app.search_re.is_none() {
            cells.extend(row.cells.iter().map(|c| (c.ch, c.style)));
        } else {
            let mask = match &app.search_re {
                Some(re) => regex_mask(&row.cells, re),
                None => match_mask(&row.cells, &needle),
            };
            cells.extend(
                row.cells
                    .iter()
                    .enumerate()
                    .map(|(i, c)| (c.ch, if mask[i] { HIGHLIGHT } else { c.style })),
            );
        }
        push_wrapped(&mut out, &cells, limit);
    }
    let pending = app.assembler.pending();
    if !pending.is_empty() {
        cells.clear();
        cells.extend(pending.iter().map(|c: &Cell| (c.ch, c.style)));
        push_wrapped(&mut out, &cells, limit);
    }
    let total = out.len();
    (out, starts, total)
}

/// Highlights every cell covered by a regex match on the row text.
fn regex_mask(cells: &[Cell], re: &regex::Regex) -> Vec<bool> {
    let mut mask = vec![false; cells.len()];
    let text: String = cells.iter().map(|cell| cell.ch).collect();
    let spans: Vec<(usize, usize)> = re.find_iter(&text).map(|m| (m.start(), m.end())).collect();
    if spans.is_empty() {
        return mask;
    }
    let mut offset = 0usize;
    for (index, cell) in cells.iter().enumerate() {
        let end = offset + cell.ch.len_utf8();
        if spans
            .iter()
            .any(|(start, stop)| *start < end && *stop > offset)
        {
            mask[index] = true;
        }
        offset = end;
    }
    mask
}

fn push_wrapped(out: &mut Vec<Line<'static>>, cells: &[(char, u8)], limit: usize) {
    if cells.is_empty() {
        out.push(Line::default());
        return;
    }
    let mut start = 0;
    let mut used = 0;
    for (i, &(ch, _)) in cells.iter().enumerate() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > limit && i > start {
            out.push(to_line(&cells[start..i]));
            start = i;
            used = 0;
        }
        used += w;
    }
    out.push(to_line(&cells[start..]));
}

fn to_line(cells: &[(char, u8)]) -> Line<'static> {
    let mut spans = Vec::new();
    let mut text = String::new();
    let mut style = cells.first().map_or(PLAIN, |&(_, key)| key);
    for &(ch, key) in cells {
        if key != style {
            spans.push(Span::styled(std::mem::take(&mut text), style_of(style)));
            style = key;
        }
        text.push(ch);
    }
    if !text.is_empty() {
        spans.push(Span::styled(text, style_of(style)));
    }
    Line::from(spans)
}

fn format_time(at: u64) -> String {
    let millis = i64::try_from(at).unwrap_or(i64::MAX);
    match Local.timestamp_millis_opt(millis).single() {
        Some(dt) => dt.format("%H:%M:%S%.3f").to_string(),
        None => "??:??:??.???".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serial::Event;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn rendered(app: &mut App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    fn app() -> App {
        App::new(115200, Vec::new(), None, false, Vec::new(), Vec::new())
    }

    #[test]
    fn help_overlay_lists_keys_and_macros() {
        let macros = vec![crate::config::Macro {
            name: "ping".to_string(),
            text: "ping".to_string(),
            key: KeyCode::Char('p'),
            mods: KeyModifiers::ALT,
        }];
        let mut app = App::new(115200, Vec::new(), None, false, macros, Vec::new());
        app.start_demo();
        app.on_key(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE));
        let screen = rendered(&mut app, 80, 24);
        assert!(screen.contains("help (Esc to close)"), "{screen}");
        assert!(screen.contains("Alt+p"), "{screen}");
        assert!(screen.contains("/repeat"), "{screen}");
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.quit);
        assert!(!rendered(&mut app, 80, 24).contains("help (Esc to close)"));
    }

    #[test]
    fn raw_mode_row_explains_how_to_leave() {
        let mut app = app();
        app.on_key(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE));
        let screen = rendered(&mut app, 80, 12);
        assert!(screen.contains("Ctrl+] to leave"), "{screen}");
        assert!(screen.contains("RAW"), "{screen}");
    }

    #[test]
    fn terminal_view_shows_lines_and_prompt() {
        let mut app = app();
        app.on_event(Event::Data(
            b"I (1) boot: hello\r\nI (2) boot: world\r\n".to_vec(),
        ));
        for ch in "ping".chars() {
            app.on_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        let text = rendered(&mut app, 40, 8);
        assert!(text.contains("hello"));
        assert!(text.contains("world"));
        assert!(text.contains("> ping"));
    }

    #[test]
    fn carriage_return_updates_one_row() {
        let mut app = app();
        app.on_event(Event::Data(b"progress 10%\r".to_vec()));
        app.on_event(Event::Data(b"progress 90%\r\n".to_vec()));
        let text = rendered(&mut app, 40, 8);
        assert_eq!(app.scrollback.len(), 1);
        assert!(text.contains("progress 90%"));
        assert!(!text.contains("progress 10%"));
    }

    #[test]
    fn hex_view_dumps_bytes_and_ascii() {
        let mut app = app();
        app.on_event(Event::Data(b"Hello\r\n".to_vec()));
        app.on_key(KeyEvent::new(KeyCode::F(8), KeyModifiers::NONE));
        let text = rendered(&mut app, 60, 6);
        assert!(text.contains("00000000"));
        assert!(text.contains("48 65 6c 6c 6f 0d 0a"));
        assert!(text.contains("|Hello..|"));
        app.on_key(KeyEvent::new(KeyCode::F(8), KeyModifiers::NONE));
        let text = rendered(&mut app, 60, 6);
        assert!(text.contains("Hello"));
    }

    #[test]
    fn search_jumps_to_match_and_highlights() {
        let mut app = app();
        let mut data = Vec::new();
        for i in 0..200 {
            if i == 101 {
                data.extend_from_slice(b"boot: needle found\r\n");
            }
            data.extend_from_slice(format!("line {i}\r\n").as_bytes());
        }
        app.on_event(Event::Data(data));
        rendered(&mut app, 40, 10);
        app.on_key(KeyEvent::new(KeyCode::F(7), KeyModifiers::NONE));
        for ch in "needle".chars() {
            app.on_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.hits.len(), 1);
        assert!(app.scroll_back > 0);
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        let text: String = buffer.content().iter().map(|cell| cell.symbol()).collect();
        assert!(text.contains("needle found"));
        assert!(!text.contains("line 199"));
        assert!(buffer.content().iter().any(|cell| cell.bg == Color::Yellow));
    }

    #[test]
    fn regex_search_highlights_exactly_the_match() {
        let mut app = app();
        app.on_event(Event::Data(b"error code 12\r\nall good\r\n".to_vec()));
        app.search_query = Some(r"re:code \d+".to_string());
        app.run_search();
        assert_eq!(app.hits.len(), 1);
        let backend = TestBackend::new(40, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        let highlighted: String = buffer
            .content()
            .iter()
            .filter(|cell| cell.bg == Color::Yellow)
            .map(|cell| cell.symbol())
            .collect();
        assert_eq!(highlighted, "code 12", "only the match is inverted");
    }

    #[test]
    fn long_lines_wrap_and_stay_on_screen() {
        let mut app = app();
        app.on_event(Event::Data(
            b"0123456789abcdefghij0123456789abcdefghij\r\n".to_vec(),
        ));
        let text = rendered(&mut app, 10, 4);
        assert!(text.contains("0123456789"));
        assert!(text.contains("abcdefghij"));
    }

    #[test]
    fn timestamps_prefix_rows_when_enabled() {
        let mut app = app();
        app.on_event(Event::Data(b"payload\r\n".to_vec()));
        app.on_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        let text = rendered(&mut app, 40, 6);
        assert!(text.contains("payload"));
        assert!(text.contains(':') && text.contains('.'));
    }

    #[test]
    fn bottom_line_is_visible_after_overflow() {
        let mut app = app();
        for i in 0..20 {
            app.on_event(Event::Data(format!("line {i}\r\n").into_bytes()));
        }
        let text = rendered(&mut app, 40, 8);
        assert!(text.contains("line 19"));
    }
}
