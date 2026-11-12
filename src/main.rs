mod app;
mod config;
mod demo;
mod esp;
mod esptool;
mod hooks;
mod logfile;
mod serial;
mod session;
mod settings;
mod term;
mod ui;

use std::io::{self, BufRead, Stdout, Write};
use std::panic;
use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event as TerminalEvent, KeyEventKind, poll, read,
};
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::crossterm::{cursor, execute};

use crate::app::{App, Eol};
use crate::logfile::LogFile;

#[derive(Parser, Debug)]
#[command(
    name = "gtui-terms",
    version,
    about = "TUI serial terminal for ESP32 debugging"
)]
struct Args {
    /// Serial port (COM4, /dev/ttyUSB0, ...). Omit to pick interactively.
    port: Option<String>,
    /// Baud rate (default 115200, or `baud` from config)
    #[arg(short, long)]
    baud: Option<u32>,
    /// Config file (default: ./.gtui-terms.toml, then the global one)
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
    /// List serial ports and exit
    #[arg(long)]
    list: bool,
    /// Run against a simulated device, no hardware needed
    #[arg(long)]
    demo: bool,
    /// With --demo, emit a fake panic dump so the decoder can be exercised
    #[arg(long)]
    panic: bool,
    /// Write raw device bytes to this file
    #[arg(long, value_name = "PATH")]
    log: Option<PathBuf>,
    /// Prefix scrollback lines with arrival time
    #[arg(long)]
    timestamps: bool,
    /// ELF file whose symbols are used to decode panic backtraces
    #[arg(long, value_name = "PATH")]
    elf: Option<PathBuf>,
    /// Data bits: 5, 6, 7 or 8 (default 8)
    #[arg(long, value_name = "BITS")]
    data_bits: Option<u64>,
    /// Parity: none, odd or even (default none)
    #[arg(long, value_name = "PARITY")]
    parity: Option<String>,
    /// Stop bits: 1 or 2 (default 1)
    #[arg(long, value_name = "BITS")]
    stop_bits: Option<u64>,
    /// Flow control: none, hardware or software (default none)
    #[arg(long, value_name = "FLOW")]
    flow: Option<String>,
    /// Do not pulse DTR/RTS to reset the chip when opening the port
    #[arg(long)]
    no_reset: bool,
    /// No TUI: copy stdin to the port and the port to stdout, then exit on EOF
    #[arg(long, conflicts_with_all = ["headless", "session_log"])]
    pipe: bool,
    /// No alternate screen or mouse: accept local commands on stdin; stream escaped RX/TX/tool records
    #[arg(long, conflicts_with = "pipe")]
    headless: bool,
    /// Save complete session capture on normal exit (never overwrite an existing file)
    #[arg(long, value_name = "PATH", conflicts_with = "pipe")]
    session_log: Option<PathBuf>,
}

fn main() -> io::Result<()> {
    let args = Args::parse();

    if args.list {
        let ports = serial::list_ports();
        if ports.is_empty() {
            println!("no serial ports found");
        }
        for port in ports {
            println!("{}\t{}", port.name, port.label);
        }
        return Ok(());
    }

    let loaded = config::load(args.config.as_deref()).unwrap_or_else(|err| {
        eprintln!("config error: {err}");
        std::process::exit(2);
    });
    for warning in &loaded.warnings {
        eprintln!("warning: {warning}");
    }
    let cfg = loaded.config;
    let baud = args.baud.or(cfg.baud).unwrap_or(115200);
    let port = args.port.or(cfg.port);
    let log_path = args.log.or(cfg.log);
    let timestamps = args.timestamps || cfg.timestamps.unwrap_or(false);
    let elf_path = args.elf.or(cfg.elf);

    let log = match &log_path {
        Some(path) => Some(LogFile::create(path)?),
        None => None,
    };

    let ports = serial::list_ports();

    let data_bits = args.data_bits.or(cfg.data_bits);
    let stop_bits = args.stop_bits.or(cfg.stop_bits);
    let parity = args.parity.clone().or(cfg.parity.clone());
    let flow = args.flow.clone().or(cfg.flow.clone());
    let mut settings = serial::Settings::new(baud);
    let applied = (|| -> Result<(), String> {
        if let Some(bits) = data_bits {
            settings.data_bits = serial::parse_data_bits(bits)?;
        }
        if let Some(bits) = stop_bits {
            settings.stop_bits = serial::parse_stop_bits(bits)?;
        }
        if let Some(text) = &parity {
            settings.parity = serial::parse_parity(text)?;
        }
        if let Some(text) = &flow {
            settings.flow = serial::parse_flow(text)?;
        }
        Ok(())
    })();
    if let Err(err) = applied {
        eprintln!("{err}");
        std::process::exit(2);
    }

    if args.pipe {
        let Some(name) = port.as_deref() else {
            eprintln!("--pipe needs a port (e.g. gtui-terms COM4 --pipe)");
            std::process::exit(2);
        };
        if let Err(err) = serial::pipe(name, settings) {
            eprintln!("{err}");
            std::process::exit(1);
        }
        return Ok(());
    }

    let mut app = App::new(baud, ports, log, timestamps, loaded.macros, loaded.hooks);
    app.config_path = args
        .config
        .unwrap_or_else(|| PathBuf::from(config::LOCAL_FILE));
    app.settings = settings;
    app.auto_reset = !(args.no_reset || cfg.no_reset.unwrap_or(false));
    app.load_history();
    if let Some(path) = elf_path.or_else(esp::elf::discover) {
        app.set_elf(path);
    }
    if let Some(text) = &cfg.eol {
        match Eol::parse(text) {
            Some(eol) => app.eol = eol,
            None => eprintln!("warning: unknown eol {text:?}, keeping crlf"),
        }
    }

    if args.headless && !args.demo && port.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--headless needs a port or --demo",
        ));
    }
    if args.headless {
        app.record_output = Some(Box::new(io::BufWriter::new(io::stdout())));
    }
    if args.demo {
        if args.panic {
            app.start_demo_with(true);
        } else {
            app.start_demo();
        }
    } else if let Some(name) = &port {
        app.connect(name);
    }

    let result = if args.headless {
        headless_loop(&mut app)
    } else {
        install_panic_hook();
        let mut terminal = init_terminal()?;
        let result = event_loop(&mut terminal, &mut app);
        result.and(restore_terminal())
    };
    app.finish();
    let export = match args.session_log {
        Some(path) => app.save_session(&path).map_err(io::Error::other),
        None => Ok(()),
    };
    result.and(export)
}

fn headless_loop(app: &mut App) -> io::Result<()> {
    let (input_tx, input_rx) = std::sync::mpsc::sync_channel(16);
    std::thread::Builder::new()
        .name("agent-stdin".to_string())
        .spawn(move || {
            for line in io::stdin().lock().lines() {
                let failed = line.is_err();
                if input_tx.send(line).is_err() || failed {
                    return;
                }
            }
        })?;
    let mut stdout = io::stdout();
    let mut eof = false;
    let mut status = String::new();
    let mut preview = String::new();
    while !app.quit {
        if eof && let Ok(event) = app.rx.recv_timeout(Duration::from_millis(16)) {
            app.on_event(event);
        }
        while let Ok(event) = app.rx.try_recv() {
            app.on_event(event);
        }
        match input_rx.recv_timeout(Duration::from_millis(16)) {
            Ok(Ok(line)) => {
                session::write_record(&mut stdout, app::now_ms(), "COMMAND", line.as_bytes())?;
                if line.trim() == "/help" {
                    session::write_record(&mut stdout, app::now_ms(), "HELP", b"One stdin line sends a payload with configured EOL; // escapes leading slash. Local commands: /ports, /settings, /hex <bytes>, /save [path], /esp chip-id|flash-id|read-mac|erase-flash|write-flash <offset> <file> ..., /confirm, /cancel, /reset, /boot, /seq <steps>, /eol crlf|lf|cr|none, /quit. Destructive ESP commands emit CONFIRM and execute only after /confirm. EOF waits for pending serial commands/tools, then exits.")?;
                } else if line.trim() == "/ports" {
                    app.replace_ports(serial::list_ports());
                    app.status = if app.ports.is_empty() {
                        "ports: none found".to_string()
                    } else {
                        format!(
                            "ports: {}",
                            app.ports
                                .iter()
                                .map(|port| port.name.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    };
                    status.clear();
                } else if line.trim() == "/settings" {
                    app.status = format!("settings: {:?}, eol={}", app.settings, app.eol.label());
                    status.clear();
                } else {
                    app.execute(&line);
                }
            }
            Ok(Err(err)) => return Err(err),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                eof = true;
                app.repeat = None;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
        app.tick();
        if app.status != status {
            session::write_record(&mut stdout, app::now_ms(), "STATE", app.status.as_bytes())?;
            status.clone_from(&app.status);
        }
        let current = app
            .pending_tool
            .as_ref()
            .map(|tool| tool.summary.as_str())
            .unwrap_or_default();
        if current != preview {
            if !current.is_empty() {
                session::write_record(&mut stdout, app::now_ms(), "CONFIRM", current.as_bytes())?;
            }
            preview.clear();
            preview.push_str(current);
        }
        stdout.flush()?;
        if let Some(err) = app.record_output_error.take() {
            return Err(err);
        }
        if eof
            && app.active_hook().is_none()
            && app.pending_writes == 0
            && app.pending_controls == 0
        {
            break;
        }
    }
    Ok(())
}

fn init_terminal() -> io::Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    Terminal::new(CrosstermBackend::new(stdout))
}

fn restore_terminal() -> io::Result<()> {
    let raw_result = disable_raw_mode();
    let screen_result = execute!(
        io::stdout(),
        DisableMouseCapture,
        LeaveAlternateScreen,
        cursor::Show
    );
    raw_result.and(screen_result)
}

fn install_panic_hook() {
    let default_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let _ = restore_terminal();
        default_hook(info);
    }));
}

fn event_loop(terminal: &mut Terminal<CrosstermBackend<Stdout>>, app: &mut App) -> io::Result<()> {
    while !app.quit {
        if poll(Duration::from_millis(16))? {
            match read()? {
                TerminalEvent::Key(key) if key.kind == KeyEventKind::Press => app.on_key(key),
                TerminalEvent::Mouse(event) => app.on_mouse(event),
                _ => app.dirty = true,
            }
        }
        while let Ok(event) = app.rx.try_recv() {
            app.on_event(event);
        }
        app.tick();
        if app.dirty {
            terminal.draw(|frame| ui::draw(frame, app))?;
            app.dirty = false;
        }
    }
    Ok(())
}
