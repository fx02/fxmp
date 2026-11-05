//! The log, written to the standard error output and, once the app knows its
//! folders, also to `fxmp.log` in the log folder (macOS `~/Library/Logs/hr.pvt.fxmp`,
//! Windows `%LOCALAPPDATA%\hr.pvt.fxmp\logs`; a debug build with `SMP_DATA_DIR`:
//! `logs` in that folder). Once the file passes [`MAX_BYTES`] it becomes
//! `fxmp.old.log` (replacing the older one) and a new file starts, so the two
//! never take more than twice that.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub const FILE_NAME: &str = "fxmp.log";
pub const OLD_FILE_NAME: &str = "fxmp.old.log";
/// Size at which the log file is rotated.
pub const MAX_BYTES: u64 = 2 * 1024 * 1024;

struct LogFile {
    file: File,
    path: PathBuf,
    written: u64,
    max: u64,
}

impl LogFile {
    fn open(dir: &Path, max: u64) -> io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(FILE_NAME);
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let written = file.metadata()?.len();
        let mut log = Self {
            file,
            path,
            written,
            max,
        };
        if log.written >= max {
            log.rotate()?;
        }
        Ok(log)
    }

    fn rotate(&mut self) -> io::Result<()> {
        let old = self.path.with_file_name(OLD_FILE_NAME);
        // Windows cannot rename over an existing file.
        let _ = std::fs::remove_file(&old);
        std::fs::rename(&self.path, &old)?;
        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        self.written = 0;
        Ok(())
    }

    fn write(&mut self, buf: &[u8]) {
        if self.written >= self.max && self.rotate().is_err() {
            // Keep appending rather than lose the log.
            self.written = 0;
        }
        if self.file.write_all(buf).is_ok() {
            self.written += buf.len() as u64;
        }
    }
}

/// Where log lines go: stderr always, the file once [`LogSink::attach`]ed.
#[derive(Clone, Default)]
pub struct LogSink(Arc<Mutex<Option<LogFile>>>);

impl LogSink {
    /// Starts writing to `fxmp.log` in `dir` as well.
    pub fn attach(&self, dir: &Path) -> io::Result<PathBuf> {
        self.attach_with(dir, MAX_BYTES)
    }

    fn attach_with(&self, dir: &Path, max: u64) -> io::Result<PathBuf> {
        let log = LogFile::open(dir, max)?;
        let path = log.path.clone();
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(log);
        Ok(path)
    }
}

impl Write for LogSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let _ = io::stderr().write_all(buf);
        if let Some(f) = self.0.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            f.write(buf);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(f) = self.0.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            let _ = f.file.flush();
        }
        io::stderr().flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fxmp-log-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn lines_go_to_the_file_once_attached() {
        let dir = temp_dir("attach");
        let mut sink = LogSink::default();
        sink.write_all(b"before\n").unwrap();
        let path = sink.attach(&dir).unwrap();
        sink.write_all(b"after\n").unwrap();
        sink.flush().unwrap();
        assert_eq!(path, dir.join(FILE_NAME));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "after\n");
        // A new start appends.
        let mut again = LogSink::default();
        again.attach(&dir).unwrap();
        again.write_all(b"next start\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "after\nnext start\n"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_full_file_moves_to_the_old_one() {
        let dir = temp_dir("rotate");
        let mut sink = LogSink::default();
        sink.attach_with(&dir, 10).unwrap();
        sink.write_all(b"0123456789").unwrap();
        sink.write_all(b"second\n").unwrap();
        sink.write_all(b"0123456789").unwrap();
        sink.write_all(b"third\n").unwrap();
        let read = |n| std::fs::read_to_string(dir.join(n)).unwrap();
        assert_eq!(read(OLD_FILE_NAME), "second\n0123456789");
        assert_eq!(read(FILE_NAME), "third\n");
        // A start finding a full file rotates at once.
        std::fs::write(dir.join(FILE_NAME), "x".repeat(20)).unwrap();
        let mut sink = LogSink::default();
        sink.attach_with(&dir, 10).unwrap();
        sink.write_all(b"fresh\n").unwrap();
        assert_eq!(read(FILE_NAME), "fresh\n");
        assert_eq!(read(OLD_FILE_NAME), "x".repeat(20));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
