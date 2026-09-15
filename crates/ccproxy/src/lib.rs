//! cc-proxy — Rust rewrite of the Node proxy.
//! Behaviour contract: conformance/golden/*.json. Plan: RUST-REWRITE-PLAN.md.
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::todo,
    clippy::unimplemented,
    clippy::dbg_macro
)]
// Fixture assertions legitimately unwrap; the production denies above stay on.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects
    )
)]

pub mod catalog;
pub mod config;
pub mod generate;
pub mod log;
pub mod models;
pub mod ndjson;
pub mod server;
pub mod sse;
pub mod stream_body;
pub mod tool_arguments;
pub mod translate;
pub mod upstream;
pub mod usage;
pub mod validation;

use serde_json::{json, Value};
use std::time::{SystemTime, UNIX_EPOCH};

/// `{error: {message, type}}` — the OpenAI error envelope. `type` is always
/// `proxy_error`; upstream status codes never change it.
pub fn json_error(message: &str) -> Value {
    json!({"error": {"message": message, "type": "proxy_error"}})
}

/// Proxy version reported by `/health` (from Cargo.toml, the single source).
pub fn proxy_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

pub fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// ISO-8601 UTC with milliseconds (`2026-09-15T12:34:56.789Z`).
pub fn now_iso8601() -> String {
    let dur = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d,
        Err(_) => return "1970-01-01T00:00:00.000Z".into(),
    };
    let secs = dur.as_secs();
    let millis = dur.subsec_millis();
    let (y, mo, d, h, mi, s) = civil_from_unix(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{millis:03}Z")
}

/// `YYYY-MM-DD` in UTC, for the CC request `config.date`.
pub fn today_utc() -> String {
    let secs = now_epoch_secs();
    let (y, mo, d, _, _, _) = civil_from_unix(secs);
    format!("{y:04}-{mo:02}-{d:02}")
}

/// Days-from-civil (Howard Hinnant's algorithm).
// Arithmetic here stays inside the ranges the algorithm is defined over:
// days since epoch fits i64 for any representable wall clock, and the era
// constants are the algorithm's own. Bounds are locally provable, so the
// crate-wide arithmetic lint is waived for this one function.
#[expect(
    clippy::arithmetic_side_effects,
    reason = "civil-date arithmetic over bounded constants; see comment above"
)]
fn civil_from_unix(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let h = (rem / 3600) as u32;
    let mi = ((rem % 3600) / 60) as u32;
    let s = (rem % 60) as u32;

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d, h, mi, s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_formats_as_iso() {
        // 2026-09-15T00:00:00Z = 1789430400
        let (y, m, d, ..) = civil_from_unix(1_789_430_400);
        assert_eq!((y, m, d), (2026, 9, 15));
    }

    #[test]
    fn error_envelope_matches_golden_shape() {
        let v = json_error("Unauthorized");
        assert_eq!(v["error"]["message"], "Unauthorized");
        assert_eq!(v["error"]["type"], "proxy_error");
        assert!(v.get("type").is_none());
    }
}
