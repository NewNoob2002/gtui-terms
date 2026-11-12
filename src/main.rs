mod app;
mod config;
mod demo;
mod esp;
mod hooks;
mod logfile;
mod serial;
mod term;
mod ui;

use std::io::{self, Stdout};
use std::panic;
use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{Event as TerminalEvent, KeyEventKind, poll, read};
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
    #[arg(long)]
    pipe: bool,
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

    if args.demo {
        if args.panic {
            app.start_demo_with(true);
        } else {
            app.start_demo();
        }
    } else if let Some(name) = &port {
        app.connect(name);
    } else if !app.ports.is_empty() {
        // picker mode
    } else {
        eprintln!("no serial ports found; use --demo or pass a port name");
        std::process::exit(1);
    }

    install_panic_hook();
    let mut terminal = init_terminal()?;
    let result = event_loop(&mut terminal, &mut app);
    restore_terminal()?;
    result
}

fn init_terminal() -> io::Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    Terminal::new(CrosstermBackend::new(stdout))
}

fn restore_terminal() -> io::Result<()> {
    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen, cursor::Show)
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
