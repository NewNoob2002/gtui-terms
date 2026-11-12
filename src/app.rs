use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;
use std::time::{SystemTime, UNIX_EPOCH};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::text::Line;

use crate::esp::{elf, panic};
use crate::logfile::LogFile;
use crate::serial::reset::Kind as ResetKind;
use crate::serial::{self, Event, PortInfo};
use crate::term::assembler::Assembler;
use crate::term::scrollback::{Cell, Row, Scrollback, match_mask};

pub const SCROLLBACK_ROWS: usize = 50_000;
pub const SCROLLBACK_BYTES: usize = 8 * 1024 * 1024;
const MAX_INPUT: usize = 4096;
const HISTORY_MAX: usize = 200;
pub const RAW_CAP: usize = 1024 * 1024;

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
    pub input: Vec<char>,
    pub cursor: usize,
    pub quit: bool,
    pub dirty: bool,
    pub show_timestamps: bool,
    pub demo: bool,
    pub reconnecting: bool,
    pub version: u64,
    pub log_path: Option<String>,
    pub cache: Vec<Line<'static>>,
    pub cache_width: u16,
    pub cache_version: u64,
    pub starts: Vec<usize>,
    pub total_rows: usize,
    pub scroll_back: usize,
    pub view_height: u16,
    pub search_query: Option<String>,
    pub search_input: Option<Vec<char>>,
    pub hits: Vec<usize>,
    pub search_re: Option<regex::Regex>,
    pub repeat: Option<Repeat>,
    pub macros: Vec<crate::config::Macro>,
    pub help: bool,
    pub raw_mode: bool,
    pub eol: Eol,
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
    elf_path: Option<std::path::PathBuf>,
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
        Self {
            mode: if ports.is_empty() {
                Mode::Terminal
            } else {
                Mode::Picker
            },
            view: View::Text,
            raw: VecDeque::new(),
            raw_total: 0,
            ports,
            selected: 0,
            settings: serial::Settings::new(baud),
            auto_reset: true,
            label: String::new(),
            status: String::new(),
            scrollback: Scrollback::new(SCROLLBACK_ROWS, SCROLLBACK_BYTES),
            assembler: Assembler::new(),
            input: Vec::new(),
            cursor: 0,
            quit: false,
            dirty: true,
            show_timestamps,
            demo: false,
            reconnecting: false,
            version: 1,
            log_path: log.as_ref().map(|l| l.path().display().to_string()),
            cache: Vec::new(),
            cache_width: 0,
            cache_version: 0,
            starts: Vec::new(),
            total_rows: 0,
            scroll_back: 0,
            view_height: 1,
            search_query: None,
            search_input: None,
            hits: Vec::new(),
            search_re: None,
            repeat: None,
            macros,
            help: false,
            raw_mode: false,
            eol: Eol::CrLf,
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
    pub fn set_elf(&mut self, path: std::path::PathBuf) {
        match elf::Symbols::load(&path) {
            Ok(symbols) => {
                self.elf_sha = elf_sha256(&path);
                self.status = match &self.elf_sha {
                    Some(sha) => format!("ELF {} ({})", path.display(), &sha[..sha.len().min(12)]),
                    None => format!("ELF {}", path.display()),
                };
                self.symbols = Some(symbols);
                self.elf_path = Some(path);
            }
            Err(err) => self.status = err,
        }
        self.dirty = true;
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
        self.shutdown_worker();
        self.stop = Arc::new(AtomicBool::new(false));
        let (writes, write_rx) = channel();
        self.writes = Some(writes);
        self.reset_kind = kind;
        self.mode = Mode::Terminal;
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
        let chosen = self
            .ports
            .get(self.selected)
            .map(|port| (port.name.clone(), ResetKind::from_pid(port.pid)));
        if let Some((name, kind)) = chosen {
            self.connect_with(&name, kind);
        }
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
        let at = now_ms();
        self.push_text(
            &format!("[hook {name}] exit {}", if ok { "0" } else { "non-zero" }),
            1,
            at,
        );
        for line in output.lines().take(40) {
            self.push_text(&format!("[hook {name}] {line}"), 0, at);
        }
        self.hook_running = None;
        self.status = format!("hook {} finished (exit {})", name, if ok { 0 } else { 1 });
        self.hook_last.insert(name, std::time::Instant::now());
        if let Some((port, kind)) = self.reopen_after_hook.take() {
            self.push_text(&format!("[hook] reopening {port}"), 0, at);
            self.connect_with(&port, kind);
        }
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
        self.push_text(&format!("[panic] {head}"), 1, at);
        if let Some(sha) = &report.sha256 {
            self.push_text(&format!("[panic] ELF SHA256 {sha}"), 0, at);
            if let Some(elf) = &self.elf_sha {
                let dump = sha.trim().to_ascii_lowercase();
                if !elf.starts_with(&dump) {
                    self.push_text(
                        &format!("[panic] ELF MISMATCH: dump {dump}, loaded {elf}"),
                        1,
                        at,
                    );
                }
            }
        }
        if report.addresses.is_empty() {
            self.push_text("[panic] no addresses recovered", 0, at);
        }
        for &address in &report.addresses {
            let text = match &self.symbols {
                Some(symbols) => match symbols.resolve(address).first() {
                    Some(frame) => format!("[panic] 0x{address:08x} → {}", frame.render()),
                    None => format!("[panic] 0x{address:08x} → ?? (no symbol)"),
                },
                None => {
                    format!("[panic] 0x{address:08x} (no ELF loaded: pass --elf PATH)")
                }
            };
            self.push_text(&text, 0, at);
        }
        self.status = format!("panic decoded ({} addresses)", report.addresses.len());
        self.version += 1;
    }

    fn save_coredump(&mut self, text: &str) {
        let name = format!(
            "coredump-{}.txt",
            chrono::Local::now().format("%Y%m%d-%H%M%S")
        );
        match std::fs::write(&name, text) {
            Ok(()) => self.status = format!("coredump saved to {name}"),
            Err(err) => self.status = format!("coredump write failed: {err}"),
        }
        self.version += 1;
    }

    fn push_text(&mut self, text: &str, style: u8, at: u64) {
        let cells = text.chars().map(|ch| Cell::new(ch, style)).collect();
        self.scrollback.push(Row { cells, at });
    }

    pub fn on_event(&mut self, event: Event) {
        match event {
            Event::Data(bytes) => {
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
                let at = now_ms();
                let rows: Vec<Row> = self
                    .assembler
                    .take()
                    .into_iter()
                    .map(|cells| Row { cells, at })
                    .collect();
                self.check_triggers(&rows);
                for row in rows {
                    if self.levels[level_of(&row.cells)] {
                        self.scrollback.push(row);
                    }
                }
                self.scan_esp(&bytes);
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
            Event::Notice(text) => {
                self.status = text;
                self.dirty = true;
            }
            Event::HookDone { name, ok, output } => {
                self.on_hook_done(name, ok, output);
            }
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('q') if ctrl => {
                self.quit = true;
                return;
            }
            _ => {}
        }
        if self.raw_mode {
            if key.code == KeyCode::F(2) || (ctrl && key.code == KeyCode::Char(']')) {
                self.raw_mode = false;
                self.status = "line mode".to_string();
                self.dirty = true;
                return;
            }
            self.raw_send(key);
            return;
        }
        match key.code {
            KeyCode::F(2) => {
                self.raw_mode = true;
                self.status = "raw mode (Ctrl+] to leave)".to_string();
                self.dirty = true;
                return;
            }
            KeyCode::Char('l') if ctrl => {
                self.clear();
                return;
            }
            KeyCode::Char('t') if ctrl => {
                self.show_timestamps = !self.show_timestamps;
                self.version += 1;
                self.dirty = true;
                return;
            }
            KeyCode::F(1) => {
                self.help = !self.help;
                self.dirty = true;
                return;
            }
            KeyCode::F(5) => {
                self.queue_reset(false);
                return;
            }
            KeyCode::F(6) => {
                self.queue_reset(true);
                return;
            }
            KeyCode::F(7) => {
                self.open_search();
                return;
            }
            KeyCode::F(8) => {
                self.view = match self.view {
                    View::Text => View::Hex,
                    View::Hex => View::Text,
                };
                self.scroll_back = 0;
                self.version += 1;
                self.dirty = true;
                return;
            }
            KeyCode::Char('n') if ctrl => {
                self.step_hit(true);
                return;
            }
            KeyCode::Char('p') if ctrl => {
                self.step_hit(false);
                return;
            }
            _ => {}
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
            self.dirty = true;
            return;
        }
        if self.search_input.is_some() {
            self.search_key(key);
            return;
        }
        match self.mode {
            Mode::Picker => self.picker_key(key),
            Mode::Terminal => self.terminal_key(key),
        }
    }

    fn open_search(&mut self) {
        if self.mode != Mode::Terminal || self.view == View::Hex {
            return;
        }
        let seed = self.search_query.clone().unwrap_or_default();
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
            .map(str::to_string)
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
            .map(|line| format!("{line}\n"))
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
        self.dirty = true;
    }

    fn picker_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.quit = true,
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
        match key.code {
            KeyCode::PageUp => self.scroll_page(true),
            KeyCode::PageDown => self.scroll_page(false),
            KeyCode::Home if key.modifiers.contains(KeyModifiers::CONTROL) => self.scroll_home(),
            KeyCode::End if key.modifiers.contains(KeyModifiers::CONTROL) => self.scroll_end(),
            KeyCode::Esc if self.help => {
                self.help = false;
                self.dirty = true;
            }
            KeyCode::Esc => self.quit = true,
            KeyCode::Enter => self.submit(),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete(),
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.input.len()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.input.len(),
            KeyCode::Up => self.history_prev(),
            KeyCode::Down => self.history_next(),
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    && self.input.len() < MAX_INPUT =>
            {
                self.input.insert(self.cursor, ch);
                self.cursor += 1;
                self.history_pos = None;
                self.dirty = true;
            }
            _ => {}
        }
    }

    fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.input.remove(self.cursor);
            self.dirty = true;
        }
    }

    fn delete(&mut self) {
        if self.cursor < self.input.len() {
            self.input.remove(self.cursor);
            self.dirty = true;
        }
    }

    fn clear(&mut self) {
        self.scrollback.clear();
        self.assembler.reset();
        self.hits.clear();
        self.search_re = None;
        self.scroll_back = 0;
        self.version += 1;
        self.dirty = true;
    }

    fn submit(&mut self) {
        let text: String = self.input.iter().collect();
        self.input.clear();
        self.cursor = 0;
        self.history_pos = None;
        if !text.is_empty() && self.history.last() != Some(&text) {
            self.history.push(text.clone());
            let excess = self.history.len().saturating_sub(HISTORY_MAX);
            self.history.drain(..excess);
            self.save_history();
        }
        if text.starts_with("//") {
            self.send_payload(&text[1..]);
        } else if text.starts_with('/') {
            self.command(&text);
        } else if !text.is_empty() {
            self.send_payload(&text);
        }
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
                    self.tx_bytes += len as u64;
                }
            }
            None if self.demo => {
                self.status = format!("{len} byte(s) sent to demo device");
                self.tx_bytes += len as u64;
            }
            None => self.status = "no port open".to_string(),
        }
        self.dirty = true;
    }

    fn send_payload(&mut self, text: &str) {
        let mut bytes = text.as_bytes().to_vec();
        bytes.extend_from_slice(self.eol.bytes());
        self.send_bytes(bytes);
        if self.demo {
            let at = now_ms();
            let cells = text
                .chars()
                .map(|ch| crate::term::scrollback::Cell::new(ch, 0))
                .collect();
            self.scrollback.push(Row { cells, at });
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
                }
            }
            None if self.demo => self.status = "demo: sequence ignored".to_string(),
            None => self.status = "no port open".to_string(),
        }
        self.dirty = true;
    }

    fn command(&mut self, text: &str) {
        let rest = text.trim_start_matches('/').trim();
        if rest == "reset" {
            self.queue_reset(false);
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
                Ok(steps) => {
                    self.queue_sequence(steps);
                    self.status = format!("custom sequence {}", arg.trim());
                }
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
            self.status = format!("unknown command {text} (try /repeat <ms> <count> <text>)");
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
        self.input = self.history[pos].chars().collect();
        self.cursor = self.input.len();
        self.dirty = true;
    }

    fn history_next(&mut self) {
        let Some(pos) = self.history_pos else { return };
        if pos + 1 < self.history.len() {
            self.history_pos = Some(pos + 1);
            self.input = self.history[pos + 1].chars().collect();
        } else {
            self.history_pos = None;
            self.input.clear();
        }
        self.cursor = self.input.len();
        self.dirty = true;
    }

    fn shutdown_worker(&mut self) {
        if let Some(worker) = self.worker.take() {
            self.stop.store(true, Ordering::Relaxed);
            let _ = worker.join();
        }
        self.writes = None;
    }

    #[cfg(test)]
    pub fn input_text(&self) -> String {
        self.input.iter().collect()
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

fn now_ms() -> u64 {
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

fn parse_hex(text: &str) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    for token in text.split([' ', ',']) {
        if token.is_empty() {
            continue;
        }
        let clean = token.trim_start_matches("0x").trim_start_matches("0X");
        if clean.len() > 2 || !clean.chars().all(|ch| ch.is_ascii_hexdigit()) {
            return Err(format!("{token}: use two hex digits per byte"));
        }
        bytes.push(u8::from_str_radix(clean, 16).map_err(|_| format!("{token}: bad byte"))?);
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
        App::new(115200, Vec::new(), None, false, Vec::new(), Vec::new())
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
            app.push_text(line, 0, 0);
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
        assert_eq!(fresh.input.iter().collect::<String>(), "/filter e");
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
        app.set_elf(std::path::PathBuf::from("target/debug/gtui-terms.exe"));
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
}
