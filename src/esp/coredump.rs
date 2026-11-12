use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use base64::Engine;

use super::panic::{COREDUMP_END, COREDUMP_START};

/// Preserve the UART capture even when decoding fails.
pub fn save(text: &str, directory: &Path) -> Result<PathBuf, String> {
    let raw = directory.join(format!(
        "coredump-{}.txt",
        chrono::Local::now().format("%Y%m%d-%H%M%S-%f")
    ));
    write_new(&raw, text.as_bytes())?;
    let bytes = decode(text).map_err(|err| format!("{err}; raw capture: {}", raw.display()))?;
    let binary = raw.with_extension("bin");
    write_new(&binary, &bytes).map_err(|err| format!("{err}; raw capture: {}", raw.display()))?;
    Ok(binary)
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|err| format!("{}: {err}", path.display()))?;
    file.write_all(bytes)
        .and_then(|()| file.flush())
        .map_err(|err| format!("{}: {err}", path.display()))
}

fn decode(text: &str) -> Result<Vec<u8>, String> {
    let mut lines = text.lines();
    if !lines
        .next()
        .is_some_and(|line| line.contains(COREDUMP_START))
    {
        return Err("missing coredump start marker".into());
    }
    let mut encoded = String::new();
    let mut complete = false;
    for line in lines {
        if line.contains(COREDUMP_END) {
            complete = true;
            break;
        }
        encoded.extend(line.chars().filter(|ch| !ch.is_ascii_whitespace()));
    }
    if !complete {
        return Err("incomplete coredump (missing end marker)".into());
    }
    if encoded.is_empty() {
        return Err("empty coredump".into());
    }
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|err| format!("invalid coredump base64: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunked_uart_capture_decodes_to_original_binary() {
        let mut scanner = super::super::panic::Scanner::new();
        let input = format!("{COREDUMP_START}\r\nAAEC\r\n/w==\r\n{COREDUMP_END}\r\n");
        let mut findings = Vec::new();
        for chunk in input.as_bytes().chunks(7) {
            findings.extend(scanner.feed(chunk));
        }
        let super::super::panic::Finding::Coredump(text) = &findings[0] else {
            panic!("expected coredump");
        };
        assert_eq!(decode(text).unwrap(), [0, 1, 2, 255]);
    }

    #[test]
    fn invalid_or_incomplete_capture_never_becomes_binary() {
        assert!(
            decode(&format!("{COREDUMP_START}\nAAEC"))
                .unwrap_err()
                .contains("incomplete")
        );
        assert!(
            decode(&format!("{COREDUMP_START}\n@@@\n{COREDUMP_END}"))
                .unwrap_err()
                .contains("base64")
        );
        assert!(
            decode(&format!("{COREDUMP_START}\n{COREDUMP_END}"))
                .unwrap_err()
                .contains("empty")
        );
    }

    #[test]
    fn saved_binary_matches_capture_and_bad_capture_is_preserved() {
        let directory = std::env::temp_dir().join(format!(
            "gtui-coredump-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        std::fs::create_dir(&directory).unwrap();
        let input = format!("{COREDUMP_START}\nAAEC/w==\n{COREDUMP_END}");
        let binary = save(&input, &directory).unwrap();
        assert_eq!(std::fs::read(&binary).unwrap(), [0, 1, 2, 255]);
        assert_eq!(
            std::fs::read_to_string(binary.with_extension("txt")).unwrap(),
            input
        );
        let error = save("truncated", &directory).unwrap_err();
        assert!(error.contains("raw capture:"));
        let captures: Vec<_> = std::fs::read_dir(&directory)
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            captures
                .iter()
                .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "bin"))
                .count(),
            1
        );
        let raw: Vec<_> = captures
            .iter()
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "txt"))
            .map(|entry| std::fs::read_to_string(entry.path()).unwrap())
            .collect();
        assert!(raw.contains(&"truncated".to_string()));
        std::fs::remove_dir_all(directory).unwrap();
    }
}
