//! Wall-clock arithmetic in one place.
//!
//! `now_epoch_secs` and the Hinnant days-from-civil inverse used to live as two
//! copies — one in `lib.rs` (returning a tuple) and one in `billing.rs`
//! (formatting straight to a string without millis). One implementation here is
//! the single source for both, so the two formats cannot drift again.

use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds since the Unix epoch, `0` on clock failure.
pub fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The current UTC time as `2026-09-15T12:34:56.789Z`, the format the Node
/// build's `new Date().toISOString()` produces.
pub fn now_iso8601() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let millis = u64::from(now.subsec_millis());
    format!("{}.{millis:03}Z", iso8601_secs(now.as_secs()))
}

/// `YYYY-MM-DD` in UTC, for the CC request `config.date`.
pub fn today_utc() -> String {
    let (y, mo, d, _, _, _) = civil_from_unix(now_epoch_secs());
    format!("{y:04}-{mo:02}-{d:02}")
}

/// Seconds since the epoch as `2026-09-15T12:34:56` — the lexicographic
/// timestamp the billing ledger stores for its SQLite cutoffs.
pub(crate) fn iso8601_secs(epoch_secs: u64) -> String {
    let (y, mo, d, h, mi, s) = civil_from_unix(epoch_secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}")
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
    fn known_epochs_map_to_civil_dates() {
        assert_eq!(
            civil_from_unix(0),
            (1970, 1, 1, 0, 0, 0),
            "the epoch itself"
        );
        assert_eq!(
            civil_from_unix(86_400),
            (1970, 1, 2, 0, 0, 0),
            "one day later"
        );
        // 2000-02-29T00:00:00Z — a leap day, the case naive day-counts get wrong.
        assert_eq!(civil_from_unix(951_782_400), (2000, 2, 29, 0, 0, 0));
        // 2023-11-14T22:13:20Z — the canonical `1700000000`.
        assert_eq!(civil_from_unix(1_700_000_000), (2023, 11, 14, 22, 13, 20));
    }

    #[test]
    fn iso8601_secs_is_the_lexicographic_shape() {
        assert_eq!(iso8601_secs(1_789_430_400), "2026-09-15T00:00:00");
        assert_eq!(iso8601_secs(1_700_000_000), "2023-11-14T22:13:20");
    }

    #[test]
    fn now_iso8601_has_three_millis_and_a_z() {
        let s = now_iso8601();
        assert_eq!(s.len(), 24, "timestamp={s:?}");
        assert!(s.ends_with('Z'), "timestamp={s:?}");
        // `2026-09-15T12:34:56.789Z` — the millis sit between the `.` and `Z`.
        assert_eq!(s.as_bytes().get(19), Some(&b'.'), "timestamp={s:?}");
        assert!(
            s.as_bytes()[20..23].iter().all(|b| b.is_ascii_digit()),
            "timestamp={s:?}"
        );
    }

    #[test]
    fn today_utc_is_the_date_prefix() {
        let date = today_utc();
        assert_eq!(date.len(), 10, "date={date:?}");
        assert!(now_iso8601().starts_with(&date), "date={date:?}");
    }
}
