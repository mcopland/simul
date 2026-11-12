use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result};

/// Size at which the log moves to `<name>.1`, replacing the previous one, so a daemon that
/// runs for weeks keeps at most twice this on disk.
const MAX_BYTES: u64 = 1 << 20;

static SINK: OnceLock<Mutex<Sink>> = OnceLock::new();

/// An append-only log file that rotates once it reaches `cap` bytes.
struct Sink {
    path: PathBuf,
    file: File,
    size: u64,
    cap: u64,
}

impl Sink {
    fn open(path: &Path, cap: u64) -> Result<Sink> {
        let file = open_append(path)?;
        let size = file
            .metadata()
            .with_context(|| format!("failed to stat daemon log: {}", path.display()))?
            .len();
        let mut sink = Sink {
            path: path.to_owned(),
            file,
            size,
            cap,
        };
        if sink.size >= cap {
            sink.rotate()?;
        }
        Ok(sink)
    }

    fn rotated_path(&self) -> PathBuf {
        let mut name = self.path.file_name().unwrap_or_default().to_owned();
        name.push(".1");
        self.path.with_file_name(name)
    }

    fn rotate(&mut self) -> Result<()> {
        let old = self.rotated_path();
        std::fs::rename(&self.path, &old).with_context(|| {
            format!(
                "failed to rotate daemon log {} to {}",
                self.path.display(),
                old.display()
            )
        })?;
        self.file = open_append(&self.path)?;
        self.size = 0;
        Ok(())
    }

    fn write(&mut self, line: &[u8]) -> Result<()> {
        let len = line.len() as u64;
        if self.size > 0
            && self.size + len > self.cap
            && let Err(e) = self.rotate()
        {
            // Stderr is /dev/null for a detached daemon, so the current file, still open,
            // takes the error and later lines; the next try waits for another cap's worth.
            let note = format!("{} {e:#}\n", crate::protocol::now_ms() / 1000);
            self.size = 0;
            self.file
                .write_all(note.as_bytes())
                .with_context(|| format!("failed to write daemon log: {}", self.path.display()))?;
        }
        self.file
            .write_all(line)
            .with_context(|| format!("failed to write daemon log: {}", self.path.display()))?;
        self.size += len;
        Ok(())
    }
}

fn open_append(path: &Path) -> Result<File> {
    OpenOptions::new()
        .mode(0o600)
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open daemon log: {}", path.display()))
}

/// Sends daemon log lines to `path` instead of stderr, which is /dev/null when detached.
pub fn init(path: &Path) -> Result<()> {
    let sink = Sink::open(path, MAX_BYTES)?;
    // A second init keeps the first sink; that is fine because there is one daemon per process.
    let _ = SINK.set(Mutex::new(sink));
    Ok(())
}

pub fn write(msg: &str) {
    let line = format!("{} {msg}\n", crate::protocol::now_ms() / 1000);
    if let Some(Ok(mut sink)) = SINK.get().map(Mutex::lock) {
        match sink.write(line.as_bytes()) {
            Ok(()) => return,
            // Nowhere else to report it; stderr is the fallback below either way.
            Err(e) => eprintln!("{e:#}"),
        }
    }
    eprint!("{line}");
}

#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => { $crate::log::write(&format!($($arg)*)) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn size(path: &Path) -> u64 {
        std::fs::metadata(path).unwrap().len()
    }

    #[test]
    fn rotates_at_the_cap_keeping_one_old_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.log");
        let mut sink = Sink::open(&path, 100).unwrap();
        let line = [b'a'; 30];
        for _ in 0..10 {
            sink.write(&line).unwrap();
            assert!(size(&path) <= 100, "{}", size(&path));
        }
        let old = dir.path().join("x.log.1");
        assert_eq!(size(&old), 90, "the previous file is kept whole");
        let names: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(names.len(), 2, "one rotated generation only");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn an_oversized_log_rotates_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.log");
        std::fs::write(&path, [b'a'; 150]).unwrap();
        let mut sink = Sink::open(&path, 100).unwrap();
        sink.write(b"new\n").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new\n");
        assert_eq!(size(&dir.path().join("x.log.1")), 150);
    }

    #[test]
    fn a_failed_rotation_keeps_logging_to_the_current_file() {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("logs");
        std::fs::create_dir(&logs).unwrap();
        let path = logs.join("x.log");
        let mut sink = Sink::open(&path, 50).unwrap();
        sink.write(&[b'a'; 48]).unwrap();
        // A read-only directory refuses the rename.
        std::fs::set_permissions(&logs, std::fs::Permissions::from_mode(0o500)).unwrap();
        let result = sink.write(b"after\n");
        let later = sink.write(b"later\n");
        std::fs::set_permissions(&logs, std::fs::Permissions::from_mode(0o700)).unwrap();
        result.unwrap();
        later.unwrap();
        let text = String::from_utf8(std::fs::read(&path).unwrap()).unwrap();
        assert!(text.contains("failed to rotate"), "{text}");
        assert!(text.ends_with("after\nlater\n"), "{text}");
        assert!(!logs.join("x.log.1").exists());
    }

    #[test]
    fn a_line_longer_than_the_cap_is_still_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.log");
        let mut sink = Sink::open(&path, 10).unwrap();
        sink.write(&[b'a'; 20]).unwrap();
        assert_eq!(size(&path), 20);
    }
}
