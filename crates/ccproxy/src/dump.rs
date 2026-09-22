//! Full-fidelity dumps of failed requests.
//!
//! When a request fails, the log line only carries a short reason. The cause
//! usually lives in the *request body* — which model, which tools, how the
//! system prompt was assembled — so a failure dumps the body the proxy actually
//! sent upstream, one file per failure, keyed by request id.
//!
//! Two deliberate limits keep this from becoming a disk or privacy problem:
//! the body is capped ([`MAX_BODY_BYTES`]), and the directory keeps only the
//! newest [`MAX_FILES`] dumps. Dumps contain the user's conversation content,
//! so they are a diagnostic artifact, not a permanent record.

use std::path::{Path, PathBuf};

/// Per-file cap. A body past this is truncated with a marker rather than
/// dropped: a partial body still answers "what shape was it".
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;
/// How many dumps to keep. Old ones are deleted oldest-first.
const MAX_FILES: usize = 50;

/// The directory dumps live in: `<log dir>/dumps`, i.e. beside the log file
/// but in its own folder so the two are never confused.
#[must_use]
pub fn dump_dir_for(log_path: &Path) -> PathBuf {
    log_path
        .parent()
        .map_or_else(|| PathBuf::from("dumps"), |p| p.join("dumps"))
}

/// Write one failure dump and prune old ones.
///
/// `request_id` names the file; `meta` is a short header (one `key: value` per
/// line) and `body` the full request body. Best-effort: a failure to write must
/// never affect the response the client gets, so every error is swallowed.
pub fn write(dump_dir: &Path, request_id: &str, meta: &[(&str, String)], body: &str) {
    let _ = std::fs::create_dir_all(dump_dir);
    let path = dump_dir.join(format!("{}.txt", sanitize(request_id)));

    let mut out = String::new();
    out.push_str(&format!("time: {}\n", crate::time::now_iso8601()));
    for (k, v) in meta {
        out.push_str(&format!("{k}: {v}\n"));
    }
    out.push_str("--- request body ---\n");
    if body.len() > MAX_BODY_BYTES {
        // Cut on a char boundary; the marker records how much was dropped.
        let cut = floor_char_boundary(body, MAX_BODY_BYTES);
        out.push_str(&body[..cut]);
        out.push_str(&format!(
            "\n--- TRUNCATED: {} of {} bytes shown ---\n",
            cut,
            body.len()
        ));
    } else {
        out.push_str(body);
        out.push('\n');
    }

    if std::fs::write(&path, out).is_err() {
        return;
    }
    prune(dump_dir);
}

/// Delete the oldest dumps once the cap is exceeded. Sorted by file name, which
/// begins with the timestamp-shaped request id — good enough for pruning, and
/// it never touches anything but `.txt` files this module wrote.
fn prune(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "txt"))
        .collect();
    if files.len() <= MAX_FILES {
        return;
    }
    files.sort();
    let excess = files.len().saturating_sub(MAX_FILES);
    for path in files.into_iter().take(excess) {
        let _ = std::fs::remove_file(path);
    }
}

/// Keep only characters safe in a file name. A request id is a uuid in
/// practice; this guards against anything else (a path separator would let a
/// crafted id escape the directory).
fn sanitize(id: &str) -> String {
    let cleaned: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(120)
        .collect();
    if cleaned.is_empty() {
        "unknown".to_string()
    } else {
        cleaned
    }
}

/// The largest byte index <= `max` that lies on a char boundary.
// `i` only ever decreases toward 0, and the loop stops there, so the
// subtraction cannot underflow.
#[expect(
    clippy::arithmetic_side_effects,
    reason = "i is bounded below by the loop condition i > 0"
)]
fn floor_char_boundary(s: &str, max: usize) -> usize {
    if max >= s.len() {
        return s.len();
    }
    let mut i = max;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dump_is_written_with_its_metadata() {
        let dir = std::env::temp_dir().join("ccproxy-dump-test-1");
        let _ = std::fs::remove_dir_all(&dir);
        write(
            &dir,
            "req-123",
            &[("model", "m".to_string()), ("status", "502".to_string())],
            "{\"params\":{}}",
        );
        let text = std::fs::read_to_string(dir.join("req-123.txt")).expect("dump written");
        assert!(text.contains("model: m"), "{text}");
        assert!(text.contains("status: 502"), "{text}");
        assert!(text.contains("--- request body ---"), "{text}");
        assert!(text.contains("{\"params\":{}}"), "{text}");
    }

    #[test]
    fn a_request_id_cannot_escape_the_directory() {
        // A path separator in the id must not become a path separator on disk.
        assert!(!sanitize("../../etc/passwd").contains('/'));
        assert!(!sanitize("..\\..\\win").contains('\\'));
        assert_eq!(sanitize(""), "unknown");
        assert_eq!(sanitize("5f0c-abc_1"), "5f0c-abc_1");
    }

    #[test]
    fn prune_keeps_only_the_newest_files() {
        let dir = std::env::temp_dir().join("ccproxy-dump-test-2");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Names sort oldest-first, matching how prune orders them.
        for i in 0..(MAX_FILES + 5) {
            std::fs::write(dir.join(format!("{i:04}.txt")), "x").unwrap();
        }
        write(&dir, "zzzz-latest", &[], "body");
        let count = std::fs::read_dir(&dir).unwrap().count();
        assert!(count <= MAX_FILES, "kept {count} files");
        // The oldest are the ones dropped.
        assert!(!dir.join("0000.txt").exists());
        assert!(dir.join("zzzz-latest.txt").exists());
    }

    #[test]
    fn an_oversize_body_is_truncated_on_a_char_boundary() {
        // A multi-byte char straddling the cap must not panic the slice.
        let body = "日".repeat(MAX_BODY_BYTES); // 3 bytes each, far past the cap
        let dir = std::env::temp_dir().join("ccproxy-dump-test-3");
        let _ = std::fs::remove_dir_all(&dir);
        write(&dir, "big", &[], &body);
        let text = std::fs::read_to_string(dir.join("big.txt")).unwrap();
        assert!(text.contains("TRUNCATED"), "no truncation marker");
        assert!(text.len() < body.len(), "not actually truncated");
    }

    #[test]
    fn the_dump_directory_sits_beside_the_log() {
        let log = Path::new("/somewhere/logs/proxy.log");
        assert_eq!(dump_dir_for(log), PathBuf::from("/somewhere/logs/dumps"));
    }
}
