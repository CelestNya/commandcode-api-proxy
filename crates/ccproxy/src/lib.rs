//! cc-proxy — Rust rewrite of the Node proxy.
//! Behaviour contract: RUST-REWRITE-SPEC.md + ADEVIATIONS.md, enforced by the
//! test suites (fixture replay + integration).
//! Behaviour contract: conformance/golden/*.json. Plan: docs/RUST-REWRITE-PLAN.md.
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

pub mod billing;
pub mod catalog;
pub mod cli_version;
pub mod config;
pub mod dump;
pub mod generate;
pub mod log;
pub mod models;
pub mod ndjson;
pub mod pricing;
pub mod proxy;
pub mod server;
pub mod sse;
pub mod stream_body;
pub mod time;
pub mod tool_arguments;
pub mod translate;
pub mod upstream;
pub mod usage;
pub mod validation;
pub mod webui;
pub mod webui_site;

use serde_json::{json, Value};

/// `{error: {message, type}}` — the OpenAI error envelope. `type` is always
/// `proxy_error`; upstream status codes never change it.
pub fn json_error(message: &str) -> Value {
    json!({"error": {"message": message, "type": "proxy_error"}})
}

/// Proxy version reported by `/health` (from Cargo.toml, the single source).
pub fn proxy_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

pub use time::{now_epoch_secs, now_iso8601, today_utc};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_envelope_matches_golden_shape() {
        let v = json_error("Unauthorized");
        assert_eq!(v["error"]["message"], "Unauthorized");
        assert_eq!(v["error"]["type"], "proxy_error");
        assert!(v.get("type").is_none());
    }
}
