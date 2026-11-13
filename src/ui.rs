use chrono::{Local, TimeZone};
use ratatui::Frame;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};
use unicode_width::UnicodeWidthChar;

use crate::app::{App, Eol, Focus, Hit, Mode, Popup, View};
use crate::term::scrollback::{Cell, HIGHLIGHT, PLAIN, match_mask};

pub fn draw(frame: &mut Frame, app: &mut App) {
    app.hits_ui.clear();
    let area = frame.area();
    let header = draw_toolbar(frame, app);
    let remaining = area.height.saturating_sub(header);
    let show_hint = area.height >= 6;
    let body = Rect::new(
        area.x,
        area.y + header,
        area.width,
        remaining.saturating_sub(u16::from(show_hint)),
    );
    match app.mode {
        Mode::Picker => draw_picker(frame, app, body),
        Mode::Terminal => draw_terminal(frame, app, body),
    }
    if show_hint && remaining > 0 {
        let hint = format!(
            " Tab: toolbar/output/input | focus {:?} | click input to send | F1 /help | Ctrl+Q quit",
            app.focus
        );
        frame.render_widget(
            Paragraph::new(hint).style(Style::default().fg(Color::Cyan)),
            Rect::new(area.x, area.bottom() - 1, area.width, 1),
        );
    }
    if app.help {
        draw_help(frame, app);
    } else if app.popup.is_some() {
        draw_popup(frame, app);
    }
    if let Some(dialog) = &mut app.settings_dialog {
        dialog.draw(frame);
    }
}

fn draw_toolbar(frame: &mut Frame, app: &mut App) -> u16 {
    let area = frame.area();
    if area.width == 0 || area.height == 0 {
        return 0;
    }
    let labels = [
        "Ports ^O",
        "Settings F3",
        "Actions F4",
        "ESP",
        "Help F1",
        "Quit ^Q",
        "Save ^S",
        if app.scroll_locked {
            "Lock:on ^B"
        } else {
            "Lock:off ^B"
        },
        if app.show_packets {
            "Packets:on"
        } else {
            "Packets:off"
        },
        if app.show_timestamps {
            "Time:on ^T"
        } else {
            "Time:off ^T"
        },
        "Clear ^L",
    ];
    let mut start = 0;
    if app.focus == Focus::Toolbar {
        while start < app.toolbar_selected
            && labels[start..=app.toolbar_selected]
                .iter()
                .map(|label| label.len() + 3)
                .sum::<usize>()
                > usize::from(area.width)
        {
            start += 1;
        }
    }
    let mut x = area.x;
    for (index, label) in labels.iter().enumerate().skip(start) {
        if x >= area.right() {
            break;
        }
        let text = format!(" {label} ");
        let width = (text.len() as u16).min(area.right() - x);
        let rect = Rect::new(x, area.y, width, 1);
        let style = if app.focus == Focus::Toolbar && app.toolbar_selected == index {
            Style::default()
                .bg(Color::Blue)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().bg(Color::DarkGray).fg(Color::White)
        };
        frame.render_widget(Paragraph::new(text).style(style), rect);
        app.hits_ui.push((rect, Hit::Toolbar(index)));
        x = x.saturating_add(width).saturating_add(1);
    }
    1
}

fn draw_picker(frame: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::bordered().title(" Ports — live refresh 500ms | Enter/click: settings ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(inner);
    frame.render_widget(
        Paragraph::new("[ Refresh (R) ]  Up/Down: select  Esc: return")
            .style(Style::default().fg(Color::Cyan)),
        chunks[0],
    );
    app.hits_ui.push((
        Rect::new(
            chunks[0].x,
            chunks[0].y,
            chunks[0].width.min(15),
            chunks[0].height,
        ),
        Hit::Refresh,
    ));
    if app.ports.is_empty() {
        frame.render_widget(
            Paragraph::new(
                "No serial ports yet. Plug in a USB device; this list updates automatically.",
            )
            .wrap(Wrap { trim: false }),
            chunks[1],
        );
    } else {
        let visible = usize::from(chunks[1].height);
        let start = app.selected.saturating_sub(visible.saturating_sub(1));
        let items: Vec<ListItem> = app
            .ports
            .iter()
            .enumerate()
            .skip(start)
            .take(visible)
            .map(|(index, port)| {
                let row = chunks[1].y + (index - start) as u16;
                app.hits_ui.push((
                    Rect::new(chunks[1].x, row, chunks[1].width, 1),
                    Hit::Port(index),
                ));
                let style = if index == app.selected {
                    Style::default().bg(Color::Blue).fg(Color::White)
                } else {
                    Style::default()
                };
                ListItem::new(format!("{:<12} {}", port.name, port.label)).style(style)
            })
            .collect();
        frame.render_widget(List::new(items), chunks[1]);
    }
    frame.render_widget(
        Paragraph::new(app.status.as_str()).style(Style::default().fg(Color::Yellow)),
        chunks[2],
    );
}

/// 输入框最多显示的文本行数，超出时在框内滚动。
const INPUT_MAX_ROWS: usize = 8;
/// 输入框文本前的提示符宽度（"› " 或续行缩进）。
const INPUT_PROMPT: u16 = 2;

fn input_height(app: &App, area: Rect) -> u16 {
    // 太矮时退化为无边框单行，保证输出区仍可见。
    if area.height < 8 {
        return 1;
    }
    let width = area.width.saturating_sub(2 + INPUT_PROMPT);
    let rows = if app.raw_mode || app.search_input.is_some() {
        1
    } else {
        app.input.rows(usize::from(width)).len()
    };
    let cap = INPUT_MAX_ROWS.min(usize::from(area.height / 3).max(1));
    u16::try_from(rows.clamp(1, cap)).unwrap_or(1) + 2
}

fn draw_terminal(frame: &mut Frame, app: &mut App, area: Rect) {
    let chunks = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(u16::from(area.height >= 4)),
        Constraint::Length(input_height(app, area)),
    ])
    .split(area);

    app.view_height = chunks[0].height.max(1);
    app.hits_ui.push((chunks[2], Hit::Input));
    draw_streams(frame, app, chunks[0]);

    let mut status = format!(
        " {} | {} | {} | {} lines",
        app.status,
        app.label,
        app.serial_label(),
        app.scrollback.len()
    );
    if app.show_timestamps {
        status.push_str(" | ts");
    }
    status.push_str(if app.scroll_locked {
        " | LOCK"
    } else {
        " | follow"
    });
    if app.show_packets {
        status.push_str(&format!(" | packets {}ms", app.packet_gap_ms));
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
    if app.assembler.invalid_utf8() > 0 {
        status.push_str(&format!(
            " | invalid UTF-8 {}",
            app.assembler.invalid_utf8()
        ));
    }
    status.push_str(&format!(
        " | rx {} B {} B/s tx {} B",
        app.rx_bytes, app.rx_rate, app.tx_bytes
    ));
    if app.assembler.cr_newline() {
        status.push_str(" | rx CR=newline");
    }
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
    frame.render_widget(
        Paragraph::new(status).style(Style::default().fg(Color::DarkGray)),
        chunks[1],
    );

    draw_input(frame, app, chunks[2]);
}

fn draw_input(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Input;
    let accent = if app.raw_mode {
        Color::Magenta
    } else if app.search_input.is_some() {
        Color::Yellow
    } else {
        Color::Cyan
    };
    let inner = if area.height >= 3 {
        let mode = if app.raw_mode {
            " RAW ".to_string()
        } else if app.search_input.is_some() {
            " SEARCH ".to_string()
        } else if app.tx_hex {
            " TX · HEX ".to_string()
        } else {
            format!(
                " TX · {} · {} ",
                if app.tx_escapes { "TEXT+ESC" } else { "TEXT" },
                app.eol.label().to_uppercase()
            )
        };
        let hint = if app.raw_mode {
            " Ctrl+] leave "
        } else if app.search_input.is_some() {
            " Enter find · Esc cancel "
        } else {
            " Enter send · Alt+Enter newline · ^X hex · ^R eol "
        };
        let border = if focused {
            Style::default().fg(accent)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let mut block = Block::default()
            .borders(Borders::ALL)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .border_style(border)
            .title(Span::styled(mode, border.add_modifier(Modifier::BOLD)));
        if focused && area.width > 60 {
            block = block.title_bottom(Line::from(hint).right_aligned());
        }
        if !app.raw_mode && app.search_input.is_none() && !app.input.is_empty() {
            let text = app.input.text();
            let count = if app.tx_hex && !text.starts_with('/') {
                match crate::app::parse_hex(&text) {
                    Ok(bytes) => Span::raw(format!(" {} B ", bytes.len())),
                    Err(_) => Span::styled(" invalid hex ", Style::default().fg(Color::Red)),
                }
            } else {
                Span::raw(format!(" {} B ", text.len()))
            };
            block = block.title(Line::from(count).right_aligned());
        }
        let inner = block.inner(area);
        frame.render_widget(block, area);
        inner
    } else {
        area
    };
    let text_width = inner.width.saturating_sub(INPUT_PROMPT);
    app.input_width = text_width.max(1);
    let prompt_style = Style::default().fg(accent).add_modifier(Modifier::BOLD);

    let (lines, cursor): (Vec<Line>, Option<(u16, u16)>) = if app.raw_mode {
        (
            vec![Line::from(vec![
                Span::styled("» ", prompt_style),
                Span::styled(
                    "keys go straight to the device",
                    Style::default().fg(Color::DarkGray),
                ),
            ])],
            None,
        )
    } else if let Some(buf) = &app.search_input {
        let text: String = buf.iter().collect();
        let used: usize = buf.iter().map(|ch| crate::input::width_of(*ch)).sum();
        (
            vec![Line::from(vec![
                Span::styled("/ ", prompt_style),
                Span::raw(text),
            ])],
            Some((u16::try_from(used).unwrap_or(u16::MAX), 0)),
        )
    } else if app.input.is_empty() {
        (
            vec![Line::from(vec![
                Span::styled("› ", prompt_style),
                Span::styled(
                    "type data to send, /help for commands",
                    Style::default().fg(Color::DarkGray),
                ),
            ])],
            Some((0, 0)),
        )
    } else {
        let width = usize::from(text_width.max(1));
        let rows = app.input.rows(width);
        let (row, col) = app.input.cursor_position(width);
        let visible = usize::from(inner.height.max(1));
        let offset = row.saturating_sub(visible - 1);
        let chars = app.input.chars();
        let lines = rows
            .iter()
            .enumerate()
            .skip(offset)
            .take(visible)
            .map(|(index, visual)| {
                let lead = if index == 0 {
                    Span::styled("› ", prompt_style)
                } else {
                    Span::raw("  ")
                };
                let text: String = chars[visual.start..visual.end]
                    .iter()
                    .map(|ch| {
                        if *ch == '\t' {
                            "    ".to_string()
                        } else {
                            ch.to_string()
                        }
                    })
                    .collect();
                Line::from(vec![lead, Span::raw(text)])
            })
            .collect();
        (
            lines,
            Some((
                u16::try_from(col).unwrap_or(u16::MAX),
                u16::try_from(row - offset).unwrap_or(0),
            )),
        )
    };
    frame.render_widget(Paragraph::new(lines), inner);

    if let Some((x, y)) = cursor
        && focused
        && !app.help
        && app.popup.is_none()
        && app.settings_dialog.is_none()
        && inner.width > 0
        && inner.height > 0
    {
        let x = (INPUT_PROMPT + x).min(inner.width.saturating_sub(1));
        let y = y.min(inner.height - 1);
        frame.set_cursor_position((inner.x + x, inner.y + y));
    }
}

fn draw_streams(frame: &mut Frame, app: &mut App, area: Rect) {
    let split = area.width >= 72 || !app.tx_packets.is_empty();
    let panes = Layout::horizontal([
        Constraint::Percentage(if split { 65 } else { 100 }),
        Constraint::Min(0),
    ])
    .split(area);
    let mut rx_block = Block::new();
    if area.height >= 3 {
        rx_block = rx_block.borders(Borders::TOP).title(if app.show_packets {
            " RX — read chunks "
        } else {
            " RX — received "
        });
    }
    let rx_area = rx_block.inner(panes[0]);
    frame.render_widget(rx_block, panes[0]);
    app.view_height = rx_area.height.max(1);
    app.hits_ui.push((rx_area, Hit::Output));
    match app.view {
        View::Hex if !app.show_packets => draw_hex(frame, app, rx_area),
        _ => {
            cached_rows(app, rx_area.width.max(1));
            let max_top = app.cache.len().saturating_sub(usize::from(app.view_height));
            let top = max_top.saturating_sub(app.scroll_back.min(max_top));
            frame.render_widget(
                Paragraph::new(app.cache.as_slice())
                    .scroll((u16::try_from(top).unwrap_or(u16::MAX), 0)),
                rx_area,
            );
        }
    }
    if !split {
        return;
    }
    let mut block = Block::new().borders(Borders::LEFT);
    if area.height >= 3 {
        block = block
            .borders(Borders::LEFT | Borders::TOP)
            .title(" TX — sent successfully ");
    }
    let tx_area = block.inner(panes[1]);
    frame.render_widget(block, panes[1]);
    app.hits_ui.push((tx_area, Hit::TxOutput));
    let key = (
        app.tx_bytes,
        tx_area.width,
        app.show_timestamps,
        app.show_packets,
        app.view,
    );
    if app.tx_cache.key != Some(key) {
        let mut rows = Vec::new();
        let mut starts = Vec::new();
        for packet in &app.tx_packets {
            starts.push(rows.len());
            packet_rows(
                &mut rows,
                packet,
                "TX",
                tx_area.width,
                app.show_timestamps,
                app.show_packets,
                app.view,
            );
        }
        let removed = app.tx_packet_dropped.saturating_sub(app.tx_cache.dropped);
        let removed_rows = if removed == 0 {
            0
        } else {
            app.tx_cache
                .starts
                .get(removed)
                .copied()
                .unwrap_or(app.tx_cache.rows.len())
        };
        preserve_scroll(
            &mut app.tx_scroll_back,
            app.tx_cache.rows.len(),
            rows.len(),
            usize::from(tx_area.height),
            removed_rows,
            app.scroll_locked,
        );
        app.tx_cache.rows = rows;
        app.tx_cache.starts = starts;
        app.tx_cache.key = Some(key);
        app.tx_cache.dropped = app.tx_packet_dropped;
    }
    let max_top = app
        .tx_cache
        .rows
        .len()
        .saturating_sub(usize::from(tx_area.height));
    let top = max_top.saturating_sub(app.tx_scroll_back.min(max_top));
    let end = top
        .saturating_add(usize::from(tx_area.height))
        .min(app.tx_cache.rows.len());
    frame.render_widget(Paragraph::new(&app.tx_cache.rows[top..end]), tx_area);
}

fn preserve_scroll(
    scroll: &mut usize,
    old: usize,
    new: usize,
    height: usize,
    removed: usize,
    locked: bool,
) {
    if locked || *scroll > 0 {
        let top = old
            .saturating_sub(height)
            .saturating_sub(*scroll)
            .saturating_sub(removed);
        *scroll = new.saturating_sub(height).saturating_sub(top);
    }
}

/// Dim style for timestamp prefixes: a separate column that never takes the data's colour.
const TIME_STYLE: Style = Style::new().fg(Color::DarkGray);
/// Escaped line-ending markers in packet view, shown but de-emphasised.
const ESCAPE_STYLE: Style = Style::new().fg(Color::DarkGray);

fn time_prefix(cells: &mut Vec<(char, Style)>, at: u64) {
    cells.extend(format_time(at).chars().map(|ch| (ch, TIME_STYLE)));
    cells.push((' ', PLAIN));
}

/// Renders one packet: a `RX 32B` header, then its content broken after every
/// `\n` so text protocols stay readable. Continuation rows are indented under
/// the content, and each line is coloured by its ESP-IDF log level.
fn packet_rows(
    out: &mut Vec<Line<'static>>,
    packet: &crate::app::Packet,
    direction: &str,
    width: u16,
    timestamps: bool,
    packets: bool,
    view: View,
) {
    let limit = usize::from(width.max(1));
    let mut header: Vec<(char, Style)> = Vec::new();
    if timestamps {
        time_prefix(&mut header, packet.at);
    }
    if packets {
        let style = Style::new().fg(if direction == "TX" {
            Color::Magenta
        } else {
            Color::Cyan
        });
        header.extend(
            format!("{direction} {}B ", packet.bytes.len())
                .chars()
                .map(|ch| (ch, style)),
        );
    }
    let indent = header
        .iter()
        .map(|&(ch, _)| UnicodeWidthChar::width(ch).unwrap_or(0))
        .sum::<usize>();
    // 缩进过宽时（窄窗口）放弃缩进，避免续行没有可用宽度。
    let indent = if indent * 2 > limit { 0 } else { indent };
    let mut lines: Vec<Vec<(char, Style)>> = Vec::new();
    if view == View::Hex {
        let mut line = Vec::new();
        for (index, byte) in packet.bytes.iter().enumerate() {
            if index > 0 {
                line.push((' ', PLAIN));
            }
            line.extend(format!("{byte:02x}").chars().map(|ch| (ch, PLAIN)));
        }
        lines.push(line);
    } else {
        let mut line: Vec<(char, Style)> = Vec::new();
        let finish = |line: &mut Vec<(char, Style)>, lines: &mut Vec<Vec<(char, Style)>>| {
            let cells: Vec<Cell> = line.iter().map(|&(ch, style)| Cell { ch, style }).collect();
            if let Some(color) = crate::esp::log::color(&cells) {
                for cell in line.iter_mut() {
                    if cell.1 == PLAIN {
                        cell.1 = Style::new().fg(color);
                    }
                }
            }
            lines.push(std::mem::take(line));
        };
        let mut remaining = packet.bytes.as_slice();
        while !remaining.is_empty() {
            let (valid, consumed) = match std::str::from_utf8(remaining) {
                Ok(text) => (text, remaining.len()),
                Err(err) => (
                    std::str::from_utf8(&remaining[..err.valid_up_to()]).unwrap_or_default(),
                    err.valid_up_to(),
                ),
            };
            for ch in valid.chars() {
                let escaped = match ch {
                    '\r' => "\\r".to_string(),
                    '\n' => "\\n".to_string(),
                    '\t' => "\\t".to_string(),
                    '\\' => "\\\\".to_string(),
                    ch if ch.is_control() => ch.escape_default().to_string(),
                    ch => {
                        line.push((ch, PLAIN));
                        continue;
                    }
                };
                line.extend(escaped.chars().map(|ch| (ch, ESCAPE_STYLE)));
                if ch == '\n' {
                    finish(&mut line, &mut lines);
                }
            }
            remaining = &remaining[consumed..];
            if let Some((&byte, rest)) = remaining.split_first() {
                line.extend(
                    format!("\\x{byte:02X}")
                        .chars()
                        .map(|ch| (ch, ESCAPE_STYLE)),
                );
                remaining = rest;
            }
        }
        if !line.is_empty() || lines.is_empty() {
            finish(&mut line, &mut lines);
        }
    }
    for (index, line) in lines.into_iter().enumerate() {
        let mut cells = if index == 0 {
            header.clone()
        } else {
            vec![(' ', PLAIN); indent]
        };
        let body = limit.saturating_sub(indent).max(1);
        // 内容按剩余宽度折行，每个折行都对齐到内容列。
        let mut used = 0;
        for (ch, style) in line {
            let w = UnicodeWidthChar::width(ch).unwrap_or(0);
            if used + w > body && used > 0 {
                out.push(to_line(&cells));
                cells = vec![(' ', PLAIN); indent];
                used = 0;
            }
            cells.push((ch, style));
            used += w;
        }
        out.push(to_line(&cells));
    }
}

fn key_label(entry: &crate::config::Macro) -> String {
    match entry.key {
        KeyCode::F(n) => format!("F{n}"),
        KeyCode::Char(ch) => format!("Alt+{ch}"),
        _ => "?".to_string(),
    }
}

fn popup_frame(frame: &mut Frame, app: &mut App, title: &str, width: u16, height: u16) -> Rect {
    let area = frame.area();
    let width = width.min(area.width);
    let height = height.min(area.height);
    let rect = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    app.popup_rect = rect;
    frame.render_widget(Clear, rect);
    let block = Block::bordered()
        .title(title)
        .style(Style::default().bg(Color::Black).fg(Color::White));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    if width >= 5 && height > 0 {
        let close = Rect::new(rect.right() - 5, rect.y, 4, 1);
        frame.render_widget(
            Paragraph::new("[X]").style(Style::default().fg(Color::Yellow)),
            close,
        );
        app.hits_ui.push((close, Hit::Close));
    }
    inner
}

fn wrapped_text(text: impl IntoIterator<Item = String>, width: u16) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for text in text {
        let cells: Vec<_> = text.chars().map(|ch| (ch, PLAIN)).collect();
        push_wrapped(&mut lines, &cells, usize::from(width.max(1)));
    }
    lines
}

fn draw_help(frame: &mut Frame, app: &mut App) {
    let height = frame.area().height.saturating_sub(2).max(1);
    let inner = popup_frame(frame, app, " help (Esc to close) ", 90, height);
    let mut text = vec![
        "GTUI-TERMS — click the toolbar or use Tab, arrows and Enter.".to_string(),
        "Ports / Ctrl+O: choose a live port, review Settings, then Apply to connect.".to_string(),
        "F3 Settings | F4 Actions | F5 reset | F6 bootloader | F1 help".to_string(),
        "Click output to read safely; click input to type/send. Tab cycles focus.".to_string(),
        "F2 toggles RAW input; Ctrl+] leaves RAW. F8 toggles text/HEX output.".to_string(),
        "PageUp/Down and mouse wheel scroll. Ctrl+Home/End: oldest/live output.".to_string(),
        "F7 search: type text or re:regex, Enter applies, Esc cancels. Ctrl+N/P: hits.".to_string(),
        "Ctrl+L clears output | Ctrl+T timestamps | Ctrl+S saves full session | Ctrl+B locks scroll.".to_string(),
        "Save/Lock/Packets toolbar controls work without typing commands. Ctrl+Q quits after active esptool finishes.".to_string(),
        "/repeat 1000 5 AT sends AT five times; /repeat 1000 0 AT repeats until /stop.".to_string(),
        "/macro ping alt+p AT saves a macro (keys F9-F12 or Alt+character).".to_string(),
    ];
    for entry in &app.macros {
        text.push(format!(
            "Macro {}: {} -> {:?}",
            entry.name,
            key_label(entry),
            entry.text
        ));
    }
    text.extend([
        "Line mode: Enter sends with selected EOL; Up/Down move between input rows, then recall history.",
        "Newline in input: Shift+Enter (if the terminal reports it), Alt+Enter or Ctrl+J; pasted text keeps its lines.",
        "Editing: Ctrl+A/E line start/end, Ctrl+W or Ctrl+Backspace delete word, Ctrl+U/K delete to start/end, Ctrl+Left/Right word.",
        "Multi-line data is sent with the selected EOL between and after lines; multi-line /commands run line by line.",
        "Ctrl+X or /txmode [text|hex] toggles HEX send (AA55 or 0xAA,0x55; no EOL added); Ctrl+R cycles EOL.",
        "/escapes [on|off] parses \\r \\n \\t \\0 \\\\ \\xHH in TEXT sends | /rxcr [newline|overwrite] handles lone CR.",
        "/dtr on|off, /rts on|off set modem lines | /counters reset zeroes RX/TX byte totals.",
        "//help sends literal /help; a single leading slash runs a local command.",
        "/clear (Ctrl+L, toolbar Clear) empties the screen; capture and /save keep everything.",
        "/help opens this help | /settings opens serial settings | /ports opens ports.",
        "/esp opens ESP-IDF tools | /elf build/app.elf loads matching debug symbols.",
        "/reset resets | /boot or /bootloader enters the ROM bootloader.",
        "ESP tools are built in (espflash): no Python, esptool or ESP-IDF install is needed.",
        "/esp chip-id|flash-id|read-mac runs read-only ROM queries; /esp erase-flash requires confirmation.",
        "/esp flash build flashes an ESP-IDF build dir via flasher_args.json; /esp flash app.elf builds bootloader+partitions+app.",
        "/esp write-flash 0x1000 \"bootloader.bin\" 0x10000 \"app.bin\" writes raw images | /esp baud 921600 sets flash speed.",
        "/confirm executes the pending destructive operation; /cancel discards it.",
        "/save [path] exports the full session; existing files are never overwritten. Records contain UTC time, RX/TX/TOOL, exact HEX and escaped text.",
        "/lock on|off freezes/resumes viewport | /timestamps on|off | /packets on|off | /packets gap <ms>.",
        "RX is left, confirmed TX is right. Packets are serial read/write chunks, not protocol frames. Mouse wheel scrolls each pane.",
        "/eol crlf|lf|cr|none changes the line ending; example: /eol lf.",
        "/hex 01 0d 0a sends exact bytes without adding EOL (also accepts 0x41,42).",
        "/filter i,w,e selects ESP log levels for new rows; /filter all restores all.",
        "/seq D0 R1 W0.1 R0 runs DTR/RTS/wait steps (W is seconds).",
        "/hooks lists configured hooks | /hook build runs the hook named build.",
        "ESP: panic detection runs automatically. Matching application ELF with debug symbols enables address/source decoding in output; a mismatched SHA is flagged.",
        "ESP coredumps: enable UART base64 coredumps in firmware; captures are saved beside the raw log, or in the working directory, and their location appears in output.",
        "Hooks run the actual configured shell command; install its tools (for example idf.py/esptool) and make them available on PATH. Select a hook explicitly to run it. Existing configured regex triggers also apply.",
        "Settings: baud, bits, parity, stop bits, flow, line ending, and reset-on-connect. Apply reconnects hardware; Cancel keeps current settings.",
        "Actions: reconnect/disconnect, clear output, timestamps, HEX, search and RAW.",
        "Help scroll: Up/Down, PageUp/Down, Home/End or mouse wheel. Esc/F1, [X] or an outside click closes. Typing here never reaches the device.",
    ].into_iter().map(str::to_string));
    let lines = wrapped_text(text, inner.width);
    app.help_height = usize::from(inner.height.max(1));
    app.help_max_scroll = lines.len().saturating_sub(app.help_height);
    app.help_scroll = app.help_scroll.min(app.help_max_scroll);
    frame.render_widget(
        Paragraph::new(lines).scroll((u16::try_from(app.help_scroll).unwrap_or(u16::MAX), 0)),
        inner,
    );
}

fn draw_prompt(frame: &mut Frame, app: &mut App) {
    let confirm = app.popup == Some(Popup::ConfirmTool);
    let (title, guidance, button, height) = match app.popup {
        Some(Popup::Save) => (
            " Save current session — Esc cancels ",
            "Destination path. Exports all captured RX/TX/tool records since startup; never overwrites an existing file.",
            " Save session ",
            10,
        ),
        Some(Popup::Flash) => (
            " Flash images — Esc cancels ",
            "One path flashes a project: ESP-IDF build dir (flasher_args.json) or app .elf (bootloader + partition table added automatically). Or <offset> <file> pairs for raw images. Enter previews; nothing is flashed yet.",
            " Preview flash ",
            12,
        ),
        _ => (
            " Confirm destructive ESP operation — Esc cancels ",
            "This operation changes device Flash. Verify target, offsets and image paths below. Up/Down, PgUp/PgDn scroll; Enter executes.",
            " Execute operation ",
            22,
        ),
    };
    let inner = popup_frame(frame, app, title, 100, height);
    let chunks = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(2),
    ])
    .split(inner);
    frame.render_widget(
        Paragraph::new(guidance).wrap(Wrap { trim: false }),
        chunks[0],
    );
    if confirm {
        let preview = app
            .pending_tool
            .as_ref()
            .map(|tool| tool.summary.clone())
            .unwrap_or_default();
        let lines = wrapped_text(preview.lines().map(str::to_string), chunks[1].width);
        app.popup_selected = app
            .popup_selected
            .min(lines.len().saturating_sub(usize::from(chunks[1].height)));
        frame.render_widget(
            Paragraph::new(lines)
                .scroll((u16::try_from(app.popup_selected).unwrap_or(u16::MAX), 0)),
            chunks[1],
        );
    } else {
        let width = usize::from(chunks[1].width.saturating_sub(2));
        let chars: Vec<char> = app.prompt_input.chars().collect();
        let shown: String = chars[chars.len().saturating_sub(width)..].iter().collect();
        frame.render_widget(Paragraph::new(format!("> {shown}")), chunks[1]);
        if chunks[1].width > 2 && chunks[1].height > 0 {
            let cursor = shown
                .chars()
                .filter_map(UnicodeWidthChar::width)
                .sum::<usize>()
                .min(usize::from(chunks[1].width.saturating_sub(3)));
            frame.set_cursor_position(Position::new(chunks[1].x + 2 + cursor as u16, chunks[1].y));
        }
    }
    frame.render_widget(
        Paragraph::new(button).style(Style::new().fg(Color::Black).bg(if confirm {
            Color::Yellow
        } else {
            Color::Cyan
        })),
        chunks[2],
    );
    app.hits_ui.push((chunks[2], Hit::DialogApply));
    frame.render_widget(
        Paragraph::new(app.status.clone()).wrap(Wrap { trim: false }),
        chunks[3],
    );
}

fn draw_popup(frame: &mut Frame, app: &mut App) {
    if matches!(
        app.popup,
        Some(Popup::Save | Popup::Flash | Popup::ConfirmTool)
    ) {
        draw_prompt(frame, app);
        return;
    }
    if app.popup == Some(Popup::Elf) {
        let inner = popup_frame(frame, app, " Load application ELF — Esc cancels ", 86, 8);
        let chunks = Layout::vertical([
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(inner);
        frame.render_widget(
            Paragraph::new("Enter the matching application .elf path. Ctrl+U clears; Enter loads.")
                .wrap(Wrap { trim: false }),
            chunks[0],
        );
        let text = format!("> {}", app.elf_input);
        let used: usize = text
            .chars()
            .map(|ch| UnicodeWidthChar::width(ch).unwrap_or(0))
            .sum();
        let offset = used.saturating_sub(usize::from(chunks[1].width.saturating_sub(1)));
        frame.render_widget(
            Paragraph::new(text)
                .scroll((0, u16::try_from(offset).unwrap_or(u16::MAX)))
                .style(Style::default().bg(Color::DarkGray)),
            chunks[1],
        );
        frame.render_widget(
            Paragraph::new("[ Load ELF ]").style(Style::default().fg(Color::Cyan)),
            chunks[2],
        );
        app.hits_ui.push((chunks[2], Hit::ElfApply));
        frame.render_widget(
            Paragraph::new(app.status.as_str()).wrap(Wrap { trim: false }),
            chunks[3],
        );
        if chunks[1].width > 0 && chunks[1].height > 0 {
            frame.set_cursor_position((
                chunks[1].x + (used.saturating_sub(offset) as u16).min(chunks[1].width - 1),
                chunks[1].y,
            ));
        }
        return;
    }
    let items = app.menu_items();
    let esp = app.popup == Some(Popup::Esp);
    let height = u16::try_from(items.len() + if esp { 14 } else { 4 }).unwrap_or(u16::MAX);
    let inner = popup_frame(
        frame,
        app,
        if esp {
            " ESP-IDF tools — Esc closes "
        } else {
            " Actions — arrows/Enter or click "
        },
        if esp { 100 } else { 58 },
        height,
    );
    let info_height = if esp {
        inner.height.saturating_sub(4).min(10)
    } else {
        0
    };
    let chunks = Layout::vertical([
        Constraint::Length(info_height),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(inner);
    if esp {
        let elf = app
            .elf_path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| {
                "none — load matching application ELF for source locations".to_string()
            });
        let mut info = vec![
            format!(
                "Device: {} — {}",
                if app.label.is_empty() {
                    "none"
                } else {
                    &app.label
                },
                app.status
            ),
            format!("ELF: {elf}"),
            "Panic decoding: automatic in output; needs matching ELF/debug symbols.".to_string(),
            format!(
                "Coredump: requires firmware UART/base64; auto-save -> {}",
                app.coredump_directory().display()
            ),
            format!("Last detection: {}", app.esp_status),
            "Reset/bootloader requires board-specific DTR/RTS wiring to EN/boot pins.".to_string(),
            "Hooks need their configured tools on PATH. Select then Enter/click to run."
                .to_string(),
        ];
        if items.len() == 8 {
            info.push(
                "No hooks configured: add [[hooks]] with name and command to config.".to_string(),
            );
        }
        if let Some(name) = app.active_hook() {
            info.push(format!("Running hook: {name}"));
        }
        frame.render_widget(
            Paragraph::new(wrapped_text(info, chunks[0].width)),
            chunks[0],
        );
    }
    let visible = usize::from(chunks[1].height);
    let start = app.popup_selected.saturating_sub(visible.saturating_sub(1));
    let rows: Vec<_> = items
        .into_iter()
        .enumerate()
        .skip(start)
        .take(visible)
        .map(|(index, text)| {
            let rect = Rect::new(
                chunks[1].x,
                chunks[1].y + (index - start) as u16,
                chunks[1].width,
                1,
            );
            app.hits_ui.push((rect, Hit::Menu(index)));
            ListItem::new(text).style(if index == app.popup_selected {
                Style::default().bg(Color::Blue).fg(Color::White)
            } else {
                Style::default()
            })
        })
        .collect();
    frame.render_widget(List::new(rows), chunks[1]);
    frame.render_widget(
        Paragraph::new("Up/Down: choose  Enter: run  Esc/[X]: close")
            .style(Style::default().fg(Color::Cyan)),
        chunks[2],
    );
}

fn draw_hex(frame: &mut Frame, app: &mut App, area: ratatui::layout::Rect) {
    let width = usize::from(area.width.max(1));
    let per_row = ((width.saturating_sub(10)) / 4).clamp(4, 16);
    let len = app.raw.len();
    let total = len.div_ceil(per_row);
    let base = app.raw_total - len as u64;
    let removed = (base as usize).saturating_sub(app.cache_dropped) / per_row;
    preserve_scroll(
        &mut app.scroll_back,
        app.total_rows,
        total,
        usize::from(area.height.max(1)),
        removed,
        app.scroll_locked,
    );
    app.cache_dropped = base as usize;
    app.total_rows = total;
    let height = usize::from(area.height.max(1));
    let max_top = total.saturating_sub(height);
    let top = max_top.saturating_sub(app.scroll_back.min(max_top));

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(total.saturating_sub(top).min(height));
    for row in top..total.min(top + height) {
        let start = row * per_row;
        let end = (start + per_row).min(len);
        let mut text = format!("{:08x} ", base + start as u64);
        for byte in app.raw.range(start..end) {
            text.push_str(&format!("{byte:02x} "));
        }
        // 末行不足 per_row 时补空格，保持 ASCII 列对齐
        for _ in end - start..per_row {
            text.push_str("   ");
        }
        text.push('|');
        for &byte in app.raw.range(start..end) {
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
        let dropped = if app.show_packets {
            app.rx_packet_dropped
        } else {
            app.scrollback.dropped()
        };
        let removed = dropped.saturating_sub(app.cache_dropped);
        let removed_rows = if removed == 0 {
            0
        } else {
            app.starts.get(removed).copied().unwrap_or(app.cache.len())
        };
        preserve_scroll(
            &mut app.scroll_back,
            app.cache.len(),
            rows.len(),
            usize::from(app.view_height),
            removed_rows,
            app.scroll_locked,
        );
        app.cache_dropped = dropped;
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
    if app.show_packets {
        for packet in &app.rx_packets {
            starts.push(out.len());
            packet_rows(
                &mut out,
                packet,
                "RX",
                width,
                app.show_timestamps,
                true,
                app.view,
            );
        }
        let total = out.len();
        return (out, starts, total);
    }
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
    let mut cells: Vec<(char, Style)> = Vec::new();
    for row in app.scrollback.iter() {
        starts.push(out.len());
        cells.clear();
        if app.show_timestamps {
            time_prefix(&mut cells, row.at);
        }
        if needle.is_empty() && app.search_re.is_none() {
            cells.extend(row.cells.iter().map(|c| (c.ch, c.style)));
        } else {
            let mask = match &app.search_re {
                Some(re) => regex_mask(&row.cells, re),
                None => match_mask(&row.cells, &needle),
            };
            cells.extend(row.cells.iter().enumerate().map(|(i, c)| {
                (
                    c.ch,
                    if mask[i] {
                        c.style.patch(HIGHLIGHT)
                    } else {
                        c.style
                    },
                )
            }));
        }
        push_wrapped(&mut out, &cells, limit);
    }
    let pending = app.assembler.pending();
    if !pending.is_empty() {
        cells.clear();
        if app.show_timestamps {
            time_prefix(&mut cells, app.last_rx_at);
        }
        let fallback = crate::esp::log::color(pending);
        cells.extend(pending.iter().map(|c: &Cell| {
            let mut style = c.style;
            if style.fg.is_none() {
                style.fg = fallback;
            }
            (c.ch, style)
        }));
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

fn push_wrapped(out: &mut Vec<Line<'static>>, cells: &[(char, Style)], limit: usize) {
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

fn to_line(cells: &[(char, Style)]) -> Line<'static> {
    let mut spans = Vec::new();
    let mut text = String::new();
    let mut style = cells.first().map_or(PLAIN, |&(_, key)| key);
    for &(ch, key) in cells {
        if key != style {
            spans.push(Span::styled(std::mem::take(&mut text), style));
            style = key;
        }
        text.push(ch);
    }
    if !text.is_empty() {
        spans.push(Span::styled(text, style));
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
    use ratatui::crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };

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
        let mut app = App::new(115200, Vec::new(), None, false, Vec::new(), Vec::new());
        app.mode = Mode::Terminal;
        app
    }

    fn pane(app: &mut App, tx: bool) -> String {
        let mut terminal = Terminal::new(TestBackend::new(120, 14)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let area = app
            .hits_ui
            .iter()
            .find_map(|(area, hit)| {
                (if tx {
                    matches!(hit, Hit::TxOutput)
                } else {
                    matches!(hit, Hit::Output)
                })
                .then_some(*area)
            })
            .unwrap();
        let mut text = String::new();
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                text.push_str(terminal.backend().buffer().cell((x, y)).unwrap().symbol());
            }
            text.push('\n');
        }
        text
    }

    #[test]
    fn locked_unfinished_receive_line_stays_anchored_after_append() {
        let mut app = app();
        app.on_event(Event::Data(b"0123456789".repeat(240)));
        let before = pane(&mut app, false);
        assert!(app.starts.is_empty());
        assert!(app.cache.len() > usize::from(app.view_height));
        app.execute("/lock on");
        let old_top = app.cache.len() - usize::from(app.view_height) - app.scroll_back;
        app.on_event(Event::Data(b"NEW-TAIL".repeat(100)));
        let after = pane(&mut app, false);
        assert_eq!(after.lines().next(), before.lines().next());
        assert_eq!(
            app.cache.len() - usize::from(app.view_height) - app.scroll_back,
            old_top
        );
        app.execute("/lock off");
        assert!(pane(&mut app, false).contains("NEW-TAIL"));
    }

    #[test]
    fn narrow_transmit_pane_shows_newest_row_beyond_u16_offset() {
        let mut app = app();
        app.on_event(Event::Written(b"x".to_vec()));
        let mut terminal = Terminal::new(TestBackend::new(8, 4)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                draw_streams(frame, &mut app, area);
            })
            .unwrap();
        let area = app
            .hits_ui
            .iter()
            .find_map(|(area, hit)| matches!(hit, Hit::TxOutput).then_some(*area))
            .unwrap();
        assert!(area.width > 0);
        let width = usize::from(area.width);
        let mut bytes = vec![b'x'; (usize::from(u16::MAX) + 10) * width];
        bytes.extend(vec![b'Z'; width]);
        app.on_event(Event::Written(bytes));
        terminal
            .draw(|frame| {
                let area = frame.area();
                draw_streams(frame, &mut app, area);
            })
            .unwrap();
        assert!(app.tx_cache.rows.len() - usize::from(area.height) > usize::from(u16::MAX));
        assert_eq!(
            terminal
                .backend()
                .buffer()
                .cell((area.x, area.bottom() - 1))
                .unwrap()
                .symbol(),
            "Z"
        );
    }

    #[test]
    fn locked_viewport_survives_append_and_retention_until_unlock() {
        for packets in [false, true] {
            let mut app = app();
            app.scrollback = crate::term::scrollback::Scrollback::new(20, 1024 * 1024);
            if packets {
                app.execute("/packets on");
            }
            for index in 0..20 {
                app.on_event(Event::Received {
                    at: index * 100,
                    bytes: format!("row-{index:02}\n").into_bytes(),
                });
            }
            let before = pane(&mut app, false);
            app.execute("/lock on");
            for index in 20..23 {
                app.on_event(Event::Received {
                    at: index * 100,
                    bytes: format!("row-{index:02}\n").into_bytes(),
                });
            }
            assert_eq!(pane(&mut app, false), before);
            app.execute("/lock off");
            assert!(pane(&mut app, false).contains("row-22"));
        }
    }

    fn line_text(line: &Line) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn color_at(line: &Line, needle: &str) -> Option<Color> {
        let mut text = String::new();
        for span in &line.spans {
            text.push_str(&span.content);
            if text.contains(needle) {
                return span.style.fg;
            }
        }
        None
    }

    #[test]
    fn packet_rows_break_lines_and_keep_log_colors_with_timestamps() {
        let mut app = app();
        app.execute("/packets on");
        app.on_event(Event::Received {
            at: 1_000,
            bytes: b"I (1) a: one\r\nE (2) b: two\r\n".to_vec(),
        });
        for timestamps in [false, true] {
            app.show_timestamps = timestamps;
            let (rows, starts, _) = build_rows(&app, 80);
            assert_eq!(starts, [0]);
            assert_eq!(
                rows.len(),
                2,
                "{:?}",
                rows.iter().map(line_text).collect::<Vec<_>>()
            );
            let first = line_text(&rows[0]);
            let second = line_text(&rows[1]);
            assert!(first.contains("RX 28B I (1) a: one\\r\\n"), "{first}");
            assert!(second.trim_start().starts_with("E (2) b: two"), "{second}");
            // 续行与首行内容对齐
            assert_eq!(second.find('E'), first.find("I ("), "{first}\n{second}");
            assert_eq!(color_at(&rows[0], "I (1"), Some(Color::Green));
            assert_eq!(color_at(&rows[1], "E (2"), Some(Color::Red));
        }
        app.show_timestamps = false;
        app.execute("/packets off");
        app.show_timestamps = true;
        let (rows, _, _) = build_rows(&app, 80);
        assert_eq!(color_at(&rows[0], "I (1"), Some(Color::Green));
    }

    #[test]
    fn toolbar_toggles_time_and_clears_screen() {
        let mut app = app();
        app.on_event(Event::Data(b"hello\r\n".to_vec()));
        let text = rendered(&mut app, 160, 10);
        assert!(
            text.contains("Time:off ^T") && text.contains("Clear ^L"),
            "{text}"
        );
        app.focus = Focus::Toolbar;
        app.toolbar_selected = 9;
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.show_timestamps);
        assert!(!app.show_packets, "timestamps are independent of packets");
        app.on_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.scrollback.len(), 0);
        app.on_event(Event::Data(b"again\r\n".to_vec()));
        app.execute("/clear");
        assert!(!rendered(&mut app, 160, 10).contains("again"));
    }

    #[test]
    fn packet_view_frames_rx_by_idle_gap() {
        let mut app = app();
        app.execute("/packets on");
        // 一行日志被驱动拆成 32B + 10B 两次 read，间隔 2ms：应合并为一个包
        app.on_event(Event::Received {
            at: 1_000,
            bytes: b"I (11374) hil_peer: TX id=0041 l".to_vec(),
        });
        app.on_event(Event::Received {
            at: 1_002,
            bytes: b"ength=32\r\n".to_vec(),
        });
        // 超过 20ms 空闲后到达：新包
        app.on_event(Event::Received {
            at: 1_100,
            bytes: b"next\r\n".to_vec(),
        });
        let rx = pane(&mut app, false);
        assert!(
            rx.contains("RX 42B I (11374) hil_peer: TX id=0041 length=32\\r\\n"),
            "{rx}"
        );
        assert!(rx.contains("RX 6B next"), "{rx}");
        assert_eq!(app.rx_packets.len(), 2);

        app.execute("/packets gap 0");
        app.on_event(Event::Received {
            at: 1_101,
            bytes: b"a".to_vec(),
        });
        app.on_event(Event::Received {
            at: 1_101,
            bytes: b"b".to_vec(),
        });
        assert_eq!(app.rx_packets.len(), 4);
        app.execute("/packets gap nope");
        assert!(app.status.starts_with("usage: /packets gap"));
        assert_eq!(app.packet_gap_ms, 0);
    }

    #[test]
    fn merged_packets_are_capped_and_tx_never_merges() {
        let mut app = app();
        for index in 0..5u64 {
            app.on_event(Event::Received {
                at: index,
                bytes: vec![b'x'; 1024],
            });
        }
        let sizes: Vec<_> = app.rx_packets.iter().map(|p| p.bytes.len()).collect();
        assert_eq!(sizes, vec![4096, 1024]);
        app.on_event(Event::Written(b"one".to_vec()));
        app.on_event(Event::Written(b"two".to_vec()));
        assert_eq!(app.tx_packets.len(), 2);
    }

    #[test]
    fn transmit_pane_is_independent_and_packet_metadata_toggles() {
        let mut app = app();
        app.on_event(Event::Data(b"from-rx\n".to_vec()));
        app.on_event(Event::Written(b"from-tx\r\n".to_vec()));
        let rx = pane(&mut app, false);
        let tx = pane(&mut app, true);
        assert!(rx.contains("from-rx") && !rx.contains("from-tx"));
        assert!(tx.contains("from-tx\\r\\n") && !tx.contains("from-rx"));
        app.execute("/packets on");
        assert!(pane(&mut app, false).contains("RX 8B from-rx\\n"));
        assert!(pane(&mut app, true).contains("TX 9B from-tx\\r\\n"));
        app.execute("/timestamps on");
        assert!(
            pane(&mut app, false)
                .lines()
                .find(|line| line.contains("RX 8B"))
                .unwrap()
                .as_bytes()[2]
                == b':'
        );
        app.execute("/packets off");
        assert!(!pane(&mut app, false).contains("RX 8B"));
    }

    #[test]
    fn unstyled_esp_levels_are_colored_without_overriding_ansi() {
        let mut app = app();
        app.on_event(Event::Data(
            b"I (1) task: ready\nE (2) task: failed\n\x1b[35mW (3) task: explicit\x1b[0m\n"
                .to_vec(),
        ));
        let mut terminal = Terminal::new(TestBackend::new(120, 14)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let area = app
            .hits_ui
            .iter()
            .find_map(|(area, hit)| matches!(hit, Hit::Output).then_some(*area))
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer.cell((area.x, area.y)).unwrap().fg, Color::Green);
        assert_eq!(buffer.cell((area.x, area.y + 1)).unwrap().fg, Color::Red);
        assert_eq!(
            buffer.cell((area.x, area.y + 2)).unwrap().fg,
            Color::Magenta
        );
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
        assert!(text.contains("› ping"));
    }

    #[test]
    fn input_box_grows_with_manual_newlines_and_shows_mode() {
        let mut app = app();
        app.focus = Focus::Input;
        for ch in "AT+CMD".chars() {
            app.on_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
        app.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        app.on_paste("line3\r\nline4".to_string());
        assert_eq!(app.input_text(), "AT+CMD\n\nline3\nline4");
        let text = rendered(&mut app, 80, 24);
        assert!(text.contains("TX · TEXT · CRLF"), "{text}");
        assert!(text.contains("› AT+CMD"), "{text}");
        assert!(text.contains("  line3"), "{text}");
        assert!(text.contains("  line4"), "{text}");
        let input = app
            .hits_ui
            .iter()
            .find_map(|(area, hit)| matches!(hit, Hit::Input).then_some(*area))
            .unwrap();
        assert_eq!(input.height, 4 + 2);
        // 光标在末行时 Up 先在输入框内移动而不是翻历史
        app.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        app.on_key(KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE));
        assert_eq!(app.input_text(), "AT+CMD\n\nline3!\nline4");
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
    fn ansi_colors_and_attributes_reach_the_terminal_buffer_and_reset() {
        let mut app = app();
        app.on_event(Event::Data(
            b"\x1b[31;44;1;4mA\x1b[38;5;196;48;5;23mB\x1b[38;2;1;4;0;48;2;5;6;7mC\x1b[22;24;39;49mD\x1b[91;107mE\x1b[0mF\n".to_vec(),
        ));
        let mut terminal = Terminal::new(TestBackend::new(80, 6)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let output = app
            .hits_ui
            .iter()
            .find_map(|(rect, hit)| matches!(hit, Hit::Output).then_some(*rect))
            .unwrap();
        let cells = &terminal.backend().buffer().content()
            [usize::from(output.y) * 80 + usize::from(output.x)..];
        assert_eq!(cells[0].symbol(), "A");
        assert_eq!(cells[0].fg, Color::Red);
        assert_eq!(cells[0].bg, Color::Blue);
        assert!(
            cells[0]
                .modifier
                .contains(Modifier::BOLD | Modifier::UNDERLINED)
        );
        assert_eq!(cells[1].fg, Color::Indexed(196));
        assert_eq!(cells[1].bg, Color::Indexed(23));
        assert!(
            cells[1]
                .modifier
                .contains(Modifier::BOLD | Modifier::UNDERLINED)
        );
        assert_eq!(cells[2].fg, Color::Rgb(1, 4, 0));
        assert_eq!(cells[2].bg, Color::Rgb(5, 6, 7));
        assert!(
            cells[2]
                .modifier
                .contains(Modifier::BOLD | Modifier::UNDERLINED)
        );
        assert_eq!(cells[3].fg, Color::Reset);
        assert_eq!(cells[3].bg, Color::Reset);
        assert!(cells[3].modifier.is_empty());
        assert_eq!(cells[4].fg, Color::LightRed);
        assert_eq!(cells[4].bg, Color::White);
        assert_eq!(cells[5].fg, Color::Reset);
        assert_eq!(cells[5].bg, Color::Reset);
        assert!(cells[5].modifier.is_empty());
    }

    #[test]
    fn search_overlay_preserves_attributes_and_adjacent_cell_styles() {
        let mut app = app();
        app.on_event(Event::Data(b"\x1b[31;44;1;4mAB\n".to_vec()));
        app.search_query = Some("A".to_string());
        app.run_search();
        let mut terminal = Terminal::new(TestBackend::new(80, 6)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let output = app
            .hits_ui
            .iter()
            .find_map(|(rect, hit)| matches!(hit, Hit::Output).then_some(*rect))
            .unwrap();
        let cells = &terminal.backend().buffer().content()
            [usize::from(output.y) * 80 + usize::from(output.x)..];
        assert_eq!(cells[0].fg, Color::Black);
        assert_eq!(cells[0].bg, Color::Yellow);
        assert!(
            cells[0]
                .modifier
                .contains(Modifier::BOLD | Modifier::UNDERLINED)
        );
        assert_eq!(cells[1].fg, Color::Red);
        assert_eq!(cells[1].bg, Color::Blue);
        assert!(
            cells[1]
                .modifier
                .contains(Modifier::BOLD | Modifier::UNDERLINED)
        );
    }

    #[test]
    fn invalid_bytes_are_visible_and_count_remains_in_status_after_clear() {
        let mut app = app();
        app.on_event(Event::Data(b"\xe2A\n\xffB\n".to_vec()));
        let text = rendered(&mut app, 180, 6);
        assert!(text.contains(r"\xE2A"), "{text}");
        assert!(text.contains(r"\xFFB"), "{text}");
        assert!(text.contains("invalid UTF-8 2"), "{text}");
        app.on_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL));
        let text = rendered(&mut app, 180, 6);
        assert!(!text.contains(r"\xE2A"), "{text}");
        assert!(text.contains("invalid UTF-8 2"), "{text}");
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

    #[test]
    fn toolbar_mouse_help_consumes_input_and_help_scrolls() {
        let mut app = app();
        app.demo = true;
        app.raw_mode = true;
        rendered(&mut app, 80, 12);
        let help = app
            .hits_ui
            .iter()
            .find_map(|(rect, hit)| matches!(hit, Hit::Toolbar(4)).then_some(*rect))
            .unwrap();
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: help.x,
            row: help.y,
            modifiers: KeyModifiers::NONE,
        });
        assert!(app.help);
        rendered(&mut app, 80, 12);
        app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.tx_bytes, 0);
        assert!(app.help_max_scroll > 0);
        app.on_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        assert_eq!(app.help_scroll, app.help_max_scroll);
        let screen = rendered(&mut app, 80, 12);
        assert!(
            screen.contains("Typing here never reaches the device"),
            "{screen}"
        );
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        assert!(!app.help);
        assert_eq!(app.tx_bytes, 0);
    }

    #[test]
    fn picker_terminal_and_all_dialogs_clip_to_small_terminals() {
        for width in [1, 8, 20, 80] {
            for height in [1, 2, 4, 12] {
                let mut app = app();
                rendered(&mut app, width, height);
                for opener in [KeyCode::F(4), KeyCode::F(1), KeyCode::F(3)] {
                    app.on_key(KeyEvent::new(opener, KeyModifiers::NONE));
                    rendered(&mut app, width, height);
                    app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
                }
                app.mode = Mode::Picker;
                rendered(&mut app, width, height);
            }
        }
    }
}
