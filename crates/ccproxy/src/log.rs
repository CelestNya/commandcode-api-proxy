//! Level-filtered logger, mirroring src/logger.ts: debug/info to stdout,
//! warn/error to stderr; an unknown LOG_LEVEL warns once and falls back to info.

use std::io::Write;
use std::sync::atomic::{AtomicU8, Ordering};

pub const DEBUG: u8 = 0;
pub const INFO: u8 = 1;
pub const WARN: u8 = 2;
pub const ERROR: u8 = 3;

static LEVEL: AtomicU8 = AtomicU8::new(INFO);

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

fn enabled(level: u8) -> bool {
    LEVEL.load(Ordering::Relaxed) <= level
}

pub fn debug(msg: &str) {
    if enabled(DEBUG) {
        out("[DEBUG]", msg)
    }
}

pub fn info(msg: &str) {
    if enabled(INFO) {
        out("[INFO]", msg)
    }
}

pub fn warn(msg: &str) {
    if enabled(WARN) {
        err("[WARN]", msg)
    }
}

pub fn error(msg: &str) {
    if enabled(ERROR) {
        err("[ERROR]", msg)
    }
}

fn out(tag: &str, msg: &str) {
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let _ = writeln!(lock, "{tag} {msg}");
}

fn err(tag: &str, msg: &str) {
    let stderr = std::io::stderr();
    let mut lock = stderr.lock();
    let _ = writeln!(lock, "{tag} {msg}");
}
