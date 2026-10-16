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
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};

pub const DEBUG: u8 = 0;
pub const INFO: u8 = 1;
pub const WARN: u8 = 2;
pub const ERROR: u8 = 3;

/// Rotation limit for the proxy's own log (UX 0.7.0 D8: size-based FIFO).
/// Three generations are kept: current + `.1` + `.2`, older files deleted.
pub const PROXY_LOG_LIMIT_BYTES: u64 = 10 * 1024 * 1024;

static LEVEL: AtomicU8 = AtomicU8::new(INFO);
/// The proxy's own log file, opened by [`init_file`]. `None` until then (and
/// forever in tests and in any embedder that does not want a file).
static FILE: OnceLock<Mutex<Option<LogTarget>>> = OnceLock::new();

/// One open log file with the path and rotation limit it was opened with.
/// `file` is `None` only during rotation (the handle must be closed before
/// Windows lets the file be renamed).
struct LogTarget {
    file: Option<std::fs::File>,
    path: std::path::PathBuf,
    limit: u64,
}

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

/// Point the logger at `path`, appending, rotating at `limit` bytes (FIFO,
/// three generations: current + `.1` + `.2`, older deleted). A failure is
/// reported on the console but never fatal: losing the log file must not
/// stop the proxy from serving.
pub fn init_file(path: &std::path::Path, limit: u64) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        Ok(file) => {
            let _ = FILE.set(Mutex::new(Some(LogTarget {
                file: Some(file),
                path: path.to_path_buf(),
                limit,
            })));
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

/// Shift `path` → `path.1` → `path.2`, dropping the oldest generation. The
/// caller must already have closed the live handle (required on Windows,
/// where an open file cannot be renamed).
fn rotate(path: &std::path::Path) {
    let p1 = rot_path(path, 1);
    let p2 = rot_path(path, 2);
    let _ = std::fs::remove_file(&p2);
    let _ = std::fs::rename(&p1, &p2);
    let _ = std::fs::rename(path, &p1);
}

/// `path` with a numeric suffix appended (`log.txt` → `log.txt.1`), so the
/// extension stays intact for the WebUI's log reader.
fn rot_path(path: &std::path::Path, n: u32) -> std::path::PathBuf {
    let mut s = path.to_string_lossy().into_owned();
    s.push_str(&format!(".{n}"));
    std::path::PathBuf::from(s)
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
    if let Some(lock) = FILE.get() {
        if let Ok(mut target) = lock.lock() {
            if let Some(t) = target.as_mut() {
                if let Some(file) = t.file.as_mut() {
                    // Size-based rotation: once the live file reaches the
                    // limit, drop the handle, shift generations and reopen.
                    let too_big = file.metadata().map(|m| m.len() >= t.limit).unwrap_or(false);
                    if too_big {
                        t.file = None; // close the handle so rename can proceed
                        rotate(&t.path);
                        if let Ok(f) = std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(&t.path)
                        {
                            t.file = Some(f);
                        }
                    }
                    if let Some(f) = t.file.as_mut() {
                        let _ = writeln!(f, "{line}");
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialises the tests that touch the process-global `LEVEL`. Without it
    /// they race: one test's mid-flight `store(WARN)` can land inside another's
    /// `init`-then-assert window and the suite flakes (observed 2026-09-26).
    static LEVEL_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn the_level_gate_admits_everything_at_or_above_it() {
        let _guard = LEVEL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
        let _guard = LEVEL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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

    #[test]
    fn rotation_keeps_generations_bounded() {
        // A tiny limit forces several rotations; the live file stays near the
        // limit and the shifted generations survive in FIFO order.
        let dir = std::env::temp_dir().join(format!("cc-proxy-log-rot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("proxy.log");
        init_file(&path, 128);
        // ~80 chars + stamp/tag ≈ 110 bytes per line: two lines cross 128.
        for _ in 0..600 {
            info("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcd");
        }
        let live = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        assert!(
            live < 2 * 128,
            "live={live} must stay under two lines of the limit"
        );
        let p1 = std::fs::metadata(rot_path(&path, 1))
            .map(|m| m.len())
            .unwrap_or(0);
        assert!(p1 > 0, "one rotated generation must exist");
        // The third generation exists too (FIFO shift ran many times).
        let p2 = std::fs::metadata(rot_path(&path, 2))
            .map(|m| m.len())
            .unwrap_or(0);
        assert!(p2 > 0, "second rotated generation must exist");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
