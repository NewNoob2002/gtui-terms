use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;
use std::time::{SystemTime, UNIX_EPOCH};

use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::Line;

use crate::esp::{elf, panic};
use crate::logfile::LogFile;
use crate::serial::reset::Kind as ResetKind;
use crate::serial::{self, Event, PortInfo};
use crate::term::assembler::Assembler;
use crate::term::scrollback::{Cell, PLAIN, Row, Scrollback, match_mask};

pub const SCROLLBACK_ROWS: usize = 50_000;
pub const SCROLLBACK_BYTES: usize = 8 * 1024 * 1024;
const MAX_INPUT: usize = 4096;
const HISTORY_MAX: usize = 200;
/// 历史文件按行存储；多行输入中的换行写成 U+001F（单元分隔符），用户无法直接键入。
const HISTORY_NEWLINE: &str = "\u{1f}";
pub const RAW_CAP: usize = 1024 * 1024;
const PACKET_CAP: usize = 2000;
/// Default idle gap that closes an RX packet; reads closer than this are one frame.
pub const PACKET_GAP_MS: u64 = 20;
/// Toolbar buttons drawn by `ui::draw_toolbar`, in `toolbar_action` order.
pub const TOOLBAR_ITEMS: usize = 11;
/// Upper bound for one merged RX packet so a continuous stream still splits.
const PACKET_MAX_BYTES: usize = 4096;

pub struct Packet {
    /// Arrival time of the first byte.
    pub at: u64,
    /// Arrival time of the latest chunk merged into this packet.
    pub last: u64,
    pub bytes: Vec<u8>,
}

impl Packet {
    fn new(at: u64, bytes: Vec<u8>) -> Self {
        Self {
            at,
            last: at,
            bytes,
        }
    }
}

#[derive(Default)]
pub struct PacketCache {
    pub rows: Vec<Line<'static>>,
    pub starts: Vec<usize>,
    pub key: Option<(u64, u16, bool, bool, View)>,
    pub dropped: usize,
}

pub struct Repeat {
    pub payload: String,
    pub every: std::time::Duration,
    next: std::time::Instant,
    pub left: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Eol {
    CrLf,
    Lf,
    Cr,
    None,
}

impl Eol {
    pub fn bytes(self) -> &'static [u8] {
        match self {
            Eol::CrLf => b"\r\n",
            Eol::Lf => b"\n",
            Eol::Cr => b"\r",
            Eol::None => b"",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Eol::CrLf => "crlf",
            Eol::Lf => "lf",
            Eol::Cr => "cr",
            Eol::None => "none",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text.to_ascii_lowercase().as_str() {
            "crlf" => Some(Eol::CrLf),
            "lf" | "nl" => Some(Eol::Lf),
            "cr" => Some(Eol::Cr),
            "none" => Some(Eol::None),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum View {
    Text,
    Hex,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Picker,
    Terminal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Toolbar,
    Output,
    Input,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Popup {
    Actions,
    Esp,
    Elf,
    Save,
    Flash,
    ConfirmTool,
}

#[derive(Clone, Copy)]
pub enum Hit {
    Toolbar(usize),
    Port(usize),
    Refresh,
    Output,
    Input,
    Menu(usize),
    Close,
    ElfApply,
    DialogApply,
    TxOutput,
}

pub struct App {
    pub mode: Mode,
    pub view: View,
    pub raw: VecDeque<u8>,
    pub raw_total: u64,
    pub ports: Vec<PortInfo>,
    pub selected: usize,
    pub settings: serial::Settings,
    pub auto_reset: bool,
    pub label: String,
    pub status: String,
    pub scrollback: Scrollback,
    pub assembler: Assembler,
    pub input: crate::input::Editor,
    /// 输入框文本区宽度（列），由 UI 绘制时更新，用于上下键在折行间移动。
    pub input_width: u16,
    pub quit: bool,
    pub dirty: bool,
    pub show_timestamps: bool,
    pub show_packets: bool,
    /// Idle gap (ms) that closes an RX packet; 0 keeps every read as its own packet.
    pub packet_gap_ms: u64,
    /// 内置烧录器擦写时使用的波特率；只读查询固定 115200。
    pub flash_baud: u32,
    pub scroll_locked: bool,
    pub rx_packets: VecDeque<Packet>,
    pub tx_packets: VecDeque<Packet>,
    pub rx_packet_dropped: usize,
    rx_packet_bytes: usize,
    tx_packet_bytes: usize,
    pub tx_packet_dropped: usize,
    pub tx_cache: PacketCache,
    pub tx_scroll_back: usize,
    pub pending_writes: usize,
    pub pending_controls: usize,
    pub last_rx_at: u64,
    session: Option<crate::session::Session>,
    session_error: String,
    pub record_output: Option<Box<dyn std::io::Write + Send>>,
    pub record_output_error: Option<std::io::Error>,
    pub demo: bool,
    pub reconnecting: bool,
    pub version: u64,
    pub log_path: Option<String>,
    pub cache: Vec<Line<'static>>,
    pub cache_width: u16,
    pub cache_version: u64,
    pub starts: Vec<usize>,
    pub cache_dropped: usize,
    pub total_rows: usize,
    pub scroll_back: usize,
    pub view_height: u16,
    pub search_query: Option<String>,
    pub search_input: Option<Vec<char>>,
    pub hits: Vec<usize>,
    pub search_re: Option<regex::Regex>,
    pub repeat: Option<Repeat>,
    pub macros: Vec<crate::config::Macro>,
    pub config_path: std::path::PathBuf,
    pub help: bool,
    pub focus: Focus,
    pub toolbar_selected: usize,
    pub hits_ui: Vec<(Rect, Hit)>,
    pub popup: Option<Popup>,
    pub popup_selected: usize,
    pub popup_rect: Rect,
    pub elf_input: String,
    pub prompt_input: String,
    pub pending_tool: Option<crate::esptool::Invocation>,
    pub help_scroll: usize,
    pub help_max_scroll: usize,
    pub help_height: usize,
    pub settings_dialog: Option<crate::settings::SettingsDialog>,
    pending_port: Option<(String, ResetKind)>,
    ports_refreshed: std::time::Instant,
    pub esp_status: String,
    pub raw_mode: bool,
    pub eol: Eol,
    /// 输入框按 HEX 解析发送（不追加行尾），Ctrl+X 切换。
    pub tx_hex: bool,
    /// TEXT 模式解析 `\r \n \t \0 \\ \xHH` 转义。
    pub tx_escapes: bool,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub rx_rate: u64,
    rate_mark: (std::time::Instant, u64),
    pub levels: [bool; 3],
    elf_sha: Option<String>,
    pub rx: Receiver<Event>,
    tx: Sender<Event>,
    writes: Option<Sender<serial::Command>>,
    reset_kind: ResetKind,
    worker: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    history: Vec<String>,
    history_pos: Option<usize>,
    log: Option<LogFile>,
    last_flush: std::time::Instant,
    scanner: panic::Scanner,
    symbols: Option<elf::Symbols>,
    pub elf_path: Option<std::path::PathBuf>,
    hooks: Vec<crate::config::Hook>,
    hook_running: Option<String>,
    reopen_after_hook: Option<(String, ResetKind)>,
    hook_last: std::collections::HashMap<String, std::time::Instant>,
}

impl App {
    pub fn new(
        baud: u32,
        ports: Vec<PortInfo>,
        log: Option<LogFile>,
        show_timestamps: bool,
        macros: Vec<crate::config::Macro>,
        hooks: Vec<crate::config::Hook>,
    ) -> Self {
        let (tx, rx) = channel();
        let (session, session_error) = match crate::session::Session::new() {
            Ok(session) => (Some(session), String::new()),
            Err(err) => (None, format!("session capture unavailable: {err}")),
        };
        Self {
            mode: Mode::Picker,
            view: View::Text,
            raw: VecDeque::new(),
            raw_total: 0,
            ports,
            selected: 0,
            settings: serial::Settings::new(baud),
            auto_reset: true,
            label: String::new(),
            status: session_error.clone(),
            scrollback: Scrollback::new(SCROLLBACK_ROWS, SCROLLBACK_BYTES),
            assembler: Assembler::new(),
            input: crate::input::Editor::new(MAX_INPUT),
            input_width: 80,
            quit: false,
            dirty: true,
            show_timestamps,
            packet_gap_ms: PACKET_GAP_MS,
            flash_baud: crate::esptool::DEFAULT_FLASH_BAUD,
            show_packets: false,
            scroll_locked: false,
            rx_packets: VecDeque::new(),
            tx_packets: VecDeque::new(),
            rx_packet_dropped: 0,
            rx_packet_bytes: 0,
            tx_packet_bytes: 0,
            tx_packet_dropped: 0,
            tx_cache: PacketCache::default(),
            tx_scroll_back: 0,
            pending_writes: 0,
            pending_controls: 0,
            last_rx_at: 0,
            session,
            session_error,
            record_output: None,
            record_output_error: None,
            demo: false,
            reconnecting: false,
            version: 1,
            log_path: log.as_ref().map(|l| l.path().display().to_string()),
            cache: Vec::new(),
            cache_width: 0,
            cache_version: 0,
            starts: Vec::new(),
            total_rows: 0,
            cache_dropped: 0,
            scroll_back: 0,
            view_height: 1,
            search_query: None,
            search_input: None,
            hits: Vec::new(),
            search_re: None,
            repeat: None,
            macros,
            config_path: std::path::PathBuf::from(crate::config::LOCAL_FILE),
            help: false,
            focus: Focus::Input,
            toolbar_selected: 0,
            hits_ui: Vec::new(),
            popup: None,
            popup_selected: 0,
            popup_rect: Rect::default(),
            elf_input: String::new(),
            prompt_input: String::new(),
            pending_tool: None,
            help_scroll: 0,
            help_max_scroll: 0,
            help_height: 1,
            settings_dialog: None,
            pending_port: None,
            ports_refreshed: std::time::Instant::now(),
            esp_status: "Listening for panic backtraces and UART base64 coredumps".to_string(),
            raw_mode: false,
            eol: Eol::CrLf,
            tx_hex: false,
            tx_escapes: false,
            rx_bytes: 0,
            tx_bytes: 0,
            rx_rate: 0,
            rate_mark: (std::time::Instant::now(), 0),
            levels: [true; 3],
            elf_sha: None,
            rx,
            tx,
            writes: None,
            reset_kind: ResetKind::Classic,
            worker: None,
            stop: Arc::new(AtomicBool::new(false)),
            history: Vec::new(),
            history_pos: None,
            log,
            last_flush: std::time::Instant::now(),
            scanner: panic::Scanner::new(),
            symbols: None,
            elf_path: None,
            hooks,
            hook_running: None,
            reopen_after_hook: None,
            hook_last: std::collections::HashMap::new(),
        }
    }

    /// Loads debug symbols; failures are reported in the status line, not fatal.
    pub fn set_elf(&mut self, path: std::path::PathBuf) -> bool {
        let loaded = match elf::Symbols::load(&path) {
            Ok(symbols) => {
                self.elf_sha = elf_sha256(&path);
                self.status = match &self.elf_sha {
                    Some(sha) => format!("ELF {} ({})", path.display(), &sha[..sha.len().min(12)]),
                    None => format!("ELF {}", path.display()),
                };
                self.symbols = Some(symbols);
                self.elf_path = Some(path);
                true
            }
            Err(err) => {
                self.status = err;
                false
            }
        };
        self.dirty = true;
        loaded
    }

    pub fn connect(&mut self, name: &str) {
        let kind = self
            .ports
            .iter()
            .find(|port| port.name == name)
            .map_or(ResetKind::Classic, |port| ResetKind::from_pid(port.pid));
        self.connect_with(name, kind);
    }

    pub fn connect_with(&mut self, name: &str, kind: ResetKind) {
        if self.hook_running.is_some() {
            self.status = "Wait for the running hook before connecting".to_string();
            self.dirty = true;
            return;
        }
        self.shutdown_worker();
        self.stop = Arc::new(AtomicBool::new(false));
        let (writes, write_rx) = channel();
        self.writes = Some(writes);
        self.reset_kind = kind;
        self.mode = Mode::Terminal;
        self.focus = Focus::Input;
        self.search_input = None;
        self.repeat = None;
        self.assembler.reset();
        self.scanner = panic::Scanner::new();
        self.version += 1;
        self.label = name.to_string();
        self.demo = false;
        self.reconnecting = false;
        self.status = format!("connecting {name} @ {}", self.settings.baud);
        self.worker = Some(serial::spawn(
            name.to_string(),
            self.settings,
            self.tx.clone(),
            write_rx,
            Arc::clone(&self.stop),
        ));
        if self.auto_reset {
            // The worker drains this after the port is actually open.
            self.queue_reset(false);
            self.status = format!("connecting {name} @ {} (resetting)", self.settings.baud);
        }
        self.dirty = true;
    }

    pub fn start_demo(&mut self) {
        self.start_demo_with(false);
    }

    pub fn start_demo_with(&mut self, with_panic: bool) {
        self.shutdown_worker();
        self.stop = Arc::new(AtomicBool::new(false));
        self.writes = None;
        self.mode = Mode::Terminal;
        self.focus = Focus::Input;
        self.repeat = None;
        self.label = "demo".to_string();
        self.demo = true;
        self.status = "simulated device".to_string();
        self.worker = Some(crate::demo::spawn(
            self.tx.clone(),
            Arc::clone(&self.stop),
            with_panic,
        ));
        self.dirty = true;
    }

    pub fn connect_selected(&mut self) {
        self.open_settings(true);
    }

    pub fn run_hook(&mut self, name: &str) {
        let Some(hook) = self.hooks.iter().find(|hook| hook.name == name).cloned() else {
            self.status = format!("unknown hook {name}");
            self.dirty = true;
            return;
        };
        if self.hook_running.is_some() {
            self.status = "hook already running".to_string();
            self.dirty = true;
            return;
        }
        let reopen = if self.demo || self.writes.is_none() {
            None
        } else {
            Some((self.label.clone(), self.reset_kind))
        };
        self.shutdown_worker();
        self.status = format!("hook {} running: {}", hook.name, hook.command);
        self.hook_running = Some(hook.name.clone());
        self.reopen_after_hook = reopen;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let outcome = crate::hooks::run(&hook.command);
            let _ = tx.send(Event::HookDone {
                name: hook.name,
                ok: outcome.ok,
                output: outcome.text,
            });
        });
        self.dirty = true;
    }

    fn on_hook_done(&mut self, name: String, ok: bool, output: String) {
        let esptool = name == "esptool";
        if !output.is_empty() {
            self.record_session(now_ms(), "TOOL", output.as_bytes());
        }
        let at = now_ms();
        self.push_text(
            &format!(
                "[{}] exit {}",
                if esptool {
                    "esptool".to_string()
                } else {
                    format!("hook {name}")
                },
                if ok { "0" } else { "non-zero" }
            ),
            Style::new().fg(if ok { Color::Green } else { Color::Red }),
            at,
        );
        for line in output.lines().take(40) {
            self.push_text(&format!("[hook {name}] {line}"), PLAIN, at);
        }
        self.hook_running = None;
        self.hook_last
            .insert(name.clone(), std::time::Instant::now());
        if let Some((port, kind)) = self.reopen_after_hook.take() {
            self.push_text(&format!("[hook] reopening {port}"), PLAIN, at);
            let auto_reset = self.auto_reset;
            if esptool {
                self.auto_reset = false;
            }
            self.connect_with(&port, kind);
            self.auto_reset = auto_reset;
        }
        self.status = format!(
            "{} finished (exit {})",
            if esptool { "esptool" } else { &name },
            if ok { 0 } else { 1 }
        );
        self.version += 1;
        self.dirty = true;
    }

    /// Fires regex-triggered hooks on freshly received rows.
    fn check_triggers(&mut self, rows: &[Row]) {
        if self.hook_running.is_some() {
            return;
        }
        let now = std::time::Instant::now();
        let mut fire = None;
        for hook in &self.hooks {
            let Some(trigger) = &hook.trigger else {
                continue;
            };
            if let Some(last) = self.hook_last.get(&hook.name)
                && now.duration_since(*last) < std::time::Duration::from_secs(5)
            {
                continue;
            }
            let hit = rows.iter().any(|row| trigger.is_match(&row.text()));
            if hit {
                fire = Some(hook.name.clone());
                break;
            }
        }
        if let Some(name) = fire {
            self.run_hook(&name);
        }
    }

    fn scan_esp(&mut self, bytes: &[u8]) {
        for finding in self.scanner.feed(bytes) {
            match finding {
                panic::Finding::Panic(report) => self.report_panic(&report),
                panic::Finding::Coredump(text) => self.save_coredump(&text),
            }
        }
    }

    fn report_panic(&mut self, report: &panic::Panic) {
        let at = now_ms();
        let head = report.lines.first().cloned().unwrap_or_default();
        self.push_text(&format!("[panic] {head}"), Style::new().fg(Color::Red), at);
        if let Some(sha) = &report.sha256 {
            self.push_text(&format!("[panic] ELF SHA256 {sha}"), PLAIN, at);
            if let Some(elf) = &self.elf_sha {
                let dump = sha.trim().to_ascii_lowercase();
                if !elf.starts_with(&dump) {
                    self.push_text(
                        &format!("[panic] ELF MISMATCH: dump {dump}, loaded {elf}"),
                        Style::new().fg(Color::Red),
                        at,
                    );
                }
            }
        }
        if report.addresses.is_empty() {
            self.push_text("[panic] no addresses recovered", PLAIN, at);
        }
        for &address in &report.addresses {
            let text = match &self.symbols {
                Some(symbols) => match symbols.resolve(address).first() {
                    Some(frame) => format!("[panic] 0x{address:08x} → {}", frame.render()),
                    None => format!("[panic] 0x{address:08x} → ?? (no symbol)"),
                },
                None => {
                    format!("[panic] 0x{address:08x} (no ELF loaded: ESP > Load ELF, or /elf PATH)")
                }
            };
            self.push_text(&text, PLAIN, at);
        }
        self.status = format!(
            "panic {} ({} addresses); details in output",
            if self.symbols.is_some() {
                "decoded"
            } else {
                "captured; load matching ELF to decode future panics"
            },
            report.addresses.len()
        );
        self.esp_status = self.status.clone();
        self.version += 1;
    }

    pub fn coredump_directory(&self) -> &std::path::Path {
        self.log
            .as_ref()
            .and_then(|log| log.path().parent())
            .unwrap_or_else(|| std::path::Path::new("."))
    }

    pub fn active_hook(&self) -> Option<&str> {
        self.hook_running.as_deref()
    }

    fn save_coredump(&mut self, text: &str) {
        let directory = self.coredump_directory();
        self.status = match crate::esp::coredump::save(text, directory) {
            Ok(path) => format!("coredump saved to {}", path.display()),
            Err(err) => format!("coredump capture failed: {err}"),
        };
        self.esp_status = self.status.clone();
        self.push_text(&format!("[coredump] {}", self.status), PLAIN, now_ms());
        self.version += 1;
    }

    fn push_text(&mut self, text: &str, style: Style, at: u64) {
        let cells = text.chars().map(|ch| Cell::new(ch, style)).collect();
        self.scrollback.push(Row { cells, at });
    }

    fn record_session(&mut self, at: u64, direction: &str, bytes: &[u8]) {
        if let Some(output) = &mut self.record_output
            && let Err(err) = crate::session::write_record(output, at, direction, bytes)
                .and_then(|()| output.flush())
        {
            self.record_output_error = Some(err);
            self.quit = true;
        }
        if !self.session_error.is_empty() {
            return;
        }
        if let Some(session) = &mut self.session
            && let Err(err) = session.record(at, direction, bytes)
        {
            self.session_error = format!("session capture incomplete: {err}");
            self.status = self.session_error.clone();
        }
    }

    pub fn save_session(&mut self, path: &std::path::Path) -> Result<(), String> {
        if !self.session_error.is_empty() {
            return Err(self.session_error.clone());
        }
        self.session
            .as_mut()
            .ok_or("session capture unavailable")?
            .export(path)
            .map_err(|err| format!("cannot save {}: {err}", path.display()))?;
        self.status = format!("session saved to {}", path.display());
        self.dirty = true;
        Ok(())
    }

    fn record_tx(&mut self, bytes: Vec<u8>) {
        let at = now_ms();
        self.record_session(at, "TX", &bytes);
        self.tx_bytes += bytes.len() as u64;
        self.tx_packet_dropped += retain_packet(
            &mut self.tx_packets,
            &mut self.tx_packet_bytes,
            Packet::new(at, bytes),
            None,
        );
        self.dirty = true;
    }

    fn on_received(&mut self, at: u64, bytes: Vec<u8>) {
        self.last_rx_at = at;
        self.record_session(at, "RX", &bytes);
        if let Some(log) = &mut self.log {
            log.write(&bytes);
            if self.last_flush.elapsed().as_millis() >= 500 {
                log.flush();
                self.last_flush = std::time::Instant::now();
            }
        }
        self.raw.extend(bytes.iter().copied());
        self.raw_total += bytes.len() as u64;
        self.rx_bytes += bytes.len() as u64;
        while self.raw.len() > RAW_CAP {
            self.raw.pop_front();
        }
        self.assembler.feed(&bytes);
        let rows: Vec<Row> = self
            .assembler
            .take()
            .into_iter()
            .map(|mut cells| {
                if let Some(color) = crate::esp::log::color(&cells) {
                    for cell in &mut cells {
                        if cell.style.fg.is_none() {
                            cell.style.fg = Some(color);
                        }
                    }
                }
                Row { cells, at }
            })
            .collect();
        self.check_triggers(&rows);
        for row in rows {
            if self.levels[level_of(&row.cells)] {
                self.scrollback.push(row);
            }
        }
        self.scan_esp(&bytes);
        self.rx_packet_dropped += retain_packet(
            &mut self.rx_packets,
            &mut self.rx_packet_bytes,
            Packet::new(at, bytes),
            Some(self.packet_gap_ms),
        );
        self.version += 1;
        self.dirty = true;
    }

    pub fn on_event(&mut self, event: Event) {
        match event {
            Event::Data(bytes) => self.on_received(now_ms(), bytes),
            Event::Received { at, bytes } => self.on_received(at, bytes),
            Event::Written(bytes) => {
                self.pending_writes = self.pending_writes.saturating_sub(1);
                self.record_tx(bytes);
            }
            Event::WriteFailed(error) => {
                self.pending_writes = self.pending_writes.saturating_sub(1);
                self.status = format!("write failed: {error}");
                self.dirty = true;
            }
            Event::ToolOutput(text) => {
                let at = now_ms();
                self.record_session(at, "TOOL", text.as_bytes());
                for line in text.lines() {
                    self.push_text(&format!("[esptool] {line}"), PLAIN, at);
                }
                self.version += 1;
                self.dirty = true;
            }
            Event::Connected(label) => {
                self.label = label;
                self.reconnecting = false;
                self.status = format!("connected @ {}", self.settings.baud);
                self.dirty = true;
            }
            Event::Disconnected(reason) => {
                self.status = format!("disconnected: {reason}");
                self.dirty = true;
            }
            Event::Reconnecting => {
                self.reconnecting = true;
                self.status = "reconnecting…".to_string();
                self.dirty = true;
            }
            Event::ControlDone(text) => {
                self.pending_controls = self.pending_controls.saturating_sub(1);
                self.record_session(now_ms(), "STATE", text.as_bytes());
                if self.session_error.is_empty() {
                    self.status = text;
                }
                self.dirty = true;
            }
            Event::HookDone { name, ok, output } => {
                self.on_hook_done(name, ok, output);
            }
        }
    }

    pub fn replace_ports(&mut self, ports: Vec<PortInfo>) {
        let selected_name = self.ports.get(self.selected).map(|port| port.name.as_str());
        let selected =
            selected_name.and_then(|name| ports.iter().position(|port| port.name == name));
        self.selected = selected.unwrap_or(self.selected.min(ports.len().saturating_sub(1)));
        self.ports = ports;
        self.dirty = true;
    }

    fn refresh_ports(&mut self) {
        self.replace_ports(serial::list_ports());
        self.ports_refreshed = std::time::Instant::now();
    }

    fn open_ports(&mut self) {
        self.popup = None;
        self.search_input = None;
        self.mode = Mode::Picker;
        self.focus = Focus::Output;
        self.refresh_ports();
        self.repeat = None;
    }

    fn open_settings(&mut self, selected: bool) {
        self.popup = None;
        self.pending_port = if selected {
            self.ports
                .get(self.selected)
                .map(|port| (port.name.clone(), ResetKind::from_pid(port.pid)))
        } else {
            None
        };
        if selected && self.pending_port.is_none() {
            self.status = "Waiting for a serial port; plug in a device or Refresh (R)".to_string();
            return;
        }
        self.settings_dialog = Some(crate::settings::SettingsDialog::new(
            self.settings,
            self.eol,
            self.auto_reset,
        ));
    }

    fn settings_action(&mut self, action: crate::settings::DialogAction) {
        use crate::settings::DialogAction;
        match action {
            DialogAction::Continue => {}
            DialogAction::Cancel => {
                self.settings_dialog = None;
                self.pending_port = None;
            }
            DialogAction::Apply => {
                let Some(dialog) = &self.settings_dialog else {
                    return;
                };
                let (settings, eol, auto_reset) = match dialog.values() {
                    Ok(values) => values,
                    Err(error) => {
                        self.status = error;
                        return;
                    }
                };
                self.settings = settings;
                self.eol = eol;
                self.auto_reset = auto_reset;
                self.settings_dialog = None;
                if let Some((name, kind)) = self.pending_port.take() {
                    self.connect_with(&name, kind);
                } else if !self.demo && self.writes.is_some() {
                    self.connect_with(&self.label.clone(), self.reset_kind);
                } else {
                    self.status = "Settings applied".to_string();
                }
            }
        }
        self.dirty = true;
    }

    fn toolbar_action(&mut self, index: usize) {
        match index {
            0 => self.open_ports(),
            1 => self.open_settings(false),
            2 => self.open_popup(Popup::Actions),
            3 => self.open_popup(Popup::Esp),
            4 => {
                self.help = true;
                self.help_scroll = 0;
            }
            5 => self.request_quit(),
            6 => self.open_popup(Popup::Save),
            7 => self.toggle_lock(),
            8 => self.toggle_packets(),
            9 => self.toggle_timestamps(),
            10 => self.clear(),
            _ => {}
        }
        self.dirty = true;
    }

    fn request_quit(&mut self) {
        if self.hook_running.as_deref() == Some("esptool")
            || self.pending_writes > 0
            || self.pending_controls > 0
        {
            self.status =
                "Serial commands or esptool are running; wait for completion before quitting"
                    .to_string();
            self.dirty = true;
        } else {
            self.quit = true;
        }
    }

    fn open_popup(&mut self, popup: Popup) {
        self.popup = Some(popup);
        self.popup_selected = 0;
        if popup == Popup::Elf {
            self.elf_input = self
                .elf_path
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_default();
        }
        if popup == Popup::Save {
            self.prompt_input = format!("session-{}.log", now_ms());
        } else if popup == Popup::Flash {
            self.prompt_input = if std::path::Path::new("build/flasher_args.json").is_file() {
                "build".to_string()
            } else {
                String::new()
            };
        }
        self.dirty = true;
    }

    pub fn menu_items(&self) -> Vec<String> {
        match self.popup {
            Some(Popup::Actions) => vec![
                "Settings (F3)".to_string(),
                "Ports (Ctrl+O)".to_string(),
                "Reconnect current device".to_string(),
                "Disconnect".to_string(),
                "Clear output (Ctrl+L)".to_string(),
                format!(
                    "Timestamps: {} (Ctrl+T)",
                    if self.show_timestamps { "on" } else { "off" }
                ),
                format!(
                    "View: {} (F8)",
                    if self.view == View::Hex {
                        "HEX"
                    } else {
                        "text"
                    }
                ),
                "Search output (F7)".to_string(),
                format!("Input: {} (F2)", if self.raw_mode { "RAW" } else { "line" }),
                "ESP-IDF tools".to_string(),
                "Help (F1)".to_string(),
                "Save current session... (Ctrl+S)".to_string(),
                format!(
                    "Scroll lock: {} (Ctrl+B)",
                    if self.scroll_locked { "on" } else { "off" }
                ),
                format!("Packets: {}", if self.show_packets { "on" } else { "off" }),
            ],
            Some(Popup::Esp) => {
                let mut items = vec![
                    "Reset device (F5)".to_string(),
                    "Enter bootloader (F6)".to_string(),
                    "Load ELF symbols...".to_string(),
                    "Chip information (built-in)".to_string(),
                    "Flash information (built-in)".to_string(),
                    "Read MAC (built-in)".to_string(),
                    "Erase entire Flash...".to_string(),
                    "Flash firmware (build dir, ELF or images)...".to_string(),
                ];
                items.extend(
                    self.hooks
                        .iter()
                        .map(|hook| format!("Run hook: {} — {}", hook.name, hook.command)),
                );
                items
            }
            _ => Vec::new(),
        }
    }

    fn toggle_hex(&mut self) {
        self.view = if self.view == View::Text {
            View::Hex
        } else {
            View::Text
        };
        self.scroll_back = 0;
        self.total_rows = 0;
        self.cache.clear();
        self.starts.clear();
        self.cache_width = 0;
        self.version += 1;
    }

    fn toggle_timestamps(&mut self) {
        self.show_timestamps = !self.show_timestamps;
        self.version += 1;
    }

    fn toggle_lock(&mut self) {
        self.scroll_locked = !self.scroll_locked;
        if !self.scroll_locked {
            self.scroll_back = 0;
        }
        if !self.scroll_locked {
            self.tx_scroll_back = 0;
        }
        self.dirty = true;
    }

    fn toggle_packets(&mut self) {
        self.show_packets = !self.show_packets;
        self.scroll_back = 0;
        self.total_rows = 0;
        self.cache.clear();
        self.starts.clear();
        self.cache_width = 0;
        self.version += 1;
        self.dirty = true;
    }

    fn menu_action(&mut self, index: usize) {
        let popup = self.popup.take();
        match popup {
            Some(Popup::Actions) => match index {
                0 => self.open_settings(false),
                1 => self.open_ports(),
                2 => {
                    if self.demo {
                        self.start_demo();
                    } else if !self.label.is_empty() {
                        self.connect_with(&self.label.clone(), self.reset_kind);
                    } else {
                        self.open_ports();
                    }
                }
                3 => {
                    self.shutdown_worker();
                    self.repeat = None;
                    self.reopen_after_hook = None;
                    self.reconnecting = false;
                    self.demo = false;
                    self.status = "Disconnected; use Actions > Reconnect or Ports".to_string();
                }
                4 => self.clear(),
                5 => self.toggle_timestamps(),
                6 => self.toggle_hex(),
                7 => self.open_search(),
                8 => {
                    self.raw_mode = !self.raw_mode;
                    self.focus = Focus::Input;
                }
                9 => self.open_popup(Popup::Esp),
                10 => {
                    self.help = true;
                    self.help_scroll = 0;
                }
                11 => self.open_popup(Popup::Save),
                12 => self.toggle_lock(),
                13 => self.toggle_packets(),
                _ => {}
            },
            Some(Popup::Esp) => match index {
                0 => self.queue_reset(false),
                1 => self.queue_reset(true),
                2 => self.open_popup(Popup::Elf),
                3 => self.prepare_esptool(crate::esptool::Operation::ChipId, ""),
                4 => self.prepare_esptool(crate::esptool::Operation::FlashId, ""),
                5 => self.prepare_esptool(crate::esptool::Operation::ReadMac, ""),
                6 => self.prepare_esptool(crate::esptool::Operation::EraseFlash, ""),
                7 => self.open_popup(Popup::Flash),
                _ => {
                    if let Some(name) = self
                        .hooks
                        .get(index.saturating_sub(8))
                        .map(|hook| hook.name.clone())
                    {
                        self.run_hook(&name);
                    }
                }
            },
            _ => {}
        }
        self.dirty = true;
    }

    fn apply_elf_input(&mut self) {
        let path = self.elf_input.trim().trim_matches('"');
        if path.is_empty() {
            self.status = "Enter a path to the matching application ELF".to_string();
            return;
        }
        if self.set_elf(std::path::PathBuf::from(path)) {
            self.open_popup(Popup::Esp);
        }
    }

    fn prepare_esptool(&mut self, operation: crate::esptool::Operation, arguments: &str) {
        if self.demo || self.label.is_empty() {
            self.status =
                "Select and connect a real serial port before using ESP tools".to_string();
            return;
        }
        if self.hook_running.is_some() {
            self.status = "Wait for the running tool before starting another".to_string();
            return;
        }
        let baud = if operation.destructive() {
            self.flash_baud
        } else {
            115_200
        };
        match crate::esptool::build(&self.label, baud, operation, arguments) {
            Ok(invocation) if operation.destructive() => {
                self.pending_tool = Some(invocation);
                self.open_popup(Popup::ConfirmTool);
            }
            Ok(invocation) => self.launch_esptool(invocation),
            Err(err) => self.status = err,
        }
        self.dirty = true;
    }

    fn launch_esptool(&mut self, invocation: crate::esptool::Invocation) {
        let reopen = if self.writes.is_some() {
            Some((self.label.clone(), self.reset_kind))
        } else {
            None
        };
        self.shutdown_worker();
        self.repeat = None;
        let first = invocation.summary.lines().next().unwrap_or_default();
        self.status = format!("ESP tool running: {first}");
        self.hook_running = Some("esptool".to_string());
        self.reopen_after_hook = reopen;
        self.popup = None;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let outcome = crate::esptool::run(&invocation, |text| {
                let _ = tx.send(Event::ToolOutput(text));
            });
            let _ = tx.send(Event::HookDone {
                name: "esptool".to_string(),
                ok: outcome.ok,
                output: outcome.text,
            });
        });
        self.dirty = true;
    }

    fn apply_prompt(&mut self) {
        match self.popup {
            Some(Popup::Save) => {
                let path = std::path::PathBuf::from(self.prompt_input.trim().trim_matches('"'));
                match self.save_session(&path) {
                    Ok(()) => self.popup = None,
                    Err(err) => self.status = err,
                }
            }
            Some(Popup::Flash) => {
                // 单个路径 = 一键烧录工程/ELF；<offset> <file> 成对 = 原始镜像写入。
                let input = self.prompt_input.clone();
                let operation = if crate::esptool::is_single_path(&input) {
                    crate::esptool::Operation::Flash
                } else {
                    crate::esptool::Operation::WriteFlash
                };
                self.prepare_esptool(operation, &input);
            }
            Some(Popup::ConfirmTool) => {
                if self.hook_running.is_some() {
                    self.status = "Wait for the running tool before confirming another".to_string();
                    self.dirty = true;
                    return;
                }
                if let Some(invocation) = self.pending_tool.take() {
                    self.launch_esptool(invocation);
                }
            }
            _ => {}
        }
    }

    fn popup_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Esc {
            self.popup = None;
            self.pending_tool = None;
            return;
        }
        if matches!(
            self.popup,
            Some(Popup::Save | Popup::Flash | Popup::ConfirmTool)
        ) {
            match key.code {
                KeyCode::Enter => self.apply_prompt(),
                KeyCode::Up if self.popup == Some(Popup::ConfirmTool) => {
                    self.popup_selected = self.popup_selected.saturating_sub(1)
                }
                KeyCode::Down if self.popup == Some(Popup::ConfirmTool) => {
                    self.popup_selected = self.popup_selected.saturating_add(1)
                }
                KeyCode::PageUp if self.popup == Some(Popup::ConfirmTool) => {
                    self.popup_selected = self.popup_selected.saturating_sub(8)
                }
                KeyCode::PageDown if self.popup == Some(Popup::ConfirmTool) => {
                    self.popup_selected = self.popup_selected.saturating_add(8)
                }
                KeyCode::Home if self.popup == Some(Popup::ConfirmTool) => self.popup_selected = 0,
                KeyCode::End if self.popup == Some(Popup::ConfirmTool) => {
                    self.popup_selected = usize::MAX
                }
                KeyCode::Backspace => {
                    self.prompt_input.pop();
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.prompt_input.clear()
                }
                KeyCode::Char(ch)
                    if self.popup != Some(Popup::ConfirmTool)
                        && !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                        && self.prompt_input.len() < MAX_INPUT =>
                {
                    self.prompt_input.push(ch)
                }
                _ => {}
            }
            return;
        }
        if self.popup == Some(Popup::Elf) {
            match key.code {
                KeyCode::Enter => self.apply_elf_input(),
                KeyCode::Backspace => {
                    self.elf_input.pop();
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.elf_input.clear()
                }
                KeyCode::Char(ch)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                        && self.elf_input.len() < MAX_INPUT =>
                {
                    self.elf_input.push(ch)
                }
                _ => {}
            }
            return;
        }
        let count = self.menu_items().len();
        match key.code {
            KeyCode::Up | KeyCode::BackTab => {
                self.popup_selected = self.popup_selected.saturating_sub(1)
            }
            KeyCode::Down | KeyCode::Tab => {
                self.popup_selected = (self.popup_selected + 1).min(count.saturating_sub(1))
            }
            KeyCode::Home => self.popup_selected = 0,
            KeyCode::End => self.popup_selected = count.saturating_sub(1),
            KeyCode::Enter => self.menu_action(self.popup_selected),
            _ => {}
        }
    }

    pub fn on_mouse(&mut self, event: MouseEvent) {
        self.dirty = true;
        if let Some(dialog) = &mut self.settings_dialog {
            let action = dialog.on_mouse(event);
            self.settings_action(action);
            return;
        }
        let position = ratatui::layout::Position::new(event.column, event.row);
        let hit = self
            .hits_ui
            .iter()
            .rev()
            .find(|(rect, _)| rect.contains(position))
            .map(|(_, hit)| *hit);
        if self.help {
            match event.kind {
                MouseEventKind::ScrollUp => self.help_scroll = self.help_scroll.saturating_sub(3),
                MouseEventKind::ScrollDown => {
                    self.help_scroll = (self.help_scroll + 3).min(self.help_max_scroll)
                }
                MouseEventKind::Down(MouseButton::Left)
                    if matches!(hit, Some(Hit::Close)) || !self.popup_rect.contains(position) =>
                {
                    self.help = false
                }
                _ => {}
            }
            return;
        }
        if self.popup.is_some() {
            match event.kind {
                MouseEventKind::ScrollUp => {
                    self.popup_selected = self.popup_selected.saturating_sub(1)
                }
                MouseEventKind::ScrollDown => {
                    self.popup_selected =
                        (self.popup_selected + 1).min(self.menu_items().len().saturating_sub(1))
                }
                MouseEventKind::Down(MouseButton::Left) => match hit {
                    Some(Hit::Menu(index)) => self.menu_action(index),
                    Some(Hit::ElfApply) => self.apply_elf_input(),
                    Some(Hit::DialogApply) => self.apply_prompt(),
                    Some(Hit::Close) => {
                        self.popup = None;
                        self.pending_tool = None;
                    }
                    _ if !self.popup_rect.contains(position) => {
                        self.popup = None;
                        self.pending_tool = None;
                    }
                    _ => {}
                },
                _ => {}
            }
            return;
        }
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => match hit {
                Some(Hit::Toolbar(index)) => {
                    self.toolbar_selected = index;
                    self.toolbar_action(index);
                }
                Some(Hit::Refresh) => self.refresh_ports(),
                Some(Hit::Port(index)) => {
                    self.selected = index;
                    self.open_settings(true);
                }
                Some(Hit::Output | Hit::TxOutput) => {
                    self.focus = Focus::Output;
                    self.search_input = None;
                }
                Some(Hit::Input) => self.focus = Focus::Input,
                _ => {}
            },
            MouseEventKind::ScrollUp if self.mode == Mode::Picker => {
                self.selected = self.selected.saturating_sub(1)
            }
            MouseEventKind::ScrollDown if self.mode == Mode::Picker => {
                self.selected = (self.selected + 1).min(self.ports.len().saturating_sub(1))
            }
            MouseEventKind::ScrollUp if matches!(hit, Some(Hit::Output)) => {
                self.scroll_back = self.scroll_back.saturating_add(3)
            }
            MouseEventKind::ScrollDown if matches!(hit, Some(Hit::Output)) => {
                self.scroll_back = self.scroll_back.saturating_sub(3)
            }
            MouseEventKind::ScrollUp if matches!(hit, Some(Hit::TxOutput)) => {
                self.tx_scroll_back = self.tx_scroll_back.saturating_add(3)
            }
            MouseEventKind::ScrollDown if matches!(hit, Some(Hit::TxOutput)) => {
                self.tx_scroll_back = self.tx_scroll_back.saturating_sub(3)
            }
            _ => {}
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('q') {
            self.request_quit();
            return;
        }
        self.dirty = true;
        if let Some(dialog) = &mut self.settings_dialog {
            let action = dialog.on_key(key);
            self.settings_action(action);
            return;
        }
        if self.help {
            match key.code {
                KeyCode::Esc | KeyCode::F(1) => self.help = false,
                KeyCode::Up => self.help_scroll = self.help_scroll.saturating_sub(1),
                KeyCode::Down => {
                    self.help_scroll = (self.help_scroll + 1).min(self.help_max_scroll)
                }
                KeyCode::PageUp => {
                    self.help_scroll = self.help_scroll.saturating_sub(self.help_height)
                }
                KeyCode::PageDown => {
                    self.help_scroll =
                        (self.help_scroll + self.help_height).min(self.help_max_scroll)
                }
                KeyCode::Home => self.help_scroll = 0,
                KeyCode::End => self.help_scroll = self.help_max_scroll,
                _ => {}
            }
            return;
        }
        if self.popup.is_some() {
            self.popup_key(key);
            return;
        }
        match key.code {
            KeyCode::F(1) => {
                self.help = true;
                self.help_scroll = 0;
                return;
            }
            KeyCode::F(3) => {
                self.open_settings(false);
                return;
            }
            KeyCode::F(4) => {
                self.open_popup(Popup::Actions);
                return;
            }
            KeyCode::Char('o') if ctrl => {
                self.open_ports();
                return;
            }
            KeyCode::F(5) if self.mode == Mode::Terminal => {
                self.queue_reset(false);
                return;
            }
            KeyCode::F(6) if self.mode == Mode::Terminal => {
                self.queue_reset(true);
                return;
            }
            KeyCode::F(7) if self.mode == Mode::Terminal && self.search_input.is_none() => {
                self.open_search();
                return;
            }
            KeyCode::F(8) if self.mode == Mode::Terminal && self.search_input.is_none() => {
                self.toggle_hex();
                return;
            }
            KeyCode::ScrollLock | KeyCode::Char('b') if key.code == KeyCode::ScrollLock || ctrl => {
                self.toggle_lock();
                return;
            }
            KeyCode::Char('s') if ctrl => {
                self.open_popup(Popup::Save);
                return;
            }
            KeyCode::Tab | KeyCode::BackTab => {
                let backwards =
                    key.code == KeyCode::BackTab || key.modifiers.contains(KeyModifiers::SHIFT);
                self.focus = match (self.focus, backwards, self.mode) {
                    (Focus::Toolbar, false, _) => Focus::Output,
                    (Focus::Output, false, Mode::Terminal) => Focus::Input,
                    (Focus::Output, false, Mode::Picker) | (Focus::Input, false, _) => {
                        Focus::Toolbar
                    }
                    (Focus::Toolbar, true, Mode::Terminal) => Focus::Input,
                    (Focus::Toolbar, true, Mode::Picker) | (Focus::Input, true, _) => Focus::Output,
                    (Focus::Output, true, _) => Focus::Toolbar,
                };
                return;
            }
            _ => {}
        }
        if self.search_input.is_some() {
            self.search_key(key);
            return;
        }
        if self.focus == Focus::Toolbar {
            match key.code {
                KeyCode::Left | KeyCode::Up => {
                    self.toolbar_selected =
                        (self.toolbar_selected + TOOLBAR_ITEMS - 1) % TOOLBAR_ITEMS
                }
                KeyCode::Right | KeyCode::Down => {
                    self.toolbar_selected = (self.toolbar_selected + 1) % TOOLBAR_ITEMS
                }
                KeyCode::Enter | KeyCode::Char(' ') => self.toolbar_action(self.toolbar_selected),
                KeyCode::Esc => self.focus = Focus::Output,
                _ => {}
            }
            return;
        }
        if self.mode == Mode::Picker {
            self.picker_key(key);
            return;
        }
        match key.code {
            KeyCode::Char('l') if ctrl => {
                self.clear();
                return;
            }
            KeyCode::Char('t') if ctrl => {
                self.toggle_timestamps();
                return;
            }
            KeyCode::Char('x') if ctrl && !self.raw_mode => {
                self.execute("/txmode");
                return;
            }
            KeyCode::Char('r') if ctrl && !self.raw_mode => {
                self.cycle_eol();
                return;
            }
            KeyCode::Char('n') if ctrl && !self.raw_mode => {
                self.step_hit(true);
                return;
            }
            KeyCode::Char('p') if ctrl && !self.raw_mode => {
                self.step_hit(false);
                return;
            }
            KeyCode::F(2) => {
                self.raw_mode = !self.raw_mode;
                return;
            }
            KeyCode::Char(']') if ctrl && self.raw_mode => {
                self.raw_mode = false;
                return;
            }
            _ => {}
        }
        if self.focus == Focus::Output {
            match key.code {
                KeyCode::Up => self.scroll_back = self.scroll_back.saturating_add(1),
                KeyCode::Down => self.scroll_back = self.scroll_back.saturating_sub(1),
                KeyCode::PageUp => self.scroll_page(true),
                KeyCode::PageDown => self.scroll_page(false),
                KeyCode::Home => self.scroll_home(),
                KeyCode::End => self.scroll_end(),
                _ => {}
            }
            return;
        }
        if self.raw_mode {
            self.raw_send(key);
            return;
        }
        if let Some(entry) = self
            .macros
            .iter()
            .find(|entry| entry.key == key.code && entry.mods == key.modifiers)
            .cloned()
        {
            if self.writes.is_some() || self.demo {
                self.send_payload(&entry.text);
                self.status = format!("macro {}", entry.name);
            } else {
                self.status = "no port open".to_string();
            }
            return;
        }
        self.terminal_key(key);
    }

    fn open_search(&mut self) {
        if self.mode != Mode::Terminal || self.view == View::Hex {
            return;
        }
        if self.show_packets {
            self.toggle_packets();
        }
        let seed = self.search_query.clone().unwrap_or_default();
        self.focus = Focus::Input;
        self.search_input = Some(seed.chars().collect());
        self.dirty = true;
    }

    fn search_key(&mut self, key: KeyEvent) {
        let Some(mut input) = self.search_input.take() else {
            return;
        };
        match key.code {
            KeyCode::Esc => {}
            KeyCode::Enter => {
                let text: String = input.iter().collect();
                let text = text.trim().to_string();
                self.search_query = if text.is_empty() { None } else { Some(text) };
                self.run_search();
                if let Some(last) = self.hits.last().copied() {
                    self.jump_to(last);
                }
            }
            KeyCode::Backspace => {
                input.pop();
                self.search_input = Some(input);
            }
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                input.push(ch);
                self.search_input = Some(input);
            }
            _ => self.search_input = Some(input),
        }
        self.version += 1;
        self.dirty = true;
    }

    pub(crate) fn run_search(&mut self) {
        self.hits.clear();
        self.search_re = None;
        let Some(query) = self.search_query.clone() else {
            return;
        };
        if let Some(pattern) = query.strip_prefix("re:") {
            match regex::RegexBuilder::new(pattern)
                .case_insensitive(true)
                .build()
            {
                Ok(re) => {
                    let mut hits = Vec::new();
                    for (i, row) in self.scrollback.iter().enumerate() {
                        if re.is_match(&row.text()) {
                            hits.push(i);
                        }
                    }
                    self.hits = hits;
                    self.search_re = Some(re);
                }
                Err(err) => {
                    self.status = format!("bad regex: {err}");
                    self.dirty = true;
                }
            }
            return;
        }
        let needle: Vec<char> = query
            .to_lowercase()
            .chars()
            .map(|ch| ch.to_ascii_lowercase())
            .collect();
        let mut hits = Vec::new();
        for (i, row) in self.scrollback.iter().enumerate() {
            if match_mask(&row.cells, &needle).iter().any(|hit| *hit) {
                hits.push(i);
            }
        }
        self.hits = hits;
    }

    fn jump_to(&mut self, logical: usize) {
        let Some(start) = self.starts.get(logical).copied() else {
            return;
        };
        let height = usize::from(self.view_height.max(1));
        let max_top = self.total_rows.saturating_sub(height);
        let top = start.min(max_top);
        self.scroll_back = max_top - top;
        self.dirty = true;
    }

    fn step_hit(&mut self, forward: bool) {
        if self.show_packets {
            self.open_search();
            return;
        }
        if self.hits.is_empty() {
            return;
        }
        let height = usize::from(self.view_height.max(1));
        let max_top = self.total_rows.saturating_sub(height);
        let current_top = max_top.saturating_sub(self.scroll_back.min(max_top));
        let target = if forward {
            self.hits
                .iter()
                .copied()
                .find(|idx| {
                    self.starts
                        .get(*idx)
                        .is_some_and(|start| *start > current_top)
                })
                .or_else(|| self.hits.first().copied())
        } else {
            self.hits
                .iter()
                .rev()
                .copied()
                .find(|idx| {
                    self.starts
                        .get(*idx)
                        .is_some_and(|start| *start < current_top)
                })
                .or_else(|| self.hits.last().copied())
        };
        if let Some(target) = target {
            self.jump_to(target);
        }
    }

    pub fn load_history(&mut self) {
        if let Some(path) = crate::config::history_path() {
            self.load_history_from(&path);
        }
    }

    fn load_history_from(&mut self, path: &std::path::Path) {
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        self.history = text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| line.replace(HISTORY_NEWLINE, "\n"))
            .collect();
        let excess = self.history.len().saturating_sub(HISTORY_MAX);
        self.history.drain(..excess);
        self.history_pos = None;
    }

    fn save_history(&self) {
        if let Some(path) = crate::config::history_path() {
            self.save_history_to(&path);
        }
    }

    fn save_history_to(&self, path: &std::path::Path) {
        if let Some(dir) = path.parent()
            && std::fs::create_dir_all(dir).is_err()
        {
            return;
        }
        let body: String = self
            .history
            .iter()
            .map(|line| format!("{}\n", line.replace('\n', HISTORY_NEWLINE)))
            .collect();
        let _ = std::fs::write(path, body);
    }

    pub fn serial_label(&self) -> String {
        let bits = match self.settings.data_bits {
            serialport::DataBits::Five => 5,
            serialport::DataBits::Six => 6,
            serialport::DataBits::Seven => 7,
            serialport::DataBits::Eight => 8,
        };
        let parity = match self.settings.parity {
            serialport::Parity::None => "N",
            serialport::Parity::Odd => "O",
            serialport::Parity::Even => "E",
        };
        let stop = match self.settings.stop_bits {
            serialport::StopBits::One => 1,
            serialport::StopBits::Two => 2,
        };
        let flow = match self.settings.flow {
            serialport::FlowControl::None => "",
            serialport::FlowControl::Hardware => " RTS/CTS",
            serialport::FlowControl::Software => " XON/XOFF",
        };
        format!(
            "{} baud {}{}{}{}",
            self.settings.baud, bits, parity, stop, flow
        )
    }

    fn scroll_page(&mut self, up: bool) {
        let page = usize::from(self.view_height.max(1));
        self.scroll_back = if up {
            self.scroll_back.saturating_add(page)
        } else {
            self.scroll_back.saturating_sub(page)
        };
        self.dirty = true;
    }

    fn scroll_home(&mut self) {
        self.scroll_back = self
            .total_rows
            .saturating_sub(usize::from(self.view_height));
        self.dirty = true;
    }

    fn scroll_end(&mut self) {
        self.scroll_back = 0;
        self.scroll_locked = false;
        self.tx_scroll_back = 0;
        self.dirty = true;
    }

    fn picker_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc if !self.label.is_empty() => {
                self.mode = Mode::Terminal;
                self.focus = Focus::Output;
            }
            KeyCode::Char('r') | KeyCode::F(5) => self.refresh_ports(),
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                self.dirty = true;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.selected + 1 < self.ports.len() {
                    self.selected += 1;
                    self.dirty = true;
                }
            }
            KeyCode::Enter => self.connect_selected(),
            _ => {}
        }
    }

    fn terminal_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let width = usize::from(self.input_width.max(1));
        let edited = match key.code {
            KeyCode::PageUp => {
                self.scroll_page(true);
                false
            }
            KeyCode::PageDown => {
                self.scroll_page(false);
                false
            }
            KeyCode::Home if ctrl => {
                self.scroll_home();
                false
            }
            KeyCode::End if ctrl => {
                self.scroll_end();
                false
            }
            KeyCode::Esc => {
                self.focus = Focus::Output;
                false
            }
            // Shift/Alt+Enter 与 Ctrl+J 插入换行；不支持区分 Shift+Enter 的终端可用后两者。
            KeyCode::Enter if shift || alt => self.input.insert('\n'),
            KeyCode::Char('j') if ctrl => self.input.insert('\n'),
            KeyCode::Enter => {
                self.submit();
                false
            }
            KeyCode::Backspace if ctrl || alt => self.input.delete_word_back(),
            KeyCode::Backspace => self.input.backspace(),
            KeyCode::Delete => self.input.delete(),
            KeyCode::Left if ctrl || alt => {
                self.input.word_left();
                false
            }
            KeyCode::Right if ctrl || alt => {
                self.input.word_right();
                false
            }
            KeyCode::Left => {
                self.input.left();
                false
            }
            KeyCode::Right => {
                self.input.right();
                false
            }
            KeyCode::Home | KeyCode::Char('a') if key.code == KeyCode::Home || ctrl => {
                self.input.home();
                false
            }
            KeyCode::End | KeyCode::Char('e') if key.code == KeyCode::End || ctrl => {
                self.input.end();
                false
            }
            KeyCode::Char('w') if ctrl => self.input.delete_word_back(),
            KeyCode::Char('u') if ctrl => self.input.kill_to_line_start(),
            KeyCode::Char('k') if ctrl => self.input.kill_to_line_end(),
            KeyCode::Up => {
                if !self.input.vertical(true, width) {
                    self.history_prev();
                }
                false
            }
            KeyCode::Down => {
                if !self.input.vertical(false, width) {
                    self.history_next();
                }
                false
            }
            KeyCode::Char(ch) if !ctrl && !alt => self.input.insert(ch),
            _ => false,
        };
        if edited {
            self.history_pos = None;
        }
        self.dirty = true;
    }

    /// 终端 bracketed paste：输入框获得整段文本（含换行），RAW 模式原样发送。
    pub fn on_paste(&mut self, text: String) {
        self.dirty = true;
        if self.mode != Mode::Terminal
            || self.help
            || self.popup.is_some()
            || self.settings_dialog.is_some()
        {
            return;
        }
        if self.raw_mode {
            if self.focus != Focus::Output {
                self.send_bytes(text.into_bytes());
            }
            return;
        }
        if let Some(search) = &mut self.search_input {
            search.extend(text.chars().filter(|ch| !ch.is_control()));
            return;
        }
        self.focus = Focus::Input;
        let wanted = text.chars().count();
        let inserted = self.input.insert_str(&text);
        self.history_pos = None;
        if inserted < wanted {
            self.status = format!("input limit {MAX_INPUT} chars: paste truncated");
        }
    }

    fn cycle_eol(&mut self) {
        self.eol = match self.eol {
            Eol::CrLf => Eol::Lf,
            Eol::Lf => Eol::Cr,
            Eol::Cr => Eol::None,
            Eol::None => Eol::CrLf,
        };
        self.status = format!("line ending {}", self.eol.label());
    }

    fn clear(&mut self) {
        self.scrollback.clear();
        self.assembler.reset();
        self.rx_packets.clear();
        self.tx_packets.clear();
        self.rx_packet_bytes = 0;
        self.tx_packet_bytes = 0;
        self.tx_cache = PacketCache::default();
        self.tx_scroll_back = 0;
        self.raw.clear();
        self.cache.clear();
        self.starts.clear();
        self.cache_dropped = if self.show_packets {
            self.rx_packet_dropped
        } else {
            self.scrollback.dropped()
        };
        self.total_rows = 0;
        self.hits.clear();
        self.search_re = None;
        self.scroll_back = 0;
        self.version += 1;
        self.dirty = true;
    }

    fn submit(&mut self) {
        let text = self.input.text();
        if self.tx_hex
            && !text.starts_with('/')
            && !text.trim().is_empty()
            && let Err(err) = parse_hex(&text)
        {
            // 保留输入，方便修改后重发
            self.status = format!("HEX: {err}");
            self.dirty = true;
            return;
        }
        self.input.clear();
        self.history_pos = None;
        if !text.is_empty() && self.history.last() != Some(&text) {
            self.history.push(text.clone());
            let excess = self.history.len().saturating_sub(HISTORY_MAX);
            self.history.drain(..excess);
            self.save_history();
        }
        self.execute(&text);
        self.dirty = true;
    }

    fn raw_send(&mut self, key: KeyEvent) {
        if let Some(bytes) = raw_bytes(key) {
            self.send_bytes(bytes);
        }
    }

    fn send_bytes(&mut self, bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
        let len = bytes.len();
        match &self.writes {
            Some(writes) => {
                if writes.send(serial::Command::Write(bytes)).is_err() {
                    self.status = "write channel closed".to_string();
                } else {
                    self.pending_writes += 1;
                }
            }
            None if self.demo => {
                self.status = format!("{len} byte(s) sent to demo device");
                self.record_tx(bytes);
            }
            None => self.status = "no port open".to_string(),
        }
        self.dirty = true;
    }

    fn send_payload(&mut self, text: &str) {
        self.send_bytes(payload_bytes(text, self.eol, self.tx_escapes));
        if self.demo {
            let at = now_ms();
            for line in text.split('\n') {
                let cells = line
                    .chars()
                    .map(|ch| crate::term::scrollback::Cell::new(ch, PLAIN))
                    .collect();
                self.scrollback.push(Row { cells, at });
            }
            self.version += 1;
        }
    }

    fn queue_reset(&mut self, into_bootloader: bool) {
        let kind = self.reset_kind;
        match &self.writes {
            Some(writes) => {
                let command = serial::Command::Reset {
                    into_bootloader,
                    kind,
                };
                if writes.send(command).is_err() {
                    self.status = "write channel closed".to_string();
                } else {
                    self.pending_controls += 1;
                    self.status = if into_bootloader {
                        "entering bootloader…".to_string()
                    } else {
                        "resetting…".to_string()
                    };
                }
            }
            None if self.demo => {
                self.status = if into_bootloader {
                    "demo: bootloader ignored".to_string()
                } else {
                    "demo: reset ignored".to_string()
                };
            }
            None => self.status = "no port open".to_string(),
        }
        self.dirty = true;
    }

    fn queue_sequence(&mut self, steps: Vec<crate::serial::reset::Step>) {
        match &self.writes {
            Some(writes) => {
                if writes.send(serial::Command::Sequence(steps)).is_err() {
                    self.status = "write channel closed".to_string();
                } else {
                    self.pending_controls += 1;
                    self.status = "running custom sequence…".to_string();
                }
            }
            None if self.demo => self.status = "demo: sequence ignored".to_string(),
            None => self.status = "no port open".to_string(),
        }
        self.dirty = true;
    }

    pub fn execute(&mut self, text: &str) {
        // 多行命令逐行执行，便于一次粘贴/输入一组 / 命令；多行数据整体发送。
        if text.contains('\n') && text.starts_with('/') && !text.starts_with("//") {
            for line in text.lines().filter(|line| !line.trim().is_empty()) {
                self.execute(line);
            }
            return;
        }
        if self.tx_hex && !text.starts_with('/') {
            if !text.trim().is_empty() {
                match parse_hex(text) {
                    Ok(bytes) => {
                        let count = bytes.len();
                        self.send_bytes(bytes);
                        self.status = format!("sent {count} byte(s) HEX");
                    }
                    Err(err) => self.status = format!("HEX: {err}"),
                }
            }
        } else if text.starts_with("//") {
            self.send_payload(&text[1..]);
        } else if text.starts_with('/') {
            self.command(text);
        } else if !text.is_empty() {
            self.send_payload(text);
        }
        self.dirty = true;
    }

    fn command(&mut self, text: &str) {
        let rest = text.trim_start_matches('/').trim();
        match rest {
            "save" => {
                let path = self
                    .coredump_directory()
                    .join(format!("gtui-session-{}.log", now_ms()));
                if let Err(err) = self.save_session(&path) {
                    self.status = err;
                }
                return;
            }
            "lock" | "lock on" | "lock off" => {
                if rest == "lock" || self.scroll_locked != rest.ends_with("on") {
                    self.toggle_lock();
                }
                return;
            }
            "timestamps" | "timestamps on" | "timestamps off" => {
                if rest == "timestamps" || self.show_timestamps != rest.ends_with("on") {
                    self.toggle_timestamps();
                }
                return;
            }
            gap if gap.starts_with("packets gap") => {
                match gap["packets gap".len()..].trim().parse::<u64>() {
                    Ok(ms) if ms <= 60_000 => {
                        self.packet_gap_ms = ms;
                        self.status = if ms == 0 {
                            "packet framing: one packet per read".to_string()
                        } else {
                            format!("packet framing: idle gap {ms} ms (applies to new data)")
                        };
                    }
                    _ => {
                        self.status = format!(
                            "usage: /packets gap <0-60000 ms> (current {} ms, 0 = per read)",
                            self.packet_gap_ms
                        );
                    }
                }
                self.dirty = true;
                return;
            }
            "packets" | "packets on" | "packets off" => {
                if rest == "packets" || self.show_packets != rest.ends_with("on") {
                    self.toggle_packets();
                }
                return;
            }
            "confirm" => {
                if self.popup == Some(Popup::ConfirmTool) {
                    self.apply_prompt();
                } else {
                    self.status = "No pending ESP operation to confirm".to_string();
                }
                return;
            }
            "cancel" => {
                self.pending_tool = None;
                self.popup = None;
                return;
            }
            "quit" => {
                self.request_quit();
                return;
            }
            _ => {}
        }
        if let Some(path) = rest.strip_prefix("save ") {
            if let Err(err) = self.save_session(std::path::Path::new(path.trim().trim_matches('"')))
            {
                self.status = err;
            }
            return;
        }
        if let Some(args) = rest.strip_prefix("esp ") {
            let (operation, arguments) = args.split_once(' ').unwrap_or((args, ""));
            let operation = match operation {
                "chip-id" => crate::esptool::Operation::ChipId,
                "flash-id" => crate::esptool::Operation::FlashId,
                "read-mac" => crate::esptool::Operation::ReadMac,
                "erase-flash" => crate::esptool::Operation::EraseFlash,
                "write-flash" => crate::esptool::Operation::WriteFlash,
                "flash" => crate::esptool::Operation::Flash,
                "baud" => {
                    match arguments.trim().parse::<u32>() {
                        Ok(baud) if (9600..=5_000_000).contains(&baud) => {
                            self.flash_baud = baud;
                            self.status = format!("flash baud {baud}");
                        }
                        _ => {
                            self.status = format!(
                                "usage: /esp baud <9600-5000000> (current {})",
                                self.flash_baud
                            )
                        }
                    }
                    return;
                }
                _ => {
                    self.status = "ESP commands: chip-id, flash-id, read-mac, erase-flash, flash <build dir|app.elf>, write-flash <offset> <file> ..., baud <n>".to_string();
                    return;
                }
            };
            self.prepare_esptool(operation, arguments);
            return;
        }
        match rest {
            "clear" | "cls" => {
                self.clear();
                return;
            }
            "help" => {
                self.help = true;
                self.help_scroll = 0;
                return;
            }
            "esp" => {
                self.open_popup(Popup::Esp);
                return;
            }
            "settings" => {
                self.open_settings(false);
                return;
            }
            "ports" => {
                self.open_ports();
                return;
            }
            _ => {}
        }
        if let Some(path) = rest.strip_prefix("elf ") {
            self.set_elf(std::path::PathBuf::from(path.trim().trim_matches('"')));
            return;
        }
        if rest == "reset" {
            self.queue_reset(false);
            return;
        }
        if rest == "macro" || rest.starts_with("macro ") || rest.starts_with("macro\t") {
            let args = rest[5..].trim_start();
            let parsed = args
                .split_once(char::is_whitespace)
                .and_then(|(name, args)| {
                    args.trim_start()
                        .split_once(char::is_whitespace)
                        .map(|(key, payload)| (name, key, payload.trim_start()))
                });
            let Some((name, key, payload)) = parsed.filter(|(_, _, text)| !text.is_empty()) else {
                self.status = "usage: /macro <name> <f9-f12|alt+char> <text>".to_string();
                return;
            };
            match crate::config::save_macro(&self.config_path, name, key, payload) {
                Ok(entry) => {
                    self.macros.retain(|existing| existing.name != name);
                    self.macros.push(entry);
                    self.status = format!("macro {name} saved to {}", self.config_path.display());
                }
                Err(err) => self.status = format!("macro: {err}"),
            }
            return;
        }
        if let Some(arg) = rest.strip_prefix("eol") {
            let arg = arg.trim();
            match Eol::parse(arg) {
                Some(eol) => {
                    self.eol = eol;
                    self.status = format!("line ending {}", eol.label());
                }
                None => self.status = "usage: /eol crlf|lf|cr|none".to_string(),
            }
            return;
        }
        if let Some(arg) = rest.strip_prefix("txmode") {
            match arg.trim() {
                "" => self.tx_hex = !self.tx_hex,
                "hex" => self.tx_hex = true,
                "text" => self.tx_hex = false,
                _ => {
                    self.status = "usage: /txmode [text|hex]".to_string();
                    return;
                }
            }
            self.status = format!("send mode {}", if self.tx_hex { "HEX" } else { "TEXT" });
            return;
        }
        if let Some(arg) = rest.strip_prefix("escapes") {
            match arg.trim() {
                "" => self.tx_escapes = !self.tx_escapes,
                "on" => self.tx_escapes = true,
                "off" => self.tx_escapes = false,
                _ => {
                    self.status = "usage: /escapes [on|off]".to_string();
                    return;
                }
            }
            self.status = if self.tx_escapes {
                "text escapes on: \\r \\n \\t \\0 \\\\ \\xHH".to_string()
            } else {
                "text escapes off".to_string()
            };
            return;
        }
        if let Some(arg) = rest.strip_prefix("rxcr") {
            let on = match arg.trim() {
                "" => !self.assembler.cr_newline(),
                "newline" | "on" => true,
                "overwrite" | "off" => false,
                _ => {
                    self.status = "usage: /rxcr [newline|overwrite]".to_string();
                    return;
                }
            };
            self.assembler.set_cr_newline(on);
            self.status = if on {
                "RX: lone CR starts a new line".to_string()
            } else {
                "RX: CR returns to line start (progress bars)".to_string()
            };
            return;
        }
        if rest == "counters reset" {
            self.rx_bytes = 0;
            self.tx_bytes = 0;
            self.rx_rate = 0;
            self.rate_mark = (std::time::Instant::now(), 0);
            self.status = "RX/TX counters reset".to_string();
            return;
        }
        for name in ["dtr", "rts"] {
            if let Some(arg) = rest.strip_prefix(name)
                && (arg.is_empty() || arg.starts_with(' '))
            {
                let level = match arg.trim() {
                    "on" | "1" => true,
                    "off" | "0" => false,
                    _ => {
                        self.status = format!("usage: /{name} on|off");
                        return;
                    }
                };
                let step = if name == "dtr" {
                    crate::serial::reset::Step::Dtr(level)
                } else {
                    crate::serial::reset::Step::Rts(level)
                };
                self.queue_sequence(vec![step]);
                if self.writes.is_some() {
                    self.status = format!(
                        "{} {}",
                        name.to_uppercase(),
                        if level { "on" } else { "off" }
                    );
                }
                return;
            }
        }
        if let Some(arg) = rest.strip_prefix("hex ") {
            match parse_hex(arg) {
                Ok(bytes) => {
                    let count = bytes.len();
                    self.send_bytes(bytes);
                    self.status = format!("sent {count} byte(s)");
                }
                Err(err) => self.status = err,
            }
            return;
        }
        if let Some(arg) = rest.strip_prefix("filter") {
            let arg = arg.trim();
            if arg.is_empty() || arg == "all" {
                self.levels = [true; 3];
                self.status = "filter off".to_string();
                return;
            }
            let mut levels = [false; 3];
            for token in arg.split([' ', ',']) {
                match token.to_ascii_uppercase().as_str() {
                    "I" => levels[0] = true,
                    "W" => levels[1] = true,
                    "E" => levels[2] = true,
                    _ => {
                        self.status = "usage: /filter i,w,e | /filter all".to_string();
                        return;
                    }
                }
            }
            self.levels = levels;
            self.status = format!("showing {} (applies to new lines)", arg.to_uppercase());
            return;
        }
        if let Some(arg) = rest.strip_prefix("seq ") {
            match crate::serial::reset::parse_steps(arg) {
                Ok(steps) => self.queue_sequence(steps),
                Err(err) => self.status = format!("sequence: {err}"),
            }
            return;
        }
        if rest == "hooks" {
            let names: Vec<&str> = self.hooks.iter().map(|h| h.name.as_str()).collect();
            self.status = if names.is_empty() {
                "no hooks configured (add [[hooks]] to .gtui-terms.toml)".to_string()
            } else {
                format!("hooks: {}", names.join(", "))
            };
            return;
        }
        if let Some(name) = rest.strip_prefix("hook ") {
            self.run_hook(name.trim());
            return;
        }
        if rest == "boot" || rest == "bootloader" {
            self.queue_reset(true);
            return;
        }
        if rest == "stop" {
            let stopped = self.repeat.take().is_some();
            self.status = if stopped {
                "repeat stopped".to_string()
            } else {
                "no repeat running".to_string()
            };
            return;
        }
        let Some(args) = rest.strip_prefix("repeat") else {
            self.status = format!("unknown command {text} (try /help)");
            return;
        };
        let mut parts = args.trim_start().splitn(3, ' ');
        let every_ms = parts.next().and_then(|v| v.parse::<u64>().ok());
        let count = parts.next().and_then(|v| v.parse::<usize>().ok());
        let payload = parts.next().unwrap_or("").trim().to_string();
        let (Some(every_ms), Some(count)) = (every_ms, count) else {
            self.status = "usage: /repeat <interval_ms> <count|0> <text>".to_string();
            return;
        };
        if every_ms == 0 || payload.is_empty() {
            self.status = "usage: /repeat <interval_ms> <count|0> <text>".to_string();
            return;
        }
        let left = if count == 0 { None } else { Some(count) };
        self.repeat = Some(Repeat {
            payload,
            every: std::time::Duration::from_millis(every_ms),
            next: std::time::Instant::now(),
            left,
        });
        self.status = format!("repeat every {every_ms}ms x{count}");
    }

    /// Fires due repeat sends; call every loop iteration.
    pub fn tick(&mut self) {
        self.refresh_rate();
        if self.mode == Mode::Picker
            && self.ports_refreshed.elapsed() >= std::time::Duration::from_millis(500)
        {
            self.refresh_ports();
        }
        let Some(mut rep) = self.repeat.take() else {
            return;
        };
        if std::time::Instant::now() < rep.next {
            self.repeat = Some(rep);
            return;
        }
        rep.next += rep.every;
        if let Some(left) = &mut rep.left {
            *left = left.saturating_sub(1);
        }
        let done = rep.left == Some(0);
        let payload = rep.payload.clone();
        if !done {
            self.repeat = Some(rep);
        }
        self.send_payload(&payload);
        if done {
            self.status = "repeat finished".to_string();
        }
        self.dirty = true;
    }

    fn refresh_rate(&mut self) {
        let (mark, mark_bytes) = self.rate_mark;
        let elapsed = mark.elapsed();
        if elapsed < std::time::Duration::from_millis(500) {
            return;
        }
        let elapsed_ms = elapsed.as_millis().max(1) as u64;
        self.rx_rate = (self.rx_bytes.saturating_sub(mark_bytes)) * 1000 / elapsed_ms;
        self.rate_mark = (std::time::Instant::now(), self.rx_bytes);
    }

    fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let pos = match self.history_pos {
            None => self.history.len() - 1,
            Some(0) => 0,
            Some(pos) => pos - 1,
        };
        self.history_pos = Some(pos);
        self.input.set(&self.history[pos].clone());
        self.dirty = true;
    }

    fn history_next(&mut self) {
        let Some(pos) = self.history_pos else { return };
        if pos + 1 < self.history.len() {
            self.history_pos = Some(pos + 1);
            self.input.set(&self.history[pos + 1].clone());
        } else {
            self.history_pos = None;
            self.input.clear();
        }
        self.dirty = true;
    }

    pub fn finish(&mut self) {
        self.shutdown_worker();
    }

    fn shutdown_worker(&mut self) {
        if let Some(worker) = self.worker.take() {
            self.stop.store(true, Ordering::Relaxed);
            let _ = worker.join();
        }
        self.writes = None;
        let mut hook_done = None;
        while let Ok(event) = self.rx.try_recv() {
            match event {
                Event::HookDone { .. } => hook_done = Some(event),
                Event::Written(_)
                | Event::WriteFailed(_)
                | Event::ControlDone(_)
                | Event::ToolOutput(_) => self.on_event(event),
                Event::Data(bytes) => self.record_session(now_ms(), "RX", &bytes),
                _ => {}
            }
        }
        self.pending_writes = 0;
        self.pending_controls = 0;
        if let Some(event) = hook_done {
            let _ = self.tx.send(event);
        }
    }

    #[cfg(test)]
    pub fn input_text(&self) -> String {
        self.input.text()
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.shutdown_worker();
        self.save_history();
        if let Some(log) = &mut self.log {
            log.flush();
        }
    }
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn elf_sha256(path: &std::path::Path) -> Option<String> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path).ok()?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let digest = hasher.finalize();
    Some(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

pub fn parse_hex(text: &str) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    for token in text.split([' ', ',', '\n', '\t']) {
        if token.is_empty() {
            continue;
        }
        let clean = token.trim_start_matches("0x").trim_start_matches("0X");
        // 允许连续书写（AA55）；一或两位为一个字节，更长时必须成对。
        if clean.is_empty()
            || (clean.len() > 2 && clean.len() % 2 == 1)
            || !clean.chars().all(|ch| ch.is_ascii_hexdigit())
        {
            return Err(format!("{token}: use two hex digits per byte"));
        }
        let pairs: Vec<&str> = if clean.len() <= 2 {
            vec![clean]
        } else {
            (0..clean.len())
                .step_by(2)
                .map(|at| &clean[at..at + 2])
                .collect()
        };
        for pair in pairs {
            bytes.push(u8::from_str_radix(pair, 16).map_err(|_| format!("{token}: bad byte"))?);
        }
    }
    if bytes.is_empty() {
        return Err("usage: /hex 01 0d 0a".to_string());
    }
    Ok(bytes)
}

fn level_of(cells: &[Cell]) -> usize {
    for cell in cells {
        match cell.ch {
            ' ' | '\t' => continue,
            'W' | 'w' => return 1,
            'E' | 'e' => return 2,
            _ => return 0,
        }
    }
    0
}

/// Turns a key press into the bytes a terminal app on the other end expects.
fn raw_bytes(key: KeyEvent) -> Option<Vec<u8>> {
    Some(match key.code {
        KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if c.is_ascii_alphabetic() {
                vec![(c.to_ascii_lowercase() as u8) & 0x1f]
            } else {
                return None;
            }
        }
        KeyCode::Char(c) => c.to_string().into_bytes(),
        KeyCode::Enter => b"\r".to_vec(),
        KeyCode::Tab => b"\t".to_vec(),
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        _ => return None,
    })
}

/// Appends `packet`, or merges it into the previous one when it arrived within
/// `gap_ms` of that packet's last chunk (idle-gap framing). Returns how many old
/// packets were evicted to stay within the caps.
fn retain_packet(
    packets: &mut VecDeque<Packet>,
    size: &mut usize,
    packet: Packet,
    gap_ms: Option<u64>,
) -> usize {
    *size += packet.bytes.len();
    let merge_into = gap_ms.filter(|gap| *gap > 0).and_then(|gap| {
        packets.back_mut().filter(|last| {
            packet.at >= last.last
                && packet.at - last.last <= gap
                && last.bytes.len() + packet.bytes.len() <= PACKET_MAX_BYTES
        })
    });
    match merge_into {
        Some(last) => {
            last.last = packet.at;
            last.bytes.extend_from_slice(&packet.bytes);
        }
        None => packets.push_back(packet),
    }
    let mut removed = 0;
    while packets.len() > PACKET_CAP || *size > RAW_CAP {
        if let Some(old) = packets.pop_front() {
            *size -= old.bytes.len();
            removed += 1;
        }
    }
    removed
}
/// 解析 TEXT 模式转义；无法识别的转义保持原样，便于发送字面反斜杠。
fn unescape(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'\\' || index + 1 >= bytes.len() {
            out.push(bytes[index]);
            index += 1;
            continue;
        }
        let (value, used) = match bytes[index + 1] {
            b'r' => (b'\r', 2),
            b'n' => (b'\n', 2),
            b't' => (b'\t', 2),
            b'0' => (0, 2),
            b'\\' => (b'\\', 2),
            b'x' => match text
                .get(index + 2..index + 4)
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
            {
                Some(value) => (value, 4),
                None => (b'\\', 1),
            },
            _ => (b'\\', 1),
        };
        out.push(value);
        index += used;
    }
    out
}

/// 输入框文本转发送字节：内部换行与结尾都使用所选行尾；行尾为 none 时保留用户输入的 `\n`。
fn payload_bytes(text: &str, eol: Eol, escapes: bool) -> Vec<u8> {
    let sep: &[u8] = if eol == Eol::None { b"\n" } else { eol.bytes() };
    let mut bytes = Vec::with_capacity(text.len() + 2);
    for (index, line) in text.split('\n').enumerate() {
        if index > 0 {
            bytes.extend_from_slice(sep);
        }
        if escapes {
            bytes.extend(unescape(line));
        } else {
            bytes.extend_from_slice(line.as_bytes());
        }
    }
    bytes.extend_from_slice(eol.bytes());
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn typed(app: &mut App, text: &str) {
        for ch in text.chars() {
            app.on_key(key(KeyCode::Char(ch)));
        }
    }

    fn app() -> App {
        let mut app = App::new(115200, Vec::new(), None, false, Vec::new(), Vec::new());
        app.mode = Mode::Terminal;
        app
    }

    fn capture_output(app: &mut App) -> Arc<std::sync::Mutex<Vec<u8>>> {
        struct Output(Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Output {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let output = Arc::new(std::sync::Mutex::new(Vec::new()));
        app.record_output = Some(Box::new(Output(Arc::clone(&output))));
        output
    }

    #[test]
    fn serial_controls_block_quit_until_success_or_failure_is_reported() {
        let mut app = app();
        let output = capture_output(&mut app);
        let (writes, commands) = channel();
        app.writes = Some(writes);
        app.execute("/reset");
        app.execute("/boot");
        app.execute("/seq D0");
        assert_eq!(app.pending_controls, 3);
        assert!(matches!(
            commands.try_recv().unwrap(),
            serial::Command::Reset {
                into_bootloader: false,
                ..
            }
        ));
        assert!(matches!(
            commands.try_recv().unwrap(),
            serial::Command::Reset {
                into_bootloader: true,
                ..
            }
        ));
        assert!(matches!(
            commands.try_recv().unwrap(),
            serial::Command::Sequence(_)
        ));
        app.execute("/quit");
        assert!(!app.quit);
        app.on_event(Event::Written(Vec::new()));
        assert_eq!(app.pending_controls, 3);
        for text in [
            "chip reset",
            "entered bootloader",
            "sequence failed: disconnected",
        ] {
            app.on_event(Event::ControlDone(text.to_string()));
        }
        assert_eq!(app.pending_controls, 0);
        app.on_event(Event::Reconnecting);
        let captured = String::from_utf8(output.lock().unwrap().clone()).unwrap();
        assert_eq!(captured.matches("direction=\"STATE\"").count(), 3);
        assert!(captured.contains("chip reset"));
        assert!(captured.contains("entered bootloader"));
        assert!(captured.contains("sequence failed: disconnected"));
        app.execute("/quit");
        assert!(app.quit);
    }

    #[test]
    fn closed_control_channel_never_leaves_pending_work() {
        let mut app = app();
        let (writes, commands) = channel();
        app.writes = Some(writes);
        drop(commands);
        for command in ["/reset", "/boot", "/seq D0"] {
            app.execute(command);
            assert_eq!(app.pending_controls, 0);
            assert_eq!(app.status, "write channel closed");
        }
    }

    #[test]
    fn shutdown_routes_queued_tool_output_and_control_completion_to_capture() {
        let mut app = app();
        let output = capture_output(&mut app);
        let (writes, _commands) = channel();
        app.writes = Some(writes);
        app.execute("/reset");
        app.execute("/hex 41");
        app.tx
            .send(Event::ControlDone("chip reset".to_string()))
            .unwrap();
        app.tx.send(Event::Written(b"A".to_vec())).unwrap();
        app.tx
            .send(Event::ToolOutput("queued output\n".to_string()))
            .unwrap();
        app.shutdown_worker();
        assert_eq!(app.pending_controls, 0);
        assert_eq!(app.pending_writes, 0);
        assert_eq!(app.tx_bytes, 1);
        let captured = String::from_utf8(output.lock().unwrap().clone()).unwrap();
        assert!(captured.contains("direction=\"TOOL\""));
        assert!(captured.contains("queued output"));
        assert!(captured.contains("chip reset"));
        let path = std::env::temp_dir().join(format!(
            "gtui-shutdown-session-{}-{}.log",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        app.save_session(&path).unwrap();
        let captured = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(path).unwrap();
        assert!(captured.contains("direction=\"TOOL\""));
        assert!(captured.contains("queued output"));
    }

    #[test]
    fn session_retains_exact_rx_and_only_confirmed_tx_after_clear() {
        let mut app = app();
        let (writes, received) = channel();
        app.writes = Some(writes);
        app.execute("/hex 41 00 ff");
        assert_eq!(app.pending_writes, 1);
        assert_eq!(app.tx_bytes, 0);
        assert!(app.tx_packets.is_empty());
        let serial::Command::Write(bytes) = received.try_recv().unwrap() else {
            panic!("expected write");
        };
        assert_eq!(bytes, [0x41, 0, 0xff]);
        app.on_event(Event::Written(bytes));
        assert_eq!(app.pending_writes, 0);
        assert_eq!(app.tx_bytes, 3);
        app.on_event(Event::Data(b"device\n".to_vec()));
        let diagnostics: String = (0..60).map(|n| format!("build-failure-{n}\n")).collect();
        app.on_hook_done("build".to_string(), false, diagnostics);
        app.clear();
        assert!(app.tx_packets.is_empty() && app.rx_packets.is_empty() && app.raw.is_empty());
        let path = std::env::temp_dir().join(format!(
            "gtui-app-session-{}-{}.log",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        app.save_session(&path).unwrap();
        let captured = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(path).unwrap();
        assert!(captured.contains("direction=\"TX\" length=3 hex=\"4100ff\""));
        assert!(captured.contains("direction=\"RX\" length=7 hex=\"6465766963650a\""));
        assert!(captured.contains("direction=\"TOOL\""));
        assert!(captured.contains("build-failure-59\\n"));
    }

    #[test]
    fn failed_write_is_not_counted_or_displayed_and_destructive_tools_wait_for_confirmation() {
        let mut app = app();
        let (writes, _received) = channel();
        app.writes = Some(writes);
        app.execute("/hex ff");
        app.on_event(Event::WriteFailed("disconnected".into()));
        assert_eq!(app.pending_writes, 0);
        assert_eq!(app.tx_bytes, 0);
        assert!(app.tx_packets.is_empty());
        app.label = "COM-test".to_string();
        app.execute("/esp erase-flash");
        assert!(app.popup == Some(Popup::ConfirmTool));
        assert!(
            app.pending_tool
                .as_ref()
                .unwrap()
                .summary
                .contains("COM-test")
        );
        assert!(app.active_hook().is_none());
        app.hook_running = Some("build".to_string());
        app.execute("/confirm");
        assert_eq!(app.active_hook(), Some("build"));
        assert!(app.pending_tool.is_some());
        assert!(app.reopen_after_hook.is_none());
        app.hook_running = None;
        app.execute("/cancel");
        assert!(app.pending_tool.is_none() && app.popup.is_none());
        assert!(app.active_hook().is_none());
    }

    #[test]
    fn typing_inserts_at_cursor_and_delete_removes() {
        let mut app = app();
        typed(&mut app, "helo");
        app.on_key(key(KeyCode::Left));
        typed(&mut app, "l");
        assert_eq!(app.input_text(), "hello");
        app.on_key(key(KeyCode::Home));
        app.on_key(key(KeyCode::Delete));
        assert_eq!(app.input_text(), "ello");
        app.on_key(key(KeyCode::End));
        app.on_key(key(KeyCode::Backspace));
        assert_eq!(app.input_text(), "ell");
    }

    #[test]
    fn history_walks_backwards_then_clears() {
        let mut app = app();
        typed(&mut app, "one");
        app.on_key(key(KeyCode::Enter));
        typed(&mut app, "two");
        app.on_key(key(KeyCode::Enter));
        assert!(app.input.is_empty());
        app.on_key(key(KeyCode::Up));
        assert_eq!(app.input_text(), "two");
        app.on_key(key(KeyCode::Up));
        assert_eq!(app.input_text(), "one");
        app.on_key(key(KeyCode::Up));
        assert_eq!(app.input_text(), "one");
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.input_text(), "two");
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.input_text(), "");
    }

    #[test]
    fn multiline_payload_uses_selected_eol_and_commands_run_per_line() {
        assert_eq!(payload_bytes("a\nb", Eol::CrLf, false), b"a\r\nb\r\n");
        assert_eq!(payload_bytes("a\nb", Eol::Cr, false), b"a\rb\r");
        assert_eq!(payload_bytes("a\nb", Eol::None, false), b"a\nb");
        let mut app = app();
        app.execute("/eol lf\n\n/timestamps on");
        assert_eq!(app.eol, Eol::Lf);
        assert!(app.show_timestamps);
    }

    #[test]
    fn hex_mode_escapes_and_eol_cycle_shape_sent_bytes() {
        let mut app = app();
        app.demo = true;
        app.focus = Focus::Input;
        let ctrl = |ch| KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL);
        let last_tx = |app: &App| app.tx_packets.back().map(|p| p.bytes.clone());

        app.on_key(ctrl('x'));
        assert!(app.tx_hex);
        typed(&mut app, "AA 5");
        typed(&mut app, "5zz");
        app.on_key(key(KeyCode::Enter));
        assert!(app.status.starts_with("HEX:"), "{}", app.status);
        assert_eq!(app.input_text(), "AA 55zz", "invalid HEX input is kept");
        app.on_key(key(KeyCode::Backspace));
        app.on_key(key(KeyCode::Backspace));
        app.on_key(key(KeyCode::Enter));
        assert_eq!(last_tx(&app), Some(vec![0xaa, 0x55]), "no EOL in HEX mode");
        app.on_key(ctrl('x'));
        assert!(!app.tx_hex);

        app.on_key(ctrl('r'));
        assert_eq!(app.eol, Eol::Lf);
        app.execute("/escapes on");
        typed(&mut app, "a\\x01\\r\\q");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(last_tx(&app), Some(b"a\x01\r\\q\n".to_vec()));
        for _ in 0..3 {
            app.on_key(ctrl('r'));
        }
        assert_eq!(app.eol, Eol::CrLf);
        app.execute("/counters reset");
        assert_eq!((app.rx_bytes, app.tx_bytes), (0, 0));
        app.execute("/dtr maybe");
        assert_eq!(app.status, "usage: /dtr on|off");
    }

    #[test]
    fn rx_lone_cr_can_start_new_lines() {
        let mut app = app();
        app.execute("/rxcr newline");
        app.on_event(Event::Data(b"one\rtwo\r".to_vec()));
        app.on_event(Event::Data(b"\nthree\r\n".to_vec()));
        let rows: Vec<String> = app
            .scrollback
            .iter()
            .map(|row| row.cells.iter().map(|cell| cell.ch).collect())
            .collect();
        assert_eq!(rows, ["one", "two", "three"]);
        app.execute("/rxcr overwrite");
        assert!(!app.assembler.cr_newline());
    }

    #[test]
    fn editing_shortcuts_and_multiline_history_round_trip() {
        let mut app = app();
        app.focus = Focus::Input;
        typed(&mut app, "hello big world");
        let ctrl = |ch| KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL);
        app.on_key(ctrl('w'));
        assert_eq!(app.input_text(), "hello big ");
        app.on_key(ctrl('a'));
        app.on_key(ctrl('k'));
        assert_eq!(app.input_text(), "");
        typed(&mut app, "x");
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));
        typed(&mut app, "y");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.history.last().map(String::as_str), Some("x\ny"));

        let dir = std::env::temp_dir().join(format!("gtui-ml-history-{}", now_ms()));
        let path = dir.join("history");
        app.save_history_to(&path);
        assert!(!std::fs::read_to_string(&path).unwrap().contains("x\ny"));
        let mut fresh = self::app();
        fresh.load_history_from(&path);
        assert_eq!(fresh.history.last().map(String::as_str), Some("x\ny"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn submit_without_port_reports_status() {
        let mut app = app();
        typed(&mut app, "at+ver?");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.status, "no port open");
        assert!(app.input.is_empty());
    }

    #[test]
    fn received_data_is_assembled_into_rows() {
        let mut app = app();
        app.on_event(Event::Data(b"I (1) boot\r\nprogress 10%\r".to_vec()));
        app.on_event(Event::Data(b"progress 90%\r\n".to_vec()));
        assert_eq!(app.scrollback.len(), 2);
        assert_eq!(app.version, 3);
    }

    #[test]
    fn raw_mode_forwards_keys_without_touching_the_line_buffer() {
        let mut app = app();
        app.on_key(key(KeyCode::F(2)));
        assert!(app.raw_mode);
        app.on_key(key(KeyCode::Char('a')));
        assert!(
            app.input.is_empty(),
            "raw mode must not edit the line buffer"
        );
        assert_eq!(app.status, "no port open");
        app.on_key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::CONTROL));
        assert!(!app.raw_mode, "Ctrl+] leaves raw mode");
        assert!(!app.quit, "Ctrl+] must not quit");
    }

    #[test]
    fn regex_search_only_matches_its_pattern() {
        let mut app = app();
        for line in ["error code 12", "error code 7", "info ok"] {
            app.push_text(line, PLAIN, 0);
        }
        app.search_query = Some(r"re:code \d{2}".to_string());
        app.run_search();
        assert_eq!(app.hits.len(), 1, "only the two-digit line matches");
        app.search_query = Some("re:[unclosed".to_string());
        app.run_search();
        assert!(app.hits.is_empty());
        assert!(app.status.contains("bad regex"), "{}", app.status);
    }

    #[test]
    fn raw_bytes_match_what_a_terminal_expects() {
        assert_eq!(raw_bytes(key(KeyCode::Char('a'))).unwrap(), b"a");
        assert_eq!(raw_bytes(key(KeyCode::Enter)).unwrap(), b"\r");
        assert_eq!(
            raw_bytes(KeyEvent::new(KeyCode::Char('C'), KeyModifiers::CONTROL)).unwrap(),
            vec![3]
        );
        assert_eq!(raw_bytes(key(KeyCode::Up)).unwrap(), b"\x1b[A");
        assert_eq!(raw_bytes(key(KeyCode::F(3))), None);
    }

    #[test]
    fn history_round_trips_through_a_file() {
        let dir = std::env::temp_dir().join(format!("gtui-hist-{}", std::process::id()));
        let path = dir.join("history.txt");
        let mut base = app();
        typed(&mut base, "/eol lf");
        base.on_key(key(KeyCode::Enter));
        typed(&mut base, "/filter e");
        base.on_key(key(KeyCode::Enter));
        base.save_history_to(&path);

        let mut fresh = app();
        fresh.load_history_from(&path);
        assert_eq!(fresh.history.last().map(String::as_str), Some("/filter e"));
        fresh.on_key(key(KeyCode::Up));
        assert_eq!(fresh.input.text(), "/filter e");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn eol_and_filter_commands_change_state() {
        let mut app = app();
        typed(&mut app, "/eol lf");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.eol, Eol::Lf);
        assert_eq!(app.eol.bytes(), b"\n");
        typed(&mut app, "/eol nonsense");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.eol, Eol::Lf, "bad value keeps the old setting");
        typed(&mut app, "/filter e");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.levels, [false, false, true]);
        typed(&mut app, "/filter all");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.levels, [true; 3]);
    }

    #[test]
    fn level_filter_drops_unwanted_lines() {
        let mut app = app();
        typed(&mut app, "/filter e");
        app.on_key(key(KeyCode::Enter));
        app.on_event(Event::Data(
            b"I (1) boot\r\nW (2) slow\r\nE (3) boom\r\nplain\r\n".to_vec(),
        ));
        assert_eq!(app.scrollback.len(), 1);
        let text: String = app
            .scrollback
            .iter()
            .flat_map(|row| row.cells.iter().map(|cell| cell.ch))
            .collect();
        assert!(text.contains("boom"), "{text}");
    }

    #[test]
    fn counters_track_received_bytes() {
        let mut app = app();
        app.on_event(Event::Data(b"0123456789\r\n".to_vec()));
        assert_eq!(app.rx_bytes, 12);
        typed(&mut app, "ping");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.tx_bytes, 0, "no port: nothing left the process");
    }

    #[test]
    fn panic_with_mismatched_elf_is_flagged() {
        let mut app = app();
        // 用测试进程自身作为可加载的目标文件：硬编码 target/debug/gtui-terms.exe
        // 在 Linux 上不存在，且 clippy/test 之前也未必构建过该文件。
        let exe = std::env::current_exe().unwrap();
        assert!(app.set_elf(exe), "{}", app.status);
        app.on_event(Event::Data(
            b"Guru Meditation Error: Core 0 panic'ed\r\nCore  0 register dump:\r\n\
              PC      : 0x400d1f2a  PS      : 0x00060d30\r\n\
              ELF file SHA256: 9f2c1b7d2a\r\n"
                .to_vec(),
        ));
        let text: String = app
            .scrollback
            .iter()
            .flat_map(|row| row.cells.iter().map(|cell| cell.ch))
            .collect();
        assert!(text.contains("ELF MISMATCH"), "{text}");
    }

    #[test]
    fn hex_command_rejects_bad_tokens() {
        assert_eq!(parse_hex("01 0d 0a").unwrap(), vec![1, 13, 10]);
        assert_eq!(parse_hex("0x41,42").unwrap(), vec![0x41, 0x42]);
        assert!(parse_hex("zz").is_err());
        assert!(parse_hex("123").is_err());
        assert_eq!(
            parse_hex("AA55 0d\n0a").unwrap(),
            vec![0xaa, 0x55, 0x0d, 0x0a]
        );
        assert_eq!(parse_hex("a").unwrap(), vec![0x0a]);
        assert!(parse_hex("AA5").is_err());
        assert!(parse_hex("  ").is_err());
    }

    #[test]
    fn reset_keys_report_demo_mode() {
        let mut app = app();
        app.start_demo();
        app.on_key(key(KeyCode::F(5)));
        assert_eq!(app.status, "demo: reset ignored");
        app.on_key(key(KeyCode::F(6)));
        assert_eq!(app.status, "demo: bootloader ignored");
        typed(&mut app, "/reset");
        app.on_key(key(KeyCode::Enter));
        assert!(app.status.starts_with("no port open") || app.status == "demo: reset ignored");
    }

    #[test]
    fn macro_key_sends_text_in_demo() {
        let macros = vec![crate::config::Macro {
            name: "ping".to_string(),
            text: "ping".to_string(),
            key: KeyCode::Char('p'),
            mods: KeyModifiers::ALT,
        }];
        let mut app = App::new(115200, Vec::new(), None, false, macros, Vec::new());
        app.start_demo();
        app.on_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::ALT));
        assert_eq!(app.status, "macro ping");
        let text: String = app
            .scrollback
            .iter()
            .map(|row| row.cells.iter().map(|cell| cell.ch).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("ping"));
    }

    #[test]
    fn macro_command_persists_and_immediately_updates_keyboard_binding() {
        let dir = std::env::temp_dir().join(format!("gtui-app-macro-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut app = app();
        app.config_path = dir.join("config.toml");
        app.start_demo();
        app.command("/macro ping alt+p hello world");
        assert!(app.status.starts_with("macro ping saved to "));
        assert_eq!(app.macros.len(), 1);
        assert_eq!(app.macros[0].text, "hello world");
        app.on_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::ALT));
        assert_eq!(app.status, "macro ping");
        assert!(app.scrollback.iter().any(|row| {
            row.cells
                .iter()
                .map(|cell| cell.ch)
                .collect::<String>()
                .contains("hello world")
        }));
        app.command("/macro\tping\tF12\tupdated payload");
        assert!(app.status.starts_with("macro ping saved to "));
        assert_eq!(app.macros.len(), 1);
        assert_eq!(app.macros[0].key, KeyCode::F(12));
        app.on_key(key(KeyCode::F(12)));
        assert_eq!(app.status, "macro ping");
        let persisted: crate::config::Config =
            toml::from_str(&std::fs::read_to_string(&app.config_path).unwrap()).unwrap();
        assert_eq!(persisted.macros.len(), 1);
        assert_eq!(persisted.macros[0].text, "updated payload");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_macro_commands_preserve_active_bindings_and_invalid_files() {
        let dir =
            std::env::temp_dir().join(format!("gtui-app-macro-errors-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut app = app();
        app.config_path = dir.join("config.toml");
        app.command("/macro ping f9 original");
        assert!(app.status.starts_with("macro ping saved to "));
        let active = app.macros.clone();
        let original = std::fs::read_to_string(&app.config_path).unwrap();
        app.command("/macro ping f1 rejected");
        assert!(app.status.contains("unsupported key"));
        assert_eq!(app.macros, active);
        assert_eq!(std::fs::read_to_string(&app.config_path).unwrap(), original);
        app.command("/macro ping f10");
        assert!(app.status.starts_with("usage: /macro"));
        assert_eq!(app.macros, active);
        for invalid in ["[broken", "macros = 'wrong type'"] {
            std::fs::write(&app.config_path, invalid).unwrap();
            app.command("/macro ping f10 rejected");
            assert!(app.status.starts_with("macro: "));
            assert_eq!(app.macros, active);
            assert_eq!(std::fs::read_to_string(&app.config_path).unwrap(), invalid);
        }
        app.config_path = dir.join("missing-parent").join("config.toml");
        app.command("/macro ping f10 rejected");
        assert!(app.status.contains("cannot create temporary config"));
        assert_eq!(app.macros, active);
        assert!(!app.config_path.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn repeat_command_sends_count_times_then_stops() {
        let mut app = app();
        app.demo = true;
        typed(&mut app, "/repeat 1 2 AT");
        app.on_key(key(KeyCode::Enter));
        assert!(app.repeat.is_some());
        app.tick();
        assert_eq!(app.scrollback.len(), 1);
        std::thread::sleep(std::time::Duration::from_millis(3));
        app.tick();
        assert_eq!(app.scrollback.len(), 2);
        assert!(app.repeat.is_none());
        assert_eq!(app.status, "repeat finished");
    }

    #[test]
    fn stop_command_cancels_repeat() {
        let mut app = app();
        typed(&mut app, "/repeat 1000 5 z");
        app.on_key(key(KeyCode::Enter));
        assert!(app.repeat.is_some());
        typed(&mut app, "/stop");
        app.on_key(key(KeyCode::Enter));
        assert!(app.repeat.is_none());
        assert_eq!(app.status, "repeat stopped");
    }

    #[test]
    fn raw_buffer_is_capped_but_total_keeps_growing() {
        let mut app = app();
        let chunk = vec![b'x'; 4096];
        let rounds = RAW_CAP / chunk.len() + 2;
        for _ in 0..rounds {
            app.on_event(Event::Data(chunk.clone()));
        }
        assert_eq!(app.raw.len(), RAW_CAP);
        assert_eq!(app.raw_total, (rounds * chunk.len()) as u64);
    }

    #[test]
    fn ctrl_l_clears_scrollback() {
        let mut app = app();
        app.on_event(Event::Data(b"hello\r\n".to_vec()));
        assert_eq!(app.scrollback.len(), 1);
        app.on_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL));
        assert_eq!(app.scrollback.len(), 0);
    }

    fn ports(names: &[&str]) -> Vec<PortInfo> {
        names
            .iter()
            .map(|name| PortInfo {
                name: (*name).to_string(),
                label: "USB serial".to_string(),
                pid: None,
            })
            .collect()
    }

    #[test]
    fn hotplug_preserves_port_identity_and_clamps_removal() {
        let mut app = App::new(
            115200,
            ports(&["COM3", "COM7"]),
            None,
            false,
            Vec::new(),
            Vec::new(),
        );
        app.selected = 1;
        app.replace_ports(ports(&["COM7", "COM1", "COM3"]));
        assert_eq!(app.ports[app.selected].name, "COM7");
        assert_eq!(app.selected, 0);
        app.selected = 2;
        app.replace_ports(ports(&["COM7"]));
        assert_eq!(app.selected, 0);
        app.replace_ports(Vec::new());
        assert_eq!(app.selected, 0);
        assert!(app.mode == Mode::Picker && !app.quit);
        app.replace_ports(ports(&["COM9"]));
        assert_eq!(app.ports[app.selected].name, "COM9");
    }

    #[test]
    fn empty_picker_waits_and_cancelled_settings_never_opens_port() {
        let mut app = App::new(115200, Vec::new(), None, false, Vec::new(), Vec::new());
        app.on_key(key(KeyCode::Enter));
        assert!(app.mode == Mode::Picker && app.settings_dialog.is_none());
        app.replace_ports(ports(&["COM7"]));
        app.on_key(key(KeyCode::Enter));
        assert!(app.settings_dialog.is_some());
        assert!(app.worker.is_none());
        app.replace_ports(ports(&["COM9", "COM7"]));
        assert_eq!(app.pending_port.as_ref().unwrap().0, "COM7");
        app.on_key(key(KeyCode::Esc));
        assert!(app.settings_dialog.is_none() && app.pending_port.is_none());
        assert!(app.mode == Mode::Picker && app.worker.is_none());
    }

    #[test]
    fn help_settings_actions_search_and_output_consume_raw_and_macro_keys() {
        let mut app = app();
        let (writes, commands) = channel();
        app.writes = Some(writes);
        app.raw_mode = true;
        app.macros.push(crate::config::Macro {
            name: "ping".to_string(),
            text: "AT".to_string(),
            key: KeyCode::Char('p'),
            mods: KeyModifiers::ALT,
        });
        for opener in [KeyCode::F(1), KeyCode::F(3), KeyCode::F(4)] {
            app.on_key(key(opener));
            typed(&mut app, "secret");
            app.on_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::ALT));
            app.on_key(key(KeyCode::Esc));
        }
        app.on_key(key(KeyCode::F(7)));
        typed(&mut app, "needle");
        app.on_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::ALT));
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.search_query.as_deref(), Some("needle"));
        app.focus = Focus::Output;
        typed(&mut app, "secret");
        app.on_key(key(KeyCode::Enter));
        app.on_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::ALT));
        app.focus = Focus::Toolbar;
        app.toolbar_selected = 4;
        app.on_key(key(KeyCode::Enter));
        assert!(app.help);
        assert_eq!(app.tx_bytes, 0);
        assert!(commands.try_recv().is_err());
        assert!(app.input.is_empty());
        app.on_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL));
        assert!(app.quit);
    }

    #[test]
    fn mouse_focus_prevents_sending_until_input_is_clicked() {
        let mut app = app();
        app.demo = true;
        app.raw_mode = true;
        app.hits_ui = vec![
            (Rect::new(0, 1, 20, 4), Hit::Output),
            (Rect::new(0, 6, 20, 1), Hit::Input),
        ];
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 1,
            row: 2,
            modifiers: KeyModifiers::NONE,
        });
        typed(&mut app, "secret");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.tx_bytes, 0);
        assert_eq!(app.focus, Focus::Output);
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 1,
            row: 6,
            modifiers: KeyModifiers::NONE,
        });
        typed(&mut app, "x");
        assert_eq!(app.tx_bytes, 1);
    }

    #[test]
    fn demo_settings_apply_never_acquires_a_port_and_shutdown_drains_old_events() {
        let mut app = app();
        app.demo = true;
        app.label = "demo".to_string();
        app.open_settings(false);
        app.settings_action(crate::settings::DialogAction::Apply);
        assert!(app.demo && app.worker.is_none() && app.writes.is_none());
        app.tx
            .send(Event::Connected("old port".to_string()))
            .unwrap();
        app.tx.send(Event::Data(b"old bytes".to_vec())).unwrap();
        app.tx
            .send(Event::HookDone {
                name: "build".to_string(),
                ok: true,
                output: String::new(),
            })
            .unwrap();
        app.shutdown_worker();
        assert!(matches!(app.rx.try_recv(), Ok(Event::HookDone { .. })));
        assert!(app.rx.try_recv().is_err());
    }
}
