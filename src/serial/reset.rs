//! ESP32 reset/bootloader entry sequences, ported from esp-idf-monitor's
//! `esp_pylib/serial_reset.py` (Apache-2.0) and `chip_specific_config.py`.
//!
//! `high == true` means "assert the line" (pyserial's `dtr/rts = True`).

use std::io;
use std::time::Duration;

use serialport::SerialPort;

/// Minimal reset timing used by current esp-idf-monitor defaults.
pub const MINIMAL_EN_LOW_DELAY: Duration = Duration::from_millis(5);
pub const ENTER_BOOT_SET: Duration = Duration::from_millis(100);
pub const ENTER_BOOT_UNSET: Duration = Duration::from_millis(50);

/// CH340C/CP210x style adapters that drive EN through RTS and IO0 through DTR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Classic,
    /// Native USB-Serial-JTAG of ESP32-C3/C6/S3 (USB PID 0x1001).
    UsbJtag,
}

impl Kind {
    pub fn from_pid(pid: Option<u16>) -> Self {
        match pid {
            Some(0x1001) => Kind::UsbJtag,
            _ => Kind::Classic,
        }
    }
}

/// The only two modem control lines these chips use.
pub trait LinePins {
    fn set_dtr(&mut self, high: bool) -> io::Result<()>;
    fn set_rts(&mut self, high: bool) -> io::Result<()>;
}

struct Pins<'a> {
    port: &'a mut dyn SerialPort,
    /// Windows `usbser.sys` only emits SET_CONTROL_LINE_STATE when RTS changes,
    /// so the current DTR value is re-written after every RTS transition.
    dtr: bool,
}

impl LinePins for Pins<'_> {
    fn set_dtr(&mut self, high: bool) -> io::Result<()> {
        self.dtr = high;
        self.port
            .write_data_terminal_ready(high)
            .map_err(io::Error::other)
    }

    fn set_rts(&mut self, high: bool) -> io::Result<()> {
        self.port
            .write_request_to_send(high)
            .map_err(io::Error::other)?;
        self.port
            .write_data_terminal_ready(self.dtr)
            .map_err(io::Error::other)
    }
}

/// Runs the sequence, leaving DTR/RTS deasserted-ish the way esp-idf-monitor does.
pub fn run(port: &mut dyn SerialPort, kind: Kind, into_bootloader: bool) -> io::Result<()> {
    let mut pins = Pins { port, dtr: false };
    let mut wait = |duration: Duration| std::thread::sleep(duration);
    apply(&mut pins, kind, into_bootloader, &mut wait)
}

/// Same as [`run`] but with an injectable delay so tests stay fast.
pub fn apply(
    pins: &mut dyn LinePins,
    kind: Kind,
    into_bootloader: bool,
    wait: &mut dyn FnMut(Duration),
) -> io::Result<()> {
    match (kind, into_bootloader) {
        (Kind::Classic, true) => {
            pins.set_dtr(false)?;
            pins.set_rts(true)?;
            wait(ENTER_BOOT_SET);
            pins.set_dtr(true)?;
            pins.set_rts(false)?;
            wait(ENTER_BOOT_UNSET);
            pins.set_dtr(false)?;
        }
        (Kind::Classic, false) => {
            pins.set_rts(true)?;
            wait(MINIMAL_EN_LOW_DELAY);
            pins.set_rts(false)?;
        }
        (Kind::UsbJtag, true) => {
            pins.set_rts(true)?;
            pins.set_dtr(true)?;
            wait(Duration::from_millis(100));
            pins.set_dtr(false)?;
            pins.set_rts(true)?;
            wait(Duration::from_millis(100));
            pins.set_rts(false)?;
            pins.set_dtr(true)?;
            pins.set_rts(false)?;
            wait(Duration::from_millis(100));
            pins.set_dtr(true)?;
            pins.set_rts(true)?;
        }
        (Kind::UsbJtag, false) => {
            pins.set_rts(true)?;
            wait(Duration::from_millis(100));
            pins.set_rts(false)?;
        }
    }
    Ok(())
}

/// One element of a user-supplied `D0 R1 W0.1` sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Dtr(bool),
    Rts(bool),
    Wait(Duration),
}

/// Parses `D0|D1|R0|R1|W<seconds>` tokens, ignoring case and extra spaces.
pub fn parse_steps(text: &str) -> Result<Vec<Step>, String> {
    let mut steps = Vec::new();
    for token in text.split_whitespace() {
        let mut chars = token.chars();
        let head = chars.next().unwrap_or_default().to_ascii_uppercase();
        let rest = chars.as_str();
        let step = match head {
            'D' | 'R' => {
                let high = match rest {
                    "0" => false,
                    "1" => true,
                    _ => return Err(format!("{token}: use D0/D1 or R0/R1")),
                };
                if head == 'D' {
                    Step::Dtr(high)
                } else {
                    Step::Rts(high)
                }
            }
            'W' => {
                let seconds: f64 = rest
                    .parse()
                    .map_err(|_| format!("{token}: wait needs a number of seconds"))?;
                if !(0.0..=10.0).contains(&seconds) {
                    return Err(format!("{token}: wait must be 0..=10 s"));
                }
                Step::Wait(Duration::from_secs_f64(seconds))
            }
            _ => return Err(format!("{token}: unknown step")),
        };
        steps.push(step);
    }
    if steps.is_empty() {
        return Err("empty sequence".to_string());
    }
    Ok(steps)
}

pub fn run_steps(port: &mut dyn SerialPort, steps: &[Step]) -> io::Result<()> {
    let mut pins = Pins { port, dtr: false };
    apply_steps(&mut pins, steps, &mut std::thread::sleep)
}

fn apply_steps(
    pins: &mut dyn LinePins,
    steps: &[Step],
    wait: &mut dyn FnMut(Duration),
) -> io::Result<()> {
    for step in steps {
        match *step {
            Step::Dtr(high) => pins.set_dtr(high)?,
            Step::Rts(high) => pins.set_rts(high)?,
            Step::Wait(duration) => wait(duration),
        }
    }
    Ok(())
}

/// Deasserts DTR before the port is closed: on CP2102C/CH343 style adapters the
/// HUPCL pulse on close would otherwise reset the chip.
pub fn park(port: &mut dyn SerialPort) {
    let _ = port.write_data_terminal_ready(false);
    let _ = port.write_request_to_send(false);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Mock {
        events: Vec<(char, bool)>,
        waits: Vec<Duration>,
    }

    impl LinePins for Mock {
        fn set_dtr(&mut self, high: bool) -> io::Result<()> {
            self.events.push(('D', high));
            Ok(())
        }

        fn set_rts(&mut self, high: bool) -> io::Result<()> {
            self.events.push(('R', high));
            Ok(())
        }
    }

    fn run_mock(kind: Kind, into_bootloader: bool) -> Mock {
        let mut mock = Mock::default();
        let mut waits = Vec::new();
        let mut wait = |duration: Duration| waits.push(duration);
        apply(&mut mock, kind, into_bootloader, &mut wait).unwrap();
        mock.waits = waits;
        mock
    }

    #[test]
    fn parses_and_runs_custom_steps() {
        let steps = parse_steps("d0 R1 w0.05 D1").unwrap();
        assert_eq!(
            steps,
            vec![
                Step::Dtr(false),
                Step::Rts(true),
                Step::Wait(Duration::from_millis(50)),
                Step::Dtr(true)
            ]
        );
        let mut mock = Mock::default();
        let mut waits = Vec::new();
        let mut wait = |duration: Duration| waits.push(duration);
        apply_steps(&mut mock, &steps, &mut wait).unwrap();
        assert_eq!(mock.events, vec![('D', false), ('R', true), ('D', true)]);
        assert_eq!(waits, vec![Duration::from_millis(50)]);
    }

    #[test]
    fn rejects_bad_sequences() {
        assert!(parse_steps("X1").is_err());
        assert!(parse_steps("D2").is_err());
        assert!(parse_steps("Wabc").is_err());
        assert!(parse_steps("W99").is_err());
        assert!(parse_steps("   ").is_err());
    }

    #[test]
    fn classic_download_holds_reset_then_samples_gpio0_low() {
        use std::cell::Cell;

        struct StrapPins<'a>(&'a Cell<(bool, bool)>);

        impl LinePins for StrapPins<'_> {
            fn set_dtr(&mut self, high: bool) -> io::Result<()> {
                self.0.set((high, self.0.get().1));
                Ok(())
            }

            fn set_rts(&mut self, high: bool) -> io::Result<()> {
                self.0.set((self.0.get().0, high));
                Ok(())
            }
        }

        let state = Cell::new((false, false));
        let mut phases = Vec::new();
        apply(&mut StrapPins(&state), Kind::Classic, true, &mut |delay| {
            let (dtr, rts) = state.get();
            // The board's transistor pair drives EN/IO0 low only when the lines differ.
            phases.push((rts && !dtr, dtr && !rts, delay));
        })
        .unwrap();

        assert_eq!(
            phases,
            vec![
                (true, false, ENTER_BOOT_SET),
                (false, true, ENTER_BOOT_UNSET)
            ]
        );
        assert_eq!(state.get(), (false, false));
    }

    #[test]
    fn classic_hard_reset_pulses_rts_only() {
        let mock = run_mock(Kind::Classic, false);
        assert_eq!(mock.events, vec![('R', true), ('R', false)]);
        assert_eq!(mock.waits, vec![MINIMAL_EN_LOW_DELAY]);
    }

    #[test]
    fn usb_jtag_bootloader_uses_its_own_sequence() {
        let mock = run_mock(Kind::UsbJtag, true);
        assert_eq!(
            mock.events,
            vec![
                ('R', true),
                ('D', true),
                ('D', false),
                ('R', true),
                ('R', false),
                ('D', true),
                ('R', false),
                ('D', true),
                ('R', true)
            ]
        );
    }

    #[test]
    fn usb_jtag_hard_reset_holds_rts() {
        let mock = run_mock(Kind::UsbJtag, false);
        assert_eq!(mock.events, vec![('R', true), ('R', false)]);
    }

    #[test]
    fn pid_1001_selects_usb_jtag() {
        assert_eq!(Kind::from_pid(Some(0x1001)), Kind::UsbJtag);
        assert_eq!(Kind::from_pid(Some(0x7523)), Kind::Classic);
        assert_eq!(Kind::from_pid(None), Kind::Classic);
    }
}
