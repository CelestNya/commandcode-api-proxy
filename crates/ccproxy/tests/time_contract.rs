//! Contract of the shared `time` module, which owns all wall-clock arithmetic.
//!
//! These functions previously lived as two copies — `lib.rs` and `billing.rs`
//! each implemented `now_epoch_secs` and the Hinnant days-from-civil algorithm
//! (billing formatted it to a string without millis, lib returned a tuple).
//! The module makes one implementation the single source for both.

use ccproxy::time::{now_epoch_secs, now_iso8601, today_utc};

/// `now_iso8601` must look like `2026-09-15T12:34:56.789Z` — the shape the Node
/// build's `new Date().toISOString()` produces, which clients and the billing
/// ledger both depend on.
#[test]
fn now_iso8601_is_a_three_millis_zulu_timestamp() {
    let s = now_iso8601();
    assert_eq!(s.len(), 24, "timestamp={s:?}");
    assert!(s.ends_with('Z'), "timestamp={s:?}");
    let date = &s[..10];
    assert_eq!(date.len(), 10, "timestamp={s:?}");
    let mut it = date.split('-');
    let year: u32 = it.next().expect("year").parse().expect("year");
    let month: u32 = it.next().expect("month").parse().expect("month");
    let day: u32 = it.next().expect("day").parse().expect("day");
    assert!((2000..2100).contains(&year), "timestamp={s:?}");
    assert!((1..=12).contains(&month), "timestamp={s:?}");
    assert!((1..=31).contains(&day), "timestamp={s:?}");
    let time = &s[11..23];
    assert_eq!(time.len(), 12, "timestamp={s:?}");
    assert!(time
        .as_bytes()
        .iter()
        .all(|b| b.is_ascii_digit() || *b == b':' || *b == b'.'));
}

/// `today_utc` must be the `YYYY-MM-DD` prefix of `now_iso8601`.
#[test]
fn today_utc_is_the_date_prefix_of_now_iso8601() {
    let date = today_utc();
    assert_eq!(date.len(), 10, "date={date:?}");
    assert!(now_iso8601().starts_with(&date), "date={date:?}");
}

/// The epoch second count must be plausible for the current era and stable
/// across the module's callers.
#[test]
fn now_epoch_secs_is_in_the_current_era() {
    let now = now_epoch_secs();
    assert!(now > 1_700_000_000, "now={now}");
    assert!(now < 2_000_000_000, "now={now}");
}
