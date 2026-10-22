use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result};

static SINK: OnceLock<Mutex<File>> = OnceLock::new();

/// Sends daemon log lines to `path` instead of stderr, which is /dev/null when detached.
pub fn init(path: &Path) -> Result<()> {
    let file = OpenOptions::new()
        .mode(0o600)
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open daemon log: {}", path.display()))?;
    // A second init keeps the first sink; that is fine because there is one daemon per process.
    let _ = SINK.set(Mutex::new(file));
    Ok(())
}

pub fn write(msg: &str) {
    let line = format!("{} {msg}\n", crate::protocol::now_ms() / 1000);
    if let Some(Ok(mut file)) = SINK.get().map(Mutex::lock)
        && file.write_all(line.as_bytes()).is_ok()
    {
        return;
    }
    eprint!("{line}");
}

#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => { $crate::log::write(&format!($($arg)*)) };
}
