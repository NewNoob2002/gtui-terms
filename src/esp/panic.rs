//! Streaming detection of ESP-IDF panic dumps and core dumps.
//!
//! Markers follow esp-idf-monitor (`esp_idf_monitor/base/constants.py` and
//! `coredump.py`) so the TUI reacts to the same text a human would see.

pub const PANIC_HINTS: [&str; 4] = [
    "Guru Meditation Error",
    "register dump:",
    "panic'ed",
    "assert failed",
];
pub const PANIC_END: &str = "ELF file SHA256:";
pub const COREDUMP_START: &str = "================= CORE DUMP START =================";
pub const COREDUMP_END: &str = "================= CORE DUMP END =================";

const MAX_PANIC_LINES: usize = 200;
const MAX_ADDRS: usize = 24;

pub enum Finding {
    Panic(Panic),
    Coredump(String),
}

pub struct Panic {
    /// Program counter first when the dump contained one, then the backtrace.
    pub addresses: Vec<u64>,
    pub sha256: Option<String>,
    pub lines: Vec<String>,
}

enum State {
    Idle,
    Panic {
        lines: Vec<String>,
        pc: Option<u64>,
        addrs: Vec<u64>,
        sha: Option<String>,
    },
    Dump {
        lines: Vec<String>,
    },
}

pub struct Scanner {
    partial: Vec<u8>,
    state: State,
}

impl Scanner {
    pub fn new() -> Self {
        Self {
            partial: Vec::new(),
            state: State::Idle,
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Finding> {
        let mut findings = Vec::new();
        self.partial.extend_from_slice(bytes);
        while let Some(pos) = self.partial.iter().position(|&byte| byte == b'\n') {
            let raw: Vec<u8> = self.partial.drain(..=pos).collect();
            let text = String::from_utf8_lossy(&raw);
            let line = text.trim_end_matches(['\r', '\n']);
            if let Some(finding) = self.line(line) {
                findings.push(finding);
            }
        }
        findings
    }

    fn line(&mut self, line: &str) -> Option<Finding> {
        match &mut self.state {
            State::Idle => {
                if line.contains(COREDUMP_START) {
                    self.state = State::Dump {
                        lines: vec![line.to_string()],
                    };
                } else if PANIC_HINTS.iter().any(|hint| line.contains(hint)) {
                    let lines = vec![line.to_string()];
                    let mut addrs = Vec::new();
                    collect_addresses(line, &mut addrs);
                    self.state = State::Panic {
                        lines,
                        pc: None,
                        addrs,
                        sha: None,
                    };
                }
                None
            }
            State::Panic {
                lines,
                pc,
                addrs,
                sha,
            } => {
                lines.push(line.to_string());
                if let Some(end) = line.find(PANIC_END) {
                    *sha = Some(line[end + PANIC_END.len()..].trim().to_string());
                    let panic = Panic {
                        addresses: finish_addrs(pc.take(), addrs),
                        sha256: sha.take(),
                        lines: std::mem::take(lines),
                    };
                    self.state = State::Idle;
                    return Some(Finding::Panic(panic));
                }
                if line.trim_start().starts_with("PC")
                    && let Some(address) = first_address(line)
                {
                    pc.get_or_insert(address);
                }
                collect_addresses(line, addrs);
                if lines.len() >= MAX_PANIC_LINES {
                    let panic = Panic {
                        addresses: finish_addrs(pc.take(), addrs),
                        sha256: sha.take(),
                        lines: std::mem::take(lines),
                    };
                    self.state = State::Idle;
                    return Some(Finding::Panic(panic));
                }
                None
            }
            State::Dump { lines } => {
                if line.contains(COREDUMP_END) {
                    let text = std::mem::take(lines).join("\n");
                    self.state = State::Idle;
                    return Some(Finding::Coredump(text));
                }
                lines.push(line.to_string());
                if lines.len() > 4096 {
                    let text = std::mem::take(lines).join("\n");
                    self.state = State::Idle;
                    return Some(Finding::Coredump(text));
                }
                None
            }
        }
    }
}

fn finish_addrs(pc: Option<u64>, addrs: &[u64]) -> Vec<u64> {
    let mut out = Vec::with_capacity(MAX_ADDRS);
    if let Some(pc) = pc {
        out.push(pc);
    }
    for &address in addrs {
        if out.len() == MAX_ADDRS {
            break;
        }
        if !out.contains(&address) {
            out.push(address);
        }
    }
    out
}

fn first_address(line: &str) -> Option<u64> {
    let mut found = Vec::new();
    collect_addresses(line, &mut found);
    found.first().copied()
}

/// Looks for `0x` followed by exactly eight hex digits (32-bit Xtensa/RISC-V)
/// and keeps only addresses that live in the chip's code/ROM/IRAM windows.
fn collect_addresses(line: &str, out: &mut Vec<u64>) {
    let bytes = line.as_bytes();
    let mut index = 0;
    while index + 10 <= bytes.len() {
        let window = &bytes[index..index + 2];
        if window == b"0x" || window == b"0X" {
            let digits = &bytes[index + 2..index + 10];
            if digits.iter().all(u8::is_ascii_hexdigit)
                && let Ok(value) =
                    u64::from_str_radix(std::str::from_utf8(digits).unwrap_or_default(), 16)
            {
                if is_code_address(value) && !out.contains(&value) {
                    out.push(value);
                }
                index += 10;
                continue;
            }
        }
        index += 1;
    }
}

fn is_code_address(value: u64) -> bool {
    (0x3c00_0000..0x3e00_0000).contains(&value) || (0x4000_0000..0x4400_0000).contains(&value)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DUMP: &str = "Guru Meditation Error: Core  0 panic'ed (LoadProhibited). Exception was unhandled.\n\nCore  0 register dump:\nPC      : 0x400d1f2a  PS      : 0x00060d30\nEXCVADDR: 0x00000000\n\nBacktrace: 0x400d1f2a:0x3ffb1f60 0x400d2234:0x3ffb1f90\n\nELF file SHA256: 9f2c1b\n\nRebooting...\n";

    #[test]
    fn detects_panic_and_collects_addresses() {
        let mut scanner = Scanner::new();
        let findings = scanner.feed(DUMP.as_bytes());
        assert_eq!(findings.len(), 1);
        let Finding::Panic(panic) = &findings[0] else {
            panic!("expected a panic");
        };
        assert_eq!(panic.sha256.as_deref(), Some("9f2c1b"));
        assert_eq!(panic.addresses.first().copied(), Some(0x400d_1f2a));
        assert!(panic.addresses.contains(&0x400d_2234));
        assert_eq!(panic.lines.len(), 9);
    }

    #[test]
    fn panic_split_across_chunks_is_detected_once() {
        let mut scanner = Scanner::new();
        let (head, tail) = DUMP.split_at(40);
        assert!(scanner.feed(head.as_bytes()).is_empty());
        let findings = scanner.feed(tail.as_bytes());
        assert_eq!(findings.len(), 1);
    }

    #[test]
    fn plain_boot_log_is_ignored() {
        let mut scanner = Scanner::new();
        assert!(
            scanner
                .feed(b"I (31) boot: ESP-IDF v5.4\nI (33) boot: chip revision: v1.1\n")
                .is_empty()
        );
    }

    #[test]
    fn coredump_is_captured_between_markers() {
        let mut scanner = Scanner::new();
        let log = format!(
            "Press Enter to print core dump to UART...\n{COREDUMP_START}\nAAECAw==\n{COREDUMP_END}\n"
        );
        let findings = scanner.feed(log.as_bytes());
        let Finding::Coredump(text) = &findings[0] else {
            panic!("expected a coredump");
        };
        assert!(text.contains("AAECAw=="));
        assert!(scanner.feed(b"normal").is_empty());
    }

    #[test]
    fn address_parser_keeps_only_code_windows() {
        let mut out = Vec::new();
        collect_addresses(
            "x 0x1f 0x400d1f2a 0x3ffb1f60 0x00060d30 0x4008abcd y",
            &mut out,
        );
        assert_eq!(out, vec![0x400d_1f2a, 0x4008_abcd]);
    }

    #[test]
    fn register_dump_noise_is_filtered_out() {
        let mut scanner = Scanner::new();
        let findings = scanner.feed(DUMP.as_bytes());
        let Finding::Panic(panic) = &findings[0] else {
            panic!("expected a panic");
        };
        assert_eq!(panic.addresses, vec![0x400d_1f2a, 0x400d_2234]);
    }
}
