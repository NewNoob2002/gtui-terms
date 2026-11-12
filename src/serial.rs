use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serialport::SerialPortInfo;

const READ_TIMEOUT: Duration = Duration::from_millis(50);
const RETRY_DELAY: Duration = Duration::from_millis(500);
const READ_BUF: usize = 8192;

pub mod reset;

pub struct PortInfo {
    pub name: String,
    pub label: String,
    pub pid: Option<u16>,
}

/// Work for the thread that owns the port.
pub enum Command {
    Write(Vec<u8>),
    Reset {
        into_bootloader: bool,
        kind: reset::Kind,
    },
    Sequence(Vec<reset::Step>),
}

#[derive(Debug)]
pub enum Event {
    Data(Vec<u8>),
    Connected(String),
    Disconnected(String),
    Reconnecting,
    Notice(String),
    HookDone {
        name: String,
        ok: bool,
        output: String,
    },
}

pub fn list_ports() -> Vec<PortInfo> {
    serialport::available_ports()
        .unwrap_or_default()
        .into_iter()
        .map(|info| PortInfo {
            name: info.port_name.clone(),
            label: describe(&info),
            pid: usb_pid(&info),
        })
        .collect()
}

fn usb_pid(info: &SerialPortInfo) -> Option<u16> {
    match &info.port_type {
        serialport::SerialPortType::UsbPort(usb) => Some(usb.pid),
        _ => None,
    }
}

fn describe(info: &SerialPortInfo) -> String {
    match &info.port_type {
        serialport::SerialPortType::UsbPort(usb) => {
            let mut label = usb
                .product
                .clone()
                .unwrap_or_else(|| "USB serial".to_string());
            label.push_str(&format!(" [{:04x}:{:04x}]", usb.vid, usb.pid));
            if let Some(serial) = &usb.serial_number {
                label.push_str(&format!(" sn:{serial}"));
            }
            label
        }
        serialport::SerialPortType::PciPort => "PCI serial".to_string(),
        serialport::SerialPortType::BluetoothPort => "Bluetooth serial".to_string(),
        serialport::SerialPortType::Unknown => "serial".to_string(),
    }
}

/// Non-TUI pass-through: stdin to the port, port to stdout. For scripts and CI.
pub fn parse_data_bits(value: u64) -> Result<serialport::DataBits, String> {
    Ok(match value {
        5 => serialport::DataBits::Five,
        6 => serialport::DataBits::Six,
        7 => serialport::DataBits::Seven,
        8 => serialport::DataBits::Eight,
        other => return Err(format!("data bits must be 5-8, got {other}")),
    })
}

pub fn parse_stop_bits(value: u64) -> Result<serialport::StopBits, String> {
    Ok(match value {
        1 => serialport::StopBits::One,
        2 => serialport::StopBits::Two,
        other => return Err(format!("stop bits must be 1 or 2, got {other}")),
    })
}

pub fn parse_parity(value: &str) -> Result<serialport::Parity, String> {
    Ok(match value.to_ascii_lowercase().as_str() {
        "none" | "n" => serialport::Parity::None,
        "odd" | "o" => serialport::Parity::Odd,
        "even" | "e" => serialport::Parity::Even,
        other => return Err(format!("parity must be none, odd or even, got {other:?}")),
    })
}

pub fn parse_flow(value: &str) -> Result<serialport::FlowControl, String> {
    Ok(match value.to_ascii_lowercase().as_str() {
        "none" | "off" => serialport::FlowControl::None,
        "hardware" | "rtscts" => serialport::FlowControl::Hardware,
        "software" | "xonxoff" => serialport::FlowControl::Software,
        other => {
            return Err(format!(
                "flow control must be none, hardware or software, got {other:?}"
            ));
        }
    })
}

pub fn pipe(path: &str, settings: Settings) -> Result<(), String> {
    let port = serialport::new(path, settings.baud)
        .data_bits(settings.data_bits)
        .parity(settings.parity)
        .stop_bits(settings.stop_bits)
        .flow_control(settings.flow)
        .timeout(Duration::from_millis(50))
        .open()
        .map_err(|err| format!("cannot open {path}: {err}"))?;
    let mut reader = port.try_clone().map_err(|err| err.to_string())?;
    let mut writer = port;
    thread::spawn(move || {
        use std::io::Read;
        let mut out = std::io::stdout().lock();
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => continue,
                Ok(n) => {
                    if out.write_all(&buf[..n]).is_err() || out.flush().is_err() {
                        break;
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::TimedOut => continue,
                Err(_) => break,
            }
        }
        std::process::exit(0);
    });
    let mut stdin = std::io::stdin().lock();
    let mut buf = [0u8; 4096];
    loop {
        use std::io::Read;
        match stdin.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if writer.write_all(&buf[..n]).is_err() {
                    break;
                }
                let _ = writer.flush();
            }
            Err(_) => break,
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
pub struct Settings {
    pub baud: u32,
    pub data_bits: serialport::DataBits,
    pub parity: serialport::Parity,
    pub stop_bits: serialport::StopBits,
    pub flow: serialport::FlowControl,
}

impl Settings {
    pub fn new(baud: u32) -> Self {
        Self {
            baud,
            ..Self::default()
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            baud: 115200,
            data_bits: serialport::DataBits::Eight,
            parity: serialport::Parity::None,
            stop_bits: serialport::StopBits::One,
            flow: serialport::FlowControl::None,
        }
    }
}

/// Owns the port: opens it, streams reads, applies writes, and reopens after a
/// disconnect until `stop` is set.
pub fn spawn(
    path: String,
    settings: Settings,
    tx: Sender<Event>,
    writes: Receiver<Command>,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        loop {
            match serialport::new(&path, settings.baud)
                .data_bits(settings.data_bits)
                .parity(settings.parity)
                .stop_bits(settings.stop_bits)
                .flow_control(settings.flow)
                .timeout(READ_TIMEOUT)
                .open()
            {
                Ok(mut port) => {
                    if tx.send(Event::Connected(path.clone())).is_err() {
                        return;
                    }
                    let mut buf = [0u8; READ_BUF];
                    let mut alive = true;
                    while alive {
                        while let Ok(command) = writes.try_recv() {
                            match command {
                                Command::Write(data) => {
                                    let _ = port.write_all(&data).and_then(|()| port.flush());
                                }
                                Command::Reset {
                                    into_bootloader,
                                    kind,
                                } => {
                                    let outcome = reset::run(port.as_mut(), kind, into_bootloader);
                                    let text = match outcome {
                                        Ok(()) if into_bootloader => {
                                            "entered bootloader".to_string()
                                        }
                                        Ok(()) => "chip reset".to_string(),
                                        Err(err) => format!("reset failed: {err}"),
                                    };
                                    if tx.send(Event::Notice(text)).is_err() {
                                        return;
                                    }
                                }
                                Command::Sequence(steps) => {
                                    let text = match reset::run_steps(port.as_mut(), &steps) {
                                        Ok(()) => "custom sequence done".to_string(),
                                        Err(err) => format!("sequence failed: {err}"),
                                    };
                                    if tx.send(Event::Notice(text)).is_err() {
                                        return;
                                    }
                                }
                            }
                        }
                        match port.read(&mut buf) {
                            Ok(0) => {}
                            Ok(n) => {
                                if tx.send(Event::Data(buf[..n].to_vec())).is_err() {
                                    return;
                                }
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
                            Err(e) => {
                                let _ = tx.send(Event::Disconnected(e.to_string()));
                                alive = false;
                            }
                        }
                        if stop.load(Ordering::Relaxed) {
                            reset::park(port.as_mut());
                            return;
                        }
                    }
                    reset::park(port.as_mut());
                }
                Err(e) => {
                    if tx.send(Event::Disconnected(e.to_string())).is_err() {
                        return;
                    }
                }
            }
            if stop.load(Ordering::Relaxed) {
                return;
            }
            if tx.send(Event::Reconnecting).is_err() {
                return;
            }
            thread::sleep(RETRY_DELAY);
        }
    })
}
