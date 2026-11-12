use crate::{
    app::Eol,
    serial::{self, Settings},
};
use ratatui::{
    Frame,
    crossterm::event::{
        KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    layout::{Position, Rect},
    style::{Color, Modifier, Style},
    widgets::{Block, Clear, Paragraph},
};

const BAUDS: &[&str] = &[
    "9600", "19200", "38400", "57600", "115200", "230400", "460800", "921600", "Custom",
];
const DATA: &[&str] = &["5", "6", "7", "8"];
const STOP: &[&str] = &["1", "2"];
const PARITY: &[&str] = &["none", "odd", "even"];
const FLOW: &[&str] = &["none", "hardware", "software"];
const EOL: &[&str] = &["crlf", "lf", "cr", "none"];
const LABELS: &[&str] = &[
    "Baud preset",
    "Custom baud",
    "Data bits",
    "Stop bits",
    "Parity",
    "Flow control",
    "Send EOL",
    "Auto-reset",
    "Apply",
    "Cancel",
];

#[derive(Debug, PartialEq, Eq)]
pub enum DialogAction {
    Continue,
    Apply,
    Cancel,
}

pub struct SettingsDialog {
    baud: String,
    selected: [usize; 7],
    auto_reset: bool,
    focus: usize,
    dropdown: Option<usize>,
    error: Option<String>,
    fields: Vec<(Rect, usize)>,
    options: Vec<(Rect, usize)>,
}

impl SettingsDialog {
    pub fn new(settings: Settings, eol: Eol, auto_reset: bool) -> Self {
        let baud = settings.baud.to_string();
        let preset = BAUDS
            .iter()
            .position(|value| *value == baud)
            .unwrap_or(BAUDS.len() - 1);
        Self {
            baud,
            selected: [
                preset,
                0,
                match settings.data_bits {
                    serialport::DataBits::Five => 0,
                    serialport::DataBits::Six => 1,
                    serialport::DataBits::Seven => 2,
                    serialport::DataBits::Eight => 3,
                },
                match settings.stop_bits {
                    serialport::StopBits::One => 0,
                    serialport::StopBits::Two => 1,
                },
                match settings.parity {
                    serialport::Parity::None => 0,
                    serialport::Parity::Odd => 1,
                    serialport::Parity::Even => 2,
                },
                match settings.flow {
                    serialport::FlowControl::None => 0,
                    serialport::FlowControl::Hardware => 1,
                    serialport::FlowControl::Software => 2,
                },
                match eol {
                    Eol::CrLf => 0,
                    Eol::Lf => 1,
                    Eol::Cr => 2,
                    Eol::None => 3,
                },
            ],
            auto_reset,
            focus: 0,
            dropdown: None,
            error: None,
            fields: Vec::new(),
            options: Vec::new(),
        }
    }

    fn choices(&self) -> &'static [&'static str] {
        match self.focus {
            0 => BAUDS,
            2 => DATA,
            3 => STOP,
            4 => PARITY,
            5 => FLOW,
            6 => EOL,
            _ => &[],
        }
    }

    pub fn values(&self) -> Result<(Settings, Eol, bool), String> {
        let baud = self
            .baud
            .parse::<u32>()
            .ok()
            .filter(|value| *value > 0)
            .ok_or_else(|| "Baud must be a positive whole number (1–4294967295).".to_owned())?;
        Ok((
            Settings {
                baud,
                data_bits: serial::parse_data_bits(DATA[self.selected[2]].parse().unwrap())?,
                stop_bits: serial::parse_stop_bits(STOP[self.selected[3]].parse().unwrap())?,
                parity: serial::parse_parity(PARITY[self.selected[4]])?,
                flow: serial::parse_flow(FLOW[self.selected[5]])?,
            },
            Eol::parse(EOL[self.selected[6]]).unwrap(),
            self.auto_reset,
        ))
    }

    fn apply(&mut self) -> DialogAction {
        match self.values() {
            Ok(_) => DialogAction::Apply,
            Err(error) => {
                self.error = Some(error);
                self.focus = 1;
                DialogAction::Continue
            }
        }
    }

    fn choose(&mut self, index: usize) {
        self.selected[self.focus] = index;
        if self.focus == 0 {
            if index == BAUDS.len() - 1 {
                self.focus = 1;
            } else {
                self.baud = BAUDS[index].to_owned();
            }
        }
        self.dropdown = None;
        self.options.clear();
        self.error = None;
    }

    fn activate(&mut self) -> DialogAction {
        match self.focus {
            7 => self.auto_reset = !self.auto_reset,
            8 => return self.apply(),
            9 => return DialogAction::Cancel,
            _ if !self.choices().is_empty() => self.dropdown = Some(self.selected[self.focus]),
            _ => {}
        }
        DialogAction::Continue
    }

    pub fn on_key(&mut self, key: KeyEvent) -> DialogAction {
        if key.kind == KeyEventKind::Release {
            return DialogAction::Continue;
        }
        if key.code == KeyCode::Esc {
            return DialogAction::Cancel;
        }
        if key.code == KeyCode::Tab || key.code == KeyCode::BackTab {
            let backwards =
                key.code == KeyCode::BackTab || key.modifiers.contains(KeyModifiers::SHIFT);
            self.focus = (self.focus + if backwards { 9 } else { 1 }) % 10;
            self.dropdown = None;
            self.options.clear();
            return DialogAction::Continue;
        }
        if let Some(index) = self.dropdown {
            let count = self.choices().len();
            match key.code {
                KeyCode::Up | KeyCode::Left => self.dropdown = Some((index + count - 1) % count),
                KeyCode::Down | KeyCode::Right => self.dropdown = Some((index + 1) % count),
                KeyCode::Home => self.dropdown = Some(0),
                KeyCode::End => self.dropdown = Some(count - 1),
                KeyCode::Enter | KeyCode::Char(' ') => self.choose(index),
                _ => {}
            }
            return DialogAction::Continue;
        }
        match key.code {
            KeyCode::Enter => return self.activate(),
            KeyCode::Char(' ') if self.focus == 7 => return self.activate(),
            KeyCode::Up if self.choices().is_empty() => self.focus = (self.focus + 9) % 10,
            KeyCode::Down if self.choices().is_empty() => self.focus = (self.focus + 1) % 10,
            KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right
                if !self.choices().is_empty() =>
            {
                return self.activate();
            }
            KeyCode::Backspace if self.focus == 1 => {
                self.baud.pop();
                self.edited_baud();
            }
            KeyCode::Delete if self.focus == 1 => {
                self.baud.clear();
                self.edited_baud();
            }
            KeyCode::Char('a')
                if key.modifiers.contains(KeyModifiers::CONTROL) && self.focus == 1 =>
            {
                self.baud.clear();
                self.edited_baud();
            }
            KeyCode::Char(c)
                if self.focus == 1
                    && c.is_ascii_digit()
                    && self.baud.len() < 20
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.baud.push(c);
                self.edited_baud();
            }
            _ => {}
        }
        DialogAction::Continue
    }

    fn edited_baud(&mut self) {
        self.selected[0] = BAUDS.len() - 1;
        self.error = self.values().err();
    }

    pub fn on_mouse(&mut self, event: MouseEvent) -> DialogAction {
        let position = Position::new(event.column, event.row);
        if let Some(index) = self.dropdown {
            match event.kind {
                MouseEventKind::ScrollUp => {
                    self.dropdown = Some((index + self.choices().len() - 1) % self.choices().len())
                }
                MouseEventKind::ScrollDown => {
                    self.dropdown = Some((index + 1) % self.choices().len())
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    if let Some((_, choice)) = self
                        .options
                        .iter()
                        .find(|(rect, _)| rect.contains(position))
                    {
                        self.choose(*choice);
                    } else {
                        self.dropdown = None;
                        self.options.clear();
                    }
                }
                _ => {}
            }
            return DialogAction::Continue;
        }
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some((_, focus)) =
                    self.fields.iter().find(|(rect, _)| rect.contains(position))
                {
                    self.focus = *focus;
                    return self.activate();
                }
            }
            MouseEventKind::ScrollUp => self.focus = (self.focus + 9) % 10,
            MouseEventKind::ScrollDown => self.focus = (self.focus + 1) % 10,
            _ => {}
        }
        DialogAction::Continue
    }

    pub fn draw(&mut self, frame: &mut Frame) {
        self.fields.clear();
        self.options.clear();
        let screen = frame.area();
        let width = screen.width.min(66);
        let height = screen.height.min(17);
        let area = Rect::new(
            screen.x + (screen.width - width) / 2,
            screen.y + (screen.height - height) / 2,
            width,
            height,
        );
        frame.render_widget(Clear, area);
        let block = Block::bordered().title(" Serial settings ");
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        let instructions = [
            "Tab/Shift-Tab: focus; Enter/arrows: open/select",
            "Custom baud: type; Backspace/Delete: erase; Esc: cancel",
        ];
        let footer_rows = if inner.height >= 5 {
            2 + usize::from(self.error.is_some())
        } else {
            0
        };
        let visible = usize::from(inner.height)
            .saturating_sub(footer_rows)
            .min(10);
        let start = self.focus.saturating_sub(visible.saturating_sub(1));
        for (row, field) in (start..(start + visible).min(10)).enumerate() {
            let value = match field {
                0 => format!("{} v", BAUDS[self.selected[0]]),
                1 => format!("{}|", self.baud),
                2 => format!("{} v", DATA[self.selected[2]]),
                3 => format!("{} v", STOP[self.selected[3]]),
                4 => format!("{} v", PARITY[self.selected[4]]),
                5 => format!("{} v", FLOW[self.selected[5]]),
                6 => format!("{} v", EOL[self.selected[6]]),
                7 => if self.auto_reset { "[x]" } else { "[ ]" }.to_owned(),
                _ => "[ Enter / click ]".to_owned(),
            };
            let rect = Rect::new(inner.x, inner.y + row as u16, inner.width, 1);
            let text = format!(
                "{} {:<12} {}",
                if field == self.focus { ">" } else { " " },
                LABELS[field],
                value
            );
            frame.render_widget(
                Paragraph::new(text).style(if field == self.focus {
                    selected_style()
                } else {
                    Style::default()
                }),
                rect,
            );
            self.fields.push((rect, field));
        }
        if footer_rows > 0 {
            let y = inner.bottom() - footer_rows as u16;
            for (row, text) in instructions.iter().enumerate() {
                frame.render_widget(
                    Paragraph::new(*text).style(Style::default().fg(Color::DarkGray)),
                    Rect::new(inner.x, y + row as u16, inner.width, 1),
                );
            }
            if let Some(error) = &self.error {
                frame.render_widget(
                    Paragraph::new(error.as_str()).style(Style::default().fg(Color::Red)),
                    Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
                );
            }
        }
        if let Some(index) = self.dropdown {
            let choices = self.choices();
            let count = choices.len().min(usize::from(inner.height));
            let first = index.saturating_sub(count.saturating_sub(1));
            let field_y = self
                .fields
                .iter()
                .find(|(_, field)| *field == self.focus)
                .map(|(rect, _)| rect.y)
                .unwrap_or(inner.y);
            let y = field_y
                .saturating_add(1)
                .min(inner.bottom().saturating_sub(count as u16));
            let popup = Rect::new(inner.x, y, inner.width, count as u16);
            frame.render_widget(Clear, popup);
            for (row, choice) in (first..first + count).enumerate() {
                let rect = Rect::new(popup.x, popup.y + row as u16, popup.width, 1);
                let text = format!(
                    "{} {}",
                    if choice == index { ">" } else { " " },
                    choices[choice]
                );
                frame.render_widget(
                    Paragraph::new(text).style(if choice == index {
                        selected_style()
                    } else {
                        Style::default().fg(Color::Cyan)
                    }),
                    rect,
                );
                self.options.push((rect, choice));
            }
        }
    }
}

fn selected_style() -> Style {
    Style::default()
        .fg(Color::Black)
        .bg(Color::Cyan)
        .add_modifier(Modifier::BOLD)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_baud_cannot_apply() {
        let mut dialog = SettingsDialog::new(Settings::new(115200), Eol::CrLf, true);
        dialog.baud = "0".to_owned();
        dialog.focus = 8;
        assert_eq!(
            dialog.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            DialogAction::Continue
        );
        assert!(dialog.values().is_err());
        assert!(dialog.error.is_some());
    }

    #[test]
    fn custom_baud_is_applied_without_using_preset() {
        let mut dialog = SettingsDialog::new(Settings::new(115200), Eol::CrLf, true);
        dialog.focus = 1;
        dialog.on_key(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE));
        for c in "74880".chars() {
            dialog.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        dialog.focus = 8;
        assert_eq!(
            dialog.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            DialogAction::Apply
        );
        assert_eq!(dialog.values().unwrap().0.baud, 74880);
    }
}
