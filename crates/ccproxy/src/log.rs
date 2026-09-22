//! Level-filtered logger.
//!
//! Four levels (`debug`/`info`/`warn`/`error`) filtered by `LOG_LEVEL`. Every
//! line carries a UTC timestamp and is written to a file as well as to the
//! console, so a proxy started by hand keeps a log without the tray's stdout
//! redirection.
//!
//! The console split is preserved: debug/info go to stdout, warn/error to
//! stderr, so a caller can still separate the streams. The *file* holds only
//! the proxy's own lines — the tray keeps its lifecycle lines in a different
//! file, so the two no longer interleave.

use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};

pub const DEBUG: u8 = 0;
pub const INFO: u8 = 1;
pub const WARN: u8 = 2;
pub const ERROR: u8 = 3;

static LEVEL: AtomicU8 = AtomicU8::new(INFO);
/// The proxy's own log file, opened by [`init_file`]. `None` until then (and
/// forever in tests and in any embedder that does not want a file).
static FILE: OnceLock<Mutex<std::fs::File>> = OnceLock::new();

pub fn init(level: &str) {
    let parsed = match level.to_ascii_lowercase().as_str() {
        "debug" => Some(DEBUG),
        "info" => Some(INFO),
        "warn" => Some(WARN),
        "error" => Some(ERROR),
        _ => None,
    };
    match parsed {
        Some(l) => LEVEL.store(l, Ordering::Relaxed),
        None => {
            LEVEL.store(INFO, Ordering::Relaxed);
            warn(&format!(
                "[WARN] Unknown LOG_LEVEL \"{level}\", falling back to \"info\""
            ));
        }
    }
}

/// Point the logger at `path`, appending. A failure is reported on the console
/// but never fatal: losing the log file must not stop the proxy from serving.
pub fn init_file(path: &Path) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        Ok(file) => {
            let _ = FILE.set(Mutex::new(file));
        }
        Err(err) => {
            // Goes to the console only; the file is what failed.
            let _ = writeln!(
                std::io::stderr(),
                "[WARN] cannot open log file {}: {err}",
                path.display()
            );
        }
    }
}

fn enabled(level: u8) -> bool {
    LEVEL.load(Ordering::Relaxed) <= level
}

pub fn debug(msg: &str) {
    if enabled(DEBUG) {
        emit("[DEBUG]", msg, false)
    }
}

pub fn info(msg: &str) {
    if enabled(INFO) {
        emit("[INFO]", msg, false)
    }
}

pub fn warn(msg: &str) {
    if enabled(WARN) {
        emit("[WARN]", msg, true)
    }
}

pub fn error(msg: &str) {
    if enabled(ERROR) {
        emit("[ERROR]", msg, true)
    }
}

/// `YYYY-MM-DDTHH:MM:SS.mmmZ`, the same shape the ledger stamps rows with.
fn timestamp() -> String {
    crate::time::now_iso8601()
}

/// Write one tagged line, stamped, to the console (split by severity) and to
/// the log file when one is open.
fn emit(tag: &str, msg: &str, to_stderr: bool) {
    let line = format!("{} {tag} {msg}", timestamp());
    // The console write is best-effort: a closed pipe (the tray may be gone)
    // must not panic, and the file write below still runs.
    if to_stderr {
        let stderr = std::io::stderr();
        let _ = writeln!(stderr.lock(), "{line}");
    } else {
        let stdout = std::io::stdout();
        let _ = writeln!(stdout.lock(), "{line}");
    }
    if let Some(file) = FILE.get() {
        if let Ok(mut f) = file.lock() {
            let _ = writeln!(f, "{line}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_level_gate_admits_everything_at_or_above_it() {
        let previous = LEVEL.load(Ordering::Relaxed);
        LEVEL.store(WARN, Ordering::Relaxed);
        assert!(!enabled(DEBUG));
        assert!(!enabled(INFO));
        assert!(enabled(WARN));
        assert!(enabled(ERROR));
        LEVEL.store(previous, Ordering::Relaxed);
    }

    #[test]
    fn an_unknown_level_falls_back_to_info() {
        let previous = LEVEL.load(Ordering::Relaxed);
        init("nonsense");
        assert_eq!(LEVEL.load(Ordering::Relaxed), INFO);
        LEVEL.store(previous, Ordering::Relaxed);
    }

    #[test]
    fn the_timestamp_is_the_ledger_shape() {
        let ts = timestamp();
        // 2026-09-22T01:02:03.456Z
        assert_eq!(ts.len(), 24, "ts={ts}");
        assert!(ts.ends_with('Z'), "ts={ts}");
        assert_eq!(ts.as_bytes().get(10), Some(&b'T'), "ts={ts}");
    }
}
