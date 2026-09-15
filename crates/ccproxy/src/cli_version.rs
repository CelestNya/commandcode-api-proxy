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

#[derive(Deserialize)]
struct NpmPackage {
    version: Option<String>,
}

/// The latest published `command-code` version, or `None` on any failure.
///
/// Failure is silent by design: an offline machine must still start, and the
/// fallback version is a working value.
pub fn fetch_latest_cli_version() -> Option<String> {
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
