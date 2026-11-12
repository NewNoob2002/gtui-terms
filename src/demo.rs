use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::serial::Event;

const BOOT_LINES: [&str; 10] = [
    "\x1b[32mI (31) boot: ESP-IDF v5.4-dirty 2nd stage bootloader\x1b[0m",
    "I (31) boot: compile time Nov 12 2026 10:00:00",
    "I (33) boot: chip revision: v1.1",
    "I (36) boot.esp32: SPI Speed      : 40MHz",
    "I (41) boot.esp32: SPI Flash Size : 4MB",
    "I (46) boot: Enabling RNG early entropy source...",
    "I (169) app_init: Project name:     gtui_demo",
    "I (173) app_init: App version:      1.0.0",
    "I (310) main_task: Started on CPU0",
    "I (320) demo: simulated ESP32 boot complete",
];

/// A realistic abort() dump: register dump, backtrace, ELF hash, reboot.
const PANIC_LINES: [&str; 9] = [
    "",
    "Guru Meditation Error: Core  0 panic'ed (LoadProhibited). Exception was unhandled.",
    "",
    "Core  0 register dump:",
    "PC      : 0x400d1f2a  PS      : 0x00060d30  A0      : 0x800d2234  A1      : 0x3ffb1f60",
    "EXCVADDR: 0x00000000",
    "",
    "Backtrace: 0x400d1f2a:0x3ffb1f60 0x400d2234:0x3ffb1f90",
    "ELF file SHA256: 9f2c1b7d2a",
];

pub fn spawn(tx: Sender<Event>, stop: Arc<AtomicBool>, with_panic: bool) -> JoinHandle<()> {
    thread::spawn(move || {
        let _ = tx.send(Event::Connected("simulated device".to_string()));
        for line in BOOT_LINES {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            if tx
                .send(Event::Data(format!("{line}\r\n").into_bytes()))
                .is_err()
            {
                return;
            }
            thread::sleep(Duration::from_millis(70));
        }
        if with_panic {
            for line in PANIC_LINES {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                if tx
                    .send(Event::Data(format!("{line}\r\n").into_bytes()))
                    .is_err()
                {
                    return;
                }
                thread::sleep(Duration::from_millis(30));
            }
        }
        let mut tick = 0;
        loop {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            tick += 1;
            let progress = (tick * 5) % 100;
            let redrawn = format!("\rdemo: progress {progress:>3}% [");
            let bar = "#".repeat(progress / 4);
            let blank = " ".repeat(25 - progress / 4);
            let line = format!("{redrawn}{bar}{blank}]");
            if tx.send(Event::Data(line.into_bytes())).is_err() {
                return;
            }
            if progress == 0 || tick % 10 == 0 {
                let note = format!("\r\nI ({}ms) demo: tick {tick}\r\n", tick * 500);
                if tx.send(Event::Data(note.into_bytes())).is_err() {
                    return;
                }
            }
            thread::sleep(Duration::from_millis(100));
        }
    })
}
