use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

/// Raw device bytes, written as they arrive; no newline translation.
pub struct LogFile {
    writer: BufWriter<File>,
    path: PathBuf,
}

impl LogFile {
    pub fn create(path: &Path) -> io::Result<Self> {
        let file = File::create(path)?;
        Ok(Self {
            writer: BufWriter::new(file),
            path: path.to_path_buf(),
        })
    }

    pub fn write(&mut self, bytes: &[u8]) {
        let _ = self.writer.write_all(bytes);
    }

    pub fn flush(&mut self) {
        let _ = self.writer.flush();
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for LogFile {
    fn drop(&mut self) {
        let _ = self.writer.flush();
    }
}
