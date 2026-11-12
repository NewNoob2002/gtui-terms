use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Datelike, Utc};

/// Complete, byte-preserving capture, independent of the UI scrollback.
pub struct Session {
    writer: Option<BufWriter<File>>,
    path: PathBuf,
    incomplete: bool,
}

impl Session {
    pub fn new() -> io::Result<Self> {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let base = std::env::temp_dir();
        for suffix in 0..u64::MAX {
            let path = base.join(format!(
                "gtui-session-{}-{stamp}-{suffix}.log",
                std::process::id()
            ));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => {
                    return Ok(Self {
                        writer: Some(BufWriter::new(file)),
                        path,
                        incomplete: false,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "cannot create a unique session capture",
        ))
    }

    pub fn record(&mut self, at: u64, direction: &str, bytes: &[u8]) -> io::Result<()> {
        self.require_complete()?;
        let result = (|| {
            let writer = self.writer.as_mut().ok_or_else(capture_closed)?;
            write_record(writer, at, direction, bytes)
        })();
        if result.is_err() {
            self.incomplete = true;
        }
        result
    }

    pub fn export(&mut self, path: &Path) -> io::Result<()> {
        self.require_complete()?;
        let writer = self.writer.as_mut().ok_or_else(capture_closed)?;
        if let Err(error) = writer.flush() {
            self.incomplete = true;
            return Err(error);
        }

        // A separate handle leaves the buffered writer's append position untouched.
        let mut source = File::open(&self.path)?;
        let mut destination = OpenOptions::new().write(true).create_new(true).open(path)?;
        let result = io::copy(&mut source, &mut destination).and_then(|_| destination.flush());
        // Windows requires the handle to be closed before removing a partial export.
        drop(destination);
        if result.is_err() {
            let _ = fs::remove_file(path);
        }
        result
    }

    fn require_complete(&self) -> io::Result<()> {
        if self.incomplete {
            Err(io::Error::other("session capture is incomplete"))
        } else {
            Ok(())
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Close the spool before removal, including when flushing fails on Windows.
        drop(self.writer.take());
        let _ = fs::remove_file(&self.path);
    }
}

fn capture_closed() -> io::Error {
    io::Error::other("session capture is closed")
}

pub fn write_record(
    writer: &mut impl Write,
    at: u64,
    direction: &str,
    bytes: &[u8],
) -> io::Result<()> {
    let timestamp = i64::try_from(at)
        .ok()
        .and_then(DateTime::<Utc>::from_timestamp_millis)
        .filter(|timestamp| timestamp.year() <= 9999)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "session timestamp is outside the RFC3339 range",
            )
        })?;
    write!(
        writer,
        "{} direction=\"",
        timestamp.format("%Y-%m-%dT%H:%M:%S%.3fZ")
    )?;
    write_escaped(writer, direction.as_bytes())?;
    write!(writer, "\" length={} hex=\"", bytes.len())?;
    for &byte in bytes {
        write_hex(writer, byte)?;
    }
    writer.write_all(b"\" text=\"")?;
    write_escaped(writer, bytes)?;
    writer.write_all(b"\"\n")
}

fn write_hex(writer: &mut impl Write, byte: u8) -> io::Result<()> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    writer.write_all(&[HEX[(byte >> 4) as usize], HEX[(byte & 0xf) as usize]])
}

fn write_escaped(writer: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    for &byte in bytes {
        match byte {
            b'\r' => writer.write_all(b"\\r")?,
            b'\n' => writer.write_all(b"\\n")?,
            b'\t' => writer.write_all(b"\\t")?,
            b'"' => writer.write_all(b"\\\"")?,
            b'\\' => writer.write_all(b"\\\\")?,
            b' '..=b'~' => writer.write_all(&[byte])?,
            _ => {
                writer.write_all(b"\\x")?;
                write_hex(writer, byte)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Exports(Vec<PathBuf>);

    impl Exports {
        fn new(session: &Session) -> Self {
            Self(vec![
                session.path.with_extension("first-export"),
                session.path.with_extension("second-export"),
            ])
        }
    }

    impl Drop for Exports {
        fn drop(&mut self) {
            for path in &self.0 {
                let _ = fs::remove_file(path);
            }
        }
    }

    #[test]
    fn binary_and_text_are_exact_and_round_trip() {
        let mut session = Session::new().unwrap();
        let exports = Exports::new(&session);
        let bytes = b"A\x00\xff\r\n\t\"\\\xc3\xa9";
        session.record(1_704_164_645_006, "RX", bytes).unwrap();
        session.export(&exports.0[0]).unwrap();
        let captured = fs::read_to_string(&exports.0[0]).unwrap();
        assert_eq!(
            captured,
            "2024-01-02T03:04:05.006Z direction=\"RX\" length=10 hex=\"4100ff0d0a09225cc3a9\" text=\"A\\x00\\xff\\r\\n\\t\\\"\\\\\\xc3\\xa9\"\n"
        );
        let hex = captured
            .split("hex=\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap();
        let decoded: Vec<u8> = hex
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect();
        assert_eq!(decoded, bytes);
    }

    #[test]
    fn later_exports_include_new_records_and_keep_previous_snapshot() {
        let mut session = Session::new().unwrap();
        let exports = Exports::new(&session);
        session.record(0, "RX", b"first").unwrap();
        session.export(&exports.0[0]).unwrap();
        let first = fs::read(&exports.0[0]).unwrap();
        session.record(1, "TX", b"second").unwrap();
        session.export(&exports.0[1]).unwrap();
        assert_eq!(fs::read(&exports.0[0]).unwrap(), first);
        let second = fs::read_to_string(&exports.0[1]).unwrap();
        assert!(second.starts_with(std::str::from_utf8(&first).unwrap()));
        assert!(second.ends_with("hex=\"7365636f6e64\" text=\"second\"\n"));
        assert_eq!(second.lines().count(), 2);
    }

    #[test]
    fn existing_destination_is_untouched_and_capture_remains_exportable() {
        let mut session = Session::new().unwrap();
        let exports = Exports::new(&session);
        fs::write(&exports.0[0], b"keep this").unwrap();
        session.record(0, "RX", b"captured").unwrap();
        let error = session.export(&exports.0[0]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&exports.0[0]).unwrap(), b"keep this");
        session.export(&exports.0[1]).unwrap();
        assert!(
            fs::read_to_string(&exports.0[1])
                .unwrap()
                .contains("text=\"captured\"")
        );
    }

    #[test]
    fn empty_capture_and_empty_records_export_without_panicking() {
        let mut session = Session::new().unwrap();
        let exports = Exports::new(&session);
        session.export(&exports.0[0]).unwrap();
        assert_eq!(fs::metadata(&exports.0[0]).unwrap().len(), 0);
        session.record(0, "TX", b"").unwrap();
        session.export(&exports.0[1]).unwrap();
        assert_eq!(
            fs::read_to_string(&exports.0[1]).unwrap(),
            "1970-01-01T00:00:00.000Z direction=\"TX\" length=0 hex=\"\" text=\"\"\n"
        );
    }

    #[test]
    fn invalid_timestamp_poisoning_prevents_incomplete_export() {
        let mut session = Session::new().unwrap();
        let exports = Exports::new(&session);
        session.record(0, "RX", b"valid").unwrap();
        assert_eq!(
            session
                .record(u64::MAX, "RX", b"missing")
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            session.export(&exports.0[0]).unwrap_err().to_string(),
            "session capture is incomplete"
        );
        assert!(!exports.0[0].exists());
    }

    #[test]
    fn flush_failure_poisoning_prevents_later_export() {
        let mut session = Session::new().unwrap();
        let exports = Exports::new(&session);
        session.writer = Some(BufWriter::new(File::open(&session.path).unwrap()));
        session.record(0, "RX", b"cannot flush").unwrap();
        assert!(session.export(&exports.0[0]).is_err());
        assert!(!exports.0[0].exists());
        assert_eq!(
            session.export(&exports.0[1]).unwrap_err().to_string(),
            "session capture is incomplete"
        );
    }

    #[test]
    fn write_failure_poisoning_prevents_incomplete_export() {
        let mut session = Session::new().unwrap();
        let exports = Exports::new(&session);
        session.writer = Some(BufWriter::new(File::open(&session.path).unwrap()));
        let bytes = vec![0xff; 16 * 1024];
        assert!(session.record(0, "RX", &bytes).is_err());
        assert_eq!(
            session.export(&exports.0[0]).unwrap_err().to_string(),
            "session capture is incomplete"
        );
        assert!(!exports.0[0].exists());
    }

    #[test]
    fn rfc3339_upper_boundary_is_supported_and_next_millisecond_is_rejected() {
        let mut session = Session::new().unwrap();
        let exports = Exports::new(&session);
        session.record(253_402_300_799_999, "RX", b"").unwrap();
        session.export(&exports.0[0]).unwrap();
        assert!(
            fs::read_to_string(&exports.0[0])
                .unwrap()
                .starts_with("9999-12-31T23:59:59.999Z ")
        );
        assert_eq!(
            session
                .record(253_402_300_800_000, "RX", b"")
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn dropping_session_closes_and_removes_spool() {
        let mut session = Session::new().unwrap();
        let path = session.path.clone();
        session.record(0, "RX", b"buffered").unwrap();
        assert!(path.exists());
        drop(session);
        assert!(!path.exists());
    }
}
