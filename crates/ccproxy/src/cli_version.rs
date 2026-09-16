//! Startup refresh of the advertised CLI version.
//!
//! Ported from `fetchLatestCliVersion()` in the Node `src/config.ts`. CC's
//! server blocks requests carrying a stale or absent `x-command-code-version`,
//! so the constant in `config.rs` is only a fallback: the published version is
//! consulted once at startup, and any failure leaves the fallback in place.
//!
//! Deliberately *not* a background refresher. The Node build caches for 24 h
//! but only ever calls this from the startup path, so within one process the
//! cache never expires — matching that means one fetch per process, before the
//! listener comes up.

use serde::Deserialize;
use std::time::Duration;

const NPM_LATEST_URL: &str = "https://registry.npmjs.org/command-code/latest";
/// The Node build bounds the whole lookup with `AbortSignal.timeout(10_000)`.
const FETCH_TIMEOUT_MS: u64 = 10_000;

/// How many times the lookup is attempted before giving up.
///
/// One attempt is not enough here, and this is a deliberate departure from the
/// Node build's single `fetch`. That fetch inherits undici's Happy Eyeballs,
/// which races IPv4 against IPv6 and keeps whichever answers first; ureq 2
/// instead takes the resolver's first answer. On a machine whose IPv6 path is
/// intercepted or partially broken — the usual case behind a local TUN-mode
/// proxy, which is also what rewrites this registry to a fake-IP address —
/// ureq fails a fraction of lookups that undici sails through. Measured on such
/// a machine: ureq succeeded 17/20 and 18/20 against this URL even with a
/// shared pooled agent, while undici succeeded 10/10.
///
/// The value is not cosmetic: CC blocks a stale `x-command-code-version`, so a
/// failed lookup silently pins the whole process to the `0.40.3` fallback. One
/// retry takes a ~10% failure rate down to ~1%.
const FETCH_ATTEMPTS: usize = 2;

#[derive(Deserialize)]
struct NpmPackage {
    version: Option<String>,
}

/// The latest published `command-code` version, or `None` on any failure.
///
/// Failure is silent by design: an offline machine must still start, and the
/// fallback version is a working value.
pub fn fetch_latest_cli_version() -> Option<String> {
    with_retries(fetch_once)
}

/// Retry a lookup until it succeeds or the attempts are spent.
///
/// Split out from [`fetch_latest_cli_version`] so the retry policy can be
/// tested without a network.
fn with_retries(fetch: impl Fn() -> Option<String>) -> Option<String> {
    for attempt in 1..=FETCH_ATTEMPTS {
        if let Some(version) = fetch() {
            return Some(version);
        }
        if attempt < FETCH_ATTEMPTS {
            crate::log::debug(&format!(
                "CLI version lookup attempt {attempt} failed; retrying"
            ));
        }
    }
    None
}

/// One lookup attempt. `None` on any failure, including a malformed reply.
fn fetch_once() -> Option<String> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_millis(FETCH_TIMEOUT_MS))
        .build();
    let response = agent.get(NPM_LATEST_URL).call().ok()?;
    let text = response.into_string().ok()?;
    let parsed: NpmPackage = serde_json::from_str(&text).ok()?;
    // `typeof pkg.version === "string"` also accepts "", which is not a version.
    parsed.version.filter(|v| !v.is_empty())
}

/// Which version to advertise: an explicit pin always wins, otherwise the
/// fetched value, otherwise the fallback.
///
/// The fetch is injected so the decision can be tested without the network.
pub fn resolve_cli_version(
    configured: Option<&str>,
    fallback: &str,
    fetch: impl FnOnce() -> Option<String>,
) -> String {
    match configured.filter(|v| !v.is_empty()) {
        Some(pinned) => pinned.to_owned(),
        None => fetch().unwrap_or_else(|| fallback.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_explicit_version_skips_the_lookup_entirely() {
        let v = resolve_cli_version(Some("9.9.9"), "0.40.3", || {
            panic!("a pinned version must not reach the network")
        });
        assert_eq!(v, "9.9.9");
    }

    #[test]
    fn an_empty_pin_is_not_a_pin() {
        // An empty CC_CLI_VERSION means "unset" in the Node build too, where
        // `process.env.CC_CLI_VERSION` is falsy.
        let v = resolve_cli_version(Some(""), "0.40.3", || Some("1.2.3".into()));
        assert_eq!(v, "1.2.3");
    }

    #[test]
    fn a_successful_lookup_wins_over_the_fallback() {
        let v = resolve_cli_version(None, "0.40.3", || Some("1.2.3".into()));
        assert_eq!(v, "1.2.3");
    }

    #[test]
    fn a_failed_lookup_leaves_the_fallback() {
        let v = resolve_cli_version(None, "0.40.3", || None);
        assert_eq!(v, "0.40.3");
    }

    #[test]
    fn a_lookup_that_fails_once_is_retried() {
        // The reason the retry exists: ureq picks the resolver's first address
        // where undici races IPv4 and IPv6, so single lookups fail on a machine
        // with a broken IPv6 path. The second attempt is what saves it.
        let calls = std::cell::Cell::new(0);
        let v = with_retries(|| {
            calls.set(calls.get() + 1);
            if calls.get() == 1 {
                None
            } else {
                Some("1.2.3".into())
            }
        });
        assert_eq!(v, Some("1.2.3".into()));
        assert_eq!(calls.get(), 2, "the second attempt must have happened");
    }

    #[test]
    fn a_lookup_that_always_fails_stops_after_the_attempt_budget() {
        let calls = std::cell::Cell::new(0);
        let v = with_retries(|| {
            calls.set(calls.get() + 1);
            None
        });
        assert_eq!(v, None);
        assert_eq!(calls.get(), FETCH_ATTEMPTS);
    }

    #[test]
    fn a_lookup_that_succeeds_immediately_is_not_retried() {
        let calls = std::cell::Cell::new(0);
        let v = with_retries(|| {
            calls.set(calls.get() + 1);
            Some("1.2.3".into())
        });
        assert_eq!(v, Some("1.2.3".into()));
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn an_empty_published_version_is_not_a_version() {
        let parsed: NpmPackage = serde_json::from_str(r#"{"version":""}"#).expect("parses");
        assert!(parsed.version.filter(|v| !v.is_empty()).is_none());
    }

    #[test]
    fn a_missing_version_field_parses_as_none() {
        let parsed: NpmPackage = serde_json::from_str("{}").expect("parses");
        assert!(parsed.version.is_none());
    }
}
