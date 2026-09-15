// cc-proxy — Rust rewrite of the Node proxy. Behaviour contract:
// conformance/golden/*.json. Plan: RUST-REWRITE-PLAN.md.
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
// Tests assert against fixture JSON and legitimately use unwrap/panic-style
// assertions; the production denies above stay in force.
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

fn main() {
    // Wired up in M1: config load, logger, tiny_http server.
}
