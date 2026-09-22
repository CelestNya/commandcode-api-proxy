//! Config loading: CLI flags > env vars > defaults, with the same
//! per-field fallbacks as the Node `loadConfig()` (src/config.ts).

use std::path::Path;

/// Hardcoded CLI version fallback. The real CLI ships frequent releases;
/// CC's server actively blocks requests whose version looks stale or absent,
/// so this must stay at or above the server's `minVersion`. It is only used
/// when the startup npm lookup fails — keep it current when the CLI bumps.
pub const DEFAULT_CC_VERSION: &str = "1.54.1";
pub const DEFAULT_CC_API_BASE: &str = "https://api.commandcode.ai";

const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_PORT: u64 = 8787;
const MAX_BODY_BYTES_DEFAULT: u64 = 10 * 1024 * 1024;
const MAX_BODY_BYTES_MAX: u64 = 50 * 1024 * 1024;
const TIMEOUT_MAX_MS: u64 = 30 * 60 * 1000;
/// Default window in which a request must produce its first byte upstream
/// before the attempt is discarded and re-sent.
const NO_OUTPUT_TIMEOUT_DEFAULT_MS: u64 = 30_000;
/// How many times one discarded-for-silence attempt may be re-sent before the
/// client is told. Counts re-sends, so 3 means the request goes out at most 4
/// times in total (the original plus three).
const NO_OUTPUT_RETRIES_DEFAULT: u64 = 3;
const NO_OUTPUT_RETRIES_MAX: u64 = 10;

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub cc_api_base: String,
    pub cc_version: String,
    pub log_level: String,
    pub cors_origin: String,
    /// Per-attempt deadline for upstream headers and any non-2xx error body.
    pub upstream_timeout_ms: u64,
    /// Max ms between consecutive chunks during streaming. 0 = disabled.
    pub idle_timeout_ms: u64,
    /// Max ms an attempt may produce *no bytes at all* before it is discarded
    /// and re-sent from scratch. Measured only before the first byte; once any
    /// byte has arrived, `idle_timeout_ms` governs instead. 0 = disabled.
    pub no_output_timeout_ms: u64,
    /// How many times a silent attempt is re-sent before the client is told.
    pub no_output_retries: u64,
    /// Maximum request body size in bytes (Content-Length pre-check + streaming guard).
    pub max_body_bytes: u64,
}

/// Environment lookup, abstracted for tests.
pub type EnvLookup<'a> = dyn Fn(&str) -> Option<String> + 'a;

pub fn load(argv: &[String], env: &EnvLookup) -> Config {
    let cli = parse_cli_args(argv);
    // A flag without a value parses as "true"; host/port treat that as missing.
    let cli_host = cli.host.as_deref().filter(|v| *v != "true");
    let cli_port = cli.port.as_deref().filter(|v| *v != "true");

    let env_host = env("HOST");
    let env_port = env("PORT");
    let host = parse_host(cli_host.or(env_host.as_deref()));
    let port = parse_port(cli_port.or(env_port.as_deref()), DEFAULT_PORT);
    let cc_api_base =
        non_empty(env("CC_API_BASE").as_deref()).unwrap_or_else(|| DEFAULT_CC_API_BASE.into());
    let cc_version =
        non_empty(env("CC_CLI_VERSION").as_deref()).unwrap_or_else(|| DEFAULT_CC_VERSION.into());
    let log_level = non_empty(env("LOG_LEVEL").as_deref()).unwrap_or_else(|| "info".into());
    // Empty string is meaningful: it disables CORS entirely.
    let cors_origin = env("CORS_ORIGIN").unwrap_or_else(|| "*".into());
    let max_body_bytes = parse_body_limit(env("CC_MAX_BODY_BYTES").as_deref());

    let raw_upstream = parse_positive_int(env("CC_UPSTREAM_TIMEOUT_MS").as_deref(), 600_000);
    let raw_idle = parse_positive_int(env("CC_IDLE_TIMEOUT_MS").as_deref(), 120_000);
    let upstream_timeout_ms = clamp_timeout(raw_upstream, TIMEOUT_MAX_MS, 600_000);
    let idle_timeout_ms = clamp_timeout(raw_idle, TIMEOUT_MAX_MS, 120_000);

    let raw_no_output = parse_positive_int(
        env("CC_NO_OUTPUT_TIMEOUT_MS").as_deref(),
        NO_OUTPUT_TIMEOUT_DEFAULT_MS,
    );
    let no_output_timeout_ms =
        clamp_timeout(raw_no_output, TIMEOUT_MAX_MS, NO_OUTPUT_TIMEOUT_DEFAULT_MS);
    let raw_no_output_retries = parse_non_negative_int(
        env("CC_NO_OUTPUT_RETRIES").as_deref(),
        NO_OUTPUT_RETRIES_DEFAULT,
    );
    let no_output_retries = raw_no_output_retries.min(NO_OUTPUT_RETRIES_MAX);

    Config {
        host,
        port: port as u16,
        cc_api_base,
        cc_version,
        log_level,
        cors_origin,
        upstream_timeout_ms,
        idle_timeout_ms,
        no_output_timeout_ms,
        no_output_retries,
        max_body_bytes,
    }
}

struct CliArgs {
    host: Option<String>,
    port: Option<String>,
}

/// Only `--host`/`--port` are meaningful; any other `--flag` is accepted and
/// ignored, matching the Node parseCliArgs (which collects and drops the rest).
/// A flag immediately followed by another `--flag` (or nothing) has no value.
fn parse_cli_args(args: &[String]) -> CliArgs {
    let mut out = CliArgs {
        host: None,
        port: None,
    };
    let mut iter = args.iter().peekable();
    while let Some(arg) = iter.next() {
        let Some(key) = arg.strip_prefix("--") else {
            continue;
        };
        if key.is_empty() {
            continue;
        }
        // A value is anything that does not itself look like a flag.
        let takes_value = iter.peek().is_some_and(|n| !n.starts_with("--"));
        let value = if takes_value {
            iter.next().map(String::as_str)
        } else {
            // Flag without a value: Node stores boolean true; host/port treat
            // that as missing.
            Some("true")
        };
        match key {
            "host" => out.host = value.map(str::to_owned),
            "port" => out.port = value.map(str::to_owned),
            _ => {}
        }
    }
    out
}

fn non_empty(raw: Option<&str>) -> Option<String> {
    raw.filter(|v| !v.is_empty()).map(str::to_owned)
}

/// Where the proxy writes its own log: `logs/proxy.log` beside the binary,
/// under a `test-<ns>` subdirectory for a namespaced (isolated) instance.
///
/// Mirrors the tray's layout so both files land in the same directory, and
/// takes its inputs as parameters so the decision is testable without touching
/// the process environment or the filesystem.
#[must_use]
pub fn log_path_from(exe_dir: Option<&Path>, ns: Option<&str>) -> std::path::PathBuf {
    let base = exe_dir.map_or_else(|| std::path::PathBuf::from("."), Path::to_path_buf);
    let logs = base.join("logs");
    let dir = match ns.filter(|v| !v.trim().is_empty()) {
        Some(ns) => logs.join(format!("test-{ns}")),
        None => logs,
    };
    dir.join("proxy.log")
}

/// The proxy log path for this process: next to `current_exe`, namespaced when
/// `CC_TRAY_NS` is set (same variable the ledger uses).
#[must_use]
pub fn log_path() -> std::path::PathBuf {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf));
    let ns = std::env::var("CC_TRAY_NS").ok();
    log_path_from(exe_dir.as_deref(), ns.as_deref())
}

/// JS `Number()` for the subset of strings these env vars realistically carry.
/// Returns None where JS would yield NaN (parse failure). Note Number("") is 0,
/// not NaN — callers treat "" before reaching here.
fn js_number(raw: &str) -> Option<f64> {
    let t = raw.trim();
    if t.is_empty() {
        return Some(0.0);
    }
    t.parse::<f64>().ok()
}

fn parse_port(raw: Option<&str>, fallback: u64) -> u64 {
    let Some(raw) = raw else { return fallback };
    if raw.is_empty() || raw == "true" {
        return fallback;
    }
    match js_number(raw) {
        Some(n) if n.fract() == 0.0 && (1.0..=65535.0).contains(&n) => n as u64,
        _ => fallback,
    }
}

fn parse_host(raw: Option<&str>) -> String {
    let Some(raw) = raw else {
        return DEFAULT_HOST.into();
    };
    if raw.is_empty() || raw == "true" {
        return DEFAULT_HOST.into();
    }
    // Reject whitespace and control characters rather than letting a garbage
    // value reach bind() and fail with a less actionable error.
    if raw.bytes().any(|b| b <= 0x20 || b == 0x7f) {
        return DEFAULT_HOST.into();
    }
    raw.to_owned()
}

fn parse_body_limit(raw: Option<&str>) -> u64 {
    let Some(raw) = raw else {
        return MAX_BODY_BYTES_DEFAULT;
    };
    if raw.is_empty() {
        return MAX_BODY_BYTES_DEFAULT;
    }
    match js_number(raw) {
        Some(n) if n.is_finite() && n > 0.0 => (n.floor() as u64).min(MAX_BODY_BYTES_MAX),
        _ => MAX_BODY_BYTES_DEFAULT,
    }
}

fn parse_positive_int(raw: Option<&str>, fallback: u64) -> f64 {
    let Some(raw) = raw else {
        return fallback as f64;
    };
    if raw.is_empty() {
        return fallback as f64;
    }
    match js_number(raw) {
        Some(n) if n.is_finite() && n >= 0.0 => n.floor(),
        _ => fallback as f64,
    }
}

fn clamp_timeout(value: f64, max: u64, fallback: u64) -> u64 {
    if !value.is_finite() || value < 0.0 {
        return fallback;
    }
    if value == 0.0 {
        return 0;
    }
    (value as u64).min(max)
}

/// A count where 0 is meaningful (retry zero times means never re-send), so it
/// follows the same shape as `parse_positive_int` but floors at the fallback
/// only for unparseable input, not for a literal zero.
fn parse_non_negative_int(raw: Option<&str>, fallback: u64) -> u64 {
    let Some(raw) = raw else {
        return fallback;
    };
    if raw.is_empty() {
        return fallback;
    }
    match js_number(raw) {
        Some(n) if n.is_finite() && n >= 0.0 => n.floor() as u64,
        _ => fallback,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k: &str| {
            pairs
                .iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| v.to_string())
        }
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn defaults_match_node() {
        let c = load(&[], &no_env);
        assert_eq!(c.host, "127.0.0.1");
        assert_eq!(c.port, 8787);
        assert_eq!(c.cc_api_base, "https://api.commandcode.ai");
        assert_eq!(c.cc_version, "1.54.1");
        assert_eq!(c.log_level, "info");
        assert_eq!(c.cors_origin, "*");
        assert_eq!(c.upstream_timeout_ms, 600_000);
        assert_eq!(c.idle_timeout_ms, 120_000);
        assert_eq!(c.max_body_bytes, 10 * 1024 * 1024);
    }

    #[test]
    fn cli_overrides_env() {
        let argv = svec(["--port", "9999", "--host", "0.0.0.0"]);
        let c = load(&argv, &env_of(&[("PORT", "7777"), ("HOST", "10.0.0.1")]));
        assert_eq!(c.port, 9999);
        assert_eq!(c.host, "0.0.0.0");
    }

    #[test]
    fn invalid_values_fall_back() {
        let c = load(
            &[],
            &env_of(&[
                ("PORT", "not-a-port"),
                ("HOST", "bad host"),
                ("CC_MAX_BODY_BYTES", "-5"),
                ("CC_IDLE_TIMEOUT_MS", "999999999"),
                ("CC_UPSTREAM_TIMEOUT_MS", "-1"),
            ]),
        );
        assert_eq!(c.port, 8787);
        assert_eq!(c.host, "127.0.0.1");
        assert_eq!(c.max_body_bytes, 10 * 1024 * 1024);
        // Typo-sized timeouts clamp to the 30 min ceiling / fall back.
        assert_eq!(c.idle_timeout_ms, TIMEOUT_MAX_MS);
        assert_eq!(c.upstream_timeout_ms, 600_000);
    }

    #[test]
    fn zero_idle_disables_and_body_cap_applies() {
        let c = load(
            &[],
            &env_of(&[
                ("CC_IDLE_TIMEOUT_MS", "0"),
                ("CC_MAX_BODY_BYTES", "999999999999"),
            ]),
        );
        assert_eq!(c.idle_timeout_ms, 0);
        assert_eq!(c.max_body_bytes, MAX_BODY_BYTES_MAX);
    }

    #[test]
    fn empty_cors_origin_is_kept() {
        let c = load(&[], &env_of(&[("CORS_ORIGIN", "")]));
        assert_eq!(c.cors_origin, "");
    }

    #[test]
    fn flag_without_value_is_ignored() {
        let argv = svec(["--port", "--host", "0.0.0.0", "--help"]);
        let c = load(&argv, &no_env);
        // --port saw "--host" as its value → not a port → fallback; --host took 0.0.0.0.
        assert_eq!(c.port, 8787);
        assert_eq!(c.host, "0.0.0.0");
    }

    fn svec<const N: usize>(items: [&str; N]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }
}
