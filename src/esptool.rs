//! Validated esptool commands and bounded, concurrent process output.

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::process::{Command, Stdio};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    ChipId,
    FlashId,
    ReadMac,
    EraseFlash,
    WriteFlash,
}

impl Operation {
    pub fn label(self) -> &'static str {
        match self {
            Self::ChipId => "chip-id",
            Self::FlashId => "flash-id",
            Self::ReadMac => "read-mac",
            Self::EraseFlash => "erase-flash",
            Self::WriteFlash => "write-flash",
        }
    }

    pub fn destructive(self) -> bool {
        matches!(self, Self::EraseFlash | Self::WriteFlash)
    }
}

pub struct Invocation {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub summary: String,
}

pub fn build(
    port: &str,
    baud: u32,
    operation: Operation,
    arguments: &str,
) -> Result<Invocation, String> {
    if port.trim().is_empty() || port.starts_with('-') || port.contains('\0') {
        return Err("select a valid serial port before running esptool".into());
    }
    if baud == 0 {
        return Err("esptool baud rate must be greater than zero".into());
    }
    if operation != Operation::WriteFlash && !arguments.trim().is_empty() {
        return Err(format!("{} does not accept arguments", operation.label()));
    }
    let program = std::env::var_os("GTUI_ESPTOOL_PYTHON").unwrap_or_else(|| "python".into());
    if program.is_empty() {
        return Err("GTUI_ESPTOOL_PYTHON must name a Python executable".into());
    }
    let mut args: Vec<OsString> = [
        "-u",
        "-m",
        "esptool",
        "--port",
        port,
        "--baud",
        &baud.to_string(),
        "--before",
        "default-reset",
        "--after",
        "hard-reset",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    // The global option is supported by esptool's ROM commands:
    // https://github.com/espressif/esptool/blob/master/esptool/__init__.py
    if !operation.destructive() {
        args.push("--no-stub".into());
    }
    args.push(operation.label().into());
    if operation == Operation::WriteFlash {
        let words = split_arguments(arguments)?;
        if words.is_empty() || words.len() % 2 != 0 {
            return Err("write-flash requires one or more <offset> <file> pairs; quote paths containing spaces".into());
        }
        let mut ranges = Vec::with_capacity(words.len() / 2);
        for pair in words.as_chunks::<2>().0 {
            let offset = parse_offset(&pair[0])?;
            if pair[1].is_empty() || pair[1].starts_with('-') || pair[1].contains('\0') {
                return Err(
                    "each flash offset must be followed by an image path, not an option".into(),
                );
            }
            let path = Path::new(&pair[1]);
            let metadata = path
                .metadata()
                .map_err(|err| format!("cannot inspect image {}: {err}", path.display()))?;
            if !metadata.is_file() {
                return Err(format!(
                    "image {} must be a nonempty regular file",
                    path.display()
                ));
            }
            let image = File::open(path)
                .map_err(|err| format!("cannot read image {}: {err}", path.display()))?;
            let metadata = image
                .metadata()
                .map_err(|err| format!("cannot inspect image {}: {err}", path.display()))?;
            if !metadata.is_file() || metadata.len() == 0 {
                return Err(format!(
                    "image {} must be a nonempty regular file",
                    path.display()
                ));
            }
            let end = u64::from(offset)
                .checked_add(metadata.len())
                .filter(|end| *end <= u64::from(u32::MAX) + 1)
                .ok_or_else(|| {
                    format!(
                        "image {} exceeds the 32-bit flash address range",
                        path.display()
                    )
                })?;
            if ranges
                .iter()
                .any(|&(start, stop)| u64::from(offset) < stop && start < end)
            {
                return Err(format!(
                    "image {} overlaps another flash image",
                    path.display()
                ));
            }
            ranges.push((u64::from(offset), end));
            // An absolute path cannot be interpreted as an esptool flag.
            let path = path
                .canonicalize()
                .map_err(|err| format!("cannot resolve image {}: {err}", path.display()))?;
            args.push(format!("0x{offset:x}").into());
            args.push(path.into_os_string());
        }
    }
    let command = std::iter::once(&program)
        .chain(args.iter())
        .map(|arg| format!("{arg:?}"))
        .collect::<Vec<_>>()
        .join(" ");
    Ok(Invocation {
        program,
        args,
        summary: format!("{} on {port} at {baud} baud\n{command}", operation.label()),
    })
}

fn parse_offset(value: &str) -> Result<u32, String> {
    let (digits, radix) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .map_or((value, 10), |digits| (digits, 16));
    if digits.is_empty()
        || !digits.bytes().all(|byte| match radix {
            16 => byte.is_ascii_hexdigit(),
            _ => byte.is_ascii_digit(),
        })
    {
        return Err(format!(
            "invalid flash offset {value:?}; use decimal or 0x hexadecimal"
        ));
    }
    u32::from_str_radix(digits, radix)
        .map_err(|_| format!("flash offset {value:?} exceeds the 32-bit address range"))
}

// Quotes group paths; backslashes are always literal, including before a closing quote.
fn split_arguments(arguments: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut started = false;
    for ch in arguments.chars() {
        match quote {
            Some(delimiter) if ch == delimiter => quote = None,
            Some(_) => word.push(ch),
            None if ch == '\'' || ch == '"' => {
                quote = Some(ch);
                started = true;
            }
            None if ch.is_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            None => {
                word.push(ch);
                started = true;
            }
        }
    }
    if quote.is_some() {
        return Err("unclosed quote in flash image arguments".into());
    }
    if started {
        words.push(word);
    }
    Ok(words)
}

pub fn run(
    invocation: &Invocation,
    on_output: impl Fn(String) + Send + Sync,
) -> crate::hooks::Outcome {
    let mut child = match Command::new(&invocation.program)
        .args(&invocation.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            return crate::hooks::Outcome {
                ok: false,
                text: format!(
                    "cannot start Python {:?}: {err}. Install Python and esptool with that interpreter's -m pip install esptool, or set GTUI_ESPTOOL_PYTHON to its executable path.",
                    invocation.program
                ),
            };
        }
    };
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");
    let read_result = std::thread::scope(|scope| {
        let output = &on_output;
        let stdout_thread = match std::thread::Builder::new()
            .name("esptool-stdout".into())
            .spawn_scoped(scope, move || drain(stdout, output))
        {
            Ok(thread) => thread,
            Err(err) => {
                let _ = child.kill();
                return Err(format!("cannot start esptool output reader: {err}"));
            }
        };
        let stderr_result = drain(stderr, output);
        let stdout_result = stdout_thread
            .join()
            .map_err(|_| "esptool stdout reader panicked".to_string())?;
        stderr_result.and(stdout_result)
    });
    let status = child.wait();
    let status_result = match status {
        Ok(status) => {
            let detail = status.code().map_or_else(
                || "terminated without an exit code".to_string(),
                |code| format!("exited with code {code}"),
            );
            on_output(format!("esptool {detail}"));
            Ok(status.success())
        }
        Err(err) => Err(format!("cannot wait for esptool: {err}")),
    };
    match (read_result, status_result) {
        (Ok(()), Ok(ok)) => crate::hooks::Outcome {
            ok,
            text: String::new(),
        },
        (Err(text), Ok(_)) | (Ok(()), Err(text)) => crate::hooks::Outcome { ok: false, text },
        (Err(read), Err(wait)) => crate::hooks::Outcome {
            ok: false,
            text: format!("{read}; {wait}"),
        },
    }
}

const OUTPUT_CHUNK: usize = 4096;

fn drain(mut reader: impl Read, on_output: &impl Fn(String)) -> Result<(), String> {
    let mut read_buffer = [0; 2048];
    let mut pending = Vec::with_capacity(OUTPUT_CHUNK + 3);
    loop {
        let count = match reader.read(&mut read_buffer) {
            Ok(count) => count,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(format!("cannot read esptool output: {err}")),
        };
        if count == 0 {
            emit(&mut pending, on_output);
            return Ok(());
        }
        for &byte in &read_buffer[..count] {
            if byte == b'\r' || byte == b'\n' {
                emit(&mut pending, on_output);
                continue;
            }
            pending.push(byte);
            if pending.len() >= OUTPUT_CHUNK {
                // Keep a UTF-8 character together when a long output line is split.
                let incomplete = std::str::from_utf8(&pending)
                    .err()
                    .is_some_and(|err| err.error_len().is_none());
                if !incomplete || pending.len() == OUTPUT_CHUNK + 3 {
                    emit(&mut pending, on_output);
                }
            }
        }
    }
}

fn emit(pending: &mut Vec<u8>, on_output: &impl Fn(String)) {
    if pending.is_empty() {
        return;
    }
    let text = String::from_utf8_lossy(pending).into_owned();
    if text.contains("No module named esptool") || text.contains("No module named 'esptool'") {
        on_output("esptool is not installed for the selected Python. Run that interpreter with -m pip install esptool, or set GTUI_ESPTOOL_PYTHON to the Python executable where esptool is installed.".into());
    }
    on_output(text);
    pending.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Images(std::path::PathBuf);

    impl Images {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "gtui-esptool-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn image(&self, name: &str, bytes: &[u8]) -> std::path::PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, bytes).unwrap();
            path
        }
    }

    impl Drop for Images {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write(arguments: &str) -> Result<Invocation, String> {
        build("COM3", 115200, Operation::WriteFlash, arguments)
    }

    #[test]
    fn quote_groups_preserve_windows_backslashes_and_shell_metacharacters() {
        assert_eq!(
            split_arguments(r#"0x1000 "C:\firmware builds\app.bin" 8192 '/tmp/a b.bin'"#).unwrap(),
            [
                "0x1000",
                r"C:\firmware builds\app.bin",
                "8192",
                "/tmp/a b.bin"
            ]
        );
        assert_eq!(
            split_arguments(r#"0 "C:\trailing\""#).unwrap(),
            ["0", "C:\\trailing\\"]
        );
        assert_eq!(
            split_arguments(r#"0 '$(touch owned); & %PATH%.bin'"#).unwrap(),
            ["0", "$(touch owned); & %PATH%.bin"]
        );
        assert!(split_arguments("0 'unclosed").is_err());
    }

    #[test]
    fn image_ranges_use_real_sizes_and_allow_adjacent_pairs() {
        let images = Images::new();
        let first = images.image("first image.bin", &[1; 32]);
        let second = images.image("second image.bin", &[2; 4]);
        assert!(
            write(&format!(
                "0x1000 \"{}\" 4128 \"{}\"",
                first.display(),
                second.display()
            ))
            .is_ok()
        );
        assert!(
            write(&format!(
                "0x1000 \"{}\" 4127 \"{}\"",
                first.display(),
                second.display()
            ))
            .err()
            .unwrap()
            .contains("overlaps")
        );
        assert!(
            write(&format!("0xffffffff \"{}\"", second.display()))
                .err()
                .unwrap()
                .contains("32-bit")
        );
        let one = images.image("last-byte.bin", &[1]);
        assert!(write(&format!("0xffffffff \"{}\"", one.display())).is_ok());
    }

    #[test]
    fn rejects_offsets_flags_missing_and_empty_images() {
        let images = Images::new();
        let image = images.image("valid.bin", &[1]);
        for offset in [
            "-1",
            "+1",
            "0x",
            "0xGG",
            "1e3",
            "4294967296",
            "0x100000000",
            "--help",
            "0;exit",
        ] {
            assert!(
                write(&format!("{offset} \"{}\"", image.display())).is_err(),
                "{offset}"
            );
        }
        for arguments in [
            "",
            "0",
            "0 --force",
            "--force 0",
            "0 missing.bin",
            "0 missing.bin ; touch owned",
        ] {
            assert!(write(arguments).is_err(), "{arguments}");
        }
        let empty = images.image("empty.bin", &[]);
        assert!(
            write(&format!("0 \"{}\"", empty.display()))
                .err()
                .unwrap()
                .contains("nonempty")
        );
        assert!(write(&format!("0 \"{}\"", images.0.display())).is_err());
        for operation in [
            Operation::ChipId,
            Operation::FlashId,
            Operation::ReadMac,
            Operation::EraseFlash,
        ] {
            assert!(build("COM3", 115200, operation, "--force").is_err());
        }
        assert!(build("", 115200, Operation::ReadMac, "").is_err());
        assert!(build("--help", 115200, Operation::ReadMac, "").is_err());
        assert!(build("COM3", 0, Operation::ReadMac, "").is_err());
    }

    #[cfg(windows)]
    #[test]
    fn rejects_images_locked_against_reading() {
        use std::os::windows::fs::OpenOptionsExt;
        let images = Images::new();
        let image = images.image("locked.bin", &[1]);
        let _lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&image)
            .unwrap();
        let error = write(&format!("0 \"{}\"", image.display())).err().unwrap();
        assert!(
            error.contains("cannot read") || error.contains("cannot inspect"),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_unreadable_images() {
        use std::os::unix::fs::PermissionsExt;
        let images = Images::new();
        let image = images.image("unreadable.bin", &[1]);
        std::fs::set_permissions(&image, std::fs::Permissions::from_mode(0)).unwrap();
        // Privileged users can read mode-000 files; exercise this only when access is denied.
        if File::open(&image).is_err() {
            assert!(
                write(&format!("0 \"{}\"", image.display()))
                    .err()
                    .unwrap()
                    .contains("cannot read")
            );
        }
        std::fs::set_permissions(&image, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[test]
    fn output_is_bounded_splits_progress_and_preserves_utf8() {
        let lines = Mutex::new(Vec::new());
        let input = format!("10%\r20%\r\n{}é\nlast", "x".repeat(OUTPUT_CHUNK - 1));
        drain(input.as_bytes(), &|line| lines.lock().unwrap().push(line)).unwrap();
        let lines = lines.into_inner().unwrap();
        assert_eq!(&lines[..2], ["10%", "20%"]);
        assert_eq!(lines[2], format!("{}é", "x".repeat(OUTPUT_CHUNK - 1)));
        assert_eq!(lines[3], "last");
        assert!(lines.iter().all(|line| line.len() <= OUTPUT_CHUNK + 3));
        let sizes = Mutex::new(Vec::new());
        drain(vec![b'x'; OUTPUT_CHUNK * 5].as_slice(), &|line| {
            sizes.lock().unwrap().push(line.len())
        })
        .unwrap();
        assert_eq!(sizes.into_inner().unwrap(), vec![OUTPUT_CHUNK; 5]);
    }

    #[test]
    fn drains_both_real_process_pipes_and_preserves_exit_status() {
        let program = std::env::var_os("GTUI_ESPTOOL_PYTHON").unwrap_or_else(|| "python".into());
        for code in [0, 7] {
            let script = format!(
                "import sys\nsys.stdout.write('o' * 200000 + '\\r50%\\r100%\\n')\nsys.stdout.flush()\nsys.stderr.write('e' * 200000 + '\\n')\nsys.stderr.flush()\nsys.exit({code})"
            );
            let invocation = Invocation {
                program: program.clone(),
                args: vec!["-u".into(), "-c".into(), script.into()],
                summary: String::new(),
            };
            let observed = Mutex::new((0usize, 0usize, false, false));
            let outcome = run(&invocation, |line| {
                let mut observed = observed.lock().unwrap();
                observed.0 += line.bytes().filter(|byte| *byte == b'o').count();
                observed.1 += line.bytes().filter(|byte| *byte == b'e').count();
                observed.2 |= line == "100%";
                observed.3 |= line == format!("esptool exited with code {code}");
                assert!(line.len() <= OUTPUT_CHUNK + 3);
            });
            assert_eq!(outcome.ok, code == 0, "{}", outcome.text);
            let observed = observed.into_inner().unwrap();
            assert!(observed.0 >= 200000 && observed.1 >= 200000);
            assert!(observed.2 && observed.3);
        }
    }

    #[test]
    fn output_read_errors_and_start_failures_are_reported() {
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("broken stream"))
            }
        }
        assert!(
            drain(Broken, &|_| {})
                .unwrap_err()
                .contains("broken stream")
        );
        let images = Images::new();
        let invocation = Invocation {
            program: images.0.join("missing-python").into_os_string(),
            args: Vec::new(),
            summary: String::new(),
        };
        let outcome = run(&invocation, |_| {
            panic!("failed spawn must not stream output")
        });
        assert!(!outcome.ok);
        assert!(outcome.text.contains("GTUI_ESPTOOL_PYTHON"));
    }
}
