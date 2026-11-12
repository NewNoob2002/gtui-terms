//! Minimal `addr2line` wrapper: load one ELF and resolve collected addresses.

use std::path::Path;

pub struct Frame {
    pub function: Option<String>,
    pub file: Option<String>,
    pub line: Option<u32>,
}

impl Frame {
    pub fn render(&self) -> String {
        let location = match (&self.file, self.line) {
            (Some(file), Some(line)) => format!("{file}:{line}"),
            (Some(file), None) => file.clone(),
            _ => "??".to_string(),
        };
        match &self.function {
            Some(name) => format!("{name} at {location}"),
            None => location,
        }
    }
}

pub struct Symbols {
    loader: addr2line::Loader,
}

impl Symbols {
    pub fn load(path: &Path) -> Result<Self, String> {
        addr2line::Loader::new(path)
            .map(|loader| Self { loader })
            .map_err(|err| format!("cannot load {}: {err}", path.display()))
    }
    /// Best-effort: first frame with a location, plus inline frames after it.
    pub fn resolve(&self, address: u64) -> Vec<Frame> {
        let mut frames = Vec::new();
        let Ok(mut iter) = self.loader.find_frames(address) else {
            return frames;
        };
        while let Ok(Some(frame)) = iter.next() {
            let function = frame
                .function
                .as_ref()
                .and_then(|name| name.demangle().ok())
                .map(|name| name.into_owned());
            frames.push(Frame {
                function,
                file: frame
                    .location
                    .as_ref()
                    .and_then(|l| l.file.map(str::to_string)),
                line: frame.location.as_ref().and_then(|l| l.line),
            });
            if frames.len() == 4 {
                break;
            }
        }
        if frames.is_empty()
            && let Some(symbol) = self.loader.find_symbol(address)
        {
            frames.push(Frame {
                function: Some(symbol.to_string()),
                file: None,
                line: None,
            });
        }
        frames
    }
}

/// Looks for a firmware ELF the way `idf.py` leaves it behind.
pub fn discover() -> Option<std::path::PathBuf> {
    let mut candidates: Vec<(std::time::SystemTime, std::path::PathBuf)> = Vec::new();
    for dir in ["build", "."] {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("elf") {
                continue;
            }
            let modified = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            candidates.push((modified, path));
        }
    }
    candidates
        .into_iter()
        .max_by_key(|(modified, _)| *modified)
        .map(|(_, path)| path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// End-to-end proof that `resolve` returns file:line, using a binary the
    /// host C compiler builds with DWARF (skipped when gcc/nm are absent).
    #[test]
    fn resolves_real_symbol_from_compiled_binary() {
        let dir = std::env::temp_dir().join(format!("gtui-elf-{}", std::process::id()));
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let source = dir.join("hello.c");
        let binary = dir.join("hello.exe");
        let ok = std::fs::write(
            &source,
            "#include <stdio.h>\nint inner(int x) { return x * 3; }\nint main(void) { return inner(7); }\n",
        )
        .is_ok()
            && Command::new("gcc")
                .args(["-g", "-O0", "-o"])
                .arg(&binary)
                .arg(&source)
                .status()
                .map(|status| status.success())
                .unwrap_or(false);
        if !ok {
            eprintln!("gcc unavailable, skipping symbolization test");
            return;
        }
        let Ok(nm) = Command::new("nm").arg(&binary).output() else {
            return;
        };
        let table = String::from_utf8_lossy(&nm.stdout);
        let address = table
            .lines()
            .find_map(|line| line.trim_end().strip_suffix(" main"))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|hex| u64::from_str_radix(hex, 16).ok());
        let Some(address) = address else {
            eprintln!("nm produced no main symbol, skipping");
            return;
        };

        let symbols = Symbols::load(&binary).expect("binary loads");
        let frames = symbols.resolve(address);
        let first = frames.first().expect("main resolves");
        assert!(
            first
                .function
                .as_deref()
                .unwrap_or_default()
                .contains("main"),
            "{:?}",
            first.function
        );
        assert!(
            first
                .file
                .as_deref()
                .unwrap_or_default()
                .ends_with("hello.c"),
            "{:?}",
            first.file
        );
        assert_eq!(first.line, Some(3));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
